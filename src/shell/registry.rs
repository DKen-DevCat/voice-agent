//! The background shells, by name. Each is a zsh in a pty with the OSC 633
//! bridge, a vt100 model of its screen, and a log of everything it printed
//! (`shells/<name>.log` in the runtime directory).

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use super::bridge::{Bridge, RunResult, ShellStatus, strip_ansi};
use super::pty::Pty;
use super::zsh::{shell_env, write_zsh_files};
use crate::log::log;
use crate::util::private_dir;

pub const MAIN: &str = "main";
const ROWS: u16 = 50;
const COLS: u16 = 160;
const SCROLLBACK: usize = 5000;

#[derive(Clone, Debug)]
pub struct ShellOptions {
    pub shell: String,
    pub dir: PathBuf,         // where new shells start
    pub runtime_dir: PathBuf, // zsh/, shells/ and the command files go here
}

pub struct Shell {
    pub name: String,
    pub bridge: Bridge,
    pub log_path: PathBuf,
    pty: Arc<Pty>,
    screen: Mutex<vt100::Parser>,
    log_file: Mutex<Option<File>>,
}

impl Shell {
    fn start(name: &str, dir: &std::path::Path, o: &ShellOptions) -> Result<Arc<Shell>, String> {
        let zdot = o.runtime_dir.join("zsh");
        write_zsh_files(&zdot).map_err(|e| format!("{}: {e}", zdot.display()))?;
        let shells = o.runtime_dir.join("shells");
        private_dir(&shells).map_err(|e| format!("{}: {e}", shells.display()))?;
        let cmd_file = shells.join(format!("{name}.cmd"));
        let log_path = shells.join(format!("{name}.log"));
        let log_file = File::create(&log_path).ok();
        let env = shell_env(&o.shell, &zdot, &cmd_file, name);
        let pty = Arc::new(Pty::spawn(&o.shell, dir, env, ROWS, COLS).map_err(|e| format!("{}: {e}", o.shell))?);
        let p = pty.clone();
        let bridge = Bridge::new(move |b| p.write(b), cmd_file);
        let shell = Arc::new(Shell {
            name: name.to_string(),
            bridge,
            log_path,
            pty,
            screen: Mutex::new(vt100::Parser::new(ROWS, COLS, SCROLLBACK)),
            log_file: Mutex::new(log_file),
        });
        let s = shell.clone();
        std::thread::spawn(move || {
            s.pty.read_loop(|data| {
                let shown = s.bridge.feed(data);
                s.screen().process(&shown);
                s.append_log(&strip_ansi(&String::from_utf8_lossy(&shown)));
            });
            log(&format!("shell {}: gone", s.name));
        });
        Ok(shell)
    }

    fn screen(&self) -> MutexGuard<'_, vt100::Parser> {
        self.screen.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn append_log(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(f) = self.log_file.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = f.write_all(text.replace("\r\n", "\n").as_bytes());
        }
    }

    /// Runs a command and waits for it (see Bridge::run). The log marks where
    /// each command starts.
    pub fn run(&self, command: &str, timeout: Duration) -> Result<RunResult, String> {
        self.append_log(&format!("\n### {} $ {command}\n", chrono::Local::now().format("%H:%M:%S")));
        self.bridge.run(command, timeout)
    }

    /// Types raw text into the shell (an answer to a prompt).
    pub fn input(&self, text: &str) {
        self.append_log(&format!("\n### input {text:?}\n"));
        self.pty.write(text.as_bytes());
    }

    pub fn interrupt(&self) {
        self.bridge.interrupt();
    }

    pub fn status(&self) -> ShellStatus {
        self.bridge.status()
    }

    pub fn alive(&self) -> bool {
        !self.pty.exited()
    }

    /// What the shell's screen shows, preceded by up to scrollback lines of
    /// history, as plain text.
    pub fn screen_text(&self, scrollback: usize) -> String {
        let mut p = self.screen();
        let (rows, cols) = p.screen().size();
        let mut lines: Vec<String> = Vec::new();
        if scrollback > 0 {
            p.screen_mut().set_scrollback(scrollback);
            let mut off = p.screen().scrollback();
            while off > 0 {
                p.screen_mut().set_scrollback(off);
                let take = off.min(rows as usize);
                lines.extend(p.screen().rows(0, cols).take(take));
                off -= take;
            }
            p.screen_mut().set_scrollback(0);
        }
        lines.extend(p.screen().rows(0, cols));
        let lines: Vec<&str> = lines.iter().map(|l| l.trim_end()).collect();
        lines.join("\n").trim_end_matches('\n').to_string()
    }

    fn close(&self) {
        self.pty.hangup();
    }
}

/// Whether name can name a shell (it becomes part of file names).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 32 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub struct Registry {
    opts: ShellOptions,
    shells: Mutex<Vec<Arc<Shell>>>,
}

impl Registry {
    pub fn new(opts: ShellOptions) -> Registry {
        Registry { opts, shells: Mutex::new(Vec::new()) }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Arc<Shell>>> {
        self.shells.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The shell named name, if it is there and alive.
    pub fn get(&self, name: &str) -> Option<Arc<Shell>> {
        self.lock().iter().find(|s| s.name == name && s.alive()).cloned()
    }

    /// The shell named name; main is started (again) when it is not there.
    pub fn shell(&self, name: &str) -> Result<Arc<Shell>, String> {
        if let Some(s) = self.get(name) {
            return Ok(s);
        }
        if name == MAIN {
            return self.start(MAIN, None);
        }
        Err(format!("there is no shell named {name:?} (see shell_list; make one with shell_new)"))
    }

    /// Starts a shell named name in dir (default: the configured directory)
    /// and waits for its first prompt.
    pub fn start(&self, name: &str, dir: Option<PathBuf>) -> Result<Arc<Shell>, String> {
        if !valid_name(name) {
            return Err(format!("{name:?} is not a usable shell name (letters, digits, - and _; up to 32)"));
        }
        if self.get(name).is_some() {
            return Err(format!("a shell named {name:?} already exists"));
        }
        let dir = dir.unwrap_or_else(|| self.opts.dir.clone());
        let shell = Shell::start(name, &dir, &self.opts)?;
        {
            let mut shells = self.lock();
            shells.retain(|s| s.name != name);
            shells.push(shell.clone());
        }
        if !shell.bridge.wait_ready(Duration::from_secs(20)) {
            log(&format!("shell {name}: no prompt within 20 s"));
        }
        log(&format!("shell {name}: started in {} (pid {})", dir.display(), shell.pty.pid));
        Ok(shell)
    }

    pub fn list(&self) -> Vec<Arc<Shell>> {
        self.lock().iter().filter(|s| s.alive()).cloned().collect()
    }

    /// Ctrl-C to every shell that is running something.
    pub fn interrupt_all(&self) {
        for s in self.list() {
            s.interrupt();
        }
    }

    pub fn close_all(&self) {
        for s in self.lock().drain(..) {
            s.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_name("main") && valid_name("build-2") && valid_name("a_b"));
        assert!(!valid_name("") && !valid_name("../x") && !valid_name("a b") && !valid_name(&"x".repeat(33)));
    }

    /// Runs real commands in a real zsh (skipped where there is none).
    #[test]
    fn runs_commands_in_a_background_zsh() {
        if !std::path::Path::new("/bin/zsh").exists() {
            return;
        }
        let rt = std::env::temp_dir().join(format!("va-shells-{}", std::process::id()));
        let reg = Registry::new(ShellOptions { shell: "/bin/zsh".into(), dir: std::env::temp_dir(), runtime_dir: rt.clone() });
        let main = reg.shell(MAIN).unwrap();
        let r = main.run("cd / && echo hello && false", Duration::from_secs(20)).unwrap();
        assert_eq!((r.exit_code, r.output.as_str()), (Some(1), "hello"));
        assert_eq!(main.status().cwd, "/", "the directory persists");
        let r = main.run("pwd", Duration::from_secs(20)).unwrap();
        assert_eq!(r.output, "/");
        // A command that outlives its timeout is interrupted
        let r = main.run("sleep 30", Duration::from_millis(500)).unwrap();
        assert!(r.timed_out, "{r:?}");
        // An unterminated quote is cancelled, not left waiting
        let r = main.run("echo 'oops", Duration::from_secs(20)).unwrap();
        assert!(r.incomplete, "{r:?}");
        assert!(main.screen_text(0).contains("hello"));
        assert!(std::fs::read_to_string(&main.log_path).unwrap().contains("$ cd / && echo hello"));
        // Shells by name
        assert!(reg.shell("other").is_err());
        let other = reg.start("other", Some("/tmp".into())).unwrap();
        assert!(reg.start("other", None).is_err());
        assert!(other.run("echo $VA_SHELL", Duration::from_secs(20)).unwrap().output.ends_with("other"));
        assert_eq!(reg.list().len(), 2);
        reg.close_all();
        let _ = std::fs::remove_dir_all(&rt);
    }
}
