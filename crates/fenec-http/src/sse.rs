//! Subscription endpoint: `GET /<name>/changes` -- Server-Sent Events.
//!
//! **Why SSE and not WebSocket.** The stream is one way: the server pushes
//! changes, and the client already does its writes with ordinary
//! `POST`/`PATCH`/`DELETE`. For that, SSE is plain HTTP -- no handshake, no
//! framing, just a long-running response as far as proxies and intermediate
//! servers are concerned. It costs ~150 lines; WebSocket would need framing,
//! masking and ping/pong for the same job.
//!
//! **Why `fetch` rather than `EventSource`.** The browser's `EventSource`
//! cannot send headers, so it cannot carry `Authorization: Bearer`. The
//! client (`web/fenec.js`) therefore reads the stream with `fetch` +
//! `ReadableStream`; the wire format is still standard SSE and can be
//! watched with `curl`.
//!
//! ## Wire format
//! ```text
//! event: seed
//! data: {"seq":42,"rows":[{...}]}
//!
//! event: change
//! data: {"seq":43,"puts":[{...}],"dels":[7],"schema":false}
//!
//! : keepalive
//! ```
//!
//! `seed` means "this is the whole shape": the client replaces its local
//! copy. `change` is an incremental diff and **carries state**, not an
//! operation (see `fenec_core::changes`): re-applying it is harmless.

use crate::api;
use crate::http::Request;
use crate::Config;
use fenec_core::prelude::*;
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

/// The point that announces writes to waiting subscribers.
///
/// `fenec-core` knows no waiting primitive (there are no threads in WASM);
/// [`Watcher`] only says "the counter reached this point". The Condvar lives
/// here, on the server side.
///
/// Condvar instead of polling is about latency, not scale: a 50 ms polling
/// loop takes 20 read locks per second per subscriber and still adds 50 ms
/// of delay. Condvar zeroes both; a subscriber only wakes on a real write or
/// on the keep-alive timeout.
#[derive(Default)]
pub struct Hub {
    seq: Mutex<u64>,
    cv: Condvar,
    /// Number of live streams. Counted apart from normal requests:
    /// subscriptions are long lived, and sharing a single ceiling would let
    /// 100 subscribers close the server to ordinary requests.
    live: AtomicUsize,
    /// Set when the database behind the hub is going away (a tenant being
    /// deleted or moved). The open streams hold the database alive; they
    /// have to be told to let go, or a moved tenant's old copy would keep
    /// answering from a file that is no longer authoritative.
    closed: AtomicBool,
}

impl Watcher for Hub {
    /// Set, not raised: a replica that takes an image can land on a lower
    /// change than it held, and a mark left above it would wake every
    /// stream at once, forever. Notifications come under the database's
    /// write lock, so they arrive in the order the counter moved.
    fn notify(&self, seq: u64) {
        let mut g = self.seq.lock().unwrap_or_else(|e| e.into_inner());
        *g = seq;
        self.cv.notify_all();
    }
}

impl Hub {
    pub fn new() -> Arc<Hub> {
        Arc::new(Hub::default())
    }

    /// Waits until it sees a counter greater than `after`. On timeout it
    /// returns as is -- the caller writes the keep-alive line.
    fn wait(&self, after: u64, timeout: Duration) -> u64 {
        let g = self.seq.lock().unwrap_or_else(|e| e.into_inner());
        if *g > after || self.is_closed() {
            return *g;
        }
        let (g, _) = self
            .cv
            .wait_timeout(g, timeout)
            .unwrap_or_else(|e| e.into_inner());
        *g
    }

    /// Ends every stream on this hub: each one writes an `error` event and
    /// closes, and a client reconnecting lands wherever the tenant is now.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _g = self.seq.lock().unwrap_or_else(|e| e.into_inner());
        self.cv.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// Reserves a stream slot; `None` when the ceiling is full.
    fn reserve(&self, max: usize) -> Option<StreamSlot<'_>> {
        let n = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        if max > 0 && n > max {
            self.live.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(StreamSlot(self))
    }
}

struct StreamSlot<'a>(&'a Hub);
impl Drop for StreamSlot<'_> {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Serves a subscription request and **consumes the connection**.
///
/// It is separate from the normal response path because of
/// `Content-Length`: an ordinary response knows its body and writes it in
/// one go, while a stream is a body with no end.
pub fn serve(
    out: &mut TcpStream,
    db: &Arc<RwLock<Database>>,
    cfg: &Config,
    hub: &Hub,
    req: &Request,
) {
    let Some(_slot) = hub.reserve(cfg.max_streams) else {
        let _ = write_head(out, cfg, 503, "text/plain; charset=utf-8");
        let _ = out.write_all(b"too many subscriptions\n");
        return;
    };

    // Parsing needs the schema, and the schema needs a read lock. The lock is
    // released immediately: held for the whole stream it would stop all writes.
    let sub = {
        let guard = db.read().unwrap_or_else(|e| e.into_inner());
        api::subscription(&guard, req)
    };
    let sub = match sub {
        Ok(s) => s,
        Err(e) => {
            let _ = write_head(
                out,
                cfg,
                api::status_of(&e),
                "application/json; charset=utf-8",
            );
            let mut body = String::from("{\"error\":");
            fenec_core::json::escape_into(&mut body, &e.to_string());
            body.push('}');
            let _ = out.write_all(body.as_bytes());
            return;
        }
    };

    if write_head(out, cfg, 200, "text/event-stream").is_err() {
        return;
    }
    // A client stalled during the stream must not hold the thread forever:
    // the write timeout returns an error and the loop exits.
    let _ = out.set_write_timeout(Some(cfg.stream_write_timeout));

    let mut cursor = match sub.since {
        Some(n) => n,
        None => match seed(out, db, &sub) {
            Ok(seq) => seq,
            Err(_) => return,
        },
    };

    loop {
        if hub.is_closed() {
            let _ = event(
                out,
                "error",
                "{\"error\":\"the tenant was closed on this node\"}",
            );
            return;
        }
        let step = {
            let guard = db.read().unwrap_or_else(|e| e.into_inner());
            guard.changes_since(
                &sub.collection,
                cursor,
                sub.filter.as_ref(),
                sub.project.as_deref(),
                &[],
            )
        };
        match step {
            Err(e) => {
                // The collection may have been dropped: write the reason and
                // close rather than cutting off silently.
                let _ = event(out, "error", &error_json(&e));
                return;
            }
            Ok(Changes::Reseed) => match seed(out, db, &sub) {
                Ok(seq) => cursor = seq,
                Err(_) => return,
            },
            Ok(Changes::Batch(b)) => {
                // The cursor advances every round, even when the batch is
                // empty. The counter and the ring are shared across all
                // collections: if a subscriber of a quiet collection never
                // moved its cursor, it would be reseeded as soon as other
                // people's writes overflowed the ring. An empty batch is not
                // sent, but the cursor is current.
                let empty = b.puts.rows.is_empty() && b.dels.is_empty() && !b.schema_changed;
                if !empty && event(out, "change", &change_json(&b)).is_err() {
                    return;
                }
                cursor = b.seq;
            }
        }

        // Keep-alive: proxies and NAT tables drop silent connections. A
        // comment line is valid in SSE and produces no event on the client
        // side.
        if hub.wait(cursor, cfg.stream_keepalive) <= cursor
            && out.write_all(b": keepalive\n\n").is_err()
        {
            return;
        }
        if out.flush().is_err() {
            return;
        }
    }
}

/// Sends the whole shape and returns the counter at that moment.
///
/// Reading the rows and taking the counter has to happen **under one lock**:
/// a write arriving in between would leave a row that is not in the seed yet
/// counts as "seen" according to the counter.
fn seed(
    out: &mut TcpStream,
    db: &Arc<RwLock<Database>>,
    sub: &api::Subscription,
) -> std::io::Result<u64> {
    let (rows, seq) = {
        let guard = db.read().unwrap_or_else(|e| e.into_inner());
        let stmt = Statement::Select(api::seed_select(sub));
        match guard.query(&stmt, &[]) {
            Ok(Response::Rows(rs)) => (api::rows_json(&rs), guard.change_seq()),
            Ok(_) => (String::from("[]"), guard.change_seq()),
            Err(e) => {
                event(out, "error", &error_json(&e))?;
                return Err(std::io::Error::other("seeding failed"));
            }
        }
    };
    event(out, "seed", &format!("{{\"seq\":{seq},\"rows\":{rows}}}"))?;
    Ok(seq)
}

fn change_json(b: &ChangeBatch) -> String {
    let mut out = format!("{{\"seq\":{},\"puts\":", b.seq);
    out.push_str(&api::rows_json(&b.puts));
    out.push_str(",\"dels\":[");
    for (i, id) in b.dels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&id.to_string());
    }
    out.push_str("],\"schema\":");
    out.push_str(if b.schema_changed { "true" } else { "false" });
    out.push('}');
    out
}

fn error_json(e: &Error) -> String {
    let mut body = String::from("{\"error\":");
    fenec_core::json::escape_into(&mut body, &e.to_string());
    body.push('}');
    body
}

/// A single SSE event. `data` is single-line JSON, so no line splitting is
/// needed -- the JSON encoder never produces a `\n`.
fn event(out: &mut TcpStream, name: &str, data: &str) -> std::io::Result<()> {
    out.write_all(format!("event: {name}\ndata: {data}\n\n").as_bytes())?;
    out.flush()
}

fn write_head(
    out: &mut TcpStream,
    cfg: &Config,
    status: u16,
    content_type: &str,
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\n\
         Cache-Control: no-cache, no-transform\r\nConnection: close\r\n",
        crate::http::reason(status)
    );
    // Turns off proxy buffering (nginx); otherwise events pile up, arrive in
    // bulk, and the stream stops being "live".
    if status == 200 {
        head.push_str("X-Accel-Buffering: no\r\n");
    }
    if let Some(origin) = &cfg.cors {
        head.push_str(&format!(
            "Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"
        ));
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes())?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A replica that takes an image can land on a lower change than it
    /// held. The hub has to follow it down: kept at the old high mark, it
    /// told every stream reseeded at the new one that a write was waiting,
    /// and the stream spun on empty batches until writes caught up.
    #[test]
    fn the_mark_follows_the_database_down() {
        let hub = Hub::new();
        hub.notify(1000);
        hub.notify(900);
        let t = std::time::Instant::now();
        assert_eq!(hub.wait(900, Duration::from_millis(50)), 900);
        assert!(t.elapsed() >= Duration::from_millis(40));
    }
}
