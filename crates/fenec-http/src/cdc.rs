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

use crate::http::{Request, Response};
use crate::replication::{Feed, Tail};
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

pub fn handle(db: &Arc<RwLock<Database>>, feed: &Feed, req: &Request) -> Response {
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
    // No cursor: from the last write on disk, what is written from now on.
    let since = since.unwrap_or_else(|| feed.durable());
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
                let g = crate::held::read_landed(db);
                let (body, next) = match lines(&g, &bytes, first, &lasts, &times, since, limit) {
                    Ok(out) => out,
                    Err(e) => return Response::error(500, &e.to_string()),
                };
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
