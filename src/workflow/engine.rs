//! The workflow script engine: QuickJS (through rquickjs) on a thread of
//! its own. A script is plain JavaScript that starts with
//! `export const meta = {...}` (a pure literal) and runs as the body of an
//! async function, so it can `await` and `return` at the top level.
//!
//! Only a few host calls cross into Rust (start an agent, record a phase
//! or a log line, read the budget, load another workflow); `parallel`,
//! `pipeline` and the argument checks live in [`PRELUDE`]. The clock and
//! the random source throw, so a script replays the same way on resume.
//! QuickJS has no file, network or timer access of its own.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rquickjs::prelude::{Async, Func};
use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, Context, Function, Promise, Runtime};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Most items one `parallel()` / `pipeline()` call takes.
pub const MAX_FAN_OUT: usize = 4096;
const MEMORY_LIMIT: usize = 512 * 1024 * 1024;
const JS_STACK: usize = 4 * 1024 * 1024;
const THREAD_STACK: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PhaseMeta {
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub name: String,
    pub description: String,
    #[serde(default, rename = "whenToUse", skip_serializing_if = "String::is_empty")]
    pub when_to_use: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<PhaseMeta>,
}

/// A parsed script: its meta and the body to run (with `export` taken off
/// the meta declaration so it parses inside a function).
#[derive(Debug, Clone)]
pub struct Script {
    pub meta: Meta,
    pub body: String,
}

/// What one host `agent()` call hands back to the script.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// The agent's result: its text, or the parsed object for a schema call.
    Value(Value),
    /// The agent failed or was skipped: the script sees `null`.
    Null(String),
    /// A hard limit or a stop: the script sees a thrown error.
    Throw(String),
}

pub type ReplyFut = Pin<Box<dyn Future<Output = Reply> + Send>>;

/// The orchestrator side of a running script.
pub trait Host: Send + Sync + 'static {
    /// Start one agent. `req` holds `prompt` plus the options the script
    /// passed (`label`, `phase`, `schema`, `model`, `effort`, `isolation`,
    /// `agentType`) and `group` for an agent of a nested workflow.
    fn agent(&self, req: Value) -> ReplyFut;
    fn phase(&self, title: &str, group: &str);
    fn log(&self, message: &str, group: &str);
    /// `(total, spent)`: the token target, if one is set, and what has
    /// been spent toward it.
    fn budget(&self) -> (Option<u64>, u64);
    /// The script a `workflow(nameOrRef)` call names, parsed.
    fn load(&self, reference: &Value) -> Result<Script, String>;
    /// The run was stopped: interrupt the script.
    fn stopped(&self) -> bool;
}

/// Parse a script: read its meta and check that the whole body compiles.
pub fn parse(source: &str) -> Result<Script, String> {
    let source = source.trim_start_matches('\u{feff}');
    let Some(decl) = find_meta(source) else {
        return Err("the script must begin with `export const meta = { name, description, ... }`".into());
    };
    let literal = &source[decl.literal.clone()];
    let rt = Runtime::new().map_err(|e| format!("script engine: {e}"))?;
    let ctx = Context::full(&rt).map_err(|e| format!("script engine: {e}"))?;
    let meta_json = ctx.with(|ctx| -> Result<String, String> {
        // Nothing the literal could call is defined: a non-literal fails here.
        ctx.globals().set("Math", rquickjs::Undefined).ok();
        let v = ctx
            .eval::<rquickjs::Value, _>(format!("({literal})"))
            .catch(&ctx)
            .map_err(|e| format!("meta must be a pure literal (no variables, calls or interpolation): {e}"))?;
        ctx.json_stringify(v)
            .map_err(|e| format!("meta: {e}"))?
            .map(|s| s.to_string().unwrap_or_default())
            .ok_or_else(|| "meta must be an object literal".to_string())
    })?;
    let meta: Meta = serde_json::from_str(&meta_json)
        .map_err(|e| format!("meta needs string fields name and description (phases: [{{title, detail?}}]): {e}"))?;
    if meta.name.trim().is_empty() || meta.description.trim().is_empty() {
        return Err("meta.name and meta.description must not be empty".into());
    }
    let mut body = String::with_capacity(source.len());
    body.push_str(&source[..decl.export.start]);
    body.push_str(&source[decl.export.end..]);
    let compiled = ctx.with(|ctx| {
        ctx.eval::<(), _>(wrap(&body))
            .catch(&ctx)
            .map_err(|e| e.to_string())
    });
    if let Err(e) = compiled {
        return Err(format!("the script does not compile (plain JavaScript only, no TypeScript or imports): {e}"));
    }
    Ok(Script { meta, body })
}

/// The body as an async function taking the script's globals, line 1 of
/// the body on line 2 of the function.
fn wrap(body: &str) -> String {
    format!("(async function __workflow_script__(args) {{\n{body}\n}})")
}

struct MetaDecl {
    /// `export` plus the whitespace after it.
    export: std::ops::Range<usize>,
    /// The object literal, braces included.
    literal: std::ops::Range<usize>,
}

/// Where `export const meta = {...}` sits, skipping comments.
fn find_meta(src: &str) -> Option<MetaDecl> {
    let re = regex::Regex::new(r"\bexport(\s+)const\s+meta\s*=\s*").ok()?;
    let m = re.captures_iter(src).find(|c| {
        let at = c.get(0).map_or(0, |m| m.start());
        !in_comment(src, at)
    })?;
    let whole = m.get(0)?;
    let export = whole.start()..m.get(1)?.end();
    let open = whole.end();
    if !src[open..].starts_with('{') {
        return None;
    }
    let close = matching_brace(src, open)?;
    Some(MetaDecl {
        export,
        literal: open..close + 1,
    })
}

/// Whether byte `at` is inside a `//` or `/* */` comment (strings are not
/// tracked: good enough to skip a commented-out example of the header).
fn in_comment(src: &str, at: usize) -> bool {
    let before = &src[..at];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    if before[line_start..].contains("//") {
        return true;
    }
    match (before.rfind("/*"), before.rfind("*/")) {
        (Some(open), Some(close)) => open > close,
        (Some(_), None) => true,
        _ => false,
    }
}

/// The index of the `}` that closes the `{` at `open`, skipping strings,
/// template literals and comments.
fn matching_brace(src: &str, open: usize) -> Option<usize> {
    let b = src.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            q @ (b'"' | b'\'' | b'`') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The script API, built on the host calls in `h`. Every name it defines
/// is a global the script sees.
const PRELUDE: &str = r#"
(function (h) {
  "use strict";
  const MAX = h.max;
  const AsyncFunction = (async function () {}).constructor;
  const D = Date;
  const noClock = (what) => () => {
    throw new Error(what + " is unavailable in workflow scripts (it would break resume); pass timestamps in via args");
  };
  function SafeDate(...a) {
    if (!new.target) noClock("Date()")();
    if (a.length === 0) noClock("new Date() without arguments")();
    return new D(...a);
  }
  SafeDate.now = noClock("Date.now()");
  SafeDate.parse = D.parse;
  SafeDate.UTC = D.UTC;
  SafeDate.prototype = D.prototype;
  globalThis.Date = SafeDate;
  Math.random = () => {
    throw new Error("Math.random() is unavailable in workflow scripts (it would break resume); vary prompts by index instead");
  };

  const OPTION_KEYS = ["label", "phase", "schema", "model", "effort", "isolation", "agentType"];
  function makeApi(group) {
    let current = null;
    async function agent(prompt, opts) {
      if (typeof prompt !== "string" || prompt.trim() === "") {
        throw new TypeError("agent(prompt, opts): prompt must be a non-empty string");
      }
      if (opts !== undefined && (opts === null || typeof opts !== "object" || Array.isArray(opts))) {
        throw new TypeError("agent(prompt, opts): opts must be an object");
      }
      const req = { prompt, group };
      for (const k of OPTION_KEYS) {
        if (opts && opts[k] !== undefined && opts[k] !== null) req[k] = opts[k];
      }
      if (req.phase === undefined && current !== null) req.phase = current;
      const reply = JSON.parse(await h.agent(JSON.stringify(req)));
      if ("throw" in reply) throw new Error(reply.throw);
      if ("null" in reply) return null;
      return reply.value;
    }
    function phase(title) {
      current = String(title);
      h.phase(current, group);
    }
    function log(message) {
      h.log(typeof message === "string" ? message : JSON.stringify(message), group);
    }
    return { agent, phase, log };
  }

  function listOf(items, what) {
    if (!Array.isArray(items)) throw new TypeError(what + " takes an array");
    if (items.length > MAX) {
      throw new RangeError(what + " accepts at most " + MAX + " items, got " + items.length + "; split the work into batches");
    }
    return items;
  }

  async function parallel(thunks) {
    listOf(thunks, "parallel()");
    return Promise.all(thunks.map((t) =>
      Promise.resolve()
        .then(() => (typeof t === "function" ? t() : t))
        .catch((e) => {
          h.log("parallel(): a task threw and became null: " + (e && e.message ? e.message : String(e)), "");
          return null;
        })
    ));
  }

  async function pipeline(items, ...stages) {
    listOf(items, "pipeline()");
    for (const s of stages) {
      if (typeof s !== "function") throw new TypeError("pipeline(items, ...stages): every stage must be a function");
    }
    return Promise.all(items.map(async (item, index) => {
      let prev = item;
      for (const stage of stages) {
        try {
          prev = await stage(prev, item, index);
        } catch (e) {
          h.log("pipeline(): item " + index + " dropped to null: " + (e && e.message ? e.message : String(e)), "");
          return null;
        }
      }
      return prev;
    }));
  }

  const budget = Object.freeze({
    get total() {
      return JSON.parse(h.budget()).total;
    },
    spent() {
      return JSON.parse(h.budget()).spent;
    },
    remaining() {
      const b = JSON.parse(h.budget());
      return b.total === null ? Infinity : Math.max(0, b.total - b.spent);
    },
  });

  const nestedWorkflow = async () => {
    throw new Error("workflow() nests one level only: a nested workflow cannot call workflow()");
  };

  async function workflow(ref, childArgs) {
    if (typeof ref !== "string" && (ref === null || typeof ref !== "object" || typeof ref.scriptPath !== "string")) {
      throw new TypeError("workflow(nameOrRef, args): pass a saved workflow's name or {scriptPath}");
    }
    const loaded = JSON.parse(h.load(JSON.stringify(ref)));
    if ("throw" in loaded) throw new Error(loaded.throw);
    const api = makeApi(loaded.name);
    const fn = new AsyncFunction("args", "agent", "phase", "log", "workflow", loaded.body);
    h.log("workflow " + loaded.name + " started", "");
    const result = await fn(childArgs, api.agent, api.phase, api.log, nestedWorkflow);
    h.log("workflow " + loaded.name + " finished", "");
    return result;
  }

  const top = makeApi("");
  globalThis.agent = top.agent;
  globalThis.phase = top.phase;
  globalThis.log = top.log;
  globalThis.parallel = parallel;
  globalThis.pipeline = pipeline;
  globalThis.budget = budget;
  globalThis.workflow = workflow;
})
"#;

/// Run a parsed script to its return value, on a thread of its own.
/// Blocks the caller until the script ends; call it from a dedicated
/// thread or `spawn_blocking`.
pub fn run(script: &Script, args: Value, host: Arc<dyn Host>) -> Result<Value, String> {
    let body = script.body.clone();
    let worker = std::thread::Builder::new()
        .name("workflow-js".into())
        .stack_size(THREAD_STACK)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("script runtime: {e}"))?;
            rt.block_on(run_async(body, args, host))
        })
        .map_err(|e| format!("script thread: {e}"))?;
    worker.join().unwrap_or_else(|_| Err("the script engine crashed".into()))
}

async fn run_async(body: String, args: Value, host: Arc<dyn Host>) -> Result<Value, String> {
    let rt = AsyncRuntime::new().map_err(|e| format!("script engine: {e}"))?;
    rt.set_memory_limit(MEMORY_LIMIT).await;
    rt.set_max_stack_size(JS_STACK).await;
    let stop = Arc::new(AtomicBool::new(false));
    {
        let host = host.clone();
        let stop = stop.clone();
        rt.set_interrupt_handler(Some(Box::new(move || {
            let hit = host.stopped();
            if hit {
                stop.store(true, Ordering::SeqCst);
            }
            hit
        })))
        .await;
    }
    let ctx = AsyncContext::full(&rt).await.map_err(|e| format!("script engine: {e}"))?;
    let args_json = args.to_string();
    let out: Result<String, String> = ctx
        .async_with(async |ctx| {
            let h = host_object(&ctx, host.clone()).map_err(|e| format!("script engine: {e}"))?;
            let install: Function = ctx
                .eval(PRELUDE)
                .catch(&ctx)
                .map_err(|e| format!("script prelude: {e}"))?;
            install.call::<_, ()>((h,)).catch(&ctx).map_err(|e| format!("script prelude: {e}"))?;
            let main: Function = ctx
                .eval(wrap(&body))
                .catch(&ctx)
                .map_err(|e| format!("script: {e}"))?;
            let args_v = ctx.json_parse(args_json).map_err(|e| format!("args: {e}"))?;
            let promise: Promise = main.call((args_v,)).catch(&ctx).map_err(|e| e.to_string())?;
            let value = promise
                .into_future::<rquickjs::Value>()
                .await
                .catch(&ctx)
                .map_err(|e| e.to_string())?;
            Ok(ctx
                .json_stringify(value)
                .map_err(|e| format!("the script's return value is not JSON: {e}"))?
                .map(|s| s.to_string().unwrap_or_else(|_| "null".into()))
                .unwrap_or_else(|| "null".into()))
        })
        .await;
    if stop.load(Ordering::SeqCst) || host.stopped() {
        return Err("stopped".into());
    }
    let text = out?;
    serde_json::from_str(&text).map_err(|e| format!("the script's return value is not JSON: {e}"))
}

/// The host calls as a JS object for [`PRELUDE`].
fn host_object<'js>(ctx: &rquickjs::Ctx<'js>, host: Arc<dyn Host>) -> rquickjs::Result<rquickjs::Object<'js>> {
    let h = rquickjs::Object::new(ctx.clone())?;
    h.set("max", MAX_FAN_OUT as f64)?;
    {
        let host = host.clone();
        h.set(
            "agent",
            Func::from(Async(move |req: String| {
                let host = host.clone();
                async move {
                    let req: Value = serde_json::from_str(&req).unwrap_or(Value::Null);
                    let reply = match req {
                        Value::Object(_) => host.agent(req).await,
                        _ => Reply::Throw("agent(): bad request".into()),
                    };
                    rquickjs::Result::Ok(reply_json(&reply))
                }
            })),
        )?;
    }
    {
        let host = host.clone();
        h.set(
            "phase",
            Func::from(move |title: String, group: String| host.phase(&title, &group)),
        )?;
    }
    {
        let host = host.clone();
        h.set(
            "log",
            Func::from(move |message: String, group: String| host.log(&message, &group)),
        )?;
    }
    {
        let host = host.clone();
        h.set(
            "budget",
            Func::from(move || {
                let (total, spent) = host.budget();
                json!({"total": total, "spent": spent}).to_string()
            }),
        )?;
    }
    {
        let host = host.clone();
        h.set(
            "load",
            Func::from(move |reference: String| {
                let reference: Value = serde_json::from_str(&reference).unwrap_or(Value::Null);
                match host.load(&reference) {
                    Ok(s) => json!({"name": s.meta.name, "body": s.body}).to_string(),
                    Err(e) => json!({"throw": e}).to_string(),
                }
            }),
        )?;
    }
    Ok(h)
}

fn reply_json(reply: &Reply) -> String {
    match reply {
        Reply::Value(v) => json!({"value": v}),
        Reply::Null(why) => json!({"null": why}),
        Reply::Throw(why) => json!({"throw": why}),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    const HEAD: &str = "export const meta = { name: 't', description: 'test' }\n";

    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<Value>>,
        logs: Mutex<Vec<String>>,
        phases: Mutex<Vec<String>>,
        stop: AtomicBool,
        spent: std::sync::atomic::AtomicU64,
        /// agent() sleeps this long, so calls overlap.
        delay_ms: u64,
    }

    impl Host for Fake {
        fn agent(&self, req: Value) -> ReplyFut {
            self.calls.lock().unwrap().push(req.clone());
            let delay = self.delay_ms;
            self.spent.fetch_add(10, Ordering::SeqCst);
            Box::pin(async move {
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                let prompt = req["prompt"].as_str().unwrap_or("").to_string();
                if prompt.contains("FAIL") {
                    Reply::Null("failed".into())
                } else if prompt.contains("LIMIT") {
                    Reply::Throw("cap reached".into())
                } else if req.get("schema").is_some() {
                    Reply::Value(json!({"echo": prompt}))
                } else {
                    Reply::Value(Value::String(format!("done:{prompt}")))
                }
            })
        }
        fn phase(&self, title: &str, group: &str) {
            self.phases.lock().unwrap().push(format!("{group}|{title}"));
        }
        fn log(&self, message: &str, group: &str) {
            self.logs.lock().unwrap().push(format!("{group}|{message}"));
        }
        fn budget(&self) -> (Option<u64>, u64) {
            (Some(25), self.spent.load(Ordering::SeqCst))
        }
        fn load(&self, reference: &Value) -> Result<Script, String> {
            if reference == &json!("child") {
                parse(&format!(
                    "{HEAD}phase('inner')\nconst r = await agent('child ' + args.n)\nreturn [r, await workflow('child').catch(e => e.message)]"
                ))
            } else {
                Err(format!("no workflow {reference}"))
            }
        }
        fn stopped(&self) -> bool {
            self.stop.load(Ordering::SeqCst)
        }
    }

    fn go(body: &str, args: Value, host: Arc<Fake>) -> Result<Value, String> {
        let script = parse(&format!("{HEAD}{body}"))?;
        run(&script, args, host)
    }

    #[test]
    fn parse_reads_meta_and_strips_export() {
        let s = parse(
            "// a comment: export const meta = {}\nexport const meta = {\n  name: 'find-bugs', description: 'Find {bugs}', // }\n  phases: [{ title: 'Scan', detail: 'grep' }, { title: 'Fix' }],\n}\nreturn 1",
        )
        .unwrap();
        assert_eq!(s.meta.name, "find-bugs");
        assert_eq!(s.meta.description, "Find {bugs}");
        assert_eq!(s.meta.phases.len(), 2);
        assert_eq!(s.meta.phases[0].detail, "grep");
        assert!(s.body.contains("const meta = {"));
        assert!(!s.body.contains("export const meta = {\n"));
    }

    #[test]
    fn parse_rejects_bad_scripts() {
        assert!(parse("return 1").unwrap_err().contains("export const meta"));
        let e = parse("const n = 'x'\nexport const meta = { name: n, description: 'd' }").unwrap_err();
        assert!(e.contains("pure literal"), "{e}");
        let e = parse("export const meta = { name: 'a', description: 'b', phases: [{ title: `t${1}` }] }\nreturn 1");
        // A template with interpolation still evaluates to a literal string; it is accepted.
        assert!(e.is_ok());
        let e = parse("export const meta = { name: 'a' }").unwrap_err();
        assert!(e.contains("description"), "{e}");
        let e = parse(&format!("{HEAD}const x: string[] = []")).unwrap_err();
        assert!(e.contains("does not compile"), "{e}");
        let e = parse(&format!("{HEAD}import fs from 'fs'")).unwrap_err();
        assert!(e.contains("does not compile"), "{e}");
    }

    #[test]
    fn agents_run_and_return_values() {
        let host = Arc::new(Fake::default());
        let out = go(
            "phase('Scan')\nconst a = await agent('one', {label: 'l1'})\nconst b = await agent('two', {schema: {type: 'object'}, phase: 'Other'})\nlog('hi')\nreturn {a, b, n: args.n}",
            json!({"n": 3}),
            host.clone(),
        )
        .unwrap();
        assert_eq!(out, json!({"a": "done:one", "b": {"echo": "two"}, "n": 3}));
        let calls = host.calls.lock().unwrap();
        assert_eq!(calls[0]["label"], "l1");
        assert_eq!(calls[0]["phase"], "Scan");
        assert_eq!(calls[1]["phase"], "Other");
        assert_eq!(host.phases.lock().unwrap().as_slice(), ["|Scan"]);
        assert_eq!(host.logs.lock().unwrap().as_slice(), ["|hi"]);
    }

    #[test]
    fn parallel_overlaps_and_turns_throws_into_null() {
        let host = Arc::new(Fake { delay_ms: 200, ..Default::default() });
        let t = std::time::Instant::now();
        let out = go(
            "return await parallel([1,2,3,4,5].map(i => () => agent('p' + i)).concat([() => { throw new Error('boom') }, () => agent('FAIL')]))",
            json!(null),
            host,
        )
        .unwrap();
        assert!(t.elapsed() < Duration::from_millis(900), "agents did not overlap: {:?}", t.elapsed());
        assert_eq!(out, json!(["done:p1", "done:p2", "done:p3", "done:p4", "done:p5", null, null]));
    }

    #[test]
    fn pipeline_passes_prev_item_and_index_and_drops_throwers() {
        let host = Arc::new(Fake::default());
        let out = go(
            "return await pipeline(['a','b','c'], (prev, item, i) => agent(prev + i), (prev, item, i) => { if (item === 'b') throw new Error('x'); return prev + '|' + item })",
            json!(null),
            host,
        )
        .unwrap();
        assert_eq!(out, json!(["done:a0|a", null, "done:c2|c"]));
    }

    #[test]
    fn hard_limits_throw_and_failures_are_null() {
        let host = Arc::new(Fake::default());
        let out = go(
            "const f = await agent('FAIL')\nlet e = null\ntry { await agent('LIMIT') } catch (x) { e = x.message }\nreturn {f, e}",
            json!(null),
            host,
        )
        .unwrap();
        assert_eq!(out, json!({"f": null, "e": "cap reached"}));
    }

    #[test]
    fn clock_and_random_throw_but_dates_from_args_work() {
        let host = Arc::new(Fake::default());
        let out = go(
            "const r = []\nfor (const f of [() => Date.now(), () => new Date(), () => Math.random(), () => Date()]) { try { f(); r.push('ok') } catch (e) { r.push(e.message.includes('unavailable')) } }\nr.push(new Date(0).toISOString())\nreturn r",
            json!(null),
            host,
        )
        .unwrap();
        assert_eq!(out, json!([true, true, true, true, "1970-01-01T00:00:00.000Z"]));
    }

    #[test]
    fn budget_reports_total_spent_and_remaining() {
        let host = Arc::new(Fake::default());
        let out = go(
            "const before = budget.remaining()\nawait agent('x')\nreturn [budget.total, budget.spent(), before, budget.remaining()]",
            json!(null),
            host,
        )
        .unwrap();
        assert_eq!(out, json!([25, 10, 25, 15]));
    }

    #[test]
    fn nested_workflow_runs_one_level() {
        let host = Arc::new(Fake::default());
        let out = go("return await workflow('child', {n: 7})", json!(null), host.clone()).unwrap();
        assert_eq!(
            out,
            json!(["done:child 7", "workflow() nests one level only: a nested workflow cannot call workflow()"])
        );
        assert_eq!(host.calls.lock().unwrap()[0]["group"], "t");
        assert_eq!(host.phases.lock().unwrap().as_slice(), ["t|inner"]);
        let e = go("return await workflow('nope')", json!(null), host).unwrap_err();
        assert!(e.contains("no workflow"), "{e}");
    }

    #[test]
    fn errors_and_bad_inputs_surface() {
        let host = Arc::new(Fake::default());
        let e = go("throw new Error('kaput')", json!(null), host.clone()).unwrap_err();
        assert!(e.contains("kaput"), "{e}");
        let e = go("await agent('')", json!(null), host.clone()).unwrap_err();
        assert!(e.contains("non-empty string"), "{e}");
        let e = go("await parallel(new Array(5000).fill(0))", json!(null), host.clone()).unwrap_err();
        assert!(e.contains("at most 4096"), "{e}");
        assert_eq!(go("return undefined", json!(null), host).unwrap(), Value::Null);
    }

    #[test]
    fn a_stop_interrupts_a_busy_loop_and_pending_agents() {
        let host = Arc::new(Fake::default());
        let h2 = host.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            h2.stop.store(true, Ordering::SeqCst);
        });
        let t = std::time::Instant::now();
        let e = go("while (true) {}", json!(null), host).unwrap_err();
        assert_eq!(e, "stopped");
        assert!(t.elapsed() < Duration::from_secs(5));
    }
}
