//! A node's lease from its router: which tenants the node may write, and
//! until when (`fenec-server --lease`, and `fenec-shard --auto-failover` on the
//! router's side).
//!
//! The router renews the lease while it reaches the node. A node it cannot
//! reach stops taking writes once the lease lapses, and the router promotes
//! the node's tenants elsewhere only once it knows the lease has -- so a
//! partition never leaves two nodes taking one tenant's writes. The node
//! measures its lease from when it takes a grant in; the router measures
//! it from when the node's answer comes back, which is later, and waits a
//! tenth longer besides: by its own clock the node stops first. Not yet of
//! a grant the router gave up waiting for: the node takes it whenever it
//! reads it, late, and grants carry no number to refuse it by -- a hole
//! the router's lease module describes, its fix to come.
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
//! two directories that both name a node cannot take turns at it. A router
//! from before terms sends none and is the global directory at term 0, so
//! a deployment that never forks a history leases as it did.
//!
//! **Started without one.** A node started to take leases takes no write
//! before its first grant: it cannot know whether its tenants were failed
//! over while it was down. Nor which term it last held: a restart forgets
//! it, and the first grant after one is taken from whoever sends it.

use std::collections::HashSet;
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

    /// [`Self::grant_from`] the global directory at term 0, as a router
    /// from before terms grants.
    pub fn grant(
        &self,
        ms: u64,
        epoch: &str,
        primaries: Option<Vec<String>>,
    ) -> Result<(), NeedList> {
        match self.grant_from(&Holder::default(), ms, epoch, primaries) {
            Ok(()) => Ok(()),
            Err(Refusal::NeedList(held)) => Err(NeedList(held)),
            Err(Refusal::Superseded(_)) => {
                let s = self.state.read().unwrap_or_else(|e| e.into_inner());
                Err(NeedList(s.epoch.clone()))
            }
        }
    }

    /// Runs the lease `ms` from now, over `primaries` when they come with
    /// the grant and over the list already held when `epoch` names it --
    /// unless the lease held supersedes `holder`'s.
    pub fn grant_from(
        &self,
        holder: &Holder,
        ms: u64,
        epoch: &str,
        primaries: Option<Vec<String>>,
    ) -> Result<(), Refusal> {
        let mut s = self.state.write().unwrap_or_else(|e| e.into_inner());
        let running = s.until.is_some_and(|u| Instant::now() < u);
        if let Some(held) = &s.holder {
            if holder.superseded_by(held, running) {
                return Err(Refusal::Superseded(held.clone()));
            }
        }
        match primaries {
            Some(list) => {
                s.primaries = list.into_iter().collect();
                s.epoch = Some(epoch.to_string());
            }
            None if s.epoch.as_deref() != Some(epoch) => {
                return Err(Refusal::NeedList(s.epoch.clone()))
            }
            None => {}
        }
        s.holder = Some(holder.clone());
        s.until = Some(Instant::now() + Duration::from_millis(ms));
        Ok(())
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

    /// `GET /_admin/lease`: the list's epoch, its length, the milliseconds
    /// left and who granted it, or `null`s before a grant.
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
        assert!(l.grant_from(&us1, 60_000, "e1", list()).is_ok());
        // Promoted: term 2 on a history of its own, taken while 1's runs.
        let us2 = holder("us", 2, 9);
        assert!(l.grant_from(&us2, 60_000, "e1", None).is_ok());
        assert!(l.allows("acme").is_ok(), "the list held went on");
        for stale in [us1.clone(), holder("us", 2, 8)] {
            match l.grant_from(&stale, 60_000, "e1", None) {
                Err(Refusal::Superseded(h)) => assert_eq!(h, us2),
                _ => panic!("{stale:?} was taken"),
            }
        }
        // Another directory, and a router from before terms, while it runs.
        assert!(matches!(
            l.grant_from(&holder("eu", 5, 1), 60_000, "e1", None),
            Err(Refusal::Superseded(_))
        ));
        assert!(l.grant(60_000, "e1", None).is_err());
        assert!(l.describe().contains(r#""authority":"us","term":2"#));
        // Lapsed, another directory may take the node.
        l.grant_from(&us2, 1, "e1", None).ok().unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(l
            .grant_from(&holder("eu", 1, 3), 60_000, "e2", Some(vec![]))
            .is_ok());
        // A promotion or a follow from the superseded router is refused; by
        // hand, or from another directory, it is not.
        assert!(l.admits(Some(&holder("eu", 0, 0))).is_err());
        assert!(l.admits(Some(&holder("eu", 1, 3))).is_ok());
        assert!(l.admits(Some(&holder("us", 1, 7))).is_ok());
        assert!(l.admits(None).is_ok());
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
