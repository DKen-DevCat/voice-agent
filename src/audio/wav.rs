//! The WAV helpers speech uses: recordings for whisper, AivisSpeech's audio.

/// 16-bit mono samples as a WAV file.
pub fn encode_wav(samples: &[i16], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + samples.len() * 2);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

pub struct Wav<'a> {
    pub rate: u32,
    pub channels: u16,
    pub bits: u16,
    pub data: &'a [u8],
}

/// Finds the format and samples of a RIFF WAV file.
pub fn parse_wav(wav: &[u8]) -> Option<Wav<'_>> {
    if wav.len() < 12 || &wav[..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return None;
    }
    let le16 = |b: &[u8]| u16::from_le_bytes([b[0], b[1]]);
    let le32 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let (mut pcm, mut rate, mut channels, mut bits) = (false, 0, 0, 0);
    let mut p = 12;
    while p + 8 <= wav.len() {
        let size = le32(&wav[p + 4..]) as usize;
        let body = &wav[p + 8..];
        match &wav[p..p + 4] {
            b"fmt " => {
                if size < 16 || body.len() < 16 {
                    return None;
                }
                pcm = le16(body) == 1;
                channels = le16(&body[2..]);
                rate = le32(&body[4..]);
                bits = le16(&body[14..]);
            }
            b"data" => {
                if !pcm || rate == 0 {
                    return None;
                }
                // Streamed WAVs may not know their length
                return Some(Wav { rate, channels, bits, data: &body[..size.min(body.len())] });
            }
            _ => {}
        }
        p += 8 + size + (size & 1);
    }
    None
}

/// How long a WAV file plays, in seconds (0 if it cannot be read).
pub fn wav_seconds(wav: &[u8]) -> f64 {
    match parse_wav(wav) {
        Some(w) if w.channels > 0 && w.bits >= 8 => w.data.len() as f64 / (w.rate as f64 * w.channels as f64 * (w.bits / 8) as f64),
        _ => 0.0,
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// secs of 16-bit mono audio at rate.
    pub fn test_wav(secs: f64, rate: u32) -> Vec<u8> {
        encode_wav(&vec![257i16; (secs * rate as f64) as usize], rate)
    }

    #[test]
    fn wav_round_trip() {
        let wav = encode_wav(&[1, -2, 3], 16000);
        let w = parse_wav(&wav).unwrap();
        assert_eq!((w.rate, w.channels, w.bits, w.data.len()), (16000, 1, 16, 6));
        assert!((wav_seconds(&test_wav(1.5, 44100)) - 1.5).abs() < 1e-3);
        assert_eq!(wav_seconds(b"not a wav"), 0.0);
        // A length that runs past the end (a streamed file) is cut to what is there
        let mut cut = wav.clone();
        cut[40..44].copy_from_slice(&1000u32.to_le_bytes());
        assert_eq!(parse_wav(&cut).unwrap().data.len(), 6);
    }
}
