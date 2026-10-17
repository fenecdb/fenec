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
//! {"at":"2026-10-01T09:30:12.041Z","event":"refused","level":"warn","proto":"http","peer":"10.0.0.7:53112","request_id":"9f2c41d07ab35e18","method":"POST","path":"/query"}
//! {"at":"2026-10-01T09:30:12.310Z","event":"schema","level":"info","proto":"http","peer":"10.0.0.7:53112","request_id":"5d0e7c2a91f4b366","statement":"create collection notes (title text)","failed":false}
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
//!
//! **Only a token verified starts the count again** ([`verified`], called
//! where a token is compared or a JWT's signature checked and found good).
//! It was inferred from the answer -- any status but 401 to a request
//! carrying `Authorization` -- and most answers verify nothing: an unknown
//! path's 404, the studio's files, `/_health`, a preflight, an open
//! `/_metrics`, a `POST /_whoami`'s 405, a body over the ceiling's 413, a
//! server that asks for no token at all. A client guessing tokens sent
//! `GET /nosuch` with any `Authorization` between its guesses and never
//! waited past the first 100 ms.
//!
//! **Whose address counts.** The connection's peer, unless something the
//! process trusts named the client: a router, to the node it forwards to
//! ([`ROUTER_HEADER`], [`request`]), whose refusals the router waits out;
//! or the global entry, to the router it sends to ([`CLIENT_HEADER`],
//! [`entry`]), whose refusals that router waits out by the address named.
//! Behind the entry every client reached a router from Cloudflare's
//! addresses: a refused token waited by Cloudflare's, and any good token
//! through the entry started that count again for everyone.

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
    // The client the global entry named for the request this thread
    // serves ([`entry`]): the connection's peer is the entry's, and this
    // stands in for it -- the refusal's wait and count, a verified token's
    // clearing, the audit and slow lines' `peer`, the trace's
    // `client.address`, the client a router names onwards. Unlike
    // [`ROUTED`], whose router waited a refusal out already, the router
    // the entry sent the request to waits it out here.
    static NAMED: std::cell::Cell<Option<IpAddr>> = const { std::cell::Cell::new(None) };
    // A token of the request this thread serves was verified, and the
    // router forwarded it: the answer's head says so ([`VERIFIED_HEADER`]),
    // since the router holds no keys and keeps the client's count.
    static VERIFIED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The header a node's answer to a forwarded request carries when a token
/// of it was verified ([`verified`]): the router, which holds no keys,
/// starts its client's count again on it alone. The router reads it only
/// off a node's answer, over its own connection to the node, and drops it
/// from what it sends on; a node writes it only on what the router
/// forwarded ([`ROUTER_HEADER`]'s mark), so a client never sees it.
pub const VERIFIED_HEADER: &str = "Fenec-Verified";

/// The header a router names a forwarded request's client in: its address
/// and the router's mark ([`router_mark`]), `<address> <mark>`. One header
/// and a mark of 16 bytes: the two headers it took at first, the mark's 32
/// bytes whole, added 2.5 us to the 22 a request through the router took
/// (`make shard-bench`, in turns), the node's header lines read one by one.
pub const ROUTER_HEADER: &str = "fenec-router";

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
    let mac = crate::crypto::hmac_sha256(
        admin_token.as_bytes(),
        b"fenec-router: a request forwarded, the client named beside it",
    );
    crate::crypto::b64url_encode(&mac[..16])
}

/// Whether `req` came through the router `mark` names, and from whom: a
/// node asks at each request. With no mark, or a header that is not it, the
/// request is the connection's own -- a client sending the headers itself
/// is believed in nothing.
pub fn request(req: &crate::http::Request, mark: Option<&str>) -> Option<IpAddr> {
    let client = mark.and_then(|mark| {
        let (client, given) = req.header(ROUTER_HEADER)?.trim().rsplit_once(' ')?;
        if !crate::constant_eq(given.as_bytes(), mark.as_bytes()) {
            return None;
        }
        client.parse::<IpAddr>().ok()
    });
    ROUTED.with(|r| r.set(client));
    // The request before on this connection named its client, and this
    // one names its own or none.
    NAMED.with(|n| n.set(None));
    // The request before on this connection -- the router's pool hands one
    // connection to many clients -- verified nothing of this one's.
    VERIFIED.with(|v| v.set(false));
    client
}

/// The header the global entry (`integrations/cloudflare/entry`) names its
/// client in: `<address> <seconds> <mark>`, the address Cloudflare's
/// `CF-Connecting-IP`, the seconds the Unix time the entry made it at, the
/// mark 16 bytes of HMAC-SHA256 of `fenec-client <address> <seconds>` under
/// the entry's token, base64url. A router with that token
/// (`fenec-shard --entry-token`) believes the address ([`entry`]); every
/// router drops the header from what it forwards, so a node never sees a
/// client's own claim.
pub const CLIENT_HEADER: &str = "fenec-client";

/// How far a [`CLIENT_HEADER`]'s time may lie from this machine's clock,
/// either way, in seconds. The mark is bound to the address and the time,
/// so a header seen on the wire -- in a log, past a proxy -- names that one
/// address, and for a minute at most: replayed, it moves that address's
/// refusal count, never another's, and a mark of the token alone would
/// have named any address for good. Both ways, since the entry's clock and
/// the router's are two machines'.
pub const CLIENT_WITHIN_SECS: u64 = 60;

/// The key a router checks the entry's marks with: `--entry-token`, its
/// padded blocks hashed once ([`crate::crypto::HmacKey`]).
#[derive(Clone)]
pub struct EntryKey(crate::crypto::HmacKey);

impl EntryKey {
    pub fn new(token: &str) -> EntryKey {
        EntryKey(crate::crypto::HmacKey::new(token.as_bytes()))
    }

    /// The mark of `address` at `seconds`, as the entry makes it.
    pub fn mark(&self, address: &str, seconds: &str) -> [u8; 22] {
        let mac = self.0.mac(&[
            b"fenec-client ",
            address.as_bytes(),
            b" ",
            seconds.as_bytes(),
        ]);
        b64url_16(&mac[..16])
    }
}

/// Why a [`CLIENT_HEADER`] named no one: the request is then the
/// connection's, as it would be without the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unnamed {
    /// Its mark is not the entry's for its address and time -- another
    /// token, a client sending the header itself -- or it is not of the
    /// form at all.
    BadMark,
    /// Its mark is the entry's, made further than [`CLIENT_WITHIN_SECS`]
    /// from this clock: replayed, or one of the two clocks is off.
    Stale,
}

/// What a [`CLIENT_HEADER`] value says, checked under `key` at `now` (Unix
/// seconds): the mark first, in constant time, then the time, so a stale
/// header is one the entry did make.
pub fn check_client(value: &str, key: &EntryKey, now: u64) -> Result<IpAddr, Unnamed> {
    let mut parts = value.trim().split(' ');
    let (Some(address), Some(seconds), Some(mark), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Unnamed::BadMark);
    };
    if !crate::constant_eq(mark.as_bytes(), &key.mark(address, seconds)) {
        return Err(Unnamed::BadMark);
    }
    // Made by the entry from an integer and an address; anything else is
    // another maker's, believed in nothing.
    let (Ok(at), Ok(ip)) = (seconds.parse::<u64>(), address.parse::<IpAddr>()) else {
        return Err(Unnamed::BadMark);
    };
    if at.abs_diff(now) > CLIENT_WITHIN_SECS {
        return Err(Unnamed::Stale);
    }
    Ok(ip)
}

/// The client the global entry named for `req`, checked under `key`
/// ([`check_client`]) and taken for this request's in place of the
/// connection's peer ([`client_ip`]); `None` without the header, which
/// costs a request one look among its headers. A router asks it after
/// [`request`], and only for a request no other router relayed: a relayed
/// one's client is the relaying router's to name.
pub fn entry(req: &crate::http::Request, key: &EntryKey) -> Option<Result<IpAddr, Unnamed>> {
    let value = req.header(CLIENT_HEADER)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let named = check_client(value, key, now);
    if let Ok(ip) = named {
        NAMED.with(|n| n.set(Some(ip)));
    }
    Some(named)
}

/// The client the global entry named for the request this thread serves,
/// where it named one ([`entry`]).
#[inline]
pub fn named() -> Option<IpAddr> {
    NAMED.with(|n| n.get())
}

/// 16 bytes in base64url, unpadded: 22 characters, on the stack.
fn b64url_16(bytes: &[u8]) -> [u8; 22] {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = [0u8; 22];
    let mut o = 0;
    for chunk in bytes[..16].chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | ((*b as u32) << (16 - 8 * i)));
        for k in 0..chunk.len() + 1 {
            out[o] = A[((n >> (18 - 6 * k)) & 63) as usize];
            o += 1;
        }
    }
    out
}

/// The connection this thread serves.
pub fn connection(proto: &'static str, peer: Option<std::net::SocketAddr>) {
    WHO.with(|w| *w.borrow_mut() = Who { proto, peer });
}

/// The address the request this thread serves comes from: the client a
/// router forwarded it for, the client the global entry named, or the
/// connection's peer.
pub fn client_ip() -> Option<IpAddr> {
    ROUTED
        .with(|r| r.get())
        .or_else(named)
        .or_else(|| WHO.with(|w| w.borrow().peer.map(|p| p.ip())))
}

/// A value of an event's line.
pub enum Field<'a> {
    Text(&'a str),
    Int(u64),
    Bool(bool),
    /// Written with three decimals: a duration in milliseconds.
    Num(f64),
}

/// How much an event matters, as the line's `level` says it: what a log
/// collector takes for its status (Datadog's `level`, Elastic's
/// `log.level`) without a rule of its own.
#[derive(Clone, Copy)]
pub enum Level {
    Info,
    Warn,
}

/// Writes an event, with when, over what, from where and as whom.
pub fn event(name: &str, fields: &[(&str, Field)]) {
    let Some(log) = LOG.get() else {
        return;
    };
    let level = match name {
        "refused" | "quota" => Level::Warn,
        _ => Level::Info,
    };
    let line = line(name, level, fields);
    let mut f = log.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = f.write_all(line.as_bytes()) {
        crate::log!("audit log: {e}");
    }
}

/// An event's JSON line, its newline included: when, what, how much it
/// matters, over what, from where, the request's id, then `fields`. The
/// slow-statement log writes its lines with it too, so that one parser
/// reads both.
pub fn line(name: &str, level: Level, fields: &[(&str, Field)]) -> String {
    let at = fenec_core::time::now_ms().map_or_else(|_| "?".into(), fenec_core::time::format_iso);
    let mut line = String::with_capacity(192);
    let text = |line: &mut String, k: &str, v: &str| {
        line.push_str(",\"");
        line.push_str(k);
        line.push_str("\":");
        fenec_core::json::escape_into(line, v);
    };
    line.push_str("{\"at\":");
    fenec_core::json::escape_into(&mut line, &at);
    text(&mut line, "event", name);
    let level = match level {
        Level::Info => "info",
        Level::Warn => "warn",
    };
    text(&mut line, "level", level);
    WHO.with(|w| {
        let w = w.borrow();
        if !w.proto.is_empty() {
            text(&mut line, "proto", w.proto);
        }
        match (ROUTED.with(|r| r.get()).or_else(named), w.peer) {
            // The client the router forwarded for, and the router; or the
            // client the global entry named, and the entry.
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
    crate::request_id::with(|id| {
        if !id.is_empty() {
            text(&mut line, "request_id", id);
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
            Field::Num(n) => {
                line.push_str(&format!(",\"{k}\":{n:.3}"));
            }
        }
    }
    line.push_str("}\n");
    line
}

/// Requests refused for their token (401), counted whether or not a log
/// is written: `fenec_refused_total`.
pub fn refused() -> u64 {
    REFUSED.load(Ordering::Relaxed)
}

static REFUSED: AtomicU64 = AtomicU64::new(0);

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
/// waits and is logged; one that changes a node's tenants or a router's
/// placement is logged. Nothing here starts a count again: the status says
/// nothing of whether a token was verified, and only [`verified`] does.
pub fn http(req: &crate::http::Request, status: u16, peer: Option<std::net::SocketAddr>) {
    use crate::http::Method;
    let ip = peer.map(|p| p.ip());
    let path = req.target.split('?').next().unwrap_or("");
    if status == 401 {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        event(
            "refused",
            &[
                ("method", Field::Text(req.method.name())),
                ("path", Field::Text(path)),
            ],
        );
        // Forwarded by the router, which waits it out by the client's
        // address: counted here, by the router's, it would hold every
        // client of the router to one count. Sent by the global entry, by
        // the client it named: by its own, every client behind the entry
        // would be Cloudflare's address.
        if ROUTED.with(|r| r.get()).is_none() {
            std::thread::sleep(failed(named().or(ip)));
        }
        return;
    }
    let admin = path.starts_with("/_admin/") || path.starts_with("/_shard/");
    // A tenant's read for a router's query across tenants changes nothing,
    // though it is a `POST`: one fan-out over a thousand tenants would be
    // a thousand lines on the nodes. The router's own `POST /_shard/query`
    // is the line, its request id the one every node's read carries.
    let read = path.starts_with("/_admin/tenants/") && path.ends_with("/query");
    if admin && !read && on() && !matches!(req.method, Method::Get | Method::Head | Method::Options)
    {
        event(
            "admin",
            &[
                ("method", Field::Text(req.method.name())),
                ("path", Field::Text(path)),
                // Not `status`, which a log collector takes for the line's
                // level: Datadog's reads `status` first, and a 200 is none.
                ("http_status", Field::Int(status as u64)),
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
    wait(base, e.0)
}

/// What a refusal to `peer` would wait now, nothing counted. A test holds
/// the wait to this, and a request's time only from below: the sleep is
/// the system's to stretch. macOS gives the timers of a process whose QoS
/// it clamps a leeway of hundreds of milliseconds -- under `taskpolicy -c
/// background` a 50 ms wait slept 229 to 257 -- and a CI runner's refusal
/// of a 50 ms wait took 256.6 ms, which a bound of 200 read as the count
/// of another address.
pub fn would_wait(peer: Option<IpAddr>) -> Duration {
    let base = DELAY_MS.load(Ordering::Relaxed);
    let Some(ip) = peer.filter(|_| base > 0) else {
        return Duration::ZERO;
    };
    let map = failures().lock().unwrap_or_else(|e| e.into_inner());
    let before = match map.get(&ip) {
        Some(&(n, at)) if at.elapsed() <= WINDOW => n,
        _ => 0,
    };
    wait(base, before.saturating_add(1))
}

/// The wait after the `n`th failure in a row, `base` after the first.
fn wait(base: u64, n: u32) -> Duration {
    let doubled = base.saturating_mul(1u64 << (n - 1).min(16));
    Duration::from_millis(doubled).min(MOST)
}

/// A token the request this thread serves carries was verified good: the
/// server's token or a JWT (`authenticate`), the admin token, a
/// replication token, a scrape's token, a router's own `--token` -- each
/// where it is compared, never inferred from an answer. Its address's count
/// starts again ([`succeeded`]) -- the client's the global entry named,
/// where it named one ([`entry`]), never the entry's own; on a node the
/// router forwarded to, which counts nothing of the client's, the answer
/// tells the router instead ([`VERIFIED_HEADER`]), and the router calls
/// this for its client.
///
/// An address with no count takes no lock: one relaxed load, and a
/// thread-local's where the request came through the router.
pub fn verified() {
    if ROUTED.with(|r| r.get()).is_some() {
        VERIFIED.with(|v| v.set(true));
        return;
    }
    if FAILING.load(Ordering::Relaxed) > 0 {
        succeeded(named().or_else(|| WHO.with(|w| w.borrow().peer).map(|p| p.ip())));
    }
}

/// The [`VERIFIED_HEADER`] line, onto an answer's `head`, where a token of
/// the forwarded request it answers was verified; taken, so the next
/// request on the connection starts without it.
pub fn push_verified(head: &mut String) {
    if VERIFIED.with(|v| v.replace(false)) {
        head.push_str(VERIFIED_HEADER);
        head.push_str(": 1\r\n");
    }
}

/// A token from `peer` was verified: its count starts again.
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
        assert_eq!(would_wait(ip).as_millis(), 100);
        let waits: Vec<u128> = (0..8).map(|_| failed(ip).as_millis()).collect();
        assert_eq!(waits, [100, 200, 400, 800, 1600, 3200, 5000, 5000]);
        assert_eq!(would_wait(ip).as_millis(), 5000);
        succeeded(ip);
        // Asked, the next wait is told and nothing is counted.
        assert_eq!(would_wait(ip).as_millis(), 100);
        assert_eq!(would_wait(ip).as_millis(), 100);
        assert_eq!(failed(ip).as_millis(), 100);
        assert_eq!(would_wait(ip).as_millis(), 200);
        // Another address has a count of its own; no address waits none.
        assert_eq!(
            failed(Some("203.0.113.10".parse().unwrap())).as_millis(),
            100
        );
        assert_eq!(failed(None), Duration::ZERO);
    }

    /// The marks the entry makes (`clientMark` in entry.js, here worked out
    /// by Python's `hmac` under the entry tests' secret) are the ones a
    /// router checks, an IPv6 address among them; any other mark, form or
    /// time names no one.
    #[test]
    fn a_client_is_named_by_the_entrys_mark_alone_and_for_a_minute() {
        let key = EntryKey::new("entry-secret");
        let at = 1_800_000_000;
        let v4 = "203.0.113.7 1800000000 U6Z3YHsO6YiFX39RTKe6Fg";
        let v6 = "2001:db8::7 1800000000 VXaSKsYF9K-m2JHHhnRfxg";
        assert_eq!(
            check_client(v4, &key, at),
            Ok("203.0.113.7".parse().unwrap())
        );
        assert_eq!(
            check_client(v6, &key, at),
            Ok("2001:db8::7".parse().unwrap())
        );
        // Within a minute either way of this clock, and no further.
        for now in [at - 60, at + 60] {
            assert!(check_client(v4, &key, now).is_ok(), "{now}");
        }
        for now in [at - 61, at + 61, 0] {
            assert_eq!(check_client(v4, &key, now), Err(Unnamed::Stale), "{now}");
        }
        // The mark is the address's and the time's: either moved, or another
        // token, and it names no one.
        let bad = [
            "203.0.113.8 1800000000 U6Z3YHsO6YiFX39RTKe6Fg",
            "203.0.113.7 1800000001 U6Z3YHsO6YiFX39RTKe6Fg",
            "203.0.113.7 1800000000 U6Z3YHsO6YiFX39RTKe6Fh",
            "203.0.113.7 1800000000 U6Z3YHsO6YiFX39RTKe6F",
            "203.0.113.7 1800000000",
            "203.0.113.7 1800000000 U6Z3YHsO6YiFX39RTKe6Fg extra",
            "203.0.113.7",
            "",
        ];
        for value in bad {
            assert_eq!(
                check_client(value, &key, at),
                Err(Unnamed::BadMark),
                "{value}"
            );
        }
        assert_eq!(
            check_client(v4, &EntryKey::new("another"), at),
            Err(Unnamed::BadMark)
        );
        // Marked, but no address and no time: another maker's.
        let odd = |address: &str, seconds: &str| {
            let mark = key.mark(address, seconds);
            let mark = std::str::from_utf8(&mark).unwrap().to_string();
            check_client(&format!("{address} {seconds} {mark}"), &key, at)
        };
        assert_eq!(odd("example.com", "1800000000"), Err(Unnamed::BadMark));
        assert_eq!(odd("203.0.113.7", "soon"), Err(Unnamed::BadMark));
        assert_eq!(
            odd("203.0.113.7", "1800000000"),
            Ok("203.0.113.7".parse().unwrap())
        );
    }

    /// The client the entry named stands in for the connection's peer for
    /// that request alone: in an event's line and the slow statement's,
    /// which `line` writes alike, the entry beside it as `via`; the next
    /// request on the connection, naming none, is the connection's again.
    #[test]
    fn the_named_client_is_a_lines_peer_for_its_request_alone() {
        let key = EntryKey::new("entry-secret");
        let cloudflare: std::net::SocketAddr = "172.64.0.9:4410".parse().unwrap();
        connection("http", Some(cloudflare));
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let mark = key.mark("203.0.113.7", &at);
        let value = format!("203.0.113.7 {at} {}", std::str::from_utf8(&mark).unwrap());
        let req = |headers: Vec<(String, String)>| crate::http::Request {
            method: crate::http::Method::Get,
            target: "/t/acme/notes".into(),
            path: "/t/acme/notes".into(),
            query: Vec::new(),
            headers,
            body: Vec::new(),
            keep_alive: true,
        };
        let named_req = req(vec![(CLIENT_HEADER.into(), value)]);
        assert_eq!(request(&named_req, None), None);
        assert_eq!(
            entry(&named_req, &key),
            Some(Ok("203.0.113.7".parse().unwrap()))
        );
        assert_eq!(client_ip(), Some("203.0.113.7".parse().unwrap()));
        let slow = line("slow", Level::Warn, &[("duration_ms", Field::Num(12.5))]);
        assert!(
            slow.contains(r#""peer":"203.0.113.7","via":"172.64.0.9:4410""#),
            "{slow}"
        );

        let next = req(Vec::new());
        assert_eq!(request(&next, None), None);
        assert_eq!(entry(&next, &key), None);
        assert_eq!(client_ip(), Some(cloudflare.ip()));
        let slow = line("slow", Level::Warn, &[]);
        assert!(slow.contains(r#""peer":"172.64.0.9:4410""#), "{slow}");
        assert!(!slow.contains("via"), "{slow}");
    }

    #[test]
    fn the_mark_is_hmac_sha256s_first_sixteen_bytes_in_base64url() {
        let key = EntryKey::new("k");
        let mac = crate::crypto::hmac_sha256(b"k", b"fenec-client 198.51.100.1 42");
        let text = crate::crypto::b64url_encode(&mac[..16]);
        assert_eq!(
            std::str::from_utf8(&key.mark("198.51.100.1", "42")).unwrap(),
            text
        );
        // Every byte value, through the encoder on the stack.
        for b in 0..=255u8 {
            let bytes = [b; 16];
            assert_eq!(
                std::str::from_utf8(&b64url_16(&bytes)).unwrap(),
                crate::crypto::b64url_encode(&bytes)
            );
        }
    }
}
