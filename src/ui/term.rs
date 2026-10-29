//! The terminal voice-agent draws in: raw mode, the alternate screen and
//! its own colors while it runs (put back on the way out, after a panic
//! too), the keys typed into it, and the frames written to it. No mouse
//! reporting is asked for, so the text can be selected and copied as usual.

use std::io::{self, Write};

use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::crossterm::{cursor, execute};

use super::canvas::{COL_BG, COL_FG, Rgb};
use super::keys::{KITTY_FLAGS, Key, from_crossterm};

pub enum Input {
    Key(Key),
    Paste(String),
    Resize,
    /// The terminal is gone
    Closed,
}

pub struct Terminal {
    /// The terminal reports keys being let go of (kitty keyboard protocol)
    pub kitty: bool,
}

fn restore(kitty: bool) {
    let mut out = io::stdout();
    if kitty {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableBracketedPaste, cursor::Show, LeaveAlternateScreen);
    // The terminal's own colors again
    let _ = out.write_all(b"\x1b]110\x07\x1b]111\x07");
    let _ = out.flush();
    let _ = terminal::disable_raw_mode();
}

impl Terminal {
    pub fn open() -> Result<Terminal, String> {
        terminal::enable_raw_mode().map_err(|e| format!("needs a terminal: {e}"))?;
        let mut out = io::stdout();
        // The voice-agent colors for this window only, set as the terminal's own
        let hex = |(r, g, b): Rgb| format!("#{r:02x}{g:02x}{b:02x}");
        let _ = write!(out, "\x1b]10;{}\x07\x1b]11;{}\x07", hex(COL_FG), hex(COL_BG));
        let _ = execute!(out, EnterAlternateScreen, cursor::Hide, EnableBracketedPaste, terminal::SetTitle("voice-agent"));
        let kitty = std::env::var("VA_FORCE_KITTY").is_ok_and(|v| v == "1") || terminal::supports_keyboard_enhancement().unwrap_or(false);
        if kitty {
            let _ = execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::from_bits_truncate(KITTY_FLAGS)));
        }
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore(kitty);
            hook(info);
        }));
        Ok(Terminal { kitty })
    }

    /// Columns and rows.
    pub fn size(&self) -> (i32, i32) {
        terminal::size().map_or((80, 24), |(w, h)| (w as i32, h as i32))
    }

    /// Puts a frame on the terminal; false once it is gone.
    pub fn write(&self, frame: &[u8]) -> bool {
        let mut out = io::stdout().lock();
        out.write_all(frame).and_then(|_| out.flush()).is_ok()
    }

    /// Hands what is typed to on, from a thread of its own, until on returns
    /// false or the terminal is gone.
    pub fn read_input(&self, on: impl Fn(Input) -> bool + Send + 'static) {
        std::thread::spawn(move || {
            loop {
                let input = match event::read() {
                    Ok(Event::Key(k)) => Input::Key(from_crossterm(k)),
                    Ok(Event::Paste(text)) => Input::Paste(text),
                    Ok(Event::Resize(..)) => Input::Resize,
                    Ok(_) => continue,
                    Err(_) => Input::Closed,
                };
                let closed = matches!(input, Input::Closed);
                if !on(input) || closed {
                    return;
                }
            }
        });
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        restore(self.kitty);
    }
}
