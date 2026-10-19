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
const PARSE_MEMORY_LIMIT: usize = 32 * 1024 * 1024;
const PARSE_DEADLINE: std::time::Duration = std::time::Duration::from_millis(500);
/// After a stop, how long the script gets to end on its own before the
/// engine is abandoned (a script waiting on a promise that never settles
/// runs no code the interrupt could stop).
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

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
    if let Some(what) = not_literal(literal) {
        return Err(format!("meta must be a pure literal (no variables, calls or interpolation): it contains {what}"));
    }
    let rt = Runtime::new().map_err(|e| format!("script engine: {e}"))?;
    // Parsing runs on the caller's thread (a tool call, a listing): bound it.
    rt.set_memory_limit(PARSE_MEMORY_LIMIT);
    let deadline = std::time::Instant::now() + PARSE_DEADLINE;
    rt.set_interrupt_handler(Some(Box::new(move || std::time::Instant::now() > deadline)));
    let ctx = Context::full(&rt).map_err(|e| format!("script engine: {e}"))?;
    let meta_json = ctx.with(|ctx| -> Result<String, String> {
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

/// The body as an async function taking the script's globals. The body
/// starts on the function's first line, so error line numbers are the
/// script file's.
fn wrap(body: &str) -> String {
    format!("(async function __workflow_script__(args) {{{body}\n}})")
}

/// The meta's `name`, read without running anything (for display).
pub fn peek_name(source: &str) -> Option<String> {
    let decl = find_meta(source.trim_start_matches('\u{feff}'))?;
    let literal = &source.trim_start_matches('\u{feff}')[decl.literal];
    let re = regex::Regex::new(r#"(?:\bname|"name"|'name')\s*:\s*(?:"([^"\\]*)"|'([^'\\]*)'|`([^`\\$]*)`)"#).ok()?;
    let code = code_mask(literal);
    // The key itself must be code (or a quoted key), not text in a string.
    let name = re
        .captures_iter(literal)
        .find(|c| {
            let at = c.get(0).map_or(0, |m| m.start());
            code[at] || matches!(literal.as_bytes()[at], b'"' | b'\'') && (at == 0 || code[at - 1])
        })
        .and_then(|c| c.get(1).or(c.get(2)).or(c.get(3)).map(|m| m.as_str().to_string()));
    name
}

/// What makes a meta literal not a plain literal (a call, a function, an
/// interpolation), if anything; strings and comments are skipped.
fn not_literal(literal: &str) -> Option<&'static str> {
    let b = literal.as_bytes();
    let regions = regions(literal);
    for (range, opener) in &regions {
        if *opener == b'`' {
            let inner = range.start + 1..range.end;
            if inner.clone().any(|i| b[i] == b'$' && b.get(i + 1) == Some(&b'{') && !escaped(b, i)) {
                return Some("a template interpolation");
            }
        }
    }
    let code = code_mask(literal);
    for i in 0..b.len() {
        if !code[i] {
            continue;
        }
        match b[i] {
            b'(' => return Some("a call or parenthesized expression"),
            b'=' if b.get(i + 1) == Some(&b'>') => return Some("a function"),
            _ => {}
        }
    }
    let words = regex::Regex::new(r"\b(function|new|class|import|await|yield)\b").ok()?;
    let plain: String = b
        .iter()
        .zip(&code)
        .map(|(c, keep)| if *keep { *c as char } else { ' ' })
        .collect();
    words.is_match(&plain).then_some("a function or an expression")
}

/// Whether byte `i` is escaped by an odd number of backslashes.
fn escaped(b: &[u8], i: usize) -> bool {
    let mut n = 0;
    let mut j = i;
    while j > 0 && b[j - 1] == b'\\' {
        n += 1;
        j -= 1;
    }
    n % 2 == 1
}

struct MetaDecl {
    /// `export` plus the whitespace after it.
    export: std::ops::Range<usize>,
    /// The object literal, braces included.
    literal: std::ops::Range<usize>,
}

/// Where `export const meta = {...}` sits: the first such declaration in
/// code (not in a comment or a string).
fn find_meta(src: &str) -> Option<MetaDecl> {
    let re = regex::Regex::new(r"^export(\s+)const\s+meta\s*=\s*").ok()?;
    let code = code_mask(src);
    let b = src.as_bytes();
    let at = (0..b.len()).find(|&i| {
        code[i]
            && b[i] == b'e'
            && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_' || b[i - 1] == b'$'))
            && re.is_match(&src[i..])
    })?;
    let m = re.captures(&src[at..])?;
    let whole = m.get(0)?;
    let export = at..at + m.get(1)?.end();
    let open = at + whole.end();
    if !src[open..].starts_with('{') {
        return None;
    }
    let close = matching_brace(src, open)?;
    Some(MetaDecl {
        export,
        literal: open..close + 1,
    })
}

/// For each byte: true where it is code, false inside a comment or a
/// string / template literal (quotes included).
fn code_mask(src: &str) -> Vec<bool> {
    let mut mask = vec![true; src.len()];
    for (range, _) in regions(src) {
        for m in &mut mask[range] {
            *m = false;
        }
    }
    mask
}

/// The comments and string / template literals of `src`, quotes and
/// comment markers included, each with the byte that opened it.
fn regions(src: &str) -> Vec<(std::ops::Range<usize>, u8)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        match b[i] {
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
            _ => {
                i += 1;
                continue;
            }
        }
        let end = (i + 1).min(b.len());
        out.push((start..end, b[start]));
        i = end;
    }
    out
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
  Object.defineProperty(D.prototype, "constructor", { value: SafeDate, writable: true, configurable: true });
  globalThis.Date = SafeDate;
  if (typeof globalThis.performance !== "undefined") {
    globalThis.performance = Object.freeze({ now: noClock("performance.now()") });
  }
  Math.random = () => {
    throw new Error("Math.random() is unavailable in workflow scripts (it would break resume); vary prompts by index instead");
  };

  const OPTION_KEYS = ["label", "phase", "schema", "model", "effort", "isolation", "agentType"];
  // Any value as a log line: strings as they are, the rest as JSON (or
  // String() for what JSON leaves out: undefined, functions).
  const text = (v) => {
    if (typeof v === "string") return v;
    try {
      const s = JSON.stringify(v);
      if (s !== undefined) return s;
    } catch (_) {}
    try {
      return String(v);
    } catch (_) {
      return Object.prototype.toString.call(v);
    }
  };
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
      current = text(title);
      h.phase(current, group);
    }
    function log(message) {
      h.log(text(message), group);
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
    // The API is an enclosing scope, not parameters, so the script may
    // declare its own `log` or `phase` at its top level, as it can alone.
    const make = new Function("agent", "phase", "log", "workflow", "return async function (args) {" + loaded.body + "\n}");
    const fn = make(api.agent, api.phase, api.log, nestedWorkflow);
    h.log("workflow " + loaded.name + " started", "");
    const result = await fn(childArgs);
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
    let js = ctx.async_with(async |ctx| -> Result<String, String> {
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
        });
    let watch = {
        let host = host.clone();
        async move {
            while !host.stopped() {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            tokio::time::sleep(STOP_GRACE).await;
        }
    };
    // The interrupt ends a script that is running code; a script stuck
    // waiting (a promise that never settles, or one QuickJS leaves pending
    // when the interrupt lands inside a promise job) is abandoned instead.
    let out = tokio::select! {
        out = js => Some(out),
        _ = watch => None,
    };
    if out.is_none() || stop.load(Ordering::SeqCst) || host.stopped() {
        return Err("stopped".into());
    }
    let text = out.unwrap_or_else(|| Err("stopped".into()))?;
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
            if reference == &json!("shadow") {
                return parse(&format!("{HEAD}const log = 'mine'\nlet phase = 2\nconst agent = 3\nreturn [log, phase, agent]"));
            }
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
        let e = parse("export const meta = { name: 'a', description: 'b', phases: [{ title: `t${1}` }] }\nreturn 1").unwrap_err();
        assert!(e.contains("template interpolation"), "{e}");
        assert!(parse("export const meta = { name: `a`, description: 'b (c) => d' }\nreturn 1").is_ok());
        for bad in ["(() => 'x')()", "String(1)", "[1].map(x => x).join()", "new Date(0).toString()", "function () {}"] {
            let e = parse(&format!("export const meta = {{ name: 'a', description: {bad} }}\nreturn 1")).unwrap_err();
            assert!(e.contains("pure literal"), "{bad}: {e}");
        }
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
        let shadow = go("return await workflow('shadow')", json!(null), host.clone()).unwrap();
        assert_eq!(shadow, json!(["mine", 2, 3]));
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
    fn the_header_is_found_after_comments_that_look_like_code() {
        for head in ["// Review src/*.rs for bugs\n", "// see src/**/*.ts\n", "/* a block */\n", "// export const meta = {}\n"] {
            let s = parse(&format!("{head}export const meta = {{ name: 'a', description: 'b' }}\nreturn 1")).unwrap();
            assert_eq!(s.meta.name, "a", "{head}");
        }
        // A header inside a string is not the header.
        let e = parse("const s = 'export const meta = { name: 1 }'\nreturn 1").unwrap_err();
        assert!(e.contains("must begin"), "{e}");
        assert_eq!(peek_name("// x\nexport const meta = { description: 'd', name: \"find-bugs\" }"), Some("find-bugs".into()));
        assert_eq!(peek_name("return 1"), None);
        assert_eq!(peek_name("export const meta = {\"name\": \"a\", \"description\": \"b\"}"), Some("a".into()));
        assert_eq!(peek_name("export const meta = { description: 'set name: \"zzz\"', name: 'real' }"), Some("real".into()));
        // A template right after a line comment is still a template.
        let e = parse("export const meta = { name: 'a', description: // note\n`x ${1+1}` }\nreturn 1").unwrap_err();
        assert!(e.contains("template interpolation"), "{e}");
        // Backticks inside plain strings are not templates.
        for ok in ["'Fill the `${name}` slot'", "\"Runs `${cmd}` per file\"", "`cost \\${x}`", "\"Edits src/**/`${file}` files\""] {
            let s = parse(&format!("export const meta = {{ name: 'a', description: {ok} }}\nreturn 1"));
            assert!(s.is_ok(), "{ok}: {s:?}");
        }
    }

    #[test]
    fn log_takes_any_value_and_nested_scripts_may_shadow_the_api() {
        let host = Arc::new(Fake::default());
        let out = go("log(undefined)\nlog()\nlog(() => 1)\nlog({a: 1})\nphase(undefined)\nlog(10n)\nconst c = {}; c.c = c; log(c)\nreturn 1", json!(null), host.clone()).unwrap();
        assert_eq!(out, json!(1));
        assert_eq!(host.logs.lock().unwrap().as_slice(), ["|undefined", "|undefined", "|() => 1", "|{\"a\":1}", "|10", "|[object Object]"]);
    }

    #[test]
    fn error_lines_match_the_script_file() {
        let host = Arc::new(Fake::default());
        // Line 1 is the header, line 2 a const, line 3 the error.
        let e = go("const x = 1\nnull.foo", json!(null), host).unwrap_err();
        assert!(e.contains(":3:"), "{e}");
    }

    #[test]
    fn the_clock_cannot_be_reached_another_way() {
        let host = Arc::new(Fake::default());
        let out = go(
            "const r = []\nfor (const f of [() => new Date(0).constructor.now(), () => performance.now()]) { try { f(); r.push('ok') } catch (e) { r.push(e.message.includes('unavailable')) } }\nreturn r",
            json!(null),
            host,
        )
        .unwrap();
        assert_eq!(out, json!([true, true]));
    }

    /// A host whose agents fail once the run is stopped, like the hub's.
    struct Stopping {
        stop: Arc<AtomicBool>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl Host for Stopping {
        fn agent(&self, _req: Value) -> ReplyFut {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let stop = self.stop.clone();
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                if stop.load(Ordering::SeqCst) {
                    Reply::Throw("workflow stopped".into())
                } else {
                    Reply::Null("failed".into())
                }
            })
        }
        fn phase(&self, _: &str, _: &str) {}
        fn log(&self, _: &str, _: &str) {}
        fn budget(&self) -> (Option<u64>, u64) {
            (None, 0)
        }
        fn load(&self, _: &Value) -> Result<Script, String> {
            Err("none".into())
        }
        fn stopped(&self) -> bool {
            self.stop.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn a_stop_ends_scripts_that_wait_forever_or_retry_in_loops() {
        for body in [
            "await new Promise(() => {})",
            "let pending = Array.from({length: 50}, (_, i) => 'f' + i)\nwhile (pending.length) { const res = await parallel(pending.map(f => () => agent('fix ' + f))); pending = pending.filter((f, i) => !res[i]) }",
            "while (true) { await pipeline([1, 2, 3], x => agent('a' + x), y => agent('b' + y)) }",
        ] {
            let stop = Arc::new(AtomicBool::new(false));
            let host = Arc::new(Stopping { stop: stop.clone(), calls: Default::default() });
            let flag = stop.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                flag.store(true, Ordering::SeqCst);
            });
            let t = std::time::Instant::now();
            let script = parse(&format!("{HEAD}{body}")).unwrap();
            assert_eq!(run(&script, json!(null), host).unwrap_err(), "stopped", "{body}");
            assert!(t.elapsed() < Duration::from_secs(5), "{body}: {:?}", t.elapsed());
        }
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
