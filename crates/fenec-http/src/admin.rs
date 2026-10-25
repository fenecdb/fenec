//! Node administration under `/_admin/`: the surface a router drives.
//!
//! ```text
//! GET    /_admin/stats                 {tenants, disk, memory, open}: flat
//! GET    /_admin/open                  the open tenants: name, memory, frozen
//! GET    /_admin/tenants               names on disk
//! GET    /_admin/sizes                 {"<tenant>": bytes on disk, ...}
//! PUT    /_admin/tenants/<t>           create an empty tenant       201 / 409
//! POST   /_admin/tenants/<t>/schema    a description, applied as /_schema/apply
//!                                      applies it: what a router's create with
//!                                      a schema sends, once the tenant is placed
//! DELETE /_admin/tenants/<t>           close and remove             204
//! POST   /_admin/tenants/<t>/query     {query, params}: a read, answered as
//!                                      /query answers it, any write refused
//!                                      (403): a router's query across tenants
//! POST   /_admin/tenants/<t>/freeze    refuse writes, wait for the ones in flight
//! POST   /_admin/tenants/<t>/thaw
//! GET    /_admin/tenants/<t>/file      the whole image (octet-stream)
//! PUT    /_admin/tenants/<t>/file      install an image as a new tenant
//! POST   /_admin/tenants/<t>/promote   a replica node takes this tenant's writes
//! POST   /_admin/tenants/<t>/follow    {from}: follow the node at `from`, or
//!                                      without it this node's upstream
//! PUT    /_admin/tenants/<t>/quota     {bytes, rows, streams, requests, writes,
//!                                      write_bytes}: what is set for the tenant
//!                                      over the node's quota, kept beside its
//!                                      file (0 none, left out or null the
//!                                      node's); the whole set, replaced
//! GET    /_admin/tenants/<t>/quota     {tenant, open, set, quota, usage}
//! GET    /_admin/quotas                every tenant's, an array; none opened
//! POST   /_admin/lease                 {ms, epoch, primaries?, authority?,
//!                                      term?, history?, grant?, acked?}: the
//!                                      router's lease, on a node started to
//!                                      take one, run from when this node
//!                                      answered the grant `acked` names; its
//!                                      answer the lease, `took`, whether it
//!                                      took the grant's list, and `ran`,
//!                                      whether it ran the lease (412: send the
//!                                      primaries; 409 with held_by: a lease
//!                                      from a newer term holds it)
//! GET    /_admin/lease                 {epoch, primaries, left_ms, authority,
//!                                      term, history, grant}
//! ```
//!
//! A separate token from the data one: a client that may read and write a
//! tenant must not be able to delete it, or take another tenant's image.
//! Without `--admin-token` the whole prefix answers 404 -- off, not open.

use crate::http::{Method, Request, Response};
use crate::tenants::{Refused, Tenants};
use crate::{constant_eq, Config};

pub fn handle(tenants: &Tenants, cfg: &Config, req: &Request) -> Response {
    let Some(token) = &cfg.admin_token else {
        return Response::error(404, "the admin endpoints are off (--admin-token)");
    };
    let given = req
        .header("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_eq(given.as_bytes(), token.as_bytes()) {
        return Response::error(401, "invalid or missing admin token")
            .header("WWW-Authenticate", "Bearer");
    }
    crate::audit::verified();

    let segs = req.segments();
    let result = match (req.method, &segs[1..]) {
        (Method::Get, ["stats"]) => Ok(stats(tenants)),
        (Method::Get, ["open"]) => Ok(open(tenants)),
        (Method::Get, ["tenants"]) => Ok(names(tenants)),
        (Method::Get, ["sizes"]) => Ok(sizes(tenants)),
        (Method::Get, ["quotas"]) => Ok(Response::json(200, tenants.quota_reports())),
        // A tenant's quota is the router's to set, as its placement is: the
        // node keeps what it is sent, and a move or a failover sends it on.
        (Method::Put, ["tenants", t, "quota"]) => {
            crate::quota::Set::parse(&String::from_utf8_lossy(&req.body))
                .map_err(|e| Refused(400, e))
                .and_then(|set| tenants.set_quota(t, set))
                .and_then(|_| tenants.quota_report(t))
                .map(|report| Response::json(200, report))
        }
        (Method::Get, ["tenants", t, "quota"]) => tenants
            .quota_report(t)
            .map(|report| Response::json(200, report)),
        // The lineage the tenant's history is of, which a router binds its
        // read replicas to.
        (Method::Put, ["tenants", t]) => tenants.create(t).map(|tenant| {
            Response::json(
                201,
                format!("{{\"created\":\"{t}\",{}}}", stands_json(&tenant)),
            )
        }),
        (Method::Delete, ["tenants", t]) => tenants.delete(t).map(|_| Response::empty(204)),
        // A file the router left here, deleted only where it holds nothing
        // the router did not leave ([`Tenants::take_back`]). A path of its
        // own, not a parameter on `DELETE`: a node from before it answers
        // this 404 and deletes nothing, where it would have read past an
        // unknown parameter and deleted whatever stood under the name.
        (Method::Post, ["tenants", t, "take-back"]) => {
            taken_back(&req.body).and_then(|(lineage, seq)| {
                match tenants.take_back(t, lineage, seq) {
                    Ok(crate::tenants::TakenBack::Deleted) => {
                        Ok(Response::json(200, r#"{"outcome":"deleted"}"#))
                    }
                    Ok(crate::tenants::TakenBack::Absent) => {
                        Ok(Response::json(200, r#"{"outcome":"absent"}"#))
                    }
                    // Its judgment, which waiting does not change. A
                    // refusal of the moment -- the tenant in use -- names
                    // no outcome, and the router asks again.
                    Ok(crate::tenants::TakenBack::Kept(why)) => {
                        let mut body = String::from("{\"outcome\":\"kept\",\"error\":");
                        fenec_core::json::escape_into(&mut body, &why);
                        body.push('}');
                        Ok(Response::json(409, body))
                    }
                    Err(e) => Err(e),
                }
            })
        }
        // The node's admin token, which the router holds, rather than its
        // data token, which reaches every tenant: an app creating tenants
        // needs the router's token alone.
        // Asked of the lease again as it is answered, as a tenant's write is.
        (Method::Post, ["tenants", t, "schema"]) => tenants.get(t).map(|tenant| {
            let _held = tenant.enter();
            match tenant.writable() {
                Err(why) => Response::retry_after(503, &why, 1),
                Ok(()) => {
                    let body = String::from_utf8_lossy(&req.body);
                    let applied = crate::apply_schema(&tenant.db, cfg, &body);
                    crate::answered_under_lease(&tenant, req, applied)
                }
            }
        }),
        // A read of one tenant for a router's query across tenants
        // (`fenec-shard`'s `POST /_shard/query`), with the admin token it
        // holds: the data token reaches every tenant and writes, and the
        // router holds it nowhere. A token that takes a tenant's whole
        // image (`GET .../file`) learns nothing more by reading it, and the
        // read refuses every statement that writes.
        (Method::Post, ["tenants", t, "query"]) => tenants
            .get(t)
            .map(|tenant| crate::admin_query(&tenant, cfg, req)),
        (Method::Post, ["tenants", t, "freeze"]) => tenants
            .freeze(t)
            .map(|_| Response::json(200, "{\"frozen\":true}")),
        (Method::Post, ["tenants", t, "thaw"]) => tenants
            .thaw(t)
            .map(|_| Response::json(200, "{\"frozen\":false}")),
        (Method::Get, ["tenants", t, "file"]) => tenants.export(t).map(|image| Response {
            status: 200,
            body: image,
            content_type: "application/octet-stream",
            extra: Vec::new(),
            seq: None,
            history: None,
            version: None,
        }),
        // A router recording the lineage of a tenant made before it kept
        // any, to bind its read replicas to, or asking the primary it
        // places a tenant on which lineage it is of. `ready`: whether the
        // file holds its primary's history -- a copy that has not taken its
        // first image is of a lineage of its own, a new file's, and says
        // nothing of the tenant it copies.
        (Method::Get, ["tenants", t, "lineage"]) => tenants.get(t).map(|tenant| {
            Response::json(
                200,
                format!(
                    "{{\"lineage\":{},\"ready\":{}}}",
                    lineage_json(&tenant),
                    tenant.ready()
                ),
            )
        }),
        (Method::Put, ["tenants", t, "file"]) => tenants
            .import(t, &req.body)
            .and_then(|_| tenants.get(t))
            .map(|tenant| {
                Response::json(
                    201,
                    format!("{{\"imported\":\"{t}\",{}}}", stands_json(&tenant)),
                )
            }),
        // The failover, tenant by tenant: what a replica's file needs to
        // take writes. It goes through the admin token the router already
        // holds rather than the replication one, which the router does not.
        // A body says what the router knows of the tenant, which the copy is
        // held to, or forces it ([`expected`]).
        (Method::Post, ["tenants", t, "promote"]) => superseded(tenants, req)
            .and_then(|_| expected(&req.body))
            .and_then(|want| tenants.promote_as(t, &want))
            .map(|(seq, id)| {
                // Its lineage, which the router records: another file of
                // the tenant, promoted by force, is of one of its own.
                let lineage = tenants.get(t).map_or("null".into(), |t| lineage_json(&t));
                Response::json(
                    200,
                    format!(
                        "{{\"promoted\":\"{t}\",\"seq\":{seq},\"history\":\"{id:016x}\",\
                         \"lineage\":{lineage}}}"
                    ),
                )
            }),
        // The other way: a node rejoining as the standby has its tenants
        // follow, as the router asks when it records the pair -- or one
        // tenant follows the node its primary is on, where the router
        // placed its replica.
        (Method::Post, ["tenants", t, "follow"]) => {
            match superseded(tenants, req).and_then(|_| from(&req.body)) {
                Ok((from, lineage)) => tenants
                    .follow_lineage(t, from.as_deref(), lineage)
                    .map(|_| Response::json(200, format!("{{\"following\":\"{t}\"}}"))),
                Err(e) => Err(e),
            }
        }
        (Method::Post, ["lease"]) => lease(tenants, &req.body),
        (Method::Get, ["lease"]) => match tenants.lease() {
            Some(l) => Ok(Response::json(200, l.describe())),
            None => Err(no_lease()),
        },
        _ => Err(Refused(404, "no such admin endpoint".into())),
    };
    result.unwrap_or_else(Refused::response)
}

/// A tenant's schema as `fenec_abi::read` reads it from FenecQL `create
/// collection` text, checked: what a router sends a node once it has placed
/// the tenant, refused before anything is placed when it does not read.
pub fn schema_description(fenecql: &str) -> Result<String, String> {
    let mut d = String::from(r#"{"format":1,"fenecql":"#);
    fenec_core::json::escape_into(&mut d, fenecql);
    d.push('}');
    fenec_abi::read(&d).map_err(|e| format!("the schema does not read: {e}"))?;
    Ok(d)
}

/// The `from` of a follow's body, when it has one.
/// A follow's body: the node to follow, and the lineage its tenant must be
/// of (`"lineage": "<8 hex>"`), a router's read replica told which tenant
/// it copies.
fn from(body: &[u8]) -> Result<(Option<String>, Option<u32>), Refused> {
    let text = String::from_utf8_lossy(body);
    if text.trim().is_empty() {
        return Ok((None, None));
    }
    let fields = fenec_core::json::parse_object(&text)
        .map_err(|e| Refused(400, format!("the body is not a JSON object: {e}")))?;
    let (mut from, mut lineage) = (None, None);
    for (k, v) in fields {
        match (k.as_str(), v) {
            ("from", fenec_core::prelude::Value::Text(url)) => from = Some(url),
            ("lineage", fenec_core::prelude::Value::Text(l)) => {
                lineage = Some(
                    crate::replication::parse_lineage(&l)
                        .ok_or_else(|| Refused(400, "`lineage` is 8 hex digits".into()))?,
                )
            }
            _ => {}
        }
    }
    Ok((from, lineage))
}

/// A promotion's body: the lineage the tenant is of (`"lineage": "<8
/// hex>"`), the history its primary forked at its last failover
/// (`"history": "<16 hex>"`), and `"force": true` for an operator promoting
/// it whatever it holds. Empty, nothing is said, as a router from before
/// lineages says nothing.
fn expected(body: &[u8]) -> Result<crate::tenants::Expected, Refused> {
    use fenec_core::prelude::Value;
    let mut want = crate::tenants::Expected::default();
    let text = String::from_utf8_lossy(body);
    if text.trim().is_empty() {
        return Ok(want);
    }
    let fields = fenec_core::json::parse_object(&text)
        .map_err(|e| Refused(400, format!("the body is not a JSON object: {e}")))?;
    for (k, v) in fields {
        match (k.as_str(), v) {
            ("lineage", Value::Text(l)) => {
                want.lineage = Some(
                    crate::replication::parse_lineage(&l)
                        .ok_or_else(|| Refused(400, "`lineage` is 8 hex digits".into()))?,
                )
            }
            ("history", Value::Text(h)) if h.len() == 16 => {
                want.history = Some(
                    u64::from_str_radix(&h, 16)
                        .map_err(|_| Refused(400, "`history` is 16 hex digits".into()))?,
                )
            }
            ("history", _) => return Err(Refused(400, "`history` is 16 hex digits".into())),
            ("force", Value::Bool(f)) => want.force = f,
            ("force", _) => return Err(Refused(400, "`force` is true or false".into())),
            _ => {}
        }
    }
    Ok(want)
}

/// A take-back's body (`{"lineage": "<8 hex>" | null, "seq": <n>}`): the
/// lineage the router knows the file was made of, if it knows one, and
/// the change it left it at, 0 where it knows none -- a file holding no
/// write.
fn taken_back(body: &[u8]) -> Result<(Option<u32>, u64), Refused> {
    use fenec_core::prelude::Value;
    let fields = fenec_core::json::parse_object(&String::from_utf8_lossy(body))
        .map_err(|e| Refused(400, format!("the body is not a JSON object: {e}")))?;
    let (mut lineage, mut seq) = (None, None);
    for (k, v) in fields {
        match (k.as_str(), v) {
            ("lineage", Value::Text(l)) => {
                lineage = Some(
                    crate::replication::parse_lineage(&l)
                        .ok_or_else(|| Refused(400, "`lineage` is 8 hex digits".into()))?,
                )
            }
            ("lineage", Value::Null) => {}
            ("lineage", _) => return Err(Refused(400, "`lineage` is 8 hex digits".into())),
            ("seq", Value::Int(n)) if n >= 0 => seq = Some(n as u64),
            ("seq", _) => return Err(Refused(400, "`seq` is a change, 0 or more".into())),
            _ => {}
        }
    }
    let seq =
        seq.ok_or_else(|| Refused(400, "a take-back names the change it left: `seq`".into()))?;
    Ok((lineage, seq))
}

/// Where a tenant just made or installed stands, as JSON members: its
/// lineage and its change, which a router taking the file back later asks
/// it to stand at still ([`Tenants::take_back`]).
fn stands_json(tenant: &crate::tenants::Tenant) -> String {
    let seq = tenant
        .db
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .change_seq();
    format!("\"lineage\":{},\"seq\":{seq}", lineage_json(tenant))
}

/// A tenant's lineage as JSON: 8 hex digits, or `null` for one that never
/// had a history of its own.
fn lineage_json(tenant: &crate::tenants::Tenant) -> String {
    let db = tenant.db.read().unwrap_or_else(|e| e.into_inner());
    match crate::replication::lineage_of(db.history()) {
        Some(l) => format!("\"{}\"", crate::replication::lineage_text(l)),
        None => "null".into(),
    }
}

/// A promotion or a follow from a router a newer term superseded, on a
/// node it leases: refused, so that a router promoted over one that is not
/// gone keeps the old one from forking or re-pointing the copies it leases
/// now -- the old one's lease round fails over a node it no longer
/// reaches, promoting a replica the new one keeps following its primary. A
/// call naming no holder -- an operator's, a router's from before terms --
/// is let through.
fn superseded(tenants: &Tenants, req: &Request) -> Result<(), Refused> {
    let Some(lease) = tenants.lease() else {
        return Ok(());
    };
    let holder = req
        .header(crate::lease::HOLDER_HEADER)
        .and_then(crate::lease::Holder::parse);
    lease.admits(holder.as_ref()).map_err(|e| Refused(409, e))
}

fn no_lease() -> Refused {
    Refused(
        409,
        "this node takes no lease: start it with --lease for its router to fail it over \
         on its own"
            .into(),
    )
}

/// A grant of the router's lease. Without the list it names by `epoch`,
/// when the node does not hold that one -- after a restart -- the answer is
/// 412 and the router sends it again with the list.
fn lease(tenants: &Tenants, body: &[u8]) -> Result<Response, Refused> {
    use fenec_core::prelude::Value;
    let lease = tenants.lease().ok_or_else(no_lease)?;
    let text = String::from_utf8_lossy(body);
    let fields = fenec_core::json::parse_object(&text)
        .map_err(|e| Refused(400, format!("the body is not a JSON object: {e}")))?;
    let (mut ms, mut epoch, mut primaries) = (None, None, None);
    // Who grants it: a router from before terms names nobody, and is the
    // global directory at term 0. Its number and the grant it acknowledges:
    // a router from before numbers sends neither.
    let mut holder = crate::lease::Holder::default();
    let (mut grant, mut acked) = (None, None);
    let hex = |field: &str, h: &str| {
        u64::from_str_radix(h, 16).map_err(|_| Refused(400, format!("`{field}` is 16 hex digits")))
    };
    for (k, v) in fields {
        match (k.as_str(), v) {
            ("ms", Value::Int(n)) if n > 0 => ms = Some(n as u64),
            ("epoch", Value::Text(e)) => epoch = Some(e),
            ("authority", Value::Text(a)) => holder.authority = a,
            ("term", Value::Int(n)) if n >= 0 => holder.term = n as u64,
            ("history", Value::Text(h)) => holder.history = hex("history", &h)?,
            ("grant", Value::Text(g)) => grant = Some(hex("grant", &g)?),
            ("acked", Value::Text(a)) => acked = Some(hex("acked", &a)?),
            ("primaries", Value::List(list)) => {
                let names = list.into_iter().map(|v| match v {
                    Value::Text(name) => Ok(name),
                    _ => Err(Refused(400, "`primaries` holds tenant names".into())),
                });
                primaries = Some(names.collect::<Result<Vec<_>, _>>()?);
            }
            _ => {}
        }
    }
    let (Some(ms), Some(epoch)) = (ms, epoch) else {
        return Err(Refused(
            400,
            "a lease needs {\"ms\": >0, \"epoch\": \"...\"}".into(),
        ));
    };
    // An acknowledgement without a number of its own is no grant a router
    // sends, and taken unnumbered it would run a lease from now.
    if grant.is_none() && acked.is_some() {
        return Err(Refused(
            400,
            "`acked` comes with the grant's own `grant`".into(),
        ));
    }
    let numbered = grant.map(|grant| crate::lease::Numbered { grant, acked });
    match lease.grant_from(&holder, ms, &epoch, primaries, numbered) {
        // Whether it took the grant -- its list the one held now -- and ran
        // the lease from its acknowledgement, beside the lease: a router
        // reads a grant answered and not taken, from a router before it or
        // a holder before it whose lease still runs, as one to acknowledge
        // at once with its list, and counts the node by its own lease only
        // once one ran it; a node from before says neither, and takes every
        // grant.
        Ok(took) => {
            let lease = lease.describe();
            let rest = lease.strip_prefix('{').unwrap_or(&lease);
            let (list, ran) = (took.list, took.lease);
            Ok(Response::json(
                200,
                format!("{{\"took\":{list},\"ran\":{ran},{rest}"),
            ))
        }
        Err(crate::lease::Refusal::NeedList(held)) => Ok(Response::json(
            412,
            format!(
                "{{\"error\":\"send the primaries: this node holds {}\"}}",
                held.map_or("none".into(), |e| format!("epoch {e}"))
            ),
        )),
        // 409, which a router from before terms reads as a node taking no
        // lease: it then never fails the node over, as it must not.
        Err(crate::lease::Refusal::Superseded(held)) => Ok(Response::json(
            409,
            crate::lease::Lease::superseded_body(&held),
        )),
    }
}

/// Flat on purpose: it is what a router reads, and a flat object is what
/// `json::parse_object` accepts -- the per-tenant list is `/_admin/open`.
fn stats(tenants: &Tenants) -> Response {
    let s = tenants.stats();
    let memory: usize = s.open.iter().map(|(_, m, _)| m).sum();
    Response::json(
        200,
        format!(
            "{{\"tenants\":{},\"disk\":{},\"memory\":{},\"open\":{}}}",
            s.tenants,
            s.disk,
            memory,
            s.open.len()
        ),
    )
}

fn open(tenants: &Tenants) -> Response {
    let s = tenants.stats();
    let mut out = String::from("[");
    for (i, (name, mem, frozen)) in s.open.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"name\":\"{name}\",\"memory\":{mem},\"frozen\":{frozen}}}"
        ));
    }
    out.push(']');
    Response::json(200, out)
}

/// `{"<tenant>": bytes, ...}`: each tenant's file and sync log, which a
/// router joins into its list of tenants for the studio. Read from the file
/// system's sizes, so no tenant is opened for it.
fn sizes(tenants: &Tenants) -> Response {
    let mut out = String::from("{");
    for (i, n) in tenants.names().iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        fenec_core::json::escape_into(&mut out, n);
        out.push(':');
        out.push_str(&tenants.disk(n).to_string());
    }
    out.push('}');
    Response::json(200, out)
}

fn names(tenants: &Tenants) -> Response {
    let list: Vec<String> = tenants.names().iter().map(|n| format!("\"{n}\"")).collect();
    Response::json(200, format!("[{}]", list.join(",")))
}
