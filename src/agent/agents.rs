//! Follows the subagents the conversation spawns, from what the claude
//! stream shows.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::stream::{ContentBlock, StreamMsg, block_text, input_str};
use crate::util::{clip_chars, unix_millis};

// States of a subagent (one Agent tool call)
pub const AGENT_RUNNING: &str = "RUNNING";
pub const AGENT_DONE: &str = "DONE";
pub const AGENT_FAILED: &str = "FAILED";
pub const AGENT_STOPPED: &str = "STOPPED";

const MAX_AGENT_STEPS: usize = 8; // tool calls remembered per agent
const MAX_AGENT_TEXT: usize = 1500; // characters kept of what an agent wrote
const MAX_AGENTS: usize = 40; // agents remembered; finished ones are dropped first

/// One tool call a subagent made.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct AgentStep {
    #[serde(skip)]
    pub id: String,
    pub tag: String, // exec / read / edit ... (see describe_tool)
    pub detail: String,
    pub at: i64, // unix milliseconds
    pub done: bool,
    pub err: bool,
}

/// What one subagent is doing, as far as the stream tells.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct AgentInfo {
    pub id: String, // the Agent tool_use id; the subagent's own messages carry it as parent_tool_use_id
    pub task_id: String,
    pub typ: String,         // opus-worker, fable-worker ...
    pub description: String, // the one-line goal the front end gave it
    pub prompt: String,      // the full task
    pub state: String,
    pub background: bool,      // launched asynchronously: the front end's turn ended before it finished
    pub started: i64,          // unix milliseconds
    pub ended: i64,            // 0 while it runs
    pub activity: String,      // Claude Code's own one-line summary of the latest step
    pub steps: Vec<AgentStep>, // latest tool calls, oldest first
    pub tool_calls: u32,
    pub tokens: i64,
    pub note: String, // the last thing the agent wrote
    pub todo_done: u32,
    pub todo_total: u32,
    pub todo_now: String, // the todo it is on
    pub result: String,   // its report, once finished
}

impl AgentInfo {
    pub fn running(&self) -> bool {
        self.state == AGENT_RUNNING
    }

    /// How long the agent has been (or was) at work, in seconds.
    pub fn elapsed(&self, now_ms: i64) -> f64 {
        let end = if self.running() || self.ended == 0 { now_ms } else { self.ended };
        (end - self.started) as f64 / 1000.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentChange {
    Updated,
    Finished, // reached DONE, FAILED or STOPPED
    Revived,  // was shown as STOPPED but is still at work
}

/// The subagent part of a system task_started / task_progress /
/// task_notification / task_updated message.
#[derive(Deserialize, Default)]
#[serde(default)]
struct TaskMsg {
    task_id: Option<String>,
    tool_use_id: Option<String>,
    description: Option<String>,
    subagent_type: Option<String>,
    prompt: Option<String>,
    status: Option<String>,
    summary: Option<String>,
    is_backgrounded: Option<bool>,
    usage: Option<TaskUsage>,
    patch: Option<TaskPatch>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TaskUsage {
    total_tokens: i64,
    tool_uses: u32,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TaskPatch {
    status: Option<String>,
}

type Describe = Box<dyn Fn(&str, &Map<String, Value>) -> (String, String) + Send + Sync>;
type OnChange = Box<dyn Fn(AgentInfo, AgentChange) + Send + Sync>;

#[derive(Default)]
struct State {
    agents: HashMap<String, AgentInfo>, // by Agent tool_use id
    by_task: HashMap<String, String>,   // task_id -> tool_use id
}

/// Follows the subagents from what the claude stream shows: the Agent tool
/// call and the system task_* messages give the goal, messages carrying
/// parent_tool_use_id show each tool the agent uses and what it writes, and
/// task_notification (or the Agent tool's own result) ends it.
pub struct AgentTracker {
    st: Mutex<State>,
    describe: Option<Describe>,
    on_change: Option<OnChange>,
}

impl AgentTracker {
    pub fn new(describe: Option<Describe>, on_change: Option<OnChange>) -> AgentTracker {
        AgentTracker { st: Mutex::new(State::default()), describe, on_change }
    }

    fn notify(&self, out: Vec<(AgentInfo, AgentChange)>) {
        if let Some(f) = &self.on_change {
            for (a, ch) in out {
                f(a, ch);
            }
        }
    }

    /// Reads the subagent-related parts of one stream message: raw is the
    /// line, blocks its content (assistant and user messages).
    pub fn observe(&self, m: &StreamMsg, raw: &[u8], blocks: &[ContentBlock]) {
        let parent = m.parent_tool_use_id.as_str();
        let mut out = Vec::new();
        {
            let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
            match m.typ.as_str() {
                "assistant" => {
                    for b in blocks {
                        if b.typ == "tool_use" && parent.is_empty() && b.name == "Agent" {
                            let a = st.start(&b.id, &input_str(&b.input, "subagent_type"), &input_str(&b.input, "description"), &input_str(&b.input, "prompt"));
                            out.push((a.clone(), AgentChange::Updated));
                        } else if b.typ == "tool_use" && !parent.is_empty() {
                            let (a, revived) = st.child_tool(parent, b, self.describe.as_ref());
                            out.push((a, if revived { AgentChange::Revived } else { AgentChange::Updated }));
                        } else if b.typ == "text" && !parent.is_empty() && !b.text.trim().is_empty() {
                            let a = st.get(parent);
                            a.note = clip_chars(b.text.trim(), MAX_AGENT_TEXT);
                            if !a.running() && a.result.is_empty() {
                                // Its report arrived after the end was announced
                                a.result = a.note.clone();
                            }
                            out.push((a.clone(), AgentChange::Updated));
                        }
                    }
                }
                "user" => {
                    for b in blocks.iter().filter(|b| b.typ == "tool_result") {
                        if !parent.is_empty() {
                            let a = st.get(parent);
                            for s in a.steps.iter_mut().filter(|s| s.id == b.tool_use_id) {
                                s.done = true;
                                s.err = b.is_error;
                            }
                            out.push((a.clone(), AgentChange::Updated));
                            continue;
                        }
                        let Some(a) = st.agents.get_mut(&b.tool_use_id) else { continue };
                        let meta = &m.tool_use_result;
                        if meta.get("isAsync").and_then(Value::as_bool).unwrap_or(false) || meta.get("status").and_then(Value::as_str) == Some("async_launched")
                        {
                            // The agent keeps working after the front end's turn; task_notification ends it
                            a.background = true;
                            out.push((a.clone(), AgentChange::Updated));
                            continue;
                        }
                        let state = if b.is_error { AGENT_FAILED } else { AGENT_DONE };
                        if let Some(ch) = finish(a, state, &block_text(&b.content)) {
                            out.push((a.clone(), ch));
                        }
                    }
                }
                "system" if m.subtype.starts_with("task_") => {
                    if let Ok(tm) = serde_json::from_slice::<TaskMsg>(raw)
                        && let Some(change) = st.task(&m.subtype, &tm)
                    {
                        out.push(change);
                    }
                }
                _ => {}
            }
        }
        self.notify(out);
    }

    /// Marks every running agent as stopped (the user aborted, or the claude
    /// process is gone). One that is in fact still working shows up as
    /// running again with its next step.
    pub fn stop_all(&self) {
        let mut out = Vec::new();
        {
            let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
            for a in st.agents.values_mut().filter(|a| a.running()) {
                a.state = AGENT_STOPPED.to_string();
                a.ended = unix_millis();
                if a.result.is_empty() {
                    a.result = a.note.clone();
                }
                out.push((a.clone(), AgentChange::Finished));
            }
        }
        self.notify(out);
    }

    pub fn running(&self) -> usize {
        self.st.lock().unwrap_or_else(|e| e.into_inner()).agents.values().filter(|a| a.running()).count()
    }

    /// Every agent, running ones first (oldest first), then the finished
    /// ones, newest first.
    #[cfg(test)]
    pub fn snapshot(&self) -> Vec<AgentInfo> {
        let mut out: Vec<AgentInfo> = self.st.lock().unwrap_or_else(|e| e.into_inner()).agents.values().cloned().collect();
        out.sort_by(|x, y| {
            y.running()
                .cmp(&x.running())
                .then_with(|| if x.running() { x.started.cmp(&y.started) } else { y.ended.cmp(&x.ended) })
                .then_with(|| x.id.cmp(&y.id))
        });
        out
    }
}

impl State {
    /// The agent with this tool_use id, made if unknown.
    fn get(&mut self, id: &str) -> &mut AgentInfo {
        if !self.agents.contains_key(id) {
            self.agents
                .insert(id.to_string(), AgentInfo { id: id.to_string(), state: AGENT_RUNNING.to_string(), started: unix_millis(), ..AgentInfo::default() });
            self.prune(id);
        }
        self.agents.get_mut(id).unwrap()
    }

    fn start(&mut self, id: &str, typ: &str, desc: &str, prompt: &str) -> &mut AgentInfo {
        let a = self.get(id);
        if !typ.is_empty() {
            a.typ = typ.to_string();
        }
        if !desc.is_empty() {
            a.description = desc.to_string();
        }
        if !prompt.is_empty() {
            a.prompt = prompt.to_string();
        }
        a
    }

    fn lookup(&mut self, tool_use_id: &str, task_id: &str) -> Option<&mut AgentInfo> {
        let id = if self.agents.contains_key(tool_use_id) { tool_use_id.to_string() } else { self.by_task.get(task_id)?.clone() };
        self.agents.get_mut(&id)
    }

    fn child_tool(&mut self, parent: &str, b: &ContentBlock, describe: Option<&Describe>) -> (AgentInfo, bool) {
        let a = self.get(parent);
        let revived = revive(a);
        let (tag, detail) = match describe {
            Some(d) => d(&b.name, &b.input),
            None => ("tool".to_string(), b.name.clone()),
        };
        if tag.is_empty() {
            return (a.clone(), revived);
        }
        a.tool_calls += 1;
        a.steps.push(AgentStep { id: b.id.clone(), tag, detail, at: unix_millis(), done: false, err: false });
        if a.steps.len() > MAX_AGENT_STEPS {
            a.steps.drain(..a.steps.len() - MAX_AGENT_STEPS);
        }
        if b.name == "TodoWrite" {
            let todos = b.input.get("todos").and_then(Value::as_array).cloned().unwrap_or_default();
            a.todo_total = todos.len() as u32;
            a.todo_done = 0;
            a.todo_now = String::new();
            for td in &todos {
                let get = |k: &str| td.get(k).and_then(Value::as_str).unwrap_or_default();
                match get("status") {
                    "completed" => a.todo_done += 1,
                    "in_progress" if a.todo_now.is_empty() => {
                        a.todo_now = if get("activeForm").is_empty() { get("content").to_string() } else { get("activeForm").to_string() };
                    }
                    _ => {}
                }
            }
        }
        (a.clone(), revived)
    }

    /// Applies a system task_* message.
    fn task(&mut self, subtype: &str, m: &TaskMsg) -> Option<(AgentInfo, AgentChange)> {
        let s = |v: &Option<String>| v.clone().unwrap_or_default();
        let usage = |a: &mut AgentInfo| {
            if let Some(u) = &m.usage {
                if u.total_tokens > 0 {
                    a.tokens = u.total_tokens;
                }
                a.tool_calls = a.tool_calls.max(u.tool_uses);
            }
        };
        match subtype {
            "task_started" => {
                let id = s(&m.tool_use_id);
                if id.is_empty() {
                    return None;
                }
                let task_id = s(&m.task_id);
                let a = self.start(&id, &s(&m.subagent_type), &s(&m.description), &s(&m.prompt));
                if !task_id.is_empty() {
                    a.task_id = task_id.clone();
                }
                a.background = a.background || m.is_backgrounded.unwrap_or(false);
                let a = a.clone();
                if !task_id.is_empty() {
                    self.by_task.insert(task_id, id);
                }
                Some((a, AgentChange::Updated))
            }
            "task_progress" => {
                let a = self.lookup(&s(&m.tool_use_id), &s(&m.task_id))?;
                let ch = if revive(a) { AgentChange::Revived } else { AgentChange::Updated };
                if let Some(d) = m.description.as_ref().filter(|d| !d.is_empty()) {
                    a.activity = d.clone();
                }
                usage(a);
                Some((a.clone(), ch))
            }
            "task_notification" => {
                let a = self.lookup(&s(&m.tool_use_id), &s(&m.task_id))?;
                usage(a);
                let state = terminal_state(&s(&m.status)).unwrap_or(AGENT_DONE);
                let summary = s(&m.summary);
                let result = if summary.len() > a.note.len() { summary } else { a.note.clone() };
                let ch = finish(a, state, &result)?;
                Some((a.clone(), ch))
            }
            "task_updated" => {
                let a = self.lookup("", &s(&m.task_id))?;
                let state = terminal_state(&s(&m.patch.as_ref()?.status))?;
                let note = a.note.clone();
                let ch = finish(a, state, &note)?;
                Some((a.clone(), ch))
            }
            _ => None,
        }
    }

    /// Forgets the oldest finished agents beyond MAX_AGENTS (never keep).
    fn prune(&mut self, keep: &str) {
        while self.agents.len() > MAX_AGENTS {
            let Some(oldest) = self.agents.values().filter(|a| !a.running() && a.id != keep).min_by_key(|a| a.ended).map(|a| (a.id.clone(), a.task_id.clone()))
            else {
                return;
            };
            self.agents.remove(&oldest.0);
            self.by_task.remove(&oldest.1);
        }
    }
}

/// Puts an agent that was shown as stopped back to work.
fn revive(a: &mut AgentInfo) -> bool {
    if a.state != AGENT_STOPPED {
        return false;
    }
    a.state = AGENT_RUNNING.to_string();
    a.ended = 0;
    true
}

/// Ends an agent; a result arriving later only fills in the report.
fn finish(a: &mut AgentInfo, state: &str, result: &str) -> Option<AgentChange> {
    if !a.running() {
        if !result.is_empty() && a.result.is_empty() {
            a.result = result.to_string();
            return Some(AgentChange::Updated);
        }
        return None;
    }
    a.state = state.to_string();
    a.ended = unix_millis();
    a.result = if result.is_empty() { a.note.clone() } else { result.to_string() };
    Some(AgentChange::Finished)
}

fn terminal_state(status: &str) -> Option<&'static str> {
    match status.to_lowercase().as_str() {
        "completed" | "success" | "done" => Some(AGENT_DONE),
        "failed" | "error" | "errored" => Some(AGENT_FAILED),
        "stopped" | "killed" | "cancelled" | "canceled" | "aborted" | "interrupted" => Some(AGENT_STOPPED),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    /// Replays the stream shapes claude 2.1 emits for two background
    /// subagents (as captured with --output-format stream-json).
    #[test]
    fn tracker() {
        let changes = Arc::new(Mutex::new(Vec::<(AgentInfo, AgentChange)>::new()));
        let ch = changes.clone();
        let describe: Describe = Box::new(|name, input| match name {
            "Read" => ("read".into(), input_str(input, "file_path").trim_start_matches("/work/").to_string()),
            "TodoWrite" => ("todo".into(), "items".into()),
            _ => ("exec".into(), input_str(input, "command")),
        });
        let tr = AgentTracker::new(Some(describe), Some(Box::new(move |a, c| ch.lock().unwrap().push((a, c)))));
        let feed = |v: Value| {
            let raw = serde_json::to_vec(&v).unwrap();
            let m: StreamMsg = serde_json::from_slice(&raw).unwrap();
            tr.observe(&m, &raw, &m.blocks());
        };
        let msg = |typ: &str, parent: &str, blocks: Value| {
            let mut m = json!({"type": typ, "message": {"content": blocks}});
            if !parent.is_empty() {
                m["parent_tool_use_id"] = json!(parent);
            }
            m
        };
        let tool_use = |id: &str, name: &str, input: Value| json!({"type": "tool_use", "id": id, "name": name, "input": input});
        let tool_result = |id: &str, content: Value| json!({"type": "tool_result", "tool_use_id": id, "content": content});
        let find = |id: &str| tr.snapshot().into_iter().find(|a| a.id == id).unwrap_or_else(|| panic!("agent {id} not tracked"));

        // The front end hands out two tasks; both go to the background and its turn ends
        let prompt = "Fix the failing test and run cargo test";
        for (id, desc) in [("A1", "first probe"), ("A2", "second probe")] {
            feed(msg("assistant", "", json!([tool_use(id, "Agent", json!({"subagent_type": "opus-worker", "description": desc, "prompt": prompt}))])));
            feed(
                json!({"type": "system", "subtype": "task_started", "task_id": format!("t{id}"), "tool_use_id": id, "description": desc, "subagent_type": "opus-worker", "is_backgrounded": true, "prompt": prompt}),
            );
            let mut launched = msg("user", "", json!([tool_result(id, json!("Async agent launched successfully."))]));
            launched["tool_use_result"] = json!({"isAsync": true, "status": "async_launched", "agentId": format!("t{id}")});
            feed(launched);
        }
        feed(json!({"type": "result", "subtype": "success", "result": "Both are launched."}));
        assert_eq!(tr.running(), 2);
        let a = find("A1");
        assert!(a.background && a.typ == "opus-worker" && a.description == "first probe" && a.task_id == "tA1", "{a:?}");

        // A1 reads a file: in flight, then done, with Claude Code's own summary
        feed(msg("assistant", "A1", json!([tool_use("R1", "Read", json!({"file_path": "/work/src/app.rs"}))])));
        let a = find("A1");
        assert_eq!(a.tool_calls, 1);
        assert_eq!(a.steps.last().map(|s| (s.tag.as_str(), s.detail.as_str(), s.done)), Some(("read", "src/app.rs", false)));
        feed(
            json!({"type": "system", "subtype": "task_progress", "task_id": "tA1", "tool_use_id": "A1", "description": "Reading app.rs", "usage": {"total_tokens": 2944, "tool_uses": 1}}),
        );
        feed(msg("user", "A1", json!([tool_result("R1", json!("1\tmod x"))])));
        let a = find("A1");
        assert!(a.activity == "Reading app.rs" && a.tokens == 2944 && a.steps[0].done, "{a:?}");
        // Its todo list
        feed(msg(
            "assistant",
            "A1",
            json!([tool_use(
                "T1",
                "TodoWrite",
                json!({"todos": [
                    {"content": "read code", "status": "completed"},
                    {"content": "fix bug", "status": "in_progress", "activeForm": "Fixing the bug"},
                    {"content": "run tests", "status": "pending"},
                ]})
            )]),
        ));
        let a = find("A1");
        assert_eq!((a.todo_total, a.todo_done, a.todo_now.as_str()), (3, 1, "Fixing the bug"));

        // A1 reports and finishes
        feed(msg("assistant", "A1", json!([{"type": "text", "text": "Fixed it; tests pass."}])));
        feed(json!({"type": "system", "subtype": "task_updated", "task_id": "tA1", "patch": {"status": "completed", "end_time": 1}}));
        feed(
            json!({"type": "system", "subtype": "task_notification", "task_id": "tA1", "tool_use_id": "A1", "status": "completed", "summary": "Fixed it; tests pass.", "usage": {"total_tokens": 4229, "tool_uses": 2}}),
        );
        let snap = tr.snapshot();
        assert_eq!(snap.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["A2", "A1"], "running agents should come first");
        let a1 = &snap[1];
        assert!(a1.state == AGENT_DONE && a1.result == "Fixed it; tests pass." && a1.tool_calls == 2 && a1.tokens == 4229 && a1.ended > 0, "{a1:?}");
        let finished: Vec<String> = changes.lock().unwrap().iter().filter(|(_, c)| *c == AgentChange::Finished).map(|(a, _)| a.id.clone()).collect();
        assert_eq!(finished, ["A1"], "task_updated and task_notification both end A1, once");

        // Aborting marks A2 stopped; its next step shows it is still alive
        tr.stop_all();
        assert_eq!(find("A2").state, AGENT_STOPPED);
        feed(json!({"type": "system", "subtype": "task_progress", "task_id": "tA2", "tool_use_id": "A2", "description": "Running tests"}));
        assert_eq!(find("A2").state, AGENT_RUNNING);
        assert_eq!(changes.lock().unwrap().last().unwrap().1, AgentChange::Revived);
        // A synchronous Agent call ends with the tool's own result
        feed(msg("user", "", json!([tool_result("A2", json!([{"type": "text", "text": "All green."}]))])));
        let a = find("A2");
        assert!(a.state == AGENT_DONE && a.result == "All green.");
        assert_eq!(tr.running(), 0);
        assert!(a.elapsed(a.ended) >= 0.0);
    }

    #[test]
    fn ignores_other_messages() {
        let tr = AgentTracker::new(None, None);
        for line in [
            r#"{"type":"user","message":{"role":"user","content":"hello"}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":"X","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}}"#,
            r#"{"type":"system","subtype":"status","status":"requesting"}"#,
            r#"{"type":"system","subtype":"task_progress","task_id":"unknown","tool_use_id":"nope","description":"x"}"#,
        ] {
            let m: StreamMsg = serde_json::from_str(line).unwrap();
            tr.observe(&m, line.as_bytes(), &m.blocks());
        }
        assert!(tr.snapshot().is_empty());
    }
}
