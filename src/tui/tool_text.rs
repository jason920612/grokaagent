fn file_tool_target(_name: &str, args: &Value) -> String {
    if _name == "screenshot" {
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
    if let Some(line) = args.get("line").or_else(|| args.get("start_line")) {
        let end = args.get("end_line");
        match end {
            Some(end) => format!("{path}  :{line}-{end}"),
            None => format!("{path}  :{line}"),
        }
    } else {
        path.to_string()
    }
}

fn tool_started_line(name: &str, args: &Value) -> String {
    match name {
        "run_command" | "attach_monitor" | "run_background" => {
            let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
            if let Some(w) = args.get("window").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                format!("▸ {name}  [{w}] $ {cmd}")
            } else {
                format!("▸ {name}  $ {cmd}")
            }
        }
        "kill_background" | "read_background" => {
            let n = args.get("name").and_then(Value::as_str).unwrap_or("");
            format!("▸ {name}  {n}")
        }
        "timer" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("start");
            if action == "cancel" || action == "list" {
                let n = args.get("name").and_then(Value::as_str).unwrap_or("");
                if n.is_empty() {
                    format!("▸ timer  {action}")
                } else {
                    format!("▸ timer  {action}  {n}")
                }
            } else {
                let secs = args
                    .get("seconds")
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                let block = args.get("block").and_then(Value::as_bool).unwrap_or(false);
                let mode = if block { "阻塞" } else { "背景" };
                let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
                if cmd.is_empty() {
                    format!("▸ timer  {secs}s {mode}")
                } else {
                    format!("▸ timer  {secs}s {mode}  $ {cmd}")
                }
            }
        }
        "write_file" | "edit_file" | "read_file" | "delete_file" | "list_dir" | "screenshot" | "read_image" => {
            format!("▸ {name}  {}", file_tool_target(name, args))
        }
        "project_memory" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("");
            let target = file_tool_target(name, args);
            if target.is_empty() || target == "." {
                format!("▸ project_memory  {action}")
            } else {
                format!("▸ project_memory  {action}  {target}")
            }
        }
        "spawn_agent" => {
            let n = args
                .get("name")
                .or_else(|| args.get("agent_name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("▸ spawn_agent  {n}")
        }
        "ask_user" => {
            let q = args.get("question").and_then(Value::as_str).unwrap_or("");
            format!("▸ ask_user  {q}")
        }
        _ => format!("▸ {name}"),
    }
}

fn tool_finished_line(name: &str, output: &str) -> String {
    match name {
        "spawn_agent" => {
            let n = serde_json::from_str::<Value>(output)
                .ok()
                .and_then(|v| {
                    v.get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            if spawn_output_still_running(output) {
                if n.is_empty() {
                    "▸ spawn_agent  執行中".into()
                } else {
                    format!("▸ spawn_agent  {n}  執行中")
                }
            } else if n.is_empty() {
                "✓ spawn_agent".into()
            } else {
                format!("✓ spawn_agent  {n}")
            }
        }
        "write_file" | "edit_file" | "delete_file" | "list_dir" | "screenshot" | "read_image" | "project_memory" => {
            format!("✓ {name}")
        }
        "run_command" => {
            if let Ok(v) = serde_json::from_str::<Value>(output) {
                let code = v
                    .get("exit_code")
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".into());
                let stdout = v.get("stdout").and_then(Value::as_str).unwrap_or("");
                let first = stdout.lines().next().unwrap_or("").trim();
                let clip: String = first.chars().take(80).collect();
                if clip.is_empty() {
                    format!("✓ run_command  exit {code}")
                } else {
                    format!("✓ run_command  exit {code}  {clip}")
                }
            } else {
                format!("✓ {name}")
            }
        }
        _ => {
            let first = output.lines().next().unwrap_or("").trim();
            let clip: String = first.chars().take(80).collect();
            if clip.is_empty() {
                format!("✓ {name}")
            } else {
                format!("✓ {name}  {clip}")
            }
        }
    }
}

fn spawn_output_still_running(output: &str) -> bool {
    serde_json::from_str::<Value>(output)
        .ok()
        .and_then(|v| v.get("state").and_then(Value::as_str).map(str::to_string))
        .is_some_and(|s| {
            s == "TASK_STATE_WORKING"
                || s.eq_ignore_ascii_case("working")
                || s == crate::a2a::TASK_WORKING
        })
}

fn server_tool_line(kind: &str, payload: &Value) -> String {
    let query = server_query(payload);
    let short = canonical_server_tool(kind);
    if query.is_empty() {
        format!("▸ {short}")
    } else {
        format!("▸ {short}  {query}")
    }
}

fn server_query(payload: &Value) -> String {
    payload
        .pointer("/action/query")
        .or_else(|| payload.get("query"))
        .or_else(|| payload.pointer("/item/action/query"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn canonical_server_tool(kind: &str) -> String {
    let k = kind.to_ascii_lowercase();
    if k.contains("x_search") {
        "x_search".into()
    } else if k.contains("web_search") {
        "web_search".into()
    } else {
        server_tool_short(kind)
    }
}

fn server_phase(kind: &str, payload: &Value) -> String {
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
    } else if k.contains("completed") || k.ends_with(".done") || status == "completed" || status == "done"
    {
        "完成".into()
    } else if !status.is_empty() {
        status
    } else {
        "進行中".into()
    }
}

fn phase_is_done(phase: &str) -> bool {
    phase == "完成" || phase == "completed" || phase == "done"
}

fn live_tool_activity(name: &str, args: &Value, phase: &str) -> String {
    match name {
        "run_command" | "run_background" | "attach_monitor" => {
            let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
            format!("{phase}  $ {cmd}")
        }
        "timer" => {
            let secs = args
                .get("seconds")
                .map(|v| v.to_string())
                .unwrap_or_default();
            format!("{phase}  timer  {secs}s")
        }
        "write_file" | "edit_file" | "read_file" | "delete_file" | "list_dir" | "screenshot" | "read_image" => {
            format!("{phase}  {name}  {}", file_tool_target(name, args))
        }
        "project_memory" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("");
            let target = file_tool_target(name, args);
            if target.is_empty() || target == "." {
                format!("{phase}  project_memory  {action}")
            } else {
                format!("{phase}  project_memory  {action}  {target}")
            }
        }
        _ => format!("{phase}  {name}"),
    }
}

fn spinner(tick: u8) -> char {
    const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    FRAMES[(tick as usize) % FRAMES.len()]
}

fn server_tool_short(kind: &str) -> String {
    kind.rsplit('.')
        .next()
        .unwrap_or(kind)
        .trim_end_matches("_call")
        .trim_end_matches(".in_progress")
        .to_string()
}

fn parse_file_changes(output: &str) -> Vec<FileChange> {
    let Ok(v) = serde_json::from_str::<Value>(output) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut push_item = |item: &Value| {
        let path = item
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if path.is_empty() {
            return;
        }
        out.push(FileChange {
            path,
            kind: item
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("modify")
                .to_string(),
            diff: item
                .get("diff")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        });
    };
    if v.get("path").and_then(Value::as_str).is_some() && v.get("diff").is_some() {
        push_item(&v);
    } else if let Some(files) = v.get("files").and_then(Value::as_array) {
        for item in files {
            push_item(item);
        }
    }
    out
}

