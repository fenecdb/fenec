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
//! one short, and the node wrote on after that router failed it over. The
//! answer says which ([`Took`]): `took`, the list, and `ran`, the lease. A
//! router counts a node by its own lease only once a grant of its own ran
//! one; a grant taken for its list alone runs none, and a late grant of the
//! router before it, acknowledging an answer kept here, may run the longer
//! lease again.
//!
//! **It is measured on the boot clock** (`Moment`): `CLOCK_BOOTTIME` on
//! Linux, `mach_continuous_time` on Apple's systems, which count the time
//! the machine was suspended. On `Instant`'s clock, which does not, a node
//! suspended while its lease ran woke with the lease left as it was and
//! wrote beside its tenants' new primaries until it ran out.
//!
//! **What it does not cover.** A virtual machine stopped by its host is no
//! suspend to the guest's kernel: under QEMU/KVM's defaults a guest's
//! `stop` and `cont` (QEMU saves the kvmclock and sets it back; on arm64 the
//! virtual counter), and under any of its settings a save and restore or a
//! live migration's blackout, leave every clock in the guest where it
//! stopped, the boot clock too -- and Hyper-V's saved state, VMware's
//! suspend and Firecracker's snapshot restore alike. A node paused so wakes
//! with lease left and writes beside the new primary until it runs out or
//! it reads the router's first grant after the failover, no late grant
//! needed. Every signal inside the guest comes once it runs again, and none
//! is sound (`deploy/RUNBOOK.md`, Known gaps). A write that passed the
//! fence just before its process stopped lands once it resumes; it is not
//! answered as a success once the lease has lapsed by then
//! (`handle_admitted`), but it is in the file, and an answer that passed
//! that check is sent whenever the process runs again. A stall of
//! `ANSWERED` (16) rounds or more can push the acknowledgement out of the
//! grants kept: the node then holds no lease until the router's next grant,
//! which it sends at once.
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
use std::time::Duration;

pub struct Lease {
    state: RwLock<State>,
}

#[derive(Default)]
struct State {
    /// `None` until the first grant.
    until: Option<Moment>,
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
    answered: VecDeque<(u64, Moment)>,
    /// What the lease and the list held came from: when the answer their
    /// grant acknowledged was given, and that grant's number. `None` where
    /// no numbered grant ran them.
    basis: Option<(Moment, u64)>,
}

/// A moment on the clock a node's lease is measured on, in nanoseconds:
/// the system's boot clock, which counts the time the machine was
/// suspended ([`boot`]). On [`std::time::Instant`]'s clock --
/// `CLOCK_MONOTONIC` on Linux, `CLOCK_UPTIME_RAW` on macOS, neither of
/// which counts a suspend -- a node suspended while its lease ran woke with
/// the lease left as it was, while the router's clock, on another machine,
/// had gone on and failed its tenants over: the node wrote beside their
/// new primaries until it ran out. On this one a suspend counts as the time
/// it was, and a node woken past its lease holds none. The router counts
/// on `Instant` still: a router suspended counts less, and fails over
/// later, never sooner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Moment(u64);

impl Moment {
    /// Now, on the boot clock.
    fn now() -> Moment {
        Moment(boot::now())
    }

    /// `d` after this moment; the clock's end where that is past it.
    fn after(self, d: Duration) -> Moment {
        let nanos = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        Moment(self.0.saturating_add(nanos))
    }

    /// From this moment to `later`, nothing where `later` is not later.
    fn until(self, later: Moment) -> Duration {
        Duration::from_nanos(later.0.saturating_sub(self.0))
    }
}

/// The boot clock, declared by hand as the crate declares its other system
/// calls (it takes no `libc`): a clock that counts the time the machine was
/// suspended, and at the rate `Instant`'s does.
mod boot {
    /// `clock_gettime(CLOCK_BOOTTIME)`: `CLOCK_MONOTONIC` and the time spent
    /// suspended, the same clock otherwise. Read through the vDSO as the
    /// monotonic clock is, since Linux 5.3's generic vDSO; a system call on
    /// a kernel before it.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn now() -> u64 {
        use std::ffi::c_long;
        // `time_t` and the nanoseconds are a `long` each on glibc, musl and
        // bionic, under the symbol declared here.
        #[repr(C)]
        struct Timespec {
            sec: c_long,
            nsec: c_long,
        }
        extern "C" {
            fn clock_gettime(clock: i32, ts: *mut Timespec) -> i32;
        }
        const CLOCK_BOOTTIME: i32 = 7;
        let mut ts = Timespec { sec: 0, nsec: 0 };
        // SAFETY: the kernel writes one `timespec`, which outlives the call.
        match unsafe { clock_gettime(CLOCK_BOOTTIME, &mut ts) } {
            0 => (ts.sec as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(ts.nsec as u64),
            // Before Linux 2.6.39, which has no boot clock: refused on every
            // call, so the lease is measured on the one clock throughout.
            _ => super::boot_fallback(),
        }
    }

    /// `mach_continuous_time`: `mach_absolute_time` -- what `Instant` reads
    /// as `CLOCK_UPTIME_RAW` -- and the time spent asleep, in the same
    /// ticks, made nanoseconds by the timebase as std makes them.
    #[cfg(target_vendor = "apple")]
    pub fn now() -> u64 {
        #[repr(C)]
        struct Timebase {
            numer: u32,
            denom: u32,
        }
        extern "C" {
            fn mach_continuous_time() -> u64;
            fn mach_timebase_info(info: *mut Timebase) -> i32;
        }
        static BASE: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
        let &(numer, denom) = BASE.get_or_init(|| {
            let mut t = Timebase { numer: 0, denom: 0 };
            // SAFETY: the kernel writes one `mach_timebase_info`.
            match unsafe { mach_timebase_info(&mut t) } {
                0 if t.numer != 0 && t.denom != 0 => (t.numer as u64, t.denom as u64),
                _ => (1, 1),
            }
        });
        // SAFETY: no arguments; it reads the commpage.
        let ticks = unsafe { mach_continuous_time() };
        // `ticks * numer / denom` with no overflow in u64: 125 / 3 on Apple
        // silicon, 1 / 1 on Intel.
        (ticks / denom)
            .saturating_mul(numer)
            .saturating_add(ticks % denom * numer / denom)
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    pub fn now() -> u64 {
        super::boot_fallback()
    }
}

/// `Instant`'s clock, counted from the first read: where there is no boot
/// clock, the lease is measured as it was.
#[cfg(not(target_vendor = "apple"))]
fn boot_fallback() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let start = *START.get_or_init(std::time::Instant::now);
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// How many answered grants a node keeps. The router acknowledges the one
/// before as it sends the next; a few more cover the grants a stalled node
/// reads late in between, each answered too.
const ANSWERED: usize = 16;

/// What a node did with a grant it answered: whether it took its list --
/// the list held now is the grant's -- and whether it ran the lease from
/// the answer the grant acknowledges. A grant may be taken for its list
/// alone, where the lease held has lapsed and the grant is no newer: a
/// router's first grant, which acknowledges nothing of its own. Read as a
/// lease, such a take had the router count the node by its own lease
/// while a grant of the router before it, read later, ran a longer one
/// again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Took {
    pub list: bool,
    pub lease: bool,
}

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
    /// supersedes `holder`'s, and says what it took of it ([`Took`]).
    /// Numbered, it runs the lease `ms` from when this
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
    ) -> Result<Took, Refusal> {
        self.grant_at(holder, ms, epoch, primaries, numbered, Moment::now)
    }

    /// [`Self::grant_from`] on the clock `now` reads: the boot clock, or a
    /// test's.
    fn grant_at(
        &self,
        holder: &Holder,
        ms: u64,
        epoch: &str,
        primaries: Option<Vec<String>>,
        numbered: Option<Numbered>,
        now: impl FnOnce() -> Moment,
    ) -> Result<Took, Refusal> {
        let mut s = self.state.write().unwrap_or_else(|e| e.into_inner());
        // Read under the lock: answers are noted in the order they were
        // given.
        let now = now();
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
            s.until = Some(now.after(Duration::from_millis(ms)));
            s.basis = None;
            return Ok(Took {
                list: true,
                lease: true,
            });
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
            s.until = Some(at.after(Duration::from_millis(ms)));
            s.basis = Some((at, numbered.grant));
        }
        if s.answered.len() == ANSWERED {
            s.answered.pop_front();
        }
        s.answered.push_back((numbered.grant, now));
        Ok(Took {
            list: took,
            lease: newer,
        })
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
        self.allows_at(tenant, Moment::now)
    }

    /// [`Self::allows`] on the clock `now` reads, read only where a lease
    /// is held.
    #[inline]
    fn allows_at(&self, tenant: &str, now: impl FnOnce() -> Moment) -> Result<(), String> {
        let s = self.state.read().unwrap_or_else(|e| e.into_inner());
        match s.until {
            None => Err(
                "this node holds no lease from its router yet, and takes no write until it does"
                    .into(),
            ),
            Some(until) if now() >= until => Err(
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
        self.describe_at(Moment::now())
    }

    /// [`Self::describe`] at `now`.
    fn describe_at(&self, now: Moment) -> String {
        let s = self.state.read().unwrap_or_else(|e| e.into_inner());
        let left = s.until.map(|u| now.until(u).as_millis());
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
        let took = |l: &Lease, ms, epoch, list, n| {
            l.grant_from(&us, ms, epoch, list, n).ok().map(|t| t.list)
        };
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
        // One acknowledging nothing, read now: its list, and no lease -- a
        // take of the list alone, which the node says.
        let list_only = Took {
            list: true,
            lease: false,
        };
        let first = l.grant_from(&us, 60_000, "e3", with(&["c"]), numbered(0, None));
        assert_eq!(first.ok(), Some(list_only));
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
        let took = |list, epoch, n| {
            let took = l.grant_from(&us, 60_000, epoch, list, n).ok();
            took.map(|t| t.list)
        };
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
            let kept = Took {
                list: false,
                lease: false,
            };
            assert_eq!(first_took.ok(), Some(kept), "{holder_after:?}");
            assert!(
                left_of(&l) > 500_000,
                "a grant acknowledging nothing ran the lease"
            );
            let next = numbered(first + 1, Some(first));
            let ran = Took {
                list: true,
                lease: true,
            };
            assert_eq!(
                l.grant_from(&holder_after, 20_000, "e1", None, next).ok(),
                Some(ran)
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

    /// A machine's two clocks: the boot clock, which counts a suspend, and
    /// the monotonic one -- `Instant`'s -- which does not. Their origins
    /// differ, as a machine's do.
    struct Clocks {
        boot: Moment,
        mono: Moment,
    }

    impl Clocks {
        fn new() -> Clocks {
            Clocks {
                boot: Moment(7 << 40),
                mono: Moment(3 << 30),
            }
        }

        /// The machine runs `ms`: both clocks go on.
        fn run(&mut self, ms: u64) {
            let d = Duration::from_millis(ms);
            (self.boot, self.mono) = (self.boot.after(d), self.mono.after(d));
        }

        /// The machine is suspended for `ms`: the boot clock alone counts it.
        fn suspend(&mut self, ms: u64) {
            self.boot = self.boot.after(Duration::from_millis(ms));
        }
    }

    /// A node suspended while its lease runs wakes without one once the
    /// suspend outlasted what was left, and with what was left less the
    /// suspend otherwise: the lease is measured on the boot clock, which
    /// counts it. A grant read on waking, acknowledging an answer given
    /// before the suspend, runs nothing past that answer's lease; one
    /// acknowledging an answer given since runs a lease again. The same
    /// grants on the monotonic clock -- `Instant`, which the lease was
    /// measured on -- left the lease running after the suspend for what it
    /// had left before it: 7.5 s of writes beside the primaries the router
    /// had promoted 11 s into the suspend.
    #[test]
    fn a_node_suspended_past_its_lease_wakes_without_one() {
        let us = holder("us", 1, 7);
        let list = || Some(vec!["acme".to_string()]);
        for on_boot in [true, false] {
            let l = Lease::new();
            let mut c = Clocks::new();
            let read = |c: &Clocks| if on_boot { c.boot } else { c.mono };
            let grant = |c: &Clocks, list, n| {
                l.grant_at(&us, 10_000, "e1", list, n, || read(c))
                    .ok()
                    .unwrap()
            };
            grant(&c, list(), numbered(1, None));
            c.run(500);
            grant(&c, None, numbered(2, Some(1)));
            c.run(2_000);
            assert!(l.allows_at("acme", || read(&c)).is_ok());
            assert_eq!(left_at(&l, read(&c)), Some(7_500));
            // A suspend shorter than what was left: counted, not ignored.
            c.suspend(3_000);
            let shorter = left_at(&l, read(&c));
            // The lid closed for 30 s.
            c.suspend(30_000);
            let woken = l.allows_at("acme", || read(&c));
            // Grant 3, sent before the router gave up, read on waking: it
            // acknowledges the answer to 2, given before the suspend.
            grant(&c, None, numbered(3, Some(2)));
            let late = l.allows_at("acme", || read(&c));
            if !on_boot {
                assert_eq!(shorter, Some(7_500));
                assert!(woken.is_ok() && late.is_ok(), "the hole closed on Instant?");
                assert_eq!(left_at(&l, read(&c)), Some(8_000));
                continue;
            }
            assert_eq!(shorter, Some(4_500), "the suspend was not counted");
            assert!(woken.unwrap_err().contains("lapsed"));
            assert!(late.unwrap_err().contains("lapsed"));
            assert_eq!(left_at(&l, read(&c)), Some(0));
            // Grant 4 acknowledges the answer to 3, given awake: a lease
            // again, run from that answer.
            c.run(100);
            grant(&c, None, numbered(4, Some(3)));
            assert!(l.allows_at("acme", || read(&c)).is_ok());
            assert_eq!(left_at(&l, read(&c)), Some(9_900));
        }
    }

    /// A lease from a router before numbered grants, run from when the
    /// grant is read, lapses across a suspend as a numbered one does.
    #[test]
    fn an_unnumbered_lease_lapses_across_a_suspend() {
        let l = Lease::new();
        let mut c = Clocks::new();
        let grant = |c: &Clocks| {
            l.grant_at(
                &Holder::default(),
                5_000,
                "e1",
                Some(vec!["acme".into()]),
                None,
                || c.boot,
            )
        };
        assert!(grant(&c).is_ok());
        c.run(1_000);
        assert!(l.allows_at("acme", || c.boot).is_ok());
        c.suspend(4_000);
        assert!(l
            .allows_at("acme", || c.boot)
            .unwrap_err()
            .contains("lapsed"));
        assert!(
            l.allows_at("acme", || c.mono).is_ok(),
            "the clocks did not part"
        );
    }

    /// The milliseconds the lease has left at `now`, by `describe`.
    fn left_at(l: &Lease, now: Moment) -> Option<u64> {
        let d = l.describe_at(now);
        let rest = d.split("\"left_ms\":").nth(1).unwrap();
        rest.split([',', '}']).next().unwrap().parse().ok()
    }

    /// The boot clock is the system's, not `Instant`'s counted from the
    /// first read: ahead of the monotonic clock by the time the machine
    /// slept since it started, never behind it. And it goes on at
    /// `Instant`'s rate, read from below: a sleep never ends early, and the
    /// boot clock's reads lie around `Instant`'s.
    #[test]
    fn the_boot_clock_is_the_systems_and_runs_as_instant_does() {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let mono = || {
            #[repr(C)]
            struct Timespec {
                sec: std::ffi::c_long,
                nsec: std::ffi::c_long,
            }
            extern "C" {
                fn clock_gettime(clock: i32, ts: *mut Timespec) -> i32;
            }
            let mut ts = Timespec { sec: 0, nsec: 0 };
            assert_eq!(unsafe { clock_gettime(1, &mut ts) }, 0);
            ts.sec as u64 * 1_000_000_000 + ts.nsec as u64
        };
        #[cfg(target_vendor = "apple")]
        let mono = || {
            extern "C" {
                fn clock_gettime_nsec_np(clock: u32) -> u64;
            }
            // `CLOCK_UPTIME_RAW`, `Instant`'s.
            unsafe { clock_gettime_nsec_np(8) }
        };
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        {
            let (up, boot) = (mono(), Moment::now().0);
            // A microsecond of the timebase's rounding either way.
            assert!(boot + 1_000 >= up, "the boot clock {boot} is behind {up}");
        }
        // `/proc/uptime` is the boot clock, read in hundredths: in a time
        // namespace whose boot clock is offset from its monotonic one
        // (`unshare --time --boottime <s>`), it tells the two apart, as no
        // machine that never slept does.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let text = std::fs::read_to_string("/proc/uptime").unwrap();
            let up: f64 = text.split_whitespace().next().unwrap().parse().unwrap();
            let boot = Moment::now().0 as f64 / 1e9;
            assert!(
                (boot - up).abs() < 1.0,
                "the boot clock reads {boot} s, /proc/uptime {up} s"
            );
        }
        let b0 = Moment::now();
        let i0 = std::time::Instant::now();
        std::thread::sleep(Duration::from_millis(20));
        let i1 = std::time::Instant::now();
        let b1 = Moment::now();
        assert!(b0.until(b1) >= Duration::from_millis(20));
        assert!(b0.until(b1) + Duration::from_micros(1) >= i1 - i0);
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
