//! `~/.config/voice-agent/config.json` (or `$VA_CONFIG`); every field is
//! optional. The persona (`persona.md`) sits beside it.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Deserialize, Default)]
#[cfg_attr(test, derive(serde::Serialize))]
#[serde(default)]
struct Raw {
    shell: String,
    dir: String,
    model: String,
    effort: String,
    tts: String,
    #[serde(rename = "aivisURL")]
    aivis_url: String,
    #[serde(rename = "aivisSpeaker")]
    aivis_speaker: i64,
    #[serde(rename = "aivisSpeed")]
    aivis_speed: f64,
    voice: String,
    #[serde(rename = "voiceRate")]
    voice_rate: i64,
    speak: Option<bool>,
    #[serde(rename = "whisperModel")]
    whisper_model: String,
    language: String,
    #[serde(rename = "holdMs")]
    hold_ms: u64,
    mascot: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub shell: String,
    pub dir: PathBuf, // where Claude and the shells start
    pub model: String,
    pub effort: String,
    pub tts: String, // aivis (falls back to say) / say
    pub aivis_url: String,
    pub aivis_speaker: i64,
    pub aivis_speed: f64,
    pub voice: String,
    pub voice_rate: i64,
    pub speak: bool,
    pub whisper_model: PathBuf,
    pub language: String,
    pub hold_ms: u64,
    pub mascot: bool,
    pub persona: PathBuf,
}

fn or(s: String, default: &str) -> String {
    if s.is_empty() { default.to_string() } else { s }
}

pub fn config_dir(home: &Path) -> PathBuf {
    home.join(".config").join("voice-agent")
}

/// `$VA_CONFIG`, else config.json in config_dir.
fn config_path(home: &Path) -> PathBuf {
    config_path_from(std::env::var_os("VA_CONFIG"), home)
}

fn config_path_from(var: Option<std::ffi::OsString>, home: &Path) -> PathBuf {
    match var {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => config_dir(home).join("config.json"),
    }
}

impl Config {
    pub fn load(home: &Path) -> Config {
        let path = config_path(home);
        let raw = match std::fs::read(&path) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_else(|e| {
                eprintln!("voice-agent: {}: {e}", path.display());
                Raw::default()
            }),
            Err(_) => Raw::default(),
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| home.to_path_buf());
        Config::from_raw(raw, home, &cwd, &path)
    }

    /// file: where raw was read from
    fn from_raw(r: Raw, home: &Path, cwd: &Path, file: &Path) -> Config {
        let expand = |s: &str| match s.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(s),
        };
        Config {
            // The shells must be zsh for the OSC 633 integration
            shell: or(r.shell, "/bin/zsh"),
            dir: if r.dir.is_empty() { cwd.to_path_buf() } else { expand(&r.dir) },
            // A fast front end; heavier work goes to its Opus and Fable subagents
            model: or(r.model, "claude-sonnet-5-5"),
            effort: r.effort,
            tts: or(r.tts, "aivis"),
            aivis_url: or(r.aivis_url, "http://127.0.0.1:10101").trim_end_matches('/').to_string(),
            // まお, AivisSpeech's bundled voice
            aivis_speaker: if r.aivis_speaker == 0 { 888753760 } else { r.aivis_speaker },
            aivis_speed: if r.aivis_speed == 0.0 { 1.3 } else { r.aivis_speed },
            voice: or(r.voice, "Kyoko"),
            voice_rate: if r.voice_rate == 0 { 260 } else { r.voice_rate },
            speak: r.speak.unwrap_or(true),
            whisper_model: if r.whisper_model.is_empty() { home.join(".local/share/whisper/ggml-large-v3-turbo-q5_0.bin") } else { expand(&r.whisper_model) },
            language: or(r.language, "ja"),
            hold_ms: if r.hold_ms == 0 { 300 } else { r.hold_ms },
            mascot: r.mascot.unwrap_or(true),
            persona: file.with_file_name("persona.md"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let c = Config::from_raw(Raw::default(), Path::new("/home/u"), Path::new("/work"), &config_dir(Path::new("/home/u")).join("config.json"));
        assert!(c.speak && c.mascot);
        assert_eq!(c.shell, "/bin/zsh");
        assert_eq!(c.dir, Path::new("/work"));
        assert_eq!(c.model, "claude-sonnet-5-5");
        assert_eq!((c.aivis_speaker, c.aivis_speed), (888753760, 1.3));
        assert_eq!((c.voice.as_str(), c.voice_rate), ("Kyoko", 260));
        assert_eq!(c.hold_ms, 300);
        assert_eq!(c.whisper_model, Path::new("/home/u/.local/share/whisper/ggml-large-v3-turbo-q5_0.bin"));
        assert_eq!(c.persona, Path::new("/home/u/.config/voice-agent/persona.md"));
    }

    #[test]
    fn keys() {
        let raw: Raw = serde_json::from_str(
            r#"{"dir":"~/src","aivisURL":"http://h:1/","aivisSpeaker":7,"aivisSpeed":1.1,"voiceRate":200,"holdMs":450,"whisperModel":"/m.bin","mascot":false,"speak":false}"#,
        )
        .unwrap();
        let c = Config::from_raw(raw, Path::new("/home/u"), Path::new("/work"), Path::new("/tmp/va-test/config.json"));
        assert!(!c.mascot && !c.speak);
        assert_eq!(c.persona, Path::new("/tmp/va-test/persona.md"), "beside the file in use");
        assert_eq!(c.dir, Path::new("/home/u/src"));
        assert_eq!(c.aivis_url, "http://h:1");
        assert_eq!((c.aivis_speaker, c.voice_rate, c.hold_ms), (7, 200, 450));
        assert_eq!(c.whisper_model, Path::new("/m.bin"));
    }

    #[test]
    fn va_config_names_another_file() {
        let home = Path::new("/home/u");
        assert_eq!(config_path_from(None, home), Path::new("/home/u/.config/voice-agent/config.json"));
        assert_eq!(config_path_from(Some("".into()), home), Path::new("/home/u/.config/voice-agent/config.json"));
        assert_eq!(config_path_from(Some("/tmp/va/config.json".into()), home), Path::new("/tmp/va/config.json"));
    }

    /// README.md's table of the keys lists every key the file is read for,
    /// with the same defaults as the code.
    #[test]
    fn readme_lists_the_keys_and_defaults() {
        let home = Path::new("/home/u");
        let c = Config::from_raw(Raw::default(), home, Path::new("/work"), &config_dir(home).join("config.json"));
        let readme = include_str!("../README.md");
        let default_of = |key: &str| {
            let row = readme.lines().find(|l| l.starts_with(&format!("| `{key}` |"))).unwrap_or_else(|| panic!("README has no row for {key}"));
            row.split('|').nth(2).unwrap().trim().to_string()
        };
        for (key, value) in [
            ("shell", c.shell.clone()),
            ("model", c.model.clone()),
            ("tts", c.tts.clone()),
            ("aivisURL", c.aivis_url.clone()),
            ("aivisSpeaker", c.aivis_speaker.to_string()),
            ("aivisSpeed", c.aivis_speed.to_string()),
            ("voice", c.voice.clone()),
            ("voiceRate", c.voice_rate.to_string()),
            ("speak", c.speak.to_string()),
            ("whisperModel", c.whisper_model.to_string_lossy().replace("/home/u/", "~/")),
            ("language", c.language.clone()),
            ("holdMs", c.hold_ms.to_string()),
            ("mascot", c.mascot.to_string()),
        ] {
            assert_eq!(default_of(key), format!("`{value}`"), "the default of {key}");
        }
        // The two without a fixed value
        assert_eq!(default_of("dir"), "起動したディレクトリ");
        assert!(c.effort.is_empty() && default_of("effort") == "Claude Code の既定");
        // Every key Raw reads is in the table
        let raw = serde_json::to_value(Raw::default()).unwrap();
        for key in raw.as_object().unwrap().keys() {
            default_of(key);
        }
    }
}
