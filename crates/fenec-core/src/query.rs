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
    pub fn conjunct_equalities<'a>(&'a self, params: &'a [Value], out: &mut Vec<(&'a str, &'a Value)>) {
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

    /// Does the filter consist of exactly one equality?
    pub fn is_bare_equality(&self, params: &[Value]) -> bool {
        self.equality_key(params).is_some()
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
                (Some(hay), Some(needle)) => Value::Bool(
                    hay.to_lowercase().contains(&needle.to_lowercase()),
                ),
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

/// Column name of the `count` result. No such field can exist in a schema --
/// field names are identifiers and `count` may be one too; on a clash the
/// column name matches but the value is still the count, because `count`
/// replaces the projection entirely.
pub const COUNT_COLUMN: &str = "count";

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Select {
    pub collection: String,
    /// None = all fields.
    pub project: Option<Vec<String>>,
    pub filter: Option<Expr>,
    pub near: Option<Near>,
    /// Sort keys in priority order: (field, ascending?).
    /// Empty = no ordering. Additional keys break ties:
    /// `order year desc, title asc`.
    pub order: Vec<(String, bool)>,
    pub limit: Option<usize>,
    pub offset: usize,
    /// `count`: returns the number of matching rows instead of the rows.
    pub count: bool,
}

impl Select {
    /// `count` does not combine with the other clauses: projection, ordering
    /// and pagination are meaningless over a count, and `near` already cuts
    /// at its own ceiling -- it would answer "how many are there" wrongly.
    /// We raise an error instead of ignoring them silently.
    pub fn check(&self) -> Result<()> {
        if !self.count {
            return Ok(());
        }
        let clash = if self.project.is_some() {
            "select"
        } else if self.near.is_some() {
            "near"
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
            Statement::Select(_) | Statement::ListCollections | Statement::Describe(_)
        )
    }

    /// Number of parameters the statement expects: the highest `$n` used.
    pub fn max_param(&self) -> usize {
        let opt = |e: &Option<Expr>| e.as_ref().map(|e| e.max_param()).unwrap_or(0);
        let pairs = |v: &Vec<(String, Expr)>| {
            v.iter().map(|(_, e)| e.max_param()).max().unwrap_or(0)
        };
        match self {
            Statement::Put { docs, .. } => docs.iter().map(pairs).max().unwrap_or(0),
            Statement::Select(sel) => {
                let near = sel
                    .near
                    .as_ref()
                    .map(|n| n.vector.max_param())
                    .unwrap_or(0);
                opt(&sel.filter).max(near)
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

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
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
