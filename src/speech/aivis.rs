//! Text-to-speech through AivisSpeech Engine's VOICEVOX-compatible HTTP API.
//!
//! The engine start, the requests and the error classification are as in
//! edex-rs. Who gets the engine when is the arbiter's (arbiter.rs): speech
//! first, then the phrase cache, then warm-ups.

use std::io::{BufRead, BufReader};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, mpsc};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::arbiter::{Arbiter, Use};
use super::tts::Cancel;
use crate::audio::playback::DEFAULT_RATE;
use crate::audio::wav::wav_seconds;
use crate::log::log;
use crate::net;
use crate::util::{clip_chars, home_dir};

/// How long one chunk waits for its audio before the wait is logged and
/// given one more round; after that the chunk is skipped. AivisSpeech cannot
/// cancel a synthesis, so sending the chunk again would only queue it behind
/// the abandoned one.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(9);
/// Bounds a warm-up, which may have to load the voice model.
const WARM_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the engine counts as warm after it last synthesized: a warm-up
/// within this time of speech is left out.
const WARM_EVERY: Duration = Duration::from_secs(10);
/// How long a warm-up waits for whisper and the subagents to finish before
/// it is given up: by then the reply it was for is usually being spoken.
const WARM_WAIT: Duration = Duration::from_secs(30);
/// How often a warm-up that waits looks again.
const WARM_POLL: Duration = Duration::from_millis(200);
/// Synthesized to get the voice model resident. Short, because a synthesis
/// cannot be stopped: speech that comes up meanwhile waits for it.
const WARM_TEXT: &str = "はい。";

#[derive(Debug, Clone, PartialEq)]
pub enum SynthError {
    /// A chunk that got no audio within two chunk timeouts
    Timeout,
    Cancelled,
    /// An error status from the engine
    Http(u16, String),
    Net(String),
}

impl std::fmt::Display for SynthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SynthError::Timeout => f.write_str("no audio in time"),
            SynthError::Cancelled => f.write_str("cancelled"),
            SynthError::Http(code, body) => write!(f, "AivisSpeech HTTP {code}: {body}"),
            SynthError::Net(e) => f.write_str(e),
        }
    }
}

impl SynthError {
    /// Tells errors meaning the engine is not there (refused or dropped
    /// connection, server error) from ones about a single chunk (timeout,
    /// rejected text). Only the former switch speech to `say`.
    pub fn unavailable(&self) -> bool {
        match self {
            SynthError::Timeout | SynthError::Cancelled => false,
            SynthError::Http(code, _) => *code >= 500,
            SynthError::Net(_) => true,
        }
    }
}

#[derive(Default)]
struct State {
    down: bool,     // not answering or still warming up: speech uses `say`
    watching: bool, // a thread waits for the engine to answer again
    closed: bool,
    timeouts: u32,             // chunks lost to timeouts in a row
    warm_at: Option<Instant>,  // the engine last synthesized something
    pace: f64,                 // synthesis time over audio time of the latest chunk (0 = not known)
    warm_seq: u64,             // the latest warm_once; an older one still waiting gives way to it
    stdin: Option<ChildStdin>, // our launch of the engine, if any
}

/// A chunk's audio, and how long it waited for the engine.
pub struct Synth {
    pub wav: Vec<u8>,
    pub waited: Duration,
}

/// When nothing answers at url, start runs the engine bundled with
/// AivisSpeech.app; until it is up and warm the speaker uses `say`.
pub struct AivisEngine {
    url: String,  // http://127.0.0.1:10101
    speaker: i64, // style ID (GET /speakers)
    speed: f64,
    pub timeout: Duration, // per chunk wait
    pub every: Duration,   // see WARM_EVERY
    pub wait: Duration,    // see WARM_WAIT
    /// Hands out the engine: speech first, then the cache, then warm-ups
    pub arbiter: Arc<Arbiter>,
    /// Called each time the engine is up and warm (at the start, and when it
    /// is back after being down): the phrase cache is built then
    pub on_ready: OnceLock<Box<dyn Fn() + Send + Sync>>,
    /// The sample rate asked for (the output device's)
    pub rate: AtomicU32,
    st: Mutex<State>,
}

/// The engine inside AivisSpeech.app (None = not installed).
fn engine_bin() -> Option<std::path::PathBuf> {
    [std::path::PathBuf::from("/Applications"), home_dir().join("Applications")]
        .into_iter()
        .map(|apps| apps.join("AivisSpeech.app/Contents/Resources/AivisSpeech-Engine/run"))
        .find(|p| p.is_file())
}

impl AivisEngine {
    pub fn new(url: &str, speaker: i64, speed: f64) -> Arc<AivisEngine> {
        Arc::new(AivisEngine {
            url: url.to_string(),
            speaker,
            speed,
            timeout: CHUNK_TIMEOUT,
            every: WARM_EVERY,
            wait: WARM_WAIT,
            arbiter: Arbiter::new(),
            on_ready: OnceLock::new(),
            rate: AtomicU32::new(DEFAULT_RATE),
            st: Mutex::new(State::default()),
        })
    }

    #[cfg(test)]
    pub fn with_timing(url: &str, timeout: Duration, every: Duration) -> Arc<AivisEngine> {
        AivisEngine::tuned(url, |a| {
            a.timeout = timeout;
            a.every = every;
        })
    }

    #[cfg(test)]
    pub fn tuned(url: &str, tune: impl FnOnce(&mut AivisEngine)) -> Arc<AivisEngine> {
        let mut a = Arc::try_unwrap(AivisEngine::new(url, 1, 1.3)).unwrap_or_else(|_| unreachable!());
        tune(&mut a);
        Arc::new(a)
    }

    /// The style and the speed, which the phrase cache is made for.
    pub fn voice(&self) -> (i64, f64) {
        (self.speaker, self.speed)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn alive(&self) -> bool {
        matches!(net::get(&format!("{}/version", self.url), Duration::from_secs(1)), Ok((200, _)))
    }

    /// Launches the engine unless one (e.g. the AivisSpeech app) already
    /// answers at url, then warms it up in the background. Speech uses `say`
    /// until the engine is ready.
    pub fn start(self: &Arc<Self>) {
        self.lock().down = true;
        if self.alive() {
            log(&format!("aivis: engine at {}", self.url));
        } else {
            match engine_bin() {
                None => log(&format!("aivis: no engine at {} and AivisSpeech.app is not installed; using say", self.url)),
                Some(bin) => self.launch(&bin),
            }
        }
        self.watch();
    }

    fn launch(&self, bin: &std::path::Path) {
        let rest = self.url.split_once("://").map_or(self.url.as_str(), |(_, r)| r);
        let hostport = rest.split('/').next().unwrap_or("");
        let (host, port) = hostport.rsplit_once(':').unwrap_or((hostport, "10101"));
        // The engine dies with our end of its stdin (exit, crash, kill -9)
        let child = Command::new("/bin/sh")
            .args(["-c", r#""$0" "$@" 2>&1 & child=$!; read _; kill $child"#])
            .arg(bin)
            .args(["--host", host, "--port", port])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => return log(&format!("aivis: {e}")),
        };
        self.lock().stdin = child.stdin.take();
        log(&format!("aivis: starting {}", bin.display()));
        let out = child.stdout.take();
        std::thread::spawn(move || {
            if let Some(out) = out {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    log(&format!("aivis: {line}"));
                }
            }
            log(&format!("aivis: engine exited ({:?})", child.wait().ok()));
        });
    }

    /// Waits in the background for the engine to answer, warms it up and
    /// then hands speech back to it.
    fn watch(self: &Arc<Self>) {
        {
            let mut st = self.lock();
            if st.watching || st.closed {
                return;
            }
            st.watching = true;
        }
        let a = self.clone();
        std::thread::spawn(move || {
            let mut delay = Duration::from_secs(2);
            let mut last_err = String::new();
            loop {
                {
                    let mut st = a.lock();
                    if st.closed {
                        st.watching = false;
                        return;
                    }
                }
                if a.alive() {
                    let t0 = Instant::now();
                    match a.warm_up() {
                        Ok(()) => {
                            {
                                let mut st = a.lock();
                                st.watching = false;
                                st.down = false;
                                st.timeouts = 0;
                            }
                            log(&format!("aivis: ready (warm-up {:.1}s)", t0.elapsed().as_secs_f64()));
                            if let Some(f) = a.on_ready.get() {
                                f();
                            }
                            return;
                        }
                        Err(e) => {
                            let e = e.to_string();
                            if e != last_err {
                                log(&format!("aivis: warm-up: {e}"));
                                last_err = e;
                            }
                            delay = (delay * 2).min(Duration::from_secs(30));
                        }
                    }
                }
                std::thread::sleep(if cfg!(test) { Duration::from_millis(20) } else { delay });
            }
        });
    }

    /// Loads the voice and runs one synthesis to see that the engine works.
    fn warm_up(&self) -> Result<(), SynthError> {
        let _grant = self.arbiter.acquire(Use::Warm, &Cancel::new(), WARM_TIMEOUT)?;
        self.post(&format!("/initialize_speaker?speaker={}&skip_reinit=true", self.speaker), None, WARM_TIMEOUT)?;
        self.request(WARM_TEXT, WARM_TIMEOUT)?;
        self.lock().warm_at = Some(Instant::now());
        Ok(())
    }

    /// Gets the voice model resident for the reply to what the user just
    /// said; called once an utterance, when it goes to Claude. An engine left
    /// idle is paged out within seconds when memory is short, and is several
    /// times slower than playback until it is back in.
    ///
    /// The warm-up waits while whisper transcribes, a subagent works (the
    /// arbiter says when) or speech wants the engine. It is left out once the
    /// engine has synthesized within `every`, or after `wait`; a newer call
    /// takes the place of one still waiting.
    pub fn warm_once(self: &Arc<Self>) {
        let seq = {
            let mut st = self.lock();
            if st.down || st.closed {
                return;
            }
            st.warm_seq += 1;
            st.warm_seq
        };
        let a = self.clone();
        std::thread::spawn(move || {
            let until = Instant::now() + a.wait;
            loop {
                {
                    let st = a.lock();
                    if st.down || st.closed || st.warm_seq != seq || !warm_left(&st, a.every).is_zero() {
                        return;
                    }
                }
                if a.arbiter.may(Use::Warm) && !a.arbiter.speech_wanted() {
                    break;
                }
                if Instant::now() >= until {
                    return log("aivis: warm-up left out: whisper or a subagent was at work");
                }
                std::thread::sleep(WARM_POLL);
            }
            let t0 = Instant::now();
            match a.warm_pass() {
                Ok(true) => log(&format!("aivis: warm-up {:.1}s", t0.elapsed().as_secs_f64())),
                Ok(false) => {}
                Err(e) => a.note(Some(&e)),
            }
        });
    }

    /// Synthesizes WARM_TEXT unless speech has warmed the engine meanwhile
    /// (false): speech that asks for the engine meanwhile gets it first.
    fn warm_pass(&self) -> Result<bool, SynthError> {
        let _grant = self.arbiter.acquire(Use::Warm, &Cancel::new(), WARM_TIMEOUT)?;
        if !warm_left(&self.lock(), self.every).is_zero() {
            return Ok(false);
        }
        self.request(WARM_TEXT, WARM_TIMEOUT)?;
        self.lock().warm_at = Some(Instant::now());
        Ok(true)
    }

    pub fn stop(&self) {
        let mut st = self.lock();
        st.closed = true;
        st.stdin = None; // closing our end ends the engine we launched
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Returns WAV audio for one chunk of speech (and how long it waited for
    /// the engine), or Timeout when the engine has not delivered it within two
    /// chunk timeouts.
    pub fn synth(self: &Arc<Self>, cancel: &Cancel, text: &str) -> Result<Synth, SynthError> {
        self.synth_as(Use::Speech, cancel, text)
    }

    /// synth for u, which decides who goes first (arbiter.rs).
    pub fn synth_as(self: &Arc<Self>, u: Use, cancel: &Cancel, text: &str) -> Result<Synth, SynthError> {
        let t0 = Instant::now();
        let grant = self.arbiter.acquire(u, cancel, Duration::from_secs(3600))?;
        let waited = t0.elapsed();
        let wait = self.timeout;
        let t1 = Instant::now();
        // The request runs on a thread of its own so that a stop does not have
        // to wait for it. The engine stays taken until the request is over,
        // since AivisSpeech goes on with it anyway
        let (tx, rx) = mpsc::channel();
        let (a, t) = (self.clone(), text.to_string());
        std::thread::spawn(move || {
            let res = a.request(&t, wait * 2 + Duration::from_secs(1));
            if res.is_ok() {
                // Speech keeps the engine warm by itself; recorded before the
                // engine is let go, so that a warm-up waiting for it sees it
                a.lock().warm_at = Some(Instant::now());
            }
            drop(grant);
            let _ = tx.send(res);
        });
        let mut warned = false;
        let res = loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(r) => break r,
                Err(mpsc::RecvTimeoutError::Disconnected) => break Err(SynthError::Net("synthesis thread ended".into())),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if cancel.is_cancelled() {
                break Err(SynthError::Cancelled);
            }
            let taking = t1.elapsed();
            if taking >= wait * 2 {
                break Err(SynthError::Timeout);
            }
            if taking >= wait && !warned {
                warned = true;
                log(&format!("aivis: no audio after {wait:?} for {:?}; waiting once more", excerpt(text)));
            }
        };
        if let Ok(wav) = &res {
            let audio = wav_seconds(wav);
            if audio > 0.0 {
                self.lock().pace = t1.elapsed().as_secs_f64() / audio;
            }
        }
        res.map(|wav| Synth { wav, waited })
    }

    /// How many times longer than the last chunk the next one may be and
    /// still be synthesized while that one plays: 2 for an engine twice as
    /// fast as playback (and one whose speed is not known yet), 1 for one
    /// that only just keeps up.
    pub fn growth(&self) -> f64 {
        let pace = self.lock().pace;
        if pace <= 0.0 { 2.0 } else { (1.0 / pace).clamp(1.0, 2.0) }
    }

    /// audio_query + synthesis for text.
    fn request(&self, text: &str, timeout: Duration) -> Result<Vec<u8>, SynthError> {
        let data = self.post(&format!("/audio_query?speaker={}&text={}", self.speaker, net::query_escape(text)), None, timeout)?;
        let mut query: Value = serde_json::from_slice(&data).map_err(|e| SynthError::Net(e.to_string()))?;
        let Some(q) = query.as_object_mut() else { return Err(SynthError::Net("audio_query: not an object".into())) };
        q.insert("speedScale".into(), self.speed.into());
        // Chunks follow each other directly: no lead-in, a short tail, and
        // tighter pauses inside a chunk to match the faster speed
        q.insert("prePhonemeLength".into(), 0.0.into());
        q.insert("postPhonemeLength".into(), 0.05.into());
        q.insert("pauseLengthScale".into(), 0.7.into());
        q.insert("outputSamplingRate".into(), self.rate.load(Ordering::SeqCst).into());
        q.insert("outputStereo".into(), false.into());
        let body = serde_json::to_vec(&query).map_err(|e| SynthError::Net(e.to_string()))?;
        self.post(&format!("/synthesis?speaker={}", self.speaker), Some(&body), timeout)
    }

    fn post(&self, path: &str, body: Option<&[u8]>, timeout: Duration) -> Result<Vec<u8>, SynthError> {
        let url = format!("{}{path}", self.url);
        let (status, data) = net::post(&url, body.map(|_| "application/json"), body.unwrap_or_default(), timeout).map_err(SynthError::Net)?;
        if status / 100 != 2 {
            return Err(SynthError::Http(status, clip_bytes(&data, 200)));
        }
        Ok(data)
    }

    pub fn is_down(&self) -> bool {
        self.lock().down
    }

    /// The engine's version, which the phrase cache is made for.
    pub fn version(&self) -> Option<String> {
        match net::get(&format!("{}/version", self.url), Duration::from_secs(2)) {
            Ok((200, body)) => Some(String::from_utf8_lossy(&body).trim().trim_matches('"').to_string()).filter(|v| !v.is_empty()),
            _ => None,
        }
    }

    /// Records how a chunk went. An engine that is not there, or that lost
    /// two chunks in a row to timeouts, hands speech to `say` until it is back.
    pub fn note(self: &Arc<Self>, err: Option<&SynthError>) {
        let down = {
            let mut st = self.lock();
            match err {
                None => {
                    st.timeouts = 0;
                    None
                }
                Some(SynthError::Timeout) => {
                    st.timeouts += 1;
                    (st.timeouts >= 2).then(|| format!("no audio for {} chunks in a row", st.timeouts))
                }
                Some(e) if e.unavailable() => Some(e.to_string()),
                Some(_) => None,
            }
        };
        if let Some(why) = down {
            self.mark_down(&why);
        }
    }

    fn mark_down(self: &Arc<Self>, why: &str) {
        let was = {
            let mut st = self.lock();
            let was = st.down;
            st.down = true;
            st.timeouts = 0;
            was
        };
        if !was {
            log(&format!("aivis: {why}; using say"));
        }
        self.watch();
    }
}

/// How much longer the engine counts as warm.
fn warm_left(st: &State, every: Duration) -> Duration {
    st.warm_at.map_or(Duration::ZERO, |at| (at + every).saturating_duration_since(Instant::now()))
}

fn clip_bytes(data: &[u8], n: usize) -> String {
    String::from_utf8_lossy(&data[..data.len().min(n)]).trim().to_string()
}

/// Shortens speech for a log line.
pub fn excerpt(s: &str) -> String {
    clip_chars(s, 20)
}

/// An AivisSpeech stand-in for the tests of everything that speaks.
#[cfg(test)]
pub mod testing {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::Value;

    use crate::audio::wav::tests::test_wav;
    use crate::net::testing::{Request, Server, serve};

    /// The engine's version as the stand-in tells it.
    pub const VERSION: &str = "1.0.0-test";

    /// The audio of a text is 10 ms per character and takes delay(text) to
    /// synthesize; status(text) other than 200 refuses it.
    pub struct Engine {
        /// What was synthesized, in the order it was finished
        pub synths: Arc<Mutex<Vec<String>>>,
        /// The synthesis requests' bodies
        pub bodies: Arc<Mutex<Vec<Value>>>,
        pub srv: Server,
    }

    pub fn engine(status: impl Fn(&str) -> u16 + Send + Sync + 'static, delay: impl Fn(&str) -> Duration + Send + Sync + 'static) -> Engine {
        let synths = Arc::new(Mutex::new(Vec::new()));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let (s, b) = (synths.clone(), bodies.clone());
        let srv = serve(move |r: &Request| {
            let path = r.url.split('?').next().unwrap_or("");
            match path {
                "/version" => (200, format!("\"{VERSION}\"").into_bytes()),
                "/initialize_speaker" => (204, Vec::new()),
                "/audio_query" => {
                    let text = r.url.split("text=").nth(1).unwrap_or("").split('&').next().unwrap_or("");
                    (200, serde_json::to_vec(&serde_json::json!({"kana": percent_decode(text), "speedScale": 1.0})).unwrap())
                }
                "/synthesis" => {
                    let q: Value = serde_json::from_slice(&r.body).unwrap_or_default();
                    let text = q["kana"].as_str().unwrap_or("").to_string();
                    b.lock().unwrap().push(q.clone());
                    let st = status(&text);
                    if st != 200 {
                        return (st, b"no".to_vec());
                    }
                    std::thread::sleep(delay(&text));
                    s.lock().unwrap().push(text.clone());
                    let rate = q["outputSamplingRate"].as_u64().unwrap_or(44100) as u32;
                    (200, test_wav(text.chars().count() as f64 * 0.01, rate))
                }
                _ => (404, Vec::new()),
            }
        });
        Engine { synths, bodies, srv }
    }

    fn percent_decode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%'
                && i + 2 < b.len()
                && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
            {
                out.push(v);
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::engine;
    use super::*;
    use crate::speech::arbiter::Activity;

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The engine is busy with a long phrase for the cache when a warm-up and
    /// then a chunk of speech ask for it: the speech goes first, and the
    /// warm-up, which cannot be stopped once it runs, only after it.
    #[test]
    fn a_warm_up_does_not_get_ahead_of_speech() {
        let e = engine(|_| 200, |text| Duration::from_millis(if text == "長い文です。" { 300 } else { 5 }));
        // every 0: the warm-up is not left out because speech warmed the engine
        let a = AivisEngine::tuned(&e.srv.url, |a| a.every = Duration::ZERO);
        let cache = {
            let a = a.clone();
            std::thread::spawn(move || a.synth_as(Use::Cache, &Cancel::new(), "長い文です。").map(|s| s.wav.len()))
        };
        wait_until("the cache holds the engine", || a.arbiter.held() == Some(Use::Cache));
        a.warm_once();
        wait_until("the warm-up waits", || a.arbiter.waiting(Use::Warm) > 0);
        let speech = a.synth(&Cancel::new(), "本番の文です。").unwrap();
        assert!(speech.waited >= Duration::from_millis(100), "it waited for the cache phrase: {:?}", speech.waited);
        assert!(cache.join().unwrap().is_ok());
        wait_until("the warm-up ran", || e.synths.lock().unwrap().len() == 3);
        assert_eq!(*e.synths.lock().unwrap(), ["長い文です。", "本番の文です。", WARM_TEXT]);
    }

    fn with_activity(a: &AivisEngine, now: Activity) -> Arc<Mutex<Activity>> {
        let now = Arc::new(Mutex::new(now));
        let n = now.clone();
        let _ = a.arbiter.activity.set(Box::new(move || *n.lock().unwrap()));
        now
    }

    /// A warm-up waits while whisper transcribes or a subagent works, and runs
    /// once they are done (Claude thinking is no reason to wait); one right
    /// after the engine synthesized is left out.
    #[test]
    fn a_warm_up_waits_for_whisper_and_subagents() {
        let e = engine(|_| 200, |_| Duration::ZERO);
        let a = AivisEngine::new(&e.srv.url, 1, 1.3);
        let now = with_activity(&a, Activity { transcribing: true, busy: true, ..Activity::default() });
        a.warm_once();
        std::thread::sleep(Duration::from_millis(300));
        *now.lock().unwrap() = Activity { agents: true, busy: true, ..Activity::default() };
        std::thread::sleep(Duration::from_millis(300));
        assert!(e.synths.lock().unwrap().is_empty(), "whisper, then a subagent, was at work");
        *now.lock().unwrap() = Activity { busy: true, ..Activity::default() };
        wait_until("warmed once they were done", || e.synths.lock().unwrap().len() == 1);
        assert_eq!(e.synths.lock().unwrap()[0], WARM_TEXT);

        a.warm_once();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(e.synths.lock().unwrap().len(), 1, "the engine has just synthesized");
    }

    /// A warm-up that waited `wait` in vain is left out.
    #[test]
    fn a_warm_up_is_left_out_after_wait() {
        let e = engine(|_| 200, |_| Duration::ZERO);
        let a = AivisEngine::tuned(&e.srv.url, |a| a.wait = Duration::from_millis(100));
        let now = with_activity(&a, Activity { transcribing: true, busy: true, ..Activity::default() });
        a.warm_once();
        // Whisper goes on well past `wait` and a poll or two
        std::thread::sleep(Duration::from_millis(1000));
        *now.lock().unwrap() = Activity::default();
        std::thread::sleep(Duration::from_millis(500));
        assert!(e.synths.lock().unwrap().is_empty());
    }

    /// docs/design.md's account of P1 gives the warm-up's numbers.
    #[test]
    fn the_design_doc_has_the_warm_up() {
        let p1 = include_str!("../../docs/design.md").split("## P1 で").nth(1).unwrap().split("\n## ").next().unwrap();
        for want in
            [format!("{} 秒たっても", WARM_WAIT.as_secs()), format!("{} 秒以内に合成していれば", WARM_EVERY.as_secs()), format!("温めの文は「{WARM_TEXT}」")]
        {
            assert!(p1.contains(&want), "the P1 section should say {want:?}");
        }
    }

    #[test]
    fn the_version_is_read() {
        let e = engine(|_| 200, |_| Duration::ZERO);
        assert_eq!(AivisEngine::new(&e.srv.url, 1, 1.0).version().as_deref(), Some(testing::VERSION));
        assert_eq!(AivisEngine::new("http://127.0.0.1:1", 1, 1.0).version(), None);
    }

    #[test]
    fn which_errors_mean_the_engine_is_gone() {
        assert!(!SynthError::Timeout.unavailable());
        assert!(!SynthError::Cancelled.unavailable());
        assert!(!SynthError::Http(422, "bad text".into()).unavailable());
        assert!(SynthError::Http(500, "down".into()).unavailable());
        assert!(SynthError::Net("connection refused".into()).unavailable());
    }

    #[test]
    fn growth_follows_the_pace() {
        let a = AivisEngine::new("http://127.0.0.1:1", 1, 1.0);
        assert_eq!(a.growth(), 2.0, "not known yet");
        for (pace, want) in [(0.25, 2.0), (0.8, 1.25), (1.5, 1.0)] {
            a.lock().pace = pace;
            assert!((a.growth() - want).abs() < 1e-9);
        }
    }

    #[test]
    fn two_timeouts_in_a_row_take_the_engine_down() {
        let a = AivisEngine::new("http://127.0.0.1:1", 1, 1.0);
        a.lock().closed = true; // no watcher thread
        a.note(Some(&SynthError::Timeout));
        assert!(!a.is_down());
        a.note(None);
        a.note(Some(&SynthError::Timeout));
        assert!(!a.is_down(), "a chunk in between resets the count");
        a.note(Some(&SynthError::Timeout));
        assert!(a.is_down());
        let b = AivisEngine::new("http://127.0.0.1:1", 1, 1.0);
        b.lock().closed = true;
        b.note(Some(&SynthError::Http(422, "rejected".into())));
        assert!(!b.is_down(), "a rejected chunk is about the chunk");
        b.note(Some(&SynthError::Net("refused".into())));
        assert!(b.is_down());
    }
}
