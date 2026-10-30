//! Who may use the CPU for speech work that nobody is waiting for yet.
//!
//! On an 8 GB M1, whisper, AivisSpeech and Claude Code compete for memory and
//! cores. P0 keeps edex-rs's behaviour (the AivisSpeech engine is warmed up
//! whenever a reply is expected); this is the seam where P1 decides instead,
//! e.g. "not while whisper is transcribing" or "not while a job is building".

/// Asked before the speech engine is warmed up in the background.
pub trait Arbiter: Send + Sync {
    /// Whether a warm-up pass may run now.
    fn may_warm(&self) -> bool;
}

/// P0: warming is always allowed.
pub struct AlwaysWarm;

impl Arbiter for AlwaysWarm {
    fn may_warm(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p0_always_warms() {
        assert!(AlwaysWarm.may_warm());
    }
}
