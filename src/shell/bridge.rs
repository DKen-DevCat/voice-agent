//! Drives one background shell on behalf of the AI: it hands the shell a
//! command to run and reads the result back through the OSC 633 markers
//! emitted by zsh/.zshrc. The markers are taken out of the stream; the rest
//! goes on to the screen model and the shell's log.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;
use regex::bytes::Regex as BytesRegex;

use crate::util::write_private;

// Key sequences bound to widgets in zsh/.zshrc
const RUN_KEY: &[u8] = b"\x1b[5151~";
const CLEAR_KEY: &[u8] = b"\x1b[5152~";

const MARKER_START: &[u8] = b"\x1b]633;";
/// A marker that never ends is not held back beyond this.
const MAX_CARRY: usize = 4096;
/// A command's output kept in memory (its end); the log has all of it.
const MAX_JOB_OUTPUT: usize = 4 << 20;
/// How long a command waits for the shell to be back at its prompt.
const IDLE_WAIT: Duration = Duration::from_secs(8);

fn marker_re() -> &'static BytesRegex {
    static RE: OnceLock<BytesRegex> = OnceLock::new();
    RE.get_or_init(|| BytesRegex::new(r"(?-u)\x1b\]633;([A-Z])(?:;([^\x07\x1b]*))?(?:\x07|\x1b\\)").unwrap())
}

/// What a shell reports about itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShellStatus {
    pub integrated: bool, // a marker has been seen (the shell is zsh with our integration)
    pub idle: bool,       // at its prompt
    pub running: bool,    // an AI command is running
    pub cwd: String,
}

/// Result of a command the AI ran in a shell.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunResult {
    pub exit_code: Option<i32>,
    pub output: String,
    pub cut: bool, // the start of the output was dropped from memory (see the log)
    pub timed_out: bool,
    pub aborted: bool,
    pub incomplete: bool,
}

#[derive(Default)]
struct Job {
    id: u64,
    output: Vec<u8>,
    cut: bool,
    pre_start: Vec<u8>,
    started: bool,
    sent_enter: bool,
    timed_out: bool,
    aborted: bool,
    incomplete: bool,
}

#[derive(Default)]
struct State {
    integrated: bool,
    idle: bool,
    job: Option<Job>,
    finished: Option<(u64, RunResult)>,
    next_job: u64,
    carry: Vec<u8>,
    output_seq: u64,
    cwd: String,
}

type Writer = dyn Fn(&[u8]) + Send + Sync;

struct Inner {
    st: Mutex<State>,
    cond: Condvar,
    write: Box<Writer>,
    cmd_file: PathBuf,
}

/// Every shell has a bridge and a command file of its own.
#[derive(Clone)]
pub struct Bridge(Arc<Inner>);

impl Bridge {
    /// write sends bytes to the shell as if typed; cmd_file is where zsh
    /// reads the command to run ($VA_CMD_FILE).
    pub fn new(write: impl Fn(&[u8]) + Send + Sync + 'static, cmd_file: PathBuf) -> Bridge {
        Bridge(Arc::new(Inner { st: Mutex::new(State::default()), cond: Condvar::new(), write: Box::new(write), cmd_file }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn send(&self, b: &[u8]) {
        (self.0.write)(b);
    }

    /// Sees every chunk the shell prints and returns it without the markers.
    /// A marker cut off by the end of the chunk is held back until the rest
    /// arrives.
    pub fn feed(&self, data: &[u8]) -> Vec<u8> {
        let mut st = self.lock();
        st.output_seq += 1;
        self.0.cond.notify_all();

        let mut text = std::mem::take(&mut st.carry);
        text.extend_from_slice(data);
        if let Some(cut) = rfind(&text, MARKER_START) {
            let tail = &text[cut..];
            if !tail.contains(&0x07) && rfind(tail, b"\x1b\\").is_none() && tail.len() <= MAX_CARRY {
                st.carry = text.split_off(cut);
            }
        } else if let Some(i) = text.iter().rposition(|&c| c == 0x1b)
            && MARKER_START.starts_with(&text[i..])
        {
            st.carry = text.split_off(i);
        }

        let mut out = Vec::with_capacity(text.len());
        let mut last = 0;
        for m in marker_re().captures_iter(&text) {
            let whole = m.get(0).unwrap();
            let chunk = &text[last..whole.start()];
            on_output(&mut st, chunk);
            out.extend_from_slice(chunk);
            let kind = m.get(1).unwrap().as_bytes()[0];
            let arg = m.get(2).map(|a| String::from_utf8_lossy(a.as_bytes()).into_owned()).unwrap_or_default();
            self.on_marker(&mut st, kind, &arg);
            last = whole.end();
        }
        on_output(&mut st, &text[last..]);
        out.extend_from_slice(&text[last..]);
        out
    }

    fn on_marker(&self, st: &mut State, kind: u8, arg: &str) {
        st.integrated = true;
        match kind {
            b'A' => {
                st.idle = true;
                self.0.cond.notify_all();
            }
            b'F' => {
                if let Some(j) = st.job.as_mut()
                    && j.sent_enter
                    && !j.started
                    && !j.incomplete
                {
                    j.incomplete = true;
                    self.send(b"\x03");
                }
            }
            b'C' => {
                st.idle = false;
                if let Some(j) = st.job.as_mut()
                    && j.sent_enter
                {
                    j.started = true;
                }
            }
            b'D' => {
                if st.job.as_ref().is_some_and(|j| j.sent_enter) {
                    self.finish(st, arg.parse().ok());
                }
            }
            b'W' => st.cwd = arg.to_string(),
            _ => {}
        }
    }

    /// Ends the running job with what it printed.
    fn finish(&self, st: &mut State, code: Option<i32>) {
        let Some(j) = st.job.take() else { return };
        // No C marker means zsh never ran it (parse error, empty line)
        let raw = if j.started { &j.output } else { &j.pre_start };
        let result = RunResult {
            exit_code: code,
            output: clean_output(&String::from_utf8_lossy(raw)),
            cut: j.cut,
            timed_out: j.timed_out,
            aborted: j.aborted,
            incomplete: j.incomplete,
        };
        st.finished = Some((j.id, result));
        self.0.cond.notify_all();
    }

    /// Waits for the shell to print anything (or the timeout).
    fn wait_output(&self, since: u64, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut st = self.lock();
        while st.output_seq == since {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            st = self.0.cond.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    pub fn status(&self) -> ShellStatus {
        let st = self.lock();
        ShellStatus { integrated: st.integrated, idle: st.idle, running: st.job.is_some(), cwd: st.cwd.clone() }
    }

    /// Waits up to timeout for the shell's first prompt.
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = self.lock();
        while !(st.integrated && st.idle) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = self.0.cond.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        true
    }

    /// Runs command in the shell and waits for it to finish.
    pub fn run(&self, command: &str, timeout: Duration) -> Result<RunResult, String> {
        let id;
        {
            let mut st = self.lock();
            if !st.integrated {
                return Err("shell integration is not active (the shell must be zsh)".into());
            }
            if st.job.is_some() {
                return Err("another command is still running in this shell; use another shell (shell_new) or wait".into());
            }
            let deadline = Instant::now() + IDLE_WAIT;
            while !st.idle {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(
                        "the shell is busy with a program that is still running (see shell_screen); answer it with shell_input or use another shell".into()
                    );
                }
                st = self.0.cond.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
            }
            st.next_job += 1;
            id = st.next_job;
            st.finished = None;
            st.job = Some(Job { id, ..Job::default() });
        }

        if let Err(e) = write_private(&self.0.cmd_file, command.as_bytes()) {
            self.lock().job = None;
            return Err(e.to_string());
        }

        // Clear whatever is on the prompt line, then the run widget puts the
        // exact command there and accepts it
        let s = self.lock().output_seq;
        self.send(CLEAR_KEY);
        self.wait_output(s, Duration::from_millis(200));
        {
            let mut st = self.lock();
            match st.job.as_mut() {
                Some(j) if j.id == id && !j.aborted => j.sent_enter = true,
                _ => {
                    st.job = None;
                    return Ok(RunResult { aborted: true, ..RunResult::default() });
                }
            }
        }
        self.send(RUN_KEY);

        let deadline = Instant::now() + timeout;
        let mut st = self.lock();
        loop {
            if st.finished.as_ref().is_some_and(|(fid, _)| *fid == id) {
                return Ok(st.finished.take().unwrap().1);
            }
            if st.job.as_ref().is_none_or(|j| j.id != id) {
                return Err("the command was lost".into());
            }
            let timed_out = st.job.as_ref().is_some_and(|j| j.timed_out);
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() && !timed_out {
                if let Some(j) = st.job.as_mut() {
                    j.timed_out = true;
                }
                drop(st);
                self.interrupt();
                st = self.lock();
                continue;
            }
            let wait = if timed_out { Duration::from_secs(1) } else { left };
            st = self.0.cond.wait_timeout(st, wait).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// Ctrl-Cs what runs in the shell: the AI command (resolving it anyway if
    /// the shell never reports back), or whatever else is in the foreground.
    pub fn interrupt(&self) {
        let id;
        {
            let mut st = self.lock();
            let Some(j) = st.job.as_mut() else {
                let busy = !st.idle && st.integrated;
                drop(st);
                if busy {
                    self.send(b"\x03");
                }
                return;
            };
            j.aborted = j.aborted || !j.timed_out;
            if !j.sent_enter {
                return;
            }
            id = j.id;
        }
        self.send(b"\x03");
        let b = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            let mut st = b.lock();
            if st.job.as_ref().is_some_and(|j| j.id == id) {
                b.finish(&mut st, None);
            }
        });
    }
}

fn on_output(st: &mut State, chunk: &[u8]) {
    let Some(j) = st.job.as_mut() else { return };
    if chunk.is_empty() || !j.sent_enter {
        return;
    }
    let buf = if j.started { &mut j.output } else { &mut j.pre_start };
    buf.extend_from_slice(chunk);
    if buf.len() > MAX_JOB_OUTPUT {
        buf.drain(..buf.len() - MAX_JOB_OUTPUT / 2);
        j.cut = true;
    }
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).rev().find(|&i| &hay[i..i + needle.len()] == needle)
}

/// Takes the terminal's escape sequences and control characters out of text,
/// keeping line breaks (for the shell logs).
pub fn strip_ansi(raw: &str) -> String {
    static RES: OnceLock<[Regex; 4]> = OnceLock::new();
    let [osc, csi, esc, control] = RES.get_or_init(|| {
        [
            Regex::new(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)").unwrap(),
            Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").unwrap(),
            Regex::new(r"\x1b[@-Z\\-_]").unwrap(),
            Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]").unwrap(),
        ]
    });
    let s = osc.replace_all(raw, "");
    let s = csi.replace_all(&s, "");
    let s = esc.replace_all(&s, "");
    control.replace_all(&s, "").into_owned()
}

/// Turns raw terminal output into what a reader of the screen would see, for
/// handing back to the AI.
pub fn clean_output(raw: &str) -> String {
    let s = strip_ansi(raw);
    // Keep what a terminal would show after carriage returns (progress bars etc.)
    let lines: Vec<&str> = s
        .split('\n')
        .map(|line| {
            let line = line.trim_end_matches('\r');
            line.rfind('\r').map_or(line, |k| &line[k + 1..])
        })
        .collect();
    lines.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge() -> (Bridge, Arc<Mutex<Vec<Vec<u8>>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let s = sent.clone();
        (Bridge::new(move |b| s.lock().unwrap().push(b.to_vec()), PathBuf::from("/dev/null")), sent)
    }

    #[test]
    fn markers_split_across_chunks() {
        let (b, _) = bridge();
        b.lock().job = Some(Job { id: 1, sent_enter: true, ..Job::default() });

        let mut shown = b.feed(b"\x1b]633;C\x07hel");
        shown.extend(b.feed(b"lo\r\n\x1b]63"));
        shown.extend(b.feed(b"3;D;0\x07\x1b]633;W;/tmp/x"));
        shown.extend(b.feed(b"\x07\x1b]633;A\x07"));

        assert_eq!(shown, b"hello\r\n");
        let st = b.lock();
        let (id, r) = st.finished.clone().expect("job did not finish");
        assert_eq!(id, 1);
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.output, "hello");
        assert_eq!(st.cwd, "/tmp/x");
        assert!(st.idle && st.integrated);
    }

    #[test]
    fn passes_other_sequences_untouched() {
        let (b, _) = bridge();
        let data = b"\x1b]0;title\x07\x1b[1;32mok\x1b[0m";
        assert_eq!(b.feed(data), data);
        assert_eq!(b.feed(b"abc\x1b"), b"abc");
        assert_eq!(b.feed(b"[0m"), b"\x1b[0m");
        let mut junk = b"\x1b]633;".to_vec();
        junk.extend(std::iter::repeat_n(b'x', MAX_CARRY + 1));
        assert_eq!(b.feed(&junk), junk);
    }

    #[test]
    fn continuation_prompt_cancels_the_command() {
        let (b, sent) = bridge();
        b.lock().job = Some(Job { id: 1, sent_enter: true, ..Job::default() });
        b.feed(b"\x1b]633;F\x07");
        assert_eq!(sent.lock().unwrap().last().unwrap(), b"\x03");
        b.feed(b"\x1b]633;D;130\x07");
        let r = b.lock().finished.clone().unwrap().1;
        assert!(r.incomplete);
        assert_eq!(r.exit_code, Some(130));
    }

    #[test]
    fn run_needs_the_integration() {
        let (b, _) = bridge();
        assert!(b.run("ls", Duration::from_secs(1)).unwrap_err().contains("integration"));
    }

    #[test]
    fn interrupt_reaches_a_busy_shell_without_a_job() {
        let (b, sent) = bridge();
        b.feed(b"\x1b]633;A\x07");
        b.interrupt();
        assert!(sent.lock().unwrap().is_empty(), "an idle shell gets no ^C");
        b.feed(b"\x1b]633;C\x07");
        b.interrupt();
        assert_eq!(sent.lock().unwrap().last().unwrap(), b"\x03");
    }

    #[test]
    fn run_hands_over_the_command_and_reads_the_result() {
        let dir = std::env::temp_dir().join(format!("va-bridge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cmd = dir.join("cmd");
        let slot: Arc<Mutex<Option<Bridge>>> = Arc::new(Mutex::new(None));
        let s = slot.clone();
        // A stand-in for zsh: the run key starts the command and ends it
        let b = Bridge::new(
            move |bytes| {
                let b = s.lock().unwrap().clone().unwrap();
                let bytes = bytes.to_vec();
                std::thread::spawn(move || {
                    if bytes == RUN_KEY {
                        b.feed(b"\x1b]633;C\x07out\r\n\x1b]633;D;3\x07\x1b]633;A\x07");
                    } else {
                        b.feed(b"\r");
                    }
                });
            },
            cmd.clone(),
        );
        *slot.lock().unwrap() = Some(b.clone());
        b.feed(b"\x1b]633;A\x07");
        assert!(b.wait_ready(Duration::from_secs(1)));
        let r = b.run("false", Duration::from_secs(5)).unwrap();
        assert_eq!((r.exit_code, r.output.as_str()), (Some(3), "out"));
        assert_eq!(std::fs::read_to_string(&cmd).unwrap(), "false");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean() {
        let raw = "\x1b[1;32mok\x1b[0m\r\n10%\r50%\r100%\r\n\x1b]0;title\x07done";
        assert_eq!(clean_output(raw), "ok\n100%\ndone");
        assert_eq!(strip_ansi("\x1b[31ma\x1b[0m\r\nb\x07"), "a\r\nb");
    }
}
