//! A long read answered beside the writers: what it reads pinned under the
//! read lock, and read once the lock is let go.
//!
//! A read takes the shared lock and a write the exclusive one, so a long
//! read held every writer for as long as it ran: a ledger's reconciliation
//! (a read-only `/batch` of three aggregates over a million entries,
//! 140-200 ms) held transfers up to 187-512 ms, 36 of them past 50 ms in
//! ten seconds (`make recon-bench`), and an aggregate of 50 000 groups
//! over a million events held every write -- and every read by id behind a
//! waiting write -- for its 0.8 s (`make concurrency-bench`). SQLite's WAL
//! reads a snapshot beside its writer.
//!
//! [`Database::pin`] takes, under the read lock, what a read of documents
//! alone needs -- each collection's schema and store, the store's segments
//! and mapped file shared and its id index copied, 12 bytes a row -- into a
//! database of its own, and the read runs on that with no lock held: the
//! database as it stood at the pin, whatever lands after it. Nothing a
//! write changes in place is shared: a write appending to a segment the pin
//! holds copies it first (`Arc::make_mut`), and a handover or a compact
//! that lets go of segments or of a file lets go of its own hold only.
//!
//! The indexes are not taken. A hash index's map, an ordered index's
//! chunks and a text index's postings are changed in place by every write;
//! a copy of one under the lock costs as much as the read it spares, and
//! one shared, copied by the first write to touch it, made that write wait
//! as long instead. A read an index answers is short besides, and is
//! answered under the lock as before: [`Database::pin`] declines it from
//! its shape, and one let through by mistake meets an index that refuses
//! ([`PINNED_OUT`]), which [`Pinned`] answers as "run it under the lock".
//! So a pinned read answers what the read under the lock at the pin would
//! have, or nothing.
//!
//! Native only: the browser module has one thread, and nothing to pin
//! beside.

use super::*;
use std::sync::atomic::AtomicUsize;

/// The rows a read reads at least for a pin to be worth it: the pin copies
/// the id index of what it reads, 12 bytes a row -- 0.26 to 0.87 ms for a
/// million, where a scan of them took 61 -- and a read of fewer than this
/// many holds a writer for under a millisecond.
pub const PIN_AT: usize = 10_000;

/// The bytes the pins of one database hold together at most: a pin past it
/// is declined, and the read runs under the lock. Each holds its copy of
/// the id indexes until it ends, 12 MB for a million rows.
pub const PIN_BUDGET: usize = 256 << 20;

/// What an index of a pinned collection answers: none is taken.
const PINNED_OUT: &str = "an index a pinned read does not hold";

/// What a long read runs on beside the writers ([`Database::pin`]): the
/// collections the statements read, as they stood at the pin. Run on it
/// the statements `pin` was handed, and only those; each answers `None`
/// where it reached for an index the pin left out, and the statements are
/// then run again together under the read lock.
pub struct Pinned {
    db: Database,
    seq: u64,
    held: usize,
    pins: Arc<AtomicUsize>,
}

impl Drop for Pinned {
    fn drop(&mut self) {
        self.pins.fetch_sub(self.held, Relaxed);
    }
}

/// `r`, or `None` where it is a pinned read's refusal to reach past what
/// it holds.
fn held<T>(r: Result<T>) -> Option<Result<T>> {
    match r {
        Err(e) if e.to_string().contains(PINNED_OUT) => None,
        r => Some(r),
    }
}

impl Pinned {
    /// The change the database stood at when it was pinned, which every
    /// answer of it is at: a read-only batch's `Fenec-Seq`.
    pub fn change_seq(&self) -> u64 {
        self.seq
    }

    /// The bytes it holds: the id indexes it copied.
    pub fn held_bytes(&self) -> usize {
        self.held
    }

    /// [`Database::query`] as at the pin; `None` where it would use an
    /// index, for the caller to run it under the lock.
    pub fn query(&self, stmt: &Statement, params: &[Value]) -> Option<Result<Response>> {
        held(self.db.query(stmt, params))
    }

    /// [`Database::query_json`] as at the pin, `None` as [`Self::query`].
    pub fn query_json(
        &self,
        stmt: &Statement,
        params: &[Value],
        out: &mut String,
    ) -> Option<Result<Option<usize>>> {
        let at = out.len();
        let r = held(self.db.query_json(stmt, params, out));
        if r.is_none() {
            out.truncate(at);
        }
        r
    }
}

impl Database {
    /// What `stmts` -- one read, or a read-only batch read as one -- read,
    /// pinned as it stands, for them to run on once the lock is let go: a
    /// writer waits for this alone rather than for the read. `None` where
    /// they are better run under the lock: a statement that is not a `get`,
    /// or that an index answers (its filter's equalities and ranges, an
    /// ordered walk, a facet a hash index counts, `match`, `near`,
    /// `lookup`) -- short, and an index cannot be pinned for nothing -- or
    /// none of them reading [`PIN_AT`] rows, or the pins past their budget.
    pub fn pin(&self, stmts: &[(&Statement, &[Value])]) -> Option<Pinned> {
        let mut long = false;
        let alone = stmts.len() == 1;
        for (stmt, params) in stmts {
            let Statement::Select(sel) = stmt else {
                return None;
            };
            long |= self.pinnable(sel, params, 0, alone)?;
        }
        if !long {
            return None;
        }
        let mut names: Vec<&str> = Vec::new();
        for (stmt, _) in stmts {
            if let Statement::Select(sel) = stmt {
                reads(sel, &mut names);
            }
        }
        let held: usize = (names.iter())
            .filter_map(|n| self.collections.get(*n))
            .map(|c| c.store.pinned_bytes())
            .sum();
        if self.pins.fetch_add(held, Relaxed) + held > self.pin_budget {
            self.pins.fetch_sub(held, Relaxed);
            return None;
        }
        Some(self.pinned_of(&names, held))
    }

    /// The rows a read reads at least for [`Self::pin`] to take it: 0 pins
    /// every read it can answer, as a test does to run them all that way.
    pub fn set_pin_at(&mut self, rows: usize) {
        self.pin_at = rows;
    }

    /// The bytes this database's pins may hold together.
    pub fn set_pin_budget(&mut self, bytes: usize) {
        self.pin_budget = bytes;
    }

    /// Every collection pinned, whatever would read it: for a test to run
    /// on a pin a read [`Self::pin`] declines, and see it answered `None`.
    #[doc(hidden)]
    pub fn pin_every_collection(&self) -> Pinned {
        let names: Vec<&str> = self.order.iter().map(String::as_str).collect();
        self.pinned_of(&names, 0)
    }

    /// `names` pinned, `held` bytes of the budget taken for them.
    fn pinned_of(&self, names: &[&str], held: usize) -> Pinned {
        let mut db = Database::new();
        db.registry = self.registry.clone();
        db.clock = self.clock;
        db.mapped = self.mapped;
        for name in &self.order {
            if let (true, Some(c)) = (names.contains(&name.as_str()), self.collections.get(name)) {
                db.collections.insert(name.clone(), c.pinned());
                db.order.push(name.clone());
            }
        }
        Pinned {
            db,
            seq: self.change_seq(),
            held,
            pins: Arc::clone(&self.pins),
        }
    }

    /// Whether `sel` reads [`Self::pin_at`] rows or more -- `None` where a
    /// pin cannot answer it, or where an index would. Asked of every read a
    /// server answers, so a statement `alone` that is short by its shape
    /// and its collection's size -- a page, a read by id -- is let go
    /// before its filter is walked, with nothing allocated; one in a batch
    /// is walked, since the batch may be pinned for another.
    fn pinnable(&self, sel: &Select, params: &[Value], depth: usize, alone: bool) -> Option<bool> {
        if sel.near.is_some()
            || sel.matcher.is_some()
            || sel.fuse.is_some()
            || sel.rerank.is_some()
            || !sel.marks.is_empty()
            || sel.lookup.is_some()
            || depth > MAX_SUBQUERY_DEPTH
        {
            return None;
        }
        let c = self.collections.get(&sel.collection)?;
        let hashed = |f: &str| c.hashes.contains_key(f);
        let ordered = |f: &str| c.sorted.iter().any(|(n, _)| n == f);
        // An ordered walk (`walk_order`), and a facet the hash index's
        // buckets count or the ordered index's ranges.
        if let [s] = sel.order.as_slice() {
            if ordered(&s.field) {
                return None;
            }
        }
        if (sel.facets.iter())
            .any(|f| hashed(&f.field) || (f.ranges.is_some() && ordered(&f.field)))
        {
            return None;
        }
        let page = |l: usize| l.saturating_add(sel.offset) >= self.pin_at;
        let mut rows = !sel.aggregate.is_empty()
            || !sel.group.is_empty()
            || !sel.facets.is_empty()
            || !sel.order.is_empty()
            || match &sel.filter {
                // Counted without one, the ids are listed and nothing read.
                None => !sel.count && sel.limit.is_none_or(page),
                Some(_) => sel.count || sel.limit.is_none_or(page),
            };
        let big = c.store.len() >= self.pin_at;
        if alone && !(rows && big) && !sel.has_subquery() {
            return Some(false);
        }
        let mut long = false;
        let filters =
            std::iter::once(&sel.filter).chain((sel.facets.iter()).filter_map(|f| f.rest.as_ref()));
        for f in filters.flatten() {
            // A comparison with `now()` is a range once the time is worked
            // out, as the read will work it out (`answer_filter`): judged
            // as written, `at >= now() - 3600000` was pinned, met the index
            // the read then took, and ran again under the lock.
            let folded;
            let f = match f.calls_now().then(|| self.now_value(params)).flatten() {
                Some(now) => {
                    let mut g = f.clone();
                    g.fold_now(&now, params);
                    folded = g;
                    &folded
                }
                None => f,
            };
            // A row by id, or a few: what the id index names, and short --
            // a server's read by id is asked no more than this.
            if let Some(("id", _)) = f.equality_key(params) {
                rows = false;
                continue;
            }
            if f.asks_expired() {
                return None;
            }
            let mut eqs = Vec::new();
            f.conjunct_equalities(params, &mut eqs);
            let mut ins = Vec::new();
            f.conjunct_in_sets(params, &mut ins);
            let named = eqs.iter().map(|e| e.0).chain(ins.iter().map(|i| i.0));
            for field in named {
                match field {
                    "id" => rows = false,
                    f if hashed(f) => return None,
                    _ => {}
                }
            }
            let mut ranges = Vec::new();
            f.conjunct_ranges(params, &mut ranges);
            if ranges.iter().any(|r| ordered(r.0)) {
                return None;
            }
            // Each inner `get` is answered first, as an `in` list the hash
            // index on its left side would answer, and reads its own
            // collection.
            let mut inner = Vec::new();
            inner_gets(f, &mut inner);
            for (lhs, s) in inner {
                if lhs.is_some_and(hashed) {
                    return None;
                }
                long |= self.pinnable(s, params, depth + 1, false)?;
            }
        }
        Some(long || (rows && big))
    }
}

impl Collection {
    /// The collection as it stands, its documents alone: the schema, and
    /// the store as [`Store::pinned`] takes it. Every index it declares is
    /// there by name, refusing ([`PINNED_OUT`]), so a read that would use
    /// one fails rather than plans without it; the graphs are left out,
    /// which only `near` reads, and `pin` declines.
    fn pinned(&self) -> Collection {
        fn out<T>(names: impl Iterator<Item = String>) -> Vec<(String, Derived<T>)> {
            let refused = || {
                Derived(std::sync::OnceLock::from(Err(Error::Query(
                    PINNED_OUT.into(),
                ))))
            };
            names.map(|n| (n, refused())).collect()
        }
        Collection {
            id: self.id,
            schema: self.schema.clone(),
            store: self.store.pinned(),
            vectors: Fields::default(),
            hashes: Fields(out(self.hashes.keys().cloned())),
            texts: Fields(out(self.texts.keys().cloned())),
            sorted: out(self.sorted.iter().map(|(n, _)| n.clone())),
            sparse: out(self.sparse.iter().map(|(n, _)| n.clone())),
        }
    }
}

/// The collections `sel` reads, its inner `get`s' among them, into `names`.
fn reads<'s>(sel: &'s Select, names: &mut Vec<&'s str>) {
    if !names.contains(&sel.collection.as_str()) {
        names.push(&sel.collection);
    }
    let mut inner = Vec::new();
    let rests = sel.facets.iter().filter_map(|f| f.rest.as_ref());
    for f in std::iter::once(&sel.filter).chain(rests).flatten() {
        inner_gets(f, &mut inner);
    }
    for (_, s) in inner {
        reads(s, names);
    }
}

/// The inner `get`s of `e`, outermost, each with the field on its left
/// when it is one.
fn inner_gets<'e>(e: &'e Expr, out: &mut Vec<(Option<&'e str>, &'e Select)>) {
    if let Expr::InSelect(lhs, inner) = e {
        let field = match &**lhs {
            Expr::Field(f) => Some(f.as_str()),
            _ => None,
        };
        out.push((field, inner));
    }
    e.each_child(&mut |c| inner_gets(c, out));
}
