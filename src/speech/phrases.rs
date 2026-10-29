//! Speech known in advance (the approval question, the replies to spoken
//! commands, the error notice), synthesized while nothing else goes on and
//! kept on disk, so that it plays at once instead of waiting for the engine.
//!
//! A clip is good for one voice: the engine's version, the style, the speed,
//! the synthesis settings and the sample rate it was made at go into its file
//! name, and the files of other voices are removed when the cache is built.
//! Clips are made at AivisSpeech's own rate whatever the output device, and
//! the player converts them for a device at another rate, so another device
//! does not make the cache anew.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::aivis::AivisEngine;
use super::arbiter::Use;
use super::tts::Cancel;
use crate::audio::wav::parse_wav;
use crate::log::log;
use crate::util::{private_dir, write_private};

/// How often a build that waits for a quiet moment looks again.
const POLL: Duration = Duration::from_millis(250);
/// The rate clips are made at: AivisSpeech's own (its engine_manifest.json
/// says default_sampling_rate 44100), which it makes without converting.
pub const RATE: u32 = 44100;
/// A phrase the engine failed on is tried again after this, TRIES times in
/// all: a timeout on an engine paged out, say, need not last.
const RETRY_AFTER: Duration = Duration::from_secs(60);
const TRIES: usize = 3;

#[derive(Default)]
struct State {
    voice: String,                   // what the clips were made with (see build_now)
    clips: HashMap<String, Vec<u8>>, // text -> WAV
}

pub struct Phrases {
    dir: PathBuf,
    st: Mutex<State>,
    building: AtomicBool,
    cancel: Cancel,        // close: a build stops
    retry_after: Duration, // see RETRY_AFTER
}

/// A file name from a text: 64-bit FNV-1a, which stays the same across Rust
/// versions (std's hasher does not promise that).
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

/// The file of text in voice.
fn file_name(voice: &str, text: &str) -> String {
    format!("{:016x}.wav", fnv1a(&format!("{voice}\n{text}")))
}

fn is_clip_file(name: &str) -> bool {
    name.strip_suffix(".wav").is_some_and(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// 16-bit mono audio, which the gapless player takes.
fn playable(wav: &[u8]) -> bool {
    parse_wav(wav).is_some_and(|w| w.channels == 1 && w.bits == 16 && !w.data.is_empty())
}

impl Phrases {
    pub fn new(dir: PathBuf) -> Arc<Phrases> {
        Arc::new(Phrases { dir, st: Mutex::default(), building: AtomicBool::new(false), cancel: Cancel::new(), retry_after: RETRY_AFTER })
    }

    #[cfg(test)]
    fn retrying_after(dir: PathBuf, retry_after: Duration) -> Arc<Phrases> {
        let p = Phrases::new(dir);
        Arc::new(Phrases { retry_after, ..Arc::try_unwrap(p).unwrap_or_else(|_| unreachable!()) })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The audio of text, if it is ready.
    pub fn get(&self, text: &str) -> Option<Vec<u8>> {
        self.lock().clips.get(text).cloned()
    }

    #[cfg(test)]
    pub fn insert(&self, text: &str, wav: Vec<u8>) {
        self.lock().clips.insert(text.to_string(), wav);
    }

    /// How many phrases are ready.
    #[cfg(test)]
    pub fn ready(&self) -> usize {
        self.lock().clips.len()
    }

    /// Loads what is on disk for the engine's voice, and synthesizes the rest
    /// in the background, one phrase at a time and only while the arbiter
    /// allows it (Use::Cache). Does nothing while a build runs.
    pub fn build(self: &Arc<Self>, engine: &Arc<AivisEngine>, texts: Vec<String>) {
        if self.building.swap(true, Ordering::SeqCst) {
            return;
        }
        let (p, engine) = (self.clone(), engine.clone());
        std::thread::spawn(move || {
            let res = p.build_now(&engine, &texts);
            p.building.store(false, Ordering::SeqCst);
            match res {
                Ok((loaded, made, 0)) => log(&format!("phrases: {loaded} from disk, {made} synthesized")),
                Ok((loaded, made, failed)) => log(&format!("phrases: {loaded} from disk, {made} synthesized, {failed} not made")),
                Err(e) => log(&format!("phrases: {e}")),
            }
        });
    }

    /// (loaded from disk, synthesized, not made)
    fn build_now(&self, engine: &Arc<AivisEngine>, texts: &[String]) -> Result<(usize, usize, usize), String> {
        let version = engine.version().ok_or("the engine did not tell its version")?;
        let voice = format!("{version}\n{}\n{RATE}", engine.voice());
        private_dir(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        let files: Vec<(&String, PathBuf)> = texts.iter().map(|t| (t, self.dir.join(file_name(&voice, t)))).collect();
        self.prune(&files);
        {
            let mut st = self.lock();
            if st.voice != voice {
                st.clips.clear();
                st.voice = voice.clone();
            }
        }
        // All that is on disk first: a phrase the engine fails on keeps none
        // of the others from being read
        let mut loaded = 0;
        let mut missing = Vec::new();
        for (text, path) in files {
            if self.lock().clips.contains_key(text) {
                continue;
            }
            match std::fs::read(&path) {
                Ok(wav) if playable(&wav) => {
                    self.lock().clips.insert(text.clone(), wav);
                    loaded += 1;
                }
                _ => missing.push((text, path)),
            }
        }
        let mut made = 0;
        for round in 0..TRIES {
            if missing.is_empty() {
                break;
            }
            if round > 0 {
                self.pause(engine, self.retry_after)?;
            }
            let mut failed = Vec::new();
            for (text, path) in missing {
                self.wait_quiet(engine)?;
                match self.make(engine, text, &path) {
                    Ok(()) => made += 1,
                    Err(_) if self.cancel.is_cancelled() => return Err("stopped".into()),
                    Err(e) => {
                        log(&format!("phrases: {text}: {e}"));
                        failed.push((text, path));
                    }
                }
            }
            missing = failed;
        }
        Ok((loaded, made, missing.len()))
    }

    /// Synthesizes text and keeps it, on disk at path too.
    fn make(&self, engine: &Arc<AivisEngine>, text: &str, path: &Path) -> Result<(), String> {
        let wav = engine.synth_as(Use::Cache, &self.cancel, text, RATE).map_err(|e| e.to_string())?.wav;
        if !playable(&wav) {
            return Err("not 16-bit mono audio".into());
        }
        // Written whole or not at all: a file cut short would not play
        let tmp = path.with_extension("tmp");
        write_private(&tmp, &wav).and_then(|()| std::fs::rename(&tmp, path)).map_err(|e| format!("{}: {e}", path.display()))?;
        self.lock().clips.insert(text.to_string(), wav);
        Ok(())
    }

    /// Whether the build may go on: not once the app closes or the engine
    /// goes away (its return builds the cache again).
    fn go_on(&self, engine: &AivisEngine) -> Result<(), String> {
        if self.cancel.is_cancelled() || engine.is_closed() {
            return Err("stopped".into());
        }
        if engine.is_down() {
            return Err("the engine went away".into());
        }
        Ok(())
    }

    /// Waits until the engine may be used for the cache.
    fn wait_quiet(&self, engine: &AivisEngine) -> Result<(), String> {
        loop {
            self.go_on(engine)?;
            if engine.arbiter.may(Use::Cache) {
                return Ok(());
            }
            std::thread::sleep(POLL);
        }
    }

    /// Waits d before trying again.
    fn pause(&self, engine: &AivisEngine, d: Duration) -> Result<(), String> {
        let until = Instant::now() + d;
        loop {
            self.go_on(engine)?;
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            std::thread::sleep(left.min(POLL));
        }
    }

    /// Removes the clip files that are not among files (another voice's, a
    /// phrase no longer said) and what an interrupted build left.
    fn prune(&self, files: &[(&String, PathBuf)]) {
        let keep: Vec<&Path> = files.iter().map(|(_, p)| p.as_path()).collect();
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let stale = (is_clip_file(&name) && !keep.contains(&path.as_path())) || name.ends_with(".tmp");
            if stale {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// Stops a build under way (the app is closing).
    pub fn stop(&self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speech::aivis::testing::engine;
    use crate::speech::arbiter::Activity;
    use std::time::Instant;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("va-phrases-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn wait_built(p: &Phrases, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while p.ready() < n || p.building.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "built {} of {n}", p.ready());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn texts() -> Vec<String> {
        ["確認です。", "実行していいですか？", "マイクを切りました。"].map(String::from).to_vec()
    }

    /// The first build synthesizes the phrases and keeps them on disk; the
    /// next one (the next start) reads them back without the engine.
    #[test]
    fn built_once_then_read_from_disk() {
        let e = engine(|_| 200, |_| Duration::ZERO);
        let a = AivisEngine::new(&e.srv.url, 1, 1.3);
        let dir = temp_dir("disk");
        let p = Phrases::new(dir.clone());
        p.build(&a, texts());
        wait_built(&p, 3);
        assert_eq!(*e.synths.lock().unwrap(), texts());
        assert!(e.bodies.lock().unwrap().iter().all(|b| b["outputSamplingRate"] == RATE), "made at AivisSpeech's own rate");
        assert!(p.get("確認です。").is_some_and(|w| playable(&w)));
        assert!(p.get("ほかの文。").is_none());
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 3);
        use std::os::unix::fs::PermissionsExt;
        assert!(files.iter().all(|f| f.metadata().unwrap().permissions().mode() & 0o077 == 0), "only the user can read them");

        let again = Phrases::new(dir.clone());
        again.build(&a, texts());
        wait_built(&again, 3);
        assert_eq!(e.synths.lock().unwrap().len(), 3, "nothing synthesized again");
        assert_eq!(again.get("確認です。"), p.get("確認です。"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Another voice (here another style) is made anew and the old files go;
    /// a damaged file is made again. (Before, another output rate also made
    /// the cache anew; now clips are made at one rate.)
    #[test]
    fn another_voice_is_made_anew() {
        let e = engine(|_| 200, |_| Duration::ZERO);
        let (a, b) = (AivisEngine::new(&e.srv.url, 1, 1.3), AivisEngine::new(&e.srv.url, 2, 1.3));
        let dir = temp_dir("voice");
        let p = Phrases::new(dir.clone());
        p.build(&a, texts());
        wait_built(&p, 3);
        p.build(&b, texts());
        wait_built(&p, 3);
        assert_eq!(e.synths.lock().unwrap().len(), 6);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 3, "the first style's files are gone");

        let one = std::fs::read_dir(&dir).unwrap().flatten().next().unwrap().path();
        std::fs::write(&one, b"RIFF").unwrap();
        let fresh = Phrases::new(dir.clone());
        fresh.build(&b, texts());
        wait_built(&fresh, 3);
        assert_eq!(e.synths.lock().unwrap().len(), 7, "only the damaged one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cache is made only while nothing goes on, and a stop ends the build.
    #[test]
    fn built_only_while_quiet() {
        let e = engine(|_| 200, |_| Duration::ZERO);
        let a = AivisEngine::new(&e.srv.url, 1, 1.3);
        let busy = Arc::new(AtomicBool::new(true));
        let b = busy.clone();
        let _ = a.arbiter.activity.set(Box::new(move || Activity { busy: b.load(Ordering::SeqCst), ..Activity::default() }));
        let dir = temp_dir("quiet");
        let p = Phrases::new(dir.clone());
        p.build(&a, texts());
        std::thread::sleep(Duration::from_millis(600));
        assert!(e.synths.lock().unwrap().is_empty(), "the user is talking");
        busy.store(false, Ordering::SeqCst);
        wait_built(&p, 3);

        let stopped = Phrases::new(temp_dir("stopped"));
        busy.store(true, Ordering::SeqCst);
        stopped.build(&a, texts());
        stopped.stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        while stopped.building.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the build went on after the stop");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(stopped.ready(), 0);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(temp_dir("stopped"));
    }

    /// A phrase the engine fails on keeps none of the others from being made
    /// or read, and is tried again later; one it always refuses is left out
    /// after TRIES tries.
    #[test]
    fn a_failed_phrase_is_tried_again() {
        use std::sync::atomic::AtomicUsize;
        let tries = Arc::new(AtomicUsize::new(0));
        let t = tries.clone();
        let e = engine(
            move |text| match text {
                "確認です。" if t.fetch_add(1, Ordering::SeqCst) == 0 => 500,
                "マイクを切りました。" => 422,
                _ => 200,
            },
            |_| Duration::ZERO,
        );
        let a = AivisEngine::new(&e.srv.url, 1, 1.3);
        let dir = temp_dir("retry");
        let p = Phrases::retrying_after(dir.clone(), Duration::from_millis(50));
        p.build(&a, texts());
        wait_built(&p, 2);
        assert!(p.get("確認です。").is_some() && p.get("実行していいですか？").is_some());
        assert!(p.get("マイクを切りました。").is_none());
        assert_eq!(tries.load(Ordering::SeqCst), 2, "made on the second try");
        let refused = e.bodies.lock().unwrap().iter().filter(|b| b["kana"] == "マイクを切りました。").count();
        assert_eq!(refused, TRIES);

        // The next start reads the two from disk and tries the third again
        let again = Phrases::retrying_after(dir.clone(), Duration::from_millis(50));
        again.build(&a, texts());
        wait_built(&again, 2);
        assert_eq!(e.synths.lock().unwrap().len(), 2, "nothing made again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// docs/design.md's account of P1 gives the cache's numbers.
    #[test]
    fn the_design_doc_has_the_cache() {
        let p1 = include_str!("../../docs/design.md").split("## P1 で").nth(1).unwrap().split("\n## ").next().unwrap();
        for want in [format!("{RATE} Hz"), format!("{} 秒後に、{TRIES} 回まで", RETRY_AFTER.as_secs())] {
            assert!(p1.contains(&want), "the P1 section should say {want:?}");
        }
    }

    #[test]
    fn file_names() {
        assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325, "the FNV-1a offset basis");
        assert_eq!(fnv1a("a"), 0xaf63_dc4c_8601_ec8c, "a published FNV-1a 64 test vector");
        assert_ne!(file_name("v1", "確認です。"), file_name("v2", "確認です。"));
        assert!(is_clip_file(&file_name("v", "x")));
        assert!(!is_clip_file("notes.wav") && !is_clip_file("0123456789abcdef.txt"));
    }
}
