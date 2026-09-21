//! Query plan types and expression evaluation.
//!
//! This module is independent of FenecQL: plan structures can also be built
//! straight from Rust (embedded use); FenecQL is just a front end producing them.

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
    IsNull(Box<Expr>),
    /// Plugin or builtin function call: `cosine(embed, $1)`
    Call(String, Vec<Expr>),
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
            Expr::Cmp(_, a, b) | Expr::Like(a, b) | Expr::Has(a, b) => {
                a.referenced_fields(out);
                b.referenced_fields(out);
            }
            Expr::In(a, items) => {
                a.referenced_fields(out);
                for i in items {
                    i.referenced_fields(out);
                }
            }
            Expr::Call(_, args) => {
                for a in args {
                    a.referenced_fields(out);
                }
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
            Expr::Cmp(_, a, b) | Expr::Like(a, b) | Expr::Has(a, b) => {
                a.max_param().max(b.max_param())
            }
            Expr::In(a, items) => items
                .iter()
                .fold(a.max_param(), |m, i| m.max(i.max_param())),
            Expr::Call(_, args) => args.iter().map(|a| a.max_param()).max().unwrap_or(0),
        }
    }

    /// Extracts the `field = literal` pattern -- for hash index pushdown.
    ///
    /// A bound parameter is resolved as well: `where year = $1` is the usual
    /// shape coming from the browser and from fenec-pg, and looking only at
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
}

pub struct EvalCtx<'a> {
    pub params: &'a [Value],
    pub registry: &'a crate::plugin::Registry,
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
/// Unicode tables: ASCII here alone changes the wasm by nothing at all, and
/// both together by 5 200 bytes of brotli. That is not worth losing `ÇALIŞMA
/// ~ çalişma`, which every non-English corpus depends on.
fn like_match(hay: &str, needle: &str) -> bool {
    if !(hay.is_ascii() && needle.is_ascii()) {
        return hay.to_lowercase().contains(&needle.to_lowercase());
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
            if l.is_null() || r.is_null() {
                // NULL comparisons return false (except Eq/Ne)
                return Ok(match op {
                    CmpOp::Eq => Value::Bool(l.is_null() && r.is_null()),
                    CmpOp::Ne => Value::Bool(l.is_null() != r.is_null()),
                    _ => Value::Bool(false),
                });
            }
            let ord = l.cmp_value(&r);
            Value::Bool(match op {
                CmpOp::Eq => ord == Ordering::Equal,
                CmpOp::Ne => ord != Ordering::Equal,
                CmpOp::Lt => ord == Ordering::Less,
                CmpOp::Le => ord != Ordering::Greater,
                CmpOp::Gt => ord == Ordering::Greater,
                CmpOp::Ge => ord != Ordering::Less,
            })
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
        Expr::Call(name, args) => {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval(a, row, ctx)?);
            }
            ctx.registry.call(name, &vals)?
        }
    })
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
}

/// The `rerank` clause: reorder what `match` found by exact vector distance.
///
/// This is the no-graph retrieval path. `match` is cheap and recall-oriented,
/// the vectors are read straight out of the store, and the reordering is
/// exact over the candidate set -- so no HNSW graph has to be built, held,
/// validated on open or rebuilt when it fails to validate. Measured on BEIR:
/// on SciFact taking 50 candidates scores nDCG@10 0.676 against 0.645 for a
/// full dense scan of the same vectors, and on FiQA 1 000 candidates match
/// the full scan exactly (0.3687), each while scoring under 2% of the corpus.
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
    pub order: Vec<(String, bool)>,
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
    /// Sort keys in priority order: (field, ascending?).
    /// Empty = no ordering. Additional keys break ties:
    /// `order year desc, title asc`.
    pub order: Vec<(String, bool)>,
    pub limit: Option<usize>,
    pub offset: usize,
    /// `count`: returns the number of matching rows instead of the rows.
    pub count: bool,
    /// `lookup`: children of another collection, attached per row.
    pub lookup: Option<Lookup>,
}

impl Select {
    /// `count` does not combine with the other clauses: projection, ordering
    /// and pagination are meaningless over a count, and `near` already cuts
    /// at its own ceiling -- it would answer "how many are there" wrongly.
    /// We raise an error instead of ignoring them silently.
    pub fn check(&self) -> Result<()> {
        // `match` and `near` both decide the ordering. A query asking for
        // both is asking two questions, and silently picking one of them
        // would answer the other one wrongly.
        if self.matcher.is_some() && self.near.is_some() {
            return Err(Error::Query(
                "`match` and `near` cannot be combined: both order the result".into(),
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
    Put {
        collection: String,
        /// Field-expression pairs per document. Supplying `id` makes it an upsert.
        docs: Vec<Vec<(String, Expr)>>,
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
    },
    Delete {
        collection: String,
        filter: Option<Expr>,
    },
    ListCollections,
    Describe(String),
    Compact(Option<String>),
}

impl Statement {
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

    /// Number of parameters the statement expects: the highest `$n` used.
    pub fn max_param(&self) -> usize {
        let opt = |e: &Option<Expr>| e.as_ref().map(|e| e.max_param()).unwrap_or(0);
        let pairs =
            |v: &Vec<(String, Expr)>| v.iter().map(|(_, e)| e.max_param()).max().unwrap_or(0);
        match self {
            Statement::Put { docs, .. } => docs.iter().map(pairs).max().unwrap_or(0),
            Statement::Select(sel) | Statement::Explain(sel) => {
                let near = sel.near.as_ref().map(|n| n.vector.max_param()).unwrap_or(0);
                let m = sel
                    .matcher
                    .as_ref()
                    .map(|m| m.query.max_param())
                    .unwrap_or(0);
                let rr = sel
                    .rerank
                    .as_ref()
                    .map(|r| r.vector.max_param())
                    .unwrap_or(0);
                // A `lookup`'s `where` belongs to the same statement, so its
                // parameters count here too. `Describe` answers with this
                // number before the query runs; a client told there are none
                // sends none, and the query then fails on an unbound `$1`.
                let lk = sel
                    .lookup
                    .as_ref()
                    .map(|l| l.chain().map(|s| opt(&s.filter)).max().unwrap_or(0))
                    .unwrap_or(0);
                opt(&sel.filter).max(near).max(m).max(rr).max(lk)
            }
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
/// The grouping sits beside the rows instead of inside `Value` because no
/// value in this database is an object and none is going to become one --
/// a field you want to filter on should be a field. Keeping the nesting in
/// the envelope leaves the value model, the codec and the JSON *parser*
/// untouched; only serialisation learns a second shape, which is the easy
/// direction. It is also the shape the codebase already uses to answer for
/// more than one collection: `Response::Schemas` goes long rather than wide.
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
        sel.order = vec![("price".into(), false)];
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
