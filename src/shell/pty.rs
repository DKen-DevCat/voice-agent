//! A shell in a pseudo terminal of its own, with nobody looking at it.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

pub struct Pty {
    master: OwnedFd,
    writer: Mutex<File>,
    child: Mutex<Child>,
    pub pid: u32,
}

impl Pty {
    /// Starts shell -l in a pty of rows x cols, in dir, with env.
    pub fn spawn(shell: &str, dir: &Path, env: Vec<(OsString, OsString)>, rows: u16, cols: u16) -> io::Result<Pty> {
        let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        let pty = nix::pty::openpty(Some(&ws), None).map_err(io::Error::from)?;
        // SAFETY: plain fcntl on a descriptor we own; the shell must not inherit the master
        unsafe { libc::fcntl(pty.master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };

        let mut cmd = Command::new(shell);
        cmd.arg("-l").env_clear().envs(env);
        if dir.is_dir() {
            cmd.current_dir(dir);
        }
        cmd.stdin(Stdio::from(pty.slave.try_clone()?)).stdout(Stdio::from(pty.slave.try_clone()?)).stderr(Stdio::from(pty.slave));
        // SAFETY: only async-signal-safe calls between fork and exec; stdin is
        // the pty slave by then, which becomes the controlling terminal
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        drop(cmd); // closes our copies of the slave
        let writer = Mutex::new(File::from(pty.master.try_clone()?));
        Ok(Pty { master: pty.master, writer, pid: child.id(), child: Mutex::new(child) })
    }

    /// Sends bytes to the shell as if typed.
    pub fn write(&self, b: &[u8]) {
        let _ = self.writer.lock().unwrap_or_else(|e| e.into_inner()).write_all(b);
    }

    /// Reads what the shell prints, chunk by chunk, until it is gone.
    pub fn read_loop(&self, mut on_data: impl FnMut(&[u8])) {
        let Ok(fd) = self.master.try_clone() else { return };
        let mut master = File::from(fd);
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match master.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => on_data(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return, // EIO once the shell is gone
            }
        }
    }

    /// Whether the shell has exited (and reaps it).
    pub fn exited(&self) -> bool {
        !matches!(self.child.lock().unwrap_or_else(|e| e.into_inner()).try_wait(), Ok(None))
    }

    /// Ends the shell as closing its terminal would.
    pub fn hangup(&self) {
        // SAFETY: signals the shell we started (and its process group), by pid
        unsafe {
            libc::kill(-(self.pid as libc::pid_t), libc::SIGHUP);
            libc::kill(self.pid as libc::pid_t, libc::SIGHUP);
        }
        let _ = self.child.lock().unwrap_or_else(|e| e.into_inner()).try_wait();
    }
}
