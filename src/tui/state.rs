#[derive(Clone, Copy)]
struct Win {
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    maximized: bool,
    minimized: bool,
}

#[derive(Clone)]
struct SideMon {
    name: String,
    command: String,
    pid: u32,
    status: String,
    alive: bool,
    detail: String,
}

#[derive(Clone)]
struct SideBg {
    name: String,
    command: String,
    pid: u32,
    status: String,
    alive: bool,
    detail: String,
    log: Vec<String>,
}

impl SideBg {
    fn push_log(&mut self, line: String) {
        if self.log.len() > 400 {
            self.log.drain(0..150);
        }
        self.log.push(line);
    }
}

#[derive(Clone, Default)]
struct Queued {
    text: String,
    images: Vec<String>,
}

impl From<&str> for Queued {
    fn from(s: &str) -> Self {
        Self {
            text: s.to_string(),
            images: Vec::new(),
        }
    }
}

impl From<String> for Queued {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

impl Queued {
    fn label(&self) -> String {
        let t = self.text.replace('\n', " ");
        if self.images.is_empty() {
            t
        } else if t.is_empty() {
            format!("[{n} 張圖片]", n = self.images.len())
        } else {
            format!("{t}  [{n}圖]", n = self.images.len())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ChatPos {
    row: usize,
    idx: usize,
}

#[derive(Clone, Default, PartialEq, Eq, Debug)]
enum ChatSel {
    #[default]
    None,
    Text { anchor: ChatPos, caret: ChatPos },
    Image(String),
}

impl ChatSel {
    fn text_range(&self) -> Option<(ChatPos, ChatPos)> {
        match *self {
            ChatSel::Text { anchor, caret } if anchor != caret => {
                if anchor <= caret {
                    Some((anchor, caret))
                } else {
                    Some((caret, anchor))
                }
            }
            _ => None,
        }
    }
}

/// Visible glyph run of a selectable chat line, in screen cells.
struct ChatGlyphLine {
    y: u16,
    x: u16,
    text_w: u16,
    row: usize,
    start: usize,
    chars: Vec<char>,
}

struct SkillView {
    title: String,
    origin: String,
    edit: Edit,
    scroll: u16,
    inner: Rect,
    dragging: bool,
}

#[derive(Clone)]
enum Inspector {
    Monitor(String),
    Background(String),
}

struct ParkedChat {
    session: SessionMeta,
    rows: Vec<Row>,
    status: String,
    cache: String,
    child_count: u32,
    running: bool,
    awaiting: bool,
    scroll: u16,
    stick_bottom: bool,
    queue: VecDeque<Queued>,
    inbox_tx: Option<mpsc::UnboundedSender<UserTurn>>,
    cancel: Option<CancelFlag>,
    streaming: bool,
    open_tool: Option<(usize, usize)>,
    seal_tools: bool,
    activity: String,
    edit: Edit,
    work_started: Option<Instant>,
    queue_edit: Option<usize>,
    composer_stash: Option<Edit>,
    pending: Vec<String>,
    bench: Workbench,
    monitors: Vec<SideMon>,
    backgrounds: Vec<SideBg>,
    inspector: Option<Inspector>,
    inspector_scroll: u16,
    task: Arc<TaskHub>,
}

struct AskState {
    question: Question,
    cursor: usize,
    chosen: Vec<bool>,
    values: Vec<String>,
    filling: bool,
    fill_edit: Edit,
    fill_scroll: u16,
}

impl AskState {
    fn new(question: Question) -> Self {
        let n = question.options.len();
        Self {
            question,
            cursor: 0,
            chosen: vec![false; n],
            values: vec![String::new(); n],
            filling: false,
            fill_edit: Edit::default(),
            fill_scroll: 0,
        }
    }

    fn n(&self) -> usize {
        self.question.options.len()
    }

    fn move_cursor(&mut self, delta: i16) {
        self.save_fill();
        let n = self.n() as i16;
        if n == 0 {
            return;
        }
        let next = (self.cursor as i16 + delta).rem_euclid(n);
        self.cursor = next as usize;
    }

    fn save_fill(&mut self) {
        if self.filling {
            let t: String = self.fill_edit.text.chars().take(ask::MAX_INPUT).collect();
            if let Some(slot) = self.values.get_mut(self.cursor) {
                *slot = t;
            }
            self.filling = false;
        }
    }

    fn enter_fill(&mut self) {
        let Some(opt) = self.question.options.get(self.cursor) else {
            return;
        };
        if !opt.input {
            return;
        }
        if !self.question.allow_multiple {
            self.chosen.fill(false);
            if let Some(c) = self.chosen.get_mut(self.cursor) {
                *c = true;
            }
        } else if let Some(c) = self.chosen.get_mut(self.cursor) {
            *c = true;
        }
        self.filling = true;
        self.fill_edit = Edit::at_end(self.values.get(self.cursor).cloned().unwrap_or_default());
        self.fill_scroll = 0;
    }

    fn mark_cursor(&mut self) {
        if self.question.allow_multiple {
            if let Some(c) = self.chosen.get_mut(self.cursor) {
                *c = !*c;
            }
        } else {
            self.chosen.fill(false);
            if let Some(c) = self.chosen.get_mut(self.cursor) {
                *c = true;
            }
        }
    }

    fn summary(&self) -> String {
        let picks: Vec<String> = self
            .question
            .options
            .iter()
            .enumerate()
            .filter(|(i, _)| self.chosen.get(*i).copied().unwrap_or(false))
            .map(|(i, o)| {
                if o.input {
                    format!("{}: {}", o.label, self.values.get(i).cloned().unwrap_or_default())
                } else {
                    o.label.clone()
                }
            })
            .collect();
        if picks.is_empty() {
            "已取消問卷".into()
        } else {
            format!("你選了  {}", picks.join("、"))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WsFocus {
    Path,
    List,
}

enum TaskUi {
    Form { edit: Edit },
    Status,
}

#[derive(Clone, Copy)]
enum TaskAction {
    Submit,
    Close,
    End,
}

struct WorkspacePick {
    view: FolderView,
    edit: Edit,
    cursor: usize,
    scroll: u16,
    path_scroll: u16,
    focus: WsFocus,
    path_inner: Rect,
    list_area: Rect,
    notice: Option<String>,
}

impl WorkspacePick {
    fn open(start: &std::path::Path) -> Self {
        let view = folderpick::list_folder(&folderpick::existing_dir(start));
        let edit = Edit::at_end(folderpick::display_path(&view.cwd));
        Self {
            view,
            edit,
            cursor: 0,
            scroll: 0,
            path_scroll: 0,
            focus: WsFocus::Path,
            path_inner: Rect::default(),
            list_area: Rect::default(),
            notice: None,
        }
    }

    fn selected(&self) -> Option<&folderpick::Entry> {
        self.view.entries.get(self.cursor)
    }

    fn reveal(&mut self, vis: u16) {
        if vis == 0 {
            return;
        }
        if self.cursor < self.scroll as usize {
            self.scroll = self.cursor as u16;
        } else if self.cursor >= self.scroll as usize + vis as usize {
            self.scroll = (self.cursor + 1 - vis as usize) as u16;
        }
    }
}

