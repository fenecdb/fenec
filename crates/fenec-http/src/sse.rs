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
//! client (`web/http.js`) therefore reads the stream with `fetch` +
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

use crate::access::{Mask, Scope, Who};
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
    /// Writes held until they can write (`Fenec-Wait`): woken here too.
    waits: crate::waits::Waits,
    /// Requests waiting here for a write their client made elsewhere
    /// (`Fenec-After`, [`Hub::reached`]): what a test waits on before it
    /// lets that write come.
    after_waiting: AtomicUsize,
}

impl Watcher for Hub {
    /// Set, not raised: a replica that takes an image can land on a lower
    /// change than it held, and a mark left above it would wake every
    /// stream at once, forever. Notifications come under the database's
    /// write lock, so they arrive in the order the counter moved.
    fn notify(&self, seq: u64) {
        self.mark(seq);
        // A load, a compact, a maintenance: whatever a held write reads.
        self.waits.wrote(None);
    }

    /// A block landed: held writes are woken by the collections it wrote
    /// alone, asked of the change ring only when some are held.
    fn landed(&self, db: &Database, before: u64) {
        self.mark(db.change_seq());
        if self.waits.holds() {
            self.waits
                .wrote(db.changed_collections_since(before).as_deref());
        }
    }
}

impl Hub {
    pub fn new() -> Arc<Hub> {
        Arc::new(Hub::default())
    }

    /// The counter moved to `seq`: every stream looks.
    fn mark(&self, seq: u64) {
        let mut g = self.seq.lock().unwrap_or_else(|e| e.into_inner());
        *g = seq;
        self.cv.notify_all();
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

    /// Waits until `seq` says the database holds write `n`, or `until`:
    /// whether it does. A replica's writes come through `notify` as they
    /// are applied, so a read sent with `Fenec-After` waits for the write
    /// its client made on the primary.
    /// `look` says whether to stop, and the change the database is at.
    pub(crate) fn reached(
        &self,
        look: impl Fn() -> (bool, u64),
        until: std::time::Instant,
    ) -> bool {
        let mut waiting = None;
        loop {
            let (done, seq) = look();
            if done {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= until || self.is_closed() {
                return false;
            }
            waiting.get_or_insert_with(|| Counted::on(&self.after_waiting));
            // Past the change it looked at, or a tenth of a second: an
            // image adopted at the same change moves no counter.
            self.wait(seq, (until - now).min(Duration::from_millis(100)));
        }
    }

    /// How many requests wait here now for a write they were sent after
    /// (`Fenec-After`) -- reads, subscriptions and `/_changes` alike.
    pub fn after_waiting(&self) -> usize {
        self.after_waiting.load(Ordering::SeqCst)
    }

    /// Ends every stream on this hub: each one writes an `error` event and
    /// closes, and a client reconnecting lands wherever the tenant is now.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _g = self.seq.lock().unwrap_or_else(|e| e.into_inner());
        self.cv.notify_all();
        self.waits.close();
    }

    /// Takes subscriptions again after a `close` whose reason fell through:
    /// a tenant delete that gave up with 409 keeps the tenant.
    pub fn reopen(&self) {
        self.closed.store(false, Ordering::SeqCst);
        self.waits.reopen();
    }

    /// The writes held here until they can write.
    pub fn waits(&self) -> &crate::waits::Waits {
        &self.waits
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

/// One more on a count for as long as it lives.
struct Counted<'a>(&'a AtomicUsize);

impl<'a> Counted<'a> {
    fn on(count: &'a AtomicUsize) -> Counted<'a> {
        count.fetch_add(1, Ordering::SeqCst);
        Counted(count)
    }
}

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
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
    who: &Who,
) {
    // Said as every 503 says it, to come back in a second: the sync reads
    // the wait from the body, where plain text left it at its own backoff.
    let Some(_slot) = hub.reserve(cfg.max_streams) else {
        let refused = crate::http::Response::retry_after(503, "too many subscriptions", 1);
        let _ = write_head(out, cfg, 503, refused.content_type, Some(1), None);
        let _ = out.write_all(&refused.body);
        return;
    };

    // Sent after a write its client made elsewhere (`Fenec-After`), a
    // stream waits for it here as a read does ([`crate::after`]): served
    // from a copy that has not caught up, its seed was from before the
    // client's own write -- a read replica a router sent it to, or one
    // left on the branch a failover abandoned. After its token and its
    // slots, so a refused one waits for nothing and one past the ceiling
    // is told so at once; before the shape is read, since a collection
    // the client made may not be here yet; before the head, so a 504
    // says where the copy stands and a router sends the stream home. A
    // stream resumed with `since` waits the same way, the header being
    // the client's position and `since` only its cursor.
    if req.header("fenec-after").is_some() {
        if let Some(refused) = crate::after(req, db, hub) {
            let _ = crate::cors(refused, cfg).write(out, false, false);
            return;
        }
    }

    // Parsing needs the schema, and the schema needs a read lock. The lock is
    // released immediately: held for the whole stream it would stop all writes.
    //
    // A scoped stream of a shape that holds nothing -- `where=false`, which
    // `FenecHttp.live` opens to learn that what its query reads was
    // written -- is told of the writes to rows its token may read, and of
    // nothing else: it follows the token's own filter, its ids kept here and
    // never sent, and a write to one of them, or one of them leaving, is a
    // change holding no rows. ANDed with the token's filter, the shape
    // matched no row, a scoped stream hears only of rows it was sent, and
    // such a live query never ran again; told of every write, it would hand
    // the token the moments of other users' writes.
    //
    // A token whose read rules name fields is held to them as a read is
    // (`Scope::shaped`): the shape's terms over a field a row may not show
    // asked only where it does, and each row sent with the fields of the
    // rules admitting it (`mask`).
    //
    // A token whose rules read other collections has them bound under each
    // look's lock ([`Reach`]), from the shape as the subscriber sent it.
    //
    // Where the database stands is read under the same lock, for the head
    // (`Fenec-Seq`, `Fenec-History`): a client running reads of its own
    // after the changes the stream names -- near, on a read replica, while
    // the stream stays home -- sends them after that history, which a seq
    // alone does not name across a failover. Read here, it costs the
    // stream no lock of its own.
    let (sub, at) = {
        let guard = crate::held::read(db);
        let at = crate::At::of(&guard);
        let sub = api::subscription(&guard, req).and_then(|mut sub| {
            let mut quiet = false;
            let mut view = None;
            let mut reach = None;
            if let Who::Scoped(scope) = who {
                quiet = matches!(sub.filter, Some(Expr::Lit(Value::Bool(false))));
                let shape = if quiet { None } else { sub.filter.take() };
                if quiet {
                    sub.project = Some(vec!["id".to_string()]);
                }
                reach = Reach::of(scope, &sub.collection, &shape, &sub.project);
                let (shape, mask) = scope.shaped(&sub.collection, shape, &mut sub.project)?;
                view = Some(View {
                    scope: Arc::clone(scope),
                    mask,
                });
                sub.filter = scope.restrict(&sub.collection, shape)?;
            }
            Ok((sub, quiet, view, reach))
        });
        (sub, at)
    };
    let (mut sub, quiet, mut view, reach) = match sub {
        Ok(s) => s,
        Err(e) => {
            let _ = write_head(
                out,
                cfg,
                api::status_of(&e),
                "application/json; charset=utf-8",
                None,
                None,
            );
            let mut body = String::from("{\"error\":");
            fenec_core::json::escape_into(&mut body, &e.to_string());
            body.push('}');
            let _ = out.write_all(body.as_bytes());
            return;
        }
    };

    if write_head(out, cfg, 200, "text/event-stream", None, Some(at)).is_err() {
        return;
    }
    // A client stalled during the stream must not hold the thread forever:
    // the write timeout returns an error and the loop exits.
    let _ = out.set_write_timeout(Some(cfg.stream_write_timeout));

    // A scoped subscriber is told of a row leaving only if it was sent the
    // row. The shape's rule -- a changed id that does not match is a
    // deletion -- would otherwise hand every user the ids everyone else
    // writes. It costs the ids of the rows sent; an unscoped subscription
    // keeps the rule and holds nothing.
    //
    // Masked, each id keeps a digest of the row as it was sent, and a row
    // written again that shows the same is not sent: a write to a field
    // the token does not see would otherwise tell it when that field
    // changed, which a scoped poll's content tag (`same_answer`) keeps
    // from it as well.
    let mut seen: Option<Seen> = who.scope().map(|_| Seen {
        ids: Default::default(),
        digests: shows(&view).is_some(),
    });
    // A stream of a shape that holds nothing, unscoped, is told of every
    // write to the collection, and so of every row reaching its time.
    let everything = seen.is_none() && matches!(sub.filter, Some(Expr::Lit(Value::Bool(false))));
    let mut expiry = Expiry::of(&crate::held::read(db), &sub.collection, everything);
    let began = now_ms() as i64;
    let mut cursor = match sub.since {
        // Resuming, a scoped subscriber is seeded all the same: the rows it
        // holds are not known here, and without them no deletion could be
        // told apart from someone else's write.
        Some(n) if seen.is_none() => n,
        _ => match seed(out, db, (&mut sub, &reach, &mut view), &mut seen, quiet) {
            Ok(seq) => {
                // The seed left out every row past its time by then.
                if let Some(x) = &mut expiry {
                    x.told = Some(began);
                }
                seq
            }
            Err(_) => return,
        },
    };

    // A token is checked as the stream opens, and a stream is held for as
    // long as its client stays: past the token's `exp` it is ended, as the
    // token's next request would be refused. Kept open, it went on
    // delivering what the token could no longer read -- a member taken
    // off a team heard the team until the client went away. The `error`
    // event says 401, which a client takes as it takes a refused request:
    // a fresh token, then the stream again.
    let expires = who.scope().and_then(|s| s.expires_ms());
    let left = || expires.map(|at| Duration::from_millis(at.saturating_sub(now_ms())));

    loop {
        if left().is_some_and(|l| l.is_zero()) {
            let _ = event(out, "error", EXPIRED);
            return;
        }
        if hub.is_closed() {
            let _ = event(
                out,
                "error",
                "{\"error\":\"the tenant was closed on this node\"}",
            );
            return;
        }
        let (step, members) = {
            let guard = crate::held::read(db);
            // Rules reading another collection, bound again where it was
            // written: rows may enter and leave the shape, and show other
            // fields, with no write to its own collection.
            let moved = match &reach {
                Some(r) => r.rebind(&guard, cursor, &mut sub, &mut view),
                None => Ok(false),
            };
            let shown = shows(&view);
            let step = moved.and_then(|moved| {
                let mut step = guard.changes_since(
                    &sub.collection,
                    cursor,
                    sub.filter.as_ref(),
                    sub.project.as_deref(),
                    &[],
                )?;
                if let (Changes::Batch(b), Some((scope, mask))) = (&mut step, shown) {
                    scope.shown_rows(mask, &guard, &mut b.puts);
                }
                // The rows the shape holds now, read under the same lock,
                // where the rules' answer moved.
                let members = match (&step, moved) {
                    (Changes::Batch(_), true) => {
                        let stmt = Statement::Select(api::seed_select(&sub));
                        match guard.query(&stmt, &[])? {
                            Response::Rows(mut rs) => {
                                if let Some((scope, mask)) = shown {
                                    scope.shown_rows(mask, &guard, &mut rs);
                                }
                                Some(rs)
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                Ok((step, members))
            });
            match step {
                Ok((step, members)) => (Ok(step), members),
                Err(e) => (Err(e), None),
            }
        };
        match step {
            Err(e) => {
                // The collection may have been dropped: write the reason and
                // close rather than cutting off silently.
                let _ = event(out, "error", &error_json(&e));
                return;
            }
            Ok(Changes::Reseed) => {
                let began = now_ms() as i64;
                match seed(out, db, (&mut sub, &reach, &mut view), &mut seen, quiet) {
                    Ok(seq) => cursor = seq,
                    Err(_) => return,
                }
                if let Some(x) = &mut expiry {
                    x.told = Some(began);
                    x.stale = true;
                }
            }
            Ok(Changes::Batch(mut b)) => {
                // A write to the collection may have moved its next moment.
                if !(b.puts.rows.is_empty() && b.dels.is_empty()) {
                    if let Some(x) = &mut expiry {
                        x.stale = true;
                    }
                }
                if let (Some(now), Some(seen)) = (members, &seen) {
                    enter_and_leave(&mut b, now, seen);
                }
                if let Some(seen) = &mut seen {
                    b.dels.retain(|id| seen.left(*id));
                    b.puts.rows.retain(|r| seen.sent(r));
                }
                // The cursor advances every round, even when the batch is
                // empty. The counter and the ring are shared across all
                // collections: if a subscriber of a quiet collection never
                // moved its cursor, it would be reseeded as soon as other
                // people's writes overflowed the ring. An empty batch is not
                // sent, but the cursor is current.
                let empty = b.puts.rows.is_empty() && b.dels.is_empty() && !b.schema_changed;
                if quiet {
                    b.puts.rows.clear();
                    b.dels.clear();
                }
                if !empty && event(out, "change", &change_json(&b)).is_err() {
                    return;
                }
                cursor = b.seq;
            }
        }

        // The shape's rows past their time since the last look, told as
        // deletions now, with no write to tell of them; then when the next
        // comes.
        let mut until = None;
        if let Some(x) = &mut expiry {
            let guard = crate::held::read(db);
            if x.due(now_ms() as i64) {
                match x.step(&guard, sub.filter.as_ref()) {
                    Err(e) => {
                        drop(guard);
                        let _ = event(out, "error", &error_json(&e));
                        return;
                    }
                    Ok(mut dels) => {
                        if let Some(seen) = &mut seen {
                            dels.retain(|id| seen.left(*id));
                        }
                        if !dels.is_empty() {
                            drop(guard);
                            // A stream of a shape that holds nothing is told
                            // a change happened, and not which rows.
                            if quiet {
                                dels.clear();
                            }
                            if event(out, "change", &expired_json(cursor, &dels)).is_err() {
                                return;
                            }
                            continue;
                        }
                    }
                }
            }
            until = x.next_at(&guard);
        }

        // Keep-alive: proxies and NAT tables drop silent connections. A
        // comment line is valid in SSE and produces no event on the client
        // side.
        let keepalive = left().map_or(cfg.stream_keepalive, |l| l.min(cfg.stream_keepalive));
        let wait = match until {
            Some(at) => {
                let left = at.saturating_sub(now_ms() as i64).max(1) as u64;
                keepalive.min(Duration::from_millis(left))
            }
            None => keepalive,
        };
        // Woken for a row's time rather than the keep-alive's, it looks
        // and writes no keep-alive line.
        let expiring = wait < keepalive;
        if hub.wait(cursor, wait) <= cursor
            && !expiring
            && out.write_all(b": keepalive\n\n").is_err()
        {
            return;
        }
        if out.flush().is_err() {
            return;
        }
    }
}

/// A subscription's rows past their `@ttl`, told at their time. A row past
/// its time leaves every read at once and the file only at the sweep, a
/// minute and its grace later, which is when a subscriber heard of it: a
/// courier whose phone went quiet stayed on a dispatcher's map 73 s after
/// it had gone from every read. So the stream works out when the next row
/// of its collection reaches its time ([`Database::wakes_at`], a step of
/// the field's index), sleeps until then if no write comes first, and
/// reads the rows that reached it since it last looked -- `expired()` and
/// the field's window, the shape ANDed in -- telling each as a deletion,
/// as the sweep's delete would be told: a scoped stream of the rows it was
/// sent alone, a shape that holds nothing (`FenecHttp.live`'s) that
/// something changed. The sweep's delete of the row comes after, a
/// deletion of what is gone already, which every client takes as it is.
struct Expiry {
    collection: String,
    field: String,
    ttl: i64,
    /// Every row of the collection's, not only the shape's: an unscoped
    /// stream of a shape that holds nothing, told of any write to the
    /// collection.
    everything: bool,
    /// Rows past their time by this moment, ms, have been told. `None` on a
    /// resumed stream, which tells every row past its time not yet swept.
    told: Option<i64>,
    /// The next moment a row of the collection reaches its time.
    next: Option<i64>,
    /// `next` is to be worked out again: a write to the collection, or a
    /// look, may have moved it.
    stale: bool,
}

impl Expiry {
    /// What a stream of `collection` tells of its rows reaching their time,
    /// if they do.
    fn of(db: &Database, collection: &str, everything: bool) -> Option<Expiry> {
        let c = db.collection(collection).ok()?;
        let (field, ttl) = c
            .schema
            .fields
            .iter()
            .find_map(|f| f.index.ttl().map(|t| (f.name.clone(), t)))?;
        Some(Expiry {
            collection: collection.to_string(),
            field,
            ttl: ttl.min(i64::MAX as u64) as i64,
            everything,
            told: None,
            next: None,
            stale: true,
        })
    }

    fn due(&self, now: i64) -> bool {
        self.told.is_none() || self.next.is_some_and(|at| at <= now)
    }

    /// The next moment a row reaches its time, worked out again where a
    /// write or a look may have moved it.
    fn next_at(&mut self, db: &Database) -> Option<i64> {
        if self.stale {
            let at = self.told.unwrap_or_else(|| now_ms() as i64);
            self.next = db.wakes_at(&self.collection, None, &[], at).ok().flatten();
            self.stale = false;
        }
        self.next
    }

    /// The rows of `shape` that reached their time since the last look.
    fn step(
        &mut self,
        db: &Database,
        shape: Option<&Expr>,
    ) -> fenec_core::error::Result<Vec<DocId>> {
        let now = now_ms() as i64;
        let field = || Box::new(Expr::Field(self.field.clone()));
        let past = |t: i64| Box::new(Expr::Lit(Value::Timestamp(t.saturating_sub(self.ttl))));
        let mut f = Expr::And(
            Box::new(Expr::Call("expired".into(), Vec::new())),
            Box::new(Expr::Cmp(CmpOp::Le, field(), past(now))),
        );
        if let Some(told) = self.told {
            f = Expr::And(
                Box::new(f),
                Box::new(Expr::Cmp(CmpOp::Gt, field(), past(told))),
            );
        }
        if let (false, Some(shape)) = (self.everything, shape) {
            f = Expr::And(Box::new(shape.clone()), Box::new(f));
        }
        let sel = Select {
            collection: self.collection.clone(),
            project: Some(vec!["id".to_string()]),
            filter: Some(f),
            ..Select::default()
        };
        let ids = match db.query(&Statement::Select(sel), &[])? {
            Response::Rows(rs) => rs.rows.iter().map(|r| r.id).collect(),
            _ => Vec::new(),
        };
        self.told = Some(now);
        self.stale = true;
        Ok(ids)
    }
}

/// What a scoped stream's rows are shown through: its token's scope -- as
/// bound, where its rules read other collections ([`Reach`]) -- and the
/// mask its shape took ([`Scope::shaped`]).
struct View {
    scope: Arc<Scope>,
    mask: Option<Mask>,
}

/// The scope and mask a stream's rows are shown through, where a mask is.
fn shows(view: &Option<View>) -> Option<(&Scope, &Mask)> {
    let v = view.as_ref()?;
    Some((&*v.scope, v.mask.as_ref()?))
}

/// A scoped stream of a collection whose rules, for its token, read other
/// collections ([`crate::access::Scope::bound`]): its shape is the rules
/// as they answer, made again under each look's lock where one of those
/// collections was written since the stream's change -- or at every look
/// where an inner `get` reads the time -- and run again where the answer
/// moved, its rows diffed by id against those the stream was sent
/// ([`enter_and_leave`]). A customer following the courier carrying their
/// order hears the courier once the order is claimed and nothing of them
/// once it is delivered, with no write to `positions` between. The cost
/// is an inner `get` a stream at each write to what its rules read: a
/// bucket's read for a customer's orders.
struct Reach {
    scope: Arc<Scope>,
    collection: String,
    /// The subscriber's own filter and list, which the rules are held to.
    shape: Option<Expr>,
    project: Option<Vec<String>>,
    /// The collections the rules read, at any depth.
    reads: Vec<String>,
    /// An inner `get` reads the time.
    clock: bool,
}

impl Reach {
    fn of(
        scope: &Arc<Scope>,
        collection: &str,
        shape: &Option<Expr>,
        project: &Option<Vec<String>>,
    ) -> Option<Reach> {
        let (reads, clock) = scope.reached_from(collection)?;
        Some(Reach {
            scope: Arc::clone(scope),
            collection: collection.to_string(),
            shape: shape.clone(),
            project: project.clone(),
            reads,
            clock,
        })
    }

    /// The stream's filter, list and view as the rules answer over `db`
    /// now.
    fn bind(
        &self,
        db: &Database,
    ) -> fenec_core::error::Result<(Option<Expr>, Option<Vec<String>>, View)> {
        let bound = Arc::new(self.scope.bound_over(db, &[&self.collection])?);
        let mut project = self.project.clone();
        let (shape, mask) = bound.shaped(&self.collection, self.shape.clone(), &mut project)?;
        let filter = bound.restrict(&self.collection, shape)?;
        Ok((filter, project, View { scope: bound, mask }))
    }

    /// The stream bound again where what its rules read was written after
    /// `since`; whether its filter moved.
    fn rebind(
        &self,
        db: &Database,
        since: u64,
        sub: &mut api::Subscription,
        view: &mut Option<View>,
    ) -> fenec_core::error::Result<bool> {
        let written = self.clock
            || db
                .changed_collections_since(since)
                .is_none_or(|w| w.iter().any(|c| self.reads.contains(c)));
        if !written {
            return Ok(false);
        }
        let (filter, project, v) = self.bind(db)?;
        if filter == sub.filter {
            return Ok(false);
        }
        (sub.filter, sub.project) = (filter, project);
        *view = Some(v);
        Ok(true)
    }
}

/// The rows the shape holds `now`, as a change to a stream that was sent
/// `seen`: those it was not sent enter, and those it was sent and the
/// shape no longer holds leave -- a deletion, told only of a row it was
/// sent. Where its rows are masked every row goes, and those that show
/// what they showed when sent are left out by their digests: a row the
/// rules now admit by another field list shows other fields.
fn enter_and_leave(b: &mut ChangeBatch, now: ResultSet, seen: &Seen) {
    let held: std::collections::HashSet<DocId> = now.rows.iter().map(|r| r.id).collect();
    let put: std::collections::HashSet<DocId> = b.puts.rows.iter().map(|r| r.id).collect();
    if b.puts.rows.is_empty() {
        b.puts.columns = now.columns;
    }
    b.puts.rows.extend(
        now.rows
            .into_iter()
            .filter(|r| !put.contains(&r.id) && (seen.digests || !seen.ids.contains_key(&r.id))),
    );
    for id in seen.ids.keys() {
        if !held.contains(id) && !b.dels.contains(id) {
            b.dels.push(*id);
        }
    }
}

/// The rows a scoped stream was sent, by id: the ones it may be told
/// left. Where its rows are masked, each with a digest of the row as sent.
struct Seen {
    ids: std::collections::HashMap<DocId, u64>,
    digests: bool,
}

impl Seen {
    fn seed(&mut self, rows: &[Row]) {
        self.ids = rows.iter().map(|r| (r.id, self.digest(r))).collect();
    }

    /// Whether the row `id` was sent, forgetting it: a deletion is told.
    fn left(&mut self, id: DocId) -> bool {
        self.ids.remove(&id).is_some()
    }

    /// Whether `row`, written, is to be sent: unless it is masked and
    /// shows what it showed when last sent.
    fn sent(&mut self, row: &Row) -> bool {
        let d = self.digest(row);
        self.ids.insert(row.id, d) != Some(d) || !self.digests
    }

    /// The row's values as their encodings take them, hashed: `==` takes
    /// -0.0 for 0.0.
    fn digest(&self, row: &Row) -> u64 {
        use std::hash::{Hash, Hasher};
        if !self.digests {
            return 0;
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let mut buf = Vec::new();
        for v in &row.values {
            buf.clear();
            fenec_core::codec::encode_value(&mut buf, v);
            buf.hash(&mut h);
        }
        h.finish()
    }
}

/// A change of the rows `dels` reaching their time, at the change the
/// stream stands at: no write moved it.
fn expired_json(seq: u64, dels: &[DocId]) -> String {
    let mut out = format!("{{\"seq\":{seq},\"puts\":[],\"dels\":[");
    for (i, id) in dels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&id.to_string());
    }
    out.push_str("],\"schema\":false}");
    out
}

/// Sends the whole shape and returns the counter at that moment.
///
/// Reading the rows and taking the counter has to happen **under one lock**:
/// a write arriving in between would leave a row that is not in the seed yet
/// counts as "seen" according to the counter.
fn seed(
    out: &mut TcpStream,
    db: &Arc<RwLock<Database>>,
    (sub, reach, view): (&mut api::Subscription, &Option<Reach>, &mut Option<View>),
    seen: &mut Option<Seen>,
    quiet: bool,
) -> std::io::Result<u64> {
    let (rows, seq) = {
        let guard = crate::held::read(db);
        let answered = match reach {
            Some(r) => r.bind(&guard).map(|(filter, project, v)| {
                (sub.filter, sub.project) = (filter, project);
                *view = Some(v);
            }),
            None => Ok(()),
        };
        let stmt = Statement::Select(api::seed_select(sub));
        match answered.and_then(|_| guard.query(&stmt, &[])) {
            Ok(Response::Rows(mut rs)) => {
                if let Some((scope, mask)) = shows(view) {
                    scope.shown_rows(mask, &guard, &mut rs);
                }
                if let Some(seen) = seen {
                    seen.seed(&rs.rows);
                }
                let rows = match quiet {
                    true => String::from("[]"),
                    false => api::rows_json(&rs),
                };
                (rows, guard.change_seq())
            }
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

/// The last event of a stream whose token's `exp` passed: what a refused
/// request says, and its status, for a client to fetch a fresh token on.
pub const EXPIRED: &str = r#"{"error":"the token has expired","status":401}"#;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
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

/// A stream's head, or a refusal's in its place; `at` where the database
/// stood as the stream opened, which its head names: the change its seed is
/// at or after, and the history every event's `seq` is a change of.
fn write_head(
    out: &mut TcpStream,
    cfg: &Config,
    status: u16,
    content_type: &str,
    retry_after: Option<u64>,
    at: Option<crate::At>,
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\n\
         Cache-Control: no-cache, no-transform\r\nConnection: close\r\n",
        crate::http::reason(status)
    );
    if let Some(secs) = retry_after {
        head.push_str(&format!("Retry-After: {secs}\r\n"));
    }
    // Turns off proxy buffering (nginx); otherwise events pile up, arrive in
    // bulk, and the stream stops being "live".
    if status == 200 {
        head.push_str("X-Accel-Buffering: no\r\n");
    }
    if let Some(at) = at {
        head.push_str("Fenec-Seq: ");
        crate::http::push_number(&mut head, at.seq);
        head.push_str("\r\n");
        // The root's history names nothing, as every answer's does.
        if let Some(h) = at.history {
            head.push_str("Fenec-History: ");
            crate::http::push_history(&mut head, h);
            head.push_str("\r\n");
        }
    }
    // Every answer's CORS headers, not the origin alone: without
    // `Access-Control-Expose-Headers` a page on another origin could not
    // read the head's `Fenec-History`, nor a refusal's `Retry-After`.
    if let Some(origin) = &cfg.cors {
        crate::cors::push_into(&mut head, origin);
    }
    crate::request_id::with(|id| {
        if !id.is_empty() {
            head.push_str(&format!("{}: {id}\r\n", crate::request_id::HEADER));
        }
    });
    crate::audit::push_verified(&mut head);
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
