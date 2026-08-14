use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::error::{Error, Result};

/// Public Grok-CLI OAuth client. xAI only allowlists this id for device-code.
pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
pub const DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
pub const SCOPE: &str =
    "openid profile email offline_access grok-cli:access api:access conversations:read conversations:write";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const EXPIRY_SKEW_SECS: u64 = 300;

static REFRESH_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AuthFile {
    tokens: TokenSet,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_refresh: Option<String>,
}

pub fn default_auth_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("GROKA_XAI_AUTH_FILE") {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().ok_or_else(|| Error::Auth("cannot resolve home directory".into()))?;
    Ok(home.join(".grokaagent").join("xai-auth.json"))
}

pub fn jwt_exp_unix(access_token: &str) -> Option<u64> {
    let payload = access_token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.get("exp")?.as_u64()
}

pub fn access_token_valid_at(access_token: &str, now: SystemTime) -> bool {
    let Some(exp) = jwt_exp_unix(access_token) else {
        return false;
    };
    let now_unix = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    now_unix + EXPIRY_SKEW_SECS < exp
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes)?;
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

pub fn save_tokens(path: &Path, tokens: &TokenSet) -> Result<()> {
    let file = AuthFile {
        tokens: tokens.clone(),
        last_refresh: Some(chrono::Utc::now().to_rfc3339()),
    };
    let bytes = serde_json::to_vec_pretty(&file)?;
    atomic_write(path, &bytes)?;
    Ok(())
}

pub fn load_tokens(path: &Path) -> Result<TokenSet> {
    let raw = fs::read_to_string(path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            Error::Auth(format!(
                "xAI auth not found at {}; run `grokaagent login`",
                path.display()
            ))
        } else {
            Error::Auth(format!("cannot read {}: {e}", path.display()))
        }
    })?;
    let file: AuthFile = serde_json::from_str(&raw)
        .map_err(|_| Error::Auth(format!("corrupt auth file {}; run `grokaagent login`", path.display())))?;
    if file.tokens.access_token.is_empty() || file.tokens.refresh_token.is_empty() {
        return Err(Error::Auth("auth file missing tokens; run `grokaagent login`".into()));
    }
    Ok(file.tokens)
}

pub fn delete_auth_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn oauth_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("grokaagent/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()?)
}

#[derive(Debug, Deserialize)]
pub struct DeviceLogin {
    device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_interval() -> u64 {
    5
}

/// Public view of an in-flight RFC 8628 device login. `device_code` stays private.
#[derive(Debug, Clone)]
pub struct DevicePending {
    pub user_code: String,
    pub verification_uri: String,
    pub open_url: String,
    device_code: String,
    pub expires_in: u64,
    interval_secs: u64,
}

impl DevicePending {
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs.max(1))
    }

    pub fn bump_interval(&mut self) {
        self.interval_secs = self.interval_secs.saturating_add(5).min(30);
    }
}

#[derive(Debug)]
pub enum DevicePoll {
    Pending,
    SlowDown,
    Success(TokenSet),
    Denied,
    Expired,
    Failed(String),
}

#[derive(Debug, Deserialize)]
struct TokenEndpointResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

fn parse_token_body(body: TokenEndpointResponse) -> Result<TokenSet> {
    if let Some(err) = body.error {
        return Err(Error::Auth(format!(
            "{err}{}",
            body.error_description
                .map(|d| format!(": {d}"))
                .unwrap_or_default()
        )));
    }
    let access_token = body
        .access_token
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Auth("token response missing access_token".into()))?;
    let refresh_token = body
        .refresh_token
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Auth("token response missing refresh_token (xAI rotates it; cannot continue)".into()))?;
    Ok(TokenSet {
        access_token,
        refresh_token,
        id_token: body.id_token.filter(|s| !s.is_empty()),
    })
}

fn host_allowed(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    matches!(
        parsed.host_str(),
        Some("auth.x.ai" | "accounts.x.ai" | "console.x.ai")
    )
}

pub fn open_login_browser(url: &str) {
    try_open_browser(url);
}

fn try_open_browser(url: &str) {
    if !host_allowed(url) {
        return;
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

pub fn parse_device_login_json(text: &str) -> Result<DevicePending> {
    let device: DeviceLogin = serde_json::from_str(text)
        .map_err(|_| Error::Auth("invalid device-code response".into()))?;
    if device.user_code.trim().is_empty() || device.device_code.trim().is_empty() {
        return Err(Error::Auth("invalid device-code response".into()));
    }
    if !host_allowed(&device.verification_uri) {
        return Err(Error::Auth("device login URL is not an xAI host".into()));
    }
    let open_url = device
        .verification_uri_complete
        .clone()
        .filter(|u| host_allowed(u))
        .unwrap_or_else(|| device.verification_uri.clone());
    Ok(DevicePending {
        user_code: device.user_code,
        verification_uri: device.verification_uri,
        open_url,
        device_code: device.device_code,
        expires_in: device.expires_in.max(1),
        interval_secs: device.interval.max(1),
    })
}

pub fn interpret_device_poll_json(status: u16, text: &str) -> DevicePoll {
    let body: TokenEndpointResponse = serde_json::from_str(text).unwrap_or(TokenEndpointResponse {
        access_token: None,
        refresh_token: None,
        id_token: None,
        error: Some(format!("http_{status}")),
        error_description: None,
    });
    match body.error.as_deref() {
        Some("authorization_pending") => DevicePoll::Pending,
        Some("slow_down") => DevicePoll::SlowDown,
        Some("access_denied") | Some("authorization_denied") => DevicePoll::Denied,
        Some("expired_token") => DevicePoll::Expired,
        Some(_) if !(200..300).contains(&status) => {
            DevicePoll::Failed(parse_token_body(body).unwrap_err().to_string())
        }
        _ => {
            if !(200..300).contains(&status) && body.access_token.is_none() {
                DevicePoll::Failed(format!("login poll failed HTTP {status}"))
            } else {
                match parse_token_body(body) {
                    Ok(tokens) => DevicePoll::Success(tokens),
                    Err(e) => DevicePoll::Failed(e.to_string()),
                }
            }
        }
    }
}

pub async fn request_device_login() -> Result<DevicePending> {
    let client = oauth_client()?;
    let resp = client
        .post(DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(Error::Auth(format!(
            "device-code request failed HTTP {status}"
        )));
    }
    parse_device_login_json(&text)
}

pub async fn poll_device_login(pending: &DevicePending) -> Result<DevicePoll> {
    let client = oauth_client()?;
    let resp = client
        .post(TOKEN_URL)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", DEVICE_GRANT),
            ("client_id", CLIENT_ID),
            ("device_code", pending.device_code.as_str()),
        ])
        .send()
        .await?;
    let status = resp.status().as_u16();
    let text = resp.text().await?;
    Ok(interpret_device_poll_json(status, &text))
}

/// RFC 8628 device-code login. Prints the verification URL; never logs tokens.
pub async fn login(path: &Path) -> Result<TokenSet> {
    let mut pending = request_device_login().await?;
    println!("xAI device login");
    println!("  open: {}", pending.open_url);
    println!("  code: {}", pending.user_code);
    open_login_browser(&pending.open_url);

    let deadline = SystemTime::now() + Duration::from_secs(pending.expires_in);
    loop {
        if SystemTime::now() >= deadline {
            return Err(Error::Auth(
                "device login expired; run `grokaagent login` again".into(),
            ));
        }
        sleep(pending.interval()).await;
        match poll_device_login(&pending).await? {
            DevicePoll::Pending => continue,
            DevicePoll::SlowDown => pending.bump_interval(),
            DevicePoll::Success(tokens) => {
                save_tokens(path, &tokens)?;
                println!("logged in");
                println!("auth file: {}", path.display());
                return Ok(tokens);
            }
            DevicePoll::Denied => {
                return Err(Error::Auth("login denied in browser".into()));
            }
            DevicePoll::Expired => {
                return Err(Error::Auth(
                    "device code expired; run `grokaagent login` again".into(),
                ));
            }
            DevicePoll::Failed(msg) => return Err(Error::Auth(msg)),
        }
    }
}

pub async fn valid_access_token(path: &Path) -> Result<String> {
    let client = oauth_client()?;
    let tokens = load_tokens(path)?;
    if access_token_valid_at(&tokens.access_token, SystemTime::now()) {
        return Ok(tokens.access_token);
    }

    let _guard = REFRESH_LOCK.lock().await;
    let tokens = load_tokens(path)?;
    if access_token_valid_at(&tokens.access_token, SystemTime::now()) {
        return Ok(tokens.access_token);
    }

    let resp = client
        .post(TOKEN_URL)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", tokens.refresh_token.as_str()),
        ])
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if status.as_u16() == 403 {
        return Err(Error::Auth(
            "xAI refused this OAuth token for API use (HTTP 403). Login succeeded but the account is not entitled on this surface. Re-login will not help.".into(),
        ));
    }
    if status.as_u16() == 400 || status.as_u16() == 401 {
        return Err(Error::Auth(
            "xAI refresh rejected; run `grokaagent login` again".into(),
        ));
    }
    if !status.is_success() {
        return Err(Error::Auth(format!("token refresh failed HTTP {status}")));
    }
    let body: TokenEndpointResponse = serde_json::from_str(&text)
        .map_err(|_| Error::Auth("invalid refresh response".into()))?;
    let mut refreshed = parse_token_body(body)?;
    if refreshed.id_token.is_none() {
        refreshed.id_token = tokens.id_token;
    }
    save_tokens(path, &refreshed)?;
    Ok(refreshed.access_token)
}

/// Used by tests to inspect mapping without a network.
pub fn map_device_poll_error(error: &str) -> &'static str {
    match error {
        "authorization_pending" => "pending",
        "slow_down" => "slow_down",
        "access_denied" | "authorization_denied" => "denied",
        "expired_token" => "expired",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt_with_exp(exp: u64) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#).as_bytes());
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn jwt_exp_reads_claim() {
        let token = jwt_with_exp(1_900_000_000);
        assert_eq!(jwt_exp_unix(&token), Some(1_900_000_000));
    }

    #[test]
    fn jwt_garbage_has_no_exp() {
        assert_eq!(jwt_exp_unix("not-a-jwt"), None);
        assert_eq!(jwt_exp_unix("a.%%%"), None);
    }

    #[test]
    fn valid_when_exp_beyond_skew() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let token = jwt_with_exp(1_000_000 + EXPIRY_SKEW_SECS + 10);
        assert!(access_token_valid_at(&token, now));
    }

    #[test]
    fn invalid_inside_skew_window() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let token = jwt_with_exp(1_000_000 + EXPIRY_SKEW_SECS - 1);
        assert!(!access_token_valid_at(&token, now));
    }

    #[test]
    fn refresh_response_without_refresh_token_is_error() {
        let body = TokenEndpointResponse {
            access_token: Some("a".into()),
            refresh_token: None,
            id_token: None,
            error: None,
            error_description: None,
        };
        assert!(parse_token_body(body).is_err());
    }

    #[test]
    fn token_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("xai-auth.json");
        let tokens = TokenSet {
            access_token: "acc".into(),
            refresh_token: "ref".into(),
            id_token: Some("id".into()),
        };
        save_tokens(&path, &tokens).unwrap();
        let loaded = load_tokens(&path).unwrap();
        assert_eq!(loaded.access_token, "acc");
        assert_eq!(loaded.refresh_token, "ref");
        assert_eq!(loaded.id_token.as_deref(), Some("id"));
    }

    #[test]
    fn missing_auth_file_asks_for_login() {
        let err = load_tokens(Path::new("/no/such/groka-auth.json")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("login"), "{msg}");
    }

    #[test]
    fn host_allowlist_rejects_http_and_foreign() {
        assert!(host_allowed("https://auth.x.ai/oauth2/device"));
        assert!(host_allowed("https://accounts.x.ai/connect"));
        assert!(!host_allowed("http://auth.x.ai/oauth2/device"));
        assert!(!host_allowed("https://evil.example/phish"));
    }

    #[test]
    fn device_poll_errors_are_classified() {
        assert_eq!(map_device_poll_error("authorization_pending"), "pending");
        assert_eq!(map_device_poll_error("slow_down"), "slow_down");
        assert_eq!(map_device_poll_error("access_denied"), "denied");
        assert_eq!(map_device_poll_error("expired_token"), "expired");
    }

    #[test]
    fn parse_device_login_json_allows_xai_and_rejects_foreign() {
        let ok = parse_device_login_json(
            r#"{
                "device_code": "dc",
                "user_code": "ABCD-EFGH",
                "verification_uri": "https://auth.x.ai/device",
                "verification_uri_complete": "https://auth.x.ai/device?user_code=ABCD-EFGH",
                "expires_in": 600,
                "interval": 5
            }"#,
        )
        .unwrap();
        assert_eq!(ok.user_code, "ABCD-EFGH");
        assert_eq!(
            ok.open_url,
            "https://auth.x.ai/device?user_code=ABCD-EFGH"
        );
        assert!(parse_device_login_json(
            r#"{
                "device_code": "dc",
                "user_code": "ABCD-EFGH",
                "verification_uri": "https://evil.example/phish",
                "expires_in": 600,
                "interval": 5
            }"#
        )
        .is_err());
        assert!(parse_device_login_json(
            r#"{
                "device_code": "dc",
                "user_code": "ABCD-EFGH",
                "verification_uri": "http://auth.x.ai/device",
                "expires_in": 600,
                "interval": 5
            }"#
        )
        .is_err());
        let dropped_complete = parse_device_login_json(
            r#"{
                "device_code": "dc",
                "user_code": "ABCD-EFGH",
                "verification_uri": "https://auth.x.ai/device",
                "verification_uri_complete": "https://evil.example/phish",
                "expires_in": 600,
                "interval": 5
            }"#,
        )
        .unwrap();
        assert_eq!(dropped_complete.open_url, "https://auth.x.ai/device");
    }

    #[test]
    fn interpret_device_poll_json_covers_rfc8628_states() {
        assert!(matches!(
            interpret_device_poll_json(400, r#"{"error":"authorization_pending"}"#),
            DevicePoll::Pending
        ));
        assert!(matches!(
            interpret_device_poll_json(400, r#"{"error":"slow_down"}"#),
            DevicePoll::SlowDown
        ));
        assert!(matches!(
            interpret_device_poll_json(400, r#"{"error":"access_denied"}"#),
            DevicePoll::Denied
        ));
        assert!(matches!(
            interpret_device_poll_json(400, r#"{"error":"expired_token"}"#),
            DevicePoll::Expired
        ));
        match interpret_device_poll_json(
            200,
            r#"{"access_token":"acc","refresh_token":"ref","id_token":"id"}"#,
        ) {
            DevicePoll::Success(t) => {
                assert_eq!(t.access_token, "acc");
                assert_eq!(t.refresh_token, "ref");
            }
            other => panic!("expected success, got {other:?}"),
        }
        match interpret_device_poll_json(500, "not-json") {
            DevicePoll::Failed(msg) => assert!(msg.contains("500"), "{msg}"),
            other => panic!("expected failed, got {other:?}"),
        }
    }

    #[test]
    fn parse_token_body_rejects_oauth_error() {
        let body = TokenEndpointResponse {
            access_token: None,
            refresh_token: None,
            id_token: None,
            error: Some("invalid_grant".into()),
            error_description: Some("rotated".into()),
        };
        let err = parse_token_body(body).unwrap_err().to_string();
        assert!(err.contains("invalid_grant"));
        assert!(err.contains("rotated"));
    }
}
