//! Install the grokaagent binary onto the user PATH (`$CARGO_HOME/bin`).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub fn exe_name() -> &'static str {
    if cfg!(windows) {
        "grokaagent.exe"
    } else {
        "grokaagent"
    }
}

pub fn cargo_bin_dir() -> Result<PathBuf> {
    cargo_bin_dir_from(env::var_os("CARGO_HOME").map(PathBuf::from), dirs::home_dir())
}

pub fn cargo_bin_dir_from(cargo_home: Option<PathBuf>, home: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(home) = cargo_home {
        return Ok(home.join("bin"));
    }
    let home = home.ok_or_else(|| Error::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "cannot resolve home directory",
    )))?;
    Ok(home.join(".cargo").join("bin"))
}

pub fn dest_path(bin_dir: &Path) -> PathBuf {
    bin_dir.join(exe_name())
}

pub fn path_contains(dir: &Path) -> bool {
    let want = canonicalize_or_clone(dir);
    env::split_paths(&env::var_os("PATH").unwrap_or_default()).any(|p| canonicalize_or_clone(&p) == want)
}

pub fn install_exe(src: &Path, dest_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dest_dir)?;
    let dest = dest_path(dest_dir);
    if same_file(src, &dest) {
        return Ok(dest);
    }
    let tmp = dest_dir.join(format!(".grokaagent-{}.tmp", std::process::id()));
    fs::copy(src, &tmp)?;
    if let Err(e) = fs::rename(&tmp, &dest) {
        if let Err(copy_err) = fs::copy(&tmp, &dest) {
            let _ = fs::remove_file(&tmp);
            return Err(copy_err.into());
        }
        let _ = fs::remove_file(&tmp);
        if dest.exists() {
            return Ok(dest);
        }
        return Err(e.into());
    }
    Ok(dest)
}

pub fn install_current_exe() -> Result<PathBuf> {
    let src = env::current_exe()?;
    install_exe(&src, &cargo_bin_dir()?)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn canonicalize_or_clone(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn cargo_bin_prefers_cargo_home() {
        let cargo_home = PathBuf::from("opt").join("cargo");
        let dir = cargo_bin_dir_from(Some(cargo_home.clone()), Some(PathBuf::from("home"))).unwrap();
        assert_eq!(dir, cargo_home.join("bin"));
    }

    #[test]
    fn cargo_bin_falls_back_to_home_dot_cargo() {
        let home = PathBuf::from("home").join("x");
        let dir = cargo_bin_dir_from(None, Some(home.clone())).unwrap();
        assert_eq!(dir, home.join(".cargo").join("bin"));
    }

    #[test]
    fn cargo_bin_errors_without_home() {
        assert!(cargo_bin_dir_from(None, None).is_err());
    }

    #[test]
    fn install_exe_copies_into_bin_dir() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("built");
        let mut f = fs::File::create(&src).unwrap();
        f.write_all(b"groka-bin").unwrap();
        drop(f);
        let dest_dir = dir.path().join("bin");
        let dest = install_exe(&src, &dest_dir).unwrap();
        assert_eq!(dest, dest_dir.join(exe_name()));
        assert_eq!(fs::read(&dest).unwrap(), b"groka-bin");
    }

    #[test]
    fn install_exe_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("built");
        fs::write(&src, b"v2").unwrap();
        let dest_dir = dir.path().join("bin");
        fs::create_dir_all(&dest_dir).unwrap();
        fs::write(dest_dir.join(exe_name()), b"v1").unwrap();
        let dest = install_exe(&src, &dest_dir).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"v2");
    }

    #[test]
    fn install_exe_is_noop_when_src_is_dest() {
        let dir = tempfile::tempdir().unwrap();
        let dest_dir = dir.path().join("bin");
        fs::create_dir_all(&dest_dir).unwrap();
        let dest = dest_dir.join(exe_name());
        fs::write(&dest, b"same").unwrap();
        let out = install_exe(&dest, &dest_dir).unwrap();
        assert_eq!(out, dest);
        assert_eq!(fs::read(&dest).unwrap(), b"same");
    }
}
