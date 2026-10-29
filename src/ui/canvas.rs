//! The cell buffer a frame is drawn into, the drawing primitives, and the
//! encoding of a frame as the bytes that update a terminal.

use std::fmt::Write as _;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as TuiRect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub type Rgb = (u8, u8, u8);

pub const fn hex(v: u32) -> Rgb {
    ((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

// Tron-like palette of the original eDEX-UI
pub const COL_BG: Rgb = hex(0x05080d);
pub const COL_FG: Rgb = hex(0xaacfd1);
pub const COL_BRIGHT: Rgb = hex(0xe6fdff);
pub const COL_DIM: Rgb = hex(0x5b8287);
pub const COL_FAINT: Rgb = hex(0x23393c);
pub const COL_ACCENT: Rgb = hex(0x4ff3ff);
pub const COL_WARN: Rgb = hex(0xf2c14e);
pub const COL_ERR: Rgb = hex(0xff5f56);
pub const COL_OK: Rgb = hex(0x7ee787);
pub const COL_HOT_BG: Rgb = hex(0x0f2a2e);

/// A cell's look: colors (None: the terminal's own) and bold.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct St {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub bold: bool,
}

pub const fn fg(c: Rgb) -> St {
    St { fg: Some(c), bg: None, bold: false }
}

pub const fn bold(c: Rgb) -> St {
    St { fg: Some(c), bg: None, bold: true }
}

impl St {
    pub const fn on(self, bg: Rgb) -> St {
        St { bg: Some(bg), ..self }
    }

    fn style(self) -> Style {
        let mut s = Style::default();
        if let Some((r, g, b)) = self.fg {
            s = s.fg(Color::Rgb(r, g, b));
        }
        if let Some((r, g, b)) = self.bg {
            s = s.bg(Color::Rgb(r, g, b));
        }
        if self.bold {
            s = s.add_modifier(Modifier::BOLD);
        }
        s
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
}

/// A piece of a line with one style.
#[derive(Clone, Debug, PartialEq)]
pub struct Seg {
    pub text: String,
    pub st: St,
}

pub fn seg(text: impl Into<String>, st: St) -> Seg {
    Seg { text: text.into(), st }
}

pub struct Canvas {
    pub buf: Buffer,
    pub w: i32,
    pub h: i32,
}

impl Canvas {
    pub fn new(w: i32, h: i32) -> Canvas {
        let (w, h) = (w.clamp(0, u16::MAX as i32), h.clamp(0, u16::MAX as i32));
        Canvas { buf: Buffer::empty(TuiRect::new(0, 0, w as u16, h as u16)), w, h }
    }

    /// Puts a one-column symbol at (x, y); outside the canvas it is dropped.
    pub fn cell(&mut self, x: i32, y: i32, ch: &str, st: St) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        self.buf[(x as u16, y as u16)].set_symbol(ch).set_style(st.style());
    }

    /// Draws s from (x, y), clipped to max_w columns; returns columns used.
    pub fn text(&mut self, x: i32, y: i32, s: &str, st: St, max_w: i32) -> i32 {
        if y < 0 || y >= self.h || x >= self.w || max_w <= 0 {
            return 0;
        }
        if x < 0 {
            return 0;
        }
        let clean;
        let s = if s.contains(['\t', '\n', '\r']) {
            clean = s.replace(['\t', '\n', '\r'], " ");
            &clean
        } else {
            s
        };
        let (end, _) = self.buf.set_stringn(x as u16, y as u16, s, max_w as usize, st.style());
        end as i32 - x
    }

    /// Draws s so it ends at column x+w.
    pub fn text_right(&mut self, x: i32, y: i32, w: i32, s: &str, st: St) -> i32 {
        let sw = str_width(s);
        if sw > w {
            return self.text(x, y, s, st, w);
        }
        self.text(x + w - sw, y, s, st, w)
    }

    /// Draws the segments one after another from (x, y), up to max_w columns.
    pub fn segs(&mut self, x: i32, y: i32, line: &[Seg], max_w: i32) {
        let mut cx = x;
        for s in line {
            cx += self.text(cx, y, &s.text, s.st, x + max_w - cx);
        }
    }

    pub fn fill(&mut self, r: Rect, ch: &str, st: St) {
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                self.cell(x, y, ch, st);
            }
        }
    }

    pub fn hline(&mut self, x: i32, y: i32, w: i32, ch: &str, st: St) {
        for i in 0..w {
            self.cell(x + i, y, ch, st);
        }
    }

    /// Draws a thin frame with an optional title in the top edge.
    pub fn boxed(&mut self, r: Rect, st: St, title: &str, title_st: St, right: &str, right_st: St) {
        if r.w < 2 || r.h < 2 {
            return;
        }
        let (x2, y2) = (r.x + r.w - 1, r.y + r.h - 1);
        self.cell(r.x, r.y, "┌", st);
        self.cell(x2, r.y, "┐", st);
        self.cell(r.x, y2, "└", st);
        self.cell(x2, y2, "┘", st);
        self.hline(r.x + 1, r.y, r.w - 2, "─", st);
        self.hline(r.x + 1, y2, r.w - 2, "─", st);
        for y in r.y + 1..y2 {
            self.cell(r.x, y, "│", st);
            self.cell(x2, y, "│", st);
        }
        let mut used = 0;
        if !title.is_empty() {
            used = self.text(r.x + 2, r.y, &format!(" {title} "), title_st, r.w - 4);
        }
        if !right.is_empty() && r.w - 6 - used > 4 {
            self.text_right(r.x + 2 + used + 1, r.y, r.w - 5 - used - 1, &format!(" {right} "), right_st);
        }
    }

    /// Draws a horizontal gauge of width w for a 0..1 fraction.
    pub fn bar(&mut self, x: i32, y: i32, w: i32, frac: f64, on: St, off: St) {
        let n = (frac * w as f64 + 0.5) as i32;
        for i in 0..w {
            if i < n {
                self.cell(x + i, y, "■", on);
            } else {
                self.cell(x + i, y, "·", off);
            }
        }
    }

    /// Paints a 2-pixels-per-cell image into r with half blocks: the top pixel
    /// is the foreground of ▀, the bottom one its background. Cells with
    /// neither pixel are left as they are.
    pub fn pixels(&mut self, r: Rect, px: &[Vec<Option<Rgb>>]) {
        for cy in 0..r.h {
            let (Some(top_row), Some(bot_row)) = (px.get(2 * cy as usize), px.get(2 * cy as usize + 1)) else { break };
            for cx in 0..r.w {
                let (Some(&top), Some(&bot)) = (top_row.get(cx as usize), bot_row.get(cx as usize)) else { break };
                match (top, bot) {
                    (None, None) => {}
                    (Some(t), None) => self.cell(r.x + cx, r.y + cy, "▀", fg(t)),
                    (None, Some(b)) => self.cell(r.x + cx, r.y + cy, "▄", fg(b)),
                    (Some(t), Some(b)) if t == b => self.cell(r.x + cx, r.y + cy, "█", fg(t)),
                    (Some(t), Some(b)) => self.cell(r.x + cx, r.y + cy, "▀", fg(t).on(b)),
                }
            }
        }
    }
}

pub fn str_width(s: &str) -> i32 {
    UnicodeWidthStr::width(s) as i32
}

/// Splits s into what takes a cell or two each: a character with the
/// zero-width ones that follow it.
pub fn clusters(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut joined = false;
    for (i, ch) in s.char_indices() {
        let zero = ch.width().unwrap_or(0) == 0 && ch != '\n' && ch != '\t';
        if i > start && !zero && !joined {
            out.push(&s[start..i]);
            start = i;
        }
        joined = ch == '\u{200d}';
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Keeps the start of s within w columns.
pub fn trunc_right(s: &str, w: i32) -> String {
    if str_width(s) <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in clusters(s) {
        let cw = str_width(c);
        if used + cw > w - 1 {
            break;
        }
        out.push_str(c);
        used += cw;
    }
    out.push('…');
    out
}

/// Wraps text to lines of at most w columns: at spaces where there are any,
/// between wide characters (Japanese has no spaces), and inside a word that
/// is longer than a line.
pub fn wrap_text(text: &str, w: i32) -> Vec<String> {
    let w = w.max(1);
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut line_w = 0;
        let mut word = String::new();
        let mut word_w = 0;
        let flush_word = |line: &mut String, line_w: &mut i32, word: &mut String, word_w: &mut i32, lines: &mut Vec<String>| {
            if word.is_empty() {
                return;
            }
            if *line_w + *word_w > w && *line_w > 0 {
                lines.push(line.trim_end().to_string());
                line.clear();
                *line_w = 0;
            }
            if *word_w <= w {
                line.push_str(word);
                *line_w += *word_w;
            } else {
                for c in clusters(word) {
                    let cw = str_width(c);
                    if *line_w + cw > w && *line_w > 0 {
                        lines.push(std::mem::take(line));
                        *line_w = 0;
                    }
                    line.push_str(c);
                    *line_w += cw;
                }
            }
            word.clear();
            *word_w = 0;
        };
        for c in clusters(para) {
            let cw = str_width(c);
            if c == " " || c == "\t" {
                flush_word(&mut line, &mut line_w, &mut word, &mut word_w, &mut lines);
                if line_w > 0 && line_w < w {
                    line.push(' ');
                    line_w += 1;
                } else if line_w > 0 {
                    lines.push(line.trim_end().to_string());
                    line.clear();
                    line_w = 0;
                }
                continue;
            }
            if cw >= 2 {
                // A wide character may start a new line by itself
                flush_word(&mut line, &mut line_w, &mut word, &mut word_w, &mut lines);
                word.push_str(c);
                word_w = cw;
                flush_word(&mut line, &mut line_w, &mut word, &mut word_w, &mut lines);
                continue;
            }
            word.push_str(c);
            word_w += cw;
        }
        flush_word(&mut line, &mut line_w, &mut word, &mut word_w, &mut lines);
        lines.push(line.trim_end().to_string());
    }
    lines
}

/// Lays styled text out as lines of w columns.
pub struct LineBuilder {
    pub w: i32,
    pub out: Vec<Vec<Seg>>,
}

impl LineBuilder {
    pub fn new(w: i32) -> LineBuilder {
        LineBuilder { w, out: Vec::new() }
    }

    pub fn line(&mut self, segs: Vec<Seg>) {
        self.out.push(segs);
    }

    /// Adds text wrapped after a prefix (continuation lines are indented as
    /// wide as the prefix). max_lines > 0 cuts it short with an ellipsis.
    pub fn wrap(&mut self, prefix: &str, pst: St, text: &str, tst: St, max_lines: usize) {
        let pw = str_width(prefix);
        let bw = (self.w - pw).max(4);
        let mut lines = wrap_text(text, bw);
        if max_lines > 0 && lines.len() > max_lines {
            lines.truncate(max_lines);
            let last = format!("{}…", lines[max_lines - 1]);
            lines[max_lines - 1] = trunc_right(&last, bw);
        }
        for (i, ln) in lines.into_iter().enumerate() {
            let p = if i == 0 { prefix.to_string() } else { " ".repeat(pw as usize) };
            self.line(vec![seg(p, pst), seg(ln, tst)]);
        }
    }
}

/// Where the text cursor goes after a frame (None: hidden).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cursor {
    pub x: i32,
    pub y: i32,
}

/// The bytes that turn a terminal showing prev into one showing next (the
/// whole of next when prev is None or of another size).
pub fn encode_frame(prev: Option<&Buffer>, next: &Buffer, cursor: Option<Cursor>) -> Vec<u8> {
    let mut out = String::from("\x1b[?2026h");
    let blank;
    let prev = match prev {
        Some(p) if p.area == next.area => p,
        _ => {
            out.push_str("\x1b[0m\x1b[2J");
            blank = Buffer::empty(next.area);
            &blank
        }
    };
    let mut at: Option<(u16, u16)> = None;
    let mut style: Option<(Color, Color, Modifier)> = None;
    for (x, y, cell) in prev.diff(next) {
        if at != Some((x, y)) {
            let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
        }
        let want = (cell.fg, cell.bg, cell.modifier);
        if style != Some(want) {
            out.push_str("\x1b[0");
            if cell.modifier.contains(Modifier::BOLD) {
                out.push_str(";1");
            }
            if let Color::Rgb(r, g, b) = cell.fg {
                let _ = write!(out, ";38;2;{r};{g};{b}");
            }
            if let Color::Rgb(r, g, b) = cell.bg {
                let _ = write!(out, ";48;2;{r};{g};{b}");
            }
            out.push('m');
            style = Some(want);
        }
        let symbol = cell.symbol();
        out.push_str(symbol);
        at = Some((x + str_width(symbol).max(1) as u16, y));
    }
    out.push_str("\x1b[0m");
    match cursor {
        Some(c) if c.x >= 0 && c.y >= 0 => {
            let _ = write!(out, "\x1b[{};{}H\x1b[6 q\x1b[?25h", c.y + 1, c.x + 1);
        }
        _ => out.push_str("\x1b[?25l"),
    }
    out.push_str("\x1b[?2026l");
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(c: &Canvas, y: u16) -> String {
        (0..c.w as u16).map(|x| c.buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn text_is_clipped_and_counts_wide_characters() {
        let mut c = Canvas::new(10, 2);
        assert_eq!(c.text(0, 0, "日本語abc", fg(COL_FG), 7), 7);
        assert_eq!(c.text(8, 1, "abcdef", fg(COL_FG), 10), 2);
        assert_eq!(c.text(0, 5, "off", fg(COL_FG), 10), 0);
        assert_eq!(c.text(-1, 0, "off", fg(COL_FG), 10), 0);
        assert_eq!(c.buf[(0, 0)].symbol(), "日");
        assert_eq!(c.buf[(6, 0)].symbol(), "a");
        assert_eq!(c.buf[(9, 1)].symbol(), "b");
    }

    #[test]
    fn truncation() {
        assert_eq!(trunc_right("abcdef", 4), "abc…");
        assert_eq!(trunc_right("abc", 4), "abc");
        assert_eq!(trunc_right("日本語です", 5), "日本…");
    }

    #[test]
    fn wrapping() {
        assert_eq!(wrap_text("the quick brown fox", 9), ["the quick", "brown fox"]);
        assert_eq!(wrap_text("日本語のテキストです", 8), ["日本語の", "テキスト", "です"]);
        assert_eq!(wrap_text("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap_text("a\n\nb", 10), ["a", "", "b"]);
        assert_eq!(wrap_text("go test ./... を実行", 12), ["go test", "./... を実行"]);
    }

    #[test]
    fn box_with_title() {
        let mut c = Canvas::new(20, 3);
        c.boxed(Rect::new(0, 0, 20, 3), fg(COL_DIM), "FILES", bold(COL_BRIGHT), "", St::default());
        assert_eq!(row(&c, 0), "┌─ FILES ──────────┐");
        assert_eq!(row(&c, 2), "└──────────────────┘");
    }

    #[test]
    fn frames_send_only_what_changed() {
        let mut a = Canvas::new(6, 2);
        a.text(0, 0, "abc", fg(COL_FG), 6);
        let full = String::from_utf8(encode_frame(None, &a.buf, None)).unwrap();
        assert!(full.contains("\x1b[2J") && full.contains("\x1b[1;1H") && full.contains("abc"));
        assert!(full.ends_with("\x1b[?25l\x1b[?2026l"));

        let mut b = Canvas::new(6, 2);
        b.text(0, 0, "abd", fg(COL_FG), 6);
        let diff = String::from_utf8(encode_frame(Some(&a.buf), &b.buf, Some(Cursor { x: 3, y: 0 }))).unwrap();
        assert!(!diff.contains("\x1b[2J") && !diff.contains("ab"));
        assert!(diff.contains("\x1b[1;3H") && diff.contains('d'));
        assert!(diff.contains("\x1b[1;4H\x1b[6 q\x1b[?25h"));
    }
}
