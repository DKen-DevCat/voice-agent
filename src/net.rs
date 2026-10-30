//! The HTTP the app speaks as a client: the speech engine and the local
//! whisper server. Blocking calls; a status is not an error. It never goes
//! through a proxy, whatever the environment says: the recordings stay on
//! this machine.

use std::sync::OnceLock;
use std::time::Duration;

use ureq::Agent;

const MAX_BODY: u64 = 64 << 20;

fn agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| Agent::new_with_config(Agent::config_builder().http_status_as_error(false).proxy(None).user_agent("voice-agent").build()))
}

/// GET url: the status and the body.
pub fn get(url: &str, timeout: Duration) -> Result<(u16, Vec<u8>), String> {
    let resp = agent().get(url).config().timeout_global(Some(timeout)).build().call();
    read(resp)
}

/// POST body to url (without a content type when there is no body to name).
pub fn post(url: &str, content_type: Option<&str>, body: &[u8], timeout: Duration) -> Result<(u16, Vec<u8>), String> {
    let mut req = agent().post(url).config().timeout_global(Some(timeout)).build();
    if let Some(ct) = content_type {
        req = req.header("Content-Type", ct);
    }
    read(req.send(body))
}

fn read(resp: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<(u16, Vec<u8>), String> {
    let mut resp = resp.map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let body = resp.body_mut().with_config().limit(MAX_BODY).read_to_vec().map_err(|e| e.to_string())?;
    Ok((status, body))
}

/// Percent-encodes a query value.
pub fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
pub mod testing {
    //! A local HTTP server for the tests of the clients above.
    use std::sync::Arc;

    pub struct Request {
        pub method: String,
        pub url: String,
        pub body: Vec<u8>,
        pub content_type: String,
    }

    pub struct Server {
        pub url: String,
        server: Arc<tiny_http::Server>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.server.unblock();
        }
    }

    /// Serves handler's (status, body) answers on a port of its own.
    pub fn serve(handler: impl Fn(&Request) -> (u16, Vec<u8>) + Send + Sync + 'static) -> Server {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let url = format!("http://{}", server.server_addr());
        let s = server.clone();
        let handler = Arc::new(handler);
        std::thread::spawn(move || {
            for mut req in s.incoming_requests() {
                let handler = handler.clone();
                std::thread::spawn(move || {
                    let mut body = Vec::new();
                    let _ = req.as_reader().read_to_end(&mut body);
                    let content_type = req.headers().iter().find(|h| h.field.equiv("Content-Type")).map(|h| h.value.to_string()).unwrap_or_default();
                    let r = Request { method: req.method().to_string(), url: req.url().to_string(), body, content_type };
                    let (status, data) = handler(&r);
                    let _ = req.respond(tiny_http::Response::from_data(data).with_status_code(status));
                });
            }
        });
        Server { url, server }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape() {
        assert_eq!(query_escape("a b&c=日"), "a%20b%26c%3D%E6%97%A5");
    }

    #[test]
    fn get_and_post() {
        let srv = testing::serve(|r| match (r.method.as_str(), r.url.as_str()) {
            ("GET", "/ok") => (200, b"hello".to_vec()),
            ("POST", "/echo") => (201, [r.content_type.as_bytes(), b"|", &r.body[..]].concat()),
            _ => (404, b"nope".to_vec()),
        });
        let t = Duration::from_secs(5);
        assert_eq!(get(&format!("{}/ok", srv.url), t).unwrap(), (200, b"hello".to_vec()));
        assert_eq!(get(&format!("{}/missing", srv.url), t).unwrap(), (404, b"nope".to_vec()));
        assert_eq!(post(&format!("{}/echo", srv.url), Some("application/json"), b"{}", t).unwrap(), (201, b"application/json|{}".to_vec()));
        assert!(get("http://127.0.0.1:1/", t).is_err());
    }
}
