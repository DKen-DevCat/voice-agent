//! A long-lived headless `claude` speaking stream-json both ways. It reuses
//! the user's Claude Code login and restarts with --resume if the process dies.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::agents::AgentTracker;
use super::stream::StreamMsg;
use super::usage::UsageTracker;
use crate::log::log;

type OnReady = dyn Fn(&str, &str) + Send + Sync;
type OnToolUse = dyn Fn(&str, &Map<String, Value>) + Send + Sync;
type OnTurnEnd = dyn Fn(bool, &str) + Send + Sync;

/// Called from the reader thread.
pub struct Callbacks {
    pub on_ready: Box<OnReady>, // session id, model
    pub on_text: Box<dyn Fn(&str) + Send + Sync>,
    pub on_text_end: Box<dyn Fn() + Send + Sync>,
    pub on_tool_use: Box<OnToolUse>,
    pub on_turn_end: Box<OnTurnEnd>, // is_error, result
}

#[derive(Default)]
struct State {
    stdin: Option<ChildStdin>,
    pid: Option<u32>,
    generation: u64, // which launch the pid belongs to
    session_id: String,
    announced: String,
    sent: u64,       // messages sent
    done: u64,       // of them, answered
    streaming: bool, // a turn of the main conversation is under way
    ours: bool,      // ...and it answers a message sent (not one claude started itself)
    req_seq: u32,
    retired: u64, // launches up to this one were ended on purpose (another conversation)
}

impl State {
    fn turn(&self) -> bool {
        self.sent > self.done || self.streaming
    }
}

/// Whether the turn that was under way when the interrupt went out (the
/// target-th message) is still not over in the same claude process.
fn still_running(st: &State, generation: u64, target: u64) -> bool {
    st.generation == generation && st.done < target
}

pub struct ClaudeSession {
    pub bin: PathBuf,
    pub dir: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    pub args: Vec<String>,
    pub cb: Callbacks,
    /// Follows the subagents the conversation spawns
    pub agents: Option<Arc<AgentTracker>>,
    /// Adds up tokens, cost and rate limits
    pub usage: Option<Arc<UsageTracker>>,
    st: Mutex<State>,
}

impl ClaudeSession {
    pub fn new(bin: PathBuf, dir: PathBuf, env: Vec<(OsString, OsString)>, args: Vec<String>, cb: Callbacks) -> ClaudeSession {
        ClaudeSession { bin, dir, env, args, cb, agents: None, usage: None, st: Mutex::default() }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn busy(&self) -> bool {
        self.lock().turn()
    }

    fn spawn(self: &Arc<Self>, st: &mut State) -> Result<(), String> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages"]);
        cmd.args(&self.args);
        if !st.session_id.is_empty() {
            cmd.args(["--resume", &st.session_id]);
        }
        cmd.current_dir(&self.dir).env_clear().envs(self.env.iter().cloned());
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        st.stdin = child.stdin.take();
        st.pid = Some(child.id());
        st.generation += 1;
        let generation = st.generation;
        if let Some(u) = &self.usage {
            u.new_process();
        }

        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    log(&format!("claude stderr: {line}"));
                }
            });
        }
        let stdout = child.stdout.take();
        let s = self.clone();
        std::thread::spawn(move || {
            let mut blocks = HashMap::new();
            if let Some(stdout) = stdout {
                let mut reader = BufReader::with_capacity(1 << 20, stdout);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    match reader.read_until(b'\n', &mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let l = line.trim_ascii();
                    // Found in the review: what a launch left behind for
                    // another conversation still had in the pipe would land
                    // on the new one (its text, its session id): read past it
                    if !l.is_empty() && generation > s.lock().retired {
                        s.on_message(l, &mut blocks);
                    }
                }
            }
            let status = child.wait();
            let had_turn = {
                let mut st = s.lock();
                // Ended to move to another conversation: what it was doing is
                // left behind, not ended with an error
                if generation <= st.retired {
                    drop(st);
                    log(&format!("claude exited: {} (moved to another conversation)", status.map_or_else(|e| e.to_string(), |s| s.to_string())));
                    return;
                }
                if st.generation == generation {
                    st.stdin = None;
                    st.pid = None;
                }
                let had = st.turn();
                st.done = st.sent;
                st.streaming = false;
                had
            };
            let how = status.map_or_else(|e| e.to_string(), |s| s.to_string());
            log(&format!("claude exited: {how}"));
            if let Some(a) = &s.agents {
                // Subagents die with the process
                a.stop_all();
            }
            if had_turn {
                (s.cb.on_turn_end)(true, &format!("claude exited ({how})"));
            }
        });
        Ok(())
    }

    /// Takes in one line of claude's output; blocks remembers the kind of
    /// each content block being streamed.
    pub fn on_message(&self, line: &[u8], blocks: &mut HashMap<i64, String>) {
        let Ok(m) = serde_json::from_slice::<StreamMsg>(line) else {
            log(&format!("claude unparsed stdout: {}", String::from_utf8_lossy(&line[..line.len().min(200)])));
            return;
        };
        let content = m.blocks();
        if let Some(a) = &self.agents {
            a.observe(&m, line, &content);
        }
        if let Some(u) = &self.usage {
            u.observe(&m, line);
        }
        // Only the main conversation is spoken; what subagents do is tracked above
        if !m.parent_tool_use_id.is_empty() {
            return;
        }
        if m.typ == "stream_event" || m.typ == "assistant" {
            let mut st = self.lock();
            if !st.streaming {
                st.streaming = true;
                st.ours = st.sent > st.done;
            }
        }
        match m.typ.as_str() {
            "system" => {
                // stream-json repeats init on every turn; announce only a new session
                if m.subtype == "init" {
                    let is_new = {
                        let mut st = self.lock();
                        let is_new = m.session_id != st.announced;
                        st.announced = m.session_id.clone();
                        st.session_id = m.session_id.clone();
                        is_new
                    };
                    if is_new {
                        (self.cb.on_ready)(&m.session_id, &m.model);
                    }
                }
            }
            "stream_event" => {
                let Some(ev) = &m.event else { return };
                match ev.typ.as_str() {
                    "content_block_start" => {
                        if let Some(cb) = &ev.content_block {
                            blocks.insert(ev.index, cb.typ.clone());
                        }
                    }
                    "content_block_delta" => {
                        if let Some(d) = ev.delta.as_ref().filter(|d| d.typ == "text_delta") {
                            (self.cb.on_text)(&d.text);
                        }
                    }
                    "content_block_stop" if blocks.remove(&ev.index).as_deref() == Some("text") => (self.cb.on_text_end)(),
                    _ => {}
                }
            }
            "assistant" => {
                for b in content.iter().filter(|b| b.typ == "tool_use") {
                    (self.cb.on_tool_use)(&b.name, &b.input);
                }
            }
            "result" => {
                {
                    let mut st = self.lock();
                    if !m.session_id.is_empty() {
                        st.session_id = m.session_id.clone();
                    }
                    // A turn claude started itself answers none of our messages
                    if !st.streaming || st.ours {
                        st.done = (st.done + 1).min(st.sent);
                    }
                    st.streaming = false;
                }
                (self.cb.on_turn_end)(m.is_error, &m.result);
            }
            _ => {}
        }
    }

    fn write(self: &Arc<Self>, v: &Value) -> Result<(), String> {
        let mut st = self.lock();
        if st.stdin.is_none() {
            self.spawn(&mut st)?;
        }
        let mut data = v.to_string().into_bytes();
        data.push(b'\n');
        let stdin = st.stdin.as_mut().ok_or("claude is not running")?;
        stdin.write_all(&data).and_then(|_| stdin.flush()).map_err(|e| e.to_string())
    }

    /// Starts the CLI ahead of the first message; it loads config and MCP
    /// while idle.
    pub fn warm(self: &Arc<Self>) -> Result<(), String> {
        let mut st = self.lock();
        if st.stdin.is_none() { self.spawn(&mut st) } else { Ok(()) }
    }

    pub fn send(self: &Arc<Self>, text: &str) -> Result<(), String> {
        self.lock().sent += 1;
        let res = self.write(&json!({"type": "user", "message": {"role": "user", "content": text}}));
        if res.is_err() {
            let mut st = self.lock();
            st.sent -= 1;
            st.done = st.done.min(st.sent);
        }
        res
    }

    pub fn interrupt(self: &Arc<Self>) {
        let (id, turn, generation, target);
        {
            let mut st = self.lock();
            turn = st.turn();
            target = st.sent;
            // Subagents may be at work in the background after the turn ended
            let agents = self.agents.as_ref().is_some_and(|a| a.running() > 0);
            if st.pid.is_none() || (!turn && !agents) {
                return;
            }
            st.req_seq += 1;
            id = format!("va_int_{}", st.req_seq);
            generation = st.generation;
        }
        let _ = self.write(&json!({"type": "control_request", "request_id": id, "request": {"subtype": "interrupt"}}));
        if !turn {
            return;
        }
        // Fall back to restarting the process (the conversation resumes) if the
        // CLI ignores it. Only that turn counts: one the user starts right after
        // is left alone.
        let s = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(4));
            let st = s.lock();
            if still_running(&st, generation, target) {
                terminate(st.pid);
            }
        });
    }

    pub fn stop(&self) {
        terminate(self.lock().pid);
    }

    /// Claude's id for the conversation under way (empty before its first reply).
    pub fn session_id(&self) -> String {
        self.lock().session_id.clone()
    }

    /// Moves to another conversation (serve): this process ends, and the next
    /// message starts claude on session (with --resume; a new one when it is
    /// empty). A turn under way is left behind, and its subagents with it.
    pub fn switch_to(&self, session: &str) {
        let pid = {
            let mut st = self.lock();
            st.session_id = session.to_string();
            // the resumed session is announced again
            st.announced.clear();
            st.retired = st.generation;
            st.stdin = None;
            st.done = st.sent;
            st.streaming = false;
            st.pid.take()
        };
        if let Some(a) = &self.agents {
            a.stop_all();
        }
        terminate(pid);
    }
}

fn terminate(pid: Option<u32>) {
    if let Some(pid) = pid {
        // SAFETY: signals a child process of ours by its pid
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> (Arc<ClaudeSession>, Arc<Mutex<Vec<String>>>) {
        session_at("claude".into())
    }

    fn session_at(bin: PathBuf) -> (Arc<ClaudeSession>, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let push = |seen: &Arc<Mutex<Vec<String>>>| {
            let s = seen.clone();
            move |line: String| s.lock().unwrap().push(line)
        };
        let (ready, text, text_end, tool, turn) = (push(&seen), push(&seen), push(&seen), push(&seen), push(&seen));
        let cb = Callbacks {
            on_ready: Box::new(move |id, model| ready(format!("ready {id} {model}"))),
            on_text: Box::new(move |t| text(format!("text {t}"))),
            on_text_end: Box::new(move || text_end("text_end".into())),
            on_tool_use: Box::new(move |name, input| tool(format!("tool {name} {}", Value::Object(input.clone())))),
            on_turn_end: Box::new(move |err, result| turn(format!("turn_end {err} {result}"))),
        };
        (Arc::new(ClaudeSession::new(bin, ".".into(), Vec::new(), Vec::new(), cb)), seen)
    }

    /// A stand-in for claude: it writes its arguments to args.log, takes the
    /// session from --resume ("new" without it), and answers each message
    /// after pause seconds.
    fn fake_bin(dir: &std::path::Path, pause: &str) -> PathBuf {
        let path = dir.join("claude");
        let script = format!(
            r#"#!/bin/sh
echo "$@" >> '{log}'
sid=new; prev=""
for a in "$@"; do [ "$prev" = "--resume" ] && sid="$a"; prev="$a"; done
while IFS= read -r line; do
  printf '%s\n' '{{"type":"system","subtype":"init","session_id":"'"$sid"'","model":"fake"}}'
  sleep {pause}
  printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"result":"ok","session_id":"'"$sid"'"}}'
done
"#,
            log = dir.join("args.log").display()
        );
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !f() {
            assert!(std::time::Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn what_a_launch_left_behind_still_says_is_not_heard() {
        // found in the review: the old process's lines still in the pipe set
        // the new conversation's session id and spoke in it
        let dir = std::env::temp_dir().join(format!("va-claude-retired-{}-{}", std::process::id(), crate::util::unix_millis()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("claude");
        let script = r#"#!/bin/sh
trap '' TERM
while IFS= read -r line; do
  printf '%s\n' '{"type":"system","subtype":"init","session_id":"old","model":"fake"}'
  sleep 0.5
  printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"left behind"}}}'
  printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"x","session_id":"old"}'
done
"#;
        std::fs::write(&bin, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (s, seen) = session_at(bin);
        s.send("one").unwrap();
        wait_until("the old session is ready", || s.session_id() == "old");
        s.switch_to("s9");
        // it ignores the signal and talks on for a while
        std::thread::sleep(Duration::from_millis(900));
        assert_eq!(s.session_id(), "s9");
        let seen = seen.lock().unwrap();
        assert!(!seen.iter().any(|l| l.contains("left behind")), "{seen:?}");
        assert!(!seen.iter().any(|l| l.starts_with("turn_end")), "{seen:?}");
    }

    #[test]
    fn switching_resumes_the_other_session_and_leaves_the_turn_behind() {
        let dir = std::env::temp_dir().join(format!("va-claude-switch-{}-{}", std::process::id(), crate::util::unix_millis()));
        std::fs::create_dir_all(&dir).unwrap();
        let (s, seen) = session_at(fake_bin(&dir, "1"));
        s.send("one").unwrap();
        wait_until("the first session is ready", || s.session_id() == "new");
        assert!(s.busy());
        // another conversation, while the turn is under way
        s.switch_to("s9");
        assert!(!s.busy());
        assert_eq!(s.session_id(), "s9");
        s.send("two").unwrap();
        wait_until("the reply in the other session", || seen.lock().unwrap().iter().any(|l| l == "turn_end false ok"));
        let args = std::fs::read_to_string(dir.join("args.log")).unwrap();
        let launches: Vec<&str> = args.lines().collect();
        assert_eq!(launches.len(), 2);
        assert!(!launches[0].contains("--resume"));
        assert!(launches[1].ends_with("--resume s9"), "{}", launches[1]);
        // the turn left behind did not end as an error
        std::thread::sleep(Duration::from_millis(300));
        assert!(!seen.lock().unwrap().iter().any(|l| l.starts_with("turn_end true")), "{:?}", seen.lock().unwrap());
        // each session is announced
        assert!(seen.lock().unwrap().iter().any(|l| l == "ready new fake"));
        assert!(seen.lock().unwrap().iter().any(|l| l == "ready s9 fake"));
        s.stop();
    }

    #[test]
    fn reads_the_stream() {
        let (s, seen) = session();
        let mut blocks = HashMap::new();
        s.lock().sent = 1;
        for line in [
            r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-sonnet-5-5"}"#,
            r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-sonnet-5-5"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"こんにちは"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":1}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"T1","name":"Read","input":{"file_path":"/x"}}]}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":"A1","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"subagent talk"}}}"#,
            "not json",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"s2"}"#,
        ] {
            s.on_message(line.as_bytes(), &mut blocks);
        }
        assert_eq!(
            *seen.lock().unwrap(),
            ["ready s1 claude-sonnet-5-5", "text こんにちは", "text_end", r#"tool Read {"file_path":"/x"}"#, "turn_end false done",]
        );
        assert!(!s.busy());
        assert_eq!(s.lock().session_id, "s2");
    }

    /// A turn claude starts itself (relaying a background subagent's result)
    /// counts as busy, and its end does not use up a message sent meanwhile.
    #[test]
    fn turns_claude_starts_itself() {
        let (s, _) = session();
        let mut blocks = HashMap::new();
        let line = |l: &str| l.as_bytes().to_vec();
        s.on_message(&line(r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text"}}}"#), &mut blocks);
        assert!(s.busy(), "a relay turn is a turn");
        s.lock().sent = 1; // the user speaks meanwhile
        s.on_message(&line(r#"{"type":"result","subtype":"success","result":"relayed"}"#), &mut blocks);
        assert!(s.busy(), "the user's message is still to be answered");
        s.on_message(&line(r#"{"type":"result","subtype":"success","result":"answered"}"#), &mut blocks);
        assert!(!s.busy());
        let st = s.lock();
        assert_eq!((st.sent, st.done), (1, 1));
    }

    /// The fallback after an interrupt ends the process only while the
    /// interrupted turn is still there, not a turn the user starts after it.
    #[test]
    fn the_interrupt_fallback_spares_the_next_turn() {
        let st = State { generation: 1, sent: 2, done: 1, ..State::default() };
        assert!(still_running(&st, 1, 2), "the interrupted turn never ended");
        let st = State { generation: 1, sent: 3, done: 2, ..State::default() };
        assert!(!still_running(&st, 1, 2), "it ended; the third message is a new turn");
        assert!(!still_running(&st, 2, 3), "another process");
    }

    #[test]
    fn talks_to_the_process_and_notices_its_end() {
        // A stand-in for claude: answers each line it is sent, then exits
        let dir = std::env::temp_dir().join(format!("va-claude-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("claude");
        std::fs::write(&script, "#!/bin/sh\necho \"$@\" > args\nread line\necho \"$line\" > got\necho '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s9\",\"model\":\"m\"}'\nread line2\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (proto, seen) = session();
        let mut s = Arc::try_unwrap(proto).unwrap_or_else(|_| unreachable!());
        s.bin = script;
        s.dir = dir.clone();
        s.env = std::env::vars_os().collect();
        s.args = vec!["--model".into(), "m".into()];
        let s = Arc::new(s);
        s.send("hello").unwrap();
        assert!(s.busy());
        let wait_for = |what: &str| {
            for _ in 0..200 {
                if seen.lock().unwrap().iter().any(|l| l.starts_with(what)) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            panic!("no {what} in {:?}", seen.lock().unwrap());
        };
        wait_for("ready s9");
        assert!(std::fs::read_to_string(dir.join("got")).unwrap().contains(r#""content":"hello""#));
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert!(args.starts_with("-p --input-format stream-json --output-format stream-json") && args.trim_end().ends_with("--model m"), "{args}");
        // It dies mid-turn: the turn ends with an error, and the next launch resumes
        s.stop();
        wait_for("turn_end true claude exited");
        assert!(!s.busy());
        s.send("again").unwrap();
        for _ in 0..100 {
            if std::fs::read_to_string(dir.join("args")).unwrap_or_default().contains("--resume s9") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(std::fs::read_to_string(dir.join("args")).unwrap().contains("--resume s9"));
        s.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
