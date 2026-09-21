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
        _ => Err(Refused(404, "no such admin endpoint".into())),
    };
    result.unwrap_or_else(|Refused(status, msg)| Response::error(status, &msg))
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
