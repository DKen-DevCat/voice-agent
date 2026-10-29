//! Keys as the UI sees them, read from crossterm's events. With the kitty
//! keyboard protocol the terminal also reports a key being let go of, which
//! is what holding Space to talk needs.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// The flags that make the terminal report a key being let go of.
pub const KITTY_FLAGS: u8 = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Delete,
    Tab,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub code: Code,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Held down: the terminal repeats it
    pub repeat: bool,
    /// Let go of (kitty keyboard protocol only)
    pub release: bool,
}

#[cfg(test)]
impl Key {
    pub fn new(code: Code) -> Key {
        Key { code, ctrl: false, alt: false, shift: false, repeat: false, release: false }
    }

    pub fn char(c: char) -> Key {
        Key::new(Code::Char(c))
    }

    pub fn ctrl(c: char) -> Key {
        Key { ctrl: true, ..Key::char(c) }
    }
}

pub fn from_crossterm(k: KeyEvent) -> Key {
    let code = match k.code {
        KeyCode::Char(c) => Code::Char(c),
        KeyCode::Enter => Code::Enter,
        KeyCode::Esc => Code::Esc,
        KeyCode::Backspace => Code::Backspace,
        KeyCode::Delete => Code::Delete,
        KeyCode::Tab | KeyCode::BackTab => Code::Tab,
        KeyCode::Left => Code::Left,
        KeyCode::Right => Code::Right,
        KeyCode::Up => Code::Up,
        KeyCode::Down => Code::Down,
        KeyCode::Home => Code::Home,
        KeyCode::End => Code::End,
        KeyCode::PageUp => Code::PageUp,
        KeyCode::PageDown => Code::PageDown,
        _ => Code::Other,
    };
    Key {
        code,
        ctrl: k.modifiers.contains(KeyModifiers::CONTROL),
        alt: k.modifiers.contains(KeyModifiers::ALT),
        shift: k.modifiers.contains(KeyModifiers::SHIFT),
        repeat: k.kind == KeyEventKind::Repeat,
        release: k.kind == KeyEventKind::Release,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEventState, KeyboardEnhancementFlags};

    #[test]
    fn keys_from_crossterm() {
        let ev = |code, modifiers, kind| KeyEvent { code, modifiers, kind, state: KeyEventState::NONE };
        let k = from_crossterm(ev(KeyCode::Char(' '), KeyModifiers::NONE, KeyEventKind::Release));
        assert!(k.release && k.code == Code::Char(' '));
        let k = from_crossterm(ev(KeyCode::Char('c'), KeyModifiers::CONTROL, KeyEventKind::Press));
        assert_eq!(k, Key::ctrl('c'));
        let k = from_crossterm(ev(KeyCode::Enter, KeyModifiers::SHIFT, KeyEventKind::Repeat));
        assert!(k.shift && k.repeat && k.code == Code::Enter);
        assert_eq!(from_crossterm(ev(KeyCode::F(5), KeyModifiers::NONE, KeyEventKind::Press)).code, Code::Other);
        assert_eq!(KeyboardEnhancementFlags::from_bits_truncate(KITTY_FLAGS).bits(), 15);
    }
}
