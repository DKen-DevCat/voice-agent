//! Who gets the speech engine, and when work nobody waits for may run.
//!
//! AivisSpeech cannot stop a synthesis once it has begun, and two at once
//! would share the machine (or wait inside the engine, out of our sight), so
//! whoever holds the engine makes everyone else wait. The engine goes to the
//! waiting request of the highest priority: speech the user is waiting to
//! hear, then the check that the engine is back, then a phrase for the
//! cache, then a warm-up. A request that holds the engine is never
//! preempted; that is why warm-ups are short (aivis.rs) and the cache is
//! built only while nothing goes on (phrases.rs).
//!
//! Work nobody waits for also yields to the rest of an 8 GB M1, where whisper,
//! AivisSpeech and Claude Code compete for memory and cores: no warm-up while
//! whisper transcribes or a subagent works, no cache building unless all is
//! quiet. Such work stands in line only while it may run, so a phrase for the
//! cache that waits for quiet neither gets the engine meanwhile nor keeps a
//! warm-up out.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use super::aivis::SynthError;
use super::tts::Cancel;

/// What the engine is wanted for, lowest priority first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Use {
    /// Gets the voice model resident ahead of a reply
    Warm,
    /// Makes a phrase for the cache
    Cache,
    /// Sees that the engine is back and warm, after which speech leaves `say`
    /// for it: speech waits for it, so whisper or a subagent at work does not
    /// keep it waiting
    Ready,
    /// Speech the user is waiting to hear
    Speech,
}

/// What else is going on, as the app tells it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Activity {
    /// whisper is transcribing
    pub transcribing: bool,
    /// a subagent is at work
    pub agents: bool,
    /// the user is talking, or a reply is on its way or being spoken
    pub busy: bool,
}

#[derive(Default)]
struct State {
    held: Option<Use>,
    waiting: [usize; 4], // by Use
}

pub struct Arbiter {
    st: Mutex<State>,
    cond: Condvar,
    /// Tells what else is going on (set once by the app; unset = nothing).
    pub activity: OnceLock<Box<dyn Fn() -> Activity + Send + Sync>>,
}

/// The engine, held until this is dropped.
pub struct Grant(Arc<Arbiter>);

impl Drop for Grant {
    fn drop(&mut self) {
        self.0.lock().held = None;
        self.0.cond.notify_all();
    }
}

impl Arbiter {
    pub fn new() -> Arc<Arbiter> {
        Arc::new(Arbiter { st: Mutex::default(), cond: Condvar::new(), activity: OnceLock::new() })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What else is going on now (nothing, until the app tells).
    pub fn now(&self) -> Activity {
        self.activity.get().map(|f| f()).unwrap_or_default()
    }

    /// Whether work of this kind may start now. Speech, and bringing the
    /// engine back for it, always may.
    pub fn may(&self, u: Use) -> bool {
        match u {
            Use::Speech | Use::Ready => true,
            Use::Warm => {
                let a = self.now();
                !a.transcribing && !a.agents
            }
            Use::Cache => {
                let a = self.now();
                !a.transcribing && !a.agents && !a.busy
            }
        }
    }

    /// Whether speech holds the engine or waits for it.
    pub fn speech_wanted(&self) -> bool {
        let st = self.lock();
        st.held == Some(Use::Speech) || st.waiting[Use::Speech as usize] > 0
    }

    #[cfg(test)]
    pub fn held(&self) -> Option<Use> {
        self.lock().held
    }

    #[cfg(test)]
    pub fn waiting(&self, u: Use) -> usize {
        self.lock().waiting[u as usize]
    }

    /// Takes the engine once nobody holds it, nobody of a higher priority
    /// waits for it and the work may run. A stopped request does not take it,
    /// nor does one whose time is up when the engine comes free.
    pub fn acquire(self: &Arc<Self>, u: Use, cancel: &Cancel, timeout: Duration) -> Result<Grant, SynthError> {
        let deadline = Instant::now() + timeout;
        let mut in_line = false;
        let res = loop {
            // Asked outside the lock: the app answers from behind locks of its own
            let may = self.may(u);
            let mut st = self.lock();
            if may != in_line {
                if may {
                    st.waiting[u as usize] += 1;
                } else {
                    st.waiting[u as usize] -= 1;
                }
                in_line = may;
            }
            let outranked = st.waiting[u as usize + 1..].iter().any(|&n| n > 0);
            let res = if cancel.is_cancelled() {
                Some(Err(SynthError::Cancelled))
            } else if Instant::now() >= deadline {
                Some(Err(SynthError::Timeout))
            } else if may && st.held.is_none() && !outranked {
                st.held = Some(u);
                Some(Ok(()))
            } else {
                None
            };
            if let Some(res) = res {
                if in_line {
                    st.waiting[u as usize] -= 1;
                }
                break res;
            }
            drop(self.cond.wait_timeout(st, Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner()));
        };
        match res {
            Ok(()) => Ok(Grant(self.clone())),
            Err(e) => {
                // One of a lower priority may go now
                self.cond.notify_all();
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Waits on a thread of its own for the engine and reports when it got
    /// it; holds it for hold.
    fn wait_for(arb: &Arc<Arbiter>, u: Use, hold: Duration, got: &mpsc::Sender<Use>) -> std::thread::JoinHandle<()> {
        let (arb, got) = (arb.clone(), got.clone());
        std::thread::spawn(move || {
            let g = arb.acquire(u, &Cancel::new(), Duration::from_secs(10)).unwrap();
            got.send(u).unwrap();
            std::thread::sleep(hold);
            drop(g);
        })
    }

    fn until_waiting(arb: &Arbiter, u: Use) {
        for _ in 0..500 {
            if arb.waiting(u) > 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("{u:?} never waited");
    }

    /// When the engine comes free, speech gets it before the cache and the
    /// cache before a warm-up, whatever order they asked in; what holds it is
    /// not preempted.
    #[test]
    fn the_engine_goes_by_priority() {
        let arb = Arbiter::new();
        let held = arb.acquire(Use::Warm, &Cancel::new(), Duration::from_secs(1)).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut threads = Vec::new();
        for u in [Use::Warm, Use::Cache, Use::Speech] {
            threads.push(wait_for(&arb, u, Duration::from_millis(20), &tx));
            until_waiting(&arb, u);
        }
        assert!(arb.speech_wanted());
        assert!(rx.try_recv().is_err(), "nothing preempts what holds the engine");
        drop(held);
        let order: Vec<Use> = (0..3).map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap()).collect();
        assert_eq!(order, [Use::Speech, Use::Cache, Use::Warm]);
        for t in threads {
            t.join().unwrap();
        }
        assert!(!arb.speech_wanted());
    }

    /// A request that gives up (stopped, or out of time) leaves the line, and
    /// one below it is not kept waiting for it.
    #[test]
    fn giving_up_leaves_the_line() {
        let arb = Arbiter::new();
        let held = arb.acquire(Use::Speech, &Cancel::new(), Duration::from_secs(1)).unwrap();
        assert_eq!(arb.acquire(Use::Speech, &Cancel::new(), Duration::from_millis(60)).err(), Some(SynthError::Timeout));
        let cancel = Cancel::new();
        cancel.cancel();
        assert_eq!(arb.acquire(Use::Speech, &cancel, Duration::from_secs(1)).err(), Some(SynthError::Cancelled));
        assert_eq!(arb.lock().waiting, [0, 0, 0, 0]);
        drop(held);
        assert_eq!(
            arb.acquire(Use::Speech, &cancel, Duration::from_secs(1)).err(),
            Some(SynthError::Cancelled),
            "a stopped request does not take a free engine"
        );
        assert_eq!(arb.held(), None);
        let t0 = Instant::now();
        drop(arb.acquire(Use::Warm, &Cancel::new(), Duration::from_secs(1)).unwrap());
        assert!(t0.elapsed() < Duration::from_millis(100), "free again");
    }

    /// Work that may not run now stays out of the line: a phrase for the
    /// cache that waits for quiet does not get the engine meanwhile (not even
    /// when it comes free), nor does it keep a warm-up out.
    #[test]
    fn only_work_that_may_run_stands_in_line() {
        let arb = Arbiter::new();
        let now = Arc::new(Mutex::new(Activity { busy: true, ..Activity::default() }));
        let n = now.clone();
        let _ = arb.activity.set(Box::new(move || *n.lock().unwrap()));
        let held = arb.acquire(Use::Speech, &Cancel::new(), Duration::from_secs(1)).unwrap();
        let (tx, rx) = mpsc::channel();
        let cache = wait_for(&arb, Use::Cache, Duration::ZERO, &tx);
        std::thread::sleep(Duration::from_millis(150));
        drop(held);
        std::thread::sleep(Duration::from_millis(150));
        assert!(rx.try_recv().is_err(), "the user is talking");
        assert_eq!(arb.waiting(Use::Cache), 0);
        drop(arb.acquire(Use::Warm, &Cancel::new(), Duration::from_millis(500)).expect("a warm-up may go"));
        *now.lock().unwrap() = Activity::default();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), Use::Cache);
        cache.join().unwrap();
    }

    #[test]
    fn what_may_run_when() {
        let arb = Arbiter::new();
        assert!(arb.may(Use::Speech) && arb.may(Use::Cache) && arb.may(Use::Warm), "nothing going on");
        let now = Arc::new(Mutex::new(Activity::default()));
        let n = now.clone();
        let _ = arb.activity.set(Box::new(move || *n.lock().unwrap()));
        // (what goes on, may warm, may build the cache)
        for (a, warm, cache) in [
            (Activity { busy: true, ..Activity::default() }, true, false),
            (Activity { transcribing: true, busy: true, ..Activity::default() }, false, false),
            (Activity { agents: true, ..Activity::default() }, false, false),
            (Activity::default(), true, true),
        ] {
            *now.lock().unwrap() = a;
            assert_eq!((arb.may(Use::Warm), arb.may(Use::Cache)), (warm, cache), "{a:?}");
            assert!(arb.may(Use::Speech) && arb.may(Use::Ready));
        }
    }
}
