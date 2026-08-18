//! OpenAI-compatible Chat Completions (`POST /v1/chat/completions`).
//!
//! Translates the kernel's Responses-shaped history into chat messages so a
//! custom base URL + model name can drive the same tool loop.

use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::provider::{
    sse_data, take_sse_block, CacheUsage, CompactRequest, CompactResponse, CompleteRequest,
    CompleteResponse, FunctionCall, Provider,
};
use crate::tools::ToolSpec;

#[derive(Clone)]
pub struct OpenAiCompatProvider {
    base_url: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl OpenAiCompatProvider {
    pub fn new(cfg: &ProviderConfig, model: Option<String>) -> Result<Self> {
        let base_url = normalize_base(&cfg.base_url)?;
        let model = model
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| cfg.effective_model().to_string());
        if model.trim().is_empty() {
            return Err(Error::Provider("custom API needs a model name".into()));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?;
        Ok(Self {
            base_url,
            api_key: cfg.api_key.trim().to_string(),
            model,
            client,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub async fn list_models(&self) -> Result<crate::catalog::ModelCatalog> {
        let url = format!("{}/models", self.base_url);
        let mut req = self
            .client
            .get(&url)
            .header("Accept", "application/json");
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        if status != 200 {
            return Err(http_error(status, &text));
        }
        crate::catalog::parse_catalog_json(&text).map_err(Error::Provider)
    }

    pub async fn generate_session_title(&self, user_message: &str) -> String {
        let source = crate::session::title_source_text(user_message);
        let req = CompleteRequest {
            instructions: crate::session::session_title_system_prompt().into(),
            input: vec![json!({"role": "user", "content": source.clone()})],
            client_tools: vec![],
            server_tools: vec![],
            cache_key: "grokaagent:title:v1".into(),
            previous_response_id: None,
            store: false,
            reasoning_effort: crate::provider::ReasoningEffort::Low,
            send_reasoning: false,
            model: self.model.clone(),
            tool_choice: Some("none".into()),
        };
        match self.complete(req).await {
            Ok(r) => {
                let line = r
                    .text
                    .lines()
                    .map(str::trim)
                    .find(|s| !s.is_empty())
                    .unwrap_or("");
                if line.is_empty() {
                    crate::session::title_fallback_from_user_text(&source)
                } else {
                    line.chars().take(80).collect()
                }
            }
            Err(_) => crate::session::title_fallback_from_user_text(&source),
        }
    }

    fn request(&self, path: &str, accept: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{path}", self.base_url);
        let mut req = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", accept)
            .header(
                "User-Agent",
                concat!("grokaagent/", env!("CARGO_PKG_VERSION")),
            );
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        req
    }
}

impl Provider for OpenAiCompatProvider {
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse> {
        let model = if req.model.is_empty() {
            self.model.as_str()
        } else {
            req.model.as_str()
        };
        let payload = chat_payload(model, &req);
        let resp = self
            .request("/chat/completions", "application/json")
            .json(&payload)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        if status != 200 {
            return Err(http_error(status, &text));
        }
        let body: Value = serde_json::from_str(&text)
            .map_err(|_| Error::Provider("endpoint returned non-JSON body".into()))?;
        parse_chat_body(&body)
    }

    async fn complete_stream<'a>(
        &'a self,
        req: CompleteRequest,
        on_text: &'a (dyn Fn(&str) + Send + Sync),
        _on_server: &'a (dyn Fn(&str, &Value) + Send + Sync),
        on_reasoning: &'a (dyn Fn(&str) + Send + Sync),
    ) -> Result<CompleteResponse> {
        let model = if req.model.is_empty() {
            self.model.as_str()
        } else {
            req.model.as_str()
        };
        let mut payload = chat_payload(model, &req);
        payload["stream"] = json!(true);
        payload["stream_options"] = json!({"include_usage": true});
        let resp = self
            .request("/chat/completions", "text/event-stream")
            .timeout(Duration::from_secs(600))
            .json(&payload)
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
            return Err(http_error(status, &text));
        }
        if !ctype.contains("event-stream") {
            let text = resp.text().await?;
            let body: Value = serde_json::from_str(&text)
                .map_err(|_| Error::Provider("endpoint returned non-JSON body".into()))?;
            return parse_chat_body(&body);
        }
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut acc = ChatStream::default();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(180), stream.next()).await;
            let chunk = match next {
                Ok(Some(chunk)) => chunk?,
                Ok(None) => break,
                Err(_) => {
                    return Err(Error::Provider("stream idle: no event for 180s".into()));
                }
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(block) = take_sse_block(&mut buf) {
                acc.feed(&block, on_text, on_reasoning)?;
            }
        }
        if !buf.trim().is_empty() {
            acc.feed(&buf, on_text, on_reasoning)?;
        }
        acc.finish()
    }

    async fn compact(&self, _req: CompactRequest) -> Result<CompactResponse> {
        Err(Error::Provider(
            "this endpoint has no /responses/compact".into(),
        ))
    }
}

pub fn normalize_base(url: &str) -> Result<String> {
    let mut u = url.trim().trim_end_matches('/').to_string();
    if let Some(stripped) = u.strip_suffix("/chat/completions") {
        u = stripped.trim_end_matches('/').to_string();
    }
    if !(u.starts_with("http://") || u.starts_with("https://")) {
        return Err(Error::Provider(
            "API base URL must start with http:// or https://".into(),
        ));
    }
    Ok(u)
}

fn http_error(status: u16, body: &str) -> Error {
    let snippet: String = body.chars().take(240).collect();
    Error::Provider(format!("HTTP {status}: {snippet}"))
}

pub fn chat_payload(model: &str, req: &CompleteRequest) -> Value {
    let mut payload = json!({
        "model": model,
        "messages": chat_messages(&req.instructions, &req.input),
    });
    let tools = chat_tools(&req.client_tools);
    if !tools.is_empty() {
        payload["tools"] = Value::Array(tools);
        if let Some(name) = &req.tool_choice {
            if name == "none" {
                payload["tool_choice"] = json!("none");
            } else {
                payload["tool_choice"] =
                    json!({"type": "function", "function": {"name": name}});
            }
        }
    }
    payload
}

fn chat_tools(client_tools: &[ToolSpec]) -> Vec<Value> {
    let mut tools: Vec<&ToolSpec> = client_tools.iter().collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
        .into_iter()
        .map(|s| {
            json!({
                "type": "function",
                "function": {
                    "name": s.name,
                    "description": s.description,
                    "parameters": s.parameters,
                }
            })
        })
        .collect()
}

pub fn chat_messages(instructions: &str, input: &[Value]) -> Vec<Value> {
    let mut msgs = Vec::new();
    if !instructions.trim().is_empty() {
        msgs.push(json!({"role": "system", "content": instructions}));
    }
    for item in input {
        if let Some(msg) = item_to_chat(item) {
            msgs.push(msg);
        }
    }
    msgs
}

fn item_to_chat(item: &Value) -> Option<Value> {
    if let Some(role) = item.get("role").and_then(Value::as_str) {
        if item.get("type").and_then(Value::as_str).is_none() || role == "user" {
            match role {
                "user" => return Some(user_to_chat(item)),
                "assistant" => {
                    return Some(json!({
                        "role": "assistant",
                        "content": content_as_text(item.get("content").unwrap_or(&Value::Null)),
                    }));
                }
                "system" => {
                    return Some(json!({
                        "role": "system",
                        "content": content_as_text(item.get("content").unwrap_or(&Value::Null)),
                    }));
                }
                _ => {}
            }
        }
    }
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("call_missing")
                .to_string();
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let arguments = match item.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "{}".into(),
            };
            Some(json!({
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments}
                }]
            }))
        }
        Some("function_call_output") => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": content_as_text(item.get("output").unwrap_or(&Value::Null)),
            }))
        }
        Some("message") => Some(json!({
            "role": "assistant",
            "content": extract_text(item),
        })),
        Some("reasoning") => None,
        Some("compaction") => {
            let text = extract_text(item);
            if text.is_empty() {
                None
            } else {
                Some(json!({"role": "user", "content": text}))
            }
        }
        _ => None,
    }
}

fn user_to_chat(item: &Value) -> Value {
    match item.get("content") {
        Some(Value::String(s)) => json!({"role": "user", "content": s}),
        Some(Value::Array(parts)) => {
            let converted: Vec<Value> = parts.iter().filter_map(part_to_chat).collect();
            if converted.is_empty() {
                json!({"role": "user", "content": ""})
            } else if converted.len() == 1 && converted[0].get("type").and_then(Value::as_str) == Some("text") {
                json!({"role": "user", "content": converted[0]["text"]})
            } else {
                json!({"role": "user", "content": converted})
            }
        }
        _ => json!({"role": "user", "content": ""}),
    }
}

fn part_to_chat(part: &Value) -> Option<Value> {
    match part.get("type").and_then(Value::as_str) {
        Some("input_text") | Some("text") | Some("output_text") => Some(json!({
            "type": "text",
            "text": part.get("text").and_then(Value::as_str).unwrap_or(""),
        })),
        Some("input_image") | Some("image_url") => {
            let url = part
                .get("image_url")
                .and_then(|u| u.as_str().or_else(|| u.get("url").and_then(Value::as_str)))
                .unwrap_or("");
            Some(json!({
                "type": "image_url",
                "image_url": {"url": url},
            }))
        }
        _ => None,
    }
}

fn content_as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        other if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

fn extract_text(item: &Value) -> String {
    if let Some(content) = item.get("content") {
        let t = content_as_text(content);
        if !t.is_empty() {
            return t;
        }
    }
    item.get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

pub fn parse_chat_body(body: &Value) -> Result<CompleteResponse> {
    if let Some(err) = body.get("error") {
        if let Some(msg) = crate::provider::provider_error_message(err) {
            return Err(Error::Provider(msg));
        }
    }
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or_else(|| Error::Provider("chat completion missing choices".into()))?;
    let message = choice.get("message").unwrap_or(choice);
    parse_message(id, message, cache_usage(body))
}

fn parse_message(id: String, message: &Value, usage: CacheUsage) -> Result<CompleteResponse> {
    let text = message_text(message);
    let mut function_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (i, call) in calls.iter().enumerate() {
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{i}"));
            let func = call.get("function").unwrap_or(call);
            let name = func
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                return Err(Error::Provider("tool_call missing name".into()));
            }
            let arguments = match func.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "{}".into(),
            };
            function_calls.push(FunctionCall {
                call_id,
                name,
                arguments,
            });
        }
    }
    Ok(complete_from_chat(id, text, function_calls, usage))
}

fn message_text(message: &Value) -> String {
    if let Some(s) = message.get("content").and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(arr) = message.get("content").and_then(Value::as_array) {
        return arr
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("");
    }
    String::new()
}

fn complete_from_chat(
    id: String,
    text: String,
    function_calls: Vec<FunctionCall>,
    usage: CacheUsage,
) -> CompleteResponse {
    let mut output_items = Vec::new();
    if !text.is_empty() {
        output_items.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}],
        }));
    }
    for c in &function_calls {
        output_items.push(json!({
            "type": "function_call",
            "call_id": c.call_id,
            "name": c.name,
            "arguments": c.arguments,
        }));
    }
    CompleteResponse {
        id,
        text,
        function_calls,
        server_items: vec![],
        usage,
        output_items,
    }
}

fn cache_usage(body: &Value) -> CacheUsage {
    let Some(u) = body.get("usage") else {
        return CacheUsage::default();
    };
    let input_tokens = u
        .get("prompt_tokens")
        .or_else(|| u.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let cached_tokens = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .or_else(|| u.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    CacheUsage {
        input_tokens,
        cached_tokens,
    }
}

#[derive(Default)]
struct ChatStream {
    id: String,
    text: String,
    calls: Vec<StreamCall>,
    usage: CacheUsage,
}

#[derive(Default, Clone)]
struct StreamCall {
    id: String,
    name: String,
    arguments: String,
}

impl ChatStream {
    fn feed(
        &mut self,
        block: &str,
        on_text: &(dyn Fn(&str) + Send + Sync),
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
        if let Some(err) = v.get("error") {
            if let Some(msg) = crate::provider::provider_error_message(err) {
                return Err(Error::Provider(msg));
            }
        }
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            if !id.is_empty() {
                self.id = id.to_string();
            }
        }
        if v.get("usage").is_some() {
            self.usage = cache_usage(&v);
        }
        let Some(choice) = v.get("choices").and_then(Value::as_array).and_then(|a| a.first()) else {
            return Ok(());
        };
        let delta = choice
            .get("delta")
            .or_else(|| choice.get("message"))
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(think) = delta
            .get("reasoning_content")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            on_reasoning(think);
        }
        if let Some(piece) = delta.get("content").and_then(Value::as_str) {
            if !piece.is_empty() {
                self.text.push_str(piece);
                on_text(piece);
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let idx = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                while self.calls.len() <= idx {
                    self.calls.push(StreamCall::default());
                }
                let slot = &mut self.calls[idx];
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    if !id.is_empty() {
                        slot.id = id.to_string();
                    }
                }
                let func = call.get("function").unwrap_or(call);
                if let Some(name) = func.get("name").and_then(Value::as_str) {
                    if !name.is_empty() {
                        slot.name.push_str(name);
                    }
                }
                if let Some(args) = func.get("arguments").and_then(Value::as_str) {
                    slot.arguments.push_str(args);
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<CompleteResponse> {
        let mut function_calls = Vec::new();
        for (i, c) in self.calls.into_iter().enumerate() {
            if c.name.is_empty() {
                continue;
            }
            function_calls.push(FunctionCall {
                call_id: if c.id.is_empty() {
                    format!("call_{i}")
                } else {
                    c.id
                },
                name: c.name,
                arguments: if c.arguments.is_empty() {
                    "{}".into()
                } else {
                    c.arguments
                },
            });
        }
        if self.text.is_empty() && function_calls.is_empty() {
            return Err(Error::Provider(
                "stream ended without assistant text or tool calls".into(),
            ));
        }
        Ok(complete_from_chat(
            self.id,
            self.text,
            function_calls,
            self.usage,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::CompleteRequest;
    use crate::tools::ToolSpec;

    fn req(input: Vec<Value>) -> CompleteRequest {
        CompleteRequest {
            instructions: "sys".into(),
            input,
            client_tools: vec![ToolSpec {
                name: "now".into(),
                description: "clock".into(),
                parameters: json!({"type": "object"}),
            }],
            server_tools: vec!["web_search".into()],
            cache_key: "k".into(),
            previous_response_id: None,
            store: false,
            reasoning_effort: crate::provider::ReasoningEffort::High,
            send_reasoning: true,
            model: "Qwen3.8-27B-ABLITERATED-Q8_0".into(),
            tool_choice: None,
        }
    }

    #[test]
    fn strips_chat_completions_suffix_and_allows_http() {
        let u = normalize_base("http://127.0.0.1:8080/v1/chat/completions/").unwrap();
        assert_eq!(u, "http://127.0.0.1:8080/v1");
        assert!(normalize_base("ftp://x").is_err());
    }

    #[test]
    fn history_becomes_chat_messages_and_drops_server_tools() {
        let payload = chat_payload(
            "Qwen3.8-27B-ABLITERATED-Q8_0",
            &req(vec![
                json!({"role": "user", "content": "hi"}),
                json!({
                    "type": "function_call",
                    "call_id": "c1",
                    "name": "now",
                    "arguments": "{}"
                }),
                json!({
                    "type": "function_call_output",
                    "call_id": "c1",
                    "output": "noon"
                }),
            ]),
        );
        assert_eq!(payload["model"], "Qwen3.8-27B-ABLITERATED-Q8_0");
        assert!(payload.get("reasoning").is_none());
        assert!(payload.get("prompt_cache_key").is_none());
        let tools = payload["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "now");
        let msgs = payload["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "hi");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "c1");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "c1");
        assert_eq!(msgs[3]["content"], "noon");
    }

    #[test]
    fn image_parts_become_chat_image_url() {
        let msgs = chat_messages(
            "",
            &[json!({
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "look"},
                    {"type": "input_image", "image_url": "data:image/jpeg;base64,xx", "detail": "high"}
                ]
            })],
        );
        let parts = msgs[0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/jpeg;base64,xx");
    }

    #[test]
    fn parses_tool_call_choice_into_responses_output_items() {
        let body = json!({
            "id": "chatcmpl-1",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_ab",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}
                    }]
                }
            }],
            "usage": {"prompt_tokens": 40, "completion_tokens": 8}
        });
        let parsed = parse_chat_body(&body).unwrap();
        assert_eq!(parsed.id, "chatcmpl-1");
        assert_eq!(parsed.function_calls.len(), 1);
        assert_eq!(parsed.function_calls[0].name, "read_file");
        assert_eq!(parsed.function_calls[0].call_id, "call_ab");
        assert_eq!(parsed.usage.input_tokens, 40);
        assert_eq!(parsed.output_items[0]["type"], "function_call");
        assert_eq!(parsed.output_items[0]["call_id"], "call_ab");
    }

    #[test]
    fn stream_accumulates_text_and_tool_args() {
        let mut acc = ChatStream::default();
        let on_text = |_: &str| {};
        let on_r = |_: &str| {};
        acc.feed(
            "data: {\"id\":\"chatcmpl-s\",\"choices\":[{\"delta\":{\"content\":\"P\"}}]}\n\n",
            &on_text,
            &on_r,
        )
        .unwrap();
        acc.feed(
            "data: {\"choices\":[{\"delta\":{\"content\":\"ONG\"}}]}\n\n",
            &on_text,
            &on_r,
        )
        .unwrap();
        acc.feed("data: [DONE]\n\n", &on_text, &on_r).unwrap();
        let r = acc.finish().unwrap();
        assert_eq!(r.text, "PONG");
        assert_eq!(r.output_items[0]["type"], "message");
    }

    #[test]
    fn parses_llamacpp_reasoning_plus_content() {
        let body = json!({
            "id": "chatcmpl-1",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "PONG",
                    "reasoning_content": "The user wants exactly PONG."
                }
            }],
            "usage": {
                "prompt_tokens": 21,
                "completion_tokens": 25,
                "prompt_tokens_details": {"cached_tokens": 308}
            }
        });
        let parsed = parse_chat_body(&body).unwrap();
        assert_eq!(parsed.text, "PONG");
        assert_eq!(parsed.usage.input_tokens, 21);
        assert_eq!(parsed.usage.cached_tokens, 308);
    }

    #[test]
    fn stream_keeps_text_when_reasoning_and_empty_usage_chunk() {
        let mut acc = ChatStream::default();
        let on_text = |_: &str| {};
        let on_r = |_: &str| {};
        acc.feed(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":null}}]}\n\n",
            &on_text,
            &on_r,
        )
        .unwrap();
        acc.feed(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n\n",
            &on_text,
            &on_r,
        )
        .unwrap();
        acc.feed(
            "data: {\"choices\":[{\"delta\":{\"content\":\"PONG\"}}]}\n\n",
            &on_text,
            &on_r,
        )
        .unwrap();
        acc.feed(
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"prompt_tokens_details\":{\"cached_tokens\":3}}}\n\n",
            &on_text,
            &on_r,
        )
        .unwrap();
        acc.feed("data: [DONE]\n\n", &on_text, &on_r).unwrap();
        let r = acc.finish().unwrap();
        assert_eq!(r.text, "PONG");
        assert_eq!(r.usage.input_tokens, 10);
        assert_eq!(r.usage.cached_tokens, 3);
    }

    #[tokio::test]
    #[ignore]
    async fn live_openai_compat_pong() {
        let base = std::env::var("GROKA_LIVE_BASE").expect("GROKA_LIVE_BASE");
        let model = std::env::var("GROKA_LIVE_MODEL").expect("GROKA_LIVE_MODEL");
        let cfg = crate::config::ProviderConfig {
            kind: crate::config::ProviderKind::Openai,
            base_url: base,
            model: model.clone(),
            api_key: String::new(),
            context_window: 262_144,
        };
        let p = OpenAiCompatProvider::new(&cfg, None).unwrap();
        let req = CompleteRequest {
            instructions: String::new(),
            input: vec![json!({
                "role": "user",
                "content": "Reply with exactly the word PONG and nothing else."
            })],
            client_tools: vec![],
            server_tools: vec![],
            cache_key: "live".into(),
            previous_response_id: None,
            store: false,
            reasoning_effort: crate::provider::ReasoningEffort::Low,
            send_reasoning: false,
            model,
            tool_choice: Some("none".into()),
        };
        let once = p.complete(req.clone()).await.unwrap();
        assert!(
            once.text.to_ascii_uppercase().contains("PONG"),
            "complete() text was {:?}",
            once.text
        );
        let streamed = std::sync::Mutex::new(String::new());
        let on_text = |s: &str| streamed.lock().unwrap().push_str(s);
        let on_server = |_: &str, _: &Value| {};
        let on_r = |_: &str| {};
        let two = p
            .complete_stream(req, &on_text, &on_server, &on_r)
            .await
            .unwrap();
        assert!(
            two.text.to_ascii_uppercase().contains("PONG"),
            "complete_stream() text was {:?}",
            two.text
        );
        assert_eq!(*streamed.lock().unwrap(), two.text);
    }
}
