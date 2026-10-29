//! voice-agent: a companion to talk to by voice. Hold Space and speak; it
//! answers aloud through Claude Code and does development work in
//! background shells nobody sees, asking before anything risky.

mod agent;
mod app;
mod audio;
mod config;
mod log;
mod net;
mod shell;
mod speech;
mod sysmon;
mod ui;
mod util;
mod voice;

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use app::{Controller, Event, Hooks};
use ui::term::{Input, Terminal};
use ui::view::{TuiView, View};
use ui::{Assistant, Effect, Ui};

const USAGE: &str = "usage: voice-agent [--help | --version]

Runs in the terminal it is started in (a Ghostty window of its own is best).
Hold Space and speak, t to type, y/n to answer an approval, m for the mic,
Esc to stop everything, q to quit. See README.md.
";

/// How often the window is drawn while nothing happens (the mascot moves).
const TICK: Duration = Duration::from_millis(100);
/// How soon after an input or event the window is drawn again.
const SOON: Duration = Duration::from_millis(16);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None => run(),
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            0
        }
        Some("-V" | "--version") => {
            println!("voice-agent {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some(other) => {
            eprint!("voice-agent: unknown argument {other:?}\n\n{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

enum Msg {
    Input(Input),
    Events(Vec<Event>),
    Cpu(f64),
    Quit,
}

/// Keeps the runtime directory to this instance: a second voice-agent would
/// write over its shells' logs.
fn lock(rt: &Path) -> Result<File, String> {
    let path = rt.join("voice-agent.lock");
    let f = File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: flock on a descriptor we own; the lock goes with the process
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(format!("もう動いています（{}）。別に動かすなら VA_RUNTIME_DIR を変えてください。", rt.display()));
    }
    Ok(f)
}

fn run() -> i32 {
    let cfg = config::Config::load(&util::home_dir());
    let rt = log::runtime_dir();
    if let Err(e) = util::private_dir(&rt) {
        eprintln!("voice-agent: {}: {e}", rt.display());
        return 1;
    }
    let _lock = match lock(&rt) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("voice-agent: {e}");
            return 1;
        }
    };
    let _ = log::init(&rt.join("voice-agent.log"));
    logf!("voice-agent {} in {}", env!("CARGO_PKG_VERSION"), cfg.dir.display());

    let term = match Terminal::open() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("voice-agent: {e}");
            return 1;
        }
    };
    let (tx, rx) = mpsc::channel::<Msg>();
    let t = tx.clone();
    let ctl = Controller::new(cfg.clone(), Hooks::from_env(), rt, Box::new(move |evs| drop(t.send(Msg::Events(evs)))));
    let ai: Arc<dyn Assistant> = Arc::new(ctl.clone());
    let mut ui = Ui::new(Some(ai), term.kitty, cfg.mascot);

    let t = tx.clone();
    term.read_input(move |i| t.send(Msg::Input(i)).is_ok());
    let t = tx.clone();
    std::thread::spawn(move || sysmon::run(|p| t.send(Msg::Cpu(p)).is_ok()));
    // The window closing (SIGHUP) or a kill ends it the same way as q
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    if let Ok(mut signals) = signal_hook::iterator::Signals::new([SIGHUP, SIGTERM, SIGINT]) {
        let t = tx.clone();
        std::thread::spawn(move || {
            if let Some(sig) = signals.forever().next() {
                logf!("signal {sig}");
                let _ = t.send(Msg::Quit);
            }
        });
    }
    drop(tx);
    if let Err(e) = ctl.start() {
        ui.error(&format!("起動できません: {e}"));
    }

    let mut view = TuiView::default();
    let mut shown: Option<ratatui::buffer::Buffer> = None;
    let hold = Duration::from_millis(cfg.hold_ms);
    let mut timers: Vec<(Instant, u64)> = Vec::new();
    let mut draw_at = Instant::now();
    'main: loop {
        let wake = timers.iter().map(|(at, _)| *at).min().map_or(draw_at, |t| t.min(draw_at));
        let msg = rx.recv_timeout(wake.saturating_duration_since(Instant::now()));
        let now = Instant::now();
        if msg.is_ok() {
            draw_at = draw_at.min(now + SOON);
        }
        match msg {
            Ok(Msg::Input(Input::Key(k))) => ui.on_key(k),
            Ok(Msg::Input(Input::Paste(text))) => ui.on_paste(&text),
            Ok(Msg::Input(Input::Resize)) => shown = None,
            Ok(Msg::Input(Input::Closed) | Msg::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Msg::Events(evs)) => ui.on_events(evs),
            Ok(Msg::Cpu(p)) => ui.on_cpu(p),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        for (at, seq) in std::mem::take(&mut timers) {
            if at <= now {
                ui.on_hold_timer(seq);
            } else {
                timers.push((at, seq));
            }
        }
        for e in ui.take_effects() {
            match e {
                Effect::HoldTimer(seq) => timers.push((now + hold, seq)),
                Effect::Quit => break 'main,
            }
        }
        if now >= draw_at {
            ui.on_tick(chrono::Local::now());
            let (w, h) = term.size();
            let f = view.render(&ui.s, w, h);
            ui.clamp_scroll(f.max_scroll);
            if !term.write(&ui::canvas::encode_frame(shown.as_ref(), &f.canvas.buf, f.cursor)) {
                break;
            }
            shown = Some(f.canvas.buf);
            draw_at = now + TICK;
        }
    }
    logf!("quitting");
    ctl.close();
    drop(term);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_instance_per_runtime_directory() {
        let rt = std::env::temp_dir().join(format!("va-lock-{}", std::process::id()));
        std::fs::create_dir_all(&rt).unwrap();
        let first = lock(&rt).unwrap();
        assert!(lock(&rt).unwrap_err().contains("もう動いています"));
        drop(first);
        assert!(lock(&rt).is_ok(), "free again once the first one is gone");
        let _ = std::fs::remove_dir_all(&rt);
    }
}
