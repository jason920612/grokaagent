use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};

use crate::auth;
use crate::error::{Error, Result};
use crate::tools::ToolSpec;

const DEFAULT_BASE: &str = "https://cli-chat-proxy.grok.com/v1";
const DEFAULT_MODEL: &str = "grok-4.6";
const GROK_CLIENT_VERSION_DEFAULT: &str = "0.2.103";

/// How hard grok-4.6 thinks. Cannot be turned off; default is high.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    Xhigh,
}

impl ReasoningEffort {
    pub const DEFAULT: Self = Self::High;

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "low · 快",
            Self::Medium => "medium",
            Self::High => "high · 預設",
            Self::Xhigh => "xhigh · 最深",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" | "l" => Some(Self::Low),
            "medium" | "med" | "m" => Some(Self::Medium),
            "high" | "h" => Some(Self::High),
            "xhigh" | "x-high" | "max" | "x" => Some(Self::Xhigh),
            _ => None,
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Xhigh,
            Self::Xhigh => Self::Low,
        }
    }

    pub fn cycle_back(self) -> Self {
        match self {
            Self::Low => Self::Xhigh,
            Self::Medium => Self::Low,
            Self::High => Self::Medium,
            Self::Xhigh => Self::High,
        }
    }
}

impl Default for ReasoningEffort {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Debug, Clone)]
pub struct FunctionCall {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub struct CompleteRequest {
    pub instructions: String,
    pub input: Vec<Value>,
    pub client_tools: Vec<ToolSpec>,
    pub server_tools: Vec<String>,
    /// Sticky routing id. Must be stable for one conversation (run id), or
    /// cache hits stay near zero. Do not share this across unrelated chats.
    pub cache_key: String,
    /// Continue a stored Responses chain. Working chat turns leave this unset
    /// and resend the full append-only `input` so the prefix can cache.
    pub previous_response_id: Option<String>,
    /// When true the server keeps the response for `previous_response_id`.
    /// Working chat turns do not chain, so they send `store: false`.
    pub store: bool,
    pub reasoning_effort: ReasoningEffort,
    /// When false, omit `reasoning` — the selected model does not support it.
    pub send_reasoning: bool,
    /// Empty means the provider's default model.
    pub model: String,
    /// When set, force a single function call by name (independent reviewers).
    pub tool_choice: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CompactRequest {
    pub input: Vec<Value>,
    pub cache_key: String,
}

#[derive(Debug, Clone)]
pub struct CompactResponse {
    pub item: Value,
    pub dropped_message_count: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CacheUsage {
    pub input_tokens: u32,
    pub cached_tokens: u32,
}

/// Warn below this rate on turn 2+, when the request is large enough to have a prefix.
pub const CACHE_HIT_TARGET: f32 = 0.90;
const CACHE_WARN_MIN_INPUT: u32 = 200;

impl CacheUsage {
    pub fn rate(&self) -> f32 {
        if self.input_tokens == 0 {
            0.0
        } else {
            self.cached_tokens as f32 / self.input_tokens as f32
        }
    }

    /// Turn 1 may be a cold fill (`cached_tokens = 0`). Later turns should be ≥90%.
    pub fn below_target(&self, turn: u32) -> bool {
        turn >= 2
            && self.input_tokens >= CACHE_WARN_MIN_INPUT
            && self.rate() < CACHE_HIT_TARGET
    }
}

#[derive(Debug, Clone)]
pub struct CompleteResponse {
    pub id: String,
    pub text: String,
    pub function_calls: Vec<FunctionCall>,
    pub server_items: Vec<Value>,
    pub usage: CacheUsage,
    /// Exact Responses `output` items. Replay these unchanged; do not rebuild them.
    pub output_items: Vec<Value>,
}

impl CompleteResponse {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            text: String::new(),
            function_calls: vec![],
            server_items: vec![],
            usage: CacheUsage::default(),
            output_items: vec![],
        }
    }
}

pub trait Provider: Send + Sync {
    fn complete(
        &self,
        req: CompleteRequest,
    ) -> impl std::future::Future<Output = Result<CompleteResponse>> + Send;

    /// Streaming complete. Default ignores callbacks and calls [`complete`].
    fn complete_stream<'a>(
        &'a self,
        req: CompleteRequest,
        _on_text: &'a (dyn Fn(&str) + Send + Sync),
        _on_server: &'a (dyn Fn(&str, &Value) + Send + Sync),
        _on_reasoning: &'a (dyn Fn(&str) + Send + Sync),
    ) -> impl std::future::Future<Output = Result<CompleteResponse>> + Send + 'a {
        async move { self.complete(req).await }
    }

    fn compact(
        &self,
        req: CompactRequest,
    ) -> impl std::future::Future<Output = Result<CompactResponse>> + Send;
}

#[derive(Clone)]
pub struct XaiOauthProvider {
    auth_path: PathBuf,
    base_url: String,
    model: String,
    client: reqwest::Client,
}

fn require_https_base(url: &str) -> Result<String> {
    if !url.starts_with("https://") {
        return Err(Error::Provider("GROKA_XAI_BASE_URL must be https".into()));
    }
    Ok(url.to_string())
}

impl XaiOauthProvider {
    pub fn new(auth_path: PathBuf, model: Option<String>) -> Result<Self> {
        let base_url = require_https_base(
            &std::env::var("GROKA_XAI_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE.into()),
        )?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self {
            auth_path,
            base_url,
            model: model.unwrap_or_else(|| DEFAULT_MODEL.into()),
            client,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Fetch the live CLI model catalog. Does not send conversation / request ids.
    pub async fn list_models(&self) -> Result<crate::catalog::ModelCatalog> {
        let override_url = crate::catalog::models_list_override_from_env();
        if let Some(url) = override_url.as_deref() {
            if !url.starts_with("https://") {
                return Err(Error::Provider(
                    "GROKA_MODELS_LIST_URL / GROK_MODELS_LIST_URL must be https".into(),
                ));
            }
        }
        let urls = crate::catalog::catalog_urls(&self.base_url, override_url.as_deref());
        let mut last = Error::Provider("model catalog fetch failed".into());
        for url in urls {
            match self.grok_catalog_get(&url).await {
                Ok((200, text)) => {
                    return crate::catalog::parse_catalog_json(&text).map_err(Error::Provider);
                }
                Ok((status, text)) => {
                    last = entitlement_error(status, &text);
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    async fn grok_catalog_get(&self, url: &str) -> Result<(u16, String)> {
        let token = auth::valid_access_token(&self.auth_path).await?;
        let resp = self
            .client
            .get(url)
            .bearer_auth(&token)
            .header("Accept", "application/json")
            .header("X-XAI-Token-Auth", "xai-grok-cli")
            .header("x-authenticateresponse", "authenticate-response")
            .header("x-grok-client-identifier", "grok-shell")
            .header("x-grok-client-version", grok_client_version())
            .header("x-grok-client-mode", grok_client_mode())
            .header("User-Agent", "xai-grok-cli")
            .send()
            .await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        Ok((status, text))
    }

    fn grok_request(
        &self,
        path: &str,
        cache_key: &str,
        model: &str,
        token: &str,
        accept: &str,
    ) -> reqwest::RequestBuilder {
        let req_id = uuid::Uuid::new_v4().to_string();
        let url = format!("{}{path}", self.base_url.trim_end_matches('/'));
        self.client
            .post(&url)
            .bearer_auth(token)
            .header("Content-Type", "application/json")
            .header("Accept", accept)
            .header("X-XAI-Token-Auth", "xai-grok-cli")
            .header("x-authenticateresponse", "authenticate-response")
            .header("x-grok-client-identifier", "grok-shell")
            .header("x-grok-client-version", grok_client_version())
            .header("x-grok-client-mode", grok_client_mode())
            .header("x-grok-model-override", model)
            .header("x-grok-conv-id", cache_key)
            .header("x-grok-session-id", cache_key)
            .header("x-grok-req-id", &req_id)
            .header("User-Agent", "xai-grok-cli")
    }

    /// Grok CLI names sessions with a forced `session_title` tool call on the
    /// same Responses API — there is no separate naming endpoint.
    pub async fn generate_session_title(&self, user_message: &str) -> String {
        let source = crate::session::title_source_text(user_message);
        let model = self.model.as_str();
        let payload = session_title_payload(model, &source);
        match self
            .grok_post("/responses", "grokaagent:title:v1", &payload, model)
            .await
        {
            Ok((200, text)) => {
                if let Ok(body) = serde_json::from_str::<Value>(&text) {
                    if let Ok(resp) = parse_response_body(&body) {
                        if let Some(title) = crate::session::title_from_response(&resp) {
                            return title;
                        }
                    }
                }
            }
            _ => {}
        }
        crate::session::title_fallback_from_user_text(&source)
    }

    async fn grok_post(
        &self,
        path: &str,
        cache_key: &str,
        payload: &Value,
        model: &str,
    ) -> Result<(u16, String)> {
        let token = auth::valid_access_token(&self.auth_path).await?;
        let resp = self
            .grok_request(path, cache_key, model, &token, "application/json")
            .json(payload)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        Ok((status, text))
    }

    async fn grok_post_sse(
        &self,
        cache_key: &str,
        payload: &Value,
        model: &str,
        on_text: &(dyn Fn(&str) + Send + Sync),
        on_server: &(dyn Fn(&str, &Value) + Send + Sync),
        on_reasoning: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<CompleteResponse> {
        let token = auth::valid_access_token(&self.auth_path).await?;
        let resp = self
            .grok_request(
                "/responses",
                cache_key,
                model,
                &token,
                "text/event-stream",
            )
            .timeout(Duration::from_secs(600))
            .json(payload)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if status != 200 {
            let text = resp.text().await?;
            return Err(entitlement_error(status, &text));
        }
        if !ctype.contains("event-stream") {
            let text = resp.text().await?;
            let body: Value = serde_json::from_str(&text)
                .map_err(|_| Error::Provider("xAI returned non-JSON body".into()))?;
            return parse_response_body(&body);
        }
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut parser = SseParser::new();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(180), stream.next()).await;
            let chunk = match next {
                Ok(Some(chunk)) => chunk?,
                Ok(None) => break,
                Err(_) => {
                    return Err(Error::Provider(
                        "stream idle: no event for 180s".into(),
                    ));
                }
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(block) = take_sse_block(&mut buf) {
                parser.feed(&block, on_text, on_server, on_reasoning)?;
            }
        }
        if !buf.trim().is_empty() {
            parser.feed(&buf, on_text, on_server, on_reasoning)?;
        }
        parser.finish()
    }
}

fn grok_client_version() -> String {
    std::env::var("GROKA_GROK_CLIENT_VERSION").unwrap_or_else(|_| GROK_CLIENT_VERSION_DEFAULT.into())
}

/// Grok Build defaults `store: false` for ZDR. Chat turns also use `store:
/// false` because they resend full history instead of chaining. Override with
/// GROKA_CLIENT_MODE.
fn grok_client_mode() -> String {
    std::env::var("GROKA_CLIENT_MODE").unwrap_or_else(|_| "interactive".into())
}

/// Server refused to hydrate `previous_response_id` (ZDR org, or it was never stored).
pub fn previous_response_unusable(err: &Error) -> bool {
    let s = err.to_string().to_ascii_lowercase();
    s.contains("zero data retention") || s.contains("previous response cannot be used")
}

fn stream_fallback_ok(err: &Error) -> bool {
    if previous_response_unusable(err) {
        return false;
    }
    let s = err.to_string().to_ascii_lowercase();
    s.contains("http 400")
        || s.contains("http 415")
        || s.contains("http 404")
        || s.contains("stream ended")
        || s.contains("stream idle")
        || s.contains("non-json")
}

struct SseParser {
    completed: Option<CompleteResponse>,
}

impl SseParser {
    fn new() -> Self {
        Self { completed: None }
    }

    fn feed(
        &mut self,
        block: &str,
        on_text: &(dyn Fn(&str) + Send + Sync),
        on_server: &(dyn Fn(&str, &Value) + Send + Sync),
        on_reasoning: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<()> {
        let Some(data) = sse_data(block) else {
            return Ok(());
        };
        if data.trim() == "[DONE]" {
            return Ok(());
        }
        let v: Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        apply_sse_json(&v, on_text, on_server, on_reasoning, &mut self.completed)
    }

    fn finish(self) -> Result<CompleteResponse> {
        self.completed
            .ok_or_else(|| Error::Provider("stream ended without response.completed".into()))
    }
}

fn take_sse_block(buf: &mut String) -> Option<String> {
    let cr = buf.find("\r\n\r\n").map(|i| (i, 4));
    let lf = buf.find("\n\n").map(|i| (i, 2));
    let (i, n) = match (cr, lf) {
        (Some((a, an)), Some((b, bn))) => {
            if a <= b {
                (a, an)
            } else {
                (b, bn)
            }
        }
        (Some(x), None) => x,
        (None, Some(x)) => x,
        (None, None) => return None,
    };
    let block = buf[..i].to_string();
    buf.drain(..i + n);
    Some(block)
}

fn sse_data(block: &str) -> Option<String> {
    let mut data = Vec::new();
    for line in block.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with(':') || line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.trim_start());
        }
    }
    if data.is_empty() {
        None
    } else {
        Some(data.join("\n"))
    }
}

fn apply_sse_json(
    v: &Value,
    on_text: &(dyn Fn(&str) + Send + Sync),
    on_server: &(dyn Fn(&str, &Value) + Send + Sync),
    on_reasoning: &(dyn Fn(&str) + Send + Sync),
    completed: &mut Option<CompleteResponse>,
) -> Result<()> {
    let typ = v.get("type").and_then(Value::as_str).unwrap_or("");
    if typ == "error" {
        if let Some(msg) = provider_error_message(v.get("error").unwrap_or(v)) {
            return Err(Error::Provider(msg));
        }
        return Err(Error::Provider(format!("stream error: {v}")));
    }
    if is_reasoning_delta(typ) {
        if let Some(d) = v.get("delta").and_then(Value::as_str) {
            if !d.is_empty() {
                on_reasoning(d);
            }
        }
        return Ok(());
    }
    if typ == "response.output_text.delta" || typ.ends_with("output_text.delta") {
        if let Some(d) = v.get("delta").and_then(Value::as_str) {
            if !d.is_empty() {
                on_text(d);
            }
        }
        return Ok(());
    }
    if typ == "response.completed" || typ == "response.done" {
        let body = v.get("response").cloned().unwrap_or_else(|| v.clone());
        *completed = Some(parse_response_body(&body)?);
        return Ok(());
    }
    if let Some((kind, payload)) = sse_server_tool(typ, v) {
        on_server(&kind, &payload);
    }
    Ok(())
}

fn is_reasoning_delta(typ: &str) -> bool {
    typ == "response.reasoning_text.delta"
        || typ == "response.reasoning_summary_text.delta"
        || typ.ends_with("reasoning_text.delta")
        || typ.ends_with("reasoning_summary_text.delta")
}

fn sse_server_tool(typ: &str, v: &Value) -> Option<(String, Value)> {
    let looks = |s: &str| s.contains("web_search") || s.contains("x_search");
    if looks(typ) && !typ.ends_with(".completed") && !typ.ends_with(".done") {
        return Some((typ.to_string(), v.clone()));
    }
    if let Some(item) = v.get("item") {
        let it = item.get("type").and_then(Value::as_str).unwrap_or("");
        if it.ends_with("_call") && it != "function_call" {
            let status = item.get("status").and_then(Value::as_str).unwrap_or("");
            if status != "completed" {
                return Some((it.to_string(), item.clone()));
            }
        }
    }
    None
}

/// Parse a full SSE document (tests / replay). Last `response.completed` wins.
pub fn parse_sse_document(
    raw: &str,
    on_text: &(dyn Fn(&str) + Send + Sync),
    on_server: &(dyn Fn(&str, &Value) + Send + Sync),
    on_reasoning: &(dyn Fn(&str) + Send + Sync),
) -> Result<CompleteResponse> {
    let mut buf = raw.replace('\r', "").to_string();
    if !buf.ends_with("\n\n") {
        buf.push_str("\n\n");
    }
    let mut parser = SseParser::new();
    let mut rest = buf;
    while let Some(block) = take_sse_block(&mut rest) {
        parser.feed(&block, on_text, on_server, on_reasoning)?;
    }
    parser.finish()
}

/// Forced-tool title request. `store: false` so it never joins a chat chain.
pub fn session_title_payload(model: &str, user_message: &str) -> Value {
    json!({
        "model": model,
        "instructions": crate::session::session_title_system_prompt(),
        "input": [{
            "role": "user",
            "content": format!("<user_query>\n{user_message}\n</user_query>")
        }],
        "store": false,
        "max_output_tokens": 100,
        "prompt_cache_key": "grokaagent:title:v1",
        "reasoning": {"effort": "low"},
        "tools": [{
            "type": "function",
            "name": "session_title",
            "description": "Generate the session_title which we use for the user_message",
            "parameters": {
                "type": "object",
                "required": ["session_title"],
                "properties": {
                    "session_title": {
                        "type": "string",
                        "description": "Final session title, just 5-10 word descriptive title for the session. Super info dense, no filler."
                    }
                },
                "additionalProperties": false
            }
        }],
        "tool_choice": {"type": "function", "name": "session_title"}
    })
}

fn complete_payload(model: &str, req: &CompleteRequest) -> Value {
    let tools = tools_json(&req.client_tools, &req.server_tools);
    let mut payload = json!({
        "model": model,
        "input": req.input,
        "store": req.store,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": req.cache_key,
    });
    if req.send_reasoning {
        payload["reasoning"] = json!({"effort": req.reasoning_effort.as_str()});
    }
    // xAI rejects `instructions` together with `previous_response_id`.
    // Turn 1 stores instructions; later turns chain off that response.
    if req.previous_response_id.is_none() {
        payload["instructions"] = json!(req.instructions);
    }
    if !tools.is_empty() {
        payload["tools"] = Value::Array(tools);
        // xAI 400 if tool_choice is set with no tools (task supervisor has none).
        if let Some(name) = &req.tool_choice {
            if name == "none" {
                payload["tool_choice"] = json!("none");
            } else {
                payload["tool_choice"] = json!({"type": "function", "name": name});
            }
        }
    }
    if let Some(prev) = &req.previous_response_id {
        payload["previous_response_id"] = json!(prev);
    }
    payload
}

fn extract_text(item: &Value) -> String {
    let mut out = String::new();
    if let Some(content) = item.get("content").and_then(Value::as_array) {
        for part in content {
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                out.push_str(t);
            }
        }
    } else if let Some(t) = item.get("text").and_then(Value::as_str) {
        out.push_str(t);
    }
    out
}

/// Plaintext thinking from a Responses `output` list. Ignores encrypted blobs.
pub fn extract_reasoning_text(items: &[Value]) -> String {
    let mut out = String::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("reasoning") {
            continue;
        }
        let mut piece = String::new();
        if let Some(summary) = item.get("summary").and_then(Value::as_array) {
            for part in summary {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    if !t.is_empty() {
                        if !piece.is_empty() {
                            piece.push('\n');
                        }
                        piece.push_str(t);
                    }
                }
            }
        }
        if piece.is_empty() {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    let pt = part.get("type").and_then(Value::as_str).unwrap_or("");
                    if pt.contains("encrypted") {
                        continue;
                    }
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        if !t.is_empty() {
                            if !piece.is_empty() {
                                piece.push('\n');
                            }
                            piece.push_str(t);
                        }
                    }
                }
            }
        }
        if !piece.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&piece);
        }
    }
    out
}

fn arguments_as_string(item: &Value) -> String {
    match item.get("arguments") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "{}".into(),
    }
}

/// Responses API success bodies include `"error": null`. That is not a failure.
pub fn provider_error_message(err: &Value) -> Option<String> {
    match err {
        Value::Null => None,
        Value::Bool(false) => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(s.clone()),
        Value::Object(map) if map.is_empty() => None,
        other => {
            if let Some(msg) = other.get("message").and_then(Value::as_str) {
                if !msg.is_empty() {
                    return Some(msg.to_string());
                }
            }
            Some(format!("provider error: {other}"))
        }
    }
}

pub fn parse_response_body(body: &Value) -> Result<CompleteResponse> {
    if let Some(err) = body.get("error") {
        if let Some(msg) = provider_error_message(err) {
            return Err(Error::Provider(msg));
        }
    }
    if body.get("status").and_then(Value::as_str) == Some("failed") {
        return Err(Error::Provider(format!(
            "response status=failed: {}",
            body.get("error").unwrap_or(&Value::Null)
        )));
    }
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let output = body
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut text = String::new();
    let mut function_calls = Vec::new();
    let mut server_items = Vec::new();
    let output_items = output.clone();

    for item in output {
        let typ = item.get("type").and_then(Value::as_str).unwrap_or("");
        match typ {
            "message" => {
                let piece = extract_text(&item);
                if !piece.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&piece);
                }
            }
            "function_call" => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let arguments = arguments_as_string(&item);
                if name.is_empty() {
                    return Err(Error::Provider("function_call missing name".into()));
                }
                function_calls.push(FunctionCall {
                    call_id,
                    name,
                    arguments,
                });
            }
            other if other.ends_with("_call") => {
                server_items.push(item);
            }
            _ => {}
        }
    }

    Ok(CompleteResponse {
        id,
        text,
        function_calls,
        server_items,
        usage: parse_cache_usage(body),
        output_items,
    })
}

pub fn parse_cache_usage(body: &Value) -> CacheUsage {
    let usage = body.get("usage");
    let input_tokens = usage
        .and_then(|u| u.get("input_tokens").or_else(|| u.get("prompt_tokens")))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let cached_tokens = usage
        .and_then(|u| {
            u.get("input_tokens_details")
                .or_else(|| u.get("prompt_tokens_details"))
        })
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    CacheUsage {
        input_tokens,
        cached_tokens,
    }
}

/// Sticky routing id for one conversation.
///
/// `conversation` is the run/session id. Sharing one key across unrelated
/// chats (omitting it) routes everyone to the same cache replica and evicts
/// prefixes. Tool order is normalized. Changing the tool set or model still
/// changes the key because those bytes sit in the request prefix.
pub fn prompt_cache_key(
    model: &str,
    client_tools: &[ToolSpec],
    server_tools: &[String],
    conversation: &str,
) -> String {
    let mut parts: Vec<String> = client_tools.iter().map(|t| t.name.clone()).collect();
    parts.extend(server_tools.iter().cloned());
    parts.sort();
    parts.dedup();
    format!("grokaagent:v1:{model}:{}:{conversation}", parts.join(","))
}

fn tools_json(client_tools: &[ToolSpec], server_tools: &[String]) -> Vec<Value> {
    let mut client: Vec<&ToolSpec> = client_tools.iter().collect();
    client.sort_by(|a, b| a.name.cmp(&b.name));
    let mut tools: Vec<Value> = client
        .iter()
        .map(|s| {
            json!({
                "type": "function",
                "name": s.name,
                "description": s.description,
                "parameters": s.parameters,
            })
        })
        .collect();
    let mut server = server_tools.to_vec();
    server.sort();
    server.dedup();
    for name in server {
        tools.push(json!({"type": name}));
    }
    tools
}

fn entitlement_error(status: u16, body: &str) -> Error {
    if status == 402 || status == 403 {
        Error::Provider(format!(
            "xAI subscription inference rejected HTTP {status}. The OAuth login may be valid while this account is not entitled on cli-chat-proxy.grok.com."
        ))
    } else if status == 426 {
        Error::Provider(
            "xAI CLI proxy rejected the client version (HTTP 426). Set GROKA_GROK_CLIENT_VERSION to a newer Grok CLI version.".into(),
        )
    } else {
        let snippet: String = body.chars().take(240).collect();
        Error::Provider(format!("xAI HTTP {status}: {snippet}"))
    }
}

pub fn parse_compact_body(body: &Value) -> Result<CompactResponse> {
    if let Some(err) = body.get("error") {
        if let Some(msg) = provider_error_message(err) {
            return Err(Error::Provider(msg));
        }
    }
    let item = body
        .get("output")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .ok_or_else(|| Error::Provider("compact response missing output item".into()))?;
    if item.get("type").and_then(Value::as_str) != Some("compaction") {
        return Err(Error::Provider(format!(
            "compact output type was {}, expected compaction",
            item.get("type").unwrap_or(&Value::Null)
        )));
    }
    let dropped = body
        .get("usage")
        .and_then(|u| u.get("dropped_message_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    Ok(CompactResponse {
        item,
        dropped_message_count: dropped,
    })
}

impl Provider for XaiOauthProvider {
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
        let model = if req.model.is_empty() {
            self.model.as_str()
        } else {
            req.model.as_str()
        };
        let payload = complete_payload(model, &req);
        let (status, text) = self
            .grok_post("/responses", &req.cache_key, &payload, model)
            .await?;
        if status != 200 {
            return Err(entitlement_error(status, &text));
        }
        let body: Value = serde_json::from_str(&text)
            .map_err(|_| Error::Provider("xAI returned non-JSON body".into()))?;
        parse_response_body(&body)
    }

    async fn complete_stream<'a>(
        &'a self,
        req: CompleteRequest,
        on_text: &'a (dyn Fn(&str) + Send + Sync),
        on_server: &'a (dyn Fn(&str, &Value) + Send + Sync),
        on_reasoning: &'a (dyn Fn(&str) + Send + Sync),
    ) -> Result<CompleteResponse> {
        let model = if req.model.is_empty() {
            self.model.as_str()
        } else {
            req.model.as_str()
        };
        let mut payload = complete_payload(model, &req);
        payload["stream"] = json!(true);
        match self
            .grok_post_sse(&req.cache_key, &payload, model, on_text, on_server, on_reasoning)
            .await
        {
            Ok(r) => Ok(r),
            Err(e) if stream_fallback_ok(&e) => self.complete(req).await,
            Err(e) => Err(e),
        }
    }

    async fn compact(&self, req: CompactRequest) -> Result<CompactResponse> {
        let payload = json!({
            "model": self.model,
            "input": req.input,
            "prompt_cache_key": req.cache_key,
        });
        let (status, text) = self
            .grok_post("/responses/compact", &req.cache_key, &payload, &self.model)
            .await?;
        if status != 200 {
            return Err(entitlement_error(status, &text));
        }
        let body: Value = serde_json::from_str(&text)
            .map_err(|_| Error::Provider("xAI compact returned non-JSON body".into()))?;
        parse_compact_body(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_message_and_function_call() {
        let body = json!({
            "id": "resp_1",
            "output": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "now",
                    "arguments": "{}"
                },
                {
                    "type": "web_search_call",
                    "status": "completed",
                    "action": {"query": "rust"}
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "hello"}]
                }
            ]
        });
        let parsed = parse_response_body(&body).unwrap();
        assert_eq!(parsed.id, "resp_1");
        assert_eq!(parsed.text, "hello");
        assert_eq!(parsed.function_calls.len(), 1);
        assert_eq!(parsed.function_calls[0].name, "now");
        assert_eq!(parsed.server_items.len(), 1);
        assert_eq!(parsed.server_items[0]["type"], "web_search_call");
        assert_eq!(parsed.output_items.len(), 3);
        assert_eq!(parsed.usage.cached_tokens, 0);
    }

    #[test]
    fn parses_cached_tokens_from_responses_usage() {
        let body = json!({
            "id": "resp_ok",
            "status": "completed",
            "error": null,
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "hello"}]
            }],
            "usage": {
                "input_tokens": 2000,
                "output_tokens": 40,
                "input_tokens_details": {"cached_tokens": 1880}
            }
        });
        let parsed = parse_response_body(&body).unwrap();
        assert_eq!(parsed.usage.input_tokens, 2000);
        assert_eq!(parsed.usage.cached_tokens, 1880);
        assert!(parsed.usage.rate() >= 0.90);
        assert!(!parsed.usage.below_target(1));
        assert!(!parsed.usage.below_target(2));
        let cold = CacheUsage {
            input_tokens: 2000,
            cached_tokens: 0,
        };
        assert!(cold.below_target(2));
        assert!(!cold.below_target(1));
    }

    #[test]
    fn cache_key_is_stable_and_order_independent() {
        use crate::tools::ClientTool;
        let a = vec![crate::tools::NowTool.spec()];
        let k1 = prompt_cache_key("grok-4.6", &a, &[], "run_a");
        let k2 = prompt_cache_key("grok-4.6", &a, &[], "run_a");
        assert_eq!(k1, k2);
        assert!(k1.starts_with("grokaagent:v1:grok-4.6:now:"));
        assert!(k1.ends_with(":run_a"));
        assert_ne!(k1, prompt_cache_key("grok-4.5", &a, &[], "run_a"));
        assert_ne!(k1, prompt_cache_key("grok-4.6", &a, &[], "run_b"));
        let mixed = prompt_cache_key("grok-4.6", &a, &["web_search".into()], "run_a");
        assert!(mixed.contains("now"));
        assert!(mixed.contains("web_search"));
        assert!(mixed.ends_with(":run_a"));
    }

    #[test]
    fn parses_compaction_item() {
        let body = json!({
            "id": "cmp_1",
            "object": "response.compaction",
            "output": [{
                "type": "compaction",
                "id": "cmp_1",
                "encrypted_content": "blob"
            }],
            "usage": {"dropped_message_count": 12}
        });
        let parsed = parse_compact_body(&body).unwrap();
        assert_eq!(parsed.item["type"], "compaction");
        assert_eq!(parsed.dropped_message_count, 12);
    }

    #[test]
    fn success_body_with_null_error_is_ok() {
        let body = json!({
            "id": "resp_ok",
            "status": "completed",
            "error": null,
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "hello"}]
            }]
        });
        let parsed = parse_response_body(&body).unwrap();
        assert_eq!(parsed.text, "hello");
    }

    #[test]
    fn error_object_without_message_still_surfaces_json() {
        let body = json!({"error": {"code": "invalid_request"}});
        let err = parse_response_body(&body).unwrap_err().to_string();
        assert!(err.contains("invalid_request"), "{err}");
        assert_ne!(err, "provider error");
    }

    #[test]
    fn provider_error_object_is_surfaced() {
        let body = json!({"error": {"message": "nope"}});
        let err = parse_response_body(&body).unwrap_err().to_string();
        assert_eq!(err, "nope");
    }

    #[test]
    fn https_required_for_base_url() {
        assert!(require_https_base("https://cli-chat-proxy.grok.com/v1").is_ok());
        let err = require_https_base("http://127.0.0.1/v1").unwrap_err();
        assert!(err.to_string().contains("https"));
    }

    #[test]
    fn reasoning_effort_parse_and_cycle() {
        assert_eq!(ReasoningEffort::parse("low"), Some(ReasoningEffort::Low));
        assert_eq!(ReasoningEffort::parse("XHIGH"), Some(ReasoningEffort::Xhigh));
        assert_eq!(ReasoningEffort::parse("nope"), None);
        assert_eq!(ReasoningEffort::High.cycle(), ReasoningEffort::Xhigh);
        assert_eq!(ReasoningEffort::Xhigh.cycle(), ReasoningEffort::Low);
        assert_eq!(ReasoningEffort::Low.cycle_back(), ReasoningEffort::Xhigh);
    }

    #[test]
    fn complete_payload_sends_effort_outside_cache_key() {
        let req = CompleteRequest {
            instructions: "stay".into(),
            input: vec![json!({"role":"user","content":"hi"})],
            client_tools: vec![],
            server_tools: vec![],
            cache_key: prompt_cache_key("grok-4.6", &[], &[], "run_a"),
            previous_response_id: None,
            store: true,
            reasoning_effort: ReasoningEffort::Xhigh,
            send_reasoning: true,
            model: "grok-4.6".into(),
            tool_choice: None,
        };
        let payload = complete_payload("grok-4.6", &req);
        assert_eq!(payload["instructions"], "stay");
        assert!(payload.get("previous_response_id").is_none());
        assert_eq!(payload["store"], true);
        assert_eq!(payload["reasoning"]["effort"], "xhigh");
        assert_eq!(payload["model"], "grok-4.6");
        let key = payload["prompt_cache_key"].as_str().unwrap();
        assert!(key.starts_with("grokaagent:v1:grok-4.6"));
        assert!(!key.contains("xhigh"), "{key}");
        let low = complete_payload(
            "grok-4.6",
            &CompleteRequest {
                reasoning_effort: ReasoningEffort::Low,
                ..req.clone()
            },
        );
        assert_eq!(low["reasoning"]["effort"], "low");
        assert_eq!(low["prompt_cache_key"], payload["prompt_cache_key"]);
        assert!(payload.get("tool_choice").is_none());
        let dummy = ToolSpec {
            name: "shell_verdict".into(),
            description: "verdict".into(),
            parameters: json!({"type": "object"}),
        };
        let forced = complete_payload(
            "grok-4.6",
            &CompleteRequest {
                tool_choice: Some("shell_verdict".into()),
                client_tools: vec![dummy.clone()],
                store: false,
                cache_key: "grokaagent:shellguard:v1:cmd".into(),
                ..req.clone()
            },
        );
        assert_eq!(forced["tool_choice"]["name"], "shell_verdict");
        assert_eq!(forced["tools"][0]["name"], "shell_verdict");
        assert_eq!(forced["store"], false);
        assert_eq!(forced["prompt_cache_key"], "grokaagent:shellguard:v1:cmd");
        // xAI 400: "A tool_choice was set on the request but no tools were specified."
        let none_without_tools = complete_payload(
            "grok-4.6",
            &CompleteRequest {
                tool_choice: Some("none".into()),
                client_tools: vec![],
                server_tools: vec![],
                store: false,
                ..req.clone()
            },
        );
        assert!(
            none_without_tools.get("tool_choice").is_none(),
            "{none_without_tools}"
        );
        assert!(none_without_tools.get("tools").is_none(), "{none_without_tools}");
        let none_with_tools = complete_payload(
            "grok-4.6",
            &CompleteRequest {
                tool_choice: Some("none".into()),
                client_tools: vec![dummy],
                store: false,
                ..req.clone()
            },
        );
        assert_eq!(none_with_tools["tool_choice"], "none");
        assert_eq!(none_with_tools["tools"][0]["name"], "shell_verdict");
        let omitted = complete_payload(
            "no-think",
            &CompleteRequest {
                send_reasoning: false,
                model: "no-think".into(),
                ..req.clone()
            },
        );
        assert!(omitted.get("reasoning").is_none(), "{omitted}");
    }

    #[test]
    fn session_title_payload_is_stateless_forced_tool() {
        let payload = session_title_payload("grok-4.6", "修 login race");
        assert_eq!(payload["store"], false);
        assert!(payload.get("previous_response_id").is_none());
        assert_eq!(payload["prompt_cache_key"], "grokaagent:title:v1");
        assert_eq!(payload["tool_choice"]["name"], "session_title");
        assert_eq!(payload["tools"][0]["name"], "session_title");
        assert_eq!(payload["reasoning"]["effort"], "low");
        assert_ne!(
            payload["prompt_cache_key"],
            prompt_cache_key("grok-4.6", &[], &[], "run_a"),
            "title calls must not share the chat cache key"
        );
        let input = payload["input"][0]["content"].as_str().unwrap();
        assert!(input.contains("修 login race"), "{input}");
    }

    #[test]
    fn chained_complete_omits_instructions() {
        let req = CompleteRequest {
            instructions: "stay".into(),
            input: vec![json!({"role": "user", "content": "next"})],
            client_tools: vec![],
            server_tools: vec![],
            cache_key: prompt_cache_key("grok-4.6", &[], &[], "run_a"),
            previous_response_id: Some("resp_1".into()),
            store: true,
            reasoning_effort: ReasoningEffort::High,
            send_reasoning: true,
            model: "grok-4.6".into(),
            tool_choice: None,
        };
        let payload = complete_payload("grok-4.6", &req);
        assert!(
            payload.get("instructions").is_none(),
            "xAI 400 if both are set: {payload}"
        );
        assert_eq!(payload["previous_response_id"], "resp_1");
        assert_eq!(payload["store"], true);
        assert_eq!(payload["input"][0]["content"], "next");
    }

    #[test]
    fn previous_response_unusable_detects_zdr_404() {
        let err = Error::Provider(
            r#"xAI HTTP 404: {"code":"not-found","error":"Previous response cannot be used for this organization due to Zero Data Retention"}"#.into(),
        );
        assert!(previous_response_unusable(&err));
        assert!(!previous_response_unusable(&Error::Provider("xAI HTTP 400: instructions".into())));
        assert!(!previous_response_unusable(&Error::EmptyPrompt));
    }

    #[test]
    fn parses_sse_text_deltas_and_completed() {
        let raw = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"He\"}\n\n",
            "event: response.web_search_call.in_progress\n",
            "data: {\"type\":\"response.web_search_call.in_progress\",\"action\":{\"query\":\"elon\"}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"llo\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_s\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\"}]}]}}\n\n",
        );
        let deltas = std::sync::Mutex::new(String::new());
        let servers = std::sync::Mutex::new(Vec::<String>::new());
        let parsed = parse_sse_document(
            raw,
            &|d| deltas.lock().unwrap().push_str(d),
            &|kind, payload| {
                servers.lock().unwrap().push(format!("{kind}:{payload}"));
            },
            &|_| {},
        )
        .unwrap();
        assert_eq!(*deltas.lock().unwrap(), "Hello");
        assert_eq!(parsed.id, "resp_s");
        assert_eq!(parsed.text, "Hello");
        let servers = servers.lock().unwrap();
        assert_eq!(servers.len(), 1);
        assert!(servers[0].contains("web_search"), "{}", servers[0]);
        assert!(servers[0].contains("elon"), "{}", servers[0]);
    }

    #[test]
    fn sse_done_without_completed_errors() {
        let err = parse_sse_document("data: [DONE]\n\n", &|_| {}, &|_, _| {}, &|_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("stream ended"), "{err}");
    }

    #[test]
    fn parses_sse_reasoning_deltas_separately_from_answer() {
        let raw = concat!(
            "event: response.reasoning_summary_text.delta\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"use energy\"}\n\n",
            "event: response.reasoning_text.delta\n",
            "data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\" then sqrt\"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"42\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_r\",\"output\":[{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"use energy then sqrt\"}]},{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"42\"}]}]}}\n\n",
        );
        let answer = std::sync::Mutex::new(String::new());
        let think = std::sync::Mutex::new(String::new());
        let parsed = parse_sse_document(
            raw,
            &|d| answer.lock().unwrap().push_str(d),
            &|_, _| {},
            &|d| think.lock().unwrap().push_str(d),
        )
        .unwrap();
        assert_eq!(*think.lock().unwrap(), "use energy then sqrt");
        assert_eq!(*answer.lock().unwrap(), "42");
        assert_eq!(parsed.text, "42");
        assert_eq!(
            extract_reasoning_text(&parsed.output_items),
            "use energy then sqrt"
        );
    }

    #[test]
    fn extract_reasoning_skips_encrypted_only() {
        let items = vec![json!({
            "type": "reasoning",
            "encrypted_content": "blob",
            "content": [{"type": "reasoning.encrypted", "text": "nope"}]
        })];
        assert_eq!(extract_reasoning_text(&items), "");
    }
}
