//! `voice-agent serve`: the agent without the terminal, for the iPhone app
//! (lfm3-vl-server docs/native-app.md §14). An HTTP API on 127.0.0.1:8082
//! behind lfm3's wrapper; every request is signed with a key of the paired
//! iPhone, and only its approval key (Face ID or passcode) can allow.

pub mod auth;
pub mod control;
pub mod crypto;
pub mod devices;
pub mod events;
pub mod http;
pub mod sessions;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};

use crate::agent::policy::resolve;
use crate::app::{Controller, Event, EventKind, Front, Hooks};
use crate::config::Config;
use crate::log::log;
use auth::{Role, Verifier};
use crypto::{hex, sha256};
use devices::Devices;
use events::{Hub, parse_event_id};
use http::{Reply, error};
use sessions::Sessions;

pub const PORT: u16 = 8082;

/// What the counts and the session map are looked at.
const POLL: Duration = Duration::from_secs(1);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The digest an allow has to name: what the iPhone showed for the approval.
pub fn digest(summary: &str, reason: &str) -> String {
    hex(&sha256(format!("{summary}\n{reason}").as_bytes()))
}

fn kind_name(k: EventKind) -> &'static str {
    match k {
        EventKind::System => "system",
        EventKind::User => "user",
        EventKind::Assistant => "assistant",
        EventKind::AssistantEnd => "assistant_end",
        EventKind::Tool => "tool",
        EventKind::Approval => "approval",
        EventKind::ApprovalResolved => "approval_resolved",
        EventKind::Notice => "notice",
        EventKind::Error => "error",
        EventKind::Status => "status",
        EventKind::Agent => "agent",
        EventKind::Usage => "usage",
        EventKind::ApprovalConfirm => "approval_confirm",
    }
}

pub struct Serve {
    pub hub: Hub,
    pub devices: Arc<Devices>,
    pub sessions: Sessions,
    pub verifier: Verifier,
    /// What the wrapper sends as `Authorization: Bearer` (VA_SERVE_KEY)
    pub key: String,
    ctl: OnceLock<Arc<Controller>>,
    /// The conversation claude is on; events are marked with it
    conv: Mutex<Option<String>>,
    /// Held while moving to another conversation and while the session map
    /// is written, so that one conversation's session never lands on another
    moving: Mutex<()>,
    /// The approvals shown: the number of their event, and their digest
    shown: Mutex<HashMap<String, (u64, String)>>,
    counts: Mutex<(usize, usize)>,
    usage: Mutex<Value>,
}

impl Serve {
    pub fn new(run: &str, devices: Arc<Devices>, sessions: Sessions, key: &str) -> Serve {
        Serve {
            hub: Hub::new(run),
            devices,
            sessions,
            verifier: Verifier::default(),
            key: key.to_string(),
            ctl: OnceLock::new(),
            conv: Mutex::new(None),
            moving: Mutex::new(()),
            shown: Mutex::new(HashMap::new()),
            counts: Mutex::new((0, 0)),
            usage: Mutex::new(Value::Null),
        }
    }

    /// The controller, once it is there.
    pub fn attach(&self, ctl: Arc<Controller>) {
        let _ = self.ctl.set(ctl);
    }

    fn ctl(&self) -> &Arc<Controller> {
        self.ctl.get().expect("serve has its controller before it takes requests")
    }

    /// The controller's events, numbered and marked with the conversation.
    pub fn on_events(&self, evs: Vec<Event>) {
        for e in evs {
            let mut v = serde_json::to_value(&e).unwrap_or_else(|_| json!({}));
            v["kind"] = json!(kind_name(e.kind));
            // The usage is the Mac's, not a conversation's
            if e.kind == EventKind::Usage {
                *lock(&self.usage) = v["usage"].clone();
            } else {
                v["conv"] = json!(*lock(&self.conv));
            }
            if e.kind == EventKind::Approval {
                let d = digest(&e.text, &e.reason);
                v["digest"] = json!(d);
                let seq = self.hub.publish(v);
                lock(&self.shown).insert(e.id.clone(), (seq, d));
                continue;
            }
            if e.kind == EventKind::ApprovalResolved {
                lock(&self.shown).remove(&e.id);
            }
            self.hub.publish(v);
        }
    }

    /// A message to conversation conv: claude moves to it first when it is
    /// on another. last_event: the last event the iPhone had when the user
    /// began it, which tells whether they had seen an approval (§14.9).
    /// raw: the text is never a command (native-app.md §18.3.1 S6).
    pub fn message(&self, conv: &str, text: &str, via: &str, raw: bool, last_event: Option<&str>) -> Result<u64, Reply> {
        if !self.sessions.exists(conv) {
            return Err(error(404, "unknown_conversation"));
        }
        let _moving = lock(&self.moving);
        let current = lock(&self.conv).clone();
        if current.as_deref() != Some(conv) {
            // Found in the review: the map is written once a POLL, so a
            // session learned just before moving would be lost
            self.keep_session();
            let session = self.sessions.session(conv).unwrap_or_default();
            // The claude warmed at the start is on no conversation yet: a new
            // one can have it
            if current.is_some() || !session.is_empty() {
                self.ctl().switch_session(&session);
            }
            *lock(&self.conv) = Some(conv.to_string());
            // The warmed claude took the conversation: the id it was started
            // with was learned before there was one to put it on
            self.keep_session();
        }
        let seen = last_event.and_then(parse_event_id).filter(|(run, _)| *run == self.hub.run()).map(|(_, seq)| seq);
        let shown = lock(&self.shown).clone();
        // From another run, the iPhone has seen none of this run's approvals
        let saw = move |id: &str| seen.zip(shown.get(id)).is_some_and(|(last, (seq, _))| last >= *seq);
        Ok(self.ctl().handle_message(text, via, raw, &saw))
    }

    /// The iPhone's answer by button. An allow takes the approval key, and
    /// both have to name what was shown.
    pub fn answer(&self, id: &str, allow: bool, digest: &str, key: &str, role: Role) -> Result<(), Reply> {
        if allow && role != Role::Approval {
            return Err(error(401, "wrong_key"));
        }
        if lock(&self.shown).get(id).is_none_or(|(_, d)| d != digest) {
            return Err(error(409, "mismatch"));
        }
        self.ctl().answer_by_id(id, allow, "button", key).map_err(|_| error(409, "not_oldest"))
    }

    /// ■ on the iPhone: as Esc (the jobs stop too), which waits for them.
    pub fn stop_all(&self) {
        let ctl = self.ctl().clone();
        std::thread::spawn(move || ctl.abort());
    }

    /// Where things stand, for a client that (re)connects.
    pub fn state(&self) -> Value {
        let ctl = self.ctl();
        let (agents, jobs) = ctl.running();
        let approval = ctl.pending_approval().map(|p| json!({"id": p.id, "summary": p.summary, "reason": p.reason, "digest": digest(&p.summary, &p.reason)}));
        json!({
            "run": self.hub.run(),
            "seq": self.hub.last(),
            "conversation": *lock(&self.conv),
            "status": ctl.status_now(),
            "approval": approval,
            "agents": agents,
            "jobs": jobs,
            "usage": *lock(&self.usage),
        })
    }

    /// Once a POLL: the counts of subagents and jobs (sent when they
    /// change), and the conversation's session as claude knows it.
    fn poll(&self) {
        let counts = self.ctl().running();
        let changed = {
            let mut last = lock(&self.counts);
            let changed = *last != counts;
            *last = counts;
            changed
        };
        if changed {
            self.hub.publish(json!({"kind": "jobs", "agents": counts.0, "jobs": counts.1}));
        }
        let _moving = lock(&self.moving);
        self.keep_session();
    }

    /// Claude says which session it is on (at once, from the controller):
    /// written down for the conversation it is on, so that an id replaced
    /// within a poll, or seen just before a crash, is kept (§18.10 item 10).
    /// Takes no `moving` lock: it can come while message() holds it.
    pub fn learned(&self, session: &str) {
        if let Some(conv) = lock(&self.conv).clone() {
            self.sessions.set_session(&conv, session);
        }
    }

    /// Writes down the session claude is on for the conversation it is on
    /// (with `moving` held).
    fn keep_session(&self) {
        let session = self.ctl().session_id();
        if let Some(conv) = lock(&self.conv).clone()
            && !session.is_empty()
        {
            self.sessions.set_session(&conv, &session);
        }
    }
}

/// serve's working directory may be neither inside what it guards nor
/// around it (Claude edits inside it without asking: acceptEdits).
pub fn check_workdir(dir: &Path, guarded: &[PathBuf]) -> Result<(), String> {
    let d = resolve(dir);
    for p in guarded {
        let g = resolve(p);
        if crate::agent::policy::is_inside(&d, &g) || crate::agent::policy::is_inside(&g, &d) {
            return Err(format!("作業フォルダ {} は、守る場所 {} の中か、それを含む所です（native-app.md §14.3）", dir.display(), p.display()));
        }
    }
    Ok(())
}

/// The longest path a Unix socket takes on macOS (sun_path holds 104 bytes
/// with the closing NUL).
const SUN_PATH_MAX: usize = 103;

/// Found in the first run of the binary: a long VA_RUNTIME_DIR made the
/// control socket fail after everything else had started.
fn socket_path_fits(p: &Path) -> bool {
    p.as_os_str().len() <= SUN_PATH_MAX
}

/// A random run number: a restart starts the event numbers, the nonces and
/// the approval numbers over, and the iPhone sees that.
fn new_run() -> String {
    sessions::new_id()[..8].to_string()
}

pub fn run(args: &[String]) -> i32 {
    let home = crate::util::home_dir();
    let mut cfg = Config::load(&home);
    let mut dir: Option<PathBuf> = None;
    let mut port = PORT;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match (a.as_str(), it.next()) {
            ("--dir", Some(d)) => dir = Some(crate::util::expand_home(d, &home)),
            ("--port", Some(p)) if p.parse::<u16>().is_ok() => port = p.parse().unwrap_or(PORT),
            _ => {
                eprintln!("usage: voice-agent serve --dir <working directory> [--port <port>]");
                return 2;
            }
        }
    }
    let Some(dir) = dir else {
        eprintln!(
            "voice-agent: serve needs --dir (a working directory outside {})",
            cfg.protect.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        );
        return 2;
    };
    // A relative --dir that is not there yet would be checked as nothing
    let dir = if dir.is_absolute() { dir } else { std::env::current_dir().unwrap_or_default().join(dir) };
    let key = std::env::var("VA_SERVE_KEY").unwrap_or_default();
    if key.len() < 16 {
        eprintln!("voice-agent: serve needs VA_SERVE_KEY (lfm3-vl-server's start.sh makes one)");
        return 2;
    }
    let rt = crate::log::runtime_dir();
    let sock = rt.join("control.sock");
    if !socket_path_fits(&sock) {
        eprintln!("voice-agent: {} は長すぎます（Unix ソケットのパスは {} バイトまで）。VA_RUNTIME_DIR を短くしてください", sock.display(), SUN_PATH_MAX);
        return 2;
    }
    let conf = cfg.file.parent().map(Path::to_path_buf).unwrap_or_else(|| home.join(".config/voice-agent"));
    let mut guarded = cfg.protect.clone();
    guarded.extend([conf.clone(), rt.clone()]);
    if let Err(e) = check_workdir(&dir, &guarded) {
        eprintln!("voice-agent: {e}");
        return 2;
    }
    // The process works there too: a relative dir given to a job or a shell
    // starts from it, as the guard reads it (start.sh runs this from inside
    // lfm3-vl-server, or WKEN-AGENTS after the rename)
    if let Err(e) = std::fs::create_dir_all(&dir).and(crate::util::private_dir(&rt)).and_then(|_| std::env::set_current_dir(&dir)) {
        eprintln!("voice-agent: {e}");
        return 1;
    }
    let _lock = match crate::lock(&rt) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("voice-agent: {e}");
            return 1;
        }
    };
    let _ = crate::log::init(&rt.join("voice-agent.log"));
    cfg.dir = dir;
    let run = new_run();
    log(&format!("voice-agent {} serve on 127.0.0.1:{port} in {} (run {run})", env!("CARGO_PKG_VERSION"), cfg.dir.display()));

    let devices = Arc::new(Devices::load(&conf.join("devices.json")));
    let serve = Arc::new(Serve::new(&run, devices.clone(), Sessions::load(&conf.join("sessions.json")), &key));
    let s = serve.clone();
    let ctl = Controller::new_with(cfg, Hooks::from_env(), rt.clone(), Box::new(move |evs| s.on_events(evs)), Front::Serve);
    serve.attach(ctl.clone());
    let s = serve.clone();
    ctl.set_on_session(Box::new(move |id| s.learned(id)));
    let server = match tiny_http::Server::http(("127.0.0.1", port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("voice-agent: 127.0.0.1:{port}: {e}");
            return 1;
        }
    };
    if let Err(e) = ctl.start() {
        eprintln!("voice-agent: {e}");
        return 1;
    }
    let s = serve.clone();
    if let Err(e) = control::listen(&sock, devices, Arc::new(move || s.hub.close_client())) {
        eprintln!("voice-agent: {}: {e}", sock.display());
        ctl.close();
        return 1;
    }
    let s = serve.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(POLL);
            s.poll();
        }
    });
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    if let Ok(mut signals) = signal_hook::iterator::Signals::new([SIGHUP, SIGTERM, SIGINT]) {
        let c = ctl.clone();
        std::thread::spawn(move || {
            if let Some(sig) = signals.forever().next() {
                log(&format!("serve: signal {sig}"));
                c.close();
                let _ = std::fs::remove_file(&sock);
                std::process::exit(0);
            }
        });
    }
    http::serve_forever(serve, server);
    0
}

/// `voice-agent pair <code>`: the code the iPhone shows, to the running serve.
pub fn pair(code: &str) -> i32 {
    match control::send(&crate::log::runtime_dir().join("control.sock"), &format!("pair {}", code.trim())) {
        Ok(reply) => {
            let (msg, code) = control::pair_message(reply.trim());
            println!("{msg}");
            code
        }
        Err(_) => {
            eprintln!("voice-agent serve が動いていません（先に lfm3-vl-server の start.sh で起動してください）");
            1
        }
    }
}

/// `voice-agent unpair`: forgets the paired iPhone (when it is lost). Through
/// the running serve, or in the file when none runs.
pub fn unpair() -> i32 {
    let rt = crate::log::runtime_dir();
    let reply = control::send(&rt.join("control.sock"), "unpair").map(|r| r.trim().to_string());
    let done = match reply {
        Ok(r) => r == "ok",
        Err(_) => {
            // No serve to tell: the file is changed, with the lock held so
            // that none starts meanwhile. Found in the review: a serve the
            // socket does not reach would go on trusting the keys it holds
            let _ = crate::util::private_dir(&rt);
            let Ok(_lock) = crate::lock(&rt) else {
                eprintln!(
                    "voice-agent（serve かターミナル）が動いていますが、control.sock に届きません。止めてから（serve は lfm3-vl-server の stop.sh）もう一度打ってください"
                );
                return 1;
            };
            let cfg = Config::load(&crate::util::home_dir());
            let conf = cfg.file.parent().map(Path::to_path_buf).unwrap_or_default();
            Devices::load(&conf.join("devices.json")).unpair("mac").is_ok()
        }
    };
    if done {
        println!("対を解きました。iPhone の「Mac と対にする」から、もう一度対にできます。");
        0
    } else {
        eprintln!("対を解けませんでした（voice-agent.log を見てください）");
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::auth::testing::sign;
    use crate::serve::crypto::testing::TestKey;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpStream;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    struct Harness {
        serve: Arc<Serve>,
        ctl: Arc<Controller>,
        addr: String,
        home: PathBuf,
        device: TestKey,
        approval: TestKey,
    }

    /// A stand-in for claude: a new session is s<pid>, a resumed one keeps
    /// its id; every message is answered 了解. Its arguments go to args.log.
    fn fake_claude(dir: &Path) -> PathBuf {
        let path = dir.join("claude");
        let script = format!(
            r#"#!/bin/sh
echo "$@" >> '{log}'
sid="s$$"; prev=""
for a in "$@"; do [ "$prev" = "--resume" ] && sid="$a"; prev="$a"; done
while IFS= read -r line; do
  printf '%s\n' '{{"type":"system","subtype":"init","session_id":"'"$sid"'","model":"fake"}}'
  printf '%s\n' '{{"type":"stream_event","event":{{"type":"content_block_start","index":0,"content_block":{{"type":"text","text":""}}}}}}'
  printf '%s\n' '{{"type":"stream_event","event":{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"了解"}}}}}}'
  printf '%s\n' '{{"type":"stream_event","event":{{"type":"content_block_stop","index":0}}}}'
  printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"result":"了解","session_id":"'"$sid"'"}}'
done
"#,
            log = dir.join("args.log").display()
        );
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn harness() -> Harness {
        let home = std::env::temp_dir().join(format!("va-serve-it-{}", &sessions::new_id()[..8]));
        std::fs::create_dir_all(home.join("work")).unwrap();
        std::fs::create_dir_all(home.join("conf")).unwrap();
        let mut cfg = Config::load(Path::new("/nonexistent-home"));
        cfg.tts = "say".into();
        cfg.dir = home.join("work");
        cfg.file = home.join("conf/config.json");
        cfg.protect = vec![home.join("trust")];
        let devices = Arc::new(Devices::load(&home.join("conf/devices.json")));
        let serve = Arc::new(Serve::new("runA", devices, Sessions::load(&home.join("conf/sessions.json")), KEY));
        let s = serve.clone();
        let ctl = Controller::new_with(cfg, Hooks::default(), home.join("rt"), Box::new(move |evs| s.on_events(evs)), Front::Serve);
        serve.attach(ctl.clone());
        let s = serve.clone();
        ctl.set_on_session(Box::new(move |id| s.learned(id)));
        ctl.test_open_claude(fake_claude(&home));
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap().to_string();
        let s = serve.clone();
        std::thread::spawn(move || http::serve_forever(s, server));
        Harness { serve, ctl, addr, home, device: TestKey::new(), approval: TestKey::new() }
    }

    /// A request as the wrapper forwards it, signed with key when given.
    fn call(h: &Harness, key: Option<&TestKey>, method: &str, path: &str, body: &str, run: &str) -> (u16, Value) {
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).proxy(None).build().into();
        let url = format!("http://{}{path}", h.addr);
        let mut headers = vec![("Authorization".to_string(), format!("Bearer {KEY}"))];
        if let Some(k) = key {
            let s = sign(k, method, path, run, body.as_bytes(), crate::util::unix_millis());
            headers.extend([
                ("X-VA-Key".into(), s.key),
                ("X-VA-Time".into(), s.time),
                ("X-VA-Nonce".into(), s.nonce),
                ("X-VA-Run".into(), s.run),
                ("X-VA-Signature".into(), s.signature),
            ]);
        }
        let resp = if method == "GET" {
            let mut r = agent.get(&url);
            for (k, v) in &headers {
                r = r.header(k, v);
            }
            r.call()
        } else {
            let mut r = agent.post(&url);
            for (k, v) in &headers {
                r = r.header(k, v);
            }
            r.send(body.as_bytes())
        };
        let mut resp = resp.unwrap();
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// The event stream, opened as the iPhone does (headers read past).
    fn open_events(h: &Harness, last: &str) -> BufReader<TcpStream> {
        let mut s = TcpStream::connect(&h.addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let sig = sign(&h.device, "GET", "/agent/events", "", b"", crate::util::unix_millis());
        write!(
            s,
            "GET /agent/events HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {KEY}\r\nX-VA-Key: {}\r\nX-VA-Time: {}\r\nX-VA-Nonce: {}\r\nX-VA-Signature: {}\r\nLast-Event-ID: {last}\r\n\r\n",
            sig.key, sig.time, sig.nonce, sig.signature
        )
        .unwrap();
        let mut r = BufReader::new(s);
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        assert!(line.starts_with("HTTP/1.1 200"), "{line}");
        while line.trim() != "" || line.is_empty() {
            line.clear();
            r.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
        }
        r
    }

    /// The next event of kind (those before it are passed over).
    fn next(r: &mut BufReader<TcpStream>, kind: &str) -> Value {
        let mut line = String::new();
        loop {
            line.clear();
            assert!(r.read_line(&mut line).unwrap() > 0, "the stream ended before a {kind} event");
            if let Some(data) = line.strip_prefix("data: ") {
                let v: Value = serde_json::from_str(data.trim()).unwrap();
                if v["kind"] == kind {
                    return v;
                }
            }
        }
    }

    fn pair(h: &Harness) {
        let b = crate::serve::devices::testing::body(&h.device, &h.approval, "123456");
        let body = json!({
            "code": b.code, "name": b.name, "deviceKey": b.device_key, "approvalKey": b.approval_key,
            "deviceSignature": b.device_signature, "approvalSignature": b.approval_signature,
        })
        .to_string();
        assert_eq!(call(h, None, "POST", "/agent/pair", &body, "").0, 202);
        // what `voice-agent pair 123456` sends
        assert!(control::reply("pair 123456", &h.serve.devices, &|| {}).starts_with("ok "));
    }

    #[test]
    fn the_iphone_pairs_talks_and_answers_an_approval() {
        let h = harness();
        // the wrapper's key, and then a signature, are needed
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        assert_eq!(agent.get(format!("http://{}/agent/state", h.addr)).call().unwrap().status().as_u16(), 401);
        assert_eq!(call(&h, Some(&h.device), "GET", "/agent/state", "", ""), (401, json!({"error": {"code": "not_paired"}})));
        pair(&h);
        let (status, devices) = call(&h, Some(&h.device), "GET", "/agent/devices", "", "");
        assert_eq!(status, 200);
        assert_eq!(devices["device"]["keys"][1]["id"], h.approval.id());

        let (_, state) = call(&h, Some(&h.device), "GET", "/agent/state", "", "");
        assert_eq!((state["run"].as_str(), state["conversation"].is_null()), (Some("runA"), true));
        let (_, created) = call(&h, Some(&h.device), "POST", "/agent/sessions", "{}", "runA");
        let conv = created["id"].as_str().unwrap().to_string();
        let mut events = open_events(&h, &format!("runA:{}", state["seq"]));

        let body = json!({"text": "こんにちは", "via": "text", "last_event": format!("runA:{}", h.serve.hub.last())}).to_string();
        assert_eq!(call(&h, Some(&h.device), "POST", &format!("/agent/sessions/{conv}/messages"), &body, "runA").0, 200);
        let user = next(&mut events, "user");
        assert_eq!((user["text"].as_str(), user["conv"].as_str()), (Some("こんにちは"), Some(conv.as_str())));
        assert_eq!(next(&mut events, "assistant")["text"], "了解");
        next(&mut events, "assistant_end");

        // claude asks: the card carries what the allow has to name
        let c = h.ctl.clone();
        let asked = std::thread::spawn(move || {
            c.test_approve(&json!({"tool_name": crate::agent::policy::SHELL_RUN, "input": {"command": "git push origin main"}}).as_object().cloned().unwrap())
        });
        let approval = next(&mut events, "approval");
        let (id, d) = (approval["id"].as_str().unwrap().to_string(), approval["digest"].as_str().unwrap().to_string());
        assert_eq!(d, digest("git push origin main", "リモートへ push します"));
        let path = format!("/agent/approvals/{id}");
        let allow = json!({"allow": true, "digest": d}).to_string();
        // the device key cannot allow
        assert_eq!(call(&h, Some(&h.device), "POST", &path, &allow, "runA").1["error"]["code"], "wrong_key");
        // the allow names what was shown
        assert_eq!(call(&h, Some(&h.approval), "POST", &path, &json!({"allow": true, "digest": "x"}).to_string(), "runA").1["error"]["code"], "mismatch");
        // an allow signed for another run is refused
        assert_eq!(call(&h, Some(&h.approval), "POST", &path, &allow, "runZ").1["error"]["code"], "stale_run");
        assert!(!asked.is_finished());
        assert_eq!(call(&h, Some(&h.approval), "POST", &path, &allow, "runA").0, 204);
        assert_eq!(asked.join().unwrap()["behavior"], "allow");
        let resolved = next(&mut events, "approval_resolved");
        assert_eq!((resolved["by"].as_str(), resolved["key"].as_str()), (Some("button"), Some(h.approval.id().as_str())));

        assert_eq!(call(&h, Some(&h.device), "POST", "/agent/stop", "{\"scope\":\"all\"}", "runA").0, 204);
        assert_eq!(call(&h, Some(&h.device), "POST", "/agent/stop", "{\"scope\":\"speech\"}", "runA").0, 400);
        h.ctl.close();
    }

    #[test]
    fn moving_between_conversations_moves_the_session() {
        let h = harness();
        pair(&h);
        let new_conv = || call(&h, Some(&h.device), "POST", "/agent/sessions", "{}", "runA").1["id"].as_str().unwrap().to_string();
        let (a, b) = (new_conv(), new_conv());
        let say = |conv: &str| {
            let body = json!({"text": "やあ", "via": "text"}).to_string();
            assert_eq!(call(&h, Some(&h.device), "POST", &format!("/agent/sessions/{conv}/messages"), &body, "runA").0, 200);
        };
        let wait_session = |conv: &str| {
            for _ in 0..500 {
                h.serve.poll();
                if let Some(s) = h.serve.sessions.session(conv) {
                    return s;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("no session for {conv}");
        };
        say(&a);
        let sa = wait_session(&a);
        say(&b);
        let sb = wait_session(&b);
        assert_ne!(sa, sb);
        // back to the first: claude is started on its session
        say(&a);
        for _ in 0..500 {
            let log = std::fs::read_to_string(h.home.join("args.log")).unwrap_or_default();
            if log.lines().count() >= 3 {
                let launches: Vec<&str> = log.lines().collect();
                assert!(!launches[0].contains("--resume"), "the warmed claude takes the first conversation");
                assert!(!launches[1].contains("--resume"), "a new conversation starts a new session");
                assert!(launches[2].ends_with(&format!("--resume {sa}")), "{}", launches[2]);
                h.ctl.close();
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("claude was not started on the first conversation's session");
    }

    #[test]
    fn a_session_learned_just_before_moving_is_kept() {
        let h = harness();
        pair(&h);
        let new_conv = || call(&h, Some(&h.device), "POST", "/agent/sessions", "{}", "runA").1["id"].as_str().unwrap().to_string();
        let (a, b) = (new_conv(), new_conv());
        let body = json!({"text": "やあ", "via": "text"}).to_string();
        assert_eq!(call(&h, Some(&h.device), "POST", &format!("/agent/sessions/{a}/messages"), &body, "runA").0, 200);
        // claude has its session; the once-a-second look (poll) has not come
        for _ in 0..500 {
            if !h.ctl.session_id().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let sa = h.ctl.session_id();
        assert!(!sa.is_empty());
        // S3: the controller told serve at once, so no poll is needed
        assert_eq!(h.serve.sessions.session(&a).as_deref(), Some(sa.as_str()));
        assert_eq!(call(&h, Some(&h.device), "POST", &format!("/agent/sessions/{b}/messages"), &body, "runA").0, 200);
        assert_eq!(h.serve.sessions.session(&a).as_deref(), Some(sa.as_str()));
        h.ctl.close();
    }

    /// S3 (§18.3.1, §18.10 item 10): the ids are written where they are
    /// learned, not by the once-a-second poll, so two replaced within a second
    /// are both kept. Found: the controller's callback runs before the poll
    /// ever does (no poll is called in this test).
    #[test]
    fn two_sessions_within_a_second_are_both_kept() {
        let h = harness();
        pair(&h);
        let conv = call(&h, Some(&h.device), "POST", "/agent/sessions", "{}", "runA").1["id"].as_str().unwrap().to_string();
        let body = json!({"text": "やあ", "via": "text"}).to_string();
        assert_eq!(call(&h, Some(&h.device), "POST", &format!("/agent/sessions/{conv}/messages"), &body, "runA").0, 200);
        for _ in 0..500 {
            if h.serve.sessions.session(&conv).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let first = h.serve.sessions.session(&conv).expect("recorded by the callback, with no poll");
        // the session is resumed and replaced twice before a poll could look
        h.serve.learned("resumed-1");
        h.serve.learned("resumed-2");
        assert_eq!(h.serve.sessions.all_sessions(&conv), [first.as_str(), "resumed-1", "resumed-2"]);
        assert_eq!(h.serve.sessions.session(&conv).as_deref(), Some("resumed-2"));
        // and they are in the file
        let again = Sessions::load(&h.home.join("conf/sessions.json"));
        assert_eq!(again.all_sessions(&conv).len(), 3);
        h.ctl.close();
    }

    /// An id learned while claude is on no conversation (the warmed one) is
    /// picked up when a conversation takes it (a real claude can speak first).
    #[test]
    fn an_id_learned_with_no_conversation_goes_to_the_first() {
        let h = harness();
        pair(&h);
        h.serve.learned("too-early");
        let conv = call(&h, Some(&h.device), "POST", "/agent/sessions", "{}", "runA").1["id"].as_str().unwrap().to_string();
        assert_eq!(h.serve.sessions.session(&conv), None, "no conversation yet: nothing to put it on");
        h.ctl.close();
    }

    /// S6 over HTTP: `raw:true` on a message makes 「止めて」 go to Claude
    /// (fate `sent`); without it (the old callers) it is a command.
    #[test]
    fn raw_and_fate_over_http() {
        let h = harness();
        pair(&h);
        let (_, state) = call(&h, Some(&h.device), "GET", "/agent/state", "", "");
        let mut events = open_events(&h, &format!("runA:{}", state["seq"]));
        let conv = call(&h, Some(&h.device), "POST", "/agent/sessions", "{}", "runA").1["id"].as_str().unwrap().to_string();
        let path = format!("/agent/sessions/{conv}/messages");

        let body = json!({"text": "止めて", "via": "text"}).to_string();
        assert_eq!(call(&h, Some(&h.device), "POST", &path, &body, "runA").0, 200);
        let user = next(&mut events, "user");
        assert_eq!((user["text"].as_str(), user["fate"].as_str()), (Some("止めて"), Some("command")));

        let body = json!({"text": "止めて", "via": "text", "raw": true}).to_string();
        assert_eq!(call(&h, Some(&h.device), "POST", &path, &body, "runA").0, 200);
        let user = next(&mut events, "user");
        assert_eq!((user["text"].as_str(), user["fate"].as_str()), (Some("止めて"), Some("sent")));
        // Claude answers it: it was a message, not a command
        assert_eq!(next(&mut events, "assistant")["text"], "了解");

        // a raw that is not a boolean is no raw
        let body = json!({"text": "ストップ", "raw": "true"}).to_string();
        assert_eq!(call(&h, Some(&h.device), "POST", &path, &body, "runA").0, 200);
        assert_eq!(next(&mut events, "user")["fate"], "command");
        h.ctl.close();
    }

    #[test]
    fn unpairing_closes_the_open_stream() {
        // found in the review: a lost iPhone kept its stream after unpairing
        let h = harness();
        pair(&h);
        let (_, state) = call(&h, Some(&h.device), "GET", "/agent/state", "", "");
        let mut events = open_events(&h, &format!("runA:{}", state["seq"]));
        assert!(control::reply("unpair", &h.serve.devices, &|| h.serve.hub.close_client()).starts_with("ok"));
        let mut rest = String::new();
        use std::io::Read;
        let _ = events.read_to_string(&mut rest);
        assert!(!rest.contains("data:"), "{rest}");
        h.ctl.close();
    }

    #[test]
    fn a_message_to_an_unknown_conversation_is_refused() {
        let h = harness();
        pair(&h);
        let body = json!({"text": "やあ"}).to_string();
        assert_eq!(call(&h, Some(&h.device), "POST", "/agent/sessions/nope/messages", &body, "runA").0, 404);
        h.ctl.close();
    }

    #[test]
    fn the_working_directory_stays_clear_of_what_is_guarded() {
        let home = crate::util::home_dir();
        let guarded = [home.join("voice-agent"), home.join("lfm3-vl-server"), home.join("WKEN-AGENTS"), home.join(".config/voice-agent")];
        assert!(check_workdir(&home.join("agent-work"), &guarded).is_ok());
        // found in the design review: start.sh moves into lfm3-vl-server,
        // where acceptEdits would let Claude edit without asking
        assert!(check_workdir(&home.join("lfm3-vl-server"), &guarded).is_err());
        // lfm3-vl-server is being renamed WKEN-AGENTS (2026-10-31)
        assert!(check_workdir(&home.join("WKEN-AGENTS"), &guarded).is_err());
        assert!(check_workdir(&home.join("WKEN-AGENTS/server"), &guarded).is_err());
        // case does not matter on the Mac's volume; the boundary stays whole components
        for p in ["wken-agents", "Wken-Agents/server", "LFM3-VL-SERVER", "Voice-Agent/sub", ".Config/voice-agent"] {
            assert!(check_workdir(&home.join(p), &guarded).is_err(), "{p}");
        }
        assert!(check_workdir(&home.join("wken-agents-notes"), &guarded).is_ok());
        // APFS folds the full way (checked 2026-10-31): the long s and the fi ligature
        for p in ["WKEN-AGENTſ", "wken-agentſ/x", "lfm3-vl-ſerver", ".conﬁg/voice-agent", ".CONﬁG/voice-agent/x"] {
            assert!(check_workdir(&home.join(p), &guarded).is_err(), "{p}");
        }
        assert!(check_workdir(&home.join("WKEN-AGENﬆ"), &guarded).is_ok());
        assert!(check_workdir(&home.join("voice-agent/sub"), &guarded).is_err());
        assert!(check_workdir(&home, &guarded).is_err(), "home holds them all");
        assert!(check_workdir(Path::new("/"), &guarded).is_err());
        assert!(check_workdir(&home.join("agent-work/../voice-agent"), &guarded).is_err());
    }

    #[test]
    fn the_control_socket_path_has_to_fit() {
        let home = crate::util::home_dir();
        assert!(socket_path_fits(&home.join("Library/Caches/voice-agent/control.sock")));
        let long = Path::new("/private/tmp").join("x".repeat(90)).join("control.sock");
        assert!(!socket_path_fits(&long));
        // and binding one that long does fail
        assert!(std::os::unix::net::UnixListener::bind(&long).is_err());
    }

    #[test]
    fn every_event_kind_has_a_name() {
        for (k, n) in
            [(EventKind::AssistantEnd, "assistant_end"), (EventKind::ApprovalResolved, "approval_resolved"), (EventKind::ApprovalConfirm, "approval_confirm")]
        {
            assert_eq!(kind_name(k), n);
        }
    }
}
