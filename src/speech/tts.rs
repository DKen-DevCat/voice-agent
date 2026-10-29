//! Speech: the reply is cut into chunks as it streams in and read aloud,
//! with AivisSpeech when its engine answers, else macOS `say`.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, mpsc};
use std::time::{Duration, Instant};

use regex::Regex;

use super::aivis::{AivisEngine, excerpt};
use super::phrases::Phrases;
use crate::audio::playback::{DEFAULT_RATE, Pcm, PcmPlayer, pause_after, wav_pcm};
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
/// than wait for the end of its sentence. Shorter is heard sooner (less to
/// wait for and to synthesize) but leaves less time to synthesize the next
/// chunk while it plays; 10 is a first guess for latency.jsonl to correct.
const FIRST_MIN: usize = 10;
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

/// Cuts text after each sentence end: the pieces speech goes by, and the
/// phrase cache too.
pub fn split_sentences(text: &str) -> Vec<String> {
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

/// A piece of speech waiting its turn: a sentence (or clause) for the
/// engine, or one the phrase cache has ready.
#[derive(Clone)]
struct Item {
    text: String,
    wav: Option<Vec<u8>>, // from the phrase cache
    turn: u64,            // the latency log's turn this is the reply to (0: voice-agent's own words)
    asked: Instant,       // when it was handed to speech
}

impl Item {
    fn new(text: &str, turn: u64) -> Item {
        Item { text: text.to_string(), wav: None, turn, asked: Instant::now() }
    }
}

/// Cuts the next synthesis request off the front of queue. Whole sentences
/// are merged up to limit characters, and a chunk well over limit is cut at
/// a comma. first (nothing is playing) keeps the chunk short so audio starts
/// sooner: short sentences merge only up to FIRST_MIN, and a longer one ends
/// at its first comma past FIRST_MIN characters. A cached phrase goes alone
/// and as it is, and the reply to another turn starts a chunk of its own.
fn take_chunk(queue: &mut VecDeque<Item>, first: bool, limit: usize) -> Item {
    let mut chunk = queue.pop_front().unwrap_or_else(|| Item::new("", 0));
    if chunk.wav.is_some() {
        return chunk;
    }
    let mut n = chunk.text.chars().count();
    while let Some(next) = queue.front() {
        if next.wav.is_some() || next.turn != chunk.turn || (first && n >= FIRST_MIN) {
            break;
        }
        let m = next.text.chars().count();
        if n + m > limit {
            break;
        }
        chunk.text = join_speech(&chunk.text, &next.text);
        n += m;
        queue.pop_front();
    }
    let at = if first {
        clause_cut(&chunk.text, FIRST_MIN, 0)
    } else if n > limit * 5 / 4 {
        clause_cut(&chunk.text, limit / 2, limit)
    } else {
        0
    };
    if at > 0 {
        let rs: Vec<char> = chunk.text.chars().collect();
        let rest: String = rs[at..].iter().collect();
        queue.push_front(Item { text: rest.trim().to_string(), ..chunk.clone() });
        chunk.text = rs[..at].iter().collect::<String>().trim().to_string();
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
    note: Option<Note>, // for the latency log; None for a chunk stopped with its run
}

/// Where a clip came from, for the latency log.
#[derive(Clone)]
struct Note {
    turn: u64,
    cached: bool,
    lead: bool, // the first chunk of its run
    asked: Instant,
    synth: Option<SynthTiming>,
}

/// A synthesis: when it asked for the engine, got it, and had the audio.
#[derive(Clone, Copy, Debug)]
pub struct SynthTiming {
    pub asked: Instant,
    pub got: Instant,
    pub done: Instant,
}

/// A clip that started to sound, or was skipped, as the latency log learns
/// of it.
#[derive(Clone, Debug)]
pub struct Sounded {
    pub text: String,
    pub turn: u64,    // the turn it is the reply to (see Item)
    pub cached: bool, // from the phrase cache
    pub say: bool,    // spoken by `say`
    pub first: bool,  // the first chunk of its run: nothing was ahead of it
    pub asked: Instant,
    pub synth: Option<SynthTiming>,
    /// Its first sample went to the speakers (for `say`, afplay and files:
    /// when they were started, told once they played); None: it was skipped
    /// (the engine or the player failed on it) and never sounded
    pub at: Option<Instant>,
}

impl Clip {
    fn sounded(&self, at: Option<Instant>) -> Option<Sounded> {
        let n = self.note.as_ref()?;
        let say = self.wav.is_none() && !self.text.is_empty();
        Some(Sounded { text: self.text.clone(), turn: n.turn, cached: n.cached, say, first: n.lead, asked: n.asked, synth: n.synth, at })
    }
}

enum Ev {
    Clip(Clip),
    /// A clip started to sound
    Sounded(Sounded),
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
    taken: usize,                      // chunks taken so far
    say: bool,                         // this run speaks with `say` (the engine was down)
    voiced: bool,                      // the engine made audio for this run: it never turns to `say`
    lost: bool,                        // the engine went away after that: what needs it is skipped
}

fn poke(wake: &(Mutex<bool>, Condvar)) {
    *wake.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
    wake.1.notify_all();
}

#[derive(Default)]
struct State {
    queue: VecDeque<Item>, // pieces not yet taken for synthesis
    cur: Option<Run>,      // None = silent
    enabled: bool,
    file_seq: u32,
    next_run: u64,
}

type PlayFn = Box<dyn Fn(&Cancel, &Clip) -> Result<(), String> + Send + Sync>;
type TapFn = Box<dyn Fn(&Sounded) + Send + Sync>;

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
    pub phrases: Option<Arc<Phrases>>,      // speech known in advance, ready to play
    pub tap: Option<TapFn>,                 // told when a clip starts to sound (the latency log)
    on_change: Box<dyn Fn() + Send + Sync>, // speaking started or stopped
    st: Mutex<State>,
}

enum Next {
    Take(Item, Plan),
    Wait,
    Over,
}

/// How a chunk taken off the queue is to be made into a clip.
#[derive(Clone, Copy)]
struct Plan {
    say: bool,  // it goes to `say`
    lost: bool, // the engine went away mid-run: skipped unless cached
    lead: bool, // the first chunk of its run
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
        Speaker {
            voice: voice.to_string(),
            rate,
            aivis,
            player,
            out_dir,
            play_fn: None,
            phrases: None,
            tap: None,
            on_change,
            st: Mutex::new(State { enabled, ..State::default() }),
        }
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

    /// Gets the voice model resident for the reply to what the user just
    /// said (once an utterance, when it goes to Claude).
    pub fn warm(&self) {
        if let Some(a) = &self.aivis
            && self.is_enabled()
        {
            a.warm_once();
        }
    }

    /// Has the phrase cache made for the engine's voice (when the engine is
    /// ready).
    pub fn build_phrases(&self, texts: Vec<String>) {
        if let (Some(a), Some(p)) = (&self.aivis, &self.phrases)
            && self.is_enabled()
        {
            p.build(a, texts);
        }
    }

    /// Says something of voice-agent's own (an approval, a notice).
    pub fn speak(self: &Arc<Self>, text: &str) {
        self.enqueue(text, 0);
    }

    /// Says a piece of Claude's reply to turn (the latency log's); false when
    /// nothing was queued (speech is off, or there were no words).
    pub fn speak_reply(self: &Arc<Self>, text: &str, turn: u64) -> bool {
        self.enqueue(text, turn)
    }

    /// Queues text sentence by sentence; a sentence the phrase cache has is
    /// played from there.
    fn enqueue(self: &Arc<Self>, text: &str, turn: u64) -> bool {
        let pieces: Vec<Item> = split_sentences(text)
            .into_iter()
            .map(|t| {
                let wav = self.phrases.as_ref().and_then(|p| p.get(&t));
                Item { wav, ..Item::new(&t, turn) }
            })
            .collect();
        let (wake, starting);
        {
            let mut st = self.lock();
            if !st.enabled || pieces.is_empty() {
                return false;
            }
            st.queue.extend(pieces);
            starting = st.cur.is_none();
            if starting {
                st.next_run += 1;
                let run =
                    Run { id: st.next_run, cancel: Cancel::new(), wake: Arc::default(), ahead: 0, last: 0, taken: 0, say: false, voiced: false, lost: false };
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
        true
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
                Next::Take(chunk, plan) => {
                    let clip = self.prepare(id, &cancel, &chunk, plan);
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
        let down = self.aivis.as_ref().is_none_or(|a| a.is_down());
        let needs_engine = self.aivis.is_none() || st.queue.front().is_some_and(|i| i.wav.is_none());
        if down && needs_engine && !r.say && !r.lost {
            // No switching voices mid-reply: a run the engine has made nothing
            // for yet goes on with `say` to its end, and one it has leaves the
            // rest of what needs the engine unheard. Cached audio needs no
            // engine (nor does it show that the engine is there): it plays on
            if r.voiced {
                let rest: String = st.queue.iter().filter(|i| i.wav.is_none()).map(|i| i.text.as_str()).collect();
                log(&format!("tts: engine lost mid-reply; not spoken: {:?}", excerpt(&rest)));
                r.lost = true;
            } else {
                r.say = true;
            }
        }
        if !st.queue.is_empty() && (r.ahead == 0 || (!r.say && r.ahead < MAX_AHEAD)) {
            let chunk = if r.say {
                // `say` gains nothing from preparing early and pauses at every
                // start, so it takes everything of the turn queued at once
                // (cached audio too: the voice does not change mid-reply)
                let first = st.queue.pop_front().unwrap_or_else(|| Item::new("", 0));
                let mut chunk = Item { wav: None, ..first };
                while let Some(p) = st.queue.front().filter(|p| p.turn == chunk.turn) {
                    chunk.text = join_speech(&chunk.text, &p.text);
                    st.queue.pop_front();
                }
                chunk
            } else {
                // Grow gradually, so that each chunk's synthesis (a fixed overhead
                // plus a share of its length) fits into the playback of the one
                // before it
                let first = r.ahead == 0;
                let growth = self.aivis.as_ref().map_or(2.0, |a| a.growth());
                let limit = if first { CHUNK_GROW } else { ((r.last as f64 * growth) as usize).clamp(CHUNK_GROW, CHUNK_MAX) };
                take_chunk(&mut st.queue, first, limit)
            };
            let plan = Plan { say: r.say, lost: r.lost, lead: r.taken == 0 };
            r.taken += 1;
            r.ahead += 1;
            r.last = chunk.text.chars().count();
            return Next::Take(chunk, plan);
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
    /// goes to `say`, unless the engine already made audio for the run; that
    /// chunk, and one the engine timed out on or rejected, is skipped, so the
    /// voice does not change mid-reply. A cached phrase needs no engine.
    fn prepare(self: &Arc<Self>, id: u64, cancel: &Cancel, chunk: &Item, plan: Plan) -> Clip {
        let note = Note { turn: chunk.turn, cached: false, lead: plan.lead, asked: chunk.asked, synth: None };
        let text = chunk.text.as_str();
        let to_say = Clip { text: text.to_string(), note: Some(note.clone()), ..Clip::default() };
        // Nothing to play, but the latency log learns of it
        let skipped = Clip { note: Some(note.clone()), ..Clip::default() };
        let Some(aivis) = self.aivis.as_ref().filter(|_| !plan.say) else { return to_say };
        if let Some(wav) = &chunk.wav {
            let pcm = self.player.as_ref().and_then(|_| wav_pcm(wav, pause_after(text)));
            return Clip { wav: Some(wav.clone()), pcm, text: text.to_string(), note: Some(Note { cached: true, ..note }) };
        }
        if plan.lost {
            return skipped;
        }
        let rate = self.player.as_ref().map_or(DEFAULT_RATE, |p| p.rate());
        let asked = Instant::now();
        let res = aivis.synth(cancel, text, rate);
        if cancel.is_cancelled() {
            return Clip::default();
        }
        aivis.note(res.as_ref().err());
        let voiced = self.voiced(id, res.is_ok());
        match res {
            Ok(s) => {
                let synth = SynthTiming { asked, got: asked + s.waited, done: Instant::now() };
                let pcm = self.player.as_ref().and_then(|_| wav_pcm(&s.wav, pause_after(text)));
                Clip { wav: Some(s.wav), pcm, text: text.to_string(), note: Some(Note { synth: Some(synth), ..note }) }
            }
            Err(e) if e.unavailable() && !voiced => to_say,
            Err(e) => {
                log(&format!("tts: skipped {:?}: {e}", excerpt(text)));
                skipped
            }
        }
    }

    /// Records that the engine made audio for the run now (got), and returns
    /// whether it had before.
    fn voiced(&self, id: u64, got: bool) -> bool {
        let mut st = self.lock();
        match st.cur.as_mut().filter(|r| r.id == id) {
            Some(r) => {
                let was = r.voiced;
                r.voiced = was || got;
                was
            }
            None => false,
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
                Ok(Ev::Sounded(s)) => {
                    if let Some(tap) = &self.tap {
                        tap(&s);
                    }
                }
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
            let (tx, started) = (heard.clone(), heard.clone());
            let note = c.sounded(None);
            // Called from the device's callback, with its first sample
            let on_start = move || {
                if let Some(mut s) = note {
                    s.at = Some(Instant::now());
                    let _ = started.send(Ev::Sounded(s));
                }
            };
            match player.enqueue(cancel, pcm, on_start, move || {
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
            // Told once it has played, as of its start: `say` and afplay do not
            // tell when their sound begins, and one that failed never sounded
            let at = Instant::now();
            let played = (c.wav.is_some() || !c.text.is_empty()) && {
                let res = match &s.play_fn {
                    Some(f) => f(&cancel, &c),
                    None => s.output(&cancel, &c),
                };
                if let Err(e) = &res
                    && !cancel.is_cancelled()
                {
                    log(&format!("tts: {e}"));
                }
                res.is_ok()
            };
            if !cancel.is_cancelled()
                && let Some(told) = c.sounded(played.then_some(at))
            {
                let _ = tx.send(Ev::Sounded(told));
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
    use crate::speech::aivis::testing::engine;

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

    fn items(texts: &[&str]) -> VecDeque<Item> {
        texts.iter().map(|t| Item::new(t, 1)).collect()
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
            // With FIRST_MIN 14 (P0) the third sentence joined in up to its
            // comma ("はい。わかりました。テストを実行して、", 19 characters);
            // with 10 the first chunk ends at 10
            (
                "first merges very short sentences",
                vec!["はい。", "わかりました。", "テストを実行して、結果を確認します。"],
                true,
                CHUNK_GROW,
                "はい。わかりました。",
                "テストを実行して、結果を確認します。",
            ),
            (
                "first ends at a comma past FIRST_MIN",
                vec![long],
                true,
                CHUNK_GROW,
                "失敗しているのは天気パッケージのキャッシュ読み込みと、",
                "地球儀の描画テストです。",
            ),
            // A comma 10 characters in ends the first chunk now (with 14 it did not)
            (
                "first ends at a comma at FIRST_MIN",
                vec!["テストを実行したら、結果を確認します。"],
                true,
                CHUNK_GROW,
                "テストを実行したら、",
                "結果を確認します。",
            ),
            ("first keeps an early comma", vec!["はい、わかりました。"], true, CHUNK_GROW, "はい、わかりました。", ""),
            ("later merges up to limit", vec!["一文目です。", "二文目です。", "三文目です。"], false, 14, "一文目です。二文目です。", "三文目です。"),
            ("later keeps a sentence slightly over limit", vec![long], false, 36, long, ""),
            ("later cuts a long sentence at a comma", vec![&commas], false, CHUNK_MAX, &cut_head, &cut_rest),
            ("no comma, no cut", vec![&no_comma], false, CHUNK_MAX, &no_comma, ""),
            ("English merges with a space", vec!["Done.", "Tests pass."], false, CHUNK_MAX, "Done. Tests pass.", ""),
        ];
        for (name, queue, first, limit, want, want_front) in cases {
            let mut q = items(&queue);
            let got = take_chunk(&mut q, first, limit);
            let front = q.front().map(|i| i.text.clone()).unwrap_or_default();
            assert_eq!((got.text.as_str(), front.as_str()), (want, want_front), "{name}");
        }
    }

    /// docs/design.md's account of P1 gives the chunk lengths the code has.
    #[test]
    fn the_design_doc_has_the_chunk_lengths() {
        let p1 = include_str!("../../docs/design.md").split("## P1 で").nth(1).unwrap().split("\n## ").next().unwrap();
        for want in [format!("FIRST_MIN を 14 から {FIRST_MIN} にした"), format!("最低 {CHUNK_GROW} 文字（CHUNK_GROW）")] {
            assert!(p1.contains(&want), "the P1 section should say {want:?}");
        }
        // The worst case it works out: the second chunk over the first
        assert!(p1.contains(&format!("その {:.1} 倍", CHUNK_GROW as f64 / FIRST_MIN as f64)));
    }

    /// A cached phrase is a chunk of its own: the sentences around it do not
    /// merge into it, and the rest of a cut keeps where it came from.
    #[test]
    fn cached_phrases_stand_alone() {
        let mut q = items(&["はい。", "確認です。", "次の文です。"]);
        q[1].wav = Some(test_wav(0.1, 24000));
        q[1].turn = 0;
        let a = take_chunk(&mut q, false, CHUNK_MAX);
        assert_eq!((a.text.as_str(), a.wav.is_some()), ("はい。", false), "stops before the cached one");
        let b = take_chunk(&mut q, false, CHUNK_MAX);
        assert_eq!((b.text.as_str(), b.wav.is_some(), b.turn), ("確認です。", true, 0));
        let mut q = items(&["テストを実行したら、結果を確認します。"]);
        let asked = q[0].asked;
        take_chunk(&mut q, true, CHUNK_GROW);
        assert!(q[0].turn == 1 && q[0].asked == asked);
    }

    /// The tail of one reply and the start of the next (typed while it was
    /// still spoken) are not merged: the latency log times the new reply from
    /// its own first chunk.
    #[test]
    fn replies_to_two_turns_do_not_merge() {
        let mut q = items(&["前の返事の最後です。", "新しい返事です。"]);
        q[1].turn = 2;
        let a = take_chunk(&mut q, false, CHUNK_MAX);
        assert_eq!((a.text.as_str(), a.turn), ("前の返事の最後です。", 1));
        assert_eq!(take_chunk(&mut q, false, CHUNK_MAX).turn, 2);
    }

    /// A speaker whose output is recorded instead of played, and what it
    /// tells the latency log.
    fn speaker(aivis: Option<Arc<AivisEngine>>) -> (Arc<Speaker>, Arc<Mutex<Vec<String>>>) {
        let (s, played, _) = tapped(aivis, None);
        (s, played)
    }

    type Taps = Arc<Mutex<Vec<Sounded>>>;

    fn tapped(aivis: Option<Arc<AivisEngine>>, phrases: Option<Arc<Phrases>>) -> (Arc<Speaker>, Arc<Mutex<Vec<String>>>, Taps) {
        let played = Arc::new(Mutex::new(Vec::new()));
        let taps = Arc::new(Mutex::new(Vec::new()));
        let (p, t) = (played.clone(), taps.clone());
        let mut s = Speaker::new("Kyoko", 260, aivis, None, None, true, Box::new(|| {}));
        s.play_fn = Some(Box::new(move |_, c| {
            p.lock().unwrap().push(format!("{}{}", if c.wav.is_some() { "aivis:" } else { "say:" }, c.text));
            std::thread::sleep(Duration::from_millis(c.text.chars().count() as u64 * 10));
            Ok(())
        }));
        s.tap = Some(Box::new(move |sounded| t.lock().unwrap().push(sounded.clone())));
        s.phrases = phrases;
        (Arc::new(s), played, taps)
    }

    fn wait_silent(s: &Speaker) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while s.speaking() {
            assert!(Instant::now() < deadline, "still speaking");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn ready(url: &str) -> Arc<AivisEngine> {
        AivisEngine::tuned(url, |a| a.timeout = Duration::from_millis(400))
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

    /// The sentences the phrase cache has play from there, at once and in
    /// their place; only the rest goes to the engine.
    #[test]
    fn cached_phrases_play_at_once() {
        let e = engine(|_| 200, |_| Duration::from_millis(30));
        let phrases = Phrases::new(std::env::temp_dir().join("va-tts-unused"));
        for t in ["確認です。", "実行していいですか？"] {
            phrases.insert(t, test_wav(0.05, 44100));
        }
        let (s, played, taps) = tapped(Some(ready(&e.srv.url)), Some(phrases));
        s.speak("確認です。リモートへ push します。実行していいですか？");
        wait_silent(&s);
        assert_eq!(*played.lock().unwrap(), ["aivis:確認です。", "aivis:リモートへ push します。", "aivis:実行していいですか？"]);
        assert_eq!(*e.synths.lock().unwrap(), ["リモートへ push します。"]);
        let taps = taps.lock().unwrap();
        let what: Vec<(bool, bool, bool)> = taps.iter().map(|t| (t.cached, t.first, t.synth.is_some())).collect();
        assert_eq!(what, [(true, true, false), (false, false, true), (true, false, false)]);
        assert!(taps.iter().all(|t| t.turn == 0 && !t.say));
        let soon = taps[0].at.unwrap() - taps[0].asked;
        assert!(soon < Duration::from_millis(100), "the cached phrase sounded after {soon:?}");
    }

    /// An engine that died unseen is found out by the first chunk it has to
    /// make. A cached phrase before it shows nothing of the engine, so the
    /// rest goes to `say` as it would have without the cache (the voice
    /// changes once rather than the rest going unheard).
    #[test]
    fn a_dead_engine_after_a_cached_phrase_hands_the_rest_to_say() {
        let phrases = Phrases::new(std::env::temp_dir().join("va-tts-unused"));
        phrases.insert(CODE_NOTICE, test_wav(0.05, 44100));
        let a = ready("http://127.0.0.1:1");
        let (s, played, _) = tapped(Some(a.clone()), Some(phrases));
        s.speak_reply(&format!("{CODE_NOTICE}残りの文です。"), 1);
        wait_silent(&s);
        assert_eq!(*played.lock().unwrap(), [format!("aivis:{CODE_NOTICE}"), "say:残りの文です。".to_string()]);
        assert!(a.is_down());
        a.stop();
    }

    /// While the engine is down, what the cache has plays on in its voice:
    /// an approval whose sentences are all cached is heard whole.
    #[test]
    fn cached_phrases_play_while_the_engine_is_down() {
        let phrases = Phrases::new(std::env::temp_dir().join("va-tts-unused"));
        for t in ["確認です。", "リモートへ push します。", "実行していいですか？"] {
            phrases.insert(t, test_wav(0.05, 44100));
        }
        let a = ready("http://127.0.0.1:1");
        a.stop(); // no watcher, so it stays down
        a.note(Some(&crate::speech::aivis::SynthError::Net("refused".into())));
        assert!(a.is_down());
        let (s, played, _) = tapped(Some(a), Some(phrases));
        s.speak("確認です。リモートへ push します。実行していいですか？");
        wait_silent(&s);
        assert_eq!(*played.lock().unwrap(), ["aivis:確認です。", "aivis:リモートへ push します。", "aivis:実行していいですか？"]);
    }

    /// Without the engine everything goes to `say`, cached phrases too (the
    /// voice does not change mid-reply).
    #[test]
    fn without_an_engine_say_takes_everything_at_once() {
        let phrases = Phrases::new(std::env::temp_dir().join("va-tts-unused"));
        phrases.insert("一文目。", test_wav(0.05, 44100));
        let (s, played, taps) = tapped(None, Some(phrases));
        s.speak("一文目。二文目。");
        wait_silent(&s);
        assert_eq!(*played.lock().unwrap(), ["say:一文目。二文目。"]);
        assert!(taps.lock().unwrap().iter().all(|t| t.say && !t.cached));
        // Muted ("speak": false): nothing is said
        let muted = Arc::new(Speaker::new("Kyoko", 260, None, None, None, false, Box::new(|| {})));
        muted.speak("三文目。");
        assert!(!muted.speaking());
    }

    /// The latency log learns which clips are the reply's and how their
    /// synthesis went.
    #[test]
    fn the_reply_is_told_apart() {
        let e = engine(|_| 200, |_| Duration::from_millis(20));
        let (s, _, taps) = tapped(Some(ready(&e.srv.url)), None);
        assert!(s.speak_reply("返事の一文目です。", 7));
        wait_silent(&s);
        s.speak("お知らせです。");
        wait_silent(&s);
        let taps = taps.lock().unwrap();
        assert_eq!(taps.iter().map(|t| (t.turn, t.first)).collect::<Vec<_>>(), [(7, true), (0, true)]);
        let syn = taps[0].synth.expect("synthesized");
        assert!(taps[0].asked <= syn.asked && syn.asked <= syn.got && syn.got < syn.done && syn.done <= taps[0].at.unwrap());
        assert!(syn.done - syn.got >= Duration::from_millis(20), "the engine took its time");
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

    /// The latency log learns of a chunk that never sounded: one the engine
    /// refused, and one `say` failed on (told once it has played, so a
    /// failure is known). A cached phrase that waited behind a skipped chunk
    /// does not count as the first of its run.
    #[test]
    fn what_never_sounded_is_told_too() {
        let e = engine(|text| if text.contains("だめ") { 422 } else { 200 }, |_| Duration::ZERO);
        let phrases = Phrases::new(std::env::temp_dir().join("va-tts-unused"));
        phrases.insert("確認です。", test_wav(0.05, 44100));
        let (s, _, taps) = tapped(Some(ready(&e.srv.url)), Some(phrases));
        s.speak_reply("これはだめな文です。確認です。", 3);
        wait_silent(&s);
        let told: Vec<(u64, bool, bool, bool, bool)> = taps.lock().unwrap().iter().map(|t| (t.turn, t.cached, t.say, t.first, t.at.is_some())).collect();
        assert_eq!(told, [(3, false, false, true, false), (3, true, false, false, true)]);

        let failing = Arc::new(Mutex::new(Vec::new()));
        let f = failing.clone();
        let mut s = Speaker::new("Kyoko", 260, None, None, None, true, Box::new(|| {}));
        s.play_fn = Some(Box::new(|_, _| Err("say: no such voice".into())));
        s.tap = Some(Box::new(move |t| f.lock().unwrap().push(t.clone())));
        let s = Arc::new(s);
        s.speak_reply("読めない文です。", 4);
        wait_silent(&s);
        let told: Vec<(u64, bool, bool)> = failing.lock().unwrap().iter().map(|t| (t.turn, t.say, t.at.is_some())).collect();
        assert_eq!(told, [(4, true, false)]);
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
        let clip = Clip { wav: Some(test_wav(0.01, 16000)), text: "テストです。".into(), ..Clip::default() };
        s.output(&Cancel::new(), &clip).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("speech-001.txt")).unwrap(), "テストです。");
        assert!(dir.join("speech-001.wav").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
