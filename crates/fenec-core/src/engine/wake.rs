//! When a filter's answer can next change with no write: the moment a
//! server wakes a write it holds (`Fenec-Wait`, fenec-http's `waits`).
//!
//! A job queue's claim, `set jobs {..} where run_at <= now() order run_at
//! limit 10`, that finds no job ready has two ways to find one later: a
//! write -- a job enqueued, one failed back with its backoff -- which the
//! server hears, and the time reaching a delayed job's `run_at` or a lease's
//! end, which nothing announces. A server that re-ran held claims every so
//! often to catch the second would spin, or be late by its period. So the
//! time is worked out instead: a comparison of a field with `now()` give or
//! take a number changes its answer for a row only as `now()` passes that
//! row's value, and the next such moment is the least value past the bound
//! -- one step of an `@sorted` index, a scan without one. Under `and`, `or`
//! or `not`, a filter whose time is read only so changes only at one of
//! those moments, so the least of them is when to look again; a time read
//! any other way -- `bucket(now(), 1h)`, `now()` beside a field's
//! arithmetic -- is refused, since nothing says when it changes.
//!
//! Every row's value counts, whatever else the filter asks of it: a claim
//! of one queue among several in a collection is woken by another queue's
//! delayed jobs too, early and never late, to find nothing and sleep to
//! the next. Counting only the rows the other `and`s let through was
//! exact, and walked the index past every row they refused: a claim held
//! on a queue no job was in scanned the 200 000 jobs beside it at every
//! look, and the busy workers beside it went 144 000 jobs a second to
//! 20 200.
//!
//! Native only: a browser module's queue has its one page to drain it, and
//! no thread to hold.

use super::*;

impl Database {
    /// The earliest moment past `at` -- milliseconds, on the clock `now()`
    /// reads -- at which `filter` over `collection` could answer a row
    /// differently than it did at `at` with no write in between, or `None`
    /// when no passing of time changes it. `at` is when the statement was
    /// last run: a row whose moment came between that run and this call is
    /// answered as due, at its moment, rather than passed over.
    ///
    /// The moments counted: each comparison of a field (or a path) with
    /// `now()`, plus or minus integers and parameters, anywhere in the
    /// filter, as `now()` passes each row's value; a row of a collection
    /// whose rows expire (`@ttl`) reaching its time, which `expired()` and
    /// every read's test of a living row turn on; and the same in each
    /// inner `get`. Every living row's value counts, which may wake a
    /// server early and never late.
    pub fn wakes_at(
        &self,
        collection: &str,
        filter: Option<&Expr>,
        params: &[Value],
        at: i64,
    ) -> Result<Option<i64>> {
        let mut next = None;
        self.moments(collection, filter, params, at, &mut next)?;
        Ok(next)
    }

    fn moments(
        &self,
        collection: &str,
        filter: Option<&Expr>,
        params: &[Value],
        at: i64,
        next: &mut Option<i64>,
    ) -> Result<()> {
        // A row lives until its field and the ttl: the next to go is the
        // least value past `at - ttl`.
        if let Some((field, ttl)) = self.ttl_of(collection) {
            let ttl = ttl.min(i64::MAX as u64) as i64;
            let probe = Probe {
                field,
                op: CmpOp::Gt,
                bound: at.saturating_sub(ttl),
            };
            if let Some(v) = self.next_value(collection, &probe, params)? {
                keep(next, v.saturating_add(ttl));
            }
        }
        let Some(filter) = filter else {
            return Ok(());
        };
        Walk {
            db: Some(self),
            collection,
            params,
            at,
            next,
        }
        .expr(filter)
    }

    /// The least value of `probe.field` past its bound among the living
    /// rows, as milliseconds: `get <c> select f where f > bound order f
    /// limit 1`, which an `@sorted` field answers with a step of its index.
    /// A value that is no time or number has no moment.
    fn next_value(&self, collection: &str, probe: &Probe, params: &[Value]) -> Result<Option<i64>> {
        let filter = Expr::Cmp(
            probe.op,
            Box::new(Expr::Field(probe.field.to_string())),
            Box::new(Expr::Lit(Value::Timestamp(probe.bound))),
        );
        let sel = Select {
            collection: collection.to_string(),
            project: Some(vec![probe.field.to_string()]),
            filter: Some(filter),
            order: vec![Sort::new(probe.field, true)],
            limit: Some(1),
            ..Select::default()
        };
        let Response::Rows(rs) = self.query(&Statement::Select(sel), params)? else {
            return Ok(None);
        };
        Ok(rs
            .rows
            .first()
            .and_then(|r| r.values.first())
            .and_then(|v| match v {
                Value::Timestamp(t) | Value::Int(t) => Some(*t),
                Value::Float(x) if x.is_finite() => Some(x.ceil() as i64),
                _ => None,
            }))
    }
}

/// A field's values past a bound: those `>` it, or `>=`.
struct Probe<'a> {
    field: &'a str,
    op: CmpOp,
    bound: i64,
}

fn keep(next: &mut Option<i64>, t: i64) {
    *next = Some(next.map_or(t, |n| n.min(t)));
}

/// `now()`, plus or minus integers and parameters, as its offset from the
/// time: `now() + 30000` is 30000, `now() - $1` minus the first parameter.
fn now_offset(e: &Expr, params: &[Value]) -> Option<i64> {
    let int = |e: &Expr| match e {
        Expr::Lit(Value::Int(n)) => Some(*n),
        Expr::Param(i) => match params.get(*i) {
            Some(Value::Int(n)) => Some(*n),
            _ => None,
        },
        _ => None,
    };
    match e {
        Expr::Call(name, args) if args.is_empty() && name.eq_ignore_ascii_case("now") => Some(0),
        Expr::Arith(ArithOp::Add, a, b) => match (now_offset(a, params), int(b)) {
            (Some(k), Some(n)) => k.checked_add(n),
            _ => now_offset(b, params)?.checked_add(int(a)?),
        },
        Expr::Arith(ArithOp::Sub, a, b) => now_offset(a, params)?.checked_sub(int(b)?),
        _ => None,
    }
}

/// Whether a filter reads the time only as [`Database::wakes_at`] can
/// place it, asked before a write is held: one that cannot be placed is
/// refused whatever its first run writes.
pub fn placeable(filter: Option<&Expr>) -> Result<()> {
    let Some(f) = filter else {
        return Ok(());
    };
    let mut next = None;
    Walk {
        db: None,
        collection: "",
        params: &[],
        at: 0,
        next: &mut next,
    }
    .expr(f)
}

struct Walk<'w> {
    /// `None` asks only whether the time can be placed.
    db: Option<&'w Database>,
    collection: &'w str,
    params: &'w [Value],
    at: i64,
    next: &'w mut Option<i64>,
}

impl Walk<'_> {
    fn expr(&mut self, e: &Expr) -> Result<()> {
        if let Expr::Cmp(op, a, b) = e {
            // `field op now() + k`, or the time on the left and the
            // operator turned round to put it on the right.
            let placed = match (&**a, &**b) {
                (Expr::Field(f), t) => now_offset(t, self.params).map(|k| (f, *op, k)),
                (t, Expr::Field(f)) => now_offset(t, self.params).map(|k| {
                    let turned = match op {
                        CmpOp::Lt => CmpOp::Gt,
                        CmpOp::Le => CmpOp::Ge,
                        CmpOp::Gt => CmpOp::Lt,
                        CmpOp::Ge => CmpOp::Le,
                        same => *same,
                    };
                    (f, turned, k)
                }),
                _ => None,
            };
            if let Some((field, op, k)) = placed {
                return self.compared(field, op, k);
            }
        }
        if let Expr::Call(name, args) = e {
            if args.is_empty() && name.eq_ignore_ascii_case("now") {
                return Err(unplaced());
            }
        }
        if let Expr::InSelect(lhs, inner) = e {
            self.expr(lhs)?;
            return match self.db {
                Some(db) => db.moments(
                    &inner.collection,
                    inner.filter.as_ref(),
                    self.params,
                    self.at,
                    self.next,
                ),
                None => placeable(inner.filter.as_ref()),
            };
        }
        let mut failed = Ok(());
        e.each_child(&mut |c| {
            if failed.is_ok() {
                failed = self.expr(c);
            }
        });
        failed
    }

    /// `field op now() + k`: its answer for a row with value `v` turns as
    /// the time passes `v - k` -- `<=` and `>` at it, `<` and `>=` a
    /// millisecond after. The rows past the bound `at + k` are those still
    /// to turn, and the least of them turns next.
    fn compared(&mut self, field: &str, op: CmpOp, k: i64) -> Result<()> {
        let (past, after) = match op {
            CmpOp::Le | CmpOp::Gt => (CmpOp::Gt, 0),
            CmpOp::Lt | CmpOp::Ge => (CmpOp::Ge, 1),
            // An instant matched to the millisecond, or missed by it: no
            // queue waits on one, and its two moments a row are not worth
            // the code.
            CmpOp::Eq | CmpOp::Ne => return Err(unplaced()),
        };
        let Some(db) = self.db else {
            return Ok(());
        };
        let probe = Probe {
            field,
            op: past,
            bound: self.at.saturating_add(k),
        };
        if let Some(v) = db.next_value(self.collection, &probe, self.params)? {
            keep(self.next, v.saturating_sub(k).saturating_add(after));
        }
        Ok(())
    }
}

fn unplaced() -> Error {
    Error::Query(
        "a held write reads the time only as a field compared with now(), give or take a \
         number: when else its answer changes is not known"
            .into(),
    )
}
