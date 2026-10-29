//! The tools claude works with in place of Bash: the background shells
//! (shell_run, shell_screen, shell_list, shell_new, shell_input) and jobs for
//! long-running work (job_start, job_status, job_tail, job_stop). A tool that
//! runs something asks the host for permission right before it does, so the
//! policy holds even if Claude Code did not ask through the approve tool.

use std::path::PathBuf;
use std::sync::Weak;
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::mcp::{Handler, Tool, ToolResult};
use super::policy::{JOB_START, SHELL_INPUT, SHELL_RUN};
use crate::shell::bridge::RunResult;
use crate::shell::jobs::{JobInfo, Jobs};
use crate::shell::registry::{MAIN, Registry, Shell};
use crate::shell::tail;
use crate::util::{fmt_elapsed, unix_millis};

/// What a tool result keeps of a command's output (its end).
pub const MAX_LINES: usize = 400;
pub const MAX_BYTES: usize = 30_000;

/// What the tools need from the app.
pub trait ToolHost: Send + Sync {
    fn shells(&self) -> &Registry;
    fn jobs(&self) -> &Jobs;
    /// Shows a tool call in the conversation: a tag (exec, job ...) and what it does
    fn note(&self, tag: &str, detail: &str);
    /// A command started (true) or ended (false)
    fn executing(&self, on: bool);
    /// Whether this call may run, asking the user if the policy says so;
    /// Err is the refusal to hand back to claude
    fn permit(&self, tool: &str, input: &Map<String, Value>) -> Result<(), String>;
}

fn str_arg<'a>(args: &'a Map<String, Value>, k: &str) -> &'a str {
    args.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// The shell a call names, main by default.
fn shell_arg(args: &Map<String, Value>) -> String {
    let s = str_arg(args, "shell").trim();
    if s.is_empty() { MAIN.to_string() } else { s.to_string() }
}

/// The shell name as the conversation shows it before a command ("" for main).
pub fn with_shell(name: &str, text: &str) -> String {
    if name.is_empty() || name == MAIN { text.to_string() } else { format!("{name} › {text}") }
}

type Call = fn(&dyn ToolHost, &Map<String, Value>) -> ToolResult;

fn wrap(host: &Weak<dyn ToolHost>, f: Call) -> Box<Handler> {
    let w = host.clone();
    Box::new(move |args| match w.upgrade() {
        Some(h) => f(h.as_ref(), args),
        None => ToolResult::error("Error: shutting down"),
    })
}

/// The shell and job tools, served to claude as mcp__va__*.
pub fn tools(host: Weak<dyn ToolHost>) -> Vec<Tool> {
    let shell_prop = json!({"type": "string", "description": "The shell to use (default \"main\"; see shell_list)"});
    vec![
        Tool {
            name: "shell_run",
            description: "Run a command in a background shell (zsh) and wait for it. The user does not see these shells. The default shell is \"main\"; its working directory and environment persist between calls. Returns the exit code, the shell's directory afterwards and the end of the output (at most 400 lines / 30 KB); the shell's log file, named in the result, has all of it. Non-interactive commands only (no editors, pagers, REPLs or watchers that do not exit); use job_start for anything long-running. One command at a time per shell; shell_new makes another.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command line to run"},
                    "shell": shell_prop,
                    "timeout_sec": {"type": "number", "description": "Seconds before the command is interrupted with Ctrl-C (default 120, max 600)"},
                },
                "required": ["command"],
            }),
            handler: wrap(&host, shell_run),
        },
        Tool {
            name: "shell_screen",
            description: "Read what a background shell's terminal shows now, as plain text: e.g. a program waiting for input, or output shell_run did not return. Read-only.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "shell": shell_prop,
                    "scrollback_lines": {"type": "number", "description": "Lines of history above the screen (default 0, max 1000)"},
                },
            }),
            handler: wrap(&host, shell_screen),
        },
        Tool {
            name: "shell_list",
            description: "List the background shells: name, working directory, whether it is at its prompt or running something, and its log file; and the jobs that are running. Read-only.",
            input_schema: json!({"type": "object", "properties": {}}),
            handler: wrap(&host, shell_list),
        },
        Tool {
            name: "shell_new",
            description: "Start another background shell (zsh) with a name of its own, e.g. to work somewhere without moving the main shell's directory.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Letters, digits, - and _ (up to 32)"},
                    "dir": {"type": "string", "description": "Where it starts (default: voice-agent's working directory)"},
                },
                "required": ["name"],
            }),
            handler: wrap(&host, shell_new),
        },
        Tool {
            name: "shell_input",
            description: "Type text into a background shell to answer a prompt that a running command waits on (e.g. y/n). The user is asked every time. Never type passwords or other secrets. Returns the shell's screen afterwards.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "What to type"},
                    "shell": shell_prop,
                    "enter": {"type": "boolean", "description": "Press Enter after the text (default true)"},
                },
                "required": ["text"],
            }),
            handler: wrap(&host, shell_input),
        },
        Tool {
            name: "job_start",
            description: "Start a long-running command (build, test suite, server, download) as a background job: a process of its own (zsh -lc), not in a shell, with its output going to a log file. Returns at once with the job id; follow it with job_status and job_tail, end it with job_stop.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command line to run"},
                    "dir": {"type": "string", "description": "Where it runs (default: the main shell's current directory)"},
                },
                "required": ["command"],
            }),
            handler: wrap(&host, job_start),
        },
        Tool {
            name: "job_status",
            description: "Whether a job still runs, or how it ended (exit code, when); every job when id is left out. Read-only.",
            input_schema: json!({"type": "object", "properties": {"id": {"type": "number", "description": "The job id (default: all jobs)"}}}),
            handler: wrap(&host, job_status),
        },
        Tool {
            name: "job_tail",
            description: "The last lines of a job's output. Read-only.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "number", "description": "The job id"},
                    "lines": {"type": "number", "description": "How many lines (default 50, max 400)"},
                },
                "required": ["id"],
            }),
            handler: wrap(&host, job_tail),
        },
        Tool {
            name: "job_stop",
            description: "Stop a running job: SIGTERM to its process group, SIGKILL 3 seconds later if it is still there.",
            input_schema: json!({"type": "object", "properties": {"id": {"type": "number", "description": "The job id"}}, "required": ["id"]}),
            handler: wrap(&host, job_stop),
        },
    ]
}

fn shell(h: &dyn ToolHost, args: &Map<String, Value>) -> Result<std::sync::Arc<Shell>, ToolResult> {
    h.shells().shell(&shell_arg(args)).map_err(|e| ToolResult::error(format!("Error: {e}")))
}

fn shell_run(h: &dyn ToolHost, args: &Map<String, Value>) -> ToolResult {
    let command = str_arg(args, "command").trim();
    if command.is_empty() {
        return ToolResult::error("Error: empty command");
    }
    let sh = match shell(h, args) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if let Err(refusal) = h.permit(SHELL_RUN, args) {
        return ToolResult::error(refusal);
    }
    let sec = args.get("timeout_sec").and_then(Value::as_f64).filter(|v| *v > 0.0).unwrap_or(120.0).clamp(1.0, 600.0);
    h.note("exec", &with_shell(&sh.name, command));
    h.executing(true);
    let res = sh.run(command, Duration::from_secs_f64(sec));
    h.executing(false);
    match res {
        Ok(r) => ToolResult::ok(describe_run(&r, sec, &sh.name, &sh.status().cwd, &sh.log_path)),
        Err(e) => ToolResult::error(format!("Error: {e}")),
    }
}

/// A command's result as shell_run hands it back.
fn describe_run(r: &RunResult, sec: f64, name: &str, cwd: &str, log: &std::path::Path) -> String {
    let head = if r.incomplete {
        "The command was incomplete (unterminated quote or heredoc?) and was cancelled with Ctrl-C.".to_string()
    } else if r.timed_out {
        format!("Timed out after {sec}s and was interrupted with Ctrl-C.")
    } else if r.aborted {
        "Interrupted by the user (Ctrl-C).".to_string()
    } else {
        match r.exit_code {
            Some(code) => format!("exit code: {code}"),
            None => "exit code: unknown".to_string(),
        }
    };
    let mut out = format!("{head} (shell {name}, cwd {cwd})");
    if r.output.is_empty() {
        out += "\n(no output)";
        return out;
    }
    let (text, cut) = tail(&r.output, MAX_LINES, MAX_BYTES);
    out += "\noutput:\n";
    out += &text;
    if cut > 0 || r.cut {
        out += &format!("\n(the first {cut} lines are left out; all of it is in {})", log.display());
    } else {
        out += &format!("\n(log: {})", log.display());
    }
    out
}

fn shell_screen(h: &dyn ToolHost, args: &Map<String, Value>) -> ToolResult {
    let sh = match shell(h, args) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let n = args.get("scrollback_lines").and_then(Value::as_f64).map_or(0, |v| v.clamp(0.0, 1000.0) as usize);
    h.note("screen", &format!("{} の画面を読む", sh.name));
    let text = sh.screen_text(n);
    if text.trim().is_empty() { ToolResult::ok(format!("(the screen of {} is empty)", sh.name)) } else { ToolResult::ok(text) }
}

fn shell_list(h: &dyn ToolHost, _: &Map<String, Value>) -> ToolResult {
    let mut lines: Vec<String> = Vec::new();
    for s in h.shells().list() {
        let st = s.status();
        let doing = if st.running {
            "running a command"
        } else if st.idle {
            "at its prompt"
        } else {
            "busy with a program (see shell_screen)"
        };
        lines.push(format!("{} · {} · {doing} · log {}", s.name, st.cwd, s.log_path.display()));
    }
    if lines.is_empty() {
        lines.push("(no shell yet; main starts with the first shell_run)".into());
    }
    let now = unix_millis();
    let running: Vec<String> = h.jobs().list().iter().filter(|j| j.running()).map(|j| describe_job(j, now)).collect();
    if !running.is_empty() {
        lines.push(format!("jobs running:\n{}", running.join("\n")));
    }
    h.note("shells", "シェルの一覧を読む");
    ToolResult::ok(lines.join("\n"))
}

fn shell_new(h: &dyn ToolHost, args: &Map<String, Value>) -> ToolResult {
    let name = str_arg(args, "name").trim();
    let dir = str_arg(args, "dir").trim();
    let dir = (!dir.is_empty()).then(|| PathBuf::from(dir));
    match h.shells().start(name, dir) {
        Ok(s) => {
            h.note("shell", &format!("新しいシェル {name}"));
            ToolResult::ok(format!("started shell {name} in {} (log {})", s.status().cwd, s.log_path.display()))
        }
        Err(e) => ToolResult::error(format!("Error: {e}")),
    }
}

fn shell_input(h: &dyn ToolHost, args: &Map<String, Value>) -> ToolResult {
    let text = str_arg(args, "text");
    let sh = match shell(h, args) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if let Err(refusal) = h.permit(SHELL_INPUT, args) {
        return ToolResult::error(refusal);
    }
    let enter = args.get("enter").and_then(Value::as_bool).unwrap_or(true);
    h.note("input", &with_shell(&sh.name, &format!("{text:?}{}", if enter { " ⏎" } else { "" })));
    sh.input(&if enter { format!("{text}\r") } else { text.to_string() });
    // What the program did with it
    std::thread::sleep(Duration::from_millis(600));
    ToolResult::ok(format!("typed into {}. The screen now:\n{}", sh.name, sh.screen_text(0)))
}

fn describe_job(j: &JobInfo, now: i64) -> String {
    let state = if j.running() {
        format!("running for {}", fmt_elapsed((now - j.started) as f64 / 1000.0))
    } else if j.stopped {
        format!("stopped (exit {})", j.exit.map_or("?".to_string(), |c| c.to_string()))
    } else {
        format!("ended with exit code {} after {}", j.exit.map_or("?".to_string(), |c| c.to_string()), fmt_elapsed((j.ended - j.started) as f64 / 1000.0))
    };
    format!("job {} · {state} · {} · in {} · log {}", j.id, j.command, j.dir.display(), j.log_path.display())
}

fn job_id(args: &Map<String, Value>) -> Option<u32> {
    args.get("id")
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)).or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
        .map(|v| v as u32)
}

fn job_start(h: &dyn ToolHost, args: &Map<String, Value>) -> ToolResult {
    let command = str_arg(args, "command").trim();
    if command.is_empty() {
        return ToolResult::error("Error: empty command");
    }
    if let Err(refusal) = h.permit(JOB_START, args) {
        return ToolResult::error(refusal);
    }
    let dir = str_arg(args, "dir").trim();
    let dir = if dir.is_empty() {
        // Where the main shell is now, as a command there would run
        h.shells().get(MAIN).map(|s| s.status().cwd).filter(|c| !c.is_empty()).map(PathBuf::from)
    } else {
        Some(PathBuf::from(dir))
    };
    match h.jobs().start(command, dir) {
        Ok(j) => {
            h.note("job", &format!("#{} {command}", j.id));
            ToolResult::ok(format!("job {} started (pid {}) in {}; its output goes to {}", j.id, j.pid, j.dir.display(), j.log_path.display()))
        }
        Err(e) => ToolResult::error(format!("Error: {e}")),
    }
}

fn job_status(h: &dyn ToolHost, args: &Map<String, Value>) -> ToolResult {
    let now = unix_millis();
    match job_id(args) {
        Some(id) => match h.jobs().get(id) {
            Some(j) => ToolResult::ok(describe_job(&j, now)),
            None => ToolResult::error(format!("Error: there is no job {id}")),
        },
        None => {
            let all = h.jobs().list();
            if all.is_empty() { ToolResult::ok("(no jobs)") } else { ToolResult::ok(all.iter().map(|j| describe_job(j, now)).collect::<Vec<_>>().join("\n")) }
        }
    }
}

fn job_tail(h: &dyn ToolHost, args: &Map<String, Value>) -> ToolResult {
    let Some(id) = job_id(args) else { return ToolResult::error("Error: id is needed") };
    let lines = args.get("lines").and_then(Value::as_f64).map_or(50, |v| v.clamp(1.0, MAX_LINES as f64) as usize);
    match h.jobs().tail(id, lines) {
        Ok(text) => {
            let (text, _) = tail(&text, MAX_LINES, MAX_BYTES);
            if text.is_empty() { ToolResult::ok(format!("(job {id} has printed nothing yet)")) } else { ToolResult::ok(text) }
        }
        Err(e) => ToolResult::error(format!("Error: {e}")),
    }
}

fn job_stop(h: &dyn ToolHost, args: &Map<String, Value>) -> ToolResult {
    let Some(id) = job_id(args) else { return ToolResult::error("Error: id is needed") };
    match h.jobs().stop(id) {
        Ok(j) if j.running() || j.stopped => {
            h.note("job", &format!("#{id} を止める"));
            ToolResult::ok(format!("job {id} is being stopped"))
        }
        Ok(j) => ToolResult::ok(describe_job(&j, unix_millis())),
        Err(e) => ToolResult::error(format!("Error: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::registry::ShellOptions;
    use std::sync::{Arc, Mutex};

    /// A host with real shells and jobs whose permission comes from a list.
    struct Host {
        shells: Registry,
        jobs: Jobs,
        notes: Mutex<Vec<String>>,
        executing: Mutex<i32>,
        refuse: Vec<&'static str>, // commands refused
        asked: Mutex<Vec<String>>,
    }

    impl ToolHost for Host {
        fn shells(&self) -> &Registry {
            &self.shells
        }
        fn jobs(&self) -> &Jobs {
            &self.jobs
        }
        fn note(&self, tag: &str, detail: &str) {
            self.notes.lock().unwrap().push(format!("[{tag}] {detail}"));
        }
        fn executing(&self, on: bool) {
            *self.executing.lock().unwrap() += if on { 1 } else { -1 };
        }
        fn permit(&self, tool: &str, input: &Map<String, Value>) -> Result<(), String> {
            let what = format!("{tool} {}", str_arg(input, if tool == SHELL_INPUT { "text" } else { "command" }));
            self.asked.lock().unwrap().push(what.clone());
            if self.refuse.iter().any(|r| what.ends_with(r)) { Err(format!("refused: {what}")) } else { Ok(()) }
        }
    }

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    fn call(tools: &[Tool], name: &str, a: Value) -> ToolResult {
        (tools.iter().find(|t| t.name == name).unwrap().handler)(&args(a))
    }

    #[test]
    fn results_keep_the_end_and_name_the_log() {
        let log = std::path::Path::new("/rt/shells/main.log");
        let r = RunResult { exit_code: Some(0), output: "a\nb".into(), ..RunResult::default() };
        assert_eq!(describe_run(&r, 120.0, "main", "/w", log), "exit code: 0 (shell main, cwd /w)\noutput:\na\nb\n(log: /rt/shells/main.log)");
        let long: String = (1..=1000).map(|i| format!("line {i}\n")).collect();
        let r = RunResult { exit_code: Some(1), output: long.trim_end().into(), ..RunResult::default() };
        let d = describe_run(&r, 120.0, "main", "/w", log);
        assert!(
            d.contains("\nline 601\n") && !d.contains("\nline 600\n") && d.ends_with("(the first 600 lines are left out; all of it is in /rt/shells/main.log)"),
            "{d}"
        );
        let r = RunResult { timed_out: true, ..RunResult::default() };
        assert!(describe_run(&r, 5.0, "b", "/", log).starts_with("Timed out after 5s"));
        assert_eq!(with_shell("main", "ls"), "ls");
        assert_eq!(with_shell("build", "ls"), "build › ls");
    }

    /// Runs the tools against real zsh shells and jobs (skipped without zsh).
    #[test]
    fn tools_drive_shells_and_jobs() {
        if !std::path::Path::new("/bin/zsh").exists() {
            return;
        }
        let rt = std::env::temp_dir().join(format!("va-tools-{}", std::process::id()));
        let opts = ShellOptions { shell: "/bin/zsh".into(), dir: std::env::temp_dir(), runtime_dir: rt.clone() };
        let host = Arc::new(Host {
            shells: Registry::new(opts),
            jobs: Jobs::new("/bin/zsh", std::env::temp_dir(), rt.clone()),
            notes: Mutex::default(),
            executing: Mutex::default(),
            refuse: vec!["rm -rf /tmp/nothing-here"],
            asked: Mutex::default(),
        });
        let dyn_host: Arc<dyn ToolHost> = host.clone();
        let tools = tools(Arc::downgrade(&dyn_host));

        let r = call(&tools, "shell_run", json!({"command": "cd / && echo hi"}));
        assert!(!r.is_error && r.text.starts_with("exit code: 0 (shell main, cwd /)") && r.text.contains("\nhi\n"), "{}", r.text);
        // Refused by the host: nothing runs
        let r = call(&tools, "shell_run", json!({"command": "rm -rf /tmp/nothing-here"}));
        assert!(r.is_error && r.text.starts_with("refused"), "{}", r.text);
        assert!(call(&tools, "shell_run", json!({"command": " "})).is_error);
        assert!(call(&tools, "shell_run", json!({"command": "ls", "shell": "nope"})).text.contains("no shell named"));

        assert!(!call(&tools, "shell_new", json!({"name": "build", "dir": "/tmp"})).is_error);
        let r = call(&tools, "shell_run", json!({"command": "pwd -P", "shell": "build"}));
        assert!(r.text.contains("/private/tmp") || r.text.contains("\n/tmp\n"), "{}", r.text);
        let list = call(&tools, "shell_list", json!({})).text;
        assert!(list.contains("main · / · at its prompt") && list.contains("build · "), "{list}");
        assert!(call(&tools, "shell_screen", json!({})).text.contains("hi"));

        // A job runs where the main shell is
        let r = call(&tools, "job_start", json!({"command": "pwd; echo done"}));
        assert!(!r.is_error && r.text.starts_with("job 1 started") && r.text.contains(" in /;"), "{}", r.text);
        for _ in 0..200 {
            if host.jobs.get(1).is_some_and(|j| !j.running()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(call(&tools, "job_status", json!({"id": 1})).text.contains("ended with exit code 0"));
        assert_eq!(call(&tools, "job_tail", json!({"id": 1})).text, "/\ndone");
        assert!(call(&tools, "job_tail", json!({})).is_error);
        let r = call(&tools, "job_start", json!({"command": "sleep 30", "dir": "/tmp"}));
        assert!(r.text.starts_with("job 2 started"), "{}", r.text);
        assert!(call(&tools, "job_stop", json!({"id": "2"})).text.contains("being stopped"));
        assert!(call(&tools, "job_status", json!({})).text.contains("job 2 · "));

        assert_eq!(*host.executing.lock().unwrap(), 0);
        let asked = host.asked.lock().unwrap().clone();
        assert!(asked.contains(&format!("{SHELL_RUN} cd / && echo hi")) && asked.contains(&format!("{JOB_START} sleep 30")), "{asked:?}");
        let notes = host.notes.lock().unwrap().clone();
        assert!(notes.contains(&"[exec] cd / && echo hi".to_string()) && notes.contains(&"[exec] build › pwd -P".to_string()), "{notes:?}");
        assert!(!notes.iter().any(|n| n.contains("nothing-here")), "a refused command is not shown as run: {notes:?}");
        host.shells.close_all();
        host.jobs.stop_all();
        let _ = std::fs::remove_dir_all(&rt);
    }
}
