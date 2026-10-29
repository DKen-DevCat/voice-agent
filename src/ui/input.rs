//! The line a message to the agent is typed in (`t`).

use super::canvas::{COL_ACCENT, COL_BRIGHT, COL_FAINT, Canvas, Cursor, bold, fg, str_width};
use super::keys::{Code, Key};
use super::{Ui, UiState};

const MAX_INPUT_HIST: usize = 50;

impl Ui {
    /// Starts a typed message to the agent.
    pub(super) fn open_input(&mut self) {
        self.s.input_on = true;
        self.s.input.clear();
        self.s.input_pos = 0;
        self.hist_idx = self.input_hist.len();
        self.hist_draft.clear();
    }

    fn close_input(&mut self) {
        self.s.input_on = false;
        self.s.input.clear();
        self.s.input_pos = 0;
    }

    /// Types s at the cursor.
    pub(super) fn insert_input(&mut self, s: &str) {
        let chars: Vec<char> = s.chars().collect();
        let n = chars.len();
        self.s.input.splice(self.s.input_pos..self.s.input_pos, chars);
        self.s.input_pos += n;
    }

    /// Edits the typed message: a line editor with the usual readline keys
    /// and a history of what was sent before.
    pub(super) fn on_input_key(&mut self, k: Key) {
        let mut code = k.code;
        if k.ctrl {
            code = match k.code {
                Code::Char('a') => Code::Home,
                Code::Char('e') => Code::End,
                Code::Char('b') => Code::Left,
                Code::Char('f') => Code::Right,
                Code::Char('d') => Code::Delete,
                Code::Char('h') => Code::Backspace,
                other => other,
            };
        }
        match code {
            Code::Enter if k.shift => self.insert_input("\n"),
            Code::Char('j') if k.ctrl => self.insert_input("\n"),
            Code::Enter => {
                let text: String = self.s.input.iter().collect();
                let text = text.trim().to_string();
                self.close_input();
                if text.is_empty() {
                    return;
                }
                self.push_history(&text);
                if let Some(ai) = &self.ai {
                    self.s.scroll = 0;
                    ai.send_text(&text);
                }
            }
            Code::Esc => self.close_input(),
            Code::Backspace => {
                if self.s.input_pos > 0 {
                    self.s.input_pos -= 1;
                    self.s.input.remove(self.s.input_pos);
                }
            }
            Code::Delete => {
                if self.s.input_pos < self.s.input.len() {
                    self.s.input.remove(self.s.input_pos);
                }
            }
            Code::Left => self.s.input_pos = self.s.input_pos.saturating_sub(1),
            Code::Right => self.s.input_pos = (self.s.input_pos + 1).min(self.s.input.len()),
            Code::Home => self.s.input_pos = 0,
            Code::End => self.s.input_pos = self.s.input.len(),
            Code::Up => self.history(-1),
            Code::Down => self.history(1),
            Code::Char('u') if k.ctrl => {
                self.s.input.clear();
                self.s.input_pos = 0;
            }
            Code::Char('k') if k.ctrl => self.s.input.truncate(self.s.input_pos),
            Code::Char('w') if k.ctrl => {
                let mut i = self.s.input_pos;
                while i > 0 && self.s.input[i - 1].is_whitespace() {
                    i -= 1;
                }
                while i > 0 && !self.s.input[i - 1].is_whitespace() {
                    i -= 1;
                }
                self.s.input.drain(i..self.s.input_pos);
                self.s.input_pos = i;
            }
            Code::Char(c) if !k.ctrl && !k.alt => self.insert_input(&c.to_string()),
            _ => {}
        }
    }

    fn push_history(&mut self, text: &str) {
        if self.input_hist.last().is_some_and(|l| l == text) {
            return;
        }
        self.input_hist.push(text.to_string());
        if self.input_hist.len() > MAX_INPUT_HIST {
            self.input_hist.drain(..self.input_hist.len() - MAX_INPUT_HIST);
        }
    }

    /// Moves up (-1) or down (+1) through what was sent before; the line
    /// being typed is kept as the newest entry.
    fn history(&mut self, dir: i32) {
        let n = self.input_hist.len();
        let idx = self.hist_idx as i32 + dir;
        if idx < 0 || idx > n as i32 {
            return;
        }
        let idx = idx as usize;
        if self.hist_idx == n {
            self.hist_draft = self.s.input.clone();
        }
        self.hist_idx = idx;
        self.s.input = if idx == n { self.hist_draft.clone() } else { self.input_hist[idx].chars().collect() };
        self.s.input_pos = self.s.input.len();
    }
}

/// Draws the message being typed, with its cursor, in w columns at (x, y).
pub fn draw_input(c: &mut Canvas, s: &UiState, x: i32, y: i32, w: i32) -> Option<Cursor> {
    let avail = w - 2;
    if avail < 4 {
        return None;
    }
    c.text(x, y, "› ", bold(COL_ACCENT), 2);
    // A line break is kept in the text but shown as a one-column mark
    let disp: Vec<char> = s.input.iter().map(|&ch| if ch == '\n' { '↵' } else { ch }).collect();
    let (shown, col) = input_window(&disp, s.input_pos, avail);
    c.text(x + 2, y, &shown, fg(COL_BRIGHT), avail);
    if s.input.is_empty() {
        c.text(x + 2, y, "メッセージ (Enter 送信 / Shift+Enter 改行 / Esc 取消 / ↑ 履歴)", fg(COL_FAINT), avail);
    }
    Some(Cursor { x: x + 2 + col, y })
}

/// The part of the input that fits in w columns with the cursor visible, and
/// the cursor's column within it.
fn input_window(r: &[char], pos: usize, w: i32) -> (String, i32) {
    if w <= 0 {
        return (String::new(), 0);
    }
    let pos = pos.min(r.len());
    let width = |cs: &[char]| str_width(&cs.iter().collect::<String>());
    let mut start = 0;
    while start < pos && width(&r[start..pos]) > w - 1 {
        start += 1;
    }
    let mut end = pos;
    while end < r.len() && width(&r[start..=end]) <= w {
        end += 1;
    }
    (r[start..end].iter().collect(), width(&r[start..pos]))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{press, ui};
    use super::*;

    fn typed(ui: &Ui) -> String {
        ui.s.input.iter().collect()
    }

    #[test]
    fn line_editing() {
        let (mut ui, ai) = ui(true);
        ui.on_key(press('t'));
        assert!(ui.s.input_on);
        for c in "hello world".chars() {
            ui.on_key(press(c));
        }
        ui.on_key(Key::ctrl('w'));
        assert_eq!(typed(&ui), "hello ");
        ui.on_key(Key::ctrl('a'));
        ui.on_key(press('>'));
        ui.on_key(Key::new(Code::Delete));
        assert_eq!((typed(&ui).as_str(), ui.s.input_pos), (">ello ", 1));
        ui.on_key(Key::ctrl('e'));
        ui.on_key(Key::new(Code::Backspace));
        ui.on_key(Key { shift: true, ..Key::new(Code::Enter) });
        ui.on_key(press('x'));
        assert_eq!(typed(&ui), ">ello\nx");
        ui.on_key(Key::new(Code::Left));
        ui.on_key(Key::ctrl('k'));
        assert_eq!(typed(&ui), ">ello\n");
        ui.on_key(Key::ctrl('u'));
        assert_eq!(typed(&ui), "");
        // q is typed, not a quit, while typing
        ui.on_key(press('q'));
        assert!(ui.take_effects().is_empty());
        ui.on_key(Key::ctrl('u'));
        // An empty line sends nothing
        ui.on_key(Key::new(Code::Enter));
        assert!(!ui.s.input_on && ai.take().is_empty());
    }

    #[test]
    fn history() {
        let (mut ui, ai) = ui(true);
        for msg in ["first", "second", "second"] {
            ui.open_input();
            ui.insert_input(msg);
            ui.on_key(Key::new(Code::Enter));
        }
        assert_eq!(ai.take(), ["send first", "send second", "send second"]);
        assert_eq!(ui.input_hist, ["first", "second"], "a repeat is kept once");
        ui.open_input();
        ui.insert_input("draft");
        ui.on_key(Key::new(Code::Up));
        assert_eq!(typed(&ui), "second");
        ui.on_key(Key::new(Code::Up));
        ui.on_key(Key::new(Code::Up));
        assert_eq!(typed(&ui), "first");
        ui.on_key(Key::new(Code::Down));
        ui.on_key(Key::new(Code::Down));
        assert_eq!(typed(&ui), "draft");
    }

    #[test]
    fn window_keeps_the_cursor_in_view() {
        let r: Vec<char> = "abcdefghij".chars().collect();
        assert_eq!(input_window(&r, 0, 5), ("abcde".into(), 0));
        assert_eq!(input_window(&r, 10, 5), ("ghij".into(), 4));
        assert_eq!(input_window(&r, 6, 5), ("cdefg".into(), 4));
        let wide: Vec<char> = "日本語のテキスト".chars().collect();
        assert_eq!(input_window(&wide, 8, 7), ("キスト".into(), 6));
        assert_eq!(input_window(&r, 3, 0), (String::new(), 0));
    }
}
