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

/// Release-installer location (`~/.grokaagent/bin`), not Cargo's bin dir.
pub fn release_bin_dir() -> Result<PathBuf> {
    release_bin_dir_from(dirs::home_dir())
}

pub fn release_bin_dir_from(home: Option<PathBuf>) -> Result<PathBuf> {
    let home = home.ok_or_else(|| Error::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "cannot resolve home directory",
    )))?;
    Ok(home.join(".grokaagent").join("bin"))
}

pub fn path_contains(dir: &Path) -> bool {
    let want = canonicalize_or_clone(dir);
    env::split_paths(&env::var_os("PATH").unwrap_or_default()).any(|p| canonicalize_or_clone(&p) == want)
}

pub fn old_sidecar(path: &Path) -> PathBuf {
    match path.file_name() {
        Some(name) => path.with_file_name(format!("{}.old", name.to_string_lossy())),
        None => path.with_extension("old"),
    }
}

/// Drop the Windows leftover from a previous self-replace (`grokaagent.exe.old`).
pub fn cleanup_old_exe() {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let _ = fs::remove_file(old_sidecar(&exe));
}

/// Replace `dest` with `src`. On Windows, a running destination is renamed aside
/// first so the new file can take its name.
pub fn replace_exe(src: &Path, dest: &Path) -> Result<PathBuf> {
    if same_file(src, dest) {
        return Ok(dest.to_path_buf());
    }
    let dest_dir = dest.parent().ok_or_else(|| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination has no parent directory",
        ))
    })?;
    fs::create_dir_all(dest_dir)?;
    let tmp = dest_dir.join(format!(".grokaagent-{}.tmp", std::process::id()));
    let _ = fs::remove_file(&tmp);
    fs::copy(src, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    }
    let old = old_sidecar(dest);
    let mut parked = false;
    if dest.exists() {
        let _ = fs::remove_file(&old);
        if fs::rename(dest, &old).is_ok() {
            parked = true;
        }
    }
    let put = fs::rename(&tmp, dest).or_else(|_| {
        fs::copy(&tmp, dest).map(|_| {
            let _ = fs::remove_file(&tmp);
        })
    });
    match put {
        Ok(()) => {
            let _ = fs::remove_file(&tmp);
            Ok(dest.to_path_buf())
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            if parked {
                let _ = fs::rename(&old, dest);
            }
            Err(e.into())
        }
    }
}

pub fn install_exe(src: &Path, dest_dir: &Path) -> Result<PathBuf> {
    replace_exe(src, &dest_path(dest_dir))
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
    fn release_bin_is_under_home_grokaagent() {
        let home = PathBuf::from("home").join("x");
        let dir = release_bin_dir_from(Some(home.clone())).unwrap();
        assert_eq!(dir, home.join(".grokaagent").join("bin"));
    }

    #[test]
    fn old_sidecar_keeps_full_file_name() {
        let dest = PathBuf::from("bin").join("grokaagent.exe");
        assert_eq!(old_sidecar(&dest), PathBuf::from("bin").join("grokaagent.exe.old"));
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

    #[test]
    fn replace_exe_overwrites_and_leaves_old_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("grokaagent-bin");
        fs::write(&dest, b"v1").unwrap();
        let src = dir.path().join("next");
        fs::write(&src, b"v2").unwrap();
        replace_exe(&src, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"v2");
        let old = old_sidecar(&dest);
        if old.exists() {
            assert_eq!(fs::read(&old).unwrap(), b"v1");
        }
    }
}
