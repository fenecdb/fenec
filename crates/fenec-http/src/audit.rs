//! The audit log, and the wait after a refused token.
//!
//! `--audit <path>` appends a JSON line an event to the file: an HTTP
//! request refused for its token (401), a request to `/_admin/` or
//! `/_shard/` that changes something, and a statement that changes the
//! schema -- `create`, `drop`, `alter`, `compact` -- by its shape, the
//! literals left out as `/_stats/statements` leaves them. Each line says
//! when, over what and from where:
//!
//! ```text
//! {"at":"2026-10-01T09:30:12.041Z","event":"refused","proto":"http","peer":"10.0.0.7:53112","method":"POST","path":"/query"}
//! {"at":"2026-10-01T09:30:12.310Z","event":"schema","proto":"http","peer":"10.0.0.7:53112","statement":"create collection notes (title text)","failed":false}
//! ```
//!
//! Nothing on a read or a write's path writes to it: a line is a refusal,
//! an admin request or a schema change. The lines are written as they come
//! and not synced -- the log of a crash may lose its last lines, never the
//! data's.
//!
//! **A refusal waits** before it is answered, as PostgreSQL's `auth_delay`
//! has it for a password: `--auth-delay` (100 ms) after the first failure
//! from an address within a minute, twice as long after each one more, 5 s
//! at most. A token guessed went from as many tries as the round trips
//! allow to about one every five seconds an address, and a client whose
//! token is right is not slowed at all.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static LOG: OnceLock<Mutex<std::fs::File>> = OnceLock::new();
static DELAY_MS: AtomicU64 = AtomicU64::new(100);
/// Addresses with a failure counted: a success asks the table only when
/// there is one, so a request with a good token takes no lock.
static FAILING: AtomicU64 = AtomicU64::new(0);
/// The longest wait after a failure.
const MOST: Duration = Duration::from_secs(5);
/// Failures further apart than this start the count again.
const WINDOW: Duration = Duration::from_secs(60);
/// Addresses remembered at most; past it the table starts again, which
/// forgives every address once rather than grow without end.
const ADDRESSES: usize = 10_000;

/// Appends the events to `path` from now on.
pub fn open(path: &std::path::Path) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let _ = LOG.set(Mutex::new(file));
    Ok(())
}

/// The wait after a first failure; 0 waits none.
pub fn set_delay(ms: u64) {
    DELAY_MS.store(ms, Ordering::Relaxed);
}

/// Whether events are written.
pub fn on() -> bool {
    LOG.get().is_some()
}

#[derive(Default)]
struct Who {
    proto: &'static str,
    peer: Option<std::net::SocketAddr>,
}

thread_local! {
    // A connection is a thread: who it is stays with the thread.
    static WHO: RefCell<Who> = RefCell::new(Who::default());
    // The request this thread serves came through the router, from this
    // client: the router waits out its refusals, keyed by it.
    static ROUTED: std::cell::Cell<Option<IpAddr>> = const { std::cell::Cell::new(None) };
}

/// The header a router marks what it forwards with ([`router_mark`]), and
/// the one it names the client's address in beside it.
pub const ROUTER_HEADER: &str = "fenec-router";
pub const CLIENT_HEADER: &str = "fenec-client";

/// What a router sends a node in [`ROUTER_HEADER`]: an HMAC of the node's
/// admin token, which the router holds for each node already. Behind the
/// router every request reaches a node from the router's address, so the
/// node's wait after a refusal keyed every client's refusals to that one
/// address -- four forged tokens in a row waited 101, 204, 402 and 803 ms
/// and a fifth client's expired token would have waited 1.6 s -- and any
/// good token from anyone started the attacker's count again. So the
/// router waits out a refusal itself, keyed by its client's address, and a
/// node waits none for a request carrying this mark: the client's address
/// is believed from no one else. Derived rather than the token itself, so
/// a mark seen on the wire reaches no `/_admin/`.
pub fn router_mark(admin_token: &str) -> String {
    crate::crypto::b64url_encode(&crate::crypto::hmac_sha256(
        admin_token.as_bytes(),
        b"fenec-router: a request forwarded, the client named beside it",
    ))
}

/// Whether `req` came through the router `mark` names, and from whom: a
/// node asks at each request. With no mark, or a header that is not it, the
/// request is the connection's own -- a client sending the headers itself
/// is believed in nothing.
pub fn request(req: &crate::http::Request, mark: Option<&str>) -> Option<IpAddr> {
    let client = mark.and_then(|mark| {
        let given = req.header(ROUTER_HEADER)?;
        if !crate::constant_eq(given.as_bytes(), mark.as_bytes()) {
            return None;
        }
        req.header(CLIENT_HEADER)?.trim().parse::<IpAddr>().ok()
    });
    ROUTED.with(|r| r.set(client));
    client
}

/// The connection this thread serves.
pub fn connection(proto: &'static str, peer: Option<std::net::SocketAddr>) {
    WHO.with(|w| *w.borrow_mut() = Who { proto, peer });
}

/// A value of an event's line.
pub enum Field<'a> {
    Text(&'a str),
    Int(u64),
    Bool(bool),
}

/// Writes an event, with when, over what, from where and as whom.
pub fn event(name: &str, fields: &[(&str, Field)]) {
    let Some(log) = LOG.get() else {
        return;
    };
    let at = fenec_core::time::now_ms().map_or_else(|_| "?".into(), fenec_core::time::format_iso);
    let mut line = String::with_capacity(160);
    let text = |line: &mut String, k: &str, v: &str| {
        line.push_str(",\"");
        line.push_str(k);
        line.push_str("\":");
        fenec_core::json::escape_into(line, v);
    };
    line.push_str("{\"at\":");
    fenec_core::json::escape_into(&mut line, &at);
    text(&mut line, "event", name);
    WHO.with(|w| {
        let w = w.borrow();
        if !w.proto.is_empty() {
            text(&mut line, "proto", w.proto);
        }
        match (ROUTED.with(|r| r.get()), w.peer) {
            // The client the router forwarded for, and the router.
            (Some(client), router) => {
                text(&mut line, "peer", &client.to_string());
                if let Some(r) = router {
                    text(&mut line, "via", &r.to_string());
                }
            }
            (None, Some(p)) => text(&mut line, "peer", &p.to_string()),
            (None, None) => {}
        }
    });
    for (k, v) in fields {
        match v {
            Field::Text(t) => text(&mut line, k, t),
            Field::Int(n) => {
                line.push_str(&format!(",\"{k}\":{n}"));
            }
            Field::Bool(b) => {
                line.push_str(&format!(",\"{k}\":{b}"));
            }
        }
    }
    line.push_str("}\n");
    let mut f = log.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = f.write_all(line.as_bytes()) {
        crate::log!("audit log: {e}");
    }
}

/// A statement counted by its shape (`statements::record`): an event where
/// it changes the schema.
pub fn statement(tenant: Option<&str>, shape: &str, failed: bool) {
    if !on() {
        return;
    }
    for one in shape.split(';') {
        let word = one.split_whitespace().next().unwrap_or("");
        if ["create", "drop", "alter", "compact"]
            .iter()
            .any(|w| word.eq_ignore_ascii_case(w))
        {
            let mut fields = vec![("statement", Field::Text(one.trim()))];
            if let Some(t) = tenant {
                fields.push(("tenant", Field::Text(t)));
            }
            fields.push(("failed", Field::Bool(failed)));
            event("schema", &fields);
        }
    }
}

/// An HTTP request answered with `status`: refused for its token (401), it
/// waits and is logged; one that changes a node's
/// tenants or a router's placement is logged. A request with a token that
/// is not refused starts its address's count again.
pub fn http(req: &crate::http::Request, status: u16, peer: Option<std::net::SocketAddr>) {
    use crate::http::Method;
    let ip = peer.map(|p| p.ip());
    let path = req.target.split('?').next().unwrap_or("");
    if status == 401 {
        event(
            "refused",
            &[
                ("method", Field::Text(req.method.name())),
                ("path", Field::Text(path)),
            ],
        );
        // Forwarded by the router, which waits it out by the client's
        // address: counted here, by the router's, it would hold every
        // client of the router to one count.
        if ROUTED.with(|r| r.get()).is_none() {
            std::thread::sleep(failed(ip));
        }
        return;
    }
    if req.header("authorization").is_some() && ROUTED.with(|r| r.get()).is_none() {
        succeeded(ip);
    }
    let admin = path.starts_with("/_admin/") || path.starts_with("/_shard/");
    if admin && on() && !matches!(req.method, Method::Get | Method::Head | Method::Options) {
        event(
            "admin",
            &[
                ("method", Field::Text(req.method.name())),
                ("path", Field::Text(path)),
                ("status", Field::Int(status as u64)),
            ],
        );
    }
}

type Failures = HashMap<IpAddr, (u32, Instant)>;

fn failures() -> &'static Mutex<Failures> {
    static F: OnceLock<Mutex<Failures>> = OnceLock::new();
    F.get_or_init(Default::default)
}

/// How long to wait before answering a refusal to `peer`, the failure
/// counted.
pub fn failed(peer: Option<IpAddr>) -> Duration {
    let base = DELAY_MS.load(Ordering::Relaxed);
    let Some(ip) = peer.filter(|_| base > 0) else {
        return Duration::ZERO;
    };
    let now = Instant::now();
    let mut map = failures().lock().unwrap_or_else(|e| e.into_inner());
    if map.len() >= ADDRESSES && !map.contains_key(&ip) {
        map.clear();
    }
    FAILING.store(map.len() as u64 + 1, Ordering::Relaxed);
    let e = map.entry(ip).or_insert((0, now));
    if now.duration_since(e.1) > WINDOW {
        e.0 = 0;
    }
    e.0 = e.0.saturating_add(1);
    e.1 = now;
    let doubled = base.saturating_mul(1u64 << (e.0 - 1).min(16));
    Duration::from_millis(doubled).min(MOST)
}

/// A token from `peer` was taken: its count starts again.
pub fn succeeded(peer: Option<IpAddr>) {
    if let Some(ip) = peer.filter(|_| FAILING.load(Ordering::Relaxed) > 0) {
        let mut map = failures().lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&ip);
        FAILING.store(map.len() as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_doubles_to_its_ceiling_and_a_success_forgives() {
        let ip: Option<IpAddr> = Some("203.0.113.9".parse().unwrap());
        let waits: Vec<u128> = (0..8).map(|_| failed(ip).as_millis()).collect();
        assert_eq!(waits, [100, 200, 400, 800, 1600, 3200, 5000, 5000]);
        succeeded(ip);
        assert_eq!(failed(ip).as_millis(), 100);
        // Another address has a count of its own; no address waits none.
        assert_eq!(
            failed(Some("203.0.113.10".parse().unwrap())).as_millis(),
            100
        );
        assert_eq!(failed(None), Duration::ZERO);
    }
}
