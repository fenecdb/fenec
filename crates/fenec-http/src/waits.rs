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
//! **One run in each group, the longest waiting first.** Woken on every
//! write, a hundred workers holding the same claim would each run it, and a
//! job enqueued would be taken by one and looked for by ninety-nine, each
//! under the write lock. Requests are grouped by what they would pick --
//! each `set` and `del` as the `get` of one row it amounts to, scope and
//! parameters bound in, the values it writes and the rows it answers left
//! out -- and a group looks, under the read lock, whether a collection it
//! reads was written and whether a row is there to take now; finding one,
//! its head, the request held longest, runs, and having written hands the
//! turn to the next, since more may be ready. A look that found nothing is
//! the group's answer: the rest would have found nothing either. So a job
//! goes to the worker that has waited longest, as Redis serves `BLPOP`'s
//! clients, and a write costs a group one look, not a run a worker.
//!
//! **One held request looks for every group.** Each head woken to look
//! for itself, an order placed beside a thousand couriers' claims, each a
//! group, woke a thousand threads, each taking the mutex, the read lock
//! and its whole pick in turn: 20 ms from the put to the nearest courier,
//! and 175 ms of the server's CPU an order -- 1.7 and 3.2 ms with the two
//! below (`make queue-bench`'s `held nearest`). So the looks are one held
//! request's -- whichever waits when none looks, until it leaves for its
//! run or its answer and hands them to another ([`resign`]) -- made for
//! every group a write, a moment or a run's end left to look, under one
//! read lock ([`look_all`]); a head is woken only when its group's bid
//! wins, or its wait ends.
//!
//! **A group that found nothing looks among the rows written since.** Its
//! look knows every row its one test passes then -- none, or the few a
//! look among rows found ([`Known`]) -- and until the time reaches the
//! test's next moment ([`Database::wakes_at`]) no row it did not pass
//! starts to pass but by a write. So its next look tests those rows and the
//! rows written since alone ([`Database::written_since`], the change ring)
//! with its own filter, and puts them in its own order
//! ([`Database::passing_among`], [`Database::picked_among`]): the row a
//! whole pick would give, at a few rows' cost, the rows a filter passes
//! found once for every group of it. A write the ring no longer reaches back to, a schema
//! change, a load or a compact, more rows than [`AMONG`], an inner `get`,
//! rows that expire, a time in the order, two tests, or a moment come:
//! the whole pick, as before.
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
use fenec_core::changes::SCHEMA_MARK;
use fenec_core::engine::Moments;
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
    /// Groups' looks under the read lock, and held requests run again:
    /// what a write cost the requests held beside it.
    looks: AtomicU64,
    runs: AtomicU64,
    /// Looks that ran a group's whole pick, not its test among the rows
    /// written since ([`Known`]).
    picks: AtomicU64,
    /// Moved by a write that says nothing of what it wrote -- a load, a
    /// compact, a maintenance: what a group knew of its rows before, it
    /// knows no more.
    epoch: AtomicU64,
}

/// What a database's held requests did ([`Waits::stats`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub held: usize,
    pub looks: u64,
    pub runs: u64,
    /// Of the looks, those that ran a group's whole pick.
    pub picks: u64,
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
    /// The held request that looks for every group ([`Ticket::next`]):
    /// the first to wait while none did, until it leaves.
    leader: Option<Arc<Turn>>,
    /// Its looks are under way, outside the mutex: what it would be told,
    /// it sees as they end.
    looking: bool,
}

/// What a group's last look knows of its one test: every row it passed --
/// `ids`, ascending, none or a few -- with the database at change `seq`
/// (its collection numbered `cid`, its writes' `epoch`) and the time at
/// `at`; and that until `until`, the test's next moment, no row starts or
/// stops passing but by a write. So the rows it passes now are among `ids`
/// and those written since `seq`.
struct Known {
    epoch: u64,
    seq: u64,
    cid: u32,
    ids: Vec<DocId>,
    at: i64,
    until: Option<i64>,
}

/// The most rows a look among them tests, those known and those written
/// since: past it the whole pick runs, which steps an index to its row
/// where the test reads each of these whole. A burst of a thousand jobs is
/// a thousand rows written since every claim's last look.
const AMONG: usize = 32;

/// The longest the looks of a batch hold the read lock before letting it
/// go and taking it again: a thousand groups' whole picks over a
/// collection no index narrows would hold every write out for as long as
/// all of them took, where each group's own look held it for its own.
const LOOK_HOLD: Duration = Duration::from_millis(2);

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
    /// What its last look knew of the rows its test passes: its next looks
    /// among the rows written since. Taken out while it looks.
    known: Option<Known>,
}

impl Group {
    /// Whether nothing holds it from looking: no look under way, no bid,
    /// no run won or under way, no run it waits behind, no rate's pause.
    fn idle(&self) -> bool {
        !self.looking
            && self.bid.is_none()
            && !self.go
            && !self.running
            && self.behind.is_none()
            && self.back.is_none()
    }

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
        let mut order = order.unwrap_or_default();
        // A key worked out of the row -- `order distance(pickup, $2)` --
        // bound as the filter is: a test runs with no parameters, and two
        // couriers' claims are two groups by the points they name.
        for e in order.iter_mut().filter_map(|s| s.expr.as_mut()) {
            bind(e, params);
        }
        // Where the first key is worked out of the row, the test answers it
        // too: what the claim ranks the row it found by, beside every other
        // claim's rank of it ([`rank`]).
        let computed: Vec<Column> = order
            .first()
            .and_then(|s| s.expr.clone())
            .map(|expr| Column {
                expr,
                name: RANK.to_string(),
            })
            .into_iter()
            .collect();
        let mut project = vec!["id".to_string()];
        project.extend(computed.iter().map(|c| c.name.clone()));
        let test = Select {
            collection: collection.clone(),
            project: Some(project),
            filter,
            order,
            limit: Some(Count::N(1)),
            computed,
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
    /// (`None`: a load, a drop, a compact): each group reading one of them
    /// looks. Called by the database's watcher under its write lock, so it
    /// is a load when nothing is held. Woken by every write whatever it
    /// wrote, 150 claims held on `orders`, each a group of its own, took a
    /// put of `positions` beside them from 0.16 to 0.29 ms p50 and 0.67 to
    /// 4.3 ms p99 at the node, every head a wake-up and a look under the
    /// read lock that found nothing written.
    pub fn wrote(&self, written: Option<&[String]>) {
        if written.is_none() {
            self.epoch.fetch_add(1, Ordering::AcqRel);
        }
        if !self.holds() {
            return;
        }
        let mut st = lock(&self.state);
        st.writes += 1;
        let mut now = None;
        let mut tell = false;
        for g in &mut st.groups {
            if let Some(cs) = written {
                if !g.collections.iter().any(|c| cs.contains(c)) {
                    continue;
                }
            }
            let was = std::mem::replace(&mut g.stale, true);
            if !g.idle() {
                continue;
            }
            // A group in its gap looks at its end: the looks are told of
            // the first write in it, for that, and of no other -- told of
            // each, a claim held beside a busy queue was a wake-up a write.
            if let Some(q) = g.quiet {
                if *now.get_or_insert_with(Instant::now) < q {
                    tell |= !was;
                    continue;
                }
            }
            tell = true;
        }
        if tell {
            kick(&st);
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
            picks: self.picks.load(Ordering::Relaxed),
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

/// A group has a look to make, or a moment to wake for: the request that
/// looks for every group is told -- once its looks under way end, it sees
/// it -- and where none does, one waiting is woken to.
fn kick(st: &State) {
    if st.looking {
        return;
    }
    match &st.leader {
        Some(l) => l.cv.notify_one(),
        None => {
            if let Some(t) = waiting(st, None) {
                t.cv.notify_one();
            }
        }
    }
}

/// A held request waiting in [`Ticket::next`], `but` aside: any but a head
/// whose group runs, or is about to.
fn waiting<'s>(st: &'s State, but: Option<&Arc<Turn>>) -> Option<&'s Arc<Turn>> {
    st.groups.iter().find_map(|g| {
        g.queue
            .iter()
            .enumerate()
            .find(|(i, t)| {
                !but.is_some_and(|b| Arc::ptr_eq(b, t)) && (*i > 0 || !(g.running || g.go))
            })
            .map(|(_, t)| t)
    })
}

/// `me` leaves [`Ticket::next`]: where it looked for every group, or none
/// did, one still waiting is woken to.
fn resign(st: &mut State, me: &Arc<Turn>) {
    if st.leader.as_ref().is_some_and(|l| !Arc::ptr_eq(l, me)) {
        return;
    }
    st.leader = None;
    if let Some(t) = waiting(st, Some(me)) {
        t.cv.notify_one();
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
                // not: the earlier of the two is kept, and the looks told
                // of it.
                let at = match (g.at, plan.at) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                let sooner = at != g.at;
                g.at = at;
                let id = g.id;
                if sooner {
                    kick(&st);
                }
                id
            }
            None => {
                st.next += 1;
                let id = st.next;
                let ranked = plan.tests.iter().any(ranks);
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
                    known: None,
                });
                kick(&st);
                id
            }
        };
        drop(st);
        Ticket { hub, group, turn }
    }

    /// Waits until this request should run again, or ends. `db` is read
    /// for the looks, never under the mutex a write announces itself
    /// through.
    ///
    /// The first request to wait while none looks for every group does,
    /// until it leaves: it looks for each group with a look to make, under
    /// one read lock, settles the round, and sleeps to the next moment a
    /// group has to look at. The rest sleep until their group's bid wins or
    /// their wait ends.
    pub fn next(&self, until: Instant, db: &RwLock<Database>) -> Next {
        let waits = self.hub.waits();
        let mut st = lock(&waits.state);
        loop {
            let ended = if st.closed || self.hub.is_closed() {
                Some(Next::Closed)
            } else if ENDING.load(Ordering::Acquire) {
                Some(Next::End)
            } else {
                None
            };
            if let Some(ended) = ended {
                resign(&mut st, &self.turn);
                return ended;
            }
            let g = group(&mut st, self.group);
            let head = g.queue.front().is_some_and(|t| Arc::ptr_eq(t, &self.turn));
            // Its bid won: the row is run for, though the wait end as it
            // does -- answered nothing, the row would go round again.
            if head && std::mem::take(&mut g.go) {
                g.running = true;
                resign(&mut st, &self.turn);
                return run(waits);
            }
            let now = Instant::now();
            if now >= until {
                resign(&mut st, &self.turn);
                return Next::End;
            }
            let mut wait = until - now;
            let leads = match &st.leader {
                Some(l) => Arc::ptr_eq(l, &self.turn),
                None => {
                    st.leader = Some(Arc::clone(&self.turn));
                    true
                }
            };
            if leads {
                let jobs = jobs(&mut st);
                if !jobs.is_empty() {
                    st.looking = true;
                    drop(st);
                    let n = jobs.len() as u64;
                    waits.looks.fetch_add(n, Ordering::Relaxed);
                    LOOKS.fetch_add(n, Ordering::Relaxed);
                    let looks = look_all(db, jobs, waits);
                    st = lock(&waits.state);
                    st.looking = false;
                    for (id, look) in looks {
                        record(&mut st, id, look);
                    }
                    settle(&mut st);
                    continue;
                }
                if let Some(next) = deadline(&st, now) {
                    wait = wait.min(asked_for(next));
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
        kick(&st);
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
        kick(&st);
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
            kick(&st);
        }
        waits.held.fetch_sub(1, Ordering::AcqRel);
    }
}

fn run(waits: &Waits) -> Next {
    waits.runs.fetch_add(1, Ordering::Relaxed);
    RUNS.fetch_add(1, Ordering::Relaxed);
    Next::Run
}

/// Groups' looks and held requests run again, across the process: what
/// `/_metrics` counts.
static LOOKS: AtomicU64 = AtomicU64::new(0);
static RUNS: AtomicU64 = AtomicU64::new(0);
static PICKS: AtomicU64 = AtomicU64::new(0);

/// The process's held requests now, groups' looks and runs so far.
pub fn totals() -> Stats {
    Stats {
        held: OPEN.load(Ordering::Acquire),
        looks: LOOKS.load(Ordering::Relaxed),
        runs: RUNS.load(Ordering::Relaxed),
        picks: PICKS.load(Ordering::Relaxed),
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
            // Looked for at once: by the looks that settled the round, or
            // told by whoever did ([`kick`]).
            g.quiet = None;
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
                // The one head woken: it runs.
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

/// A group out of a line or from behind a run: it looks at once, once
/// the looks are told ([`kick`]).
fn wake(g: &mut Group) {
    g.behind = None;
    g.line = None;
    g.now = true;
}

fn group(st: &mut State, id: u64) -> &mut Group {
    st.groups
        .iter_mut()
        .find(|g| g.id == id)
        .expect("a ticket's group stays while it holds a place")
}

/// A group's look to make: what it reads, from where it last looked, and
/// what it knows of its rows.
struct Job {
    id: u64,
    since: u64,
    tests: Arc<Vec<Select>>,
    collections: Arc<Vec<String>>,
    /// Its moment came, or the run before wrote: it looks whether or not
    /// a collection it reads was written.
    due: bool,
    known: Option<Known>,
}

/// The looks each group with one to make is to make now, the longest held
/// first: each marked looking and its look taken out to make.
fn jobs(st: &mut State) -> Vec<Job> {
    let (instant, now) = (Instant::now(), now_us() / 1000);
    let writes = st.writes;
    let mut jobs = Vec::new();
    for g in &mut st.groups {
        // Held back by its tenant's rate: it looks once the rate lets a
        // write through.
        if let Some(back) = g.back.filter(|_| !g.running && !g.go) {
            if instant < back {
                continue;
            }
            g.back = None;
            g.now = true;
        }
        if g.queue.is_empty() || !g.idle() {
            continue;
        }
        // The head before wrote: more may be ready, so it looks at once --
        // a look, not a run, since most often the one job is gone.
        let due = g.now || g.at.is_some_and(|at| at <= now);
        let quiet = g.quiet.is_some_and(|q| instant < q);
        if !due && !(g.stale && !quiet) {
            continue;
        }
        g.now = false;
        g.stale = false;
        g.looking = true;
        g.seen = writes;
        jobs.push(Job {
            id: g.id,
            since: g.since,
            tests: Arc::clone(&g.tests),
            collections: Arc::clone(&g.collections),
            due,
            known: g.known.take(),
        });
    }
    jobs
}

/// How long until a group not looking now has to: its rate's pause over,
/// the end of its gap where a write landed in it, its moment.
fn deadline(st: &State, now: Instant) -> Option<Duration> {
    let us = now_us();
    let mut least: Option<Duration> = None;
    let mut keep = |d: Duration| least = Some(least.map_or(d, |l| l.min(d)));
    for g in &st.groups {
        if let Some(back) = g.back.filter(|_| !g.running && !g.go) {
            keep(back.saturating_duration_since(now));
            continue;
        }
        if g.queue.is_empty() || !g.idle() {
            continue;
        }
        if let (true, Some(q)) = (g.stale, g.quiet) {
            keep(q.saturating_duration_since(now));
        }
        if let Some(at) = g.at {
            // To the microsecond: a millisecond's rounding woke a head
            // before a delayed job's time, to wait one more.
            let left = (at * 1000).saturating_sub(us).max(1) as u64;
            keep(Duration::from_micros(left));
        }
    }
    least
}

/// What to ask a timed wait for, to wake by `d`. macOS lets one run late
/// by half again -- 3 ms took 4.5 every time, 6 ms 9, a thread's sleep
/// as a condition's wait -- and a delayed job went from 0.2 ms after its
/// time at the median to 1.3, p99 1.5 to 29, once the looks' one request
/// slept to its moment, where each head had slept a gap at a time: so
/// two thirds of it are asked for, and the rest again from there. Where a
/// wait keeps its time, that is a few wakes more on the way to a moment.
fn asked_for(d: Duration) -> Duration {
    match d > Duration::from_micros(200) {
        true => d * 2 / 3,
        false => d,
    }
}

/// What a look found, kept for its group under the mutex.
fn record(st: &mut State, id: u64, look: Look) {
    let writes = st.writes;
    let Some(g) = st.groups.iter_mut().find(|g| g.id == id) else {
        // Its last request left while it was looked for.
        return;
    };
    g.looking = false;
    g.since = look.seq;
    g.known = look.known;
    if let Some(at) = look.at {
        g.at = at;
    }
    let (seen, joined) = (g.seen, g.queue.front().map(|t| t.joined));
    let Some(joined) = joined else {
        return;
    };
    match look.found {
        Some((row, rank)) => {
            g.quiet = None;
            let gone = row.is_some() && st.gone.iter().any(|(r, at)| *r == row && seen < *at);
            let taker = row
                .as_ref()
                .and_then(|_| st.taken.iter().find(|(r, _)| *r == row).map(|(_, w)| *w));
            let g = group(st, id);
            if gone {
                // A run took it after this look began: it looks again
                // rather than run for nothing.
                g.now = true;
            } else if let Some(taker) = taker {
                // Given already, its run under way: behind it at once, no
                // round to wait for.
                g.behind = Some(taker);
                g.line = rank.is_none().then_some(joined);
            } else {
                g.bid = Some(Bid { row, rank, joined });
                // The writes announced by now, not as it began: a look
                // waits for the read lock, and reads what landed meanwhile.
                // Held to its start, a bid for a job announced while it
                // waited set a horizon the claim held longest had looked
                // past, and the job went to the bidder.
                st.horizon.get_or_insert(writes);
            }
        }
        None => {
            g.quiet = Some(Instant::now() + LOOK_GAP);
            // Nothing is left for the line behind it, most likely: each
            // looks for itself, at once.
            disband(st, id);
        }
    }
}

struct Look {
    /// The row a test found and where the claim's order ranks it.
    found: Option<(Row, Option<f64>)>,
    seq: u64,
    /// The next moment, worked out again where a write or the time made
    /// the old one stale.
    at: Option<Option<i64>>,
    /// What it knows of the rows its test passes, for its next look.
    known: Option<Known>,
}

/// Each group's look, under the read lock -- let go and taken again each
/// [`LOOK_HOLD`], so a writer waits out a part of them, not all: was a
/// collection its tests read written since it last looked, and if so -- or
/// if its moment came -- is a row there to take now.
fn look_all(db: &RwLock<Database>, jobs: Vec<Job>, waits: &Waits) -> Vec<(u64, Look)> {
    let _looking = crate::trace::span("fenec.wait_look");
    let mut out = Vec::with_capacity(jobs.len());
    let mut jobs = jobs.into_iter().peekable();
    while jobs.peek().is_some() {
        let g = crate::held::read(db);
        let t = g.now_ms().unwrap_or_else(|_| now_ms());
        let held = Instant::now();
        let mut looks = Looks::new(&g, waits.epoch.load(Ordering::Acquire), t);
        for job in jobs.by_ref() {
            let id = job.id;
            out.push((id, looks.look(job)));
            if held.elapsed() >= LOOK_HOLD {
                break;
            }
        }
        waits.picks.fetch_add(looks.picks, Ordering::Relaxed);
        PICKS.fetch_add(looks.picks, Ordering::Relaxed);
    }
    out
}

/// The rows of a collection written since a change, as
/// [`Database::written_since`] gave them, by the change and the collection.
type Written = (u64, String, Option<(u32, Vec<DocId>)>);

/// The rows of `ids` the filter of a group's one test let through: what
/// the next group of the same filter and rows is answered, a thousand
/// couriers' `run_at <= now()` asked once of the one order written.
struct Passed {
    tests: Arc<Vec<Select>>,
    ids: Vec<DocId>,
    kept: Vec<DocId>,
}

/// How many filters' answers [`Passed`] keeps a batch: each group asks
/// them all before its own, so groups of as many filters as groups ask
/// past a few for nothing.
const PASSED: usize = 4;

/// Looks under one read lock, at the time `t` read before it: what one
/// asks of the database as it stands, the others ask again for nothing --
/// the collections written since a change, the rows, an index's step.
struct Looks<'d> {
    db: &'d Database,
    epoch: u64,
    t: i64,
    seq: u64,
    changed: Vec<(u64, Option<Vec<String>>)>,
    written: Vec<Written>,
    passed: Vec<Passed>,
    moments: Moments,
    /// Whole picks run.
    picks: u64,
}

impl<'d> Looks<'d> {
    fn new(db: &'d Database, epoch: u64, t: i64) -> Looks<'d> {
        Looks {
            db,
            epoch,
            t,
            seq: db.change_seq(),
            changed: Vec::new(),
            written: Vec::new(),
            passed: Vec::new(),
            moments: Moments::default(),
            picks: 0,
        }
    }

    fn look(&mut self, mut job: Job) -> Look {
        let seq = self.seq;
        let written = seq != job.since && self.wrote(job.since, &job.collections);
        if !written && !job.due {
            return Look {
                found: None,
                seq,
                at: None,
                known: job.known.take(),
            };
        }
        if let Some(look) = self.among(&job) {
            return look;
        }
        self.whole(&job)
    }

    /// Whether a collection of `collections` was written since `since`.
    fn wrote(&mut self, since: u64, collections: &[String]) -> bool {
        let at = match self.changed.iter().position(|(s, _)| *s == since) {
            Some(at) => at,
            None => {
                let cs = self.db.changed_collections_since(since);
                self.changed.push((since, cs));
                self.changed.len() - 1
            }
        };
        self.changed[at]
            .1
            .as_ref()
            .is_none_or(|cs| cs.iter().any(|c| collections.contains(c)))
    }

    /// The look among the rows its group knows and those written since,
    /// where what it knows still holds: `None` runs the whole pick.
    fn among(&mut self, job: &Job) -> Option<Look> {
        let known = job.known.as_ref()?;
        let [test] = job.tests.as_slice() else {
            return None;
        };
        // A load or a compact since, the clock set back, or a moment come:
        // a row neither known nor written may pass now.
        if known.epoch != self.epoch
            || self.t < known.at
            || known.until.is_some_and(|u| self.t >= u)
        {
            return None;
        }
        let at = match self
            .written
            .iter()
            .position(|(s, c, _)| *s == known.seq && *c == test.collection)
        {
            Some(at) => at,
            None => {
                let w = self.db.written_since(&test.collection, known.seq);
                self.written.push((known.seq, test.collection.clone(), w));
                self.written.len() - 1
            }
        };
        let (cid, written) = self.written[at].2.as_ref()?;
        // Dropped and made again, its schema changed, or too many rows to
        // test one by one.
        if *cid != known.cid
            || written.first() == Some(&SCHEMA_MARK)
            || known.ids.len() + written.len() > AMONG
        {
            return None;
        }
        let mut ids = Vec::with_capacity(known.ids.len() + written.len());
        ids.extend_from_slice(&known.ids);
        ids.extend_from_slice(written);
        ids.sort_unstable();
        ids.dedup();
        let cid = *cid;
        // An error the whole pick meets, or not, as it plans: it answers.
        let shared = self.passed.iter().find(|p| {
            let t = &p.tests[0];
            t.collection == test.collection && t.filter == test.filter && p.ids == ids
        });
        let kept = match shared {
            Some(p) => p.kept.clone(),
            None => {
                let kept = self
                    .db
                    .passing_among(&test.collection, test.filter.as_ref(), &ids, self.t)
                    .ok()??;
                let p = Passed {
                    tests: Arc::clone(&job.tests),
                    ids,
                    kept: kept.clone(),
                };
                if self.passed.len() == PASSED {
                    self.passed.remove(0);
                }
                self.passed.push(p);
                kept
            }
        };
        let rs = self.db.picked_among(test, kept).ok()??;
        let until = self
            .db
            .wakes_at_with(
                &test.collection,
                test.filter.as_ref(),
                &[],
                self.t,
                &mut self.moments,
            )
            .ok()?;
        let found = rs.rows.first().map(|r| {
            (
                Some((test.collection.clone(), r.id)),
                rank(self.db, test, &rs),
            )
        });
        let known = (rs.rows.len() <= AMONG).then(|| Known {
            epoch: self.epoch,
            seq: self.seq,
            cid,
            ids: rs.rows.iter().map(|r| r.id).collect(),
            at: self.t,
            until,
        });
        Some(Look {
            at: found.is_none().then_some(until),
            found,
            seq: self.seq,
            known,
        })
    }

    /// The whole pick: each test run as a `get`, the first that finds a row
    /// the look's. One that finds none knows its one test passes no row.
    fn whole(&mut self, job: &Job) -> Look {
        self.picks += 1;
        for t in job.tests.iter() {
            let stmt = Statement::Select(t.clone());
            let found = match self.db.query(&stmt, &[]) {
                Ok(Response::Rows(rs)) => match rs.rows.first() {
                    None => continue,
                    Some(r) => (Some((t.collection.clone(), r.id)), rank(self.db, t, &rs)),
                },
                // An error the run should meet and answer.
                _ => (None, None),
            };
            return Look {
                found: Some(found),
                seq: self.seq,
                at: None,
                known: None,
            };
        }
        let (at, known) = match job.tests.as_slice() {
            [test] => match self.db.wakes_at_with(
                &test.collection,
                test.filter.as_ref(),
                &[],
                self.t,
                &mut self.moments,
            ) {
                Ok(until) => (until, self.none_known(test, until)),
                Err(_) => (None, None),
            },
            tests => (
                next_moment_with(self.db, tests, self.t, &mut self.moments),
                None,
            ),
        };
        Look {
            found: None,
            seq: self.seq,
            at: Some(at),
            known,
        }
    }

    /// That `test` passes no row from `t` to its next moment, `until`: its
    /// pick read the clock as it ran, after `t`, and what it answered holds
    /// from `t` only where no moment came before that.
    fn none_known(&self, test: &Select, until: Option<i64>) -> Option<Known> {
        if until.is_some_and(|u| self.db.now_ms().map_or(true, |now| now >= u)) {
            return None;
        }
        let (cid, _) = self.db.written_since(&test.collection, self.seq)?;
        Some(Known {
            epoch: self.epoch,
            seq: self.seq,
            cid,
            ids: Vec::new(),
            at: self.t,
            until,
        })
    }
}

/// [`next_moment`] with each step of an index asked once a batch.
fn next_moment_with(
    db: &Database,
    tests: &[Select],
    at: i64,
    moments: &mut Moments,
) -> Option<i64> {
    tests
        .iter()
        .filter_map(|t| {
            db.wakes_at_with(&t.collection, t.filter.as_ref(), &[], at, moments)
                .ok()
                .flatten()
        })
        .min()
}

/// The column a test answers its first key under, where that key is worked
/// out of the row.
const RANK: &str = "_rank";

/// Whether a test ranks rows its own way: by `near`, or by a first key
/// worked out of the row -- a courier's `order distance(pickup, $2)` --
/// which two claims may rank one row by differently.
fn ranks(test: &Select) -> bool {
    test.near.is_some() || test.order.first().is_some_and(|s| s.expr.is_some())
}

/// Where a test's order puts the row it found, lower first: a first key
/// worked out of the row, as its column answers it (turned round where the
/// order is descending); under `near` the row's `_score` -- metres from a
/// point, a vector's distance -- or a sparse vector's dot product turned
/// round, the higher being nearer. A test ordered by fields ranks a row as
/// every other test of that order does: `None`, and the tie is the
/// arrival's.
fn rank(db: &Database, test: &Select, rs: &ResultSet) -> Option<f64> {
    let row = rs.rows.first()?;
    if let Some(key) = test.order.first().filter(|s| s.expr.is_some()) {
        let at = rs.columns.iter().position(|c| c == RANK)?;
        let v = match row.values.get(at)? {
            Value::Int(n) => *n as f64,
            Value::Float(x) if x.is_finite() => *x,
            Value::Timestamp(t) => *t as f64,
            _ => return None,
        };
        return Some(if key.asc { v } else { -v });
    }
    let near = test.near.as_ref()?;
    let score = f64::from(row.score?);
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
    use crate::access::Access;

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

    /// The waves are the seed's: a failure is one seed run again.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        fn chance(&mut self, percent: u64) -> bool {
            self.below(100) < percent
        }

        fn of<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[self.below(items.len() as u64) as usize]
        }
    }

    const SECRET: &[u8] = b"thirty-two bytes and a few more, for HS256";

    /// Couriers' tokens, as Plover's: a claim holds the rules the token's
    /// update and read rules make of it, `or`s and lists among them.
    const POLICY: &str = "\
orders  read                   where owner = $jwt.sub or team in $jwt.teams  for courier
orders  update(owner, run_at)  where team in $jwt.teams                      for courier
orders  update(owner, run_at)  where kind = $jwt.kind and prio >= 2          for courier
kinds   read                                                                 for courier
";

    fn exec(db: &mut Database, text: &str) {
        db.execute(&parse(text))
            .unwrap_or_else(|e| panic!("{text}: {e}"));
    }

    /// A point in a kilometre or so, three in ten on a grid of sixteen so
    /// that distances tie.
    fn point(rng: &mut Rng) -> String {
        if rng.chance(30) {
            let (x, y) = (rng.below(4) as f64, rng.below(4) as f64);
            format!("[{}, {}]", 29.0 + x * 0.002, 41.0 + y * 0.002)
        } else {
            let (x, y) = (rng.below(1000) as f64, rng.below(1000) as f64);
            format!("[{}, {}]", 29.0 + x / 100_000.0, 41.0 + y / 100_000.0)
        }
    }

    /// An order as a `put` writes it at `t`: ready `ready` times in a
    /// hundred, else delayed a wave or a few, or leased; a priority null now
    /// and then; ties of every field.
    fn order(rng: &mut Rng, t: i64, ready: u64) -> String {
        let run_at = if rng.chance(ready) {
            t - rng.below(600) as i64
        } else if rng.chance(25) {
            t + 50 + rng.below(900) as i64
        } else {
            t + 600_000
        };
        let mut fields = vec![
            format!("pickup: {}", point(rng)),
            format!("run_at: {run_at}"),
            format!("kind: \"{}\"", rng.of(&["a", "b", "c"])),
            format!("team: {}", 1 + rng.below(3)),
            format!("fee: {}.5", rng.below(9)),
            format!("owner: \"{}\"", rng.of(&["alice", "bob", ""])),
        ];
        if !rng.chance(15) {
            fields.push(format!("prio: {}", rng.below(4)));
        }
        format!("{{{}}}", fields.join(", "))
    }

    /// A courier's claim, under a token one time in three: the plan the
    /// server makes of it once its run wrote nothing.
    fn claim(rng: &mut Rng, db: &Database, access: &Access, t: i64) -> Option<Plan> {
        let mut params = vec!["\"w\"".to_string()];
        let mut bind = |v: String| {
            params.push(v);
            format!("${}", params.len())
        };
        let p = bind(point(rng));
        let mut terms = Vec::new();
        if rng.chance(85) {
            terms.push("run_at <= now()".to_string());
        }
        if rng.chance(15) {
            // Rows that stop passing as the time moves on.
            terms.push("run_at > now() - 500".to_string());
        }
        if rng.chance(35) {
            let k = bind(format!("\"{}\"", rng.of(&["a", "b", "c"])));
            terms.push(format!("kind = {k}"));
        }
        if rng.chance(40) {
            terms.push(format!("distance(pickup, {p}) <= {}", 100 + rng.below(900)));
        }
        if rng.chance(25) {
            let q = bind(rng.below(4).to_string());
            terms.push(format!("prio >= {q}"));
        }
        if rng.chance(15) {
            terms.push("(team = 1 or fee > 5.0)".to_string());
        }
        if rng.chance(10) {
            terms.push("team in [1, 3]".to_string());
        }
        if rng.chance(10) {
            terms.push("not (owner = \"bob\")".to_string());
        }
        if rng.chance(5) {
            terms.push("kind in (get kinds select name where live = true)".to_string());
        }
        if terms.is_empty() {
            terms.push("run_at <= now()".to_string());
        }
        let order = match rng.below(9) {
            0 | 1 => format!("order distance(pickup, {p})"),
            2 => format!("order distance(pickup, {p}) desc"),
            3 => "order run_at".to_string(),
            4 => "order prio desc".to_string(),
            5 => "order prio desc, run_at".to_string(),
            6 => "order prio * 2 + team".to_string(),
            7 => format!("order distance(pickup, {p}), prio desc"),
            _ => String::new(),
        };
        let returning = if rng.chance(50) { " returning id" } else { "" };
        let text = format!(
            "set orders {{owner: $1, run_at: now() + 600000}} where {} {order} limit 1{returning}",
            terms.join(" and ")
        );
        let stmt = parse(&text);
        // Read as a server reads them for a point: as written.
        let params =
            fenec_core::json::parse_params_exact(&format!("[{}]", params.join(", "))).unwrap();
        let stmt = if rng.chance(35) {
            let claims = format!(
                r#"{{"sub":"{}","role":"courier","teams":[{},{}],"kind":"{}"}}"#,
                rng.of(&["alice", "bob"]),
                1 + rng.below(3),
                1 + rng.below(3),
                rng.of(&["a", "b", "c"])
            );
            let token = access.mint(&claims).unwrap();
            // Checked at the time it was minted, the waves' clock aside.
            let secs = (now_ms() / 1000) as u64;
            let scope = access.scope(&token, secs).unwrap();
            scope.rewrite(stmt).ok()?
        } else {
            stmt
        };
        holdable(&stmt, &params).ok()?;
        plan(db, &[(&stmt, &params)], t);
        planned()
    }

    /// A wave's writes: orders placed, moved, taken back to the queue and
    /// cancelled; now and then a burst past [`AMONG`], a schema change, a
    /// compact or a write of the collection an inner `get` reads.
    fn writes(rng: &mut Rng, db: &mut Database, t: i64, epoch: &mut u64) {
        let ids: Vec<DocId> = match db.query(&parse("get orders select id"), &[]) {
            Ok(Response::Rows(rs)) => rs.rows.iter().map(|r| r.id).collect(),
            other => panic!("{other:?}"),
        };
        match rng.below(100) {
            0..=2 => {
                let burst: Vec<String> = (0..40).map(|_| order(rng, t, 60)).collect();
                exec(db, &format!("put orders [{}]", burst.join(", ")));
            }
            3..=4 => exec(
                db,
                &format!(
                    "alter collection orders add field n{} int",
                    rng.next() % 1_000_000
                ),
            ),
            5..=6 => {
                exec(db, "compact orders");
                // As the hub hears of it: a write that says not what it wrote.
                *epoch += 1;
            }
            7..=9 => exec(db, "set kinds {live: not live}"),
            _ => {
                for _ in 0..rng.below(3) {
                    exec(db, &format!("put orders {}", order(rng, t, 60)));
                }
                for _ in 0..rng.below(4) {
                    let Some(&id) = ids.get(rng.below(ids.len().max(1) as u64) as usize) else {
                        break;
                    };
                    let mut set = Vec::new();
                    if rng.chance(60) {
                        let run_at = match rng.below(3) {
                            0 => t + 600_000,
                            1 => t + 50 + rng.below(900) as i64,
                            _ => t - rng.below(300) as i64,
                        };
                        set.push(format!("run_at: {run_at}"));
                    }
                    if rng.chance(40) {
                        set.push(format!("pickup: {}", point(rng)));
                    }
                    if rng.chance(30) {
                        set.push(format!("prio: {}", rng.below(4)));
                    }
                    if rng.chance(20) {
                        set.push(format!("kind: \"{}\"", rng.of(&["a", "b", "c"])));
                    }
                    if !set.is_empty() {
                        exec(
                            db,
                            &format!("set orders {{{}}} where id = {id}", set.join(", ")),
                        );
                    }
                }
                if rng.chance(30) {
                    if let Some(&id) = ids.get(rng.below(ids.len().max(1) as u64) as usize) {
                        exec(db, &format!("del orders where id = {id}"));
                    }
                }
            }
        }
    }

    /// Each row bid for and the group that takes it, as `settle` gives it:
    /// the bid ranking it first, then the group held longest -- here the
    /// first in `bids`.
    fn winners(bids: &[Option<(Row, Option<f64>)>]) -> Vec<(DocId, usize)> {
        let mut best: Vec<(DocId, f64, usize)> = Vec::new();
        for (g, found) in bids.iter().enumerate() {
            let Some((Some((_, id)), rank)) = found else {
                continue;
            };
            let r = rank.unwrap_or(f64::INFINITY);
            match best.iter_mut().find(|b| b.0 == *id) {
                Some(b) if r.total_cmp(&b.1).is_lt() => *b = (*id, r, g),
                Some(_) => {}
                None => best.push((*id, r, g)),
            }
        }
        best.into_iter().map(|(id, _, g)| (id, g)).collect()
    }

    /// Every row a test passes, in its order: what a look's knowledge of
    /// its rows is held to.
    fn passing(db: &Database, test: &Select) -> Vec<DocId> {
        let all = Select {
            limit: None,
            ..test.clone()
        };
        match db.query(&Statement::Select(all), &[]) {
            Ok(Response::Rows(rs)) => rs.rows.iter().map(|r| r.id).collect(),
            other => panic!("{other:?}"),
        }
    }

    /// A look among the rows a group knows and those written since picks
    /// what the whole pick picks, ranked as it ranks it, so every row goes
    /// to the claim the whole picks' arbiter gives it; and what it knows
    /// next is every row its test passes, in its order. Thousands of waves
    /// of couriers' claims -- points, radii, kinds, priorities, orders by a
    /// distance, a field, an expression, ties of each, tokens' rules --
    /// beside orders placed, moved, taken by the winners and cancelled, the
    /// time moving past rows' moments, bursts, schema changes, compacts.
    #[test]
    fn a_look_among_the_rows_written_picks_as_the_whole_pick() {
        const SEEDS: u64 = 120;
        const WAVES: usize = 25;
        let access = Access::new(SECRET, POLICY).unwrap();
        let (mut among, mut whole, mut waves, mut won) = (0u64, 0u64, 0u64, 0u64);
        for seed in 1..=SEEDS {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut db = Database::new();
            let geo = if rng.chance(50) { " @geo" } else { "" };
            let sorted = if rng.chance(70) { " @sorted" } else { "" };
            let hash = if rng.chance(40) { " @hash" } else { "" };
            exec(
                &mut db,
                &format!(
                    "create collection orders (pickup geo{geo}, run_at timestamp{sorted}, \
                     kind text{hash}, prio int, team int, fee float, owner text)"
                ),
            );
            exec(&mut db, "create collection kinds (name text, live bool)");
            exec(
                &mut db,
                "put kinds [{name: \"a\", live: true}, {name: \"b\", live: false}]",
            );
            let mut t = 1_800_000_000_000i64;
            db.set_clock(Some(t));
            for _ in 0..rng.below(25) {
                exec(&mut db, &format!("put orders {}", order(&mut rng, t, 15)));
            }
            let mut epoch = 0;
            let mut groups = Vec::new();
            for _ in 0..2 + rng.below(12) {
                if let Some(plan) = claim(&mut rng, &db, &access, t) {
                    let known: Option<Known> = None;
                    groups.push((Arc::new(plan.tests), Arc::new(plan.collections), known));
                }
            }
            for _ in 0..WAVES {
                waves += 1;
                // The time moves on, a few milliseconds a wave and past a
                // row's moment now and then, and once in a while back.
                t += match rng.below(100) {
                    0..=1 => -400,
                    2..=11 => 200 + rng.below(800) as i64,
                    _ => rng.below(20) as i64,
                };
                db.set_clock(Some(t));
                writes(&mut rng, &mut db, t, &mut epoch);
                // Rounds as the arbiter's: every claim looks, each row goes
                // to the claim ranking it first, the winners' runs take
                // theirs, and the rest look again -- until none finds one.
                for _ in 0..64 {
                    let mut looks = Looks::new(&db, epoch, t);
                    let (mut by_among, mut by_whole) = (Vec::new(), Vec::new());
                    for (i, (tests, collections, known)) in groups.iter_mut().enumerate() {
                        let job = |known| Job {
                            id: i as u64,
                            since: 0,
                            tests: Arc::clone(tests),
                            collections: Arc::clone(collections),
                            due: true,
                            known,
                        };
                        let full = looks.whole(&job(None));
                        by_whole.push(full.found.clone());
                        let look = match looks.among(&job(known.take())) {
                            Some(look) => {
                                among += 1;
                                let what = || format!("seed {seed}, claim {i}: {:?}", tests[0]);
                                assert_eq!(look.found, full.found, "{}", what());
                                // What it knows next: every row its test
                                // passes, in its order.
                                if let Some(k) = &look.known {
                                    assert_eq!(k.ids, passing(&db, &tests[0]), "{}", what());
                                }
                                look
                            }
                            None => {
                                whole += 1;
                                full
                            }
                        };
                        by_among.push(look.found);
                        *known = look.known;
                    }
                    let given = winners(&by_whole);
                    assert_eq!(winners(&by_among), given, "seed {seed}");
                    drop(looks);
                    if given.is_empty() {
                        break;
                    }
                    // Each winner's run takes its row out of every claim's
                    // reach -- a claim of no time but its point's would take
                    // a leased one again -- the next round's looks among it.
                    for (id, _) in given {
                        won += 1;
                        exec(&mut db, &format!("del orders where id = {id}"));
                    }
                }
            }
        }
        eprintln!(
            "{waves} waves: {among} looks among the rows written, {whole} whole picks, {won} rows won"
        );
        // The looks among the rows were held, not the fallback alone.
        assert!(among > 2 * whole, "{among} among, {whole} whole");
    }
}
