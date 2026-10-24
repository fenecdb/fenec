//! Aggregating select lists, and a plain list's computed columns.
//!
//! An item is an expression: a field, `px * qty`, `bucket(at, 1m)`, an
//! aggregate of any of those -- `sum(px * qty)`, `count(distinct user)`,
//! `first(px by at)` -- or an expression over aggregates and group keys,
//! `sum(px * qty) / sum(qty)`. Each is bound to the collection once a query
//! ([`Reader`]): every field and path the list reads is decoded in one pass
//! a row into the slots of `env`, a field read as it is answers from its
//! slot with nothing copied, `bucket` over a constant interval works the
//! interval out once, and anything else is the expression with its fields
//! made parameters past the query's own, which `eval` reads by place rather
//! than look up by name. So a row costs what the old fixed aggregates cost
//! when the list is fields alone, and the rest one `eval` an item.

use super::*;
#[cfg(feature = "analytics")]
use crate::hll::Sketch;

/// The fields and paths a list reads, bound to its collection: what a row
/// is decoded into, and what is worked out of it.
///
/// Every value an item reads is a slot of `env`: a field's or a path's,
/// decoded in one pass a row, or one worked out of them right after it --
/// `bucket(at, 1m)`, `px * qty`, the id -- once a row whatever reads it.
/// So the fold reads a `&Value` by its place: handed back as a `Result`
/// of a `Cow` a call, the fixed aggregates of before took 67 -> 90 ms over
/// a million rows in 100 groups.
pub(super) struct Reader<'q> {
    c: &'q Collection,
    /// The fields' places in a payload, ascending, and their positions in
    /// the schema in the same order.
    places: Vec<usize>,
    positions: Vec<usize>,
    /// The paths into json fields, each its field's position and the keys.
    paths: Vec<(usize, &'q str)>,
    names: Vec<&'q str>,
    /// How many parameters the query was given.
    given: usize,
    /// What is worked out of a row, in turn, and the slot each goes in.
    calc: Vec<(Calc, usize)>,
    /// A row's values: its fields', its paths', the query's parameters --
    /// which an expression's own `$n` is moved past the paths to -- and
    /// what is worked out.
    pub(super) env: Vec<Value>,
}

/// A value worked out of a row's slots.
enum Calc {
    Id,
    /// `bucket(x, 15m)` over a constant interval, `x`'s slot -- and a
    /// constant zone's clock, `bucket(x, 1d, 'Europe/Berlin')`, resolved
    /// once a query; boxed, so a `Calc` stays the size it was.
    #[cfg(feature = "analytics")]
    Bucket(usize, Interval, Option<Box<crate::tz::Zone>>),
    /// Anything else: the expression, its fields made parameters.
    Expr(Expr),
}

/// A row's id for what `eval` reads of it; its fields are parameters.
struct IdRow(DocId);
impl RowAccess for IdRow {
    fn id(&self) -> DocId {
        self.0
    }
    fn field(&mut self, name: &str) -> Result<Value> {
        Err(Error::Query(format!("`{name}` is not read here")))
    }
}

impl<'q> Reader<'q> {
    /// Bound to every field and path `exprs` read, an unknown one refused.
    // Out of line: the plain select's computed columns and the
    // aggregate share one copy.
    #[inline(never)]
    pub(super) fn new(c: &'q Collection, exprs: &[&'q Expr], params: &[Value]) -> Result<Self> {
        let mut used = Vec::new();
        for e in exprs {
            e.referenced_fields(&mut used);
        }
        let mut rd = Reader {
            c,
            places: Vec::new(),
            positions: Vec::new(),
            paths: Vec::new(),
            names: Vec::new(),
            given: params.len(),
            calc: Vec::new(),
            env: Vec::new(),
        };
        for name in &used {
            if name == "id" {
                continue;
            }
            if let (p, None) = source_or_err(&c.schema, name, "")? {
                if let Err(i) = rd.positions.binary_search(&p) {
                    rd.positions.insert(i, p);
                }
            }
        }
        // The paths by the names the expressions borrow them under.
        for e in exprs {
            rd.find_paths(e)?;
        }
        rd.places = rd.positions.iter().map(|&p| c.schema.place(p)).collect();
        rd.env.resize(rd.base(), Value::Null);
        rd.env.extend_from_slice(params);
        Ok(rd)
    }

    fn find_paths(&mut self, e: &'q Expr) -> Result<()> {
        if let Expr::Field(name) = e {
            if let Some(p) = self.c.schema.path_of(name)? {
                if !self.names.contains(&name.as_str()) {
                    self.paths.push(p);
                    self.names.push(name);
                }
            }
            return Ok(());
        }
        let mut r = Ok(());
        e.each_child(&mut |c| {
            if r.is_ok() {
                r = self.find_paths(c);
            }
        });
        r
    }

    /// The slot `name` is read into.
    pub(super) fn slot(&self, name: &str) -> usize {
        match self.names.iter().position(|n| *n == name) {
            Some(i) => self.positions.len() + i,
            None => {
                let p = self.c.schema.field_pos(name).unwrap_or(usize::MAX);
                self.positions.iter().position(|q| *q == p).unwrap_or(0)
            }
        }
    }

    /// Where the query's own parameters start in `env`.
    pub(super) fn base(&self) -> usize {
        self.positions.len() + self.paths.len()
    }

    /// The field a slot holds as it is, typed: a path's values have none.
    pub(super) fn typed(&self, slot: usize) -> Option<&'q crate::schema::Field> {
        self.positions.get(slot).map(|&p| &self.c.schema.fields[p])
    }

    /// The slot `e`'s value is in once a row is read: a field's or a
    /// path's own, or one it is worked out into.
    // Out of line: the plain select's computed columns and the
    // aggregate share one copy.
    #[inline(never)]
    pub(super) fn bind(&mut self, e: &Expr) -> Result<usize> {
        let calc = match e {
            Expr::Field(n) if n == "id" => Calc::Id,
            Expr::Field(n) => return Ok(self.slot(n)),
            #[cfg(feature = "analytics")]
            Expr::Call(name, args) if name == "bucket" && matches!(args.len(), 2 | 3) => {
                let constant = |a: &Expr| match a {
                    Expr::Lit(v) => Some(v.clone()),
                    Expr::Param(i) => self.env.get(self.base() + i).cloned(),
                    _ => None,
                };
                let (iv, zone) = (constant(&args[1]), args.get(2).map(constant));
                match (iv, zone) {
                    (Some(v), None) => Calc::Bucket(self.bind(&args[0])?, Interval::of(&v)?, None),
                    (Some(v), Some(Some(z))) => {
                        let zone = Box::new(crate::tz::zone_of(&z)?);
                        Calc::Bucket(self.bind(&args[0])?, Interval::of(&v)?, Some(zone))
                    }
                    _ => Calc::Expr(self.lift(e)?),
                }
            }
            _ => Calc::Expr(self.lift(e)?),
        };
        self.env.push(Value::Null);
        self.calc.push((calc, self.env.len() - 1));
        Ok(self.env.len() - 1)
    }

    /// `e` with each field it reads made the parameter its slot is, and
    /// each of the query's parameters moved past the slots.
    fn lift(&self, e: &Expr) -> Result<Expr> {
        let (base, given) = (self.base(), self.given);
        let mut e = e.clone();
        e.rewrite(&mut |x| {
            *x = match x {
                Expr::Field(n) if n != "id" => Expr::Param(self.slot(n)),
                Expr::Param(i) if *i >= given => return Err(unbound(*i)),
                Expr::Param(i) => Expr::Param(base + *i),
                _ => return Ok(false),
            };
            Ok(true)
        })?;
        Ok(e)
    }

    /// Reads row `id` into the slots, and works out what is worked out of
    /// it; false when there is no such row.
    // Out of line: the plain select's computed columns and the
    // aggregate share one copy.
    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(super) fn read(
        &mut self,
        id: DocId,
        registry: &Registry,
        clock: Option<i64>,
    ) -> Result<bool> {
        if !self.c.store.read_fields(id, &self.places, &mut self.env)? {
            return Ok(false);
        }
        // The paths and what is worked out, out of line: a list of fields
        // alone -- every aggregate of before -- asks one length.
        if self.paths.len() + self.calc.len() != 0 {
            self.read_more(id, registry, clock)?;
        }
        Ok(true)
    }

    #[inline(never)]
    fn read_more(&mut self, id: DocId, registry: &Registry, clock: Option<i64>) -> Result<()> {
        if !self.paths.is_empty() {
            let (n, m) = (self.positions.len(), self.paths.len());
            if !self
                .c
                .store
                .read_paths(id, &self.paths, &mut self.env[n..n + m])?
            {
                self.env[n..n + m].fill(Value::Null);
            }
        }
        self.work_out(id, registry, clock)
    }

    /// Works out what is worked out of the slots as they stand.
    pub(super) fn work_out(
        &mut self,
        id: DocId,
        registry: &Registry,
        clock: Option<i64>,
    ) -> Result<()> {
        for (calc, at) in &self.calc {
            let v = match calc {
                Calc::Id => Value::Int(id as i64),
                #[cfg(feature = "analytics")]
                Calc::Bucket(x, iv, None) => iv.truncate(&self.env[*x])?,
                #[cfg(feature = "analytics")]
                Calc::Bucket(x, iv, Some(z)) => iv.truncate_in(&self.env[*x], Some(z))?,
                Calc::Expr(e) => {
                    let ctx = EvalCtx {
                        params: &self.env,
                        registry,
                        clock,
                    };
                    eval(e, &mut IdRow(id), &ctx)?
                }
            };
            self.env[*at] = v;
        }
        Ok(())
    }
}

/// One aggregate's running value over a group's rows. Nulls are skipped
/// by every one but the row count, as SQL's are.
#[derive(Clone)]
enum Fold {
    Count(i64),
    /// An int sum, until a float comes -- an expression's or a path's
    /// values have no type to say which they will be -- and how many
    /// values went in.
    SumInt(i64, u64),
    SumFloat(f64, u64),
    Avg(f64, u64),
    /// `true` for `max`; how the field's text compares -- its collation,
    /// its fold -- which its text orders by.
    Extreme(Option<Value>, bool, TextOrder),
    /// `count(distinct ...)`: the values are in the query's one set, a
    /// `nocase` field's by their fold.
    Distinct(i64, Option<CaseFold>),
    /// `first`, or `last` when `true`: the row's order and id, and its value.
    /// Boxed, so that every fold is the size it was: a fold the size of
    /// two values and an id took the fixed aggregates of before 11%
    /// longer. The order's text compares as its field's does.
    Pick(Option<Box<(Value, DocId, Value)>>, bool, TextOrder),
    /// `approx_count_distinct`, `hll_accumulate` and `hll_combine`: the
    /// group's sketch, made at its first value, which of the three, and the
    /// fold a `nocase` field's values are counted by. The `analytics`
    /// feature's: without it none is made, and the compiler drops its fold.
    #[cfg(feature = "analytics")]
    Hll(Option<Box<Sketch>>, u8, Option<CaseFold>),
}

/// What a sketch answers: a count, or itself as bytes; `MERGE` folds
/// sketches rather than values.
#[cfg(feature = "analytics")]
const HLL_COUNT: u8 = 0;
#[cfg(feature = "analytics")]
const HLL_STATE: u8 = 1;
#[cfg(feature = "analytics")]
const HLL_MERGE: u8 = 2;

impl Fold {
    /// The fold `name` starts at; `ty`, the type of the field it reads as
    /// it is, refuses what that type cannot be folded by before any row.
    fn new(name: &str, distinct: bool, ty: Option<&crate::schema::Field>) -> Result<Fold> {
        let t = ty.map(|f| &f.ty).filter(|t| **t != DataType::Json);
        let refuse = |what: &str| {
            let f = ty.map_or("", |f| f.name.as_str());
            Err(Error::Type(format!(
                "`{name}({f})` needs {what}; `{f}` is {}",
                t.map_or(String::new(), |t| t.name())
            )))
        };
        #[cfg(not(feature = "analytics"))]
        if SKETCHES.contains(&name) {
            return Err(not_built(&format!("`{name}`"), "analytics"));
        }
        let fold = ty.and_then(|f| f.case_fold());
        Ok(match (name, t) {
            ("count", _) if distinct => Fold::Distinct(0, fold),
            #[cfg(feature = "analytics")]
            ("approx_count_distinct", _) => Fold::Hll(None, HLL_COUNT, fold),
            #[cfg(feature = "analytics")]
            ("hll_accumulate", _) => Fold::Hll(None, HLL_STATE, fold),
            #[cfg(feature = "analytics")]
            ("hll_combine", _) => Fold::Hll(None, HLL_MERGE, None),
            ("count", _) => Fold::Count(0),
            ("sum", Some(DataType::Float)) => Fold::SumFloat(0.0, 0),
            ("sum" | "avg", Some(DataType::Int | DataType::Float) | None) => match name {
                "sum" => Fold::SumInt(0, 0),
                _ => Fold::Avg(0.0, 0),
            },
            ("sum" | "avg", _) => return refuse("an int or float field"),
            ("first" | "last", _) => Fold::Pick(None, name == "last", TextOrder::BYTES),
            (
                _,
                Some(
                    DataType::Int
                    | DataType::Float
                    | DataType::Timestamp
                    | DataType::Text
                    | DataType::Bool,
                )
                | None,
            ) => Fold::Extreme(
                None,
                name == "max",
                ty.map_or(TextOrder::BYTES, |f| f.text_order()),
            ),
            _ => return refuse("a field with an order"),
        })
    }

    /// Folds `v` in; false for `count(distinct ...)`, `first` and `last`,
    /// which need the row as well ([`Fold::row_held`]). One dispatch a
    /// value: asked which they are first, the fixed aggregates of before
    /// took 4% longer.
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    fn add(&mut self, v: &Value) -> Result<bool> {
        // `count(*)` is handed `true` a row, and `count(x)` counts the rows
        // where `x` is not null, as SQL's does.
        if matches!(v, Value::Null) {
            return Ok(true);
        }
        match self {
            Fold::Count(n) => *n += 1,
            Fold::SumInt(sum, n) => match v {
                Value::Int(i) => {
                    *sum = sum
                        .checked_add(*i)
                        .ok_or_else(|| Error::Query("the sum does not fit a 64-bit int".into()))?;
                    *n += 1;
                }
                Value::Float(f) => *self = Fold::SumFloat(*sum as f64 + f, *n + 1),
                _ => return Err(refused("sum", v)),
            },
            Fold::SumFloat(sum, n) | Fold::Avg(sum, n) => {
                *sum += match v {
                    Value::Int(i) => *i as f64,
                    Value::Float(f) => *f,
                    _ => {
                        let what = if matches!(self, Fold::Avg(..)) {
                            "avg"
                        } else {
                            "sum"
                        };
                        return Err(refused(what, v));
                    }
                };
                *n += 1;
            }
            Fold::Extreme(best, max, text) => {
                let better = match best {
                    None => true,
                    Some(b) => {
                        let o = match text.is_bytes() {
                            true => v.cmp_value(b),
                            false => text.compare_values(v, b),
                        };
                        o == if *max {
                            Ordering::Greater
                        } else {
                            Ordering::Less
                        }
                    }
                };
                if better {
                    *best = Some(v.clone());
                }
            }
            Fold::Distinct(..) | Fold::Pick(..) => return Ok(false),
            #[cfg(feature = "analytics")]
            Fold::Hll(..) => return Ok(false),
        }
        Ok(true)
    }

    /// What `count(distinct ...)`, `first` and `last` make of a row: the
    /// value put in the query's set of them under its item's and its
    /// group's numbers (`at`), or the row taken when it comes before, or
    /// after, the one held. Out of line, so that the loop every other
    /// aggregate folds in stays the size it was: with these in it, the
    /// fixed aggregates of before took 15% longer.
    #[inline(never)]
    fn row_held(
        &mut self,
        v: &Value,
        by: Option<&Value>,
        id: DocId,
        at: (usize, usize),
        seen: &mut crate::maps::Map<Vec<u8>, Bucket>,
        scratch: &mut (Vec<u8>, usize),
    ) -> Result<()> {
        if v.is_null() {
            return Ok(());
        }
        #[cfg_attr(not(feature = "analytics"), allow(unused_variables))]
        let (key, sketched) = (&mut scratch.0, &mut scratch.1);
        match self {
            #[cfg(feature = "analytics")]
            Fold::Hll(s, kind, fold) => {
                let s = s.get_or_insert_with(Default::default);
                let before = s.bytes();
                match (*kind, v) {
                    (HLL_MERGE, Value::Bytes(b)) => s.merge(&Sketch::from_bytes(b)?),
                    (HLL_MERGE, other) => {
                        return Err(Error::Type(format!(
                            "`hll_combine` takes sketches, the bytes `hll_accumulate` makes; \
                             found {}",
                            other.type_name()
                        )))
                    }
                    _ => {
                        key.clear();
                        encode_folded(key, v, *fold);
                        s.add(crate::hll::hash(key));
                    }
                }
                // A sketch is 16 KB once dense, a group's each: bounded
                // as `count(distinct ...)`'s values are, refused past it.
                *sketched += s.bytes().saturating_sub(before);
                if *sketched > crate::hll::MAX_BYTES {
                    return Err(Error::Query(format!(
                        "the sketches of `approx_count_distinct` and `hll_*` hold more than {} MB, \
                         16 KB a group: fewer groups, or a narrower filter",
                        crate::hll::MAX_BYTES >> 20
                    )));
                }
            }
            Fold::Distinct(n, fold) => {
                key.clear();
                key.extend_from_slice(&(at.0 as u32).to_le_bytes());
                key.extend_from_slice(&(at.1 as u32).to_le_bytes());
                encode_folded(key, v, *fold);
                if !seen.contains_key(key.as_slice()) {
                    if seen.len() >= MAX_DISTINCT_VALUES {
                        return Err(Error::Query(format!(
                            "`count(distinct ...)` holds more than {MAX_DISTINCT_VALUES} \
                             values: a count is not cut short, so narrow the filter"
                        )));
                    }
                    seen.insert(key.clone(), Bucket::default());
                    *n += 1;
                }
            }
            Fold::Pick(best, later, text) => {
                let written = Value::Int(id as i64);
                let by = by.unwrap_or(&written);
                if by.is_null() {
                    return Ok(());
                }
                let better = match best.as_deref() {
                    None => true,
                    Some((b, bid, _)) => {
                        let o = match text.is_bytes() {
                            true => by.cmp_value(b),
                            false => text.compare_values(by, b),
                        }
                        .then(id.cmp(bid));
                        o == if *later {
                            Ordering::Greater
                        } else {
                            Ordering::Less
                        }
                    }
                };
                // The box a group's first row made, written over after.
                match (better, best) {
                    (false, _) => {}
                    (true, Some(held)) => **held = (by.clone(), id, v.clone()),
                    (true, best) => *best = Some(Box::new((by.clone(), id, v.clone()))),
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn value(self) -> Value {
        match self {
            Fold::Count(n) | Fold::Distinct(n, _) => Value::Int(n),
            #[cfg(feature = "analytics")]
            Fold::Hll(s, HLL_COUNT, _) => Value::Int(s.map_or(0, |s| s.estimate() as i64)),
            #[cfg(feature = "analytics")]
            Fold::Hll(s, ..) => s.map_or(Value::Null, |s| Value::Bytes(s.to_bytes())),
            Fold::SumInt(_, 0) | Fold::SumFloat(_, 0) | Fold::Avg(_, 0) => Value::Null,
            Fold::SumInt(s, _) => Value::Int(s),
            Fold::SumFloat(s, _) => Value::Float(s),
            Fold::Avg(s, n) => Value::Float(s / n as f64),
            Fold::Extreme(v, ..) => v.unwrap_or(Value::Null),
            Fold::Pick(p, ..) => p.map_or(Value::Null, |p| p.2),
        }
    }
}

/// The sketches the `analytics` feature brings: refused by name without it.
#[cfg(not(feature = "analytics"))]
const SKETCHES: [&str; 3] = ["approx_count_distinct", "hll_accumulate", "hll_combine"];

/// What a fold says of a value it cannot fold: out of line, so that the
/// fold a value goes through stays small enough to be inlined into the loop.
#[cold]
#[inline(never)]
fn refused(what: &str, v: &Value) -> Error {
    let takes = match what {
        "min" | "max" => "values with an order",
        _ => "numbers",
    };
    Error::Type(format!("`{what}` takes {takes}; found {}", v.type_name()))
}

/// An aggregate of the list, as the rows are folded by it: what it reads,
/// and for `first` and `last` what orders the rows.
struct Item {
    start: Fold,
    arg: Option<usize>,
    by: Option<usize>,
}

/// What a column of an aggregating list answers: a group key's value, an
/// aggregate's, or an expression over them, its keys and aggregates made
/// parameters -- the keys first, then the aggregates, then the query's.
enum Out {
    Key(usize),
    Fold(usize),
    Expr(Expr),
}

/// The aggregates `e` calls, outside what is a group key.
fn collect<'q>(e: &'q Expr, keys: &[&Expr], out: &mut Vec<&'q Expr>) {
    if keys.iter().any(|k| is_key(k, e)) {
        return;
    }
    if e.aggregate_name().is_some() {
        out.push(e);
        return;
    }
    e.each_child(&mut |c| collect(c, keys, out));
}

/// An aggregating list bound to its collection, once a query: a `get`'s,
/// or a `lookup` level's, whose every parent's children are folded by the
/// one binding -- what a row is read into, the fold each item starts at,
/// and how a group's row is worked out of its keys and folds. Bound a
/// parent, a page of 20 shops' orders folded took 0.2 ms more than the
/// folds themselves.
pub(super) struct Folding<'q> {
    pub(super) rd: Reader<'q>,
    keys: Vec<&'q Expr>,
    terms: Vec<usize>,
    /// A `nocase` field's groups are its folds: `Alice` and `alice` one,
    /// shown as the first row to hold it wrote it.
    key_folds: Vec<Option<CaseFold>>,
    folded: bool,
    items: Vec<Item>,
    /// Each item's slot, `count(*)`'s one holding a value that is no null:
    /// one slot read a value whatever the item.
    args: Vec<usize>,
    outs: Vec<Out>,
    having: Option<Expr>,
    list: &'q [Column],
}

/// The groups rows were folded into: emptied for each parent of a
/// `lookup` level, the room of its maps and lists kept.
#[derive(Default)]
pub(super) struct Groups {
    /// A group's number under its keys' encoding. The hash index's own map
    /// type, holding one number: a map of another type was 1 KB of the
    /// browser module. `count(distinct ...)`'s values go in one more of
    /// them, each under its item's and its group's numbers.
    index: crate::maps::Map<Vec<u8>, Bucket>,
    seen: crate::maps::Map<Vec<u8>, Bucket>,
    key_values: Vec<Value>,
    folds: Vec<Fold>,
    n: usize,
    key: Vec<u8>,
    last_key: Vec<u8>,
    dkey: (Vec<u8>, usize),
    /// The group the row before went to, and whether it was the one
    /// before that's too: only then is a row's key asked of it first.
    last: usize,
    streak: bool,
}

/// What keeps a group's row -- a `lookup ... required` hanging off the
/// groups, asked of it -- or keeps every one.
pub(super) type Keep<'k> = Option<&'k mut dyn FnMut(&[Value]) -> Result<bool>>;

/// The groups a list answers, worked out: each one's columns, `width` a
/// group, and its keys' values, `nk` a group.
pub(super) struct Answered {
    values: Vec<Value>,
    key_values: Vec<Value>,
    n: usize,
}

impl<'q> Folding<'q> {
    /// `list` folded over rows of `c`, grouped by `group`, its groups held
    /// to `having`.
    pub(super) fn new(
        c: &'q Collection,
        list: &'q [Column],
        group: &'q [Expr],
        having: Option<&'q Expr>,
        params: &[Value],
    ) -> Result<Folding<'q>> {
        let keys = group_keys(list, group);
        let mut calls: Vec<&Expr> = Vec::new();
        for col in list {
            collect(&col.expr, &keys, &mut calls);
        }
        // `having`'s own aggregates are folded after the list's, numbered on.
        if let Some(h) = having {
            collect(h, &keys, &mut calls);
        }
        let mut read: Vec<&Expr> = keys.clone();
        read.extend(calls.iter().copied());
        let mut rd = Reader::new(c, &read, params)?;
        let mut terms = Vec::with_capacity(keys.len());
        for k in &keys {
            terms.push(rd.bind(k)?);
        }
        let key_folds: Vec<Option<CaseFold>> = terms
            .iter()
            .map(|&t| rd.typed(t).and_then(|f| f.case_fold()))
            .collect();
        let folded = key_folds.iter().any(Option::is_some);
        let mut items = Vec::with_capacity(calls.len());
        for call in &calls {
            let Expr::Call(name, args) = call else {
                unreachable!("an aggregate is a call")
            };
            let (arg, distinct) = match args.first() {
                Some(Expr::Call(d, x)) if d == "distinct" => (x.first(), true),
                a => (a, false),
            };
            let arg = arg.map(|a| rd.bind(a)).transpose()?;
            let ty = arg.and_then(|t| rd.typed(t));
            let by = args.get(1).map(|b| rd.bind(b)).transpose()?;
            let mut start = Fold::new(name, distinct, ty)?;
            // `first(e by k)` orders by `k` as its field compares.
            if let (Fold::Pick(_, _, text), Some(b)) = (&mut start, by) {
                *text = rd.typed(b).map_or(TextOrder::BYTES, |f| f.text_order());
            }
            items.push(Item { start, arg, by });
        }
        let (nk, ni) = (keys.len(), items.len());
        let mut outs = Vec::with_capacity(list.len());
        // The aggregates are numbered as `collect` met them, column by
        // column, so a copy's are told apart by their turn.
        let mut turn = 0;
        for col in list {
            let e = &col.expr;
            outs.push(if let Some(j) = keys.iter().position(|k| is_key(k, e)) {
                Out::Key(j)
            } else if e.aggregate_name().is_some() {
                turn += 1;
                Out::Fold(turn - 1)
            } else {
                let given = params.len();
                let mut e = e.clone();
                e.rewrite(&mut |x| {
                    *x = if let Some(j) = keys.iter().position(|k| is_key(k, x)) {
                        Expr::Param(j)
                    } else if x.aggregate_name().is_some() {
                        turn += 1;
                        Expr::Param(nk + turn - 1)
                    } else {
                        match x {
                            Expr::Param(i) if *i >= given => return Err(unbound(*i)),
                            Expr::Param(i) => Expr::Param(nk + ni + *i),
                            _ => return Ok(false),
                        }
                    };
                    Ok(true)
                })?;
                Out::Expr(e)
            });
        }
        // `having` as a column is worked out, over the keys, the folds and
        // the parameters -- and the list's columns after them, by name.
        let given = params.len();
        let having = match having {
            None => None,
            Some(h) => {
                let mut h = h.clone();
                h.rewrite(&mut |x| {
                    *x = if let Some(j) = keys.iter().position(|k| is_key(k, x)) {
                        Expr::Param(j)
                    } else if x.aggregate_name().is_some() {
                        turn += 1;
                        Expr::Param(nk + turn - 1)
                    } else {
                        match x {
                            Expr::Field(n) => match list.iter().position(|c| c.name == *n) {
                                Some(i) => Expr::Param(nk + ni + given + i),
                                None => return Ok(false),
                            },
                            Expr::Param(i) if *i >= given => return Err(unbound(*i)),
                            Expr::Param(i) => Expr::Param(nk + ni + *i),
                            _ => return Ok(false),
                        }
                    };
                    Ok(true)
                })?;
                Some(h)
            }
        };
        rd.env.push(Value::Bool(true));
        let counted = rd.env.len() - 1;
        let args: Vec<usize> = items.iter().map(|it| it.arg.unwrap_or(counted)).collect();
        Ok(Folding {
            rd,
            keys,
            terms,
            key_folds,
            folded,
            items,
            args,
            outs,
            having,
            list,
        })
    }

    /// The list's column names.
    pub(super) fn columns(&self) -> Vec<String> {
        self.list.iter().map(|c| c.name.clone()).collect()
    }

    /// `g` emptied for rows of a new set: none, or ungrouped the one row
    /// every list answers, its folds at their start.
    pub(super) fn start(&self, g: &mut Groups) {
        g.index.clear();
        g.seen.clear();
        g.key_values.clear();
        g.folds.clear();
        (g.n, g.last, g.streak, g.dkey.1) = (0, usize::MAX, false, 0);
        if self.keys.is_empty() {
            g.n = 1;
            g.folds.extend(self.items.iter().map(|it| it.start.clone()));
        }
    }

    /// Folds the rows `ids` into `g`, the group of each by its keys.
    pub(super) fn fold(
        &mut self,
        g: &mut Groups,
        ids: &[DocId],
        reg: &Registry,
        clock: Option<i64>,
    ) -> Result<()> {
        let (nk, ni) = (self.keys.len(), self.items.len());
        for &id in ids {
            if !self.rd.read(id, reg, clock)? {
                continue;
            }
            let env = &self.rd.env;
            let at = match nk {
                0 => 0,
                _ => {
                    g.key.clear();
                    match self.folded {
                        false => {
                            for t in &self.terms {
                                crate::codec::encode_key(&mut g.key, &env[*t]);
                            }
                        }
                        true => {
                            for (t, f) in self.terms.iter().zip(&self.key_folds) {
                                encode_folded(&mut g.key, &env[*t], *f);
                            }
                        }
                    }
                    // Rows in a time series come in the order they were
                    // written, a bucket's together: while they do, the
                    // group the row before went to is asked first. Asked
                    // every row, keys in no order took 8% longer.
                    if g.streak && g.key == g.last_key {
                        g.last
                    } else {
                        let n = match g.index.get(&g.key) {
                            Some(n) => n.first().unwrap_or(0) as usize,
                            None => {
                                g.index.insert(g.key.clone(), Bucket::one(g.n as DocId));
                                for t in &self.terms {
                                    g.key_values.push(env[*t].clone());
                                }
                                g.folds.extend(self.items.iter().map(|it| it.start.clone()));
                                g.n += 1;
                                g.n - 1
                            }
                        };
                        // Swapped, not copied: `key` is written over
                        // next row.
                        std::mem::swap(&mut g.key, &mut g.last_key);
                        g.streak = n == g.last;
                        g.last = n;
                        n
                    }
                }
            };
            let base = at * ni;
            for (i, (fold, &arg)) in g.folds[base..base + ni]
                .iter_mut()
                .zip(&self.args)
                .enumerate()
            {
                let v = &env[arg];
                if !fold.add(v)? {
                    let by = self.items[i].by.map(|b| &env[b]);
                    fold.row_held(v, by, id, (i, at), &mut g.seen, &mut g.dkey)?
                }
            }
        }
        Ok(())
    }

    /// The groups of `g` answered, each a row of the list's columns: the
    /// ones `having` holds for and `keep` keeps -- a `lookup ... required`
    /// hanging off them, asked of each group's row -- before any order or
    /// page sees them. Takes the folds out of `g`.
    pub(super) fn rows(
        &self,
        g: &mut Groups,
        params: &[Value],
        reg: &Registry,
        clock: Option<i64>,
        mut keep: Keep,
    ) -> Result<Answered> {
        let (nk, ni, n) = (self.keys.len(), self.items.len(), g.n);
        let width = self.list.len();
        let mut values: Vec<Value> = Vec::with_capacity(n * width);
        let mut env: Vec<Value> = Vec::with_capacity(nk + ni + params.len());
        let mut folds = g.folds.drain(..);
        let mut passed: Vec<Value> = Vec::new();
        let sifted = self.having.is_some() || keep.is_some();
        for at in 0..n {
            env.clear();
            env.extend_from_slice(&g.key_values[at * nk..at * nk + nk]);
            // A loop: `extend` over the folds' `take` was 0.3 KB more of
            // the browser module.
            for f in folds.by_ref().take(ni) {
                env.push(f.value());
            }
            env.extend_from_slice(params);
            for out in &self.outs {
                values.push(match out {
                    Out::Key(j) => env[*j].clone(),
                    Out::Fold(i) => env[nk + i].clone(),
                    Out::Expr(e) => {
                        let ctx = EvalCtx {
                            params: &env,
                            registry: reg,
                            clock,
                        };
                        eval(e, &mut IdRow(0), &ctx)?
                    }
                });
            }
            if !sifted {
                continue;
            }
            // A group `having` does not hold for is taken back out, its
            // keys with it, before the order and the page see it: the
            // groups answered are the groups that pass.
            let row = values.len() - width;
            let mut holds = true;
            if let Some(h) = &self.having {
                env.extend_from_slice(&values[row..]);
                let ctx = EvalCtx {
                    params: &env,
                    registry: reg,
                    clock,
                };
                holds = truthy(&eval(h, &mut IdRow(0), &ctx)?);
            }
            if let (true, Some(k)) = (holds, keep.as_mut()) {
                holds = k(&values[row..])?;
            }
            match holds {
                true => passed.extend_from_slice(&g.key_values[at * nk..at * nk + nk]),
                false => values.truncate(row),
            }
        }
        drop(folds);
        let (key_values, n) = match sifted {
            true => (passed, values.len() / width.max(1)),
            false => (std::mem::take(&mut g.key_values), n),
        };
        Ok(Answered {
            values,
            key_values,
            n,
        })
    }

    /// The rows of `a` in the order `order` names -- the list's columns --
    /// the keys breaking what it leaves tied, `offset` skipped and `limit`
    /// kept: groups come out by their keys unless it says otherwise.
    pub(super) fn page(
        &self,
        c: &Collection,
        a: Answered,
        order: &[Sort],
        limit: Option<usize>,
        offset: usize,
    ) -> Result<Vec<Row>> {
        let (nk, n, width) = (self.keys.len(), a.n, self.list.len());
        let mut keys: Vec<OrderKey> = Vec::with_capacity(order.len() + nk);
        let mut picked: Vec<usize> = Vec::with_capacity(order.len());
        for s in order {
            let name = &s.field;
            let Some(at) = self.list.iter().position(|c| c.name == *name) else {
                return Err(Error::Query(format!(
                    "`order {name}`: not a column of this select"
                )));
            };
            // A key, `min`, `max`, `first` and `last` carry a field's
            // values, and order in its collation when the query names
            // none; `collate` orders only text.
            let field = carried_field(&self.list[at].expr).and_then(|f| c.schema.field(f));
            if let Some(coll) = s.collate {
                if !field.is_some_and(|f| collatable(&f.ty)) {
                    return Err(Error::Query(format!(
                        "`collate {}` orders text; `{name}` is not",
                        coll.name()
                    )));
                }
            }
            picked.push(at);
            let text = field.map_or(TextOrder::BYTES, |f| f.text_order());
            keys.push((
                None,
                s.asc,
                TextOrder {
                    coll: s.collate.or(text.coll),
                    fold: text.fold,
                },
                None,
            ));
        }
        // A `nocase` key by its fold, as its groups were told apart.
        for k in &self.keys {
            let fold = match k {
                Expr::Field(f) => c.schema.field(f).and_then(|f| f.case_fold()),
                _ => None,
            };
            keys.push((None, true, TextOrder { coll: None, fold }, None));
        }
        let w = keys.len();
        let mut flat: Vec<Value> = Vec::with_capacity(n * w);
        for at in 0..n {
            for &p in &picked {
                flat.push(a.values[at * width + p].clone());
            }
            flat.extend_from_slice(&a.key_values[at * nk..at * nk + nk]);
        }
        let k = limit.map_or(n, |l| l.saturating_add(offset)).min(n);
        let mut rows = Vec::with_capacity(k.saturating_sub(offset));
        for at in order_rows(&flat, w, n, &keys, k).into_iter().skip(offset) {
            rows.push(Row {
                id: 0,
                values: a.values[at * width..at * width + width].to_vec(),
                score: None,
            });
        }
        Ok(rows)
    }
}

impl Database {
    /// The groups of a count by a timestamp or int field's `bucket` -- or
    /// the field itself -- over a range of it, read off its ordered index:
    /// every key `count(*)` alone, the filter that range and nothing more
    /// (a row's `@ttl` is a bound of it too), so no row is read. The keys
    /// come out in order and `bucket` keeps it, so a group ends where the
    /// next begins. The number of values counted, or `None` where the
    /// query is not that shape. Native only: per-minute counts over a day
    /// of a million events took 23.5 ms reading the rows, the browser
    /// module would carry it for nothing it is asked.
    #[cfg(not(target_arch = "wasm32"))]
    fn counted_by_index(
        &self,
        c: &Collection,
        sel: &Select,
        f: &mut Folding,
        g: &mut Groups,
    ) -> Result<Option<usize>> {
        let (keys, terms, items, rd) = (&f.keys, &f.terms, &f.items, &mut f.rd);
        let params = &rd.env[rd.base()..];
        fn field(e: &Expr) -> Option<&str> {
            match e {
                Expr::Field(f) => Some(f),
                Expr::Call(b, a) if b == "bucket" && matches!(a.len(), 2 | 3) => match &a[0] {
                    Expr::Field(f) => Some(f),
                    _ => None,
                },
                _ => None,
            }
        }
        // By a zone's clock a bucket may come back as the keys go up -- an
        // hour a clock shows twice, cut by an interval not of whole hours --
        // so its groups are looked up by their keys, as the rows' are.
        let zoned =
            (keys.iter()).any(|k| matches!(k, Expr::Call(b, a) if b == "bucket" && a.len() == 3));
        let mut made: crate::maps::Map<Vec<u8>, Bucket> = Default::default();
        let mut cur = 0usize;
        let (Some(f), Some(filter)) = (keys.first().and_then(|k| field(k)), &sel.filter) else {
            return Ok(None);
        };
        if keys.iter().any(|k| field(k) != Some(f))
            || items
                .iter()
                .any(|it| it.arg.is_some() || !matches!(it.start, Fold::Count(_)))
            || !c.sorted.iter().any(|(n, _)| n == f)
        {
            return Ok(None);
        }
        let Some(fd) = c.schema.field(f) else {
            return Ok(None);
        };
        if !matches!(fd.ty, DataType::Timestamp | DataType::Int) {
            return Ok(None);
        }
        // The filter's terms: ranges on the field, and `@ttl`'s `not (f <=
        // cutoff)`, which is a bound of it too; anything else reads rows.
        let mut terms_of = vec![filter];
        let (mut ranges, mut alive) = (0, Vec::new());
        while let Some(t) = terms_of.pop() {
            match t {
                Expr::And(a, b) => terms_of.extend([&**a, &**b]),
                Expr::Not(x) => match &**x {
                    Expr::Cmp(CmpOp::Le, a, b) => match (&**a, &**b) {
                        (Expr::Field(n), Expr::Lit(v)) if n == f => alive.push(v),
                        _ => return Ok(None),
                    },
                    _ => return Ok(None),
                },
                t if t.range_key(params).is_some_and(|r| r.0 == f) => ranges += 1,
                _ => return Ok(None),
            }
        }
        let Some((mut range, true)) = sorted_range(fd, f, filter, params).filter(|_| ranges > 0)
        else {
            return Ok(None);
        };
        for v in alive {
            match SortedIndex::bound(&fd.ty, None, v) {
                Some(k) => range.narrow(CmpOp::Gt, k),
                None => return Ok(None),
            }
        }
        let Some(ix) = c.sorted_index(f)? else {
            return Ok(None);
        };
        let slot = rd.slot(f);
        let int = fd.ty == DataType::Int;
        let (reg, clock) = (&self.registry, self.clock);
        let (mut key, mut last, mut n) = (Vec::new(), Vec::new(), 0usize);
        let walked = ix.each_int(&range, &mut |t| {
            rd.env[slot] = if int {
                Value::Int(t)
            } else {
                Value::Timestamp(t)
            };
            rd.work_out(0, reg, clock)?;
            key.clear();
            for t in terms.iter() {
                crate::codec::encode_value(&mut key, &rd.env[*t]);
            }
            if n == 0 || key != last {
                let back = match zoned {
                    true => made.get(&key).and_then(Bucket::first),
                    false => None,
                };
                match back {
                    Some(j) => cur = j as usize,
                    None => {
                        cur = g.folds.len() / items.len().max(1);
                        if zoned {
                            made.insert(key.clone(), Bucket::one(cur as DocId));
                        }
                        for t in terms.iter() {
                            g.key_values.push(rd.env[*t].clone());
                        }
                        g.folds.extend(items.iter().map(|it| it.start.clone()));
                    }
                }
                std::mem::swap(&mut key, &mut last);
            }
            let at = cur * items.len();
            for f in &mut g.folds[at..at + items.len()] {
                f.add(&Value::Bool(true))?;
            }
            n += 1;
            Ok(())
        })?;
        if !walked {
            return Ok(None);
        }
        if !keys.is_empty() {
            g.n = g.key_values.len() / keys.len();
        }
        plan(|| format!("aggregate: the ordered index on {f} counted, no row read"));
        Ok(Some(n))
    }

    /// An aggregating select: the rows the filter finds -- through the same
    /// indexes any select uses -- folded into one row, or one per distinct
    /// set of the group's keys, each row in the select list's order; and
    /// the level a `lookup` hangs off them, keyed by their columns.
    ///
    /// Written as plain loops over types the engine already has -- the hash
    /// index's map, `order`'s sort: the first version, in iterator chains
    /// over types of its own, was 25 KB of the browser module.
    pub(super) fn aggregate(
        &self,
        c: &Collection,
        sel: &Select,
        params: &[Value],
    ) -> Result<ResultSet> {
        let mut f = Folding::new(c, &sel.aggregate, &sel.group, sel.having.as_ref(), params)?;
        let mut g = Groups::default();
        f.start(&mut g);
        let (reg, clock) = (&self.registry, self.clock);
        let ctx = EvalCtx {
            params,
            registry: reg,
            clock,
        };
        // Ungrouped, `order`, `limit` and `offset` pick the rows the one
        // row folds, as they pick a `get`'s page.
        let picks = sel.group.is_empty()
            && (!sel.order.is_empty() || sel.limit.is_some() || !sel.offset.is_zero());
        #[cfg(not(target_arch = "wasm32"))]
        let walked = match picks {
            true => None,
            false => self.counted_by_index(c, sel, &mut f, &mut g)?,
        };
        #[cfg(target_arch = "wasm32")]
        let walked: Option<usize> = None;
        let ids = match walked {
            Some(_) => Vec::new(),
            None if picks => {
                // Picked as the `get`'s page is, without the `lookup`, which
                // hangs off the groups rather than sifting the rows.
                let bare;
                let page = match sel.lookup {
                    Some(_) => {
                        bare = Select {
                            lookup: None,
                            ..sel.clone()
                        };
                        &bare
                    }
                    None => sel,
                };
                let all = sel.filter.is_none();
                let (mut ids, _) = self.page_ids(c, page, params, &ctx, false, all, false)?;
                ids.drain(..sel.offset.at(params).min(ids.len()));
                ids
            }
            None => self.matching_ids(&sel.collection, &sel.filter, params)?,
        };
        f.fold(&mut g, &ids, reg, clock)?;
        // In the browser module `walked` is always `None`.
        #[allow(clippy::unnecessary_literal_unwrap)]
        let read = walked.unwrap_or(ids.len());
        let n = g.n;
        plan(|| {
            format!(
                "aggregate: {read} rows into {n} {}",
                if n == 1 { "group" } else { "groups" }
            )
        });
        // A `lookup` hangs off the groups: resolved against the list's
        // columns, and where `required`, asked of each group's row before
        // the order and the page.
        let steps = match &sel.lookup {
            Some(l) => Some(self.lookup_chain(c, l, Some(&sel.aggregate))?),
            None => None,
        };
        let mut keep = |row: &[Value]| -> Result<bool> {
            let steps = steps.as_deref().unwrap_or_default();
            steps[0].any_of(row, &mut |cid| survives(steps, 0, cid, &ctx))
        };
        let required = steps.as_ref().is_some_and(|s| s[0].l.required);
        let keep: Keep = match required {
            true => Some(&mut keep),
            false => None,
        };
        let answered = f.rows(&mut g, params, reg, clock, keep)?;
        if sel.count {
            return Ok(ResultSet::counted(answered.n, Vec::new()));
        }
        let rows = match picks {
            true => f.page(c, answered, &[], None, 0)?,
            false => f.page(
                c,
                answered,
                &sel.order,
                sel.limit.map(|l| l.at(params)),
                sel.offset.at(params),
            )?,
        };
        let nested = match &steps {
            Some(steps) => Some(self.run_level(steps, 0, Parents::Rows(&rows), &ctx)?),
            None => None,
        };
        Ok(ResultSet {
            columns: f.columns(),
            rows,
            nested,
            facets: Vec::new(),
        })
    }

    /// A `lookup` level whose list aggregates: each parent's children --
    /// probed once, its `where` and any `required` level below held, and
    /// ungrouped those its `order`, `limit` and `offset` pick -- folded by
    /// the list bound once for the level, into one row a parent, or one a
    /// group of them in the order and page the level names. A parent with
    /// no child answers the one row every ungrouped list does -- `count`
    /// 0, a sum null -- and no group.
    pub(super) fn fold_level(
        &self,
        steps: &[Step],
        depth: usize,
        parents: Parents,
        ctx: &EvalCtx,
    ) -> Result<Nested> {
        let l = steps[depth].l;
        let child = steps[depth].child;
        let params = ctx.params;
        let (reg, clock) = (ctx.registry, ctx.clock);
        let mut f = Folding::new(child, &l.aggregate, &l.group, l.having.as_ref(), params)?;
        let mut g = Groups::default();
        let grouped = !l.group.is_empty();
        let (limit, offset) = (l.limit.map(|n| n.at(params)), l.offset.at(params));
        // Ungrouped, the children are picked as the level's rows would be;
        // grouped, every child is folded and the groups paged.
        let mut keys = Vec::new();
        if !grouped {
            let owner = format!("{}.", l.collection);
            for s in &l.order {
                keys.push(order_key(&child.schema, s, &owner)?);
            }
        }
        let want = match grouped {
            true => usize::MAX,
            false => offset.saturating_add(limit.unwrap_or(usize::MAX)),
        };
        // A `required` level below drops a group no child of which survives,
        // before the order and the page.
        let below = steps.get(depth + 1).filter(|s| s.l.required);
        let mut keep = |row: &[Value]| -> Result<bool> {
            match below {
                Some(s) => s.any_of(row, &mut |cid| survives(steps, depth + 1, cid, ctx)),
                None => Ok(true),
            }
        };
        let mut groups = Vec::with_capacity(parents.len());
        let mut bucket: Vec<DocId> = Vec::new();
        let (mut children, mut answered) = (0usize, 0usize);
        for i in 0..parents.len() {
            let mut kept =
                self.kept_children(steps, depth, parents.at(i), &keys, want, &mut bucket, ctx)?;
            if !grouped {
                kept.truncate(want);
                kept.drain(..offset.min(kept.len()));
            }
            children += kept.len();
            f.start(&mut g);
            f.fold(&mut g, &kept, reg, clock)?;
            let sift: Keep = match below {
                Some(_) => Some(&mut keep),
                None => None,
            };
            let a = f.rows(&mut g, params, reg, clock, sift)?;
            // Ungrouped, the one row as it is: put in order and paged, a
            // page of 20 parents' latest 50 took 24 us against 18.
            let rows = match grouped {
                true => f.page(child, a, &l.order, limit, offset)?,
                false => vec![Row {
                    id: 0,
                    values: a.values,
                    score: None,
                }],
            };
            answered += rows.len();
            groups.push(rows);
        }
        plan(|| {
            format!(
                "lookup: {} on {}, {} for {} parents, {children} children folded into {answered} rows",
                l.collection,
                l.side(true),
                steps[depth].probed(),
                parents.len()
            )
        });
        let nested = match steps.get(depth + 1) {
            None => None,
            Some(_) => {
                let rows: Vec<Row> = groups.iter().flatten().cloned().collect();
                Some(Box::new(self.run_level(
                    steps,
                    depth + 1,
                    Parents::Rows(&rows),
                    ctx,
                )?))
            }
        };
        Ok(Nested {
            name: l.collection.clone(),
            columns: f.columns(),
            groups,
            nested,
            flat: None,
        })
    }
}
