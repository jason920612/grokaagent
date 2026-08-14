//! Minimal A2A v1 HTTP+JSON types and client. Hand-rolled to the spec JSON.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};

pub const PROTOCOL_VERSION: &str = "0.3.0";
pub const TASK_COMPLETED: &str = "TASK_STATE_COMPLETED";
pub const TASK_FAILED: &str = "TASK_STATE_FAILED";
pub const TASK_CANCELED: &str = "TASK_STATE_CANCELED";
pub const TASK_WORKING: &str = "TASK_STATE_WORKING";
pub const TASK_INPUT_REQUIRED: &str = "TASK_STATE_INPUT_REQUIRED";
pub const ROLE_USER: &str = "ROLE_USER";
pub const ROLE_AGENT: &str = "ROLE_AGENT";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handshake {
    pub v: u32,
    pub agent_card_url: String,
}

impl Handshake {
    pub fn parse_line(line: &str) -> Result<Self> {
        let h: Handshake = serde_json::from_str(line.trim())
            .map_err(|_| Error::A2a(format!("bad handshake: {}", line.trim())))?;
        if h.v != 1 {
            return Err(Error::A2a(format!("unsupported handshake v={}", h.v)));
        }
        if !h.agent_card_url.starts_with("http://127.0.0.1:")
            && !h.agent_card_url.starts_with("http://localhost:")
        {
            return Err(Error::A2a("handshake URL must be loopback http".into()));
        }
        Ok(h)
    }

    pub fn origin(&self) -> Result<String> {
        origin_from_card_url(&self.agent_card_url)
    }
}

pub fn origin_from_card_url(card_url: &str) -> Result<String> {
    let trimmed = card_url.trim_end_matches('/');
    if let Some(i) = trimmed.find("/.well-known/") {
        return Ok(trimmed[..i].to_string());
    }
    Err(Error::A2a("agent card URL missing /.well-known/".into()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    pub protocol_version: String,
    pub url: String,
    pub version: String,
    pub capabilities: AgentCapabilities,
    pub default_input_modes: Vec<String>,
    pub default_output_modes: Vec<String>,
    pub skills: Vec<AgentSkill>,
    pub supported_interfaces: Vec<AgentInterface>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    pub streaming: bool,
    pub push_notifications: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInterface {
    pub url: String,
    pub protocol_binding: String,
    pub protocol_version: String,
}

pub fn local_card(name: &str, origin: &str) -> AgentCard {
    AgentCard {
        name: name.to_string(),
        description: "grokaagent A2A worker".into(),
        protocol_version: PROTOCOL_VERSION.into(),
        url: origin.to_string(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: AgentCapabilities {
            streaming: false,
            push_notifications: false,
        },
        default_input_modes: vec!["text/plain".into()],
        default_output_modes: vec!["text/plain".into()],
        skills: vec![AgentSkill {
            id: "task".into(),
            name: "task".into(),
            description: "Run a local agent task".into(),
            tags: vec!["agent".into()],
        }],
        supported_interfaces: vec![AgentInterface {
            url: origin.to_string(),
            protocol_binding: "HTTP+JSON".into(),
            protocol_version: PROTOCOL_VERSION.into(),
        }],
    }
}

pub fn user_message(text: &str, context_id: Option<&str>) -> Value {
    let mut m = json!({
        "role": ROLE_USER,
        "messageId": uuid::Uuid::new_v4().to_string(),
        "parts": [{"text": text}],
    });
    if let Some(cid) = context_id {
        m["contextId"] = json!(cid);
    }
    m
}

pub fn extract_text(value: &Value) -> String {
    let msg = value.get("message").unwrap_or(value);
    if let Some(parts) = msg.get("parts").and_then(Value::as_array) {
        let mut out = String::new();
        for p in parts {
            if let Some(t) = p.get("text").and_then(Value::as_str) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    msg.get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

pub fn context_id_of(value: &Value) -> Option<String> {
    let msg = value.get("message").unwrap_or(value);
    msg.get("contextId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub fn task_state(task: &Value) -> &str {
    task.get("status")
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

pub fn artifact_text(task: &Value) -> String {
    let Some(arts) = task.get("artifacts").and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for a in arts {
        if let Some(parts) = a.get("parts").and_then(Value::as_array) {
            for p in parts {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
        }
    }
    out
}

pub fn completed_task(text: &str, context_id: &str) -> Value {
    json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "contextId": context_id,
        "status": {"state": TASK_COMPLETED},
        "artifacts": [{
            "artifactId": uuid::Uuid::new_v4().to_string(),
            "name": "reply",
            "parts": [{"text": text}]
        }]
    })
}

pub fn failed_task(message: &str, context_id: &str) -> Value {
    json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "contextId": context_id,
        "status": {"state": TASK_FAILED, "message": message},
        "artifacts": [{
            "artifactId": uuid::Uuid::new_v4().to_string(),
            "name": "error",
            "parts": [{"text": message}]
        }]
    })
}

pub fn is_terminal(state: &str) -> bool {
    matches!(state, "TASK_STATE_COMPLETED" | "TASK_STATE_FAILED" | "TASK_STATE_CANCELED")
}

#[derive(Clone)]
pub struct A2aClient {
    http: reqwest::Client,
}

impl A2aClient {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(180))
                .build()?,
        })
    }

    pub async fn fetch_card(&self, url: &str) -> Result<AgentCard> {
        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(Error::A2a(format!("agent card HTTP {status}: {text}")));
        }
        Ok(serde_json::from_str(&text)?)
    }

    pub async fn send_text(
        &self,
        origin: &str,
        text: &str,
        context_id: Option<&str>,
    ) -> Result<Value> {
        let body = json!({ "message": user_message(text, context_id) });
        let url = format!("{}/message:send", origin.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(Error::A2a(format!("message:send HTTP {status}: {text}")));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|_| Error::A2a("message:send returned non-JSON".into()))?;
        v.get("task")
            .cloned()
            .ok_or_else(|| Error::A2a("message:send missing task".into()))
    }

    pub async fn get_task(&self, origin: &str, id: &str) -> Result<Value> {
        let url = format!("{}/tasks/{id}", origin.trim_end_matches('/'));
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(Error::A2a(format!("get task HTTP {status}: {text}")));
        }
        Ok(serde_json::from_str(&text)?)
    }

    pub async fn cancel_task(&self, origin: &str, id: &str) -> Result<()> {
        let url = format!("{}/tasks/{id}/cancel", origin.trim_end_matches('/'));
        let resp = self.http.post(&url).send().await?;
        if !resp.status().is_success() {
            return Err(Error::A2a(format!("cancel HTTP {}", resp.status())));
        }
        Ok(())
    }

    pub async fn wait_task(
        &self,
        origin: &str,
        mut task: Value,
        timeout: std::time::Duration,
    ) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let state = task_state(&task);
            if is_terminal(state) {
                return Ok(task);
            }
            if tokio::time::Instant::now() >= deadline {
                if let Some(id) = task.get("id").and_then(Value::as_str) {
                    let _ = self.cancel_task(origin, id).await;
                }
                return Err(Error::A2a("child task timed out".into()));
            }
            let id = task
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::A2a("task missing id".into()))?;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            task = self.get_task(origin, id).await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_rejects_non_loopback() {
        let err = Handshake::parse_line(r#"{"v":1,"agent_card_url":"http://example.com/.well-known/agent-card.json"}"#)
            .unwrap_err();
        assert!(err.to_string().contains("loopback"));
    }

    #[test]
    fn handshake_origin() {
        let h = Handshake::parse_line(
            r#"{"v":1,"agent_card_url":"http://127.0.0.1:3456/.well-known/agent-card.json"}"#,
        )
        .unwrap();
        assert_eq!(h.origin().unwrap(), "http://127.0.0.1:3456");
    }

    #[test]
    fn extract_text_from_parts() {
        let v = json!({"message":{"parts":[{"text":"hi"},{"text":"there"}]}});
        assert_eq!(extract_text(&v), "hi\nthere");
    }
}
