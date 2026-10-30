//! Checking a signed request (lfm3-vl-server docs/native-app.md §14.5): the
//! key is one of the paired iPhone's and fits the endpoint, the time is
//! within 120 s of ours, the nonce is new, a POST names this serve's run,
//! and the signature is good.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::crypto::{hex, sha256, unbase64, verify_p256};

/// How far the iPhone's clock may be from ours.
pub const WINDOW_MS: i64 = 120_000;
/// Nonces are kept this long: an older request is refused by its time.
const NONCE_KEEP_MS: i64 = 2 * WINDOW_MS;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Everything but allowing: no Face ID
    Device,
    /// Allowing and unpairing: Face ID or the passcode each time
    Approval,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    NotPaired,
    Unsigned,
    KeyUnknown,
    WrongKey,
    Stale,
    Replayed,
    BadSignature,
    StaleRun,
}

impl Refusal {
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::NotPaired => "not_paired",
            Refusal::Unsigned => "unsigned",
            Refusal::KeyUnknown => "key_unknown",
            Refusal::WrongKey => "wrong_key",
            Refusal::Stale => "stale",
            Refusal::Replayed => "replayed",
            Refusal::BadSignature => "bad_signature",
            Refusal::StaleRun => "stale_run",
        }
    }

    pub fn status(&self) -> u16 {
        if *self == Refusal::StaleRun { 409 } else { 401 }
    }
}

/// What a request carries to be checked.
pub struct Signed<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub key: &'a str,
    pub time: &'a str,
    pub nonce: &'a str,
    pub run: &'a str,
    pub signature: &'a str,
    pub body: &'a [u8],
}

/// The text that is signed: VA1, the method, the path with its query, the
/// time, the nonce, the run (empty for GET) and the body's SHA-256.
pub fn string_to_sign(method: &str, path: &str, time: &str, nonce: &str, run: &str, body: &[u8]) -> String {
    format!("VA1\n{method}\n{path}\n{time}\n{nonce}\n{run}\n{}", hex(&sha256(body)))
}

/// The paired keys: a key's number to its role and public part.
pub type Keys<'a> = dyn Fn(&str) -> Option<(Role, Vec<u8>)> + 'a;

#[derive(Default)]
pub struct Verifier {
    /// Nonces seen, with when (unix ms)
    nonces: Mutex<HashMap<String, i64>>,
}

impl Verifier {
    /// Checks req against the paired keys (keys: id -> role and public key).
    /// allowed: the roles the endpoint takes. run: this serve's run, which a
    /// POST has to name. Gives the key's id and role.
    pub fn check(&self, req: &Signed, keys: &Keys, paired: bool, allowed: &[Role], run: &str, now_ms: i64) -> Result<(String, Role), Refusal> {
        if !paired {
            return Err(Refusal::NotPaired);
        }
        let post = req.method == "POST";
        if req.key.is_empty() || req.time.is_empty() || req.signature.is_empty() || (post && req.run.is_empty()) {
            return Err(Refusal::Unsigned);
        }
        if req.nonce.len() != 32 || !req.nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Refusal::Unsigned);
        }
        let (role, public) = keys(req.key).ok_or(Refusal::KeyUnknown)?;
        if !allowed.contains(&role) {
            return Err(Refusal::WrongKey);
        }
        let time: i64 = req.time.parse().map_err(|_| Refusal::Stale)?;
        if (now_ms - time).abs() > WINDOW_MS {
            return Err(Refusal::Stale);
        }
        let signature = unbase64(req.signature).ok_or(Refusal::BadSignature)?;
        let run_signed = if post { req.run } else { "" };
        let text = string_to_sign(req.method, req.path, req.time, req.nonce, run_signed, req.body);
        if !verify_p256(&public, text.as_bytes(), &signature) {
            return Err(Refusal::BadSignature);
        }
        // Only a signed request takes a nonce (junk cannot fill the table)
        {
            let mut seen = self.nonces.lock().unwrap_or_else(|e| e.into_inner());
            seen.retain(|_, at| now_ms - *at <= NONCE_KEEP_MS);
            if seen.contains_key(req.nonce) {
                return Err(Refusal::Replayed);
            }
            seen.insert(req.nonce.to_string(), now_ms);
        }
        if post && req.run != run {
            return Err(Refusal::StaleRun);
        }
        Ok((req.key.to_string(), role))
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use crate::serve::crypto::base64;
    use crate::serve::crypto::testing::TestKey;

    /// The headers a request signed with key would carry.
    pub struct Headers {
        pub key: String,
        pub time: String,
        pub nonce: String,
        pub run: String,
        pub signature: String,
    }

    pub fn sign(key: &TestKey, method: &str, path: &str, run: &str, body: &[u8], time_ms: i64) -> Headers {
        let nonce: String = (0..32).map(|_| format!("{:x}", rand_nibble())).collect();
        let time = time_ms.to_string();
        let run_signed = if method == "POST" { run } else { "" };
        let text = string_to_sign(method, path, &time, &nonce, run_signed, body);
        Headers { key: key.id(), time, nonce, run: run.to_string(), signature: base64(&key.sign(text.as_bytes())) }
    }

    fn rand_nibble() -> u8 {
        use std::io::Read;
        let mut b = [0u8; 1];
        let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
        b[0] & 15
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Headers, sign};
    use super::*;
    use crate::serve::crypto::testing::TestKey;

    const NOW: i64 = 1_800_000_000_000;

    struct Pair {
        device: TestKey,
        approval: TestKey,
    }

    impl Pair {
        fn new() -> Pair {
            Pair { device: TestKey::new(), approval: TestKey::new() }
        }

        fn keys(&self) -> impl Fn(&str) -> Option<(Role, Vec<u8>)> + '_ {
            move |id| {
                if id == self.device.id() {
                    Some((Role::Device, self.device.public()))
                } else if id == self.approval.id() {
                    Some((Role::Approval, self.approval.public()))
                } else {
                    None
                }
            }
        }
    }

    fn check(v: &Verifier, p: &Pair, method: &str, path: &str, h: &Headers, body: &[u8], allowed: &[Role]) -> Result<(String, Role), Refusal> {
        let req = Signed { method, path, key: &h.key, time: &h.time, nonce: &h.nonce, run: &h.run, signature: &h.signature, body };
        v.check(&req, &p.keys(), true, allowed, "run1", NOW)
    }

    #[test]
    fn a_signed_request_passes_once() {
        let (v, p) = (Verifier::default(), Pair::new());
        let h = sign(&p.device, "POST", "/agent/sessions", "run1", b"{}", NOW);
        assert_eq!(check(&v, &p, "POST", "/agent/sessions", &h, b"{}", &[Role::Device]), Ok((p.device.id(), Role::Device)));
        // the same request again
        assert_eq!(check(&v, &p, "POST", "/agent/sessions", &h, b"{}", &[Role::Device]), Err(Refusal::Replayed));
    }

    #[test]
    fn every_signed_part_counts() {
        let (v, p) = (Verifier::default(), Pair::new());
        let h = sign(&p.device, "POST", "/agent/sessions/a/messages", "run1", b"{\"text\":\"x\"}", NOW);
        let d = &[Role::Device];
        assert_eq!(check(&v, &p, "POST", "/agent/sessions/b/messages", &h, b"{\"text\":\"x\"}", d), Err(Refusal::BadSignature));
        assert_eq!(check(&v, &p, "POST", "/agent/sessions/a/messages", &h, b"{\"text\":\"y\"}", d), Err(Refusal::BadSignature));
        assert_eq!(check(&v, &p, "GET", "/agent/sessions/a/messages", &h, b"{\"text\":\"x\"}", d), Err(Refusal::BadSignature));
        let other_time = Headers { time: (NOW + 1).to_string(), ..sign(&p.device, "POST", "/agent/sessions/a/messages", "run1", b"{\"text\":\"x\"}", NOW) };
        assert_eq!(check(&v, &p, "POST", "/agent/sessions/a/messages", &other_time, b"{\"text\":\"x\"}", d), Err(Refusal::BadSignature));
    }

    #[test]
    fn the_time_has_to_be_within_two_minutes() {
        let (v, p) = (Verifier::default(), Pair::new());
        let d = &[Role::Device];
        let ok = sign(&p.device, "GET", "/agent/state", "", b"", NOW - WINDOW_MS);
        assert!(check(&v, &p, "GET", "/agent/state", &ok, b"", d).is_ok());
        let late = sign(&p.device, "GET", "/agent/state", "", b"", NOW - WINDOW_MS - 1);
        assert_eq!(check(&v, &p, "GET", "/agent/state", &late, b"", d), Err(Refusal::Stale));
        let early = sign(&p.device, "GET", "/agent/state", "", b"", NOW + WINDOW_MS + 1);
        assert_eq!(check(&v, &p, "GET", "/agent/state", &early, b"", d), Err(Refusal::Stale));
    }

    #[test]
    fn a_post_has_to_name_this_run() {
        // found in the design review: a restart starts the nonces and the
        // approval numbers over, so an allow signed for the run before could
        // land on another approval
        let (v, p) = (Verifier::default(), Pair::new());
        let h = sign(&p.approval, "POST", "/agent/approvals/ap1", "run0", b"{}", NOW);
        assert_eq!(check(&v, &p, "POST", "/agent/approvals/ap1", &h, b"{}", &[Role::Approval]), Err(Refusal::StaleRun));
        let unsigned_run = Headers { run: String::new(), ..sign(&p.approval, "POST", "/agent/approvals/ap1", "", b"{}", NOW) };
        assert_eq!(check(&v, &p, "POST", "/agent/approvals/ap1", &unsigned_run, b"{}", &[Role::Approval]), Err(Refusal::Unsigned));
    }

    #[test]
    fn the_key_has_to_fit_the_endpoint() {
        let (v, p) = (Verifier::default(), Pair::new());
        let h = sign(&p.device, "POST", "/agent/approvals/ap1", "run1", b"{}", NOW);
        assert_eq!(check(&v, &p, "POST", "/agent/approvals/ap1", &h, b"{}", &[Role::Approval]), Err(Refusal::WrongKey));
        let stranger = TestKey::new();
        let h = sign(&stranger, "GET", "/agent/state", "", b"", NOW);
        assert_eq!(check(&v, &p, "GET", "/agent/state", &h, b"", &[Role::Device]), Err(Refusal::KeyUnknown));
    }

    #[test]
    fn a_bad_signature_takes_no_nonce() {
        let (v, p) = (Verifier::default(), Pair::new());
        let good = sign(&p.device, "GET", "/agent/state", "", b"", NOW);
        let forged = Headers {
            signature: sign(&TestKey::new(), "GET", "/agent/state", "", b"", NOW).signature,
            nonce: good.nonce.clone(),
            ..sign(&p.device, "GET", "/agent/state", "", b"", NOW)
        };
        assert_eq!(check(&v, &p, "GET", "/agent/state", &forged, b"", &[Role::Device]), Err(Refusal::BadSignature));
        assert!(check(&v, &p, "GET", "/agent/state", &good, b"", &[Role::Device]).is_ok());
    }

    #[test]
    fn unpaired_and_unsigned() {
        let v = Verifier::default();
        let p = Pair::new();
        let h = sign(&p.device, "GET", "/agent/state", "", b"", NOW);
        let req = Signed { method: "GET", path: "/agent/state", key: &h.key, time: &h.time, nonce: &h.nonce, run: "", signature: &h.signature, body: b"" };
        assert_eq!(v.check(&req, &p.keys(), false, &[Role::Device], "run1", NOW), Err(Refusal::NotPaired));
        let bare = Signed { method: "GET", path: "/agent/state", key: "", time: "", nonce: "", run: "", signature: "", body: b"" };
        assert_eq!(v.check(&bare, &p.keys(), true, &[Role::Device], "run1", NOW), Err(Refusal::Unsigned));
        let short_nonce = Signed { nonce: "abc", ..req };
        assert_eq!(v.check(&short_nonce, &p.keys(), true, &[Role::Device], "run1", NOW), Err(Refusal::Unsigned));
    }

    #[test]
    fn old_nonces_are_let_go() {
        let (v, p) = (Verifier::default(), Pair::new());
        for i in 0..5 {
            let h = sign(&p.device, "GET", "/agent/state", "", b"", NOW);
            let req = Signed { method: "GET", path: "/agent/state", key: &h.key, time: &h.time, nonce: &h.nonce, run: "", signature: &h.signature, body: b"" };
            assert!(v.check(&req, &p.keys(), true, &[Role::Device], "run1", NOW + i).is_ok());
        }
        assert_eq!(v.nonces.lock().unwrap().len(), 5);
        let h = sign(&p.device, "GET", "/agent/state", "", b"", NOW + NONCE_KEEP_MS + 10);
        let req = Signed { method: "GET", path: "/agent/state", key: &h.key, time: &h.time, nonce: &h.nonce, run: "", signature: &h.signature, body: b"" };
        assert!(v.check(&req, &p.keys(), true, &[Role::Device], "run1", NOW + NONCE_KEEP_MS + 10).is_ok());
        assert_eq!(v.nonces.lock().unwrap().len(), 1);
    }
}
