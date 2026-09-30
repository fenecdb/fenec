//! `Idempotency-Key`: a write sent again is answered as it was the first
//! time, and not made again. A `put` with no id makes a row each time it
//! runs, so a client that retries a request whose answer it never got --
//! a timeout, a dropped connection -- wrote the row twice.
//!
//! The key and the answer are kept in the same block as the write
//! (`_idempotency`), under the write lock: the write and its key land
//! together or not at all, so a crash between the two cannot leave a
//! write whose retry makes it again, and a second request with the key
//! waits for the lock and finds it. A key is the subject's, for a scoped
//! token, so no user is handed another's answer; the request it came with
//! is kept as a hash, and the key sent with another request is refused
//! (422), as the IETF draft has it. Kept for `idempotency_ttl`, then let
//! go of a batch at a time; the change stream leaves these writes out.

use crate::access::Who;
use crate::http::{Request, Response};
use fenec_core::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The collection the keys are kept in.
pub const KEYS: &str = "_idempotency";

/// An answer longer than this is kept as its status and a note saying so:
/// a write's answer is a count, and a batch's reads can be long.
const KEPT_BODY: usize = 64 << 10;

/// A key, the subject's for a scoped token, and a hash of the request.
pub struct Key {
    name: String,
    request: i64,
}

/// The request's key, if it has one: `None` without, a refusal for one
/// that cannot be kept.
pub fn key(req: &Request, who: &Who) -> std::result::Result<Option<Key>, Response> {
    let Some(given) = req.header("idempotency-key") else {
        return Ok(None);
    };
    if given.is_empty() || given.len() > 255 {
        return Err(Response::error(400, "an Idempotency-Key is 1 to 255 bytes"));
    }
    // Keys are the subject's: two users' keys never meet. A scoped token
    // with no subject has no one to keep them for.
    let owner = match who {
        Who::Full => "",
        Who::Scoped(scope) => match scope.subject() {
            Some(sub) => sub,
            None => {
                return Err(Response::error(
                    400,
                    "an Idempotency-Key needs a token with a subject (`sub`)",
                ))
            }
        },
    };
    let mut name = String::with_capacity(owner.len() + 1 + given.len());
    name.push_str(owner);
    name.push('\u{1f}');
    name.push_str(given);
    Ok(Some(Key {
        name,
        request: fnv(&[
            req.method.name().as_bytes(),
            req.target.as_bytes(),
            &req.body,
        ]) as i64,
    }))
}

fn fnv(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for p in parts {
        for &b in *p {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
        h = (h ^ 0xff).wrapping_mul(0x100000001b3);
    }
    h
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The statements the keys take, parsed once: parsed for each keyed
/// write, with the collection made if missing every time, they were most
/// of what a key cost.
fn stmt(which: usize) -> &'static Statement {
    const SQL: [&str; 5] = [
        "create collection if not exists _idempotency \
         (key text @hash, request int, status int, body text, at int @sorted)",
        "get _idempotency select request, status, body, at where key = $1",
        "del _idempotency where key = $1",
        "put _idempotency {key: $1, request: $2, status: $3, body: $4, at: $5}",
        "del _idempotency where at < $1",
    ];
    static PARSED: std::sync::OnceLock<Vec<Statement>> = std::sync::OnceLock::new();
    &PARSED.get_or_init(|| {
        SQL.iter()
            .map(|s| fenec_ql::parse_one(s).expect("the key statements parse"))
            .collect()
    })[which]
}

const CREATE: usize = 0;
const GET: usize = 1;
const DEL: usize = 2;
const PUT: usize = 3;
const PURGE: usize = 4;

/// The answer kept for `key`, as it was sent: under the write lock, so a
/// request with the key that is still running has landed by now.
pub fn answered(db: &Database, key: &Key, ttl_ms: i64) -> Option<Response> {
    db.collection(KEYS).ok()?;
    let fenec_core::query::Response::Rows(rs) =
        db.query(stmt(GET), &[Value::Text(key.name.clone())]).ok()?
    else {
        return None;
    };
    let row = rs.rows.first()?;
    let [Value::Int(request), Value::Int(status), Value::Text(body), Value::Int(at)] =
        row.values.as_slice()
    else {
        return None;
    };
    // Past its time: as if never sent, and let go of by the next purge.
    if now_ms() - at > ttl_ms {
        return None;
    }
    if *request != key.request {
        return Some(Response::error(
            422,
            "this Idempotency-Key came with another request: a key is for one",
        ));
    }
    Some(Response::json(*status as u16, body.clone()).header("Idempotent-Replayed", "true"))
}

/// Keeps `resp` as `key`'s answer, in the block the write is in.
pub fn keep(db: &mut Database, key: &Key, resp: &Response) -> fenec_core::error::Result<()> {
    let fresh = db.collection(KEYS).is_err();
    if fresh {
        db.execute_with(stmt(CREATE), &[])?;
    }
    let body = match resp.body.len() <= KEPT_BODY {
        true => String::from_utf8_lossy(&resp.body).into_owned(),
        false => format!(
            "{{\"idempotent\":\"the first answer was {} bytes, more than is kept\"}}",
            resp.body.len()
        ),
    };
    let row = [
        Value::Text(key.name.clone()),
        Value::Int(key.request),
        Value::Int(resp.status as i64),
        Value::Text(body),
        Value::Int(now_ms()),
    ];
    // Once past its time a key may be sent again: the row is written over.
    if !fresh {
        db.execute_with(stmt(DEL), &row[..1])?;
    }
    db.execute_with(stmt(PUT), &row)?;
    Ok(())
}

/// When the keys past their time were last let go of.
static PURGED: AtomicU64 = AtomicU64::new(0);

/// Lets go of the keys past their time, at most once a minute: a range of
/// the ordered index, not a scan.
pub fn purge(db: &mut Database, ttl_ms: i64) {
    let now = now_ms();
    let last = PURGED.load(Ordering::Relaxed) as i64;
    if now - last < 60_000 || db.collection(KEYS).is_err() {
        return;
    }
    PURGED.store(now as u64, Ordering::Relaxed);
    let _ = db.execute_with(stmt(PURGE), &[Value::Int(now - ttl_ms)]);
}
