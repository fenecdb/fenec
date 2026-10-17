//! Read replicas: a follower of a tenant's primary on a node of another
//! region, so that a client far from the tenant's home reads near itself
//! and writes home -- a read from Singapore of a tenant living in Frankfurt
//! paid the 158 ms between them on every request.
//!
//! **Placed by region, kept by the directory holding the node.**
//! `PUT /_shard/tenants/<t>/read-replicas/<region>` gives the tenant one
//! in that region: on its node holding the fewest tenants and copies, made
//! there and told to follow the tenant's primary node, as a replica of its
//! own is ([`Router::attach`]), and recorded once it follows. The record
//! goes in the directory that holds the node: the global one, or the
//! region's own where it keeps one -- whose router then places it, leases
//! its node and answers its reads with no word from the tenant's home. One
//! a region, never in the tenant's home region, never on a standby;
//! `DELETE` takes it out of the directory, then off its node. It is held
//! to its tenant's quota as the directory placing the tenant keeps it,
//! wanted by the quota request and by each look where it changed --
//! another region's directory may keep it, which answers it with the
//! placement -- and sent by its node's sender alone ([`QuotaQueue`]).
//!
//! **Deleted with its tenant, or the tenant kept.** A tenant's delete holds
//! every change of its read replicas, waits one under way out, and takes
//! each off before the tenant goes; one whose node does not take it off
//! keeps the tenant, 502. Another region's directory's go at its router's
//! next look, and the name is not made again while any directory keeps
//! one of it ([`Router::no_reader_traces`]): a look finding the name made
//! again, another owner's, pointed the old copy at it. A region's router
//! tells the global router as it places one (`traces`), and a create asks
//! those regions alone.
//!
//! **Bound to its tenant's lineage.** Each replica is a copy of one tenant,
//! not of a name: its record keeps the lineage the directory recorded for
//! the tenant (`lineages`), every `follow` tells the copy, and the copy
//! takes nothing from a primary of another, and deletes itself. A look
//! finding the tenant's lineage another than the replica's takes the
//! replica off. Whatever a router forgets or records late, a tenant made
//! again under the name is never copied into a region nobody chose for it.
//! A tenant made before lineages were kept has its lineage asked of its
//! node ([`Router::backfill_lineages`]), and its replicas are bound to it.
//!
//! **Never promoted.** A failover promotes a tenant's standby copy or its
//! replica of its own, both in its home region, and reads nothing here:
//! a read replica is neither, by construction, so no failover of the
//! home region carries the tenant out of it.
//!
//! **It follows the primary wherever it goes.** Every router looks once a
//! [`FOLLOW_EVERY`] at where each primary it keeps a read replica of is --
//! in its own directory, its copy of the global one, or the tenant's
//! region's router, asked for every tenant of that region at once -- and
//! points the ones that follow another address there, on a worker a node
//! that the look waits for none of, each call bounded by [`CALL`]; a
//! failover or a move it makes has that look
//! come at once, on its own thread: the failover waits on no replica's
//! node, a region away or cut off, and holds nothing the look needs. A
//! region keeping its own directory learns of a failover in another within
//! a look. One whose tenant is gone is taken off; one whose tenant moved
//! into its region is too, the tenant's own copy being nearer still. Apart
//! from the looks, every [`CHECK_EVERY`], a thread asks each replica's node
//! what it holds, and a copy gone is made again at the look it wakes.
//!
//! **Which reads go there, and what they read.** The router parses no
//! FenecQL, so a read is what its client says is one: `Fenec-Read:
//! replica`, which the SDKs set for the builders' reads and a caller sets
//! for a text it knows reads. A router near a region (`--near`, or a
//! region's router `--region`) forwards such a request of a tenant with a
//! read replica there to it, and every other request home; every node runs
//! a marked request read-only on every route (`fenec_http`'s `marked`), so
//! a write marked by mistake is refused, never landed. A copy answers none
//! until, since it opened or was told to follow, its primary has said hello
//! -- its image adopted, or its place in the primary's history found -- so
//! one placed a moment ago, an empty file, sends the read home with a 503,
//! and so does one whose file the primary's history may not continue. A
//! client sends the position its
//! last answer from home named -- `Fenec-Seq` and `Fenec-History`, the
//! history that change is of -- as `Fenec-After` and `Fenec-History`, and
//! the replica answers once it holds that change of that history: a
//! failover forks the history where the copy promoted stood, and a copy
//! left on the old branch, at the same change number, holds other writes.
//! One the replica cannot answer in the wait, or that does not answer in
//! [`crate::Config::replica_timeout`] past it, goes home, and a replica
//! that did not answer is let be for [`RESTING`]. Home answers it, or --
//! the write lost in the failover -- says at once where it stands, which
//! the SDK takes as its position over the one that read was sent after,
//! never an earlier term of the same lineage -- its history id's top half,
//! which a promotion keeps and a tenant made again does not
//! (`fenec_http::replication::fork_id`). A read sent with no `Fenec-After` reads
//! what the replica has applied: behind home, and between a failover and
//! the look that points the replica at the new primary -- and its image --
//! writes the failover abandoned.

use crate::directory::Reader;
use crate::{internal, message, quote, region_name, Fail, Outcome, Router};
use fenec_core::prelude::*;
use fenec_http::http::{Method, Request, Response};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often a router looks at where the primary of each tenant it keeps a
/// read replica of is, and points the ones following another address
/// there. A failover the same router makes has the look come at once; this
/// is how a region keeping its own directory learns of one in another, and
/// how a re-pointing that failed -- the replica's node down -- is made
/// again. A look over a router's own directory reads its maps alone; a
/// tenant at home in a region keeping its own costs one call to that
/// region's router a look, for all of its tenants.
pub const FOLLOW_EVERY: Duration = Duration::from_secs(1);

/// The longest a call a read replica's upkeep makes may take: a node made
/// to follow closes and opens its tenant, waiting for the requests on it
/// up to 5 s. The upstream timeout, a minute by default, held a look over
/// every read replica that long for one node that did not answer.
pub const CALL: Duration = Duration::from_secs(10);

/// The longest a tenant's delete or move waits, in all, for the changes of
/// its read replicas under way to end, past which it is refused, 409,
/// nothing of it done: `reader_timeout`, one call to a node, where that is
/// shorter. Well inside the minute a global router waits for the region's
/// router to answer the delete it relays: a wait of every call a change
/// makes, one region after another, outlasted it.
const CHANGE_WAIT_MOST: Duration = Duration::from_secs(15);

/// How long a worker takes the primary a look resolved for a tenant at
/// home in another region as it stands: past it, it asks that region's
/// router again, for every such item it has left at once. Asked again for
/// each item, a worker made one call to the region an item where the look
/// made one a region.
const PRIMARY_FRESH: Duration = Duration::from_secs(1);

/// How many times in a row a read replica's lost copy is made again before
/// it is given up on ([`Router::may_remake`]).
pub const REMAKES: u32 = 3;

/// How long a read replica that did not answer a marked read is let be:
/// every read of its region goes home meanwhile, rather than each wait out
/// the replica first.
pub const RESTING: Duration = Duration::from_secs(5);

/// How often a router asks each read replica's node which tenants it
/// holds, once a call a node, apart from the looks: a copy gone -- the node wiped, the file
/// deleted by hand -- is made again there, though no marked read reached
/// the router keeping its record to say so: a standby router, or another
/// region's, routes the reads and keeps no record.
pub const CHECK_EVERY: Duration = Duration::from_secs(10);

/// The threads the check of the read replicas' nodes asks them on, at
/// most: in turn, each node that did not answer held the others up for its
/// [`CALL`].
const LOOKERS: usize = 8;

/// Where a tenant's primary is, as far as this router can tell.
#[derive(Clone)]
pub(crate) enum Primary {
    /// On the node at `addr` (`host:port`), in `region`: its name where
    /// this router's own directory holds it, the tenant's quota where the
    /// directory placing it keeps one -- which its read replicas are sent
    /// wherever they are recorded -- and its lineage where that directory
    /// knows it (`lineages`), which its read replicas are bound to, with
    /// the ones it succeeded (`was`), which they are bound to again.
    At {
        region: String,
        addr: String,
        node: Option<String>,
        quota: Option<String>,
        lineage: Option<String>,
        was: Vec<String>,
    },
    /// In no directory this router reads: deleted.
    Gone,
    /// Not known now -- a region's router not reached, a directory not
    /// arrived, a move under way -- and why.
    Unknown(String),
}

/// A change of one read replica under way (`reader_busy`): let go of as it
/// is dropped.
pub(crate) struct ReaderHold<'a> {
    router: &'a Router,
    key: String,
}

impl Drop for ReaderHold<'_> {
    fn drop(&mut self) {
        self.router
            .reader_busy
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}

/// The quotas the read replicas are to be sent, a node at a time
/// ([`Router::deliver_quotas`]). Sent by the look and by a quota request
/// themselves, a node that did not answer held each up for its
/// `reader_timeout` -- a move of another tenant waited 4.4 s behind one,
/// and after a router's restart, its sent quotas forgotten, every look
/// waited for each node cut off -- and a look's send, of the quota its
/// round began with, landed over the one a quota request had sent
/// meanwhile. Here what is wanted last is what is sent, by one sender a
/// node, each replica's quotas in the order they were wanted.
#[derive(Default)]
pub(crate) struct QuotaQueue {
    /// `node -> tenant -> quota` wanted, `None` for the node's own.
    wanted: BTreeMap<String, BTreeMap<String, Option<String>>>,
    /// The nodes a sender is under way to.
    sending: HashSet<String>,
    /// The replicas (`<tenant>/<node>`) whose quota did not land, said in
    /// the log once, and again once one does.
    said: HashSet<String>,
}

impl QuotaQueue {
    /// Wants `quota` sent to `tenant`'s read replica on `node`, over any
    /// wanted before it: whether that changed what is wanted.
    fn want(&mut self, tenant: &str, node: &str, quota: Option<&str>) -> bool {
        let slot = self.wanted.entry(node.to_string()).or_default();
        let quota = quota.map(str::to_string);
        if slot.get(tenant) == Some(&quota) {
            return false;
        }
        slot.insert(tenant.to_string(), quota);
        true
    }

    /// Whether a quota is wanted for `tenant`'s read replica on `node`.
    fn wants(&self, tenant: &str, node: &str) -> bool {
        self.wanted
            .get(node)
            .is_some_and(|m| m.contains_key(tenant))
    }

    /// The nodes with a quota wanted and no sender under way, each taken
    /// by the sender started for it now.
    fn idle(&mut self) -> Vec<String> {
        let idle: Vec<String> = self
            .wanted
            .keys()
            .filter(|n| !self.sending.contains(*n))
            .cloned()
            .collect();
        self.sending.extend(idle.iter().cloned());
        idle
    }

    /// What `node`'s sender sends next, taken off what is wanted; `None`
    /// once nothing is, the node let go in the same breath, so a quota
    /// wanted after it finds no sender under way and starts one.
    fn next(&mut self, node: &str) -> Option<(String, Option<String>)> {
        let next = self.wanted.get_mut(node).and_then(BTreeMap::pop_first);
        if self.wanted.get(node).is_some_and(BTreeMap::is_empty) {
            self.wanted.remove(node);
        }
        if next.is_none() {
            self.sending.remove(node);
        }
        next
    }

    /// A send that landed.
    fn landed(&mut self, tenant: &str, node: &str) {
        self.said.remove(&format!("{tenant}/{node}"));
    }

    /// A send that did not land: wanted again unless a newer one is -- an
    /// older quota never goes out after it -- and the node let go, its
    /// sender ended, to be started again at the next pass. Whether to say
    /// so in the log.
    fn failed(&mut self, node: &str, tenant: &str, quota: Option<String>) -> bool {
        self.wanted
            .entry(node.to_string())
            .or_default()
            .entry(tenant.to_string())
            .or_insert(quota);
        self.sending.remove(node);
        self.said.insert(format!("{tenant}/{node}"))
    }

    /// Nothing more for `tenant`'s read replica on `node`, taken off.
    fn forget(&mut self, tenant: &str, node: &str) {
        if let Some(m) = self.wanted.get_mut(node) {
            m.remove(tenant);
            if m.is_empty() {
                self.wanted.remove(node);
            }
        }
        self.said.remove(&format!("{tenant}/{node}"));
    }
}

impl Router {
    /// The region this router answers marked reads for: `--near`, or the
    /// region whose directory it keeps.
    pub(crate) fn near(&self) -> Option<&str> {
        self.cfg.near.as_deref().or(self.cfg.region.as_deref())
    }

    /// The read replica a read of `tenant` its client marked goes to: its
    /// node, address and mark. `None` where the router is near no region,
    /// the tenant lives in the one it is near, or it has no read replica
    /// there -- or no directory it reads places the tenant: a record
    /// another region's directory keeps outlives its tenant until that
    /// router's next look, and a marked read was answered from the deleted
    /// tenant's copy meanwhile. Nor one given up on ([`Self::may_remake`]),
    /// which holds no copy: every marked read of its region went there,
    /// was answered that the tenant was not on the node, and went home.
    pub(crate) fn reader_near(&self, tenant: &str) -> Option<(String, String, Arc<str>)> {
        let near = self.near()?;
        let dir = self.read_dir();
        if let Some(p) = dir.placement(tenant) {
            if dir.region_of(&p.node) == near {
                return None;
            }
        }
        let r = dir.reader_in(tenant, near)?;
        if !dir.exists(tenant)
            && !self
                .global
                .as_ref()
                .is_some_and(|g| crate::read(g).exists(tenant))
        {
            return None;
        }
        if self.given_up(&format!("{tenant}/{}", r.node)) {
            return None;
        }
        let n = dir.node(&r.node)?;
        Some((r.node.clone(), n.addr.clone(), self.mark(&n.token)))
    }

    /// Whether marked reads pass `node` by for now: it did not answer one
    /// less than [`RESTING`] ago.
    pub(crate) fn resting(&self, node: &str) -> bool {
        let mut resting = self.resting.lock().unwrap_or_else(|e| e.into_inner());
        match resting.get(node) {
            None => false,
            Some(until) if Instant::now() < *until => true,
            Some(_) => {
                resting.remove(node);
                false
            }
        }
    }

    /// Has marked reads pass `node` by for [`RESTING`]
    /// ([`crate::Config::replica_rest`]).
    pub(crate) fn rest(&self, node: &str) {
        self.resting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(node.to_string(), Instant::now() + self.cfg.replica_rest);
    }

    /// How long a read replica has to answer `req`: the wait its
    /// `Fenec-After` asks of the node -- `Fenec-Wait`, or the node's own
    /// -- and [`crate::Config::replica_timeout`] past it.
    pub(crate) fn replica_within(&self, req: &Request) -> Duration {
        let waits = req.header("fenec-after").is_some().then(|| {
            req.header("fenec-wait")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map_or(fenec_http::AFTER_WAIT, Duration::from_millis)
                .min(fenec_http::AFTER_LONGEST)
        });
        self.cfg.replica_timeout + waits.unwrap_or_default()
    }

    /// A marked read found `tenant` gone from its read replica's `node`:
    /// made there again at the next look, which comes now. The reads go
    /// home meanwhile.
    pub(crate) fn reader_lost(&self, tenant: &str, node: &str) {
        self.reader_lost
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(format!("{tenant}/{node}"));
        self.follow_soon();
    }

    /// Holds the read replica of `tenant` in `region` for a change: `None`
    /// where one is under way.
    fn hold_reader(&self, tenant: &str, region: &str) -> Option<ReaderHold<'_>> {
        let key = format!("{tenant}/{region}");
        let fresh = self
            .reader_busy
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone());
        // `then`, never `then_some`: a hold made where the key was taken
        // already is dropped at once, and its drop lets go of the other's
        // key -- under the lock this held, which the drop takes again: a
        // tenant's delete beside the look's re-point waited on itself for
        // good (a CI run's 120 s).
        fresh.then(|| ReaderHold { router: self, key })
    }

    fn held_elsewhere(tenant: &str, region: &str) -> Fail {
        Fail(
            409,
            format!(
                "a change of `{tenant}`'s read replica in {} is under way",
                region_name(region)
            ),
        )
    }

    /// `GET /_shard/tenants/<t>/read-replicas`: the read replicas this
    /// router's directory keeps of `tenant`.
    pub(crate) fn list_readers(&self, tenant: &str) -> Response {
        let items = self.reader_items(tenant);
        let changing = self.changing_said(tenant);
        Response::json(
            200,
            format!(
                "{{\"tenant\":{},\"read_replicas\":[{}],\"failed\":[],\"changing\":{changing}}}",
                quote(tenant),
                items.join(",")
            ),
        )
    }

    fn reader_items(&self, tenant: &str) -> Vec<String> {
        let dir = self.read_dir();
        dir.readers(tenant)
            .iter()
            .map(|r| {
                let mut item = reader_item(dir.region_of(&r.node), r);
                if self.given_up(&format!("{tenant}/{}", r.node)) {
                    item.pop();
                    item.push_str(",\"given_up\":true}");
                }
                item
            })
            .collect()
    }

    /// The same on the global router, where regions keep their own
    /// directory: its own, and each such region's, asked of its router. A
    /// region not reached is named in `failed`, 502, beside the others.
    pub(crate) fn gather_readers(&self, tenant: &str) -> Response {
        let mut items = self.reader_items(tenant);
        let mut changing = self.changing_said(tenant);
        let mut failed = Vec::new();
        let regions: Vec<String> = self.read_dir().regions().keys().cloned().collect();
        let target = format!("/_shard/tenants/{tenant}/read-replicas");
        for region in regions {
            let answer =
                self.ask_region(&region, "GET", &target, &[], b"")
                    .and_then(|(status, _, body)| match status {
                        200 => Ok(body),
                        s => Err(Fail(s, String::from_utf8_lossy(&body).into_owned())),
                    });
            let listed = answer.and_then(|body| {
                let fields = fenec_core::json::parse_object(&String::from_utf8_lossy(&body))
                    .map_err(|e| Fail(502, e.to_string()))?;
                changing = match (changing, fields.iter().find(|(k, _)| k == "changing")) {
                    ("true", _) | (_, Some((_, Value::Bool(true)))) => "true",
                    (_, Some((_, Value::Bool(false)))) => changing,
                    _ => "null",
                };
                Ok(fields
                    .into_iter()
                    .find(|(k, _)| k == "read_replicas")
                    .map(|(_, v)| v))
            });
            match listed {
                Ok(Some(Value::List(list))) => {
                    for v in list {
                        let mut item = String::new();
                        fenec_core::json::value_into(&mut item, &v);
                        items.push(item);
                    }
                }
                Ok(_) => {}
                Err(Fail(_, why)) => failed.push(format!(
                    "{{\"region\":{},\"why\":{}}}",
                    quote(&region),
                    quote(&why)
                )),
            }
        }
        Response::json(
            if failed.is_empty() { 200 } else { 502 },
            format!(
                "{{\"tenant\":{},\"read_replicas\":[{}],\"failed\":[{}],\"changing\":{changing}}}",
                quote(tenant),
                items.join(","),
                failed.join(",")
            ),
        )
    }

    /// `PUT /_shard/tenants/<t>/read-replicas/<region>`: a read replica of
    /// `tenant` in `region`, on a node of this router's directory -- 201,
    /// or 200 where it has one there already. Made and told to follow
    /// before it is recorded: recorded first, a marked read could reach the
    /// file the `PUT` makes, a primary of its own holding nothing. Holds no
    /// claim on the tenant across the calls to the node, a region away: a
    /// move or a failover of the tenant meanwhile goes on, and the next
    /// look ([`Self::follow_round`]) fits the replica to it. A delete waits
    /// it out, holding the region's changes ([`Self::readers_gone`]).
    pub(crate) fn place_reader(&self, tenant: &str, region: &str) -> Outcome<Response> {
        fenec_http::tenants::check_name(region).map_err(|_| {
            Fail(
                400,
                format!(
                    "a region is named [a-z0-9_-], 1 to {} characters",
                    fenec_http::tenants::MAX_NAME
                ),
            )
        })?;
        let _hold = self
            .hold_reader(tenant, region)
            .ok_or_else(|| Router::held_elsewhere(tenant, region))?;
        if let Some(r) = self.read_dir().reader_in(tenant, region).cloned() {
            return Ok(Response::json(200, reader_item(region, &r)));
        }
        let (addr, quota, lineage) = match self.primary_of(tenant) {
            Primary::At { region: home, .. } if home == region => {
                return Err(Fail(
                    409,
                    format!(
                        "`{tenant}` lives in {}: a read replica goes in another region, its \
                         own copies are its standby and its replica",
                        region_name(&home)
                    ),
                ))
            }
            Primary::At {
                addr,
                quota,
                lineage,
                ..
            } => (addr, quota, lineage),
            Primary::Gone => {
                return Err(Fail(404, format!("no tenant `{tenant}` in the directory")))
            }
            Primary::Unknown(why) => {
                return Ok(Response::retry_after(
                    503,
                    &format!("where `{tenant}`'s primary is is not known here now: {why}"),
                    1,
                ))
            }
        };
        let node = self.reader_node(tenant, region)?;
        let url = format!("http://{addr}");
        // Its trace in the global directory first, before any copy exists:
        // a create of the name asks the regions traced there alone.
        self.trace_globally(tenant, "PUT").map_err(|w| {
            Fail(
                503,
                format!(
                    "a read replica here is made once the global router knows this region \
                     may keep one of `{tenant}`: {w}"
                ),
            )
        })?;
        if let Err(w) = self.tell_follow(tenant, &node, &url, true, lineage.as_deref()) {
            self.untrace_if_none(tenant);
            return Err(Fail(502, w));
        }
        // The tenant's quota, as its standby and its replica are sent it: a
        // read replica counts the streams and the requests its region
        // sends. One that does not reach it is sent it again by a look.
        if let Err(w) = self.send_reader_quota(tenant, &node, quota.as_deref()) {
            fenec_http::log!("tenant `{tenant}`'s read replica on `{node}` has no quota yet: {w}");
        }
        // Bound apart: in an `if let`, the directory's write guard was held
        // into the block, whose taking the copy off reads the directory --
        // a directory refusing writes, its disk failing, held the router.
        let recorded = self
            .write_dir()
            .set_reader(tenant, &node, &url, lineage.as_deref());
        if let Err(e) = recorded {
            let r = Reader {
                node,
                follows: url,
                lineage,
            };
            let _ = self.delete_copy(tenant, &r);
            self.untrace_if_none(tenant);
            return Err(internal(e));
        }
        let reader = Reader {
            node,
            follows: url,
            lineage,
        };
        Ok(Response::json(201, reader_item(region, &reader)))
    }

    /// `DELETE /_shard/tenants/<t>/read-replicas/<region>`: the read
    /// replica out of the directory, then off its node. A node that does
    /// not take it off keeps it recorded, 502 -- a copy left following
    /// with no record is a follower nothing would ever stop -- unless
    /// `?forget`, for a node gone for good: its record goes, and its copy,
    /// should the node come back, follows unread until deleted there.
    pub(crate) fn drop_reader(
        &self,
        tenant: &str,
        region: &str,
        req: &Request,
    ) -> Outcome<Response> {
        let _hold = self
            .hold_reader(tenant, region)
            .ok_or_else(|| Router::held_elsewhere(tenant, region))?;
        let r = self
            .read_dir()
            .reader_in(tenant, region)
            .cloned()
            .ok_or_else(|| {
                Fail(
                    404,
                    format!("`{tenant}` has no read replica in {}", region_name(region)),
                )
            })?;
        if req.query.iter().any(|(k, _)| k == "forget") {
            self.write_dir()
                .remove_reader(tenant, &r.node)
                .map_err(internal)?;
            self.forget_reader(tenant, &r.node);
            self.remakes_done(&format!("{tenant}/{}", r.node));
            self.untrace_if_none(tenant);
            return Ok(Response::empty(204));
        }
        self.take_reader(tenant, &r).map_err(|w| {
            Fail(
                502,
                format!("{w}: it stays recorded; DELETE again, or with ?forget for a node gone"),
            )
        })?;
        Ok(Response::empty(204))
    }

    /// Takes the read replica `r` of `tenant` out of the directory, and,
    /// once the standby routers have been sent that, off its node -- in
    /// that order, so that no marked read is sent to a copy being deleted.
    /// A node that does not take it off has the record put back, and what
    /// went wrong is returned. The file stays where the tenant itself has
    /// come to live on the node. Called with the replica held
    /// ([`Self::hold_reader`]).
    fn take_reader(&self, tenant: &str, r: &Reader) -> std::result::Result<(), String> {
        self.write_dir()
            .remove_reader(tenant, &r.node)
            .map_err(|e| message(&e))?;
        self.standbys_sent();
        if let Err(w) = self.delete_copy(tenant, r) {
            let put_back =
                self.write_dir()
                    .set_reader(tenant, &r.node, &r.follows, r.lineage.as_deref());
            if let Err(e) = put_back {
                fenec_http::log!(
                    "tenant `{tenant}`'s read replica on `{}` stayed on its node, unrecorded: {e}",
                    r.node
                );
            }
            return Err(w);
        }
        self.forget_reader(tenant, &r.node);
        self.remakes_done(&format!("{tenant}/{}", r.node));
        self.untrace_if_none(tenant);
        Ok(())
    }

    /// Deletes `tenant`'s copy on `r`'s node, unless the tenant itself has
    /// come to live there.
    fn delete_copy(&self, tenant: &str, r: &Reader) -> std::result::Result<(), String> {
        let (n, home) = {
            let dir = self.read_dir();
            let home = dir.placement(tenant).is_some_and(|p| p.node == r.node);
            (dir.node(&r.node).cloned(), home)
        };
        let (Some(n), false) = (n, home) else {
            return Ok(());
        };
        let target = format!("/_admin/tenants/{tenant}");
        match self
            .reader_pool
            .call(&n.addr, "DELETE", &target, &n.token, b"")
        {
            Ok((204 | 404, _)) => Ok(()),
            Ok((status, body)) => Err(format!(
                "`{}` answered {status}: {}",
                r.node,
                String::from_utf8_lossy(&body).trim()
            )),
            Err(e) => Err(format!("`{}` did not answer: {e}", r.node)),
        }
    }

    /// Sends the read replica of `tenant` on `node` the tenant's quota --
    /// `{}`, the node's own, where none is set -- and notes it sent, so a
    /// look sends it again only once it changes.
    pub(crate) fn send_reader_quota(
        &self,
        tenant: &str,
        node: &str,
        quota: Option<&str>,
    ) -> std::result::Result<(), String> {
        let n = self
            .read_dir()
            .node(node)
            .cloned()
            .ok_or_else(|| format!("no node `{node}`"))?;
        let target = format!("/_admin/tenants/{tenant}/quota");
        let body = quota.unwrap_or("{}");
        match self
            .reader_pool
            .call(&n.addr, "PUT", &target, &n.token, body.as_bytes())
        {
            Ok((200, _)) => {
                self.noted_quota(tenant, node, quota);
                Ok(())
            }
            Ok((status, body)) => Err(format!(
                "`{node}` answered {status}: {}",
                String::from_utf8_lossy(&body).trim()
            )),
            Err(e) => Err(format!("`{node}` did not answer: {e}")),
        }
    }

    /// Notes the quota the read replica of `tenant` on `node` holds.
    pub(crate) fn noted_quota(&self, tenant: &str, node: &str, quota: Option<&str>) {
        self.reader_quotas
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(format!("{tenant}/{node}"), quota.map(str::to_string));
    }

    /// Whether the read replica of `tenant` on `node` holds `quota`, as far
    /// as this router has sent it one.
    fn holds_quota(&self, tenant: &str, node: &str, quota: Option<&str>) -> bool {
        self.reader_quotas
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&format!("{tenant}/{node}"))
            .is_some_and(|q| q.as_deref() == quota)
    }

    /// Wants `quota` sent to `tenant`'s read replica on `node`, unless it
    /// holds it and nothing else is wanted: the sender of its node starts,
    /// or takes it in, at once ([`Self::deliver_quotas`]). No call is made
    /// here.
    pub(crate) fn want_reader_quota(&self, tenant: &str, node: &str, quota: Option<&str>) {
        let mut q = self
            .reader_quota_queue
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let changed = self.want_in(&mut q, tenant, node, quota);
        drop(q);
        if changed {
            self.quotas_soon();
        }
    }

    /// [`Self::want_reader_quota`] of the quota this directory keeps for
    /// `tenant`, read under the queue's lock: read before a quota request
    /// wrote and wanted after it wanted its own, a look's would have gone
    /// out after the request's, an older quota over a newer one.
    fn want_kept_quota(&self, tenant: &str, node: &str) {
        let mut q = self
            .reader_quota_queue
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let quota = self.read_dir().quota(tenant).map(str::to_string);
        let changed = self.want_in(&mut q, tenant, node, quota.as_deref());
        drop(q);
        if changed {
            self.quotas_soon();
        }
    }

    fn want_in(&self, q: &mut QuotaQueue, tenant: &str, node: &str, quota: Option<&str>) -> bool {
        if !q.wants(tenant, node) && self.holds_quota(tenant, node, quota) {
            return false;
        }
        q.want(tenant, node, quota)
    }

    fn quotas_soon(&self) {
        let (due, wake) = &self.quotas_soon;
        *due.lock().unwrap_or_else(|e| e.into_inner()) = true;
        wake.notify_all();
    }

    /// Starts a sender for each node with a quota wanted and none under
    /// way, on a thread of its own: a node that does not answer holds up
    /// its own replicas' quotas alone.
    pub(crate) fn deliver_quotas(self: &Arc<Router>) {
        let idle = self
            .reader_quota_queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .idle();
        for node in idle {
            let router = Arc::clone(self);
            let n = node.clone();
            let started = std::thread::Builder::new()
                .name("fenec-readers-quota".into())
                .spawn(move || router.send_quotas(&n));
            if started.is_err() {
                // Let go, so the next pass starts it again.
                self.reader_quota_queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .sending
                    .remove(&node);
            }
        }
    }

    /// `node`'s sender: each quota wanted for its read replicas, the
    /// newest of each, sent in turn until none is left or one does not
    /// land -- wanted again then, for the next pass.
    fn send_quotas(&self, node: &str) {
        loop {
            let next = self
                .reader_quota_queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .next(node);
            let Some((tenant, quota)) = next else {
                return;
            };
            // Taken off since: nothing to hold to it.
            if !self
                .read_dir()
                .readers(&tenant)
                .iter()
                .any(|r| r.node == node)
            {
                continue;
            }
            match self.send_reader_quota(&tenant, node, quota.as_deref()) {
                Ok(()) => self
                    .reader_quota_queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .landed(&tenant, node),
                Err(w) => {
                    let say = self
                        .reader_quota_queue
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .failed(node, &tenant, quota);
                    if say {
                        fenec_http::log!(
                            "tenant `{tenant}`'s read replica on `{node}` is without its \
                             quota yet: {w}"
                        );
                    }
                    return;
                }
            }
        }
    }

    /// What this router keeps of a read replica taken off.
    fn forget_reader(&self, tenant: &str, node: &str) {
        let key = format!("{tenant}/{node}");
        self.unfollowed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        self.reader_lost
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        self.reader_quotas
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        self.reader_quota_queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .forget(tenant, node);
    }

    /// Holds the read replica of `tenant` in `region` for the move that
    /// brings the tenant into that region, and takes it off: its own copies
    /// there are nearer still -- and one may be about to land on that very
    /// node. A change of it under way -- a look telling it to follow the
    /// tenant's primary, a placement -- is waited out first, and the hold
    /// kept by the move until the directory places the tenant there:
    /// passed over, a look's `follow` landing on the node after the image
    /// was installed there made the tenant's new primary a follower of the
    /// node it had left, which the move then deleted, and the tenant took
    /// no write again. One still under way past [`CHANGE_WAIT_MOST`]
    /// refuses the move, 409, before anything of it is done. One whose node
    /// does not answer stays, and the next look takes it off as it finds
    /// the tenant in its region.
    pub(crate) fn readers_arriving(&self, tenant: &str, region: &str) -> Outcome<ReaderHold<'_>> {
        let hold = self
            .wait_reader(tenant, region, self.change_deadline())
            .ok_or_else(|| Router::held_elsewhere(tenant, region))?;
        // Bound apart: in an `if let`, the directory's guard would be held
        // into the block, whose taking off writes the directory.
        let recorded = self.read_dir().reader_in(tenant, region).cloned();
        if let Some(r) = recorded {
            if let Err(w) = self.take_reader(tenant, &r) {
                fenec_http::log!(
                    "tenant `{tenant}`'s read replica on `{}` stayed as its tenant moves into \
                     its region: {w}",
                    r.node
                );
            }
        }
        Ok(hold)
    }

    /// Holds every change of `tenant`'s read replicas, in each region a
    /// node is in, for its delete: a change under way in one -- a look
    /// re-pointing a replica, one taken off whose record is out until its
    /// node answers, one being placed -- waited out first, for
    /// [`CHANGE_WAIT_MOST`] at most in all, past which the delete is
    /// refused, 409. Kept until the tenant is gone: passed over, a change
    /// under way left a record after the delete's 204, a marked read
    /// answered from the deleted tenant's copy until the next look took it
    /// off; let go of before the tenant is, a placement could record one
    /// after. Waited for with no claim on the tenant held, which a failover
    /// of it would have found taken, and passed it over.
    pub(crate) fn hold_readers(&self, tenant: &str) -> Outcome<Vec<ReaderHold<'_>>> {
        let regions: std::collections::BTreeSet<String> = self
            .read_dir()
            .nodes()
            .values()
            .map(|n| n.region.clone())
            .collect();
        let until = self.change_deadline();
        let mut holds = Vec::with_capacity(regions.len());
        for region in &regions {
            let hold = self
                .wait_reader(tenant, region, until)
                .ok_or_else(|| Router::held_elsewhere(tenant, region))?;
            holds.push(hold);
        }
        Ok(holds)
    }

    /// Takes off every read replica of `tenant` this directory keeps, the
    /// tenant to be deleted, each held ([`Self::hold_readers`]) and all at
    /// once, so the delete waits for the slowest node alone. One whose node
    /// does not take it off refuses the delete, 502, the tenant kept, as
    /// `DELETE .../read-replicas/<region>` keeps its record: deleted
    /// regardless, the record outlived its tenant, and the look that found
    /// the name made again -- by another owner, on another node -- pointed
    /// the old copy at it, which took the new tenant's image into a region
    /// nobody had chosen. Another directory's go at its router's next look,
    /// which finds the tenant gone; a create of the name waits for that
    /// ([`Self::reader_traces`]).
    pub(crate) fn take_readers(&self, tenant: &str) -> Outcome<()> {
        let readers: Vec<Reader> = self.read_dir().readers(tenant).to_vec();
        let stayed: Vec<(Reader, String)> = match readers.len() {
            0 => Vec::new(),
            1 => readers
                .into_iter()
                .filter_map(|r| self.take_reader(tenant, &r).err().map(|w| (r, w)))
                .collect(),
            _ => std::thread::scope(|s| {
                let taking: Vec<_> = readers
                    .iter()
                    .map(|r| (r, s.spawn(move || self.take_reader(tenant, r))))
                    .collect();
                taking
                    .into_iter()
                    .filter_map(|(r, h)| {
                        let w = match h.join() {
                            Ok(taken) => taken.err()?,
                            Err(_) => "its taking off failed".to_string(),
                        };
                        Some((r.clone(), w))
                    })
                    .collect()
            }),
        };
        if stayed.is_empty() {
            return Ok(());
        }
        let named: Vec<String> = stayed
            .iter()
            .map(|(r, w)| {
                let region = self.read_dir().region_of(&r.node).to_string();
                format!("{} on `{}`: {w}", region_name(&region), r.node)
            })
            .collect();
        Err(Fail(
            502,
            format!(
                "`{tenant}` is kept: its read replica stayed ({}). DELETE it again once the \
                 node answers, or take a replica off a node gone for good first, with \
                 DELETE /_shard/tenants/{tenant}/read-replicas/<region>?forget",
                named.join("; ")
            ),
        ))
    }

    /// Whether this router keeps a trace of a read replica of `tenant`: a
    /// record, or a change of one under way -- whose record is out while
    /// its node is asked to take the copy off. A create of the name is
    /// refused while one is ([`Router::create`]): the look keeping it would
    /// point the copy at the new tenant.
    pub(crate) fn reader_traces(&self, tenant: &str) -> bool {
        !self.read_dir().readers(tenant).is_empty() || self.changing_readers(tenant)
    }

    /// [`Self::changing_readers`] as the list says it: `null` on a router
    /// following another, whose own holds say nothing of the changes the
    /// one it follows has under way -- `false` there let a create of the
    /// name go on while the primary took a replica off.
    fn changing_said(&self, tenant: &str) -> &'static str {
        match (self.read_dir().following(), self.changing_readers(tenant)) {
            (true, _) => "null",
            (false, true) => "true",
            (false, false) => "false",
        }
    }

    /// Whether a change of a read replica of `tenant` is under way here.
    fn changing_readers(&self, tenant: &str) -> bool {
        let prefix = format!("{tenant}/");
        self.reader_busy
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|k| k.starts_with(&prefix))
    }

    /// Refuses a create of `tenant`, 409, while a trace of a read replica
    /// of the name is kept ([`Self::reader_traces`]): here, or -- on the
    /// global router -- by a region keeping its own directory that its
    /// router said may keep one (`traces`, [`Self::trace_globally`]), each
    /// such region asked at once and bounded by `reader_timeout`; one not
    /// reached, or whose router answering cannot say what is under way,
    /// refuses it too, 503. A deleted tenant's replica in another region's
    /// directory goes at that router's next look; made again before it,
    /// the name was another owner's, and the look pointed the old copy at
    /// the new tenant, which took its image into a region nobody had
    /// chosen. Every region was asked, one after another: one out of reach
    /// refused every create in every region, the one a partition is meant
    /// to leave working. A name no region traced asks none, and a region
    /// that says it keeps none has its trace taken off.
    pub(crate) fn no_reader_traces(&self, tenant: &str) -> Outcome<()> {
        let kept = |whose: &str| {
            Fail(
                409,
                format!(
                    "a read replica of a tenant `{tenant}` is still kept {whose}: it goes once \
                     its node answers, or with DELETE \
                     /_shard/tenants/{tenant}/read-replicas/<region>?forget for a node gone for \
                     good; the name is free after"
                ),
            )
        };
        if self.reader_traces(tenant) {
            return Err(kept("here"));
        }
        let asked: Vec<(String, Vec<String>)> = {
            let dir = self.read_dir();
            let traced = dir.traced_in(tenant).map_err(internal)?;
            traced
                .into_iter()
                .filter_map(|r| dir.routers(&r).map(|rs| (r, rs.to_vec())))
                .collect()
        };
        let answers: Vec<(String, Traced)> = match asked.len() {
            0 => return Ok(()),
            1 => asked
                .iter()
                .map(|(r, rs)| (r.clone(), self.region_traces(tenant, r, rs)))
                .collect(),
            _ => std::thread::scope(|s| {
                let asking: Vec<_> = asked
                    .iter()
                    .map(|(r, rs)| (r, s.spawn(move || self.region_traces(tenant, r, rs))))
                    .collect();
                asking
                    .into_iter()
                    .map(|(r, h)| {
                        let t = h
                            .join()
                            .unwrap_or_else(|_| Traced::Unknown("asking it failed".into()));
                        (r.clone(), t)
                    })
                    .collect()
            }),
        };
        if let Some((r, _)) = answers.iter().find(|(_, t)| matches!(t, Traced::Kept)) {
            return Err(kept(&format!("in {}", region_name(r))));
        }
        if let Some((r, Traced::Unknown(why))) = answers
            .iter()
            .find(|(_, t)| matches!(t, Traced::Unknown(_)))
        {
            return Err(Fail(
                503,
                format!(
                    "{} may keep a read replica of a tenant `{tenant}`, and whether it does is \
                     not known ({why}): the name is made once its router answers. For a region \
                     gone for good, DELETE /_shard/tenants/{tenant}/traces/{r} lets go of its \
                     trace: a copy left there is of the old tenant's lineage, takes nothing \
                     from a new one, and deletes itself",
                    region_name(r)
                ),
            ));
        }
        for (r, _) in &answers {
            if let Err(e) = self.write_dir().remove_trace(tenant, r) {
                fenec_http::log!("the trace of `{tenant}` in {} stayed: {e}", region_name(r));
            }
        }
        Ok(())
    }

    /// Whether `region`'s router keeps a trace of a read replica of
    /// `tenant`, asked of `routers` for its list.
    fn region_traces(&self, tenant: &str, region: &str, routers: &[String]) -> Traced {
        let target = format!("/_shard/tenants/{tenant}/read-replicas");
        let (auth, global) = self.own_headers();
        let mut headers: Vec<(&str, &str)> = global.to_vec();
        if let Some(a) = &auth {
            headers.push(("Authorization", a));
        }
        let call = ("GET", target.as_str(), &headers[..], &b""[..]);
        match self.call_routers_by(&self.reader_pool, region, routers, call) {
            Ok((200, _, body)) => {
                match fenec_core::json::parse_object(&String::from_utf8_lossy(&body)) {
                    Ok(fields) => traced(&fields),
                    Err(e) => Traced::Unknown(format!("its answer: {e}")),
                }
            }
            Ok((status, _, body)) => Traced::Unknown(format!(
                "its router answered {status}: {}",
                String::from_utf8_lossy(&body).trim()
            )),
            Err(e) => Traced::Unknown(e),
        }
    }

    /// Tells the global router, from a region's, whether this region may
    /// keep a read replica of `tenant`'s name (`PUT`) or keeps none
    /// (`DELETE`): the `traces` a create of the name asks by. Nothing on a
    /// router keeping no region's directory, which a create asks itself.
    pub(crate) fn trace_globally(
        &self,
        tenant: &str,
        method: &str,
    ) -> std::result::Result<(), String> {
        let Some(region) = self.cfg.region.as_deref() else {
            return Ok(());
        };
        if self.cfg.global.is_empty() {
            return Ok(());
        }
        let target = format!("/_shard/tenants/{tenant}/traces/{region}");
        let auth = self.cfg.token.as_ref().map(|t| format!("Bearer {t}"));
        let mut headers: Vec<(&str, &str)> = Vec::new();
        if let Some(a) = &auth {
            headers.push(("Authorization", a));
        }
        let routers = self.cfg.global.clone();
        let call = (method, target.as_str(), &headers[..], &b""[..]);
        match self.call_routers_by(&self.reader_pool, "", &routers, call) {
            Ok((200 | 204, _, _)) => Ok(()),
            Ok((status, _, body)) => Err(format!(
                "the global router answered {status}: {}",
                String::from_utf8_lossy(&body).trim()
            )),
            Err(e) => Err(format!("the global router was not reached: {e}")),
        }
    }

    /// On a region's router, once it keeps no read replica of `tenant`:
    /// the global router told, so a create of the name asks this region no
    /// more. Not told -- out of reach -- the trace stays, and the create
    /// asks, finds none, and takes it off. Called with the replica held.
    fn untrace_if_none(&self, tenant: &str) {
        if self.cfg.region.is_none() || !self.read_dir().readers(tenant).is_empty() {
            return;
        }
        if let Err(w) = self.trace_globally(tenant, "DELETE") {
            fenec_http::log!(
                "tenant `{tenant}`'s read replicas here are gone, and the global router was not \
                 told: {w}"
            );
        }
    }

    /// `PUT /_shard/tenants/<t>/traces/<region>`: a region's router saying
    /// it may keep a read replica of `tenant`'s name.
    /// Refused, 404, for a name the directory holds no tenant of: a
    /// region's router whose copy of this directory still showed a tenant
    /// just deleted traced and placed a replica of it as the name was made
    /// again, the create having asked before the trace landed. Held under
    /// the tenant's claim, as a create of the name is.
    pub(crate) fn set_trace(&self, tenant: &str, region: &str) -> Outcome<Response> {
        let _claim = self.claim(tenant)?;
        if !self.read_dir().exists(tenant) {
            return Err(Fail(
                404,
                format!("no tenant `{tenant}`: no read replica of it is placed"),
            ));
        }
        self.write_dir()
            .set_trace(tenant, region)
            .map_err(internal)?;
        self.standbys_sent();
        Ok(Response::empty(204))
    }

    /// `DELETE /_shard/tenants/<t>/traces/<region>`: one saying it keeps
    /// none.
    pub(crate) fn drop_trace(&self, tenant: &str, region: &str) -> Outcome<Response> {
        self.write_dir()
            .remove_trace(tenant, region)
            .map_err(internal)?;
        Ok(Response::empty(204))
    }

    /// When a delete or a move gives up waiting for a change of a read
    /// replica under way: `reader_timeout`, one call to its node, or
    /// [`CHANGE_WAIT_MOST`], whichever comes first.
    fn change_deadline(&self) -> Instant {
        Instant::now() + self.cfg.reader_timeout.min(CHANGE_WAIT_MOST)
    }

    /// [`Self::hold_reader`], waiting out a change under way until `until`:
    /// `None` past it.
    fn wait_reader(&self, tenant: &str, region: &str, until: Instant) -> Option<ReaderHold<'_>> {
        loop {
            if let Some(hold) = self.hold_reader(tenant, region) {
                return Some(hold);
            }
            if Instant::now() >= until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The node of `region` a read replica of `tenant` goes on: the one
    /// holding the fewest tenants and copies, then by name -- no node's
    /// disk asked, as a replica's node is chosen ([`Router::replica_node`]).
    /// Never a standby, whose files follow its node's.
    fn reader_node(&self, tenant: &str, region: &str) -> Outcome<String> {
        let dir = self.read_dir();
        let (tenants, _) = dir.load_by_node();
        let replicas = dir.replicas_by_node();
        let readers = dir.readers_by_node();
        let held = |n: &str| {
            tenants.get(n).unwrap_or(&0)
                + replicas.get(n).unwrap_or(&0)
                + readers.get(n).unwrap_or(&0)
        };
        let mut there = dir
            .nodes()
            .iter()
            .filter(|(_, n)| n.region == region)
            .peekable();
        if there.peek().is_none() {
            return Err(Fail(
                409,
                format!(
                    "no node in {} here: add one with PUT /_shard/nodes/<name> \
                     {{\"addr\": .., \"token\": .., \"region\": \"{region}\"}}",
                    region_name(region)
                ),
            ));
        }
        there
            .map(|(k, _)| k)
            .filter(|k| !dir.is_standby(k) && dir.replica(tenant) != Some(k.as_str()))
            .min_by_key(|k| (held(k), k.as_str()))
            .cloned()
            .ok_or_else(|| {
                Fail(
                    409,
                    format!(
                        "no node in {} takes a read replica: a standby takes none",
                        region_name(region)
                    ),
                )
            })
    }

    /// Has the read replica of `tenant` on `node` follow the primary at
    /// `url` (`http://host:port`): made there first where `create` -- or
    /// where the node has no file of it, a node wiped -- then told to
    /// follow, as a replica of its own is ([`Router::attach`]). A file made
    /// here that then would not follow is deleted again: left, it was a
    /// primary of its own holding nothing, which a marked read reaching it
    /// would have taken for the tenant.
    ///
    /// `lineage`, the tenant's where known, goes with the follow: the copy
    /// takes nothing from a primary of another ([`fenec_http::replication::Follower::expect_lineage`])
    /// and deletes itself, refusing it across a restart -- whatever this router
    /// records or forgets, a tenant made again under the name, on the very
    /// node, is never copied into a region nobody chose for it.
    fn tell_follow(
        &self,
        tenant: &str,
        node: &str,
        url: &str,
        create: bool,
        lineage: Option<&str>,
    ) -> std::result::Result<(), String> {
        let n = self
            .read_dir()
            .node(node)
            .cloned()
            .ok_or_else(|| format!("no node `{node}`"))?;
        let base = format!("/_admin/tenants/{tenant}");
        let body = match lineage {
            Some(l) => format!("{{\"from\":{},\"lineage\":{}}}", quote(url), quote(l)),
            None => format!("{{\"from\":{}}}", quote(url)),
        };
        // Named, so a node a newer term leases refuses to follow at the
        // word of a router it superseded.
        let holder = self.holder_header();
        let named = [(fenec_http::lease::HOLDER_HEADER, holder.as_str())];
        let call = |method: &str, target: &str, body: &[u8]| {
            self.reader_pool
                .call_with(&n.addr, method, target, &n.token, &named, body)
                .map_err(|e| format!("`{node}` did not answer: {e}"))
        };
        let answered = |status: u16, body: Vec<u8>| {
            format!(
                "`{node}` answered {status}: {}",
                String::from_utf8_lossy(&body).trim()
            )
        };
        let follow = format!("{base}/follow");
        if !create {
            match call("POST", &follow, body.as_bytes())? {
                (200, _) => return Ok(()),
                (404, _) => {}
                (status, body) => return Err(answered(status, body)),
            }
        }
        let made = match call("PUT", &base, b"")? {
            (201, _) => true,
            (409, _) => false,
            (status, body) => return Err(answered(status, body)),
        };
        let followed = match call("POST", &follow, body.as_bytes()) {
            Ok((200, _)) => return Ok(()),
            Ok((status, body)) => answered(status, body),
            Err(w) => w,
        };
        if made && !matches!(call("DELETE", &base, b""), Ok((204 | 404, _))) {
            fenec_http::log!(
                "tenant `{tenant}`'s copy made on `{node}` for a read replica would not follow \
                 ({followed}) and stayed: nothing routes to it"
            );
        }
        Err(followed)
    }

    /// Where `tenant`'s primary is: [`Self::primaries_of`] of one.
    pub(crate) fn primary_of(&self, tenant: &str) -> Primary {
        self.primaries_of(&[tenant.to_string()])
            .remove(tenant)
            .unwrap_or_else(|| Primary::Unknown("not looked up".into()))
    }

    /// Where each of `tenants`' primary is: in this router's own directory,
    /// its copy of the global directory, or -- for a tenant at home in a
    /// region keeping its own -- as that region's router says, one call a
    /// region for all of its tenants asked.
    pub(crate) fn primaries_of(&self, tenants: &[String]) -> HashMap<String, Primary> {
        let mut out = HashMap::new();
        let mut remote: BTreeMap<String, Vec<String>> = BTreeMap::new();
        {
            let dir = self.read_dir();
            let global = self.global.as_ref().map(crate::read);
            let at = |n: &crate::directory::Node,
                      node: Option<&str>,
                      quota: Option<&str>,
                      lineage: Option<String>,
                      was: &[String]| Primary::At {
                region: n.region.clone(),
                addr: n.addr.clone(),
                node: node.map(str::to_string),
                quota: quota.map(str::to_string),
                lineage,
                was: was.to_vec(),
            };
            for t in tenants {
                if let Some(p) = dir.placement(t) {
                    let found = match dir.node(&p.node) {
                        Some(n) => at(
                            n,
                            Some(&p.node),
                            dir.quota(t),
                            dir.lineage(t),
                            dir.former_lineages(t),
                        ),
                        None => Primary::Unknown(format!("no node `{}` here", p.node)),
                    };
                    out.insert(t.clone(), found);
                    continue;
                }
                if let Some(r) = dir.home(t) {
                    remote.entry(r.to_string()).or_default().push(t.clone());
                    continue;
                }
                let Some(g) = &global else {
                    let found = match dir.arrived() {
                        true => Primary::Gone,
                        false => Primary::Unknown("the directory has not arrived yet".into()),
                    };
                    out.insert(t.clone(), found);
                    continue;
                };
                if let Some(p) = g.placement(t) {
                    let found = match g.node(&p.node) {
                        Some(n) => at(n, None, g.quota(t), g.lineage(t), g.former_lineages(t)),
                        None => Primary::Unknown(format!("no node `{}` known", p.node)),
                    };
                    out.insert(t.clone(), found);
                    continue;
                }
                match g.home(t) {
                    // At home here, and not in this directory: being made,
                    // or moving in.
                    Some(r) if Some(r) == self.cfg.region.as_deref() => {
                        out.insert(
                            t.clone(),
                            Primary::Unknown("it is moving into this region".into()),
                        );
                    }
                    Some(r) => remote.entry(r.to_string()).or_default().push(t.clone()),
                    None if dir.arrived() && g.arrived() => {
                        out.insert(t.clone(), Primary::Gone);
                    }
                    None => {
                        out.insert(
                            t.clone(),
                            Primary::Unknown("the global directory has not arrived yet".into()),
                        );
                    }
                }
            }
        }
        for (region, names) in remote {
            match self.ask_placements(&region, &names, false) {
                Ok(mut found) => {
                    for t in names {
                        let p = found.remove(&t).unwrap_or_else(|| {
                            Primary::Unknown(format!(
                                "{}'s router places it nowhere: it is moving, or being made",
                                region_name(&region)
                            ))
                        });
                        out.insert(t, p);
                    }
                }
                Err(why) => {
                    for t in names {
                        out.insert(t, Primary::Unknown(why.clone()));
                    }
                }
            }
        }
        out
    }

    /// `region`'s routers' word on where each of `names` is placed in its
    /// directory (`POST /_shard/placements`), each primary's lineage asked
    /// of its node first where `verify`.
    fn ask_placements(
        &self,
        region: &str,
        names: &[String],
        verify: bool,
    ) -> std::result::Result<HashMap<String, Primary>, String> {
        let routers = {
            let own = self.read_dir().routers(region).map(<[String]>::to_vec);
            own.or_else(|| {
                let g = self.global.as_ref().map(crate::read)?;
                g.routers(region).map(<[String]>::to_vec)
            })
        }
        .ok_or_else(|| format!("no router is recorded for {}", region_name(region)))?;
        self.ask_placements_of(region, &routers, names, verify)
    }

    /// [`Self::ask_placements`] of the routers `routers`: a region's, or
    /// the global router's (`region` empty).
    fn ask_placements_of(
        &self,
        region: &str,
        routers: &[String],
        names: &[String],
        verify: bool,
    ) -> std::result::Result<HashMap<String, Primary>, String> {
        let region_name = |r: &str| match r {
            "" => "the global directory".to_string(),
            r => region_name(r),
        };
        let list: Vec<String> = names.iter().map(|n| quote(n)).collect();
        let body = match verify {
            true => format!("{{\"tenants\":[{0}],\"verify\":[{0}]}}", list.join(",")),
            false => format!("{{\"tenants\":[{}]}}", list.join(",")),
        };
        let auth = self.cfg.token.as_ref().map(|t| format!("Bearer {t}"));
        let mut headers: Vec<(&str, &str)> = vec![("content-type", "application/json")];
        if let Some(a) = &auth {
            headers.push(("authorization", a));
        }
        let call = ("POST", "/_shard/placements", &headers[..], body.as_bytes());
        let (status, _, answer) = self.call_routers_by(&self.reader_pool, region, routers, call)?;
        if status != 200 {
            return Err(format!(
                "{}'s router answered {status}: {}",
                region_name(region),
                String::from_utf8_lossy(&answer).trim()
            ));
        }
        let fields = fenec_core::json::parse_object(&String::from_utf8_lossy(&answer))
            .map_err(|e| format!("{}'s router's answer: {e}", region_name(region)))?;
        let text = |v: &Value, k: &str| match v.member(k) {
            Some(Value::Text(s)) => s.clone(),
            _ => String::new(),
        };
        Ok(fields
            .into_iter()
            .filter(|(_, v)| matches!(v, Value::Object(_)))
            .map(|(t, v)| {
                let quota = match v.member("quota") {
                    Some(q @ Value::Object(_)) => {
                        let mut out = String::new();
                        fenec_core::json::value_into(&mut out, q);
                        Some(out)
                    }
                    _ => None,
                };
                let lineage = match v.member("lineage") {
                    Some(Value::Text(l)) => Some(l.clone()),
                    _ => None,
                };
                let was = match v.member("was") {
                    Some(Value::List(w)) => w
                        .iter()
                        .filter_map(|l| match l {
                            Value::Text(l) => Some(l.clone()),
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                let p = Primary::At {
                    region: text(&v, "region"),
                    addr: text(&v, "addr"),
                    node: None,
                    quota,
                    lineage,
                    was,
                };
                (t, p)
            })
            .collect())
    }

    /// `POST /_shard/placements {"tenants": [..], "verify": [..]}`: where
    /// this router's own directory places each, its quota and its lineage
    /// -- `{"<t>": {"node", "addr", "region", "quota", "lineage", "was"} |
    /// null}` -- which another region's router asks to point the read
    /// replicas it keeps of them at their primaries, to hold them to the
    /// tenant's quota and to bind them to its lineage. Those in `verify`
    /// -- a replica's copy lost, which may have refused its primary -- have
    /// their primary's node asked its lineage first
    /// ([`Self::verify_lineage`]). A read otherwise: a standby answers it
    /// from the placements it follows.
    pub(crate) fn placements(&self, req: &Request) -> Response {
        let (names, verify) =
            match fenec_core::json::parse_object(&String::from_utf8_lossy(&req.body)) {
                Ok(fields) => {
                    let list = |key: &str| match fields.iter().find(|(k, _)| k == key) {
                        Some((_, Value::List(list))) => list.clone(),
                        _ => Vec::new(),
                    };
                    (list("tenants"), list("verify"))
                }
                Err(e) => return Response::error(400, &format!("the body is not JSON: {e}")),
            };
        for v in verify {
            if let Value::Text(t) = v {
                self.verify_lineage(&t);
            }
        }
        let dir = self.read_dir();
        let mut items = Vec::with_capacity(names.len());
        for v in names {
            let Value::Text(t) = v else { continue };
            let found = dir
                .placement(&t)
                .and_then(|p| dir.node(&p.node).map(|n| (p, n)));
            if found.is_some() && dir.lineage(&t).is_none() {
                self.want_lineage(&t);
            }
            let was: Vec<String> = dir.former_lineages(&t).iter().map(|l| quote(l)).collect();
            items.push(match found {
                Some((p, n)) => format!(
                    "{}:{{\"node\":{},\"addr\":{},\"region\":{},\"quota\":{},\"lineage\":{},\
                     \"was\":[{}]}}",
                    quote(&t),
                    quote(&p.node),
                    quote(&n.addr),
                    quote(&n.region),
                    dir.quota(&t).unwrap_or("null"),
                    dir.lineage(&t).map_or("null".into(), |l| quote(&l)),
                    was.join(",")
                ),
                None => format!("{}:null", quote(&t)),
            });
        }
        Response::json(200, format!("{{{}}}", items.join(",")))
    }

    /// Has the next look over the read replicas come now: a failover or a
    /// move changed a primary, and the replicas following the old one are
    /// pointed at the new one by the looking thread -- the failover itself
    /// waits on no replica's node, which may be a region away or cut off,
    /// and holds no tenant a look would need.
    pub(crate) fn follow_soon(&self) {
        let (due, wake) = &self.follow_soon;
        *due.lock().unwrap_or_else(|e| e.into_inner()) = true;
        wake.notify_all();
    }

    /// One look over every read replica this router keeps
    /// ([`FOLLOW_EVERY`], or at once after a failover or a move): each
    /// pointed at its tenant's primary where it follows another address,
    /// made again where a marked read found it gone, taken off where its
    /// tenant is gone or has come to its region. What needs a call to a
    /// node is handed to a worker of that node's, one at a time a node, the
    /// look waiting for none ([`Self::look_on`]); each call bounded by
    /// [`CALL`], each replica held as it is changed and no lock held across
    /// a call. Done on threads the look waited for, a node that did not
    /// answer held up every look after it for its `reader_timeout` -- a
    /// failover promoting a tenant after the one whose replica sat there
    /// left the next tenant's replica following the old primary as long.
    /// Nothing on a router following another, whose directory its primary
    /// keeps.
    pub fn follow_round(self: &Arc<Router>) {
        let readers = {
            let dir = self.read_dir();
            if dir.following() {
                return;
            }
            dir.all_readers()
        };
        if readers.is_empty() {
            return;
        }
        let mut tenants: Vec<String> = readers.iter().map(|(t, _)| t.clone()).collect();
        tenants.dedup();
        let primaries = self.primaries_of(&tenants);
        let resolved = Instant::now();
        let due: Vec<(String, Reader, Primary)> = readers
            .into_iter()
            .filter_map(|(t, r)| {
                // A primary this directory places is looked up again: a
                // failover since the look would have the replica pointed
                // back at the one it failed over from.
                let here = self.read_dir().placement(&t).is_some();
                let p = match here {
                    true => self.primary_of(&t),
                    false => primaries.get(&t)?.clone(),
                };
                // Placed here with no lineage known -- made before any was
                // kept: its node is asked for it ([`Self::backfill_lineages`]).
                if here && self.read_dir().lineage(&t).is_none() {
                    self.want_lineage(&t);
                }
                // Its quota, as the directory placing the tenant keeps it,
                // which may be another region's: handed to its node's
                // sender, never sent here.
                if let Primary::At { quota, .. } = &p {
                    match here {
                        true => self.want_kept_quota(&t, &r.node),
                        false => self.want_reader_quota(&t, &r.node, quota.as_deref()),
                    }
                }
                self.due(&t, &r, &p).then_some((t, r, p))
            })
            .collect();
        let mut by_node: BTreeMap<String, Vec<Item>> = BTreeMap::new();
        for (t, r, p) in due {
            by_node.entry(r.node.clone()).or_default().push(Item {
                tenant: t,
                reader: r,
                primary: p,
                resolved,
            });
        }
        for (node, items) in by_node {
            self.look_on(node, items);
        }
    }

    /// `items`, read replicas on `node` needing a call to it, looked at in
    /// turn on a thread of the node's -- unless one is under way there,
    /// whose node has not answered yet: they wait for the next look.
    fn look_on(self: &Arc<Router>, node: String, items: Vec<Item>) {
        if !self
            .lookers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(node.clone())
        {
            return;
        }
        let router = Arc::clone(self);
        let n = node.clone();
        let started = std::thread::Builder::new()
            .name("fenec-readers-look".into())
            .spawn(move || {
                // Let go of however it ends.
                struct Done(Arc<Router>, String);
                impl Drop for Done {
                    fn drop(&mut self) {
                        self.0
                            .lookers
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&self.1);
                    }
                }
                let done = Done(router, n);
                let mut items = items;
                for i in 0..items.len() {
                    done.0.fresh_primary(&mut items, i);
                    let it = &items[i];
                    done.0.look_at(&it.tenant, &it.reader, &it.primary);
                }
            });
        if started.is_err() {
            self.lookers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&node);
        }
    }

    /// Asks each node holding one of this router's read replicas which
    /// tenants it holds, and marks the ones it does not hold lost, having
    /// the next look made now to make them again: whether any was. A node
    /// that does not answer is passed over -- the marked reads sent there
    /// go home, and it is asked again at the next check. On a thread of its
    /// own ([`Self::start_following`]): in the look, every node out of
    /// reach held each re-pointing after a failover up for its [`CALL`].
    pub(crate) fn check_copies(&self) -> bool {
        let readers = {
            let dir = self.read_dir();
            if dir.following() {
                return false;
            }
            dir.all_readers()
        };
        let mut by_node: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (t, r) in &readers {
            by_node.entry(&r.node).or_default().push(t);
        }
        let nodes: Vec<(&str, Vec<&str>)> = by_node.into_iter().collect();
        let each = nodes.len().div_ceil(LOOKERS).max(1);
        let found = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            for part in nodes.chunks(each) {
                let found = &found;
                s.spawn(move || {
                    for (node, tenants) in part {
                        if self.check_node(node, tenants) {
                            found.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        let found = found.into_inner();
        if found {
            self.follow_soon();
        }
        found
    }

    /// Whether `node` lacks one of `tenants`, each marked lost.
    fn check_node(&self, node: &str, tenants: &[&str]) -> bool {
        let Some(n) = self.read_dir().node(node).cloned() else {
            return false;
        };
        let held = match self
            .reader_pool
            .call(&n.addr, "GET", "/_admin/tenants", &n.token, b"")
        {
            Ok((200, body)) => {
                match fenec_core::json::parse_json(&String::from_utf8_lossy(&body)) {
                    Ok(Value::List(names)) => names,
                    _ => return false,
                }
            }
            _ => return false,
        };
        // A set of the node's tenants: asked of each of its read replicas,
        // a list was every tenant it holds, each time.
        let held: std::collections::HashSet<String> = held
            .into_iter()
            .filter_map(|v| match v {
                Value::Text(t) => Some(t),
                _ => None,
            })
            .collect();
        let mut lost = self.reader_lost.lock().unwrap_or_else(|e| e.into_inner());
        let mut any = false;
        for t in tenants {
            let key = format!("{t}/{node}");
            match held.contains(*t) {
                false => any |= lost.insert(key),
                true => self.remakes_done(&key),
            }
        }
        any
    }

    /// Counts a lost copy of the read replica `key` made again: whether it
    /// may be, at most [`REMAKES`] in a row. A copy that refuses its
    /// primary deletes itself, and made again by every look it refused
    /// again for good -- a primary of a lineage this router could not learn
    /// (its node out of reach, the tenant's directory a router's from
    /// before lineages) -- so past that the replica is left recorded and
    /// without a copy, said once and counted
    /// (`fenec_router_read_replicas_given_up`), for an operator to place
    /// again; its primary moving, or its lineage, tries it again.
    /// The count goes one past [`REMAKES`] as it is given up on, which the
    /// caller says and counts, once.
    fn may_remake(&self, key: &str) -> bool {
        let mut remade = self.reader_remade.lock().unwrap_or_else(|e| e.into_inner());
        let n = remade.entry(key.to_string()).or_insert(0);
        *n += 1;
        *n <= REMAKES
    }

    /// Whether the read replica `key` was given up on ([`Self::may_remake`]).
    fn given_up(&self, key: &str) -> bool {
        self.reader_remade
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .is_some_and(|n| *n > REMAKES)
    }

    /// Lets go of the count of `key`'s copy made again: its copy found on
    /// its node, its primary or lineage changed, or its record gone.
    pub(crate) fn remakes_done(&self, key: &str) {
        let gone = self
            .reader_remade
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
        if gone.is_some_and(|n| n > REMAKES) {
            crate::metrics::given_up(-1);
        }
    }

    /// Whether the read replica `r` of `tenant` needs anything done, its
    /// primary `p`: no call is made for one that does not.
    fn due(&self, tenant: &str, r: &Reader, p: &Primary) -> bool {
        match p {
            Primary::At {
                region,
                addr,
                node,
                lineage,
                was,
                ..
            } => {
                let dir = self.read_dir();
                let n = dir.node(&r.node);
                another_lineage(r, lineage.as_deref(), was)
                    || (r.lineage.is_none() && lineage.is_some())
                    || bound_before(r, was)
                    || node.as_deref() == Some(r.node.as_str())
                    || n.is_some_and(|n| n.addr == *addr || n.region == *region)
                    || r.follows != format!("http://{addr}")
                    || self
                        .reader_lost
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .contains(&format!("{tenant}/{}", r.node))
            }
            Primary::Gone => true,
            Primary::Unknown(_) => false,
        }
    }

    /// Where item `i`'s primary is as the worker reaches it: a node's
    /// worker reaches an item after the ones before it on that node, tens
    /// of seconds after the look where that node is slow, and a failover
    /// meanwhile had the primary of the look's time, the old one, pointed
    /// at until the next. Looked up again in this directory each time --
    /// its maps alone; for one at home in another region only once the
    /// look's answer is older than [`PRIMARY_FRESH`], and then for every
    /// such item left at once, one call a region.
    fn fresh_primary(&self, items: &mut [Item], i: usize) {
        let here = |t: &str| self.read_dir().placement(t).is_some();
        if here(&items[i].tenant) {
            items[i].primary = self.primary_of(&items[i].tenant);
            items[i].resolved = Instant::now();
            return;
        }
        if items[i].resolved.elapsed() < PRIMARY_FRESH {
            return;
        }
        let mut stale: Vec<String> = items[i..]
            .iter()
            .filter(|it| !here(&it.tenant) && it.resolved.elapsed() >= PRIMARY_FRESH)
            .map(|it| it.tenant.clone())
            .collect();
        stale.sort();
        stale.dedup();
        let fresh = self.primaries_of(&stale);
        let now = Instant::now();
        for it in items[i..].iter_mut() {
            if let Some(p) = fresh.get(&it.tenant) {
                it.primary = p.clone();
                it.resolved = now;
            }
        }
    }

    /// [`Self::keep_following`] of one read replica, held for it: passed
    /// over where a change of it is under way, and its record read again
    /// once held; `p` where its primary is ([`Self::fresh_primary`]).
    fn look_at(&self, tenant: &str, r: &Reader, p: &Primary) {
        let region = self.read_dir().region_of(&r.node).to_string();
        let Some(_hold) = self.hold_reader(tenant, &region) else {
            return;
        };
        let now = self
            .read_dir()
            .readers(tenant)
            .iter()
            .find(|x| x.node == r.node)
            .cloned();
        let Some(r) = now else {
            return;
        };
        if self.due(tenant, &r, p) {
            self.keep_following(tenant, &r, p);
        }
    }

    /// Binds the read replica `r` of `tenant`, recorded with no lineage, to
    /// `lineage`, its tenant's: where its copy's node says the copy is of
    /// it -- or says nothing of lineages, a node from before them -- the
    /// record takes it and the copy is told it with a follow of the node
    /// it follows. A copy of another lineage is another tenant's -- once it
    /// holds its primary's history: one that has not taken its first image
    /// is of a lineage of its own, as every file a replicating node makes
    /// is, and is asked again at a later look.
    fn bind_reader(&self, tenant: &str, r: &Reader, lineage: &str) -> Bound {
        let Some(n) = self.read_dir().node(&r.node).cloned() else {
            return Bound::Unknown;
        };
        let target = format!("/_admin/tenants/{tenant}/lineage");
        let copy = match self
            .reader_pool
            .call(&n.addr, "GET", &target, &n.token, b"")
        {
            Ok((200, body)) if ready_in(&body) == Some(false) => return Bound::Unknown,
            Ok((200, body)) => crate::lineage_in(&body),
            // A node from before lineages: nothing to ask, nothing it
            // would refuse; the record is bound all the same.
            Ok((404, body))
                if String::from_utf8_lossy(&body).contains("no such admin endpoint") =>
            {
                None
            }
            _ => return Bound::Unknown,
        };
        if copy.as_deref().is_some_and(|c| c != lineage) {
            return Bound::Other;
        }
        if let Err(w) = self.tell_follow(tenant, &r.node, &r.follows, false, Some(lineage)) {
            fenec_http::log!(
                "tenant `{tenant}`'s read replica on `{}` was not told its lineage yet: {w}",
                r.node
            );
            return Bound::Unknown;
        }
        let recorded = self
            .write_dir()
            .set_reader(tenant, &r.node, &r.follows, Some(lineage));
        if let Err(e) = recorded {
            fenec_http::log!("tenant `{tenant}`'s read replica's lineage stayed unrecorded: {e}");
            return Bound::Unknown;
        }
        Bound::Kept(Reader {
            lineage: Some(lineage.to_string()),
            ..r.clone()
        })
    }

    /// Asks the node this directory places `tenant` on which lineage its
    /// file is of, and where that is another than the directory's records
    /// the node's ([`crate::directory::Directory::succeed_lineage`]): the
    /// primary is the tenant -- every read and write of it goes there --
    /// and the lineage before is kept as one it was, its read replicas
    /// bound to it again rather than taken off. A tenant made again under
    /// the name through a router is no such case: its create recorded its
    /// lineage, and the replicas of the one before are of no lineage it
    /// was. Nothing where the node is not reached, from before lineages,
    /// or the tenant was placed elsewhere or given a lineage meanwhile.
    pub(crate) fn verify_lineage(&self, tenant: &str) {
        let (node, n, known) = {
            let dir = self.read_dir();
            let Some(p) = dir.placement(tenant) else {
                return;
            };
            let Some(n) = dir.node(&p.node).cloned() else {
                return;
            };
            (p.node.clone(), n, dir.lineage(tenant))
        };
        let target = format!("/_admin/tenants/{tenant}/lineage");
        let theirs = match self
            .reader_pool
            .call(&n.addr, "GET", &target, &n.token, b"")
        {
            Ok((200, body)) if ready_in(&body) != Some(false) => crate::lineage_in(&body),
            _ => None,
        };
        let Some(theirs) = theirs else {
            return;
        };
        if known.as_deref() == Some(theirs.as_str()) {
            return;
        }
        let mut dir = self.write_dir();
        if dir.placement(tenant).map(|p| p.node.as_str()) != Some(node.as_str())
            || dir.lineage(tenant) != known
        {
            return;
        }
        match dir.succeed_lineage(tenant, &theirs) {
            Ok(()) => fenec_http::log!(
                "tenant `{tenant}`'s primary on `{node}` is of lineage {theirs}, not {}: recorded, \
                 its read replicas bound to it",
                known.as_deref().unwrap_or("none")
            ),
            Err(e) => fenec_http::log!("tenant `{tenant}`'s lineage stayed unrecorded: {e}"),
        }
    }

    /// [`Self::verify_lineage`] of `tenant` wherever its directory is --
    /// this router's, a region's router's, the global router's -- and its
    /// lineage then, with the ones it was: `None` where not known.
    fn verify_primary(&self, tenant: &str) -> Option<(Option<String>, Vec<String>)> {
        let here = self.read_dir().placement(tenant).is_some();
        if here {
            self.verify_lineage(tenant);
            let dir = self.read_dir();
            return Some((dir.lineage(tenant), dir.former_lineages(tenant).to_vec()));
        }
        // At home in a region keeping its own directory, as this one's or
        // the global directory's copy says, or placed by the global one.
        let names = [tenant.to_string()];
        // This directory first, then the global copy, as `primaries_of`
        // takes them.
        let (home, global_places) = {
            let dir = self.read_dir();
            let global = self.global.as_ref().map(crate::read);
            let home = dir.home(tenant).map(str::to_string);
            let home = home.or_else(|| global.as_ref()?.home(tenant).map(str::to_string));
            let places = global.is_some_and(|g| g.placement(tenant).is_some());
            (home, places)
        };
        let asked = match (home, global_places) {
            (Some(region), _) => self.ask_placements(&region, &names, true),
            (None, true) if !self.cfg.global.is_empty() => {
                let routers = self.cfg.global.clone();
                self.ask_placements_of("", &routers, &names, true)
            }
            _ => return None,
        };
        match asked.ok()?.remove(tenant)? {
            Primary::At { lineage, was, .. } => Some((lineage, was)),
            _ => None,
        }
    }

    /// How many of the nodes' answers [`Self::backfill_lineages`] has
    /// taken, recorded or not: for a test to wait on, as
    /// [`Router::grants_waiting`].
    pub fn lineages_asked(&self) -> u64 {
        self.lineage_answers
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Asks for `tenant`'s lineage to be had from its node, at the next
    /// check ([`Self::backfill_lineages`]).
    pub(crate) fn want_lineage(&self, tenant: &str) {
        self.lineage_wanted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tenant.to_string());
    }

    /// Records the lineage of each tenant this directory places and knows
    /// none of -- made before lineages were kept, by a router or a node
    /// from before them -- as its node says (`GET
    /// /_admin/tenants/<t>/lineage`): unknown, its read replicas were bound
    /// to nothing and took any tenant of the name. A node that does not
    /// answer, or from before lineages, leaves it to the next check. On the
    /// check's thread, a node at a time on [`LOOKERS`] threads at most.
    pub(crate) fn backfill_lineages(&self) {
        let wanted: Vec<String> = self
            .lineage_wanted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .collect();
        let mut by_node: BTreeMap<String, Vec<String>> = BTreeMap::new();
        {
            let dir = self.read_dir();
            if dir.following() {
                return;
            }
            for t in wanted {
                if dir.lineage(&t).is_some() {
                    continue;
                }
                if let Some(p) = dir.placement(&t) {
                    by_node.entry(p.node.clone()).or_default().push(t);
                }
            }
        }
        let nodes: Vec<(String, Vec<String>)> = by_node.into_iter().collect();
        let each = nodes.len().div_ceil(LOOKERS).max(1);
        std::thread::scope(|s| {
            for part in nodes.chunks(each) {
                s.spawn(move || {
                    for (node, tenants) in part {
                        let Some(n) = self.read_dir().node(node).cloned() else {
                            continue;
                        };
                        for t in tenants {
                            let target = format!("/_admin/tenants/{t}/lineage");
                            let said = match self
                                .reader_pool
                                .call(&n.addr, "GET", &target, &n.token, b"")
                            {
                                Ok((200, body)) => crate::lineage_in(&body),
                                _ => None,
                            };
                            let Some(l) = said else {
                                self.want_lineage(t);
                                continue;
                            };
                            let _taken = Taken(&self.lineage_answers);
                            // Placed elsewhere since, or made again -- whose
                            // create recorded its own lineage while this was
                            // asked: the node's word is of another tenant, or
                            // of another moment. Asked under the guard that
                            // writes it, so no create lands between.
                            let mut dir = self.write_dir();
                            let still = dir.placement(t).is_some_and(|p| &p.node == node)
                                && dir.lineage(t).is_none();
                            if !still {
                                continue;
                            }
                            if let Err(e) = dir.set_lineage(t, Some(&l)) {
                                fenec_http::log!("tenant `{t}`'s lineage stayed unrecorded: {e}");
                            }
                        }
                    }
                });
            }
        });
    }

    /// The thread making [`Self::follow_round`] once a [`FOLLOW_EVERY`],
    /// or at once when told ([`Self::follow_soon`]). Started with the
    /// router's listener ([`Router::serve_on`]): a look over a directory
    /// keeping no read replica reads one map and returns.
    pub(crate) fn start_following(self: &Arc<Router>) -> std::io::Result<()> {
        // The quotas' senders, started as one is wanted, and every
        // [`CHECK_EVERY`] again for one that did not land.
        let quotas = Arc::clone(self);
        std::thread::Builder::new()
            .name("fenec-readers-quotas".into())
            .spawn(move || loop {
                {
                    let (due, wake) = &quotas.quotas_soon;
                    let mut due = due.lock().unwrap_or_else(|e| e.into_inner());
                    if !*due {
                        due = wake
                            .wait_timeout(due, CHECK_EVERY)
                            .unwrap_or_else(|e| e.into_inner())
                            .0;
                    }
                    *due = false;
                }
                quotas.deliver_quotas();
            })?;
        let checker = Arc::clone(self);
        std::thread::Builder::new()
            .name("fenec-readers-check".into())
            .spawn(move || loop {
                checker.check_copies();
                checker.backfill_lineages();
                std::thread::sleep(CHECK_EVERY);
            })?;
        let router = Arc::clone(self);
        std::thread::Builder::new()
            .name("fenec-readers".into())
            .spawn(move || loop {
                {
                    let (due, wake) = &router.follow_soon;
                    let mut due = due.lock().unwrap_or_else(|e| e.into_inner());
                    if !*due {
                        due = wake
                            .wait_timeout(due, FOLLOW_EVERY)
                            .unwrap_or_else(|e| e.into_inner())
                            .0;
                    }
                    *due = false;
                }
                router.follow_round();
            })?;
        Ok(())
    }

    /// Points the read replica `r` of `tenant` at `primary` where it
    /// follows another address, or a marked read found it gone from its
    /// node; takes it off where its tenant is gone or has come to live in
    /// its region; leaves it following what it follows where the primary
    /// is not known now -- a region cut off serves the state it last
    /// applied. Called with the replica held ([`Self::hold_reader`]).
    fn keep_following(&self, tenant: &str, r: &Reader, primary: &Primary) {
        // A record bound to no lineage -- placed before lineages were kept
        // -- whose tenant's is known now: bound to it where its copy is of
        // it, and the copy told so; taken off where the copy is another
        // tenant's of the name.
        let bound;
        let r = match (r.lineage.as_deref(), primary) {
            (
                None,
                Primary::At {
                    lineage: Some(l), ..
                },
            ) => match self.bind_reader(tenant, r, l) {
                Bound::Kept(b) => {
                    bound = b;
                    &bound
                }
                Bound::Other => {
                    if let Err(w) = self.take_reader(tenant, r) {
                        fenec_http::log!(
                            "tenant `{tenant}`'s read replica on `{}` copies another tenant of \
                             its name, and stayed: {w}",
                            r.node
                        );
                    }
                    return;
                }
                Bound::Unknown => return,
            },
            _ => r,
        };
        let key = format!("{tenant}/{}", r.node);
        let mut why = "";
        let failed = match primary {
            // Another tenant of the name -- deleted and made again, its
            // record kept by a node out of reach or forgotten by hand: the
            // replica goes, never pointed at it. Its copy refuses the new
            // one's image all the same ([`Self::tell_follow`]).
            Primary::At { lineage, was, .. } if another_lineage(r, lineage.as_deref(), was) => {
                why = "as its tenant was made again under its name";
                self.take_reader(tenant, r).err()
            }
            Primary::At {
                region,
                addr,
                node,
                lineage,
                was,
                ..
            } => {
                let (here, theirs) = {
                    let dir = self.read_dir();
                    let n = dir.node(&r.node);
                    (
                        n.map_or(String::new(), |n| n.region.clone()),
                        n.map(|n| n.addr.clone()),
                    )
                };
                // The tenant itself lives on the node now -- a move or a
                // create placed it there -- or in its region: the record
                // goes, and with it, where the tenant is not on the node,
                // the copy.
                if node.as_deref() == Some(r.node.as_str()) || theirs.as_deref() == Some(addr) {
                    if let Err(e) = self.write_dir().remove_reader(tenant, &r.node) {
                        fenec_http::log!("tenant `{tenant}`'s read replica record stayed: {e}");
                    }
                    self.forget_reader(tenant, &r.node);
                    self.remakes_done(&key);
                    self.untrace_if_none(tenant);
                    return;
                }
                if *region == here {
                    why = "as its tenant moved into its region";
                    self.take_reader(tenant, r).err()
                } else {
                    let url = format!("http://{addr}");
                    why = "where it was";
                    let lost = self
                        .reader_lost
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .contains(&key);
                    let (mut lineage, mut was) = (lineage.clone(), was.clone());
                    // Its copy lost with nothing else changed: the copy may
                    // have refused its primary, which is then of a lineage
                    // the tenant's directory does not know -- a copy that
                    // never took its first image promoted, an older file
                    // restored, a file put in its place. The primary is the
                    // tenant: its node is asked, and the directory takes its
                    // word, the lineage before kept as one the tenant was
                    // (`was`), so this replica is bound to it again rather
                    // than made again to refuse it for good.
                    if lost && r.follows == url && !bound_before(r, &was) {
                        if self.given_up(&key) {
                            return;
                        }
                        if let Some((l, w)) = self.verify_primary(tenant) {
                            (lineage, was) = (l, w);
                        }
                        if !bound_before(r, &was) && !self.may_remake(&key) {
                            crate::metrics::given_up(1);
                            fenec_http::log!(
                                "tenant `{tenant}`'s read replica on `{}` lost its copy {REMAKES} \
                                 times in a row -- it refuses its primary at {url} -- and is made \
                                 again no more: DELETE it and place it again",
                                r.node
                            );
                            return;
                        }
                    }
                    // Bound to a lineage its tenant was of: bound to the
                    // one it is of now, a new beginning for its count.
                    let rebound = bound_before(r, &was) && lineage.is_some();
                    if rebound || r.follows != url {
                        self.remakes_done(&key);
                    }
                    let bind = match rebound {
                        true => lineage.as_deref(),
                        false => r.lineage.as_deref(),
                    };
                    let mut failed = None;
                    if r.follows != url || lost || rebound {
                        let told = self
                            .tell_follow(tenant, &r.node, &url, false, bind)
                            .and_then(|_| {
                                self.write_dir()
                                    .set_reader(tenant, &r.node, &url, bind)
                                    .map_err(|e| message(&e))
                            });
                        match told {
                            Ok(()) => {
                                self.forget_reader(tenant, &r.node);
                                fenec_http::log!(
                                    "tenant `{tenant}`'s read replica on `{}` follows its \
                                     primary at {url} now",
                                    r.node
                                );
                            }
                            Err(w) => {
                                failed = Some(format!(
                                    "it follows {} still, not its primary at {url}: {w}",
                                    r.follows
                                ))
                            }
                        }
                    }
                    failed
                }
            }
            Primary::Gone => {
                why = "as its tenant was deleted";
                self.take_reader(tenant, r).err()
            }
            Primary::Unknown(_) => None,
        };
        // Said once, not at every look, and again once one succeeds.
        if let Some(w) = failed {
            let mut unfollowed = self.unfollowed.lock().unwrap_or_else(|e| e.into_inner());
            if unfollowed.insert(key) {
                fenec_http::log!(
                    "tenant `{tenant}`'s read replica on `{}` stayed {why}: {w}",
                    r.node
                );
            }
        }
    }
}

/// Counts an answer taken as it is dropped, whatever became of it.
struct Taken<'a>(&'a std::sync::atomic::AtomicU64);

impl Drop for Taken<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A read replica a look's worker reaches, with where its primary was and
/// when that was found.
struct Item {
    tenant: String,
    reader: Reader,
    primary: Primary,
    resolved: Instant,
}

/// What became of binding a read replica to its tenant's lineage.
enum Bound {
    /// Bound: the record as it is now.
    Kept(Reader),
    /// Its copy is another tenant's of the name.
    Other,
    /// Not now: its node not reached.
    Unknown,
}

/// Whether the read replica `r` copies another tenant of its name than the
/// primary of `lineage`: both known, and not the same nor one the tenant
/// was of (`was`).
fn another_lineage(r: &Reader, lineage: Option<&str>, was: &[String]) -> bool {
    matches!((r.lineage.as_deref(), lineage),
        (Some(ours), Some(theirs)) if ours != theirs && !was.iter().any(|w| w == ours))
}

/// Whether the read replica `r` is bound to a lineage its tenant was of
/// before the one it is of now.
fn bound_before(r: &Reader, was: &[String]) -> bool {
    r.lineage.as_ref().is_some_and(|l| was.contains(l))
}

/// Whether a node's word on a tenant's lineage says its file holds its
/// primary's history (`"ready"`): `None` from a node that does not say.
fn ready_in(body: &[u8]) -> Option<bool> {
    let fields = fenec_core::json::parse_object(&String::from_utf8_lossy(body)).ok()?;
    fields.into_iter().find_map(|(k, v)| match (k.as_str(), v) {
        ("ready", Value::Bool(b)) => Some(b),
        _ => None,
    })
}

/// What a region's router said of the read replicas of a name it keeps.
#[derive(Debug, PartialEq)]
pub(crate) enum Traced {
    /// None, and no change of one under way.
    None,
    /// One, or a change of one under way.
    Kept,
    /// Not known, and why: not reached, or answered by a router following
    /// the region's, which cannot see a change under way on the one it
    /// follows -- whose record of a replica being taken off is out until
    /// its node answers, and put back if it does not.
    Unknown(String),
}

/// [`Traced`] of a region's `GET /_shard/tenants/<t>/read-replicas`.
pub(crate) fn traced(fields: &[(String, Value)]) -> Traced {
    let listed = fields
        .iter()
        .any(|(k, v)| k == "read_replicas" && matches!(v, Value::List(l) if !l.is_empty()));
    let changing = fields.iter().find(|(k, _)| k == "changing").map(|(_, v)| v);
    match (listed, changing) {
        (true, _) | (_, Some(Value::Bool(true))) => Traced::Kept,
        (false, Some(Value::Bool(false))) => Traced::None,
        _ => Traced::Unknown("its router follows another, and cannot say what is under way".into()),
    }
}

/// One read replica as `GET /_shard/tenants/<t>/read-replicas` lists it.
fn reader_item(region: &str, r: &Reader) -> String {
    format!(
        "{{\"region\":{},\"node\":{},\"follows\":{},\"lineage\":{}}}",
        quote(region),
        quote(&r.node),
        quote(&r.follows),
        r.lineage.as_deref().map_or("null".into(), quote)
    )
}

/// Whether `req` is one of the read replicas' endpoints the directory
/// only reads: their list, and the placements another region's router
/// asks for.
pub(crate) fn reads_only(req: &Request, rest: &[&str]) -> bool {
    matches!(
        (req.method, rest),
        (Method::Get, ["tenants", _, "read-replicas"]) | (Method::Post, ["placements"])
    )
}

#[cfg(test)]
mod tests {
    use super::QuotaQueue;
    use crate::directory::{Directory, Node, State};
    use crate::{Config, Router};
    use std::sync::mpsc;
    use std::time::Duration;

    /// A router following another says it cannot tell whether a change of
    /// a read replica is under way: `false` there let the global router
    /// make a name again while the region's primary router was taking a
    /// replica of it off, which a node not answering put back.
    #[test]
    fn a_router_following_another_cannot_say_what_is_under_way() {
        use fenec_core::prelude::Database;
        use std::sync::{Arc, RwLock};
        let mut db = Database::new();
        db.follow(Vec::new()).unwrap();
        let dir = Directory::load(Arc::new(RwLock::new(db))).unwrap();
        let router = Router::new(dir, Config::default());
        let body = String::from_utf8_lossy(&router.list_readers("acme").body).into_owned();
        assert!(body.contains(r#""changing":null"#), "{body}");
        let own = Router::new(Directory::in_memory(), Config::default());
        let body = String::from_utf8_lossy(&own.list_readers("acme").body).into_owned();
        assert!(body.contains(r#""changing":false"#), "{body}");
    }

    /// What a region's list says of a name, read.
    #[test]
    fn a_region_s_list_is_read_as_kept_none_or_not_known() {
        use super::{traced, Traced};
        let of = |text: &str| traced(&fenec_core::json::parse_object(text).unwrap());
        let one =
            r#"{"read_replicas":[{"region":"us","node":"us1","follows":"x"}],"changing":false}"#;
        assert_eq!(of(one), Traced::Kept);
        assert_eq!(of(r#"{"read_replicas":[],"changing":true}"#), Traced::Kept);
        assert_eq!(of(r#"{"read_replicas":[],"changing":false}"#), Traced::None);
        assert!(matches!(
            of(r#"{"read_replicas":[],"changing":null}"#),
            Traced::Unknown(_)
        ));
        assert!(matches!(of(r#"{"read_replicas":[]}"#), Traced::Unknown(_)));
    }

    /// A marked read goes to no read replica of a tenant no directory the
    /// router reads places: a record another region's directory keeps
    /// outlives its tenant until that router's next look, and the deleted
    /// tenant's copy answered meanwhile.
    #[test]
    fn a_marked_read_goes_to_no_read_replica_of_a_tenant_gone() {
        let cfg = Config {
            near: Some("us".into()),
            ..Config::default()
        };
        let router = Router::new(Directory::in_memory(), cfg);
        {
            let mut dir = router.write_dir();
            for (name, region) in [("eu1", "eu"), ("us1", "us")] {
                let node = Node {
                    addr: "127.0.0.1:1".into(),
                    token: "t".into(),
                    region: region.into(),
                    ..Node::default()
                };
                dir.set_node(name, node).unwrap();
            }
            dir.set_reader("acme", "us1", "http://127.0.0.1:1", None)
                .unwrap();
        }
        assert!(router.reader_near("acme").is_none(), "a tenant gone");
        router
            .write_dir()
            .place("acme", "eu1", State::Active)
            .unwrap();
        assert!(router.reader_near("acme").is_some(), "a tenant placed");
    }

    /// What is wanted last is what a read replica's sender sends: a newer
    /// quota over one not yet sent, and a send that did not land wanted
    /// again only where nothing newer is -- an older quota never goes out
    /// after a newer one. One sender a node.
    #[test]
    fn a_quota_wanted_last_is_the_one_sent() {
        let mut q = QuotaQueue::default();
        assert!(q.want("acme", "us1", Some("5")));
        assert!(!q.want("acme", "us1", Some("5")), "the same, wanted again");
        assert!(q.want("acme", "us1", Some("9")));
        assert_eq!(q.idle(), vec!["us1".to_string()]);
        assert!(q.idle().is_empty(), "one sender a node");
        assert_eq!(q.next("us1"), Some(("acme".into(), Some("9".into()))));
        // A newer one wanted while the older is under way, which then
        // does not land: the newer stays.
        q.want("acme", "us1", Some("12"));
        q.failed("us1", "acme", Some("9".into()));
        assert_eq!(q.idle(), vec!["us1".to_string()], "the node let go");
        assert_eq!(q.next("us1"), Some(("acme".into(), Some("12".into()))));
        // One that did not land, nothing newer wanted: wanted again.
        q.failed("us1", "acme", Some("12".into()));
        assert_eq!(q.idle(), vec!["us1".to_string()]);
        assert_eq!(q.next("us1"), Some(("acme".into(), Some("12".into()))));
        assert_eq!(q.next("us1"), None);
        assert_eq!(q.idle(), Vec::<String>::new(), "nothing wanted");
        q.want("acme", "us1", None);
        q.forget("acme", "us1");
        assert_eq!(q.idle(), Vec::<String>::new(), "taken off");
    }

    /// A change asked for while another of the same read replica is under
    /// way is refused and leaves the other's hold in place. The asks run
    /// on a thread of their own: one that waits on itself never returns,
    /// and the test bounds only that wait.
    #[test]
    fn a_change_under_way_refuses_another_and_keeps_its_hold() {
        let router = Router::new(Directory::in_memory(), Config::default());
        let (tx, rx) = mpsc::channel();
        let r = std::sync::Arc::clone(&router);
        std::thread::spawn(move || {
            let first = r.hold_reader("acme", "us");
            let second = r.hold_reader("acme", "us").is_some();
            let third = r.hold_reader("acme", "us").is_some();
            let elsewhere = r.hold_reader("acme", "eu").is_some();
            drop(first);
            let after = r.hold_reader("acme", "us").is_some();
            let _ = tx.send((second, third, elsewhere, after));
        });
        let (second, third, elsewhere, after) = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("a hold refused waits on nothing");
        assert!(!second, "a second change of the same copy is refused");
        assert!(!third, "and the first change's hold is still in place");
        assert!(elsewhere, "another region's copy is a change of its own");
        assert!(after, "the copy is free once the first change is over");
    }
}
