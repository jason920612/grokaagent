//! New-chat workspace picker: list a folder, filter by typed path/name, create dirs.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_parent: bool,
}

#[derive(Clone, Debug)]
pub struct FolderView {
    pub cwd: PathBuf,
    pub entries: Vec<Entry>,
    pub error: Option<String>,
}

pub fn display_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    #[cfg(windows)]
    {
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{rest}");
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return rest.to_string();
        }
    }
    s.into_owned()
}

pub fn existing_dir(p: &Path) -> PathBuf {
    let mut cur = if p.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        p.to_path_buf()
    };
    if cur.is_file() {
        if let Some(parent) = cur.parent() {
            if !parent.as_os_str().is_empty() {
                cur = parent.to_path_buf();
            }
        }
    }
    loop {
        if cur.is_dir() {
            return normalize(&cur);
        }
        match cur.parent() {
            Some(parent) if parent.as_os_str() != cur.as_os_str() && !parent.as_os_str().is_empty() => {
                cur = parent.to_path_buf();
            }
            _ => break,
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn normalize(p: &Path) -> PathBuf {
    match fs::canonicalize(p) {
        Ok(c) => PathBuf::from(display_path(&c)),
        Err(_) if p.is_absolute() => p.to_path_buf(),
        Err(_) => std::env::current_dir()
            .map(|c| c.join(p))
            .unwrap_or_else(|_| p.to_path_buf()),
    }
}

pub fn parent_dir(p: &Path) -> Option<PathBuf> {
    let parent = p.parent()?;
    if parent.as_os_str().is_empty() {
        return None;
    }
    if parent == p {
        return None;
    }
    Some(normalize(parent))
}

pub fn list_folder(dir: &Path) -> FolderView {
    let cwd = if dir.is_dir() {
        normalize(dir)
    } else {
        existing_dir(dir)
    };
    let mut entries = Vec::new();
    if let Some(parent) = parent_dir(&cwd) {
        entries.push(Entry {
            name: "..".into(),
            path: parent,
            is_dir: true,
            is_parent: true,
        });
    }
    let mut kids: Vec<Entry> = Vec::new();
    let mut error = None;
    match fs::read_dir(&cwd) {
        Ok(rd) => {
            for ent in rd {
                let Ok(ent) = ent else {
                    continue;
                };
                let path = ent.path();
                let name = ent.file_name().to_string_lossy().into_owned();
                if name == "." || name == ".." {
                    continue;
                }
                let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or_else(|_| path.is_dir());
                kids.push(Entry {
                    name,
                    path,
                    is_dir,
                    is_parent: false,
                });
            }
        }
        Err(e) => error = Some(format!("無法讀取資料夾: {e}")),
    }
    kids.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    entries.extend(kids);
    FolderView {
        cwd,
        entries,
        error,
    }
}

/// Resolve the path box into a directory to list plus a name filter.
pub fn split_input(input: &str, fallback: &Path) -> (PathBuf, String) {
    let t = input.trim();
    let fallback = existing_dir(fallback);
    if t.is_empty() {
        return (fallback, String::new());
    }
    let raw = PathBuf::from(t);
    let abs = if raw.is_absolute() {
        raw
    } else {
        fallback.join(&raw)
    };
    let ends_sep = t.ends_with('/') || t.ends_with('\\');
    if ends_sep {
        if abs.is_dir() {
            return (normalize(&abs), String::new());
        }
        return split_missing(&abs, &fallback);
    }
    if abs.is_dir() {
        return (normalize(&abs), String::new());
    }
    if abs.is_file() {
        let parent = abs
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(normalize)
            .unwrap_or(fallback);
        let name = abs
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        return (parent, name);
    }
    split_missing(&abs, &fallback)
}

fn split_missing(abs: &Path, fallback: &Path) -> (PathBuf, String) {
    if let Some(parent) = abs.parent() {
        if !parent.as_os_str().is_empty() && parent.is_dir() {
            let name = abs
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            return (normalize(parent), name);
        }
    }
    let name = abs
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| abs.to_string_lossy().into_owned());
    (fallback.to_path_buf(), name)
}

pub fn apply_filter(entries: &[Entry], filter: &str) -> Vec<Entry> {
    let f = filter.trim();
    if f.is_empty() {
        return entries.to_vec();
    }
    let needle = f.to_lowercase();
    entries
        .iter()
        .filter(|e| e.is_parent || e.name.to_lowercase().contains(&needle))
        .cloned()
        .collect()
}

pub fn view_for_input(input: &str, fallback: &Path) -> FolderView {
    let (dir, filter) = split_input(input, fallback);
    let mut view = list_folder(&dir);
    view.entries = apply_filter(&view.entries, &filter);
    view
}

pub fn create_target(input: &str, cwd: &Path) -> PathBuf {
    let t = input.trim();
    let cwd = existing_dir(cwd);
    if t.is_empty() {
        return unique_child(&cwd, "新資料夾");
    }
    let raw = PathBuf::from(t);
    let abs = if raw.is_absolute() {
        raw
    } else {
        cwd.join(raw)
    };
    if abs.is_dir() {
        unique_child(&abs, "新資料夾")
    } else {
        abs
    }
}

pub fn mkdir(path: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(path)?;
    Ok(normalize(path))
}

pub fn workspace_of(cwd: &Path, selected: Option<&Entry>) -> PathBuf {
    match selected {
        Some(e) if e.is_dir && !e.is_parent => {
            if e.path.is_dir() {
                normalize(&e.path)
            } else {
                existing_dir(cwd)
            }
        }
        Some(e) if !e.is_dir => e
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty() && p.is_dir())
            .map(normalize)
            .unwrap_or_else(|| existing_dir(cwd)),
        _ => existing_dir(cwd),
    }
}

fn unique_child(dir: &Path, base: &str) -> PathBuf {
    let p = dir.join(base);
    if !p.exists() {
        return p;
    }
    for i in 2..1000 {
        let p = dir.join(format!("{base} ({i})"));
        if !p.exists() {
            return p;
        }
    }
    dir.join(format!("{base}-new"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn list_folder_shows_parent_dirs_then_files() {
        let dir = tmp();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::create_dir(dir.path().join("docs")).unwrap();
        fs::write(dir.path().join("README.md"), b"x").unwrap();
        fs::write(dir.path().join("Cargo.toml"), b"x").unwrap();
        let view = list_folder(dir.path());
        assert!(view.error.is_none(), "{:?}", view.error);
        assert_eq!(view.cwd, normalize(dir.path()));
        let names: Vec<&str> = view.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names[0], "..", "{names:?}");
        let rest: Vec<&str> = names.iter().copied().skip(1).collect();
        assert_eq!(rest, ["docs", "src", "Cargo.toml", "README.md"], "{names:?}");
        assert!(view.entries.iter().find(|e| e.name == "src").unwrap().is_dir);
        assert!(!view.entries.iter().find(|e| e.name == "README.md").unwrap().is_dir);
    }

    #[test]
    fn split_input_existing_dir_has_no_filter() {
        let dir = tmp();
        fs::create_dir(dir.path().join("src")).unwrap();
        let (listed, filter) = split_input(&display_path(&dir.path().join("src")), dir.path());
        assert!(filter.is_empty(), "{filter}");
        assert_eq!(listed, normalize(&dir.path().join("src")));
    }

    #[test]
    fn split_input_missing_name_filters_parent() {
        let dir = tmp();
        fs::create_dir(dir.path().join("src")).unwrap();
        let typed = dir.path().join("src").join("mod");
        let (listed, filter) = split_input(&display_path(&typed), dir.path());
        assert_eq!(listed, normalize(&dir.path().join("src")));
        assert_eq!(filter, "mod");
    }

    #[test]
    fn split_input_relative_name_searches_fallback() {
        let dir = tmp();
        let (listed, filter) = split_input("Read", dir.path());
        assert_eq!(listed, normalize(dir.path()));
        assert_eq!(filter, "Read");
    }

    #[test]
    fn apply_filter_keeps_parent_and_matches_case_insensitive() {
        let entries = vec![
            Entry {
                name: "..".into(),
                path: PathBuf::from("/"),
                is_dir: true,
                is_parent: true,
            },
            Entry {
                name: "Src".into(),
                path: PathBuf::from("/Src"),
                is_dir: true,
                is_parent: false,
            },
            Entry {
                name: "README.md".into(),
                path: PathBuf::from("/README.md"),
                is_dir: false,
                is_parent: false,
            },
        ];
        let got = apply_filter(&entries, "src");
        let names: Vec<&str> = got.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["..", "Src"], "{names:?}");
    }

    #[test]
    fn view_for_input_filters_as_you_type() {
        let dir = tmp();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("README.md"), b"x").unwrap();
        let typed = format!("{}{}s", display_path(dir.path()), std::path::MAIN_SEPARATOR);
        let view = view_for_input(&typed, dir.path());
        let names: Vec<&str> = view
            .entries
            .iter()
            .filter(|e| !e.is_parent)
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, ["src"], "{names:?} typed={typed}");
    }

    #[test]
    fn mkdir_creates_nested_and_normalize() {
        let dir = tmp();
        let target = dir.path().join("a").join("b");
        let created = mkdir(&target).unwrap();
        assert!(created.is_dir());
        assert_eq!(created, normalize(&target));
    }

    #[test]
    fn create_target_inside_existing_dir_makes_new_folder() {
        let dir = tmp();
        let t = create_target(&display_path(dir.path()), dir.path());
        assert_eq!(t, dir.path().join("新資料夾"));
        fs::create_dir(&t).unwrap();
        let t2 = create_target(&display_path(dir.path()), dir.path());
        assert_eq!(t2, dir.path().join("新資料夾 (2)"));
    }

    #[test]
    fn create_target_typed_missing_path() {
        let dir = tmp();
        let typed = dir.path().join("proj");
        let t = create_target(&display_path(&typed), dir.path());
        assert_eq!(t, typed);
    }

    #[test]
    fn workspace_of_file_uses_parent() {
        let dir = tmp();
        let file = dir.path().join("a.png");
        fs::write(&file, b"x").unwrap();
        let e = Entry {
            name: "a.png".into(),
            path: file,
            is_dir: false,
            is_parent: false,
        };
        assert_eq!(workspace_of(dir.path(), Some(&e)), normalize(dir.path()));
    }

    #[test]
    fn workspace_of_dir_uses_that_dir() {
        let dir = tmp();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        let e = Entry {
            name: "src".into(),
            path: src.clone(),
            is_dir: true,
            is_parent: false,
        };
        assert_eq!(workspace_of(dir.path(), Some(&e)), normalize(&src));
    }

    #[test]
    fn existing_dir_file_walks_to_parent() {
        let dir = tmp();
        let file = dir.path().join("x.txt");
        fs::write(&file, b"x").unwrap();
        assert_eq!(existing_dir(&file), normalize(dir.path()));
    }

    #[cfg(windows)]
    #[test]
    fn display_path_strips_verbatim_prefix() {
        let p = PathBuf::from(r"\\?\C:\Users\jason");
        assert_eq!(display_path(&p), r"C:\Users\jason");
        let unc = PathBuf::from(r"\\?\UNC\server\share");
        assert_eq!(display_path(&unc), r"\\server\share");
    }
}
