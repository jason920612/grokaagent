//! Saved connection: Grok OAuth or an OpenAI-compatible base URL + model.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    Xai,
    Openai,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Xai => "xai",
            Self::Openai => "openai",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "xai" | "grok" | "grok-cli" => Some(Self::Xai),
            "openai" | "openai-compat" | "custom" | "oai" => Some(Self::Openai),
            _ => None,
        }
    }

    pub fn is_openai(self) -> bool {
        matches!(self, Self::Openai)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub kind: ProviderKind,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    /// 0 = look up from the model name (Grok) or 128k (custom).
    #[serde(default)]
    pub context_window: u32,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: ProviderKind::Xai,
            base_url: String::new(),
            model: String::new(),
            api_key: String::new(),
            context_window: 0,
        }
    }
}

impl ProviderConfig {
    pub fn load() -> Self {
        let mut cfg = groka_dir()
            .ok()
            .map(|d| load_at(&d))
            .unwrap_or_default();
        overlay_from_vars(&mut cfg, |k| std::env::var(k).ok());
        cfg
    }

    pub fn save(&self) -> Result<()> {
        save_at(&groka_dir()?, self)
    }

    pub fn ready(&self, xai_logged_in: bool) -> bool {
        if self.route().is_openai() {
            return !self.base_url.trim().is_empty() && !self.effective_model().is_empty();
        }
        if !Self::looks_like_grok(self.effective_model()) {
            return false;
        }
        xai_logged_in
    }

    pub fn effective_model(&self) -> &str {
        let m = self.model.trim();
        if !m.is_empty() {
            return m;
        }
        if self.kind == ProviderKind::Xai {
            "grok-4.6"
        } else {
            ""
        }
    }

    /// Grok ids are `grok-…`. Anything else is a custom-API model.
    pub fn looks_like_grok(model: &str) -> bool {
        model.trim().to_ascii_lowercase().starts_with("grok")
    }

    /// Which backend should actually serve this config.
    ///
    /// A non-Grok model name must never be sent to xAI, even if Settings still
    /// says "Grok" — that is how `Qwen…` ended up as `xAI HTTP 400 Model not found`.
    pub fn route(&self) -> ProviderKind {
        self.route_for(self.effective_model())
    }

    pub fn route_for(&self, model: &str) -> ProviderKind {
        if self.kind.is_openai() {
            return ProviderKind::Openai;
        }
        let m = model.trim();
        let m = if m.is_empty() {
            self.effective_model()
        } else {
            m
        };
        if !self.base_url.trim().is_empty() && !Self::looks_like_grok(m) {
            ProviderKind::Openai
        } else {
            ProviderKind::Xai
        }
    }

    pub fn missing_endpoint_error(model: &str) -> String {
        format!("「{model}」不是 Grok 模型。請在設定切到「自訂 API」並填端點。")
    }

    pub fn window_tokens(&self) -> u32 {
        if self.context_window > 0 {
            self.context_window
        } else if self.route().is_openai() {
            crate::compact::DEFAULT_WINDOW
        } else {
            0
        }
    }

    pub fn apply_cli(
        &mut self,
        base_url: Option<String>,
        api_key: Option<String>,
        context: Option<&str>,
        model: Option<&str>,
    ) {
        if let Some(u) = base_url.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            self.kind = ProviderKind::Openai;
            self.base_url = u;
        }
        if let Some(k) = api_key {
            self.api_key = k;
        }
        if let Some(m) = model.map(str::trim).filter(|s| !s.is_empty()) {
            self.model = m.to_string();
        }
        if let Some(c) = context {
            if let Some(n) = crate::compact::parse_window(c) {
                self.context_window = n;
            }
        }
    }
}

pub fn groka_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("GROKA_HOME") {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().ok_or_else(|| Error::Auth("cannot resolve home directory".into()))?;
    Ok(home.join(".grokaagent"))
}

pub fn default_path() -> Result<PathBuf> {
    Ok(groka_dir()?.join("provider.json"))
}

pub fn load_at(dir: &Path) -> ProviderConfig {
    let path = dir.join("provider.json");
    let Ok(bytes) = fs::read(&path) else {
        return ProviderConfig::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

pub fn save_at(dir: &Path, cfg: &ProviderConfig) -> Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join("provider.json");
    let bytes = serde_json::to_vec_pretty(cfg)?;
    atomic_write(&path, &bytes)?;
    Ok(())
}

pub fn overlay_from_vars(cfg: &mut ProviderConfig, get: impl Fn(&str) -> Option<String>) {
    if let Some(kind) = get("GROKA_PROVIDER").and_then(|s| ProviderKind::parse(&s)) {
        cfg.kind = kind;
    }
    let base = first(&get, &["GROKA_API_BASE", "OPENAI_BASE_URL", "OPENAI_API_BASE"]);
    if let Some(base) = base {
        cfg.base_url = base;
        if get("GROKA_PROVIDER").is_none() {
            cfg.kind = ProviderKind::Openai;
        }
    }
    if let Some(key) = first(&get, &["GROKA_API_KEY", "OPENAI_API_KEY"]) {
        cfg.api_key = key;
    }
    if let Some(model) = get("GROKA_MODEL").filter(|s| !s.trim().is_empty()) {
        cfg.model = model;
    }
    if let Some(raw) = first(&get, &["GROKA_CONTEXT_WINDOW", "GROKA_CONTEXT"]) {
        if let Some(n) = crate::compact::parse_window(&raw) {
            cfg.context_window = n;
        }
    }
}

fn first(get: &impl Fn(&str) -> Option<String>, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(v) = get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            return Some(v);
        }
    }
    None
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ProviderConfig {
            kind: ProviderKind::Openai,
            base_url: "http://127.0.0.1:40056/v1".into(),
            model: "Qwen3.8-27B-ABLITERATED-Q8_0".into(),
            api_key: "sk-test".into(),
            context_window: 262_144,
        };
        save_at(dir.path(), &cfg).unwrap();
        let loaded = load_at(dir.path());
        assert_eq!(loaded, cfg);
        assert!(loaded.ready(false));
    }

    #[test]
    fn env_base_implies_openai() {
        let mut cfg = ProviderConfig::default();
        overlay_from_vars(&mut cfg, |k| match k {
            "GROKA_API_BASE" => Some("http://127.0.0.1:40056/v1".into()),
            "GROKA_MODEL" => Some("Qwen3.8-27B-ABLITERATED-Q8_0".into()),
            "GROKA_CONTEXT_WINDOW" => Some("262K".into()),
            _ => None,
        });
        assert_eq!(cfg.kind, ProviderKind::Openai);
        assert_eq!(cfg.base_url, "http://127.0.0.1:40056/v1");
        assert_eq!(cfg.model, "Qwen3.8-27B-ABLITERATED-Q8_0");
        assert_eq!(cfg.context_window, 262 * 1024);
        assert!(cfg.ready(false));
    }

    #[test]
    fn xai_needs_login_openai_needs_endpoint() {
        let xai = ProviderConfig::default();
        assert!(!xai.ready(false));
        assert!(xai.ready(true));
        let mut oai = ProviderConfig {
            kind: ProviderKind::Openai,
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "m".into(),
            ..Default::default()
        };
        assert!(oai.ready(false));
        oai.model.clear();
        assert!(!oai.ready(false));
    }

    #[test]
    fn qwen_model_routes_to_openai_even_when_kind_is_still_xai() {
        let cfg = ProviderConfig {
            kind: ProviderKind::Xai,
            base_url: "http://127.0.0.1:40056/v1".into(),
            model: "Qwen3.8-27B-ABLITERATED-Q8_0".into(),
            ..Default::default()
        };
        assert_eq!(cfg.route(), ProviderKind::Openai);
        assert!(cfg.ready(false));
        let mut no_url = cfg.clone();
        no_url.base_url.clear();
        assert_eq!(no_url.route(), ProviderKind::Xai);
        assert!(!no_url.ready(true), "xAI login must not send Qwen to Grok");
        assert!(ProviderConfig::looks_like_grok("grok-4.6"));
        assert!(!ProviderConfig::looks_like_grok("Qwen3.8-27B-ABLITERATED-Q8_0"));
    }
}
