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

enum Op {
    Ctx(String),
    Del(String),
    Add(String),
}

/// One `@@` hunk. Header numbers are only a location hint: models often
/// miscount, so the old/new line counts come from the body.
struct Hunk {
    /// 1-based old start line from the header, when it had one.
    hint: Option<usize>,
    ops: Vec<Op>,
}

impl Hunk {
    fn old_lines(&self) -> Vec<&str> {
        self.ops
            .iter()
            .filter_map(|op| match op {
                Op::Ctx(s) | Op::Del(s) => Some(s.as_str()),
                Op::Add(_) => None,
            })
            .collect()
    }
}

fn strip_fences(patch: &str) -> String {
    let s = patch.trim_matches('\n');
    if !s.trim_start().starts_with("```") {
        return s.to_string();
    }
    let mut lines: Vec<&str> = s.lines().collect();
    if lines.first().is_some_and(|l| l.trim_start().starts_with("```")) {
        lines.remove(0);
    }
    if lines.last().is_some_and(|l| l.trim() == "```") {
        lines.pop();
    }
    lines.join("\n")
}

/// Lines that only make sense outside a hunk (git headers).
fn is_file_header(line: &str) -> bool {
    [
        "diff --git ", "index ", "--- ", "+++ ", "new file mode ", "deleted file mode ",
        "old mode ", "new mode ", "similarity index ", "rename from ", "rename to ",
        "copy from ", "copy to ", "Binary files ",
    ]
    .iter()
    .any(|p| line.starts_with(p))
}

/// First number after `-` in `@@ -12,5 +12,6 @@`; `None` for a bare `@@`.
fn header_hint(line: &str) -> Option<usize> {
    let rest = line.trim_start().strip_prefix("@@")?.trim_start();
    let rest = rest.strip_prefix('-')?;
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn parse_hunks(patch: &str) -> Result<Vec<Hunk>> {
    let stripped = strip_fences(patch);
    let lines: Vec<&str> = stripped.lines().collect();
    let mut hunks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !lines[i].trim_start().starts_with("@@") {
            i += 1;
            continue;
        }
        let hint = header_hint(lines[i]);
        i += 1;
        let mut ops = Vec::new();
        while i < lines.len() {
            let body = lines[i];
            if body.trim_start().starts_with("@@") {
                break;
            }
            // A new file section ends the hunk: `diff --git`, or a `---` line
            // directly followed by `+++` (a deleted "-- comment" line is not).
            if body.starts_with("diff --git ")
                || (body.starts_with("--- ") && lines.get(i + 1).is_some_and(|n| n.starts_with("+++ ")))
            {
                break;
            }
            let op = match body.as_bytes().first().copied() {
                Some(b' ') => Op::Ctx(body[1..].to_string()),
                Some(b'-') => Op::Del(body[1..].to_string()),
                Some(b'+') => Op::Add(body[1..].to_string()),
                Some(b'\\') => {
                    i += 1;
                    continue;
                }
                // Blank line, or a context line whose leading space got lost.
                _ => Op::Ctx(body.to_string()),
            };
            ops.push(op);
            i += 1;
        }
        while matches!(ops.last(), Some(Op::Ctx(s)) if s.trim().is_empty()) {
            ops.pop();
        }
        if !ops.is_empty() {
            hunks.push(Hunk { hint, ops });
        }
    }
    if hunks.is_empty() {
        let only_headers = stripped.lines().all(|l| l.trim().is_empty() || is_file_header(l));
        return Err(Error::Tool(if only_headers {
            "edit_file diff has no @@ hunk. Start each hunk with a line beginning with @@, then ' ' context, '-' deleted and '+' added lines (or use old_string/new_string)".into()
        } else {
            "edit_file diff needs at least one @@ hunk with context, - deleted and + added lines (or use old_string/new_string)".into()
        }));
    }
    Ok(hunks)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Match {
    Exact,
    TrailingSpace,
    Indent,
}

impl Match {
    fn eq(self, file: &str, want: &str) -> bool {
        match self {
            Match::Exact => file == want,
            Match::TrailingSpace => file.trim_end() == want.trim_end(),
            Match::Indent => file.trim() == want.trim(),
        }
    }
}

/// Where `old` sits in `file`: 0-based start. Closest to the hint wins; with
/// no hint the block must be unique.
fn locate(file: &[String], old: &[&str], hint: Option<usize>, level: Match) -> std::result::Result<usize, Vec<usize>> {
    if old.len() > file.len() {
        return Err(Vec::new());
    }
    let hits: Vec<usize> = (0..=file.len() - old.len())
        .filter(|&p| old.iter().enumerate().all(|(k, w)| level.eq(&file[p + k], w)))
        .collect();
    match (hits.len(), hint) {
        (0, _) => Err(Vec::new()),
        (1, _) => Ok(hits[0]),
        (_, Some(h)) => {
            let want = h.saturating_sub(1);
            let best = *hits.iter().min_by_key(|&&p| p.abs_diff(want)).unwrap();
            let tied = hits.iter().filter(|&&p| p.abs_diff(want) == best.abs_diff(want)).count();
            if tied > 1 {
                Err(hits)
            } else {
                Ok(best)
            }
        }
        (_, None) => Err(hits),
    }
}

/// Numbered excerpt of the file around the line most like the hunk's first
/// old line, so a failed edit can be fixed without another read_file.
fn closest_excerpt(file: &[String], old: &[&str], hint: Option<usize>) -> String {
    let probe = old.iter().find(|l| !l.trim().is_empty()).map(|l| l.trim());
    let want = hint.unwrap_or(1).saturating_sub(1);
    let at = probe
        .and_then(|p| {
            file.iter()
                .enumerate()
                .filter(|(_, l)| l.trim() == p)
                .map(|(i, _)| i)
                .min_by_key(|i| i.abs_diff(want))
        })
        .or(hint.map(|_| want.min(file.len().saturating_sub(1))));
    let Some(at) = at else {
        return match probe {
            Some(p) => format!("the line `{}` does not occur in the file", clip_line(p)),
            None => String::new(),
        };
    };
    let from = at.saturating_sub(2);
    let to = (at + old.len().max(3) + 2).min(file.len());
    let mut out = format!("closest text in the file (lines {}-{}):", from + 1, to);
    for (i, line) in file[from..to].iter().enumerate() {
        out.push_str(&format!("\n{}|{}", from + i + 1, clip_line(line)));
    }
    out
}

fn clip_line(s: &str) -> String {
    if s.chars().count() > 160 {
        format!("{}…", s.chars().take(160).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Result of applying an edit: the new text plus notes worth telling the model
/// (hunks that moved or matched loosely).
pub struct Applied {
    pub text: String,
    pub notes: Vec<String>,
}

/// Apply a unified diff. Hunks are located by their context and deleted
/// lines; header line numbers only break ties. With `loose`, lines may also
/// match ignoring trailing whitespace, then indentation.
pub fn apply_patch(original: &str, patch: &str, loose: bool) -> Result<Applied> {
    if patch.len() > MAX_DIFF_BYTES * 8 {
        return Err(Error::Tool("diff is larger than 256KiB".into()));
    }
    let mut file = split_text(original);
    let hunks = parse_hunks(patch)?;
    let levels: &[Match] = if loose {
        &[Match::Exact, Match::TrailingSpace, Match::Indent]
    } else {
        &[Match::Exact]
    };
    let mut notes = Vec::new();
    // (start, old len, replacement lines)
    let mut plan: Vec<(usize, usize, Vec<String>)> = Vec::new();
    for (n, h) in hunks.iter().enumerate() {
        let old = h.old_lines();
        let label = format!("hunk {}{}", n + 1, h.hint.map(|l| format!(" (@@ -{l})")).unwrap_or_default());
        if old.is_empty() {
            let Some(line) = h.hint else {
                return Err(Error::Tool(format!(
                    "{label} only adds lines; give its position with @@ -N (insert after line N) or include a context line"
                )));
            };
            let at = line.min(file.lines.len());
            let adds = h.ops.iter().filter_map(|op| match op {
                Op::Add(s) => Some(s.clone()),
                _ => None,
            });
            plan.push((at, 0, adds.collect()));
            continue;
        }
        let mut found = None;
        let mut ambiguous = Vec::new();
        for &level in levels {
            match locate(&file.lines, &old, h.hint, level) {
                Ok(p) => {
                    found = Some((p, level));
                    break;
                }
                Err(hits) if !hits.is_empty() => {
                    ambiguous = hits;
                    break;
                }
                Err(_) => {}
            }
        }
        let Some((start, level)) = found else {
            if !ambiguous.is_empty() {
                let at: Vec<String> = ambiguous.iter().take(8).map(|p| (p + 1).to_string()).collect();
                return Err(Error::Tool(format!(
                    "{label} matches {} places (lines {}); add more context lines or a correct @@ -N line number",
                    ambiguous.len(),
                    at.join(", ")
                )));
            }
            return Err(Error::Tool(format!(
                "{label} does not match the file: its context and '-' lines must be copied exactly. {}",
                closest_excerpt(&file.lines, &old, h.hint)
            )));
        };
        if let Some(line) = h.hint {
            if line != start + 1 {
                notes.push(format!("{label} applied at line {} (header said {line})", start + 1));
            }
        }
        if level != Match::Exact {
            notes.push(format!(
                "{label} matched ignoring {}",
                if level == Match::TrailingSpace { "trailing whitespace" } else { "indentation" }
            ));
        }
        // Context keeps the file's own text; only '+' lines come from the model.
        let mut k = start;
        let mut replacement = Vec::new();
        for op in &h.ops {
            match op {
                Op::Ctx(_) => {
                    replacement.push(file.lines[k].clone());
                    k += 1;
                }
                Op::Del(_) => k += 1,
                Op::Add(s) => replacement.push(s.clone()),
            }
        }
        plan.push((start, old.len(), replacement));
    }
    plan.sort_by_key(|p| p.0);
    for pair in plan.windows(2) {
        if pair[1].0 < pair[0].0 + pair[0].1 {
            return Err(Error::Tool(format!(
                "diff hunks overlap near line {}; merge them into one hunk",
                pair[1].0 + 1
            )));
        }
    }
    for (start, len, replacement) in plan.into_iter().rev() {
        file.lines.splice(start..start + len, replacement);
    }
    let no_nl = patch.lines().any(|l| l.trim_start().starts_with('\\'));
    if no_nl {
        file.trailing_nl = false;
    } else if original.is_empty() && !file.lines.is_empty() {
        file.trailing_nl = true;
    }
    Ok(Applied { text: join_text(&file), notes })
}

/// Strict entry point kept for callers that want plain text back.
pub fn apply_unified(original: &str, patch: &str) -> Result<String> {
    apply_patch(original, patch, true).map(|a| a.text)
}

/// Exact text replacement (`old_string` → `new_string`). Line endings in the
/// strings follow the file's.
pub fn apply_replace(original: &str, old: &str, new: &str, replace_all: bool) -> Result<Applied> {
    if old.is_empty() {
        return Err(Error::Tool("old_string is empty; copy the exact text to replace".into()));
    }
    let crlf = original.contains("\r\n");
    let fix = |s: &str| {
        let s = s.replace("\r\n", "\n");
        if crlf { s.replace('\n', "\r\n") } else { s }
    };
    let (old, new) = (fix(old), fix(new));
    if old == new {
        return Err(Error::Tool("old_string and new_string are the same; nothing to change".into()));
    }
    let hits: Vec<usize> = original.match_indices(&old).map(|(i, _)| i).collect();
    let line_of = |byte: usize| original[..byte].matches('\n').count() + 1;
    match hits.len() {
        0 => {
            let file = split_text(original);
            let want: Vec<&str> = old.lines().collect();
            Err(Error::Tool(format!(
                "old_string not found in the file (it must match exactly, including indentation). {}",
                closest_excerpt(&file.lines, &want, None)
            )))
        }
        n if n > 1 && !replace_all => {
            let at: Vec<String> = hits.iter().take(8).map(|&b| line_of(b).to_string()).collect();
            Err(Error::Tool(format!(
                "old_string occurs {n} times (lines {}); include more surrounding text to make it unique, or set replace_all",
                at.join(", ")
            )))
        }
        n => {
            let text = if replace_all { original.replace(&old, &new) } else { original.replacen(&old, &new, 1) };
            let notes = if n > 1 { vec![format!("replaced {n} occurrences")] } else { Vec::new() };
            Ok(Applied { text, notes })
        }
    }
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
    fn wrong_header_counts_and_numbers_are_only_hints() {
        let before = "fn a() {\n    1\n}\n\nfn b() {\n    2\n}\n";
        // Counts are wrong (-3,9 / +3,1) and the start line is off by two.
        let patch = "@@ -3,9 +3,1 @@\n fn b() {\n-    2\n+    20\n }\n";
        let a = apply_patch(before, patch, true).unwrap();
        assert_eq!(a.text, "fn a() {\n    1\n}\n\nfn b() {\n    20\n}\n");
        assert!(a.notes.iter().any(|n| n.contains("applied at line 5")), "{:?}", a.notes);
    }

    #[test]
    fn blank_context_lines_without_a_space_still_match() {
        let before = "a\n\nb\n";
        let patch = "@@ -1,3 +1,3 @@\n a\n\n-b\n+B\n";
        assert_eq!(apply_unified(before, patch).unwrap(), "a\n\nB\n");
    }

    #[test]
    fn loose_matching_keeps_the_files_own_context() {
        let before = "if x:\n    y = 1   \n    z = 2\n";
        let patch = "@@\n if x:\n     y = 1\n-    z = 2\n+    z = 3\n";
        let a = apply_patch(before, patch, true).unwrap();
        assert_eq!(a.text, "if x:\n    y = 1   \n    z = 3\n");
        assert!(a.notes.iter().any(|n| n.contains("trailing whitespace")), "{:?}", a.notes);
        assert!(apply_patch(before, patch, false).is_err(), "strict mode wants exact text");
    }

    #[test]
    fn ambiguous_hunk_without_a_hint_is_refused() {
        let before = "x\ny\nx\ny\n";
        let err = apply_unified(before, "@@\n-x\n+X\n").unwrap_err().to_string();
        assert!(err.contains("matches 2 places (lines 1, 3)"), "{err}");
        assert_eq!(apply_unified(before, "@@ -3 @@\n-x\n+X\n").unwrap(), "x\ny\nX\ny\n");
    }

    #[test]
    fn deleting_a_sql_comment_is_not_a_file_header() {
        let before = "-- old note\nSELECT 1;\n";
        let patch = "@@ -1,2 +1,1 @@\n--- old note\n SELECT 1;\n";
        assert_eq!(apply_unified(before, patch).unwrap(), "SELECT 1;\n");
    }

    #[test]
    fn mismatch_error_shows_the_nearby_lines() {
        let before = "alpha\nbeta\ngamma\n";
        let err = apply_unified(before, "@@ -2 @@\n beta\n-GAMMA\n+g\n").unwrap_err().to_string();
        assert!(err.contains("2|beta") && err.contains("3|gamma"), "{err}");
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
