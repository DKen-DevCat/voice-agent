//! Speech: the reply is cut into chunks as it streams in and read aloud,
//! with AivisSpeech when its engine answers, else macOS `say`.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, mpsc};
use std::time::Duration;

use regex::Regex;

use super::aivis::{AivisEngine, excerpt};
use crate::audio::playback::{Pcm, PcmPlayer, pause_after, wav_pcm};
use crate::log::log;

/// Spoken in place of a fenced code block.
pub const CODE_NOTICE: &str = "コードは画面に表示しています。";

const SENTENCE_ENDS: &str = "。！？!?";
/// Stay with the sentence end before them
pub const CLOSERS: &str = "」』）)]\"'”’";

// Speech goes to AivisSpeech in chunks (lengths in characters): small at the
// start of a reply so audio begins soon, larger later so there are fewer
// requests.
/// How long a reply's first chunk must be before it may end at a comma rather
/// than wait for the end of its sentence.
const FIRST_MIN: usize = 14;
/// Caps the first chunk when it merges short sentences, and is the least the
/// following chunks may hold. Each later chunk may be longer than the one
/// before (up to CHUNK_MAX) by as much as the engine is faster than playback,
/// at most twice, so that its synthesis fits into the playback of what is
/// already queued.
const CHUNK_GROW: usize = 28;
const CHUNK_MAX: usize = 64;
/// How many chunks may be synthesized ahead of what is heard.
const MAX_AHEAD: usize = 3;

struct Patterns {
    rule_line: Regex,
    table_sep: Regex,
    heading: Regex,
    list_mark: Regex,
    md_link: Regex,
    bare_url: Regex,
    emphasis: Regex,
    code_fence: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        rule_line: Regex::new(r"^[-=*_]{3,}$").unwrap(),
        table_sep: Regex::new(r"^:?-+:?$").unwrap(),
        heading: Regex::new(r"^#{1,6}\s+").unwrap(),
        list_mark: Regex::new(r"^\s*(?:[-*+]|\d+[.)])\s+").unwrap(),
        md_link: Regex::new(r"!?\[([^\]]*)\]\([^)]*\)").unwrap(),
        bare_url: Regex::new(r"https?://\S+").unwrap(),
        emphasis: Regex::new("[*_~`>]").unwrap(),
        code_fence: Regex::new(r"^\s*```").unwrap(),
    })
}

/// Turns a markdown-ish line into something worth reading aloud ("" = skip).
fn speakable(line: &str) -> String {
    let p = patterns();
    let mut s = line.trim().to_string();
    if p.rule_line.is_match(&s) {
        return String::new();
    }
    if s.starts_with('|') {
        // A table row is read cell by cell; the |---| separator drops out
        let cells: Vec<&str> = s.trim_matches('|').split('|').map(str::trim).filter(|c| !c.is_empty() && !p.table_sep.is_match(c)).collect();
        s = cells.join("、");
    }
    let s = p.heading.replace(&s, "");
    let s = p.list_mark.replace(&s, "");
    let s = p.md_link.replace_all(&s, "$1");
    let s = p.bare_url.replace_all(&s, "リンク");
    let s = p.emphasis.replace_all(&s, "");
    let s = s.trim();
    if has_words(s) { s.to_string() } else { String::new() }
}

fn has_words(s: &str) -> bool {
    s.chars().any(char::is_alphanumeric)
}

pub fn is_comma(c: char) -> bool {
    c == '、' || c == '，'
}

/// Splits streamed text into sentences, announcing fenced code blocks
/// instead of reading them.
pub struct SentenceChunker {
    on_sentence: Box<dyn Fn(&str) + Send + Sync>,
    /// Reports that nothing is being spoken: the first clause of a reply then
    /// goes out at a comma instead of waiting for the end of its sentence
    eager: Option<Box<dyn Fn() -> bool + Send + Sync>>,
    line: String,
    line_spoken: bool, // part of the current line was already emitted
    in_code: bool,
}

impl SentenceChunker {
    pub fn new(on_sentence: Box<dyn Fn(&str) + Send + Sync>, eager: Option<Box<dyn Fn() -> bool + Send + Sync>>) -> SentenceChunker {
        SentenceChunker { on_sentence, eager, line: String::new(), line_spoken: false, in_code: false }
    }

    pub fn push(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                self.end_line();
                continue;
            }
            let prev = self.line.chars().next_back();
            self.line.push(ch);
            if self.in_code || self.fence_line() {
                continue;
            }
            if SENTENCE_ENDS.contains(ch) || (prev == Some('.') && ch.is_whitespace()) || self.clause_end(ch) {
                let piece = std::mem::take(&mut self.line);
                self.say(&piece);
                self.line_spoken = true;
            }
        }
    }

    fn clause_end(&self, ch: char) -> bool {
        is_comma(ch) && self.line.chars().count() >= FIRST_MIN && !self.line.trim().starts_with('|') && self.eager.as_ref().is_some_and(|eager| eager())
    }

    pub fn flush(&mut self) {
        self.end_line();
        self.in_code = false;
    }

    fn fence_line(&self) -> bool {
        !self.line_spoken && patterns().code_fence.is_match(&self.line)
    }

    fn end_line(&mut self) {
        if self.fence_line() {
            self.in_code = !self.in_code;
            if self.in_code {
                (self.on_sentence)(CODE_NOTICE);
            }
        } else if !self.in_code {
            let piece = std::mem::take(&mut self.line);
            self.say(&piece);
        }
        self.line.clear();
        self.line_spoken = false;
    }

    fn say(&self, piece: &str) {
        let s = speakable(piece);
        if !s.is_empty() {
            (self.on_sentence)(&s);
        }
    }
}

/// Cuts text after each sentence end.
fn split_sentences(text: &str) -> Vec<String> {
    let rs: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut add = |s: &[char]| {
        let s: String = s.iter().collect();
        let s = s.trim();
        if !s.is_empty() {
            out.push(s.to_string());
        }
    };
    let mut start = 0;
    let mut i = 0;
    while i < rs.len() {
        let end = SENTENCE_ENDS.contains(rs[i]) || (rs[i] == '.' && rs.get(i + 1).is_some_and(|c| c.is_whitespace()));
        if end {
            while rs.get(i + 1).is_some_and(|c| SENTENCE_ENDS.contains(*c) || CLOSERS.contains(*c)) {
                i += 1;
            }
            add(&rs[start..=i]);
            start = i + 1;
        }
        i += 1;
    }
    add(&rs[start.min(rs.len())..]);
    out
}

/// Puts two pieces of speech together, with a space only between words of a
/// language that uses them.
fn join_speech(a: &str, b: &str) -> String {
    match (a.chars().next_back(), b.chars().next()) {
        (Some(x), Some(y)) if x.is_ascii() && y.is_ascii() => format!("{a} {b}"),
        _ => format!("{a}{b}"),
    }
}

/// Cuts the next synthesis request off the front of queue. Whole sentences
/// are merged up to limit characters, and a chunk well over limit is cut at
/// a comma. first (nothing is playing) keeps the chunk short so audio starts
/// sooner: short sentences merge only up to FIRST_MIN, and a longer one ends
/// at its first comma past FIRST_MIN characters.
fn take_chunk(queue: &mut VecDeque<String>, first: bool, limit: usize) -> String {
    let mut chunk = queue.pop_front().unwrap_or_default();
    let mut n = chunk.chars().count();
    while let Some(next) = queue.front() {
        if first && n >= FIRST_MIN {
            break;
        }
        let m = next.chars().count();
        if n + m > limit {
            break;
        }
        chunk = join_speech(&chunk, next);
        n += m;
        queue.pop_front();
    }
    let at = if first {
        clause_cut(&chunk, FIRST_MIN, 0)
    } else if n > limit * 5 / 4 {
        clause_cut(&chunk, limit / 2, limit)
    } else {
        0
    };
    if at > 0 {
        let rs: Vec<char> = chunk.chars().collect();
        let rest: String = rs[at..].iter().collect();
        queue.push_front(rest.trim().to_string());
        chunk = rs[..at].iter().collect::<String>().trim().to_string();
    }
    chunk
}

/// Where to cut s, as a character index just past a comma (0 = don't): with
/// upto 0 the first comma at or past from; otherwise the last one between
/// from and upto, else the first one past upto. Nothing is cut that would
/// leave no words behind.
fn clause_cut(s: &str, from: usize, upto: usize) -> usize {
    let rs: Vec<char> = s.chars().collect();
    let mut best = 0;
    for (i, &r) in rs.iter().enumerate() {
        let p = i + 1;
        let comma = is_comma(r) || (r == ',' && rs.get(p) == Some(&' '));
        if !comma || p < from || !rs[p..].iter().any(|c| c.is_alphanumeric()) {
            continue;
        }
        if upto > 0 && p <= upto {
            best = p;
            continue;
        }
        if best == 0 {
            best = p;
        }
        break;
    }
    best
}

/// Stops a run of speech: what is playing and any synthesis in flight.
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Cancel {
        Cancel::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// One chunk ready to play: AivisSpeech audio of text, text for `say` (no
/// audio), or nothing (a chunk that was skipped).
#[derive(Clone, Default)]
pub struct Clip {
    pub wav: Option<Vec<u8>>,
    pub pcm: Option<Pcm>, // wav's samples plus a pause, for gapless playback
    pub text: String,
}

enum Ev {
    Clip(Clip),
    /// The oldest clip playing has been heard
    Heard,
    /// The producer has nothing more
    End,
}

/// One stretch of speech; cancel stops it.
struct Run {
    id: u64,
    cancel: Cancel,
    wake: Arc<(Mutex<bool>, Condvar)>, // text was queued or a clip was heard
    ahead: usize,                      // chunks taken for synthesis and not yet heard
    last: usize,                       // characters in the latest chunk
    say: bool,                         // this run speaks with `say` (the engine was down)
    voiced: bool,                      // this run has AivisSpeech audio: it never turns to `say`
}

fn poke(wake: &(Mutex<bool>, Condvar)) {
    *wake.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
    wake.1.notify_all();
}

#[derive(Default)]
struct State {
    queue: VecDeque<String>, // sentences (or clauses) not yet taken for synthesis
    cur: Option<Run>,        // None = silent
    enabled: bool,
    file_seq: u32,
    next_run: u64,
}

type PlayFn = Box<dyn Fn(&Cancel, &Clip) -> Result<(), String> + Send + Sync>;

/// Text-to-speech fed sentence by sentence while the reply is still
/// streaming in.
///
/// A run (from the first sentence until everything queued has been heard) is
/// two threads: produce takes chunks off the queue and synthesizes them one
/// at a time, up to MAX_AHEAD ahead of playback; consume plays them in order.
pub struct Speaker {
    voice: String,
    rate: i64,
    pub aivis: Option<Arc<AivisEngine>>,    // None = `say` only
    pub player: Option<Arc<PcmPlayer>>,     // gapless playback of AivisSpeech audio; None = afplay
    out_dir: Option<PathBuf>,               // write audio files instead of playing (tests)
    pub play_fn: Option<PlayFn>,            // tests: replaces output
    on_change: Box<dyn Fn() + Send + Sync>, // speaking started or stopped
    st: Mutex<State>,
}

enum Next {
    Take(String, bool),
    Wait,
    Over,
}

impl Speaker {
    pub fn new(
        voice: &str,
        rate: i64,
        aivis: Option<Arc<AivisEngine>>,
        player: Option<Arc<PcmPlayer>>,
        out_dir: Option<PathBuf>,
        enabled: bool,
        on_change: Box<dyn Fn() + Send + Sync>,
    ) -> Speaker {
        Speaker { voice: voice.to_string(), rate, aivis, player, out_dir, play_fn: None, on_change, st: Mutex::new(State { enabled, ..State::default() }) }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn speaking(&self) -> bool {
        self.lock().cur.is_some()
    }

    pub fn is_enabled(&self) -> bool {
        self.lock().enabled
    }

    /// Tells the chunker that nothing is being spoken, so the first clause of
    /// a reply is worth synthesizing before its sentence is complete.
    pub fn eager_first(&self) -> bool {
        let idle = {
            let st = self.lock();
            st.cur.is_none() && st.enabled
        };
        idle && self.aivis.as_ref().is_some_and(|a| !a.is_down())
    }

    /// Gets the voice model resident ahead of a reply and keeps it so until
    /// the reply is there (push-to-talk, text sent).
    pub fn warm(&self) {
        if let Some(a) = &self.aivis
            && self.is_enabled()
        {
            a.keep_warm();
        }
    }

    pub fn speak(self: &Arc<Self>, text: &str) {
        let pieces = split_sentences(text);
        let (wake, starting);
        {
            let mut st = self.lock();
            if !st.enabled || pieces.is_empty() {
                return;
            }
            st.queue.extend(pieces);
            starting = st.cur.is_none();
            if starting {
                st.next_run += 1;
                let run = Run { id: st.next_run, cancel: Cancel::new(), wake: Arc::default(), ahead: 0, last: 0, say: false, voiced: false };
                let (s, id, cancel, w) = (self.clone(), run.id, run.cancel.clone(), run.wake.clone());
                st.cur = Some(run);
                std::thread::spawn(move || s.produce(id, cancel, w));
            }
            wake = st.cur.as_ref().map(|r| r.wake.clone());
        }
        if let Some(w) = wake {
            poke(&w);
        }
        if starting {
            (self.on_change)();
        }
    }

    /// Synthesizes the queue chunk by chunk until the run is over.
    fn produce(self: Arc<Self>, id: u64, cancel: Cancel, wake: Arc<(Mutex<bool>, Condvar)>) {
        let (tx, rx) = mpsc::channel();
        let (s, c, w, heard) = (self.clone(), cancel.clone(), wake.clone(), tx.clone());
        std::thread::spawn(move || s.consume(id, c, w, rx, heard));
        loop {
            match self.next(id) {
                Next::Over => break,
                Next::Wait => {
                    let mut woken = wake.0.lock().unwrap_or_else(|e| e.into_inner());
                    while !*woken && !cancel.is_cancelled() {
                        woken = wake.1.wait_timeout(woken, Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner()).0;
                    }
                    *woken = false;
                    if cancel.is_cancelled() {
                        break;
                    }
                }
                Next::Take(text, say) => {
                    let clip = self.prepare(id, &cancel, &text, say);
                    if cancel.is_cancelled() || tx.send(Ev::Clip(clip)).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = tx.send(Ev::End);
    }

    /// Takes the next chunk off the queue (Wait = wait for text or for
    /// playback to catch up). Over when the run is over: stopped, or
    /// everything queued has been heard.
    fn next(self: &Arc<Self>, id: u64) -> Next {
        let mut guard = self.lock();
        let st = &mut *guard;
        let Some(r) = st.cur.as_mut().filter(|r| r.id == id) else { return Next::Over };
        if !r.say && self.aivis.as_ref().is_none_or(|a| a.is_down()) {
            // No switching voices mid-reply: a run that has not spoken yet stays
            // `say` to its end, and one that lost the engine after speaking with
            // it leaves the rest unheard
            if r.voiced {
                if !st.queue.is_empty() {
                    let rest: String = st.queue.iter().map(String::as_str).collect();
                    log(&format!("tts: engine lost mid-reply; not spoken: {:?}", excerpt(&rest)));
                }
                st.queue.clear();
            } else {
                r.say = true;
            }
        }
        if !st.queue.is_empty() && (r.ahead == 0 || (!r.say && r.ahead < MAX_AHEAD)) {
            let text = if r.say {
                // `say` gains nothing from preparing early and pauses at every
                // start, so it takes everything queued at once
                st.queue.drain(..).fold(String::new(), |acc, p| join_speech(&acc, &p))
            } else {
                // Grow gradually, so that each chunk's synthesis (a fixed overhead
                // plus a share of its length) fits into the playback of the one
                // before it
                let first = r.ahead == 0;
                let growth = self.aivis.as_ref().map_or(2.0, |a| a.growth());
                let limit = if first { CHUNK_GROW } else { ((r.last as f64 * growth) as usize).clamp(CHUNK_GROW, CHUNK_MAX) };
                take_chunk(&mut st.queue, first, limit)
            };
            r.ahead += 1;
            r.last = text.chars().count();
            return Next::Take(text, r.say);
        }
        if !st.queue.is_empty() || r.ahead > 0 {
            return Next::Wait;
        }
        let run = st.cur.take();
        drop(guard);
        if let Some(run) = run {
            run.cancel.cancel();
        }
        if let Some(p) = &self.player {
            p.idle();
        }
        (self.on_change)();
        Next::Over
    }

    /// Turns one chunk into a clip. When AivisSpeech is not there the chunk
    /// goes to `say`, unless the run already speaks with AivisSpeech; that
    /// chunk, and one the engine timed out on or rejected, is skipped, so the
    /// voice does not change mid-reply.
    fn prepare(self: &Arc<Self>, id: u64, cancel: &Cancel, text: &str, say: bool) -> Clip {
        let Some(aivis) = self.aivis.as_ref().filter(|_| !say) else {
            return Clip { text: text.to_string(), ..Clip::default() };
        };
        if let Some(p) = &self.player {
            aivis.rate.store(p.rate(), Ordering::SeqCst);
        }
        let res = aivis.synth(cancel, text);
        if cancel.is_cancelled() {
            return Clip::default();
        }
        aivis.note(res.as_ref().err());
        let voiced = {
            let mut st = self.lock();
            match st.cur.as_mut().filter(|r| r.id == id) {
                Some(r) => {
                    let was = r.voiced;
                    r.voiced = was || res.is_ok();
                    was
                }
                None => false,
            }
        };
        match res {
            Ok(wav) => {
                let pcm = self.player.as_ref().and_then(|_| wav_pcm(&wav, pause_after(text)));
                Clip { wav: Some(wav), pcm, text: text.to_string() }
            }
            Err(e) if e.unavailable() && !voiced => Clip { text: text.to_string(), ..Clip::default() },
            Err(e) => {
                log(&format!("tts: skipped {:?}: {e}", excerpt(text)));
                Clip::default()
            }
        }
    }

    /// Plays the run's clips in order. AivisSpeech audio goes to the gapless
    /// player, where the next clip is queued while the current one plays;
    /// anything else (afplay, `say`, files) plays alone.
    fn consume(self: Arc<Self>, id: u64, cancel: Cancel, wake: Arc<(Mutex<bool>, Condvar)>, rx: mpsc::Receiver<Ev>, heard: mpsc::Sender<Ev>) {
        let mut playing: VecDeque<bool> = VecDeque::new(); // oldest first; true = on the gapless player
        let mut held: VecDeque<Clip> = VecDeque::new(); // received, waiting for their turn
        let mut ended = false;
        loop {
            if cancel.is_cancelled() {
                return;
            }
            while let Some(c) = held.front() {
                if !self.start(c, &cancel, &mut playing, &heard) {
                    break;
                }
                held.pop_front();
            }
            if ended && held.is_empty() && playing.is_empty() {
                return;
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Ev::Clip(c)) => held.push_back(c),
                Ok(Ev::Heard) => {
                    playing.pop_front();
                    if let Some(r) = self.lock().cur.as_mut().filter(|r| r.id == id) {
                        r.ahead = r.ahead.saturating_sub(1);
                    }
                    poke(&wake);
                }
                Ok(Ev::End) => ended = true,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Starts a clip if it is its turn.
    fn start(self: &Arc<Self>, c: &Clip, cancel: &Cancel, playing: &mut VecDeque<bool>, heard: &mpsc::Sender<Ev>) -> bool {
        if let (Some(pcm), Some(player)) = (&c.pcm, self.player.as_ref().filter(|p| p.usable())) {
            if playing.len() > 1 || playing.front() == Some(&false) {
                return false;
            }
            let tx = heard.clone();
            match player.enqueue(cancel, pcm, move || {
                let _ = tx.send(Ev::Heard);
            }) {
                Ok(()) => {
                    playing.push_back(true);
                    return true;
                }
                Err(e) => log(&format!("tts: speakers: {e}; using afplay")),
            }
        }
        if !playing.is_empty() {
            return false;
        }
        playing.push_back(false);
        let (s, c, cancel, tx) = (self.clone(), c.clone(), cancel.clone(), heard.clone());
        std::thread::spawn(move || {
            if c.wav.is_some() || !c.text.is_empty() {
                let res = match &s.play_fn {
                    Some(f) => f(&cancel, &c),
                    None => s.output(&cancel, &c),
                };
                if let Err(e) = res
                    && !cancel.is_cancelled()
                {
                    log(&format!("tts: {e}"));
                }
            }
            let _ = tx.send(Ev::Heard);
        });
        true
    }

    /// Plays one clip that is not for the gapless player.
    fn output(&self, cancel: &Cancel, c: &Clip) -> Result<(), String> {
        let path = self.out_dir.as_ref().map(|dir| {
            let mut st = self.lock();
            st.file_seq += 1;
            dir.join(format!("speech-{:03}.{}", st.file_seq, if c.wav.is_some() { "wav" } else { "aiff" }))
        });
        if let Some(p) = &path {
            // What was said, beside the audio, so a test can read it
            let _ = std::fs::write(p.with_extension("txt"), &c.text);
        }
        let Some(wav) = &c.wav else {
            let mut cmd = Command::new("say");
            cmd.args(["-v", &self.voice, "-r", &self.rate.to_string()]);
            if let Some(p) = &path {
                cmd.arg("-o").arg(p);
            }
            return run_cancellable(cmd, Some(c.text.as_bytes()), cancel);
        };
        if let Some(p) = path {
            return std::fs::write(p, wav).map_err(|e| e.to_string());
        }
        // afplay only reads files
        let tmp = std::env::temp_dir().join(format!("voice-agent-{}-{}.wav", std::process::id(), crate::util::unix_millis()));
        std::fs::write(&tmp, wav).map_err(|e| e.to_string())?;
        let mut cmd = Command::new("afplay");
        cmd.arg(&tmp);
        let res = run_cancellable(cmd, None, cancel);
        let _ = std::fs::remove_file(&tmp);
        res
    }

    pub fn stop(self: &Arc<Self>) {
        let run = {
            let mut st = self.lock();
            st.queue.clear();
            st.cur.take()
        };
        if let Some(run) = run {
            run.cancel.cancel(); // kills what is playing and any synthesis in flight
            poke(&run.wake);
            if let Some(p) = &self.player {
                p.drop_queued();
            }
            (self.on_change)();
        }
    }
}

/// Runs a command to its end, killing it when cancel is set.
fn run_cancellable(mut cmd: Command, stdin: Option<&[u8]>, cancel: &Cancel) -> Result<(), String> {
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let _ = pipe.write_all(data);
    }
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("{:?}: {status}", cmd.get_program())),
            Ok(None) => {}
            Err(e) => return Err(e.to_string()),
        }
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::wav::tests::test_wav;
    use crate::net::testing::{Request, serve};
    use serde_json::Value;
    use std::time::Instant;

    fn chunker(eager: Option<Box<dyn Fn() -> bool + Send + Sync>>) -> (SentenceChunker, Arc<Mutex<Vec<String>>>) {
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        (SentenceChunker::new(Box::new(move |s| g.lock().unwrap().push(s.to_string())), eager), got)
    }

    #[test]
    fn sentences_from_a_stream() {
        let (mut c, got) = chunker(None);
        for piece in ["確認します。", "結果は", "**3件**です！\n```sh\nls -la\n```\n", "| 名前 | 件数 |\n|---|:-:|\n| a | 3 |\n", "- 以上\n"]
        {
            c.push(piece);
        }
        c.flush();
        assert_eq!(*got.lock().unwrap(), ["確認します。", "結果は3件です！", CODE_NOTICE, "名前、件数", "a、3", "以上"]);
    }

    #[test]
    fn eager_first_clause() {
        let idle = Arc::new(AtomicBool::new(true));
        let i = idle.clone();
        let (mut c, got) = chunker(Some(Box::new(move || i.load(Ordering::SeqCst))));
        for ch in "失敗しているのは天気パッケージのキャッシュ読み込みと、地球儀の描画テストです。原因は、たぶんタイムゾーンです。".chars()
        {
            c.push(&ch.to_string());
            if !got.lock().unwrap().is_empty() {
                idle.store(false, Ordering::SeqCst);
            }
        }
        c.flush();
        assert_eq!(
            *got.lock().unwrap(),
            ["失敗しているのは天気パッケージのキャッシュ読み込みと、", "地球儀の描画テストです。", "原因は、たぶんタイムゾーンです。"]
        );
    }

    #[test]
    fn split() {
        for (text, want) in [
            ("確認です。危険な操作です。実行していいですか？", vec!["確認です。", "危険な操作です。", "実行していいですか？"]),
            ("「はい。」と言いました！？ 次へ", vec!["「はい。」", "と言いました！？", "次へ"]),
            ("Done. Next step", vec!["Done.", "Next step"]),
            ("  ", vec![]),
        ] {
            assert_eq!(split_sentences(text), want, "{text:?}");
        }
    }

    #[test]
    fn chunks() {
        let long = "失敗しているのは天気パッケージのキャッシュ読み込みと、地球儀の描画テストです。";
        let commas = format!("{}、{}、{}。", "あ".repeat(30), "い".repeat(30), "う".repeat(30));
        let no_comma = format!("{}。", "あ".repeat(100));
        let cut_head = format!("{}、{}、", "あ".repeat(30), "い".repeat(30));
        let cut_rest = format!("{}。", "う".repeat(30));
        // (what is tested, the queue, first, limit, the chunk, what is left at the front)
        type Case<'a> = (&'a str, Vec<&'a str>, bool, usize, &'a str, &'a str);
        let cases: Vec<Case> = vec![
            (
                "first stays short",
                vec!["確認しました。", "テストは全部で十二件あって、そのうち二件が失敗しています。"],
                true,
                CHUNK_GROW,
                "確認しました。",
                "テストは全部で十二件あって、そのうち二件が失敗しています。",
            ),
            (
                "first merges very short sentences",
                vec!["はい。", "わかりました。", "テストを実行して、結果を確認します。"],
                true,
                CHUNK_GROW,
                "はい。わかりました。テストを実行して、",
                "結果を確認します。",
            ),
            (
                "first ends at a comma past FIRST_MIN",
                vec![long],
                true,
                CHUNK_GROW,
                "失敗しているのは天気パッケージのキャッシュ読み込みと、",
                "地球儀の描画テストです。",
            ),
            ("first keeps an early comma", vec!["はい、わかりました。"], true, CHUNK_GROW, "はい、わかりました。", ""),
            ("later merges up to limit", vec!["一文目です。", "二文目です。", "三文目です。"], false, 14, "一文目です。二文目です。", "三文目です。"),
            ("later keeps a sentence slightly over limit", vec![long], false, 36, long, ""),
            ("later cuts a long sentence at a comma", vec![&commas], false, CHUNK_MAX, &cut_head, &cut_rest),
            ("no comma, no cut", vec![&no_comma], false, CHUNK_MAX, &no_comma, ""),
            ("English merges with a space", vec!["Done.", "Tests pass."], false, CHUNK_MAX, "Done. Tests pass.", ""),
        ];
        for (name, queue, first, limit, want, want_front) in cases {
            let mut q: VecDeque<String> = queue.iter().map(|s| s.to_string()).collect();
            let got = take_chunk(&mut q, first, limit);
            let front = q.front().cloned().unwrap_or_default();
            assert_eq!((got.as_str(), front.as_str()), (want, want_front), "{name}");
        }
    }

    /// An AivisSpeech stand-in: the audio of a text is 10 ms per character
    /// and takes delay(text) to synthesize.
    struct Engine {
        synths: Arc<Mutex<Vec<String>>>,
        bodies: Arc<Mutex<Vec<Value>>>,
        srv: crate::net::testing::Server,
    }

    fn engine(status: impl Fn(&str) -> u16 + Send + Sync + 'static, delay: impl Fn(&str) -> Duration + Send + Sync + 'static) -> Engine {
        let synths = Arc::new(Mutex::new(Vec::new()));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let (s, b) = (synths.clone(), bodies.clone());
        let srv = serve(move |r: &Request| {
            let path = r.url.split('?').next().unwrap_or("");
            match path {
                "/version" => (200, b"\"1.0\"".to_vec()),
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

    /// A speaker whose output is recorded instead of played.
    fn speaker(aivis: Option<Arc<AivisEngine>>) -> (Arc<Speaker>, Arc<Mutex<Vec<String>>>) {
        let played = Arc::new(Mutex::new(Vec::new()));
        let p = played.clone();
        let mut s = Speaker::new("Kyoko", 260, aivis, None, None, true, Box::new(|| {}));
        s.play_fn = Some(Box::new(move |_, c| {
            p.lock().unwrap().push(format!("{}{}", if c.wav.is_some() { "aivis:" } else { "say:" }, c.text));
            std::thread::sleep(Duration::from_millis(c.text.chars().count() as u64 * 10));
            Ok(())
        }));
        (Arc::new(s), played)
    }

    fn wait_silent(s: &Speaker) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while s.speaking() {
            assert!(Instant::now() < deadline, "still speaking");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn ready(url: &str) -> Arc<AivisEngine> {
        AivisEngine::with_timing(url, Duration::from_millis(400), Duration::from_secs(10))
    }

    #[test]
    fn speaks_through_the_engine_in_order() {
        let e = engine(|_| 200, |_| Duration::from_millis(5));
        let (s, played) = speaker(Some(ready(&e.srv.url)));
        s.speak("一文目です。二文目はもう少し長い文になっています。");
        s.speak("三文目です。");
        wait_silent(&s);
        let heard: String = played.lock().unwrap().iter().map(|p| p.strip_prefix("aivis:").expect("spoken by the engine")).collect();
        assert_eq!(heard, "一文目です。二文目はもう少し長い文になっています。三文目です。");
        let body = e.bodies.lock().unwrap()[0].clone();
        assert_eq!(body["speedScale"], 1.3);
        assert_eq!(body["prePhonemeLength"], 0.0);
        assert_eq!(body["outputSamplingRate"], 44100);
        assert_eq!(body["outputStereo"], false);
        assert!(!e.synths.lock().unwrap().is_empty());
    }

    #[test]
    fn without_an_engine_say_takes_everything_at_once() {
        let (s, played) = speaker(None);
        s.speak("一文目。二文目。");
        wait_silent(&s);
        assert_eq!(*played.lock().unwrap(), ["say:一文目。二文目。"]);
        // Muted ("speak": false): nothing is said
        let muted = Arc::new(Speaker::new("Kyoko", 260, None, None, None, false, Box::new(|| {})));
        muted.speak("三文目。");
        assert!(!muted.speaking());
    }

    #[test]
    fn an_engine_that_is_gone_hands_the_reply_to_say() {
        let a = ready("http://127.0.0.1:1");
        let (s, played) = speaker(Some(a.clone()));
        s.speak("エンジンがいません。");
        wait_silent(&s);
        assert_eq!(*played.lock().unwrap(), ["say:エンジンがいません。"]);
        assert!(a.is_down());
        a.stop();
    }

    #[test]
    fn a_rejected_chunk_is_skipped_not_said() {
        let e = engine(|text| if text.contains("だめ") { 422 } else { 200 }, |_| Duration::ZERO);
        let a = ready(&e.srv.url);
        let (s, played) = speaker(Some(a.clone()));
        s.speak("最初の文はちゃんと読めます。");
        s.speak("これはだめな文です。");
        s.speak("最後の文も読めます。");
        wait_silent(&s);
        let p = played.lock().unwrap().clone();
        assert!(p.iter().all(|x| x.starts_with("aivis:")), "the voice must not change mid-reply: {p:?}");
        assert!(!p.concat().contains("だめ"));
        assert!(!a.is_down());
    }

    #[test]
    fn stop_silences_at_once() {
        let e = engine(|_| 200, |_| Duration::from_millis(5));
        let (s, played) = speaker(Some(ready(&e.srv.url)));
        s.speak(&"これは長い返事の一部です。".repeat(20));
        std::thread::sleep(Duration::from_millis(150));
        assert!(s.speaking());
        s.stop();
        assert!(!s.speaking());
        let n = played.lock().unwrap().len();
        std::thread::sleep(Duration::from_millis(300));
        assert!(played.lock().unwrap().len() <= n + 1, "nothing more starts after a stop");
        s.speak("もう一度。");
        wait_silent(&s);
        assert!(played.lock().unwrap().last().unwrap().ends_with("もう一度。"));
    }

    #[test]
    fn writes_files_with_the_text_beside_them() {
        let dir = std::env::temp_dir().join(format!("va-tts-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let s = Speaker::new("Kyoko", 260, None, None, Some(dir.clone()), true, Box::new(|| {}));
        let clip = Clip { wav: Some(test_wav(0.01, 16000)), pcm: None, text: "テストです。".into() };
        s.output(&Cancel::new(), &clip).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("speech-001.txt")).unwrap(), "テストです。");
        assert!(dir.join("speech-001.wav").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
