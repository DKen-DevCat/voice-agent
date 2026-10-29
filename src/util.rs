//! Small helpers shared by the modules: paths and time.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// Finds an executable on PATH, or in the places Homebrew and Claude Code
/// install to when the app was not started from a login shell.
pub fn look_path(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.push(home_dir().join(".local/bin"));
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs.into_iter().map(|d| d.join(name)).find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| !m.is_dir() && m.permissions().mode() & 0o111 != 0)
}

pub fn unix_millis() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

pub fn unix_secs() -> i64 {
    unix_millis() / 1000
}

/// Keeps the first n characters of s, with an ellipsis when it was longer.
pub fn clip_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

/// A duration as "12s", "1m23s" or "1h02m".
pub fn fmt_elapsed(secs: f64) -> String {
    let d = secs.round() as i64;
    if d < 0 {
        "0s".into()
    } else if d < 60 {
        format!("{d}s")
    } else if d < 3600 {
        format!("{}m{:02}s", d / 60, d % 60)
    } else {
        format!("{}h{:02}m", d / 3600, d / 60 % 60)
    }
}

/// Creates a directory (and its parents) only the user can enter.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// Writes a file only the user can read.
pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed() {
        assert_eq!(fmt_elapsed(-3.0), "0s");
        assert_eq!(fmt_elapsed(12.4), "12s");
        assert_eq!(fmt_elapsed(83.0), "1m23s");
        assert_eq!(fmt_elapsed(3720.0), "1h02m");
    }

    #[test]
    fn clip() {
        assert_eq!(clip_chars("abc", 3), "abc");
        assert_eq!(clip_chars("あいうえ", 2), "あい…");
    }
}
