//! Speech-to-text through a local whisper.cpp server, kept running so the
//! model stays loaded between utterances.

use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;

use crate::log::log;
use crate::net;

/// Whisper's usual inventions on silence or noise
fn hallucinations() -> &'static [Regex; 5] {
    static RES: OnceLock<[Regex; 5]> = OnceLock::new();
    RES.get_or_init(|| {
        [
            Regex::new(r"^ご視聴ありがとうございました[。!！]*$").unwrap(),
            Regex::new(r"^ご清聴ありがとうございました[。!！]*$").unwrap(),
            Regex::new(r"^(チャンネル登録|高評価).*お願いします[。!！]*$").unwrap(),
            Regex::new(r"^[(\[（【].*[)\]）】]$").unwrap(),
            Regex::new(r"^[\s、。.,!?！？…・]*$").unwrap(),
        ]
    })
}

/// What whisper heard, tidied; "" for its inventions on silence.
fn clean_transcript(raw: &str) -> String {
    let text = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.trim_end_matches(['、', ',']);
    if hallucinations().iter().any(|re| re.is_match(text)) {
        return String::new();
    }
    text.to_string()
}

#[derive(Default)]
struct State {
    starting: bool, // a launch is under way
    ready: bool,
    err: String,
    port: u16,
    stdin: Option<ChildStdin>,
}

pub struct WhisperServer {
    bin: PathBuf,
    model: PathBuf,
    language: String,
    st: Mutex<State>,
    cond: Condvar,
}

fn free_port() -> std::io::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

impl WhisperServer {
    pub fn new(bin: &Path, model: &Path, language: &str) -> Arc<WhisperServer> {
        Arc::new(WhisperServer {
            bin: bin.to_path_buf(),
            model: model.to_path_buf(),
            language: language.to_string(),
            st: Mutex::default(),
            cond: Condvar::new(),
        })
    }

    pub fn is_ready(&self) -> bool {
        self.st.lock().unwrap_or_else(|e| e.into_inner()).ready
    }

    /// Launches the server if needed and waits until it accepts requests. A
    /// failed launch is retried on the next call.
    pub fn start(&self) -> Result<(), String> {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if st.ready {
                return Ok(());
            }
            if !st.starting {
                break;
            }
            st = self.cond.wait(st).unwrap_or_else(|e| e.into_inner());
            if !st.starting && !st.ready {
                return Err(st.err.clone());
            }
        }
        st.starting = true;
        drop(st);
        let res = self.launch();
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        st.starting = false;
        st.ready = res.is_ok();
        st.err = res.clone().err().unwrap_or_default();
        self.cond.notify_all();
        res
    }

    fn launch(&self) -> Result<(), String> {
        let port = free_port().map_err(|e| e.to_string())?;
        // whisper-server would outlive a crashed voice-agent; run it under a
        // shell that kills it as soon as our end of its stdin closes (exit,
        // crash, kill -9)
        let mut child = Command::new("/bin/sh")
            .args(["-c", r#""$0" "$@" & child=$!; read _; kill $child"#])
            .arg(&self.bin)
            .arg("-m")
            .arg(&self.model)
            .args(["-l", &self.language, "--host", "127.0.0.1", "--port", &port.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let last_line = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = child.stderr.take() {
            let last = last_line.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    log(&format!("whisper: {line}"));
                    *last.lock().unwrap_or_else(|e| e.into_inner()) = line;
                }
            });
        }
        {
            let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
            st.stdin = child.stdin.take();
            st.port = port;
        }
        // Loading the model can take minutes on a machine short of memory; wait
        // as long as the server is alive
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let deadline = Instant::now() + Duration::from_secs(300);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!("whisper-server exited ({status}): {}", last_line.lock().unwrap_or_else(|e| e.into_inner())));
            }
            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                // Reaps the shell when the server ends
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        self.st.lock().unwrap_or_else(|e| e.into_inner()).stdin = None;
        Err("whisper-server did not start within 5 minutes".into())
    }

    pub fn transcribe(&self, wav: &[u8]) -> Result<String, String> {
        self.start()?;
        let port = self.st.lock().unwrap_or_else(|e| e.into_inner()).port;
        transcribe_at(&format!("http://127.0.0.1:{port}/inference"), wav)
    }

    pub fn stop(&self) {
        self.st.lock().unwrap_or_else(|e| e.into_inner()).stdin = None;
    }
}

fn transcribe_at(url: &str, wav: &[u8]) -> Result<String, String> {
    let boundary = format!("va{:x}{:x}", std::process::id(), crate::util::unix_millis());
    let mut body = Vec::with_capacity(wav.len() + 512);
    let mut field = |name: &str, extra: &str, data: &[u8]| {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"{extra}\r\n\r\n").as_bytes());
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    };
    field("file", "; filename=\"speech.wav\"\r\nContent-Type: audio/wav", wav);
    field("response_format", "", b"json");
    field("temperature", "", b"0.0");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    let (status, data) = net::post(url, Some(&format!("multipart/form-data; boundary={boundary}")), &body, Duration::from_secs(120))?;
    if status != 200 {
        return Err(format!("whisper-server HTTP {status}: {}", String::from_utf8_lossy(&data[..data.len().min(200)])));
    }
    let v: serde_json::Value = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
    Ok(clean_transcript(v.get("text").and_then(|t| t.as_str()).unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcripts() {
        assert_eq!(clean_transcript("  テストを\n実行して、 "), "テストを 実行して");
        assert_eq!(clean_transcript("ご視聴ありがとうございました。"), "");
        assert_eq!(clean_transcript("(音楽)"), "");
        assert_eq!(clean_transcript(" 。 "), "");
        assert_eq!(clean_transcript("ls を実行して"), "ls を実行して");
    }

    #[test]
    fn posts_the_recording_as_a_form() {
        let srv = crate::net::testing::serve(|r| {
            let body = String::from_utf8_lossy(&r.body).into_owned();
            let ok = r.url == "/inference"
                && r.content_type.starts_with("multipart/form-data; boundary=")
                && body.contains("name=\"file\"; filename=\"speech.wav\"")
                && body.contains("RIFFdata")
                && body.contains("name=\"response_format\"\r\n\r\njson");
            if ok { (200, r#"{"text":" こんにちは、\n"}"#.as_bytes().to_vec()) } else { (400, b"bad form".to_vec()) }
        });
        assert_eq!(transcribe_at(&format!("{}/inference", srv.url), b"RIFFdata").unwrap(), "こんにちは");
        assert!(transcribe_at(&format!("{}/other", srv.url), b"RIFFdata").unwrap_err().contains("HTTP 400"));
    }
}
