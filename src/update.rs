//! Self-update from GitHub Releases. Replaces the running binary after SHA-256 check.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::install;

pub const DEFAULT_REPO: &str = "jason920612/grokaagent";
pub const CHECK_INTERVAL_SECS: u64 = 6 * 60 * 60;
const MAX_ASSET_BYTES: u64 = 80 * 1024 * 1024;
const SUMS_NAME: &str = "SHA256SUMS";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Updated { from: String, to: String },
    UpToDate { version: String },
    Skipped { reason: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

impl Release {
    pub fn version(&self) -> Result<(u64, u64, u64)> {
        parse_version(&self.tag_name).ok_or_else(|| {
            Error::Update(format!("unrecognized release tag `{}`", self.tag_name))
        })
    }

    pub fn version_string(&self) -> String {
        self.tag_name.trim().trim_start_matches('v').to_string()
    }

    pub fn is_newer_than(&self, current: &str) -> Result<bool> {
        let cur = parse_version(current).ok_or_else(|| {
            Error::Update(format!("unrecognized current version `{current}`"))
        })?;
        Ok(self.version()? > cur)
    }

    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

pub fn repo() -> String {
    env::var("GROKA_UPDATE_REPO").unwrap_or_else(|_| DEFAULT_REPO.to_string())
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn current_target() -> Option<&'static str> {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("x86_64-pc-windows-msvc")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else {
        None
    }
}

pub fn binary_asset_name(target: &str) -> String {
    if target.contains("windows") {
        format!("grokaagent-{target}.exe")
    } else {
        format!("grokaagent-{target}")
    }
}

pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches('v');
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

pub fn parse_release_json(json: &str) -> Result<Release> {
    Ok(serde_json::from_str(json)?)
}

pub fn parse_sha256sums(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut bits = line.split_whitespace();
        let Some(hash) = bits.next() else {
            continue;
        };
        let Some(name) = bits.next() else {
            continue;
        };
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let name = name.strip_prefix('*').unwrap_or(name);
        out.insert(name.to_string(), hash.to_ascii_lowercase());
    }
    out
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub fn hashes_match(actual: &str, expected: &str) -> bool {
    actual.eq_ignore_ascii_case(expected)
}

pub fn is_dev_exe(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "target")
        && path.components().any(|c| {
            let s = c.as_os_str();
            s == "debug" || s == "release"
        })
}

pub fn check_is_due(last: Option<u64>, now: u64, interval: u64) -> bool {
    match last {
        None => true,
        Some(t) => now.saturating_sub(t) >= interval,
    }
}

pub fn auto_update_allowed() -> bool {
    match env::var("GROKA_NO_UPDATE") {
        Ok(v) if matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "YES") => false,
        _ => true,
    }
}

fn groka_dir() -> Result<PathBuf> {
    if let Ok(p) = env::var("GROKA_HOME") {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().ok_or_else(|| {
        Error::Update("cannot resolve home directory".into())
    })?;
    Ok(home.join(".grokaagent"))
}

fn stamp_path() -> Result<PathBuf> {
    Ok(groka_dir()?.join("last-update-check"))
}

fn read_stamp() -> Option<u64> {
    let text = fs::read_to_string(stamp_path().ok()?).ok()?;
    text.trim().parse().ok()
}

fn write_stamp() {
    let Ok(path) = stamp_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let _ = fs::write(path, format!("{now}\n"));
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn http_client(timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("grokaagent/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()?)
}

pub async fn fetch_latest(client: &reqwest::Client, repo: &str) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let res = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(Error::Update("no GitHub release found".into()));
    }
    if !res.status().is_success() {
        return Err(Error::Update(format!(
            "GitHub releases API returned {}",
            res.status()
        )));
    }
    let text = res.text().await?;
    parse_release_json(&text)
}

async fn download(client: &reqwest::Client, url: &str, max: u64) -> Result<Vec<u8>> {
    let res = client.get(url).send().await?;
    if !res.status().is_success() {
        return Err(Error::Update(format!(
            "download failed ({}) for {url}",
            res.status()
        )));
    }
    if let Some(len) = res.content_length() {
        if len > max {
            return Err(Error::Update(format!(
                "asset is {len} bytes; refusing anything over {max}"
            )));
        }
    }
    let bytes = res.bytes().await?;
    if bytes.len() as u64 > max {
        return Err(Error::Update(format!(
            "asset is {} bytes; refusing anything over {max}",
            bytes.len()
        )));
    }
    Ok(bytes.to_vec())
}

pub fn apply_bytes(bytes: &[u8], dest: &Path) -> Result<PathBuf> {
    let dir = dest.parent().ok_or_else(|| {
        Error::Update("current executable has no parent directory".into())
    })?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".grokaagent-update-{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    let out = install::replace_exe(&tmp, dest);
    let _ = fs::remove_file(&tmp);
    out
}

async fn download_and_replace(release: &Release, target: &str) -> Result<Outcome> {
    let asset_name = binary_asset_name(target);
    let asset = release.asset(&asset_name).ok_or_else(|| {
        Error::Update(format!("release {} has no asset `{asset_name}`", release.tag_name))
    })?;
    if asset.size > MAX_ASSET_BYTES {
        return Err(Error::Update(format!(
            "`{asset_name}` is {} bytes; refusing anything over {MAX_ASSET_BYTES}",
            asset.size
        )));
    }
    let sums = release.asset(SUMS_NAME).ok_or_else(|| {
        Error::Update("release is missing SHA256SUMS".into())
    })?;
    let client = http_client(Duration::from_secs(120))?;
    let sums_bytes = download(&client, &sums.browser_download_url, 64 * 1024).await?;
    let sums_text = String::from_utf8(sums_bytes).map_err(|_| {
        Error::Update("SHA256SUMS is not UTF-8".into())
    })?;
    let sums_map = parse_sha256sums(&sums_text);
    let expected = sums_map
        .get(&asset_name)
        .cloned()
        .ok_or_else(|| Error::Update(format!("SHA256SUMS has no entry for `{asset_name}`")))?;
    eprintln!(
        "downloading grokaagent {} ({asset_name})…",
        release.version_string()
    );
    let bytes = download(&client, &asset.browser_download_url, MAX_ASSET_BYTES).await?;
    let actual = sha256_hex(&bytes);
    if !hashes_match(&actual, &expected) {
        return Err(Error::Update(format!(
            "checksum mismatch for `{asset_name}` (got {actual}, expected {expected})"
        )));
    }
    let dest = env::current_exe()?;
    apply_bytes(&bytes, &dest)?;
    Ok(Outcome::Updated {
        from: current_version().to_string(),
        to: release.version_string(),
    })
}

/// Always talks to GitHub. Used by `grokaagent update`.
pub async fn update_now() -> Result<Outcome> {
    let Some(target) = current_target() else {
        return Err(Error::Update(
            "this OS/arch is not a published release target".into(),
        ));
    };
    let client = http_client(Duration::from_secs(20))?;
    let release = fetch_latest(&client, &repo()).await?;
    if release.prerelease {
        return Ok(Outcome::Skipped {
            reason: "latest GitHub release is marked prerelease".into(),
        });
    }
    if !release.is_newer_than(current_version())? {
        return Ok(Outcome::UpToDate {
            version: current_version().to_string(),
        });
    }
    download_and_replace(&release, target).await
}

/// Quiet, rate-limited. Failures become `Skipped` so the TUI still opens.
pub async fn auto_update() -> Outcome {
    if !auto_update_allowed() {
        return Outcome::Skipped {
            reason: "GROKA_NO_UPDATE is set".into(),
        };
    }
    let exe = match env::current_exe() {
        Ok(p) => p,
        Err(_) => {
            return Outcome::Skipped {
                reason: "cannot resolve current executable".into(),
            };
        }
    };
    if is_dev_exe(&exe) {
        return Outcome::Skipped {
            reason: "dev build (target/debug or target/release)".into(),
        };
    }
    if !check_is_due(read_stamp(), now_unix(), CHECK_INTERVAL_SECS) {
        return Outcome::Skipped {
            reason: "checked recently".into(),
        };
    }
    match update_now().await {
        Ok(outcome) => {
            write_stamp();
            outcome
        }
        Err(e) => Outcome::Skipped {
            reason: e.to_string(),
        },
    }
}

/// Replace this process with the (possibly new) binary. Windows waits on a child.
pub fn reexec() -> Result<()> {
    let exe = env::current_exe()?;
    let args: Vec<String> = env::args().skip(1).collect();
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&args);
    cmd.env("GROKA_NO_UPDATE", "1");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(err.into())
    }
    #[cfg(windows)]
    {
        let status = cmd.status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (exe, cmd);
        Err(Error::Update("re-exec is not supported on this OS".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "tag_name": "v0.1.1",
        "prerelease": false,
        "assets": [
            {
                "name": "SHA256SUMS",
                "browser_download_url": "https://example.test/SHA256SUMS",
                "size": 200
            },
            {
                "name": "grokaagent-x86_64-pc-windows-msvc.exe",
                "browser_download_url": "https://example.test/win.exe",
                "size": 12
            },
            {
                "name": "grokaagent-x86_64-unknown-linux-gnu",
                "browser_download_url": "https://example.test/linux",
                "size": 12
            },
            {
                "name": "grokaagent-aarch64-apple-darwin",
                "browser_download_url": "https://example.test/mac",
                "size": 12
            }
        ]
    }"#;

    #[test]
    fn parse_version_strips_v_prefix() {
        assert_eq!(parse_version("v0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_version("0.1.1"), Some((0, 1, 1)));
        assert_eq!(parse_version("1.0.0-beta"), None);
        assert_eq!(parse_version("nope"), None);
    }

    #[test]
    fn newer_release_is_detected() {
        let rel = parse_release_json(SAMPLE).unwrap();
        assert!(rel.is_newer_than("0.1.0").unwrap());
        assert!(!rel.is_newer_than("0.1.1").unwrap());
        assert!(!rel.is_newer_than("v0.2.0").unwrap());
    }

    #[test]
    fn asset_names_match_published_layout() {
        assert_eq!(
            binary_asset_name("x86_64-pc-windows-msvc"),
            "grokaagent-x86_64-pc-windows-msvc.exe"
        );
        assert_eq!(
            binary_asset_name("x86_64-unknown-linux-gnu"),
            "grokaagent-x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            binary_asset_name("aarch64-apple-darwin"),
            "grokaagent-aarch64-apple-darwin"
        );
        let rel = parse_release_json(SAMPLE).unwrap();
        assert!(rel.asset(&binary_asset_name("x86_64-pc-windows-msvc")).is_some());
        assert!(rel.asset(&binary_asset_name("x86_64-unknown-linux-gnu")).is_some());
        assert!(rel.asset(&binary_asset_name("aarch64-apple-darwin")).is_some());
    }

    #[test]
    fn sha256sums_parse_gnu_and_star_names() {
        let text = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  grokaagent-x86_64-unknown-linux-gnu
BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB *grokaagent-x86_64-pc-windows-msvc.exe
# comment
not-a-hash  skip-me
";
        let map = parse_sha256sums(text);
        assert_eq!(
            map.get("grokaagent-x86_64-unknown-linux-gnu").unwrap(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(
            map.get("grokaagent-x86_64-pc-windows-msvc.exe").unwrap(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
        assert!(!map.contains_key("skip-me"));
    }

    #[test]
    fn sha256_hex_matches_known_empty() {
        // SHA-256 of empty input, from FIPS 180-4.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(hashes_match(
            &sha256_hex(b"groka"),
            &sha256_hex(b"groka").to_ascii_uppercase()
        ));
        assert!(!hashes_match(&sha256_hex(b"a"), &sha256_hex(b"b")));
    }

    #[test]
    fn apply_bytes_replaces_destination() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("grokaagent");
        fs::write(&dest, b"old").unwrap();
        apply_bytes(b"new-bin", &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"new-bin");
    }

    #[test]
    fn dev_exe_detection_uses_cargo_target_layout() {
        assert!(is_dev_exe(Path::new("C:/src/target/debug/grokaagent.exe")));
        assert!(is_dev_exe(Path::new("/src/target/release/grokaagent")));
        assert!(!is_dev_exe(Path::new("/home/x/.grokaagent/bin/grokaagent")));
        assert!(!is_dev_exe(Path::new("C:/Users/x/.grokaagent/bin/grokaagent.exe")));
    }

    #[test]
    fn check_interval_is_due_when_missing_or_stale() {
        assert!(check_is_due(None, 100, 50));
        assert!(!check_is_due(Some(80), 100, 50));
        assert!(check_is_due(Some(40), 100, 50));
    }
}
