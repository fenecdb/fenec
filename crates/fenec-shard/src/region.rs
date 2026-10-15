//! Regions that keep their own directory (`fenec-shard --region`): their
//! leases, failovers and placements decided inside the region, so a region
//! cut off from the others goes on taking writes and fails a node over in
//! itself.
//!
//! **Who decides what.** The global directory -- the router started
//! without `--region`, and its standbys -- holds every tenant's name and
//! the regions keeping their own directory, with their routers' URLs
//! (`PUT /_shard/regions/<r>`). A region's router keeps its region's nodes
//! and tenants in a directory of its own, the same collections, and is the
//! only router that leases those nodes: a lease, a failover, a repair, a
//! move inside the region, a node added to it, a quota, write that
//! directory alone. The global directory holds a home for each of the
//! region's tenants (`@<region>`), and changes only for what needs every
//! region to agree: a tenant's name taken (a create) or given back (a
//! delete), and a tenant leaving its region (a move across). Those go
//! through the global router, which asks the region's router for its part,
//! and are refused, 503 with `Retry-After`, while it cannot reach it --
//! nothing is placed or moved by half. A region with no directory of its
//! own -- the unnamed one, and every region of a deployment that names
//! none -- is the global directory's, as before.
//!
//! **Forwarding goes through the region's router.** A router asked for a
//! tenant at home in another region relays the request to that region's
//! routers, which forward it to the node: they hold its placement as the
//! region's last failover left it, where a copy of their directory in every
//! other router would lag a failover by a partition's length. It costs a
//! request a hop inside the region the tenant lives in -- one the request
//! for a tenant of the region a client is in never takes. A relayed
//! request is never relayed again: a router whose copy of the global
//! directory is behind answers 503 rather than send it round.
//!
//! **Fencing.** A lease names its holder: the directory and its term
//! ([`fenec_http::lease::Holder`]), which a router's promotion moves on. A
//! region's router lost is replaced by promoting one following its
//! directory -- in the region, or in another -- as a standby router is
//! promoted: the nodes refuse the old one's grants once they hear from the
//! new one, and so do their promotions and follows. The global router's
//! list of a region's routers names them all, the one keeping the
//! directory first, and a relay moves to the next where one cannot be
//! reached: a router following the directory forwards from its copy, and
//! answers a change 409 until it is promoted.

use crate::directory::{Directory, State, HOME};
use crate::upstream::{Answer, Tried};
use crate::{body_fields, expect, field, internal, place_field, quote, region_name, Fail, Outcome};
use crate::{read, replica_field, Config, Router};
use fenec_http::http::{Method, Request, Response};
use fenec_http::replication::Replication;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// The header the global router marks what it asks a region's router with:
/// the global part of the change is the global router's, done or about to
/// be, and the request is carried out here and never relayed on.
pub const GLOBAL_HEADER: &str = "fenec-global";

/// The header a tenant's request asks for its home region with, and the
/// answer names it in: `Fenec-Home: <region>`, written by the router that
/// forwards the request to the tenant's node -- the node's region -- and
/// passed through unchanged by a router that relayed it there. The global
/// entry (`integrations/cloudflare/entry`) sends every request to the
/// region it last heard for the tenant, and learns the region from this:
/// a tenant it has not seen, or one moved across regions since, costs the
/// one relay a router makes, and its next request goes home. Asked for,
/// so a client that does not ask is told no home; never named by a
/// relaying router, whose own region is not the tenant's; and named on the
/// placing router's own 502 for a node it cannot reach ([`homed`]).
pub const HOME_HEADER: &str = "fenec-home";

/// The mark on every answer a router writes, its own and those it
/// forwards (`Fenec-Shard: 1`; one it relays carries the relaying
/// router's, the other router's dropped): a proxy in front of the routers
/// -- the global entry -- tells a router's answer from one something
/// between them wrote, a tunnel's connector answering 502 for a router it
/// cannot reach, and sends a request a router never answered to the next.
/// `X-Request-Id` could not tell them apart: a load balancer may write its
/// own on its own 502.
pub const SHARD_HEADER: &str = "Fenec-Shard";

/// `resp` naming the tenant's home where it is known: a placing router's
/// own failure -- a node it could not reach -- is the home's answer, which
/// the entry passes on rather than try another region.
pub(crate) fn homed(resp: Response, home: &Option<String>) -> Response {
    match home {
        Some(region) => resp.header("Fenec-Home", region),
        None => resp,
    }
}

/// The region to name in an answer's [`HOME_HEADER`]: the placing node's,
/// where `req` asks and the node is in one -- the unnamed region is every
/// router's, and there is nothing to learn.
pub(crate) fn home_asked(req: &Request, region: &str) -> Option<String> {
    (!region.is_empty() && req.header(HOME_HEADER).is_some()).then(|| region.to_string())
}

/// A whole answer from another router: its status, headers and body.
type Whole = (u16, Vec<(String, String)>, Vec<u8>);

/// Where a tenant this router's own directory does not place is.
pub(crate) enum Target {
    /// On a node the global directory places it on: its name, address,
    /// mark and region.
    Node(String, String, Arc<str>, String),
    /// At home in a region keeping its own directory: the region and its
    /// routers.
    Region(String, Vec<String>),
    /// Nowhere this router can send it: the answer.
    Answer(Response),
}

/// What stands for a region's routers where a node's name goes: a label
/// of a metric or a span, the node of a failure.
pub(crate) fn label(region: &str) -> String {
    format!("{HOME}{region}")
}

/// A router's URL as the pool takes it: `host:port`.
pub(crate) fn host(url: &str) -> &str {
    url.strip_prefix("http://")
        .unwrap_or(url)
        .trim_end_matches('/')
}

impl Router {
    /// A router keeping region `cfg.region`'s own directory, `dir`, with
    /// `global` its copy of the global directory, which a follower keeps up
    /// from `cfg.global`: refused where `dir` is another region's, or the
    /// global directory.
    pub fn regional(
        mut dir: Directory,
        cfg: Config,
        repl: Option<Arc<Replication>>,
        global: Directory,
    ) -> std::result::Result<Arc<Router>, String> {
        let Some(region) = cfg.region.clone() else {
            return Err("a region's router names its region (--region)".into());
        };
        if fenec_http::tenants::check_name(&region).is_err() {
            return Err(format!(
                "--region `{region}`: a region is named [a-z0-9_-], 1 to {} characters",
                fenec_http::tenants::MAX_NAME
            ));
        }
        dir.settle_scope(Some(&region))?;
        Ok(Router::assemble(dir, cfg, repl, Some(RwLock::new(global))))
    }

    /// Where `tenant` is, this router's own directory placing it on none of
    /// its nodes: at home in a region keeping its own directory, which the
    /// global directory says -- this router's own, or its copy of it.
    /// `relayed`: another router sent the request here, and it goes on to
    /// no third.
    pub(crate) fn elsewhere(&self, tenant: &str, relayed: bool) -> Target {
        let not_here = |region: &str| {
            Target::Answer(Response::retry_after(
                503,
                &format!(
                    "tenant `{tenant}` lives in region `{region}`, and is not in its directory \
                     as this router has it: it is moving, or being made"
                ),
                1,
            ))
        };
        let to_region = |dir: &Directory, region: &str| match dir.routers(region) {
            Some(routers) if !routers.is_empty() => {
                Target::Region(region.to_string(), routers.to_vec())
            }
            _ => Target::Answer(Response::retry_after(
                503,
                &format!("no router is recorded for region `{region}`"),
                1,
            )),
        };
        let dir = self.read_dir();
        if let Some(region) = dir.home(tenant) {
            if relayed {
                return not_here(region);
            }
            return to_region(&dir, region);
        }
        let arrived = dir.arrived();
        drop(dir);
        let absent = |arrived: bool| {
            Target::Answer(match arrived {
                // A standby whose maps have not come from the primary yet
                // cannot say a tenant does not exist: 404 tells a client to
                // stop asking, this to ask again.
                false => Response::retry_after(
                    503,
                    "the directory has not arrived from the primary yet",
                    1,
                ),
                true => Response::error(404, &format!("no tenant `{tenant}` in the directory")),
            })
        };
        let Some(global) = &self.global else {
            return absent(arrived);
        };
        let g = read(global);
        if let Some(p) = g.placement(tenant) {
            if let Some(n) = g.node(&p.node) {
                let mark = self.mark(&n.token);
                return Target::Node(p.node.clone(), n.addr.clone(), mark, n.region.clone());
            }
        }
        if let Some(region) = g.home(tenant) {
            if relayed || self.cfg.region.as_deref() == Some(region) {
                return not_here(region);
            }
            return to_region(&g, region);
        }
        absent(arrived && g.arrived())
    }

    /// The index of the router of `region`'s list to ask first: the one
    /// that answered last.
    pub(crate) fn first_router(&self, region: &str, routers: &[String]) -> usize {
        let answered = self.answered.lock().unwrap_or_else(|e| e.into_inner());
        answered
            .get(region)
            .copied()
            .filter(|&i| i < routers.len())
            .unwrap_or(0)
    }

    fn note_answered(&self, region: &str, i: usize) {
        let mut answered = self.answered.lock().unwrap_or_else(|e| e.into_inner());
        if answered.get(region) != Some(&i) {
            answered.insert(region.to_string(), i);
        }
    }

    /// Sends a request to `region`'s routers, from the one that answered
    /// last, moving to the next only where the one asked cannot have run
    /// it ([`Tried`]): its answer's head.
    pub(crate) fn relay_send(
        &self,
        region: &str,
        routers: &[String],
        req: &Request,
        headers: &[(&str, &str)],
        held: Duration,
    ) -> std::io::Result<Answer> {
        let start = self.first_router(region, routers);
        let mut last = None;
        for k in 0..routers.len() {
            let i = (start + k) % routers.len();
            let sent = self.pool.send_tried(
                host(&routers[i]),
                req.method.name(),
                &req.target,
                headers,
                &req.body,
                held,
            );
            match sent {
                Ok(a) => {
                    self.note_answered(region, i);
                    return Ok(a);
                }
                Err(Tried::Unrun(e)) => last = Some(e),
                Err(t @ Tried::MayHaveRun(_)) => return Err(t.into_error()),
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("no router is recorded")))
    }

    /// One call to `region`'s routers -- `""` the global directory's -- its
    /// whole answer: the status, the headers, the body. A router answering
    /// that it follows another ran nothing, and the next is asked.
    pub(crate) fn call_routers(
        &self,
        region: &str,
        routers: &[String],
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> std::result::Result<Whole, String> {
        let call = (method, target, headers, body);
        self.call_routers_by(&self.pool, region, routers, call)
    }

    /// [`Self::call_routers`] through `pool`, bounded by its timeout.
    pub(crate) fn call_routers_by(
        &self,
        pool: &crate::upstream::Pool,
        region: &str,
        routers: &[String],
        (method, target, headers, body): (&str, &str, &[(&str, &str)], &[u8]),
    ) -> std::result::Result<Whole, String> {
        let start = self.first_router(region, routers);
        let mut last = String::from("no router is recorded");
        for k in 0..routers.len() {
            let i = (start + k) % routers.len();
            let url = &routers[i];
            match pool.send_tried(host(url), method, target, headers, body, Duration::ZERO) {
                Ok(a) => {
                    let (status, hs) = (a.status, a.headers.clone());
                    let body = a
                        .read_body(pool, false)
                        .map_err(|e| format!("{url} broke off: {e}"))?;
                    if status == 409 && contains(&body, b"follows another") {
                        last = format!("{url} follows another router");
                        continue;
                    }
                    self.note_answered(region, i);
                    return Ok((status, hs, body));
                }
                Err(Tried::Unrun(e)) => last = format!("{url}: {e}"),
                Err(Tried::MayHaveRun(e)) => return Err(format!("{url}: {e}")),
            }
        }
        Err(last)
    }

    /// The headers of a call this router makes of another on its own: its
    /// `--token`, which the routers of a deployment share, and the mark of
    /// the global router.
    pub(crate) fn own_headers(&self) -> (Option<String>, [(&'static str, &'static str); 1]) {
        (
            self.cfg.token.as_ref().map(|t| format!("Bearer {t}")),
            [(GLOBAL_HEADER, "1")],
        )
    }

    /// A call this router makes of `region`'s routers on its own, as the
    /// global router: its whole answer, or the failure a client is told.
    pub(crate) fn ask_region(
        &self,
        region: &str,
        method: &str,
        target: &str,
        extra: &[(&str, &str)],
        body: &[u8],
    ) -> Outcome<Whole> {
        let routers = self
            .read_dir()
            .routers(region)
            .map(<[String]>::to_vec)
            .ok_or_else(|| {
                Fail(
                    409,
                    format!("{} keeps no directory of its own", region_name(region)),
                )
            })?;
        let (auth, global) = self.own_headers();
        let mut headers: Vec<(&str, &str)> = global.to_vec();
        if let Some(a) = &auth {
            headers.push(("Authorization", a));
        }
        headers.extend_from_slice(extra);
        self.call_routers(region, &routers, method, target, &headers, body)
            .map_err(|e| unreachable_region(region, &e))
    }

    /// `req` sent on to `region`'s routers -- `""` and `routers` the global
    /// router's -- as the client sent it, its token with it; `global` marks
    /// it the global router's, carried out there and relayed no further.
    fn relay_admin(
        &self,
        region: &str,
        routers: &[String],
        req: &Request,
        global: bool,
    ) -> Response {
        let mut headers: Vec<(&str, &str)> = Vec::new();
        for name in ["authorization", "content-type"] {
            if let Some(v) = req.header(name) {
                headers.push((name, v));
            }
        }
        if global {
            headers.push((GLOBAL_HEADER, "1"));
        }
        match self.call_routers(
            region,
            routers,
            req.method.name(),
            &req.target,
            &headers,
            &req.body,
        ) {
            Ok((status, hs, body)) => response_of(status, &hs, body),
            Err(e) => {
                let Fail(status, why) = unreachable_region(region, &e);
                Response::retry_after(status, &why, 1)
            }
        }
    }

    /// `req`, a change the global directory takes, sent to the global
    /// router from a region's.
    fn relay_global(&self, req: &Request) -> Response {
        if self.cfg.global.is_empty() {
            return Response::error(
                409,
                "this router keeps its region's directory and knows no global router (--global): \
                 send the change to the global router",
            );
        }
        let routers = self.cfg.global.clone();
        self.relay_admin("", &routers, req, false)
    }

    /// The part of `/_shard/` regions keeping their own directory change:
    /// answered here, or relayed to the router keeping the directory it
    /// touches, or carried out across two of them. `None`: the request is
    /// this router's own directory's, as before regions kept any.
    pub(crate) fn regional_admin(&self, req: &Request) -> Option<Response> {
        let segs = req.segments();
        let rest = &segs[1..];
        let from_global = req.header(GLOBAL_HEADER).is_some();
        let failed = failure;
        match (req.method, rest) {
            (Method::Get, ["region"]) => return Some(self.describe_region()),
            (Method::Get, ["regions"]) => return Some(self.list_regions()),
            (Method::Put | Method::Delete, ["regions", r]) => {
                return Some(match self.cfg.region {
                    Some(_) => self.relay_global(req),
                    None => self.set_region_of(req, r).unwrap_or_else(failed),
                })
            }
            // What the global router asks of a region's to move a tenant
            // across: never a client's to call.
            (Method::Post, ["tenants", t, "export" | "thaw" | "release"])
            | (Method::Put, ["tenants", t, "file"]) => {
                return Some(match from_global {
                    false => Response::error(
                        404,
                        "this is what the global router asks a region's router to move a tenant \
                         across regions: POST /_shard/tenants/<t>/move {\"region\": ..} does it",
                    ),
                    true => self.exchange(req, t, rest).unwrap_or_else(failed),
                })
            }
            _ => {}
        }
        match self.cfg.region.as_deref() {
            Some(_) if from_global => None,
            Some(own) => self.route_in_region(req, rest, own),
            None => self.route_global(req, rest),
        }
    }

    /// On a region's router: a change this region's directory takes stays
    /// here, the rest goes to the global router.
    fn route_in_region(&self, req: &Request, rest: &[&str], own: &str) -> Option<Response> {
        let body = body_fields(req).unwrap_or_default();
        let here = {
            let dir = self.read_dir();
            match (req.method, rest) {
                // A name taken or given back is the global directory's.
                (Method::Put | Method::Delete, ["tenants", _]) => false,
                // A read replica is kept by the directory holding its node:
                // this one's in this region, the global router's -- or
                // another region's, through it -- elsewhere. Their list
                // spans every directory, which the global router gathers.
                (Method::Put | Method::Delete, ["tenants", _, "read-replicas", r]) => *r == own,
                (Method::Get, ["tenants", _, "read-replicas"]) => false,
                // The global directory's, which a create asks by.
                (_, ["tenants", _, "traces", _]) => false,
                (Method::Post, ["tenants", t, "move"]) => {
                    let to = field(&body, "to");
                    let region = place_field(&body, "region").ok().flatten();
                    dir.placement(t).is_some()
                        && region.is_none_or(|r| r == own)
                        && to.is_none_or(|n| dir.node(n).is_some())
                }
                (_, ["tenants", t]) | (_, ["tenants", t, "quota"]) => dir.placement(t).is_some(),
                // A node of this region, unless the global directory holds
                // one of that name: a second would be another node under it.
                (Method::Put, ["nodes", n]) => {
                    let elsewhere = dir.node(n).is_none()
                        && self
                            .global
                            .as_ref()
                            .is_some_and(|g| read(g).node(n).is_some());
                    !elsewhere
                        && place_field(&body, "region")
                            .ok()
                            .flatten()
                            .is_none_or(|r| r == own)
                }
                (_, ["nodes", n, ..]) => dir.node(n).is_some(),
                _ => true,
            }
        };
        match here {
            true => None,
            false => Some(self.relay_global(req)),
        }
    }

    /// On the global router: a change of a region keeping its own directory
    /// goes to its router, one across two directories is carried out here.
    fn route_global(&self, req: &Request, rest: &[&str]) -> Option<Response> {
        // A deployment whose regions keep none: as before regions did.
        if self.read_dir().regions().is_empty() {
            return None;
        }
        enum Do {
            Create(String),
            Delete(String),
            Relay(String),
            Across(String, String, Option<String>),
            Refuse(Fail),
            Gather,
        }
        let body = body_fields(req).unwrap_or_default();
        let todo = {
            let dir = self.read_dir();
            let keeps = |r: &str| dir.routers(r).is_some();
            match (req.method, rest) {
                (Method::Put, ["tenants", _]) => {
                    let node = field(&body, "node");
                    let region = place_field(&body, "region").ok().flatten().or(match node {
                        Some(n) => dir.node(n).map(|n| n.region.as_str()),
                        None => self.cfg.default_region.as_deref(),
                    });
                    region
                        .filter(|r| keeps(r))
                        .map(|r| Do::Create(r.to_string()))
                }
                (Method::Delete, ["tenants", t]) => dir.home(t).map(|r| Do::Delete(r.to_string())),
                (Method::Post, ["tenants", t, "move"]) => {
                    let to = field(&body, "to");
                    let named = place_field(&body, "region").ok().flatten();
                    let (from, homed) = match (dir.placement(t), dir.home(t)) {
                        (Some(p), _) => (dir.region_of(&p.node).to_string(), false),
                        (None, Some(r)) => (r.to_string(), true),
                        (None, None) => return None,
                    };
                    let into = match (named, to) {
                        (Some(r), _) => r.to_string(),
                        (None, Some(n)) if dir.node(n).is_some() => dir.region_of(n).to_string(),
                        // A node this directory does not know: one of the
                        // tenant's own region, which keeps its own.
                        (None, _) => from.clone(),
                    };
                    if from == into {
                        homed.then_some(Do::Relay(from))
                    } else if !homed && !keeps(&into) {
                        // Between two regions this directory keeps: as
                        // before.
                        None
                    } else if named.is_none() {
                        Some(Do::Refuse(Fail(
                            409,
                            format!(
                                "`{t}` is in {}: a move across regions names the region it goes \
                                 to, {{\"region\": \"{into}\"}}",
                                region_name(&from)
                            ),
                        )))
                    } else {
                        Some(Do::Across(from, into, to.map(str::to_string)))
                    }
                }
                (_, ["tenants", t]) | (_, ["tenants", t, "quota"]) => {
                    dir.home(t).map(|r| Do::Relay(r.to_string()))
                }
                // A read replica in a region keeping its own directory is
                // that region's router's to place, whatever region its
                // tenant lives in; their list is every directory's.
                (Method::Put | Method::Delete, ["tenants", _, "read-replicas", r]) => {
                    keeps(r).then(|| Do::Relay(r.to_string()))
                }
                (Method::Get, ["tenants", _, "read-replicas"]) => Some(Do::Gather),
                (Method::Put, ["nodes", n]) if dir.node(n).is_none() => {
                    place_field(&body, "region")
                        .ok()
                        .flatten()
                        .filter(|r| keeps(r))
                        .map(|r| Do::Relay(r.to_string()))
                }
                _ => None,
            }
        };
        let tenant = rest.get(1).copied().unwrap_or("");
        Some(match todo? {
            Do::Create(r) => self.create_homed(req, tenant, &r).unwrap_or_else(failure),
            Do::Delete(r) => self.delete_homed(req, tenant, &r).unwrap_or_else(failure),
            Do::Across(from, into, to) => self
                .move_across(tenant, &from, &into, to.as_deref())
                .unwrap_or_else(failure),
            Do::Relay(r) => {
                let routers = self.read_dir().routers(&r).map(<[String]>::to_vec);
                match routers {
                    Some(routers) => self.relay_admin(&r, &routers, req, true),
                    None => Response::error(
                        409,
                        &format!("{} keeps no directory of its own", region_name(&r)),
                    ),
                }
            }
            Do::Refuse(Fail(s, w)) => Response::error(s, &w),
            Do::Gather => self.gather_readers(tenant),
        })
    }

    /// A tenant made in `region`, which keeps its own directory: its name
    /// taken here first -- every create anywhere is checked against it --
    /// then the region's router asked to make it. Refused there, or not
    /// reached, the name is given back; told there that it exists, it is
    /// kept, which takes in one whose create was made there and whose
    /// answer was lost.
    fn create_homed(&self, req: &Request, tenant: &str, region: &str) -> Outcome<Response> {
        fenec_http::tenants::check_name(tenant).map_err(|r| Fail(r.0, r.1))?;
        let _claim = self.claim(tenant)?;
        if self.read_dir().exists(tenant) {
            return Err(Fail(409, format!("tenant `{tenant}` already exists")));
        }
        self.no_reader_traces(tenant)?;
        self.write_dir()
            .place_home(tenant, region)
            .map_err(internal)?;
        let routers = self
            .read_dir()
            .routers(region)
            .map(<[String]>::to_vec)
            .unwrap_or_default();
        let resp = self.relay_admin(region, &routers, req, true);
        let kept =
            resp.status == 201 || (resp.status == 409 && contains(&resp.body, b"already exists"));
        if !kept {
            if let Err(e) = self.write_dir().remove_tenant(tenant) {
                fenec_http::log!(
                    "tenant `{tenant}`'s name stayed taken in the global directory: {e}"
                );
            }
        }
        Ok(resp)
    }

    /// A tenant of `region`, which keeps its own directory, deleted: by
    /// the region's router first, then its name given back. A region not
    /// reached deletes nothing.
    fn delete_homed(&self, req: &Request, tenant: &str, region: &str) -> Outcome<Response> {
        // Its read replicas on this directory's nodes, which the region's
        // router keeps no record of, held and taken off first, as a
        // tenant of this directory's are ([`Router::delete`]): one that
        // stays refuses the delete, 502, nothing else of it done.
        let _held = self.hold_readers(tenant)?;
        let _claim = self.claim(tenant)?;
        self.take_readers(tenant)?;
        let routers = self
            .read_dir()
            .routers(region)
            .map(<[String]>::to_vec)
            .unwrap_or_default();
        let resp = self.relay_admin(region, &routers, req, true);
        // 404: not there -- a create whose answer was lost, never made.
        if resp.status != 204 && resp.status != 404 {
            return Ok(resp);
        }
        self.write_dir().remove_tenant(tenant).map_err(internal)?;
        Ok(Response::empty(204))
    }

    /// `GET /_shard/region`: what this router keeps -- a region's directory
    /// or the global one -- whether it follows another, and the term it
    /// leases at.
    fn describe_region(&self) -> Response {
        let dir = self.read_dir();
        let holder = self.holder_of(&dir);
        let region = match &self.cfg.region {
            Some(r) => quote(r),
            None => "null".into(),
        };
        let global: Vec<String> = self.cfg.global.iter().map(|u| quote(u)).collect();
        Response::json(
            200,
            format!(
                "{{\"region\":{region},\"following\":{},\"term\":{},\"history\":\"{:016x}\",\"global\":[{}]}}",
                dir.following(),
                holder.term,
                holder.history,
                global.join(",")
            ),
        )
    }

    /// `GET /_shard/regions`: the regions keeping their own directory, their
    /// routers, and how many tenants live there -- from the global
    /// directory, or a region's router's copy of it.
    fn list_regions(&self) -> Response {
        let render = |dir: &Directory| {
            let homes = dir.homes();
            let items: Vec<String> = dir
                .regions()
                .iter()
                .map(|(r, routers)| {
                    let urls: Vec<String> = routers.iter().map(|u| quote(u)).collect();
                    format!(
                        "{{\"name\":{},\"routers\":[{}],\"tenants\":{}}}",
                        quote(r),
                        urls.join(","),
                        homes.iter().filter(|(_, h)| h == r).count()
                    )
                })
                .collect();
            Response::json(200, format!("[{}]", items.join(",")))
        };
        match &self.global {
            Some(g) => render(&read(g)),
            None => render(&self.read_dir()),
        }
    }

    /// `PUT /_shard/regions/<r> {routers}`: region `r` keeps its own
    /// directory, kept by the first of `routers` and followed by the rest;
    /// `DELETE`: it keeps none any more. The global router's.
    fn set_region_of(&self, req: &Request, region: &str) -> Outcome<Response> {
        if region.is_empty() || fenec_http::tenants::check_name(region).is_err() {
            return Err(Fail(
                400,
                format!(
                    "invalid region `{region}`: [a-z0-9_-], 1 to {} characters",
                    fenec_http::tenants::MAX_NAME
                ),
            ));
        }
        if req.method == Method::Delete {
            let mut dir = self.write_dir();
            if dir.homes().iter().any(|(_, r)| r == region) {
                return Err(Fail(
                    409,
                    format!(
                        "tenants live in {}: move or delete them first",
                        region_name(region)
                    ),
                ));
            }
            dir.set_region(region, None).map_err(internal)?;
            return Ok(Response::empty(204));
        }
        let body = body_fields(req).map_err(|e| Fail(400, e))?;
        let mut routers = Vec::new();
        for (k, v) in &body {
            match (k.as_str(), v) {
                ("routers", fenec_core::prelude::Value::List(list)) => {
                    for u in list {
                        match u {
                            fenec_core::prelude::Value::Text(u) => routers.push(u.clone()),
                            _ => return Err(Fail(400, "`routers` holds URLs".into())),
                        }
                    }
                }
                ("router", fenec_core::prelude::Value::Text(u)) => routers.push(u.clone()),
                _ => {}
            }
        }
        if routers.is_empty() {
            return Err(Fail(
                400,
                "the body needs {\"routers\": [\"http://host:port\", ..]}: the router keeping \
                 the region's directory first"
                    .into(),
            ));
        }
        for u in &routers {
            fenec_http::replication::Upstream::check_node(u).map_err(|e| Fail(400, e))?;
        }
        if let Some((n, _)) = self
            .read_dir()
            .nodes()
            .iter()
            .find(|(_, n)| n.region == region)
        {
            return Err(Fail(
                409,
                format!(
                    "node `{n}` of {} is in the global directory: a region keeps its own once \
                     its tenants are moved off and its nodes removed here, then added to its \
                     router",
                    region_name(region)
                ),
            ));
        }
        // Each router asked what it keeps: one at least is the region's,
        // and none another region's or the global directory.
        let (auth, _) = self.own_headers();
        let headers: Vec<(&str, &str)> =
            auth.iter().map(|a| ("Authorization", a.as_str())).collect();
        let mut keeping = 0;
        for u in &routers {
            let Ok((200, _, body)) = self.call_routers(
                region,
                std::slice::from_ref(u),
                "GET",
                "/_shard/region",
                &headers,
                b"",
            ) else {
                continue;
            };
            let said = format!("\"region\":{}", quote(region));
            if !contains(&body, said.as_bytes()) {
                return Err(Fail(
                    409,
                    format!(
                        "{u} keeps another directory than {}'s: {}",
                        region_name(region),
                        String::from_utf8_lossy(&body)
                    ),
                ));
            }
            keeping += 1;
        }
        if keeping == 0 {
            return Err(Fail(
                502,
                format!("no router of {} answered", region_name(region)),
            ));
        }
        self.write_dir()
            .set_region(region, Some(&routers))
            .map_err(internal)?;
        let urls: Vec<String> = routers.iter().map(|u| quote(u)).collect();
        Ok(Response::json(
            200,
            format!(
                "{{\"region\":{},\"routers\":[{}]}}",
                quote(region),
                urls.join(",")
            ),
        ))
    }

    // ------------------------------------------------- a move across regions

    /// What the global router asks of the router keeping a tenant's
    /// directory, to move it across regions.
    fn exchange(&self, req: &Request, tenant: &str, rest: &[&str]) -> Outcome<Response> {
        let _claim = self.claim(tenant)?;
        match rest {
            [_, _, "export"] => {
                let (image, quota, node) = self.export_held(tenant)?;
                let mut resp = Response {
                    status: 200,
                    body: image,
                    content_type: "application/octet-stream",
                    extra: Vec::new(),
                    seq: None,
                    history: None,
                    version: None,
                };
                resp = resp.header("Fenec-Node", &node);
                if let Some(q) = quota {
                    resp = resp.header("Fenec-Quota", &q);
                }
                Ok(resp)
            }
            [_, _, "thaw"] => {
                self.thaw_held(tenant)?;
                Ok(Response::json(
                    200,
                    format!("{{\"thawed\":{}}}", quote(tenant)),
                ))
            }
            [_, _, "release"] => {
                let removed = self.release_held(tenant)?;
                Ok(Response::json(
                    200,
                    format!(
                        "{{\"released\":{},\"source_removed\":{removed}}}",
                        quote(tenant)
                    ),
                ))
            }
            _ => {
                let to = req
                    .query
                    .iter()
                    .find(|(k, _)| k == "to")
                    .map(|(_, v)| v.as_str());
                let quota = req.header("fenec-quota");
                let (node, own) = self.import_held(tenant, to, &req.body, quota)?;
                let region = self.read_dir().region_of(&node).to_string();
                Ok(Response::json(
                    201,
                    format!(
                        "{{\"tenant\":{},\"node\":{}{}{}}}",
                        quote(tenant),
                        quote(&node),
                        crate::region_field(&region),
                        replica_field(&own)
                    ),
                ))
            }
        }
    }

    /// Freezes `tenant` on its node and takes its image, marked moving
    /// here: the image, its quota, and its node. Thawed again where it
    /// fails.
    fn export_held(&self, tenant: &str) -> Outcome<(Vec<u8>, Option<String>, String)> {
        let from = self
            .read_dir()
            .placement(tenant)
            .map(|p| p.node.clone())
            .ok_or_else(|| Fail(404, format!("no tenant `{tenant}` in the directory")))?;
        let src = self.node(&from)?;
        let base = format!("/_admin/tenants/{tenant}");
        self.write_dir()
            .place(tenant, &from, State::Moving)
            .map_err(internal)?;
        let thaw = |why: Fail| -> Fail {
            let _ = self
                .pool
                .call(&src.addr, "POST", &format!("{base}/thaw"), &src.token, b"");
            let _ = self.write_dir().place(tenant, &from, State::Active);
            why
        };
        let freeze = self.pool.call(
            &src.addr,
            "POST",
            &format!("{base}/freeze"),
            &src.token,
            b"",
        );
        expect(freeze, 200, &from).map_err(thaw)?;
        let image = expect(
            self.pool
                .call(&src.addr, "GET", &format!("{base}/file"), &src.token, b""),
            200,
            &from,
        )
        .map_err(thaw)?;
        let quota = self.read_dir().quota(tenant).map(str::to_string);
        Ok((image, quota, from))
    }

    /// Undoes [`Self::export_held`]: the tenant takes writes on its node
    /// again.
    fn thaw_held(&self, tenant: &str) -> Outcome<()> {
        let from = self
            .read_dir()
            .placement(tenant)
            .map(|p| p.node.clone())
            .ok_or_else(|| Fail(404, format!("no tenant `{tenant}` in the directory")))?;
        let src = self.node(&from)?;
        let target = format!("/_admin/tenants/{tenant}/thaw");
        expect(
            self.pool.call(&src.addr, "POST", &target, &src.token, b""),
            200,
            &from,
        )?;
        self.write_dir()
            .place(tenant, &from, State::Active)
            .map_err(internal)
    }

    /// Installs `image` as `tenant` on the node `to` and gives it `quota`:
    /// nothing recorded. Taken back off where its quota is refused.
    fn install(&self, tenant: &str, to: &str, image: &[u8], quota: Option<&str>) -> Outcome<()> {
        let dst = self.node(to)?;
        let base = format!("/_admin/tenants/{tenant}");
        let put = |image: &[u8]| {
            self.pool
                .call(&dst.addr, "PUT", &format!("{base}/file"), &dst.token, image)
        };
        let mut installed = put(image);
        // 409: a copy a move that died before its flip left there.
        if matches!(installed, Ok((409, _))) {
            let _ = self.pool.call(&dst.addr, "DELETE", &base, &dst.token, b"");
            installed = put(image);
        }
        expect(installed, 201, to)?;
        if let Some(w) = quota.and_then(|q| self.put_quota(tenant, to, q)) {
            let _ = self.pool.call(&dst.addr, "DELETE", &base, &dst.token, b"");
            return Err(Fail(502, w));
        }
        Ok(())
    }

    /// Takes `tenant` in from another region: its image installed on the
    /// node `to`, or the one with the fewest bytes here, recorded, leased,
    /// and given its standby's copy and a replica, as a move's target is.
    fn import_held(
        &self,
        tenant: &str,
        to: Option<&str>,
        image: &[u8],
        quota: Option<&str>,
    ) -> Outcome<(String, Option<std::result::Result<String, String>>)> {
        if self.read_dir().exists(tenant) {
            return Err(Fail(409, format!("tenant `{tenant}` already exists here")));
        }
        let to = match to {
            Some(n) => n.to_string(),
            None => self.least_loaded(self.cfg.region.as_deref(), None)?,
        };
        // A read replica of it in the region it arrives in goes: its own
        // copies there are nearer still, and it may sit on `to`.
        let into = self.read_dir().region_of(&to).to_string();
        let _arriving = self.readers_arriving(tenant, &into)?;
        if self.read_dir().is_standby(&to) {
            return Err(Fail(
                409,
                format!("`{to}` is a standby: its tenants follow another node's"),
            ));
        }
        self.install(tenant, &to, image, quota)?;
        let base = format!("/_admin/tenants/{tenant}");
        let recorded = {
            let mut dir = self.write_dir();
            dir.place(tenant, &to, State::Active)
                .and_then(|_| dir.set_quota(tenant, quota))
        };
        if let Err(e) = recorded {
            if let Ok(n) = self.node(&to) {
                let _ = self.pool.call(&n.addr, "DELETE", &base, &n.token, b"");
            }
            let _ = self.write_dir().remove_tenant(tenant);
            return Err(internal(e));
        }
        self.lease_now(&to);
        self.after_arrival(tenant, &to);
        Ok((to.clone(), self.replicate(tenant, &to)))
    }

    /// A tenant newly on `to`: its copy on `to`'s standby, with its quota.
    fn after_arrival(&self, tenant: &str, to: &str) {
        let base = format!("/_admin/tenants/{tenant}");
        if let Some(w) = self
            .on_standby(to, "PUT", &base)
            .or_else(|| self.quota_on_standby(tenant, to))
        {
            fenec_http::log!("tenant `{tenant}` has no replica on `{to}` yet: {w}");
        }
    }

    /// Lets go of `tenant`, moved into another region: out of this
    /// directory first, so its node's lease names it no more -- the node is
    /// frozen meanwhile, and one restarted takes no write before a grant
    /// from here -- then its copies deleted, once every standby router of
    /// this directory has been sent the change. Whether the copy on its
    /// node went.
    fn release_held(&self, tenant: &str) -> Outcome<bool> {
        let (from, replica) = {
            let dir = self.read_dir();
            let p = dir
                .placement(tenant)
                .ok_or_else(|| Fail(404, format!("no tenant `{tenant}` in the directory")))?;
            (p.node.clone(), dir.replica(tenant).map(str::to_string))
        };
        self.write_dir().remove_tenant(tenant).map_err(internal)?;
        self.lease_now(&from);
        Ok(self.remove_copies(tenant, &from, replica.as_deref()))
    }

    /// Deletes what a tenant that left `from` left behind: its standby's
    /// copy, its replica's, and its own, once every standby router has been
    /// sent the change -- one forwarding from a directory before it would
    /// find the node holding the tenant no more. Whether its own went.
    fn remove_copies(&self, tenant: &str, from: &str, replica: Option<&str>) -> bool {
        self.standbys_sent();
        let base = format!("/_admin/tenants/{tenant}");
        if let Some(w) = self.on_standby(from, "DELETE", &base) {
            fenec_http::log!("tenant `{tenant}`'s replica on `{from}`'s standby stayed: {w}");
        }
        if let Some(r) = replica.and_then(|r| self.read_dir().node(r).cloned()) {
            if !matches!(
                self.pool.call(&r.addr, "DELETE", &base, &r.token, b""),
                Ok((204 | 404, _))
            ) {
                fenec_http::log!("tenant `{tenant}`'s replica on {} stayed", r.addr);
            }
        }
        let removed = match self.node(from) {
            Ok(n) => matches!(
                self.pool.call(&n.addr, "DELETE", &base, &n.token, b""),
                Ok((204 | 404, _))
            ),
            Err(_) => false,
        };
        if !removed {
            fenec_http::log!(
                "tenant `{tenant}` left `{from}`, but its copy there could not be removed; it is \
                 frozen there and no longer routed to"
            );
        }
        removed
    }

    /// Moves a tenant from region `from` to region `into` where one of them
    /// keeps its own directory: frozen and taken out of `from`, installed
    /// and recorded in `into`, let go of in `from` -- its directory no
    /// longer placing it, so no lease there names it -- and only then its
    /// home moved in the global directory, which routes it there. Each
    /// step undone where the next fails, so a region that cannot be
    /// reached -- cut off -- leaves the tenant where it was, taking writes
    /// again: 503, to be asked again.
    fn move_across(
        &self,
        tenant: &str,
        from: &str,
        into: &str,
        to: Option<&str>,
    ) -> Outcome<Response> {
        let _claim = self.claim(tenant)?;
        let started = Instant::now();
        let (keeps_from, keeps_into) = {
            let dir = self.read_dir();
            (dir.routers(from).is_some(), dir.routers(into).is_some())
        };
        // Out of `from`: frozen there, its image taken.
        let (image, quota, source) = match keeps_from {
            false => self.export_held(tenant)?,
            true => {
                let (_, hs, image) = self
                    .ask_region(
                        from,
                        "POST",
                        &format!("/_shard/tenants/{tenant}/export"),
                        &[],
                        b"",
                    )
                    .and_then(|answer| answered(answer, 200, from))?;
                let header = |name: &str| {
                    hs.iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(name))
                        .map(|(_, v)| v.clone())
                };
                (
                    image,
                    header("fenec-quota"),
                    header("fenec-node").unwrap_or_default(),
                )
            }
        };
        let thaw = || match keeps_from {
            false => {
                if let Err(Fail(_, w)) = self.thaw_held(tenant) {
                    fenec_http::log!("tenant `{tenant}` stayed frozen on `{source}`: {w}");
                }
            }
            true => {
                let thawed = self.ask_region(
                    from,
                    "POST",
                    &format!("/_shard/tenants/{tenant}/thaw"),
                    &[],
                    b"",
                );
                if let Err(Fail(_, w)) = thawed.and_then(|a| answered(a, 200, from)) {
                    fenec_http::log!(
                        "tenant `{tenant}` stayed frozen in {}: {w}",
                        region_name(from)
                    );
                }
            }
        };
        // Into `into`: installed there, and where `into` keeps its own
        // directory, recorded and leased by its router.
        let landed = match keeps_into {
            false => (|| {
                let to = match to {
                    Some(n) => n.to_string(),
                    None => self.least_loaded(Some(into), None)?,
                };
                let dir = self.read_dir();
                if dir.node(&to).is_some_and(|n| n.region != into) {
                    return Err(Fail(409, format!("`{to}` is not in {}", region_name(into))));
                }
                if dir.is_standby(&to) {
                    return Err(Fail(409, format!("`{to}` is a standby")));
                }
                drop(dir);
                self.install(tenant, &to, &image, quota.as_deref())?;
                Ok((to, String::new()))
            })(),
            true => (|| {
                let mut target = format!("/_shard/tenants/{tenant}/file");
                if let Some(n) = to {
                    target.push_str(&format!("?to={n}"));
                }
                let mut extra = vec![("Content-Type", "application/octet-stream")];
                if let Some(q) = &quota {
                    extra.push(("Fenec-Quota", q.as_str()));
                }
                let (_, _, body) = self
                    .ask_region(into, "PUT", &target, &extra, &image)
                    .and_then(|a| answered(a, 201, into))?;
                let text = String::from_utf8_lossy(&body).into_owned();
                let node = fenec_core::json::parse_object(&text)
                    .ok()
                    .and_then(|f| {
                        f.into_iter().find_map(|(k, v)| match (k.as_str(), v) {
                            ("node", fenec_core::prelude::Value::Text(n)) => Some(n),
                            _ => None,
                        })
                    })
                    .unwrap_or_default();
                // What its router said of its replica, passed on.
                let replica = ["replica_on", "unreplicated"]
                    .iter()
                    .find_map(|k| {
                        let key = format!(",\"{k}\":");
                        text.find(&key)
                            .map(|at| text[at..text.len() - 1].to_string())
                    })
                    .unwrap_or_default();
                Ok((node, replica))
            })(),
        };
        let (to, replica) = match landed {
            Ok(l) => l,
            Err(f) => {
                thaw();
                crate::metrics::moved(false, started.elapsed());
                return Err(f);
            }
        };
        let unland = || match keeps_into {
            false => {
                if let Ok(n) = self.node(&to) {
                    let base = format!("/_admin/tenants/{tenant}");
                    let _ = self.pool.call(&n.addr, "DELETE", &base, &n.token, b"");
                }
            }
            true => {
                let gone = self.ask_region(
                    into,
                    "DELETE",
                    &format!("/_shard/tenants/{tenant}"),
                    &[],
                    b"",
                );
                if let Err(Fail(_, w)) = gone.and_then(|a| answered(a, 204, into)) {
                    fenec_http::log!(
                        "tenant `{tenant}`'s copy in {} stayed after its move was undone: {w}",
                        region_name(into)
                    );
                }
            }
        };
        // Let go of in `from`, where it keeps its own directory: before the
        // home moves, so no lease there names it once it takes writes in
        // `into`.
        if keeps_from {
            let released = self
                .ask_region(
                    from,
                    "POST",
                    &format!("/_shard/tenants/{tenant}/release"),
                    &[],
                    b"",
                )
                .and_then(|a| answered(a, 200, from));
            if let Err(f) = released {
                unland();
                thaw();
                crate::metrics::moved(false, started.elapsed());
                return Err(f);
            }
        }
        // The flip: one statement of the global directory.
        let flipped = match keeps_into {
            false => self.write_dir().place(tenant, &to, State::Active),
            true => self.write_dir().place_home(tenant, into),
        };
        if let Err(e) = flipped {
            fenec_http::log!(
                "tenant `{tenant}` is in {} on `{to}`, but the global directory could not record \
                 it: {e}",
                region_name(into)
            );
            return Err(internal(e));
        }
        let replica = match keeps_into {
            // Arrived in a region this directory keeps: leased, its quota
            // and copies as a move's.
            false => {
                if let Some(q) = &quota {
                    if let Err(e) = self.write_dir().set_quota(tenant, Some(q)) {
                        fenec_http::log!("tenant `{tenant}`'s quota stayed unrecorded: {e}");
                    }
                }
                self.lease_now(&to);
                self.after_arrival(tenant, &to);
                replica_field(&self.replicate(tenant, &to))
            }
            true => replica,
        };
        // Left a region this directory keeps: its node leased without it,
        // its records and copies here gone.
        let removed = match keeps_from {
            true => true,
            false => {
                let replica = self.read_dir().replica(tenant).map(str::to_string);
                {
                    let mut dir = self.write_dir();
                    let _ = dir.set_quota(tenant, None);
                    let _ = dir.set_replica(tenant, None);
                }
                self.lease_now(&source);
                self.remove_copies(tenant, &source, replica.as_deref())
            }
        };
        crate::metrics::moved(true, started.elapsed());
        Ok(Response::json(
            200,
            format!(
                "{{\"tenant\":{},\"from\":{},\"to\":{}{},\"bytes\":{},\"ms\":{},\"source_removed\":{removed}{replica}}}",
                quote(tenant),
                quote(&source),
                quote(&to),
                crate::region_field(into),
                image.len(),
                started.elapsed().as_millis(),
            ),
        ))
    }
}

/// A call's answer, where its status is `want`; its failure otherwise.
fn answered((status, hs, body): Whole, want: u16, region: &str) -> Outcome<Whole> {
    if status == want {
        return Ok((status, hs, body));
    }
    Err(Fail(
        if status >= 500 { 502 } else { status },
        format!(
            "{}'s router: {}",
            region_name(region),
            String::from_utf8_lossy(&body).trim()
        ),
    ))
}

/// The failure a client is told of a region's routers not reached: 503,
/// asked again once the region is.
fn unreachable_region(region: &str, why: &str) -> Fail {
    let whose = match region {
        "" => "the global router".to_string(),
        r => format!("{}'s router", region_name(r)),
    };
    Fail(
        503,
        format!(
            "{whose} cannot be reached ({why}): a change it takes waits until it can, and \
             nothing was changed"
        ),
    )
}

/// A failure as a client is told it: a region not reached with when to ask
/// again.
fn failure(Fail(status, why): Fail) -> Response {
    match status {
        503 => Response::retry_after(503, &why, 1),
        s => Response::error(s, &why),
    }
}

/// An answer another router gave, passed on as it came.
fn response_of(status: u16, headers: &[(String, String)], body: Vec<u8>) -> Response {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let content_type = match header("content-type") {
        Some(t) if t.contains("octet-stream") => "application/octet-stream",
        Some(t) if t.contains("text/plain") => "text/plain; charset=utf-8",
        _ => "application/json; charset=utf-8",
    };
    let mut resp = Response {
        status,
        body,
        content_type,
        extra: Vec::new(),
        seq: None,
        history: None,
        version: None,
    };
    for name in [
        "Retry-After",
        "Fenec-Quota",
        "Fenec-Node",
        "WWW-Authenticate",
    ] {
        if let Some(v) = header(name) {
            resp = resp.header(name, v);
        }
    }
    resp
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_routers_url_is_its_host() {
        assert_eq!(host("http://10.0.0.1:8090"), "10.0.0.1:8090");
        assert_eq!(host("http://h:1/"), "h:1");
        assert_eq!(host("h:1"), "h:1");
        assert_eq!(label("us"), "@us");
    }
}
