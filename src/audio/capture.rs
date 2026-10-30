//! Microphone capture for push-to-talk. (Always-on listening is P2.)

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::wav::encode_wav;

pub const SAMPLE_RATE: u32 = 16000; // what whisper wants

#[derive(Debug, PartialEq, Eq)]
pub enum RecError {
    /// Nothing but digital silence: the microphone is not allowed
    NoAudio,
    /// Nobody spoke
    Silence,
}

impl std::fmt::Display for RecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecError::NoAudio => {
                f.write_str("マイクの音が取れていません。システム設定 > プライバシーとセキュリティ > マイク で、使っているターミナルアプリを許可してください。")
            }
            RecError::Silence => f.write_str("silence"),
        }
    }
}

/// Averages the device's samples down to 16 kHz mono as they arrive.
struct Downsampler {
    step: f64, // device samples per output sample
    pos: f64,
    sum: f64,
    n: u32,
}

impl Downsampler {
    fn new(rate: u32) -> Downsampler {
        Downsampler { step: rate as f64 / SAMPLE_RATE as f64, pos: 0.0, sum: 0.0, n: 0 }
    }

    fn push(&mut self, sample: f32, out: &mut Vec<i16>) {
        self.sum += sample as f64;
        self.n += 1;
        self.pos += 1.0;
        if self.pos >= self.step {
            self.pos -= self.step;
            let v = self.sum / self.n as f64;
            out.push((v * 32767.0).clamp(-32768.0, 32767.0) as i16);
            self.sum = 0.0;
            self.n = 0;
        }
    }
}

struct Shared {
    running: bool,
    samples: Vec<i16>,
    peak: i32,
    last_level: Option<Instant>,
    down: Downsampler,
}

/// Captures the default microphone as 16 kHz mono PCM. The device is set up
/// once (that is the slow part) but only runs while recording, so macOS
/// shows the mic indicator only then. close() lets the device go entirely
/// ("マイクを切って").
pub struct Recorder {
    on_level: Arc<dyn Fn(f64) + Send + Sync>, // 0..1, about 20 times a second
    shared: Arc<Mutex<Shared>>,
    stream: Mutex<Option<cpal::Stream>>,
}

impl Recorder {
    pub fn new(on_level: impl Fn(f64) + Send + Sync + 'static) -> Recorder {
        Recorder {
            on_level: Arc::new(on_level),
            shared: Arc::new(Mutex::new(Shared { running: false, samples: Vec::new(), peak: 0, last_level: None, down: Downsampler::new(SAMPLE_RATE) })),
            stream: Mutex::new(None),
        }
    }

    /// Sets the device up ahead of the first recording.
    pub fn prepare(&self) -> Result<(), String> {
        let mut stream = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        if stream.is_none() {
            *stream = Some(self.build()?);
        }
        Ok(())
    }

    fn build(&self) -> Result<cpal::Stream, String> {
        let device = cpal::default_host().default_input_device().ok_or("マイクが見つかりません")?;
        let config = device.default_input_config().map_err(|e| e.to_string())?;
        let channels = config.channels().max(1) as usize;
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).down = Downsampler::new(config.sample_rate());
        let shared = self.shared.clone();
        let on_level = self.on_level.clone();
        device
            .build_input_stream(
                config.config(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| on_data(&shared, on_level.as_ref(), data, channels),
                |err| crate::logf!("audio: {err}"),
                None,
            )
            .map_err(|e| e.to_string())
    }

    pub fn start(&self) -> Result<(), String> {
        let mut stream = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            if sh.running {
                return Ok(());
            }
            sh.samples.clear();
            sh.peak = 0;
            sh.running = true;
        }
        let started = match stream.as_ref() {
            Some(s) => s.play().map_err(|e| e.to_string()),
            None => Err("not set up".to_string()),
        };
        if started.is_ok() {
            return Ok(());
        }
        // The device may have gone away (headset unplugged); set it up again
        *stream = None;
        let res = self.build().and_then(|s| {
            s.play().map_err(|e| e.to_string())?;
            *stream = Some(s);
            Ok(())
        });
        if res.is_err() {
            self.shared.lock().unwrap_or_else(|e| e.into_inner()).running = false;
        }
        res
    }

    /// Ends the recording and returns it as a WAV file with its length in
    /// seconds; None when nothing was being recorded.
    pub fn stop(&self) -> Option<(Result<Vec<u8>, RecError>, f64)> {
        let stream = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        let (samples, peak) = {
            let mut sh = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            if !sh.running {
                return None;
            }
            sh.running = false;
            (std::mem::take(&mut sh.samples), sh.peak)
        };
        if let Some(s) = stream.as_ref() {
            let _ = s.pause();
        }
        let seconds = samples.len() as f64 / SAMPLE_RATE as f64;
        // A denied microphone delivers pure digital silence
        let res = if samples.len() > SAMPLE_RATE as usize / 4 && peak == 0 {
            Err(RecError::NoAudio)
        } else if voiced_ms(&samples) < 150 {
            // Whisper invents phrases for silence ("お疲れ様でした"), so don't send it any
            Err(RecError::Silence)
        } else {
            Ok(encode_wav(&samples, SAMPLE_RATE))
        };
        Some((res, seconds))
    }

    /// Lets go of the device (the mic is off until prepare or start).
    pub fn close(&self) {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).running = false;
        *self.stream.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

fn on_data(shared: &Mutex<Shared>, on_level: &(dyn Fn(f64) + Send + Sync), data: &[f32], channels: usize) {
    let mut sum = 0.0f64;
    let mut frames = 0u32;
    let report;
    {
        let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
        let sh = &mut *guard;
        if !sh.running {
            return;
        }
        for frame in data.chunks(channels) {
            let v = frame.iter().sum::<f32>() / frame.len() as f32;
            sh.down.push(v, &mut sh.samples);
            sh.peak = sh.peak.max((v.abs() * 32768.0) as i32);
            sum += (v as f64) * (v as f64);
            frames += 1;
        }
        report = sh.last_level.is_none_or(|t| t.elapsed() > Duration::from_millis(50));
        if report {
            sh.last_level = Some(Instant::now());
        }
    }
    if report && frames > 0 {
        let rms = (sum / frames as f64).sqrt();
        // Rough dBFS mapping: -60 dB..0 dB -> 0..1
        let lvl = (20.0 * (rms + 1e-9).log10() + 60.0) / 60.0;
        on_level(lvl.clamp(0.0, 1.0));
    }
}

/// A crude voice activity measure: how long the signal stays above a
/// speaking level, in 20 ms frames.
pub fn voiced_ms(samples: &[i16]) -> usize {
    const FRAME: usize = SAMPLE_RATE as usize / 50;
    const THRESHOLD: f64 = 0.01; // about -40 dBFS
    samples
        .as_chunks::<FRAME>()
        .0
        .iter()
        .filter(|frame| {
            let sum: f64 = frame.iter().map(|&v| (v as f64 / 32768.0).powi(2)).sum();
            (sum / FRAME as f64).sqrt() > THRESHOLD
        })
        .count()
        * 20
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voiced() {
        assert_eq!(voiced_ms(&vec![0i16; SAMPLE_RATE as usize]), 0, "silence");
        let tone: Vec<i16> =
            (0..SAMPLE_RATE / 2).map(|i| (8000.0 * (i as f64 * 2.0 * std::f64::consts::PI * 220.0 / SAMPLE_RATE as f64).sin()) as i16).collect();
        assert!(voiced_ms(&tone) >= 400);
    }

    #[test]
    fn downsampling() {
        // 48 kHz to 16 kHz: every three samples become their mean
        let mut d = Downsampler::new(48000);
        let mut out = Vec::new();
        for v in [0.1f32, 0.2, 0.3, -0.5, -0.5, -0.5, 0.0] {
            d.push(v, &mut out);
        }
        assert_eq!(out.len(), 2);
        assert!((out[0] as f64 - 0.2 * 32767.0).abs() < 2.0 && (out[1] as f64 + 0.5 * 32767.0).abs() < 2.0);
        // 44.1 kHz: a second of input is a second of output
        let mut d = Downsampler::new(44100);
        let mut out = Vec::new();
        for _ in 0..44100 {
            d.push(0.5, &mut out);
        }
        assert!((out.len() as i64 - 16000).abs() <= 1, "{}", out.len());
    }

    /// Opens the real microphone for half a second and counts what arrived
    /// (nothing is kept): `VA_LIVE_AUDIO=1 cargo test live_ -- --nocapture`
    #[test]
    fn live_microphone_delivers_samples() {
        if std::env::var("VA_LIVE_AUDIO").is_err() {
            return;
        }
        let levels = Arc::new(Mutex::new(0usize));
        let l = levels.clone();
        let r = Recorder::new(move |_| *l.lock().unwrap() += 1);
        r.prepare().expect("input device");
        r.start().expect("start");
        std::thread::sleep(Duration::from_millis(500));
        let (res, secs) = r.stop().expect("was recording");
        println!("recorded {secs:.2} s, {} level reports, result: {:?}", levels.lock().unwrap(), res.as_ref().map(Vec::len));
        assert!((0.3..0.8).contains(&secs), "half a second of 16 kHz audio, got {secs}");
        assert_ne!(res, Err(RecError::NoAudio), "the microphone is not allowed for this terminal");
        r.close();
    }

    #[test]
    fn recorder_without_a_recording() {
        let r = Recorder::new(|_| {});
        assert!(r.stop().is_none());
    }
}
