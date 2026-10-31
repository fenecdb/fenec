//! Query plan types and expression evaluation.
//!
//! This module is independent of FenecQL: plan structures can also be built
//! straight from Rust (embedded use); FenecQL is just a front end producing them.

use crate::collate::{Collation, TextOrder};
use crate::error::{Error, Result};
use crate::schema::Schema;
use crate::value::{DocId, Value};
use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// `+`, `-`, `*`, `/` between two values ([`arith`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
}

impl ArithOp {
    pub fn symbol(self) -> &'static str {
        match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Field reference. `id` specifically yields the document id.
    Field(String),
    Lit(Value),
    /// Bound parameter: `$1`, `$2`...
    Param(usize),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    /// Text contains: `title ~ "rust"` (case insensitive)
    Like(Box<Expr>, Box<Expr>),
    /// List contains an element: `tags has "ai"`
    Has(Box<Expr>, Box<Expr>),
    In(Box<Expr>, Vec<Expr>),
    /// `customer in (get customers select id where country = "TR")`: the
    /// inner `get` runs once, before the query, and its one column becomes
    /// the list of an [`Expr::In`] (`Database::query` answers it so before
    /// anything else runs); `eval` never meets one.
    InSelect(Box<Expr>, Box<Select>),
    IsNull(Box<Expr>),
    /// Plugin or builtin function call: `cosine(embed, $1)`
    Call(String, Vec<Expr>),
    /// `n + 1`, `price * $1`: arithmetic over numbers, and a timestamp
    /// moved by milliseconds ([`arith`]).
    Arith(ArithOp, Box<Expr>, Box<Expr>),
}

impl Expr {
    /// A copy, made by this crate's `clone`: the derive's is `#[inline]`,
    /// so another crate calling it compiled a copy of its own -- fenec-ql's
    /// was 2.2 KB of the browser module.
    #[inline(never)]
    pub fn duplicate(&self) -> Expr {
        self.clone()
    }

    /// Collects the field names used in the expression (for the planner).
    pub fn referenced_fields(&self, out: &mut Vec<String>) {
        match self {
            Expr::Field(f) => {
                if !out.contains(f) {
                    out.push(f.clone());
                }
            }
            Expr::Lit(_) | Expr::Param(_) => {}
            Expr::And(a, b) | Expr::Or(a, b) => {
                a.referenced_fields(out);
                b.referenced_fields(out);
            }
            Expr::Not(a) | Expr::IsNull(a) => a.referenced_fields(out),
            Expr::Cmp(_, a, b) | Expr::Like(a, b) | Expr::Has(a, b) | Expr::Arith(_, a, b) => {
                a.referenced_fields(out);
                b.referenced_fields(out);
            }
            Expr::In(a, items) => {
                a.referenced_fields(out);
                for i in items {
                    i.referenced_fields(out);
                }
            }
            // The inner `get`'s fields are its own collection's.
            Expr::InSelect(a, _) => a.referenced_fields(out),
            Expr::Call(_, args) => {
                for a in args {
                    a.referenced_fields(out);
                }
            }
        }
    }

    /// Calls `f` on each expression directly under this one -- the
    /// operands, a call's arguments, an `in`'s left side and list -- and
    /// not on an inner `get`, whose fields are its own collection's. One
    /// walk, through `dyn`, for every question asked of a select list's
    /// items: a generic one is a copy a caller in the browser module.
    pub fn each_child<'a>(&'a self, f: &mut dyn FnMut(&'a Expr)) {
        match self {
            Expr::Field(_) | Expr::Lit(_) | Expr::Param(_) => {}
            Expr::And(a, b)
            | Expr::Or(a, b)
            | Expr::Cmp(_, a, b)
            | Expr::Like(a, b)
            | Expr::Has(a, b)
            | Expr::Arith(_, a, b) => {
                f(a);
                f(b);
            }
            Expr::Not(a) | Expr::IsNull(a) | Expr::InSelect(a, _) => f(a),
            Expr::In(a, items) => {
                f(a);
                items.iter().for_each(f);
            }
            Expr::Call(_, args) => args.iter().for_each(f),
        }
    }

    /// [`Expr::each_child`] over the children to change.
    pub fn each_child_mut(&mut self, f: &mut dyn FnMut(&mut Expr)) {
        match self {
            Expr::Field(_) | Expr::Lit(_) | Expr::Param(_) => {}
            Expr::And(a, b)
            | Expr::Or(a, b)
            | Expr::Cmp(_, a, b)
            | Expr::Like(a, b)
            | Expr::Has(a, b)
            | Expr::Arith(_, a, b) => {
                f(a);
                f(b);
            }
            Expr::Not(a) | Expr::IsNull(a) | Expr::InSelect(a, _) => f(a),
            Expr::In(a, items) => {
                f(a);
                items.iter_mut().for_each(f);
            }
            Expr::Call(_, args) => args.iter_mut().for_each(f),
        }
    }

    /// Changes it in place, outermost first: `f` puts what it answers for
    /// in its place and says so, and the rest is walked into. How a select
    /// list's items are bound once a query, over a copy: rebuilding the
    /// tree a variant at a time was 2.1 KB of the browser module.
    pub fn rewrite(&mut self, f: &mut dyn FnMut(&mut Expr) -> Result<bool>) -> Result<()> {
        if f(self)? {
            return Ok(());
        }
        let mut r = Ok(());
        self.each_child_mut(&mut |c| {
            if r.is_ok() {
                r = c.rewrite(f);
            }
        });
        r
    }

    /// The aggregate it calls at its top, if it is one -- `count`, `sum`,
    /// `avg`, `min`, `max`, `first` or `last` -- by the lowercased name the
    /// parser gives every call.
    pub fn aggregate_name(&self) -> Option<&str> {
        match self {
            Expr::Call(name, _) if AGGREGATES.contains(&name.as_str()) => Some(name),
            _ => None,
        }
    }

    /// Whether an aggregate is anywhere in it: what makes a select list
    /// fold its rows.
    pub fn has_aggregate(&self) -> bool {
        if self.aggregate_name().is_some() {
            return true;
        }
        let mut any = false;
        self.each_child(&mut |c| any = any || c.has_aggregate());
        any
    }

    /// Whether an `in (get ...)` is anywhere in it.
    pub fn has_subquery(&self) -> bool {
        match self {
            Expr::InSelect(..) => true,
            Expr::Field(_) | Expr::Lit(_) | Expr::Param(_) => false,
            Expr::And(a, b)
            | Expr::Or(a, b)
            | Expr::Cmp(_, a, b)
            | Expr::Like(a, b)
            | Expr::Has(a, b)
            | Expr::Arith(_, a, b) => a.has_subquery() || b.has_subquery(),
            Expr::Not(a) | Expr::IsNull(a) => a.has_subquery(),
            Expr::In(a, items) => a.has_subquery() || items.iter().any(Expr::has_subquery),
            Expr::Call(_, args) => args.iter().any(Expr::has_subquery),
        }
    }

    /// Whether it calls `now()`, outside an inner `get`: a filter that does
    /// has the time worked out before it is planned ([`Self::fold_now`]).
    /// A match of its own rather than through `each_child`: asked of every
    /// write's filter, the walk through `dyn` was most of a lock's release
    /// coming out 2-4% slower.
    pub fn calls_now(&self) -> bool {
        match self {
            Expr::Call(name, args) => {
                (args.is_empty() && name.eq_ignore_ascii_case("now"))
                    || args.iter().any(Expr::calls_now)
            }
            Expr::Field(_) | Expr::Lit(_) | Expr::Param(_) => false,
            Expr::And(a, b)
            | Expr::Or(a, b)
            | Expr::Cmp(_, a, b)
            | Expr::Like(a, b)
            | Expr::Has(a, b)
            | Expr::Arith(_, a, b) => a.calls_now() || b.calls_now(),
            Expr::Not(a) | Expr::IsNull(a) | Expr::InSelect(a, _) => a.calls_now(),
            Expr::In(a, items) => a.calls_now() || items.iter().any(Expr::calls_now),
        }
    }

    /// Each `now()` in it, outside an inner `get`, made the time `now`,
    /// and each `+ - * /` over what is then literals and parameters alone
    /// worked out: so `run_at <= now()` and `at >= now() - 3600000` are
    /// ranges an ordered index answers, as `at >= $1` is. Left a call, a
    /// job queue's claim (`where run_at <= now() order run_at limit 10`)
    /// walked the index past the ready jobs into the leased and the
    /// delayed ones, testing each, and once the queue held fewer ready
    /// than the page it gave up past an eighth of the collection and
    /// scanned it. What does not work out -- a type error -- stays as
    /// written, for the first row to meet as it did.
    pub fn fold_now(&mut self, now: &Value, params: &[Value]) {
        if let Expr::Call(name, args) = self {
            if args.is_empty() && name.eq_ignore_ascii_case("now") {
                *self = Expr::Lit(now.clone());
                return;
            }
        }
        self.each_child_mut(&mut |c| c.fold_now(now, params));
        if let Expr::Arith(op, a, b) = self {
            fn side<'a>(e: &'a Expr, params: &'a [Value]) -> Option<&'a Value> {
                match e {
                    Expr::Lit(v) => Some(v),
                    Expr::Param(i) => params.get(*i),
                    _ => None,
                }
            }
            if let (Some(l), Some(r)) = (side(a, params), side(b, params)) {
                if let Ok(v) = arith(*op, l, r) {
                    *self = Expr::Lit(v);
                }
            }
        }
    }

    /// Whether it asks `expired()` -- the rows past their `@ttl`, which
    /// every read otherwise leaves out -- outside an inner `get`, whose
    /// collection is its own to ask of.
    pub fn asks_expired(&self) -> bool {
        match self {
            Expr::Call(name, args) => {
                (args.is_empty() && name.eq_ignore_ascii_case(EXPIRED))
                    || args.iter().any(Expr::asks_expired)
            }
            Expr::Field(_) | Expr::Lit(_) | Expr::Param(_) => false,
            Expr::And(a, b)
            | Expr::Or(a, b)
            | Expr::Cmp(_, a, b)
            | Expr::Like(a, b)
            | Expr::Has(a, b)
            | Expr::Arith(_, a, b) => a.asks_expired() || b.asks_expired(),
            Expr::Not(a) | Expr::IsNull(a) | Expr::InSelect(a, _) => a.asks_expired(),
            Expr::In(a, items) => a.asks_expired() || items.iter().any(Expr::asks_expired),
        }
    }

    /// Each `expired()` in it, outside an inner `get`, made `past`: the
    /// test of a row past its time. Whether it held one.
    pub fn answer_expired(&mut self, past: &Expr) -> bool {
        match self {
            Expr::Call(name, args) if args.is_empty() && name.eq_ignore_ascii_case(EXPIRED) => {
                *self = past.clone();
                true
            }
            Expr::Call(_, args) => args
                .iter_mut()
                .fold(false, |any, a| a.answer_expired(past) | any),
            Expr::Field(_) | Expr::Lit(_) | Expr::Param(_) => false,
            Expr::And(a, b)
            | Expr::Or(a, b)
            | Expr::Cmp(_, a, b)
            | Expr::Like(a, b)
            | Expr::Has(a, b)
            | Expr::Arith(_, a, b) => a.answer_expired(past) | b.answer_expired(past),
            Expr::Not(a) | Expr::IsNull(a) | Expr::InSelect(a, _) => a.answer_expired(past),
            Expr::In(a, items) => items.iter_mut().fold(a.answer_expired(past), |any, i| {
                i.answer_expired(past) | any
            }),
        }
    }

    /// Calls `f` on each `in (get ...)` in it, outermost first and not
    /// inside one another: the inner `get`s are `f`'s to walk.
    pub fn each_subquery_mut(&mut self, f: &mut dyn FnMut(&mut Expr) -> Result<()>) -> Result<()> {
        match self {
            Expr::InSelect(..) => f(self),
            Expr::Field(_) | Expr::Lit(_) | Expr::Param(_) => Ok(()),
            Expr::And(a, b)
            | Expr::Or(a, b)
            | Expr::Cmp(_, a, b)
            | Expr::Like(a, b)
            | Expr::Has(a, b)
            | Expr::Arith(_, a, b) => {
                a.each_subquery_mut(f)?;
                b.each_subquery_mut(f)
            }
            Expr::Not(a) | Expr::IsNull(a) => a.each_subquery_mut(f),
            Expr::In(a, items) => {
                a.each_subquery_mut(f)?;
                for i in items {
                    i.each_subquery_mut(f)?;
                }
                Ok(())
            }
            Expr::Call(_, args) => {
                for a in args {
                    a.each_subquery_mut(f)?;
                }
                Ok(())
            }
        }
    }

    /// Highest parameter number used in the expression (`$3` -> 3), else 0.
    ///
    /// Needed to report how many parameters we expect in the `Describe`
    /// message of the PostgreSQL extended protocol. `Expr::Param` is
    /// zero-based (`$1` -> `Param(0)`), the number is one greater.
    pub fn max_param(&self) -> usize {
        match self {
            Expr::Param(i) => *i + 1,
            Expr::Field(_) | Expr::Lit(_) => 0,
            Expr::And(a, b) | Expr::Or(a, b) => a.max_param().max(b.max_param()),
            Expr::Not(a) | Expr::IsNull(a) => a.max_param(),
            Expr::Cmp(_, a, b) | Expr::Like(a, b) | Expr::Has(a, b) | Expr::Arith(_, a, b) => {
                a.max_param().max(b.max_param())
            }
            Expr::In(a, items) => items
                .iter()
                .fold(a.max_param(), |m, i| m.max(i.max_param())),
            // The inner `get` binds from the same parameters.
            Expr::InSelect(a, sel) => a.max_param().max(sel.max_param()),
            Expr::Call(_, args) => args.iter().map(|a| a.max_param()).max().unwrap_or(0),
        }
    }

    /// Extracts the `field = literal` pattern -- for hash index pushdown.
    ///
    /// A bound parameter is resolved as well: `where year = $1` is the usual
    /// shape coming from the browser and from fenec-server, and looking only at
    /// `Lit` disabled pushdown entirely on those two paths.
    pub fn equality_key<'a>(&'a self, params: &'a [Value]) -> Option<(&'a str, &'a Value)> {
        fn value<'a>(e: &'a Expr, params: &'a [Value]) -> Option<&'a Value> {
            match e {
                Expr::Lit(v) => Some(v),
                Expr::Param(i) => params.get(*i),
                _ => None,
            }
        }
        if let Expr::Cmp(CmpOp::Eq, a, b) = self {
            if let (Expr::Field(f), Some(v)) = (a.as_ref(), value(b, params)) {
                return Some((f, v));
            }
            if let (Some(v), Expr::Field(f)) = (value(a, params), b.as_ref()) {
                return Some((f, v));
            }
        }
        None
    }

    /// The lowest id a row can have and pass the `and` chain, from its `id >
    /// x` and `id >= x` (or `x < id`, `x <= id`), so a scan in id order can
    /// start there: a page read by its last id, as a keyset pagination
    /// reads one, went through every row before it again --
    /// a million rows read a page of 1 000 at a time took 8.6 s, and take
    /// 110 ms, 100 000 took 95.8 ms and take 10.9. The filter still tests
    /// each row after it, so the floor need only be no higher than any
    /// match; it is taken from a whole number alone, which is what a page
    /// passes. Native only: the browser module leaves it out.
    pub fn conjunct_id_floor(&self, params: &[Value]) -> Option<u64> {
        match self {
            Expr::And(a, b) => match (a.conjunct_id_floor(params), b.conjunct_id_floor(params)) {
                (Some(x), Some(y)) => Some(x.max(y)),
                (x, y) => x.or(y),
            },
            Expr::Cmp(op, a, b) => {
                let id = |e: &Expr| matches!(e, Expr::Field(f) if f == "id");
                let (strict, x) = match op {
                    CmpOp::Gt if id(a) => (true, b),
                    CmpOp::Ge if id(a) => (false, b),
                    CmpOp::Lt if id(b) => (true, a),
                    CmpOp::Le if id(b) => (false, a),
                    _ => return None,
                };
                let x = match x.as_ref() {
                    Expr::Lit(Value::Int(i)) => *i,
                    Expr::Param(i) => match params.get(*i) {
                        Some(Value::Int(i)) => *i,
                        _ => return None,
                    },
                    _ => return None,
                };
                Some(x.saturating_add(strict as i64).max(0) as u64)
            }
            _ => None,
        }
    }

    /// Collects the indexable equalities inside an `and` chain.
    ///
    /// In a filter like `category = "a" and score > 500`, looking only at the
    /// top node misses the hash index; the selective equality can sit in any
    /// branch of the `and`. It does not descend under `or` -- even if one
    /// branch were indexed there, the other still requires the whole table.
    pub fn conjunct_equalities<'a>(
        &'a self,
        params: &'a [Value],
        out: &mut Vec<(&'a str, &'a Value)>,
    ) {
        match self {
            Expr::And(a, b) => {
                a.conjunct_equalities(params, out);
                b.conjunct_equalities(params, out);
            }
            other => {
                if let Some(kv) = other.equality_key(params) {
                    out.push(kv);
                }
            }
        }
    }

    /// Collects `field in [..]` lists from the same `and` chain, resolved to
    /// values.
    ///
    /// `in` is a set of equalities written short, so it can reach the hash
    /// index the same way: the candidate set is the union of one bucket per
    /// element. Before this it was the one predicate whose meaning and whose
    /// cost disagreed -- `year in [2024]` scanned the collection while
    /// `year = 2024` read a bucket, 12.98 ms against 0.092 ms over 200 000
    /// rows for the same question.
    ///
    /// A list is reported only when **every** element resolves. A partial
    /// one would narrow the candidates by the half it understood and lose
    /// whatever the other half matched -- and a wrong answer is not worth an
    /// index. Like `conjunct_equalities` it does not descend under `or`.
    pub fn conjunct_in_sets<'a>(
        &'a self,
        params: &'a [Value],
        out: &mut Vec<(&'a str, Vec<&'a Value>)>,
    ) {
        match self {
            Expr::And(a, b) => {
                a.conjunct_in_sets(params, out);
                b.conjunct_in_sets(params, out);
            }
            Expr::In(lhs, items) => {
                let Expr::Field(field) = lhs.as_ref() else {
                    return;
                };
                let mut vals = Vec::with_capacity(items.len());
                for it in items {
                    match it {
                        Expr::Lit(v) => vals.push(v),
                        // An unbound parameter must reach the eval path, which
                        // is where the error is raised.
                        Expr::Param(i) => match params.get(*i) {
                            Some(v) => vals.push(v),
                            None => return,
                        },
                        _ => return,
                    }
                }
                // An empty list matches nothing, and an empty candidate set
                // says so without reading a row.
                out.push((field, vals));
            }
            _ => {}
        }
    }

    /// The terms of an `and` chain, left to right -- the expression itself
    /// when it is not one: what an index over several fields matches its
    /// parts against, one term at a time, to know whether it answered every
    /// one.
    pub fn conjuncts<'a>(&'a self, out: &mut Vec<&'a Expr>) {
        match self {
            Expr::And(a, b) => {
                a.conjuncts(out);
                b.conjuncts(out);
            }
            other => out.push(other),
        }
    }

    /// Does the filter consist of exactly one equality?
    pub fn is_bare_equality(&self, params: &[Value]) -> bool {
        self.equality_key(params).is_some()
    }

    /// `field <op> value` for one comparison, with `value <op> field` turned
    /// round so the field is always on the left. `!=` is left out: it bounds
    /// nothing.
    pub fn range_key<'a>(&'a self, params: &'a [Value]) -> Option<(&'a str, CmpOp, &'a Value)> {
        fn value<'a>(e: &'a Expr, params: &'a [Value]) -> Option<&'a Value> {
            match e {
                Expr::Lit(v) => Some(v),
                Expr::Param(i) => params.get(*i),
                _ => None,
            }
        }
        let Expr::Cmp(op, a, b) = self else {
            return None;
        };
        if *op == CmpOp::Ne {
            return None;
        }
        if let (Expr::Field(f), Some(v)) = (a.as_ref(), value(b, params)) {
            return Some((f, *op, v));
        }
        if let (Some(v), Expr::Field(f)) = (value(a, params), b.as_ref()) {
            let flipped = match op {
                CmpOp::Lt => CmpOp::Gt,
                CmpOp::Le => CmpOp::Ge,
                CmpOp::Gt => CmpOp::Lt,
                CmpOp::Ge => CmpOp::Le,
                other => *other,
            };
            return Some((f, flipped, v));
        }
        None
    }

    /// The comparisons inside an `and` chain an ordered index can narrow by.
    /// Like `conjunct_equalities` it does not descend under `or` or `not`.
    pub fn conjunct_ranges<'a>(
        &'a self,
        params: &'a [Value],
        out: &mut Vec<(&'a str, CmpOp, &'a Value)>,
    ) {
        match self {
            Expr::And(a, b) => {
                a.conjunct_ranges(params, out);
                b.conjunct_ranges(params, out);
            }
            other => {
                if let Some(r) = other.range_key(params) {
                    out.push(r);
                }
            }
        }
    }

    /// Whether the whole filter is comparisons of `field` against values --
    /// an `and` chain of them and nothing else. Answered from an ordered
    /// index whose range expressed every one of them, such a filter needs no
    /// second look at the rows.
    pub fn only_ranges_on(&self, field: &str, params: &[Value]) -> bool {
        match self {
            Expr::And(a, b) => a.only_ranges_on(field, params) && b.only_ranges_on(field, params),
            other => matches!(other.range_key(params), Some((f, _, _)) if f == field),
        }
    }
}

/// Lazy access to a row's fields. The engine provides this over the store,
/// so fields the filter never touches are never decoded.
pub trait RowAccess {
    fn id(&self) -> DocId;
    fn field(&mut self, name: &str) -> Result<Value>;
    /// How a field's text compares, when its schema says: in a collation
    /// (`name text collate tr`), whose order `<` and `>` against it take
    /// rather than the bytes', and by its case fold (`email text nocase`),
    /// which `=`, `in` and `~` take as well.
    fn text_order(&self, name: &str) -> TextOrder {
        let _ = name;
        TextOrder::BYTES
    }
}

pub struct EvalCtx<'a> {
    pub params: &'a [Value],
    pub registry: &'a crate::plugin::Registry,
    /// What `now()` answers when set: the database's clock
    /// (`Database::set_clock`), which the browser module sets from
    /// `Date.now()` before each statement -- it has no clock of its own --
    /// and a test pins. The system's clock, through the registry, when
    /// `None`.
    pub clock: Option<i64>,
}

/// `l op r` for `+ - * /`: what an expression in a `set` (`{n: n + 1}`)
/// or a filter works out. A null on either side is null, as SQL's is
/// (`coalesce(n, 0) + 1` counts from nothing). Two ints make an int, and
/// one past 64 bits is refused rather than wrapped, as a division by zero
/// is; `/` between ints divides whole, toward zero, as PostgreSQL's does.
/// An int and a float make a float, and a float that is no longer finite
/// is refused. A timestamp moves by milliseconds (`now() + 30000`), and
/// two timestamps apart are the milliseconds between them. `+` joins two
/// texts, and only two texts: a text and a number is no join. Anything else
/// is a type error: the field's type check is what a result meets next.
/// The function a filter calls to read the rows past their `@ttl`
/// ([`Expr::asks_expired`]): answered before the query runs, as the test
/// of a row past its time, never called.
pub const EXPIRED: &str = "expired";

pub fn arith(op: ArithOp, l: &Value, r: &Value) -> Result<Value> {
    use Value::{Float, Int, Timestamp};
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    let over = || Error::Query("the int overflows 64 bits".into());
    let whole = |a: i64, b: i64| -> Result<i64> {
        match op {
            ArithOp::Add => a.checked_add(b),
            ArithOp::Sub => a.checked_sub(b),
            ArithOp::Mul => a.checked_mul(b),
            ArithOp::Div if b == 0 => return Err(Error::Query("division by zero".into())),
            ArithOp::Div => a.checked_div(b),
        }
        .ok_or_else(over)
    };
    Ok(match (l, r) {
        (Int(a), Int(b)) => Int(whole(*a, *b)?),
        (Timestamp(t), Int(d)) if matches!(op, ArithOp::Add | ArithOp::Sub) => {
            Timestamp(whole(*t, *d)?)
        }
        (Int(d), Timestamp(t)) if op == ArithOp::Add => Timestamp(whole(*d, *t)?),
        // Two texts joined: an entry id made of its movement's
        // (`$1 + ":dr"`) travelled as a parameter of its own. Only two
        // texts: `"n" + 1` stays a type error, as a number made text
        // silently would hide the mistake a typed field exists to catch.
        (Value::Text(a), Value::Text(b)) if op == ArithOp::Add => {
            let mut s = String::with_capacity(a.len() + b.len());
            s.push_str(a);
            s.push_str(b);
            Value::Text(s)
        }
        (Timestamp(a), Timestamp(b)) if op == ArithOp::Sub => Int(whole(*a, *b)?),
        (Int(_) | Float(_), Int(_) | Float(_)) => {
            let (a, b) = (l.as_f64().unwrap_or(0.0), r.as_f64().unwrap_or(0.0));
            let x = match op {
                ArithOp::Add => a + b,
                ArithOp::Sub => a - b,
                ArithOp::Mul => a * b,
                ArithOp::Div if b == 0.0 => return Err(Error::Query("division by zero".into())),
                ArithOp::Div => a / b,
            };
            if !x.is_finite() {
                return Err(Error::Query("the float overflows".into()));
            }
            Float(x)
        }
        _ => {
            return Err(Error::Type(format!(
                "`{}` takes numbers, a timestamp and milliseconds, or (`+`) two texts; found {} \
                 and {}",
                op.symbol(),
                l.type_name(),
                r.type_name()
            )))
        }
    })
}

/// Case-insensitive substring test, the `~` operator.
///
/// `~` never reaches an index, so this runs once per row of a full scan. The
/// obvious `hay.to_lowercase().contains(&needle.to_lowercase())` allocated
/// two Strings for every one of those rows and folded the needle again each
/// time, though the needle is the same on every row.
///
/// Unicode default case mapping agrees with ASCII over U+0000..U+007F, so
/// when both sides are ASCII the in-place comparison below is the same
/// answer, only without the allocations; anything else still folds. The
/// trade is that the ASCII path is a naive scan rather than the two-way
/// search behind `contains` -- quadratic in theory, but the needle is a
/// search term and not allocating beats the better asymptote at these sizes.
///
/// Dropping the folding path altogether was measured and turned down. It only
/// pays together with `lower`/`upper` in `plugin.rs`, which reach for the same
/// Unicode tables: ASCII here alone changed the wasm by nothing at all, and
/// both together by 5 200 bytes of brotli while the three were the standard
/// library's `to_lowercase` and `to_uppercase`. That is not worth losing
/// `ÇALIŞMA ~ çalişma`, which every non-English corpus depends on; the three
/// fold through `case` now, which costs less.
///
/// Folded, the two are searched as bytes as well: a needle of valid UTF-8
/// can only match at a character's first byte, so the answer is `contains`',
/// whose searcher slices the string where a slice can panic.
pub(crate) fn like_match(hay: &str, needle: &str) -> bool {
    if !(hay.is_ascii() && needle.is_ascii()) {
        let (h, n) = (crate::case::lower(hay), crate::case::lower(needle));
        let (h, n) = (h.as_bytes(), n.as_bytes());
        return n.is_empty() || h.windows(n.len()).any(|w| w[0] == n[0] && w == n);
    }
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.is_empty() {
        return true;
    }
    if n.len() > h.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// Whether `h` is `p` as SQL's `like` reads a pattern, both UTF-8: `%` any
/// run of characters, `_` one, and `e` -- the escape, empty for none --
/// before a character taking it as written. `None` for a pattern that ends
/// in its escape, which SQL refuses. A mismatch after a `%` takes one more
/// character of the text into it and goes on from there; only the last
/// `%` is gone back to, since an earlier one could only take what the
/// later one takes.
pub(crate) fn like_pattern(h: &[u8], p: &[u8], e: &[u8]) -> Option<bool> {
    // A character's length in bytes, by its first.
    let len = |b: u8| 1 + (b >= 0xC0) as usize + (b >= 0xE0) as usize + (b >= 0xF0) as usize;
    // Refused whatever the text, as a mistake in the statement: found as
    // the match reached it, it was an error for some rows and no other.
    let mut k = 0;
    while k < p.len() {
        if !e.is_empty() && p[k..].starts_with(e) {
            k += e.len();
            if k == p.len() {
                return None;
            }
        }
        k += 1;
    }
    let (mut i, mut j) = (0, 0);
    let mut back = None;
    loop {
        if let Some(&c) = p.get(j) {
            let lit = !e.is_empty() && p[j..].starts_with(e);
            let at = j + if lit { e.len() } else { 0 };
            if !lit && c == b'%' {
                j += 1;
                if j == p.len() {
                    return Some(true);
                }
                back = Some((j, i));
                continue;
            }
            if let Some(&b) = h.get(i) {
                if !lit && c == b'_' {
                    i += len(b);
                    j += 1;
                    continue;
                }
                let n = len(p[at]);
                if h[i..].starts_with(&p[at..at + n]) {
                    i += n;
                    j = at + n;
                    continue;
                }
            }
        } else if i == h.len() {
            return Some(true);
        }
        match back {
            Some((bj, bi)) if bi < h.len() => {
                i = bi + len(h[bi]);
                // A character written after the `%` can match only where
                // its first byte stands, found by a scan of the bytes
                // rather than a match tried at each character: a scan by
                // `%99 of%` took 29 ms over 100 000 rows, by `~` 8.
                let lit = !e.is_empty() && p[bj..].starts_with(e);
                let c = p[bj + if lit { e.len() } else { 0 }];
                if lit || (c != b'%' && c != b'_') {
                    match h[i..].iter().position(|&b| b == c) {
                        Some(k) => i += k,
                        None => return Some(false),
                    }
                }
                j = bj;
                back = Some((bj, i));
            }
            _ => return Some(false),
        }
    }
}

/// SQL's `x like p [escape e]` and `ilike`, which the parser writes as
/// calls of those names, so that no walk over an expression grew a
/// variant. A `nocase` field compares by its fold, as its `=` does, and
/// `ilike` by Unicode's where the field has none; a collated field by its
/// bytes, as its `=` does. No escape unless one is named, as in standard
/// SQL and SQLite. A null either side is null; any other value but text a
/// type error, as PostgreSQL's.
#[inline(never)]
fn sql_like(
    ci: bool,
    x: &Expr,
    p: &Expr,
    e: &[Expr],
    row: &mut dyn RowAccess,
    ctx: &EvalCtx,
) -> Result<Value> {
    let fold = folded(&*row, x);
    #[cfg(feature = "nocase")]
    let fold = fold.or(ci.then_some(crate::case::CaseFold::Unicode));
    #[cfg(not(feature = "nocase"))]
    if ci {
        return Err(crate::error::not_built("`ilike`", "nocase"));
    }
    let (h, p) = (eval(x, row, ctx)?, eval(p, row, ctx)?);
    let e = match e {
        [] => Value::Text(String::new()),
        [e] => eval(e, row, ctx)?,
        _ => {
            return Err(Error::Query(
                "`like` takes a text, a pattern and an escape".into(),
            ))
        }
    };
    let (Value::Text(h), Value::Text(p), Value::Text(e)) = (&h, &p, &e) else {
        if h.is_null() || p.is_null() || e.is_null() {
            return Ok(Value::Null);
        }
        return Err(Error::Type(format!(
            "`like` takes text, not {} and {}",
            h.type_name(),
            p.type_name()
        )));
    };
    let fit = |s| match fold {
        Some(f) => crate::case::fold(s, f),
        None => std::borrow::Cow::Borrowed(s),
    };
    // Folded, an escape with a case would be read in the pattern's letters.
    if e.chars().nth(1).is_some() || fit(e) != e.as_str() {
        return Err(Error::Query(
            "`escape` takes one character, with no case where case is ignored".into(),
        ));
    }
    match like_pattern(fit(h).as_bytes(), fit(p).as_bytes(), e.as_bytes()) {
        Some(m) => Ok(Value::Bool(m)),
        None => Err(Error::Query("a `like` pattern ends in its escape".into())),
    }
}

/// `$n` (`Param(i)`) asked of a statement handed fewer parameters: one
/// message, where four places each wrote it.
#[inline(never)]
pub fn unbound(i: usize) -> Error {
    Error::Query(format!("parameter ${} is not bound", i + 1))
}

pub fn eval(expr: &Expr, row: &mut dyn RowAccess, ctx: &EvalCtx) -> Result<Value> {
    Ok(match expr {
        Expr::Lit(v) => v.clone(),
        Expr::Param(i) => ctx.params.get(*i).cloned().ok_or_else(|| unbound(*i))?,
        Expr::Field(name) => {
            if name == "id" {
                Value::Int(row.id() as i64)
            } else {
                row.field(name)?
            }
        }
        Expr::Not(a) => Value::Bool(!truthy(&eval(a, row, ctx)?)),
        Expr::And(a, b) => {
            if !truthy(&eval(a, row, ctx)?) {
                Value::Bool(false) // short circuit
            } else {
                Value::Bool(truthy(&eval(b, row, ctx)?))
            }
        }
        Expr::Or(a, b) => {
            if truthy(&eval(a, row, ctx)?) {
                Value::Bool(true)
            } else {
                Value::Bool(truthy(&eval(b, row, ctx)?))
            }
        }
        Expr::IsNull(a) => Value::Bool(eval(a, row, ctx)?.is_null()),
        Expr::Cmp(op, a, b) => {
            let (l, r) = (eval(a, row, ctx)?, eval(b, row, ctx)?);
            let field = |e: &Expr| match e {
                Expr::Field(name) => row.text_order(name),
                _ => TextOrder::BYTES,
            };
            // The first side that is a field with an order of its own.
            let order = || match field(a) {
                o if o.is_bytes() => field(b),
                o => o,
            };
            // A `nocase` field's text equals any text that folds to it.
            if let (CmpOp::Eq | CmpOp::Ne, Value::Text(x), Value::Text(y)) = (op, &l, &r) {
                if let Some(f) = order().fold {
                    let eq = crate::case::eq_folded(x, y, f);
                    return Ok(Value::Bool(eq == (*op == CmpOp::Eq)));
                }
            }
            Value::Bool(compare(*op, &l, &r, &order))
        }
        Expr::Like(a, b) => {
            let l = eval(a, row, ctx)?;
            let r = eval(b, row, ctx)?;
            match (l.as_text(), r.as_text()) {
                // A `nocase` field's fold holds the needle's: `straße ~ SS`.
                (Some(hay), Some(needle)) => Value::Bool(match folded(&*row, a) {
                    Some(f) => {
                        like_match(&crate::case::fold(hay, f), &crate::case::fold(needle, f))
                    }
                    None => like_match(hay, needle),
                }),
                _ => Value::Bool(false),
            }
        }
        // `has` and `in` equality must read from the same definition as `=`:
        // `cmp_value` also works across types (`Int(3)` ~ `Float(3.0)`,
        // `Timestamp` ~ ISO text), while the derived `==` only matches the
        // same variant. It showed: `year = $1` matched, `year in [$1]` did not.
        Expr::Has(a, b) => {
            let l = eval(a, row, ctx)?;
            let r = eval(b, row, ctx)?;
            match l {
                Value::List(items) => {
                    Value::Bool(items.iter().any(|i| i.cmp_value(&r) == Ordering::Equal))
                }
                _ => Value::Bool(false),
            }
        }
        Expr::In(a, items) => {
            let l = eval(a, row, ctx)?;
            // A `nocase` field's text is any of the texts that fold to it.
            let fold = match &l {
                Value::Text(_) => folded(&*row, a),
                _ => None,
            };
            let mut found = false;
            for it in items {
                let v = eval(it, row, ctx)?;
                let same = match (fold, &v, &l) {
                    (Some(f), Value::Text(x), Value::Text(y)) => crate::case::eq_folded(x, y, f),
                    _ => v.cmp_value(&l) == Ordering::Equal,
                };
                if same {
                    found = true;
                    break;
                }
            }
            Value::Bool(found)
        }
        // Answered before the query runs, as the list it becomes: one met
        // here is in a filter evaluated on its own.
        Expr::InSelect(..) => {
            return Err(Error::Query(
                "`in (get ...)` is answered before a query runs, and cannot be here".into(),
            ))
        }
        Expr::Call(name, args) => {
            if let (Some(t), true) = (
                ctx.clock,
                args.is_empty() && name.eq_ignore_ascii_case("now"),
            ) {
                return Ok(Value::Timestamp(t));
            }
            // `case when c then v ... else e end`, which the parser makes a
            // call of its conditions and values in turn and the `else` last:
            // a call, so that every walk over an expression reaches its
            // parts with no variant of its own, and worked out lazily, as
            // SQL's is -- `case when qty = 0 then null else px / qty end`
            // divides only where it may.
            if name == "case" {
                let (pairs, rest) = args.as_chunks::<2>();
                for [c, v] in pairs {
                    if truthy(&eval(c, row, ctx)?) {
                        return eval(v, row, ctx);
                    }
                }
                return match rest {
                    [e] => eval(e, row, ctx),
                    _ => Ok(Value::Null),
                };
            }
            if let ("like" | "ilike", [x, p, e @ ..]) = (name.as_str(), args.as_slice()) {
                return sql_like(name.starts_with('i'), x, p, e, row, ctx);
            }
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval(a, row, ctx)?);
            }
            ctx.registry.call(name, &vals)?
        }
        Expr::Arith(op, a, b) => arith(*op, &eval(a, row, ctx)?, &eval(b, row, ctx)?)?,
    })
}

/// The fold `e` compares by, when it is a `nocase` field of the row's.
fn folded(row: &dyn RowAccess, e: &Expr) -> Option<crate::case::CaseFold> {
    match e {
        Expr::Field(name) => row.text_order(name).fold,
        _ => None,
    }
}

/// `l op r` as a filter takes it: a null equal only to a null and ordered
/// against nothing. Text against a field orders as the field does
/// (`order`, asked only of two texts ordered), so the scan and the field's
/// `@sorted` index agree: in its collation, and by its case fold where it
/// is `nocase`. Equality is the bytes', as a collation ties no two strings:
/// a `nocase` field's is asked before (`eval`, `Filter`'s `Folded`) -- asked
/// here, every text `=` of a bound filter called for the order it had no
/// use for, and the browser module's count by one text over 20 000 rows
/// went 1.55 -> 1.66 ms.
#[inline]
pub(crate) fn compare(op: CmpOp, l: &Value, r: &Value, order: &dyn Fn() -> TextOrder) -> bool {
    if l.is_null() || r.is_null() {
        return match op {
            CmpOp::Eq => l.is_null() && r.is_null(),
            CmpOp::Ne => l.is_null() != r.is_null(),
            _ => false,
        };
    }
    let ord = match (op, l, r) {
        (CmpOp::Eq | CmpOp::Ne, ..) => l.cmp_value(r),
        (_, Value::Text(x), Value::Text(y)) => match order() {
            o if o.is_bytes() => l.cmp_value(r),
            o => o.compare(x, y),
        },
        _ => l.cmp_value(r),
    };
    match op {
        CmpOp::Eq => ord == Ordering::Equal,
        CmpOp::Ne => ord != Ordering::Equal,
        CmpOp::Lt => ord == Ordering::Less,
        CmpOp::Le => ord != Ordering::Greater,
        CmpOp::Gt => ord == Ordering::Greater,
        CmpOp::Ge => ord != Ordering::Less,
    }
}

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Null => false,
        Value::Int(i) => *i != 0,
        Value::Timestamp(ms) => *ms != 0,
        Value::Float(f) => *f != 0.0,
        Value::Text(s) => !s.is_empty(),
        Value::Bytes(b) => !b.is_empty(),
        Value::List(l) => !l.is_empty(),
        Value::Vector(v) => !v.is_empty(),
        Value::Sparse(_, e) => !e.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

/// The `near` clause: vector similarity search.
#[derive(Debug, Clone, PartialEq)]
pub struct Near {
    pub field: String,
    /// The query vector is either given directly or comes from a parameter.
    pub vector: Expr,
    /// HNSW candidate list width; None means the schema default.
    pub ef: Option<usize>,
    /// When true HNSW is skipped and an exact scan is run.
    pub exact: bool,
    /// The distance pgvector's operator named -- `<->` L2, `<=>` cosine,
    /// `<#>` the inner product -- which the field's index must measure, so
    /// that `near`'s order is the operator's: SQL's `order by emb <-> $1
    /// limit 10`. None for FenecQL's `near`, which measures as the index
    /// does.
    pub metric: Option<crate::schema::Metric>,
}

/// The `match` clause: BM25 over a full-text index.
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    pub field: String,
    /// The query text, given directly or through a parameter.
    pub query: Expr,
    /// BM25's statistics -- how many documents, their mean length, how
    /// many hold each term -- over the rows the filter selects rather than
    /// over the collection. No FenecQL says it: a scoped token's `match`
    /// is made so (`fenec_http::access`), its filter holding its rules, so
    /// that a score says nothing of rows it may not read -- over the
    /// collection, alice's own memo scored 9.87 and then 4.79 once bob
    /// wrote 200 private ones holding the same word.
    pub within: bool,
    /// `fuzzy [N]`: the most edits a word of the query may take to match a
    /// word of the index, 0 to 2 -- `fuzzy` alone 2 -- each word held
    /// besides to what its length allows: none under 5 characters, one up
    /// to 8 ([`crate::fuzzy::typos`]). 0 reads every word as typed.
    pub fuzzy: u8,
    /// `prefix`: the query's last word also matches the words it begins,
    /// as a search box is typed into.
    pub prefix: bool,
}

impl Match {
    /// How loosely it reads its words.
    pub fn loose(&self) -> crate::text::Loose {
        crate::text::Loose {
            fuzzy: self.fuzzy,
            prefix: self.prefix,
        }
    }
}

/// The most edits `fuzzy N` takes: past two, a word of nine letters is a
/// third rewritten, and the walk for one reaches most of a dictionary.
pub const MAX_FUZZY: u8 = 2;

/// What a `fuzzy` past [`MAX_FUZZY`] is refused with, by the parser and
/// the engine in the same words.
pub const FUZZY_RANGE: &str = "`fuzzy` takes 0 to 2 edits a word";

/// The `rerank` clause: reorder what `match` found by exact vector distance.
///
/// This is the no-graph retrieval path. `match` is cheap and recall-oriented,
/// the vectors are read straight out of the store, and the reordering is
/// exact over the candidate set -- so no HNSW graph has to be built, held,
/// validated on open or rebuilt when it fails to validate. Measured on BEIR
/// (`make beir`): on SciFact taking 50 candidates scores nDCG@10 0.654
/// against 0.645 for a full dense scan of the same vectors, and on FiQA 1 000
/// candidates come within 0.0003 of the full scan (0.368), each while scoring
/// under 2% of the corpus.
/// The lexical stage does not only save work -- it removes documents that are
/// semantically close but lexically wrong.
#[derive(Debug, Clone, PartialEq)]
pub struct Rerank {
    pub field: String,
    /// The query vector, given directly or through a parameter.
    pub vector: Expr,
    /// How many `match` candidates to rescore. None means the default.
    pub candidates: Option<usize>,
}

/// `fuse`: `match` and `near` each rank their own candidates, and the two
/// rankings are combined by reciprocal rank -- a document scores
/// `1 / (k + rank)` from each list it is on. Ranks, not scores: a BM25 score
/// and a cosine distance have no common scale to add them on.
///
/// Measured on BEIR (`make beir`, nDCG@10): on SciFact, where a claim shares
/// its words with the evidence, 0.699 against 0.662 for `match` and 0.645 for
/// `near` alone; on FiQA, where a question shares few words with its answer,
/// 0.366 against 0.232 and 0.365 -- the weak side does not drag the strong
/// one down. `rerank` at its default scores 0.643 and 0.360: it can only
/// reorder what the words found, and `fuse` also takes what they missed. What
/// that costs is the graph `near` walks, which `rerank` does without.
#[derive(Debug, Clone, PartialEq)]
pub struct Fuse {
    /// The rank offset; 60 unless given, the value the method was published with.
    pub k: Option<u32>,
    /// How many candidates each side ranks. None means the default.
    pub candidates: Option<usize>,
}

/// A count a statement is paged or cut by -- a `limit`, an `offset`, a
/// facet's `top` -- written in the text or handed as a parameter, `limit
/// $2`. A page size and an offset are a request's data: written into the
/// text, each page of a list was a text of its own, parsed again and kept
/// apart in the server's parse cache (16 shards of 64, a shard emptied when
/// full), and every client spliced a number into its statement. A
/// parameter is checked as the statement runs ([`Select::check_counts`]),
/// refused as the literal it stands for is ([`count_refused`]), and read
/// where the plan reads the number ([`Count::at`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Count {
    /// Written in the text: `limit 20`.
    N(usize),
    /// `$n`, zero-based as [`Expr::Param`] is: checked before the plan
    /// and read where it reads the number.
    Param(usize),
}

impl Default for Count {
    fn default() -> Count {
        Count::N(0)
    }
}

impl Count {
    /// The number a plan reads: its own, or its parameter's, which
    /// [`Select::check_counts`] has held to a count before the plan -- and
    /// had it not, nothing rather than everything. Read where it is used
    /// rather than bound into a copy of the select: the copy took a read by
    /// id 472 -> 710 ns in process.
    #[inline]
    pub fn at(self, params: &[Value]) -> usize {
        match self {
            Count::N(n) => n,
            Count::Param(i) => param_count(params, i),
        }
    }

    /// The number it stands for: its own, or its parameter's out of
    /// `params`, refused as a literal is ([`count_refused`]).
    pub fn of(self, what: &str, params: &[Value]) -> Result<usize> {
        match self {
            Count::N(n) => Ok(n),
            Count::Param(i) => match params.get(i) {
                Some(v) => as_count(v).ok_or_else(|| Error::Query(count_refused(what, v))),
                None => Err(unbound(i)),
            },
        }
    }

    /// Whether it is `0` as written: no `offset`, which a parameter is not
    /// -- `offset $1` is an offset whatever it is handed.
    pub fn is_zero(self) -> bool {
        self == Count::N(0)
    }

    fn param(self) -> usize {
        match self {
            Count::N(_) => 0,
            Count::Param(i) => i + 1,
        }
    }
}

/// [`Count::at`] of a parameter, out of line: the number written is the
/// one read on every page.
#[inline(never)]
fn param_count(params: &[Value], i: usize) -> usize {
    let n = params.get(i).and_then(as_count);
    debug_assert!(n.is_some(), "a count is checked before the plan");
    n.unwrap_or(0)
}

/// The count a value gives a `limit`, an `offset` or a `top`: a whole
/// number from 0 -- a float too where it has no fraction, `20.0`, which is
/// what a JSON number may be read as -- and `None` for anything else, which
/// [`count_refused`] says. Past what `usize` holds (a 32-bit target's
/// 4 294 967 295) it is that: no more rows than memory holds can be asked
/// for, where `as` wrapped a literal `limit 5000000000` to 705 million in
/// the browser module.
pub fn as_count(v: &Value) -> Option<usize> {
    match *v {
        Value::Int(n) if n >= 0 => Some(usize::try_from(n).unwrap_or(usize::MAX)),
        // `as` saturates; a NaN or an infinity has no zero fraction.
        Value::Float(f) if f >= 0.0 && f.fract() == 0.0 => Some(f as usize),
        _ => None,
    }
}

/// Why `v` is no count for `what`, the value as JSON writes it: a literal
/// and a parameter are refused in the same words. Written by the JSON
/// writer the module has anyway: by type, with a number's own text, it was
/// 459 bytes of the browser module.
pub fn count_refused(what: &str, v: &Value) -> String {
    let mut out = format!("`{what}` takes a whole number from 0, not ");
    crate::json::value_into(&mut out, v);
    out
}

/// The `lookup` clause: attach each matching document of another collection
/// to the row it belongs to.
///
/// It is a terminal clause -- everything before it binds to the driving
/// collection, everything after it to `collection`. That positional scoping
/// is what keeps qualified names (`reviews.stars`) out of the language
/// altogether: `where` means on either side exactly what it always meant,
/// and each side still reaches its own indexes through the ordinary path.
///
/// `limit` here counts children *per parent*, which is the whole reason the
/// clause exists. A join's limit counts pairs, so one parent with 56 374
/// children eats the entire page, and asking a join for three children each
/// needs a window function -- measured at 0.458 ms against 0.237 ms for
/// doing the same thing by hand.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Lookup {
    /// Where the children come from.
    pub collection: String,
    /// The child's fields the key is matched on: `on product_id` is one,
    /// `on (org, order) = (org, id)` -- an order's lines in a tenant's file
    /// -- two.
    ///
    /// One field is a bucket probe: the child's field is `id` or carries
    /// `@hash`, because a scan per parent would be a different feature
    /// wearing the same name. Several are an index over several fields
    /// whose leading parts they are, in any order: the parent's values laid
    /// end to end as its keys hold them are a prefix of those keys, and its
    /// children one run of the entries, which an `order` on the parts after
    /// them reads in order -- as one field leading such an index is, where
    /// it has no `@hash` or the order walks it. A key joined into one text
    /// field to have a `@hash` would be a field of each side kept up by
    /// hand for every write.
    pub on: Vec<String>,
    /// The field of the level above each of `on` equals, in order: `id`
    /// unless `on a = b` names one, the foreign-key-to-primary-key case
    /// spelled out. Two lists rather than a list of pairs: a `Vec<String>`
    /// is code the browser module has already.
    ///
    /// Where the level above folds -- a `get` or a level whose list
    /// aggregates -- its rows are its groups, and each name is a column of
    /// its list: `get orders select courier, count() as n group courier
    /// lookup couriers on user = courier`.
    pub parent: Vec<String>,
    /// None = all fields of the child.
    pub project: Option<Vec<String>>,
    /// A list that aggregates -- `select count() as n, sum(total) as
    /// revenue` -- in the order written: each parent's children folded, one
    /// row a parent, or one a group of them with `group`, rather than the
    /// children themselves. Empty: the children.
    ///
    /// A level is the `get` its children would be with the parent's key in
    /// its `where`, so its list folds as a `get`'s does, by the same
    /// `Fold`s: ungrouped, `order`, `limit` and `offset` pick the children
    /// it folds -- the latest 50 of a courier's deliveries -- and grouped
    /// they page each parent's groups, its three best-selling products. A
    /// parent's own fields beside its children's folds would be two
    /// collections in one list, which the positional scoping keeps out: the
    /// parent's level selects those.
    pub aggregate: Vec<Column>,
    /// `group product`: one row a parent and a set of the keys' values.
    pub group: Vec<Expr>,
    /// `having <expr>` after `group`, a parent's groups it holds for.
    pub having: Option<Expr>,
    pub filter: Option<Expr>,
    pub order: Vec<Sort>,
    pub limit: Option<Count>,
    pub offset: Count,
    /// `required`: drop a parent that no child matches.
    ///
    /// Without it the page is the parents and a childless one keeps its row
    /// with an empty group, which is what a listing wants. With it the
    /// children decide who appears -- "products that have a five-star
    /// review" -- and that is a different question, so it is asked for
    /// rather than inferred from the presence of a child `where`.
    ///
    /// It is tested before `offset` and `limit`: a parent whose matches the
    /// page happened to skip has matches all the same. A level that folds
    /// asks it of its children too, not of its groups: an ungrouped list
    /// answers one row for every parent, so asked of the rows it would
    /// keep every one, and "couriers who delivered, with their counts" is
    /// the question.
    pub required: bool,
    /// A further `lookup`, hanging off the children of this one:
    /// `products -> reviews -> authors`.
    ///
    /// The positional scoping simply reads one level deeper -- everything
    /// after this clause's `lookup` binds to *its* collection, and `on child
    /// = parent` names a field of the level immediately above. So the rule
    /// that keeps qualified names out of the language survives the chain
    /// unchanged: at any point in the query exactly one collection is in
    /// scope, and it is the last one named.
    ///
    /// The second level is where a batch stops being an answer. One level
    /// of N+1 fits in a single round trip, because every query in it can be
    /// written from the page's ids; the level below cannot, because its keys
    /// are in the rows that have not come back yet. So the client pays two
    /// round trips whatever it does. Over 2 000 shops, 20 000 orders and
    /// 200 000 lines, a page of 20 x 3 x 5: 49.1 us in process against
    /// 139.8 us for the same page as 81 separate queries, and over HTTP
    /// 0.204 ms for the one request against 0.415 ms for two `/batch` trips
    /// -- on loopback, where the extra trip costs almost nothing and still
    /// doubles it.
    pub next: Option<Box<Lookup>>,
}

/// How many `lookup` levels one query may chain.
///
/// A bound on the stack rather than on the work: the parser recurses once
/// per level, `check` and the engine walk the chain the same way, and
/// dropping the boxed chain recurses too. `products -> reviews -> authors`
/// is three collections and two levels; the rest is headroom nobody has
/// asked for. Exceeding it is an error, not a truncation -- a chain quietly
/// cut short is a wrong answer believed right.
pub const MAX_LOOKUP_DEPTH: usize = 8;

/// The most distinct values an `in (get ...)` may hand its query, a value
/// many rows hold counted once. A larger set is a query error, never a set
/// cut short, which would be a wrong answer believed right. The list is
/// held whole while the query runs, and a
/// question over more is one `lookup ... required` asks from the other
/// side, probing each parent's children rather than listing them.
pub const MAX_SUBQUERY_VALUES: usize = 100_000;

/// How deep `in (get ...)` may nest: `a in (get b select x where y in
/// (get c ...))` is two. Each level runs a query before the one around it,
/// and the parser and the engine recurse once a level; four is room for
/// what a question asks without a join, and a fifth is refused rather
/// than run.
pub const MAX_SUBQUERY_DEPTH: usize = 4;

impl Lookup {
    /// One side of `on` as written: `product_id`, or `(org, order)`.
    pub fn side(&self, child: bool) -> String {
        let names = if child { &self.on } else { &self.parent };
        // Pushed rather than `join`ed or formatted: the standard library's
        // join was 1.1 KB of the browser module.
        let several = names.len() > 1;
        let mut s = String::from(if several { "(" } else { "" });
        for (i, n) in names.iter().enumerate() {
            s.push_str(if i > 0 { ", " } else { "" });
            s.push_str(n);
        }
        s.push_str(if several { ")" } else { "" });
        s
    }

    /// What `on` may say: a field of each side for each part of the key.
    /// A child's `id` names its row alone, and no index over several fields
    /// holds it as a part (`Schema::new` reserves the name), so a key of
    /// several naming it is refused rather than read as `id` and a test of
    /// the rest. More than an index has parts, or a field twice, no index
    /// leads with, and the engine refuses it naming the index it lacks.
    fn check_key(&self) -> Result<()> {
        let n = self.on.len();
        let why = if n == 0 || self.parent.len() != n {
            "pairs a field of each side for each part: `on (a, b) = (c, d)`"
        } else if n > 1 && self.on.iter().any(|c| c == "id") {
            "on several fields cannot name the child's `id`, which names one row alone: \
             `on id = <field>`"
        } else {
            return Ok(());
        };
        Err(Error::Query(format!("`lookup {}` {why}", self.collection)))
    }

    /// Whether its list aggregates: its rows are its children folded.
    pub fn folds(&self) -> bool {
        !self.aggregate.is_empty() || !self.group.is_empty()
    }

    /// The expressions of its aggregating list, its keys and `having`.
    pub fn list_exprs(&self) -> impl Iterator<Item = &Expr> {
        (self.aggregate.iter().map(|c| &c.expr))
            .chain(&self.group)
            .chain(&self.having)
    }

    /// What a level must be beside its list: one that folds as a `get`'s
    /// does ([`check_folds`]) and orders its groups by columns; and where
    /// the level above folds (`above`, its list), a key read from the
    /// columns of that level's rows, which have no `id`.
    fn check_level(&self, above: Option<&[Column]>) -> Result<()> {
        if self.folds() {
            check_folds(&self.aggregate, &self.group, self.having.as_ref())?;
            if self.group.is_empty() {
                picks_by_rows(&self.aggregate, &self.order)?;
            }
        } else if self.having.is_some() {
            return Err(Error::Query(
                "`having` keeps the groups it holds for: it follows `group`".into(),
            ));
        }
        let Some(list) = above else {
            return Ok(());
        };
        for p in &self.parent {
            if !list.iter().any(|c| c.name == *p) {
                return Err(Error::Query(format!(
                    "`lookup {}` on {} = {}: the level above answers groups, and `{p}` is \
                     not a column of its list -- name a key or a value it selects",
                    self.collection,
                    self.side(true),
                    self.side(false)
                )));
            }
        }
        Ok(())
    }

    /// This clause and every one hanging off it, outermost first.
    pub fn chain(&self) -> impl Iterator<Item = &Lookup> {
        let mut cur = Some(self);
        std::iter::from_fn(move || {
            let l = cur?;
            cur = l.next.as_deref();
            Some(l)
        })
    }
}

/// Column name of the `count` result. No such field can exist in a schema --
/// field names are identifiers and `count` may be one too; on a clash the
/// column name matches but the value is still the count, because `count`
/// replaces the projection entirely.
pub const COUNT_COLUMN: &str = "count";

/// The one column `explain` answers with.
pub const PLAN_COLUMN: &str = "plan";

/// The aggregates a select list may call, as the parser lowercases them.
/// `count(*)` is a call with no argument, `count(distinct f)` one whose
/// argument is a call of `distinct`, and `first(px by at)` one of two.
pub const AGGREGATES: [&str; 10] = [
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "first",
    "last",
    "approx_count_distinct",
    "hll_accumulate",
    "hll_combine",
];

/// The most distinct values a query's `count(distinct ...)` items hold
/// between them, over all their groups. Each is its encoding in a hash
/// map, about 60 to 70 bytes with the map's own -- some 70 MB at the
/// bound -- so a count past it is refused rather than held, or cut short:
/// a distinct count that stopped counting is a wrong answer believed right.
pub const MAX_DISTINCT_VALUES: usize = 1_000_000;

/// An item of a select list that is more than a field as it is: an
/// aggregating list's every item -- `status`, `sum(total)`, `bucket(at,
/// 1h) as hour`, `sum(px * qty) / sum(qty) as vwap` -- and a plain list's
/// computed column, `px * qty as notional`. It answers under `name`: what
/// `as` gave it, or, for a field or an aggregate of one, the text it was
/// written as ([`label_of`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub expr: Expr,
    pub name: String,
}

/// The name an item answers under when no `as` gives it one: a field's, or
/// an aggregate of a field as it is written -- `count`, `sum(total)`,
/// `count(distinct user)`, `first(px)` (with or without its `by`). `None`
/// for anything else, which needs a name: a column called `px * qty` would
/// have to be written back exactly as the parser read it.
pub fn label_of(e: &Expr) -> Option<String> {
    fn field(e: Option<&Expr>) -> Option<&str> {
        match e {
            Some(Expr::Field(f)) => Some(f),
            _ => None,
        }
    }
    let Expr::Call(name, args) = e else {
        return field(Some(e)).map(str::to_string);
    };
    let one = args.len() == 1 || (args.len() == 2 && matches!(name.as_str(), "first" | "last"));
    match (name.as_str(), args.first()) {
        ("count", None) => Some(COUNT_COLUMN.to_string()),
        ("count", Some(Expr::Call(d, inner))) if d == "distinct" && inner.len() == 1 => {
            field(inner.first()).map(|f| format!("count(distinct {f})"))
        }
        _ if one && e.aggregate_name().is_some() => {
            field(args.first()).map(|f| format!("{name}({f})"))
        }
        _ => None,
    }
}

/// The keys of an `order` -- a `get`'s, a `set`'s or a `del`'s -- that are
/// expressions, held to what a row can work out: no inner `get`, which a
/// filter holds and a token's rules reach, no aggregate and no
/// `expired()`, each asked of a set of rows. Over groups a key names a
/// column of the list, so an expression there is named with `as` first.
pub fn check_order(order: &[Sort], grouped: bool) -> Result<()> {
    for e in order.iter().filter_map(|s| s.expr.as_ref()) {
        let why = if grouped {
            "over groups `order` names a column: name the expression with `as`"
        } else if e.has_subquery() {
            "an `order` key holds no inner `get`"
        } else if e.has_aggregate() {
            "an aggregate in `order` needs `group`"
        } else if e.asks_expired() {
            "`expired()` goes in `where`"
        } else {
            continue;
        };
        return Err(Error::Query(why.into()));
    }
    Ok(())
}

/// Whether an expression among `order`'s keys holds a list of numbers.
fn sorts_read_vectors(order: &[Sort]) -> bool {
    order
        .iter()
        .any(|s| s.expr.as_ref().is_some_and(Expr::reads_vectors))
}

/// The highest `$n` among `order`'s expressions.
fn sorts_max_param(order: &[Sort]) -> usize {
    let mut n = 0;
    for e in order.iter().filter_map(|s| s.expr.as_ref()) {
        n = n.max(e.max_param());
    }
    n
}

/// The field whose values a column carries, if it carries one's: the field
/// itself, or its `min`, `max`, `first` or `last`. Its collation is the
/// column's, and `collate` orders the column only when it is text.
pub fn carried_field(e: &Expr) -> Option<&str> {
    match e {
        Expr::Field(f) => Some(f),
        Expr::Call(name, args) if matches!(name.as_str(), "min" | "max" | "first" | "last") => {
            match args.first() {
                Some(Expr::Field(f)) => Some(f),
                _ => None,
            }
        }
        _ => None,
    }
}

/// `bucket(at, 15m)`'s interval: a whole number of milliseconds from the
/// epoch (`ms`, `s`, `m`, `h`, `d`), of weeks from a Monday (`w`), as ISO's
/// begin, or of calendar months from January (`mo`, and `y` for twelve),
/// all in UTC.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Interval {
    /// A step of milliseconds, counted from an origin: the epoch, or for
    /// weeks the Monday after it, 1970-01-05.
    Ms(i64, i64),
    Months(i64),
}

const DAY_MS: i64 = 86_400_000;

impl Interval {
    /// The interval a value names: a whole number of milliseconds past
    /// zero, or a duration's text, `15m`, `1h`, `1mo` -- what the parser
    /// makes of `bucket(at, 15m)`, and what a parameter may hold.
    pub fn of(v: &Value) -> Result<Interval> {
        // Each unit's milliseconds, or as a negative number its months.
        const UNITS: [(&str, i64); 8] = [
            ("ms", 1),
            ("s", 1_000),
            ("m", 60_000),
            ("h", 3_600_000),
            ("d", DAY_MS),
            ("w", 7 * DAY_MS),
            ("mo", -1),
            ("y", -12),
        ];
        let (mut n, mut unit) = (0i64, "ms");
        match v {
            Value::Int(ms) => n = *ms,
            Value::Text(t) => {
                unit = "";
                for (i, b) in t.bytes().enumerate() {
                    if !b.is_ascii_digit() {
                        unit = t.get(i..).unwrap_or("");
                        break;
                    }
                    n = n.saturating_mul(10).saturating_add((b - b'0') as i64);
                }
            }
            _ => {}
        }
        let iv = match UNITS.iter().find(|u| u.0 == unit).map(|u| u.1) {
            _ if n <= 0 => None,
            Some(m) if m < 0 => n.checked_mul(-m).map(Interval::Months),
            // 1970-01-05 was a Monday.
            Some(ms) => n
                .checked_mul(ms)
                .map(|step| Interval::Ms(step, if unit == "w" { 4 * DAY_MS } else { 0 })),
            None => None,
        };
        iv.ok_or_else(|| {
            Error::Query(
                "`bucket` takes an interval such as 15m, 1h, 1d, 1w or 1mo \
                 (ms, s, m, h, d, w, mo, y), or milliseconds past zero"
                    .into(),
            )
        })
    }

    /// The start of the interval a timestamp falls in -- or epoch
    /// milliseconds as an int, answered as an int. Null stays null.
    pub fn truncate(self, v: &Value) -> Result<Value> {
        let t = match v {
            Value::Null => return Ok(Value::Null),
            Value::Timestamp(t) | Value::Int(t) => *t,
            other => {
                return Err(Error::Type(format!(
                    "`bucket` takes a timestamp; found {}",
                    other.type_name()
                )))
            }
        };
        Ok(match v {
            Value::Int(_) => Value::Int(self.start(t)),
            _ => Value::Timestamp(self.start(t)),
        })
    }

    /// [`Interval::truncate`] by a zone's clock where one is given
    /// ([`crate::tz::Zone::bucket`]): a day from local midnight.
    #[cfg(feature = "analytics")]
    pub fn truncate_in(self, v: &Value, zone: Option<&crate::tz::Zone>) -> Result<Value> {
        let Some(z) = zone else {
            return self.truncate(v);
        };
        let t = match v {
            Value::Null => return Ok(Value::Null),
            Value::Timestamp(t) | Value::Int(t) => *t,
            other => {
                return Err(Error::Type(format!(
                    "`bucket` takes a timestamp; found {}",
                    other.type_name()
                )))
            }
        };
        let start = z.bucket(self, t)?;
        Ok(match v {
            Value::Int(_) => Value::Int(start),
            _ => Value::Timestamp(start),
        })
    }

    /// The start of the interval epoch milliseconds `t` fall in, UTC's.
    pub fn start(self, t: i64) -> i64 {
        match self {
            // Wrapping only past the ends of time, ±292 million years; in
            // `i128` the division brought the compiler's own, 1 KB of the
            // browser module.
            Interval::Ms(n, origin) => t
                .wrapping_sub(origin)
                .div_euclid(n)
                .wrapping_mul(n)
                .wrapping_add(origin),
            Interval::Months(n) => {
                let (y, m, _) = crate::time::civil_from_days(t.div_euclid(DAY_MS));
                let months = y * 12 + m as i64 - 1;
                let b = months - months.rem_euclid(n);
                crate::time::days_from_civil(b.div_euclid(12), b.rem_euclid(12) as u32 + 1, 1)
                    .wrapping_mul(DAY_MS)
            }
        }
    }
}

/// `highlight(body)` or `snippet(body, 20)` in a select list: the spans of
/// a text field that the terms of the query's `match` were read from
/// (`highlight.rs`), answered under its [`Mark::label`].
///
/// A select-list item rather than a clause, as SQLite's FTS5 and Turso
/// write `highlight()` and `snippet()`: it is a column of the row, it goes
/// where the list puts it, and the tags are its arguments -- values, a
/// literal or a parameter, never markup the engine chooses.
///
/// Its arguments after the field are one list, as written: a highlight's
/// tags, `pre` and `post`, or none; a snippet's ellipsis, then its tags.
/// Held as fields of their own, a select's clone carried a copy of the
/// code for them, 432 bytes of the browser module.
#[derive(Debug, Clone, PartialEq)]
pub struct Mark {
    pub field: String,
    /// `snippet(body, n)`: a window of `n` words around the densest marks.
    /// None: `highlight`, the whole text.
    pub snippet: Option<usize>,
    /// `highlight`: none, or `pre` and `post`. `snippet`: none, the
    /// ellipsis -- what stands for the text left out at either end -- or
    /// the ellipsis, `pre` and `post`. With the tags the column is the
    /// marked text; without, where the marks are (UTF-16 offsets).
    pub args: Vec<Expr>,
}

impl Mark {
    /// The column it answers under: `highlight(body)`, `snippet(body)`.
    pub fn label(&self) -> String {
        let f = if self.snippet.is_some() {
            "snippet"
        } else {
            "highlight"
        };
        format!("{f}({})", self.field)
    }

    /// The ellipsis and the tags its arguments name, each where given.
    pub fn parts(&self) -> (Option<&Expr>, Option<(&Expr, &Expr)>) {
        let a = &self.args;
        match (self.snippet.is_some(), a.len()) {
            (true, 1) => (Some(&a[0]), None),
            (true, 3) => (Some(&a[0]), Some((&a[1], &a[2]))),
            (false, 2) => (None, Some((&a[0], &a[1]))),
            _ => (None, None),
        }
    }
}

/// `facet brand top 10`: the values a field holds over every row the query
/// matches -- before `limit` and `offset` -- each with how many rows hold
/// it, most first. A list counts once a row for each value it holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Facet {
    /// A field, or a path into a json field.
    pub field: String,
    /// How many values, the commonest; None: every one, up to
    /// [`MAX_FACET_VALUES`].
    pub top: Option<Count>,
    /// `ranges [0, 2500, 5000]`: rows counted by the range their number
    /// falls in -- from each bound, included, to the next, excluded --
    /// rather than by value, every range answered in order, an empty one
    /// with 0. A value outside them all, a null and a `NaN` count in none.
    pub ranges: Option<Vec<Value>>,
    /// `disjunctive`: counted over the rows the query selects with the
    /// filter's own conditions on this field left out -- the `and` chain's
    /// terms that read it alone -- so a sidebar with a brand chosen still
    /// counts every brand the other conditions leave.
    pub disjunctive: bool,
    /// A disjunctive facet's filter: the query's without its own
    /// conditions, split off where the filter is written -- the parser,
    /// a REST `facet=` -- and before a token's rules or `@ttl`'s expiry
    /// are ANDed in, which [`Select::each_filter_mut`] then ANDs into this
    /// one too: split after them, `facet owner disjunctive` would have
    /// dropped `owner = $jwt.sub` and counted every user's rows. `None`
    /// until split ([`Select::split_facets`]).
    pub rest: Option<Option<Expr>>,
}

impl Facet {
    /// `facet <field>`: every value, counted over the rows selected.
    pub fn new(field: impl Into<String>) -> Facet {
        Facet {
            field: field.into(),
            top: None,
            ranges: None,
            disjunctive: false,
            rest: None,
        }
    }
}

/// The most values one facet answers with. Past it a facet without `top` is
/// refused rather than cut: a list cut where it happened to stop is a
/// wrong answer believed right, and a sidebar shows far fewer anyway.
pub const MAX_FACET_VALUES: usize = 10_000;

/// A facet's `top`, written or a parameter's: from 1 to
/// [`MAX_FACET_VALUES`].
fn top_in_range(field: &str, n: usize) -> Result<()> {
    if n == 0 {
        return Err(Error::Query(format!(
            "`facet {field} top 0` answers nothing: `top` takes at least 1"
        )));
    }
    if n > MAX_FACET_VALUES {
        return Err(Error::Query(format!(
            "`facet {field} top {n}`: a facet answers at most {MAX_FACET_VALUES} values"
        )));
    }
    Ok(())
}

/// One facet's answer: each value and how many matched rows hold it, by
/// count, most first, then by value.
#[derive(Debug, Clone, PartialEq)]
pub struct FacetValues {
    pub field: String,
    pub values: Vec<(Value, u64)>,
}

/// One key of `order`: `order year desc`, `order name collate tr`, `order
/// distance(loc, $1)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Sort {
    /// The field, or the path into a json field, it orders by -- over
    /// groups, the column of the list (`order sum(total) desc`). Empty for
    /// an expression.
    pub field: String,
    pub asc: bool,
    /// `collate tr`: the field's text in a language's order rather than
    /// its bytes'. None: byte order, which is what `@sorted` holds.
    pub collate: Option<Collation>,
    /// `order distance(loc, $1)`, `order price * qty desc`: a key worked
    /// out of each row rather than read -- the rows a `get` answers, and a
    /// `set` or a `del` picks, put in its order as by a field's, ties in id
    /// order. `distance(<field>, <point>)` ascending over a field with
    /// `@geo` walks the point index to the page, as `near` does; the rest
    /// is every match's key worked out and put in order. None: `field`.
    pub expr: Option<Expr>,
}

impl Sort {
    /// `field` ascending, or descending, in byte order.
    pub fn new(field: impl Into<String>, asc: bool) -> Sort {
        Sort {
            field: field.into(),
            asc,
            collate: None,
            expr: None,
        }
    }

    /// `e` ascending, or descending: a key worked out of each row.
    pub fn by(e: Expr, asc: bool) -> Sort {
        Sort {
            field: String::new(),
            asc,
            collate: None,
            expr: Some(e),
        }
    }
}

/// `order`, `limit` and `returning` on a `set` or a `del`: which of the
/// rows its filter matches it writes, and what it answers. A job queue's
/// claim is one statement --
///
/// ```text
/// set jobs {owner: $1, run_at: now() + 30000, attempts: attempts + 1}
///   where run_at <= now() order run_at limit 10 returning *
/// ```
///
/// -- the ten oldest ready jobs found and written under the one writer's
/// lock, and handed back as written: PostgreSQL's `UPDATE ... WHERE id IN
/// (SELECT ... ORDER BY ... LIMIT 10 FOR UPDATE SKIP LOCKED) RETURNING *`,
/// with the single writer standing in for the row locks. Read first and
/// written after, by a client, two workers take the same rows, and one of
/// them finds it lost only when its compare-and-set answers 0.
///
/// The rows are picked as a `get` with the same `where`, `order` and
/// `limit` would answer them (`Database::page_ids`), so `order` over an
/// `@sorted` field walks the index and stops at the page.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pick {
    pub order: Vec<Sort>,
    pub limit: Option<Count>,
    /// `returning`: the rows answered rather than counted -- a `set`'s as
    /// it wrote them, a `del`'s as they were -- in the order they were
    /// picked, under the columns a `get`'s `select` names: `None` every
    /// field after the id (`returning *`), or those listed.
    pub returning: Option<Option<Vec<String>>>,
    /// A `put`'s alone: SQL's `on conflict (email)`, the key its `if
    /// absent` names, held by the engine to the one unique key the
    /// collection's rows are found by ([`Schema::arbiter`]). A clause that
    /// shapes the write, as `returning` is; `None` for FenecQL's own `if
    /// absent`, held by every unique key.
    pub conflict: Option<Vec<String>>,
}

impl Pick {
    /// Whether it picks rather than writes every match: an `order` or a
    /// `limit`.
    pub fn picks(&self) -> bool {
        !self.order.is_empty() || self.limit.is_some()
    }
}

/// The longest reason `require <n> else '<reason>'` gives, in bytes: a
/// sentence an app tells its user or maps to one -- "insufficient funds",
/// 66 characters of a script three bytes each -- and no more, since it
/// goes out in every refusal it makes, in the 412's body, the sync's
/// refusal and the shell's line. A longer one is refused, never cut.
pub const MAX_REASON: usize = 200;

/// `require <n> [else '<reason>']`: a statement's count made a condition
/// -- the rows a write writes, a `get` answers -- and what its refusal
/// says where the count is another ([`Error::Unmet`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Require {
    pub n: u64,
    /// `else '<reason>'`: the statement's own words, a literal of its
    /// text and never a parameter, as `n` is never one -- a caller tells
    /// "insufficient funds" from "no such account" by it, where the place
    /// of the statement in its `/batch` (`at`) changes with every
    /// statement added in front. At most [`MAX_REASON`] bytes, and empty
    /// where the statement gave none: the parser refuses an empty one, so
    /// an empty box, which allocates nothing, says none, where an `Option`
    /// of it made every `require` 8 bytes more to move -- 0.3 KB of the
    /// browser module.
    pub reason: Box<str>,
}

impl Require {
    /// The clause as FenecQL writes it, ` require <n> [else '<reason>']`,
    /// the reason's quote and backslash escaped: what a sync replica sends
    /// for a write it read.
    pub fn clause_into(&self, out: &mut String) {
        out.push_str(" require ");
        out.push_str(&self.n.to_string());
        if !self.reason.is_empty() {
            out.push_str(" else '");
            for c in self.reason.chars() {
                match c {
                    '\'' => out.push_str("''"),
                    '\\' => out.push_str("\\\\"),
                    c => out.push(c),
                }
            }
            out.push('\'');
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Select {
    pub collection: String,
    /// None = all fields.
    pub project: Option<Vec<String>>,
    pub filter: Option<Expr>,
    pub near: Option<Near>,
    /// `match`: relevance ordering out of a full-text index. Named `matcher`
    /// because `match` is a Rust keyword.
    pub matcher: Option<Match>,
    /// `rerank`: exact vector ordering over the `match` candidates.
    pub rerank: Option<Rerank>,
    /// Sort keys in priority order. Empty = no ordering. Additional keys
    /// break ties: `order year desc, title asc`.
    pub order: Vec<Sort>,
    pub limit: Option<Count>,
    pub offset: Count,
    /// `count`: returns the number of matching rows instead of the rows.
    pub count: bool,
    /// `lookup`: children of another collection, attached per row.
    pub lookup: Option<Lookup>,
    /// A select list that aggregates -- `select status, sum(total), count(*)`
    /// -- in the order written, every item of it. Empty: no aggregation.
    pub aggregate: Vec<Column>,
    /// `group a, bucket(at, 1h)`: one row per distinct set of the keys'
    /// values rather than one in all. A name the list gives an item with
    /// `as` stands for that item ([`Select::group_keys`]).
    pub group: Vec<Expr>,
    /// `having <expr>` after `group`: the groups it holds for, the rest
    /// left out before `order` and `limit`. It reads a group's keys, the
    /// list's columns by their names and any aggregate, as a column does;
    /// `count` after it counts the groups that pass.
    pub having: Option<Expr>,
    /// A plain list's items that are more than a field: `px * qty as
    /// notional`. Each answers under its name, which `project` holds where
    /// it was written.
    pub computed: Vec<Column>,
    /// `fuse`: `match` and `near` both, their rankings combined.
    pub fuse: Option<Fuse>,
    /// `highlight()` and `snippet()` items of the select list, in the order
    /// written. Each answers under its label, which the list (`project`)
    /// holds where it was written, or after every field when it is `*`.
    pub marks: Vec<Mark>,
    /// `facet`: value counts over every matched row, beside the page.
    pub facets: Vec<Facet>,
    /// `require <n>`: the rows the `get` answers -- after `offset` and
    /// `limit`, not counting a `lookup`'s children -- must number `n`, or
    /// it is refused as `Error::Unmet` and the block it is in put back, as
    /// a write's `require` is: a checkout's "the price is still 12" or "the
    /// coupon is still good", read under the block's lock. With `else
    /// '<reason>'`, the refusal says why ([`Require::reason`]).
    pub require: Option<Require>,
    /// SQL's join, answered as SQL answers one: flat rows rather than the
    /// `lookup`'s children nested in their parents ([`Flat`]). Set by the
    /// SQL spelling alone -- `select o.id, c.name from orders o join
    /// customers c on c.id = o.customer_id` -- so no FenecQL text answers
    /// otherwise than it did.
    pub flat: Option<Box<Flat>>,
}

/// SQL's join read as a `lookup` and answered as its rows: one row for
/// each path down the chain -- a parent no child matched one row, `null`s
/// in the children's place, as [`ResultSet::flatten`] makes them -- each
/// column picked from its level and named as SQL names it, and the page
/// cut from those rows. Rendered as the answer is written
/// ([`ResultSet::answered`]), after a token's masks have read the levels:
/// a level's fields are held to its own collection's rules, as the
/// `lookup`'s are.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Flat {
    /// The columns of a flat row, in the order SQL's list names them.
    pub columns: Vec<FlatColumn>,
    /// The page of flat rows: where a level may hold several children of
    /// a row, its rows are not the parents', and the `get` reads as many
    /// parents as the page could need ([`Lookup`]'s keys).
    pub offset: Count,
    pub limit: Option<Count>,
    /// The fields a level shows, where a token's rules list them
    /// (fenec-http's `fields::hold`): the engine checks a `*`'s names for a
    /// clash before the levels are masked, and a field no rule grants,
    /// named in its refusal, told the token it was there -- `x.*` beside
    /// `s.place as code` was refused where `as nosuch` was answered.
    pub shown: Vec<(usize, Vec<String>)>,
    /// The names the join's statement writes ([`JoinNames`]). A statement
    /// read off a join without flat rows carries them in a `Flat` with no
    /// columns. Shared: every statement whose `in (get ...)` is answered
    /// is copied first, and a copy of the names would be allocations of
    /// every such join.
    pub names: Option<std::sync::Arc<JoinNames>>,
    /// Whether the join, read as `key in (get <joined> select id ...)`,
    /// may be read as the `lookup`'s probe instead ([`Select::probed`]),
    /// which the engine reads where that list would hold more than
    /// [`MAX_SUBQUERY_VALUES`] values. The list answers a small joined
    /// table through the first table's index, where the probe looks every
    /// row of the first table up. A flag, the probe made where it is read:
    /// made beside the list by the parser, it took a join's parse 2.25 ->
    /// 3.0 us.
    pub probe: bool,
}

/// The names a SQL join's statement writes, which the engine holds to its
/// tables' fields: the parser reads no schema.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct JoinNames {
    /// The join's tables, each as the statement names it -- its alias, or
    /// its collection -- and its collection, in the order of `from`.
    pub tables: Vec<(String, String)>,
    /// Every column the statement names, its table's place among `tables`
    /// and whether it was written with no table, which the parser takes for
    /// the first table's: the engine refuses a name its table has no field
    /// of, as SQL refuses an unknown column -- read as written, one only a
    /// joined table has was null in every row, a condition over one failed
    /// at the first row and over no rows not at all, and a LEFT JOIN's
    /// `on` the reading leaves out was never read.
    pub names: Vec<(usize, String, bool)>,
}

/// The last term of `filter`'s `and` chain that is `<field> in (get <c>
/// select id ...)` asking nothing but a filter of `c`, taken out: the field
/// and the inner `get`.
fn take_id_list(filter: &mut Option<Expr>) -> Option<(String, Box<Select>)> {
    fn listed(e: &Expr) -> bool {
        let Expr::InSelect(lhs, s) = e else {
            return false;
        };
        matches!(&**lhs, Expr::Field(_))
            && matches!(s.project.as_deref(), Some([c]) if c == "id")
            && s.limit.is_none()
            && s.offset == Count::N(0)
            && s.lookup.is_none()
            && s.aggregate.is_empty()
            && s.group.is_empty()
            && s.computed.is_empty()
            && s.near.is_none()
            && s.matcher.is_none()
            && s.require.is_none()
            && !s.count
    }
    // The term out of its `and`, the other side in the `and`'s place.
    fn take(e: &mut Expr) -> Option<Expr> {
        let Expr::And(a, b) = e else {
            return None;
        };
        if let Some(t) = take(b) {
            return Some(t);
        }
        let right = listed(b);
        if !right && !listed(a) {
            return take(a);
        }
        let Expr::And(a, b) = std::mem::replace(e, Expr::Lit(Value::Null)) else {
            return None;
        };
        let (kept, term) = if right { (a, b) } else { (b, a) };
        *e = *kept;
        Some(*term)
    }
    let term = match filter {
        Some(e) if listed(e) => filter.take()?,
        Some(e) => take(e)?,
        None => return None,
    };
    let Expr::InSelect(lhs, inner) = term else {
        return None;
    };
    let Expr::Field(key) = *lhs else {
        return None;
    };
    Some((key, inner))
}

/// A column of a [`Flat`] row: the level it is read from -- 0 the `get`'s
/// rows, 1 its `lookup`'s and so on down -- its column there, and the name
/// it answers under. An empty `column` is every column of the level, each
/// under its own name: SQL's `o.*`.
#[derive(Debug, Clone, PartialEq)]
pub struct FlatColumn {
    pub level: usize,
    pub column: String,
    pub name: String,
}

impl Select {
    /// A SQL join read as `key in (get <c> select id where <f>)` read as
    /// the `lookup`'s probe ([`Flat::probe`]): the list's term taken out of
    /// the `and` chain -- the last such term, the one the join made -- and
    /// `lookup <c> on id = key required where <f>` hung off the rows, which
    /// answer flat as they stand. `key` is in that list exactly where `c`
    /// holds a row of that `id` passing `f`, which is what `required`
    /// keeps a row for, a null key in neither. `None` where `self` holds
    /// no such term or already a `lookup`.
    pub fn probed(&self) -> Option<Select> {
        if self.lookup.is_some() || !self.flat.as_ref().is_some_and(|f| f.probe) {
            return None;
        }
        let mut out = self.clone();
        let (key, inner) = take_id_list(&mut out.filter)?;
        let Select {
            collection, filter, ..
        } = *inner;
        out.lookup = Some(Lookup {
            collection,
            on: vec!["id".to_string()],
            parent: vec![key],
            project: Some(Vec::new()),
            filter,
            required: true,
            ..Lookup::default()
        });
        let columns = match &out.project {
            None => vec![FlatColumn {
                level: 0,
                column: String::new(),
                name: String::new(),
            }],
            Some(list) => (list.iter())
                .map(|c| FlatColumn {
                    level: 0,
                    column: c.clone(),
                    name: c.clone(),
                })
                .collect(),
        };
        out.flat = Some(Box::new(Flat {
            columns,
            names: self.flat.as_ref().and_then(|f| f.names.clone()),
            ..Flat::default()
        }));
        Some(out)
    }

    /// `count` does not combine with the other clauses: projection, ordering
    /// and pagination are meaningless over a count, and `near` already cuts
    /// at its own ceiling -- it would answer "how many are there" wrongly.
    /// We raise an error instead of ignoring them silently.
    pub fn check(&self) -> Result<()> {
        // `match` and `near` both decide the ordering. A query asking for
        // both is asking two questions, and silently picking one of them
        // would answer the other one wrongly -- unless `fuse` says how the
        // two answers make one.
        if let Some(f) = &self.fuse {
            if self.matcher.is_none() || self.near.is_none() {
                return Err(Error::Query(
                    "`fuse` combines `match` and `near`: the query needs both".into(),
                ));
            }
            if self.rerank.is_some() {
                return Err(Error::Query(
                    "`fuse` and `rerank` are two ways to use a vector with `match`: pick one"
                        .into(),
                ));
            }
            if f.candidates == Some(0) {
                return Err(Error::Query(
                    "`fuse` needs at least one candidate a side".into(),
                ));
            }
            if !self.order.is_empty() {
                return Err(Error::Query(
                    "`fuse` cannot be combined with `order`: it orders by both rankings".into(),
                ));
            }
        } else if self.matcher.is_some() && self.near.is_some() {
            return Err(Error::Query(
                "`match` and `near` cannot be combined: both order the result; \
                 `fuse` ranks by both"
                    .into(),
            ));
        }
        if self.matcher.is_some() && !self.order.is_empty() {
            return Err(Error::Query(
                "`match` cannot be combined with `order`: match orders results by relevance".into(),
            ));
        }
        if self.matcher.as_ref().is_some_and(|m| m.fuzzy > MAX_FUZZY) {
            return Err(Error::Query(FUZZY_RANGE.into()));
        }
        // `rerank` reorders candidates; without `match` there are none. An
        // exact scan over a vector field is `near ... exact`, which says so.
        if self.rerank.is_some() && self.matcher.is_none() {
            return Err(Error::Query(
                "`rerank` needs `match`: it reorders the candidates match found".into(),
            ));
        }
        // `lookup` does not combine with the clauses that rank or collapse
        // the parent rows. `near`, `match` and `rerank` each order one
        // collection's documents by a score, and a score spanning a parent
        // and its children has no defensible meaning; `count` collapses the
        // very rows the children would hang from. Refused rather than
        // resolved silently against the parent.
        if let Some(l) = &self.lookup {
            let clash = if self.near.is_some() {
                "near"
            } else if self.matcher.is_some() {
                "match"
            } else if self.rerank.is_some() {
                "rerank"
            } else {
                ""
            };
            if !clash.is_empty() {
                return Err(Error::Query(format!(
                    "`lookup` cannot be used together with `{clash}`"
                )));
            }
            // `count` collapses the rows the children would hang from, so it
            // cannot combine with a `lookup` that attaches them. With
            // `required` the children only decide who is counted -- "how
            // many products have a five-star review" -- and that is a
            // question with an answer.
            if self.count && !l.required {
                return Err(Error::Query(
                    "`count` cannot be used with `lookup` unless it is `required`: \
                     there is nothing to attach children to"
                        .into(),
                ));
            }
            // Both sides would answer to the same name, so neither `on` nor
            // a child `where` could say which one it meant. Aliases would
            // fix it; there are none, so it is refused rather than guessed.
            //
            // A chain makes this a whole-query rule rather than a pairwise
            // one: `products lookup reviews lookup products` would put the
            // driving collection back in scope two levels down, where `on
            // child = parent` reaches the level above and could mean either.
            // Every collection in the chain must therefore be distinct.
            let mut seen = vec![self.collection.as_str()];
            // The list of the level above where it folds: its rows are its
            // groups, and a key is read from their columns.
            let mut above = (!self.aggregate.is_empty()).then_some(self.aggregate.as_slice());
            for (depth, step) in l.chain().enumerate() {
                if depth + 1 > MAX_LOOKUP_DEPTH {
                    return Err(Error::Query(format!(
                        "`lookup` chained too deep: at most {MAX_LOOKUP_DEPTH} levels"
                    )));
                }
                if seen.contains(&step.collection.as_str()) {
                    return Err(Error::Query(format!(
                        "`{}` cannot look itself up: both sides would answer to the same name",
                        step.collection
                    )));
                }
                seen.push(step.collection.as_str());
                // Its children are put in order a group at a time, by keys
                // read: an expression would be every group's rows worked
                // out apart, for a shape nobody has asked for.
                if step.order.iter().any(|s| s.expr.is_some()) {
                    return Err(Error::Query(
                        "a `lookup` level orders its children by fields".into(),
                    ));
                }
                step.check_key()?;
                step.check_level(above)?;
                above = step.folds().then_some(step.aggregate.as_slice());
            }
        }
        // A SQL join's flat rows are its `lookup`'s, levels and all.
        if let Some(f) = self.flat.as_deref().filter(|f| !f.columns.is_empty()) {
            let levels = self.lookup.as_ref().map_or(0, |l| l.chain().count());
            if levels == 0
                || self.count
                || !self.aggregate.is_empty()
                || f.columns.iter().any(|c| c.level > levels)
            {
                return Err(Error::Query(
                    "flat rows are a join's: a `lookup`, its rows not folded".into(),
                ));
            }
        }
        check_order(&self.order, !self.group.is_empty())?;
        self.check_marks_and_facets()?;
        // `count` and an aggregate answer one row whatever matched: a
        // `require 1` over them would always hold, and say nothing.
        if self.require.is_some()
            && (self.count || (!self.aggregate.is_empty() && self.group.is_empty()))
        {
            return Err(Error::Query(
                "`require` counts the rows a `get` answers, and `count` or an aggregate answers \
                 one: require the rows themselves, `limit 1 require 1` for one to exist"
                    .into(),
            ));
        }
        if self.having.is_some() && self.group.is_empty() {
            return Err(Error::Query(
                "`having` keeps the groups it holds for: it follows `group`".into(),
            ));
        }
        if !self.aggregate.is_empty() || !self.group.is_empty() {
            self.check_aggregate()?;
        }
        // A computed column answers under its name, and a JSON row holds a
        // name once.
        for (i, c) in self.computed.iter().enumerate() {
            let named = |n: &&String| **n == c.name;
            if self.computed[..i].iter().any(|d| d.name == c.name)
                || self.project.iter().flatten().filter(named).count() > 1
            {
                return Err(Error::Query(format!(
                    "`{}` names two columns of the list",
                    c.name
                )));
            }
        }
        if !self.count {
            return Ok(());
        }
        let clash = if self.project.is_some() {
            "select"
        } else if self.near.is_some() {
            "near"
        } else if self.matcher.is_some() {
            "match"
        } else if !self.order.is_empty() {
            "order"
        } else if self.limit.is_some() {
            "limit"
        } else if !self.offset.is_zero() {
            "offset"
        } else {
            return Ok(());
        };
        Err(Error::Query(format!(
            "`count` cannot be used together with `{clash}`"
        )))
    }

    /// `highlight`, `snippet` and `facet` need what they are over: marks
    /// the terms of a `match`, and facets a set of rows -- which `near`
    /// does not narrow: it ranks every row the filter passes, so counts over
    /// it would be the filter's alone, the same whatever the vector. Asked
    /// beside one, they are refused rather than answered for another
    /// question.
    fn check_marks_and_facets(&self) -> Result<()> {
        if let (Some(m), false) = (self.marks.first(), cfg!(feature = "highlight")) {
            let what = if m.snippet.is_some() {
                "`snippet`"
            } else {
                "`highlight`"
            };
            return Err(crate::error::not_built(what, "highlight"));
        }
        if !self.facets.is_empty() && !cfg!(feature = "facets") {
            return Err(crate::error::not_built("`facet`", "facets"));
        }
        if let Some(m) = self.marks.first() {
            let why = if self.matcher.is_none() {
                "the terms `match` found: the query needs `match`"
            } else if !self.aggregate.is_empty() {
                "a row's text; aggregates answer groups"
            } else if self.marks.iter().any(|m| m.snippet == Some(0)) {
                "at least one word: `snippet(<field>, <words>)`"
            } else if self.marks.iter().any(|m| {
                !matches!(
                    (m.snippet.is_some(), m.args.len()),
                    (_, 0) | (false, 2) | (true, 1 | 3)
                )
            }) {
                "with `highlight(<field> [, <pre>, <post>])` or \
                 `snippet(<field>, <words> [, <ellipsis> [, <pre>, <post>]])`"
            } else {
                ""
            };
            if !why.is_empty() {
                let what = if m.snippet.is_some() {
                    "snippet"
                } else {
                    "highlight"
                };
                return Err(Error::Query(format!("`{what}` marks {why}")));
            }
            // Each answers under its label, and a JSON row holds a name once.
            for (i, m) in self.marks.iter().enumerate() {
                if self.marks[..i].iter().any(|n| n.label() == m.label()) {
                    return Err(Error::Query(format!(
                        "`{}` is asked twice: its columns would answer to one name",
                        m.label()
                    )));
                }
            }
        }
        if self.facets.is_empty() {
            return Ok(());
        }
        if self.near.is_some() {
            return Err(Error::Query(
                "`facet` counts the rows a filter or `match` selects, and `near` ranks every \
                 row the filter passes: ask the facets without `near`"
                    .into(),
            ));
        }
        if !self.aggregate.is_empty() {
            return Err(Error::Query(
                "`facet` cannot be used together with aggregates: `group` counts by value".into(),
            ));
        }
        for (i, f) in self.facets.iter().enumerate() {
            if self.facets[..i].iter().any(|g| g.field == f.field) {
                return Err(Error::Query(format!("`facet {}` is asked twice", f.field)));
            }
            // A parameter's is judged where it is checked
            // (`check_counts`), as the number written is here.
            if let Some(Count::N(n)) = f.top {
                top_in_range(&f.field, n)?;
            }
            if let Some(bounds) = &f.ranges {
                // One message for every way to get them wrong: each was a
                // string of the browser module's.
                if f.top.is_some()
                    || bounds.len() < 2
                    || bounds.len() > MAX_FACET_VALUES
                    || bounds
                        .iter()
                        .any(|b| !matches!(b, Value::Int(_) | Value::Float(_)))
                    || bounds
                        .windows(2)
                        .any(|w| w[0].cmp_value(&w[1]) != std::cmp::Ordering::Less)
                {
                    return Err(Error::Query(format!(
                        "`facet {} ranges` takes 2 to 10 001 numbers, each above the one \
                         before, and no `top`: every range answers, in order",
                        f.field
                    )));
                }
            }
        }
        Ok(())
    }

    /// Number of parameters it expects: the highest `$n` in any of its
    /// clauses, a `lookup` level's and an inner `get`'s among them.
    pub fn max_param(&self) -> usize {
        let opt = |e: &Option<Expr>| e.as_ref().map(|e| e.max_param()).unwrap_or(0);
        let near = self
            .near
            .as_ref()
            .map(|n| n.vector.max_param())
            .unwrap_or(0);
        let m = self
            .matcher
            .as_ref()
            .map(|m| m.query.max_param())
            .unwrap_or(0);
        let rr = self
            .rerank
            .as_ref()
            .map(|r| r.vector.max_param())
            .unwrap_or(0);
        // A `lookup`'s `where` belongs to the same statement, so its
        // parameters count here too: a client told there are fewer sends
        // fewer, and the query then fails on an unbound `$1`.
        let counts = |l: &Option<Count>, o: &Count| l.map_or(0, Count::param).max(o.param());
        let lk = self
            .lookup
            .as_ref()
            .map(|l| {
                (l.chain())
                    .map(|s| {
                        let list = s.list_exprs().map(Expr::max_param).max().unwrap_or(0);
                        opt(&s.filter).max(counts(&s.limit, &s.offset)).max(list)
                    })
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let mut marks = counts(&self.limit, &self.offset);
        if let Some(f) = &self.flat {
            marks = marks.max(counts(&f.limit, &f.offset));
        }
        for f in &self.facets {
            marks = marks.max(f.top.map_or(0, Count::param));
        }
        for m in &self.marks {
            for a in &m.args {
                marks = marks.max(a.max_param());
            }
        }
        let items = self.aggregate.iter().chain(&self.computed);
        for e in items
            .map(|c| &c.expr)
            .chain(&self.group)
            .chain(&self.having)
        {
            marks = marks.max(e.max_param());
        }
        marks = marks.max(sorts_max_param(&self.order));
        opt(&self.filter)
            .max(near)
            .max(m)
            .max(rr)
            .max(lk)
            .max(marks)
    }

    /// Whether a literal in its filters holds a vector
    /// ([`Expr::reads_vectors`]) -- or in what else may be handed a point:
    /// `near`'s, and the items of its select list.
    pub fn reads_vectors(&self) -> bool {
        let opt = |e: &Option<Expr>| e.as_ref().is_some_and(Expr::reads_vectors);
        let mut level = self.lookup.as_ref();
        let item = |c: &Column| c.expr.reads_vectors();
        let mut any = opt(&self.filter)
            || opt(&self.having)
            || self.near.as_ref().is_some_and(|n| n.vector.reads_vectors())
            || self.group.iter().any(Expr::reads_vectors)
            || self.aggregate.iter().any(item)
            || self.computed.iter().any(item)
            || sorts_read_vectors(&self.order);
        while let (false, Some(l)) = (any, level) {
            any = opt(&l.filter) || l.list_exprs().any(Expr::reads_vectors);
            level = l.next.as_deref();
        }
        any
    }

    /// Whether an `in (get ...)` is in its filter or a `lookup` level's.
    pub fn has_subquery(&self) -> bool {
        let opt = |e: &Option<Expr>| e.as_ref().is_some_and(Expr::has_subquery);
        opt(&self.filter)
            || self
                .lookup
                .as_ref()
                .is_some_and(|l| l.chain().any(|s| opt(&s.filter)))
    }

    /// Each count of its own that is a parameter -- its `limit` or
    /// `offset`, a facet's `top`, a `lookup` level's -- held to what the
    /// number written there is held to: a value [`as_count`] refuses, by
    /// the literal's message, and a `top` from 1 to [`MAX_FACET_VALUES`];
    /// past a ranked page's ceiling the plan refuses it as it refuses the
    /// number written. An inner `get`'s are checked where it is answered.
    pub fn check_counts(&self, params: &[Value]) -> Result<()> {
        for f in &self.facets {
            if let Some(t @ Count::Param(_)) = f.top {
                top_in_range(&f.field, t.of("top", params)?)?;
            }
        }
        // A SQL join's flat page, as the select's own.
        if let Some(f) = &self.flat {
            if let Some(l @ Count::Param(_)) = f.limit {
                l.of("limit", params)?;
            }
            if let o @ Count::Param(_) = f.offset {
                o.of("offset", params)?;
            }
        }
        // The select's own, then each `lookup` level's, in one loop.
        let (mut limit, mut offset) = (self.limit, self.offset);
        let mut level = self.lookup.as_ref();
        // A number written is looked at and passed over: no call a page.
        loop {
            if let Some(l @ Count::Param(_)) = limit {
                l.of("limit", params)?;
            }
            if let o @ Count::Param(_) = offset {
                o.of("offset", params)?;
            }
            let Some(l) = level else {
                return Ok(());
            };
            (limit, offset) = (l.limit, l.offset);
            level = l.next.as_deref();
        }
    }

    /// Whether a filter of its own or of a `lookup` level calls `now()`
    /// ([`Expr::fold_now`]); an inner `get`'s is asked where it is
    /// answered.
    pub fn calls_now(&self) -> bool {
        let opt = |e: &Option<Expr>| e.as_ref().is_some_and(Expr::calls_now);
        opt(&self.filter)
            || self.facets.iter().any(|f| f.rest.as_ref().is_some_and(opt))
            || self
                .lookup
                .as_ref()
                .is_some_and(|l| l.chain().any(|s| opt(&s.filter)))
    }

    /// Calls `f` on each filter it holds, its own and each `lookup`
    /// level's, with the collection the filter is over.
    pub fn each_filter_mut(
        &mut self,
        f: &mut dyn FnMut(&str, &mut Option<Expr>) -> Result<()>,
    ) -> Result<()> {
        f(&self.collection, &mut self.filter)?;
        // A disjunctive facet's filter is the query's, short of its own
        // conditions: what holds the query holds it.
        for facet in &mut self.facets {
            if let Some(rest) = &mut facet.rest {
                f(&self.collection, rest)?;
            }
        }
        let mut level = self.lookup.as_mut();
        while let Some(l) = level {
            f(&l.collection, &mut l.filter)?;
            level = l.next.as_deref_mut();
        }
        Ok(())
    }

    /// Splits each disjunctive facet's filter off the query's
    /// ([`Facet::rest`]): the `and` chain without the terms that read the
    /// facet's field alone. A term reading it beside another field -- `brand
    /// = $1 or sale` -- cannot be left out alone and is refused, rather
    /// than kept, which would count it, or dropped, which would drop the
    /// other field's condition. Idempotent: a facet split already stays.
    pub fn split_facets(&mut self) -> Result<()> {
        // A build without facets refuses them where the query is checked.
        if !cfg!(feature = "facets") {
            return Ok(());
        }
        for i in 0..self.facets.len() {
            if !self.facets[i].disjunctive || self.facets[i].rest.is_some() {
                continue;
            }
            let field = self.facets[i].field.clone();
            let mut terms = Vec::new();
            let mut todo: Vec<&Expr> = self.filter.iter().collect();
            while let Some(e) = todo.pop() {
                match e {
                    Expr::And(a, b) => {
                        todo.push(b);
                        todo.push(a);
                    }
                    e => terms.push(e),
                }
            }
            let mut rest: Option<Expr> = None;
            let mut names = Vec::new();
            let mut dropped = false;
            for t in terms {
                names.clear();
                t.referenced_fields(&mut names);
                // By name: a path is a field of its own here, as it is to an
                // index (`facet meta.lang` leaves `meta.lang = $1` out, and
                // not `meta.source = $2`).
                let own = names.contains(&field);
                let other = names.iter().any(|n| *n != field);
                if own && other {
                    return Err(Error::Query(format!(
                        "`facet {field} disjunctive` leaves the filter's conditions on `{field}` \
                         out, and one of them reads another field too: write it as a condition \
                         of its own, joined by `and`"
                    )));
                }
                if own {
                    dropped = true;
                } else {
                    rest = Some(match rest {
                        None => t.clone(),
                        Some(r) => Expr::And(Box::new(r), Box::new(t.clone())),
                    });
                }
            }
            // A filter with no condition of the field's own is the query's:
            // counted as a plain facet, over the rows the query found, rather
            // than over the same rows found again.
            match dropped {
                true => self.facets[i].rest = Some(rest),
                false => self.facets[i].disjunctive = false,
            }
        }
        Ok(())
    }

    /// What an inner `get` must be to answer `in (get ...)`: one column,
    /// which is the list -- a field named, or one aggregate -- and nothing
    /// that attaches children to its rows or counts them in its place.
    pub fn check_subquery(&self) -> Result<()> {
        let one = match (&self.project, self.aggregate.len()) {
            (_, 1) => true,
            (Some(cols), 0) => cols.len() == 1,
            _ => false,
        };
        if !one || self.count {
            return Err(Error::Query(format!(
                "`in (get {} ...)` takes one column: `select` exactly one field",
                self.collection
            )));
        }
        if self.lookup.is_some() {
            return Err(Error::Query(
                "`in (get ...)` takes one column, and a `lookup` attaches children to it".into(),
            ));
        }
        if self.require.is_some() {
            return Err(Error::Query(
                "`in (get ...)` takes no `require`: the statement around it does".into(),
            ));
        }
        self.check()
    }

    /// Aggregates follow `count`'s rules: they collapse rows, so nothing that
    /// ranks the rows combines with them. Ungrouped, `order`, `limit` and
    /// `offset` pick the rows the one row folds -- what the `get` would
    /// answer without the aggregates, `limit 50` its 50 latest; grouped,
    /// the rows are the groups, and those they order and page. A `lookup`
    /// hangs off the rows either way, keyed by their columns.
    fn check_aggregate(&self) -> Result<()> {
        check_folds(&self.aggregate, &self.group, self.having.as_ref())?;
        if self.group.is_empty() {
            picks_by_rows(&self.aggregate, &self.order)?;
        }
        let clash = if self.near.is_some() {
            "near"
        } else if self.matcher.is_some() {
            "match"
        } else if self.count && self.group.is_empty() {
            "count"
        } else if self.project.is_some() {
            "select"
        } else {
            return Ok(());
        };
        Err(Error::Query(format!(
            "aggregates cannot be used together with `{clash}`"
        )))
    }

    /// The group's keys as the rows are grouped by them ([`group_keys`]).
    pub fn group_keys(&self) -> Vec<&Expr> {
        group_keys(&self.aggregate, &self.group)
    }
}

/// The group's keys as the rows are grouped by them: a name the list gives
/// an item with `as` -- `select bucket(at, 1h) as hour, count(*) group
/// hour` -- stands for that item's expression, before a field of the same
/// name; anything else is the key as written. A `get`'s and a `lookup`
/// level's alike.
pub fn group_keys<'a>(list: &'a [Column], group: &'a [Expr]) -> Vec<&'a Expr> {
    let mut out = Vec::with_capacity(group.len());
    for g in group {
        let named = match g {
            Expr::Field(n) => list.iter().find(|c| c.name == *n),
            _ => None,
        };
        out.push(named.map_or(g, |c| &c.expr));
    }
    out
}

/// An ungrouped list's `order` picks the rows it folds, by what a row
/// holds: a key naming one of its columns -- `order sum(n)`, an alias --
/// would order the one row it answers, and is refused rather than read as
/// a field that may share the name.
fn picks_by_rows(list: &[Column], order: &[Sort]) -> Result<()> {
    match order
        .iter()
        .find(|s| list.iter().any(|c| c.name == s.field))
    {
        Some(s) => Err(Error::Query(format!(
            "`order {}`: ungrouped, `order` picks the rows the list folds, by what a row \
             holds; `group` makes rows to order by their columns",
            s.field
        ))),
        None => Ok(()),
    }
}

/// What an aggregating list must be, a `get`'s or a `lookup` level's: an
/// aggregate to answer with where it groups, keys that are the rows'
/// values, items each a group key or folded ([`grouped`]) and named once,
/// and a `having` that reads what an item may and the items by name.
fn check_folds(list: &[Column], group: &[Expr], having: Option<&Expr>) -> Result<()> {
    if !group.is_empty() && list.is_empty() {
        return Err(Error::Query(
            "`group` needs an aggregate to answer with: `select <key>, count(*)`".into(),
        ));
    }
    if having.is_some() && group.is_empty() {
        return Err(Error::Query(
            "`having` keeps the groups it holds for: it follows `group`".into(),
        ));
    }
    let keys = group_keys(list, group);
    for k in &keys {
        if k.has_aggregate() {
            return Err(Error::Query(
                "`group` takes the rows' values; an aggregate is the group's own".into(),
            ));
        }
    }
    for (i, c) in list.iter().enumerate() {
        grouped(&c.expr, &keys, false)?;
        if list[..i].iter().any(|d| d.name == c.name) {
            return Err(Error::Query(format!(
                "`{}` names two columns of the list: name one with `as`",
                c.name
            )));
        }
    }
    // `having` reads what a column may, and the columns by their names.
    if let Some(h) = having {
        let names: Vec<Expr> = list.iter().map(|c| Expr::Field(c.name.clone())).collect();
        let mut known = keys.clone();
        known.extend(&names);
        grouped(h, &known, false)?;
    }
    Ok(())
}

/// Refuses what an item of an aggregating list cannot answer: a field read
/// outside an aggregate that is no group key -- which value of the group's
/// rows would it be? -- an aggregate inside another, `distinct` anywhere
/// but in `count`, and an aggregate given the wrong arguments. `inside`:
/// within an aggregate's arguments, where the rows' fields are what it reads.
fn grouped(e: &Expr, keys: &[&Expr], inside: bool) -> Result<()> {
    if !inside && keys.iter().any(|k| is_key(k, e)) {
        return Ok(());
    }
    let Expr::Call(name, args) = e else {
        if let (Expr::Field(f), false) = (e, inside) {
            return Err(Error::Query(format!(
                "`{f}` is neither aggregated nor grouped by: group by it, or by the name \
                 an item reading it is given with `as`"
            )));
        }
        let mut r = Ok(());
        e.each_child(&mut |c| {
            if r.is_ok() {
                r = grouped(c, keys, inside);
            }
        });
        return r;
    };
    let shape = match (name.as_str(), args.as_slice()) {
        ("distinct", _) => "`distinct` counts values inside `count`: `count(distinct <field>)`",
        _ if e.aggregate_name().is_some() && inside => {
            "an aggregate inside another has no rows to fold: they are the outer one's"
        }
        ("count", [Expr::Call(d, x)]) if d == "distinct" && x.len() == 1 => {
            return grouped(&x[0], keys, true);
        }
        ("count", []) => return Ok(()),
        ("count", [_, _, ..]) => {
            "`count` counts rows, values or distinct values: `count(*)`, `count(<value>)`, \
             `count(distinct <value>)`"
        }
        // SQL's `count(x)`: the rows where `x` is not null.
        ("count", [_])
        | ("first" | "last", [_] | [_, _])
        | (
            "sum"
            | "avg"
            | "min"
            | "max"
            | "approx_count_distinct"
            | "hll_accumulate"
            | "hll_combine",
            [_],
        ) => "",
        ("first" | "last", _) => {
            "`first` and `last` take a value and what orders the rows: \
                                  `first(<value> [by <key>])`"
        }
        ("min" | "max", [_, _, ..]) => {
            "`min` and `max` fold one value over the rows; `least(a, b)` and \
             `greatest(a, b)` take the smaller and larger of a row's"
        }
        (
            "sum"
            | "avg"
            | "min"
            | "max"
            | "approx_count_distinct"
            | "hll_accumulate"
            | "hll_combine",
            _,
        ) => "an aggregate takes one value: `sum(<value>)`",
        _ => "",
    };
    if !shape.is_empty() {
        return Err(Error::Query(shape.into()));
    }
    let inside = inside || e.aggregate_name().is_some();
    for a in args {
        grouped(a, keys, inside)?;
    }
    Ok(())
}

/// Whether `e` is the group key `k`: the same field, or the very item a
/// key names by the name `as` gives it. Not the same expression written
/// twice -- `select bucket(at, 1h) as h ... group h` names it once -- since
/// comparing two expressions whole was 1.5 KB of the browser module.
pub fn is_key(k: &Expr, e: &Expr) -> bool {
    std::ptr::eq(k, e) || matches!((k, e), (Expr::Field(a), Expr::Field(b)) if a == b)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateCollection {
        schema: Schema,
        if_not_exists: bool,
    },
    DropCollection {
        name: String,
        if_exists: bool,
    },
    /// Builds an index on an existing field.
    ///
    /// Required for the bulk-load pattern: write the data first (~1M
    /// documents per second), then build the index in one pass. Updating the
    /// graph on every insert does the same work far more expensively.
    CreateIndex {
        collection: String,
        field: String,
        kind: crate::schema::IndexKind,
        if_not_exists: bool,
    },
    /// `create index on posts (owner, created desc) @sorted`, or `@unique`:
    /// an index over several fields ([`crate::schema::Composite`]), built
    /// from the documents there as `create index` builds one on a field.
    CreateComposite {
        collection: String,
        index: crate::schema::Composite,
        if_not_exists: bool,
    },
    /// `drop index on posts (owner, created)`: an index over several fields
    /// taken off, named by its fields. A field's own index goes with a
    /// rebuild of the field, as the declared schema's plan says.
    DropComposite {
        collection: String,
        keys: Vec<String>,
        if_exists: bool,
    },
    /// `alter collection <name> add field | drop field | rename field`: the
    /// fields change, and no document is rewritten for it (`Alter`).
    AlterCollection {
        collection: String,
        change: Alter,
    },
    Put {
        collection: String,
        /// Field-expression pairs per document. Supplying `id` makes it an upsert.
        docs: Vec<Vec<(String, Expr)>>,
        /// `put <name> $n`: the documents are the parameter's value, an
        /// object or a list of objects, each member a field -- read as a
        /// body's are, a json field's numbers as written. `docs` is empty.
        docs_param: Option<usize>,
        /// `insert`: a document naming an id that is taken is refused
        /// (`Error::Duplicate`), where `put` writes over it.
        insert: bool,
        /// `put ... if absent`: a document whose id, or a `@unique` value
        /// of, a live row holds already is passed over rather than
        /// refused, and not counted -- `SET NX`, whose answer (0 or 1) says
        /// whether the write was made. The parser sets `insert` with it.
        if_absent: bool,
        /// `put ... if absent else set {..}`: the row holding the id or the
        /// `@unique` value is set as a `set` sets it, each value over that
        /// row, `new.f` the document's own `f`, and counted -- an upsert,
        /// one statement under the write lock.
        else_set: Option<Vec<(String, Expr)>>,
        /// `... require <n>`: the statement is refused (`Error::Unmet`), and
        /// so its block put back whole, unless it wrote exactly `n` rows;
        /// with `else '<reason>'`, saying why.
        require: Option<Require>,
        /// `returning * | a, b`: the rows it wrote answered rather than
        /// counted, ids and all, in the order of its documents -- a row
        /// `if absent` passed over is neither, the row `else set` set is.
        /// A `Pick` as a `set`'s is, its `order` and `limit` empty: a put
        /// picks no rows.
        pick: Option<Box<Pick>>,
    },
    Select(Select),
    /// `explain get ...`: the query runs, and what comes back is the path it
    /// took -- which index answered, how many rows each stage read -- one
    /// row a step, in a single `plan` column.
    Explain(Select),
    Update {
        collection: String,
        set: Vec<(String, Expr)>,
        filter: Option<Expr>,
        /// As `Put`'s: the rows it changed must be exactly this many.
        require: Option<Require>,
        /// `order`, `limit`, `returning`: boxed, since every other `set`
        /// has none and a `Statement` is as large as its largest variant.
        pick: Option<Box<Pick>>,
    },
    Delete {
        collection: String,
        filter: Option<Expr>,
        /// As `Put`'s: the rows it deleted must be exactly this many.
        require: Option<Require>,
        /// As `Update`'s.
        pick: Option<Box<Pick>>,
    },
    ListCollections,
    Describe(String),
    Compact(Option<String>),
    /// `create [or replace] statement <name>(...) ... as <body>`: a named
    /// statement's definition ([`crate::named`]). Boxed, as each variant
    /// past a select's size would grow every `Statement`. Like each of
    /// named statements' variants, in a build with them alone (the
    /// `statements` feature): the browser module's parser reads none.
    #[cfg(feature = "statements")]
    CreateStatement(Box<crate::named::Definition>),
    /// `drop statement [if exists] <name>`.
    #[cfg(feature = "statements")]
    DropStatement {
        name: String,
        if_exists: bool,
    },
    /// `statements`: the named statements, as `collections` lists the
    /// collections.
    #[cfg(feature = "statements")]
    ListStatements,
    /// `describe statement <name>`; `describe statement` alone is the
    /// collection named `statement`'s description, as it always was.
    #[cfg(feature = "statements")]
    DescribeStatement(String),
    /// `call <name> ...` and SQL's `CALL <name>(...)`. Boxed, as
    /// `CreateStatement` is.
    #[cfg(feature = "statements")]
    Call(Box<crate::named::Call>),
}

/// What `alter collection` changes. A document is a run of values in field
/// order, so each change is one no document has to be rewritten for: a
/// field added goes last, and a document written before it ends before its
/// place, which reads as `null`; a field dropped leaves its place, skipped
/// on read and written as `null` until a `compact`; a rename is the schema
/// alone. A change of type would be every document rewritten under the
/// write lock, and is not one of them.
#[derive(Debug, Clone, PartialEq)]
pub enum Alter {
    AddField(crate::schema::Field),
    DropField(String),
    RenameField(String, String),
    /// `alter field seen @ttl(1h)`, or `@sorted` to let the rows live: a
    /// `@sorted` or `@ttl` field's expiry set or taken off. The ordered
    /// index stays as it is, and no document is read.
    Ttl(String, Option<u64>),
}

/// Whether `v` is or holds a vector: what a reader with no schema makes of a
/// list of numbers alone.
pub fn holds_vector(v: &Value) -> bool {
    match v {
        Value::Vector(_) => true,
        Value::List(items) => items.iter().any(holds_vector),
        Value::Object(m) => m.iter().any(|(_, v)| holds_vector(v)),
        _ => false,
    }
}

impl Expr {
    /// Whether a literal anywhere in it holds a vector, as a list of
    /// numbers alone is read: what a json field could be handed instead of
    /// the numbers written ([`crate::engine::Database::exactly`]).
    pub fn reads_vectors(&self) -> bool {
        match self {
            Expr::Lit(v) => holds_vector(v),
            Expr::Field(_) | Expr::Param(_) => false,
            Expr::And(a, b)
            | Expr::Or(a, b)
            | Expr::Cmp(_, a, b)
            | Expr::Like(a, b)
            | Expr::Has(a, b)
            | Expr::Arith(_, a, b) => a.reads_vectors() || b.reads_vectors(),
            Expr::Not(a) | Expr::IsNull(a) => a.reads_vectors(),
            Expr::In(a, items) => a.reads_vectors() || items.iter().any(Expr::reads_vectors),
            Expr::InSelect(a, sel) => a.reads_vectors() || sel.reads_vectors(),
            Expr::Call(_, args) => args.iter().any(Expr::reads_vectors),
        }
    }
}

/// The documents parameter `i` holds for `put <collection> $i`: an object,
/// or a list of objects, each a document's members.
pub fn documents_in<'p>(
    collection: &str,
    i: usize,
    params: &'p [Value],
) -> Result<Vec<&'p [(String, Value)]>> {
    let refused = |what: &str| {
        Error::Type(format!(
            "`put {collection} ${}` takes an object or a list of objects, not {what}",
            i + 1
        ))
    };
    match params.get(i) {
        None => Err(unbound(i)),
        Some(Value::Object(m)) => Ok(vec![m.as_slice()]),
        Some(Value::List(items)) => {
            // A loop: collected into a `Result`, the iterator's machinery
            // was 0.5 KB more of the browser module.
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::Object(m) => out.push(m.as_slice()),
                    other => return Err(refused(&format!("a list holding {}", other.type_name()))),
                }
            }
            Ok(out)
        }
        Some(other) => Err(refused(other.type_name())),
    }
}

impl Statement {
    /// Whether a literal in it holds a vector: when none does, no json
    /// field can be handed one, and nothing has to be asked of the schema.
    pub fn reads_vectors(&self) -> bool {
        let opt = |e: &Option<Expr>| e.as_ref().is_some_and(Expr::reads_vectors);
        let picked =
            |p: &Option<Box<Pick>>| p.as_ref().is_some_and(|p| sorts_read_vectors(&p.order));
        match self {
            Statement::Put { docs, else_set, .. } => docs
                .iter()
                .flatten()
                .chain(else_set.iter().flatten())
                .any(|(_, e)| e.reads_vectors()),
            Statement::Update {
                set, filter, pick, ..
            } => set.iter().any(|(_, e)| e.reads_vectors()) || opt(filter) || picked(pick),
            Statement::Delete { filter, pick, .. } => opt(filter) || picked(pick),
            Statement::Select(s) | Statement::Explain(s) => s.reads_vectors(),
            #[cfg(feature = "statements")]
            Statement::Call(c) => c.args.exprs().any(Expr::reads_vectors),
            _ => false,
        }
    }

    /// Statements that need no write access. The server runs these under a
    /// shared (read) lock; the others take the exclusive write lock.
    ///
    /// A `call` is never one, whatever its body: a definition read before
    /// the lock may be replaced before it is taken, and a body judged a
    /// read that writes under the read lock is a write with readers beside
    /// it (`docs/design/named-statements.md`).
    pub fn is_read_only(&self) -> bool {
        match self {
            Statement::Select(_)
            | Statement::Explain(_)
            | Statement::ListCollections
            | Statement::Describe(_) => true,
            #[cfg(feature = "statements")]
            Statement::ListStatements | Statement::DescribeStatement(_) => true,
            _ => false,
        }
    }

    /// Whether it defines, drops or calls a named statement: what nothing
    /// runs yet ([`crate::named::NOT_BUILT`]).
    #[cfg(feature = "statements")]
    pub fn is_named(&self) -> bool {
        matches!(
            self,
            Statement::CreateStatement(_) | Statement::DropStatement { .. } | Statement::Call(_)
        )
    }

    /// Statements a block of writes may hold: every one but a compact,
    /// which rewrites the file -- nothing a block's undo could put back. A
    /// schema change is put back as a write is: a collection made goes, one
    /// dropped comes back, an index built goes.
    pub fn fits_block(&self) -> bool {
        !matches!(self, Statement::Compact(_))
    }

    /// A `put <name> $n` with the parameter's documents written into it,
    /// each value a literal: what a scope's rules -- no `id`, its pinned
    /// fields -- and a sync replica's matching read a document by. Any
    /// other statement as it is.
    pub fn with_documents(self, params: &[Value]) -> Result<Statement> {
        let Statement::Put {
            collection,
            mut docs,
            docs_param: Some(i),
            insert,
            if_absent,
            else_set,
            require,
            pick,
        } = self
        else {
            return Ok(self);
        };
        for members in documents_in(&collection, i, params)? {
            docs.push(
                members
                    .iter()
                    .map(|(k, v)| (k.clone(), Expr::Lit(v.clone())))
                    .collect(),
            );
        }
        Ok(Statement::Put {
            collection,
            docs,
            docs_param: None,
            insert,
            if_absent,
            else_set,
            require,
            pick,
        })
    }

    /// Number of parameters the statement expects: the highest `$n` used.
    pub fn max_param(&self) -> usize {
        let opt = |e: &Option<Expr>| e.as_ref().map(|e| e.max_param()).unwrap_or(0);
        let pairs =
            |v: &Vec<(String, Expr)>| v.iter().map(|(_, e)| e.max_param()).max().unwrap_or(0);
        let picked = |p: &Option<Box<Pick>>| {
            p.as_ref().map_or(0, |p| {
                p.limit
                    .map_or(0, Count::param)
                    .max(sorts_max_param(&p.order))
            })
        };
        match self {
            Statement::Put {
                docs,
                docs_param,
                else_set,
                ..
            } => docs
                .iter()
                .chain(else_set)
                .map(pairs)
                .max()
                .unwrap_or(0)
                .max(docs_param.map_or(0, |i| i + 1)),
            Statement::Select(sel) | Statement::Explain(sel) => sel.max_param(),
            Statement::Update {
                set, filter, pick, ..
            } => pairs(set).max(opt(filter)).max(picked(pick)),
            Statement::Delete { filter, pick, .. } => opt(filter).max(picked(pick)),
            // A definition's parameters are its calls', numbered in its
            // body: the text that defines it is handed none.
            #[cfg(feature = "statements")]
            Statement::Call(c) => c
                .args
                .max_param()
                .max(c.claims.as_ref().map_or(0, |a| a.max_param())),
            _ => 0,
        }
    }

    /// Whether it will return rows: a read, or a write with `returning`.
    pub fn returns_rows(&self) -> bool {
        self.is_read_only() || self.pick().is_some_and(|p| p.returning.is_some())
    }

    /// A `set`'s or a `del`'s `order`, `limit` and `returning`, or a
    /// `put`'s `returning`, if it has any.
    pub fn pick(&self) -> Option<&Pick> {
        match self {
            Statement::Update { pick, .. }
            | Statement::Delete { pick, .. }
            | Statement::Put { pick, .. } => pick.as_deref(),
            _ => None,
        }
    }
}

/// Query result.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: DocId,
    pub values: Vec<Value>,
    /// Similarity score, when `near` was used.
    pub score: Option<f32>,
}

/// Children attached by `lookup`, grouped per parent row.
///
/// The grouping sits beside the rows instead of inside `Value`: a
/// `Value::Object` is a `json` field's value, which a filter reads into,
/// while children are rows of another collection with ids and scores of
/// their own, and the PostgreSQL wire flattens them into a join's shape
/// (`ResultSet::flatten`) rather than send an object a cell. It is also the
/// shape the codebase already uses to answer for more than one collection:
/// `Response::Schemas` goes long rather than wide.
#[derive(Debug, Clone, PartialEq)]
pub struct Nested {
    /// The looked-up collection's name, and the key it serialises under.
    pub name: String,
    pub columns: Vec<String>,
    /// One group per row of the level above, in the same order.
    pub groups: Vec<Vec<Row>>,
    /// The next level down, attached to the rows of *this* one.
    ///
    /// A tree stored one level at a time: `nested.groups` holds one group
    /// per row of `groups` read left to right, concatenated. The alignment
    /// rule is therefore the same sentence at every depth, and a level costs
    /// one vector rather than a node per row -- which matters, because the
    /// row count multiplies going down.
    pub nested: Option<Box<Nested>>,
    /// The first level's alone: the rows answered flat, a SQL join's
    /// ([`Select::flat`]), its page's numbers worked out.
    pub flat: Option<Box<Flat>>,
}

impl Nested {
    /// The rows of this level, in the order the level below is aligned to.
    pub fn rows(&self) -> impl Iterator<Item = &Row> {
        self.groups.iter().flatten()
    }

    /// The group belonging to row `i` of the level above. A row with no
    /// matches has an empty group, not a missing one.
    pub fn group(&self, i: usize) -> &[Row] {
        self.groups.get(i).map_or(&[], |g| g.as_slice())
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
    /// Set only by `lookup`; `None` for every query that could be written
    /// before it existed.
    pub nested: Option<Nested>,
    /// What `facet` counted, a facet each in the order asked; empty when
    /// none was. Beside the rows, as `nested` is, and never in a `Value`:
    /// it answers for the query's whole set, not for a row.
    pub facets: Vec<FacetValues>,
}

impl ResultSet {
    /// What `count` answers: one row of one column, and the facets asked
    /// beside it. Made in one place for its three callers.
    #[inline(never)]
    pub(crate) fn counted(n: usize, facets: Vec<FacetValues>) -> ResultSet {
        ResultSet {
            columns: vec![COUNT_COLUMN.to_string()],
            rows: vec![Row {
                id: 0,
                values: vec![Value::Int(n as i64)],
                score: None,
            }],
            nested: None,
            facets,
        }
    }

    /// The same data as one flat table: the parent columns, then the child's
    /// under `<collection>.<field>`, one row per pair.
    ///
    /// This is what a transport that cannot carry nesting gets -- the
    /// PostgreSQL wire, which has no nested row, and the terminal table. The
    /// shape is exactly a join's, and since `lookup` caps children per parent
    /// it is a join's shape with the per-parent limit a join cannot express.
    /// A parent with no children keeps one row with the child columns null,
    /// because the page is the parents either way.
    ///
    /// A chain renders the same way, one column block per level: a row is
    /// one root-to-leaf path, and a level that ran out of matches fills its
    /// block -- and every block below it -- with nulls. That is a left join's
    /// shape repeated, which is the only rendering a transport with no
    /// nested row can be given; what it loses is the per-level `limit`,
    /// still readable in the nesting the other transports keep.
    ///
    /// `Row::id` stays the parent's. It is never serialised -- both JSON
    /// writers emit columns and values only -- so a repeated id here reaches
    /// nobody who could be misled by it.
    /// Borrowed unchanged when there is nothing nested, so the transports
    /// that call it on every query do not pay for a clone they do not need.
    ///
    /// A SQL join's ([`Nested::flat`]) is its rows as SQL answers them: the
    /// columns its list names, under SQL's names, and its page.
    pub fn flatten(&self) -> std::borrow::Cow<'_, ResultSet> {
        let Some(n) = &self.nested else {
            return std::borrow::Cow::Borrowed(self);
        };
        let all = self.paths(n);
        std::borrow::Cow::Owned(match &n.flat {
            None => all,
            Some(f) => self.picked(n, f, all),
        })
    }

    /// What a transport writes: a SQL join's flat rows ([`Select::flat`]),
    /// and any other result as it is, its children nested. Every JSON
    /// writer asks it, after a token's masks have read the levels.
    pub fn answered(&self) -> std::borrow::Cow<'_, ResultSet> {
        match self.nested.as_ref().is_some_and(|n| n.flat.is_some()) {
            true => self.flatten(),
            false => std::borrow::Cow::Borrowed(self),
        }
    }

    /// The flat rows of a SQL join out of every path's: each column its
    /// list names, read from its level's block -- a column a token's mask
    /// took off reads `null` -- and the page cut from them.
    #[inline(never)]
    fn picked(&self, n: &Nested, f: &Flat, all: ResultSet) -> ResultSet {
        // Each level's columns, and where its block starts in a path.
        let mut levels: Vec<(&[String], usize)> = vec![(&self.columns, 0)];
        let (mut level, mut at) = (Some(n), self.columns.len());
        while let Some(l) = level {
            levels.push((&l.columns, at));
            at += l.columns.len();
            level = l.nested.as_deref();
        }
        let (mut places, mut columns) = (Vec::new(), Vec::new());
        for c in &f.columns {
            let Some(&(cols, start)) = levels.get(c.level) else {
                continue;
            };
            if c.column.is_empty() {
                for (j, name) in cols.iter().enumerate() {
                    places.push(start + j);
                    columns.push(name.clone());
                }
                continue;
            }
            let j = cols.iter().position(|x| *x == c.column);
            places.push(j.map_or(usize::MAX, |j| start + j));
            columns.push(c.name.clone());
        }
        let limit = f.limit.map_or(usize::MAX, |l| l.at(&[]));
        let mut rows = Vec::new();
        for r in all.rows.into_iter().skip(f.offset.at(&[])).take(limit) {
            let mut values = Vec::with_capacity(places.len());
            for &p in &places {
                values.push(r.values.get(p).cloned().unwrap_or(Value::Null));
            }
            rows.push(Row {
                id: r.id,
                values,
                score: None,
            });
        }
        ResultSet {
            columns,
            rows,
            nested: None,
            facets: all.facets,
        }
    }

    /// Every path down the nesting as a row: the parent's columns, then
    /// each level's under `<collection>.<field>`.
    fn paths(&self, n: &Nested) -> ResultSet {
        let mut columns = self.columns.clone();
        // Each level contributes a block of columns, and the prefix sums of
        // its groups turn "row `t` of group `k`" into the single index the
        // level below is keyed by -- the alignment `Nested::rows` describes.
        let mut levels: Vec<(&Nested, Vec<usize>)> = Vec::new();
        let mut level = Some(n);
        while let Some(l) = level {
            columns.extend(l.columns.iter().map(|c| format!("{}.{}", l.name, c)));
            let mut prefix = Vec::with_capacity(l.groups.len());
            let mut acc = 0;
            for g in &l.groups {
                prefix.push(acc);
                acc += g.len();
            }
            levels.push((l, prefix));
            level = l.nested.as_deref();
        }
        let width = columns.len();
        let mut rows = Vec::with_capacity(self.rows.len());
        let mut path = Vec::with_capacity(width);
        for (i, row) in self.rows.iter().enumerate() {
            path.clear();
            path.extend(row.values.iter().cloned());
            flatten_level(&mut rows, &mut path, &levels, 0, i, row, width);
        }
        ResultSet {
            columns,
            rows,
            nested: None,
            facets: self.facets.clone(),
        }
    }
}

/// One output row per root-to-leaf path through the nesting.
///
/// A level with nothing for the row above writes nulls for its own block and
/// for every block below it, and stops: the page is the parents, so the path
/// still produces a row. Recursion is bounded by `MAX_LOOKUP_DEPTH`.
fn flatten_level(
    out: &mut Vec<Row>,
    path: &mut Vec<Value>,
    levels: &[(&Nested, Vec<usize>)],
    depth: usize,
    group: usize,
    root: &Row,
    width: usize,
) {
    let mut leaf = |path: &mut Vec<Value>| {
        let mark = path.len();
        path.resize(width, Value::Null);
        out.push(Row {
            id: root.id,
            values: path.clone(),
            score: root.score,
        });
        path.truncate(mark);
    };
    let Some((n, prefix)) = levels.get(depth) else {
        leaf(path);
        return;
    };
    let children = n.group(group);
    if children.is_empty() {
        leaf(path);
        return;
    }
    let base = prefix[group];
    for (t, child) in children.iter().enumerate() {
        let mark = path.len();
        path.extend(child.values.iter().cloned());
        flatten_level(out, path, levels, depth + 1, base + t, root, width);
        path.truncate(mark);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    /// Returns rows.
    Rows(ResultSet),
    /// Number of affected records.
    Affected(usize),
    /// Informational message (DDL).
    Ok(String),
    /// Collection schemas.
    Schemas(Vec<Schema>),
}

impl Response {
    pub fn rows(&self) -> Option<&ResultSet> {
        match self {
            Response::Rows(r) => Some(r),
            _ => None,
        }
    }
}

/// Turns the projection list into column names.
pub fn projection_columns(schema: &Schema, project: &Option<Vec<String>>) -> Vec<String> {
    match project {
        Some(cols) => cols.clone(),
        None => {
            let mut c = vec!["id".to_string()];
            c.extend(schema.fields.iter().map(|f| f.name.clone()));
            c
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `like` by its definition, over characters: what the pattern's first
    /// piece takes, the rest matched against what is left, every split
    /// tried.
    fn like_by_definition(h: &[char], p: &[char], e: Option<char>) -> Option<bool> {
        let Some(&c) = p.first() else {
            return Some(h.is_empty());
        };
        if Some(c) == e {
            let &lit = p.get(1)?;
            return match h.first() {
                Some(&x) if x == lit => like_by_definition(&h[1..], &p[2..], e),
                _ => {
                    // The rest must still be read whole, to refuse a
                    // pattern ending in its escape whatever the text.
                    like_by_definition(&[], &p[2..], e)?;
                    Some(false)
                }
            };
        }
        if c == '%' {
            let mut any = false;
            for k in 0..=h.len() {
                any |= like_by_definition(&h[k..], &p[1..], e)?;
            }
            return Some(any || (h.is_empty() && like_by_definition(h, &p[1..], e)?));
        }
        match h.first() {
            Some(&x) if c == '_' || x == c => like_by_definition(&h[1..], &p[1..], e),
            _ => {
                like_by_definition(&[], &p[1..], e)?;
                Some(false)
            }
        }
    }

    /// The matcher answers every text and pattern of a small alphabet --
    /// letters of one, two and four bytes, the wildcards, an escape -- as
    /// the definition does, and refuses the same patterns.
    #[test]
    fn like_pattern_matches_by_its_definition() {
        let alphabet = ['a', 'b', 'ç', '𝔸', '%', '_', '!'];
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut pick = |n: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n as u64) as usize
        };
        for _ in 0..60_000 {
            let h: Vec<char> = (0..pick(7)).map(|_| alphabet[pick(4)]).collect();
            let p: Vec<char> = (0..pick(7)).map(|_| alphabet[pick(7)]).collect();
            let e = [None, Some('!'), Some('%')][pick(3)];
            let (hs, ps): (String, String) = (h.iter().collect(), p.iter().collect());
            let mut eb = [0u8; 4];
            let es = e.map(|c| &*c.encode_utf8(&mut eb)).unwrap_or("");
            let got = like_pattern(hs.as_bytes(), ps.as_bytes(), es.as_bytes());
            assert_eq!(
                got,
                like_by_definition(&h, &p, e),
                "{hs:?} like {ps:?} escape {e:?}"
            );
        }
    }

    fn lookup_sel() -> Select {
        Select {
            collection: "products".into(),
            lookup: Some(Lookup {
                collection: "reviews".into(),
                on: vec!["product_id".into()],
                parent: vec!["id".into()],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// `lookup` hangs children off parent rows, so anything that reorders
    /// those rows by a score or replaces them with a number has to be
    /// refused rather than quietly applied to the parent alone. `check` runs
    /// in the engine as well as the parser, because a plan can be built in
    /// Rust and never see FenecQL.
    #[test]
    fn lookup_refuses_clauses_that_rank_or_collapse_the_parent() {
        for (name, mutate) in [
            (
                "near",
                (|s: &mut Select| {
                    s.near = Some(Near {
                        field: "embed".into(),
                        vector: Expr::Param(0),
                        ef: None,
                        exact: false,
                        metric: None,
                    })
                }) as fn(&mut Select),
            ),
            ("match", |s: &mut Select| {
                s.matcher = Some(Match {
                    field: "body".into(),
                    query: Expr::Param(0),
                    within: false,
                    fuzzy: 0,
                    prefix: false,
                })
            }),
            ("count", |s: &mut Select| s.count = true),
        ] {
            let mut sel = lookup_sel();
            mutate(&mut sel);
            let e = sel.check().unwrap_err().to_string();
            assert!(e.contains("lookup") && e.contains(name), "{name} -> {e}");
        }

        // `rerank` needs `match`, so it can only be reached with both set --
        // and then `match` is the clash that is reported first.
        let mut sel = lookup_sel();
        sel.rerank = Some(Rerank {
            field: "embed".into(),
            vector: Expr::Param(0),
            candidates: None,
        });
        assert!(sel.check().is_err());
    }

    /// The qualifier for a child field is the collection name, so a
    /// self-lookup would leave `on` and the child `where` with no way to say
    /// which side they meant.
    #[test]
    fn a_collection_cannot_look_itself_up() {
        let mut sel = lookup_sel();
        sel.lookup.as_mut().unwrap().collection = "products".into();
        let e = sel.check().unwrap_err().to_string();
        assert!(e.contains("itself"), "{e}");
    }

    /// The clauses `lookup` does combine with have to keep working, or the
    /// refusals above are just a way of banning the feature.
    #[test]
    fn lookup_combines_with_filter_order_and_pagination() {
        let mut sel = lookup_sel();
        sel.filter = Some(Expr::Cmp(
            CmpOp::Ge,
            Box::new(Expr::Field("price".into())),
            Box::new(Expr::Lit(Value::Int(10))),
        ));
        sel.order = vec![Sort::new("price", false)];
        sel.limit = Some(Count::N(20));
        sel.offset = Count::N(40);
        sel.project = Some(vec!["name".into()]);
        assert!(sel.check().is_ok());
    }

    /// What `~` folded to before the ASCII fast path existed. The fast path is
    /// only allowed to be cheaper, never to answer differently.
    fn folding(hay: &str, needle: &str) -> bool {
        hay.to_lowercase().contains(&needle.to_lowercase())
    }

    #[test]
    fn like_matches_regardless_of_case() {
        for (hay, needle, want) in [
            ("Rust and WASM", "rust", true),
            ("Rust and WASM", "WASM", true),
            ("Rust and WASM", "and w", true),
            ("Rust and WASM", "python", false),
            ("Rust", "", true),
            ("", "rust", false),
            ("ab", "abc", false),
            ("aaab", "aab", true),
        ] {
            assert_eq!(like_match(hay, needle), want, "{hay:?} ~ {needle:?}");
        }
    }

    #[test]
    fn the_ascii_path_agrees_with_folding() {
        // Unicode default case mapping and ASCII case mapping are the same
        // over U+0000..U+007F, so every ASCII pair must give the same answer
        // on both paths -- that equality is what lets the fast path exist.
        let words = ["Vector", "vector", "VECTOR", "tor", "ToR", "x", "", "or v"];
        for hay in words {
            for needle in words {
                assert_eq!(
                    like_match(hay, needle),
                    folding(hay, needle),
                    "{hay:?} ~ {needle:?}"
                );
            }
        }
    }

    #[test]
    fn non_ascii_still_folds() {
        // Anything outside ASCII takes the folding path, so case-insensitive
        // matching keeps working for text the fast path cannot handle.
        assert!(like_match("ÉCOLE", "école"));
        assert!(like_match("Straße", "STRASSE") == folding("Straße", "STRASSE"));
        assert!(like_match("ÇALIŞMA raporu", "çalişma"));
        assert!(!like_match("ÉCOLE", "schule"));
    }
}
