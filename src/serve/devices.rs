//! The paired iPhone (lfm3-vl-server docs/native-app.md §14.6): its two
//! public keys, the history of pairing and unpairing, and the one pending
//! request. Pairing is taken only while nothing is paired; the code the
//! iPhone shows is typed on the Mac (`voice-agent pair <code>`).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::auth::Role;
use super::crypto::{key_id, unbase64, verify_p256};
use crate::util::unix_millis;

/// How long a request waits for its code to be typed on the Mac.
pub const REQUEST_FOR: Duration = Duration::from_secs(300);
/// Wrong codes a request takes before it is dropped.
pub const MAX_MISSES: u32 = 5;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Key {
    pub id: String,
    pub role: Role,
    /// The public part, X9.63 in base64
    pub key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Device {
    pub name: String,
    pub keys: Vec<Key>,
    pub at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct History {
    pub at: i64,
    /// paired / unpaired
    pub action: String,
    /// iphone / mac
    pub from: String,
    pub name: String,
    pub keys: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    version: u32,
    device: Option<Device>,
    history: Vec<History>,
}

/// What the iPhone sends to pair (POST /agent/pair).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairBody {
    pub code: String,
    pub name: String,
    pub device_key: String,
    pub approval_key: String,
    pub device_signature: String,
    pub approval_signature: String,
}

/// The text both keys sign to pair.
pub fn pair_text(code: &str, device_key: &str, approval_key: &str) -> String {
    format!("VA1-pair\n{code}\n{device_key}\n{approval_key}")
}

struct Request {
    code: String,
    name: String,
    device: String,
    approval: String,
    at: Instant,
    misses: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairError {
    /// Something is paired already (unpair first)
    Paired,
    /// devices.json could not be read: refused until `voice-agent unpair`
    Broken,
    Malformed(&'static str),
    BadSignature,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ConfirmError {
    NoRequest,
    Expired,
    /// A wrong code: how many tries are left
    Wrong(u32),
    /// The last try was wrong: the request is gone
    Dropped,
    Paired,
    Broken,
    Write(String),
}

struct State {
    reg: Registry,
    broken: bool,
    request: Option<Request>,
}

pub struct Devices {
    path: PathBuf,
    st: Mutex<State>,
}

impl Devices {
    /// Reads path (devices.json). A file that cannot be read is broken: no
    /// key is trusted and pairing is refused until `voice-agent unpair`, so
    /// that spoiling the file cannot reopen pairing.
    pub fn load(path: &Path) -> Devices {
        let (reg, broken) = match std::fs::read(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Registry::default(), false),
            Err(_) => (Registry::default(), true),
            Ok(data) => match serde_json::from_slice::<Registry>(&data) {
                Ok(r) => (r, false),
                Err(_) => (Registry::default(), true),
            },
        };
        Devices { path: path.to_path_buf(), st: Mutex::new(State { reg, broken, request: None }) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn paired(&self) -> bool {
        let st = self.lock();
        !st.broken && st.reg.device.is_some()
    }

    /// The role and public key of a paired key.
    pub fn lookup(&self, id: &str) -> Option<(Role, Vec<u8>)> {
        let st = self.lock();
        if st.broken {
            return None;
        }
        let k = st.reg.device.as_ref()?.keys.iter().find(|k| k.id == id)?;
        Some((k.role, unbase64(&k.key)?))
    }

    /// Takes a request to pair: both keys have to have signed it. It
    /// replaces the one before.
    pub fn request(&self, b: &PairBody) -> Result<(), PairError> {
        if b.code.len() != 6 || !b.code.bytes().all(|c| c.is_ascii_digit()) {
            return Err(PairError::Malformed("code"));
        }
        let name = b.name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            return Err(PairError::Malformed("name"));
        }
        let (Some(device), Some(approval)) = (unbase64(&b.device_key), unbase64(&b.approval_key)) else {
            return Err(PairError::Malformed("key"));
        };
        if device == approval {
            return Err(PairError::Malformed("key"));
        }
        let (Some(ds), Some(as_)) = (unbase64(&b.device_signature), unbase64(&b.approval_signature)) else {
            return Err(PairError::BadSignature);
        };
        let text = pair_text(&b.code, &b.device_key, &b.approval_key);
        if !verify_p256(&device, text.as_bytes(), &ds) || !verify_p256(&approval, text.as_bytes(), &as_) {
            return Err(PairError::BadSignature);
        }
        let mut st = self.lock();
        if st.broken {
            return Err(PairError::Broken);
        }
        if st.reg.device.is_some() {
            return Err(PairError::Paired);
        }
        st.request = Some(Request {
            code: b.code.clone(),
            name: name.to_string(),
            device: b.device_key.clone(),
            approval: b.approval_key.clone(),
            at: Instant::now(),
            misses: 0,
        });
        Ok(())
    }

    /// The code typed on the Mac: when it is the request's, its keys are
    /// paired.
    pub fn confirm(&self, code: &str) -> Result<Device, ConfirmError> {
        let mut st = self.lock();
        if st.broken {
            return Err(ConfirmError::Broken);
        }
        if st.reg.device.is_some() {
            return Err(ConfirmError::Paired);
        }
        let Some(r) = st.request.as_mut() else { return Err(ConfirmError::NoRequest) };
        if r.at.elapsed() > REQUEST_FOR {
            st.request = None;
            return Err(ConfirmError::Expired);
        }
        if r.code != code.trim() {
            r.misses += 1;
            if r.misses >= MAX_MISSES {
                st.request = None;
                return Err(ConfirmError::Dropped);
            }
            return Err(ConfirmError::Wrong(MAX_MISSES - r.misses));
        }
        let r = st.request.take().expect("checked above");
        let key = |role: Role, b64: &str| Key { id: key_id(&unbase64(b64).unwrap_or_default()), role, key: b64.to_string() };
        let device = Device { name: r.name, keys: vec![key(Role::Device, &r.device), key(Role::Approval, &r.approval)], at: unix_millis() };
        st.reg.history.push(History {
            at: device.at,
            action: "paired".into(),
            from: "mac".into(),
            name: device.name.clone(),
            keys: device.keys.iter().map(|k| k.id.clone()).collect(),
        });
        st.reg.device = Some(device.clone());
        save(&self.path, &mut st.reg).map_err(ConfirmError::Write)?;
        Ok(device)
    }

    /// Forgets the paired iPhone (from: iphone / mac). A broken file is
    /// replaced by an empty one: this is how the Mac recovers from it.
    pub fn unpair(&self, from: &str) -> Result<(), String> {
        let mut st = self.lock();
        let gone = st.reg.device.take();
        st.reg.history.push(History {
            at: unix_millis(),
            action: "unpaired".into(),
            from: from.into(),
            name: gone.as_ref().map(|d| d.name.clone()).unwrap_or_default(),
            keys: gone.map(|d| d.keys.into_iter().map(|k| k.id).collect()).unwrap_or_default(),
        });
        st.broken = false;
        st.request = None;
        save(&self.path, &mut st.reg)
    }

    /// What GET /agent/devices shows: the paired keys and the history, for
    /// the iPhone to see that only its own keys are there.
    pub fn view(&self) -> Value {
        let st = self.lock();
        json!({"device": st.reg.device, "history": st.reg.history})
    }
}

/// Writes the registry in one step (a temporary file renamed over it), only
/// the user can read it.
fn save(path: &Path, reg: &mut Registry) -> Result<(), String> {
    reg.version = 1;
    let data = serde_json::to_vec_pretty(reg).map_err(|e| e.to_string())?;
    write_atomic(path, &data).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        crate::util::private_dir(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    crate::util::write_private(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use crate::serve::crypto::base64;
    use crate::serve::crypto::testing::TestKey;

    /// A request to pair as the iPhone makes it.
    pub fn body(device: &TestKey, approval: &TestKey, code: &str) -> PairBody {
        let (dk, ak) = (base64(&device.public()), base64(&approval.public()));
        let text = pair_text(code, &dk, &ak);
        PairBody {
            code: code.into(),
            name: "iPhone".into(),
            device_signature: base64(&device.sign(text.as_bytes())),
            approval_signature: base64(&approval.sign(text.as_bytes())),
            device_key: dk,
            approval_key: ak,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::body;
    use super::*;
    use crate::serve::crypto::testing::TestKey;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("va-devices-{}", crate::serve::sessions::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("devices.json")
    }

    #[test]
    fn the_code_typed_on_the_mac_pairs_both_keys() {
        let path = temp();
        let d = Devices::load(&path);
        let (dk, ak) = (TestKey::new(), TestKey::new());
        assert!(!d.paired());
        d.request(&body(&dk, &ak, "123456")).unwrap();
        let dev = d.confirm("123456").unwrap();
        assert!(d.paired());
        assert_eq!(d.lookup(&dk.id()), Some((Role::Device, dk.public())));
        assert_eq!(d.lookup(&ak.id()), Some((Role::Approval, ak.public())));
        assert_eq!(dev.keys.len(), 2);
        // it is kept, only the user can read it, and a restart reads it back
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let again = Devices::load(&path);
        assert_eq!(again.lookup(&ak.id()), Some((Role::Approval, ak.public())));
        assert_eq!(again.view()["history"][0]["action"], "paired");
        assert_eq!(again.view()["history"][0]["from"], "mac");
    }

    #[test]
    fn pairing_is_taken_only_while_nothing_is_paired() {
        let d = Devices::load(&temp());
        let (dk, ak) = (TestKey::new(), TestKey::new());
        d.request(&body(&dk, &ak, "111111")).unwrap();
        d.confirm("111111").unwrap();
        let (x, y) = (TestKey::new(), TestKey::new());
        assert_eq!(d.request(&body(&x, &y, "222222")), Err(PairError::Paired));
        assert_eq!(d.confirm("222222"), Err(ConfirmError::Paired));
        assert_eq!(d.lookup(&x.id()), None);
    }

    #[test]
    fn five_wrong_codes_drop_the_request() {
        let d = Devices::load(&temp());
        d.request(&body(&TestKey::new(), &TestKey::new(), "123456")).unwrap();
        for left in (1..MAX_MISSES).rev() {
            assert_eq!(d.confirm("000000"), Err(ConfirmError::Wrong(left)));
        }
        assert_eq!(d.confirm("000000"), Err(ConfirmError::Dropped));
        assert_eq!(d.confirm("123456"), Err(ConfirmError::NoRequest));
        assert!(!d.paired());
    }

    #[test]
    fn a_new_request_replaces_the_one_before() {
        let d = Devices::load(&temp());
        d.request(&body(&TestKey::new(), &TestKey::new(), "111111")).unwrap();
        let (dk, ak) = (TestKey::new(), TestKey::new());
        d.request(&body(&dk, &ak, "222222")).unwrap();
        assert_eq!(d.confirm("111111"), Err(ConfirmError::Wrong(MAX_MISSES - 1)));
        d.confirm("222222").unwrap();
        assert!(d.lookup(&dk.id()).is_some());
    }

    #[test]
    fn both_keys_have_to_sign_the_request() {
        let d = Devices::load(&temp());
        let (dk, ak) = (TestKey::new(), TestKey::new());
        let mut b = body(&dk, &ak, "123456");
        b.approval_signature = body(&dk, &TestKey::new(), "123456").approval_signature;
        assert_eq!(d.request(&b), Err(PairError::BadSignature));
        // the code is signed too
        let mut b = body(&dk, &ak, "123456");
        b.code = "654321".into();
        assert_eq!(d.request(&b), Err(PairError::BadSignature));
        // one key for both roles is not two keys
        assert_eq!(d.request(&body(&dk, &dk, "123456")), Err(PairError::Malformed("key")));
        let mut b = body(&dk, &ak, "12345");
        b.code = "12345".into();
        assert_eq!(d.request(&b), Err(PairError::Malformed("code")));
    }

    #[test]
    fn unpairing_is_kept_in_the_history_and_reopens_pairing() {
        let path = temp();
        let d = Devices::load(&path);
        let (dk, ak) = (TestKey::new(), TestKey::new());
        d.request(&body(&dk, &ak, "123456")).unwrap();
        d.confirm("123456").unwrap();
        d.unpair("iphone").unwrap();
        assert!(!d.paired());
        assert_eq!(d.lookup(&dk.id()), None);
        let h = Devices::load(&path).view()["history"].clone();
        assert_eq!(h[1]["action"], "unpaired");
        assert_eq!(h[1]["from"], "iphone");
        assert_eq!(h[1]["keys"].as_array().unwrap().len(), 2);
        d.request(&body(&TestKey::new(), &TestKey::new(), "654321")).unwrap();
    }

    #[test]
    fn a_spoiled_file_trusts_nothing_until_the_mac_unpairs() {
        let path = temp();
        std::fs::write(&path, b"{not json").unwrap();
        let d = Devices::load(&path);
        assert!(!d.paired());
        assert_eq!(d.request(&body(&TestKey::new(), &TestKey::new(), "123456")), Err(PairError::Broken));
        assert_eq!(d.confirm("123456"), Err(ConfirmError::Broken));
        d.unpair("mac").unwrap();
        d.request(&body(&TestKey::new(), &TestKey::new(), "123456")).unwrap();
    }
}
