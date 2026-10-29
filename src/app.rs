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
    APPROVE, Decision, DenyRule, JOB_START, JOB_STATUS, JOB_STOP, JOB_TAIL, SHELL_INPUT, SHELL_LIST, SHELL_NEW, SHELL_RUN, SHELL_SCREEN, decide,
    load_deny_rules, parse_consent,
};
use crate::agent::prompt;
use crate::agent::tools::{self, ToolHost, with_shell};
use crate::agent::usage::{Usage, UsageTracker};
use crate::audio::capture::{RecError, Recorder};
use crate::audio::playback::PcmPlayer;
use crate::config::Config;
use crate::log::log;
use crate::shell::jobs::Jobs;
use crate::shell::registry::{MAIN, Registry, ShellOptions};
use crate::speech::aivis::AivisEngine;
use crate::speech::stt::WhisperServer;
use crate::speech::tts::{SentenceChunker, Speaker};
use crate::util::{fmt_elapsed, home_dir, look_path, unix_millis};
use crate::voice::commands::{self, Command};

const APPROVAL_TIMEOUT: Duration = Duration::from_secs(180);
/// How long a call the user allowed through the approve tool may take to arrive.
const APPROVED_FOR: Duration = Duration::from_secs(600);

/// The notice of everything being stopped (Esc, 「生成もストップ」).
pub const ABORTED: &str = "（すべて止めました）";
/// Said when a reply to a pending approval is neither yes nor no.
const CONFIRM_AGAIN: &str = "承認待ちです。はいか、いいえで答えてください。";

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
    pub tts_out: Option<PathBuf>,   // VA_TTS_OUT: speech goes to audio files instead of the speakers
    pub event_log: Option<PathBuf>, // VA_EVENT_LOG: every event is appended as a JSON line
    pub test_steps: String,         // VA_TEST_STEPS: [{"at":3000,"text":"..."},{"at":9000,"wav":"/x.wav"}]
}

impl Hooks {
    pub fn from_env() -> Hooks {
        let path = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        Hooks { tts_out: path("VA_TTS_OUT"), event_log: path("VA_EVENT_LOG"), test_steps: std::env::var("VA_TEST_STEPS").unwrap_or_default() }
    }
}

struct Approval {
    id: String,
    answer: mpsc::Sender<(bool, String)>,
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
    /// 「止めて」 or Esc during a reply: the rest of it is shown, not spoken
    hushed: bool,
    mic_off: bool,
    last_status: String,
    ap_seq: u32,
    stt_err: String,
}

type Deliver = Box<dyn Fn(Vec<Event>) + Send + Sync>;

pub struct Controller {
    cfg: Config,
    hooks: Hooks,
    deliver: Deliver,

    speaker: Arc<Speaker>,
    chunker: Mutex<SentenceChunker>,
    stt: Mutex<Option<Arc<WhisperServer>>>,
    rec: Recorder,
    mcp: Mutex<Option<McpServer>>,
    claude: Mutex<Option<Arc<ClaudeSession>>>,
    agents: Arc<AgentTracker>,
    usage: Arc<UsageTracker>,
    shells: Registry,
    jobs: Jobs,
    deny: Vec<DenyRule>,
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

            let (mut aivis, mut player) = (None, None);
            if cfg.tts == "aivis" {
                let engine = AivisEngine::new(&cfg.aivis_url, cfg.aivis_speaker, cfg.aivis_speed);
                let w = weak.clone();
                let _ = engine.expected.set(Box::new(move || w.upgrade().is_some_and(|c| c.reply_expected())));
                aivis = Some(engine);
                if hooks.tts_out.is_none() {
                    player = Some(Arc::new(PcmPlayer::default()));
                }
            }
            let w = weak.clone();
            let speaker = Arc::new(Speaker::new(
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
            ));
            let chunker = chunker_for(&speaker);
            let level = Arc::new(AtomicU64::new(0));
            let l = level.clone();
            let shells = Registry::new(ShellOptions { shell: cfg.shell.clone(), dir: cfg.dir.clone(), runtime_dir: runtime_dir.clone() });
            let jobs = Jobs::new(&cfg.shell, cfg.dir.clone(), runtime_dir);
            Controller {
                cfg,
                hooks,
                deliver,
                speaker,
                chunker: Mutex::new(chunker),
                stt: Mutex::new(None),
                rec: Recorder::new(move |v| l.store(v.to_bits(), Ordering::Relaxed)),
                mcp: Mutex::new(None),
                claude: Mutex::new(None),
                agents,
                usage,
                shells,
                jobs,
                deny: load_deny_rules(),
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
        let mcp_config = mcp.config();
        *self.mcp.lock().unwrap_or_else(|e| e.into_inner()) = Some(mcp);

        match look_path("claude") {
            None => self.emit(Event::new(EventKind::Error, "claude コマンドが見つかりません。Claude Code をインストールしてください。")),
            Some(bin) => self.start_claude(bin, mcp_config),
        }
        // After claude: the engine asks reply_expected, which looks at the session
        if let Some(a) = self.speaker.aivis.clone() {
            std::thread::spawn(move || a.start());
        }
        self.status();
        self.run_test_steps();
        Ok(())
    }

    fn start_claude(self: &Arc<Self>, bin: PathBuf, mcp_config: String) {
        let system = prompt::system_prompt(&prompt::load_persona(&self.cfg.persona));
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
        // A claude started from inside another Claude Code session would refuse to run
        let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(k, _)| {
                let k = k.to_string_lossy();
                k != "CLAUDECODE" && k != "CLAUDE_CODE_ENTRYPOINT" && !k.starts_with("VA_")
            })
            .collect();
        // MCP tools load up front instead of through an extra ToolSearch round trip
        env.push(("ENABLE_TOOL_SEARCH".into(), "false".into()));

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
                    if is_error {
                        c.emit(Event::new(EventKind::Error, if result.is_empty() { "エラーが発生しました" } else { result }));
                        if !hushed {
                            c.speaker.speak("エラーが発生しました。");
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
                    c.lock().transcribing += 1;
                    c.status();
                    c.transcribe_counted(&data);
                }
            });
        }
    }

    // --- voice ---

    /// Opens the microphone (Space held).
    pub fn start_recording(self: &Arc<Self>) {
        if self.lock().mic_off {
            self.emit(Event::new(EventKind::Notice, "（マイクはオフです。m キーで戻せます）"));
            return;
        }
        // Talking over the assistant silences it (the reply goes on)
        self.speaker.stop();
        self.level.store(0f64.to_bits(), Ordering::Relaxed);
        let t0 = Instant::now();
        if let Err(e) = self.rec.start() {
            self.emit(Event::new(EventKind::Error, format!("マイクを開けません: {e}")));
            self.status();
            return;
        }
        log(&format!("mic open in {:?}", t0.elapsed()));
        self.lock().recording = true;
        // A reply is coming: get the voice model resident meanwhile
        self.speaker.warm();
        self.status();
    }

    /// Closes the microphone and sends what was said (cancel: throws it away).
    pub fn stop_recording(self: &Arc<Self>, cancel: bool) {
        {
            let mut st = self.lock();
            if !st.recording {
                return;
            }
            st.recording = false;
            if !cancel {
                // Straight from listening to transcribing
                st.transcribing += 1;
            }
        }
        self.status();
        let c = self.clone();
        std::thread::spawn(move || {
            let recorded = c.rec.stop();
            if cancel {
                return;
            }
            let notice = match recorded {
                Some((Ok(wav), secs)) if secs >= 0.35 => return c.transcribe_counted(&wav),
                Some((Err(RecError::Silence), _)) => Event::new(EventKind::Notice, "（声が聞こえませんでした）"),
                Some((Err(e), _)) => Event::new(EventKind::Error, e.to_string()),
                _ => Event::new(EventKind::Notice, "（短すぎて聞き取れませんでした）"),
            };
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
            self.speaker.speak("マイクを戻しました。");
        } else {
            self.stop_recording(true);
            self.rec.close();
            self.emit(Event::new(EventKind::Notice, "（マイクを切りました。m キーで戻せます）"));
            self.speaker.speak("マイクを切りました。");
        }
        self.status();
    }

    pub fn mic_on(&self) -> bool {
        !self.lock().mic_off
    }

    /// Expects State.transcribing to count this request already.
    fn transcribe_counted(self: &Arc<Self>, wav: &[u8]) {
        let stt = self.stt.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let res = stt.as_ref().map(|s| {
            if !s.is_ready() {
                self.emit(Event::new(EventKind::Notice, "（音声認識エンジンを起動中です。最初だけ時間がかかります）"));
            }
            s.transcribe(wav)
        });
        let stt_err = {
            let mut st = self.lock();
            st.transcribing = st.transcribing.saturating_sub(1);
            st.stt_err.clone()
        };
        self.status();
        match res {
            None => {
                let why = if stt_err.is_empty() { String::new() } else { format!("（{stt_err}）") };
                self.emit(Event::new(EventKind::Error, format!("音声認識が使えません。{why}")));
            }
            Some(Err(e)) => self.emit(Event::new(EventKind::Error, format!("音声認識エラー: {e}"))),
            Some(Ok(text)) if text.is_empty() => self.emit(Event::new(EventKind::Notice, "（聞き取れませんでした）")),
            Some(Ok(text)) => self.handle_user_text(&text, "voice"),
        }
    }

    // --- conversation ---

    /// What the user said or typed: a spoken command, the answer to a
    /// pending approval, or a message for Claude.
    pub fn handle_user_text(self: &Arc<Self>, text: &str, via: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.emit(Event { via: via.to_string(), ..Event::new(EventKind::User, text) });

        if let Some(cmd) = commands::parse(text) {
            match cmd {
                Command::Stop => self.stop(),
                Command::StopAll => self.abort(),
                Command::MicOff => self.set_mic(false),
            }
            return;
        }

        let first = self.lock().approvals.first().map(|a| a.id.clone());
        if let Some(id) = first {
            match parse_consent(text) {
                Some(true) => self.resolve_approval(&id, true, ""),
                Some(false) => self.resolve_approval(&id, false, "The user denied this action."),
                // Neither: the approval keeps waiting for a clear answer
                None => {
                    self.emit(Event::new(EventKind::Notice, format!("（{CONFIRM_AGAIN}）")));
                    self.speaker.speak(CONFIRM_AGAIN);
                }
            }
            return;
        }

        let Some(claude) = self.claude() else {
            return self.emit(Event::new(EventKind::Error, "Claude に接続できません。"));
        };
        self.lock().hushed = false;
        let msg = format!("{text}\n\n<va_context>main shell cwd: {}</va_context>", self.cwd());
        if let Err(e) = claude.send(&msg) {
            self.emit(Event::new(EventKind::Error, format!("Claude に送れません: {e}")));
        }
        self.speaker.warm();
        self.status();
    }

    /// Gets the voice ready ahead of a message the user is about to type.
    pub fn warm(&self) {
        self.speaker.warm();
    }

    /// Reports that a reply is on its way or being spoken (and speech is not
    /// muted): the user is talking, claude is working on what they said, or a
    /// subagent is still at work (the front end relays its result).
    fn reply_expected(&self) -> bool {
        if !self.speaker.is_enabled() {
            return false;
        }
        let hearing = {
            let st = self.lock();
            st.recording || st.transcribing > 0
        };
        hearing || self.claude().is_some_and(|c| c.busy()) || self.speaker.speaking() || self.agents.running() > 0
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

    /// Esc and 「生成もストップ」: that, and the reply is interrupted too.
    /// Jobs keep running; job_stop ends them.
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
    }

    /// Silences the speech and the rest of the reply; the half sentence the
    /// chunker holds is dropped.
    fn hush(&self) {
        {
            let mut chunker = self.chunker.lock().unwrap_or_else(|e| e.into_inner());
            self.lock().hushed = true;
            *chunker = chunker_for(&self.speaker);
        }
        self.speaker.stop();
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
        self.resolve_approval(&id, allow, "The user denied this action.");
        true
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
        let id = {
            let mut st = self.lock();
            st.ap_seq += 1;
            let id = format!("ap{}", st.ap_seq);
            st.approvals.push(Approval { id: id.clone(), answer: tx });
            id
        };
        self.emit(Event { id: id.clone(), reason: reason.to_string(), ..Event::new(EventKind::Approval, summary) });
        self.speaker.speak(&format!("確認です。{reason}。実行していいですか？"));
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
        let approval = {
            let mut st = self.lock();
            st.approvals.iter().position(|a| a.id == id).map(|i| st.approvals.remove(i))
        };
        let Some(a) = approval else { return };
        let message = if message.is_empty() { "The user denied this action." } else { message };
        self.emit(Event { id: id.to_string(), allow, ..Event::new(EventKind::ApprovalResolved, "") });
        let _ = a.answer.send((allow, message.to_string()));
        self.status();
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
        if let Some(c) = self.claude() {
            c.stop();
        }
        if let Some(s) = self.stt.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            s.stop();
        }
        self.speaker.stop();
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
        self.shells.close_all();
        self.jobs.stop_all();
    }
}

/// Cuts the reply into sentences for speaker.
fn chunker_for(speaker: &Arc<Speaker>) -> SentenceChunker {
    let (s1, s2) = (speaker.clone(), speaker.clone());
    SentenceChunker::new(Box::new(move |s| s1.speak(s)), Some(Box::new(move || s2.eager_first())))
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
        let events = Arc::new(Mutex::new(Vec::new()));
        let ev = events.clone();
        let mut cfg = Config::load(Path::new("/nonexistent-home"));
        cfg.tts = "say".into();
        cfg.speak = false;
        cfg.dir = "/work".into();
        let rt = std::env::temp_dir().join(format!("va-app-{}-{}", std::process::id(), unix_millis()));
        (Controller::new(cfg, Hooks::default(), rt, Box::new(move |evs| ev.lock().unwrap().extend(evs))), events)
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

    #[test]
    fn spoken_commands_act_at_once() {
        let (c, events) = controller();
        // 「止めて」 refuses what waits and leaves Claude alone
        let c2 = c.clone();
        let asked = std::thread::spawn(move || c2.approve(&args(json!({"tool_name": SHELL_RUN, "input": {"command": "gh pr create"}}))));
        wait_status(&c, "CONFIRM");
        c.handle_user_text("止めて。", "voice");
        let v = asked.join().unwrap();
        assert_eq!((v["behavior"].as_str(), v["message"].as_str()), (Some("deny"), Some("The user said stop.")));
        assert!(c.lock().hushed, "the rest of the reply is not spoken");

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
