//! `grok_search`: lend Grok's server-side web / X search to non-Grok models.
//!
//! The calling model asks one natural-language question. The tool sends it to
//! the newest Grok model through the TUI's xAI login, with `web_search` and
//! `x_search` enabled, and returns Grok's answer plus its sources. Grok models
//! use the native server tools instead, so this tool is only offered when the
//! session's live model routes to another backend.

use std::path::PathBuf;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::error::{Error, Result};
use crate::provider::{CompleteRequest, Provider, ReasoningEffort, XaiOauthProvider};
use crate::tools::{ClientTool, ToolCallFut, ToolSpec};

/// Marker in `server_tools` that switches this tool on. Never sent to a backend.
pub const GATE: &str = "grok_search";
pub const NAME: &str = "grok_search";

const MAX_QUERY_CHARS: usize = 4000;
const MAX_ANSWER_CHARS: usize = 12_000;
const MAX_SOURCES: usize = 20;

/// True when a saved xAI login exists, so the tool can reach Grok.
pub fn grok_login_available() -> bool {
    crate::auth::default_auth_path()
        .ok()
        .and_then(|p| crate::auth::load_tokens(&p).ok())
        .is_some()
}

pub struct GrokSearchTool {
    auth_path: PathBuf,
    model: Mutex<Option<(String, bool)>>,
}

impl GrokSearchTool {
    pub fn new(auth_path: PathBuf) -> Self {
        Self {
            auth_path,
            model: Mutex::new(None),
        }
    }

    /// `GROKA_GROK_SEARCH_MODEL`, else the first (newest) catalog model, else
    /// the built-in default. The bool says whether to send `reasoning`. Only a
    /// catalog answer is cached, so a failed fetch (expired login) retries.
    async fn model(&self, xai: &XaiOauthProvider) -> (String, bool) {
        if let Ok(m) = std::env::var("GROKA_GROK_SEARCH_MODEL") {
            if !m.trim().is_empty() {
                return (m.trim().to_string(), false);
            }
        }
        if let Some(hit) = self.model.lock().await.clone() {
            return hit;
        }
        match xai.list_models().await {
            Ok(cat) => match cat.models.first() {
                Some(m) => {
                    let hit = (m.id.clone(), m.send_reasoning());
                    *self.model.lock().await = Some(hit.clone());
                    hit
                }
                None => (xai.model().to_string(), false),
            },
            Err(_) => (xai.model().to_string(), false),
        }
    }

    async fn run(&self, args: Value) -> Result<String> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if query.is_empty() {
            return Err(Error::Tool("grok_search needs a non-empty `query`".into()));
        }
        if query.chars().count() > MAX_QUERY_CHARS {
            return Err(Error::Tool(format!(
                "grok_search query is over {MAX_QUERY_CHARS} characters; ask one focused question"
            )));
        }
        let sources = args.get("sources").and_then(Value::as_str).unwrap_or("both");
        let server_tools = server_tools_for(sources)?;
        if !self.auth_path.exists() {
            return Err(Error::Tool(
                "grok_search needs a Grok login; sign in from Settings (F2) or run `grokaagent login`".into(),
            ));
        }
        let xai = XaiOauthProvider::new(self.auth_path.clone(), None)?;
        let (model, send_reasoning) = self.model(&xai).await;
        let req = CompleteRequest {
            instructions: search_instructions(&chrono::Utc::now().to_rfc3339()),
            input: vec![json!({"role": "user", "content": query})],
            client_tools: Vec::new(),
            server_tools,
            cache_key: format!("grokaagent:grok_search:{}", uuid::Uuid::new_v4()),
            previous_response_id: None,
            store: false,
            reasoning_effort: ReasoningEffort::Low,
            send_reasoning,
            model: model.clone(),
            tool_choice: None,
        };
        let noop_text = |_: &str| {};
        let noop_server = |_: &str, _: &Value| {};
        let resp = xai
            .complete_stream(req, &noop_text, &noop_server, &noop_text)
            .await
            .map_err(|e| Error::Tool(format!("grok_search ({model}) failed: {e}")))?;
        Ok(format_answer(&model, &resp.text, &resp.output_items))
    }
}

fn server_tools_for(sources: &str) -> Result<Vec<String>> {
    Ok(match sources {
        "web" => vec!["web_search".into()],
        "x" => vec!["x_search".into()],
        "both" | "" => vec!["web_search".into(), "x_search".into()],
        other => {
            return Err(Error::Tool(format!(
                "grok_search `sources` must be web, x or both (got `{other}`)"
            )))
        }
    })
}

fn search_instructions(now: &str) -> String {
    format!(
        "You are a search assistant answering a question from another AI agent. \
Current UTC time: {now}.\n\
- Search the web and/or X as needed; prefer primary and recent sources.\n\
- Answer directly in the language of the question. Give concrete facts: names, versions, dates, numbers, quotes.\n\
- Cite sources inline with their URLs. Say when sources disagree, when information may be outdated, or when nothing was found.\n\
- Do not ask follow-up questions; make the best answer from what you can find."
    )
}

/// Answer text, then the cited URLs and the searches Grok ran.
pub fn format_answer(model: &str, text: &str, output_items: &[Value]) -> String {
    let mut sources: Vec<(String, String)> = Vec::new();
    let mut push = |url: &str, title: &str| {
        if url.is_empty() || sources.len() >= MAX_SOURCES || sources.iter().any(|(u, _)| u == url) {
            return;
        }
        sources.push((url.to_string(), title.to_string()));
    };
    let mut searches = 0usize;
    for item in output_items {
        let typ = item.get("type").and_then(Value::as_str).unwrap_or("");
        if typ == "message" {
            for part in item.get("content").and_then(Value::as_array).into_iter().flatten() {
                for ann in part.get("annotations").and_then(Value::as_array).into_iter().flatten() {
                    let url = ann.get("url").and_then(Value::as_str).unwrap_or("");
                    let title = ann.get("title").and_then(Value::as_str).unwrap_or("");
                    push(url, title);
                }
            }
        } else if typ.ends_with("_call") {
            searches += 1;
            let found = item
                .get("action")
                .and_then(|a| a.get("sources"))
                .and_then(Value::as_array);
            for src in found.into_iter().flatten() {
                let url = src.get("url").and_then(Value::as_str).unwrap_or("");
                let title = src.get("title").and_then(Value::as_str).unwrap_or("");
                push(url, title);
            }
        }
    }
    let answer = text.trim();
    let mut out = if answer.is_empty() {
        "(Grok returned no answer text)".to_string()
    } else if answer.chars().count() > MAX_ANSWER_CHARS {
        let cut: String = answer.chars().take(MAX_ANSWER_CHARS).collect();
        format!("{cut}\n…(answer clipped)")
    } else {
        answer.to_string()
    };
    if !sources.is_empty() {
        out.push_str("\n\nSources:");
        for (url, title) in &sources {
            if title.is_empty() || title == url {
                out.push_str(&format!("\n- {url}"));
            } else {
                out.push_str(&format!("\n- {title} — {url}"));
            }
        }
    }
    out.push_str(&format!("\n\n[answered by {model} · {searches} search call(s)]"));
    out
}

impl ClientTool for GrokSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME.into(),
            description: "Ask Grok (xAI) a natural-language question. Grok searches the live web and X (Twitter) and answers with source URLs. \
Use it for current events, recent releases, prices, docs, people, and posts: anything outside the workspace or possibly newer than your training data. \
Ask one self-contained question and include the context Grok needs (names, versions, time range, the form of answer you want). \
Each call is a fresh conversation; Grok does not see this chat."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The question for Grok, in natural language."
                    },
                    "sources": {
                        "type": "string",
                        "enum": ["both", "web", "x"],
                        "description": "Where Grok may search: web, x (X/Twitter posts), or both (default)."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    fn gate(&self) -> Option<&'static str> {
        Some(GATE)
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let args = args.clone();
        Box::pin(async move { self.run(args).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_lists_annotations_then_search_sources_once() {
        let items = vec![
            json!({"type": "web_search_call", "action": {"query": "rust 2026", "sources": [
                {"url": "https://a.example", "title": "A"},
                {"url": "https://b.example"}
            ]}}),
            json!({"type": "x_search_call", "action": {"query": "rust"}}),
            json!({"type": "message", "content": [{"type": "output_text", "text": "Rust 1.99 shipped.",
                "annotations": [{"type": "url_citation", "url": "https://b.example", "title": "B post"}]}]}),
        ];
        let out = format_answer("grok-9", "Rust 1.99 shipped.", &items);
        assert!(out.starts_with("Rust 1.99 shipped."));
        assert!(out.contains("- A — https://a.example"));
        assert_eq!(out.matches("https://b.example").count(), 1, "{out}");
        assert!(out.ends_with("[answered by grok-9 · 2 search call(s)]"), "{out}");
    }

    #[test]
    fn empty_answer_and_bad_sources_are_reported() {
        assert!(format_answer("g", "  ", &[]).starts_with("(Grok returned no answer text)"));
        assert!(server_tools_for("news").is_err());
        assert_eq!(server_tools_for("x").unwrap(), vec!["x_search".to_string()]);
    }

    #[test]
    fn search_follows_the_route_toggle_and_login() {
        use crate::kit::search_tools;
        assert!(search_tools(false, false, true).is_empty());
        assert_eq!(search_tools(true, false, false), vec!["web_search", "x_search"]);
        assert_eq!(search_tools(true, true, true), vec![GATE]);
        assert!(search_tools(true, true, false).is_empty());
    }

    #[test]
    fn registry_offers_grok_search_only_behind_its_gate() {
        let reg = crate::tools::ToolRegistry::new(vec![
            Box::new(crate::tools::NowTool),
            Box::new(GrokSearchTool::new(PathBuf::from("unused"))),
        ]);
        let names = |tools: &[String]| -> Vec<String> {
            reg.specs_for(tools).into_iter().map(|s| s.name).collect()
        };
        assert_eq!(names(&[]), vec!["now"]);
        assert_eq!(names(&["web_search".into()]), vec!["now"]);
        assert_eq!(names(&[GATE.into()]), vec!["now", NAME]);
    }

    #[tokio::test]
    async fn missing_query_or_login_fails_before_any_request() {
        let tool = GrokSearchTool::new(PathBuf::from("Z:/definitely/missing/xai-auth.json"));
        let err = tool.call(&json!({"query": " "})).await.unwrap_err().to_string();
        assert!(err.contains("non-empty"), "{err}");
        let err = tool.call(&json!({"query": "hi"})).await.unwrap_err().to_string();
        assert!(err.contains("Grok login"), "{err}");
    }
}
