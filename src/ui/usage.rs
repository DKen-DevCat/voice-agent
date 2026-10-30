//! Claude's usage in one line for the status line: cost, the plan's rate
//! limits, the context and the tokens, as many as fit.

use super::canvas::{COL_ERR, COL_FG, COL_WARN, St, bold, fg, str_width};
use crate::agent::usage::Usage;

/// The usage in at most max_w columns, and how to show it (hotter as a
/// limit comes near). now_ms tells which limits have started over.
pub fn usage_brief(u: &Usage, now_ms: i64, max_w: i32) -> (String, St) {
    let mut st = fg(COL_FG);
    if !u.known() || max_w <= 0 {
        return (String::new(), st);
    }
    let mut parts = Vec::new();
    if u.turns > 0 {
        parts.push(fmt_usd(u.cost_usd));
    }
    let now = now_ms / 1000;
    let mut hot: f64 = 0.0;
    for l in &u.limits {
        if l.used < 0.0 || (l.resets_at > 0 && now >= l.resets_at) {
            continue;
        }
        parts.push(format!("{} {}", window_label(&l.window), pct(l.used)));
        hot = hot.max(l.used);
    }
    if u.context > 0 && u.context_max > 0 {
        parts.push(format!("CTX {}", pct(u.context as f64 / u.context_max as f64)));
    }
    if u.turns > 0 {
        parts.push(format!("{} tok", fmt_count(u.tokens())));
    }
    let mut out = String::new();
    for p in parts {
        let next = if out.is_empty() { p } else { format!("{out} · {p}") };
        if str_width(&next) > max_w {
            break;
        }
        out = next;
    }
    if u.limit_status == "rejected" || hot >= 0.85 {
        st = bold(COL_ERR);
    } else if u.limit_status == "allowed_warning" || hot >= 0.6 || u.overage {
        st = fg(COL_WARN);
    }
    (out, st)
}

/// Names a rate limit window shortly: five_hour 5H, seven_day 7D,
/// seven_day_opus 7D OPUS.
fn window_label(name: &str) -> String {
    for (long, short) in [("five_hour", "5H"), ("seven_day", "7D")] {
        if let Some(rest) = name.strip_prefix(long) {
            let rest = rest.strip_prefix('_').unwrap_or(rest);
            return match rest {
                "" => short.to_string(),
                "overage_included" => format!("{short}+EX"),
                _ => format!("{short} {}", rest.replace('_', " ").to_uppercase()),
            };
        }
    }
    name.replace('_', " ").to_uppercase()
}

fn pct(f: f64) -> String {
    format!("{}%", (f * 100.0 + 0.5) as i64)
}

/// A cost in dollars: "$0.42", "<$0.01", "$123".
fn fmt_usd(v: f64) -> String {
    if v <= 0.0 {
        "$0.00".into()
    } else if v < 0.005 {
        "<$0.01".into()
    } else if v < 99.995 {
        format!("${v:.2}")
    } else {
        format!("${v:.0}")
    }
}

/// A token count in 4-6 columns: "987", "12.3k", "456k", "1.23M".
fn fmt_count(n: i64) -> String {
    let f = n as f64;
    if n < 1000 {
        n.to_string()
    } else if f < 99_950.0 {
        format!("{:.1}k", f / 1e3)
    } else if f < 999_500.0 {
        format!("{:.0}k", f / 1e3)
    } else if f < 9_995_000.0 {
        format!("{:.2}M", f / 1e6)
    } else if f < 99_950_000.0 {
        format!("{:.1}M", f / 1e6)
    } else if f < 999_500_000.0 {
        format!("{:.0}M", f / 1e6)
    } else {
        format!("{:.2}B", f / 1e9)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::usage::RateLimit;

    #[test]
    fn formats() {
        assert_eq!([fmt_usd(0.0), fmt_usd(0.001), fmt_usd(0.424), fmt_usd(123.4)], ["$0.00", "<$0.01", "$0.42", "$123"]);
        assert_eq!(
            [fmt_count(987), fmt_count(12_340), fmt_count(456_000), fmt_count(1_234_000), fmt_count(2_500_000_000)],
            ["987", "12.3k", "456k", "1.23M", "2.50B"]
        );
        assert_eq!(
            [window_label("five_hour"), window_label("seven_day_opus"), window_label("seven_day_overage_included"), window_label("monthly_cap")],
            ["5H", "7D OPUS", "7D+EX", "MONTHLY CAP"]
        );
        assert_eq!([pct(0.734), pct(0.0), pct(1.0)], ["73%", "0%", "100%"]);
    }

    #[test]
    fn brief_takes_what_fits() {
        let now_ms = 1_800_000_000_000;
        let now = now_ms / 1000;
        assert_eq!(usage_brief(&Usage::default(), now_ms, 60).0, "", "nothing before claude says anything");
        let u = Usage {
            turns: 3,
            input: 1200,
            output: 34_000,
            cache_read: 1_380_000,
            cache_write: 9500,
            cost_usd: 0.42,
            context: 230_000,
            context_max: 1_000_000,
            limits: vec![
                RateLimit { window: "five_hour".into(), used: 0.73, resets_at: now + 3600 },
                // Started over since claude said so: left out
                RateLimit { window: "seven_day".into(), used: 0.24, resets_at: now - 10 },
            ],
            limit_status: "allowed".into(),
            ..Usage::default()
        };
        let (brief, st) = usage_brief(&u, now_ms, 60);
        assert_eq!(brief, "$0.42 · 5H 73% · CTX 23% · 1.42M tok");
        assert_eq!(st.fg, Some(COL_WARN), "73% of a window is getting warm");
        assert_eq!(usage_brief(&u, now_ms, 16).0, "$0.42 · 5H 73%");
        assert_eq!(usage_brief(&u, now_ms, 0).0, "");
        let rejected = Usage { limit_status: "rejected".into(), ..u };
        assert_eq!(usage_brief(&rejected, now_ms, 60).1, bold(COL_ERR));
    }
}
