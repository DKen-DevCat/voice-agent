//! Conversations and Claude's sessions (lfm3-vl-server docs/native-app.md
//! §6.2, §14.7). The iPhone holds a conversation's number, which serve makes;
//! Claude's session id is learned after the first reply and changes when it
//! is resumed, so the map is written again with every reply. It lives in the
//! config directory (the caches can be purged, and the resume with them).

use std::collections::BTreeMap;
use std::io::Read;
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
        let map = match std::fs::read(path) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_else(|e| {
                log(&format!("serve: {}: {e} (starting empty)", path.display()));
                Map::default()
            }),
            Err(_) => Map::default(),
        };
        Sessions { path: path.to_path_buf(), st: Mutex::new(map) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Map> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A new conversation (no session until its first reply).
    pub fn create(&self) -> String {
        let id = new_id();
        let mut m = self.lock();
        m.conversations.insert(id.clone(), Entry { session: String::new(), created: unix_millis() });
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

    pub fn set_session(&self, id: &str, session: &str) {
        let mut m = self.lock();
        let Some(e) = m.conversations.get_mut(id) else { return };
        if e.session == session {
            return;
        }
        e.session = session.to_string();
        self.save(&mut m);
    }

    fn save(&self, m: &mut Map) {
        m.version = 1;
        let res = serde_json::to_vec_pretty(m).map_err(|e| e.to_string()).and_then(|d| write_atomic(&self.path, &d).map_err(|e| e.to_string()));
        if let Err(e) = res {
            log(&format!("serve: {}: {e}", self.path.display()));
        }
    }
}

/// A random UUID (version 4).
pub fn new_id() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
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
