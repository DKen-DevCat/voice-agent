//! Decides whether a tool call from Claude runs, is refused, or needs the
//! user's OK. Commands go through our own shell tools rather than Claude
//! Code's Bash, so the rules are applied here:
//!
//! 1. Rewriting git history is forbidden outright (no question asked), even
//!    without a deny rule in ~/.claude/settings.json.
//! 2. The user's own `Bash(...)` deny rules refuse what they match.
//! 3. Risky commands (PRs, deleting branches, pushing, deploying, adding
//!    dependencies over the network, deleting files...) wait for the user.
//! 4. Anything else runs.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use crate::util::home_dir;

pub const SHELL_RUN: &str = "mcp__va__shell_run";
pub const SHELL_SCREEN: &str = "mcp__va__shell_screen";
pub const SHELL_LIST: &str = "mcp__va__shell_list";
pub const SHELL_NEW: &str = "mcp__va__shell_new";
pub const SHELL_INPUT: &str = "mcp__va__shell_input";
pub const JOB_START: &str = "mcp__va__job_start";
pub const JOB_STATUS: &str = "mcp__va__job_status";
pub const JOB_TAIL: &str = "mcp__va__job_tail";
pub const JOB_STOP: &str = "mcp__va__job_stop";
pub const APPROVE: &str = "mcp__va__approve";

const AUTO_ALLOW: [&str; 15] = [
    SHELL_SCREEN,
    SHELL_LIST,
    SHELL_NEW,
    JOB_STATUS,
    JOB_TAIL,
    JOB_STOP,
    "Read",
    "Glob",
    "Grep",
    "WebSearch",
    "WebFetch",
    "TodoWrite",
    "Agent",
    // Stopping a subagent and sending one a message mid-task
    "TaskStop",
    "SendMessage",
];

/// A word of a command line: quoted, or with backslash escapes.
const WORD: &str = r#"(?:"[^"]*"|'[^']*'|(?:\\.|[^\s\;&|])+)"#;
/// The rest of one simple command.
const REST: &str = r"[^;&|\n]*";
/// The end of an option: the end of its word (or of the quotes around it).
const END: &str = r#"(?:[\s;&|)'"]|$)"#;

/// git with any global options before its subcommand (`git -C "my dir"`,
/// `git --work-tree x`, `git -P`, `git -c k=v`); an option may take the next
/// word as its value.
fn git() -> String {
    format!(r"\bgit(?:\s+-[^\s;&|]*(?:\s+{WORD})?)*\s+")
}

/// History rewrites: never run, whatever the user's deny rules say. git also
/// takes a long option's unique prefix (`--amen`), so those count too.
fn forbidden() -> &'static Vec<(Regex, &'static str)> {
    static RES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    RES.get_or_init(|| {
        let git = git();
        [
            (format!(r"{git}reset\b{REST}\s--ha(?:rd?)?{END}"), "git reset --hard"),
            (format!(r"{git}branch\b{REST}\s(?:-[A-Za-z]*[Df][A-Za-z]*|--force){END}"), "git branch -D"),
            (format!(r#"{git}push\b{REST}(?:\s(?:-[A-Za-z]*f[A-Za-z]*|--force(?:-with-lease|-if-includes)?)(?:[\s;&|)='"]|$)|\s\+\S)"#), "git push --force"),
            (format!(r"{git}rebase\b"), "git rebase"),
            (format!(r#"{git}pull\b{REST}\s(?:--rebase|-r)(?:[\s;&|)'"]|$|=(?:true|merges|interactive|i)\b)"#), "git pull --rebase"),
            (format!(r#"{git}commit\b{REST}\s--am(?:e(?:nd?)?)?(?:[\s;&|)='"]|$)"#), "git commit --amend"),
            (format!(r"{git}(?:filter-branch|filter-repo)\b"), "git filter-branch"),
        ]
        .into_iter()
        .map(|(re, what)| (Regex::new(&re).unwrap(), what))
        .collect()
    })
}

/// A pattern and why it needs asking (shown and spoken to the user).
fn risky() -> &'static Vec<(Regex, &'static str)> {
    static RES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    RES.get_or_init(|| {
        let git = git();
        // Services and the verbs that change something there, anywhere after the
        // tool's name (`wrangler pages deploy`, `kubectl -n prod delete`)
        let services = "wrangler|vercel|netlify|firebase|terraform|tofu|pulumi|cdktf|kubectl|helm|fly|flyctl|heroku|supabase|aws|gcloud|gsutil|az|doctl|railway|serverless|sls";
        let changes = "deploy|publish|apply|destroy|delete|push|rm|remove|rollback|release|scale|up|down|put|cp|mv|sync|create|update|set|patch|replace|drain|cordon|install|uninstall|upgrade";
        [
            (r"\bgh\s+pr\s+(?:create|new)\b".to_string(), "プルリクエストを作成します"),
            (
                r"\bgh\s+(?:pr\s+(?:merge|close|edit|ready|reopen|comment|review)|issue\s+(?:create|close|edit|delete|comment|reopen|transfer)|release|repo\s+(?:create|delete|edit|rename|archive|fork)|secret|variable|workflow\s+(?:run|enable|disable)|run\s+(?:rerun|cancel|delete))\b".into(),
                "GitHub に変更を反映します",
            ),
            // A method other than GET, or fields (which make it a POST)
            (
                format!(r"\bgh\s+api\b{REST}(?:\s(?:-X|--method)[\s=]*(?i:post|put|patch|delete)\b|\s(?:-f|-F|--field|--raw-field|--input)(?:[\s=]|$))"),
                "GitHub に変更を反映します",
            ),
            (format!(r"{git}branch\b{REST}\s(?:-[A-Za-z]*d[A-Za-z]*|--delete){END}"), "ブランチを削除します"),
            (format!(r"{git}push\b{REST}(?:\s--delete\b|\s-d{END}|\s:\S)"), "リモートのブランチを削除します"),
            (format!(r"{git}push\b"), "リモートへ push します"),
            (
                format!(r"{git}(?:reset|clean|restore|merge|cherry-pick|revert|stash\s+(?:drop|clear))\b|{git}checkout\s+(?:--\s|\.)"),
                "git の作業ツリーを書き換えます",
            ),
            (r"\b(?:npm|pnpm|yarn|bun)\s+(?:i|install|add|remove|rm|uninstall|publish|update|upgrade)\b".into(), "ネットワークから依存パッケージを追加・変更します"),
            (
                r"\b(?:pip3?|uv|poetry|brew|gem|cargo|go|bundle|composer)\s+(?:install|uninstall|remove|add|get|update|upgrade|publish)\b|\buv\s+pip\s+install\b".into(),
                "ネットワークから依存パッケージを追加・変更します",
            ),
            (
                format!(r"\b(?:curl|wget)\b{REST}\|\s*(?:sudo\s+)?(?:env\s+)?(?:(?:ba|z|da|k|fi)?sh|python[0-9.]*|node|ruby|perl|php)\b"),
                "ダウンロードしたスクリプトを実行します",
            ),
            (format!(r"\b(?:(?:ba|z|da|k)?sh|python[0-9.]*|node|ruby|perl)\b{REST}(?:\$\(|<\(|`)\s*(?:curl|wget)\b"), "ダウンロードしたスクリプトを実行します"),
            (format!(r"\b(?:{services})\b{REST}\s(?:{changes})\b"), "外部サービスへ変更を反映します"),
            (format!(r"\bvercel\b{REST}\s--prod\b"), "外部サービスへ変更を反映します"),
            (r"\bdocker\s+(?:push|rm|rmi|system\s+prune|volume\s+rm|compose\s+down\s+-v)\b".into(), "Docker のイメージやリソースを変更します"),
            (format!(r"\brm\b{REST}\s(?:-[A-Za-z]*[rRf][A-Za-z]*|--recursive|--force){END}"), "ファイルを再帰的または強制的に削除します"),
            (r"\bsudo\b".into(), "管理者権限で実行します"),
            (r"\b(?:shutdown|reboot|halt|launchctl|diskutil|mkfs|dd|csrutil|nvram)\b".into(), "システムに関わる操作です"),
            (r"\b(?:kill|killall|pkill)\b".into(), "プロセスを終了させます"),
            (r"\b(?:chmod|chown)\s+-R\b".into(), "権限を再帰的に変更します"),
            (r"\bcrontab\b".into(), "定期実行の設定を変更します"),
            (r">\s*/(?:etc|usr|System|Library|bin|sbin)/".into(), "システム領域のファイルに書き込みます"),
        ]
        .into_iter()
        .map(|(re, why)| (Regex::new(&re).unwrap(), why))
        .collect()
    })
}

/// Why raw keystrokes into a shell are asked.
const INPUT_REASON: &str = "シェルに直接入力します";
/// Why a tool that may write outside the working directory is asked.
const OUTSIDE_REASON: &str = "作業ディレクトリの外を変更する可能性があります";

/// Every reason a call can be asked for, once each (the phrase cache says
/// them). A question names several joined by "、", so none contains one.
pub fn ask_reasons() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for why in risky().iter().map(|(_, w)| *w).chain([INPUT_REASON, OUTSIDE_REASON]) {
        if !out.contains(&why) {
            out.push(why);
        }
    }
    out
}

pub struct DenyRule {
    pub rule: String,
    pub re: Regex,
}

/// The Bash rules of permissions.deny in the user's ~/.claude/settings.json
/// and in dir's .claude/settings.json and settings.local.json (they can only
/// refuse more).
pub fn load_deny_rules(dir: &Path) -> Vec<DenyRule> {
    let files = [home_dir().join(".claude/settings.json"), dir.join(".claude/settings.json"), dir.join(".claude/settings.local.json")];
    let mut rules = Vec::new();
    for f in files {
        let Ok(data) = std::fs::read(&f) else { continue };
        let Ok(v) = serde_json::from_slice::<Value>(&data) else { continue };
        let deny = v.pointer("/permissions/deny").and_then(Value::as_array).cloned().unwrap_or_default();
        rules.extend(deny.iter().filter_map(Value::as_str).filter_map(deny_rule));
    }
    rules
}

fn deny_rule(r: &str) -> Option<DenyRule> {
    // A bare "Bash" refuses every command
    if r == "Bash" {
        return Some(DenyRule { rule: r.to_string(), re: Regex::new("^.*$").unwrap() });
    }
    let inner = r.strip_prefix("Bash(")?.strip_suffix(')')?;
    let rule = match inner.strip_suffix(":*") {
        Some(head) => format!("{head} *"),
        None => inner.to_string(),
    };
    let mut re = rule.split('*').map(regex::escape).collect::<Vec<_>>().join(".*");
    // "rm -rf *" also matches a bare "rm -rf"
    if let Some(head) = re.strip_suffix(" .*") {
        re = format!("{head}( .*)?");
    }
    Regex::new(&format!("^{re}$")).ok().map(|re| DenyRule { rule: r.to_string(), re })
}

/// Roughly splits a command line into simple commands, dropping leading
/// VAR=value assignments and the commands that run the rest of the line
/// (`nohup`, `time`, `env` ...): what they run is what the rules see.
fn segments(command: &str) -> Vec<String> {
    static RES: OnceLock<[Regex; 4]> = OnceLock::new();
    let [split, vars, group, wrapper] = RES.get_or_init(|| {
        [
            Regex::new(r"&&|\|\||;|\||&|\n|\$\(|<\(|`").unwrap(),
            Regex::new(r"^(?:\w+=\S*\s+)+").unwrap(),
            Regex::new(r"^[({]\s*").unwrap(),
            Regex::new(r"^(?:(?:time|nohup|env|command|builtin|exec|nice|caffeinate|xargs)(?:\s+-\S+)*\s+)+").unwrap(),
        ]
    });
    split
        .split(command)
        .map(|s| {
            let mut s = s.trim().to_string();
            loop {
                let next = wrapper.replace(&vars.replace(&group.replace(&s, ""), ""), "").into_owned();
                if next == s {
                    return s;
                }
                s = next;
            }
        })
        .filter(|s| !s.is_empty())
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask(String),
    Deny(String),
}

/// What happens to a shell command line. A question names every reason
/// that applies (`git push && wrangler deploy` is both).
pub fn check_command(command: &str, deny: &[DenyRule]) -> Decision {
    // zsh joins a line that ends in a backslash with the next one
    let command = &command.replace("\\\n", " ");
    if let Some((_, what)) = forbidden().iter().find(|(re, _)| re.is_match(command)) {
        return Decision::Deny(format!(
            "Forbidden by voice-agent's fixed policy: it rewrites git history ({what}). This is never allowed, with or without approval. Do not try to work around it (no other spelling, no script, no subagent); tell the user briefly that they can run it themselves if they really want to."
        ));
    }
    for seg in segments(command) {
        if let Some(r) = deny.iter().find(|r| r.re.is_match(&seg)) {
            return Decision::Deny(format!(
                "Blocked by the user's Claude Code deny rule {}. Do not try to work around it; tell the user they can run it themselves if they really want to.",
                r.rule
            ));
        }
    }
    let mut why: Vec<&str> = Vec::new();
    for (re, w) in risky() {
        if re.is_match(command) && !why.contains(w) {
            why.push(w);
        }
    }
    if why.is_empty() { Decision::Allow } else { Decision::Ask(why.join("、")) }
}

pub fn decide(tool: &str, input: &Map<String, Value>, deny: &[DenyRule]) -> Decision {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or_default();
    match tool {
        SHELL_RUN | JOB_START => check_command(s("command"), deny),
        // Raw keystrokes into a shell: always asked, unless they are forbidden anyway
        SHELL_INPUT => match check_command(s("text"), deny) {
            d @ Decision::Deny(_) => d,
            _ => Decision::Ask(INPUT_REASON.to_string()),
        },
        _ if AUTO_ALLOW.contains(&tool) => Decision::Allow,
        // e.g. Edit/Write outside the working directory (acceptEdits covers the inside)
        _ => Decision::Ask(OUTSIDE_REASON.to_string()),
    }
}

/// What voice-agent keeps out of the agent's reach (lfm3-vl-server
/// docs/native-app.md §14.6): writing there is refused outright, whatever the
/// tool. Always voice-agent's config directory (the paired iPhone's keys, the
/// sessions) and its control socket; under serve also the folders approvals
/// rest on (voice-agent's and lfm3-vl-server's, now WKEN-AGENTS too), whose names are enough in a
/// command. Commands are read as text, so a script can still get around it:
/// this narrows the way, and the iPhone notices what gets through.
#[derive(Clone, Debug, Default)]
pub struct Guarded {
    paths: Vec<std::path::PathBuf>,
    /// Under serve, a command naming a guarded folder is refused too: the
    /// patterns for those names, made once
    names: Vec<Regex>,
    /// Where relative paths start (Claude's working directory)
    base: std::path::PathBuf,
}

impl Guarded {
    /// paths: what is guarded; names: whether a command naming one by its
    /// last part is refused (serve); base: where relative paths start.
    pub fn new(paths: Vec<std::path::PathBuf>, names: bool, base: std::path::PathBuf) -> Guarded {
        let names = if names {
            paths
                .iter()
                .filter_map(|p| p.file_name())
                .filter_map(|n| Regex::new(&format!(r#"(?:^|[\s/'"=:])\.?{}(?:$|[\s/'";&|)])"#, regex::escape(&n.to_string_lossy()))).ok())
                .collect()
        } else {
            Vec::new()
        };
        Guarded { paths: paths.into_iter().filter(|p| !p.as_os_str().is_empty()).collect(), names, base }
    }
}

pub const GUARD_REASON: &str = "voice-agent guards this (the paired iPhone's keys, its settings, and the folders approvals rest on): it is refused outright. Do not try to work around it; tell the user they can do it themselves at the Mac.";

/// Claude Code's own tools that write files.
const WRITE_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// wd: where a shell tool's command would run (the shell's directory, or the
/// dir it is given). Found in the review: a command that names nothing
/// guarded can still write there when it runs in a guarded folder.
pub fn guard(tool: &str, input: &Map<String, Value>, g: &Guarded, wd: Option<&str>) -> Option<String> {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or_default();
    let runs_inside = || wd.is_some_and(|w| path_inside(w, g));
    let hit = match tool {
        SHELL_RUN | JOB_START => command_touches(s("command"), g) || runs_inside(),
        SHELL_INPUT => command_touches(s("text"), g) || runs_inside(),
        SHELL_NEW => runs_inside(),
        _ if WRITE_TOOLS.contains(&tool) => ["file_path", "notebook_path", "path"].iter().any(|k| path_inside(s(k), g)),
        _ => false,
    };
    hit.then(|| GUARD_REASON.to_string())
}

fn command_touches(cmd: &str, g: &Guarded) -> bool {
    static SUB: OnceLock<Regex> = OnceLock::new();
    let sub = SUB.get_or_init(|| Regex::new(&format!(r"\bvoice-agent\b{REST}\b(?:pair|unpair|serve)\b")).unwrap());
    if sub.is_match(cmd) {
        return true;
    }
    let home = home_dir();
    g.paths.iter().any(|p| mentions(cmd, p, &home)) || g.names.iter().any(|re| re.is_match(cmd))
}

/// Whether cmd names p in full, or from ~ or $HOME.
fn mentions(cmd: &str, p: &Path, home: &Path) -> bool {
    let mut forms = vec![p.to_string_lossy().into_owned()];
    if let Ok(rest) = p.strip_prefix(home)
        && !home.as_os_str().is_empty()
    {
        let rest = rest.to_string_lossy();
        forms.extend([format!("~/{rest}"), format!("$HOME/{rest}"), format!("${{HOME}}/{rest}")]);
    }
    forms.iter().any(|f| cmd.contains(f.as_str()))
}

fn path_inside(target: &str, g: &Guarded) -> bool {
    if target.is_empty() {
        return false;
    }
    // an absolute target replaces the base
    let t = resolve(&g.base.join(crate::util::expand_home(target, &home_dir())));
    g.paths.iter().any(|p| t.starts_with(resolve(p)))
}

/// Where p really is: links followed as far as the path exists, and `..`
/// taken from where a link really leads (found in the security review:
/// taking `..` as text first let `link/../x` reach past a link). /tmp is
/// /private/tmp.
pub fn resolve(p: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if let Ok(real) = out.canonicalize() {
                    out = real;
                }
                out.pop();
            }
            c => {
                out.push(c);
                if let Ok(real) = out.canonicalize() {
                    out = real;
                }
            }
        }
    }
    out
}

/// Reads a spoken/typed reply to a pending approval: Some(yes), Some(no), or
/// None when it is neither (the approval then keeps waiting). A yes has to be
/// the whole reply, since Japanese puts the negation at the end
/// (「許可しません」, 「お願い、やめて」), and a negation anywhere makes it a no.
/// A question (「はい？」) is not a yes.
pub fn parse_consent(text: &str) -> Option<bool> {
    static RES: OnceLock<[Regex; 2]> = OnceLock::new();
    let [yes, no] = RES.get_or_init(|| {
        [
            Regex::new(
                r"^(?:はい|うん|ええ|いいよ|オッケー|オーケー|おっけー|おけ|ok|okay|yes|yeah|yep|お願い|どうぞ|許可|実行して|やって|進めて|了解|大丈夫)+(?:です|ます|します|しますね|ください|下さい|して|よ|ね)*$",
            )
            .unwrap(),
            Regex::new(r"いいえ|いや|だめ|ダメ|駄目|やめ|止め|とめ|ストップ|待って|まって|待て|キャンセル|拒否|しないで|ない|なく|ません|no|stop|wait|cancel|don't|not").unwrap(),
        ]
    });
    let question = text.trim_end().ends_with(['?', '？']);
    let t: String = text.chars().filter(|c| !c.is_whitespace() && !"、。，．,.!！?？…・「」『』\"〜~".contains(*c)).flat_map(char::to_lowercase).collect();
    if t.is_empty() {
        None
    } else if no.is_match(&t) {
        Some(false)
    } else if yes.is_match(&t) && !question {
        Some(true)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(k: &str, v: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert(k.into(), Value::String(v.into()));
        m
    }

    fn action(d: Decision) -> &'static str {
        match d {
            Decision::Allow => "allow",
            Decision::Ask(_) => "ask",
            Decision::Deny(_) => "deny",
        }
    }

    fn guarded(names: bool) -> Guarded {
        let home = home_dir();
        // lfm3-vl-server is being renamed WKEN-AGENTS (2026-10-31); both are
        // kept until the move is done, then lfm3-vl-server can go
        Guarded::new(vec![home.join(".config/voice-agent"), home.join("voice-agent"), home.join("lfm3-vl-server"), home.join("WKEN-AGENTS")], names, home.join("agent-work"))
    }

    #[test]
    fn the_guard_refuses_writes_with_any_tool() {
        // found in the design review: the fixed refusals only looked at the
        // shell tools, so Claude's own Write reached devices.json as an
        // ordinary approval
        let g = guarded(false);
        let home = home_dir().to_string_lossy().into_owned();
        for (tool, k, v) in [
            ("Write", "file_path", format!("{home}/.config/voice-agent/devices.json")),
            ("Edit", "file_path", "~/.config/voice-agent/sessions.json".to_string()),
            ("MultiEdit", "file_path", format!("{home}/lfm3-vl-server/server/app.py")),
            ("MultiEdit", "file_path", format!("{home}/WKEN-AGENTS/server/app.py")),
            ("Edit", "file_path", format!("{home}/WKEN-AGENTS/docs/design.md")),
            ("Edit", "file_path", "~/WKEN-AGENTS/docs/design.md".to_string()),
            ("NotebookEdit", "notebook_path", format!("{home}/voice-agent/x.ipynb")),
            // out of the working directory and back in
            ("Write", "file_path", "../voice-agent/src/app.rs".to_string()),
            ("Write", "file_path", format!("{home}/agent-work/../lfm3-vl-server/ios/LFM3/LFM3App.swift")),
            ("Write", "file_path", format!("{home}/agent-work/../WKEN-AGENTS/ios/LFM3/LFM3App.swift")),
            ("Write", "file_path", format!("{home}/WKEN-AGENTS/scripts/start.sh")),
        ] {
            assert!(guard(tool, &input(k, &v), &g, None).is_some(), "{tool} {v}");
        }
        for (tool, k, v) in [
            ("Write", "file_path", format!("{home}/agent-work/notes.md")),
            ("Write", "file_path", "notes.md".to_string()),
            ("Write", "file_path", format!("{home}/voice-agent-notes/a.md")),
            ("Write", "file_path", format!("{home}/WKEN-AGENTS-notes/a.md")),
            // reading is not writing
            ("Read", "file_path", format!("{home}/voice-agent/src/app.rs")),
        ] {
            assert!(guard(tool, &input(k, &v), &g, None).is_none(), "{tool} {v}");
        }
    }

    #[test]
    fn the_guard_refuses_commands_that_name_what_it_guards() {
        let home = home_dir().to_string_lossy().into_owned();
        for names in [false, true] {
            let g = guarded(names);
            for cmd in [
                "voice-agent pair 123456".to_string(),
                "./target/release/voice-agent unpair".to_string(),
                "nohup voice-agent serve --dir /tmp &".to_string(),
                format!("cat x > {home}/.config/voice-agent/devices.json"),
                "rm ~/.config/voice-agent/sessions.json".to_string(),
                "cp k.json \"$HOME/.config/voice-agent/devices.json\"".to_string(),
                "echo x >> ${HOME}/lfm3-vl-server/scripts/start.sh".to_string(),
                "echo x >> ${HOME}/WKEN-AGENTS/scripts/start.sh".to_string(),
                "rm ~/WKEN-AGENTS/server/app.py".to_string(),
            ] {
                assert!(guard(SHELL_RUN, &input("command", &cmd), &g, None).is_some(), "{names} {cmd}");
                assert!(guard(JOB_START, &input("command", &cmd), &g, None).is_some(), "{names} {cmd}");
            }
            assert!(guard(SHELL_INPUT, &input("text", "voice-agent pair 1\n"), &g, None).is_some());
            assert!(guard(SHELL_RUN, &input("command", "cargo test && ls ~/agent-work"), &g, None).is_none());
        }
        // under serve the folders' names are enough
        let serve = guarded(true);
        for cmd in ["cd ~ && cd voice-agent && git pull", "cd .. && sed -i '' s/a/b/ lfm3-vl-server/server/app.py", "ls /x/voice-agent", "cd .. && sed -i '' s/a/b/ WKEN-AGENTS/server/app.py", "ls /x/WKEN-AGENTS"] {
            assert!(guard(SHELL_RUN, &input("command", cmd), &serve, None).is_some(), "{cmd}");
            assert!(guard(SHELL_RUN, &input("command", cmd), &guarded(false), None).is_none(), "{cmd}");
        }
        assert!(guard(SHELL_RUN, &input("command", "echo voice-agents-are-fun"), &serve, None).is_none());
    }

    #[test]
    fn a_link_does_not_lead_past_the_guard() {
        let dir = std::env::temp_dir().join(format!("va-guard-link-{}", crate::util::unix_millis()));
        std::fs::create_dir_all(dir.join("work")).unwrap();
        std::fs::create_dir_all(dir.join("conf/sub")).unwrap();
        std::os::unix::fs::symlink(dir.join("conf/sub"), dir.join("work/link")).unwrap();
        let g = Guarded::new(vec![dir.join("conf")], false, dir.join("work"));
        // link/.. is conf, where the link leads, not work
        assert!(guard("Write", &input("file_path", "link/../devices.json"), &g, None).is_some());
        assert!(guard("Write", &input("file_path", "link/x"), &g, None).is_some());
        assert!(guard("Write", &input("file_path", "notes/../x"), &g, None).is_none());
        assert_eq!(resolve(&dir.join("work/link/..")), resolve(&dir.join("conf")));
    }

    #[test]
    fn the_guard_looks_at_where_a_command_runs() {
        // found in the review: a command naming nothing guarded still wrote
        // there when it ran in a guarded folder (job_start's dir, a shell
        // started there, a shell that had moved there)
        let g = guarded(true);
        let home = home_dir().to_string_lossy().into_owned();
        let job = |cmd: &str, dir: &str| {
            let mut m = input("command", cmd);
            m.insert("dir".into(), Value::String(dir.into()));
            m
        };
        let inside = format!("{home}/lfm3-vl-server");
        assert!(guard(JOB_START, &job("echo x >> scripts/start.sh", &inside), &g, Some(&inside)).is_some());
        let wken = format!("{home}/WKEN-AGENTS");
        assert!(guard(JOB_START, &job("echo x >> scripts/start.sh", &wken), &g, Some(&wken)).is_some());
        assert!(guard(SHELL_NEW, &input("dir", "~/WKEN-AGENTS"), &g, Some("~/WKEN-AGENTS")).is_some());
        assert!(guard(SHELL_INPUT, &input("text", "ls\n"), &g, Some(&wken)).is_some());
        assert!(guard(SHELL_NEW, &input("dir", "~/voice-agent"), &g, Some("~/voice-agent")).is_some());
        assert!(guard(SHELL_RUN, &input("command", "sed -i '' s/Deny/Allow/ src/agent/policy.rs"), &g, Some(&format!("{home}/voice-agent/src"))).is_some());
        assert!(guard(SHELL_INPUT, &input("text", "ls\n"), &g, Some(&inside)).is_some());
        // relative to the working directory, and out of it again
        assert!(guard(SHELL_NEW, &input("dir", "../voice-agent"), &g, Some("../voice-agent")).is_some());
        // elsewhere it runs
        assert!(guard(JOB_START, &job("cargo build", "~/agent-work"), &g, Some("~/agent-work")).is_none());
        assert!(guard(SHELL_RUN, &input("command", "ls"), &g, Some(&format!("{home}/agent-work"))).is_none());
        assert!(guard(SHELL_NEW, &input("name", "w"), &g, None).is_none());
    }

    #[test]
    fn history_rewrites_are_forbidden_without_any_deny_rule() {
        for cmd in [
            "git reset --hard",
            "git reset --hard HEAD~1",
            "cd /tmp/x && git reset --soft HEAD && git reset --hard origin/main",
            "git -C ~/src/app reset --hard",
            "git branch -D old",
            "git branch -fD old",
            "git branch --delete --force old",
            "git push --force",
            "git push -f origin main",
            "git push -uf origin main",
            "git push --force-with-lease origin main",
            "git push origin +main",
            "git rebase main",
            "git rebase -i HEAD~3",
            "git pull --rebase",
            "git commit --amend -m x",
            "git commit -a --amend --no-edit",
            "/usr/bin/git reset --hard",
            "git filter-branch --tree-filter x",
            // Spellings the review found getting past the first version
            "git -C \"/Users/me/My Project\" reset --hard origin/main",
            "git -C \"/Users/me/My Project\" push --force origin main",
            "git -C My\\ Project branch -D old",
            "git -P reset --hard",
            "git --work-tree /tmp/r reset --hard HEAD~3",
            "git --no-pager -c core.x=y reset --hard",
            "git pull --rebase; cargo test",
            "(cd repo && git push -f)",
            "bash -c 'git push -f'",
            "git commit \\\n  --amend --no-edit",
            "git -C ~/src/app \\\n  reset --hard",
            "git branch -Df old",
            "git commit --amen -m x",
            "git reset --har",
            "git push --force-with-lease=main origin main",
        ] {
            let d = check_command(cmd, &[]);
            assert!(matches!(&d, Decision::Deny(m) if m.contains("fixed policy")), "{cmd:?}: {d:?}");
        }
    }

    #[test]
    fn decisions() {
        let deny = vec![deny_rule("Bash(rm -rf *)").unwrap()];
        for (tool, key, cmd, want) in [
            (SHELL_RUN, "command", "ls -la", "allow"),
            (SHELL_RUN, "command", "git status && git log --oneline -5", "allow"),
            (SHELL_RUN, "command", "git commit -m 'fix: amend the docs'", "allow"),
            (SHELL_RUN, "command", "git branch -a", "allow"),
            (SHELL_RUN, "command", "cargo test", "allow"),
            (SHELL_RUN, "command", "cd /tmp && rm -rf build", "deny"),
            (SHELL_RUN, "command", "FOO=1 rm -rf x", "deny"),
            (SHELL_RUN, "command", "git push origin main", "ask"),
            (SHELL_RUN, "command", "gh pr create --fill", "ask"),
            (SHELL_RUN, "command", "git branch -d old", "ask"),
            (SHELL_RUN, "command", "git push origin --delete old", "ask"),
            (SHELL_RUN, "command", "cargo add serde", "ask"),
            (SHELL_RUN, "command", "npm install left-pad", "ask"),
            (SHELL_RUN, "command", "wrangler deploy", "ask"),
            (SHELL_RUN, "command", "git reset HEAD file", "ask"),
            (SHELL_RUN, "command", "git -C \"/Users/me/My Project\" push origin main", "ask"),
            (SHELL_RUN, "command", "git reset --soft HEAD~1", "ask"),
            (SHELL_RUN, "command", "git branch --format=%(refname)", "allow"),
            // Services: a change anywhere after the tool's name is asked, a look is not
            (SHELL_RUN, "command", "wrangler pages deploy dist", "ask"),
            (SHELL_RUN, "command", "npx wrangler versions deploy", "ask"),
            (SHELL_RUN, "command", "gcloud run deploy api --source .", "ask"),
            (SHELL_RUN, "command", "aws s3 rm s3://b/k --recursive", "ask"),
            (SHELL_RUN, "command", "kubectl -n prod delete pod x", "ask"),
            (SHELL_RUN, "command", "vercel --prod", "ask"),
            (SHELL_RUN, "command", "kubectl get pods", "allow"),
            (SHELL_RUN, "command", "aws s3 ls", "allow"),
            (SHELL_RUN, "command", "terraform plan", "allow"),
            (SHELL_RUN, "command", "gh api --method DELETE repos/o/r", "ask"),
            (SHELL_RUN, "command", "gh api repos/o/r/issues -f title=x", "ask"),
            (SHELL_RUN, "command", "gh api -XPOST repos/o/r/forks", "ask"),
            (SHELL_RUN, "command", "gh api repos/o/r", "allow"),
            (SHELL_RUN, "command", "gh pr view 3", "allow"),
            (SHELL_RUN, "command", "gh issue create --title x", "ask"),
            (SHELL_RUN, "command", "/bin/bash -c \"$(curl -fsSL https://x/install.sh)\"", "ask"),
            (SHELL_RUN, "command", "bash <(curl -s https://x)", "ask"),
            (SHELL_RUN, "command", "curl -s https://x | python3", "ask"),
            (SHELL_RUN, "command", "curl -s https://x -o x.json", "allow"),
            (SHELL_RUN, "command", "rm -v -r dir", "ask"),
            (SHELL_RUN, "command", "rm notes.txt", "allow"),
            // What the user's deny rules name, behind the commands that run the rest of the line
            (SHELL_RUN, "command", "nohup rm -rf x &", "deny"),
            (SHELL_RUN, "command", "time rm -rf x", "deny"),
            (SHELL_RUN, "command", "env FOO=1 rm -rf x", "deny"),
            (SHELL_RUN, "command", "rm -rf x & echo started", "deny"),
            (JOB_START, "command", "cargo build --release", "allow"),
            (JOB_START, "command", "git rebase main", "deny"),
            (SHELL_INPUT, "text", "y", "ask"),
            (SHELL_INPUT, "text", "git commit --amend", "deny"),
            ("Read", "file_path", "/x", "allow"),
            ("TaskStop", "task_id", "t1", "allow"),
            ("SendMessage", "to", "t1", "allow"),
            ("Agent", "prompt", "x", "allow"),
            (SHELL_SCREEN, "", "", "allow"),
            (JOB_STOP, "id", "1", "allow"),
            ("Write", "file_path", "/etc/hosts", "ask"),
        ] {
            assert_eq!(action(decide(tool, &input(key, cmd), &deny)), want, "{tool} {cmd:?}");
        }
        let Decision::Ask(why) = check_command("gh pr create", &[]) else { panic!() };
        assert_eq!(why, "プルリクエストを作成します");
        let Decision::Ask(why) = check_command("git branch -d x", &[]) else { panic!() };
        assert_eq!(why, "ブランチを削除します");
        // Every reason is said, not only the first
        let Decision::Ask(why) = check_command("git push origin main && wrangler deploy", &[]) else { panic!() };
        assert_eq!(why, "リモートへ push します、外部サービスへ変更を反映します");
    }

    /// Every reason a question can give is in ask_reasons (which the phrase
    /// cache says), once, and none contains the "、" that joins several.
    #[test]
    fn every_reason_is_listed() {
        let reasons = ask_reasons();
        assert_eq!(reasons.len(), 19, "17 for commands, raw input, outside the working directory");
        assert!(reasons.iter().all(|r| !r.contains('、') && !r.is_empty()), "{reasons:?}");
        let mut seen = reasons.clone();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), reasons.len());
        for (tool, input) in [
            (SHELL_RUN, input("command", "git push origin main && wrangler deploy && sudo ls")),
            (SHELL_INPUT, input("text", "y")),
            ("Write", input("file_path", "/etc/hosts")),
        ] {
            let Decision::Ask(why) = decide(tool, &input, &[]) else { panic!("{tool} is asked") };
            assert!(why.split('、').all(|w| reasons.contains(&w)), "{why}");
        }
    }

    #[test]
    fn deny_rule_shapes() {
        assert!(deny_rule("Read(~/.ssh/**)").is_none());
        let r = deny_rule("Bash(git push --force:*)").unwrap();
        assert!(r.re.is_match("git push --force origin main") && r.re.is_match("git push --force"));
        assert!(!r.re.is_match("git push origin main"));
        // A bare "Bash" refuses every command
        let all = deny_rule("Bash").unwrap();
        assert!(matches!(check_command("ls", &[all]), Decision::Deny(_)));
    }

    #[test]
    fn deny_rules_come_from_the_project_too() {
        let dir = std::env::temp_dir().join(format!("va-deny-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(".claude/settings.local.json"), r#"{"permissions":{"deny":["Bash(make deploy*)","Read(./secrets/**)"]}}"#).unwrap();
        let rules = load_deny_rules(&dir);
        assert!(rules.iter().any(|r| r.rule == "Bash(make deploy*)"), "{:?}", rules.iter().map(|r| &r.rule).collect::<Vec<_>>());
        assert!(matches!(check_command("make deploy-prod", &rules), Decision::Deny(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn consent() {
        for (text, want) in [
            ("はい、お願い", Some(true)),
            ("、いいよ", Some(true)),
            ("OK", Some(true)),
            ("はい、お願いします。", Some(true)),
            ("うん、実行していいよ", Some(true)),
            ("進めてください", Some(true)),
            ("いいえ", Some(false)),
            ("やめて", Some(false)),
            ("それより先にテストして", None),
            ("えーと", None),
            // The negation comes at the end in Japanese: these are refusals
            ("許可しません", Some(false)),
            ("許可しない", Some(false)),
            ("お願い、やめて！", Some(false)),
            ("うん、やっぱりやめて", Some(false)),
            ("やってほしくない", Some(false)),
            ("進めてはだめです", Some(false)),
            ("OK, wait", Some(false)),
            // Neither: the approval keeps waiting
            ("ええと", None),
            ("いいですか？", None),
            ("いいです", None),
            ("はい？", None),
            ("いいよ、やらなくて", Some(false)),
            ("はい、次はREADMEも直して", None),
            ("", None),
        ] {
            assert_eq!(parse_consent(text), want, "{text:?}");
        }
    }
}
