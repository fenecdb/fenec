//! `GET /_changes`: the writes on this database's disk, one JSON object a
//! line, from any write the feed still keeps -- change data capture, for a
//! consumer that must see every write once it is durable, and pick up where
//! it stopped. A subscription (`/<name>/changes`) keeps a query's rows up to
//! date and is reseeded past its ring of ids, which holds no documents; this
//! reads the records the primary's feed holds for its replicas, documents
//! and all (`Feed::changes_after`, `Database::changes_in`).
//!
//! `since` is the last write a consumer has, `Fenec-Next` the last one an
//! answer holds: sent as the next `since`, nothing is missed or had twice.
//! `Fenec-Seq` is the last write the database holds: a reader whose
//! `Fenec-Next` has reached it has every write there is.
//! A block's writes come together, numbered one by one, so a cursor inside
//! one resumes after the write it names. With `wait` an answer with nothing
//! in it waits for a write that long first. A cursor the feed no longer
//! reaches is answered 410 with the first `since` it does, never with a
//! stretch of writes missing from the middle.
//!
//! A consumer that keeps no state of its own has the server keep where it
//! is (`/_changes/consumers/<name>`), as a Kafka consumer group's committed
//! offset: a `POST` makes it -- at the last write on disk unless it names a
//! `since` -- and leaves one that is there where it stands, as Kafka reads
//! `auto.offset.reset` only for a group with no offset: a relay that makes
//! its consumer at every start goes on from where it stopped. A `since`
//! ahead of where it stands is a commit sent as a `POST`, refused 409
//! ([`ahead`]). `?consumer=<name>` reads from where it is, and a
//! `PATCH` of `since` moves it once the consumer has done with what it
//! read -- so it has each write at least once, and again after a crash
//! before the `PATCH` -- or back, to read from there again. The `POST`
//! moved one that was there to the last write on disk, and a relay making
//! its consumer at each start skipped every write made while it was
//! stopped. The cursors are rows of [`CONSUMERS`], written as any write
//! is: on disk before the answer where writes are, and on the replicas,
//! which change none of them ([`changed`]). Their own writes are not in the
//! stream, or every acknowledgement would be a write to read -- nor do
//! they end a `wait`, and a commit past nothing but cursors writes nothing
//! ([`unmoved`]), or a consumer committing every answer would read its own
//! commit's empty answer and commit again.

use crate::http::{Method, Request, Response};
use crate::replication::{Feed, Tail};
use crate::Config;
use fenec_core::engine::{Change, ChangeKind};
use fenec_core::json;
use fenec_core::prelude::*;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Writes an answer holds unless `limit` says fewer.
const LIMIT: usize = 1_000;
/// The most an answer holds, and the longest `wait`.
const MOST: usize = 10_000;
const LONGEST: Duration = Duration::from_secs(30);
/// Bytes of records read from the feed at a time.
const READ: usize = 4 << 20;

/// The collection the consumers' cursors are kept in.
pub const CONSUMERS: &str = "_consumers";

/// `/_changes` and `/_changes/consumers/...`.
pub fn route(db: &Arc<RwLock<Database>>, feed: &Feed, cfg: &Config, req: &Request) -> Response {
    match (req.method, req.segments().as_slice()) {
        (Method::Get, ["_changes"]) => {
            let consumer = req.query.iter().find(|(k, _)| k == "consumer");
            match consumer {
                None => handle(db, feed, req, None),
                Some((_, name)) => match cursor(db, name) {
                    Ok(Some(since)) => handle(db, feed, req, Some(since)),
                    // Read from "now" each time, it would miss what is
                    // written between two reads: a consumer is made first.
                    Ok(None) => Response::error(
                        404,
                        &format!("no consumer `{name}`: POST /_changes/consumers/{name} first"),
                    ),
                    Err(e) => crate::error_response(&e),
                },
            }
        }
        (Method::Get, ["_changes", "consumers"]) => list(db, feed),
        (Method::Post, ["_changes", "consumers", name]) => make(db, feed, cfg, name, req),
        (Method::Patch, ["_changes", "consumers", name]) => commit(db, cfg, name, req),
        (Method::Delete, ["_changes", "consumers", name]) => forget(db, cfg, name),
        _ => Response::error(404, &format!("path `{}`", req.path)),
    }
}

fn run(db: &Database, sql: &str, params: &[Value]) -> Result<Response2> {
    db.query(&fenec_ql::parse_one(sql)?, params)
}

type Response2 = fenec_core::query::Response;

/// Where `name` stands, `None` for a consumer not seen yet.
fn cursor(db: &Arc<RwLock<Database>>, name: &str) -> Result<Option<u64>> {
    cursor_in(&crate::held::read(db), name)
}

fn cursor_in(g: &Database, name: &str) -> Result<Option<u64>> {
    if g.collection(CONSUMERS).is_err() {
        return Ok(None);
    }
    let r = run(
        g,
        "get _consumers select since where name = $1",
        &[Value::Text(name.into())],
    )?;
    Ok(match r {
        Response2::Rows(rs) => rs.rows.first().and_then(|row| match row.values.first() {
            Some(&Value::Int(n)) => Some(n as u64),
            _ => None,
        }),
        _ => None,
    })
}

fn list(db: &Arc<RwLock<Database>>, feed: &Feed) -> Response {
    let durable = feed.durable();
    let g = crate::held::read(db);
    let mut out = String::from("[");
    if g.collection(CONSUMERS).is_ok() {
        let rows = match run(
            &g,
            "get _consumers select name, since order name limit 10000",
            &[],
        ) {
            Ok(Response2::Rows(rs)) => rs.rows,
            Ok(_) => Vec::new(),
            Err(e) => return crate::error_response(&e),
        };
        for (i, row) in rows.iter().enumerate() {
            let (Some(Value::Text(name)), Some(&Value::Int(since))) =
                (row.values.first(), row.values.get(1))
            else {
                continue;
            };
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            json::escape_into(&mut out, name);
            out.push_str(&format!(
                ",\"since\":{since},\"behind\":{}}}",
                durable.saturating_sub(since as u64)
            ));
        }
    }
    out.push(']');
    Response::json(200, out)
}

/// The `since` a consumer's body names: `None` for no body, `{}` or a
/// `since` of `null`.
fn since_in(req: &Request) -> std::result::Result<Option<i64>, Response> {
    let body = std::str::from_utf8(&req.body).unwrap_or("").trim();
    if body.is_empty() {
        return Ok(None);
    }
    let named = match json::parse_object(body) {
        Ok(o) => o.into_iter().find(|(k, _)| k == "since").map(|(_, v)| v),
        Err(e) => return Err(crate::error_response(&e)),
    };
    match named {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Int(n)) if n >= 0 => Ok(Some(n)),
        Some(_) => Err(Response::error(
            400,
            "the body is {\"since\": <the last write done with>}",
        )),
    }
}

/// `name` refused as a consumer's.
fn misnamed(name: &str) -> Option<Response> {
    (name.is_empty() || name.len() > 200)
        .then(|| Response::error(400, "a consumer's name is 1 to 200 bytes"))
}

/// A `since` past the last write the database holds, on disk or not:
/// another database's cursor, or this one's before a restore, 409 as a
/// read's cursor past it is (`Tail::Ahead`). One past the disk alone is
/// taken: a 410 names the first `since` the feed still keeps, and the
/// feed keeps writes by their size, on disk or not, so that `since` may be
/// ahead of the disk -- refused, a consumer told to go on from there could
/// not. Taken, its row lands after it, a read from it waits for the disk,
/// and a crash before the disk has it hands those writes over again,
/// never skips them.
fn past(g: &Database, since: i64) -> Option<Response> {
    let held = g.change_seq();
    (since as u64 > held).then(|| {
        Response::json(
            409,
            format!("{{\"error\":\"{since} is past the last write, {held}\",\"seq\":{held}}}"),
        )
    })
}

/// A `POST` naming a `since` ahead of where `name` stands: a commit sent
/// as one, as every commit was before the `PATCH`. Answered 200 and left
/// where it was, its client read the same page for ever and was told
/// nothing.
fn ahead(name: &str, at: u64) -> Response {
    let mut out = String::from("{\"error\":");
    json::escape_into(
        &mut out,
        &format!(
            "consumer {name} is there, at {at}: a POST makes a consumer and moves none; \
             PATCH /_changes/consumers/{name} moves it"
        ),
    );
    out.push_str(&format!(",\"since\":{at}}}"));
    Response::json(409, out)
}

/// Makes `name` at the `since` the body names, or at the last write on
/// disk: 201. One that is there stays where it stands and is answered so,
/// 200, for no `since` or one at or behind it -- where a new one starts.
/// Made at every start, as a relay or a follower makes its consumer, it
/// goes on from where it stopped; moved to the last write on disk, it
/// skipped every write made while it was stopped. Nor is one that is there
/// a 409, as Redis's `XGROUP CREATE` answers `BUSYGROUP`: every caller
/// would take that error for a success, and a consumer made from the first
/// write (`{"since": 0}`) would fail every start but its first. A `since`
/// ahead of it is the one 409 ([`ahead`]).
fn make(
    db: &Arc<RwLock<Database>>,
    feed: &Feed,
    cfg: &Config,
    name: &str,
    req: &Request,
) -> Response {
    let start = match since_in(req) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if let Some(r) = misnamed(name) {
        return r;
    }
    let durable = feed.durable();
    let done = changed(db, cfg, |g| {
        if let Some(at) = cursor_in(g, name)? {
            return Ok(match start {
                Some(n) if n as u64 > at => Err(ahead(name, at)),
                _ => Ok((at as i64, false)),
            });
        }
        if let Some(r) = start.and_then(|n| past(g, n)) {
            return Ok(Err(r));
        }
        // None given: from the last write on disk, what is written from
        // now on.
        let at = start.unwrap_or(durable as i64);
        exec(
            g,
            "create collection if not exists _consumers (name text @hash, since int)",
            &[],
        )?;
        exec(
            g,
            "put _consumers {name: $1, since: $2}",
            &[Value::Text(name.into()), Value::Int(at)],
        )?;
        Ok(Ok((at, true)))
    });
    match done {
        Ok(((at, made), seq)) => at_seq(standing(if made { 201 } else { 200 }, name, at), seq),
        Err(r) => r,
    }
}

/// Moves `name` to the `since` the body names: once the consumer has done
/// with every write up to it, or back, to read from there again. Only a
/// consumer that is there moves, 404 otherwise: a `POST` is the one way to
/// make one, so a commit racing a `DELETE` makes none again. Answered with
/// where it stands after: a commit past nothing but consumers' writes
/// writes nothing ([`unmoved`]) and leaves it where it was, which reads the
/// same writes -- answered as sent, the answer named a place no `GET` of
/// the consumers and no later `POST` would find.
fn commit(db: &Arc<RwLock<Database>>, cfg: &Config, name: &str, req: &Request) -> Response {
    let since = match since_in(req) {
        Ok(Some(n)) => n,
        Ok(None) => {
            return Response::error(400, "the body is {\"since\": <the last write done with>}")
        }
        Err(r) => return r,
    };
    if let Some(r) = misnamed(name) {
        return r;
    }
    let done = changed(db, cfg, |g| {
        let Some(at) = cursor_in(g, name)? else {
            return Ok(Err(Response::error(
                404,
                &format!("no consumer `{name}`: POST /_changes/consumers/{name} makes it"),
            )));
        };
        if let Some(r) = past(g, since) {
            return Ok(Err(r));
        }
        if unmoved(g, at, since) {
            return Ok(Ok(at as i64));
        }
        exec(
            g,
            "set _consumers {since: $2} where name = $1",
            &[Value::Text(name.into()), Value::Int(since)],
        )?;
        Ok(Ok(since))
    });
    match done {
        Ok((at, seq)) => at_seq(standing(200, name, at), seq),
        Err(r) => r,
    }
}

/// Lets `name` go: unseen again until a `POST` makes it.
fn forget(db: &Arc<RwLock<Database>>, cfg: &Config, name: &str) -> Response {
    let done = changed(db, cfg, |g| {
        if g.collection(CONSUMERS).is_ok() {
            exec(
                g,
                "del _consumers where name = $1",
                &[Value::Text(name.into())],
            )?;
        }
        Ok(Ok(()))
    });
    match done {
        Ok(((), seq)) => at_seq(Response::empty(204), seq),
        Err(r) => r,
    }
}

fn exec(g: &mut Database, sql: &str, params: &[Value]) -> Result<Response2> {
    g.execute_with(&fenec_ql::parse_one(sql)?, params)
}

/// Where `name` stands, as a `POST` or a `PATCH` answers it.
fn standing(status: u16, name: &str, since: i64) -> Response {
    let mut out = String::from("{\"name\":");
    json::escape_into(&mut out, name);
    out.push_str(&format!(",\"since\":{since}}}"));
    Response::json(status, out)
}

/// `r` naming the change the database stood at once the consumer's write
/// ran (`Fenec-Seq`), as a write's answer does: a read sent after it
/// (`Fenec-After`) -- to a replica, where a router sends one near -- finds
/// the consumer where this answer left it.
fn at_seq(mut r: Response, seq: u64) -> Response {
    r.seq = Some(seq);
    r
}

/// Runs `f` over the consumers under the write lock, then waits for the
/// file to be durable before the answer: what `f` wrote, and where it
/// wrote nothing, what it read -- a consumer another request made a
/// moment before, its fsync not yet run, is not answered as there until
/// it is. `f`'s refusal is answered as it is, nothing written. Gives `f`'s
/// answer and the change the database stood at after it. A replica's
/// consumers are its primary's, so it refuses each of these whatever `f`
/// would change: a `POST` of one that is there writes nothing, and had
/// passed, leaving a relay pointed at a replica to fail at its first
/// commit rather than as it starts.
fn changed<T>(
    db: &Arc<RwLock<Database>>,
    cfg: &Config,
    f: impl FnOnce(&mut Database) -> Result<std::result::Result<T, Response>>,
) -> std::result::Result<(T, u64), Response> {
    if cfg.read_only {
        return Err(crate::read_only_refused(cfg));
    }
    let done = (|| -> Result<_> {
        let mut g = crate::held::write(db);
        if g.history().following {
            return Err(Error::ReadOnly(
                "this database is a replica: its consumers are its primary's".into(),
            ));
        }
        let out = match f(&mut g)? {
            Ok(out) => out,
            Err(refused) => return Ok(Err(refused)),
        };
        let seq = g.change_seq();
        Ok(Ok((out, seq, crate::flush_for(cfg, &mut g)?)))
    })();
    let (out, seq, durability) = match done {
        Ok(Ok(done)) => done,
        Ok(Err(refused)) => return Err(refused),
        Err(e) => return Err(crate::error_response(&e)),
    };
    crate::await_durable(db, durability).map_err(|e| crate::error_response(&e))?;
    Ok((out, seq))
}

/// Whether moving a consumer standing at `at` to `since` changes nothing
/// it would read: every write after `at` is a cursor's (`_consumers`),
/// which the stream leaves out, and `since` is past `at`. Written, a sink
/// that commits each answer -- an empty one past its own last commit too
/// -- made a write for every one it read, for ever.
fn unmoved(db: &Database, at: u64, since: i64) -> bool {
    if at > since as u64 {
        return false;
    }
    // `None`: past the ring, or a collection dropped since -- moved.
    db.changed_collections_since(at)
        .is_some_and(|names| names.iter().all(|n| n == CONSUMERS))
}

/// How long a page of `req` may wait for a write (`wait`, [`LONGEST`] at
/// most), as [`handle`] reads it: what holds a thread here as a stream
/// does, and what a router asking a read replica for the page allows it
/// besides its own bound.
pub fn page_wait(req: &Request) -> Duration {
    req.query
        .iter()
        .find(|(k, _)| k == "wait")
        .and_then(|(_, v)| v.parse::<u64>().ok())
        .map_or(Duration::ZERO, |ms| Duration::from_millis(ms).min(LONGEST))
}

pub fn handle(
    db: &Arc<RwLock<Database>>,
    feed: &Feed,
    req: &Request,
    from: Option<u64>,
) -> Response {
    let param = |k: &str| {
        req.query
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    };
    let mut numbers = [None; 3];
    for (n, k) in numbers.iter_mut().zip(["since", "limit", "wait"]) {
        if let Some(v) = param(k) {
            match v.parse::<u64>() {
                Ok(v) => *n = Some(v),
                Err(_) => return Response::error(400, &format!("`{k}` expects a whole number")),
            }
        }
    }
    let [since, limit, wait] = numbers;
    let limit = limit.map_or(LIMIT, |n| n as usize).clamp(1, MOST);
    let wait = Duration::from_millis(wait.unwrap_or(0)).min(LONGEST);
    // No cursor: the consumer's, or the last write on disk -- what is
    // written from now on.
    let mut since = since.or(from).unwrap_or_else(|| feed.durable());
    let until = Instant::now() + wait;
    loop {
        let epoch = feed.epoch();
        match feed.changes_after(since, READ) {
            Tail::Records {
                first,
                lasts,
                times,
                bytes,
            } => {
                let g = crate::held::read(db);
                let (body, next) = match lines(&g, &bytes, first, &lasts, &times, since, limit) {
                    Ok(out) => out,
                    Err(e) => return Response::error(500, &e.to_string()),
                };
                drop(g);
                // Only writes the stream leaves out -- a consumer's own
                // commit among them -- and time left to wait: wait on past
                // them. Answered at once, a sink that commits every answer
                // committed in a loop, 30 000 fsynced writes in a few idle
                // minutes, each waking its own next read.
                if body.is_empty() && next > since && Instant::now() < until {
                    since = next;
                    continue;
                }
                return answer(body, next, feed);
            }
            Tail::Nothing => {
                let now = Instant::now();
                if now >= until {
                    return answer(String::new(), since, feed);
                }
                feed.wait(since, epoch, until - now);
            }
            Tail::Behind(oldest) => {
                let from = oldest - 1;
                return Response::json(
                    410,
                    format!(
                        "{{\"error\":\"the writes after {since} are no longer kept: \
                         the first since that is is {from}\",\"since\":{from}}}"
                    ),
                );
            }
            Tail::Ahead(seq) => {
                return Response::json(
                    409,
                    format!(
                        "{{\"error\":\"{since} is past the last write, {seq}: another \
                         database's cursor, or this one restored\",\"seq\":{seq}}}"
                    ),
                )
            }
            Tail::Gone => return Response::retry_after(503, "the database is going", 1),
        }
    }
}

/// An answer: its writes, `Fenec-Next` the last of them, and `Fenec-Seq` the
/// last write the database holds -- on disk or not yet, and of the
/// collections the stream leaves out too. An answer whose `Fenec-Next` is
/// that holds every write there is, so a reader knows at once that it has
/// caught up: from `Fenec-Next` alone an empty page could not tell a
/// consumer that had from one whose writes were not on disk yet, and
/// Kestrel's worker waited out 600 ms of empty pages before it believed it.
fn answer(body: String, next: u64, feed: &Feed) -> Response {
    let mut r = Response::json(200, body).header("Fenec-Next", &next.to_string());
    r.content_type = "application/x-ndjson";
    // Read after the writes: a write landing in between makes it larger,
    // never smaller than the page.
    r.seq = Some(feed.seq().max(next));
    r
}

/// The writes after `since` in `bytes`, at most `limit` of them, a line
/// each, and the number of the last one.
fn lines(
    db: &Database,
    bytes: &[u8],
    first: u64,
    lasts: &[u64],
    times: &[u64],
    since: u64,
    limit: usize,
) -> Result<(String, u64)> {
    let mut out = String::new();
    let (mut n, mut next) = (0usize, since);
    db.changes_in(bytes, first, &mut |c: Change| {
        if c.seq <= since {
            return true;
        }
        if n == limit {
            return false;
        }
        // The consumers' own cursors and the writes' keys: passed over,
        // and the cursor with them.
        if matches!(
            c.collection.as_deref(),
            Some(CONSUMERS | crate::idempotent::KEYS)
        ) {
            next = c.seq;
            return true;
        }
        let at = times[lasts.partition_point(|&l| l < c.seq).min(times.len() - 1)];
        line(&mut out, &c, at);
        n += 1;
        next = c.seq;
        true
    })?;
    Ok((out, next))
}

fn line(out: &mut String, c: &Change, at: u64) {
    out.push_str("{\"seq\":");
    out.push_str(&c.seq.to_string());
    out.push_str(",\"at\":");
    out.push_str(&at.to_string());
    out.push_str(",\"collection\":");
    match &c.collection {
        Some(name) => json::escape_into(out, name),
        None => out.push_str("null"),
    }
    let (op, id) = match &c.kind {
        ChangeKind::Put(id, _) => ("put", Some(*id)),
        ChangeKind::Del(id) => ("del", Some(*id)),
        ChangeKind::Create(_) => ("create", None),
        ChangeKind::Alter(_) => ("alter", None),
        ChangeKind::Drop => ("drop", None),
    };
    out.push_str(",\"op\":\"");
    out.push_str(op);
    out.push('"');
    if let Some(id) = id {
        out.push_str(",\"id\":");
        out.push_str(&id.to_string());
    }
    if let ChangeKind::Put(_, Some(doc)) = &c.kind {
        out.push_str(",\"doc\":{\"id\":");
        out.push_str(&doc.id.to_string());
        for (name, v) in &doc.fields {
            out.push(',');
            json::escape_into(out, name);
            out.push(':');
            json::value_into(out, v);
        }
        out.push('}');
    }
    out.push_str("}\n");
}
