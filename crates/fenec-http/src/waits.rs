//! A write held until it can write: `Fenec-Wait` on `/query` or `/batch`.
//!
//! A worker that finds its queue empty had to sleep and claim again --
//! late by its sleep, or a claim under the write lock every few
//! milliseconds per idle worker. Sent with `Fenec-Wait: <ms>`, a request
//! whose `set`s and `del`s write nothing is held here instead and run again
//! once it may write: answered with what it wrote, or at the end of the
//! wait with what it answered last (no rows, `affected 0`, a `require`'s
//! 412). Redis's `BLPOP`, SQS's long poll, BullMQ's blocking fetch.
//!
//! **A header, not a clause.** Waiting is the request's, not the
//! statement's: a statement runs under the write lock, where nothing may
//! wait, and in a page or an app's library there is no other worker to wait
//! for. So the text is the claim it always was, every builder writes it as
//! before, and `Fenec-Wait` is the family `Fenec-After` began: how long the
//! request may wait for what it waits for.
//!
//! **When it runs again.** Two things change what a held write would
//! write: a write to a collection it reads, which the database's watcher
//! announces ([`Waits::wrote`], under the write lock, a counter and a
//! wake-up), and the time reaching a row's moment -- a delayed job's ready
//! time, a lease's end, a `@ttl` -- which nothing announces, and which
//! [`Database::wakes_at`] works out from the filter: one step of an
//! `@sorted` index past `now()`. A held request sleeps on a `Condvar` until
//! the one or the other, so a quiet server holding a thousand claims does
//! nothing at all.
//!
//! **One awake in each group, the longest waiting first.** Woken on every
//! write, a hundred workers holding the same claim would each run it, and a
//! job enqueued would be taken by one and looked for by ninety-nine, each
//! under the write lock. Requests are grouped by what they would pick --
//! each `set` and `del` as the `get` of one row it amounts to, scope and
//! parameters bound in, the values it writes and the rows it answers left
//! out -- and of a group only the head, the request held longest, is woken.
//! It looks, under the read lock, whether a collection it reads was written
//! and whether a row is there to take now; finding one, it runs the
//! request, and having written it hands the turn to the next, since more
//! may be ready. One that found nothing is the group's answer: the rest
//! would have found nothing either. So a job goes to the worker that has
//! waited longest, as Redis serves `BLPOP`'s clients, and a write costs a
//! group one look, not a run a worker.
//!
//! What a look sees is the statement's own filter as the token runs it --
//! its rules ANDed in by `scoped()` before the request is held -- so a
//! held request wakes for a row it could write and for no other, and it is
//! the run that answers, under the scope, as any request's.

use crate::sse::Hub;
use fenec_core::prelude::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

/// The longest a request may be held, as `Fenec-After`'s wait.
pub const LONGEST: Duration = Duration::from_secs(30);

/// One database's held requests: a part of its [`Hub`], which the
/// database's watcher reaches under the write lock.
#[derive(Default)]
pub struct Waits {
    state: Mutex<State>,
    /// Requests held here: what a write asks before taking the mutex, so a
    /// database holding none pays a load a write.
    held: AtomicUsize,
    /// Heads' looks under the read lock, and held requests run again: what
    /// a write cost the requests held beside it.
    looks: AtomicU64,
    runs: AtomicU64,
}

/// What a database's held requests did ([`Waits::stats`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub held: usize,
    pub looks: u64,
    pub runs: u64,
}

#[derive(Default)]
struct State {
    /// Writes announced so far: a head that last looked at an older count
    /// looks again.
    writes: u64,
    /// The tenant was closed on this node -- moved, deleted -- and every
    /// held request is answered 503, to come back through the router.
    closed: bool,
    groups: Vec<Group>,
    next: u64,
}

struct Group {
    id: u64,
    /// What its requests would pick: each `set` and `del` as the `get` of
    /// one row it amounts to.
    tests: Arc<Vec<Select>>,
    /// Whose writes wake it: the tests' collections and their inner `get`s'.
    collections: Arc<Vec<String>>,
    /// Who waits, the longest first: the first is the head.
    queue: VecDeque<Arc<Turn>>,
    /// `State::writes` when the head last looked or ran.
    writes: u64,
    /// The database's change when the head last looked or ran.
    since: u64,
    /// The next moment its answer may change with no write, ms.
    at: Option<i64>,
    /// The head before wrote: the next looks at once, more may be ready.
    now: bool,
    /// No write wakes the head before this: it looked and found nothing
    /// less than [`LOOK_GAP`] ago ([`Waits::wrote`]).
    quiet: Option<Instant>,
}

/// The least time between two looks of a group that found nothing, unless
/// its moment comes. Woken at every write, the head of 16 claims held on a
/// queue that stayed empty looked 3 400 times a second beside 16 busy
/// workers over HTTP, each look under the read lock, and their jobs a
/// second went 150 000 -> 142 000; with the gap it looks 730 times and they
/// take 148 000 (`make queue-bench`'s `held busy`). A write in the gap costs
/// a group a counter, and a job enqueued in it is taken at most this much
/// later.
pub const LOOK_GAP: Duration = Duration::from_millis(1);

/// A held request's place.
#[derive(Default)]
struct Turn {
    cv: Condvar,
}

/// What a held request is run again for, and how it ends.
pub enum Next {
    Run,
    /// The wait passed, or the server is going down: answered with what it
    /// answered last.
    End,
    /// The tenant was closed here: 503, come back.
    Closed,
}

/// What `handle` leaves for the connection when a request sent with
/// `Fenec-Wait` wrote nothing ([`plan`]).
pub struct Plan {
    tests: Vec<Select>,
    collections: Vec<String>,
    at: Option<i64>,
    seq: u64,
}

thread_local! {
    static PLANNED: RefCell<Option<Plan>> = const { RefCell::new(None) };
}

/// Left by a request held for nothing more: the plan of its last run, or
/// none when that run wrote, failed or was not held.
pub fn planned() -> Option<Plan> {
    PLANNED.with(|p| p.borrow_mut().take())
}

/// Held requests across the process, until each is answered and written:
/// what a shutdown waits for ([`end_all`]).
static OPEN: AtomicUsize = AtomicUsize::new(0);
/// Set once the server is going down: a held request is answered at once,
/// and none is held from then on.
static ENDING: AtomicBool = AtomicBool::new(false);
/// The hubs that held a request, for a shutdown to wake.
static HUBS: Mutex<Vec<Weak<Hub>>> = Mutex::new(Vec::new());

/// How long `req` asks to be held, if it does: `Fenec-Wait` on a write
/// sent to `/query` or `/batch` without `Fenec-After` -- whose wait the
/// header is, there -- at most [`LONGEST`]. `Err` is the refusal.
pub fn asked(req: &crate::http::Request) -> std::result::Result<Option<Duration>, String> {
    let Some(v) = req.header("fenec-wait") else {
        return Ok(None);
    };
    if req.header("fenec-after").is_some() {
        return Ok(None);
    }
    let Ok(ms) = v.trim().parse::<u64>() else {
        return Err("Fenec-Wait is milliseconds".into());
    };
    if ms == 0 || ENDING.load(Ordering::Relaxed) {
        return Ok(None);
    }
    Ok(Some(Duration::from_millis(ms).min(LONGEST)))
}

/// Whether a held request may hold `stmt`: a `set` or a `del`, whose
/// writes a look can test, or a read beside them in a `/batch`. Its time
/// must be one [`Database::wakes_at`] can place.
pub fn holdable(stmt: &Statement, params: &[Value]) -> std::result::Result<(), Error> {
    match stmt {
        Statement::Update { filter, .. } | Statement::Delete { filter, .. } => {
            let mut f = filter.clone();
            if let Some(f) = &mut f {
                bind(f, params);
            }
            fenec_core::engine::placeable(f.as_ref())
        }
        Statement::Select(_) | Statement::Explain(_) => Ok(()),
        _ => Err(Error::Query(
            "Fenec-Wait holds a `set` or a `del` that writes nothing until it can; this \
             request writes otherwise"
                .into(),
        )),
    }
}

/// Whether `stmt`, answered `r`, wrote nothing: a count of 0, no row
/// answered, or a `require` it did not meet -- whose block was put back.
pub fn wrote_nothing(stmt: &Statement, r: &fenec_core::error::Result<Response>) -> bool {
    match r {
        Err(Error::Unmet(_)) => true,
        Err(_) => false,
        Ok(r) => nothing_written(stmt, r),
    }
}

/// [`wrote_nothing`] of a statement that answered `r`: a read writes
/// nothing.
pub fn nothing_written(stmt: &Statement, r: &Response) -> bool {
    match r {
        _ if stmt.is_read_only() => true,
        Response::Affected(0) => true,
        Response::Rows(rs) => rs.rows.is_empty(),
        _ => false,
    }
}

/// Leaves the plan of a held request's run that wrote nothing, made under
/// the write lock it ran under: the gets its writes amount to, and when
/// their answer next changes with no write. `before` is when it ran.
pub fn plan(db: &Database, stmts: &[(&Statement, &[Value])], before: i64) {
    let mut tests = Vec::new();
    let mut collections = Vec::new();
    for (stmt, params) in stmts {
        let (collection, filter, order) = match stmt {
            Statement::Update {
                collection,
                filter,
                pick,
                ..
            }
            | Statement::Delete {
                collection,
                filter,
                pick,
                ..
            } => (collection, filter, pick.as_ref().map(|p| p.order.clone())),
            _ => continue,
        };
        let mut filter = filter.clone();
        if let Some(f) = &mut filter {
            bind(f, params);
            read_by(f, &mut collections);
        }
        if !collections.contains(collection) {
            collections.push(collection.clone());
        }
        let test = Select {
            collection: collection.clone(),
            project: Some(vec!["id".to_string()]),
            filter,
            order: order.unwrap_or_default(),
            limit: Some(1),
            ..Select::default()
        };
        if !tests.contains(&test) {
            tests.push(test);
        }
    }
    let at = next_moment(db, &tests, before);
    let p = Plan {
        tests,
        collections,
        at,
        seq: db.change_seq(),
    };
    PLANNED.with(|slot| *slot.borrow_mut() = Some(p));
}

/// The least moment of every test's, a refusal or a failed probe counting
/// as none: [`holdable`] refused an unplaceable time before the request
/// ran, and a probe's error is the run's to meet.
fn next_moment(db: &Database, tests: &[Select], at: i64) -> Option<i64> {
    tests
        .iter()
        .filter_map(|t| {
            db.wakes_at(&t.collection, t.filter.as_ref(), &[], at)
                .ok()
                .flatten()
        })
        .min()
}

/// Each parameter in `e` made the value it is bound to, inner `get`s
/// included: a test runs with none, and two requests whose filters bind the
/// same values are one group whatever else their parameters hold.
fn bind(e: &mut Expr, params: &[Value]) {
    if let Expr::Param(i) = e {
        if let Some(v) = params.get(*i) {
            *e = Expr::Lit(v.clone());
        }
        return;
    }
    if let Expr::InSelect(_, inner) = e {
        if let Some(f) = &mut inner.filter {
            bind(f, params);
        }
    }
    e.each_child_mut(&mut |c| bind(c, params));
}

/// The collections an inner `get` in `e` reads.
fn read_by(e: &Expr, out: &mut Vec<String>) {
    if let Expr::InSelect(_, inner) = e {
        if !out.contains(&inner.collection) {
            out.push(inner.collection.clone());
        }
        if let Some(f) = &inner.filter {
            read_by(f, out);
        }
    }
    e.each_child(&mut |c| read_by(c, out));
}

/// The time, as `now()` reads it.
pub fn now_ms() -> i64 {
    fenec_core::time::now_ms().unwrap_or(0)
}

/// The same clock in microseconds.
fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as i64)
}

impl Waits {
    /// A write landed: the head of each group looks. Called by the
    /// database's watcher under its write lock, so it is a load when
    /// nothing is held.
    pub fn wrote(&self) {
        if self.held.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut st = lock(&self.state);
        st.writes += 1;
        let mut now = None;
        for g in &st.groups {
            // A head in its gap looks at its end, and wakes for that.
            if let Some(q) = g.quiet {
                if *now.get_or_insert_with(Instant::now) < q {
                    continue;
                }
            }
            if let Some(head) = g.queue.front() {
                head.cv.notify_one();
            }
        }
    }

    /// The tenant goes away from this node: every held request is answered
    /// 503 and lets go of it.
    pub fn close(&self) {
        let mut st = lock(&self.state);
        st.closed = true;
        wake_all(&st);
    }

    pub fn reopen(&self) {
        lock(&self.state).closed = false;
    }

    /// Requests held here now, and what they did.
    pub fn stats(&self) -> Stats {
        Stats {
            held: self.held.load(Ordering::Acquire),
            looks: self.looks.load(Ordering::Relaxed),
            runs: self.runs.load(Ordering::Relaxed),
        }
    }
}

fn lock(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn wake_all(st: &State) {
    for g in &st.groups {
        for t in &g.queue {
            t.cv.notify_one();
        }
    }
}

/// A held request's place in its group, from its first run that wrote
/// nothing until it is answered.
pub struct Ticket<'h> {
    hub: &'h Arc<Hub>,
    group: u64,
    turn: Arc<Turn>,
}

impl<'h> Ticket<'h> {
    /// Takes a place at the back of the group `plan`'s tests make, or
    /// starts it.
    pub fn join(hub: &'h Arc<Hub>, plan: Plan) -> Ticket<'h> {
        enlist(hub);
        OPEN.fetch_add(1, Ordering::AcqRel);
        let waits = hub.waits();
        let turn = Arc::new(Turn::default());
        let mut st = lock(&waits.state);
        waits.held.fetch_add(1, Ordering::AcqRel);
        let group = match st.groups.iter_mut().find(|g| *g.tests == plan.tests) {
            Some(g) => {
                g.queue.push_back(Arc::clone(&turn));
                // Its run may have seen a moment the group's last look had
                // not: the earlier of the two is kept.
                g.at = match (g.at, plan.at) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                g.id
            }
            None => {
                st.next += 1;
                let id = st.next;
                // Writes between the run and this moment were announced to
                // no one here: the first look is due at once, and finds
                // them by the change the run was at.
                let writes = st.writes.wrapping_sub(1);
                st.groups.push(Group {
                    id,
                    tests: Arc::new(plan.tests),
                    collections: Arc::new(plan.collections),
                    queue: VecDeque::from([Arc::clone(&turn)]),
                    writes,
                    since: plan.seq,
                    at: plan.at,
                    now: false,
                    quiet: None,
                });
                id
            }
        };
        drop(st);
        Ticket { hub, group, turn }
    }

    /// Waits until this request should run again, or ends. `db` is read
    /// for a head's looks, never under the mutex a write announces itself
    /// through.
    pub fn next(&self, until: Instant, db: &RwLock<Database>) -> Next {
        let waits = self.hub.waits();
        let mut st = lock(&waits.state);
        loop {
            if st.closed || self.hub.is_closed() {
                return Next::Closed;
            }
            let now = Instant::now();
            if now >= until || ENDING.load(Ordering::Acquire) {
                return Next::End;
            }
            let writes = st.writes;
            let g = group(&mut st, self.group);
            let head = g.queue.front().is_some_and(|t| Arc::ptr_eq(t, &self.turn));
            let mut wait = until - now;
            if head {
                // The head before wrote: more may be ready, so this one
                // looks at once -- a look, not a run, since most often the
                // one job is gone.
                let due =
                    std::mem::take(&mut g.now) || g.at.is_some_and(|at| at <= now_us() / 1000);
                let quiet = g.quiet.filter(|q| now < *q);
                if due || (g.writes != writes && quiet.is_none()) {
                    let (since, tests, collections) =
                        (g.since, Arc::clone(&g.tests), Arc::clone(&g.collections));
                    g.writes = writes;
                    drop(st);
                    waits.looks.fetch_add(1, Ordering::Relaxed);
                    LOOKS.fetch_add(1, Ordering::Relaxed);
                    let look = look(db, since, &tests, &collections, due);
                    st = lock(&waits.state);
                    let g = group(&mut st, self.group);
                    g.since = look.seq;
                    if let Some(at) = look.at {
                        g.at = at;
                    }
                    if look.found {
                        g.quiet = None;
                        return run(waits);
                    }
                    g.quiet = Some(Instant::now() + LOOK_GAP);
                    continue;
                }
                if let Some(q) = quiet {
                    wait = wait.min(q - now);
                }
                if let Some(at) = g.at {
                    // To the microsecond: a millisecond's rounding woke a
                    // head before a delayed job's time, to wait one more.
                    let left = (at * 1000).saturating_sub(now_us()).max(1) as u64;
                    wait = wait.min(Duration::from_micros(left));
                }
            }
            st = self
                .turn
                .cv
                .wait_timeout(st, wait)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// The run it was woken for wrote nothing after all -- another group
    /// took the row, or a `require` was not met: it keeps its place, and
    /// its group looks from where that run left the database.
    pub fn again(&self, plan: Plan) {
        let mut st = lock(&self.hub.waits().state);
        let g = group(&mut st, self.group);
        g.since = plan.seq;
        g.at = plan.at;
    }

    /// Answered: it leaves its group. One that wrote -- or whose client
    /// went away as a row was there for it -- hands the turn to the next
    /// at once, since more may be ready.
    pub fn leave(self, hand_on: bool) {
        let waits = self.hub.waits();
        let mut st = lock(&waits.state);
        let at = st.groups.iter().position(|g| g.id == self.group);
        if let Some(i) = at {
            let g = &mut st.groups[i];
            let was_head = g.queue.front().is_some_and(|t| Arc::ptr_eq(t, &self.turn));
            g.queue.retain(|t| !Arc::ptr_eq(t, &self.turn));
            if g.queue.is_empty() {
                st.groups.swap_remove(i);
            } else if was_head {
                g.now |= hand_on;
                if let Some(next) = g.queue.front() {
                    next.cv.notify_one();
                }
            }
        }
        waits.held.fetch_sub(1, Ordering::AcqRel);
    }
}

fn run(waits: &Waits) -> Next {
    waits.runs.fetch_add(1, Ordering::Relaxed);
    RUNS.fetch_add(1, Ordering::Relaxed);
    Next::Run
}

/// Heads' looks and held requests run again, across the process: what
/// `/_metrics` counts.
static LOOKS: AtomicU64 = AtomicU64::new(0);
static RUNS: AtomicU64 = AtomicU64::new(0);

/// The process's held requests now, heads' looks and runs so far.
pub fn totals() -> Stats {
    Stats {
        held: OPEN.load(Ordering::Acquire),
        looks: LOOKS.load(Ordering::Relaxed),
        runs: RUNS.load(Ordering::Relaxed),
    }
}

/// A held request answered and written: a shutdown waits for none more.
pub fn answered() {
    OPEN.fetch_sub(1, Ordering::AcqRel);
}

fn group(st: &mut State, id: u64) -> &mut Group {
    st.groups
        .iter_mut()
        .find(|g| g.id == id)
        .expect("a ticket's group stays while it holds a place")
}

struct Look {
    found: bool,
    seq: u64,
    /// The next moment, worked out again where a write or the time made
    /// the old one stale.
    at: Option<Option<i64>>,
}

/// A head's look, under the read lock: was a collection its tests read
/// written since `since`, and if so -- or if its moment came -- is a row
/// there to take now.
fn look(
    db: &RwLock<Database>,
    since: u64,
    tests: &[Select],
    collections: &[String],
    due: bool,
) -> Look {
    let before = now_ms();
    let _looking = crate::trace::span("fenec.wait_look");
    let g = crate::held::read(db);
    let seq = g.change_seq();
    let written = seq != since
        && g.changed_collections_since(since)
            .is_none_or(|cs| cs.iter().any(|c| collections.contains(c)));
    if !written && !due {
        return Look {
            found: false,
            seq,
            at: None,
        };
    }
    for t in tests {
        let stmt = Statement::Select(t.clone());
        match g.query(&stmt, &[]) {
            Ok(Response::Rows(rs)) if rs.rows.is_empty() => {}
            // A row to take, or an error the run should meet and answer.
            _ => {
                return Look {
                    found: true,
                    seq,
                    at: None,
                }
            }
        }
    }
    Look {
        found: false,
        seq,
        at: Some(next_moment(&g, tests, before)),
    }
}

/// Keeps `hub` where a shutdown finds it.
fn enlist(hub: &Arc<Hub>) {
    let mut hubs = HUBS.lock().unwrap_or_else(|e| e.into_inner());
    if hubs.iter().any(|h| h.as_ptr() == Arc::as_ptr(hub)) {
        return;
    }
    hubs.retain(|h| h.strong_count() > 0);
    hubs.push(Arc::downgrade(hub));
}

/// The server is going down: every held request is answered with what it
/// answered last, at once, and none is held from now on. Waits up to
/// `grace` for their answers to be written, so the process does not exit
/// under them.
pub fn end_all(grace: Duration) {
    ENDING.store(true, Ordering::Release);
    let hubs: Vec<Arc<Hub>> = HUBS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    for hub in &hubs {
        wake_all(&lock(&hub.waits().state));
    }
    drop(hubs);
    let until = Instant::now() + grace;
    while OPEN.load(Ordering::Acquire) > 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(2));
    }
}
