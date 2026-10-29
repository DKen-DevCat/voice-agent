//! Gapless playback of the speech engine's audio through one output device.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::wav::parse_wav;
use crate::speech::tts::{CLOSERS, Cancel, is_comma};

/// The sample rate asked of AivisSpeech until the output device's is known.
pub const DEFAULT_RATE: u32 = 44100;

// Silence added after each clip on gapless playback, so that a pause between
// chunks sounds like one inside a chunk: there AivisSpeech leaves about
// 0.45 s at "。" and 0.35 s at "、", while a clip's own quiet tail and head
// make about 0.3 s.
const PAUSE_SENTENCE: Duration = Duration::from_millis(150);
const PAUSE_CLAUSE: Duration = Duration::from_millis(60);

/// The silence that follows a chunk of speech.
pub fn pause_after(text: &str) -> Duration {
    let last = text.trim_end_matches(|c: char| CLOSERS.contains(c) || c == ' ').chars().next_back();
    match last {
        Some(c) if is_comma(c) || c == ',' => PAUSE_CLAUSE,
        _ => PAUSE_SENTENCE,
    }
}

/// The samples of a clip and the rate they play at.
#[derive(Clone, Debug, PartialEq)]
pub struct Pcm {
    pub samples: Vec<i16>,
    pub rate: u32,
}

/// The samples of a 16-bit mono WAV followed by pause of silence, or None
/// for any other format.
pub fn wav_pcm(wav: &[u8], pause: Duration) -> Option<Pcm> {
    let w = parse_wav(wav)?;
    if w.channels != 1 || w.bits != 16 {
        return None;
    }
    let mut samples: Vec<i16> = w.data.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
    samples.resize(samples.len() + (pause.as_secs_f64() * w.rate as f64) as usize, 0);
    Some(Pcm { samples, rate: w.rate })
}

/// Samples at one rate as samples at another (linear interpolation).
fn resample(samples: &[i16], from: u32, to: u32) -> Vec<i16> {
    if from == to || samples.is_empty() || from == 0 || to == 0 {
        return samples.to_vec();
    }
    let n = (samples.len() as u64 * to as u64 / from as u64) as usize;
    let step = from as f64 / to as f64;
    (0..n)
        .map(|i| {
            let pos = i as f64 * step;
            let k = pos as usize;
            let a = samples[k.min(samples.len() - 1)] as f64;
            let b = samples[(k + 1).min(samples.len() - 1)] as f64;
            (a + (b - a) * (pos - k as f64)) as i16
        })
        .collect()
}

type Done = Box<dyn FnOnce() + Send>;

/// What the device callback drains: the queued samples and, for each clip,
/// what to call once the device has been handed all of it.
#[derive(Default)]
struct Queue {
    buf: VecDeque<i16>,
    read: u64,  // samples handed to the device
    write: u64, // samples appended
    marks: VecDeque<(u64, Done)>,
}

impl Queue {
    fn add(&mut self, samples: &[i16], done: Done) {
        self.buf.extend(samples);
        self.write += samples.len() as u64;
        self.marks.push_back((self.write, done));
    }

    /// Fills out (frames of `channels` samples) with what is queued, silence
    /// when there is nothing; returns what to call now.
    fn fill(&mut self, out: &mut [f32], channels: usize) -> Vec<Done> {
        for frame in out.chunks_mut(channels.max(1)) {
            let v = match self.buf.pop_front() {
                Some(s) => {
                    self.read += 1;
                    s as f32 / 32768.0
                }
                None => 0.0,
            };
            frame.fill(v);
        }
        let mut fired = Vec::new();
        while self.marks.front().is_some_and(|(end, _)| *end <= self.read) {
            fired.push(self.marks.pop_front().unwrap().1);
        }
        fired
    }

    /// Discards everything not yet played; returns what to call now.
    fn drop_all(&mut self) -> Vec<Done> {
        self.buf.clear();
        self.read = self.write;
        self.marks.drain(..).map(|(_, d)| d).collect()
    }
}

struct Device {
    stream: cpal::Stream,
    started: bool,
}

/// Plays 16-bit mono PCM through the default output device. Clips are
/// appended to a buffer the device drains, so one follows another without
/// the gap of starting a player per clip. The device runs only while there
/// is something to play.
pub struct PcmPlayer {
    failed: AtomicBool, // the device could not be used; afplay takes over
    rate: AtomicU32,    // the device's sample rate
    dev: Mutex<Option<Device>>,
    queue: Arc<Mutex<Queue>>,
}

impl Default for PcmPlayer {
    fn default() -> Self {
        PcmPlayer { failed: AtomicBool::new(false), rate: AtomicU32::new(DEFAULT_RATE), dev: Mutex::new(None), queue: Arc::default() }
    }
}

impl PcmPlayer {
    pub fn usable(&self) -> bool {
        !self.failed.load(Ordering::SeqCst)
    }

    /// The rate clips should come in, to play without resampling.
    pub fn rate(&self) -> u32 {
        self.rate.load(Ordering::SeqCst)
    }

    /// Sets the device up ahead of the first clip.
    pub fn prepare(&self) -> Result<(), String> {
        let mut dev = self.dev.lock().unwrap_or_else(|e| e.into_inner());
        self.setup(&mut dev)
    }

    fn setup(&self, dev: &mut Option<Device>) -> Result<(), String> {
        if dev.is_some() {
            return Ok(());
        }
        let device = cpal::default_host().default_output_device().ok_or("出力デバイスが見つかりません")?;
        let config = device.default_output_config().map_err(|e| e.to_string())?;
        let channels = config.channels().max(1) as usize;
        let queue = self.queue.clone();
        let stream = device
            .build_output_stream(
                config.config(),
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let fired = queue.lock().unwrap_or_else(|e| e.into_inner()).fill(out, channels);
                    for done in fired {
                        done();
                    }
                },
                |err| crate::logf!("tts: speakers: {err}"),
                None,
            )
            .map_err(|e| e.to_string())?;
        self.rate.store(config.sample_rate(), Ordering::SeqCst);
        *dev = Some(Device { stream, started: false });
        Ok(())
    }

    /// Starts the device.
    fn run(&self, dev: &mut Option<Device>) -> Result<(), String> {
        if dev.as_ref().is_some_and(|d| d.started) {
            return Ok(());
        }
        self.setup(dev)?;
        if dev.as_ref().unwrap().stream.play().is_err() {
            // The device may have gone away (headphones unplugged); set it up again
            *dev = None;
            self.setup(dev)?;
            dev.as_ref().unwrap().stream.play().map_err(|e| e.to_string())?;
        }
        dev.as_mut().unwrap().started = true;
        Ok(())
    }

    /// Appends a clip after whatever is still playing; done is called once it
    /// has been played (or dropped). Nothing is queued once cancel is set.
    pub fn enqueue(&self, cancel: &Cancel, pcm: &Pcm, done: impl FnOnce() + Send + 'static) -> Result<(), String> {
        let mut dev = self.dev.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = self.run(&mut dev) {
            self.failed.store(true, Ordering::SeqCst);
            return Err(e);
        }
        let samples = resample(&pcm.samples, pcm.rate, self.rate());
        // stop() cancels before it drops the buffer, so a clip that lost the
        // race is not left behind to play
        if cancel.is_cancelled() || samples.is_empty() {
            done();
            return Ok(());
        }
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).add(&samples, Box::new(done));
        Ok(())
    }

    /// Discards everything not yet played.
    pub fn drop_queued(self: &Arc<Self>) {
        let fired = self.queue.lock().unwrap_or_else(|e| e.into_inner()).drop_all();
        for done in fired {
            done();
        }
        self.idle();
    }

    /// Stops the device in a while if it has had nothing more to play.
    pub fn idle(self: &Arc<Self>) {
        let p = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            let mut dev = p.dev.lock().unwrap_or_else(|e| e.into_inner());
            let empty = p.queue.lock().unwrap_or_else(|e| e.into_inner()).buf.is_empty();
            if let Some(d) = dev.as_mut()
                && empty
                && d.started
            {
                let _ = d.stream.pause();
                d.started = false;
            }
        });
    }

    pub fn close(&self) {
        self.failed.store(true, Ordering::SeqCst);
        *self.dev.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::wav::encode_wav;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn pauses() {
        assert_eq!(pause_after("はい。"), PAUSE_SENTENCE);
        assert_eq!(pause_after("それで、"), PAUSE_CLAUSE);
        assert_eq!(pause_after("「それで、」 "), PAUSE_CLAUSE);
        assert_eq!(pause_after("well, "), PAUSE_CLAUSE);
    }

    #[test]
    fn wav_to_pcm() {
        let wav = encode_wav(&[5; 441], 44100);
        let pcm = wav_pcm(&wav, Duration::from_millis(100)).unwrap();
        assert_eq!(pcm.rate, 44100);
        assert_eq!(pcm.samples.len(), 441 + 4410);
        assert!(pcm.samples[..441].iter().all(|&s| s == 5) && pcm.samples[441..].iter().all(|&s| s == 0));
        // Stereo is not for the gapless player
        let mut stereo = wav.clone();
        stereo[22] = 2;
        assert!(wav_pcm(&stereo, Duration::ZERO).is_none());
        assert!(wav_pcm(b"junk", Duration::ZERO).is_none());
    }

    #[test]
    fn resampling() {
        assert_eq!(resample(&[1, 2, 3], 48000, 48000), [1, 2, 3]);
        let up = resample(&[0, 100, 200, 300], 24000, 48000);
        assert_eq!(up.len(), 8);
        assert_eq!(&up[..4], [0, 50, 100, 150]);
        assert_eq!(resample(&[0; 44100], 44100, 48000).len(), 48000);
    }

    #[test]
    fn queue_plays_clips_in_order() {
        let mut q = Queue::default();
        let fired = Arc::new(AtomicUsize::new(0));
        for n in [4usize, 2] {
            let f = fired.clone();
            q.add(
                &vec![16384; n],
                Box::new(move || {
                    f.fetch_add(1, Ordering::SeqCst);
                }),
            );
        }
        let run = |q: &mut Queue, out: &mut [f32]| {
            for d in q.fill(out, 2) {
                d();
            }
        };
        // Stereo out: each mono sample goes to both channels
        let mut out = [9.0f32; 6];
        run(&mut q, &mut out);
        assert_eq!(out, [0.5; 6]);
        assert_eq!(fired.load(Ordering::SeqCst), 0, "the first clip has a sample left");
        let mut out = [9.0f32; 4];
        run(&mut q, &mut out);
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        let mut out = [9.0f32; 6];
        run(&mut q, &mut out);
        assert_eq!(out, [0.5, 0.5, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(fired.load(Ordering::SeqCst), 2);

        // Dropping calls what was waiting
        let f = fired.clone();
        q.add(
            &[1; 100],
            Box::new(move || {
                f.fetch_add(1, Ordering::SeqCst);
            }),
        );
        for d in q.drop_all() {
            d();
        }
        assert_eq!(fired.load(Ordering::SeqCst), 3);
        assert!(q.buf.is_empty() && q.read == q.write);
    }
}
