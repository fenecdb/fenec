//! Automatic failover (`--auto-failover <s>`): the router grants every node
//! a lease over the tenants it places there, renews it while it reaches the
//! node, and fails a node over once its lease has certainly lapsed.
//!
//! A node started with `--lease` writes a tenant only while its lease names
//! it and has not lapsed ([`fenec_http::lease`]), and measures a grant from
//! when it takes it in. The router measures it from when the node's answer
//! comes back, which is later, and waits a tenth longer: by the time it
//! promotes a node's tenants elsewhere, the node has stopped taking their
//! writes by its own clock. So a node the router cannot reach -- gone, or
//! cut off with its clients still reaching it -- never takes a write that
//! its tenants' new primaries do not see, and the router needs no answer
//! from it to be sure. What it cannot do is tell the two apart, which is
//! why the node stops on its own rather than wait to be told.
//!
//! A router that starts leasing -- a restart, or a standby promoted --
//! counts every node as granted at that moment: a lease its predecessor
//! granted just before it stopped may run as long, and a failover before it
//! lapsed would make two primaries. A standby keeps forgetting every lease
//! for as long as it follows, so the moment is its promotion, not its
//! start.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How much longer than a lease the router waits before it counts one
/// lapsed: the node's clock and the router's may run apart, and a node's
/// lease is measured from a moment the router only knows the next thing
/// after. A tenth is two orders past any clock's drift.
pub fn lapse(term: Duration) -> Duration {
    term + term / 10
}

pub struct Leases {
    pub term: Duration,
    nodes: Mutex<HashMap<String, Held>>,
    /// When this router began leasing: what a node it never reached counts
    /// from.
    started: Mutex<Instant>,
    /// Each node's turn to be granted, holding the reading of the newest
    /// list sent to it ([`Leases::turn`]).
    turns: Mutex<HashMap<String, Arc<Mutex<u64>>>>,
    /// Numbers the directory's readings for grants, in the order they were
    /// taken ([`Leases::reading`]).
    readings: AtomicU64,
    /// Grants waiting for their node's turn: what a test waits for.
    waiting: AtomicUsize,
}

#[derive(Clone)]
struct Held {
    /// When the answer to the last grant the node took came back.
    acked: Instant,
    /// The epoch of the list the node holds, as far as the router knows.
    epoch: Option<String>,
    /// Failed over: its tenants were promoted elsewhere, and when it answers
    /// again a repair has its copies follow them.
    lost: bool,
    /// It said it takes no lease, and is never failed over on its own.
    refuses: bool,
}

/// What a grant to a node said back.
pub enum Answer {
    /// It holds the lease.
    Taken,
    /// It does not hold the list the grant named: send the list.
    NeedsList,
    /// It takes no lease (`fenec-server` without `--lease`).
    Refuses,
    /// No answer, or an error.
    Silent,
}

impl Leases {
    pub fn new(term: Duration) -> Leases {
        Leases {
            term,
            nodes: Mutex::new(HashMap::new()),
            started: Mutex::new(Instant::now()),
            turns: Mutex::new(HashMap::new()),
            readings: AtomicU64::new(0),
            waiting: AtomicUsize::new(0),
        }
    }

    /// The number of a reading of the directory a grant's list comes from,
    /// taken while the reading's lock is held: a later number read a
    /// directory no older.
    pub fn reading(&self) -> u64 {
        self.readings.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// `node`'s turn to be granted, locked by the grant: the number of the
    /// reading the newest list sent to it came from. A node is granted one
    /// grant at a time, and never a list older than one it was sent -- a
    /// round reads every node's list at its start, and its grant, landing
    /// after the one a create sent at once, put the list from before the
    /// create back: the new tenant was refused on its node (503) until the
    /// next round.
    pub fn turn(&self, node: &str) -> Arc<Mutex<u64>> {
        Arc::clone(
            self.turns
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(node.to_string())
                .or_default(),
        )
    }

    /// Locks `turn`, counted among the grants waiting meanwhile.
    pub fn wait_turn<'t>(&self, turn: &'t Mutex<u64>) -> std::sync::MutexGuard<'t, u64> {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let held = turn.lock().unwrap_or_else(|e| e.into_inner());
        self.waiting.fetch_sub(1, Ordering::SeqCst);
        held
    }

    /// Grants waiting for their node's turn now: what a test waits for to
    /// know one is queued behind another.
    pub fn grants_waiting(&self) -> usize {
        self.waiting.load(Ordering::SeqCst)
    }

    /// A standby router's: it leases nothing, and forgets what it knew, so
    /// that its first round once promoted counts from then.
    pub fn standby(&self) {
        self.held().clear();
        *self.started.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    fn held(&self) -> std::sync::MutexGuard<'_, HashMap<String, Held>> {
        self.nodes.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn entry<'a>(&self, m: &'a mut HashMap<String, Held>, node: &str) -> &'a mut Held {
        let started = *self.started.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(node.to_string()).or_insert_with(|| Held {
            acked: started,
            epoch: None,
            lost: false,
            refuses: false,
        })
    }

    /// The epoch of the list `node` holds, as far as the router knows.
    pub fn epoch(&self, node: &str) -> Option<String> {
        self.held().get(node).and_then(|h| h.epoch.clone())
    }

    /// Records what `node` answered a grant naming `epoch`, `at` the moment
    /// the answer came back: whether it had been failed over and is back.
    pub fn answered(&self, node: &str, epoch: &str, answer: &Answer, at: Instant) -> bool {
        let mut m = self.held();
        let h = self.entry(&mut m, node);
        match answer {
            Answer::Taken => {
                h.acked = at;
                h.epoch = Some(epoch.to_string());
                h.refuses = false;
                std::mem::take(&mut h.lost)
            }
            Answer::NeedsList => {
                h.epoch = None;
                false
            }
            Answer::Refuses => {
                if !h.refuses {
                    fenec_http::log!(
                        "node `{node}` takes no lease: it is not failed over on its own \
                         (start it with --lease)"
                    );
                }
                h.refuses = true;
                false
            }
            Answer::Silent => false,
        }
    }

    /// The nodes whose lease has certainly lapsed and that are not failed
    /// over yet, now marked as failed over.
    pub fn lapsed(&self, now: Instant) -> Vec<String> {
        let mut m = self.held();
        let mut out = Vec::new();
        for (node, h) in m.iter_mut() {
            if !h.refuses && !h.lost && now.duration_since(h.acked) > lapse(self.term) {
                h.lost = true;
                out.push(node.clone());
            }
        }
        out
    }

    /// Makes sure every node the directory names is counted from the
    /// router's start, even one the router never reached.
    pub fn know(&self, node: &str) {
        let mut m = self.held();
        self.entry(&mut m, node);
    }

    /// Forgets a node the directory no longer names.
    pub fn forget(&self, node: &str) {
        self.held().remove(node);
    }

    /// `(node, seconds since it last took a grant, failed over)` for
    /// `/_metrics`.
    pub fn ages(&self) -> Vec<(String, f64, bool)> {
        let now = Instant::now();
        let mut out: Vec<(String, f64, bool)> = self
            .held()
            .iter()
            .map(|(n, h)| (n.clone(), now.duration_since(h.acked).as_secs_f64(), h.lost))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// The name of a node's list: FNV-1a over its sorted names. A node holding
/// the list by this name is sent only the name.
pub fn epoch_of(primaries: &[String]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for name in primaries {
        for b in name.bytes().chain(std::iter::once(b'\n')) {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_is_failed_over_once_its_lease_has_certainly_lapsed() {
        // The router's start lies between these two: each moment asked is
        // placed from the one that bounds it, not from a clock read after
        // the start, which a thread stalled 5 ms between them put past it.
        let before = Instant::now();
        let l = Leases::new(Duration::from_millis(100));
        l.know("n1");
        let after = Instant::now();
        // Counted as granted at the router's start: nothing lapses before.
        assert!(l.lapsed(before).is_empty());
        assert!(l.lapsed(before + Duration::from_millis(105)).is_empty());
        assert_eq!(l.lapsed(after + Duration::from_millis(115)), ["n1"]);
        // Once: a node failed over is not failed over again.
        assert!(l.lapsed(after + Duration::from_secs(5)).is_empty());
        // It comes back, and says so once.
        assert!(l.answered("n1", "e", &Answer::Taken, Instant::now()));
        assert!(!l.answered("n1", "e", &Answer::Taken, Instant::now()));
        assert_eq!(l.epoch("n1").as_deref(), Some("e"));
        // A node that takes no lease is never failed over.
        l.answered("n2", "e", &Answer::Refuses, Instant::now());
        assert!(l
            .lapsed(Instant::now() + Duration::from_secs(60))
            .iter()
            .all(|n| n != "n2"));
        // A standby forgets, and counts from the moment it leases again:
        // counted from its grant 120 ms before, n1 would have lapsed.
        std::thread::sleep(Duration::from_millis(120));
        let before = Instant::now();
        l.standby();
        l.know("n1");
        assert!(l.lapsed(before + Duration::from_millis(105)).is_empty());
    }

    #[test]
    fn a_list_is_named_by_its_names() {
        let a = epoch_of(&["a".into(), "b".into()]);
        assert_eq!(a, epoch_of(&["a".into(), "b".into()]));
        assert_ne!(a, epoch_of(&["ab".into()]));
        assert_ne!(a, epoch_of(&[]));
    }
}
