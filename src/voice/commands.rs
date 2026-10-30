//! What the user can say to voice-agent itself instead of to Claude. The
//! whole utterance has to be the command, so 「サーバーを止めて」 is still a
//! request for Claude, while 「止めて。」 stops the speech.

use std::sync::OnceLock;

use regex::Regex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// 「止めて」: stop speaking and Ctrl-C what runs in the shells
    Stop,
    /// 「生成もストップ」: that, and Claude's reply is interrupted and the jobs
    /// are stopped too (as Esc)
    StopAll,
    /// 「マイクを切って」: let go of the microphone (the m key brings it back)
    MicOff,
}

/// The command text is, if it is one.
pub fn parse(text: &str) -> Option<Command> {
    static RES: OnceLock<[(Regex, Command); 3]> = OnceLock::new();
    let res = RES.get_or_init(|| {
        let please = "(?:ください|下さい)?";
        [
            // Whisper writes 生成 as 精製 now and then
            (
                format!("^(?:(?:生成|精製|せいせい)(?:も|を)?(?:ストップ|すとっぷ|止めて|とめて|止めろ|とめろ|やめて)(?:して)?|(?:全部|ぜんぶ)(?:ストップ|止めて|とめて)){please}$"),
                Command::StopAll,
            ),
            (format!("^(?:マイク|まいく)(?:を|は)?(?:切って|きって|オフにして|オフ|おふ|止めて|とめて|消して|けして){please}$"), Command::MicOff),
            (
                format!("^(?:止めて|とめて|止まって|とまって|止めろ|とめろ|やめて|ストップ|すとっぷ|stop|黙って|だまって|静かにして|しずかにして|静かに|しずかに)+{please}$"),
                Command::Stop,
            ),
        ]
        .map(|(re, cmd)| (Regex::new(&re).unwrap(), cmd))
    });
    let t = normalize(text);
    res.iter().find(|(re, _)| re.is_match(&t)).map(|(_, cmd)| *cmd)
}

/// The utterance without spaces and punctuation, ASCII in lower case.
fn normalize(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace() && !"、。，．,.!！?？…・「」『』\"'ー〜~".contains(*c)).flat_map(char::to_lowercase).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_whole_utterances() {
        for (text, want) in [
            ("止めて", Some(Command::Stop)),
            ("止めて。", Some(Command::Stop)),
            ("止めて止めて！", Some(Command::Stop)),
            ("ストップ", Some(Command::Stop)),
            (" Stop. ", Some(Command::Stop)),
            ("止めてください", Some(Command::Stop)),
            ("生成もストップ", Some(Command::StopAll)),
            ("生成も止めて。", Some(Command::StopAll)),
            ("精製もストップ", Some(Command::StopAll)),
            ("全部止めて", Some(Command::StopAll)),
            ("マイクを切って", Some(Command::MicOff)),
            ("マイク切って。", Some(Command::MicOff)),
            ("マイクをオフにしてください", Some(Command::MicOff)),
            // Requests for Claude
            ("サーバーを止めて", None),
            ("止めてもいいよ", None),
            ("ビルドを止めてから生成もストップして", None),
            ("マイクの設定を見て", None),
            ("", None),
        ] {
            assert_eq!(parse(text), want, "{text:?}");
        }
    }
}
