//! Long-running work (builds, test suites, servers): plain child processes,
//! not through a pty, each in a process group of its own with its output in
//! `jobs/<id>.log` in the runtime directory.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::log::log;
use crate::util::{private_dir, unix_millis};

/// How much of the end of a log job_tail reads at most.
const TAIL_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct JobInfo {
    pub id: u32,
    pub command: String,
    pub dir: PathBuf,
    pub log_path: PathBuf,
    pub pid: u32,
    pub started: i64, // unix milliseconds
    pub ended: i64,   // 0 while it runs
    pub exit: Option<i32>,
    pub stopped: bool, // ended by job_stop (or by voice-agent quitting)
}

impl JobInfo {
    pub fn running(&self) -> bool {
        self.ended == 0
    }
}

pub struct Jobs {
    shell: String,
    dir: PathBuf, // where jobs start by default
    log_dir: PathBuf,
    st: Arc<Mutex<Vec<JobInfo>>>,
}

impl Jobs {
    pub fn new(shell: &str, dir: PathBuf, runtime_dir: PathBuf) -> Jobs {
        Jobs { shell: shell.to_string(), dir, log_dir: runtime_dir.join("jobs"), st: Arc::default() }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<JobInfo>> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts command (`zsh -lc`) in dir and returns at once.
    pub fn start(&self, command: &str, dir: Option<PathBuf>) -> Result<JobInfo, String> {
        let dir = dir.unwrap_or_else(|| self.dir.clone());
        if !dir.is_dir() {
            return Err(format!("{} is not a directory", dir.display()));
        }
        private_dir(&self.log_dir).map_err(|e| format!("{}: {e}", self.log_dir.display()))?;
        let id = self.lock().iter().map(|j| j.id).max().unwrap_or(0) + 1;
        let log_path = self.log_dir.join(format!("{id}.log"));
        let out = File::create(&log_path).map_err(|e| format!("{}: {e}", log_path.display()))?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let env = std::env::vars_os().filter(|(k, _)| {
            let k = k.to_string_lossy();
            !k.starts_with("VA_") && k != "CLAUDECODE" && k != "CLAUDE_CODE_ENTRYPOINT"
        });
        let mut child = Command::new(&self.shell)
            .args(["-l", "-c", command])
            .current_dir(&dir)
            .env_clear()
            .envs(env)
            .envs([("PAGER", "cat"), ("GIT_PAGER", "cat"), ("GIT_EDITOR", "true"), ("EDITOR", "true")])
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .process_group(0)
            .spawn()
            .map_err(|e| format!("{}: {e}", self.shell))?;
        let info = JobInfo { id, command: command.to_string(), dir, log_path, pid: child.id(), started: unix_millis(), ..JobInfo::default() };
        self.lock().push(info.clone());
        log(&format!("job {id}: started (pid {}): {command}", info.pid));
        let st = self.st.clone();
        std::thread::spawn(move || {
            let status = child.wait();
            let code = status.as_ref().ok().and_then(|s| s.code().or_else(|| s.signal().map(|n| 128 + n)));
            log(&format!("job {id}: ended ({code:?})"));
            let mut st = st.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(j) = st.iter_mut().find(|j| j.id == id) {
                j.ended = unix_millis();
                j.exit = code;
            }
        });
        Ok(info)
    }

    pub fn get(&self, id: u32) -> Option<JobInfo> {
        self.lock().iter().find(|j| j.id == id).cloned()
    }

    /// Every job, newest first.
    pub fn list(&self) -> Vec<JobInfo> {
        let mut v = self.lock().clone();
        v.reverse();
        v
    }

    pub fn running(&self) -> usize {
        self.lock().iter().filter(|j| j.running()).count()
    }

    /// The last lines of a job's log (at most lines, from its last 256 KB).
    pub fn tail(&self, id: u32, lines: usize) -> Result<String, String> {
        let j = self.get(id).ok_or_else(|| format!("there is no job {id}"))?;
        let mut f = File::open(&j.log_path).map_err(|e| e.to_string())?;
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let from = len.saturating_sub(TAIL_BYTES);
        f.seek(SeekFrom::Start(from)).map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        let text = crate::shell::bridge::clean_output(&String::from_utf8_lossy(&buf));
        let all: Vec<&str> = text.lines().collect();
        Ok(all[all.len().saturating_sub(lines)..].join("\n"))
    }

    /// Ends a job's process group: SIGTERM, then SIGKILL if it is still there
    /// after 3 seconds.
    pub fn stop(&self, id: u32) -> Result<JobInfo, String> {
        let j = {
            let mut st = self.lock();
            let j = st.iter_mut().find(|j| j.id == id).ok_or_else(|| format!("there is no job {id}"))?;
            if !j.running() {
                return Ok(j.clone());
            }
            j.stopped = true;
            j.clone()
        };
        signal_group(j.pid, libc::SIGTERM);
        let st = self.st.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            if st.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|x| x.id == id && x.running()) {
                signal_group(j.pid, libc::SIGKILL);
            }
        });
        Ok(j)
    }

    pub fn stop_all(&self) {
        let ids: Vec<u32> = self.lock().iter().filter(|j| j.running()).map(|j| j.id).collect();
        for id in ids {
            let _ = self.stop(id);
        }
    }
}

fn signal_group(pid: u32, sig: libc::c_int) {
    // SAFETY: signals the process group of a child we started with process_group(0)
    unsafe { libc::kill(-(pid as libc::pid_t), sig) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_until(f: impl Fn() -> bool) {
        for _ in 0..300 {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out");
    }

    #[test]
    fn jobs_run_in_the_background() {
        let rt = std::env::temp_dir().join(format!("va-jobs-{}", std::process::id()));
        let jobs = Jobs::new("/bin/sh", std::env::temp_dir(), rt.clone());
        let a = jobs.start("echo one; echo two; exit 3", None).unwrap();
        wait_until(|| jobs.get(a.id).is_some_and(|j| !j.running()));
        let a = jobs.get(a.id).unwrap();
        assert_eq!(a.exit, Some(3));
        assert_eq!(jobs.tail(a.id, 1).unwrap(), "two");
        assert_eq!(jobs.tail(a.id, 10).unwrap(), "one\ntwo");

        let b = jobs.start("sleep 30", Some("/tmp".into())).unwrap();
        assert_eq!(jobs.running(), 1);
        assert!(jobs.stop(b.id).unwrap().stopped);
        wait_until(|| jobs.running() == 0);
        assert_eq!(jobs.list()[0].id, b.id, "newest first");
        assert!(jobs.stop(99).is_err() && jobs.tail(99, 1).is_err());
        assert!(jobs.start("true", Some("/nonexistent".into())).is_err());
        let _ = std::fs::remove_dir_all(&rt);
    }
}
