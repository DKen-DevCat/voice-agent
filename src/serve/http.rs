//! The HTTP API on 127.0.0.1 (lfm3-vl-server docs/native-app.md §14.4). The
//! wrapper adds `Authorization: Bearer <VA_SERVE_KEY>`; the iPhone signs every
//! request but the pairing one. One thread a request; the event stream keeps
//! its thread for as long as it is open.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use serde_json::{Value, json};
use tiny_http::{Header, Request, Response};

use super::Serve;
use super::auth::{Refusal, Role, Signed};
use super::devices::{PairBody, PairError};
use crate::util::unix_millis;

/// The biggest body taken (a message's text; pictures come later).
const MAX_BODY: u64 = 1 << 20;
/// A comment line this often keeps the stream open through the wrapper (§5).
const HEARTBEAT: Duration = Duration::from_secs(15);

pub fn serve_forever(s: Arc<Serve>, server: tiny_http::Server) {
    for req in server.incoming_requests() {
        let s = s.clone();
        std::thread::spawn(move || handle(&s, req));
    }
}

/// A JSON reply and its status.
pub type Reply = (u16, Value);

pub fn error(status: u16, code: &str) -> Reply {
    (status, json!({"error": {"code": code}}))
}

#[derive(Debug, PartialEq, Eq)]
enum Route {
    Devices,
    Unpair,
    NewSession,
    Message(String),
    Approve(String),
    Stop,
    Events,
    State,
}

fn route(method: &str, path: &str) -> Option<Route> {
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    Some(match (method, parts.as_slice()) {
        ("GET", ["agent", "devices"]) => Route::Devices,
        ("POST", ["agent", "unpair"]) => Route::Unpair,
        ("POST", ["agent", "sessions"]) => Route::NewSession,
        ("POST", ["agent", "sessions", id, "messages"]) if !id.is_empty() => Route::Message(id.to_string()),
        ("POST", ["agent", "approvals", id]) if !id.is_empty() => Route::Approve(id.to_string()),
        ("POST", ["agent", "stop"]) => Route::Stop,
        ("GET", ["agent", "events"]) => Route::Events,
        ("GET", ["agent", "state"]) => Route::State,
        _ => return None,
    })
}

fn header<'a>(req: &'a Request, name: &'static str) -> &'a str {
    req.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str()).unwrap_or_default()
}

fn handle(s: &Serve, mut req: Request) {
    let method = req.method().to_string();
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or_default().to_string();
    let bearer = header(&req, "Authorization").strip_prefix("Bearer ").unwrap_or_default();
    if !crate::util::constant_time_eq(bearer.as_bytes(), s.key.as_bytes()) {
        return respond(req, error(401, "unauthorized"));
    }
    let mut body = Vec::new();
    if req.as_reader().take(MAX_BODY + 1).read_to_end(&mut body).is_err() {
        return respond(req, error(400, "bad_body"));
    }
    if body.len() as u64 > MAX_BODY {
        return respond(req, error(413, "too_large"));
    }
    if method == "POST" && path == "/agent/pair" {
        return respond(req, pair(s, &body));
    }
    let Some(route) = route(&method, &path) else { return respond(req, error(404, "not_found")) };
    let allowed: &[Role] = match route {
        // a no may come from either key; an allow is checked below
        Route::Approve(_) => &[Role::Device, Role::Approval],
        Route::Unpair => &[Role::Approval],
        _ => &[Role::Device],
    };
    let signed = Signed {
        method: &method,
        path: &url,
        key: header(&req, "X-VA-Key"),
        time: header(&req, "X-VA-Time"),
        nonce: header(&req, "X-VA-Nonce"),
        run: header(&req, "X-VA-Run"),
        signature: header(&req, "X-VA-Signature"),
        body: &body,
    };
    let checked = s.verifier.check(&signed, &|id| s.devices.lookup(id), s.devices.paired(), allowed, s.hub.run(), unix_millis());
    let (key, role) = match checked {
        Ok(k) => k,
        Err(r) => return respond(req, refusal(&r)),
    };
    let reply = match route {
        Route::Events => {
            let last = header(&req, "Last-Event-ID").to_string();
            return events(s, req, (!last.is_empty()).then_some(last));
        }
        Route::State => (200, s.state()),
        Route::Devices => (200, s.devices.view()),
        Route::Unpair => match s.devices.unpair("iphone") {
            Ok(()) => {
                s.hub.close_client();
                (204, Value::Null)
            }
            Err(_) => error(500, "write_failed"),
        },
        Route::NewSession => (200, json!({"id": s.sessions.create()})),
        Route::Message(id) => match serde_json::from_slice::<Value>(&body) {
            Ok(v) => {
                let text = v["text"].as_str().unwrap_or_default();
                let via = v["via"].as_str().filter(|v| *v == "text" || *v == "voice").unwrap_or("text");
                // true is the app's: its text is never a command (S6)
                let raw = v["raw"].as_bool().unwrap_or(false);
                match s.message(&id, text, via, raw, v["last_event"].as_str()) {
                    Ok(turn) => (200, json!({"turn": turn})),
                    Err(e) => e,
                }
            }
            Err(_) => error(400, "bad_json"),
        },
        Route::Approve(id) => match serde_json::from_slice::<Value>(&body) {
            Ok(v) => match (v["allow"].as_bool(), v["digest"].as_str()) {
                (Some(allow), Some(digest)) => match s.answer(&id, allow, digest, &key, role) {
                    Ok(()) => (204, Value::Null),
                    Err(e) => e,
                },
                _ => error(400, "bad_json"),
            },
            Err(_) => error(400, "bad_json"),
        },
        Route::Stop => match serde_json::from_slice::<Value>(&body).ok().and_then(|v| v["scope"].as_str().map(String::from)).as_deref() {
            Some("all") => {
                s.stop_all();
                (204, Value::Null)
            }
            _ => error(400, "bad_scope"),
        },
    };
    respond(req, reply)
}

fn refusal(r: &Refusal) -> Reply {
    error(r.status(), r.code())
}

fn pair(s: &Serve, body: &[u8]) -> Reply {
    let Ok(b) = serde_json::from_slice::<PairBody>(body) else { return error(400, "bad_json") };
    match s.devices.request(&b) {
        Ok(()) => (202, Value::Null),
        Err(PairError::Paired) => error(409, "already_paired"),
        Err(PairError::Broken) => error(409, "broken"),
        Err(PairError::Malformed(what)) => error(400, &format!("bad_{what}")),
        Err(PairError::BadSignature) => error(401, "bad_signature"),
    }
}

fn respond(req: Request, (status, body): Reply) {
    let res = if status == 204 {
        Response::from_data(Vec::new()).with_status_code(204)
    } else {
        Response::from_data(body.to_string().into_bytes())
            .with_status_code(status)
            .with_header(Header::from_bytes("Content-Type", "application/json").expect("a valid header"))
    };
    let _ = req.respond(res);
}

/// The event stream: what the client missed, then what comes, with a
/// comment line every HEARTBEAT. Written straight to the socket: tiny_http's
/// own responses hold small writes back.
fn events(s: &Serve, req: Request, last: Option<String>) {
    let sub = s.hub.subscribe(last.as_deref());
    let mut w = req.into_writer();
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    let mut send = |text: &str| w.write_all(text.as_bytes()).and_then(|_| w.flush());
    if send(head).is_err() {
        return;
    }
    for f in &sub.backlog {
        if send(f).is_err() {
            return;
        }
    }
    loop {
        let res = match sub.rx.recv_timeout(HEARTBEAT) {
            Ok(f) => send(&f),
            Err(RecvTimeoutError::Timeout) => send(": ping\n\n"),
            // another client took over
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if res.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        assert_eq!(route("GET", "/agent/devices"), Some(Route::Devices));
        assert_eq!(route("POST", "/agent/sessions/abc/messages"), Some(Route::Message("abc".into())));
        assert_eq!(route("POST", "/agent/approvals/ap3"), Some(Route::Approve("ap3".into())));
        assert_eq!(route("GET", "/agent/approvals/ap3"), None);
        assert_eq!(route("POST", "/agent/sessions//messages"), None);
        assert_eq!(route("POST", "/agent/stop/now"), None);
        assert_eq!(route("GET", "/v1/models"), None);
    }
}
