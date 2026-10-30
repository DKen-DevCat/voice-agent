//! Conversations and Claude's sessions (lfm3-vl-server docs/native-app.md
//! §6.2, §14.7). The iPhone holds a conversation's number, which serve makes;
//! Claude's session id is learned after the first reply and changes when it
//! is resumed, so a conversation keeps every id it has had (`sessions`, in the
//! order they were learned; native-app.md §18.3.1 S3) and the latest as
//! `session`: Claude's transcript of an old id is reached only through the
//! list. It lives in the config directory (the caches can be purged, and the
//! resume with them).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::devices::write_atomic;
use crate::log::log;
use crate::util::unix_millis;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Entry {
    /// Claude's session id, empty until the first reply
    session: String,
    /// Every id the conversation has had, oldest first, the latest last.
    /// Missing in a file written before S3 (version 1): `load` fills it from
    /// `session`.
    #[serde(default)]
    sessions: Vec<String>,
    created: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Map {
    version: u32,
    conversations: BTreeMap<String, Entry>,
}

pub struct Sessions {
    path: PathBuf,
    st: Mutex<Map>,
}

impl Sessions {
    pub fn load(path: &Path) -> Sessions {
        let mut map = match std::fs::read(path) {
            Ok(data) => serde_json::from_slice::<Map>(&data).unwrap_or_else(|e| {
                log(&format!("serve: {}: {e} (starting empty)", path.display()));
                Map::default()
            }),
            Err(_) => Map::default(),
        };
        // The old shape held the latest id only: it is the first of the list
        for e in map.conversations.values_mut() {
            if e.sessions.is_empty() && !e.session.is_empty() {
                e.sessions.push(e.session.clone());
            }
        }
        Sessions { path: path.to_path_buf(), st: Mutex::new(map) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Map> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A new conversation (no session until its first reply).
    pub fn create(&self) -> String {
        let id = new_id();
        let mut m = self.lock();
        m.conversations.insert(id.clone(), Entry { session: String::new(), sessions: Vec::new(), created: unix_millis() });
        self.save(&mut m);
        id
    }

    pub fn exists(&self, id: &str) -> bool {
        self.lock().conversations.contains_key(id)
    }

    /// Claude's session for the conversation, when it has one.
    pub fn session(&self, id: &str) -> Option<String> {
        self.lock().conversations.get(id).map(|e| e.session.clone()).filter(|s| !s.is_empty())
    }

    /// Every Claude session the conversation has had, oldest first.
    #[allow(dead_code)] // read by forget (S2) and the tests
    pub fn all_sessions(&self, id: &str) -> Vec<String> {
        self.lock().conversations.get(id).map(|e| e.sessions.clone()).unwrap_or_default()
    }

    /// Learns a session of the conversation: it becomes the latest, and an id
    /// already in the list is not added again.
    pub fn set_session(&self, id: &str, session: &str) {
        if session.is_empty() {
            return;
        }
        let mut m = self.lock();
        let Some(e) = m.conversations.get_mut(id) else { return };
        if e.session == session && e.sessions.last().map(String::as_str) == Some(session) {
            return;
        }
        e.session = session.to_string();
        e.sessions.retain(|s| s != session);
        e.sessions.push(session.to_string());
        self.save(&mut m);
    }

    fn save(&self, m: &mut Map) {
        m.version = 2;
        let res = serde_json::to_vec_pretty(m).map_err(|e| e.to_string()).and_then(|d| write_atomic(&self.path, &d).map_err(|e| e.to_string()));
        if let Err(e) = res {
            log(&format!("serve: {}: {e}", self.path.display()));
        }
    }
}

/// A random UUID (version 4).
pub fn new_id() -> String {
    let mut b = crate::util::random_bytes::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("va-sessions-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("sessions.json")
    }

    #[test]
    fn a_conversation_learns_its_session_and_keeps_it() {
        let path = temp();
        let s = Sessions::load(&path);
        let id = s.create();
        assert!(s.exists(&id));
        assert_eq!(s.session(&id), None);
        s.set_session(&id, "s1");
        // resuming gives a new id; the map follows
        s.set_session(&id, "s2");
        let again = Sessions::load(&path);
        assert_eq!(again.session(&id).as_deref(), Some("s2"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn every_session_is_kept_in_order_without_repeats() {
        let path = temp();
        let s = Sessions::load(&path);
        let id = s.create();
        for sid in ["s1", "s2", "s2", "s3"] {
            s.set_session(&id, sid);
        }
        assert_eq!(s.all_sessions(&id), ["s1", "s2", "s3"]);
        // moving back to an old id makes it the latest and keeps one copy
        s.set_session(&id, "s1");
        assert_eq!(s.all_sessions(&id), ["s2", "s3", "s1"]);
        assert_eq!(s.session(&id).as_deref(), Some("s1"));
        let again = Sessions::load(&path);
        assert_eq!(again.all_sessions(&id), ["s2", "s3", "s1"]);
        assert_eq!(again.session(&id).as_deref(), Some("s1"));
        // an empty id is learned as nothing
        again.set_session(&id, "");
        assert_eq!(again.all_sessions(&id).len(), 3);
    }

    #[test]
    fn a_file_of_the_old_shape_loads_and_grows() {
        let path = temp();
        // version 1: one id per conversation, no list (what serve wrote before S3)
        std::fs::write(
            &path,
            r#"{"version":1,"conversations":{"c-with":{"session":"old1","created":5},"c-new":{"session":"","created":6}}}"#,
        )
        .unwrap();
        let s = Sessions::load(&path);
        assert_eq!(s.session("c-with").as_deref(), Some("old1"));
        assert_eq!(s.all_sessions("c-with"), ["old1"]);
        assert!(s.exists("c-new"));
        assert!(s.all_sessions("c-new").is_empty());
        // the next id is added after the old one, and the file is rewritten as version 2
        s.set_session("c-with", "old2");
        let again = Sessions::load(&path);
        assert_eq!(again.all_sessions("c-with"), ["old1", "old2"]);
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["version"], 2);
        assert_eq!(raw["conversations"]["c-with"]["session"], "old2");
        // a version 2 file read again does not double the list
        assert_eq!(Sessions::load(&path).all_sessions("c-with"), ["old1", "old2"]);
    }

    #[test]
    fn unknown_conversations_take_nothing() {
        let s = Sessions::load(&temp());
        s.set_session("nope", "s1");
        assert!(!s.exists("nope"));
        assert_eq!(s.session("nope"), None);
    }

    #[test]
    fn ids_are_uuids_and_differ() {
        let (a, b) = (new_id(), new_id());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        assert!("89ab".contains(&a[19..20]));
    }
}
