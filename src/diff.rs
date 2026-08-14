//! Unified diffs for file tools and post-command git status.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};
use similar::TextDiff;

const MAX_FILES: usize = 24;
const MAX_DIFF_BYTES: usize = 32 * 1024;

pub fn unified_diff(path: &str, before: &str, after: &str) -> String {
    if before == after {
        return String::new();
    }
    TextDiff::from_lines(before, after)
        .unified_diff()
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

pub fn kind_for(before: Option<&str>, after: Option<&str>) -> &'static str {
    match (before, after) {
        (None, Some(_)) => "create",
        (Some(_), None) => "delete",
        _ => "modify",
    }
}

pub fn file_change_json(path: &str, before: Option<&str>, after: Option<&str>) -> Value {
    let kind = kind_for(before, after);
    let diff = unified_diff(path, before.unwrap_or(""), after.unwrap_or(""));
    json!({ "path": path, "kind": kind, "diff": diff })
}

/// Workspace-relative git changes after a command. Empty if not a git repo.
pub fn git_file_changes(workspace: &Path) -> Vec<Value> {
    let Ok(root) = git_toplevel(workspace) else {
        return Vec::new();
    };
    let status = git_output(&root, &["status", "--porcelain", "-unormal"]);
    let Ok(status) = status else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in status.lines() {
        if out.len() >= MAX_FILES {
            out.push(json!({
                "path": "(truncated)",
                "kind": "modify",
                "diff": format!("more than {MAX_FILES} changed files; omitted the rest")
            }));
            break;
        }
        if line.len() < 4 {
            continue;
        }
        let code = &line[..2];
        let path = porcelain_path(&line[3..]);
        if path.is_empty() {
            continue;
        }
        let abs = root.join(&path);
        if abs.is_dir() {
            out.push(json!({"path": path, "kind": "create", "diff": "(directory)"}));
            continue;
        }
        match code {
            "??" | "A " | "AM" => {
                out.push(change_from_disk(&path, &abs, None));
            }
            " D" | "D " | "AD" => {
                let before = git_output(&root, &["show", &format!("HEAD:{path}")]).unwrap_or_default();
                out.push(change_from_text(&path, Some(&before), None));
            }
            _ => {
                let before = git_output(&root, &["show", &format!("HEAD:{path}")]).unwrap_or_default();
                out.push(change_from_disk(&path, &abs, Some(before)));
            }
        }
    }
    out
}

fn porcelain_path(raw: &str) -> String {
    let s = raw.trim().replace('\\', "/");
    if let Some(rest) = s.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        rest.to_string()
    } else if let Some((left, _)) = s.split_once(" -> ") {
        left.trim().to_string()
    } else {
        s
    }
}

fn change_from_disk(path: &str, abs: &Path, before: Option<String>) -> Value {
    match std::fs::read(abs) {
        Ok(bytes) if bytes.len() > MAX_DIFF_BYTES => json!({
            "path": path,
            "kind": kind_for(before.as_deref(), Some("")),
            "diff": format!("(file larger than {MAX_DIFF_BYTES} bytes)")
        }),
        Ok(bytes) => {
            let after = String::from_utf8_lossy(&bytes);
            change_from_text(path, before.as_deref(), Some(after.as_ref()))
        }
        Err(_) => json!({"path": path, "kind": "modify", "diff": "(unreadable)"}),
    }
}

fn change_from_text(path: &str, before: Option<&str>, after: Option<&str>) -> Value {
    if before.unwrap_or("").len() > MAX_DIFF_BYTES || after.unwrap_or("").len() > MAX_DIFF_BYTES {
        return json!({
            "path": path,
            "kind": kind_for(before, after),
            "diff": format!("(file larger than {MAX_DIFF_BYTES} bytes)")
        });
    }
    file_change_json(path, before, after)
}

fn git_toplevel(workspace: &Path) -> std::io::Result<PathBuf> {
    let out = git_cmd(workspace)
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "not a git repo",
        ));
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(PathBuf::from(s))
}

fn git_output(root: &Path, args: &[&str]) -> std::io::Result<String> {
    let out = git_cmd(root).args(args).output()?;
    if !out.status.success() {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "git failed"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(dir);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_diff_marks_additions() {
        let d = unified_diff("a.txt", "", "hello\n");
        assert!(d.contains("+++ b/a.txt"), "{d}");
        assert!(d.contains("+hello"), "{d}");
        assert!(!d.contains("-hello"), "{d}");
    }

    #[test]
    fn delete_diff_marks_removals() {
        let d = unified_diff("a.txt", "bye\n", "");
        assert!(d.contains("-bye"), "{d}");
    }

    #[test]
    fn modify_diff_shows_both_sides() {
        let d = unified_diff("a.txt", "old\n", "new\n");
        assert!(d.contains("-old"), "{d}");
        assert!(d.contains("+new"), "{d}");
        assert_eq!(kind_for(Some("old"), Some("new")), "modify");
    }

    #[test]
    fn identical_is_empty() {
        assert!(unified_diff("a.txt", "x\n", "x\n").is_empty());
    }

    #[test]
    fn git_file_changes_caps_many_untracked() {
        let dir = tempfile::tempdir().unwrap();
        let init = Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output();
        let Ok(init) = init else {
            return;
        };
        if !init.status.success() {
            return;
        }
        let _ = Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t", "commit", "--allow-empty", "-m", "i"])
            .current_dir(dir.path())
            .output();
        for i in 0..80 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "x\n").unwrap();
        }
        let changes = git_file_changes(dir.path());
        assert!(
            changes.len() <= MAX_FILES + 1,
            "expected cap, got {}",
            changes.len()
        );
        assert!(changes.len() >= 2, "expected some untracked files");
    }
}
