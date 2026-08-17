//! Unified diffs for file tools and post-command git status.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};
use similar::TextDiff;

use crate::error::{Error, Result};

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

struct TextLines {
    lines: Vec<String>,
    newline: &'static str,
    trailing_nl: bool,
}

fn split_text(s: &str) -> TextLines {
    let newline = if s.contains("\r\n") { "\r\n" } else { "\n" };
    let trailing_nl = s.ends_with('\n');
    let mut body = if trailing_nl {
        s.strip_suffix('\n').unwrap_or(s)
    } else {
        s
    };
    if newline == "\r\n" {
        body = body.strip_suffix('\r').unwrap_or(body);
    }
    let lines = if s.is_empty() {
        Vec::new()
    } else {
        body.split(newline).map(|l| l.to_string()).collect()
    };
    TextLines {
        lines,
        newline,
        trailing_nl,
    }
}

fn join_text(file: &TextLines) -> String {
    if file.lines.is_empty() {
        return if file.trailing_nl {
            file.newline.to_string()
        } else {
            String::new()
        };
    }
    let mut out = file.lines.join(file.newline);
    if file.trailing_nl {
        out.push_str(file.newline);
    }
    out
}

struct Hunk {
    old_start: usize,
    old_count: usize,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
}

fn strip_fences(patch: &str) -> String {
    let s = patch.trim();
    if !s.starts_with("```") {
        return s.to_string();
    }
    let mut lines: Vec<&str> = s.lines().collect();
    if lines.first().is_some_and(|l| l.starts_with("```")) {
        lines.remove(0);
    }
    if lines.last().is_some_and(|l| l.trim() == "```") {
        lines.pop();
    }
    lines.join("\n")
}

fn is_file_header(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("diff --git ")
        || t.starts_with("index ")
        || t.starts_with("--- ")
        || t.starts_with("+++ ")
        || t.starts_with("new file mode ")
        || t.starts_with("deleted file mode ")
        || t.starts_with("old mode ")
        || t.starts_with("new mode ")
        || t.starts_with("similarity index ")
        || t.starts_with("rename from ")
        || t.starts_with("rename to ")
        || t.starts_with("copy from ")
        || t.starts_with("copy to ")
        || t.starts_with("Binary files ")
}

fn parse_hunk_range(s: &str) -> Result<((usize, usize), &str)> {
    let s = s.trim_start();
    let s = s
        .strip_prefix(['-', '+'])
        .ok_or_else(|| Error::Tool("hunk header missing +/- range".into()))?;
    let digits = s
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(s.len());
    if digits == 0 {
        return Err(Error::Tool("hunk header has no line number".into()));
    }
    let start: usize = s[..digits]
        .parse()
        .map_err(|_| Error::Tool("hunk header line is not an integer".into()))?;
    let rest = &s[digits..];
    if let Some(rest) = rest.strip_prefix(',') {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits == 0 {
            return Err(Error::Tool("hunk header count is empty".into()));
        }
        let count: usize = rest[..digits]
            .parse()
            .map_err(|_| Error::Tool("hunk header count is not an integer".into()))?;
        Ok(((start, count), &rest[digits..]))
    } else {
        Ok(((start, 1), rest))
    }
}

fn parse_hunk_header(line: &str) -> Result<(usize, usize, usize, usize)> {
    let s = line.trim();
    let s = s
        .strip_prefix("@@")
        .ok_or_else(|| Error::Tool("expected @@ hunk header".into()))?
        .trim();
    let (old, s) = parse_hunk_range(s)?;
    let (new, _) = parse_hunk_range(s)?;
    Ok((old.0, old.1, new.0, new.1))
}

fn parse_hunks(patch: &str) -> Result<Vec<Hunk>> {
    let stripped = strip_fences(patch);
    let lines: Vec<&str> = stripped.lines().collect();
    let mut hunks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if !line.starts_with("@@") {
            i += 1;
            continue;
        }
        let (old_start, old_count, new_start, new_count) = parse_hunk_header(line)?;
        let _ = (new_start, new_count);
        i += 1;
        let mut old_lines = Vec::new();
        let mut new_lines = Vec::new();
        while i < lines.len() {
            let body = lines[i];
            if body.starts_with("@@") {
                break;
            }
            if body.is_empty() || is_file_header(body) {
                i += 1;
                continue;
            }
            if body.starts_with('\\') {
                i += 1;
                continue;
            }
            match body.as_bytes().first().copied() {
                Some(b' ') => {
                    let t = &body[1..];
                    old_lines.push(t.to_string());
                    new_lines.push(t.to_string());
                }
                Some(b'-') => old_lines.push(body[1..].to_string()),
                Some(b'+') => new_lines.push(body[1..].to_string()),
                _ => {
                    return Err(Error::Tool(format!(
                        "diff line must start with space, + or -: {body}"
                    )));
                }
            }
            i += 1;
        }
        if old_lines.len() != old_count {
            return Err(Error::Tool(format!(
                "hunk @@ -{old_start},{old_count} has {} old lines (space or -)",
                old_lines.len()
            )));
        }
        if new_lines.len() != new_count {
            return Err(Error::Tool(format!(
                "hunk @@ +{new_start},{new_count} has {} new lines (space or +)",
                new_lines.len()
            )));
        }
        hunks.push(Hunk {
            old_start,
            old_count,
            old_lines,
            new_lines,
        });
    }
    if hunks.is_empty() {
        return Err(Error::Tool(
            "edit_file diff needs at least one @@ hunk with - deleted lines and + added lines".into(),
        ));
    }
    Ok(hunks)
}

fn hunk_insert_at(h: &Hunk) -> Result<usize> {
    if h.old_count == 0 {
        return Ok(h.old_start);
    }
    if h.old_start == 0 {
        return Err(Error::Tool("hunk old start is 0 but count is not 0".into()));
    }
    Ok(h.old_start - 1)
}

/// Apply a git unified diff (`---`/`+++`/`@@`, context, `-` deletes, `+` adds).
pub fn apply_unified(original: &str, patch: &str) -> Result<String> {
    if patch.len() > MAX_DIFF_BYTES * 8 {
        return Err(Error::Tool("diff is larger than 256KiB".into()));
    }
    let mut file = split_text(original);
    let mut hunks = parse_hunks(patch)?;
    hunks.sort_by_key(|h| h.old_start);
    for pair in hunks.windows(2) {
        let a_end = if pair[0].old_count == 0 {
            pair[0].old_start
        } else {
            pair[0].old_start.saturating_add(pair[0].old_count)
        };
        if pair[1].old_start < a_end && pair[1].old_count > 0 {
            return Err(Error::Tool("diff hunks overlap".into()));
        }
    }
    hunks.reverse();
    for h in &hunks {
        let at = hunk_insert_at(h)?;
        if h.old_count == 0 {
            if at > file.lines.len() {
                return Err(Error::Tool(format!(
                    "hunk inserts past end of file ({} lines)",
                    file.lines.len()
                )));
            }
            for (i, line) in h.new_lines.iter().enumerate() {
                file.lines.insert(at + i, line.clone());
            }
            continue;
        }
        let end = at.saturating_add(h.old_count);
        if end > file.lines.len() {
            return Err(Error::Tool(format!(
                "hunk @@ -{},{} is past end of file ({} lines)",
                h.old_start,
                h.old_count,
                file.lines.len()
            )));
        }
        if file.lines[at..end] != h.old_lines {
            return Err(Error::Tool(format!(
                "hunk @@ -{},{} does not match the file. read_file again and copy context lines exactly",
                h.old_start, h.old_count
            )));
        }
        file.lines.splice(at..end, h.new_lines.iter().cloned());
    }
    let no_nl = patch.lines().any(|l| l.trim_start().starts_with('\\'));
    if no_nl {
        file.trailing_nl = false;
    } else if original.is_empty() && !file.lines.is_empty() {
        file.trailing_nl = true;
    }
    Ok(join_text(&file))
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
    let plus = diff
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count();
    let minus = diff
        .lines()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .count();
    json!({
        "path": path,
        "kind": kind,
        "diff": diff,
        "plus": plus,
        "minus": minus,
        "unchanged": diff.is_empty()
    })
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
        let j = file_change_json("a.txt", Some("old\n"), Some("new\n"));
        assert_eq!(j["plus"], 1);
        assert_eq!(j["minus"], 1);
        assert_eq!(j["unchanged"], false);
        let d = j["diff"].as_str().unwrap();
        assert!(d.contains("--- a/a.txt"), "{d}");
        assert!(d.contains("+++ b/a.txt"), "{d}");
        assert!(d.contains("@@"), "{d}");
    }

    #[test]
    fn identical_is_empty() {
        assert!(unified_diff("a.txt", "x\n", "x\n").is_empty());
    }

    #[test]
    fn apply_unified_roundtrips_similar_diff() {
        let before = "one\ntwo\nthree\n";
        let after = "one\nTWO\nTWO-B\nthree\n";
        let patch = unified_diff("a.txt", before, after);
        assert_eq!(apply_unified(before, &patch).unwrap(), after, "{patch}");
        let crlf_before = "a\r\nb\r\nc\r\n";
        let crlf_after = "a\r\nB\r\nc\r\n";
        let patch = unified_diff("w.txt", crlf_before, crlf_after);
        assert_eq!(
            apply_unified(crlf_before, &patch).unwrap(),
            crlf_after,
            "{patch}"
        );
    }

    #[test]
    fn apply_unified_inserts_at_start_of_empty_file() {
        let patch = "--- a/n.txt\n+++ b/n.txt\n@@ -0,0 +1,2 @@\n+hello\n+world\n";
        assert_eq!(apply_unified("", patch).unwrap(), "hello\nworld\n");
    }

    #[test]
    fn apply_unified_rejects_context_mismatch() {
        let patch = "@@ -1,2 +1,2 @@\n a\n-b\n+B\n";
        let err = apply_unified("a\nx\n", patch).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
    }

    #[test]
    fn apply_unified_strips_fences_and_multiple_hunks() {
        let before = "a\nb\nc\nd\n";
        let patch = "```diff\n@@ -1,2 +1,2 @@\n a\n-b\n+B\n@@ -4,1 +4,1 @@\n-d\n+D\n```\n";
        assert_eq!(apply_unified(before, patch).unwrap(), "a\nB\nc\nD\n");
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
