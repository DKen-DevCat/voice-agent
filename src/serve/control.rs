//! What the user types on the Mac (`voice-agent pair <code>`, `voice-agent
//! unpair`) reaches the running serve through a Unix socket in the runtime
//! directory (0700), only the user can open (lfm3-vl-server
//! docs/native-app.md §14.3). The agent could reach it too: policy refuses
//! the commands, and the iPhone notices a pairing it did not make (§14.6).

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::devices::{ConfirmError, Devices};
use crate::log::log;

pub fn listen(path: &Path, devices: Arc<Devices>) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let devices = devices.clone();
            std::thread::spawn(move || answer(conn, &devices));
        }
    });
    Ok(())
}

fn answer(mut conn: UnixStream, devices: &Devices) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    let mut line = String::new();
    let _ = BufReader::new((&conn).take(256)).read_line(&mut line);
    let reply = reply(line.trim(), devices);
    let _ = conn.write_all(reply.as_bytes());
}

/// One command, one line back: "ok ...", or what went wrong.
pub fn reply(line: &str, devices: &Devices) -> String {
    let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));
    match cmd {
        "pair" => match devices.confirm(arg) {
            Ok(d) => {
                let ids: Vec<String> = d.keys.iter().map(|k| k.id.clone()).collect();
                log(&format!("serve: paired {} ({})", d.name, ids.join(", ")));
                format!("ok {}\t{}", ids.join(" "), d.name)
            }
            Err(ConfirmError::Wrong(left)) => format!("wrong {left}"),
            Err(ConfirmError::Dropped) => "dropped".into(),
            Err(ConfirmError::NoRequest) => "none".into(),
            Err(ConfirmError::Expired) => "expired".into(),
            Err(ConfirmError::Paired) => "paired".into(),
            Err(ConfirmError::Broken) => "broken".into(),
            Err(ConfirmError::Write(e)) => format!("error {e}"),
        },
        "unpair" => match devices.unpair("mac") {
            Ok(()) => {
                log("serve: unpaired from the Mac");
                "ok".into()
            }
            Err(e) => format!("error {e}"),
        },
        _ => "error unknown command".into(),
    }
}

/// Sends a command to the running serve; Err when there is none.
pub fn send(path: &Path, line: &str) -> Result<String, String> {
    let mut conn = UnixStream::connect(path).map_err(|e| e.to_string())?;
    let _ = conn.set_read_timeout(Some(Duration::from_secs(10)));
    conn.write_all(format!("{line}\n").as_bytes()).map_err(|e| e.to_string())?;
    let mut reply = String::new();
    conn.read_to_string(&mut reply).map_err(|e| e.to_string())?;
    Ok(reply)
}

/// What `voice-agent pair` prints for the reply (and its exit code).
pub fn pair_message(reply: &str) -> (String, i32) {
    let (word, rest) = reply.split_once(' ').unwrap_or((reply, ""));
    match word {
        "ok" => {
            let (ids, name) = rest.split_once('\t').unwrap_or((rest, ""));
            (format!("{name} と対にしました（鍵 {ids}）。iPhone に同じ鍵の番号が出ていることを確かめてください。"), 0)
        }
        "wrong" => (format!("番号が違います（あと {rest} 回）。"), 1),
        "dropped" => ("番号を 5 回まちがえたので、申し込みを消しました。iPhone で最初からやり直してください。".into(), 1),
        "none" => ("申し込みがありません。iPhone の「Mac と対にする」から始めてください。".into(), 1),
        "expired" => ("申し込みは 5 分で切れました。iPhone でやり直してください。".into(), 1),
        "paired" => ("もう iPhone と対にしています。替えるなら、先に voice-agent unpair を打ってください。".into(), 1),
        "broken" => ("devices.json が読めません。voice-agent unpair で作り直してから、やり直してください。".into(), 1),
        _ => (format!("対にできませんでした: {rest}"), 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::crypto::testing::TestKey;
    use crate::serve::devices::testing::body;
    use crate::serve::sessions::new_id;

    fn temp() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("va-control-{}", &new_id()[..8]));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_mac_pairs_and_unpairs_through_the_socket() {
        let dir = temp();
        let devices = Arc::new(Devices::load(&dir.join("devices.json")));
        let sock = dir.join("control.sock");
        listen(&sock, devices.clone()).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777, 0o600);

        assert_eq!(send(&sock, "pair 123456").unwrap(), "none");
        let (dk, ak) = (TestKey::new(), TestKey::new());
        devices.request(&body(&dk, &ak, "123456")).unwrap();
        assert_eq!(send(&sock, "pair 000000").unwrap(), "wrong 4");
        let ok = send(&sock, "pair 123456").unwrap();
        assert_eq!(ok, format!("ok {} {}\tiPhone", dk.id(), ak.id()));
        assert!(pair_message(&ok).0.contains(&dk.id()));
        assert!(devices.paired());
        assert_eq!(send(&sock, "unpair").unwrap(), "ok");
        assert!(!devices.paired());
        assert_eq!(send(&sock, "rm -rf /").unwrap(), "error unknown command");
    }

    #[test]
    fn every_reply_has_a_message() {
        for (reply, code) in [("wrong 3", 1), ("dropped", 1), ("none", 1), ("expired", 1), ("paired", 1), ("broken", 1), ("error x", 1)] {
            let (msg, c) = pair_message(reply);
            assert!(!msg.is_empty());
            assert_eq!(c, code, "{reply}");
        }
        assert_eq!(pair_message("wrong 3").0, "番号が違います（あと 3 回）。");
    }
}
