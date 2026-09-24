fn intro_rows() -> Vec<Row> {
    vec![
        Row::Meta("Cursor 風格聊天。工作中可排隊或直接插入下一輪。".into()),
        Row::Meta("點訊息或圖片後 Ctrl-C 複製 · 無選取則離開  ·  Ctrl-N 新對話 · 點「貼上圖片」附圖".into()),
    ]
}

struct BootSession {
    session: SessionMeta,
    rows: Vec<Row>,
    created: bool,
}

fn load_session_rows(store: Option<&SessionStore>, id: &str) -> Vec<Row> {
    store
        .and_then(|s| s.load_transcript(id).ok())
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

fn session_has_chat_content(store: Option<&SessionStore>, meta: &SessionMeta) -> bool {
    if meta.named || meta.name_is_manual {
        return true;
    }
    load_session_rows(store, &meta.id)
        .iter()
        .any(|r| !matches!(r, Row::Meta(_)))
}

/// Resume the most recently updated real chat. Blank drafts left by earlier
/// launches are skipped so opening the TUI does not keep minting 新對話.
fn pick_resume_session(store: Option<&SessionStore>, listed: &[SessionMeta]) -> Option<SessionMeta> {
    if listed.is_empty() {
        return None;
    }
    listed
        .iter()
        .find(|m| session_has_chat_content(store, m))
        .cloned()
        .or_else(|| listed.first().cloned())
}

fn boot_session(
    store: Option<&SessionStore>,
    listed: &[SessionMeta],
    workspace: PathBuf,
) -> BootSession {
    if let Some(session) = pick_resume_session(store, listed) {
        let rows = load_session_rows(store, &session.id);
        let rows = if rows.is_empty() {
            intro_rows()
        } else {
            rows
        };
        return BootSession {
            session,
            rows,
            created: false,
        };
    }
    let session = match store {
        Some(s) => s
            .create(workspace.clone())
            .unwrap_or_else(|_| SessionMeta::new(workspace)),
        None => SessionMeta::new(workspace),
    };
    BootSession {
        session,
        rows: intro_rows(),
        created: true,
    }
}

fn fresh_chat(session: SessionMeta) -> ParkedChat {
    let task = TaskHub::new(session.id.clone());
    ParkedChat {
        session,
        rows: intro_rows(),
        status: "待命".into(),
        cache: "cache —".into(),
        child_count: 0,
        running: false,
        awaiting: false,
        scroll: 0,
        stick_bottom: true,
        queue: VecDeque::new(),
        inbox_tx: None,
        cancel: None,
        streaming: false,
        open_tool: None,
        seal_tools: false,
        activity: String::new(),
        edit: Edit::default(),
        work_started: None,
        queue_edit: None,
        composer_stash: None,
            pending: Vec::new(),
            bench: Workbench::default(),
            monitors: Vec::new(),
            backgrounds: Vec::new(),
            inspector: None,
            inspector_scroll: 0,
            task,
    }
}

#[cfg(test)]
fn dummy_session() -> SessionMeta {
    let mut m = SessionMeta::new(PathBuf::from("."));
    m.id = "s".into();
    m
}

