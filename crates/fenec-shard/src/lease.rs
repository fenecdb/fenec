//! Automatic failover (`--auto-failover <s>`): the router grants every node
//! a lease over the tenants it places there, renews it while it reaches the
//! node, and fails a node over once its lease has certainly lapsed.
//!
//! A node started with `--lease` writes a tenant only while its lease names
//! it and has not lapsed ([`fenec_http::lease`]). Each grant to a node is
//! numbered and names the newest grant whose answer came back in time
//! (`acked`), and the node runs its lease from when it answered that one.
//! The router measures it from when that answer came back, which is later,
//! and waits a tenth longer: by the time it promotes a node's tenants
//! elsewhere, the node has stopped taking their writes by its own clock.
//! So a node the router cannot reach -- gone, or cut off with its clients
//! still reaching it -- never takes a write that its tenants' new
//! primaries do not see, and the router needs no answer from it to be
//! sure. What it cannot do is tell the two apart, which is why the node
//! stops on its own rather than wait to be told.
//!
//! That holds of a grant however late the node reads it. The node ran its
//! lease from when it read a grant, and one the router gave up on -- no
//! answer within the lease pool's third of a lease, so [`Answer::Silent`]
//! -- was a whole lease to a node that read it late: held on its way, or
//! left unread by a node stopped or slow, and read after the router failed
//! it over, the old primary wrote beside the new one for a lease. Now a
//! grant read late acknowledges what the router had heard by then, a lease
//! the failover outlasted, and an answer that came back late is
//! acknowledged by no grant: it runs nothing. The price is a round: a
//! node's lease runs from the grant before the last it read, so cut off it
//! stops a third of a lease sooner, and a node holding no lease -- new,
//! restarted or back from a failover -- needs two grants for one, the
//! second sent at once where the first's answer says the lease is short
//! ([`Leases::short`]). So is it where the node says it did not take the
//! grant ([`Answer::Kept`]): the first of a router restarted or promoted,
//! the lease of the one before still running, which the grant
//! acknowledging it, newer, replaces -- shorter, where this router's lease
//! is. What a node's monotonic clock misses -- its VM paused, its machine
//! suspended -- leases do not cover ([`fenec_http::lease`]).
//!
//! No node waits for another. A round sends every node its grant on a
//! thread of its own and judges the leases without waiting for any
//! (`Router::round`): it waited for them all, and a grant that took long --
//! to a node gone whose name its resolver was slow to refuse, the lookup
//! bounded by nothing -- held up every other node's renewal until those
//! leases lapsed too and the router failed the healthy nodes over (V5's
//! drill). A node's grants go one at a time, in its turn ([`Leases::turn`]),
//! a round sending none while one of its own is under way
//! ([`Leases::begin_grant`]); its lapse, suspected by a round
//! ([`Leases::suspects`]), is judged in that turn
//! ([`Leases::confirm_lapse`]), when no grant to it that the router still
//! waits on is in flight -- an answer to it that comes back says the node
//! is there -- and its failover keeps the turn until the node was sent the
//! list the failover left.
//!
//! A router that starts leasing -- a restart, or a standby promoted --
//! counts every node as granted at that moment: a lease its predecessor
//! granted just before it stopped may run as long, and a failover before it
//! lapsed would make two primaries. A node added after that is counted from
//! its addition, which no lease of the directory's can have named before
//! ([`Leases::entry`]). A standby keeps forgetting every lease
//! for as long as it follows, and its first round once promoted counts
//! from then ([`Leases::leading`]), not from its last round while it
//! followed, nor from its start. As long, or longer: the directory records
//! the longest lease a router leasing from it may have granted, and a node
//! that has taken none of this router's grants is counted by it, one that
//! has by this router's own -- a router started with a shorter lease
//! failed a node it never reached over by its own, while the node held the
//! longer lease and wrote.
//!
//! A grant names its holder (`fenec_http::lease::Holder`): the directory
//! the router leases from -- the global one, or a region's own -- and its
//! term, how many times that directory's history forked. A promotion forks
//! it, so the router promoted grants at a term the one before cannot reach,
//! and every node that hears from the new one refuses the old one from
//! then on ([`Answer::Superseded`]): a router promoted while the one before
//! still runs -- cut off, or paused -- cannot leave the two leasing a node
//! they both reach. A node only the old one reaches is beyond that, which
//! is why a router is promoted once the one before it is gone, as before
//! terms. A grant's number is the router's own, counted per node: a
//! router promoted counts afresh, and a node new to a holder forgets the
//! numbers it answered another.
//!
//! A node from before numbered grants ignores them, and runs its lease from
//! when it reads a grant as it did: leased as before, the late grant's hole
//! open for it until it is upgraded (said once in the log). Failed over
//! never instead, as a region's router treats a node from before terms
//! ([`Answer::Unfenced`]), a rolling upgrade would take every node's
//! failover away until the last node ran this build, where leased so it
//! is protected as much as before.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How much longer than a lease the router waits before it counts one
/// lapsed: the node's clock and the router's may run apart, and a node's
/// lease is measured from a moment the router only knows the next thing
/// after. A tenth is two orders past any clock's drift.
pub fn lapse(term: Duration) -> Duration {
    term + term / 10
}

/// When a round that starts `now` counts as due, the next one coming
/// `period` after that: the moment it was due -- a `period` after the last
/// was -- where it woke within a period of it, and `now` where it woke
/// later still, a process stopped, so the rounds it missed are not run
/// back to back. Counted from when each woke, every wake's lateness put
/// the rounds after it as late: at background QoS, where a park of a
/// second wakes up to 0.2 s late, the rounds of a 3 s lease came 1.18 s
/// apart on average and two of them up to 2.40 s, so the next grant had
/// 0.6 s to be late in before the node's lease lapsed, which beside busy
/// loops it was; counted from when each was due, 1.00 s apart and two up
/// to 2.04-2.18 s.
pub fn round_due(last: Option<Instant>, now: Instant, period: Duration) -> Instant {
    match last.map(|at| at + period) {
        Some(due) if now < due + period => due,
        _ => now,
    }
}

/// `d` in whole milliseconds, as a grant names a lease.
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
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
    /// Set by a round of a standby router ([`Leases::standby`]) and taken by
    /// its first round once promoted ([`Leases::leading`]); set from the
    /// start, so a router counts from its first round as the primary
    /// whether it started as one or as a standby promoted before any round
    /// of its own.
    following: AtomicBool,
    /// By node, the tenants a failover left on it, held by another
    /// operation, that its grants name no more, and from when a promotion
    /// of each may come ([`Leases::withhold`]).
    withheld: Mutex<HashMap<String, HashMap<String, Instant>>>,
    /// The longest lease a node that has taken none of this router's
    /// grants may hold: its own, or a longer one the directory recorded
    /// as it began leasing ([`Leases::leading`]), until that is lowered
    /// ([`Leases::lowerable`]).
    inherited: Mutex<Duration>,
    /// This router made sure the directory's record covers its own lease,
    /// which it does before its first grant ([`Leases::recorded`]).
    recorded: AtomicBool,
    /// It said it could not ([`Leases::say_unrecorded`]).
    said: AtomicBool,
    /// The lease a grant runs now, in milliseconds: `term`, but for a
    /// directory that cannot record it, where it is the record's
    /// ([`Leases::granting`]).
    granting: AtomicU64,
    /// The leasing thread, woken when the lease granted gets shorter.
    leasing: Mutex<Option<std::thread::Thread>>,
}

#[derive(Clone)]
struct Held {
    /// When the answer to the last grant the node took came back.
    acked: Instant,
    /// The number of that grant: what every grant after it acknowledges,
    /// and the node runs its lease from. `None` until one came back.
    acked_grant: Option<u64>,
    /// When the router last stopped waiting on a grant to it, answered or
    /// not: what a node from before numbered grants may run a lease from,
    /// reading any grant sent before then.
    settled: Instant,
    /// The number the next grant to the node goes out under, counted on
    /// from a random start: a grant this router sent before a restart,
    /// read late, names no number it acknowledges since.
    next: u64,
    /// It took a grant without answering its number: a node from before
    /// numbered grants, whose lease runs from when it reads one. Said once.
    unnumbered: bool,
    /// The epoch of the list the node holds, as far as the router knows.
    epoch: Option<String>,
    /// Failed over: its tenants were promoted elsewhere, and when it answers
    /// again a repair has its copies follow them.
    lost: bool,
    /// It said it takes no lease, and is never failed over on its own.
    refuses: bool,
    /// It ran a lease from this router's answer ([`Leases::ran`]): its
    /// lease runs from this router's answers, its own length, cutting short
    /// any longer one a router before it granted. Until then it is counted
    /// by [`Leases::inherited`].
    own: bool,
    /// A round's grant to it is under way: the next round sends it none
    /// ([`Leases::begin_grant`]).
    granting: bool,
    /// How many of the rounds' grants to it have ended ([`Leases::rounds`]).
    rounds: u64,
    /// Its lease looked lapsed, and a failover of it waits for its turn to
    /// judge again ([`Leases::suspects`]).
    failing: bool,
}

/// What a grant to a node said back.
pub enum Answer {
    /// It holds the lease.
    Taken,
    /// It answered and kept the lease and the list it held (`"took":
    /// false`): a lease from a router before this one, or from a holder
    /// before it, runs, and the grant acknowledged nothing of this one's.
    /// The grant that acknowledges this one goes at once, with the list.
    Kept,
    /// It does not hold the list the grant named: send the list.
    NeedsList,
    /// It takes no lease (`fenec-server` without `--lease`).
    Refuses,
    /// A lease from a newer term, or from another directory, holds it: this
    /// router was superseded there, and fails it over never -- the router
    /// holding it does.
    Superseded,
    /// It took the grant without its term: a node from before terms, which
    /// a router superseded could go on leasing. A region's router fails it
    /// over never, rather than count on a fence it does not have.
    Unfenced,
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
            following: AtomicBool::new(true),
            withheld: Mutex::new(HashMap::new()),
            inherited: Mutex::new(term),
            recorded: AtomicBool::new(false),
            said: AtomicBool::new(false),
            granting: AtomicU64::new(millis(term)),
            leasing: Mutex::new(None),
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

    /// A standby router's round: it leases nothing, and forgets what it knew.
    pub fn standby(&self) {
        self.held().clear();
        self.withheld().clear();
        self.following.store(true, Ordering::SeqCst);
        self.recorded.store(false, Ordering::SeqCst);
        *self.inherited() = self.term;
        self.granting.store(millis(self.term), Ordering::SeqCst);
    }

    /// A round of a router that leases: the first after following, its
    /// promotion's, forgets what it knew and counts every node as granted
    /// from now. Counted from its last round while it followed -- up to a
    /// third of a lease before the promotion -- a node the router before it
    /// renewed after that round could be failed over while that grant still
    /// ran, two primaries.
    ///
    /// A node that has taken none of its grants is counted by the longest
    /// lease the directory recorded, `recorded` asked then, where it is
    /// longer than this router's own: a router started with a shorter
    /// lease than the one before it -- restarted, or a standby promoted --
    /// failed over a node it never reached after its own lease and a
    /// tenth, while the node, cut off, held the longer lease and wrote.
    pub fn leading(&self, recorded: impl FnOnce() -> Option<Duration>) {
        if self.following.swap(false, Ordering::SeqCst) {
            *self.inherited() = self.term.max(recorded().unwrap_or_default());
            let now = Instant::now();
            // Every node granted now, what was learnt of each kept: a router
            // primary from its start may have heard a node say it takes no
            // lease before its first round -- a create's grant -- and,
            // forgetting it, failed over a node that fences nothing.
            for h in self.held().values_mut() {
                h.acked = h.acked.max(now);
            }
            self.withheld().clear();
            *self.started.lock().unwrap_or_else(|e| e.into_inner()) = now;
        }
    }

    fn withheld(&self) -> std::sync::MutexGuard<'_, HashMap<String, HashMap<String, Instant>>> {
        self.withheld.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn inherited(&self) -> std::sync::MutexGuard<'_, Duration> {
        self.inherited.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The lease `h` may hold: this router's own once it took a grant of
    /// it, the longest recorded until then.
    fn term_of(&self, h: &Held) -> Duration {
        match h.own {
            true => self.term,
            false => *self.inherited(),
        }
    }

    /// How long `node` may go unrenewed before it is failed over: a lease
    /// and a tenth, of the one it may hold: its own once it took a grant
    /// of this router's, the longest recorded until then.
    pub fn lapse_of(&self, node: &str) -> Duration {
        let mut m = self.held();
        let h = self.entry(&mut m, node);
        lapse(self.term_of(h))
    }

    /// Whether this router has yet to make sure the directory's record
    /// covers its own lease: asked before every grant, done once.
    pub fn unrecorded(&self) -> bool {
        !self.recorded.load(Ordering::SeqCst)
    }

    /// The directory's record covers this router's own lease, which every
    /// grant runs from now.
    pub fn recorded(&self) {
        self.granting.store(millis(self.term), Ordering::SeqCst);
        self.recorded.store(true, Ordering::SeqCst);
    }

    /// The lease a grant runs now: this router's own, but for a directory
    /// that cannot record it, where it is no longer than the record
    /// ([`Leases::grants_short`]). The leasing thread's rounds come a third
    /// of it apart, and an answer is short against it.
    pub fn granting(&self) -> Duration {
        Duration::from_millis(self.granting.load(Ordering::SeqCst))
    }

    /// The directory could not record this router's lease, and grants run
    /// `term`, the record's: the rounds come a third of it apart. A third
    /// of the router's own apart, a 20 s router over a record of 1 s
    /// granted a second every 6.7 s, every node without a lease most of
    /// the time (the review of #343's
    /// `r3_a_grant_of_the_record_s_length_between_rounds_of_the_router_s_own`).
    pub fn grants_short(&self, term: Duration) {
        let ms = millis(term);
        // Woken, the leasing thread sleeps no longer than the new third:
        // asleep for the third of its own lease it read before the grant,
        // the round after the first came 6.7 s on all the same.
        if ms < self.granting.swap(ms, Ordering::SeqCst) {
            let leasing = self.leasing.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(t) = leasing.as_ref() {
                t.unpark();
            }
        }
    }

    /// `thread` runs the rounds ([`Leases::grants_short`] wakes it).
    pub fn leasing_on(&self, thread: std::thread::Thread) {
        *self.leasing.lock().unwrap_or_else(|e| e.into_inner()) = Some(thread);
    }

    /// Whether to say the record could not be written: once a process.
    pub fn say_unrecorded(&self) -> bool {
        !self.said.swap(true, Ordering::SeqCst)
    }

    /// `node` ran its lease from this router's answer -- it said so
    /// (`"ran"`) -- so its lease is this router's length from now: a
    /// longer one from before was cut short. A take of the list alone, a
    /// grant acknowledging nothing on a lease that had lapsed, runs none:
    /// counted by its own lease from it, a grant of the router before,
    /// read later, acknowledging an answer the node had kept, ran the
    /// longer lease again while this router failed the node over by the
    /// shorter (the review of #343's `r2_a_late_grant_...`).
    pub fn ran(&self, node: &str) {
        let mut m = self.held();
        self.entry(&mut m, node).own = true;
    }

    /// Whether the record may come down to this router's own lease: it is
    /// longer, and this router has led a lease and a tenth of it, so every
    /// lease granted before it led has run out, while every one granted
    /// since is its own -- and a router started after this one, finding it
    /// lowered, counts by what can still run. Raised again by the first
    /// router with a longer lease to lead.
    pub fn lowerable(&self, now: Instant) -> bool {
        let inherited = *self.inherited();
        let started = *self.started.lock().unwrap_or_else(|e| e.into_inner());
        inherited > self.term
            && !self.following.load(Ordering::SeqCst)
            && now.saturating_duration_since(started) > lapse(inherited)
    }

    /// The record came down to this router's own lease.
    pub fn lowered(&self) {
        *self.inherited() = self.term;
    }

    /// `tenant`, left on `node` by its failover, named by no grant to the
    /// node from now: asked with the node's turn held, so no grant naming it
    /// is in flight. Its promotion may come a lease and a tenth after the
    /// last answer the node gave in time, what every grant naming it
    /// acknowledged at the most -- which a failover's lapse has passed, so
    /// at once. A node from before numbered grants runs a lease from any
    /// grant it reads, so for one of those it is a lease and a tenth past
    /// the last grant the router stopped waiting on, answered or not --
    /// which bounds no grant read later still. Left out of the node's list
    /// either way: a grant naming it would have the node take it back once
    /// it answers again, a copy of a tenant that may have gone elsewhere.
    pub fn withhold(&self, node: &str, tenant: &str) {
        let from = {
            let mut m = self.held();
            let h = self.entry(&mut m, node);
            let from = if h.unnumbered { h.settled } else { h.acked };
            from + lapse(self.term_of(h))
        };
        self.withheld()
            .entry(node.to_string())
            .or_default()
            .insert(tenant.to_string(), from);
    }

    /// From when `tenant`, withheld from `node`, may be promoted; `None`
    /// where it is not withheld.
    pub fn withheld_until(&self, node: &str, tenant: &str) -> Option<Instant> {
        self.withheld()
            .get(node)
            .and_then(|t| t.get(tenant))
            .copied()
    }

    /// `tenant` named by `node`'s grants again: promoted, gone, or left
    /// where it is.
    pub fn release(&self, node: &str, tenant: &str) {
        let mut w = self.withheld();
        if let Some(t) = w.get_mut(node) {
            t.remove(tenant);
            if t.is_empty() {
                w.remove(node);
            }
        }
    }

    /// Takes the tenants withheld from each node out of its list.
    pub fn withhold_from(&self, lists: &mut std::collections::BTreeMap<String, Vec<String>>) {
        let w = self.withheld();
        for (node, tenants) in w.iter() {
            if let Some(list) = lists.get_mut(node) {
                list.retain(|t| !tenants.contains_key(t));
            }
        }
    }

    /// Whether `node` was failed over on its own and has not answered a
    /// grant since.
    pub fn is_lost(&self, node: &str) -> bool {
        self.held().get(node).is_some_and(|h| h.lost)
    }

    fn held(&self) -> std::sync::MutexGuard<'_, HashMap<String, Held>> {
        self.nodes.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `node`'s record, counted as granted when this router first knew it:
    /// as it began leading, for every node the directory named then, or as
    /// the node was added since -- which no lease of this directory's can
    /// have named before. Counted from the router's start, a node added a
    /// lease and a tenth into the router's life was lapsed before its first
    /// grant came back, and one given up on -- a second at a 3 s lease, at
    /// background QoS beside busy loops -- had it failed over, every replica
    /// it kept dropped, before any lease of it could run.
    fn entry<'a>(&self, m: &'a mut HashMap<String, Held>, node: &str) -> &'a mut Held {
        let started = *self.started.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(node.to_string()).or_insert_with(|| {
            let known = started.max(Instant::now());
            Held {
                acked: known,
                acked_grant: None,
                settled: known,
                next: fenec_http::replication::fresh_id(),
                unnumbered: false,
                own: false,
                epoch: None,
                lost: false,
                refuses: false,
                granting: false,
                rounds: 0,
                failing: false,
            }
        })
    }

    /// Whether a round may send `node` a grant: none of a round's is under
    /// way to it, which it now is. The rounds wait for no grant
    /// ([`crate::Router::lease_round`]), so one to a node that takes its
    /// whole bound to fail would otherwise be joined by another every
    /// round, each waiting its turn behind the one before.
    pub fn begin_grant(&self, node: &str) -> bool {
        let mut m = self.held();
        let h = self.entry(&mut m, node);
        !std::mem::replace(&mut h.granting, true)
    }

    /// A round's grant to `node` has ended.
    pub fn end_grant(&self, node: &str) {
        if let Some(h) = self.held().get_mut(node) {
            h.granting = false;
            h.rounds += 1;
        }
    }

    /// How many of the rounds' grants to `node` have ended, answered or
    /// not: what a test counts to know a node is granted round after round
    /// while something else hangs. A grant a failover or a create sends is
    /// not a round's, and does not count.
    pub fn rounds(&self, node: &str) -> u64 {
        self.held().get(node).map_or(0, |h| h.rounds)
    }

    /// The epoch of the list `node` holds, as far as the router knows.
    pub fn epoch(&self, node: &str) -> Option<String> {
        self.held().get(node).and_then(|h| h.epoch.clone())
    }

    /// The number of the next grant to `node`, and the grant it
    /// acknowledges: the newest whose answer came back. Asked with the
    /// node's turn held, as grants go one at a time.
    pub fn number(&self, node: &str) -> fenec_http::lease::Numbered {
        let mut m = self.held();
        let h = self.entry(&mut m, node);
        let grant = h.next;
        h.next = h.next.wrapping_add(1);
        fenec_http::lease::Numbered {
            grant,
            acked: h.acked_grant,
        }
    }

    /// Whether a lease with `left_ms` left -- what a node answers a grant,
    /// `None` where it holds none -- would lapse before the next round's
    /// grant could acknowledge the one it answered: the grant that does is
    /// sent at once. A node renewed every round has two thirds of a lease
    /// left as it answers, the grant before acknowledged; one with no lease,
    /// or that missed the acknowledgement of a grant whose answer was lost,
    /// has none or a third, and waited for the next round it would not
    /// write meanwhile -- a tenant created on a node new to the router
    /// refused its writes for up to a round.
    pub fn short(&self, left_ms: Option<u64>) -> bool {
        left_ms.is_none_or(|left| Duration::from_millis(left) < self.granting() / 2)
    }

    /// `node` took a grant without answering its number: a node from
    /// before numbered grants, which runs its lease from when it reads one.
    /// Said once: leased as before, it is open to the late grant until it
    /// is upgraded.
    pub fn unnumbered(&self, node: &str) {
        let mut m = self.held();
        if !std::mem::replace(&mut self.entry(&mut m, node).unnumbered, true) {
            fenec_http::log!(
                "node `{node}` took its lease without the grant's number: a node from before \
                 numbered grants runs it from when it reads a grant, so one it reads late can \
                 outlast its failover (upgrade it)"
            );
        }
    }

    /// Records what `node` answered `grant`, naming `epoch`, `at` the
    /// moment the answer came back: whether it had been failed over and is
    /// back.
    pub fn answered(
        &self,
        node: &str,
        grant: u64,
        epoch: &str,
        answer: &Answer,
        at: Instant,
    ) -> bool {
        let mut m = self.held();
        let h = self.entry(&mut m, node);
        h.settled = h.settled.max(at);
        match answer {
            // Kept, the node answered at `at` -- the grant is one a later
            // grant may acknowledge -- and holds a list this router does not
            // know.
            Answer::Taken | Answer::Kept => {
                h.acked = at;
                h.acked_grant = Some(grant);
                h.epoch = match answer {
                    Answer::Taken => Some(epoch.to_string()),
                    _ => None,
                };
                h.refuses = false;
                let back = std::mem::take(&mut h.lost);
                // Back, it holds the tenants still placed on it again,
                // those a failover withheld from its list among them.
                if back {
                    drop(m);
                    self.withheld().remove(node);
                }
                back
            }
            Answer::NeedsList => {
                h.epoch = None;
                false
            }
            Answer::Refuses | Answer::Superseded | Answer::Unfenced => {
                if !h.refuses {
                    match answer {
                        Answer::Superseded => fenec_http::log!(
                            "node `{node}` is leased by a newer term or another directory: this \
                             router was superseded there, and fails it over never"
                        ),
                        Answer::Unfenced => fenec_http::log!(
                            "node `{node}` took its lease without the term: a node from before \
                             terms is not failed over on its own by a region's router (upgrade it)"
                        ),
                        _ => fenec_http::log!(
                            "node `{node}` takes no lease: it is not failed over on its own \
                             (start it with --lease)"
                        ),
                    }
                }
                h.refuses = true;
                false
            }
            Answer::Silent => false,
        }
    }

    /// The nodes whose lease looks certainly lapsed, not failed over and
    /// with no failover of theirs under way, now marked as under way: each
    /// is judged again by [`Leases::confirm_lapse`] in its turn, once any
    /// grant to it in flight has ended.
    pub fn suspects(&self, now: Instant) -> Vec<String> {
        let mut m = self.held();
        let mut out = Vec::new();
        for (node, h) in m.iter_mut() {
            let term = self.term_of(h);
            if !h.refuses && !h.lost && !h.failing && lapsed(h, now, term) {
                h.failing = true;
                out.push(node.clone());
            }
        }
        out
    }

    /// Whether `node`'s lease has certainly lapsed, now marked as failed
    /// over where it has: asked with the node's turn held
    /// ([`Leases::turn`]), so no grant to it that the router still waits on
    /// is in flight -- its answer, coming back after the suspicion, says
    /// the node is there, and a failover of a node that answers would be
    /// for nothing; one given up on runs nothing past the answers that came
    /// back (the module comment). Ends the failover [`Leases::suspects`]
    /// marked as under way either way.
    pub fn confirm_lapse(&self, node: &str, now: Instant) -> bool {
        let mut m = self.held();
        let Some(h) = m.get_mut(node) else {
            return false;
        };
        h.failing = false;
        let term = self.term_of(h);
        if h.refuses || h.lost || !lapsed(h, now, term) {
            return false;
        }
        h.lost = true;
        true
    }

    /// A failover [`Leases::suspects`] marked as under way that never
    /// started (its thread could not be made): judged again next round.
    pub fn unsuspect(&self, node: &str) {
        if let Some(h) = self.held().get_mut(node) {
            h.failing = false;
        }
    }

    /// Makes sure every node the directory names is counted from when this
    /// router first knew it, even one the router never reached
    /// ([`Leases::entry`]).
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

/// Whether `h`'s lease has certainly lapsed by `now`: a lease and a tenth
/// since its last grant's answer came back.
fn lapsed(h: &Held, now: Instant, term: Duration) -> bool {
    now.saturating_duration_since(h.acked) > lapse(term)
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
        assert!(l.suspects(before).is_empty());
        assert!(l.suspects(before + Duration::from_millis(105)).is_empty());
        let late = after + Duration::from_millis(115);
        assert_eq!(l.suspects(late), ["n1"]);
        // Suspected once while its failover is under way.
        assert!(l.suspects(late).is_empty());
        assert!(l.confirm_lapse("n1", late));
        // Once: a node failed over is not failed over again.
        assert!(l.suspects(after + Duration::from_secs(5)).is_empty());
        assert!(!l.confirm_lapse("n1", after + Duration::from_secs(5)));
        // It comes back, and says so once.
        assert!(l.answered("n1", 0, "e", &Answer::Taken, Instant::now()));
        assert!(!l.answered("n1", 0, "e", &Answer::Taken, Instant::now()));
        assert_eq!(l.epoch("n1").as_deref(), Some("e"));
        // An answer that came back between the suspicion and the judgement,
        // a grant that was in flight, keeps it: nothing is failed over, and
        // it may be suspected again later.
        let later = Instant::now() + Duration::from_millis(115);
        assert_eq!(l.suspects(later), ["n1"]);
        l.answered("n1", 0, "e", &Answer::Taken, later);
        assert!(!l.confirm_lapse("n1", later));
        assert_eq!(l.suspects(later + Duration::from_millis(115)), ["n1"]);
        l.unsuspect("n1");
        // A node that takes no lease is never failed over.
        l.answered("n2", 0, "e", &Answer::Refuses, Instant::now());
        assert!(l
            .suspects(Instant::now() + Duration::from_secs(60))
            .iter()
            .all(|n| n != "n2"));
        // A round's grant to a node is one at a time.
        assert!(l.begin_grant("n1"));
        assert!(!l.begin_grant("n1"));
        l.end_grant("n1");
        assert!(l.begin_grant("n1"));
        l.end_grant("n1");
        // A standby forgets, and counts from the moment it leases again --
        // its first round once promoted, not its last round while it
        // followed: counted from that round 120 ms before, n1 would have
        // lapsed. A round of a router that never followed changes nothing.
        l.standby();
        std::thread::sleep(Duration::from_millis(120));
        let before = Instant::now();
        l.leading(|| None);
        l.know("n1");
        assert!(l.suspects(before + Duration::from_millis(105)).is_empty());
        let led = Instant::now();
        l.leading(|| None);
        l.know("n1");
        assert_eq!(l.suspects(led + Duration::from_millis(115)), ["n1"]);
    }

    /// A tenant withheld from a node is out of every list sent it, may be
    /// promoted a lease and a tenth past the last answer the node gave in
    /// time -- a grant given up on since runs nothing -- and is the node's
    /// again once released or once the node answers after its failover.
    #[test]
    fn a_withheld_tenant_is_named_by_no_grant_until_released() {
        let term = Duration::from_millis(100);
        let l = Leases::new(term);
        let answered = Instant::now();
        l.answered("n1", 7, "e", &Answer::Taken, answered);
        l.answered("n1", 8, "e", &Answer::Silent, answered + term);
        l.withhold("n1", "t");
        assert_eq!(l.withheld_until("n1", "t"), Some(answered + lapse(term)));
        // A node from before numbered grants runs a lease from any grant it
        // reads: past the last one the router stopped waiting on.
        let old = Leases::new(term);
        old.answered("n1", 7, "e", &Answer::Taken, answered);
        old.unnumbered("n1");
        old.answered("n1", 8, "e", &Answer::Silent, answered + term);
        old.withhold("n1", "t");
        assert_eq!(
            old.withheld_until("n1", "t"),
            Some(answered + term + lapse(term))
        );
        let mut lists: std::collections::BTreeMap<String, Vec<String>> =
            [("n1".into(), vec!["t".into(), "u".into()])].into();
        l.withhold_from(&mut lists);
        assert_eq!(lists["n1"], ["u"]);
        l.release("n1", "t");
        assert_eq!(l.withheld_until("n1", "t"), None);
        // Back after its failover, it holds what is placed on it again.
        l.withhold("n1", "t");
        let late = Instant::now() + Duration::from_secs(1);
        l.know("n1");
        assert_eq!(l.suspects(late), ["n1"]);
        assert!(l.confirm_lapse("n1", late));
        assert!(l.answered("n1", 0, "e", &Answer::Taken, late));
        assert_eq!(l.withheld_until("n1", "t"), None);
    }

    /// Each grant to a node has a number of its own and acknowledges the
    /// newest whose answer came back: none before one did, never one given
    /// up on, one that was asked for the list neither. A node forgotten --
    /// a standby's round -- is numbered afresh, from another start.
    #[test]
    fn a_grant_acknowledges_the_newest_answer_that_came_back() {
        let l = Leases::new(Duration::from_millis(100));
        let first = l.number("n1");
        assert_eq!(first.acked, None);
        let second = l.number("n1");
        assert_eq!(second.grant, first.grant.wrapping_add(1));
        l.answered("n1", first.grant, "e", &Answer::Taken, Instant::now());
        assert_eq!(l.number("n1").acked, Some(first.grant));
        for answer in [Answer::Silent, Answer::NeedsList] {
            let given_up = l.number("n1");
            l.answered("n1", given_up.grant, "e", &answer, Instant::now());
            assert_eq!(l.number("n1").acked, Some(first.grant));
        }
        let before = l.number("n1").grant;
        l.standby();
        let afresh = l.number("n1");
        assert_eq!(afresh.acked, None);
        assert_ne!(afresh.grant, before.wrapping_add(1));
        // Short: no lease, or less than would last to the next round's.
        assert!(l.short(None) && l.short(Some(10)));
        assert!(!l.short(Some(66)));
    }

    /// A node that has taken no grant of this router's is counted by the
    /// longest lease the directory recorded; once it takes one -- not one
    /// it answered and kept -- by this router's own. The record may come
    /// down once this router has led a lease and a tenth of it, and not
    /// before; a standby's round forgets all of it.
    #[test]
    fn a_node_unanswered_since_the_start_is_counted_by_the_longest_lease_recorded() {
        let term = Duration::from_millis(100);
        let l = Leases::new(term);
        let before = Instant::now();
        l.leading(|| Some(Duration::from_millis(1_000)));
        l.know("n1");
        l.know("n2");
        let after = Instant::now();
        // Past its own lease and a tenth, short of the recorded one's.
        assert!(l.suspects(before + Duration::from_millis(1_000)).is_empty());
        // n2 answers and keeps the lease it held; n3 takes a grant's list
        // alone, running no lease; n1 takes one and runs it.
        l.answered("n2", 1, "e", &Answer::Kept, after);
        l.know("n3");
        l.answered("n3", 1, "e", &Answer::Taken, after);
        l.answered("n1", 1, "e", &Answer::Taken, after);
        l.ran("n1");
        let lapsed = after + Duration::from_millis(115);
        assert_eq!(l.suspects(lapsed), ["n1"]);
        assert!(l.confirm_lapse("n1", lapsed));
        assert!(!l.lowerable(before + Duration::from_millis(1_000)));
        let past = after + Duration::from_millis(1_115);
        let mut late = l.suspects(past);
        late.sort();
        assert_eq!(late, ["n2", "n3"]);
        assert!(l.lowerable(past));
        l.lowered();
        assert!(!l.lowerable(past));
        // A standby forgets: promoted again, it counts by what it reads then.
        l.standby();
        assert!(l.unrecorded());
        l.leading(|| None);
        l.know("n3");
        assert!(!l.lowerable(Instant::now() + Duration::from_secs(5)));
    }

    /// A router counts from its first round as the primary even where no
    /// round of its own came before -- a standby promoted before its first,
    /// or a router primary from its start -- not from when it was made.
    #[test]
    fn a_first_round_counts_from_itself() {
        let l = Leases::new(Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(120));
        let before = Instant::now();
        l.leading(|| None);
        l.know("n1");
        assert!(l.suspects(before + Duration::from_millis(105)).is_empty());
    }

    /// A node added once the router leads is counted from when it was
    /// added, not from the router's start: no lease of the directory's can
    /// have named it before. Counted from the start, a node added a lease
    /// and a tenth in was lapsed before its first grant came back.
    #[test]
    fn a_node_added_later_is_counted_from_its_addition() {
        let l = Leases::new(Duration::from_millis(100));
        l.leading(|| None);
        l.know("n1");
        // A sleep never ends early: the router has led past a lapse.
        std::thread::sleep(Duration::from_millis(120));
        let before = Instant::now();
        l.know("n2");
        assert_eq!(l.suspects(before), ["n1"]);
        assert!(l.suspects(before + Duration::from_millis(105)).is_empty());
        let after = Instant::now();
        assert_eq!(l.suspects(after + Duration::from_millis(115)), ["n2"]);
    }

    /// A round woken late counts as due when it was, so the one after it is
    /// as early as it would have been: counted from the wake, every round
    /// after one woken late came as late, the leeway of each wake adding up.
    /// One woken more than a round late -- the process stopped -- counts
    /// from the wake, so the rounds it missed are not run back to back.
    #[test]
    fn a_round_woken_late_puts_none_after_it_late() {
        let period = Duration::from_millis(1_000);
        let start = Instant::now();
        assert_eq!(round_due(None, start, period), start);
        let mut due = start;
        for late in [300, 370, 0, 999] {
            let woke = due + period + Duration::from_millis(late);
            let next = round_due(Some(due), woke, period);
            assert_eq!(next, due + period, "a round woken {late} ms late");
            due = next;
        }
        assert_eq!(due, start + period * 4, "the rounds drifted");
        let stopped = due + period * 3;
        assert_eq!(round_due(Some(due), stopped, period), stopped);
        let at_two = due + period * 2;
        assert_eq!(round_due(Some(due), at_two, period), at_two);
    }

    #[test]
    fn a_list_is_named_by_its_names() {
        let a = epoch_of(&["a".into(), "b".into()]);
        assert_eq!(a, epoch_of(&["a".into(), "b".into()]));
        assert_ne!(a, epoch_of(&["ab".into()]));
        assert_ne!(a, epoch_of(&[]));
    }
}
