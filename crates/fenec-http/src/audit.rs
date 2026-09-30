//! The audit log, and the wait after a failed login.
//!
//! `--audit <path>` appends a JSON line an event to the file: a login over
//! the pg wire and one refused, an HTTP request refused for its token (401),
//! a request to `/_admin/` or `/_shard/` that changes something, and a
//! statement that changes the schema -- `create`, `drop`, `alter`,
//! `compact`, a SQL `CREATE TABLE` -- by its shape, the literals left out as
//! `/_stats/statements` leaves them. Each line says when, over what, from
//! where and as whom:
//!
//! ```text
//! {"at":"2026-10-01T09:30:12.041Z","event":"login","proto":"pg","peer":"10.0.0.7:53112","user":"app","database":"acme"}
//! {"at":"2026-10-01T09:30:12.310Z","event":"schema","proto":"pg","peer":"10.0.0.7:53112","user":"app","statement":"create collection notes (title text)","failed":false}
//! ```
//!
//! Nothing on a read or a write's path writes to it: a line is a login, a
//! refusal or a schema change. The lines are written as they come and not
//! synced -- the log of a crash may lose its last lines, never the data's.
//!
//! **A failed login waits** before it is answered, as PostgreSQL's
//! `auth_delay` has it: `--auth-delay` (100 ms) after the first failure from
//! an address within a minute, twice as long after each one more, 5 s at
//! most. A password guessed over the pg wire went from as many tries as the
//! round trips allow to about one every five seconds an address, and a
//! client that gets it right is not slowed at all.

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
    user: Option<String>,
}

thread_local! {
    // A connection is a thread: who it is stays with the thread.
    static WHO: RefCell<Who> = RefCell::new(Who::default());
}

/// The connection this thread serves.
pub fn connection(proto: &'static str, peer: Option<std::net::SocketAddr>) {
    WHO.with(|w| {
        *w.borrow_mut() = Who {
            proto,
            peer,
            user: None,
        }
    });
}

/// Who the connection logged in as.
pub fn user(name: &str) {
    WHO.with(|w| w.borrow_mut().user = Some(name.to_string()));
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
        if let Some(p) = w.peer {
            text(&mut line, "peer", &p.to_string());
        }
        if let Some(u) = &w.user {
            text(&mut line, "user", u);
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
        if ["create", "drop", "alter", "compact", "vacuum", "truncate"]
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
/// waits as a failed login does and is logged; one that changes a node's
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
        std::thread::sleep(failed(ip));
        return;
    }
    if req.header("authorization").is_some() {
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

/// How long to wait before answering a failed login from `peer`, the
/// failure counted.
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

/// A login from `peer` went through: its count starts again.
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
