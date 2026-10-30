//! The log file (`voice-agent.log` in the runtime directory) and where the
//! runtime files live.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::util::home_dir;

/// Where logs, shell output, job output and the zsh integration go:
/// `$VA_RUNTIME_DIR`, else `~/Library/Caches/voice-agent`.
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("VA_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => home_dir().join("Library/Caches/voice-agent"),
    }
}

static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// Sends the log lines to this file; before that (and in tests) they are dropped.
pub fn init(path: &Path) -> std::io::Result<()> {
    let f = File::create(path)?;
    let _ = LOG_FILE.set(Mutex::new(f));
    Ok(())
}

pub fn log(msg: &str) {
    if let Some(f) = LOG_FILE.get() {
        let stamp = chrono::Local::now().format("%Y/%m/%d %H:%M:%S%.6f");
        if let Ok(mut f) = f.lock() {
            let _ = writeln!(f, "{stamp} {msg}");
        }
    }
}

#[macro_export]
macro_rules! logf {
    ($($arg:tt)*) => { $crate::log::log(&format!($($arg)*)) };
}
