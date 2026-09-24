//! Settings: provider connection, model catalogs, Grok login, and the live
//! knobs a running session reads each turn.

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::auth;
use crate::catalog::{clamp_effort_for_model, cycle_effort, EffortOpt, ModelCatalog};
use crate::config::{ProviderConfig, ProviderKind};
use crate::provider::ReasoningEffort;
use crate::skills::Skill;

use super::app::App;
use super::edit::Edit;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Field {
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

impl Field {
    /// Fields of each panel in Tab order.
    pub fn order(openai: bool) -> &'static [Field] {
        if openai {
            &[
                Field::Kind,
                Field::Endpoint,
                Field::ApiKey,
                Field::Model,
                Field::ChildModel,
                Field::Context,
                Field::Search,
                Field::Dispatcher,
                Field::ImportClaude,
                Field::ImportCodex,
                Field::Skills,
            ]
        } else {
            &[
                Field::Kind,
                Field::Account,
                Field::Model,
                Field::ChildModel,
                Field::Effort,
                Field::Search,
                Field::Dispatcher,
                Field::ImportClaude,
                Field::ImportCodex,
                Field::Skills,
            ]
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropKind {
    Model,
    ChildModel,
    Effort,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CatalogStatus {
    Idle,
    Loading,
    Ready,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LoginUi {
    Idle,
    Starting,
    Waiting { url: String, user_code: String },
    Failed(String),
}

impl LoginUi {
    pub fn in_flight(&self) -> bool {
        matches!(self, Self::Starting | Self::Waiting { .. })
    }
}

#[derive(Debug)]
pub(crate) enum LoginEvent {
    Waiting { gen: u64, url: String, user_code: String },
    Success { gen: u64 },
    Failed { gen: u64, message: String },
}

pub(crate) struct Settings {
    pub conn: ProviderConfig,
    pub auth_path: PathBuf,
    pub endpoint: Edit,
    pub api_key: Edit,
    pub model_edit: Edit,
    pub child_model_edit: Edit,
    pub context: Edit,
    pub field: Field,
    pub drop: Option<DropKind>,
    pub drop_cursor: usize,
    pub drop_scroll: usize,
    /// Merged Grok + custom catalog: what every picker reads.
    pub catalog: ModelCatalog,
    pub grok_catalog: ModelCatalog,
    pub custom_catalog: ModelCatalog,
    /// (base_url, api_key) the custom catalog was fetched for.
    pub custom_key: (String, String),
    pub custom_loading: bool,
    pub custom_err: Option<String>,
    pub catalog_status: CatalogStatus,
    pub want_catalog: bool,
    pub login: LoginUi,
    pub login_gen: u64,
    pub want_login: bool,
    /// xAI OAuth tokens exist on disk.
    pub xai_ready: bool,
    /// The route of the current model is usable.
    pub logged_in: bool,
    pub skill_list: Vec<Skill>,
    pub skill_cursor: usize,
    pub skill_scroll: usize,
}

impl Settings {
    pub fn new(conn: ProviderConfig, auth_path: PathBuf) -> Self {
        let xai_ready = auth::load_tokens(&auth_path).is_ok();
        let logged_in = conn.ready(xai_ready);
        Self {
            endpoint: Edit::at_end(conn.base_url.clone()),
            api_key: Edit::at_end(conn.api_key.clone()),
            model_edit: Edit::default(),
            child_model_edit: Edit::default(),
            context: Edit::at_end(if conn.context_window > 0 {
                crate::compact::format_window(conn.context_window)
            } else {
                String::new()
            }),
            conn,
            auth_path,
            field: Field::Kind,
            drop: None,
            drop_cursor: 0,
            drop_scroll: 0,
            catalog: ModelCatalog::default(),
            grok_catalog: ModelCatalog::default(),
            custom_catalog: ModelCatalog::default(),
            custom_key: (String::new(), String::new()),
            custom_loading: false,
            custom_err: None,
            catalog_status: CatalogStatus::Idle,
            want_catalog: xai_ready,
            login: LoginUi::Idle,
            login_gen: 0,
            want_login: false,
            xai_ready,
            logged_in,
            skill_list: Vec::new(),
            skill_cursor: 0,
            skill_scroll: 0,
        }
    }

    /// The open text field, if the focused field is typed.
    pub fn edit_mut(&mut self) -> Option<&mut Edit> {
        let typed_models = self.conn.kind.is_openai() && self.custom_catalog.models.is_empty();
        match self.field {
            Field::Endpoint => Some(&mut self.endpoint),
            Field::ApiKey => Some(&mut self.api_key),
            Field::Model if typed_models => Some(&mut self.model_edit),
            Field::ChildModel if typed_models => Some(&mut self.child_model_edit),
            Field::Context => Some(&mut self.context),
            _ => None,
        }
    }

    /// Model fields open a dropdown on the Grok panel always, and on the
    /// custom panel once the endpoint listed its models.
    pub fn model_picker(&self) -> bool {
        !self.conn.kind.is_openai() || !self.custom_catalog.models.is_empty()
    }
}

impl App {
    fn refresh_ready(&mut self) {
        let st = &mut self.settings;
        st.xai_ready = auth::load_tokens(&st.auth_path).is_ok();
        st.logged_in = st.conn.ready(st.xai_ready);
        // Logging in or out turns `grok_search` on or off for custom models.
        self.sync_knobs();
    }

    /// Push the chosen model / effort / tools into the running sessions.
    pub fn sync_knobs(&self) {
        let st = &self.settings;
        let model = if self.opts.model.trim().is_empty() {
            st.catalog
                .models
                .first()
                .map(|m| m.id.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "grok-4.6".into())
        } else {
            self.opts.model.clone()
        };
        // Search and reasoning passthrough exist on the xAI route only.
        let openai = st.conn.route_for(&model).is_openai();
        let choice = clamp_effort_for_model(&st.catalog, &model, self.opts.reasoning_effort);
        if let Ok(mut k) = self.knobs.lock() {
            k.model = model;
            k.reasoning_effort = choice.effort;
            k.send_reasoning = choice.send_reasoning && !openai;
            k.server_tools = crate::kit::search_tools(self.opts.web_search, openai, st.xai_ready);
            k.search = self.opts.web_search;
            k.dispatcher = self.opts.dispatcher;
            k.child_model = self.opts.child_model.trim().to_string();
        }
    }

    pub fn model_choices(&self) -> Vec<(String, String)> {
        let cat = &self.settings.catalog;
        if cat.models.is_empty() {
            let id = self.opts.model.trim();
            return if id.is_empty() {
                Vec::new()
            } else {
                vec![(id.to_string(), id.to_string())]
            };
        }
        cat.models.iter().map(|m| (m.id.clone(), m.name.clone())).collect()
    }

    pub fn child_model_choices(&self) -> Vec<(String, String)> {
        let mut out = vec![(String::new(), "跟隨主模型".to_string())];
        out.extend(self.model_choices());
        out
    }

    pub fn effort_choices(&self) -> Vec<EffortOpt> {
        let cat = &self.settings.catalog;
        let listed = cat.picker_efforts(&self.opts.model);
        if listed.is_empty() && cat.find(&self.opts.model).is_none() {
            let e = self.opts.reasoning_effort;
            return vec![EffortOpt {
                id: e.as_str().to_string(),
                value: e,
                label: e.label().to_string(),
                default: true,
            }];
        }
        listed
    }

    pub fn model_label(&self) -> String {
        self.settings
            .catalog
            .find(&self.opts.model)
            .map(|m| m.name.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.opts.model.clone())
    }

    pub fn child_model_label(&self) -> String {
        let id = self.opts.child_model.trim();
        if id.is_empty() {
            return "跟隨主模型".into();
        }
        self.settings
            .catalog
            .find(id)
            .map(|m| m.name.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| id.to_string())
    }

    pub fn effort_label(&self) -> String {
        self.effort_choices()
            .iter()
            .find(|e| e.value == self.opts.reasoning_effort)
            .map(|e| e.label.clone())
            .unwrap_or_else(|| self.opts.reasoning_effort.label().to_string())
    }

    pub fn select_model(&mut self, id: String) {
        self.opts.model = id.clone();
        self.settings.model_edit = Edit::at_end(id.clone());
        self.settings.conn.model = id;
        let choice = clamp_effort_for_model(&self.settings.catalog, &self.opts.model, self.opts.reasoning_effort);
        self.opts.reasoning_effort = choice.effort;
        // Routing follows the model: picking a custom model on the Grok panel
        // (or the reverse) does not flip the panel.
        let _ = self.settings.conn.save();
        self.rebuild_catalog();
        self.refresh_ready();
        self.sync_knobs();
    }

    pub fn select_child_model(&mut self, id: String) {
        self.opts.child_model = id.trim().to_string();
        self.settings.child_model_edit = Edit::at_end(self.opts.child_model.clone());
        self.sync_knobs();
    }

    pub fn select_effort(&mut self, effort: ReasoningEffort) {
        self.opts.reasoning_effort = effort;
        self.sync_knobs();
    }

    /// Whether Claude Code (`true`) or Codex skills are imported.
    pub fn skills_pref(&self, claude: bool) -> bool {
        let g = self.skills.lock().unwrap_or_else(|e| e.into_inner());
        if claude {
            g.prefs().import_claude
        } else {
            g.prefs().import_codex
        }
    }

    pub fn toggle_search(&mut self) {
        self.opts.web_search = !self.opts.web_search;
        self.sync_knobs();
    }

    pub fn toggle_dispatcher(&mut self) {
        self.opts.dispatcher = !self.opts.dispatcher;
        self.sync_knobs();
    }

    /// Copy the settings text fields into the saved connection.
    pub fn snapshot_conn(&mut self) {
        let st = &mut self.settings;
        st.conn.base_url = st.endpoint.text.trim().to_string();
        st.conn.api_key = st.api_key.text.trim().to_string();
        if st.conn.kind.is_openai() {
            let model = st.model_edit.text.trim();
            if !model.is_empty() {
                st.conn.model = model.to_string();
            }
            st.conn.context_window = crate::compact::parse_window(&st.context.text).unwrap_or(0);
        }
        let _ = st.conn.save();
        self.refresh_ready();
    }

    /// Commit settings edits into options and knobs.
    pub fn flush_conn(&mut self) {
        self.snapshot_conn();
        if self.settings.conn.kind.is_openai() {
            if !self.settings.conn.model.trim().is_empty() {
                self.opts.model = self.settings.conn.model.clone();
            }
            if self.settings.custom_catalog.models.is_empty() {
                // Typed while the endpoint has no model list.
                self.opts.child_model = self.settings.child_model_edit.text.trim().to_string();
            }
            self.rebuild_catalog();
        } else {
            self.settings.conn.model = self.opts.model.clone();
            let _ = self.settings.conn.save();
        }
        self.sync_knobs();
    }

    /// Before a run: a custom-route model persists the endpoint choice.
    pub fn adopt_route_for_model(&mut self) {
        if !self.settings.conn.route_for(&self.opts.model).is_openai() {
            return;
        }
        let st = &mut self.settings;
        if !self.opts.model.trim().is_empty() {
            st.conn.model = self.opts.model.clone();
        }
        if !st.conn.base_url.trim().is_empty() && !st.conn.kind.is_openai() {
            st.conn.kind = ProviderKind::Openai;
            let _ = st.conn.save();
        }
        self.refresh_ready();
    }

    pub fn not_ready_message(&self) -> String {
        let st = &self.settings;
        if st.conn.route_for(&self.opts.model).is_openai() {
            if st.conn.base_url.trim().is_empty() {
                let model = if self.opts.model.trim().is_empty() {
                    st.conn.effective_model().to_string()
                } else {
                    self.opts.model.clone()
                };
                ProviderConfig::missing_endpoint_error(&model)
            } else {
                "自訂 API 未就緒 — 請在設定填端點和模型名".into()
            }
        } else {
            "尚未登入 — 請在設定中登入 Grok 帳號，或改連自訂 API".into()
        }
    }

    /// Grok models first, then the custom endpoint's (tagged), then whatever
    /// was typed, so the current selection is always listed.
    pub fn rebuild_catalog(&mut self) {
        let st = &mut self.settings;
        let mut cat = st.grok_catalog.clone();
        for m in &st.custom_catalog.models {
            if cat.find(&m.id).is_some() {
                continue;
            }
            let mut m = m.clone();
            let base = if m.name.trim().is_empty() { m.id.clone() } else { m.name.clone() };
            m.name = format!("{base}（自訂）");
            cat.models.push(m);
        }
        cat.ensure_current(&self.opts.model, self.opts.reasoning_effort);
        let child = self.opts.child_model.trim();
        if !child.is_empty() {
            cat.ensure_current(child, self.opts.reasoning_effort);
        }
        st.catalog = cat;
    }

    pub fn ingest_catalog(&mut self, result: crate::error::Result<ModelCatalog>) {
        match result {
            Ok(cat) => {
                self.settings.grok_catalog = cat;
                self.settings.catalog_status = CatalogStatus::Ready;
                self.rebuild_catalog();
                let choice = clamp_effort_for_model(&self.settings.catalog, &self.opts.model, self.opts.reasoning_effort);
                self.opts.reasoning_effort = choice.effort;
                self.sync_knobs();
            }
            Err(e) => {
                self.rebuild_catalog();
                self.settings.catalog_status = CatalogStatus::Failed(e.to_string().chars().take(80).collect());
            }
        }
    }

    pub fn ingest_custom_catalog(&mut self, result: crate::error::Result<ModelCatalog>) {
        let st = &mut self.settings;
        st.custom_loading = false;
        match result {
            Ok(cat) => {
                st.custom_err = None;
                st.custom_catalog = cat;
            }
            Err(e) => {
                // No /models: the model field stays hand-typed.
                st.custom_err = Some(e.to_string().chars().take(80).collect());
                st.custom_catalog = ModelCatalog::default();
            }
        }
        self.rebuild_catalog();
        self.sync_knobs();
    }

    pub fn set_provider_kind(&mut self, kind: ProviderKind) {
        self.flush_conn();
        self.settings.conn.kind = kind;
        if kind.is_openai() {
            let st = &mut self.settings;
            if st.model_edit.text.trim().is_empty() {
                st.model_edit = Edit::at_end(self.opts.model.clone());
            }
            if st.conn.model.trim().is_empty() {
                st.conn.model = self.opts.model.clone();
            }
            st.want_catalog = st.xai_ready;
            st.field = Field::Endpoint;
        } else {
            // Back on Grok: default to a Grok model; the merged dropdown still
            // offers the endpoint's models.
            if !ProviderConfig::looks_like_grok(&self.opts.model) {
                self.opts.model = self
                    .settings
                    .catalog
                    .models
                    .iter()
                    .map(|m| m.id.as_str())
                    .find(|id| ProviderConfig::looks_like_grok(id))
                    .unwrap_or("grok-4.6")
                    .to_string();
            }
            let st = &mut self.settings;
            st.conn.model = self.opts.model.clone();
            st.model_edit = Edit::at_end(self.opts.model.clone());
            st.want_catalog = true;
            st.catalog_status = CatalogStatus::Idle;
            st.field = Field::Account;
        }
        self.rebuild_catalog();
        let _ = self.settings.conn.save();
        self.refresh_ready();
        self.sync_knobs();
    }

    /// Tab: next field of the current panel.
    pub fn cycle_field(&mut self, back: bool) {
        self.flush_conn();
        let order = Field::order(self.settings.conn.kind.is_openai());
        let i = order.iter().position(|f| *f == self.settings.field).unwrap_or(0) as i32;
        let n = order.len() as i32;
        self.settings.field = order[(i + if back { -1 } else { 1 }).rem_euclid(n) as usize];
        self.settings.drop = None;
    }

    pub fn drop_len(&self) -> usize {
        match self.settings.drop {
            Some(DropKind::Model) => self.model_choices().len(),
            Some(DropKind::ChildModel) => self.child_model_choices().len(),
            Some(DropKind::Effort) => self.effort_choices().len(),
            None => 0,
        }
    }

    pub fn open_drop(&mut self, kind: DropKind) {
        let cursor = match kind {
            DropKind::Model => self.model_choices().iter().position(|(id, _)| *id == self.opts.model),
            DropKind::ChildModel => self
                .child_model_choices()
                .iter()
                .position(|(id, _)| id == self.opts.child_model.trim()),
            DropKind::Effort => self
                .effort_choices()
                .iter()
                .position(|e| e.value == self.opts.reasoning_effort),
        };
        self.settings.drop = Some(kind);
        if self.drop_len() == 0 {
            self.settings.drop = None;
            return;
        }
        self.settings.drop_cursor = cursor.unwrap_or(0);
        self.settings.drop_scroll = 0;
        self.reveal_drop();
    }

    pub fn reveal_drop(&mut self) {
        let vis = super::theme::DROP_VISIBLE.min(self.drop_len().max(1));
        let st = &mut self.settings;
        if st.drop_cursor < st.drop_scroll {
            st.drop_scroll = st.drop_cursor;
        } else if st.drop_cursor >= st.drop_scroll + vis {
            st.drop_scroll = st.drop_cursor + 1 - vis;
        }
    }

    pub fn move_drop(&mut self, delta: i32) {
        let len = self.drop_len();
        if len == 0 {
            return;
        }
        self.settings.drop_cursor = (self.settings.drop_cursor as i32 + delta).clamp(0, len as i32 - 1) as usize;
        self.reveal_drop();
    }

    pub fn pick_drop(&mut self, index: usize) {
        match self.settings.drop {
            Some(DropKind::Model) => {
                if let Some((id, _)) = self.model_choices().get(index).cloned() {
                    self.select_model(id);
                }
            }
            Some(DropKind::ChildModel) => {
                if let Some((id, _)) = self.child_model_choices().get(index).cloned() {
                    self.select_child_model(id);
                }
            }
            Some(DropKind::Effort) => {
                if let Some(e) = self.effort_choices().get(index).cloned() {
                    self.select_effort(e.value);
                }
            }
            None => {}
        }
        self.settings.drop = None;
    }

    pub fn cycle_effort(&mut self, back: bool) {
        if let Some(e) = cycle_effort(&self.effort_choices(), self.opts.reasoning_effort, back) {
            self.select_effort(e);
        }
    }

    // —— Grok login ——

    pub fn activate_account(&mut self) {
        if self.settings.login.in_flight() {
            self.cancel_login();
        } else if self.settings.xai_ready {
            self.logout();
        } else {
            self.begin_login();
        }
    }

    fn begin_login(&mut self) {
        if self.settings.login.in_flight() {
            return;
        }
        if auth::load_tokens(&self.settings.auth_path).is_ok() {
            self.login_success();
            return;
        }
        let st = &mut self.settings;
        st.login_gen = st.login_gen.wrapping_add(1);
        st.login = LoginUi::Starting;
        st.want_login = true;
        self.flash("正在開始 Grok 登入…");
    }

    pub fn cancel_login(&mut self) {
        let st = &mut self.settings;
        if !st.login.in_flight() && !st.want_login {
            return;
        }
        st.login_gen = st.login_gen.wrapping_add(1);
        st.want_login = false;
        st.login = LoginUi::Idle;
        self.flash("已取消登入");
    }

    pub fn logout(&mut self) {
        let st = &mut self.settings;
        st.login_gen = st.login_gen.wrapping_add(1);
        st.want_login = false;
        st.login = LoginUi::Idle;
        if let Err(e) = auth::delete_auth_file(&st.auth_path) {
            st.login = LoginUi::Failed(e.to_string());
            return;
        }
        self.refresh_ready();
        if !self.settings.xai_ready {
            self.settings.want_catalog = false;
            self.settings.grok_catalog = ModelCatalog::default();
            self.rebuild_catalog();
        }
        self.flash("已登出");
    }

    fn login_success(&mut self) {
        let st = &mut self.settings;
        st.login = LoginUi::Idle;
        st.want_login = false;
        st.want_catalog = true;
        self.refresh_ready();
        self.flash("已登入 Grok");
    }

    pub fn apply_login_event(&mut self, ev: LoginEvent) {
        let gen = match &ev {
            LoginEvent::Waiting { gen, .. } | LoginEvent::Success { gen } | LoginEvent::Failed { gen, .. } => *gen,
        };
        if gen != self.settings.login_gen {
            return;
        }
        match ev {
            LoginEvent::Waiting { url, user_code, .. } => {
                self.settings.login = LoginUi::Waiting { url, user_code };
                self.flash("請在瀏覽器核准 Grok 登入");
            }
            LoginEvent::Success { .. } => {
                self.login_success();
                self.cur_mut()
                    .chat
                    .push(super::model::rows::Row::meta("已登入 Grok 帳號"));
            }
            LoginEvent::Failed { message, .. } => {
                self.settings.login = LoginUi::Failed(message.chars().take(80).collect());
                self.flash("Grok 登入失敗");
            }
        }
    }
}

/// Device-code login in the background; progress arrives as [`LoginEvent`]s.
pub(crate) async fn run_login(path: PathBuf, gen: u64, tx: mpsc::UnboundedSender<LoginEvent>) {
    let fail = |message: String| LoginEvent::Failed { gen, message };
    let mut pending = match auth::request_device_login().await {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(fail(e.to_string()));
            return;
        }
    };
    auth::open_login_browser(&pending.open_url);
    let _ = tx.send(LoginEvent::Waiting {
        gen,
        url: pending.open_url.clone(),
        user_code: pending.user_code.clone(),
    });
    let deadline = std::time::SystemTime::now() + Duration::from_secs(pending.expires_in);
    loop {
        if std::time::SystemTime::now() >= deadline {
            let _ = tx.send(fail("登入已逾時".into()));
            return;
        }
        tokio::time::sleep(pending.interval()).await;
        let msg = match auth::poll_device_login(&pending).await {
            Ok(auth::DevicePoll::Pending) => continue,
            Ok(auth::DevicePoll::SlowDown) => {
                pending.bump_interval();
                continue;
            }
            Ok(auth::DevicePoll::Success(tokens)) => match auth::save_tokens(&path, &tokens) {
                Ok(()) => LoginEvent::Success { gen },
                Err(e) => fail(e.to_string()),
            },
            Ok(auth::DevicePoll::Denied) => fail("瀏覽器拒絕登入".into()),
            Ok(auth::DevicePoll::Expired) => fail("登入代碼已過期".into()),
            Ok(auth::DevicePoll::Failed(message)) => fail(message),
            Err(e) => fail(e.to_string()),
        };
        let _ = tx.send(msg);
        return;
    }
}
