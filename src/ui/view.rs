//! Draws the window from a UiState: a frame titled VOICE-AGENT with the CPU
//! in its top edge, the conversation with the mascot to its right, a status
//! line with Claude's usage, the approval while one waits, and the keys in
//! the bottom edge. The drawing is behind View, so another kind of display
//! can take the same state later.

use super::canvas::{
    COL_ACCENT, COL_BRIGHT, COL_DIM, COL_ERR, COL_FAINT, COL_FG, COL_HOT_BG, COL_OK, COL_WARN, Canvas, Cursor, LineBuilder, Rect, Seg, St, bold, fg, str_width,
    trunc_right,
};
use super::input::draw_input;
use super::mascot::{MASCOT_COLS, Mascot};
use super::usage::usage_brief;
use super::{LogEntry, Talk, UiState};
use crate::app::EventKind;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPARKS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
/// CPU samples in the top edge
const SPARK_N: usize = 8;
/// Below this the window only says it is too small.
const MIN_W: i32 = 40;
const MIN_H: i32 = 8;
/// What the user and the agent say starts after this many columns.
const SPEAKER_W: usize = 8;

/// One frame, and how far up the conversation can be scrolled in it.
pub struct Frame {
    pub canvas: Canvas,
    pub cursor: Option<Cursor>,
    pub max_scroll: usize,
}

pub trait View {
    /// Draws the state into w x h cells.
    fn render(&mut self, s: &UiState, w: i32, h: i32) -> Frame;
}

/// The terminal UI. The mascot's whereabouts are its own: they follow the
/// states it sees go by.
#[derive(Default)]
pub struct TuiView {
    pub(super) mascot: Mascot,
    state: String,
    errors: u64,
    aborts: u64,
}

impl View for TuiView {
    fn render(&mut self, s: &UiState, w: i32, h: i32) -> Frame {
        self.follow(s);
        let mut c = Canvas::new(w, h);
        if w < MIN_W || h < MIN_H {
            c.text(0, 0, &format!("小さすぎます ({MIN_W}x{MIN_H} 以上)"), fg(COL_WARN), w);
            return Frame { canvas: c, cursor: None, max_scroll: 0 };
        }
        let edge = match s.state.as_str() {
            "LISTENING" => fg(COL_ERR),
            "CONFIRM" => fg(COL_WARN),
            _ => fg(COL_DIM),
        };
        let (cpu, cpu_st) = cpu_label(&s.cpu);
        c.boxed(Rect::new(0, 0, w, h), edge, "VOICE-AGENT", bold(COL_BRIGHT), &cpu, cpu_st);
        // Bottom-up: the keys in the bottom edge, the approval, the status line
        let mut y = h - 1;
        draw_keys(&mut c, s, y, w);
        if let Some(a) = &s.approval {
            y -= 1;
            draw_approval(&mut c, a, y, w);
        }
        y -= 1;
        draw_status(&mut c, s, y, w);
        y -= 1;
        c.cell(0, y, "├", edge);
        c.hline(1, y, w - 2, "─", edge);
        c.cell(w - 1, y, "┤", edge);
        let (cursor, max_scroll) = self.draw_body(&mut c, s, Rect::new(2, 1, w - 4, y - 1));
        Frame { canvas: c, cursor, max_scroll }
    }
}

impl TuiView {
    /// Keeps the mascot in step with the agent: its state, errors, aborts.
    fn follow(&mut self, s: &UiState) {
        if s.state != self.state {
            self.mascot.on_state(&self.state, &s.state, s.now_ms);
            self.state = s.state.clone();
        }
        if s.errors != self.errors {
            self.errors = s.errors;
            self.mascot.sad(s.now_ms, true);
        }
        if s.aborts != self.aborts {
            self.aborts = s.aborts;
            self.mascot.sad(s.now_ms, false);
        }
        if s.mascot {
            self.mascot.step(s.now_ms, &s.state, MASCOT_COLS);
        }
    }

    /// The conversation (and the line being typed) with the mascot to its
    /// right; returns the cursor and how far the conversation can scroll.
    fn draw_body(&mut self, c: &mut Canvas, s: &UiState, r: Rect) -> (Option<Cursor>, usize) {
        if r.h < 1 || r.w < 10 {
            return (None, 0);
        }
        let (stage, mut inner) = self.mascot_stage(s, r);
        if !stage.empty() {
            self.draw_mascot(c, stage, s);
        }
        let mut cursor = None;
        if s.input_on {
            cursor = draw_input(c, s, inner.x, inner.y + inner.h - 1, inner.w);
            inner.h -= 1;
        }
        if inner.h <= 0 {
            return (cursor, 0);
        }
        let lines = log_lines(&s.log, inner.w);
        if lines.is_empty() {
            let hint = if s.kitty {
                "Space を長押しして話しかけてください · t で文字入力"
            } else {
                "Space で話し始め、もう一度 Space で送信 · t で文字入力"
            };
            c.text(inner.x, inner.y, &format!("◇ {hint}"), fg(COL_DIM), inner.w);
            return (cursor, 0);
        }
        let max = lines.len().saturating_sub(inner.h as usize);
        let scroll = s.scroll.min(max);
        let end = lines.len() - scroll;
        let start = end.saturating_sub(inner.h as usize);
        for (i, ln) in lines[start..end].iter().enumerate() {
            c.segs(inner.x, inner.y + i as i32, ln, inner.w);
        }
        if scroll > 0 {
            c.text_right(inner.x, inner.y, inner.w, &format!("▲ {scroll}"), bold(COL_WARN));
        }
        (cursor, max)
    }
}

/// The conversation as wrapped, styled lines.
fn log_lines(log: &[LogEntry], w: i32) -> Vec<Vec<Seg>> {
    let pad = " ".repeat(SPEAKER_W);
    let speaker = |name: &str| format!("{name}{}", " ".repeat(SPEAKER_W.saturating_sub(str_width(name) as usize)));
    let mut b = LineBuilder::new(w);
    for e in log {
        match e.kind {
            EventKind::User => b.wrap(&speaker("あなた"), bold(COL_ACCENT), &e.text, bold(COL_BRIGHT), 0),
            EventKind::Assistant => {
                let text = if e.open { format!("{}▌", e.text) } else { e.text.clone() };
                for (i, para) in text.trim_end_matches('\n').split('\n').enumerate() {
                    let who = if i == 0 { speaker("AI") } else { pad.clone() };
                    b.wrap(&who, bold(COL_OK), para, fg(COL_FG), 0);
                }
            }
            EventKind::Tool => {
                let st = if e.tag == "blocked" { fg(COL_ERR) } else { fg(COL_DIM) };
                b.wrap(&format!("{pad}[{}] ", e.tag), bold(COL_DIM), &e.text, st, 0);
            }
            EventKind::Approval => {
                let (res, st) = match e.allow {
                    Some(true) => ("許可", fg(COL_OK)),
                    Some(false) => ("拒否", fg(COL_ERR)),
                    None => ("…", fg(COL_WARN)),
                };
                b.wrap(&format!("{pad}[確認] "), bold(COL_WARN), &format!("{}  → {res}", e.text), st, 0);
            }
            EventKind::System => b.wrap("◇ ", fg(COL_ACCENT), &e.text, fg(COL_ACCENT), 0),
            EventKind::Notice => b.wrap(&pad, fg(COL_DIM), &e.text, fg(COL_DIM), 0),
            EventKind::Error => b.wrap("✖ ", bold(COL_ERR), &e.text, fg(COL_ERR), 0),
            _ => {}
        }
    }
    b.out
}

/// What the agent is doing, as the status line says it.
fn status_label(s: &UiState) -> (String, St) {
    let frame = SPINNER[(s.now_ms / 100).rem_euclid(SPINNER.len() as i64) as usize];
    let blink = s.now_ms / 400 % 2 == 0;
    match s.state.as_str() {
        "LISTENING" if blink => ("● 聞き取り中".into(), bold(COL_ERR)),
        "LISTENING" => ("○ 聞き取り中".into(), fg(COL_ERR)),
        "TRANSCRIBING" => (format!("{frame} 文字起こし中"), bold(COL_WARN)),
        "THINKING" => (format!("{frame} 考え中"), bold(COL_ACCENT)),
        "EXECUTING" => (format!("{frame} 実行中"), bold(COL_ACCENT)),
        "SPEAKING" => ("◖ 読み上げ中".into(), bold(COL_OK)),
        "AGENTS" => (format!("{frame} エージェント作業中"), bold(COL_ACCENT)),
        "CONFIRM" if blink => ("▲ 承認待ち".into(), bold(COL_WARN)),
        "CONFIRM" => ("△ 承認待ち".into(), fg(COL_WARN)),
        "MIC_OFF" => ("✕ マイクオフ".into(), fg(COL_WARN)),
        _ => ("◇ 待機".into(), fg(COL_DIM)),
    }
}

/// The status line: the state (with the mic's level while listening), what
/// works in the background, a fresh note, and Claude's usage at the right.
fn draw_status(c: &mut Canvas, s: &UiState, y: i32, w: i32) {
    let (x0, right) = (2, w - 2);
    let (label, st) = status_label(s);
    let mut x = x0 + c.text(x0, y, &label, st, right - x0);
    let mut items: Vec<(String, St)> = Vec::new();
    if s.state == "LISTENING" {
        let hint = if s.talk == Talk::Toggle { "Space で送信 / Esc 取消" } else { "離すと送信 / Esc 取消" };
        let bw = (right - x - 6 - str_width(hint) - 2).clamp(0, 24);
        if bw >= 4 {
            x += 1 + c.text(x + 1, y, "MIC", bold(COL_ERR), 3);
            c.bar(x + 1, y, bw, s.level, fg(COL_ERR), fg(COL_FAINT));
            x += 1 + bw;
        }
        items.push((hint.into(), fg(COL_DIM)));
    } else if !s.mic_on && s.state != "MIC_OFF" {
        items.push(("マイクオフ".into(), fg(COL_WARN)));
    }
    if s.agents > 0 {
        items.push((format!("エージェント ×{}", s.agents), fg(COL_ACCENT)));
    }
    if s.jobs > 0 {
        items.push((format!("ジョブ ×{}", s.jobs), fg(COL_ACCENT)));
    }
    if !s.note.is_empty() {
        items.push((format!("▸ {}", s.note), bold(COL_BRIGHT)));
    }
    for (text, st) in items {
        let need = str_width(&text) + 2;
        if x + need > right {
            break;
        }
        x += 2 + c.text(x + 2, y, &text, st, right - x - 2);
    }
    let (brief, bst) = usage_brief(&s.usage, s.now_ms, right - x - 3);
    if !brief.is_empty() {
        c.text_right(x0, y, right - x0, &brief, bst);
    }
}

/// The approval waiting for the user, on a line of its own.
fn draw_approval(c: &mut Canvas, a: &LogEntry, y: i32, w: i32) {
    let st = |s: St| s.on(COL_HOT_BG);
    c.fill(Rect::new(1, y, w - 2, 1), " ", St::default().on(COL_HOT_BG));
    let mut x = 2;
    let right = w - 2;
    x += c.text(x, y, &format!("▲ 確認: {}", a.tag), st(bold(COL_WARN)), right - x);
    let hint = "「はい」「いいえ」か y / n";
    let room = right - x - 3 - str_width(hint) - 2;
    if room >= 8 {
        x += 2 + c.text(x + 2, y, &format!("$ {}", trunc_right(&a.text, room - 2)), st(fg(COL_BRIGHT)), room);
    }
    if x + 2 + str_width(hint) <= right {
        c.text_right(x, y, right - x, hint, st(fg(COL_FG)));
    }
}

/// The keys that work now, in the bottom edge.
fn draw_keys(c: &mut Canvas, s: &UiState, y: i32, w: i32) {
    let items: &[(&str, &str)] = if s.input_on {
        &[("Enter", "送信"), ("Esc", "取消"), ("↑↓", "履歴"), ("^U", "消去")]
    } else if s.talk == Talk::Hold {
        &[("離す", "送信"), ("Esc", "取消")]
    } else if s.talk == Talk::Toggle {
        &[("Space/Enter", "送信"), ("Esc", "取消")]
    } else if s.kitty {
        &[("Space長押し", "話す"), ("t", "文字"), ("y/n", "承認"), ("m", "マイク"), ("Esc", "止める"), ("↑↓", "スクロール"), ("q", "終了")]
    } else {
        &[("Space", "話す"), ("t", "文字"), ("y/n", "承認"), ("m", "マイク"), ("Esc", "止める"), ("↑↓", "スクロール"), ("q", "終了")]
    };
    let mut x = 2;
    for (k, what) in items {
        let need = str_width(k) + str_width(what) + 3;
        if x + need > w - 3 {
            break;
        }
        x += c.text(x, y, &format!(" {k}"), bold(COL_ACCENT), need);
        x += c.text(x, y, &format!(" {what} "), fg(COL_DIM), need);
    }
}

/// "CPU ▂▃▅▂ 23%" for the top edge, colored by how busy the CPU is now.
fn cpu_label(cpu: &[f64]) -> (String, St) {
    let Some(&now) = cpu.last() else { return (String::new(), St::default()) };
    let spark: String = cpu[cpu.len().saturating_sub(SPARK_N)..].iter().map(|v| SPARKS[((v / 100.0 * 7.0).round() as usize).min(7)]).collect();
    let st = if now >= 85.0 {
        fg(COL_ERR)
    } else if now >= 60.0 {
        fg(COL_WARN)
    } else {
        fg(COL_FG)
    };
    (format!("CPU {spark} {:.0}%", now), st)
}

#[cfg(test)]
mod tests {
    use super::super::Ui;
    use super::super::tests::ui;
    use super::*;
    use crate::agent::usage::Usage;
    use crate::app::Event;

    fn screen(c: &Canvas) -> Vec<String> {
        (0..c.h as u16)
            .map(|y| {
                let mut line = String::new();
                let mut skip = 0;
                for x in 0..c.w as u16 {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    let sym = c.buf[(x, y)].symbol();
                    skip = str_width(sym).max(1) - 1;
                    line.push_str(sym);
                }
                line.trim_end().to_string()
            })
            .collect()
    }

    fn feed(ui: &mut Ui, events: Vec<Event>) {
        ui.on_events(events);
    }

    #[test]
    fn the_window_has_its_parts() {
        let (mut ui, _) = ui(true);
        let mut v = TuiView::default();
        for p in [10.0, 30.0, 95.0] {
            ui.on_cpu(p);
        }
        let f = v.render(&ui.s, 100, 20);
        let lines = screen(&f.canvas);
        assert!(lines[0].starts_with("┌─ VOICE-AGENT ") && lines[0].ends_with(" CPU ▂▃█ 95% ──┐"), "{}", lines[0]);
        assert!(lines[1].contains("◇ Space を長押しして話しかけてください"), "{}", lines[1]);
        assert!(lines[17].starts_with("├───"), "{}", lines[17]);
        assert!(lines[18].starts_with("│ ◇ 待機"), "{}", lines[18]);
        assert!(lines[19].starts_with("└─ Space長押し 話す  t 文字  y/n 承認 "), "{}", lines[19]);
        assert!(f.cursor.is_none());

        ui.s.usage = Usage { turns: 1, cost_usd: 0.42, output: 1200, ..Usage::default() };
        feed(
            &mut ui,
            vec![
                Event { kind: EventKind::User, text: "テストを走らせて".into(), via: "voice".into(), ..Event::default() },
                Event { kind: EventKind::Assistant, text: "テストを実行します。".into(), ..Event::default() },
                Event { kind: EventKind::Tool, tag: "exec".into(), text: "cargo test".into(), ..Event::default() },
                Event {
                    kind: EventKind::Approval,
                    id: "ap1".into(),
                    reason: "リモートへ push します".into(),
                    text: "git push origin main".into(),
                    ..Event::default()
                },
                Event { kind: EventKind::Status, state: "CONFIRM".into(), ..Event::default() },
            ],
        );
        ui.s.now_ms = 0; // the blink's "on" phase
        let lines = screen(&v.render(&ui.s, 100, 20).canvas);
        assert!(lines[1].starts_with("│ あなた  テストを走らせて"), "{}", lines[1]);
        assert!(lines[2].starts_with("│ AI      テストを実行します。"), "{}", lines[2]);
        assert!(lines[3].starts_with("│         [exec] cargo test"), "{}", lines[3]);
        assert!(lines[4].starts_with("│         [確認] git push origin main  → …"), "{}", lines[4]);
        assert!(lines[17].starts_with("│ ▲ 承認待ち") && lines[17].ends_with("$0.42 · 1.2k tok │"), "{}", lines[17]);
        assert!(
            lines[18].starts_with("│ ▲ 確認: リモートへ push します  $ git push origin main") && lines[18].ends_with("「はい」「いいえ」か y / n │"),
            "{}",
            lines[18]
        );
    }

    #[test]
    fn typing_shows_a_cursor_and_the_conversation_scrolls() {
        let (mut ui, _) = ui(true);
        let mut v = TuiView::default();
        let many = (0..40).map(|i| Event { kind: EventKind::Notice, text: format!("line {i}"), ..Event::default() }).collect();
        feed(&mut ui, many);
        let f = v.render(&ui.s, 80, 12);
        // 8 rows of conversation: 40 lines scroll up to 32
        assert_eq!(f.max_scroll, 32);
        assert!(screen(&f.canvas)[8].contains("line 39"));
        ui.s.scroll = 99;
        ui.clamp_scroll(f.max_scroll);
        let lines = screen(&v.render(&ui.s, 80, 12).canvas);
        assert!(lines[1].contains("line 0") && lines[1].contains("▲ 32"), "{}", lines[1]);

        ui.on_key(super::super::tests::press('t'));
        ui.on_key(super::super::tests::press('a'));
        let f = v.render(&ui.s, 80, 12);
        assert_eq!(f.cursor, Some(Cursor { x: 5, y: 8 }));
        assert!(screen(&f.canvas)[11].contains("Enter 送信"));
    }

    #[test]
    fn listening_shows_the_mic() {
        let (mut ui, _) = ui(true);
        ui.on_key(super::super::tests::press(' '));
        ui.on_hold_timer(1);
        ui.on_events(vec![Event { kind: EventKind::Status, state: "LISTENING".into(), ..Event::default() }]);
        ui.on_tick(chrono::Local::now());
        ui.s.now_ms = 0;
        let lines = screen(&TuiView::default().render(&ui.s, 100, 12).canvas);
        assert!(
            lines[10].starts_with("│ ● 聞き取り中 MIC ■") && lines[10].contains("離すと送信 / Esc 取消") && lines[10].contains("エージェント ×1  ジョブ ×2"),
            "{}",
            lines[10]
        );
        assert!(lines[11].starts_with("└─ 離す 送信  Esc 取消"), "{}", lines[11]);
    }

    #[test]
    fn too_small_says_so() {
        let (ui, _) = ui(true);
        let lines = screen(&TuiView::default().render(&ui.s, 30, 5).canvas);
        assert_eq!(lines[0], "小さすぎます (40x8 以上)");
    }

    #[test]
    fn cpu() {
        assert_eq!(cpu_label(&[]).0, "");
        let (text, st) = cpu_label(&[0.0; 20]);
        assert_eq!((text.as_str(), st), ("CPU ▁▁▁▁▁▁▁▁ 0%", fg(COL_FG)));
        assert_eq!(cpu_label(&[100.0, 61.0]).1, fg(COL_WARN));
    }
}
