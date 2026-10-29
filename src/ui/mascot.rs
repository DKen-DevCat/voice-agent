//! The mascot is a small pixel-art companion to the right of the
//! conversation: white and gold cracked armor, a glowing amber visor, long
//! white-gold hair. She is drawn with half blocks (two pixels per cell) in
//! 24-bit color, no image protocol. While the assistant idles she walks about
//! and looks around; otherwise she acts out its state: a hand to the helmet
//! while listening, a hand on the chin while thinking, typing at a console
//! while a command runs, gesturing while speaking, and a little jump with
//! sparkles when a job is done. With the mic off she sits down.

use super::UiState;
use super::canvas::{COL_ACCENT, COL_DIM, COL_ERR, COL_OK, COL_WARN, Canvas, Rect, Rgb, St, bold, fg, hex, str_width};
use super::view::TuiView;

const SPRITE_W: usize = 10; // pixels
const SPRITE_H: usize = 14; // pixels, two per cell row
const MASCOT_ROWS: i32 = SPRITE_H as i32 / 2;
pub const MASCOT_COLS: i32 = 20; // the stage, in cells (one pixel per column)
const MASCOT_MIN_LOG: i32 = 40; // the log keeps at least this many columns beside her
const WALK_STEP_MS: i64 = 140;
const CHEER_FOR_MS: i64 = 2400;
const SHAKE_FOR_MS: i64 = 1000;
const SAD_FOR_MS: i64 = 3000; // no cheering this soon after an error or abort

type Frame = [[u8; SPRITE_W]; SPRITE_H];

const fn frame(rows: [&str; SPRITE_H]) -> Frame {
    let mut f = [[b'.'; SPRITE_W]; SPRITE_H];
    let mut y = 0;
    while y < SPRITE_H {
        let row = rows[y].as_bytes();
        assert!(row.len() == SPRITE_W);
        let mut x = 0;
        while x < SPRITE_W {
            f[y][x] = row[x];
            x += 1;
        }
        y += 1;
    }
    f
}

fn palette(ch: u8) -> Option<Rgb> {
    Some(match ch {
        b'W' => hex(0xf4f6fa), // white armor
        b'K' => hex(0x14161c), // black undersuit
        b'G' => hex(0xc9962e), // gold trim and plates
        b'Y' => hex(0xffe66d), // visor highlight
        b'O' => hex(0xff9f1a), // amber visor
        b'o' => hex(0x6e4210), // visor, unlit
        b'H' => hex(0xead394), // hair, near the helmet
        b'h' => hex(0xd8a243), // hair, further down
        b'R' => hex(0xff5a3c), // the glow in the cracks
        _ => return None,
    })
}

const VISOR_BRIGHT: &[(u8, u8)] = &[(b'O', b'Y')];
const VISOR_DIM: &[(u8, u8)] = &[(b'Y', b'O')];
const VISOR_DARK: &[(u8, u8)] = &[(b'O', b'o'), (b'Y', b'o')];

/// Standing, facing the viewer
const FR_FRONT: Frame = frame([
    "....GG....",
    "...WGGW...",
    "..WOOOOW..",
    ".HWYYOOWH.",
    ".HWWOOWWH.",
    ".HH.KK.HH.",
    "HHWWGGWWHH",
    "HWKGGGGKWH",
    "hWKKRRKKWh",
    ".W.WKKW.W.",
    "..WW..WW..",
    "..WG..GW..",
    "..WW..WW..",
    "..KK..KK..",
]);

/// Facing right, hair streaming behind; the first of the walk frames
const FR_SIDE: Frame = frame([
    "....GG....",
    "...WGGW...",
    "..HWOOOO..",
    ".HHWYYOO..",
    ".HHWWOO...",
    ".HHHKK....",
    ".HHWWGG...",
    ".hHWKGG...",
    ".hhWKRK...",
    "..h.WKW...",
    "....WW....",
    "....WG....",
    "....WW....",
    "....KK....",
]);

/// A hand to the helmet, listening
const FR_LISTEN: Frame = frame([
    "....GG....",
    "...WGGW...",
    "..WOOOOW..",
    ".HWYYOOWHW",
    ".HWWOOWWHW",
    ".HH.KK.HHW",
    "HHWWGGWWHW",
    "HWKGGGGKWH",
    "hWKKRRKKWh",
    ".W.WKKW...",
    "..WW..WW..",
    "..WG..GW..",
    "..WW..WW..",
    "..KK..KK..",
]);

/// A hand on the chin (the head is tilted when drawn)
const FR_THINK: Frame = frame([
    "....GG....",
    "...WGGW...",
    "..WOOOOW..",
    ".HWYYOOWH.",
    ".HWWOOWWH.",
    ".HH.KKWHH.",
    "HHWWGGWWHH",
    "HWKGGGGWWH",
    "hWKKRRKKWh",
    ".W.WKKW...",
    "..WW..WW..",
    "..WG..GW..",
    "..WW..WW..",
    "..KK..KK..",
]);

/// Both arms up
const FR_CHEER: Frame = frame([
    "....GG....",
    ".W.WGGW.W.",
    ".WWOOOOWW.",
    ".WWYYOOWW.",
    ".WWWOOWWW.",
    ".WH.KK.HW.",
    "HWWWGGWWWH",
    "H.KGGGGK.H",
    "h.KKRRKK.h",
    "...WKKW...",
    "..WW..WW..",
    "..WG..GW..",
    "..WW..WW..",
    "..KK..KK..",
]);

/// Sitting, asleep (the visor is unlit when drawn)
const FR_SIT: Frame = frame([
    "..........",
    "..........",
    "....GG....",
    "...WGGW...",
    "..WOOOOW..",
    ".HWYYOOWH.",
    ".HWWOOWWH.",
    ".HH.KK.HH.",
    "HHWWGGWWHH",
    "HWKGGGGKWH",
    "hWKKRRKKWh",
    ".W.WKKW.W.",
    ".WWWWWWWW.",
    ".KK....KK.",
]);

fn with_rows(mut f: Frame, from: usize, rows: &[&str]) -> Frame {
    for (i, r) in rows.iter().enumerate() {
        f[from + i].copy_from_slice(r.as_bytes());
    }
    f
}

fn with_legs(f: Frame, rows: &[&str]) -> Frame {
    with_rows(f, SPRITE_H - rows.len(), rows)
}

/// Walk cycle: legs together (FR_SIDE), half stride, full stride
fn fr_walk(k: usize) -> Frame {
    match k {
        1 => with_legs(FR_SIDE, &["....WW....", "...G..G...", "...W..W...", "...K..K..."]),
        2 => with_legs(FR_SIDE, &["...WWWW...", "..WG..GW..", "..W....W..", "..K....K.."]),
        _ => FR_SIDE,
    }
}

/// Typing at a console in front of her (drawn mirrored, facing the shell)
fn fr_type(k: usize) -> Frame {
    if k == 0 { with_rows(FR_SIDE, 7, &[".hHWKGGWW.", ".hhWKRK..."]) } else { with_rows(FR_SIDE, 7, &[".hHWKGG...", ".hhWKRKWW."]) }
}

/// An arm out, making a point
fn fr_speak() -> Frame {
    with_rows(FR_FRONT, 6, &["HHWWGGWWWW", "HWKGGGGKWH", "hWKKRRKKWh", ".W.WKKW..."])
}

/// Hands on the hips, waiting for an answer
fn fr_confirm() -> Frame {
    with_rows(FR_FRONT, 8, &["h.WKRRKW.h", "...WKKW..."])
}

/// Pixels of lift per 100 ms while cheering: two hops and a bounce
const CHEER_HOPS: [i32; 17] = [0, 2, 4, 5, 4, 2, 0, 0, 2, 4, 5, 4, 2, 0, 0, 1, 0];

/// Sparkles around her while cheering: cell offsets from her top-left cell
const CHEER_SPARKS: [(i32, i32, &str); 6] =
    [(-2, 1, "✦"), (SPRITE_W as i32 + 1, 0, "✧"), (-3, 3, "·"), (SPRITE_W as i32 + 2, 2, "✦"), (-1, 0, "+"), (SPRITE_W as i32 + 1, 4, "·")];

/// Flips a frame to face the other way.
fn mirror(mut f: Frame) -> Frame {
    for row in &mut f {
        row.reverse();
    }
    f
}

/// Swaps palette letters (a brighter or an unlit visor).
fn recolor(mut f: Frame, swap: &[(u8, u8)]) -> Frame {
    for ch in f.iter_mut().flatten() {
        if let Some(&(_, to)) = swap.iter().find(|(from, _)| from == ch) {
            *ch = to;
        }
    }
    f
}

/// Moves the helmet (the top five rows) by dx, dy pixels: a tilt or a nod.
/// Where it lands on the neck row it covers it.
fn head_shift(f: Frame, dx: i32, dy: i32) -> Frame {
    const HEAD_ROWS: usize = 5;
    let mut out = f;
    for row in out.iter_mut().take(HEAD_ROWS) {
        row.fill(b'.');
    }
    for (y, row) in f.iter().enumerate().take(HEAD_ROWS) {
        for (x, &ch) in row.iter().enumerate() {
            if ch == b'.' {
                continue;
            }
            let (tx, ty) = (x as i32 + dx, y as i32 + dy);
            if tx >= 0 && (tx as usize) < SPRITE_W && ty >= 0 && (ty as usize) < SPRITE_H {
                out[ty as usize][tx as usize] = ch;
            }
        }
    }
    out
}

/// Where she is and what she is up to; the drawing follows from it and the
/// clock (all times in unix milliseconds).
#[derive(Default)]
pub struct Mascot {
    x: i32,   // left edge in stage pixels
    dir: i32, // facing (+1 right, -1 left)
    walking: bool,
    target: i32,
    until: i64,      // standing still until then
    step_at: i64,    // the last step
    face_front: i64, // standing, she looks the way she walked until then
    cheer_at: i64,   // the job-done jump started (0: not jumping)
    shake_at: i64,   // an error shook her
    sad_at: i64,     // an error or an abort; no cheering right after
    placed: bool,
    rnd: u64,
}

/// A mark beside her: cells from her top-left cell, the text and its style.
struct Fx(i32, i32, String, St);

impl Mascot {
    fn cheering(&self, now: i64) -> bool {
        self.cheer_at != 0 && now - self.cheer_at < CHEER_FOR_MS
    }

    fn shaking(&self, now: i64) -> bool {
        self.shake_at != 0 && now - self.shake_at < SHAKE_FOR_MS
    }

    /// The next of a stream of pseudo-random numbers below n.
    fn rand(&mut self, n: u64) -> u64 {
        // xorshift64*
        self.rnd ^= self.rnd >> 12;
        self.rnd ^= self.rnd << 25;
        self.rnd ^= self.rnd >> 27;
        (self.rnd.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 33) % n.max(1)
    }

    fn pause(&mut self) -> i64 {
        1500 + self.rand(4000) as i64
    }

    fn ensure(&mut self, now: i64, stage_w: i32) {
        if self.rnd == 0 {
            self.rnd = (now as u64) ^ 0x5eed_5eed_5eed_5eed | 1;
        }
        if !self.placed {
            self.placed = true;
            self.x = (stage_w - SPRITE_W as i32) / 2;
            self.dir = 1;
            self.until = now + 2000;
        }
    }

    /// Moves her along: every tick while the assistant is in state and the
    /// stage is stage_w pixels wide. Idle, she walks to random spots and
    /// stands a while between; otherwise she stays put and faces the viewer.
    pub fn step(&mut self, now: i64, state: &str, stage_w: i32) {
        self.ensure(now, stage_w);
        let max_x = (stage_w - SPRITE_W as i32).max(0);
        self.x = self.x.clamp(0, max_x);
        if state != "STANDBY" || self.cheering(now) || self.shaking(now) {
            self.walking = false;
            self.face_front = now;
            return;
        }
        if self.walking {
            if now - self.step_at >= WALK_STEP_MS {
                self.step_at = now;
                self.x += self.dir;
                if self.x == self.target || self.x <= 0 || self.x >= max_x {
                    self.walking = false;
                    self.until = now + self.pause();
                    self.face_front = now + 700;
                }
            }
            return;
        }
        if now > self.until {
            self.target = self.rand(max_x as u64 + 1) as i32;
            if self.target == self.x {
                self.until = now + 1000;
                return;
            }
            self.dir = if self.target < self.x { -1 } else { 1 };
            self.walking = true;
            self.step_at = now;
        }
    }

    /// Sees the assistant go from one state to the next. Work finishing (back
    /// to STANDBY from thinking, running, speaking or minding agents) is
    /// celebrated, unless it ended in an error or an abort.
    pub fn on_state(&mut self, prev: &str, next: &str, now: i64) {
        if next != "STANDBY" {
            return;
        }
        self.until = now + 2000;
        if matches!(prev, "THINKING" | "EXECUTING" | "SPEAKING" | "AGENTS") && (self.sad_at == 0 || now - self.sad_at > SAD_FOR_MS) {
            self.cheer_at = now;
            self.until = now + CHEER_FOR_MS + 2000;
        }
    }

    /// An error (she flinches) or an abort (she just does not cheer).
    pub fn sad(&mut self, now: i64, flinch: bool) {
        self.sad_at = now;
        if flinch {
            self.shake_at = now;
        }
    }
}

impl TuiView {
    /// The frame to draw now, her pixel offsets (sideways, lift off the
    /// floor) and the marks around her.
    fn mascot_pose(&self, s: &UiState) -> (Frame, i32, i32, Vec<Fx>) {
        let now = s.now_ms;
        let ms = &self.mascot;
        let t = now / 100; // tenths of a second
        let sw = SPRITE_W as i32;
        if ms.cheering(now) {
            let i = ((now - ms.cheer_at) / 100) as usize;
            let lift = CHEER_HOPS.get(i).copied().unwrap_or(0);
            let mut fx = Vec::new();
            for (k, (dx, row, ch)) in CHEER_SPARKS.iter().enumerate() {
                if (2..20).contains(&i) && !(i / 2 + k).is_multiple_of(3) {
                    fx.push(Fx(*dx, *row, ch.to_string(), if k % 2 == 1 { fg(COL_ACCENT) } else { fg(COL_WARN) }));
                }
            }
            let f = if i >= CHEER_HOPS.len() { recolor(FR_FRONT, VISOR_BRIGHT) } else { FR_CHEER };
            return (f, 0, lift, fx);
        }
        if ms.shaking(now) {
            let i = (now - ms.shake_at) / 100;
            let dx = if i < 6 { 1 - 2 * (i % 2) as i32 } else { 0 };
            return (head_shift(FR_FRONT, 0, 1), dx, 0, vec![Fx(sw, 1, "!".into(), bold(COL_ERR))]);
        }

        let state = s.state.as_str();
        match state {
            "OFFLINE" => {
                let row = 1 - (now / 1000 % 3) as i32;
                return (recolor(FR_SIT, VISOR_DARK), 0, 0, vec![Fx(sw, row, "z".into(), fg(COL_DIM))]);
            }
            "MIC_OFF" => return (recolor(FR_SIT, VISOR_DIM), 0, 0, Vec::new()),
            "LISTENING" => {
                let loud = s.level > 0.3;
                let f = if loud { recolor(FR_LISTEN, VISOR_BRIGHT) } else { FR_LISTEN };
                let arcs = [")", "))", ")))"][(t / 3 % 3) as usize];
                return (f, 0, 0, vec![Fx(sw + 1, 1, arcs.into(), fg(COL_ERR))]);
            }
            "TRANSCRIBING" | "THINKING" => {
                let dots = ["·", "··", "···"][(t / 4 % 3) as usize];
                let st = if state == "TRANSCRIBING" { fg(COL_WARN) } else { fg(COL_ACCENT) };
                return (head_shift(FR_THINK, 1, 0), 0, 0, vec![Fx(sw + 1, 0, dots.into(), st)]);
            }
            "EXECUTING" => {
                // Facing left, toward the log and the shell beyond it
                let f = mirror(fr_type((t / 2 % 2) as usize));
                let cur = if t / 3 % 2 == 0 { "> " } else { ">_" };
                return (f, 0, 0, vec![Fx(-3, 3, cur.into(), fg(COL_ACCENT))]);
            }
            "SPEAKING" => {
                let mut f = if t / 5 % 2 == 1 { fr_speak() } else { FR_FRONT };
                if t / 2 % 2 == 0 {
                    f = recolor(f, VISOR_BRIGHT);
                }
                let waves = [")", "))"][(t / 2 % 2) as usize];
                return (f, 0, 0, vec![Fx(sw, 1, waves.into(), fg(COL_OK))]);
            }
            "AGENTS" => {
                // Two little drones hover beside her
                let fx = (0..2).map(|k| Fx(sw + 1 + 2 * k, ((t / 3 + k as i64) % 3) as i32, if k == 1 { "•" } else { "◦" }.into(), fg(COL_ACCENT))).collect();
                return (FR_FRONT, 0, 0, fx);
            }
            "CONFIRM" => {
                let fx = if t / 4 % 2 == 0 { vec![Fx(sw + 1, 0, "?".into(), bold(COL_WARN))] } else { Vec::new() };
                return (fr_confirm(), 0, 0, fx);
            }
            _ => {}
        }

        // Idle
        if ms.walking {
            let k = [0, 2, 1, 2][(now / WALK_STEP_MS % 4) as usize];
            let f = if ms.dir < 0 { mirror(fr_walk(k)) } else { fr_walk(k) };
            // Legs together: the high point of the stride
            return (f, 0, if k == 0 { 1 } else { 0 }, Vec::new());
        }
        if now < ms.face_front {
            return (if ms.dir < 0 { mirror(FR_SIDE) } else { FR_SIDE }, 0, 0, Vec::new());
        }
        let mut f = FR_FRONT;
        if t % 24 < 3 {
            f = head_shift(f, 0, 1); // breathing
        }
        if t % 37 == 0 {
            f = recolor(f, VISOR_DIM); // the visor flickers
        }
        (f, 0, 0, Vec::new())
    }

    /// Where she is drawn inside the window's body, and the part of it that
    /// is left; the stage is empty when there is no room.
    pub(super) fn mascot_stage(&self, s: &UiState, mut inner: Rect) -> (Rect, Rect) {
        if !s.mascot || inner.w < MASCOT_COLS + 1 + MASCOT_MIN_LOG || inner.h < MASCOT_ROWS {
            return (Rect::default(), inner);
        }
        let stage = Rect::new(inner.x + inner.w - MASCOT_COLS, inner.y, MASCOT_COLS, inner.h);
        inner.w -= MASCOT_COLS + 1;
        (stage, inner)
    }

    /// Draws her on the stage: pixels first, then the marks around her, all
    /// clipped to the stage.
    pub(super) fn draw_mascot(&mut self, c: &mut Canvas, r: Rect, s: &UiState) {
        if r.w < SPRITE_W as i32 || r.h < MASCOT_ROWS {
            return;
        }
        self.mascot.ensure(s.now_ms, r.w);
        let (f, dx, lift, fx) = self.mascot_pose(s);
        let (pw, ph) = (r.w, r.h * 2);
        let lift = lift.min(ph - SPRITE_H as i32 + 2); // at the least room, a hop may hide her crest
        let mut px: Vec<Vec<Option<Rgb>>> = vec![vec![None; pw as usize]; ph as usize];
        let sx = (self.mascot.x + dx).clamp(0, pw - SPRITE_W as i32);
        let sy = ph - SPRITE_H as i32 - lift;
        for (y, row) in f.iter().enumerate() {
            let py = sy + y as i32;
            if py < 0 || py >= ph {
                continue;
            }
            for (x, &ch) in row.iter().enumerate() {
                if let Some(col) = palette(ch) {
                    px[py as usize][sx as usize + x] = Some(col);
                }
            }
        }
        c.pixels(r, &px);
        let top = r.y + sy.div_euclid(2);
        for Fx(fdx, row, text, st) in fx {
            let (x, y) = (r.x + sx + fdx, top + row);
            if y < r.y || y >= r.y + r.h || x < r.x || x + str_width(&text) > r.x + r.w {
                continue;
            }
            c.text(x, y, &text, st, r.x + r.w - x);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_frames() -> Vec<Frame> {
        vec![FR_FRONT, FR_SIDE, FR_LISTEN, FR_THINK, FR_CHEER, FR_SIT, fr_walk(1), fr_walk(2), fr_type(0), fr_type(1), fr_speak(), fr_confirm()]
    }

    #[test]
    fn frames_use_only_the_palette() {
        for f in all_frames() {
            for ch in f.iter().flatten() {
                assert!(*ch == b'.' || palette(*ch).is_some(), "unknown pixel {:?}", *ch as char);
            }
        }
        assert_eq!(mirror(mirror(FR_SIDE)), FR_SIDE);
        assert_ne!(mirror(FR_SIDE), FR_SIDE);
        assert!(recolor(FR_FRONT, VISOR_DARK).iter().flatten().all(|&c| c != b'O' && c != b'Y'));
        // A nod moves the helmet down a row and keeps the body
        let nod = head_shift(FR_FRONT, 0, 1);
        assert_eq!(nod[1], FR_FRONT[0]);
        assert_eq!(nod[8..], FR_FRONT[8..]);
    }

    #[test]
    fn walks_about_when_idle_and_stays_on_stage() {
        let mut m = Mascot::default();
        let mut now = 1_000_000;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..3000 {
            now += 100;
            m.step(now, "STANDBY", MASCOT_COLS);
            assert!((0..=MASCOT_COLS - SPRITE_W as i32).contains(&m.x));
            seen.insert(m.x);
        }
        assert!(seen.len() > 3, "she never moved: {seen:?}");
        // Busy, she stays put
        let x = m.x;
        for _ in 0..50 {
            now += 100;
            m.step(now, "THINKING", MASCOT_COLS);
        }
        assert_eq!(m.x, x);
        assert!(!m.walking);
    }

    #[test]
    fn cheers_when_work_is_done_but_not_after_an_error() {
        let mut m = Mascot::default();
        m.on_state("THINKING", "STANDBY", 10_000);
        assert!(m.cheering(10_100) && !m.cheering(10_000 + CHEER_FOR_MS));
        let mut m = Mascot::default();
        m.sad(10_000, true);
        m.on_state("EXECUTING", "STANDBY", 10_500);
        assert!(!m.cheering(10_600) && m.shaking(10_600));
        // Listening that ends is not a job done
        let mut m = Mascot::default();
        m.on_state("LISTENING", "STANDBY", 10_000);
        assert!(!m.cheering(10_100));
    }

    #[test]
    fn draws_into_the_stage() {
        let mut v = TuiView::default();
        let mut s = UiState::new(true, true);
        let inner = Rect::new(2, 1, 80, 8);
        let (stage, rest) = v.mascot_stage(&s, inner);
        assert_eq!((stage, rest.w), (Rect::new(62, 1, MASCOT_COLS, 8), 59));
        let mut c = Canvas::new(84, 10);
        v.draw_mascot(&mut c, stage, &s);
        let drawn = (1..9u16).flat_map(|y| (62..82u16).map(move |x| (x, y))).filter(|&p| c.buf[p].symbol() != " ").count();
        assert!(drawn > 20, "{drawn} cells");
        assert!((0..62u16).all(|x| (0..10u16).all(|y| c.buf[(x, y)].symbol() == " ")), "nothing outside the stage");
        // Too narrow a window has no stage
        assert!(v.mascot_stage(&s, Rect::new(0, 0, 50, 8)).0.empty());
        s.mascot = false;
        assert!(v.mascot_stage(&s, inner).0.empty());
    }
}
