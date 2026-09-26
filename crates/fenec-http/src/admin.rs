//! Node administration under `/_admin/`: the surface a router drives.
//!
//! ```text
//! GET    /_admin/stats                 {tenants, disk, memory, open}: flat
//! GET    /_admin/open                  the open tenants: name, memory, frozen
//! GET    /_admin/tenants               names on disk
//! PUT    /_admin/tenants/<t>           create an empty tenant       201 / 409
//! DELETE /_admin/tenants/<t>           close and remove             204
//! POST   /_admin/tenants/<t>/freeze    refuse writes, wait for the ones in flight
//! POST   /_admin/tenants/<t>/thaw
//! GET    /_admin/tenants/<t>/file      the whole image (octet-stream)
//! PUT    /_admin/tenants/<t>/file      install an image as a new tenant
//! POST   /_admin/tenants/<t>/promote   a replica node takes this tenant's writes
//! POST   /_admin/tenants/<t>/follow    {from}: follow the node at `from`, or
//!                                      without it this node's upstream
//! POST   /_admin/lease                 {ms, epoch, primaries?}: the router's
//!                                      lease, on a node started to take one
//!                                      (412: send the primaries)
//! GET    /_admin/lease                 {epoch, primaries, left_ms}
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

    let segs = req.segments();
    let result = match (req.method, &segs[1..]) {
        (Method::Get, ["stats"]) => Ok(stats(tenants)),
        (Method::Get, ["open"]) => Ok(open(tenants)),
        (Method::Get, ["tenants"]) => Ok(names(tenants)),
        (Method::Put, ["tenants", t]) => tenants
            .create(t)
            .map(|_| Response::json(201, format!("{{\"created\":\"{t}\"}}"))),
        (Method::Delete, ["tenants", t]) => tenants.delete(t).map(|_| Response::empty(204)),
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
        }),
        (Method::Put, ["tenants", t, "file"]) => tenants
            .import(t, &req.body)
            .map(|_| Response::json(201, format!("{{\"imported\":\"{t}\"}}"))),
        // The failover, tenant by tenant: what a replica's file needs to
        // take writes. It goes through the admin token the router already
        // holds rather than the replication one, which the router does not.
        (Method::Post, ["tenants", t, "promote"]) => tenants.promote(t).map(|(seq, id)| {
            Response::json(
                200,
                format!("{{\"promoted\":\"{t}\",\"seq\":{seq},\"history\":\"{id:016x}\"}}"),
            )
        }),
        // The other way: a node rejoining as the standby has its tenants
        // follow, as the router asks when it records the pair -- or one
        // tenant follows the node its primary is on, where the router
        // placed its replica.
        (Method::Post, ["tenants", t, "follow"]) => match from(&req.body) {
            Ok(from) => tenants
                .follow(t, from.as_deref())
                .map(|_| Response::json(200, format!("{{\"following\":\"{t}\"}}"))),
            Err(e) => Err(e),
        },
        (Method::Post, ["lease"]) => lease(tenants, &req.body),
        (Method::Get, ["lease"]) => match tenants.lease() {
            Some(l) => Ok(Response::json(200, l.describe())),
            None => Err(no_lease()),
        },
        _ => Err(Refused(404, "no such admin endpoint".into())),
    };
    result.unwrap_or_else(|Refused(status, msg)| Response::error(status, &msg))
}

/// The `from` of a follow's body, when it has one.
fn from(body: &[u8]) -> Result<Option<String>, Refused> {
    let text = String::from_utf8_lossy(body);
    if text.trim().is_empty() {
        return Ok(None);
    }
    let fields = fenec_core::json::parse_object(&text)
        .map_err(|e| Refused(400, format!("the body is not a JSON object: {e}")))?;
    Ok(fields.into_iter().find_map(|(k, v)| match (k.as_str(), v) {
        ("from", fenec_core::prelude::Value::Text(url)) => Some(url),
        _ => None,
    }))
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
    for (k, v) in fields {
        match (k.as_str(), v) {
            ("ms", Value::Int(n)) if n > 0 => ms = Some(n as u64),
            ("epoch", Value::Text(e)) => epoch = Some(e),
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
    match lease.grant(ms, &epoch, primaries) {
        Ok(()) => Ok(Response::json(200, lease.describe())),
        Err(crate::lease::NeedList(held)) => Ok(Response::json(
            412,
            format!(
                "{{\"error\":\"send the primaries: this node holds {}\"}}",
                held.map_or("none".into(), |e| format!("epoch {e}"))
            ),
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

fn names(tenants: &Tenants) -> Response {
    let list: Vec<String> = tenants.names().iter().map(|n| format!("\"{n}\"")).collect();
    Response::json(200, format!("[{}]", list.join(",")))
}
