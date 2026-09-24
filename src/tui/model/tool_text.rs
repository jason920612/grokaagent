//! One-line descriptions of tool calls for rows, the activity line and the
//! tool timeline.

use serde_json::Value;

fn arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or("")
}

fn file_target(name: &str, args: &Value) -> String {
    if name == "screenshot" {
        if let Some(n) = args.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return format!("name={n}");
        }
        if let Some(pid) = args.get("pid") {
            return format!("pid={pid}");
        }
    }
    let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
    if let Some(pattern) = args.get("pattern").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        return format!("{path}  /{pattern}/");
    }
    match (args.get("line").or_else(|| args.get("start_line")), args.get("end_line")) {
        (Some(line), Some(end)) => format!("{path}  :{line}-{end}"),
        (Some(line), None) => format!("{path}  :{line}"),
        _ => path.to_string(),
    }
}

/// What the call is about, without a status prefix: `$ cargo test`, `src/a.rs`.
pub(crate) fn tool_subject(name: &str, args: &Value) -> String {
    match name {
        "run_command" | "attach_monitor" | "run_background" => {
            let cmd = arg(args, "command");
            match args.get("window").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                Some(w) => format!("[{w}] $ {cmd}"),
                None => format!("$ {cmd}"),
            }
        }
        "kill_background" | "read_background" => arg(args, "name").to_string(),
        "timer" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("start");
            if action == "cancel" || action == "list" {
                format!("{action}  {}", arg(args, "name")).trim_end().to_string()
            } else {
                let secs = args.get("seconds").map(|v| v.to_string()).unwrap_or_default();
                let mode = if args.get("block").and_then(Value::as_bool).unwrap_or(false) {
                    "阻塞"
                } else {
                    "背景"
                };
                let cmd = arg(args, "command");
                if cmd.is_empty() {
                    format!("{secs}s {mode}")
                } else {
                    format!("{secs}s {mode}  $ {cmd}")
                }
            }
        }
        "write_file" | "edit_file" | "read_file" | "delete_file" | "list_dir" | "screenshot"
        | "read_image" => file_target(name, args),
        "project_memory" => {
            let target = file_target(name, args);
            let action = arg(args, "action");
            if target.is_empty() || target == "." {
                action.to_string()
            } else {
                format!("{action}  {target}")
            }
        }
        "spawn_agent" | "send_message" | "stop_agent" => arg(args, "name").to_string(),
        "wait_agents" => match args.get("names").and_then(Value::as_array) {
            Some(n) if !n.is_empty() => n
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", "),
            _ => "全部".into(),
        },
        "ask_user" => arg(args, "question").to_string(),
        _ => String::new(),
    }
}

/// `▸ name  subject` for a call that is starting.
pub(crate) fn tool_started_line(name: &str, args: &Value) -> String {
    let subject = tool_subject(name, args);
    if subject.is_empty() {
        format!("▸ {name}")
    } else {
        format!("▸ {name}  {subject}")
    }
}

/// `✓ name  summary` for a finished call.
pub(crate) fn tool_finished_line(name: &str, output: &str) -> String {
    let parsed = serde_json::from_str::<Value>(output).ok();
    match name {
        "spawn_agent" => {
            let n = parsed
                .as_ref()
                .and_then(|v| v.get("name").and_then(Value::as_str))
                .unwrap_or("");
            format!("✓ spawn_agent  {n}").trim_end().to_string()
        }
        "write_file" | "edit_file" | "delete_file" | "list_dir" | "screenshot" | "read_image"
        | "project_memory" => format!("✓ {name}"),
        "run_command" => match parsed {
            Some(v) => {
                let code = v
                    .get("exit_code")
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".into());
                let first: String = arg(&v, "stdout")
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .chars()
                    .take(80)
                    .collect();
                if first.is_empty() {
                    format!("✓ run_command  exit {code}")
                } else {
                    format!("✓ run_command  exit {code}  {first}")
                }
            }
            None => format!("✓ {name}"),
        },
        _ => {
            let first: String = output
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .chars()
                .take(80)
                .collect();
            if first.is_empty() {
                format!("✓ {name}")
            } else {
                format!("✓ {name}  {first}")
            }
        }
    }
}

/// Activity line while a tool runs: `執行中  $ cargo test`.
pub(crate) fn live_tool_activity(name: &str, args: &Value, phase: &str) -> String {
    let subject = tool_subject(name, args);
    match name {
        "run_command" | "run_background" | "attach_monitor" => format!("{phase}  {subject}"),
        "timer" => format!("{phase}  timer  {subject}"),
        _ if subject.is_empty() => format!("{phase}  {name}"),
        _ => format!("{phase}  {name}  {subject}"),
    }
}

pub(crate) fn server_query(payload: &Value) -> String {
    payload
        .pointer("/action/query")
        .or_else(|| payload.get("query"))
        .or_else(|| payload.pointer("/item/action/query"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// `web_search` / `x_search` for any event kind about them.
pub(crate) fn canonical_server_tool(kind: &str) -> String {
    let k = kind.to_ascii_lowercase();
    if k.contains("x_search") {
        "x_search".into()
    } else if k.contains("web_search") {
        "web_search".into()
    } else {
        kind.rsplit('.')
            .next()
            .unwrap_or(kind)
            .trim_end_matches("_call")
            .to_string()
    }
}

pub(crate) fn server_phase(kind: &str, payload: &Value) -> String {
    let k = kind.to_ascii_lowercase();
    let status = payload
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if k.contains("searching") || status == "searching" {
        "搜尋中".into()
    } else if k.contains("in_progress") || status == "in_progress" {
        "進行中".into()
    } else if k.contains("completed") || k.ends_with(".done") || status == "completed" || status == "done" {
        "完成".into()
    } else if !status.is_empty() {
        status
    } else {
        "進行中".into()
    }
}

pub(crate) fn phase_is_done(phase: &str) -> bool {
    matches!(phase, "完成" | "completed" | "done")
}

pub(crate) fn server_tool_line(kind: &str, payload: &Value) -> String {
    let query = server_query(payload);
    let name = canonical_server_tool(kind);
    if query.is_empty() {
        format!("▸ {name}")
    } else {
        format!("▸ {name}  {query}")
    }
}

pub(crate) fn spinner(tick: u8) -> char {
    const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    FRAMES[(tick as usize) % FRAMES.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn started_lines_name_the_subject() {
        assert_eq!(tool_started_line("run_command", &json!({"command": "ls"})), "▸ run_command  $ ls");
        assert_eq!(
            tool_started_line("read_file", &json!({"path": "a.rs", "start_line": 3, "end_line": 9})),
            "▸ read_file  a.rs  :3-9"
        );
        assert_eq!(tool_started_line("spawn_agent", &json!({"name": "coder"})), "▸ spawn_agent  coder");
        assert_eq!(tool_started_line("wait_agents", &json!({})), "▸ wait_agents  全部");
        assert_eq!(tool_started_line("now", &json!({})), "▸ now");
    }

    #[test]
    fn finished_lines_summarize_output() {
        assert_eq!(
            tool_finished_line("run_command", r#"{"exit_code":0,"stdout":"ok\nmore"}"#),
            "✓ run_command  exit 0  ok"
        );
        assert_eq!(tool_finished_line("spawn_agent", r#"{"name":"coder"}"#), "✓ spawn_agent  coder");
        assert_eq!(tool_finished_line("now", "2026-01-01"), "✓ now  2026-01-01");
    }

    #[test]
    fn server_tools_are_named_and_phased() {
        assert_eq!(canonical_server_tool("response.web_search_call.searching"), "web_search");
        assert_eq!(server_phase("response.web_search_call.completed", &json!({})), "完成");
        assert!(phase_is_done("完成"));
        assert_eq!(
            server_tool_line("x_search_call", &json!({"action": {"query": "rust"}})),
            "▸ x_search  rust"
        );
    }
}
