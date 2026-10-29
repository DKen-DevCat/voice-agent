//! The zsh integration files and the environment a background shell gets.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use crate::util::{home_dir, private_dir, write_private};

const ZSH_FILES: [(&str, &str); 3] =
    [(".zshenv", include_str!("zsh/.zshenv")), (".zprofile", include_str!("zsh/.zprofile")), (".zshrc", include_str!("zsh/.zshrc"))];

/// Puts the integration files in dir (ZDOTDIR of the shells).
pub fn write_zsh_files(dir: &Path) -> std::io::Result<()> {
    private_dir(dir)?;
    for (name, body) in ZSH_FILES {
        write_private(&dir.join(name), body.as_bytes())?;
    }
    Ok(())
}

/// The shell's environment: ours, without voice-agent's own settings and the
/// markers of a Claude Code session, with zsh pointed at the integration.
pub fn shell_env(shell: &str, zdot: &Path, cmd_file: &Path, name: &str) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(k, _)| {
            let k = k.to_string_lossy();
            k != "ZDOTDIR" && !k.starts_with("VA_") && k != "CLAUDECODE" && k != "CLAUDE_CODE_ENTRYPOINT"
        })
        .collect();
    if std::env::var_os("LANG").is_none_or(|v| v.is_empty()) {
        env.push(("LANG".into(), "en_US.UTF-8".into()));
    }
    env.push(("TERM".into(), "xterm-256color".into()));
    if Path::new(shell).file_name() == Some(OsStr::new("zsh")) {
        let user_zdot = std::env::var_os("ZDOTDIR").filter(|v| !v.is_empty() && Path::new(v) != zdot).unwrap_or_else(|| home_dir().into_os_string());
        env.push(("ZDOTDIR".into(), zdot.into()));
        env.push(("VA_USER_ZDOTDIR".into(), user_zdot));
        env.push(("VA_CMD_FILE".into(), cmd_file.into()));
    }
    env.push(("VA_SHELL".into(), name.into()));
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_points_zsh_at_the_integration() {
        let env = shell_env("/bin/zsh", Path::new("/tmp/rt/zsh"), Path::new("/tmp/rt/cmd-main"), "main");
        let get = |k: &str| env.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.to_string_lossy().into_owned());
        assert_eq!(get("ZDOTDIR").as_deref(), Some("/tmp/rt/zsh"));
        assert_eq!(get("VA_CMD_FILE").as_deref(), Some("/tmp/rt/cmd-main"));
        assert_eq!(get("VA_SHELL").as_deref(), Some("main"));
        assert_eq!(get("PATH"), std::env::var("PATH").ok());
        assert!(get("CLAUDECODE").is_none());
    }

    #[test]
    fn writes_the_files() {
        let dir = std::env::temp_dir().join(format!("va-zsh-{}", std::process::id()));
        write_zsh_files(&dir).unwrap();
        let rc = std::fs::read_to_string(dir.join(".zshrc")).unwrap();
        assert!(rc.contains("633;D") && rc.contains("RM_STAR_SILENT") && !rc.contains("GHOSTTY"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
