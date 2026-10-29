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
//! `chunk_chars` how long it was. `end` says how the turn ended: sound,
//! silent (nothing was spoken: no text, or speech is off), stopped, error,
//! busy (sent while Claude was still at another reply, so the times would not
//! be its own), command, approval, no-speech, superseded (the next turn began
//! first), quit.
//!
//! A phrase from the cache that starts speech gets a line of its own
//! (`"kind":"phrase"`): `ms` is how soon it sounded.
//!
//! What was said is not written down, only times.

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

struct Turn {
    t0: Instant,
    at: String, // local time at t0
    via: String,
    marks: [Option<Instant>; 9],
    info: Vec<(&'static str, Value)>,
}

pub struct Latency {
    path: Option<PathBuf>, // None: nothing is written
    turn: Mutex<Option<Turn>>,
}

/// t (an instant in the past) as local time.
fn local_time(t: Instant) -> String {
    let ago = chrono::Duration::from_std(t.elapsed()).unwrap_or_default();
    (chrono::Local::now() - ago).format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string()
}

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

impl Latency {
    pub fn new(path: Option<PathBuf>) -> Latency {
        Latency { path, turn: Mutex::new(None) }
    }

    fn lock(&self) -> MutexGuard<'_, Option<Turn>> {
        self.turn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts the turn of what the user said or typed (via), which ended at
    /// t0. A turn still open is written out as superseded.
    pub fn begin(&self, via: &str, t0: Instant) {
        let mut turn = self.lock();
        if let Some(t) = turn.take() {
            self.write_turn(t, "superseded");
        }
        *turn = Some(Turn { t0, at: local_time(t0), via: via.to_string(), marks: [None; 9], info: Vec::new() });
    }

    /// Records that the turn reached m now, unless it did before.
    pub fn mark(&self, m: Mark) {
        if let Some(t) = self.lock().as_mut() {
            t.set(m, Instant::now());
        }
    }

    /// Adds something else worth knowing about the turn.
    pub fn note(&self, key: &'static str, value: impl Into<Value>) {
        if let Some(t) = self.lock().as_mut() {
            t.note(key, value.into());
        }
    }

    #[cfg(test)]
    pub fn is_open(&self) -> bool {
        self.lock().is_some()
    }

    /// Writes the turn out, if one is open.
    pub fn finish(&self, end: &str) {
        let t = self.lock().take();
        if let Some(t) = t {
            self.write_turn(t, end);
        }
    }

    /// A clip started to sound: the first one of the reply ends the turn, and
    /// a cached phrase that starts speech gets its line.
    pub fn sounded(&self, s: &Sounded) {
        if s.cached && s.first {
            self.write(&[("kind", "phrase".into()), ("at", local_time(s.at).into()), ("text", s.text.as_str().into()), ("ms", ms(s.at - s.asked).into())]);
        }
        if !s.reply {
            return;
        }
        let mut turn = self.lock();
        // A clip asked for before the turn began is from the reply before it
        let Some(t) = turn.as_mut().filter(|t| s.asked >= t.t0) else { return };
        if let Some(syn) = &s.synth {
            t.set(Mark::SynthAsk, syn.asked);
            t.set(Mark::SynthStart, syn.got);
            t.set(Mark::SynthEnd, syn.done);
        }
        t.set(Mark::FirstSound, s.at);
        let engine = if s.cached {
            "cache"
        } else if s.say {
            "say"
        } else {
            "aivis"
        };
        t.note("engine", engine.into());
        t.note("chunk_chars", s.text.chars().count().into());
        if let Some(t) = turn.take() {
            self.write_turn(t, "sound");
        }
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

    fn sounded(t0: Instant, reply: bool, cached: bool, first: bool) -> Sounded {
        Sounded {
            text: "はい、わかりました。".into(),
            reply,
            cached,
            say: false,
            first,
            asked: t0 + Duration::from_millis(1000),
            synth: (!cached).then(|| SynthTiming {
                asked: t0 + Duration::from_millis(1010),
                got: t0 + Duration::from_millis(1100),
                done: t0 + Duration::from_millis(1500),
            }),
            at: t0 + Duration::from_millis(1520),
        }
    }

    /// A turn is one line: the steps in order, as milliseconds from its
    /// start, the first time each was reached; the reply's first sound ends it.
    #[test]
    fn a_turn_is_a_line() {
        let path = log_file("turn");
        let l = Latency::new(Some(path.clone()));
        let t0 = Instant::now() - Duration::from_millis(30);
        l.mark(Mark::Sent); // no turn yet: dropped
        l.begin("voice", t0);
        l.note("stt", "final");
        l.mark(Mark::SttStart);
        l.mark(Mark::SttEnd);
        l.mark(Mark::Sent);
        std::thread::sleep(Duration::from_millis(5));
        l.mark(Mark::FirstText);
        l.mark(Mark::FirstChunk);
        let first_chunk = l.lock().as_ref().unwrap().marks[Mark::FirstChunk as usize];
        l.mark(Mark::FirstChunk); // later pieces do not count
        assert_eq!(l.lock().as_ref().unwrap().marks[Mark::FirstChunk as usize], first_chunk);
        l.sounded(&sounded(t0 - Duration::from_secs(2), true, false, true)); // from the reply before
        assert!(l.is_open());
        l.sounded(&sounded(t0, false, false, true)); // a notice, not the reply
        assert!(l.is_open());
        l.sounded(&sounded(t0, true, false, true));
        assert!(!l.is_open());
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

    /// README.md's table of the latency log names every step.
    #[test]
    fn readme_names_the_steps() {
        let readme = include_str!("../README.md");
        let section = readme.split("## 返答の速さ").nth(1).unwrap().split("\n## ").next().unwrap();
        for key in MARKS.iter().chain(&["engine", "chunk_chars", "end", "ms"]) {
            assert!(section.contains(&format!("`{key}`")), "README does not name {key}");
        }
    }

    /// A turn that ends any other way says so; a new one closes the one still
    /// open; without a path nothing is written.
    #[test]
    fn how_turns_end() {
        let path = log_file("end");
        let l = Latency::new(Some(path.clone()));
        l.begin("text", Instant::now());
        l.mark(Mark::Sent);
        l.begin("voice", Instant::now());
        l.finish("no-speech");
        l.finish("stopped"); // nothing open
        let got = lines(&path);
        let ends: Vec<(&str, &str)> = got.iter().map(|l| (l["via"].as_str().unwrap(), l["end"].as_str().unwrap())).collect();
        assert_eq!(ends, [("text", "superseded"), ("voice", "no-speech")]);
        assert!(got[0].contains_key("sent") && !got[0].contains_key("first_text"));
        let _ = std::fs::remove_file(&path);

        let quiet = Latency::new(None);
        quiet.begin("text", Instant::now());
        quiet.finish("silent");
    }

    /// A cached phrase that starts speech gets a line of its own; one that
    /// waited behind other speech does not.
    #[test]
    fn phrases_get_a_line() {
        let path = log_file("phrase");
        let l = Latency::new(Some(path.clone()));
        let t0 = Instant::now();
        l.sounded(&sounded(t0, false, true, true));
        l.sounded(&sounded(t0, false, true, false));
        let got = lines(&path);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0]["kind"].as_str(), got[0]["text"].as_str(), got[0]["ms"].as_u64()), (Some("phrase"), Some("はい、わかりました。"), Some(520)));
        let _ = std::fs::remove_file(&path);
    }
}
