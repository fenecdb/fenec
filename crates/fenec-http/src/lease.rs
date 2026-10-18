//! A node's lease from its router: which tenants the node may write, and
//! until when (`fenec-server --lease`, and `fenec-shard --auto-failover` on the
//! router's side).
//!
//! The router renews the lease while it reaches the node. A node it cannot
//! reach stops taking writes once the lease lapses, and the router promotes
//! the node's tenants elsewhere only once it knows the lease has -- so a
//! partition never leaves two nodes taking one tenant's writes.
//!
//! **The lease runs from a moment the router acknowledged.** A grant is
//! numbered, and names the newest grant whose answer the router had back
//! in time (`acked`); the node notes when it answered each grant, and runs
//! its lease from when it answered the one acknowledged -- never from when
//! it reads the grant. The router measures the lease from when that answer
//! came back, which is later, and waits a tenth longer besides: by its own
//! clock the node stops first. Run from when it read the grant, as it was,
//! a grant the router gave up on -- held on its way, or left unread by a
//! node stopped or slow -- and read after the router had failed the node
//! over took a whole lease, the old primary writing beside the new one.
//! Now a grant read late names an acknowledgement as old as the router's
//! last answer from the node, whose lease ran out before the failover; an
//! answer the router gave up on is acknowledged by no grant; a number this
//! node never answered, or answered for another holder, runs nothing. So a
//! node needs two grants for a lease, the second acknowledging the first:
//! the router sends it at once where a node's answer says its lease is
//! short, or that it did not take the grant (`took`).
//!
//! **A grant is taken whole where it is newer.** Newer than the grant the
//! lease came from by the answer it acknowledges, then by its own number;
//! taken whole -- its list and the lease it runs, which may cut the one
//! held short -- where it is newer, its list alone where the lease has
//! lapsed, and not at all otherwise. Taken apart, a late grant carrying a
//! list from before a failover would name a tenant promoted elsewhere
//! beside the lease a newer grant runs; judged by the lease it would run,
//! a router restarted or promoted with a shorter lease never cut a longer
//! one short, and the node wrote on after that router failed it over.
//!
//! **What it does not cover.** The lease is measured on [`Instant`] --
//! `CLOCK_MONOTONIC` on Linux, `CLOCK_UPTIME_RAW` on macOS -- which stands
//! still while a virtual machine is stopped (KVM's kvmclock across a stop
//! and a continue) or a machine suspended: a node paused so wakes with
//! lease left, and writes beside the new primary until it runs out, no
//! late grant needed. A write that passed the fence just before its
//! process stopped lands once it resumes. A stall of `ANSWERED` (16)
//! rounds or more can push the acknowledgement out of the grants kept: the
//! node then holds no lease until the router's next grant, which it sends
//! at once.
//! Two directories leasing one node -- a misconfiguration -- can take it in
//! turns as each other's lease lapses, neither holding one for long.
//!
//! **The lease names tenants.** A node the router failed over comes back
//! holding the files of tenants now served elsewhere, primaries' files
//! still. A lease for the whole node would let them take writes again the
//! moment it was renewed; a lease naming the tenants the router places on
//! the node leaves them refused until a repair has them follow. The list is
//! sent when it changes, named by an `epoch` the router derives from it,
//! and asked for again when the node does not hold it -- after a restart.
//!
//! **It names who granted it.** A grant carries its [`Holder`]: the
//! directory the router leases from -- the global one, or a region's
//! (`fenec-shard --region`) -- and that directory's term, the number of
//! times its history forked, which a router's promotion does. A node takes
//! a grant from the directory it holds a lease from at a term no lower,
//! and none from another directory while that lease runs: a router
//! promoted over one that is not gone -- cut off, or come back from a
//! pause -- fences it at every node that has heard from the new one, and
//! two directories that both name a node cannot take turns at it while a
//! lease runs. A router
//! from before terms sends none and is the global directory at term 0, so
//! a deployment that never forks a history leases as it did.
//!
//! **A router from before numbered grants** sends no number, and its grant
//! runs the lease from when the node reads it, as it did: a node refusing
//! them would stop writing the moment it was upgraded ahead of its router,
//! every node of a rolling upgrade, where taken so it protects as much as
//! it did. The late grant is then still a hole, until the router is
//! upgraded too.
//!
//! **Started without one.** A node started to take leases takes no write
//! before its first grant: it cannot know whether its tenants were failed
//! over while it was down. Nor which term it last held: a restart forgets
//! it, and the first grant after one is taken from whoever sends it. Nor
//! which grants it answered: an acknowledgement from before runs nothing.

use std::collections::{HashSet, VecDeque};
use std::sync::RwLock;
use std::time::{Duration, Instant};

pub struct Lease {
    state: RwLock<State>,
}

#[derive(Default)]
struct State {
    /// `None` until the first grant.
    until: Option<Instant>,
    primaries: HashSet<String>,
    /// The name of the list held: the router's `epoch` it came with.
    epoch: Option<String>,
    /// Who granted the lease held: `None` until the first grant.
    holder: Option<Holder>,
    /// The numbered grants answered for `holder`, and when each was: what
    /// an acknowledgement runs the lease from. The last [`ANSWERED`],
    /// oldest first -- by when they came, not by number: a router's
    /// grants are numbered from a random start a process, and kept by
    /// number the ones of a router before its restart could have pushed
    /// out every one of its own.
    answered: VecDeque<(u64, Instant)>,
    /// What the lease and the list held came from: when the answer their
    /// grant acknowledged was given, and that grant's number. `None` where
    /// no numbered grant ran them.
    basis: Option<(Instant, u64)>,
}

/// How many answered grants a node keeps. The router acknowledges the one
/// before as it sends the next; a few more cover the grants a stalled node
/// reads late in between, each answered too.
const ANSWERED: usize = 16;

/// A grant's number, and the number of the newest grant whose answer the
/// router had back in time: what the lease runs from, `None` where no
/// answer did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Numbered {
    pub grant: u64,
    pub acked: Option<u64>,
}

/// Who grants a lease: the directory a router leases from, and how many
/// times its history forked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Holder {
    /// The region whose directory it is, `""` for the global one.
    pub authority: String,
    /// The directory's history's forks: one more at each promotion.
    pub term: u64,
    /// The history it is on: two routers promoted from one term each fork
    /// one of their own, and only the first a node hears from is taken.
    pub history: u64,
}

impl Holder {
    /// `<authority> <term> <history>`, as [`HOLDER_HEADER`] carries it:
    /// `-` for the global directory, so the three parts split on spaces.
    pub fn header(&self) -> String {
        let authority = match self.authority.as_str() {
            "" => "-",
            a => a,
        };
        format!("{authority} {} {:016x}", self.term, self.history)
    }

    /// The other way: `None` for anything else.
    pub fn parse(value: &str) -> Option<Holder> {
        let mut parts = value.split_whitespace();
        let authority = match parts.next()? {
            "-" => String::new(),
            a => a.to_string(),
        };
        let term = parts.next()?.parse().ok()?;
        let history = u64::from_str_radix(parts.next()?, 16).ok()?;
        parts.next().is_none().then_some(Holder {
            authority,
            term,
            history,
        })
    }

    /// Whether a grant from `self` is superseded by the lease `held`, which
    /// runs until `until`: from its directory at a higher term, or at its
    /// term on another history; from another directory while it runs.
    fn superseded_by(&self, held: &Holder, running: bool) -> bool {
        if self.authority != held.authority {
            return running;
        }
        self.term < held.term || (self.term == held.term && self.history != held.history)
    }

    /// The region named for a message.
    fn name(&self) -> String {
        match self.authority.as_str() {
            "" => "the global directory".into(),
            a => format!("region `{a}`'s directory"),
        }
    }
}

/// The header a router names itself in on a call that changes which copy
/// of a tenant takes writes -- a promotion, a follow -- so that a node held
/// by a newer term refuses one from a router it superseded.
pub const HOLDER_HEADER: &str = "fenec-lease-holder";

/// Why a grant was not taken.
pub enum Refusal {
    /// It named a list the node does not hold: the epoch it holds, if any.
    NeedList(Option<String>),
    /// The node holds a lease that supersedes it: from whom.
    Superseded(Holder),
}

/// A grant without the list, to a node that does not hold the one it
/// names: the epoch it holds, if any.
pub struct NeedList(pub Option<String>);

impl Default for Lease {
    fn default() -> Lease {
        Lease::new()
    }
}

impl Lease {
    pub fn new() -> Lease {
        Lease {
            state: RwLock::new(State::default()),
        }
    }

    /// [`Self::grant_from`] the global directory at term 0, unnumbered, as
    /// a router from before terms grants.
    pub fn grant(
        &self,
        ms: u64,
        epoch: &str,
        primaries: Option<Vec<String>>,
    ) -> Result<(), NeedList> {
        match self.grant_from(&Holder::default(), ms, epoch, primaries, None) {
            Ok(_) => Ok(()),
            Err(Refusal::NeedList(held)) => Err(NeedList(held)),
            Err(Refusal::Superseded(_)) => {
                let s = self.state.read().unwrap_or_else(|e| e.into_inner());
                Err(NeedList(s.epoch.clone()))
            }
        }
    }

    /// Answers a grant -- over `primaries` when they come with it, over the
    /// list already held when `epoch` names it -- unless the lease held
    /// supersedes `holder`'s, and says whether it took it: whether its list
    /// is the one held now. Numbered, it runs the lease `ms` from when this
    /// node answered the grant it acknowledges, its list and that lease
    /// taken together where it is newer than the grant they came from, and
    /// its list alone where the lease has lapsed; unnumbered -- a router
    /// from before numbers -- `ms` from now.
    pub fn grant_from(
        &self,
        holder: &Holder,
        ms: u64,
        epoch: &str,
        primaries: Option<Vec<String>>,
        numbered: Option<Numbered>,
    ) -> Result<bool, Refusal> {
        let mut s = self.state.write().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let running = s.until.is_some_and(|u| now < u);
        if let Some(held) = &s.holder {
            if holder.superseded_by(held, running) {
                return Err(Refusal::Superseded(held.clone()));
            }
        }
        if primaries.is_none() && s.epoch.as_deref() != Some(epoch) {
            return Err(Refusal::NeedList(s.epoch.clone()));
        }
        let Some(numbered) = numbered else {
            take_list(&mut s, epoch, primaries);
            s.holder = Some(holder.clone());
            s.until = Some(now + Duration::from_millis(ms));
            s.basis = None;
            return Ok(true);
        };
        // Another holder's numbers name none of its grants.
        if s.holder.as_ref() != Some(holder) {
            s.answered.clear();
            s.basis = None;
            s.holder = Some(holder.clone());
        }
        let from = numbered.acked.and_then(|acked| {
            let mut answered = s.answered.iter();
            answered
                .find(|(grant, _)| *grant == acked)
                .map(|(_, at)| *at)
        });
        // Newer than the grant the lease came from: by the answer it
        // acknowledges, then -- the same answer, which only grants of one
        // router process name -- by its own number, counted on as that
        // router sent them. Judged by the lease it would run, a grant of a
        // router restarted with a shorter lease, or promoted over one with
        // a longer, never cut the longer one short: the node wrote on for
        // it after the new router had failed it over, two primaries; and a
        // grant sent before another acknowledging the same answer, read
        // after it, ran a lease as long and put its older list back.
        let newer = match (from, s.basis) {
            (Some(at), Some((was, by))) => {
                at > was || (at == was && (numbered.grant.wrapping_sub(by) as i64) > 0)
            }
            (Some(_), None) => true,
            (None, _) => false,
        };
        // Taken whole or not at all where a lease runs: a grant read late
        // runs nothing past what its acknowledgement does, and one whose
        // list named a tenant since promoted elsewhere, taken beside the
        // lease a newer grant runs, would have this node write it again.
        // A lease that has lapsed is held to nothing, so any grant may set
        // the list -- what a node back from a failover, or new to this
        // holder, writes once a newer grant runs it a lease.
        let took = !running || newer;
        if took {
            take_list(&mut s, epoch, primaries);
        }
        if let (true, Some(at)) = (newer, from) {
            s.until = Some(at + Duration::from_millis(ms));
            s.basis = Some((at, numbered.grant));
        }
        if s.answered.len() == ANSWERED {
            s.answered.pop_front();
        }
        s.answered.push_back((numbered.grant, now));
        Ok(took)
    }

    /// Whether a router naming itself `holder` may promote or re-point a
    /// copy here, and why not: refused where the lease held supersedes it.
    /// One from another directory is let through -- the node holds no
    /// tenant of it to change -- and so is one naming nobody: an operator's
    /// call by hand, or a router from before terms.
    pub fn admits(&self, holder: Option<&Holder>) -> Result<(), String> {
        let (Some(holder), s) = (holder, self.state.read().unwrap_or_else(|e| e.into_inner()))
        else {
            return Ok(());
        };
        match &s.holder {
            Some(held)
                if held.authority == holder.authority && holder.superseded_by(held, true) =>
            {
                Err(format!(
                    "this node is leased by {} at term {}: a router at term {} was superseded \
                     and changes nothing here",
                    held.name(),
                    held.term,
                    holder.term
                ))
            }
            _ => Ok(()),
        }
    }

    /// Whether `tenant` may take a write now, and why not.
    pub fn allows(&self, tenant: &str) -> Result<(), String> {
        let s = self.state.read().unwrap_or_else(|e| e.into_inner());
        match s.until {
            None => Err(
                "this node holds no lease from its router yet, and takes no write until it does"
                    .into(),
            ),
            Some(until) if Instant::now() >= until => Err(
                "this node's lease from its router lapsed: its tenants may be failing over \
                 to other nodes; send the write through the router"
                    .into(),
            ),
            Some(_) if !s.primaries.contains(tenant) => Err(format!(
                "the router places `{tenant}` on another node: this copy takes no write"
            )),
            Some(_) => Ok(()),
        }
    }

    /// `GET /_admin/lease`, and a grant's answer: the list's epoch, its
    /// length, the milliseconds left, who granted it and the newest
    /// numbered grant answered for them, or `null`s before a grant. The
    /// router reads `left_ms` to send at once the grant that acknowledges
    /// this one where the lease is short, and the number's absence to tell
    /// a node from before numbered grants.
    pub fn describe(&self) -> String {
        let s = self.state.read().unwrap_or_else(|e| e.into_inner());
        let left = s.until.map(|u| {
            u.checked_duration_since(Instant::now())
                .unwrap_or_default()
                .as_millis()
        });
        let mut epoch = String::new();
        match &s.epoch {
            Some(e) => fenec_core::json::escape_into(&mut epoch, e),
            None => epoch.push_str("null"),
        }
        let mut out = format!(
            "{{\"epoch\":{epoch},\"primaries\":{},\"left_ms\":{}",
            s.primaries.len(),
            left.map_or("null".into(), |l| l.to_string())
        );
        match &s.holder {
            None => out.push_str(",\"authority\":null,\"term\":null"),
            Some(h) => {
                out.push_str(",\"authority\":");
                fenec_core::json::escape_into(&mut out, &h.authority);
                out.push_str(&format!(
                    ",\"term\":{},\"history\":\"{:016x}\"",
                    h.term, h.history
                ));
            }
        }
        match s.answered.back() {
            Some((grant, _)) => out.push_str(&format!(",\"grant\":\"{grant:016x}\"")),
            None => out.push_str(",\"grant\":null"),
        }
        out.push('}');
        out
    }

    /// The refusal of a superseded grant, as a node answers it (409): who
    /// holds the node, which the router reads to tell it from a node that
    /// takes no lease -- a router from before terms reads any 409 so, and
    /// fails over neither.
    pub fn superseded_body(held: &Holder) -> String {
        let mut out = String::from("{\"error\":");
        fenec_core::json::escape_into(
            &mut out,
            &format!(
                "this node is leased by {} at term {} on history {:016x}: a grant from a router \
                 it superseded, or from another directory while that lease runs, is refused",
                held.name(),
                held.term,
                held.history
            ),
        );
        out.push_str(",\"held_by\":{\"authority\":");
        fenec_core::json::escape_into(&mut out, &held.authority);
        out.push_str(&format!(
            ",\"term\":{},\"history\":\"{:016x}\"}}}}",
            held.term, held.history
        ));
        out
    }
}

/// Holds `primaries` under `epoch` where a grant brought them.
fn take_list(s: &mut State, epoch: &str, primaries: Option<Vec<String>>) {
    if let Some(list) = primaries {
        s.primaries = list.into_iter().collect();
        s.epoch = Some(epoch.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lease_names_its_tenants_and_lapses() {
        let l = Lease::new();
        assert!(l.allows("a").is_err(), "no write before the first grant");
        l.grant(60_000, "e1", Some(vec!["a".into(), "b".into()]))
            .ok()
            .unwrap();
        assert!(l.allows("a").is_ok() && l.allows("b").is_ok());
        assert!(l.allows("c").unwrap_err().contains("another node"));
        // The list rides only when it changed: a renewal names it.
        assert!(l.grant(60_000, "e1", None).is_ok());
        assert!(matches!(l.grant(60_000, "e2", None), Err(NeedList(Some(e))) if e == "e1"));
        l.grant(60_000, "e2", Some(vec!["c".into()])).ok().unwrap();
        assert!(l.allows("a").is_err() && l.allows("c").is_ok());
        l.grant(0, "e2", None).ok().unwrap();
        assert!(l.allows("c").unwrap_err().contains("lapsed"));
    }

    fn holder(authority: &str, term: u64, history: u64) -> Holder {
        Holder {
            authority: authority.into(),
            term,
            history,
        }
    }

    /// A grant from a superseded router -- its directory at a lower term,
    /// or at the same term on another history -- is refused, and so is one
    /// from another directory while the lease held runs; a higher term is
    /// taken at once, and another directory once the lease has lapsed.
    #[test]
    fn a_grant_from_a_superseded_router_is_refused() {
        let l = Lease::new();
        let us1 = holder("us", 1, 7);
        let list = || Some(vec!["acme".to_string()]);
        assert!(l.grant_from(&us1, 60_000, "e1", list(), None).is_ok());
        // Promoted: term 2 on a history of its own, taken while 1's runs.
        let us2 = holder("us", 2, 9);
        assert!(l.grant_from(&us2, 60_000, "e1", None, None).is_ok());
        assert!(l.allows("acme").is_ok(), "the list held went on");
        for stale in [us1.clone(), holder("us", 2, 8)] {
            match l.grant_from(&stale, 60_000, "e1", None, None) {
                Err(Refusal::Superseded(h)) => assert_eq!(h, us2),
                _ => panic!("{stale:?} was taken"),
            }
        }
        // Another directory, and a router from before terms, while it runs.
        assert!(matches!(
            l.grant_from(&holder("eu", 5, 1), 60_000, "e1", None, None),
            Err(Refusal::Superseded(_))
        ));
        assert!(l.grant(60_000, "e1", None).is_err());
        assert!(l.describe().contains(r#""authority":"us","term":2"#));
        // Lapsed, another directory may take the node.
        l.grant_from(&us2, 1, "e1", None, None).ok().unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(l
            .grant_from(&holder("eu", 1, 3), 60_000, "e2", Some(vec![]), None)
            .is_ok());
        // A promotion or a follow from the superseded router is refused; by
        // hand, or from another directory, it is not.
        assert!(l.admits(Some(&holder("eu", 0, 0))).is_err());
        assert!(l.admits(Some(&holder("eu", 1, 3))).is_ok());
        assert!(l.admits(Some(&holder("us", 1, 7))).is_ok());
        assert!(l.admits(None).is_ok());
    }

    fn numbered(grant: u64, acked: Option<u64>) -> Option<Numbered> {
        Some(Numbered { grant, acked })
    }

    /// The milliseconds the lease has left, by `describe`.
    fn left(l: &Lease) -> Option<u64> {
        let d = l.describe();
        let rest = d.split("\"left_ms\":").nth(1).unwrap();
        rest.split([',', '}']).next().unwrap().parse().ok()
    }

    /// A numbered grant runs the lease from when this node answered the
    /// grant it acknowledges, never from when it is read: the first, which
    /// acknowledges none, runs nothing; one naming a number never answered,
    /// or one answered for another holder, nothing either; and the lease
    /// runs no longer than its acknowledgement does however late a grant
    /// carrying it comes.
    #[test]
    fn a_numbered_grant_runs_from_the_answer_it_acknowledges() {
        let l = Lease::new();
        let us = holder("us", 1, 7);
        let list = || Some(vec!["acme".to_string()]);
        assert!(l
            .grant_from(&us, 60_000, "e1", list(), numbered(10, None))
            .is_ok());
        assert!(l.allows("acme").unwrap_err().contains("no lease"));
        assert!(l.describe().contains(r#""grant":"000000000000000a""#));
        assert!(l
            .grant_from(&us, 60_000, "e1", None, numbered(11, Some(99)))
            .is_ok());
        assert!(l.allows("acme").is_err(), "a number never answered ran it");
        // Acknowledged, it runs from when 10 was answered.
        std::thread::sleep(Duration::from_millis(30));
        assert!(l
            .grant_from(&us, 60_000, "e1", None, numbered(12, Some(10)))
            .is_ok());
        assert!(l.allows("acme").is_ok());
        let first = left(&l).unwrap();
        assert!(
            first <= 60_000 - 30,
            "{first} left: run from the read, not the answer"
        );
        // Read again later, the same acknowledgement runs no further.
        std::thread::sleep(Duration::from_millis(30));
        assert!(l
            .grant_from(&us, 60_000, "e1", None, numbered(13, Some(10)))
            .is_ok());
        assert!(left(&l).unwrap() <= first - 30);
        // A short lease from an old answer is over, whenever it is read.
        let l = Lease::new();
        assert!(l
            .grant_from(&us, 20, "e1", list(), numbered(1, None))
            .is_ok());
        std::thread::sleep(Duration::from_millis(40));
        assert!(l
            .grant_from(&us, 20, "e1", None, numbered(2, Some(1)))
            .is_ok());
        assert!(l.allows("acme").unwrap_err().contains("lapsed"));
        // Another holder's numbers name none of its grants.
        assert!(l
            .grant_from(&us, 60_000, "e1", None, numbered(3, Some(2)))
            .is_ok());
        assert!(l.allows("acme").is_ok());
        let us2 = holder("us", 2, 9);
        let l2 = Lease::new();
        assert!(l2
            .grant_from(&us, 60_000, "e1", list(), numbered(5, None))
            .is_ok());
        assert!(l2
            .grant_from(&us2, 60_000, "e1", None, numbered(6, Some(5)))
            .is_ok());
        assert!(
            l2.allows("acme").is_err(),
            "us at term 1's answer ran us at term 2's lease"
        );
    }

    /// A grant is taken whole -- its list and its lease -- or not at all:
    /// one read late, carrying a list from before a failover, leaves the
    /// newer lease and its list as they are, where taken beside them it
    /// would have this node write a tenant promoted elsewhere. Once the
    /// lease has lapsed any grant sets the list, the lease still running
    /// from its acknowledgement.
    #[test]
    fn a_grant_read_late_takes_no_list_beside_a_newer_lease() {
        let l = Lease::new();
        let us = holder("us", 1, 7);
        let with = |names: &[&str]| Some(names.iter().map(|n| n.to_string()).collect());
        assert!(l
            .grant_from(&us, 60_000, "e1", with(&["acme", "b"]), numbered(1, None))
            .is_ok());
        assert!(l
            .grant_from(&us, 60_000, "e1", None, numbered(2, Some(1)))
            .is_ok());
        // acme moves off: the grant after the failover names b alone, and
        // the one after it acknowledges it.
        assert!(l
            .grant_from(&us, 60_000, "e2", with(&["b"]), numbered(4, Some(2)))
            .is_ok());
        assert!(l
            .grant_from(&us, 60_000, "e2", None, numbered(5, Some(4)))
            .is_ok());
        assert!(l.allows("acme").is_err() && l.allows("b").is_ok());
        // Grant 3, sent before the failover and read now: refused nothing,
        // taken nothing.
        assert!(l
            .grant_from(
                &us,
                60_000,
                "e1",
                with(&["acme", "b"]),
                numbered(3, Some(2))
            )
            .is_ok());
        assert!(
            l.allows("acme").is_err(),
            "a late grant's list named acme again"
        );
        assert!(l.describe().contains(r#""epoch":"e2""#));
        // A first grant read late, acknowledging nothing, neither.
        assert!(l
            .grant_from(&us, 60_000, "e1", with(&["acme"]), numbered(0, None))
            .is_ok());
        assert!(l.allows("acme").is_err() && l.allows("b").is_ok());
        // Lapsed, a grant sets the list, and runs no lease past its
        // acknowledgement's: a lease of 20 ms, asked of once the clock has
        // gone past it, never while it may still run.
        let l = Lease::new();
        let took = |l: &Lease, ms, epoch, list, n| l.grant_from(&us, ms, epoch, list, n).ok();
        assert_eq!(
            took(&l, 20, "e1", with(&["acme"]), numbered(1, None)),
            Some(true)
        );
        assert_eq!(took(&l, 20, "e1", None, numbered(2, Some(1))), Some(true));
        std::thread::sleep(Duration::from_millis(40));
        // The same answer acknowledged, by a later grant: newer, and taken.
        assert_eq!(
            took(&l, 20, "e2", with(&["b"]), numbered(3, Some(1))),
            Some(true)
        );
        assert!(l.allows("b").unwrap_err().contains("lapsed"));
        // One acknowledging nothing, read now: its list, and no lease.
        assert_eq!(
            took(&l, 60_000, "e3", with(&["c"]), numbered(0, None)),
            Some(true)
        );
        assert!(l.allows("c").unwrap_err().contains("lapsed"));
        assert_eq!(
            took(&l, 60_000, "e2", with(&["b"]), numbered(4, Some(3))),
            Some(true)
        );
        assert!(l.allows("b").is_ok() && l.allows("acme").is_err());
    }

    /// Two grants acknowledging the same answer -- the second sent once the
    /// first went unanswered, `acked` unmoved -- are told apart by their
    /// numbers: the one sent first, held on its way and read after the
    /// other, takes nothing. Judged by the lease each would run, the two
    /// were equal, and the late one put its list back -- naming a tenant
    /// since promoted elsewhere -- beside the lease that ran (the review of
    /// #343's `equal_ack_late_grant_takes_its_stale_list`).
    #[test]
    fn a_late_grant_acknowledging_the_same_answer_takes_nothing() {
        let l = Lease::new();
        let us = holder("us", 1, 7);
        let with = |names: &[&str]| Some(names.iter().map(|n| n.to_string()).collect());
        let took = |list, epoch, n| l.grant_from(&us, 60_000, epoch, list, n).ok();
        assert_eq!(
            took(with(&["acme", "b"]), "e1", numbered(1, None)),
            Some(true)
        );
        assert_eq!(took(None, "e1", numbered(2, Some(1))), Some(true));
        assert!(l.allows("acme").is_ok());
        // Grant 3 (acked 2, naming acme) is held on its way; acme leaves
        // this node, and grant 4 (acked 2) names b alone.
        assert_eq!(took(with(&["b"]), "e2", numbered(4, Some(2))), Some(true));
        assert!(l.allows("acme").is_err());
        // Grant 3 is read now.
        assert_eq!(
            took(with(&["acme", "b"]), "e1", numbered(3, Some(2))),
            Some(false)
        );
        assert!(
            l.allows("acme").is_err(),
            "late grant 3 named acme again beside the lease grant 4 runs: {}",
            l.describe()
        );
        assert!(l.allows("b").is_ok());
        // Grant 4's answer lost, grant 5 acknowledges 2 as it did: sent
        // after it, it is newer, and taken while the lease runs.
        assert_eq!(
            took(with(&["b", "c"]), "e3", numbered(5, Some(2))),
            Some(true)
        );
        assert!(l.allows("c").is_ok() && l.allows("b").is_ok());
    }

    /// A newer grant runs the lease from its acknowledgement however long
    /// the lease held: a router restarted, or one promoted, with a shorter
    /// lease cuts the longer one short as soon as a grant of its own is
    /// acknowledged. Kept, the longer lease ran on while the new router
    /// counted its own, and failed the node over beside it (the review of
    /// #343's `a_router_restarted_with_a_shorter_lease_...`, which
    /// `late_grants.rs` holds through a router). Its first grant, which
    /// acknowledges nothing, is answered as not taken.
    #[test]
    fn a_newer_grant_cuts_a_longer_lease_short() {
        let left_of = |l: &Lease| left(l).unwrap_or(0);
        for (holder_after, first) in [(holder("us", 1, 7), 70), (holder("us", 2, 9), 5)] {
            let l = Lease::new();
            let us = holder("us", 1, 7);
            let list = || Some(vec!["acme".to_string()]);
            assert!(l
                .grant_from(&us, 600_000, "e1", list(), numbered(1, None))
                .is_ok());
            assert!(l
                .grant_from(&us, 600_000, "e1", None, numbered(2, Some(1)))
                .is_ok());
            assert!(left_of(&l) > 500_000);
            let first_took =
                l.grant_from(&holder_after, 20_000, "e1", list(), numbered(first, None));
            assert_eq!(first_took.ok(), Some(false), "{holder_after:?}");
            assert!(
                left_of(&l) > 500_000,
                "a grant acknowledging nothing ran the lease"
            );
            let next = numbered(first + 1, Some(first));
            assert_eq!(
                l.grant_from(&holder_after, 20_000, "e1", None, next).ok(),
                Some(true)
            );
            assert!(left_of(&l) <= 20_000, "{holder_after:?}: {}", l.describe());
            assert!(l.allows("acme").is_ok());
        }
    }

    /// A grant naming a list the node does not hold is asked for it again
    /// (412), noted as answered nowhere; the grant sent with the list
    /// acknowledges what the first one did, and runs from it.
    #[test]
    fn the_list_asked_for_again_runs_from_the_same_acknowledgement() {
        let l = Lease::new();
        let us = holder("us", 1, 7);
        assert!(l
            .grant_from(&us, 60_000, "e1", Some(vec![]), numbered(1, None))
            .is_ok());
        assert!(matches!(
            l.grant_from(&us, 60_000, "e2", None, numbered(2, Some(1))),
            Err(Refusal::NeedList(Some(e))) if e == "e1"
        ));
        assert!(l.describe().contains(r#""grant":"0000000000000001""#));
        let list = Some(vec!["acme".to_string()]);
        assert!(l
            .grant_from(&us, 60_000, "e2", list, numbered(3, Some(1)))
            .is_ok());
        assert!(l.allows("acme").is_ok());
        // 2 was never answered: acknowledged, it runs nothing.
        let l = Lease::new();
        assert!(l
            .grant_from(&us, 60_000, "e1", Some(vec![]), numbered(1, None))
            .is_ok());
        assert!(l
            .grant_from(&us, 60_000, "e2", None, numbered(2, None))
            .is_err());
        let list = Some(vec!["acme".to_string()]);
        assert!(l
            .grant_from(&us, 60_000, "e2", list, numbered(3, Some(2)))
            .is_ok());
        assert!(l.allows("acme").is_err());
    }

    #[test]
    fn a_holder_goes_through_its_header() {
        for h in [holder("", 0, 0), holder("us-east", 3, u64::MAX)] {
            assert_eq!(Holder::parse(&h.header()), Some(h));
        }
        assert_eq!(Holder::parse("us 1"), None);
        assert_eq!(Holder::parse("us 1 zz"), None);
        assert_eq!(Holder::parse("us 1 00 extra"), None);
    }
}
