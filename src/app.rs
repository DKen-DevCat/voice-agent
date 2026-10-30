//! The voice agent: mic -> whisper -> claude -> the conversation and speech.
//! Claude works through the tools of agent/tools.rs, in background shells
//! nobody sees; a risky call waits for the user's OK (spoken or y/n), and
//! rewriting git history is refused outright (agent/policy.rs).

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak, mpsc};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::agent::agents::{AGENT_FAILED, AGENT_STOPPED, AgentChange, AgentInfo, AgentTracker};
use crate::agent::claude::{Callbacks, ClaudeSession};
use crate::agent::mcp::{McpServer, Tool, ToolResult};
use crate::agent::policy::{
    APPROVE, Decision, DenyRule, JOB_START, JOB_STATUS, JOB_STOP, JOB_TAIL, SHELL_INPUT, SHELL_LIST, SHELL_NEW, SHELL_RUN, SHELL_SCREEN, ask_reasons, decide,
    load_deny_rules, parse_consent,
};
use crate::agent::prompt;
use crate::agent::tools::{self, ToolHost, with_shell};
use crate::agent::usage::{Usage, UsageTracker};
use crate::audio::capture::{RecError, Recorder};
use crate::audio::playback::PcmPlayer;
use crate::config::Config;
use crate::latency::{Latency, Mark};
use crate::log::log;
use crate::shell::jobs::Jobs;
use crate::shell::registry::{MAIN, Registry, ShellOptions};
use crate::speech::aivis::AivisEngine;
use crate::speech::arbiter::Activity;
use crate::speech::phrases::Phrases;
use crate::speech::stt::WhisperServer;
use crate::speech::tts::{CODE_NOTICE, SentenceChunker, Speaker, split_sentences};
use crate::util::{fmt_elapsed, home_dir, look_path, unix_millis, write_private};
use crate::voice::commands::{self, Command};

const APPROVAL_TIMEOUT: Duration = Duration::from_secs(180);
/// How long a call the user allowed through the approve tool may take to arrive.
const APPROVED_FOR: Duration = Duration::from_secs(600);

/// The notice of everything being stopped (Esc, 「生成もストップ」).
pub const ABORTED: &str = "（すべて止めました）";
/// Said when a reply to a pending approval is neither yes nor no.
const CONFIRM_AGAIN: &str = "承認待ちです。はいか、いいえで答えてください。";
// What voice-agent says itself; the phrase cache keeps it ready (fixed_phrases)
const ASK_HEAD: &str = "確認です。";
const ASK_TAIL: &str = "実行していいですか？";
const ERROR_SAID: &str = "エラーが発生しました。";
const MIC_OFF_SAID: &str = "マイクを切りました。";
const MIC_ON_SAID: &str = "マイクを戻しました。";

/// The tools whose handlers show their own calls in the conversation.
const SELF_NOTED: [&str; 9] = [SHELL_RUN, SHELL_SCREEN, SHELL_LIST, SHELL_NEW, SHELL_INPUT, JOB_START, JOB_STATUS, JOB_TAIL, JOB_STOP];

fn tool_tag(name: &str) -> &'static str {
    match name {
        "Read" => "read",
        "Edit" => "edit",
        "Write" => "write",
        "Glob" => "glob",
        "Grep" => "grep",
        "WebFetch" => "fetch",
        "WebSearch" => "search",
        "TodoWrite" => "todo",
        "Agent" => "agent",
        "TaskStop" => "stop",
        "SendMessage" => "message",
        _ => "tool",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Default)]
pub enum EventKind {
    #[default]
    System,
    User,
    Assistant,    // a piece of the reply being streamed
    AssistantEnd, // the reply closes
    Tool,
    Approval,
    ApprovalResolved,
    Notice,
    Error,
    Status,
    Agent, // a subagent started, moved on or finished
    Usage, // tokens, cost or rate limits changed
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Event {
    pub kind: EventKind,
    pub text: String,
    pub via: String,    // voice / text (User)
    pub tag: String,    // exec / read / edit / blocked ... (Tool)
    pub id: String,     // approval id
    pub reason: String, // why an approval is needed
    pub allow: bool,
    pub state: String, // Status
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub time: i64, // unix milliseconds
}

impl Event {
    fn new(kind: EventKind, text: impl Into<String>) -> Event {
        Event { kind, text: text.into(), ..Event::default() }
    }

    fn tool(tag: &str, text: impl Into<String>) -> Event {
        Event { tag: tag.to_string(), ..Event::new(EventKind::Tool, text) }
    }
}

/// What only tests set.
#[derive(Clone, Debug, Default)]
pub struct Hooks {
    pub tts_out: Option<PathBuf>,     // VA_TTS_OUT: speech goes to audio files instead of the speakers
    pub event_log: Option<PathBuf>,   // VA_EVENT_LOG: every event is appended as a JSON line
    pub test_steps: String,           // VA_TEST_STEPS: [{"at":3000,"text":"..."},{"at":9000,"wav":"/x.wav"}]
    pub latency_log: Option<PathBuf>, // VA_LATENCY_LOG: the latency log goes here instead of the runtime directory
}

impl Hooks {
    pub fn from_env() -> Hooks {
        let path = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        Hooks {
            tts_out: path("VA_TTS_OUT"),
            event_log: path("VA_EVENT_LOG"),
            test_steps: std::env::var("VA_TEST_STEPS").unwrap_or_default(),
            latency_log: path("VA_LATENCY_LOG"),
        }
    }
}

struct Approval {
    id: String,
    answer: mpsc::Sender<(bool, String)>,
    at: Instant,    // when it was raised: an answer has to begin after it
    prompt: String, // what is said for it
}

#[derive(Default)]
struct State {
    approvals: Vec<Approval>,
    /// Calls the user allowed through the approve tool, until they run
    approved: Vec<(String, Instant)>,
    recording: bool,
    transcribing: u32,
    executing: u32,
    assistant_open: bool,
    /// 「止めて」, Esc or talking over a reply: the rest of it is shown, not spoken
    hushed: bool,
    mic_off: bool,
    /// When the recording under way began
    began: Option<Instant>,
    /// What is to be said once the user stops talking (an approval's prompt)
    deferred: Vec<String>,
    last_status: String,
    ap_seq: u32,
    stt_err: String,
    /// Jobs Esc stopped, for Claude to hear of with the next message
    jobs_stopped: Vec<String>,
}

type Deliver = Box<dyn Fn(Vec<Event>) + Send + Sync>;

pub struct Controller {
    cfg: Config,
    hooks: Hooks,
    deliver: Deliver,

    speaker: Arc<Speaker>,
    chunker: Mutex<SentenceChunker>,
    latency: Arc<Latency>,
    stt: Mutex<Option<Arc<WhisperServer>>>,
    rec: Recorder,
    mcp: Mutex<Option<McpServer>>,
    claude: Mutex<Option<Arc<ClaudeSession>>>,
    agents: Arc<AgentTracker>,
    usage: Arc<UsageTracker>,
    shells: Registry,
    jobs: Jobs,
    deny: Vec<DenyRule>,
    runtime_dir: PathBuf,
    level: Arc<AtomicU64>, // mic level (f64 bits)
    st: Mutex<State>,
}

impl Controller {
    /// Events reach the UI through deliver, in order. The shells, jobs and
    /// logs live in runtime_dir.
    pub fn new(cfg: Config, hooks: Hooks, runtime_dir: PathBuf, deliver: Deliver) -> Arc<Controller> {
        Arc::new_cyclic(|weak: &Weak<Controller>| {
            let w = weak.clone();
            let describe = Box::new(move |name: &str, input: &Map<String, Value>| w.upgrade().map(|c| c.describe_tool(name, input)).unwrap_or_default());
            let w = weak.clone();
            let agents = Arc::new(AgentTracker::new(
                Some(describe),
                Some(Box::new(move |a, change| {
                    if let Some(c) = w.upgrade() {
                        c.on_agent_change(a, change);
                    }
                })),
            ));
            let w = weak.clone();
            let usage = Arc::new(UsageTracker::new(Some(Box::new(move |u| {
                if let Some(c) = w.upgrade() {
                    c.emit(Event { usage: Some(u), ..Event::new(EventKind::Usage, "") });
                }
            }))));

            let latency = Arc::new(Latency::new(Some(hooks.latency_log.clone().unwrap_or_else(|| runtime_dir.join("latency.jsonl")))));
            let (mut aivis, mut player, mut phrases) = (None, None, None);
            if cfg.tts == "aivis" {
                let engine = AivisEngine::new(&cfg.aivis_url, cfg.aivis_speaker, cfg.aivis_speed);
                let w = weak.clone();
                let _ = engine.arbiter.activity.set(Box::new(move || w.upgrade().map(|c| c.activity()).unwrap_or_default()));
                let w = weak.clone();
                let _ = engine.on_ready.set(Box::new(move || {
                    if let Some(c) = w.upgrade() {
                        c.speaker.build_phrases(fixed_phrases());
                    }
                }));
                aivis = Some(engine);
                if hooks.tts_out.is_none() {
                    player = Some(Arc::new(PcmPlayer::default()));
                }
                phrases = Some(Phrases::new(runtime_dir.join("phrases")));
            }
            let w = weak.clone();
            let mut speaker = Speaker::new(
                &cfg.voice,
                cfg.voice_rate,
                aivis,
                player,
                hooks.tts_out.clone(),
                cfg.speak,
                Box::new(move || {
                    if let Some(c) = w.upgrade() {
                        c.status();
                    }
                }),
            );
            speaker.phrases = phrases;
            let l = latency.clone();
            speaker.tap = Some(Box::new(move |s| l.sounded(s)));
            let speaker = Arc::new(speaker);
            let chunker = chunker_for(&speaker, &latency);
            let level = Arc::new(AtomicU64::new(0));
            let l = level.clone();
            let shells = Registry::new(ShellOptions { shell: cfg.shell.clone(), dir: cfg.dir.clone(), runtime_dir: runtime_dir.clone() });
            let jobs = Jobs::new(&cfg.shell, cfg.dir.clone(), runtime_dir.clone());
            let deny = load_deny_rules(&cfg.dir);
            Controller {
                cfg,
                hooks,
                deliver,
                speaker,
                chunker: Mutex::new(chunker),
                latency,
                stt: Mutex::new(None),
                rec: Recorder::new(move |v| l.store(v.to_bits(), Ordering::Relaxed)),
                mcp: Mutex::new(None),
                claude: Mutex::new(None),
                agents,
                usage,
                shells,
                jobs,
                deny,
                runtime_dir,
                level,
                st: Mutex::new(State::default()),
            }
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn claude(&self) -> Option<Arc<ClaudeSession>> {
        self.claude.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The main shell's directory (the configured one until it reports one).
    fn cwd(&self) -> String {
        let cwd = self.shells.get(MAIN).map(|s| s.status().cwd).unwrap_or_default();
        if cwd.is_empty() { self.cfg.dir.to_string_lossy().into_owned() } else { cwd }
    }

    pub fn start(self: &Arc<Self>) -> Result<(), String> {
        match look_path("whisper-server") {
            None => {
                self.emit(Event::new(EventKind::Error, "whisper-server が見つかりません (brew install whisper-cpp)。音声入力は使えません。"));
                self.lock().stt_err = "whisper-server がありません".into();
            }
            Some(_) if !self.cfg.whisper_model.is_file() => {
                self.emit(Event::new(EventKind::Error, format!("Whisper モデルがありません: {}", self.cfg.whisper_model.display())));
                self.lock().stt_err = "Whisper モデルがありません".into();
            }
            Some(bin) => {
                let stt = WhisperServer::new(&bin, &self.cfg.whisper_model, &self.cfg.language);
                *self.stt.lock().unwrap_or_else(|e| e.into_inner()) = Some(stt.clone());
                let c = self.clone();
                std::thread::spawn(move || match stt.start() {
                    Ok(()) => log("whisper-server ready"),
                    Err(e) => c.emit(Event::new(EventKind::Error, format!("音声認識を起動できません: {e}"))),
                });
            }
        }

        if let Some(p) = self.speaker.player.clone() {
            std::thread::spawn(move || {
                if let Err(e) = p.prepare() {
                    log(&format!("tts: speakers: {e}"));
                }
            });
        }
        let c = self.clone();
        std::thread::spawn(move || {
            if let Err(e) = c.rec.prepare() {
                log(&format!("audio: {e}"));
            }
        });
        // The main shell is ready before the first command needs it
        let c = self.clone();
        std::thread::spawn(move || {
            if let Err(e) = c.shells.shell(MAIN) {
                c.emit(Event::new(EventKind::Error, format!("裏のシェルを起動できません: {e}")));
            }
        });

        let host: Arc<dyn ToolHost> = self.clone();
        let mut tools = tools::tools(Arc::downgrade(&host));
        tools.push(self.approve_tool());
        let mcp = McpServer::start("va", tools)?;
        log(&format!("MCP server on 127.0.0.1:{}", mcp.port));
        // The config holds the server's token: a file only the user can read,
        // not claude's arguments, which every account on the Mac can see
        let mcp_config = self.mcp_config_path();
        write_private(&mcp_config, mcp.config().as_bytes()).map_err(|e| format!("{}: {e}", mcp_config.display()))?;
        *self.mcp.lock().unwrap_or_else(|e| e.into_inner()) = Some(mcp);

        match look_path("claude") {
            None => self.emit(Event::new(EventKind::Error, "claude コマンドが見つかりません。Claude Code をインストールしてください。")),
            Some(bin) => self.start_claude(bin, &mcp_config),
        }
        // After claude: the arbiter asks about the activity, which looks at the session
        if let Some(a) = self.speaker.aivis.clone() {
            std::thread::spawn(move || a.start());
        }
        self.status();
        self.run_test_steps();
        Ok(())
    }

    fn mcp_config_path(&self) -> PathBuf {
        self.runtime_dir.join("mcp.json")
    }

    fn start_claude(self: &Arc<Self>, bin: PathBuf, mcp_config: &std::path::Path) {
        let args = self.claude_args(mcp_config);
        // A claude started from inside another Claude Code session would refuse to run
        let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(k, _)| {
                let k = k.to_string_lossy();
                k != "CLAUDECODE" && k != "CLAUDE_CODE_ENTRYPOINT" && !k.starts_with("VA_")
            })
            .collect();
        // MCP tools load up front instead of through an extra ToolSearch round trip
        env.push(("ENABLE_TOOL_SEARCH".into(), "false".into()));
        self.open_claude(bin, env, args);
    }

    /// Starts the claude session the conversation goes to.
    fn open_claude(self: &Arc<Self>, bin: PathBuf, env: Vec<(OsString, OsString)>, args: Vec<String>) {
        let w = Arc::downgrade(self);
        let with = move |f: &dyn Fn(&Arc<Controller>)| {
            if let Some(c) = w.upgrade() {
                f(&c);
            }
        };
        let (w1, w2, w3, w4, w5) = (with.clone(), with.clone(), with.clone(), with.clone(), with);
        let cb = Callbacks {
            on_ready: Box::new(move |id, model| {
                log(&format!("claude session {id} ({model})"));
                w1(&|c| c.emit(Event::new(EventKind::System, format!("Claude につながりました // {model}"))));
            }),
            on_text: Box::new(move |t| {
                w2(&|c| {
                    c.lock().assistant_open = true;
                    c.latency.mark_reply(Mark::FirstText);
                    c.emit(Event::new(EventKind::Assistant, t));
                    // Under the chunker's lock, so that hush cannot slip in between
                    let mut chunker = c.chunker.lock().unwrap_or_else(|e| e.into_inner());
                    if !c.lock().hushed {
                        chunker.push(t);
                    }
                })
            }),
            on_text_end: Box::new(move || {
                w3(&|c| {
                    c.chunker.lock().unwrap_or_else(|e| e.into_inner()).flush();
                    // A newline between text blocks of the same reply
                    if c.lock().assistant_open {
                        c.emit(Event::new(EventKind::Assistant, "\n"));
                    }
                })
            }),
            on_tool_use: Box::new(move |name, input| w4(&|c| c.on_tool_use(name, input))),
            on_turn_end: Box::new(move |is_error, result| {
                w5(&|c| {
                    c.chunker.lock().unwrap_or_else(|e| e.into_inner()).flush();
                    c.close_assistant();
                    let hushed = std::mem::take(&mut c.lock().hushed);
                    c.latency.reply_ended(is_error);
                    if is_error {
                        c.emit(Event::new(EventKind::Error, if result.is_empty() { "エラーが発生しました" } else { result }));
                        if !hushed {
                            c.speaker.speak(ERROR_SAID);
                        }
                    }
                    c.status();
                })
            }),
        };
        let mut session = ClaudeSession::new(bin, self.cfg.dir.clone(), env, args, cb);
        session.agents = Some(self.agents.clone());
        session.usage = Some(self.usage.clone());
        let session = Arc::new(session);
        *self.claude.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.clone());
        if let Err(e) = session.warm() {
            self.emit(Event::new(EventKind::Error, format!("claude を起動できません: {e}")));
        }
    }

    /// claude's arguments: the tools, the subagents, the permission handler
    /// and the system prompt; the MCP server is named by its config file.
    fn claude_args(&self, mcp_config: &std::path::Path) -> Vec<String> {
        let system = prompt::system_prompt(&prompt::load_persona(&self.cfg.persona));
        let mcp_config = mcp_config.to_string_lossy();
        let mut args: Vec<String> = [
            "--mcp-config",
            &mcp_config,
            "--strict-mcp-config",
            "--tools",
            "Read,Edit,Write,Glob,Grep,WebFetch,WebSearch,TodoWrite,Agent,TaskStop,SendMessage",
            "--agents",
            &prompt::subagents().to_string(),
            "--disallowedTools",
            "Bash",
            "--permission-mode",
            "acceptEdits",
            "--permission-prompt-tool",
            APPROVE,
            "--append-system-prompt",
            &system,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if !self.cfg.model.is_empty() {
            args.extend(["--model".to_string(), self.cfg.model.clone()]);
        }
        if !self.cfg.effort.is_empty() {
            args.extend(["--effort".to_string(), self.cfg.effort.clone()]);
        }
        args
    }

    fn run_test_steps(self: &Arc<Self>) {
        if self.hooks.test_steps.is_empty() {
            return;
        }
        #[derive(serde::Deserialize)]
        struct Step {
            #[serde(default)]
            at: u64,
            #[serde(default)]
            text: String,
            #[serde(default)]
            wav: String,
        }
        let steps: Vec<Step> = match serde_json::from_str(&self.hooks.test_steps) {
            Ok(s) => s,
            Err(e) => return self.emit(Event::new(EventKind::Error, format!("VA_TEST_STEPS: {e}"))),
        };
        for step in steps {
            let c = self.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(step.at));
                if step.wav.is_empty() {
                    c.handle_user_text(&step.text, "text");
                } else if let Ok(data) = std::fs::read(&step.wav) {
                    c.transcribe_wav(&data);
                }
            });
        }
    }

    /// What the user just said, as a WAV recording (a test step).
    fn transcribe_wav(self: &Arc<Self>, wav: &[u8]) {
        let now = Instant::now();
        let turn = self.latency.begin("voice", now);
        self.lock().transcribing += 1;
        self.status();
        self.transcribe_counted(wav, now, turn);
    }

    // --- voice ---

    /// Opens the microphone (Space held).
    pub fn start_recording(self: &Arc<Self>) {
        if self.lock().mic_off {
            self.emit(Event::new(EventKind::Notice, "（マイクはオフです。m キーで戻せます）"));
            return;
        }
        // Talking over the assistant silences the rest of its reply (the reply
        // itself goes on, shown)
        self.hush();
        self.level.store(0f64.to_bits(), Ordering::Relaxed);
        let t0 = Instant::now();
        if let Err(e) = self.rec.start() {
            self.emit(Event::new(EventKind::Error, format!("マイクを開けません: {e}")));
            self.status();
            return;
        }
        log(&format!("mic open in {:?}", t0.elapsed()));
        {
            let mut st = self.lock();
            st.recording = true;
            st.began = Some(t0);
        }
        self.status();
    }

    /// Closes the microphone and sends what was said (cancel: throws it away).
    pub fn stop_recording(self: &Arc<Self>, cancel: bool) {
        let released = Instant::now();
        let (began, deferred) = {
            let mut st = self.lock();
            if !st.recording {
                return;
            }
            st.recording = false;
            if !cancel {
                // Straight from listening to transcribing
                st.transcribing += 1;
            }
            (st.began.take().unwrap_or_else(Instant::now), std::mem::take(&mut st.deferred))
        };
        let turn = if cancel { 0 } else { self.latency.begin("voice", released) };
        self.status();
        for text in deferred {
            self.speaker.speak(&text);
        }
        let c = self.clone();
        std::thread::spawn(move || {
            let recorded = c.rec.stop();
            if cancel {
                return;
            }
            let (notice, end) = match recorded {
                Some((Ok(wav), secs)) if secs >= 0.35 => return c.transcribe_counted(&wav, began, turn),
                Some((Err(RecError::Silence), _)) => (Event::new(EventKind::Notice, "（声が聞こえませんでした）"), "no-speech"),
                Some((Err(e), _)) => (Event::new(EventKind::Error, e.to_string()), "error"),
                _ => (Event::new(EventKind::Notice, "（短すぎて聞き取れませんでした）"), "no-speech"),
            };
            c.latency.finish(turn, end);
            c.lock().transcribing -= 1;
            c.status();
            c.emit(notice);
        });
    }

    pub fn recording(&self) -> bool {
        self.lock().recording
    }

    /// The microphone level 0..1 while recording.
    pub fn level(&self) -> f64 {
        f64::from_bits(self.level.load(Ordering::Relaxed))
    }

    /// Turns the microphone off (the device is let go of) or on again.
    pub fn set_mic(self: &Arc<Self>, on: bool) {
        let changed = std::mem::replace(&mut self.lock().mic_off, !on) == on;
        if !changed {
            return;
        }
        if on {
            // The device is set up again by the next recording
            self.emit(Event::new(EventKind::Notice, "（マイクを戻しました）"));
            self.speaker.speak(MIC_ON_SAID);
        } else {
            self.stop_recording(true);
            self.rec.close();
            self.emit(Event::new(EventKind::Notice, "（マイクを切りました。m キーで戻せます）"));
            self.speaker.speak(MIC_OFF_SAID);
        }
        self.status();
    }

    pub fn mic_on(&self) -> bool {
        !self.lock().mic_off
    }

    /// Expects State.transcribing to count this request already. began: when
    /// the user started saying it; turn: its turn in the latency log.
    fn transcribe_counted(self: &Arc<Self>, wav: &[u8], began: Instant, turn: u64) {
        let stt = self.stt.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let res = stt.as_ref().map(|s| {
            if !s.is_ready() {
                self.emit(Event::new(EventKind::Notice, "（音声認識エンジンを起動中です。最初だけ時間がかかります）"));
                // stt_start comes once it is up: the turn says it waited for that
                self.latency.note(turn, "stt_cold", true);
            }
            self.latency.note(turn, "stt", "final");
            s.start().and_then(|()| {
                self.latency.mark(turn, Mark::SttStart);
                let res = s.transcribe(wav);
                self.latency.mark(turn, Mark::SttEnd);
                res
            })
        });
        let stt_err = {
            let mut st = self.lock();
            st.transcribing = st.transcribing.saturating_sub(1);
            st.stt_err.clone()
        };
        self.status();
        match res {
            None => {
                self.latency.finish(turn, "error");
                let why = if stt_err.is_empty() { String::new() } else { format!("（{stt_err}）") };
                self.emit(Event::new(EventKind::Error, format!("音声認識が使えません。{why}")));
            }
            Some(Err(e)) => {
                self.latency.finish(turn, "error");
                self.emit(Event::new(EventKind::Error, format!("音声認識エラー: {e}")));
            }
            Some(Ok(text)) if text.is_empty() => {
                self.latency.finish(turn, "no-speech");
                self.emit(Event::new(EventKind::Notice, "（聞き取れませんでした）"));
            }
            Some(Ok(text)) => self.handle_heard(&text, "voice", began, turn),
        }
    }

    // --- conversation ---

    /// What the user typed (or a test step said), just now.
    pub fn handle_user_text(self: &Arc<Self>, text: &str, via: &str) {
        let now = Instant::now();
        let turn = if text.trim().is_empty() { 0 } else { self.latency.begin(via, now) };
        self.handle_heard(text, via, now, turn);
    }

    /// What the user said or typed, beginning at began (turn: its turn in
    /// the latency log): the answer to the approval that waits, a spoken
    /// command, or a message for Claude.
    fn handle_heard(self: &Arc<Self>, text: &str, via: &str, began: Instant, turn: u64) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.emit(Event { via: via.to_string(), ..Event::new(EventKind::User, text) });

        let cmd = commands::parse(text);
        let first = self.lock().approvals.first().map(|a| (a.id.clone(), a.at));
        if let Some((id, at)) = first {
            self.latency.finish(turn, "approval");
            // Only a clear yes or no answers it, and only one begun after it
            // was raised (not a request that was under way). 「生成もストップ」
            // refuses it with everything else; 「静かにして」 only quiets.
            let answer = if at <= began && cmd != Some(Command::StopAll) { parse_consent(text) } else { None };
            match (answer, cmd) {
                (Some(allow), _) => self.answer(&id, allow),
                (None, Some(Command::StopAll)) => self.abort(),
                (None, Some(Command::MicOff)) => self.set_mic(false),
                (None, Some(Command::Stop)) => self.hush(),
                (None, None) => {
                    self.emit(Event::new(EventKind::Notice, format!("（{CONFIRM_AGAIN}）")));
                    self.announce(CONFIRM_AGAIN);
                }
            }
            return;
        }

        if let Some(cmd) = cmd {
            self.latency.finish(turn, "command");
            match cmd {
                Command::Stop => self.stop(),
                Command::StopAll => self.abort(),
                Command::MicOff => self.set_mic(false),
            }
            return;
        }

        let Some(claude) = self.claude() else {
            self.latency.finish(turn, "error");
            return self.emit(Event::new(EventKind::Error, "Claude に接続できません。"));
        };
        // A reply whose end is still being handled counts as under way
        let busy = claude.busy() || self.latency.answering();
        if busy {
            // It goes in after the reply under way: the times would be that reply's
            self.latency.finish(turn, "busy");
        } else {
            // A reply silenced before is over; this one is heard
            self.lock().hushed = false;
        }
        let msg = format!("{text}\n\n<va_context>{}</va_context>", self.context());
        match claude.send(&msg) {
            Ok(()) => {
                if !busy {
                    self.latency.replying_to(turn);
                }
                self.latency.mark(turn, Mark::Sent);
            }
            Err(e) => {
                self.latency.finish(turn, "error");
                self.emit(Event::new(EventKind::Error, format!("Claude に送れません: {e}")));
            }
        }
        // Whisper is done with it, and the reply is a few seconds off: the time
        // to get the voice model resident (once an utterance)
        self.speaker.warm();
        self.status();
    }

    /// What else is going on, for the arbiter: whether the engine may be
    /// warmed up or used for the phrase cache.
    fn activity(&self) -> Activity {
        let (talking, transcribing) = {
            let st = self.lock();
            (st.recording, st.transcribing > 0)
        };
        let agents = self.agents.running() > 0;
        let busy = talking || transcribing || agents || self.claude().is_some_and(|c| c.busy()) || self.speaker.speaking();
        Activity { transcribing, agents, busy }
    }

    /// 「止めて」: silence for the rest of the reply, pending approvals refused,
    /// Ctrl-C to what runs in the shells. Claude's reply goes on (shown).
    pub fn stop(self: &Arc<Self>) {
        self.hush();
        self.deny_all("The user said stop.");
        self.shells.interrupt_all();
        self.emit(Event::new(EventKind::Notice, "（読み上げとコマンドを止めました）"));
        self.status();
    }

    /// Esc and 「生成もストップ」: that, and the reply is interrupted too, and
    /// the jobs are stopped.
    pub fn abort(self: &Arc<Self>) {
        self.hush();
        self.deny_all("The user aborted.");
        self.shells.interrupt_all();
        if let Some(c) = self.claude() {
            c.interrupt();
        }
        // Shown as stopped; a subagent Claude Code keeps running comes back as RUNNING
        self.agents.stop_all();
        self.emit(Event::new(EventKind::Notice, ABORTED));
        self.status();
        // Waits up to STOP_GRACE for what does not end at once (the key
        // handler calls this on a thread of its own)
        let stopped = self.jobs.stop_all();
        self.lock().jobs_stopped.extend(stopped.iter().map(|j| format!("#{} {}", j.id, j.command.trim())));
    }

    /// What Claude is told with a message: where the main shell is, and the
    /// jobs Esc has stopped since the last message (Claude would take them
    /// to be running).
    fn context(&self) -> String {
        let stopped = std::mem::take(&mut self.lock().jobs_stopped);
        let mut ctx = format!("main shell cwd: {}", self.cwd());
        if !stopped.is_empty() {
            ctx.push_str(&format!("\njobs the user stopped with Esc: {}", stopped.join(", ")));
        }
        ctx
    }

    /// Silences the speech and the rest of the reply; the half sentence the
    /// chunker holds is dropped.
    fn hush(&self) {
        {
            let mut chunker = self.chunker.lock().unwrap_or_else(|e| e.into_inner());
            self.lock().hushed = true;
            *chunker = chunker_for(&self.speaker, &self.latency);
        }
        self.speaker.stop();
        self.latency.finish_reply("stopped");
    }

    fn deny_all(&self, message: &str) {
        let ids: Vec<String> = self.lock().approvals.iter().map(|a| a.id.clone()).collect();
        for id in ids {
            self.resolve_approval(&id, false, message);
        }
    }

    /// Answers the oldest pending approval from the keyboard.
    pub fn answer_approval(&self, allow: bool) -> bool {
        let Some(id) = self.lock().approvals.first().map(|a| a.id.clone()) else { return false };
        self.answer(&id, allow);
        true
    }

    /// The user's yes or no to an approval. The rest of the turn is heard
    /// again (talking to answer silenced it), and the engine is warmed for
    /// the reply that comes after it, as for anything sent to Claude.
    fn answer(&self, id: &str, allow: bool) {
        self.lock().hushed = false;
        self.resolve_approval(id, allow, "The user denied this action.");
        self.speaker.warm();
    }

    /// How many subagents and jobs are at work.
    pub fn running(&self) -> (usize, usize) {
        (self.agents.running(), self.jobs.running())
    }

    fn on_tool_use(&self, name: &str, input: &Map<String, Value>) {
        if SELF_NOTED.contains(&name) {
            return;
        }
        let (tag, detail) = self.describe_tool(name, input);
        if !tag.is_empty() {
            self.emit(Event::tool(&tag, detail));
        }
    }

    /// Passes a subagent's state on; when it finishes (or turns out to be
    /// still running), a line in the conversation says so.
    fn on_agent_change(&self, a: AgentInfo, change: AgentChange) {
        let line = match change {
            AgentChange::Finished => {
                let mark = match a.state.as_str() {
                    AGENT_FAILED => "✖",
                    AGENT_STOPPED => "■",
                    _ => "✓",
                };
                format!("{mark} {} {} · {} tools · {}", a.typ, a.description, a.tool_calls, fmt_elapsed(a.elapsed(unix_millis())))
            }
            AgentChange::Revived => format!("● {} {} · まだ動いています", a.typ, a.description),
            AgentChange::Updated => String::new(),
        };
        self.emit(Event { agent: Some(a), ..Event::new(EventKind::Agent, "") });
        if !line.is_empty() {
            self.emit(Event::tool("agent", line));
        }
        self.status();
    }

    /// A tool call as one short line: a tag (see tool_tag) and what it
    /// touches. The tag is empty for calls not worth showing.
    fn describe_tool(&self, name: &str, input: &Map<String, Value>) -> (String, String) {
        let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or_default();
        let id = || input.get("id").map(|v| format!("#{}", v.as_str().map_or_else(|| v.to_string(), str::to_string))).unwrap_or_default();
        let lines = |text: &str| {
            let text = text.strip_suffix('\n').unwrap_or(text);
            if text.is_empty() { 0 } else { text.matches('\n').count() + 1 }
        };
        let (tag, detail) = match name {
            APPROVE => return (String::new(), String::new()),
            SHELL_RUN => ("exec", with_shell(s("shell"), s("command").trim())),
            SHELL_INPUT => ("input", with_shell(s("shell"), &format!("{:?}", s("text")))),
            SHELL_SCREEN => ("screen", format!("{} の画面を読む", if s("shell").is_empty() { MAIN } else { s("shell") })),
            SHELL_LIST => ("shells", "シェルの一覧を読む".to_string()),
            SHELL_NEW => ("shell", format!("新しいシェル {}", s("name"))),
            JOB_START => ("job", s("command").trim().to_string()),
            JOB_STATUS | JOB_TAIL | JOB_STOP => ("job", format!("{} {}", name.trim_start_matches("mcp__va__"), id()).trim().to_string()),
            "Read" => ("read", self.short(s("file_path"))),
            "Edit" => ("edit", format!("{} +{} -{}", self.short(s("file_path")), lines(s("new_string")), lines(s("old_string")))),
            "Write" => ("write", format!("{} ({} lines)", self.short(s("file_path")), lines(s("content")))),
            "Glob" | "Grep" => (tool_tag(name), s("pattern").to_string()),
            "WebFetch" => ("fetch", s("url").to_string()),
            "WebSearch" => ("search", s("query").to_string()),
            "TodoWrite" => ("todo", format!("{} items", input.get("todos").and_then(Value::as_array).map_or(0, Vec::len))),
            "Agent" => ("agent", format!("{} {}", s("subagent_type"), s("description")).trim().to_string()),
            "TaskStop" => ("stop", s("task_id").to_string()),
            "SendMessage" => ("message", s("to").to_string()),
            _ => (tool_tag(name), name.to_string()),
        };
        (tag.to_string(), detail)
    }

    /// A path as the conversation shows it: from the main shell's directory
    /// or from home.
    fn short(&self, p: &str) -> String {
        if p.is_empty() {
            return String::new();
        }
        let cwd = self.cwd();
        if !cwd.is_empty()
            && let Some(rest) = p.strip_prefix(&format!("{cwd}/"))
        {
            return rest.to_string();
        }
        let home = home_dir();
        match p.strip_prefix(&format!("{}/", home.to_string_lossy())) {
            Some(rest) if !home.as_os_str().is_empty() => format!("~/{rest}"),
            _ => p.to_string(),
        }
    }

    // --- approvals ---

    fn approve_tool(self: &Arc<Self>) -> Tool {
        let w = Arc::downgrade(self);
        Tool {
            name: "approve",
            description: "Permission prompt handler used by voice-agent (not for direct use).",
            input_schema: json!({
                "type": "object",
                "properties": {"tool_name": {"type": "string"}, "input": {"type": "object"}, "tool_use_id": {"type": "string"}},
                "required": ["tool_name", "input"],
            }),
            handler: Box::new(move |args| match w.upgrade() {
                Some(c) => ToolResult::ok(c.approve(args).to_string()),
                None => ToolResult::error("Error: shutting down"),
            }),
        }
    }

    /// Answers claude's permission prompt for a tool call.
    fn approve(&self, args: &Map<String, Value>) -> Value {
        let name = args.get("tool_name").and_then(Value::as_str).unwrap_or_default();
        let input = args.get("input").and_then(Value::as_object).cloned().unwrap_or_default();
        match self.check(name, &input, false) {
            Ok(()) => json!({"behavior": "allow", "updatedInput": input}),
            Err(message) => json!({"behavior": "deny", "message": message}),
        }
    }

    /// The policy's verdict on a call, asking the user when it says so. A
    /// call allowed that way through the approve tool (prompt) is remembered,
    /// so that the tool itself does not ask again when it runs (!prompt).
    fn check(&self, tool: &str, input: &Map<String, Value>, from_tool: bool) -> Result<(), String> {
        let decision = decide(tool, input, &self.deny);
        let reason = match decision {
            Decision::Allow => return Ok(()),
            Decision::Deny(reason) => {
                self.emit(Event::tool("blocked", self.summary(tool, input)));
                return Err(reason);
            }
            Decision::Ask(reason) => reason,
        };
        let key = approval_key(tool, input);
        if from_tool {
            let mut st = self.lock();
            st.approved.retain(|(_, at)| at.elapsed() < APPROVED_FOR);
            if let Some(i) = st.approved.iter().position(|(k, _)| *k == key) {
                st.approved.remove(i);
                return Ok(());
            }
        }
        let (allowed, message) = self.ask_approval(&self.summary(tool, input), &reason);
        if !allowed {
            return Err(message);
        }
        if !from_tool {
            self.lock().approved.push((key, Instant::now()));
        }
        Ok(())
    }

    /// What a call does, as the approval shows it.
    fn summary(&self, tool: &str, input: &Map<String, Value>) -> String {
        let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or_default();
        match tool {
            SHELL_RUN | JOB_START => with_shell(s("shell"), s("command")),
            SHELL_INPUT => with_shell(s("shell"), &format!("入力 {:?}", s("text"))),
            _ => {
                let target = ["file_path", "path", "url"].iter().map(|k| s(k)).find(|v| !v.is_empty()).unwrap_or_default();
                format!("{tool} {}", self.short(target)).trim().to_string()
            }
        }
    }

    /// Asks the user and waits for the answer: (allowed, message for claude).
    fn ask_approval(&self, summary: &str, reason: &str) -> (bool, String) {
        let (tx, rx) = mpsc::channel();
        let prompt = ask_prompt(reason);
        let (id, first) = {
            let mut st = self.lock();
            st.ap_seq += 1;
            let id = format!("ap{}", st.ap_seq);
            st.approvals.push(Approval { id: id.clone(), answer: tx, at: Instant::now(), prompt: prompt.clone() });
            (id, st.approvals.len() == 1)
        };
        // The reply waits for the user now: its times would be theirs
        self.latency.finish_reply("approval");
        self.emit(Event { id: id.clone(), reason: reason.to_string(), ..Event::new(EventKind::Approval, summary) });
        // Approvals are answered oldest first: one waiting behind another is
        // said when its turn comes
        if first {
            self.announce(&prompt);
        }
        self.status();
        match rx.recv_timeout(APPROVAL_TIMEOUT) {
            Ok(answer) => answer,
            Err(_) => {
                self.resolve_approval(&id, false, "No answer from the user in time.");
                (false, "No answer from the user in time.".to_string())
            }
        }
    }

    fn resolve_approval(&self, id: &str, allow: bool, message: &str) {
        let (approval, next) = {
            let mut st = self.lock();
            let i = st.approvals.iter().position(|a| a.id == id);
            let a = i.map(|i| st.approvals.remove(i));
            // The one behind it is the one an answer goes to now
            let next = if i == Some(0) { st.approvals.first().map(|n| n.prompt.clone()) } else { None };
            (a, next)
        };
        let Some(a) = approval else { return };
        let message = if message.is_empty() { "The user denied this action." } else { message };
        self.emit(Event { id: id.to_string(), allow, ..Event::new(EventKind::ApprovalResolved, "") });
        let _ = a.answer.send((allow, message.to_string()));
        if let Some(prompt) = next {
            self.announce(&prompt);
        }
        self.status();
    }

    /// Says text now, or once the user stops talking.
    fn announce(&self, text: &str) {
        {
            let mut st = self.lock();
            if st.recording {
                st.deferred.push(text.to_string());
                return;
            }
        }
        self.speaker.speak(text);
    }

    fn close_assistant(&self) {
        let was = std::mem::take(&mut self.lock().assistant_open);
        if was {
            self.emit(Event::new(EventKind::AssistantEnd, ""));
        }
    }

    /// What the agent is doing right now.
    pub fn status_now(&self) -> &'static str {
        let (rec, tr, ap, ex, mic_off) = {
            let st = self.lock();
            (st.recording, st.transcribing, st.approvals.len(), st.executing, st.mic_off)
        };
        if rec {
            "LISTENING"
        } else if tr > 0 {
            "TRANSCRIBING"
        } else if ap > 0 {
            "CONFIRM"
        } else if ex > 0 {
            "EXECUTING"
        } else if self.claude().is_some_and(|c| c.busy()) {
            "THINKING"
        } else if self.speaker.speaking() {
            "SPEAKING"
        } else if self.agents.running() > 0 {
            "AGENTS"
        } else if mic_off {
            "MIC_OFF"
        } else {
            "STANDBY"
        }
    }

    fn status(&self) {
        let s = self.status_now();
        let changed = {
            let mut st = self.lock();
            let changed = st.last_status != s;
            st.last_status = s.to_string();
            changed
        };
        if changed {
            self.emit(Event { state: s.to_string(), ..Event::new(EventKind::Status, "") });
        }
    }

    fn emit(&self, mut e: Event) {
        e.time = unix_millis();
        if e.kind == EventKind::Tool || e.kind == EventKind::Approval {
            self.close_assistant();
        }
        if let Some(path) = &self.hooks.event_log
            && let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(path)
            && let Ok(mut line) = serde_json::to_vec(&e)
        {
            line.push(b'\n');
            let _ = f.write_all(&line);
        }
        (self.deliver)(vec![e]);
    }

    /// Stops everything voice-agent started: claude, the engines, the
    /// shells and the jobs.
    pub fn close(&self) {
        self.latency.finish_all("quit");
        if let Some(c) = self.claude() {
            c.stop();
        }
        if let Some(s) = self.stt.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            s.stop();
        }
        self.speaker.stop();
        if let Some(p) = &self.speaker.phrases {
            p.stop();
        }
        if let Some(a) = &self.speaker.aivis {
            a.stop();
        }
        if let Some(p) = &self.speaker.player {
            p.close();
        }
        self.rec.close();
        if let Some(m) = self.mcp.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            m.stop();
        }
        let _ = std::fs::remove_file(self.mcp_config_path());
        self.shells.close_all();
        self.jobs.stop_all();
    }
}

/// Cuts the reply into sentences for speaker.
fn chunker_for(speaker: &Arc<Speaker>, latency: &Arc<Latency>) -> SentenceChunker {
    let (s1, s2, l) = (speaker.clone(), speaker.clone(), latency.clone());
    SentenceChunker::new(
        Box::new(move |s| {
            let turn = l.replying();
            // Only speech that is queued will sound (or be skipped) and end the
            // turn; marked before, since it may sound at once
            if s1.is_enabled() {
                l.mark(turn, Mark::FirstChunk);
            }
            s1.speak_reply(s, turn);
        }),
        Some(Box::new(move || s2.eager_first())),
    )
}

/// What is said for an approval: each reason a sentence of its own, so that
/// every part of it is in the phrase cache.
fn ask_prompt(reason: &str) -> String {
    let reasons: String = reason.split('、').map(|r| format!("{r}。")).collect();
    format!("{ASK_HEAD}{reasons}{ASK_TAIL}")
}

/// What voice-agent says itself, sentence by sentence as speech goes: the
/// phrase cache keeps these ready.
fn fixed_phrases() -> Vec<String> {
    let mut texts: Vec<String> =
        [ASK_HEAD, ASK_TAIL, CONFIRM_AGAIN, ERROR_SAID, MIC_OFF_SAID, MIC_ON_SAID, CODE_NOTICE].iter().flat_map(|t| split_sentences(t)).collect();
    texts.extend(ask_reasons().iter().map(|r| format!("{r}。")));
    texts
}

/// Identifies a call: the tool and its exact input.
fn approval_key(tool: &str, input: &Map<String, Value>) -> String {
    format!("{tool} {}", Value::Object(input.clone()))
}

impl ToolHost for Controller {
    fn shells(&self) -> &Registry {
        &self.shells
    }

    fn jobs(&self) -> &Jobs {
        &self.jobs
    }

    fn note(&self, tag: &str, detail: &str) {
        self.emit(Event::tool(tag, detail));
    }

    fn executing(&self, on: bool) {
        {
            let mut st = self.lock();
            st.executing = if on { st.executing + 1 } else { st.executing.saturating_sub(1) };
        }
        self.status();
    }

    fn permit(&self, tool: &str, input: &Map<String, Value>) -> Result<(), String> {
        self.check(tool, input, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn controller() -> (Arc<Controller>, Arc<Mutex<Vec<Event>>>) {
        controller_with(Hooks::default(), Path::new("/work"))
    }

    /// Speech is off ("speak": false), and there is no claude until a test
    /// opens one (in dir, which then has to be there).
    fn controller_with(hooks: Hooks, dir: &Path) -> (Arc<Controller>, Arc<Mutex<Vec<Event>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let ev = events.clone();
        let mut cfg = Config::load(Path::new("/nonexistent-home"));
        cfg.tts = "say".into();
        cfg.speak = false;
        cfg.dir = dir.to_path_buf();
        let rt = std::env::temp_dir().join(format!("va-app-{}-{}", std::process::id(), unix_millis()));
        (Controller::new(cfg, hooks, rt, Box::new(move |evs| ev.lock().unwrap().extend(evs))), events)
    }

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    fn wait_status(c: &Controller, want: &str) {
        for _ in 0..300 {
            if c.status_now() == want {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("status is {}, want {want}", c.status_now());
    }

    fn kinds(events: &Mutex<Vec<Event>>, kind: EventKind) -> Vec<Event> {
        events.lock().unwrap().iter().filter(|e| e.kind == kind).cloned().collect()
    }

    #[test]
    fn approvals_are_asked_and_answered() {
        let (c, events) = controller();
        // Reading is allowed outright, rewriting history is refused without asking
        assert_eq!(c.approve(&args(json!({"tool_name": "Read", "input": {"file_path": "/x"}})))["behavior"], "allow");
        let v = c.approve(&args(json!({"tool_name": SHELL_RUN, "input": {"command": "git reset --hard"}})));
        assert!(v["behavior"] == "deny" && v["message"].as_str().unwrap().contains("fixed policy"), "{v}");
        assert_eq!(c.status_now(), "STANDBY");

        // A risky command waits for the user; "はい" allows it
        let c2 = c.clone();
        let asked = std::thread::spawn(move || c2.approve(&args(json!({"tool_name": SHELL_RUN, "input": {"command": "git push origin main"}}))));
        wait_status(&c, "CONFIRM");
        // Neither yes nor no: it keeps waiting
        c.handle_user_text("それより先にテストして", "voice");
        assert_eq!(c.status_now(), "CONFIRM");
        assert!(!asked.is_finished());
        c.handle_user_text("はい、お願い", "voice");
        let v = asked.join().unwrap();
        assert_eq!((v["behavior"].as_str(), v["updatedInput"]["command"].as_str()), (Some("allow"), Some("git push origin main")));
        wait_status(&c, "STANDBY");

        // The keyboard can refuse
        let c2 = c.clone();
        let asked = std::thread::spawn(move || c2.approve(&args(json!({"tool_name": "Write", "input": {"file_path": "/etc/hosts"}}))));
        wait_status(&c, "CONFIRM");
        assert!(c.answer_approval(false));
        let v = asked.join().unwrap();
        assert_eq!((v["behavior"].as_str(), v["message"].as_str()), (Some("deny"), Some("The user denied this action.")));
        assert!(!c.answer_approval(true), "nothing left to answer");

        let approval = kinds(&events, EventKind::Approval).remove(0);
        assert_eq!((approval.text.as_str(), approval.reason.as_str()), ("git push origin main", "リモートへ push します"));
        assert!(kinds(&events, EventKind::Notice).iter().any(|e| e.text.contains("はいか、いいえ")), "the user is told how to answer");
        assert!(kinds(&events, EventKind::Tool).iter().any(|e| e.tag == "blocked" && e.text == "git reset --hard"));
        assert!(kinds(&events, EventKind::Status).iter().any(|e| e.state == "CONFIRM"));
    }

    /// A call allowed through the approve tool runs without a second
    /// question; one that never went through it is asked when it runs; a
    /// forbidden one is refused there too.
    #[test]
    fn tools_check_the_policy_when_they_run() {
        let (c, events) = controller();
        let push = args(json!({"command": "git push origin main"}));
        let c2 = c.clone();
        let p = push.clone();
        let asked = std::thread::spawn(move || c2.approve(&args(json!({"tool_name": SHELL_RUN, "input": p}))));
        wait_status(&c, "CONFIRM");
        c.answer_approval(true);
        assert_eq!(asked.join().unwrap()["behavior"], "allow");
        assert_eq!(c.permit(SHELL_RUN, &push), Ok(()), "allowed once already");
        assert_eq!(kinds(&events, EventKind::Approval).len(), 1);

        // The same command again, straight to the tool: asked now
        let c2 = c.clone();
        let p = push.clone();
        let ran = std::thread::spawn(move || c2.permit(SHELL_RUN, &p));
        wait_status(&c, "CONFIRM");
        c.handle_user_text("いいえ", "voice");
        assert_eq!(ran.join().unwrap(), Err("The user denied this action.".to_string()));
        assert_eq!(kinds(&events, EventKind::Approval).len(), 2);

        assert!(c.permit(SHELL_RUN, &args(json!({"command": "git commit --amend -m x"}))).unwrap_err().contains("fixed policy"));
        assert_eq!(c.permit(SHELL_RUN, &args(json!({"command": "ls"}))), Ok(()));
        assert_eq!(kinds(&events, EventKind::Approval).len(), 2, "neither asked");
    }

    /// Esc (and 「生成もストップ」) stops the jobs too; 「止めて」 leaves them be.
    #[test]
    fn esc_stops_the_jobs() {
        let dir = std::env::temp_dir().join(format!("va-jobs-{}-{}", std::process::id(), unix_millis()));
        std::fs::create_dir_all(&dir).unwrap();
        let (c, _) = controller_with(Hooks::default(), &dir);
        let job = c.jobs.start("sleep 20", None).unwrap();
        c.stop();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(c.running().1, 1, "「止めて」 leaves the job running");
        c.abort();
        wait_for("the job is stopped", || c.running().1 == 0);
        // Claude hears of it with the next message, once
        assert!(c.context().contains(&format!("jobs the user stopped with Esc: #{} sleep 20", job.id)));
        assert!(!c.context().contains("jobs the user stopped"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&c.runtime_dir);
    }

    #[test]
    fn spoken_commands_act_at_once() {
        let (c, events) = controller();
        // With nothing waiting, 「止めて」 quiets the rest of the reply and leaves Claude alone
        c.handle_user_text("止めて。", "voice");
        assert!(c.lock().hushed, "the rest of the reply is not spoken");
        assert!(kinds(&events, EventKind::Notice).iter().any(|e| e.text.contains("止めました")));

        c.handle_user_text("生成もストップ", "voice");
        assert!(kinds(&events, EventKind::Notice).iter().any(|e| e.text == ABORTED));

        c.handle_user_text("マイクを切って", "voice");
        assert!(!c.mic_on());
        wait_status(&c, "MIC_OFF");
        c.start_recording();
        assert!(!c.recording(), "no recording while the mic is off");
        c.set_mic(true);
        assert!(c.mic_on());
        wait_status(&c, "STANDBY");

        // None of them went to Claude: there is none, which would be an error
        assert!(kinds(&events, EventKind::Error).is_empty());
        c.handle_user_text("サーバーを止めて", "voice");
        assert_eq!(kinds(&events, EventKind::Error).len(), 1, "a request, not a command");
    }

    /// While an approval waits: 「止めて」 is a no, 「静かにして」 only quiets the
    /// speech, and 「生成もストップ」 refuses it with everything else.
    #[test]
    fn spoken_commands_while_an_approval_waits() {
        let (c, _) = controller();
        let ask = |cmd: &'static str| {
            let c2 = c.clone();
            std::thread::spawn(move || c2.approve(&args(json!({"tool_name": SHELL_RUN, "input": {"command": cmd}}))))
        };
        let asked = ask("gh pr create");
        wait_status(&c, "CONFIRM");
        c.handle_user_text("止めて。", "voice");
        assert_eq!(asked.join().unwrap()["message"], "The user denied this action.");

        let asked = ask("gh pr create");
        wait_status(&c, "CONFIRM");
        c.handle_user_text("静かにして", "voice");
        assert!(c.lock().hushed && !asked.is_finished(), "it keeps waiting");
        c.handle_user_text("生成もストップ", "voice");
        assert_eq!(asked.join().unwrap()["message"], "The user aborted.");
    }

    /// An utterance that began before the approval was raised (a request
    /// under way, 「はい、次は…」) does not answer it, even if it is a yes.
    #[test]
    fn an_answer_begins_after_the_question() {
        let (c, events) = controller();
        let before = Instant::now();
        let c2 = c.clone();
        let asked = std::thread::spawn(move || c2.approve(&args(json!({"tool_name": SHELL_RUN, "input": {"command": "git push origin main"}}))));
        wait_status(&c, "CONFIRM");
        c.handle_heard("はい", "voice", before, 0);
        assert!(!asked.is_finished(), "said before the question");
        assert!(kinds(&events, EventKind::Notice).iter().any(|e| e.text.contains("はいか、いいえ")));
        c.handle_heard("はい", "voice", Instant::now(), 0);
        assert_eq!(asked.join().unwrap()["behavior"], "allow");
    }

    /// Two approvals at once (e.g. two subagents): the answer goes to the
    /// older one, the one the UI shows, then to the next.
    #[test]
    fn approvals_are_answered_in_turn() {
        let (c, events) = controller();
        let ask = |cmd: &'static str| {
            let c2 = c.clone();
            std::thread::spawn(move || c2.approve(&args(json!({"tool_name": SHELL_RUN, "input": {"command": cmd}}))))
        };
        // (commands no deny rule of the user's is likely to refuse first)
        let first = ask("gh pr create --fill");
        wait_status(&c, "CONFIRM");
        let second = ask("npm install x");
        while kinds(&events, EventKind::Approval).len() < 2 {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(c.answer_approval(false));
        assert_eq!(first.join().unwrap()["behavior"], "deny", "the older one is answered first");
        assert!(!second.is_finished());
        assert!(c.answer_approval(true));
        assert_eq!(second.join().unwrap()["behavior"], "allow");
    }

    /// claude is told where the MCP config is; the token is not among its
    /// arguments (every account on the Mac can read those).
    #[test]
    fn the_token_stays_out_of_claudes_arguments() {
        let (c, _) = controller();
        let path = c.mcp_config_path();
        let args = c.claude_args(&path);
        let at = args.iter().position(|a| a == "--mcp-config").unwrap();
        assert_eq!(args[at + 1], path.to_string_lossy());
        assert!(args.iter().all(|a| !a.contains("Bearer")), "{args:?}");
        assert!(args.windows(2).any(|w| w[0] == "--disallowedTools" && w[1] == "Bash"));
        assert!(args.windows(2).any(|w| w[0] == "--permission-prompt-tool" && w[1] == APPROVE));
    }

    #[test]
    fn tool_calls_in_the_conversation() {
        let (c, _) = controller();
        assert_eq!(c.describe_tool(SHELL_RUN, &args(json!({"command": " cargo test "}))), ("exec".into(), "cargo test".into()));
        assert_eq!(c.describe_tool(SHELL_RUN, &args(json!({"command": "ls", "shell": "build"}))).1, "build › ls");
        assert_eq!(c.describe_tool(JOB_TAIL, &args(json!({"id": 3}))), ("job".into(), "job_tail #3".into()));
        assert_eq!(c.describe_tool("Read", &args(json!({"file_path": "/work/src/main.rs"}))), ("read".into(), "src/main.rs".into()));
        assert_eq!(c.describe_tool("Edit", &args(json!({"file_path": "/work/a", "new_string": "x\ny\n", "old_string": "z"}))).1, "a +2 -1");
        assert_eq!(c.describe_tool("TaskStop", &args(json!({"task_id": "t1"}))), ("stop".into(), "t1".into()));
        assert_eq!(c.describe_tool(APPROVE, &Map::new()).0, "");
        assert_eq!(c.summary(SHELL_INPUT, &args(json!({"text": "y", "shell": "b"}))), "b › 入力 \"y\"");
        assert_eq!(approval_key("t", &args(json!({"b": 1, "a": "x"}))), approval_key("t", &args(json!({"a": "x", "b": 1}))));
    }

    /// What voice-agent says for an approval and its other fixed sentences
    /// are all in the phrase cache, sentence by sentence as speech cuts them.
    #[test]
    fn what_voice_agent_says_is_cached() {
        let cached = fixed_phrases();
        assert_eq!(cached.len(), 27);
        let mut said: Vec<String> = [CONFIRM_AGAIN, ERROR_SAID, MIC_OFF_SAID, MIC_ON_SAID, CODE_NOTICE].iter().flat_map(|t| split_sentences(t)).collect();
        for r in ask_reasons() {
            said.extend(split_sentences(&ask_prompt(r)));
        }
        // A question with several reasons says each as a sentence
        let both = ask_prompt("リモートへ push します、外部サービスへ変更を反映します");
        assert_eq!(both, "確認です。リモートへ push します。外部サービスへ変更を反映します。実行していいですか？");
        said.extend(split_sentences(&both));
        for s in said {
            assert!(cached.contains(&s), "{s:?} is not cached");
        }
    }

    /// docs/design.md's account of P1 counts the phrases the cache keeps.
    #[test]
    fn the_design_doc_counts_the_phrases() {
        let p1 = include_str!("../docs/design.md").split("## P1 で").nth(1).unwrap().split("\n## ").next().unwrap();
        for want in [format!("キャッシュ**: {} 文", fixed_phrases().len()), format!("理由 {}", ask_reasons().len())] {
            assert!(p1.contains(&want), "the P1 section should say {want:?}");
        }
    }

    /// A claude stand-in: answers every message with the same short reply,
    /// pause seconds after it came in.
    fn fake_claude(dir: &Path, pause: &str) -> PathBuf {
        let path = dir.join("claude");
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"s1","model":"fake"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"はい、テストを"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"実行します。全部通りました。"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"はい、テストを実行します。全部通りました。","session_id":"s1"}"#,
        ];
        let body: String = lines.iter().map(|l| format!("  printf '%s\\n' '{l}'\n")).collect();
        std::fs::write(&path, format!("#!/bin/sh\nwhile IFS= read -r line; do\n  sleep {pause}\n{body}done\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn log_lines(path: &Path, kind: &str) -> Vec<Map<String, Value>> {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()?.as_object().cloned()).filter(|l| l["kind"] == kind).collect()
    }

    fn wait_for(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !f() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A spoken turn from end to end, with stand-ins for whisper, claude and
    /// AivisSpeech: every step is in latency.jsonl, in order. Then, once the
    /// phrase cache is built, a cached phrase sounds within 100 ms.
    #[test]
    fn a_turn_in_the_latency_log() {
        use crate::audio::wav::tests::test_wav;
        use crate::latency::MARKS;
        use crate::net::testing::serve;
        use crate::speech::aivis::testing::engine;

        let dir = std::env::temp_dir().join(format!("va-turn-{}-{}", std::process::id(), unix_millis()));
        std::fs::create_dir_all(dir.join("tts")).unwrap();
        let aivis = engine(|_| 200, |_| Duration::from_millis(5));
        let whisper = serve(|r| if r.url == "/inference" { (200, r#"{"text":"テストを実行して"}"#.as_bytes().to_vec()) } else { (404, Vec::new()) });
        let mut cfg = Config::load(Path::new("/nonexistent-home"));
        cfg.tts = "aivis".into();
        cfg.aivis_url = aivis.srv.url.clone();
        cfg.speak = true;
        cfg.dir = dir.clone();
        let log = dir.join("latency.jsonl");
        let hooks = Hooks { tts_out: Some(dir.join("tts")), latency_log: Some(log.clone()), ..Hooks::default() };
        let c = Controller::new(cfg, hooks, dir.join("rt"), Box::new(|_| {}));
        *c.stt.lock().unwrap() = Some(WhisperServer::ready_at(&whisper.url));
        c.open_claude(fake_claude(&dir, "0"), Vec::new(), Vec::new());
        let a = c.speaker.aivis.clone().unwrap();
        a.start();
        wait_for("the engine is up", || !a.is_down());

        c.transcribe_wav(&test_wav(1.0, 16000));
        wait_for("the turn is written", || !log_lines(&log, "turn").is_empty());
        let turn = &log_lines(&log, "turn")[0];
        assert_eq!((turn["via"].as_str(), turn["end"].as_str(), turn["stt"].as_str()), (Some("voice"), Some("sound"), Some("final")), "{turn:?}");
        let times: Vec<u64> = MARKS.iter().map(|k| turn.get(*k).and_then(Value::as_u64).unwrap_or_else(|| panic!("no {k}: {turn:?}"))).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "in order: {turn:?}");
        // The first chunk ended at the sentence: the comma came before FIRST_MIN
        assert_eq!((turn["engine"].as_str(), turn["chunk_chars"].as_u64()), (Some("aivis"), Some(13)));

        let phrases = c.speaker.phrases.clone().unwrap();
        wait_for("the phrase cache is built", || phrases.ready() == fixed_phrases().len());
        wait_for("the reply is over", || !c.speaker.speaking());
        c.set_mic(false);
        wait_for("the phrase is written", || !log_lines(&log, "phrase").is_empty());
        let phrase = &log_lines(&log, "phrase")[0];
        assert_eq!(phrase["text"].as_str(), Some(MIC_OFF_SAID));
        assert!(phrase["ms"].as_u64().unwrap() < 100, "{phrase:?}");
        assert!(aivis.synths.lock().unwrap().iter().filter(|t| *t == MIC_OFF_SAID).count() == 1, "made once, for the cache");
        c.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// How the controller ends turns in the latency log: a command, a request
    /// with no claude, an answer to an approval and the reply it came in
    /// before; a message sent while Claude is at a reply (busy), a reply with
    /// speech off (silent), one stopped, and the app closing.
    #[test]
    fn turns_end_in_the_latency_log() {
        let dir = std::env::temp_dir().join(format!("va-ends-{}-{}", std::process::id(), unix_millis()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("latency.jsonl");
        let (c, _) = controller_with(Hooks { latency_log: Some(log.clone()), ..Hooks::default() }, &dir);
        let ends = || log_lines(&log, "turn").iter().map(|l| l["end"].as_str().unwrap_or_default().to_string()).collect::<Vec<_>>();
        c.handle_user_text("止めて。", "text");
        c.handle_user_text("こんにちは", "text");
        assert_eq!(ends(), ["command", "error"]);

        c.open_claude(fake_claude(&dir, "0.8"), Vec::new(), Vec::new());
        c.handle_user_text("一つ目", "text");
        c.handle_user_text("二つ目", "text");
        wait_for("the first reply is over", || ends().len() == 4);
        assert_eq!(ends()[2..], ["busy", "silent"], "the first keeps its own times");
        wait_for("claude is free", || !c.claude().unwrap().busy());

        c.handle_user_text("三つ目", "text");
        c.hush();
        assert_eq!(ends().last().map(String::as_str), Some("stopped"));
        wait_for("claude is free", || !c.claude().unwrap().busy());

        c.handle_user_text("四つ目", "text");
        let c2 = c.clone();
        let asked = std::thread::spawn(move || c2.approve(&args(json!({"tool_name": SHELL_RUN, "input": {"command": "git push origin main"}}))));
        wait_status(&c, "CONFIRM");
        c.handle_user_text("はい", "voice");
        assert_eq!(asked.join().unwrap()["behavior"], "allow");
        assert_eq!(ends()[5..], ["approval", "approval"], "the reply it came before, and the answer");
        wait_for("claude is free", || !c.claude().unwrap().busy());

        // Claude says it is free a moment before the end of its reply is
        // handled: a message sent then does not take that end as its own
        let ending = c.latency.begin("text", Instant::now());
        c.latency.replying_to(ending);
        c.handle_user_text("五つ目", "text");
        c.latency.reply_ended(false);
        assert_eq!(ends()[7..], ["busy", "silent"]);
        wait_for("claude is free", || !c.claude().unwrap().busy());

        let open = c.latency.begin("voice", Instant::now());
        c.close();
        assert!(!c.latency.is_open(open));
        assert_eq!(ends().last().map(String::as_str), Some("quit"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Talking to answer an approval silences the speech (as any talking
    /// does); the answer lets the rest of the turn be heard again, by voice
    /// as by the keyboard. 「静かにして」 while it waits only quiets.
    #[test]
    fn an_answer_lets_the_rest_be_heard() {
        let (c, _) = controller();
        let ask = || {
            let c2 = c.clone();
            std::thread::spawn(move || c2.approve(&args(json!({"tool_name": SHELL_RUN, "input": {"command": "git push origin main"}}))))
        };
        let asked = ask();
        wait_status(&c, "CONFIRM");
        c.hush(); // what Space does
        c.handle_user_text("はい", "voice");
        assert_eq!(asked.join().unwrap()["behavior"], "allow");
        assert!(!c.lock().hushed, "the rest of the reply is heard");

        let asked = ask();
        wait_status(&c, "CONFIRM");
        c.handle_user_text("静かにして", "voice");
        assert!(c.lock().hushed);
        assert!(c.answer_approval(false));
        assert_eq!(asked.join().unwrap()["behavior"], "deny");
        assert!(!c.lock().hushed);
    }

    #[test]
    fn typed_text_without_claude_is_an_error() {
        let (c, events) = controller();
        c.handle_user_text("  ", "text");
        assert!(events.lock().unwrap().is_empty());
        c.handle_user_text("こんにちは", "text");
        let evs = events.lock().unwrap();
        assert_eq!((evs[0].kind, evs[0].via.as_str()), (EventKind::User, "text"));
        assert_eq!(evs[1].kind, EventKind::Error);
    }
}
