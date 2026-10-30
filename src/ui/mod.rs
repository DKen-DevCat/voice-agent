//! The one window voice-agent has: the conversation with the mascot beside
//! it, the CPU and Claude's usage, a status line and, while one waits, the
//! approval. Keys are few: hold Space to talk, t to type, y/n to answer an
//! approval, m for the mic, Esc to stop everything, q to quit.
//!
//! Ui keeps the state, takes the keys and the agent's events; a View draws
//! frames from the state alone (view.rs).

pub mod canvas;
pub mod input;
pub mod keys;
pub mod mascot;
pub mod term;
pub mod usage;
pub mod view;

use std::sync::Arc;

use chrono::{DateTime, Local};

use crate::agent::usage::Usage;
use crate::app::{ABORTED, Controller, Event, EventKind};
use keys::{Code, Key};

const MAX_LOG: usize = 400;
const MAX_HOLD_REC_MS: i64 = 90_000;
const NOTE_FOR_MS: i64 = 2500;
/// CPU samples kept (one a second)
const CPU_HISTORY: usize = 60;

/// What the agent does for the UI (the controller; a stand-in in tests).
pub trait Assistant: Send + Sync {
    fn start_recording(&self);
    fn stop_recording(&self, cancel: bool);
    fn recording(&self) -> bool;
    fn level(&self) -> f64;
    fn send_text(&self, text: &str);
    fn warm(&self);
    fn abort(&self);
    fn answer_approval(&self, allow: bool) -> bool;
    fn set_mic(&self, on: bool);
    fn mic_on(&self) -> bool;
    /// How many subagents and jobs are at work
    fn running(&self) -> (usize, usize);
}

impl Assistant for Arc<Controller> {
    fn start_recording(&self) {
        Controller::start_recording(self);
    }
    fn stop_recording(&self, cancel: bool) {
        Controller::stop_recording(self, cancel);
    }
    fn recording(&self) -> bool {
        Controller::recording(self)
    }
    fn level(&self) -> f64 {
        Controller::level(self)
    }
    fn send_text(&self, text: &str) {
        // Sending may have to start claude; the UI does not wait for it
        let (c, text) = (self.clone(), text.to_string());
        std::thread::spawn(move || c.handle_user_text(&text, "text"));
    }
    fn warm(&self) {
        Controller::warm(self);
    }
    fn abort(&self) {
        let c = self.clone();
        std::thread::spawn(move || Controller::abort(&c));
    }
    fn answer_approval(&self, allow: bool) -> bool {
        Controller::answer_approval(self, allow)
    }
    fn set_mic(&self, on: bool) {
        Controller::set_mic(self, on);
    }
    fn mic_on(&self) -> bool {
        Controller::mic_on(self)
    }
    fn running(&self) -> (usize, usize) {
        Controller::running(self)
    }
}

/// What the UI wants done outside itself; the main loop carries these out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Call on_hold_timer(seq) after the hold delay
    HoldTimer(u64),
    Quit,
}

#[derive(Clone, Debug, Default)]
pub struct LogEntry {
    pub kind: EventKind,
    pub text: String,
    pub tag: String,
    pub id: String,
    pub allow: Option<bool>,
    pub open: bool,
}

/// How the user is talking now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Talk {
    #[default]
    No,
    /// While Space is held (the terminal reports releases)
    Hold,
    /// From one Space to the next (it does not)
    Toggle,
}

/// Everything a view draws from.
#[derive(Clone, Debug)]
pub struct UiState {
    pub now_ms: i64,
    pub now: DateTime<Local>,
    pub state: String, // the agent's: STANDBY, LISTENING ... (see Controller::status_now)
    pub log: Vec<LogEntry>,
    pub scroll: usize, // lines up from the newest
    pub approval: Option<LogEntry>,
    pub usage: Usage,
    pub cpu: Vec<f64>, // percent, oldest first
    pub level: f64,    // the mic, 0..1
    pub mic_on: bool,
    pub agents: usize, // subagents at work
    pub jobs: usize,   // jobs running
    pub kitty: bool,   // the terminal reports key releases: Space is held to talk
    pub talk: Talk,
    pub input_on: bool, // a message is being typed
    pub input: Vec<char>,
    pub input_pos: usize, // the cursor, in characters
    pub note: String,     // a short note for the status line, while it is fresh
    pub errors: u64,      // errors so far (the mascot flinches at a new one)
    pub aborts: u64,
    pub mascot: bool,
}

impl UiState {
    pub fn new(kitty: bool, mascot: bool) -> UiState {
        let now = Local::now();
        UiState {
            now_ms: now.timestamp_millis(),
            now,
            state: "STANDBY".into(),
            log: Vec::new(),
            scroll: 0,
            approval: None,
            usage: Usage::default(),
            cpu: Vec::new(),
            level: 0.0,
            mic_on: true,
            agents: 0,
            jobs: 0,
            kitty,
            talk: Talk::No,
            input_on: false,
            input: Vec::new(),
            input_pos: 0,
            note: String::new(),
            errors: 0,
            aborts: 0,
            mascot,
        }
    }
}

pub struct Ui {
    pub s: UiState,
    ai: Option<Arc<dyn Assistant>>, // None in some tests

    input_hist: Vec<String>, // messages sent by typing, oldest first
    hist_idx: usize,         // where ↑/↓ is in input_hist; its length is the line being typed
    hist_draft: Vec<char>,
    space_down: bool,
    space_seq: u64,
    hold_start: i64,
    /// Approvals waiting, oldest first: the first is shown and is what y/n answers
    approvals: Vec<LogEntry>,
    note: String,
    note_at: i64,
    effects: Vec<Effect>,
}

impl Ui {
    pub fn new(ai: Option<Arc<dyn Assistant>>, kitty: bool, mascot: bool) -> Ui {
        Ui {
            s: UiState::new(kitty, mascot),
            ai,
            input_hist: Vec::new(),
            hist_idx: 0,
            hist_draft: Vec::new(),
            space_down: false,
            space_seq: 0,
            hold_start: 0,
            approvals: Vec::new(),
            note: String::new(),
            note_at: 0,
            effects: Vec::new(),
        }
    }

    /// What the UI asked for since the last call.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    fn flash(&mut self, note: &str) {
        self.note = note.to_string();
        self.note_at = self.s.now_ms;
        self.s.note = self.note.clone();
    }

    // --- the outside world ---

    pub fn on_tick(&mut self, now: DateTime<Local>) {
        self.s.now = now;
        self.s.now_ms = now.timestamp_millis();
        if self.s.talk == Talk::Hold && self.s.now_ms - self.hold_start > MAX_HOLD_REC_MS {
            self.stop_talking(false);
        }
        self.s.note = if self.s.now_ms - self.note_at < NOTE_FOR_MS { self.note.clone() } else { String::new() };
        if let Some(ai) = &self.ai {
            self.s.level = ai.level();
            self.s.mic_on = ai.mic_on();
            (self.s.agents, self.s.jobs) = ai.running();
        }
    }

    pub fn on_cpu(&mut self, percent: f64) {
        self.s.cpu.push(percent);
        if self.s.cpu.len() > CPU_HISTORY {
            self.s.cpu.remove(0);
        }
    }

    /// The view says how far the conversation can scroll.
    pub fn clamp_scroll(&mut self, max: usize) {
        self.s.scroll = self.s.scroll.min(max);
    }

    /// An error of voice-agent's own (not the agent's), shown in the conversation.
    pub fn error(&mut self, text: &str) {
        self.s.errors += 1;
        self.add_log(LogEntry { kind: EventKind::Error, text: text.to_string(), ..LogEntry::default() });
    }

    /// The hold delay passed since Space went down (see Effect::HoldTimer).
    pub fn on_hold_timer(&mut self, seq: u64) {
        if seq == self.space_seq && self.space_down && self.s.talk == Talk::No {
            self.space_down = false;
            if let Some(ai) = &self.ai {
                self.s.talk = Talk::Hold;
                self.hold_start = self.s.now_ms;
                ai.start_recording();
            }
        }
    }

    fn stop_talking(&mut self, cancel: bool) {
        self.s.talk = Talk::No;
        if let Some(ai) = &self.ai {
            ai.stop_recording(cancel);
        }
    }

    // --- keys ---

    pub fn on_key(&mut self, k: Key) {
        if k.release {
            if k.code == Code::Char(' ') {
                if self.s.talk == Talk::Hold {
                    self.stop_talking(false);
                } else {
                    self.space_down = false;
                }
            }
            return;
        }
        // Talking: Space/Enter sends, Esc throws it away
        match self.s.talk {
            Talk::Toggle => {
                match k.code {
                    Code::Char(' ') | Code::Enter => self.stop_talking(false),
                    Code::Esc => self.stop_talking(true),
                    _ => {}
                }
                return;
            }
            Talk::Hold => {
                if k.code == Code::Esc {
                    self.stop_talking(true);
                }
                return;
            }
            Talk::No => {}
        }
        if self.s.input_on {
            self.on_input_key(k);
            return;
        }
        let Some(ai) = self.ai.clone() else {
            if k.code == Code::Char('q') {
                self.effects.push(Effect::Quit);
            }
            return;
        };
        if k.code == Code::Char(' ') && !k.ctrl && !k.alt {
            if !ai.mic_on() {
                self.flash("マイクはオフです（m で戻す）");
            } else if self.s.kitty {
                // Hold Space to talk (needs key release events from the terminal)
                if !k.repeat && !self.space_down {
                    self.space_down = true;
                    self.space_seq += 1;
                    self.effects.push(Effect::HoldTimer(self.space_seq));
                }
            } else {
                self.s.talk = Talk::Toggle;
                ai.start_recording();
            }
            return;
        }
        if k.repeat && !matches!(k.code, Code::Up | Code::Down | Code::PageUp | Code::PageDown) {
            return;
        }
        match k.code {
            Code::Up => self.s.scroll += 1,
            Code::Down => self.s.scroll = self.s.scroll.saturating_sub(1),
            Code::PageUp => self.s.scroll += 8,
            Code::PageDown => self.s.scroll = self.s.scroll.saturating_sub(8),
            Code::End => self.s.scroll = 0,
            Code::Esc => ai.abort(),
            Code::Char('c') if k.ctrl => ai.abort(),
            Code::Char('t') | Code::Enter => self.open_input(),
            Code::Char(c @ ('y' | 'n')) => {
                if !ai.answer_approval(c == 'y') {
                    self.flash("承認待ちはありません");
                }
            }
            Code::Char('m') => {
                let on = !ai.mic_on();
                ai.set_mic(on);
                self.s.mic_on = on;
                self.flash(if on { "マイク ON" } else { "マイク OFF" });
            }
            Code::Char('q') => self.effects.push(Effect::Quit),
            _ => {}
        }
    }

    /// Text pasted into the window.
    pub fn on_paste(&mut self, text: &str) {
        if self.s.input_on {
            self.insert_input(&text.replace('\n', " "));
        }
    }

    // --- the agent ---

    pub fn on_events(&mut self, events: Vec<Event>) {
        for e in events {
            self.on_event(e);
        }
    }

    fn on_event(&mut self, e: Event) {
        match e.kind {
            EventKind::Status => {
                self.s.state = e.state;
                if self.s.state != "LISTENING" && self.s.talk != Talk::No && self.ai.as_ref().is_some_and(|ai| !ai.recording()) {
                    // The mic did not open
                    self.s.talk = Talk::No;
                }
                return;
            }
            EventKind::Assistant => {
                if let Some(last) = self.s.log.last_mut().filter(|l| l.kind == EventKind::Assistant && l.open) {
                    last.text += &e.text;
                    return;
                }
                self.add_log(LogEntry { kind: EventKind::Assistant, text: e.text, open: true, ..LogEntry::default() });
                return;
            }
            EventKind::AssistantEnd => {
                if let Some(l) = self.s.log.iter_mut().rev().find(|l| l.kind == EventKind::Assistant && l.open) {
                    l.open = false;
                    l.text = l.text.trim_end_matches('\n').to_string();
                }
                return;
            }
            EventKind::Usage => {
                if let Some(u) = e.usage {
                    self.s.usage = u;
                }
                return;
            }
            // The counts come from the agent itself (on_tick)
            EventKind::Agent => return,
            EventKind::Approval => {
                let entry = LogEntry { kind: e.kind, text: e.text, tag: e.reason, id: e.id, ..LogEntry::default() };
                self.approvals.push(entry.clone());
                self.s.approval = self.approvals.first().cloned();
                self.add_log(entry);
                return;
            }
            EventKind::ApprovalResolved => {
                if let Some(l) = self.s.log.iter_mut().rev().find(|l| l.kind == EventKind::Approval && l.id == e.id) {
                    l.allow = Some(e.allow);
                }
                self.approvals.retain(|a| a.id != e.id);
                self.s.approval = self.approvals.first().cloned();
                return;
            }
            EventKind::Error => self.s.errors += 1,
            EventKind::Notice if e.text == ABORTED => self.s.aborts += 1,
            _ => {}
        }
        self.add_log(LogEntry { kind: e.kind, text: e.text, tag: e.tag, ..LogEntry::default() });
    }

    fn add_log(&mut self, e: LogEntry) {
        // A tool call or a new turn closes the reply being streamed
        if e.kind != EventKind::Assistant
            && let Some(last) = self.s.log.last_mut().filter(|l| l.open)
        {
            last.open = false;
            last.text = last.text.trim_end_matches('\n').to_string();
        }
        self.s.log.push(e);
        if self.s.log.len() > MAX_LOG {
            self.s.log.drain(..self.s.log.len() - MAX_LOG);
        }
        self.s.scroll = 0;
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records what the UI asks of the agent.
    #[derive(Default)]
    pub struct FakeAi {
        calls: Mutex<Vec<String>>,
        recording: Mutex<bool>,
        mic_off: Mutex<bool>,
        approval: Mutex<bool>,
    }

    impl FakeAi {
        pub fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }
        fn call(&self, s: String) {
            self.calls.lock().unwrap().push(s);
        }
    }

    impl Assistant for FakeAi {
        fn start_recording(&self) {
            *self.recording.lock().unwrap() = true;
            self.call("start".into());
        }
        fn stop_recording(&self, cancel: bool) {
            *self.recording.lock().unwrap() = false;
            self.call(format!("stop cancel={cancel}"));
        }
        fn recording(&self) -> bool {
            *self.recording.lock().unwrap()
        }
        fn level(&self) -> f64 {
            0.5
        }
        fn send_text(&self, text: &str) {
            self.call(format!("send {text}"));
        }
        fn warm(&self) {}
        fn abort(&self) {
            self.call("abort".into());
        }
        fn answer_approval(&self, allow: bool) -> bool {
            self.call(format!("answer {allow}"));
            std::mem::take(&mut *self.approval.lock().unwrap())
        }
        fn set_mic(&self, on: bool) {
            *self.mic_off.lock().unwrap() = !on;
            self.call(format!("mic {on}"));
        }
        fn mic_on(&self) -> bool {
            !*self.mic_off.lock().unwrap()
        }
        fn running(&self) -> (usize, usize) {
            (1, 2)
        }
    }

    pub fn ui(kitty: bool) -> (Ui, Arc<FakeAi>) {
        let ai = Arc::new(FakeAi::default());
        let dyn_ai: Arc<dyn Assistant> = ai.clone();
        (Ui::new(Some(dyn_ai), kitty, true), ai)
    }

    pub fn press(c: char) -> Key {
        Key::char(c)
    }

    fn release(c: char) -> Key {
        Key { release: true, ..Key::char(c) }
    }

    #[test]
    fn hold_space_to_talk() {
        let (mut ui, ai) = ui(true);
        ui.on_key(press(' '));
        assert_eq!(ui.take_effects(), [Effect::HoldTimer(1)]);
        // Repeats while held do nothing more
        ui.on_key(Key { repeat: true, ..press(' ') });
        assert!(ui.take_effects().is_empty());
        ui.on_hold_timer(1);
        assert_eq!(ui.s.talk, Talk::Hold);
        ui.on_key(release(' '));
        assert_eq!(ai.take(), ["start", "stop cancel=false"]);
        // A tap shorter than the hold delay records nothing
        ui.on_key(press(' '));
        ui.on_key(release(' '));
        let Some(Effect::HoldTimer(seq)) = ui.take_effects().pop() else { panic!() };
        ui.on_hold_timer(seq);
        assert!(ai.take().is_empty() && ui.s.talk == Talk::No);
        // Esc throws a recording away
        ui.on_key(press(' '));
        ui.on_hold_timer(3);
        ui.on_key(Key::new(Code::Esc));
        assert_eq!(ai.take(), ["start", "stop cancel=true"]);
    }

    #[test]
    fn without_key_releases_space_toggles() {
        let (mut ui, ai) = ui(false);
        ui.on_key(press(' '));
        assert_eq!(ui.s.talk, Talk::Toggle);
        ui.on_key(press('x')); // ignored while talking
        ui.on_key(Key::new(Code::Enter));
        assert_eq!(ai.take(), ["start", "stop cancel=false"]);
    }

    #[test]
    fn keys() {
        let (mut ui, ai) = ui(true);
        ui.on_key(press('y'));
        assert_eq!(ui.s.note, "承認待ちはありません");
        *ai.approval.lock().unwrap() = true;
        ui.on_key(press('n'));
        ui.on_key(Key::new(Code::Esc));
        ui.on_key(Key::ctrl('c'));
        ui.on_key(press('m'));
        assert!(!ui.s.mic_on && ui.s.note == "マイク OFF");
        // With the mic off Space does nothing
        ui.on_key(press(' '));
        assert!(ui.take_effects().is_empty());
        ui.on_key(press('m'));
        ui.on_key(press('q'));
        assert_eq!(ui.take_effects(), [Effect::Quit]);
        assert_eq!(ai.take(), ["answer true", "answer false", "abort", "abort", "mic false", "mic true"]);
        ui.on_tick(Local::now());
        assert_eq!((ui.s.agents, ui.s.jobs, ui.s.level), (1, 2, 0.5));
    }

    #[test]
    fn the_reply_streams_into_one_entry() {
        let (mut ui, _) = ui(true);
        let ev = |kind, text: &str| Event { kind, text: text.into(), ..Event::default() };
        ui.on_events(vec![
            Event { via: "voice".into(), ..ev(EventKind::User, "テストして") },
            ev(EventKind::Assistant, "はい、"),
            ev(EventKind::Assistant, "実行します。\n"),
            Event { tag: "exec".into(), ..ev(EventKind::Tool, "cargo test") },
            ev(EventKind::Assistant, "通りました。"),
            ev(EventKind::AssistantEnd, ""),
            Event { id: "ap1".into(), reason: "リモートへ push します".into(), ..ev(EventKind::Approval, "git push") },
        ]);
        let log: Vec<(EventKind, &str, bool)> = ui.s.log.iter().map(|l| (l.kind, l.text.as_str(), l.open)).collect();
        assert_eq!(
            log,
            [
                (EventKind::User, "テストして", false),
                (EventKind::Assistant, "はい、実行します。", false),
                (EventKind::Tool, "cargo test", false),
                (EventKind::Assistant, "通りました。", false),
                (EventKind::Approval, "git push", false),
            ]
        );
        assert_eq!(ui.s.approval.as_ref().map(|a| a.tag.as_str()), Some("リモートへ push します"));
        ui.on_events(vec![Event { id: "ap1".into(), allow: true, ..ev(EventKind::ApprovalResolved, "") }]);
        assert!(ui.s.approval.is_none() && ui.s.log[4].allow == Some(true));
        // Two at once: the older one is shown (it is what y/n answers), then the next
        ui.on_events(vec![
            Event { id: "ap2".into(), ..ev(EventKind::Approval, "rm -r ../shared") },
            Event { id: "ap3".into(), ..ev(EventKind::Approval, "npm install x") },
        ]);
        assert_eq!(ui.s.approval.as_ref().map(|a| a.id.as_str()), Some("ap2"));
        ui.on_events(vec![Event { id: "ap2".into(), ..ev(EventKind::ApprovalResolved, "") }]);
        assert_eq!(ui.s.approval.as_ref().map(|a| a.id.as_str()), Some("ap3"));
        ui.on_events(vec![ev(EventKind::Error, "x"), ev(EventKind::Notice, ABORTED)]);
        assert_eq!((ui.s.errors, ui.s.aborts), (1, 1));
    }
}
