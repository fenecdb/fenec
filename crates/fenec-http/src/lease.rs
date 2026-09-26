//! A node's lease from its router: which tenants the node may write, and
//! until when (`fenec-pg --lease`, and `fenec-shard --auto-failover` on the
//! router's side).
//!
//! The router renews the lease while it reaches the node. A node it cannot
//! reach stops taking writes once the lease lapses, and the router promotes
//! the node's tenants elsewhere only once it knows the lease has -- so a
//! partition never leaves two nodes taking one tenant's writes. The node
//! measures its lease from when it takes a grant in; the router measures
//! it from when the node's answer comes back, which is later, and waits a
//! tenth longer besides: by its own clock the node stops first.
//!
//! **The lease names tenants.** A node the router failed over comes back
//! holding the files of tenants now served elsewhere, primaries' files
//! still. A lease for the whole node would let them take writes again the
//! moment it was renewed; a lease naming the tenants the router places on
//! the node leaves them refused until a repair has them follow. The list is
//! sent when it changes, named by an `epoch` the router derives from it,
//! and asked for again when the node does not hold it -- after a restart.
//!
//! **Started without one.** A node started to take leases takes no write
//! before its first grant: it cannot know whether its tenants were failed
//! over while it was down.

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

    /// Runs the lease `ms` from now, over `primaries` when they come with
    /// the grant and over the list already held when `epoch` names it.
    pub fn grant(
        &self,
        ms: u64,
        epoch: &str,
        primaries: Option<Vec<String>>,
    ) -> Result<(), NeedList> {
        let mut s = self.state.write().unwrap_or_else(|e| e.into_inner());
        match primaries {
            Some(list) => {
                s.primaries = list.into_iter().collect();
                s.epoch = Some(epoch.to_string());
            }
            None if s.epoch.as_deref() != Some(epoch) => return Err(NeedList(s.epoch.clone())),
            None => {}
        }
        s.until = Some(Instant::now() + Duration::from_millis(ms));
        Ok(())
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

    /// `GET /_admin/lease`: the list's epoch, its length and the
    /// milliseconds left, or `null` before a grant.
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
        format!(
            "{{\"epoch\":{epoch},\"primaries\":{},\"left_ms\":{}}}",
            s.primaries.len(),
            left.map_or("null".into(), |l| l.to_string())
        )
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
}
