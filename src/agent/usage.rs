//! Adds up what the conversation with Claude has used, as far as claude's
//! stream-json tells.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::stream::{StreamMsg, TokenUsage};
use crate::util::unix_secs;

/// What the conversation with Claude has used since voice-agent started.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Usage {
    pub turns: u32,       // results seen (replies finished, including interrupted ones)
    pub input: i64,       // input tokens that were not cached
    pub output: i64,      // output tokens (thinking included)
    pub cache_read: i64,  // input tokens read from the prompt cache
    pub cache_write: i64, // input tokens written to the prompt cache
    pub cost_usd: f64,    // Claude Code's estimate at API list prices (subagents included)
    pub context: i64,     // tokens in the main conversation's latest prompt
    pub context_max: i64, // its model's context window (0: not known yet)

    pub limits: Vec<RateLimit>, // the plan's usage windows (claude.ai login only)
    pub limit_status: String,   // allowed / allowed_warning / rejected ("" until reported)
    pub overage: bool,          // running on extra usage beyond the plan
    pub limits_at: i64,         // when claude last reported them (unix seconds)
}

/// One usage window of the plan: how much of it is used and when it starts over.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct RateLimit {
    pub window: String, // five_hour, seven_day ...
    pub used: f64,      // 0..1; negative when not reported
    pub resets_at: i64, // unix seconds; 0 when not reported
}

impl Usage {
    /// Whether claude has said anything about usage yet.
    pub fn known(&self) -> bool {
        self.turns > 0 || !self.limits.is_empty() || self.context > 0
    }

    /// All tokens counted, cached ones included.
    pub fn tokens(&self) -> i64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

/// One entry of a result's modelUsage: the session's running totals for one model.
#[derive(Deserialize, Default, Clone, Copy)]
#[serde(default, rename_all = "camelCase")]
struct ModelUsage {
    input_tokens: i64,
    output_tokens: i64,
    cache_read_input_tokens: i64,
    cache_creation_input_tokens: i64,
    #[serde(rename = "costUSD")]
    cost_usd: f64,
    context_window: i64,
}

impl ModelUsage {
    fn sum(&self) -> i64 {
        self.input_tokens + self.output_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct LimitWindow {
    utilization: Option<f64>,
    resets_at: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RateLimitInfo {
    status: Option<String>,
    resets_at: Option<i64>,
    rate_limit_type: Option<String>,
    utilization: Option<f64>,
    is_using_overage: Option<bool>,
    unified_windows: Option<HashMap<String, LimitWindow>>,
}

/// The token and cost part of Usage.
#[derive(Clone, Copy, Default, PartialEq)]
struct Totals {
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    cost: f64,
}

impl Totals {
    fn plus(self, b: Totals) -> Totals {
        Totals {
            input: self.input + b.input,
            output: self.output + b.output,
            cache_read: self.cache_read + b.cache_read,
            cache_write: self.cache_write + b.cache_write,
            cost: self.cost + b.cost,
        }
    }
}

/// The usage part of a result message.
#[derive(Deserialize, Default)]
#[serde(default)]
struct ResultUsage {
    total_cost_usd: Option<f64>,
    usage: Option<TokenUsage>, // this turn, main conversation only
    #[serde(rename = "modelUsage")]
    model_usage: Option<HashMap<String, ModelUsage>>,
}

#[derive(Default)]
struct State {
    base: Totals,   // earlier processes whose totals were not carried over
    prev: Totals,   // the previous process's last totals, until its successor's first result
    prev_main: i64, // ...its main model's tokens
    cur: Totals,    // the current process's totals
    cur_main: i64,
    results: u32,  // results from the current process
    model: String, // the main conversation's model, as its messages name it
    windows: HashMap<String, i64>,
    u: Usage,
}

/// Adds up Usage from the claude stream.
///
/// A result carries the session's running totals (total_cost_usd and
/// modelUsage, subagents included), not the turn's. They live in the claude
/// process: when claude is restarted with --resume, Claude Code carries the
/// session's totals over only if it saved them for that session. So the
/// first result of a restarted process tells whether they were carried over:
/// its main model's total then exceeds what the turn itself used by about
/// what the previous process had reported.
pub struct UsageTracker {
    st: Mutex<State>,
    on_change: Option<Box<dyn Fn(Usage) + Send + Sync>>,
}

impl UsageTracker {
    pub fn new(on_change: Option<Box<dyn Fn(Usage) + Send + Sync>>) -> UsageTracker {
        UsageTracker { st: Mutex::new(State::default()), on_change }
    }

    /// Called when claude (re)starts.
    pub fn new_process(&self) {
        let mut t = self.st.lock().unwrap_or_else(|e| e.into_inner());
        if t.results == 0 {
            // Nothing reported since the last start: what is pending stays pending
            return;
        }
        t.prev = t.cur;
        t.prev_main = t.cur_main;
        t.cur = Totals::default();
        t.cur_main = 0;
        t.results = 0;
    }

    #[cfg(test)]
    pub fn snapshot(&self) -> Usage {
        usage(&self.st.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Reads the usage parts of one stream message: raw is the line (results
    /// and rate limits are parsed from it here so that an unexpected shape
    /// cannot spoil the rest of the message). Subagents' messages do not
    /// change the context size.
    pub fn observe(&self, m: &StreamMsg, raw: &[u8]) {
        let changed = {
            let mut t = self.st.lock().unwrap_or_else(|e| e.into_inner());
            match m.typ.as_str() {
                "result" => {
                    let res: ResultUsage = serde_json::from_slice(raw).unwrap_or_default();
                    result(&mut t, &res);
                }
                "rate_limit_event" => {
                    #[derive(Deserialize)]
                    struct Wrap {
                        rate_limit_info: Option<RateLimitInfo>,
                    }
                    let Some(info) = serde_json::from_slice::<Wrap>(raw).ok().and_then(|w| w.rate_limit_info) else { return };
                    limits(&mut t, &info);
                }
                "assistant" => {
                    let Some(msg) = m.message.as_ref().filter(|_| m.parent_tool_use_id.is_empty()) else { return };
                    let Some(mu) = msg.usage else { return };
                    let ctx = mu.input_tokens + mu.cache_read_input_tokens + mu.cache_creation_input_tokens;
                    if ctx <= 0 || (ctx == t.u.context && msg.model == t.model) {
                        return;
                    }
                    t.u.context = ctx;
                    if !msg.model.is_empty() {
                        t.model = msg.model.clone();
                    }
                }
                _ => return,
            }
            usage(&t)
        };
        if let Some(f) = &self.on_change {
            f(changed);
        }
    }
}

/// The current Usage.
fn usage(t: &State) -> Usage {
    let mut u = t.u.clone();
    let tot = if t.results == 0 { t.base.plus(t.prev) } else { t.base.plus(t.cur) };
    (u.input, u.output, u.cache_read, u.cache_write, u.cost_usd) = (tot.input, tot.output, tot.cache_read, tot.cache_write, tot.cost);
    if let Some(&w) = t.windows.get(&t.model).filter(|w| **w > 0) {
        u.context_max = w;
    }
    u
}

/// Takes in a turn's result.
fn result(t: &mut State, m: &ResultUsage) {
    t.u.turns += 1;
    let turn = m.usage.unwrap_or_default();
    let mut cum = Totals::default();
    let mut main = -1;
    match m.model_usage.as_ref().filter(|mu| !mu.is_empty()) {
        Some(models) => {
            for (name, mu) in models {
                cum.input += mu.input_tokens;
                cum.output += mu.output_tokens;
                cum.cache_read += mu.cache_read_input_tokens;
                cum.cache_write += mu.cache_creation_input_tokens;
                cum.cost += mu.cost_usd;
                if mu.context_window > 0 {
                    t.windows.insert(name.clone(), mu.context_window);
                }
                if *name == t.model {
                    main = mu.sum();
                }
            }
        }
        None => {
            // No per-model totals: count the turn's own tokens (the main conversation's)
            cum = t.cur;
            cum.input += turn.input_tokens;
            cum.output += turn.output_tokens;
            cum.cache_read += turn.cache_read_input_tokens;
            cum.cache_write += turn.cache_creation_input_tokens;
        }
    }
    if let Some(cost) = m.total_cost_usd {
        cum.cost = cost;
    }

    if t.results == 0 && t.prev != Totals::default() {
        // The first result after a restart: were the earlier totals carried over?
        let carried = if main >= 0 && t.prev_main > 0 { main - turn.sum() >= t.prev_main / 2 } else { cum.cost >= t.prev.cost / 2.0 && cum.cost > 0.0 };
        if !carried {
            t.base = t.base.plus(t.prev);
        }
        t.prev = Totals::default();
        t.prev_main = 0;
    }
    t.results += 1;
    t.cur = cum;
    if main >= 0 {
        t.cur_main = main;
    } else {
        t.cur_main += turn.sum();
    }
}

/// Takes in a rate_limit_event.
fn limits(t: &mut State, info: &RateLimitInfo) {
    let at = |sec: Option<i64>| sec.filter(|s| *s > 0).unwrap_or(0);
    let used = |p: Option<f64>| p.unwrap_or(-1.0);
    let mut ls: Vec<RateLimit> = info
        .unified_windows
        .iter()
        .flatten()
        .map(|(name, w)| RateLimit { window: name.clone(), used: used(w.utilization), resets_at: at(w.resets_at) })
        .collect();
    let kind = info.rate_limit_type.clone().unwrap_or_default();
    if ls.is_empty() && (!kind.is_empty() || at(info.resets_at) > 0) {
        let window = if kind.is_empty() { "limit".to_string() } else { kind };
        ls.push(RateLimit { window, used: used(info.utilization), resets_at: at(info.resets_at) });
    }
    ls.sort_by(|a, b| (window_order(&a.window), &a.window).cmp(&(window_order(&b.window), &b.window)));
    t.u.limits = ls;
    t.u.limit_status = info.status.clone().unwrap_or_default();
    t.u.overage = info.is_using_overage.unwrap_or(false);
    t.u.limits_at = unix_secs();
}

fn window_order(name: &str) -> u8 {
    match name {
        "five_hour" => 0,
        "seven_day" => 1,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // Shapes as claude 2.1 emits them in stream-json (captured, trimmed).
    const ASSISTANT: &str = r#"{"type":"assistant","message":{"model":"claude-sonnet-5-5","id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":10,"cache_creation_input_tokens":9525,"cache_read_input_tokens":13796,"output_tokens":4,"service_tier":"standard"}},"parent_tool_use_id":null,"session_id":"s1"}"#;
    const SUBAGENT: &str = r#"{"type":"assistant","message":{"model":"claude-opus-5-5","content":[{"type":"text","text":"working"}],"usage":{"input_tokens":3,"cache_creation_input_tokens":50000,"cache_read_input_tokens":0,"output_tokens":4}},"parent_tool_use_id":"toolu_A1","session_id":"s1"}"#;
    const RATE_LIMIT: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1791036600,"rateLimitType":"five_hour","isUsingOverage":false,"unifiedWindows":{"seven_day":{"utilization":0.24,"resetsAt":1791165600},"five_hour":{"utilization":0.73,"resetsAt":1791036600}}},"uuid":"u1","session_id":"s1"}"#;

    /// A result whose running totals are the main model's (in, out, read,
    /// write) and a subagent's opus tokens, with the turn's own usage.
    fn result_line(cost: f64, main: [i64; 4], opus: [i64; 4], turn: [i64; 4]) -> String {
        let mu = |t: [i64; 4], c: f64| {
            format!(
                r#"{{"inputTokens":{},"outputTokens":{},"cacheReadInputTokens":{},"cacheCreationInputTokens":{},"costUSD":{c},"contextWindow":1000000}}"#,
                t[0], t[1], t[2], t[3]
            )
        };
        let mut models = format!(r#""claude-sonnet-5-5":{}"#, mu(main, cost));
        if opus != [0; 4] {
            models += &format!(r#","claude-opus-5-5":{}"#, mu(opus, 0.0));
        }
        format!(
            r#"{{"type":"result","subtype":"success","is_error":false,"result":"ok","session_id":"s1","total_cost_usd":{cost},"usage":{{"input_tokens":{},"output_tokens":{},"cache_read_input_tokens":{},"cache_creation_input_tokens":{}}},"modelUsage":{{{models}}}}}"#,
            turn[0], turn[1], turn[2], turn[3]
        )
    }

    fn feed(tr: &UsageTracker, line: &str) {
        let m: StreamMsg = serde_json::from_str(line).unwrap_or_else(|e| panic!("bad test line: {e}: {line}"));
        tr.observe(&m, line.as_bytes());
    }

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn tracker() {
        let events = Arc::new(Mutex::new(Vec::<Usage>::new()));
        let ev = events.clone();
        let tr = UsageTracker::new(Some(Box::new(move |u| ev.lock().unwrap().push(u))));

        tr.new_process();
        assert!(!tr.snapshot().known(), "nothing is known before claude says anything");

        feed(&tr, ASSISTANT);
        feed(&tr, SUBAGENT);
        let u = tr.snapshot();
        assert_eq!((u.context, u.context_max, u.turns), (10 + 9525 + 13796, 0, 0));

        feed(&tr, RATE_LIMIT);
        let u = tr.snapshot();
        assert_eq!(u.limits.iter().map(|l| l.window.as_str()).collect::<Vec<_>>(), ["five_hour", "seven_day"]);
        assert!(near(u.limits[0].used, 0.73) && u.limits[0].resets_at == 1791036600);

        feed(&tr, &result_line(0.05, [10, 136, 13796, 9525], [2, 40, 0, 4298], [10, 136, 13796, 9525]));
        let u = tr.snapshot();
        assert_eq!((u.turns, u.input, u.output, u.cache_read, u.cache_write), (1, 12, 176, 13796, 9525 + 4298));
        assert!(near(u.cost_usd, 0.05));
        assert_eq!(u.context_max, 1000000);
        feed(&tr, &result_line(0.08, [20, 200, 27592, 9600], [2, 40, 0, 4298], [10, 64, 13796, 75]));
        let u = tr.snapshot();
        assert_eq!((u.turns, u.input, u.output), (2, 22, 240));

        // claude restarts and its totals start over: what came before is kept
        tr.new_process();
        feed(&tr, ASSISTANT);
        feed(&tr, &result_line(0.01, [10, 30, 13796, 100], [0; 4], [10, 30, 13796, 100]));
        let u = tr.snapshot();
        assert_eq!((u.turns, u.input, u.output), (3, 32, 270), "restart without carry-over");
        assert!(near(u.cost_usd, 0.09));

        // Restarted again, and this time the totals were carried over
        tr.new_process();
        feed(&tr, ASSISTANT);
        feed(&tr, &result_line(0.03, [20, 60, 27592, 200], [0; 4], [10, 30, 13796, 100]));
        let u = tr.snapshot();
        assert_eq!((u.turns, u.input, u.output), (4, 42, 300), "restart with carry-over");
        assert!(near(u.cost_usd, 0.11));
        assert_eq!(events.lock().unwrap().last().unwrap().turns, 4);
        assert_eq!(u.tokens(), u.input + u.output + u.cache_read + u.cache_write);
    }

    #[test]
    fn rate_limit_shapes() {
        let tr = UsageTracker::new(None);
        feed(
            &tr,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1791036600,"rateLimitType":"seven_day_opus","utilization":0.91,"isUsingOverage":true}}"#,
        );
        let u = tr.snapshot();
        assert_eq!((u.limits[0].window.as_str(), u.limits[0].used, u.limit_status.as_str(), u.overage), ("seven_day_opus", 0.91, "allowed_warning", true));
        feed(
            &tr,
            r#"{"type":"result","subtype":"success","total_cost_usd":0.5,"usage":{"input_tokens":5,"output_tokens":7,"cache_read_input_tokens":100,"cache_creation_input_tokens":0}}"#,
        );
        let u = tr.snapshot();
        assert_eq!((u.turns, u.input, u.output, u.cost_usd), (1, 5, 7, 0.5));
    }
}
