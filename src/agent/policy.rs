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

/// git with any global options before its subcommand (`git -C dir`, `git --git-dir=x`).
const GIT: &str = r"\bgit(?:\s+(?:-[cC]\s+\S+|--[\w-]+(?:=\S+)?))*\s+";
/// The rest of one simple command.
const REST: &str = r"[^;&|\n]*";

/// History rewrites: never run, whatever the user's deny rules say.
fn forbidden() -> &'static Vec<(Regex, &'static str)> {
    static RES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    RES.get_or_init(|| {
        [
            (format!(r"{GIT}reset\b{REST}--hard\b"), "git reset --hard"),
            (format!(r"{GIT}branch\b{REST}(\s-[A-Za-z]*D\b|\s--force\b|\s-f\b)"), "git branch -D"),
            (format!(r"{GIT}push\b{REST}(\s(-[A-Za-z]*f[A-Za-z]*|--force(-with-lease|-if-includes)?)(\s|=|$)|\s\+\S)"), "git push --force"),
            (format!(r"{GIT}rebase\b"), "git rebase"),
            (format!(r"{GIT}pull\b{REST}\s(--rebase|-r)(\s|$|=(true|merges|interactive|i)\b)"), "git pull --rebase"),
            (format!(r"{GIT}commit\b{REST}\s--amend\b"), "git commit --amend"),
            (format!(r"{GIT}(filter-branch|filter-repo)\b"), "git filter-branch"),
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
        [
            (r"\bgh\s+pr\s+(create|new)\b".to_string(), "プルリクエストを作成します"),
            (r"\bgh\s+(pr\s+(merge|close|edit|ready)|release|repo\s+(create|delete|edit|rename|archive))\b|\bgh\s+api\b.*-X\s*(POST|PUT|PATCH|DELETE)".into(), "GitHub に変更を反映します"),
            (format!(r"{GIT}branch\b{REST}\s(-[A-Za-z]*d\b|--delete\b)"), "ブランチを削除します"),
            (format!(r"{GIT}push\b{REST}(\s--delete\b|\s-d\b|\s:\S)"), "リモートのブランチを削除します"),
            (format!(r"{GIT}push\b"), "リモートへ push します"),
            (
                format!(r"{GIT}(reset|clean|restore|merge|cherry-pick|revert|stash\s+(drop|clear))\b|{GIT}checkout\s+(--\s|\.)"),
                "git の作業ツリーを書き換えます",
            ),
            (r"\b(npm|pnpm|yarn|bun)\s+(i|install|add|remove|rm|uninstall|publish|update|upgrade)\b".into(), "ネットワークから依存パッケージを追加・変更します"),
            (r"\b(pip3?|uv|poetry|brew|gem|cargo|go|bundle|composer)\s+(install|uninstall|remove|add|get|update|upgrade|publish)\b|\buv\s+pip\s+install\b".into(), "ネットワークから依存パッケージを追加・変更します"),
            (r"\b(curl|wget)\b[^|]*\|\s*(sudo\s+)?(ba|z)?sh\b".into(), "ダウンロードしたスクリプトを実行します"),
            (r"\b(wrangler|vercel|netlify|firebase|terraform|kubectl|fly|flyctl|heroku|supabase|aws|gcloud)\s+(deploy|publish|apply|destroy|delete|push|rm)\b".into(), "外部サービスへ変更を反映します"),
            (r"\bdocker\s+(push|rm|rmi|system\s+prune|volume\s+rm|compose\s+down\s+-v)\b".into(), "Docker のイメージやリソースを変更します"),
            (r"\brm\s+(-\w*[rRf]|--recursive|--force)".into(), "ファイルを再帰的または強制的に削除します"),
            (r"\bsudo\b".into(), "管理者権限で実行します"),
            (r"\b(shutdown|reboot|halt|launchctl|diskutil|mkfs|dd|csrutil|nvram)\b".into(), "システムに関わる操作です"),
            (r"\b(kill|killall|pkill)\b".into(), "プロセスを終了させます"),
            (r"\b(chmod|chown)\s+-R\b".into(), "権限を再帰的に変更します"),
            (r"\bcrontab\b".into(), "定期実行の設定を変更します"),
            (r">\s*/(etc|usr|System|Library|bin|sbin)/".into(), "システム領域のファイルに書き込みます"),
        ]
        .into_iter()
        .map(|(re, why)| (Regex::new(&re).unwrap(), why))
        .collect()
    })
}

pub struct DenyRule {
    pub rule: String,
    pub re: Regex,
}

/// The Bash(...) rules of permissions.deny in ~/.claude/settings.json.
pub fn load_deny_rules() -> Vec<DenyRule> {
    let Ok(data) = std::fs::read(home_dir().join(".claude").join("settings.json")) else { return Vec::new() };
    let Ok(v) = serde_json::from_slice::<Value>(&data) else { return Vec::new() };
    let deny = v.pointer("/permissions/deny").and_then(Value::as_array).cloned().unwrap_or_default();
    deny.iter().filter_map(Value::as_str).filter_map(deny_rule).collect()
}

fn deny_rule(r: &str) -> Option<DenyRule> {
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
/// VAR=value assignments.
fn segments(command: &str) -> Vec<String> {
    static RES: OnceLock<[Regex; 3]> = OnceLock::new();
    let [split, vars, group] =
        RES.get_or_init(|| [Regex::new("&&|\\|\\||;|\\||\n|\\$\\(|`").unwrap(), Regex::new(r"^(\w+=\S*\s+)+").unwrap(), Regex::new(r"^[({]\s*").unwrap()]);
    split
        .split(command)
        .map(|s| {
            let s = vars.replace(s.trim(), "");
            group.replace(&s, "").into_owned()
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

/// What happens to a shell command line.
pub fn check_command(command: &str, deny: &[DenyRule]) -> Decision {
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
    match risky().iter().find(|(re, _)| re.is_match(command)) {
        Some((_, why)) => Decision::Ask(why.to_string()),
        None => Decision::Allow,
    }
}

pub fn decide(tool: &str, input: &Map<String, Value>, deny: &[DenyRule]) -> Decision {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or_default();
    match tool {
        SHELL_RUN | JOB_START => check_command(s("command"), deny),
        // Raw keystrokes into a shell: always asked, unless they are forbidden anyway
        SHELL_INPUT => match check_command(s("text"), deny) {
            d @ Decision::Deny(_) => d,
            _ => Decision::Ask("シェルに直接入力します".to_string()),
        },
        _ if AUTO_ALLOW.contains(&tool) => Decision::Allow,
        // e.g. Edit/Write outside the working directory (acceptEdits covers the inside)
        _ => Decision::Ask("作業ディレクトリの外を変更する可能性があります".to_string()),
    }
}

/// Reads a spoken/typed reply to a pending approval: Some(yes), Some(no), or
/// None when it is neither (the approval then keeps waiting).
pub fn parse_consent(text: &str) -> Option<bool> {
    static RES: OnceLock<[Regex; 3]> = OnceLock::new();
    let [yes, no, trim] = RES.get_or_init(|| {
        [
            Regex::new(r"(?i)^(はい|うん|ええ|いいよ|いいです|いいね|オッケー|おけ|ok|okay|yes|yeah|お願い|どうぞ|許可|実行して|やって|進めて)").unwrap(),
            Regex::new(r"(?i)^(いいえ|いや|だめ|ダメ|やめ|ストップ|no|nope|キャンセル|待って|拒否|しないで)").unwrap(),
            Regex::new(r"^[\s、。,.!！?？]+").unwrap(),
        ]
    });
    let t = trim.replace(text, "");
    if no.is_match(&t) {
        Some(false)
    } else if yes.is_match(&t) {
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
    }

    #[test]
    fn deny_rule_shapes() {
        assert!(deny_rule("Read(~/.ssh/**)").is_none());
        let r = deny_rule("Bash(git push --force:*)").unwrap();
        assert!(r.re.is_match("git push --force origin main") && r.re.is_match("git push --force"));
        assert!(!r.re.is_match("git push origin main"));
    }

    #[test]
    fn consent() {
        for (text, want) in [
            ("はい、お願い", Some(true)),
            ("、いいよ", Some(true)),
            ("OK", Some(true)),
            ("いいえ", Some(false)),
            ("やめて", Some(false)),
            ("それより先にテストして", None),
            ("えーと", None),
        ] {
            assert_eq!(parse_consent(text), want, "{text:?}");
        }
    }
}
