//! Text-to-speech through AivisSpeech Engine's VOICEVOX-compatible HTTP API.
//!
//! The engine start, the requests and the error classification are as in
//! edex-rs. acquire / keep_warm / warm_pass are to be rebuilt in P1; until
//! then the warm-up loop only asks the arbiter (see arbiter.rs) whether it
//! may run.

use std::io::{BufRead, BufReader};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, mpsc};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::arbiter::{AlwaysWarm, Arbiter};
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
/// How long the engine counts as warm after it last kept up with playback;
/// while a reply is expected it is warmed again this often.
const WARM_EVERY: Duration = Duration::from_secs(10);
/// How many more times the engine is warmed after a reply, for the question
/// that may follow it.
const WARM_LINGER: u32 = 6;
/// How many slow warm-ups in a row it takes to conclude that the engine is
/// as fast as it gets.
const WARM_TRIES: u32 = 5;
/// How often the keep-warm loop looks whether a reply is still expected.
const WARM_POLL: Duration = Duration::from_millis(200);
/// Synthesized to keep the voice model resident. It is long enough that a
/// warm engine is done before its audio would have played.
const WARM_TEXT: &str = "はい、音声の準備ができています。";

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
    warm_at: Option<Instant>,  // the engine was last seen warm: it spoke, or kept up with playback
    pace: f64,                 // synthesis time over audio time of the latest chunk (0 = not known)
    warming: bool,             // the keep-warm loop is running
    warm_want: bool,           // keep_warm was called since the loop last looked
    stdin: Option<ChildStdin>, // our launch of the engine, if any
}

/// When nothing answers at url, start runs the engine bundled with
/// AivisSpeech.app; until it is up and warm the speaker uses `say`.
pub struct AivisEngine {
    url: String,  // http://127.0.0.1:10101
    speaker: i64, // style ID (GET /speakers)
    speed: f64,
    pub timeout: Duration, // per chunk wait
    pub every: Duration,   // keep-warm interval
    /// Reports that a reply is on its way or being spoken (the user is
    /// talking, claude is working): the engine is kept warm meanwhile and for
    /// a while after. Unset = never.
    pub expected: OnceLock<Box<dyn Fn() -> bool + Send + Sync>>,
    /// Decides whether background warm-ups may use the CPU now
    pub arbiter: Box<dyn Arbiter>,
    /// The sample rate asked for (the output device's)
    pub rate: AtomicU32,
    st: Mutex<State>,
    busy: (Mutex<bool>, Condvar),
}

/// Holds the engine for one request.
struct Turn<'a>(&'a AivisEngine);

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        *self.0.busy.0.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.0.busy.1.notify_one();
    }
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
            expected: OnceLock::new(),
            arbiter: Box::new(AlwaysWarm),
            rate: AtomicU32::new(DEFAULT_RATE),
            st: Mutex::new(State::default()),
            busy: (Mutex::new(false), Condvar::new()),
        })
    }

    #[cfg(test)]
    pub fn with_timing(url: &str, timeout: Duration, every: Duration) -> Arc<AivisEngine> {
        let mut a = Arc::try_unwrap(AivisEngine::new(url, 1, 1.3)).unwrap_or_else(|_| unreachable!());
        a.timeout = timeout;
        a.every = every;
        Arc::new(a)
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
                            a.keep_warm();
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
        let _turn = self.acquire(&Cancel::new(), WARM_TIMEOUT)?;
        self.post(&format!("/initialize_speaker?speaker={}&skip_reinit=true", self.speaker), None, WARM_TIMEOUT)?;
        self.request(WARM_TEXT, WARM_TIMEOUT).map(|_| ())
    }

    /// Gets the voice model resident and keeps it so while a reply is
    /// expected, and a little longer for a follow-up. An engine left idle is
    /// paged out within seconds when memory is short, and is then several
    /// times slower than playback until it has synthesized a few times: so
    /// WARM_TEXT is synthesized until that goes faster than its audio plays,
    /// and again every `every`. Push-to-talk and sent text call this.
    pub fn keep_warm(self: &Arc<Self>) {
        {
            let mut st = self.lock();
            st.warm_want = true;
            if st.down || st.closed || st.warming {
                return;
            }
            st.warming = true;
        }
        let a = self.clone();
        std::thread::spawn(move || {
            let mut settling = false; // asked for, and not warm yet
            let mut until: Option<Instant> = None; // keep warm until then
            let mut slow = 0;
            loop {
                let expected = a.expected.get().is_some_and(|f| f());
                let wait;
                {
                    let mut st = a.lock();
                    if st.warm_want {
                        settling = true;
                        st.warm_want = false;
                    }
                    wait = warm_left(&st, a.every);
                    if expected {
                        until = Some(Instant::now() + a.every * WARM_LINGER);
                    }
                    let lingering = until.is_some_and(|u| Instant::now() < u);
                    if st.down || st.closed || !(settling || lingering) {
                        st.warming = false;
                        return;
                    }
                }
                if !wait.is_zero() || !a.arbiter.may_warm() {
                    if !wait.is_zero() {
                        settling = false;
                        slow = 0;
                    }
                    std::thread::sleep(if wait.is_zero() { WARM_POLL } else { wait.min(WARM_POLL) });
                    continue;
                }
                match a.warm_pass() {
                    Err(e) => {
                        a.note(Some(&e));
                        a.lock().warming = false;
                        return;
                    }
                    Ok((took, audio)) if audio.is_zero() || took < audio => {} // warm (or speech got there first)
                    Ok((took, audio)) => {
                        log(&format!("aivis: cold: {:.1}s for {:.1}s of audio", took.as_secs_f64(), audio.as_secs_f64()));
                        slow += 1;
                        if slow >= WARM_TRIES {
                            a.lock().warm_at = Some(Instant::now());
                        }
                    }
                }
            }
        });
    }

    /// Synthesizes WARM_TEXT unless speech has warmed the engine meanwhile
    /// (audio 0), and records the engine as warm when that took less time
    /// than the audio plays.
    fn warm_pass(&self) -> Result<(Duration, Duration), SynthError> {
        let _turn = self.acquire(&Cancel::new(), WARM_TIMEOUT)?;
        if !warm_left(&self.lock(), self.every).is_zero() {
            return Ok((Duration::ZERO, Duration::ZERO));
        }
        let t0 = Instant::now();
        let wav = self.request(WARM_TEXT, WARM_TIMEOUT)?;
        let took = t0.elapsed();
        let audio = Duration::from_secs_f64(wav_seconds(&wav));
        if took < audio || audio.is_zero() {
            self.lock().warm_at = Some(Instant::now());
        }
        Ok((took, audio))
    }

    pub fn stop(&self) {
        let mut st = self.lock();
        st.closed = true;
        st.stdin = None; // closing our end ends the engine we launched
    }

    /// Takes the engine for one request: requests go one at a time, so a
    /// chunk never competes with another for the CPU.
    fn acquire(&self, cancel: &Cancel, timeout: Duration) -> Result<Turn<'_>, SynthError> {
        let deadline = Instant::now() + timeout;
        let mut busy = self.busy.0.lock().unwrap_or_else(|e| e.into_inner());
        while *busy {
            if cancel.is_cancelled() {
                return Err(SynthError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(SynthError::Timeout);
            }
            busy = self.busy.1.wait_timeout(busy, Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner()).0;
        }
        *busy = true;
        Ok(Turn(self))
    }

    /// Returns WAV audio for one chunk of speech, or Timeout when the engine
    /// has not delivered it within two chunk timeouts.
    pub fn synth(self: &Arc<Self>, cancel: &Cancel, text: &str) -> Result<Vec<u8>, SynthError> {
        let _turn = self.acquire(cancel, Duration::from_secs(3600))?;
        let wait = self.timeout;
        let t0 = Instant::now();
        // The request runs on a thread of its own so that a stop does not have
        // to wait for it
        let (tx, rx) = mpsc::channel();
        let (a, t) = (self.clone(), text.to_string());
        std::thread::spawn(move || {
            let _ = tx.send(a.request(&t, wait * 2 + Duration::from_secs(1)));
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
            let waited = t0.elapsed();
            if waited >= wait * 2 {
                break Err(SynthError::Timeout);
            }
            if waited >= wait && !warned {
                warned = true;
                log(&format!("aivis: no audio after {wait:?} for {:?}; waiting once more", excerpt(text)));
            }
        };
        if let Ok(wav) = &res {
            // Speech keeps the engine warm by itself; recorded before the engine
            // is released, so that a warm-up waiting for it sees it
            let mut st = self.lock();
            st.warm_at = Some(Instant::now());
            let audio = wav_seconds(wav);
            if audio > 0.0 {
                st.pace = t0.elapsed().as_secs_f64() / audio;
            }
        }
        res
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

#[cfg(test)]
mod tests {
    use super::*;

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
