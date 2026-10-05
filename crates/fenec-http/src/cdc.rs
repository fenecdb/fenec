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
//! A block's writes come together, numbered one by one, so a cursor inside
//! one resumes after the write it names. With `wait` an answer with nothing
//! in it waits for a write that long first. A cursor the feed no longer
//! reaches is answered 410 with the first `since` it does, never with a
//! stretch of writes missing from the middle.
//!
//! A consumer that keeps no state of its own has the server keep where it
//! is (`/_changes/consumers/<name>`), as a Kafka consumer group's committed
//! offset: a `POST` makes it, at the last write on disk unless it names a
//! `since`; `?consumer=<name>` reads from where it is, and a `POST` of
//! `since` moves it once the consumer has done with what it read -- so it
//! has each write at least once, and again after a crash before the `POST`. The
//! cursors are rows of [`CONSUMERS`], written as any write is: on disk
//! before the answer where writes are, and on the replicas. Their own
//! writes are not in the stream, or every acknowledgement would be a write
//! to read -- nor do they end a `wait`, and a commit past nothing but
//! cursors writes nothing ([`unmoved`]), or a consumer committing every
//! answer would read its own commit's empty answer and commit again.

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
        (Method::Post, ["_changes", "consumers", name]) => commit(db, feed, cfg, name, req),
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

/// Moves `name` to the `since` the body names: once the consumer has done
/// with every write up to it.
fn commit(
    db: &Arc<RwLock<Database>>,
    feed: &Feed,
    cfg: &Config,
    name: &str,
    req: &Request,
) -> Response {
    if name.is_empty() || name.len() > 200 {
        return Response::error(400, "a consumer's name is 1 to 200 bytes");
    }
    let body = std::str::from_utf8(&req.body).unwrap_or("").trim();
    let since = match body.is_empty() {
        true => None,
        false => match json::parse_object(body) {
            Ok(o) => o.into_iter().find(|(k, _)| k == "since").map(|(_, v)| v),
            Err(e) => return crate::error_response(&e),
        },
    };
    let durable = feed.durable();
    // None given: from the last write on disk, what is written from now on.
    let since = match since {
        None | Some(Value::Null) => durable as i64,
        Some(Value::Int(n)) if n >= 0 => n,
        Some(_) => {
            return Response::error(400, "the body is {\"since\": <the last write done with>}")
        }
    };
    if since as u64 > durable {
        return Response::json(
            409,
            format!("{{\"error\":\"{since} is past the last write on disk, {durable}\",\"seq\":{durable}}}"),
        );
    }
    write(db, cfg, name, Some(since))
}

fn forget(db: &Arc<RwLock<Database>>, cfg: &Config, name: &str) -> Response {
    write(db, cfg, name, None)
}

/// Sets `name`'s cursor, or with `None` lets it go, on disk before the
/// answer where writes are.
fn write(db: &Arc<RwLock<Database>>, cfg: &Config, name: &str, since: Option<i64>) -> Response {
    if cfg.read_only {
        return Response::error(403, "this server takes no writes (--http-read-only)");
    }
    let key = Value::Text(name.into());
    let done = (|| -> Result<_> {
        let mut g = crate::held::write(db);
        let there = g.collection(CONSUMERS).is_ok();
        let unmoved = match since {
            Some(n) if there => unmoved(&g, name, n)?,
            _ => false,
        };
        let mut exec =
            |sql: &str, params: &[Value]| g.execute_with(&fenec_ql::parse_one(sql)?, params);
        let n = match since {
            Some(n) if unmoved => n,
            Some(n) => {
                exec(
                    "create collection if not exists _consumers (name text @hash, since int)",
                    &[],
                )?;
                let at = Value::Int(n);
                let set = exec(
                    "set _consumers {since: $2} where name = $1",
                    &[key.clone(), at.clone()],
                )?;
                if !matches!(set, Response2::Affected(1..)) {
                    exec("put _consumers {name: $1, since: $2}", &[key.clone(), at])?;
                }
                n
            }
            None => {
                if there {
                    exec("del _consumers where name = $1", std::slice::from_ref(&key))?;
                }
                -1
            }
        };
        Ok((n, crate::flush_for(cfg, &mut g)?))
    })();
    match done {
        Ok((n, durability)) => match crate::await_durable(db, durability) {
            Ok(()) if n >= 0 => {
                let mut out = String::from("{\"name\":");
                json::escape_into(&mut out, name);
                out.push_str(&format!(",\"since\":{n}}}"));
                Response::json(200, out)
            }
            Ok(()) => Response::empty(204),
            Err(e) => crate::error_response(&e),
        },
        Err(e) => crate::error_response(&e),
    }
}

/// Whether moving `name` to `since` changes nothing it would read: where
/// it stands, every write after it is a cursor's (`_consumers`), which
/// the stream leaves out, and `since` is past where it stands. Written, a
/// sink that commits each answer -- an empty one past its own last commit
/// too -- made a write for every one it read, for ever.
fn unmoved(db: &Database, name: &str, since: i64) -> Result<bool> {
    let Some(at) = cursor_in(db, name)? else {
        return Ok(false);
    };
    if at > since as u64 {
        return Ok(false);
    }
    // `None`: past the ring, or a collection dropped since -- moved.
    Ok(db
        .changed_collections_since(at)
        .is_some_and(|names| names.iter().all(|n| n == CONSUMERS)))
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
                return answer(body, next);
            }
            Tail::Nothing => {
                let now = Instant::now();
                if now >= until {
                    return answer(String::new(), since);
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
            Tail::Gone => return Response::error(503, "the database is going"),
        }
    }
}

fn answer(body: String, next: u64) -> Response {
    let mut r = Response::json(200, body).header("Fenec-Next", &next.to_string());
    r.content_type = "application/x-ndjson";
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
