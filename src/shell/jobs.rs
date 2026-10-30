//! Long-running work (builds, test suites, servers): plain child processes,
//! not through a pty, each in a session (and process group) of its own,
//! without voice-agent's terminal, with its output in `jobs/<id>.log` in the
//! runtime directory.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

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
    pub stopped: bool, // ended by job_stop, Esc, or voice-agent quitting
}

/// How long a stopped job's processes get after SIGTERM, before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(3);

impl JobInfo {
    pub fn running(&self) -> bool {
        self.ended == 0
    }
}

pub struct Jobs {
    shell: String,
    dir: PathBuf, // where jobs start by default
    log_dir: PathBuf,
    last_id: AtomicU32,
    st: Arc<Mutex<Vec<JobInfo>>>,
}

impl Jobs {
    pub fn new(shell: &str, dir: PathBuf, runtime_dir: PathBuf) -> Jobs {
        Jobs { shell: shell.to_string(), dir, log_dir: runtime_dir.join("jobs"), last_id: AtomicU32::new(0), st: Arc::default() }
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
        let id = self.last_id.fetch_add(1, Ordering::SeqCst) + 1;
        let log_path = self.log_dir.join(format!("{id}.log"));
        let out = File::create(&log_path).map_err(|e| format!("{}: {e}", log_path.display()))?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let env = std::env::vars_os().filter(|(k, _)| {
            let k = k.to_string_lossy();
            !k.starts_with("VA_") && k != "CLAUDECODE" && k != "CLAUDE_CODE_ENTRYPOINT"
        });
        let mut cmd = Command::new(&self.shell);
        cmd.args(["-l", "-c", command])
            .current_dir(&dir)
            .env_clear()
            .envs(env)
            .envs([("PAGER", "cat"), ("GIT_PAGER", "cat"), ("GIT_EDITOR", "true"), ("EDITOR", "true"), ("VISUAL", "true")])
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err));
        // SAFETY: setsid is async-signal-safe. A session of its own takes the
        // job off voice-agent's terminal (a prompt for a password fails instead
        // of drawing over the window and stopping the job), and its process
        // group is the one job_stop signals.
        unsafe {
            cmd.pre_exec(|| if libc::setsid() == -1 { Err(io::Error::last_os_error()) } else { Ok(()) });
        }
        let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", self.shell))?;
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

    /// Ends a job's process group: SIGTERM, then SIGKILL if anything of it is
    /// still there after STOP_GRACE. The group is signalled even when the
    /// job's shell has ended, since what it started in the background
    /// (`server &`) lives on in it.
    pub fn stop(&self, id: u32) -> Result<JobInfo, String> {
        let j = {
            let mut st = self.lock();
            let j = st.iter_mut().find(|j| j.id == id).ok_or_else(|| format!("there is no job {id}"))?;
            if !j.running() && !group_alive(j.pid) {
                return Ok(j.clone());
            }
            j.stopped = true;
            j.clone()
        };
        signal_group(j.pid, libc::SIGTERM);
        std::thread::spawn(move || {
            std::thread::sleep(STOP_GRACE);
            if group_alive(j.pid) {
                signal_group(j.pid, libc::SIGKILL);
            }
        });
        Ok(j)
    }

    /// Stops every job that has anything left, and waits for them (SIGKILL
    /// after STOP_GRACE): Esc, or voice-agent quitting. Returns the jobs it
    /// stopped.
    pub fn stop_all(&self) -> Vec<JobInfo> {
        let jobs: Vec<JobInfo> = {
            let mut st = self.lock();
            st.iter_mut()
                .filter(|j| j.running() || group_alive(j.pid))
                .map(|j| {
                    j.stopped = true;
                    j.clone()
                })
                .collect()
        };
        let pids: Vec<u32> = jobs.iter().map(|j| j.pid).collect();
        for &pid in &pids {
            signal_group(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline && pids.iter().any(|&p| group_alive(p)) {
            std::thread::sleep(Duration::from_millis(50));
        }
        for &pid in pids.iter().filter(|&&p| group_alive(p)) {
            signal_group(pid, libc::SIGKILL);
        }
        jobs
    }
}

fn signal_group(pid: u32, sig: libc::c_int) {
    // SAFETY: signals the process group of a child we started with setsid
    unsafe { libc::kill(-(pid as libc::pid_t), sig) };
}

/// Whether anything of a job's process group is still there.
fn group_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the group exists
    pid > 0 && unsafe { libc::kill(-(pid as libc::pid_t), 0) } == 0
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

    /// What a job starts in the background outlives its shell, and is still
    /// stopped; a job has no terminal to ask on.
    #[test]
    fn stopping_reaches_what_a_job_left_behind() {
        let rt = std::env::temp_dir().join(format!("va-jobs-bg-{}", std::process::id()));
        let jobs = Jobs::new("/bin/sh", std::env::temp_dir(), rt.clone());
        let a = jobs.start("sleep 30 & echo started", None).unwrap();
        wait_until(|| jobs.get(a.id).is_some_and(|j| !j.running()));
        assert!(group_alive(a.pid), "the background sleep lives on in the group");
        assert!(jobs.stop(a.id).unwrap().stopped);
        wait_until(|| !group_alive(a.pid));

        let b = jobs.start("sleep 30 & sleep 30", None).unwrap();
        let c = jobs.start("tty; true", None).unwrap();
        assert!(b.id != c.id);
        wait_until(|| jobs.get(c.id).is_some_and(|j| !j.running()));
        assert_eq!(jobs.tail(c.id, 1).unwrap(), "not a tty");
        jobs.stop_all();
        assert!(!group_alive(b.pid), "quitting waits until the job is gone");
        let _ = std::fs::remove_dir_all(&rt);
    }
}
