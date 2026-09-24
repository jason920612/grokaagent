#[derive(Clone, Serialize, Deserialize)]
struct FileChange {
    path: String,
    kind: String,
    diff: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct ToolCall {
    name: String,
    args: Value,
    output: String,
    files: Vec<FileChange>,
    done: bool,
    phase: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct ToolGroup {
    calls: Vec<ToolCall>,
    expanded: bool,
}

#[derive(Clone, Serialize, Deserialize, Default)]
struct Think {
    text: String,
    expanded: bool,
    done: bool,
    #[serde(default)]
    elapsed_ms: u64,
    #[serde(skip)]
    started: Option<Instant>,
}

#[derive(Clone)]
struct UserMsg {
    text: String,
    images: Vec<String>,
}

impl From<&str> for UserMsg {
    fn from(s: &str) -> Self {
        Self {
            text: s.to_string(),
            images: Vec::new(),
        }
    }
}

impl From<String> for UserMsg {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

impl serde::Serialize for UserMsg {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        if self.images.is_empty() {
            serializer.serialize_str(&self.text)
        } else {
            use serde::ser::SerializeStruct;
            let mut st = serializer.serialize_struct("UserMsg", 2)?;
            st.serialize_field("text", &self.text)?;
            st.serialize_field("images", &self.images)?;
            st.end()
        }
    }
}

impl<'de> serde::Deserialize<'de> for UserMsg {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum De {
            Text(String),
            Full {
                text: String,
                #[serde(default)]
                images: Vec<String>,
            },
        }
        match De::deserialize(deserializer)? {
            De::Text(text) => Ok(Self {
                text,
                images: Vec::new(),
            }),
            De::Full { text, images } => Ok(Self { text, images }),
        }
    }
}

#[derive(Clone)]
struct AgentMsg {
    text: String,
    work_ms: u64,
}

impl AgentMsg {
    fn new(text: String) -> Self {
        Self { text, work_ms: 0 }
    }
}

impl serde::Serialize for AgentMsg {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        if self.work_ms == 0 {
            serializer.serialize_str(&self.text)
        } else {
            use serde::ser::SerializeStruct;
            let mut st = serializer.serialize_struct("AgentMsg", 2)?;
            st.serialize_field("text", &self.text)?;
            st.serialize_field("work_ms", &self.work_ms)?;
            st.end()
        }
    }
}

impl<'de> serde::Deserialize<'de> for AgentMsg {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum De {
            Text(String),
            Full {
                text: String,
                #[serde(default)]
                work_ms: u64,
            },
        }
        match De::deserialize(deserializer)? {
            De::Text(text) => Ok(Self { text, work_ms: 0 }),
            De::Full { text, work_ms } => Ok(Self { text, work_ms }),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
enum Row {
    User(UserMsg),
    Agent(AgentMsg),
    Tools(ToolGroup),
    Think(Think),
    Meta(String),
    Err(String),
    Picture { path: String, label: String },
}

#[derive(Clone)]
pub struct TuiOptions {
    pub model: String,
    pub events: PathBuf,
    pub workspace: PathBuf,
    pub max_turns: u32,
    pub web_search: bool,
    pub dispatcher: bool,
    /// Default model for spawned child agents. Empty = follow the main model.
    pub child_model: String,
    pub reasoning_effort: ReasoningEffort,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SendMode {
    Queue,
    Insert,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Submit {
    Start,
    Queue,
    Insert,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Chat,
    Settings,
    Rename,
    Inspector,
    Ask,
    Workspace,
    Task,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SettingField {
    Kind,
    Account,
    Endpoint,
    ApiKey,
    Model,
    ChildModel,
    Context,
    Effort,
    Search,
    Dispatcher,
    ImportClaude,
    ImportCodex,
    Skills,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropKind {
    Model,
    ChildModel,
    Effort,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CatalogStatus {
    Idle,
    Loading,
    Ready,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LoginUi {
    Idle,
    Starting,
    Waiting { url: String, user_code: String },
    Failed(String),
}

#[derive(Debug)]
enum LoginEvent {
    Waiting {
        gen: u64,
        url: String,
        user_code: String,
    },
    Success {
        gen: u64,
    },
    Failed {
        gen: u64,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    Gear,
    TaskChip,
    TaskConfirm,
    TaskCancel,
    TaskEnd,
    TaskPanel,
    TaskDraft,
    ModelChip,
    QueueChip,
    InsertChip,
    Chat,
    Composer,
    Title,
    Close,
    Min,
    Max,
    SettingModel,
    SettingChildModel,
    SettingEffort,
    SettingEndpoint,
    SettingApiKey,
    SettingContext,
    ProviderXai,
    ProviderOpenai,
    CatalogPick(u16),
    Search,
    Dispatcher,
    ImportClaude,
    ImportCodex,
    SkillToggle(u16),
    SkillRow(u16),
    SkillList,
    SkillView,
    SkillViewClose,
    SkillViewDismiss,
    AccountBtn,
    LoginCode,
    Dock,
    ToolGroup(usize),
    ToolItem(usize, usize),
    Think(usize),
    ToolPanel,
    ToolPanelClose,
    DismissTool,
    NewChat,
    Session(u16),
    RenameSession(u16),
    DeleteSession(u16),
    QueueItem(u16),
    CancelQueueEdit,
    RailChild(u16),
    RailMon(u16),
    RailBg(u16),
    Inspector,
    InspectorClose,
    ChatRow(u16),
    ChatImage(u16),
    JumpBottom,
    ScrollBar,
    ScrollThumb,
    ImageView,
    ImageViewClose,
    ImageViewDismiss,
    PasteImage,
    PendingClose(u16),
    AskOption(u16),
    AskConfirm,
    AskCancel,
    AskFill,
    AskPanel,
    WsPanel,
    WsPath,
    WsEntry(u16),
    WsConfirm,
    WsCreate,
    WsCancel,
}

fn submit_kind(has_session: bool, running: bool, mode: SendMode) -> Submit {
    if !has_session {
        Submit::Start
    } else if running && mode == SendMode::Queue {
        Submit::Queue
    } else {
        Submit::Insert
    }
}

fn ch_width(c: char) -> u16 {
    Line::from(c.to_string()).width() as u16
}

fn display_cols(s: &str) -> u16 {
    Line::from(s).width() as u16
}

#[cfg(test)]
fn caret_in(inner: Rect, text: &str, caret: usize) -> Position {
    let prefix: String = text.chars().take(caret).collect();
    let w = Line::from(prefix.as_str()).width() as u16;
    let max = inner.width.saturating_sub(1);
    Position::new(inner.x.saturating_add(w.min(max)), inner.y)
}
