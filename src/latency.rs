//! The latency log, `latency.jsonl` in the runtime directory (or
//! `$VA_LATENCY_LOG`): a line of JSON per turn, with the time of each step in
//! milliseconds from the end of what the user said (the Space key let go, or
//! the text sent) to the first sound of the reply:
//!
//! | key | the step |
//! |---|---|
//! | stt_start, stt_end | whisper transcribing it (`stt`: final; P2 adds early runs) |
//! | sent | the message going to Claude |
//! | first_text | the first text of the reply |
//! | first_chunk | the first piece of it handed to speech |
//! | synth_ask, synth_start, synth_end | the first chunk asking for the engine, getting it, its audio ready |
//! | first_sound | that chunk's first sample going to the speakers (for `say`, `say` starting) |
//!
//! `engine` says what spoke the first chunk (aivis, cache, say) and
//! `chunk_chars` how long it was; `stt_cold` that whisper had to be started
//! first (stt_start is when it was up). `end` says how the turn ended: sound,
//! silent (nothing was spoken: no text, or speech is off), stopped, error
//! (whisper, Claude or the first piece of speech failed), busy (sent while
//! Claude was still at another reply, so the times would not be its own),
//! command, approval (an answer to one, or one came before the reply
//! sounded), no-speech, superseded (never ended on its own), quit.
//!
//! A phrase from the cache that starts a run of speech (nothing was ahead of
//! it) gets a line of its own (`"kind":"phrase"`): `ms` is how soon it
//! sounded, `text` which phrase it was.
//!
//! What the user said and what Claude replied are not written down, only
//! times.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::speech::tts::Sounded;

/// The steps of a turn, in the order they happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    SttStart,
    SttEnd,
    Sent,
    FirstText,
    FirstChunk,
    SynthAsk,
    SynthStart,
    SynthEnd,
    FirstSound,
}

/// The keys of the marks, by Mark.
pub const MARKS: [&str; 9] = ["stt_start", "stt_end", "sent", "first_text", "first_chunk", "synth_ask", "synth_start", "synth_end", "first_sound"];

/// More turns open at once than this means some never ended: the oldest is
/// written out as superseded.
const MAX_OPEN: usize = 4;

struct Turn {
    id: u64,
    t0: Instant,
    at: String, // local time at t0
    via: String,
    marks: [Option<Instant>; 9],
    info: Vec<(&'static str, Value)>,
}

#[derive(Default)]
struct State {
    open: Vec<Turn>, // oldest first
    last: u64,       // the latest turn's id (ids start at 1)
    replying: u64,   // the turn Claude's reply is for (0: none yet)
}

/// Turns are told apart by the id begin gives: what happens to one (a
/// transcription, a reply) never lands on another that began meanwhile.
pub struct Latency {
    path: Option<PathBuf>, // None: nothing is written
    st: Mutex<State>,
}

/// t (an instant in the past) as local time.
fn local_time(t: Instant) -> String {
    let ago = chrono::Duration::from_std(t.elapsed()).unwrap_or_default();
    (chrono::Local::now() - ago).format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string()
}

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

impl State {
    fn turn(&mut self, id: u64) -> Option<&mut Turn> {
        self.open.iter_mut().find(|t| t.id == id)
    }

    fn take(&mut self, id: u64) -> Option<Turn> {
        let i = self.open.iter().position(|t| t.id == id)?;
        Some(self.open.remove(i))
    }
}

impl Latency {
    pub fn new(path: Option<PathBuf>) -> Latency {
        Latency { path, st: Mutex::default() }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts the turn of what the user said or typed (via), which ended at
    /// t0, and returns its id.
    pub fn begin(&self, via: &str, t0: Instant) -> u64 {
        let mut st = self.lock();
        st.last += 1;
        let id = st.last;
        st.open.push(Turn { id, t0, at: local_time(t0), via: via.to_string(), marks: [None; 9], info: Vec::new() });
        if st.open.len() > MAX_OPEN {
            let t = st.open.remove(0);
            self.write_turn(t, "superseded");
        }
        id
    }

    /// Claude's reply is for turn from now on (it was sent to Claude while
    /// Claude was free). The turn of the reply before, if still open, will
    /// hear no more of it: it is written out as superseded.
    pub fn replying_to(&self, turn: u64) {
        let mut st = self.lock();
        let before = std::mem::replace(&mut st.replying, turn);
        if before != turn
            && let Some(t) = st.take(before)
        {
            self.write_turn(t, "superseded");
        }
    }

    /// The turn Claude's reply is for (0: none).
    pub fn replying(&self) -> u64 {
        self.lock().replying
    }

    /// Records that turn reached m now, unless it did before.
    pub fn mark(&self, turn: u64, m: Mark) {
        self.mark_at(turn, m, Instant::now());
    }

    /// Records that turn reached m at at, unless it did before.
    pub fn mark_at(&self, turn: u64, m: Mark, at: Instant) {
        if let Some(t) = self.lock().turn(turn) {
            t.set(m, at);
        }
    }

    /// mark for the turn Claude's reply is for.
    pub fn mark_reply(&self, m: Mark) {
        let mut st = self.lock();
        let id = st.replying;
        if let Some(t) = st.turn(id) {
            t.set(m, Instant::now());
        }
    }

    /// Adds something else worth knowing about turn.
    pub fn note(&self, turn: u64, key: &'static str, value: impl Into<Value>) {
        if let Some(t) = self.lock().turn(turn) {
            t.note(key, value.into());
        }
    }

    #[cfg(test)]
    pub fn is_open(&self, turn: u64) -> bool {
        self.lock().turn(turn).is_some()
    }

    /// Writes turn out, if it is open.
    pub fn finish(&self, turn: u64, end: &str) {
        let t = self.lock().take(turn);
        if let Some(t) = t {
            self.write_turn(t, end);
        }
    }

    /// finish for the turn Claude's reply is for.
    pub fn finish_reply(&self, end: &str) {
        let mut st = self.lock();
        let id = st.replying;
        if let Some(t) = st.take(id) {
            self.write_turn(t, end);
        }
    }

    /// Writes out every turn still open (the app is closing).
    pub fn finish_all(&self, end: &str) {
        let open = std::mem::take(&mut self.lock().open);
        for t in open {
            self.write_turn(t, end);
        }
    }

    /// Claude's reply has ended (error: it failed). A reply that handed
    /// nothing to speech ends its turn as silent; one that did waits for
    /// that speech to sound or fail.
    pub fn reply_ended(&self, error: bool) {
        let mut st = self.lock();
        let id = st.replying;
        let Some(t) = st.turn(id) else { return };
        if error || t.marks[Mark::FirstChunk as usize].is_none() {
            let t = st.take(id);
            if let Some(t) = t {
                self.write_turn(t, if error { "error" } else { "silent" });
            }
        }
    }

    /// A clip started to sound or was skipped: the reply's first one ends
    /// its turn, and a cached phrase that starts a run gets its line.
    pub fn sounded(&self, s: &Sounded) {
        if s.cached
            && s.first
            && let Some(at) = s.at
        {
            self.write(&[("kind", "phrase".into()), ("at", local_time(at).into()), ("text", s.text.as_str().into()), ("ms", ms(at - s.asked).into())]);
        }
        let mut st = self.lock();
        let Some(mut t) = st.take(s.turn) else { return };
        let Some(at) = s.at else {
            // Its first piece of speech failed: nothing of it sounded in time
            return self.write_turn(t, "error");
        };
        if let Some(syn) = &s.synth {
            t.set(Mark::SynthAsk, syn.asked);
            t.set(Mark::SynthStart, syn.got);
            t.set(Mark::SynthEnd, syn.done);
        }
        t.set(Mark::FirstSound, at);
        let engine = if s.cached {
            "cache"
        } else if s.say {
            "say"
        } else {
            "aivis"
        };
        t.note("engine", engine.into());
        t.note("chunk_chars", s.text.chars().count().into());
        self.write_turn(t, "sound");
    }

    fn write_turn(&self, t: Turn, end: &str) {
        let mut fields: Vec<(&str, Value)> = vec![("kind", "turn".into()), ("at", t.at.into()), ("via", t.via.into()), ("end", end.into())];
        for (key, at) in MARKS.iter().zip(t.marks) {
            if let Some(at) = at {
                fields.push((key, ms(at.saturating_duration_since(t.t0)).into()));
            }
        }
        fields.extend(t.info);
        self.write(&fields);
    }

    /// Appends fields as a line of JSON, in their order.
    fn write(&self, fields: &[(&str, Value)]) {
        let Some(path) = &self.path else { return };
        let body: Vec<String> = fields.iter().map(|(k, v)| format!("{}:{v}", Value::from(*k))).collect();
        let line = format!("{{{}}}\n", body.join(","));
        use std::os::unix::fs::OpenOptionsExt;
        let res = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(path).and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = res {
            crate::logf!("latency: {}: {e}", path.display());
        }
    }
}

impl Turn {
    fn set(&mut self, m: Mark, at: Instant) {
        self.marks[m as usize].get_or_insert(at);
    }

    fn note(&mut self, key: &'static str, value: Value) {
        match self.info.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => *v = value,
            None => self.info.push((key, value)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speech::tts::SynthTiming;

    fn log_file(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("va-latency-{name}-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn lines(path: &PathBuf) -> Vec<serde_json::Map<String, Value>> {
        std::fs::read_to_string(path).unwrap_or_default().lines().map(|l| serde_json::from_str::<Value>(l).unwrap().as_object().unwrap().clone()).collect()
    }

    fn ends(path: &PathBuf) -> Vec<(String, String)> {
        lines(path).iter().map(|l| (l["via"].as_str().unwrap().to_string(), l["end"].as_str().unwrap().to_string())).collect()
    }

    fn sounded(t0: Instant, turn: u64, cached: bool, first: bool) -> Sounded {
        Sounded {
            text: "はい、わかりました。".into(),
            turn,
            cached,
            say: false,
            first,
            asked: t0 + Duration::from_millis(1000),
            synth: (!cached).then(|| SynthTiming {
                asked: t0 + Duration::from_millis(1010),
                got: t0 + Duration::from_millis(1100),
                done: t0 + Duration::from_millis(1500),
            }),
            at: Some(t0 + Duration::from_millis(1520)),
        }
    }

    /// A turn is one line: the steps in order, as milliseconds from its
    /// start, the first time each was reached; the reply's first sound ends it.
    #[test]
    fn a_turn_is_a_line() {
        let path = log_file("turn");
        let l = Latency::new(Some(path.clone()));
        let t0 = Instant::now() - Duration::from_millis(30);
        l.mark_reply(Mark::Sent); // no turn yet: dropped
        let turn = l.begin("voice", t0);
        l.note(turn, "stt", "final");
        l.mark(turn, Mark::SttStart);
        l.mark(turn, Mark::SttEnd);
        l.replying_to(turn);
        l.mark(turn, Mark::Sent);
        std::thread::sleep(Duration::from_millis(5));
        l.mark_reply(Mark::FirstText);
        l.mark_reply(Mark::FirstChunk);
        let first_chunk = l.lock().turn(turn).unwrap().marks[Mark::FirstChunk as usize];
        l.mark_reply(Mark::FirstChunk); // later pieces do not count
        assert_eq!(l.lock().turn(turn).unwrap().marks[Mark::FirstChunk as usize], first_chunk);
        l.sounded(&sounded(t0, turn - 1, false, true)); // the reply to the turn before
        assert!(l.is_open(turn));
        l.sounded(&sounded(t0, 0, false, true)); // voice-agent's own words, not the reply
        assert!(l.is_open(turn));
        l.sounded(&sounded(t0, turn, false, true));
        assert!(!l.is_open(turn));
        let got = lines(&path);
        assert_eq!(got.len(), 1);
        let line = &got[0];
        let keys: Vec<&str> = line.keys().map(String::as_str).collect();
        for key in MARKS.iter().chain(&["kind", "at", "via", "end", "stt", "engine", "chunk_chars"]) {
            assert!(keys.contains(key), "{key} is missing: {line:?}");
        }
        assert_eq!((line["kind"].as_str(), line["via"].as_str(), line["end"].as_str()), (Some("turn"), Some("voice"), Some("sound")));
        assert_eq!((line["synth_ask"].as_u64(), line["synth_start"].as_u64(), line["synth_end"].as_u64()), (Some(1010), Some(1100), Some(1500)));
        assert_eq!(line["first_sound"].as_u64(), Some(1520));
        assert_eq!((line["engine"].as_str(), line["chunk_chars"].as_u64()), (Some("aivis"), Some(10)));
        let times: Vec<u64> = MARKS.iter().map(|k| line[*k].as_u64().unwrap()).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "in order: {times:?}");
        assert!(times[0] >= 30, "from the end of the utterance: {times:?}");
        // The order of the keys is the order of the steps
        let raw = std::fs::read_to_string(&path).unwrap();
        let at = |k: &str| raw.find(&format!("\"{k}\"")).unwrap();
        assert!(MARKS.windows(2).all(|w| at(w[0]) < at(w[1])), "{raw}");
        let _ = std::fs::remove_file(&path);
    }

    /// README.md's account of the latency log names every step, key and end.
    #[test]
    fn readme_names_the_steps() {
        let readme = include_str!("../README.md");
        let section = readme.split("## 返答の速さ").nth(1).unwrap().split("\n## ").next().unwrap();
        let ends = ["sound", "silent", "stopped", "error", "busy", "command", "approval", "no-speech", "superseded", "quit"];
        for key in MARKS.iter().chain(&["engine", "chunk_chars", "stt_cold", "end", "ms", "text"]).chain(&ends) {
            assert!(section.contains(&format!("`{key}`")), "README does not name {key}");
        }
    }

    /// A turn that ends any other way says so; turns that overlap keep their
    /// own steps; without a path nothing is written.
    #[test]
    fn how_turns_end() {
        let path = log_file("end");
        let l = Latency::new(Some(path.clone()));
        let text = l.begin("text", Instant::now());
        l.replying_to(text);
        l.mark(text, Mark::Sent);
        // Spoken while the reply to it is still to come: each keeps its own
        let voice = l.begin("voice", Instant::now());
        l.mark(voice, Mark::SttStart);
        l.finish(voice, "no-speech");
        l.finish(voice, "stopped"); // already written
        l.reply_ended(false); // the reply had nothing to say aloud
        let got = lines(&path);
        assert_eq!(ends(&path), [("voice".into(), "no-speech".into()), ("text".into(), "silent".into())]);
        assert!(got[1].contains_key("sent") && !got[1].contains_key("stt_start"));

        // A reply that handed speech on waits for it; its first piece failing ends it
        let t = l.begin("text", Instant::now());
        l.replying_to(t);
        l.mark_reply(Mark::FirstChunk);
        l.reply_ended(false);
        assert!(l.is_open(t));
        l.sounded(&Sounded { at: None, ..sounded(Instant::now(), t, false, true) });
        // An approval before the reply sounded, a reply sent while another was
        // still open, a failed reply, and the app closing
        let asked = l.begin("voice", Instant::now());
        l.replying_to(asked);
        l.finish_reply("approval");
        let stuck = l.begin("text", Instant::now());
        l.replying_to(stuck);
        let next = l.begin("text", Instant::now());
        l.replying_to(next);
        l.reply_ended(true);
        let left = l.begin("voice", Instant::now());
        l.finish_all("quit");
        assert!(!l.is_open(left));
        let got: Vec<String> = ends(&path).into_iter().skip(2).map(|(_, e)| e).collect();
        assert_eq!(got, ["error", "approval", "superseded", "error", "quit"]);
        let _ = std::fs::remove_file(&path);

        let quiet = Latency::new(None);
        let t = quiet.begin("text", Instant::now());
        quiet.finish(t, "silent");
    }

    /// A cached phrase that starts a run gets a line of its own; one that
    /// waited behind other speech does not, nor does one that never sounded.
    #[test]
    fn phrases_get_a_line() {
        let path = log_file("phrase");
        let l = Latency::new(Some(path.clone()));
        let t0 = Instant::now();
        l.sounded(&sounded(t0, 0, true, true));
        l.sounded(&sounded(t0, 0, true, false));
        l.sounded(&Sounded { at: None, ..sounded(t0, 0, true, true) });
        let got = lines(&path);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0]["kind"].as_str(), got[0]["text"].as_str(), got[0]["ms"].as_u64()), (Some("phrase"), Some("はい、わかりました。"), Some(520)));
        let _ = std::fs::remove_file(&path);
    }

    /// Turns left open (a transcription that never came back) do not pile up.
    #[test]
    fn open_turns_are_bounded() {
        let path = log_file("bound");
        let l = Latency::new(Some(path.clone()));
        let first = l.begin("voice", Instant::now());
        for _ in 0..MAX_OPEN {
            l.begin("voice", Instant::now());
        }
        assert!(!l.is_open(first));
        assert_eq!(ends(&path), [("voice".into(), "superseded".into())]);
        let _ = std::fs::remove_file(&path);
    }
}
