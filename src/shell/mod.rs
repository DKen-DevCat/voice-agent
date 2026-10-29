//! The shells the AI works in. Nobody sees them: zsh runs in a pty of its
//! own, the OSC 633 bridge tells when a command ends and how, a vt100 model
//! keeps its screen, and everything it prints goes to a log file. Long work
//! runs as jobs (plain child processes with a log).

pub mod bridge;
pub mod jobs;
pub mod pty;
pub mod registry;
pub mod zsh;

/// The end of text that a tool hands back: at most max_lines lines and
/// max_bytes bytes. Returns it and how many lines were left out.
pub fn tail(text: &str, max_lines: usize, max_bytes: usize) -> (String, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let mut start = lines.len().saturating_sub(max_lines);
    let mut bytes: usize = lines[start..].iter().map(|l| l.len() + 1).sum();
    while bytes > max_bytes && start < lines.len() {
        bytes -= lines[start].len() + 1;
        start += 1;
    }
    let mut out = lines[start..].join("\n");
    if start == lines.len() && !lines.is_empty() {
        // One line longer than max_bytes: keep its end
        let last = lines[lines.len() - 1];
        let mut i = last.len().saturating_sub(max_bytes);
        while !last.is_char_boundary(i) {
            i += 1;
        }
        out = last[i..].to_string();
        start = lines.len() - 1;
    }
    (out, start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tails() {
        let text: String = (1..=500).map(|i| format!("line {i}\n")).collect();
        let (t, cut) = tail(&text, 400, 30_000);
        assert_eq!((t.lines().count(), cut), (400, 100));
        assert!(t.starts_with("line 101\n") && t.ends_with("line 500"));
        let (t, cut) = tail(&text, 400, 100);
        assert!(t.len() <= 100 && t.ends_with("line 500") && cut > 400);
        assert_eq!(tail("short", 400, 30_000), ("short".to_string(), 0));
        let (t, _) = tail(&"あ".repeat(100), 400, 10);
        assert!(t.len() <= 10 && t.chars().all(|c| c == 'あ'));
        assert_eq!(tail("", 400, 30_000), (String::new(), 0));
    }
}
