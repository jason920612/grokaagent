//! Which shell runs workspace commands.
//!
//! Models are trained on bash, so `bash` is the default everywhere. On
//! Windows that is Git Bash when installed, otherwise a pinned busybox-w32
//! build downloaded once into the grokaagent home (hash-checked). `cmd` and
//! `powershell` stay available for Windows-native work.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Cmd,
    PowerShell,
}

impl Shell {
    pub fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Cmd => "cmd",
            Self::PowerShell => "powershell",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bash" | "sh" => Some(Self::Bash),
            "cmd" | "cmd.exe" => Some(Self::Cmd),
            "powershell" | "pwsh" | "ps" => Some(Self::PowerShell),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BashKind {
    /// bash/sh on a Unix system.
    System,
    GitBash,
    Busybox,
}

#[derive(Clone, Debug)]
pub struct BashRuntime {
    pub exe: PathBuf,
    pub kind: BashKind,
}

impl BashRuntime {
    pub fn label(&self) -> &'static str {
        match self.kind {
            BashKind::System => "bash",
            BashKind::GitBash => "Git Bash",
            BashKind::Busybox => "busybox-w32 sh (bash-compatible subset)",
        }
    }
}

/// busybox-w32 (UTF-8 build), pinned by hash. https://frippery.org/busybox/
const BUSYBOX_URL: &str = "https://frippery.org/files/busybox/busybox-w64u-FRP-6075-g169694ebd.exe";
const BUSYBOX_SHA256: &str = "6e263d154d8548d1eb936f65d1d8312c80df31c45974e48d6335e4dcc0f4f34c";
const BUSYBOX_FILE: &str = "busybox-w64u-FRP-6075.exe";

/// Windows bash lacks `python3` when only `python` is installed.
const WINDOWS_PRELUDE: &str =
    "command -v python3 >/dev/null 2>&1 || python3() { python \"$@\"; }\n";

static RUNTIME: OnceLock<Option<BashRuntime>> = OnceLock::new();

/// The bash this process uses, if one was found (or prepared).
pub fn bash() -> Option<&'static BashRuntime> {
    RUNTIME.get_or_init(find_local).as_ref()
}

/// Find or fetch bash before the first command. Never fails: without bash,
/// commands default to cmd on Windows and the tool descriptions say so.
pub async fn prepare() -> Option<&'static BashRuntime> {
    if let Some(found) = RUNTIME.get() {
        return found.as_ref();
    }
    let found = match find_local() {
        Some(rt) => Some(rt),
        None if cfg!(windows) && std::env::var_os("GROKA_NO_BASH_DOWNLOAD").is_none() => {
            download_busybox().await.ok()
        }
        None => None,
    };
    let _ = RUNTIME.set(found);
    bash()
}

/// Default shell for a command that did not ask for one.
pub fn default_shell() -> Shell {
    if bash().is_some() {
        Shell::Bash
    } else if cfg!(windows) {
        Shell::Cmd
    } else {
        Shell::Bash
    }
}

/// `shell` argument of a command tool (absent = default).
pub fn shell_arg(args: &serde_json::Value) -> Result<Shell> {
    match args.get("shell").and_then(serde_json::Value::as_str) {
        None => Ok(default_shell()),
        Some(s) if s.trim().is_empty() => Ok(default_shell()),
        Some(s) => {
            let sh = Shell::parse(s)
                .ok_or_else(|| Error::Tool(format!("unknown shell `{s}` (bash, cmd or powershell)")))?;
            if sh == Shell::Cmd && !cfg!(windows) {
                return Err(Error::Tool("shell=cmd exists only on Windows; use bash".into()));
            }
            if sh == Shell::Bash && cfg!(windows) && bash().is_none() {
                return Err(Error::Tool(
                    "bash is not available on this machine (no Git Bash, busybox download failed); use shell=cmd or powershell".into(),
                ));
            }
            Ok(sh)
        }
    }
}

/// One line for tool descriptions: what `shell` defaults to here.
pub fn describe() -> String {
    match (cfg!(windows), bash()) {
        (true, Some(rt)) => format!(
            "Default shell: bash ({} on Windows). Write standard bash: pipes, grep/sed/awk/find/head/tail/wc, $(...), &&, 2>/dev/null. Absolute paths look like C:/Users/x. Set shell=\"cmd\" or \"powershell\" only for Windows-native commands.",
            rt.label()
        ),
        (true, None) => "Default shell: cmd (bash is unavailable on this machine). shell=\"powershell\" is also available.".into(),
        (false, _) => "Shell: bash (sh if bash is missing). shell=\"powershell\" works when pwsh is installed.".into(),
    }
}

/// Build the process for `command` in `shell`.
pub fn command(shell: Shell, command: &str) -> Result<Command> {
    match shell {
        Shell::Bash => bash_command(command),
        Shell::Cmd => {
            if !cfg!(windows) {
                return Err(Error::Tool("shell=cmd exists only on Windows".into()));
            }
            let mut c = Command::new("cmd");
            c.arg("/C").arg(command);
            Ok(c)
        }
        Shell::PowerShell => {
            let exe = if cfg!(windows) { "powershell" } else { "pwsh" };
            let mut c = Command::new(exe);
            c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command"])
                .arg(format!(
                    "[Console]::OutputEncoding=[Text.Encoding]::UTF8; $OutputEncoding=[Text.Encoding]::UTF8; {command}"
                ));
            Ok(c)
        }
    }
}

fn bash_command(command: &str) -> Result<Command> {
    let Some(rt) = bash() else {
        if cfg!(windows) {
            return Err(Error::Tool(
                "bash is not available on this machine; use shell=cmd or powershell".into(),
            ));
        }
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        return Ok(c);
    };
    let script = if cfg!(windows) {
        format!("{WINDOWS_PRELUDE}{command}")
    } else {
        command.to_string()
    };
    let mut c = Command::new(&rt.exe);
    match rt.kind {
        BashKind::Busybox => {
            c.args(["sh", "-c"]).arg(script);
        }
        BashKind::GitBash => {
            // MSYS would otherwise rewrite /c/-style arguments of native tools.
            c.args(["--noprofile", "--norc", "-c"]).arg(script);
            c.env("MSYS_NO_PATHCONV", "1").env("CHERE_INVOKING", "1");
        }
        BashKind::System => {
            c.arg("-c").arg(script);
        }
    }
    Ok(c)
}

fn force_busybox() -> bool {
    std::env::var("GROKA_BASH").is_ok_and(|v| v.eq_ignore_ascii_case("busybox"))
}

fn find_local() -> Option<BashRuntime> {
    if cfg!(windows) && force_busybox() {
        return cached_busybox()
            .filter(|p| p.is_file())
            .map(|exe| BashRuntime { exe, kind: BashKind::Busybox });
    }
    if let Some(p) = std::env::var_os("GROKA_BASH").map(PathBuf::from).filter(|p| p.is_file()) {
        let is_busybox = p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.to_ascii_lowercase().contains("busybox"));
        let kind = if is_busybox {
            BashKind::Busybox
        } else if cfg!(windows) {
            BashKind::GitBash
        } else {
            BashKind::System
        };
        return Some(BashRuntime { exe: p, kind });
    }
    if !cfg!(windows) {
        return ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash", "/opt/homebrew/bin/bash"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .map(|exe| BashRuntime { exe, kind: BashKind::System });
    }
    git_bash()
        .map(|exe| BashRuntime { exe, kind: BashKind::GitBash })
        .or_else(|| {
            cached_busybox()
                .filter(|p| p.is_file())
                .map(|exe| BashRuntime { exe, kind: BashKind::Busybox })
        })
}

/// Git for Windows' bash. Never System32\bash.exe, which is the WSL launcher.
fn git_bash() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"]
        .iter()
        .filter_map(|v| std::env::var_os(v).map(|p| PathBuf::from(p).join("Git")))
        .collect();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local).join("Programs").join("Git"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if dir.join("git.exe").is_file() {
                // …\Git\cmd or …\Git\bin or …\Git\mingw64\bin
                for up in dir.ancestors().take(3) {
                    roots.push(up.to_path_buf());
                }
            }
        }
    }
    roots
        .into_iter()
        .map(|r| r.join("bin").join("bash.exe"))
        .find(|p| p.is_file() && !is_wsl_launcher(p))
}

fn is_wsl_launcher(p: &Path) -> bool {
    p.to_string_lossy().to_ascii_lowercase().contains("system32")
}

fn cached_busybox() -> Option<PathBuf> {
    crate::config::groka_dir().ok().map(|d| d.join("bin").join(BUSYBOX_FILE))
}

async fn download_busybox() -> Result<BashRuntime> {
    let dest = cached_busybox().ok_or_else(|| Error::Tool("no grokaagent home".into()))?;
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let bytes = client.get(BUSYBOX_URL).send().await?.error_for_status()?.bytes().await?;
    let digest = hex(&Sha256::digest(&bytes));
    if digest != BUSYBOX_SHA256 {
        return Err(Error::Tool(format!("busybox download hash mismatch ({digest})")));
    }
    // Several agent processes may race here: write aside, then rename.
    let tmp = dest.with_extension(format!("{}.part", std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    if std::fs::rename(&tmp, &dest).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    if !dest.is_file() {
        return Err(Error::Tool("busybox could not be saved".into()));
    }
    Ok(BashRuntime { exe: dest, kind: BashKind::Busybox })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Command output as text: UTF-8 when it is, else the Windows OEM code page
/// (what cmd and many console tools write).
pub fn decode_output(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => decode_oem(bytes).unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned()),
    }
}

#[cfg(windows)]
fn decode_oem(bytes: &[u8]) -> Option<String> {
    use windows_sys::Win32::Globalization::{GetOEMCP, MultiByteToWideChar};
    if bytes.is_empty() {
        return Some(String::new());
    }
    unsafe {
        let cp = GetOEMCP();
        let n = MultiByteToWideChar(cp, 0, bytes.as_ptr(), bytes.len() as i32, std::ptr::null_mut(), 0);
        if n <= 0 {
            return None;
        }
        let mut wide = vec![0u16; n as usize];
        let n = MultiByteToWideChar(cp, 0, bytes.as_ptr(), bytes.len() as i32, wide.as_mut_ptr(), n);
        (n > 0).then(|| String::from_utf16_lossy(&wide[..n as usize]))
    }
}

#[cfg(not(windows))]
fn decode_oem(_bytes: &[u8]) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_names_parse() {
        assert_eq!(Shell::parse("BASH"), Some(Shell::Bash));
        assert_eq!(Shell::parse("pwsh"), Some(Shell::PowerShell));
        assert_eq!(Shell::parse("cmd.exe"), Some(Shell::Cmd));
        assert_eq!(Shell::parse("fish"), None);
        let err = shell_arg(&serde_json::json!({"shell": "fish"})).unwrap_err().to_string();
        assert!(err.contains("bash, cmd or powershell"), "{err}");
    }

    #[test]
    fn wsl_launcher_is_not_git_bash() {
        assert!(is_wsl_launcher(Path::new(r"C:\Windows\System32\bash.exe")));
        assert!(!is_wsl_launcher(Path::new(r"C:\Program Files\Git\bin\bash.exe")));
    }

    #[test]
    fn utf8_output_passes_through() {
        assert_eq!(decode_output("中文 ok".as_bytes()), "中文 ok");
    }

    #[cfg(windows)]
    #[test]
    fn oem_output_is_decoded_not_mangled() {
        // Whatever the OEM code page, invalid UTF-8 must not come back as U+FFFD soup
        // when the code page can represent it.
        let s = decode_output(&[0x41, 0x42, 0xff]);
        assert!(s.starts_with("AB"), "{s:?}");
    }

    #[tokio::test]
    async fn bash_runs_a_pipeline_when_available() {
        let Some(_) = prepare().await else {
            return;
        };
        let out = command(Shell::Bash, "printf 'b\\na\\n' | sort | head -n 1")
            .unwrap()
            .output()
            .await
            .unwrap();
        assert_eq!(decode_output(&out.stdout).trim(), "a");
        let out = command(Shell::Bash, "python3 -c 'print(1+1)' 2>/dev/null || echo nopython")
            .unwrap()
            .output()
            .await
            .unwrap();
        let text = decode_output(&out.stdout);
        assert!(text.trim() == "2" || text.trim() == "nopython", "{text:?}");
    }
}
