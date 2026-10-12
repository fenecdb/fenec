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
//! announces with the collections it wrote ([`Waits::wrote`], under the
//! write lock: a flag and a wake-up for the groups reading one), and the
//! time reaching a row's moment -- a delayed job's ready time, a lease's
//! end, a `@ttl` -- which nothing announces, and which
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
//! **A row several groups would take goes to the one whose claim ranks it
//! first, the one held longest on a tie.** A courier's claim is a group of
//! its own -- its point is in its filter, its token's `sub` in its rules --
//! so an order placed woke every courier's head, and whichever ran first
//! took it: of ten couriers waiting within reach the nearest was handed it
//! one time in ten, of a hundred one in a hundred, and each head that
//! found the row ran the request under the write lock for nothing. So a
//! head that finds a row bids for it and waits for its round: once every
//! group woken by the writes the first bidder had seen has looked past
//! them -- each once, so a round ends however many writes land meanwhile
//! -- each row goes to the bid that ranks it first ([`settle`]), the others
//! waiting behind the winner's run. The rank is the claim's own order: a
//! claim ordering by distance from its point (`near`) ranks a row by that
//! distance, so the nearest claim takes it, and the others look again once
//! its run is done, each for the row nearest it then. One ordering by a
//! field ranks every row as every other claim does, and the tie goes to
//! the head held longest -- first come, first served across groups as
//! within one, which is what a job queue's `order run_at` asks -- and the
//! others wait in a line in the order they came, the next looking once the
//! run before it is done: looking all at once after each run, a burst of
//! a thousand jobs to a thousand claims would be a thousand rounds of a
//! thousand looks. A look in the line that finds nothing ends the line,
//! the rest looking at once. A tie is settled as soon as no claim held
//! before the best bid is still to look -- no look to come could beat it
//! -- so a job to a thousand claims of a queue waits for the longest
//! held's look, not for the slowest of a thousand; a row a ranked claim
//! may bid for waits for every look, which is what asking each courier
//! how far it is costs. No clause or header turns it on: the claim's
//! order says what "first" means, and a tie keeps the order requests came
//! in.
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
    /// The tenant was closed on this node -- moved, deleted -- and every
    /// held request is answered 503, to come back through the router.
    closed: bool,
    /// In the order their heads came, the longest held first: the first
    /// still to bid is the one a tie could go to ([`settle`]), and a
    /// write wakes the heads in that order.
    groups: Vec<Group>,
    /// Groups whose tests rank rows by their own order (`near`): while any
    /// waits, a round settles only once every group has looked.
    ranked: usize,
    next: u64,
    /// Writes announced so far ([`Waits::wrote`]).
    writes: u64,
    /// The writes the round's first bidder had seen as it looked: the
    /// round settles once every group they woke has looked past them
    /// ([`settle`]). `None` while no bid waits.
    horizon: Option<u64>,
    /// Rows given to a group whose run has not ended: a bid for one waits
    /// behind that run.
    taken: Vec<(Row, u64)>,
    /// Rows a run took, and the writes announced once it had: a bid for
    /// one from a look begun before then is stale, its head looking again
    /// rather than running for a row that is gone. A round settled early
    /// left such bids to land after the winner had run.
    gone: Vec<(Row, u64)>,
}

/// A row a look found: its collection and id. `None` is a look that met an
/// error, which the run is to meet and answer, and no other group bids for.
type Row = Option<(String, DocId)>;

/// What a head's look found, bid for in its round.
struct Bid {
    row: Row,
    /// Where the claim's own order puts the row: its distance from the
    /// claim's point under `near`, `None` under an order every claim
    /// ranks the row by alike.
    rank: Option<f64>,
    /// When the head joined: the longest held wins a tie.
    joined: Instant,
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
    /// A write to a collection it reads landed since the head last looked
    /// or ran: the head looks again.
    stale: bool,
    /// The database's change when the head last looked or ran.
    since: u64,
    /// The next moment its answer may change with no write, ms.
    at: Option<i64>,
    /// The head before wrote: the next looks at once, more may be ready.
    now: bool,
    /// No write wakes the head before this: it looked and found nothing
    /// less than [`LOOK_GAP`] ago ([`Waits::wrote`]).
    quiet: Option<Instant>,
    /// Its head is looking now, outside the mutex.
    looking: bool,
    /// The writes announced when its head last began a look.
    seen: u64,
    /// What its head's look found, waiting for the round to settle.
    bid: Option<Bid>,
    /// Its bid won: the head runs.
    go: bool,
    /// Its head runs the request now.
    running: bool,
    /// Its bid lost to this group's: it looks again once that run ends.
    behind: Option<u64>,
    /// Behind a run in a line, a tie's: when its head was held, the line's
    /// order. `None` behind a ranked win, where every loser looks again.
    line: Option<Instant>,
    /// Its tests rank rows by its own order (`near`): a round waits for its
    /// bid, whenever its head was held.
    ranked: bool,
    /// Its tenant's rate refused its run: it looks again, at once, from
    /// this moment ([`Ticket::pause`]).
    back: Option<Instant>,
}

impl Group {
    /// Whether its head is to look before a round whose first bidder had
    /// seen `horizon` writes settles, at `now` (ms): woken by a write and
    /// not yet looked past them, told to look at once -- a run before it
    /// ended, its line came to it -- or come to its moment, a lease
    /// lapsed, which every courier waiting for it wakes for at once and
    /// the first awake would take. A head in its gap counts, looking at
    /// the gap's end: settled without it, a claim that looked a moment
    /// before the row came would lose it for the gap. Each looks past
    /// `horizon` once, a run's end and a moment once each, so a round ends
    /// however many writes land.
    fn owes(&self, horizon: u64, now: i64) -> bool {
        let woken = self.now || (self.stale && self.seen < horizon);
        let due = self.at.is_some_and(|at| at <= now);
        (woken || due)
            && self.back.is_none()
            && !self.looking
            && self.bid.is_none()
            && !self.go
            && !self.running
            && self.behind.is_none()
    }
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
struct Turn {
    cv: Condvar,
    /// When it was held: a tie between bids goes to the head held longest.
    joined: Instant,
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
            limit: Some(Count::N(1)),
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
    /// Whether any request is held here: what a write asks before it works
    /// out which collections it wrote.
    pub fn holds(&self) -> bool {
        self.held.load(Ordering::Acquire) > 0
    }

    /// A write landed to `written`, or to whatever a held request may read
    /// (`None`: a load, a drop, a compact): the head of each group reading
    /// one of them looks. Called by the database's watcher under its write
    /// lock, so it is a load when nothing is held. Woken by every write
    /// whatever it wrote, 150 claims held on `orders`, each a group of its
    /// own, took a put of `positions` beside them from 0.16 to 0.29 ms p50
    /// and 0.67 to 4.3 ms p99 at the node, every head a wake-up and a look
    /// under the read lock that found nothing written.
    pub fn wrote(&self, written: Option<&[String]>) {
        if !self.holds() {
            return;
        }
        let mut st = lock(&self.state);
        st.writes += 1;
        let mut now = None;
        for g in &mut st.groups {
            if let Some(cs) = written {
                if !g.collections.iter().any(|c| cs.contains(c)) {
                    continue;
                }
            }
            g.stale = true;
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
        let turn = Arc::new(Turn {
            cv: Condvar::new(),
            joined: Instant::now(),
        });
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
                let ranked = plan.tests.iter().any(|t| t.near.is_some());
                st.ranked += usize::from(ranked);
                st.groups.push(Group {
                    id,
                    tests: Arc::new(plan.tests),
                    collections: Arc::new(plan.collections),
                    queue: VecDeque::from([Arc::clone(&turn)]),
                    // Writes between the run and this moment were announced
                    // to no one here: the first look is due at once, and
                    // finds them by the change the run was at.
                    stale: true,
                    since: plan.seq,
                    at: plan.at,
                    now: false,
                    quiet: None,
                    looking: false,
                    seen: 0,
                    bid: None,
                    go: false,
                    running: false,
                    behind: None,
                    line: None,
                    ranked,
                    back: None,
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
            if ENDING.load(Ordering::Acquire) {
                return Next::End;
            }
            let writes = st.writes;
            let g = group(&mut st, self.group);
            let head = g.queue.front().is_some_and(|t| Arc::ptr_eq(t, &self.turn));
            // Its bid won: the row is run for, though the wait end as it
            // does -- answered nothing, the row would go round again.
            if head && std::mem::take(&mut g.go) {
                g.running = true;
                return run(waits);
            }
            let now = Instant::now();
            if now >= until {
                return Next::End;
            }
            let mut wait = until - now;
            // Held back by its tenant's rate: it looks once the rate lets a
            // write through.
            if let Some(back) = g.back.filter(|_| head) {
                if now < back {
                    wait = wait.min(back - now);
                } else {
                    g.back = None;
                    g.now = true;
                }
            }
            // A bid waits for its round, a loser for the winner's run.
            if head && g.bid.is_none() && g.behind.is_none() && g.back.is_none() {
                // The head before wrote: more may be ready, so this one
                // looks at once -- a look, not a run, since most often the
                // one job is gone.
                let due =
                    std::mem::take(&mut g.now) || g.at.is_some_and(|at| at <= now_us() / 1000);
                let quiet = g.quiet.filter(|q| now < *q);
                if due || (g.stale && quiet.is_none()) {
                    let (since, tests, collections) =
                        (g.since, Arc::clone(&g.tests), Arc::clone(&g.collections));
                    g.stale = false;
                    g.looking = true;
                    g.seen = writes;
                    drop(st);
                    waits.looks.fetch_add(1, Ordering::Relaxed);
                    LOOKS.fetch_add(1, Ordering::Relaxed);
                    let look = look(db, since, &tests, &collections, due);
                    st = lock(&waits.state);
                    let g = group(&mut st, self.group);
                    g.looking = false;
                    g.since = look.seq;
                    if let Some(at) = look.at {
                        g.at = at;
                    }
                    match look.found {
                        Some((row, rank)) => {
                            g.quiet = None;
                            let (seen, joined) = (g.seen, self.turn.joined);
                            let gone = row.is_some()
                                && st.gone.iter().any(|(r, at)| *r == row && seen < *at);
                            let taker = row.as_ref().and_then(|_| {
                                st.taken.iter().find(|(r, _)| *r == row).map(|(_, w)| *w)
                            });
                            let g = group(&mut st, self.group);
                            if gone {
                                // A run took it after this look began: it
                                // looks again rather than run for nothing.
                                g.now = true;
                            } else if let Some(taker) = taker {
                                // Given already, its run under way: behind
                                // it at once, no round to wait for.
                                g.behind = Some(taker);
                                g.line = rank.is_none().then_some(joined);
                            } else {
                                g.bid = Some(Bid { row, rank, joined });
                                // The writes announced by now, not as it
                                // began: a look waits for the read lock, and
                                // reads what landed meanwhile. Held to its
                                // start, a bid for a job announced while it
                                // waited set a horizon the claim held
                                // longest had looked past, and the job went
                                // to the bidder.
                                let writes = st.writes;
                                st.horizon.get_or_insert(writes);
                            }
                        }
                        None => {
                            g.quiet = Some(Instant::now() + LOOK_GAP);
                            // Nothing is left for the line behind it, most
                            // likely: each looks for itself, at once.
                            disband(&mut st, self.group);
                        }
                    }
                    settle(&mut st);
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

    /// The run it was woken for wrote nothing after all -- a request not
    /// held took the row, or a `require` was not met: it keeps its place,
    /// its group looks from where that run left the database, and the
    /// groups behind it look again.
    pub fn again(&self, plan: Plan) {
        let mut st = lock(&self.hub.waits().state);
        let g = group(&mut st, self.group);
        g.since = plan.seq;
        g.at = plan.at;
        g.running = false;
        release(&mut st, self.group);
    }

    /// Its run was refused by its tenant's rate of writes before it wrote:
    /// it keeps its place, and its group looks again once `wait` has passed
    /// and the rate lets a write through, the groups behind it looking now.
    /// Answered the 429, the claim came back at the end of every group, and
    /// a claim held to wait was told to come back when the server knew when
    /// it could run.
    pub fn pause(&self, wait: Duration) {
        let mut st = lock(&self.hub.waits().state);
        let g = group(&mut st, self.group);
        g.running = false;
        g.back = Some(Instant::now() + wait);
        release(&mut st, self.group);
        settle(&mut st);
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
            // What the head held for its group goes with it: its run, a
            // win it did not run, a bid -- which the next head looks for.
            let held = was_head && (g.bid.take().is_some() || std::mem::take(&mut g.go));
            if was_head {
                g.running = false;
            }
            if g.queue.is_empty() {
                let g = st.groups.remove(i);
                st.ranked -= usize::from(g.ranked);
            } else if was_head {
                g.now |= hand_on || held;
                if let Some(next) = g.queue.front() {
                    next.cv.notify_one();
                }
                // Its next head came later: the group goes where that puts
                // it, so the order stays the heads'.
                let g = st.groups.remove(i);
                let joined = g.queue.front().map(|t| t.joined);
                let at = st
                    .groups
                    .partition_point(|o| o.queue.front().map(|t| t.joined) <= joined);
                st.groups.insert(at, g);
            }
            if was_head {
                // Its run took what it was given: a look begun before then
                // that finds it is stale, and looks again.
                if hand_on {
                    let writes = st.writes;
                    let id = self.group;
                    let state = &mut *st;
                    let groups = &state.groups;
                    state
                        .gone
                        .retain(|(_, at)| groups.iter().any(|g| g.looking && g.seen < *at));
                    if st.groups.iter().any(|g| g.looking && g.seen < writes) {
                        let took: Vec<Row> = st
                            .taken
                            .iter()
                            .filter(|(r, g)| *g == id && r.is_some())
                            .map(|(r, _)| r.clone())
                            .collect();
                        st.gone.extend(took.into_iter().map(|r| (r, writes)));
                    }
                }
                release(&mut st, self.group);
            }
            settle(&mut st);
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

/// Settles the round once every group woken by the writes its first bidder
/// had seen has looked past them: each
/// row bid for goes to the bid ranking it first -- the claim's own order,
/// then the head held longest -- whose head runs, and the other bidders
/// wait behind that run; a row given already, its run not ended, keeps
/// every bidder for it behind that run.
fn settle(st: &mut State) {
    let horizon = st.horizon.unwrap_or(st.writes);
    let (now, instant) = (now_us() / 1000, Instant::now());
    // The groups are in the order their heads came: the first still to bid
    // -- looking, woken and yet to look, or behind a run, which bids once
    // it ends -- is the one held longest of them, and a bid ahead of it is
    // one no bid to come can beat on a tie. Asked at every look, so the
    // scan stops there: a full one, and a sort, at every look held the
    // mutex every look waits for through a thousand of each when a job
    // came to a thousand claims.
    let mut first = None;
    let mut ahead = false;
    for (i, g) in st.groups.iter().enumerate() {
        if g.looking || g.owes(horizon, now) || g.behind.is_some() {
            first = Some(i);
            break;
        }
        ahead |= g.bid.is_some();
    }
    if let Some(i) = first {
        // A head in its gap holds a round up: its gap ends, where a bid
        // waits for it -- kept, a job to claims that had looked a moment
        // before waited out the gap, the round with it. Where none waits,
        // the gap stands: ended at every look, a claim held beside a busy
        // queue looked 1 500 times a second against 700.
        let waited = ahead || st.groups[i + 1..].iter().any(|g| g.bid.is_some());
        let g = &mut st.groups[i];
        if waited && g.owes(horizon, now) && g.quiet.is_some_and(|q| instant < q) {
            g.quiet = None;
            if let Some(head) = g.queue.front() {
                head.cv.notify_one();
            }
        }
        // Nothing ahead of it, or a group ranking rows its own way among
        // those to come, which every bid waits for.
        if !ahead || st.ranked > 0 {
            return;
        }
    } else if !ahead {
        // No bid waits: the next is the first of a round of its own. Kept
        // from a round whose bids were all dropped as stale, a horizon
        // below the next round's write let the claim held longest, which
        // had looked past it, go unwaited for, and the job to whichever
        // claim bid first.
        st.horizon = None;
        return;
    }
    // A row settles once no bid to come could beat its best: none is to
    // come, or each would be a tie's from a head held after the best.
    let first = first.and_then(|i| st.groups[i].queue.front().map(|t| t.joined));
    let settles = |b: &Bid| match first {
        None => true,
        Some(first) => b.rank.is_none() && b.joined < first,
    };
    let mut bidders: Vec<usize> = (0..st.groups.len())
        .filter(|&i| st.groups[i].bid.is_some())
        .collect();
    fn bid(st: &State, i: usize) -> &Bid {
        st.groups[i].bid.as_ref().expect("a bidder bids")
    }
    // A row of its own -- a look that met an error -- is no other's.
    bidders.sort_by(|&a, &b| {
        let (a, b) = (bid(st, a), bid(st, b));
        a.row
            .cmp(&b.row)
            .then(
                a.rank
                    .unwrap_or(f64::INFINITY)
                    .total_cmp(&b.rank.unwrap_or(f64::INFINITY)),
            )
            .then(a.joined.cmp(&b.joined))
    });
    let mut at = 0;
    while at < bidders.len() {
        let row = bid(st, bidders[at]).row.clone();
        let end = match row {
            None => at + 1,
            Some(_) => {
                at + bidders[at..]
                    .iter()
                    .take_while(|&&i| bid(st, i).row == row)
                    .count()
            }
        };
        let given = row
            .as_ref()
            .and_then(|_| st.taken.iter().find(|(r, _)| *r == row).map(|(_, g)| *g));
        let best = st.groups[bidders[at]].bid.as_ref().unwrap();
        if given.is_none() && !settles(best) {
            at = end;
            continue;
        }
        // Bidders that rank the row alike wait in a line in the order they
        // came; ranked ones each look again once the winner has run.
        let tie = bidders[at..end]
            .iter()
            .all(|&i| st.groups[i].bid.as_ref().is_some_and(|b| b.rank.is_none()));
        let winner = match given {
            Some(g) => g,
            None => {
                let g = &mut st.groups[bidders[at]];
                g.bid = None;
                g.go = true;
                if let Some(head) = g.queue.front() {
                    head.cv.notify_one();
                }
                let id = g.id;
                if row.is_some() {
                    st.taken.push((row, id));
                }
                at += 1;
                id
            }
        };
        for &i in &bidders[at..end] {
            let g = &mut st.groups[i];
            let joined = g.bid.take().map(|b| b.joined);
            g.behind = Some(winner);
            g.line = joined.filter(|_| tie);
        }
        at = end;
    }
    if !st.groups.iter().any(|g| g.bid.is_some()) {
        st.horizon = None;
    }
}

/// `id`'s run ended, or its head left: the rows it was given are no
/// longer held for it, and the groups behind it look again -- each behind
/// a ranked win at once, of a line the first, the rest of the line behind
/// it.
fn release(st: &mut State, id: u64) {
    st.taken.retain(|(_, g)| *g != id);
    let first = st
        .groups
        .iter()
        .filter(|g| g.behind == Some(id))
        .filter_map(|g| g.line.map(|at| (at, g.id)))
        .min()
        .map(|(_, g)| g);
    for g in &mut st.groups {
        if g.behind != Some(id) {
            continue;
        }
        if g.line.is_some() && first != Some(g.id) {
            g.behind = first;
            continue;
        }
        wake(g);
    }
}

/// `id` looked and found nothing: the line behind it looks at once, each
/// for itself, rather than one after another for what is most likely
/// gone.
fn disband(st: &mut State, id: u64) {
    for g in &mut st.groups {
        if g.behind == Some(id) {
            wake(g);
        }
    }
}

/// A group out of a line or from behind a run: its head looks at once.
fn wake(g: &mut Group) {
    g.behind = None;
    g.line = None;
    g.now = true;
    if let Some(head) = g.queue.front() {
        head.cv.notify_one();
    }
}

fn group(st: &mut State, id: u64) -> &mut Group {
    st.groups
        .iter_mut()
        .find(|g| g.id == id)
        .expect("a ticket's group stays while it holds a place")
}

struct Look {
    /// The row a test found and where the claim's order ranks it.
    found: Option<(Row, Option<f64>)>,
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
            found: None,
            seq,
            at: None,
        };
    }
    for t in tests {
        let stmt = Statement::Select(t.clone());
        let found = match g.query(&stmt, &[]) {
            Ok(Response::Rows(rs)) => match rs.rows.first() {
                None => continue,
                Some(r) => (Some((t.collection.clone(), r.id)), rank(&g, t, r.score)),
            },
            // An error the run should meet and answer.
            _ => (None, None),
        };
        return Look {
            found: Some(found),
            seq,
            at: None,
        };
    }
    Look {
        found: None,
        seq,
        at: Some(next_moment(&g, tests, before)),
    }
}

/// Where a test's order puts the row it found, lower first: under `near`
/// the row's `_score` -- metres from a point, a vector's distance -- or a
/// sparse vector's dot product turned round, the higher being nearer. A
/// test ordered by fields ranks a row as every other test of that order
/// does: `None`, and the tie is the arrival's.
fn rank(db: &Database, test: &Select, score: Option<f32>) -> Option<f64> {
    let near = test.near.as_ref()?;
    let score = f64::from(score?);
    let sparse = db
        .collection(&test.collection)
        .ok()
        .and_then(|c| {
            c.schema
                .field(&near.field)
                .map(|f| matches!(f.ty, DataType::Sparse(_)))
        })
        .unwrap_or(false);
    Some(if sparse { -score } else { score })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A point read as written, not into a vector's `f32`s.
    fn parse(text: &str) -> Statement {
        fenec_ql::parse_exact(text).unwrap().remove(0)
    }

    fn run_sql(db: &RwLock<Database>, text: &str) {
        db.write().unwrap().execute(&parse(text)).unwrap();
    }

    fn select(text: &str) -> Select {
        match parse(text) {
            Statement::Select(s) => s,
            other => panic!("{other:?}"),
        }
    }

    /// `n` claims held in the order of `tests`, each a group of its own,
    /// and one order placed: which of them was run for, a claim run taking
    /// the order away as a courier's claim would.
    fn first_run(tests: Vec<Select>) -> usize {
        let hub = Hub::new();
        let db = Arc::new(RwLock::new(Database::new()));
        run_sql(
            &db,
            "create collection orders (pickup geo @geo, run_at timestamp @sorted)",
        );
        db.write()
            .unwrap()
            .set_watcher(Arc::clone(&hub) as Arc<dyn Watcher>);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut threads = Vec::new();
        for (i, test) in tests.into_iter().enumerate() {
            let (hub, db, tx) = (Arc::clone(&hub), Arc::clone(&db), tx.clone());
            let (joined_tx, joined) = std::sync::mpsc::channel();
            threads.push(std::thread::spawn(move || {
                let plan = Plan {
                    tests: vec![test],
                    collections: vec!["orders".to_string()],
                    at: None,
                    seq: db.read().unwrap().change_seq(),
                };
                let ticket = Ticket::join(&hub, plan);
                joined_tx.send(()).unwrap();
                let until = Instant::now() + Duration::from_secs(60);
                match ticket.next(until, &db) {
                    Next::Run => {
                        run_sql(&db, "del orders");
                        tx.send(i).unwrap();
                        ticket.leave(true);
                    }
                    _ => ticket.leave(false),
                }
                answered();
            }));
            // Held in order: each joins before the next is started.
            joined.recv().unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        run_sql(&db, "put orders {pickup: [29.0, 41.0], run_at: 0}");
        let first = rx.recv().unwrap();
        // The rest are let go of: they would wait out their minute.
        hub.waits().close();
        for t in threads {
            t.join().unwrap();
        }
        assert!(rx.try_recv().is_err(), "one claim ran for the one order");
        first
    }

    /// A row several groups would take goes to the one whose own order
    /// ranks it first: under `near`, the claim nearest the row, though it
    /// was held after the others; under an order every claim ranks the row
    /// by alike, the one held longest. Raced, whichever head ran first took
    /// it: the nearest of ten couriers one time in ten.
    #[test]
    fn a_row_goes_to_the_claim_ranking_it_first() {
        // Couriers 3, 1 and 2 km east of the pickup, held in that order.
        let near = |km: f64| {
            select(&format!(
                "get orders select id where run_at <= now() near pickup [{}, 41.0] limit 1",
                29.0 + km / 84.0
            ))
        };
        assert_eq!(first_run(vec![near(3.0), near(1.0), near(2.0)]), 1);
        // Within 5 km of each courier, oldest first: a tie, the first held.
        let within = |km: f64| {
            select(&format!(
                "get orders select id where run_at <= now() and \
                 distance(pickup, [{}, 41.0]) <= 5000 order run_at limit 1",
                29.0 + km / 84.0
            ))
        };
        assert_eq!(first_run(vec![within(3.0), within(1.0), within(2.0)]), 0);
    }
}
