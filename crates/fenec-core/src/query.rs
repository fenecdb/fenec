//! Query plan types and expression evaluation.
//!
//! This module is independent of FenecQL: plan structures can also be built
//! straight from Rust (embedded use); FenecQL is just a front end producing them.

use crate::collate::Collation;
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
    /// The collation a field's text compares in, when its schema names one
    /// (`name text collate tr`); `<` and `>` against it then compare in
    /// that order rather than the bytes'.
    fn collation(&self, name: &str) -> Option<Collation> {
        let _ = name;
        None
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
/// two timestamps apart are the milliseconds between them. Anything else
/// is a type error: the field's type check is what a result meets next.
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
                "`{}` takes numbers, or a timestamp and milliseconds; found {} and {}",
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

pub fn eval(expr: &Expr, row: &mut dyn RowAccess, ctx: &EvalCtx) -> Result<Value> {
    Ok(match expr {
        Expr::Lit(v) => v.clone(),
        Expr::Param(i) => ctx
            .params
            .get(*i)
            .cloned()
            .ok_or_else(|| Error::Query(format!("parameter ${} is not bound", i + 1)))?,
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
                Expr::Field(name) => row.collation(name),
                _ => None,
            };
            Value::Bool(compare(*op, &l, &r, &|| field(a).or_else(|| field(b))))
        }
        Expr::Like(a, b) => {
            let l = eval(a, row, ctx)?;
            let r = eval(b, row, ctx)?;
            match (l.as_text(), r.as_text()) {
                (Some(hay), Some(needle)) => Value::Bool(like_match(hay, needle)),
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
            let mut found = false;
            for it in items {
                if eval(it, row, ctx)?.cmp_value(&l) == Ordering::Equal {
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
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval(a, row, ctx)?);
            }
            ctx.registry.call(name, &vals)?
        }
        Expr::Arith(op, a, b) => arith(*op, &eval(a, row, ctx)?, &eval(b, row, ctx)?)?,
    })
}

/// `l op r` as a filter takes it: a null equal only to a null and ordered
/// against nothing. Text against a field in a collation orders as the field
/// does (`coll`, asked only then), so the scan and the field's `@sorted`
/// index agree; equality is the bytes' either way, as the collation ties
/// no two strings.
#[inline]
pub(crate) fn compare(
    op: CmpOp,
    l: &Value,
    r: &Value,
    coll: &dyn Fn() -> Option<Collation>,
) -> bool {
    if l.is_null() || r.is_null() {
        return match op {
            CmpOp::Eq => l.is_null() && r.is_null(),
            CmpOp::Ne => l.is_null() != r.is_null(),
            _ => false,
        };
    }
    let ord = match (op, l, r) {
        (CmpOp::Eq | CmpOp::Ne, ..) => l.cmp_value(r),
        (_, Value::Text(x), Value::Text(y)) => match coll() {
            Some(c) => c.compare(x, y),
            None => l.cmp_value(r),
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
}

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
    /// The child field matched against the parent. `id` is allowed; anything
    /// else must carry `@hash`, because the probe is a bucket lookup and a
    /// scan per parent would be a different feature wearing the same name.
    pub child_field: String,
    /// The parent field holding the key. `id` unless `on a = b` says so,
    /// which is the foreign-key-to-primary-key case spelled out.
    pub parent_field: String,
    /// None = all fields of the child.
    pub project: Option<Vec<String>>,
    pub filter: Option<Expr>,
    pub order: Vec<Sort>,
    pub limit: Option<usize>,
    pub offset: usize,
    /// `required`: drop a parent that no child matches.
    ///
    /// Without it the page is the parents and a childless one keeps its row
    /// with an empty group, which is what a listing wants. With it the
    /// children decide who appears -- "products that have a five-star
    /// review" -- and that is a different question, so it is asked for
    /// rather than inferred from the presence of a child `where`.
    ///
    /// It is tested before `offset` and `limit`: a parent whose matches the
    /// page happened to skip has matches all the same.
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

/// An item of an aggregating select list.
#[derive(Debug, Clone, PartialEq)]
pub enum Agg {
    /// The group's own value: the `group` field, listed.
    Key(String),
    /// `count(*)`: the rows.
    Count,
    Sum(String),
    Avg(String),
    Min(String),
    Max(String),
}

impl Agg {
    /// The column it answers under: the field, `count`, `sum(total)`.
    pub fn label(&self) -> String {
        match self {
            Agg::Key(f) => f.clone(),
            Agg::Count => COUNT_COLUMN.to_string(),
            Agg::Sum(f) => format!("sum({f})"),
            Agg::Avg(f) => format!("avg({f})"),
            Agg::Min(f) => format!("min({f})"),
            Agg::Max(f) => format!("max({f})"),
        }
    }

    /// The field it reads, if any.
    pub fn field(&self) -> Option<&str> {
        match self {
            Agg::Count => None,
            Agg::Key(f) | Agg::Sum(f) | Agg::Avg(f) | Agg::Min(f) | Agg::Max(f) => Some(f),
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
    pub top: Option<usize>,
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

/// One facet's answer: each value and how many matched rows hold it, by
/// count, most first, then by value.
#[derive(Debug, Clone, PartialEq)]
pub struct FacetValues {
    pub field: String,
    pub values: Vec<(Value, u64)>,
}

/// One key of `order`: `order year desc`, `order name collate tr`.
#[derive(Debug, Clone, PartialEq)]
pub struct Sort {
    pub field: String,
    pub asc: bool,
    /// `collate tr`: the field's text in a language's order rather than
    /// its bytes'. None: byte order, which is what `@sorted` holds.
    pub collate: Option<Collation>,
}

impl Sort {
    /// `field` ascending, or descending, in byte order.
    pub fn new(field: impl Into<String>, asc: bool) -> Sort {
        Sort {
            field: field.into(),
            asc,
            collate: None,
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
    pub limit: Option<usize>,
    pub offset: usize,
    /// `count`: returns the number of matching rows instead of the rows.
    pub count: bool,
    /// `lookup`: children of another collection, attached per row.
    pub lookup: Option<Lookup>,
    /// A select list that aggregates -- `select status, sum(total), count(*)`
    /// -- in the order written. Empty: no aggregation.
    pub aggregate: Vec<Agg>,
    /// `group <field>`: one row per value of the field rather than one in all.
    pub group: Option<String>,
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
    /// coupon is still good", read under the block's lock.
    pub require: Option<u64>,
}

impl Select {
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
            }
        }
        self.check_marks_and_facets()?;
        // `count` and an aggregate answer one row whatever matched: a
        // `require 1` over them would always hold, and say nothing.
        if self.require.is_some()
            && (self.count || (!self.aggregate.is_empty() && self.group.is_none()))
        {
            return Err(Error::Query(
                "`require` counts the rows a `get` answers, and `count` or an aggregate answers \
                 one: require the rows themselves, `limit 1 require 1` for one to exist"
                    .into(),
            ));
        }
        if !self.aggregate.is_empty() || self.group.is_some() {
            self.check_aggregate()?;
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
        } else if self.offset != 0 {
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
            match f.top {
                Some(0) => {
                    return Err(Error::Query(format!(
                        "`facet {} top 0` answers nothing: `top` takes at least 1",
                        f.field
                    )))
                }
                Some(n) if n > MAX_FACET_VALUES => {
                    return Err(Error::Query(format!(
                        "`facet {} top {n}`: a facet answers at most {MAX_FACET_VALUES} values",
                        f.field
                    )))
                }
                _ => {}
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
        let lk = self
            .lookup
            .as_ref()
            .map(|l| l.chain().map(|s| opt(&s.filter)).max().unwrap_or(0))
            .unwrap_or(0);
        let mut marks = 0;
        for m in &self.marks {
            for a in &m.args {
                marks = marks.max(a.max_param());
            }
        }
        opt(&self.filter)
            .max(near)
            .max(m)
            .max(rr)
            .max(lk)
            .max(marks)
    }

    /// Whether a literal in its filters holds a vector
    /// ([`Expr::reads_vectors`]).
    pub fn reads_vectors(&self) -> bool {
        let opt = |e: &Option<Expr>| e.as_ref().is_some_and(Expr::reads_vectors);
        let mut level = self.lookup.as_ref();
        let mut any = opt(&self.filter);
        while let (false, Some(l)) = (any, level) {
            any = opt(&l.filter);
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
    /// ranks the rows or hangs children from them combines with them, and a
    /// single row has nothing to order or page. Grouped, the rows are the
    /// groups, and those do.
    fn check_aggregate(&self) -> Result<()> {
        let Some(group) = &self.group else {
            let clash = if !self.order.is_empty() {
                "order"
            } else if self.limit.is_some() {
                "limit"
            } else if self.offset != 0 {
                "offset"
            } else {
                ""
            };
            if !clash.is_empty() {
                return Err(Error::Query(format!(
                    "aggregates answer one row, which `{clash}` has nothing to do with; \
                     `group` makes a row per value"
                )));
            }
            if let Some(Agg::Key(f)) = self.aggregate.iter().find(|a| matches!(a, Agg::Key(_))) {
                return Err(Error::Query(format!(
                    "`{f}` is neither aggregated nor grouped by"
                )));
            }
            return self.check_aggregate_company();
        };
        if self.aggregate.is_empty() {
            return Err(Error::Query(format!(
                "`group {group}` needs an aggregate to answer with: `select {group}, count(*)`"
            )));
        }
        for a in &self.aggregate {
            if let Agg::Key(f) = a {
                if f != group {
                    return Err(Error::Query(format!(
                        "`{f}` is neither aggregated nor grouped by"
                    )));
                }
            }
        }
        self.check_aggregate_company()
    }

    fn check_aggregate_company(&self) -> Result<()> {
        let clash = if self.near.is_some() {
            "near"
        } else if self.matcher.is_some() {
            "match"
        } else if self.lookup.is_some() {
            "lookup"
        } else if self.count {
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
        /// `insert`: a document naming an id that is taken is refused
        /// (`Error::Duplicate`), where `put` writes over it.
        insert: bool,
        /// `put ... if absent`: a document whose id, or a `@unique` value
        /// of, a live row holds already is passed over rather than
        /// refused, and not counted -- `SET NX`, whose answer (0 or 1) says
        /// whether the write was made. The parser sets `insert` with it.
        if_absent: bool,
        /// `... require <n>`: the statement is refused (`Error::Unmet`), and
        /// so its block put back whole, unless it wrote exactly `n` rows.
        require: Option<u64>,
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
        require: Option<u64>,
    },
    Delete {
        collection: String,
        filter: Option<Expr>,
        /// As `Put`'s: the rows it deleted must be exactly this many.
        require: Option<u64>,
    },
    ListCollections,
    Describe(String),
    Compact(Option<String>),
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

impl Statement {
    /// Whether a literal in it holds a vector: when none does, no json
    /// field can be handed one, and nothing has to be asked of the schema.
    pub fn reads_vectors(&self) -> bool {
        let opt = |e: &Option<Expr>| e.as_ref().is_some_and(Expr::reads_vectors);
        match self {
            Statement::Put { docs, .. } => docs.iter().flatten().any(|(_, e)| e.reads_vectors()),
            Statement::Update { set, filter, .. } => {
                set.iter().any(|(_, e)| e.reads_vectors()) || opt(filter)
            }
            Statement::Delete { filter, .. } => opt(filter),
            Statement::Select(s) | Statement::Explain(s) => s.reads_vectors(),
            _ => false,
        }
    }

    /// Statements that need no write access. The server runs these under a
    /// shared (read) lock; the others take the exclusive write lock.
    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Statement::Select(_)
                | Statement::Explain(_)
                | Statement::ListCollections
                | Statement::Describe(_)
        )
    }

    /// Statements a block of writes may hold: every one but a compact,
    /// which rewrites the file -- nothing a block's undo could put back. A
    /// schema change is put back as a write is: a collection made goes, one
    /// dropped comes back, an index built goes.
    pub fn fits_block(&self) -> bool {
        !matches!(self, Statement::Compact(_))
    }

    /// Number of parameters the statement expects: the highest `$n` used.
    pub fn max_param(&self) -> usize {
        let opt = |e: &Option<Expr>| e.as_ref().map(|e| e.max_param()).unwrap_or(0);
        let pairs =
            |v: &Vec<(String, Expr)>| v.iter().map(|(_, e)| e.max_param()).max().unwrap_or(0);
        match self {
            Statement::Put { docs, .. } => docs.iter().map(pairs).max().unwrap_or(0),
            Statement::Select(sel) | Statement::Explain(sel) => sel.max_param(),
            Statement::Update { set, filter, .. } => pairs(set).max(opt(filter)),
            Statement::Delete { filter, .. } => opt(filter),
            _ => 0,
        }
    }

    /// Whether it will return rows. Needed to know that `NoData` is sent
    /// instead of `RowDescription` in the `Describe` response.
    pub fn returns_rows(&self) -> bool {
        self.is_read_only()
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
    pub fn flatten(&self) -> std::borrow::Cow<'_, ResultSet> {
        let Some(n) = &self.nested else {
            return std::borrow::Cow::Borrowed(self);
        };
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
        std::borrow::Cow::Owned(ResultSet {
            columns,
            rows,
            nested: None,
            facets: self.facets.clone(),
        })
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

    fn lookup_sel() -> Select {
        Select {
            collection: "products".into(),
            lookup: Some(Lookup {
                collection: "reviews".into(),
                child_field: "product_id".into(),
                parent_field: "id".into(),
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
                    })
                }) as fn(&mut Select),
            ),
            ("match", |s: &mut Select| {
                s.matcher = Some(Match {
                    field: "body".into(),
                    query: Expr::Param(0),
                    within: false,
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
        sel.limit = Some(20);
        sel.offset = 40;
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
