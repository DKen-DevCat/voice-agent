//! Numbered events for the iPhone (lfm3-vl-server docs/native-app.md §14.8).
//! Every event gets the serve's run and a number that rises from 1; the last
//! KEEP are held so that a reconnect with Last-Event-ID gets what it missed,
//! and a reset when that is no longer there. One client at a time: a new one
//! replaces the one before (there is one iPhone).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};

use serde_json::{Value, json};

pub const KEEP: usize = 2000;

pub type Frame = Arc<str>;

struct State {
    seq: u64,
    ring: VecDeque<(u64, Frame)>,
    client: Option<mpsc::Sender<Frame>>,
}

pub struct Hub {
    run: String,
    st: Mutex<State>,
}

/// A client's stream: what it missed first (a reset when that is gone),
/// then what comes.
pub struct Subscription {
    pub backlog: Vec<Frame>,
    pub rx: mpsc::Receiver<Frame>,
}

/// "<run>:<seq>", as Last-Event-ID and last_event carry it.
pub fn parse_event_id(s: &str) -> Option<(&str, u64)> {
    let (run, seq) = s.trim().rsplit_once(':')?;
    if run.is_empty() {
        return None;
    }
    Some((run, seq.parse().ok()?))
}

fn frame(run: &str, seq: u64, v: &Value) -> Frame {
    format!("id: {run}:{seq}\ndata: {v}\n\n").into()
}

impl Hub {
    pub fn new(run: &str) -> Hub {
        Hub { run: run.to_string(), st: Mutex::new(State { seq: 0, ring: VecDeque::new(), client: None }) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn run(&self) -> &str {
        &self.run
    }

    /// The number of the last event.
    pub fn last(&self) -> u64 {
        self.lock().seq
    }

    /// Numbers v (an object) and sends it to the client: gives its number.
    pub fn publish(&self, mut v: Value) -> u64 {
        let mut st = self.lock();
        st.seq += 1;
        let seq = st.seq;
        v["run"] = json!(self.run);
        v["seq"] = json!(seq);
        let f = frame(&self.run, seq, &v);
        st.ring.push_back((seq, f.clone()));
        while st.ring.len() > KEEP {
            st.ring.pop_front();
        }
        if let Some(tx) = &st.client
            && tx.send(f).is_err()
        {
            st.client = None;
        }
        seq
    }

    /// A client (re)connects with the last event it has, if any. Without one
    /// it gets only what comes (it read GET /agent/state first). Taken under
    /// the lock, so that nothing falls between the backlog and the stream.
    pub fn subscribe(&self, last_event_id: Option<&str>) -> Subscription {
        let (tx, rx) = mpsc::channel();
        let mut st = self.lock();
        let backlog = match last_event_id.map(parse_event_id) {
            None => Vec::new(),
            Some(Some((run, last))) if run == self.run && last <= st.seq && last + 1 >= st.ring.front().map_or(st.seq + 1, |f| f.0) => {
                st.ring.iter().filter(|(seq, _)| *seq > last).map(|(_, f)| f.clone()).collect()
            }
            // Another run, or older than what is kept: the client starts over
            Some(_) => vec![frame(&self.run, st.seq, &json!({"kind": "reset", "run": self.run, "seq": st.seq}))],
        };
        // The client before is let go: its receiver sees the end
        st.client = Some(tx);
        Subscription { backlog, rx }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seqs(frames: &[Frame]) -> Vec<String> {
        frames.iter().map(|f| f.lines().next().unwrap_or_default().to_string()).collect()
    }

    #[test]
    fn events_are_numbered_and_streamed() {
        let hub = Hub::new("r1");
        let sub = hub.subscribe(None);
        assert_eq!(hub.publish(json!({"kind": "assistant", "text": "は"})), 1);
        assert_eq!(hub.publish(json!({"kind": "assistant_end"})), 2);
        let first = sub.rx.recv().unwrap();
        assert!(first.starts_with("id: r1:1\ndata: "));
        let v: Value = serde_json::from_str(first.lines().nth(1).unwrap().strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!((v["run"].as_str(), v["seq"].as_u64(), v["text"].as_str()), (Some("r1"), Some(1), Some("は")));
        assert!(sub.rx.recv().unwrap().starts_with("id: r1:2\n"));
    }

    #[test]
    fn a_reconnect_gets_what_it_missed() {
        let hub = Hub::new("r1");
        for _ in 0..5 {
            hub.publish(json!({"kind": "tool"}));
        }
        let sub = hub.subscribe(Some("r1:3"));
        assert_eq!(seqs(&sub.backlog), ["id: r1:4", "id: r1:5"]);
        assert!(hub.subscribe(Some("r1:5")).backlog.is_empty());
    }

    #[test]
    fn another_run_or_a_lost_gap_resets() {
        let hub = Hub::new("r2");
        hub.publish(json!({"kind": "tool"}));
        let other = hub.subscribe(Some("r1:1500"));
        assert_eq!(other.backlog.len(), 1);
        assert!(other.backlog[0].contains("\"kind\":\"reset\""));
        for _ in 0..KEEP + 10 {
            hub.publish(json!({"kind": "tool"}));
        }
        // event 2 is no longer kept
        let gone = hub.subscribe(Some("r2:1"));
        assert!(gone.backlog[0].contains("reset"));
        // a number from the future is not believed either
        assert!(hub.subscribe(Some("r2:999999")).backlog[0].contains("reset"));
        // the oldest kept one still is
        let oldest_missing = hub.last() - KEEP as u64;
        assert_eq!(hub.subscribe(Some(&format!("r2:{oldest_missing}"))).backlog.len(), KEEP);
    }

    #[test]
    fn a_new_client_replaces_the_one_before() {
        let hub = Hub::new("r1");
        let old = hub.subscribe(None);
        let new = hub.subscribe(None);
        hub.publish(json!({"kind": "tool"}));
        assert!(new.rx.recv().is_ok());
        assert!(old.rx.recv().is_err());
    }

    #[test]
    fn event_ids_parse() {
        assert_eq!(parse_event_id("abc:12"), Some(("abc", 12)));
        assert_eq!(parse_event_id("a:b:7"), Some(("a:b", 7)));
        assert_eq!(parse_event_id(":7"), None);
        assert_eq!(parse_event_id("abc"), None);
        assert_eq!(parse_event_id("abc:x"), None);
    }
}
