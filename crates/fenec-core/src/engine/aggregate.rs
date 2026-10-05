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
    /// `bucket(x, 15m)` over a constant interval, `x`'s slot.
    Bucket(usize, Interval),
    /// Anything else: the expression, its fields made parameters.
    Expr(Expr),
}

fn unbound(i: usize) -> Error {
    Error::Query(format!("parameter ${} is not bound", i + 1))
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
            Expr::Call(name, args) if name == "bucket" && args.len() == 2 => {
                let iv = match &args[1] {
                    Expr::Lit(v) => Some(v),
                    Expr::Param(i) => self.env.get(self.base() + i),
                    _ => None,
                };
                match iv {
                    Some(v) => {
                        let iv = Interval::of(v)?;
                        Calc::Bucket(self.bind(&args[0])?, iv)
                    }
                    None => Calc::Expr(self.lift(e)?),
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
                Calc::Bucket(x, iv) => iv.truncate(&self.env[*x])?,
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
    /// `true` for `max`; the field's collation, which its text orders in.
    Extreme(Option<Value>, bool, Option<Collation>),
    /// `count(distinct ...)`: the values are in the query's one set.
    Distinct(i64),
    /// `first`, or `last` when `true`: the row's order and id, and its value.
    /// Boxed, so that every fold is the size it was: a fold the size of
    /// two values and an id took the fixed aggregates of before 11%
    /// longer.
    Pick(Option<Box<(Value, DocId, Value)>>, bool),
}

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
        Ok(match (name, t) {
            ("count", _) if distinct => Fold::Distinct(0),
            ("count", _) => Fold::Count(0),
            ("sum", Some(DataType::Float)) => Fold::SumFloat(0.0, 0),
            ("sum" | "avg", Some(DataType::Int | DataType::Float) | None) => match name {
                "sum" => Fold::SumInt(0, 0),
                _ => Fold::Avg(0.0, 0),
            },
            ("sum" | "avg", _) => return refuse("an int or float field"),
            ("first" | "last", _) => Fold::Pick(None, name == "last"),
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
            ) => Fold::Extreme(None, name == "max", ty.and_then(|f| f.collate)),
            _ => return refuse("a field with an order"),
        })
    }

    /// Folds `v` in; false for `count(distinct ...)`, `first` and `last`,
    /// which need the row as well ([`Fold::row_held`]). One dispatch a
    /// value: asked which they are first, the fixed aggregates of before
    /// took 4% longer.
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    fn add(&mut self, v: &Value) -> Result<bool> {
        if matches!(v, Value::Null) && !matches!(self, Fold::Count(_)) {
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
            Fold::Extreme(best, max, coll) => {
                let better = match best {
                    None => true,
                    Some(b) => {
                        let o = match coll {
                            Some(c) => c.compare_values(v, b),
                            None => v.cmp_value(b),
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
            Fold::Distinct(_) | Fold::Pick(..) => return Ok(false),
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
        seen: &mut crate::maps::Map<Vec<u8>, Vec<DocId>>,
        key: &mut Vec<u8>,
    ) -> Result<()> {
        if v.is_null() {
            return Ok(());
        }
        match self {
            Fold::Distinct(n) => {
                key.clear();
                key.extend_from_slice(&(at.0 as u32).to_le_bytes());
                key.extend_from_slice(&(at.1 as u32).to_le_bytes());
                crate::codec::encode_value(key, v);
                if !seen.contains_key(key.as_slice()) {
                    if seen.len() >= MAX_DISTINCT_VALUES {
                        return Err(Error::Query(format!(
                            "`count(distinct ...)` holds more than {MAX_DISTINCT_VALUES} \
                             values: a count is not cut short, so narrow the filter"
                        )));
                    }
                    seen.insert(key.clone(), Vec::new());
                    *n += 1;
                }
            }
            Fold::Pick(best, later) => {
                let written = Value::Int(id as i64);
                let by = by.unwrap_or(&written);
                if by.is_null() {
                    return Ok(());
                }
                let better = match best.as_deref() {
                    None => true,
                    Some((b, bid, _)) => {
                        let o = by.cmp_value(b).then(id.cmp(bid));
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
            Fold::Count(n) | Fold::Distinct(n) => Value::Int(n),
            Fold::SumInt(_, 0) | Fold::SumFloat(_, 0) | Fold::Avg(_, 0) => Value::Null,
            Fold::SumInt(s, _) => Value::Int(s),
            Fold::SumFloat(s, _) => Value::Float(s),
            Fold::Avg(s, n) => Value::Float(s / n as f64),
            Fold::Extreme(v, ..) => v.unwrap_or(Value::Null),
            Fold::Pick(p, _) => p.map_or(Value::Null, |p| p.2),
        }
    }
}

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
    #[allow(clippy::too_many_arguments)]
    fn counted_by_index(
        &self,
        c: &Collection,
        sel: &Select,
        keys: &[&Expr],
        terms: &[usize],
        items: &[Item],
        rd: &mut Reader,
        key_values: &mut Vec<Value>,
        folds: &mut Vec<Fold>,
    ) -> Result<Option<usize>> {
        let params = &rd.env[rd.base()..];
        fn field(e: &Expr) -> Option<&str> {
            match e {
                Expr::Field(f) => Some(f),
                Expr::Call(b, a) if b == "bucket" && a.len() == 2 => match &a[0] {
                    Expr::Field(f) => Some(f),
                    _ => None,
                },
                _ => None,
            }
        }
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
            match SortedIndex::bound(&fd.ty, v) {
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
            for t in terms {
                crate::codec::encode_value(&mut key, &rd.env[*t]);
            }
            if n == 0 || key != last {
                for t in terms {
                    key_values.push(rd.env[*t].clone());
                }
                folds.extend(items.iter().map(|it| it.start.clone()));
                std::mem::swap(&mut key, &mut last);
            }
            let at = folds.len() - items.len();
            for f in &mut folds[at..] {
                f.add(&Value::Bool(true))?;
            }
            n += 1;
            Ok(())
        })?;
        if !walked {
            return Ok(None);
        }
        plan(|| format!("aggregate: the ordered index on {f} counted, no row read"));
        Ok(Some(n))
    }

    /// An aggregating select: the rows the filter finds -- through the same
    /// indexes any select uses -- folded into one row, or one per distinct
    /// set of the group's keys, each row in the select list's order.
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
        let keys = sel.group_keys();
        let mut calls: Vec<&Expr> = Vec::new();
        for col in &sel.aggregate {
            collect(&col.expr, &keys, &mut calls);
        }
        let mut read: Vec<&Expr> = keys.clone();
        read.extend(calls.iter().copied());
        let mut rd = Reader::new(c, &read, params)?;
        let mut terms = Vec::with_capacity(keys.len());
        for k in &keys {
            terms.push(rd.bind(k)?);
        }
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
            items.push(Item {
                start: Fold::new(name, distinct, ty)?,
                arg,
                by: args.get(1).map(|b| rd.bind(b)).transpose()?,
            });
        }
        let (nk, ni) = (keys.len(), items.len());
        let mut outs = Vec::with_capacity(sel.aggregate.len());
        // The aggregates are numbered as `collect` met them, column by
        // column, so a copy's are told apart by their turn.
        let mut turn = 0;
        for col in &sel.aggregate {
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

        let (reg, clock) = (&self.registry, self.clock);
        // A group's number under its keys' encoding. The hash index's own
        // map type, holding one number: a map of another type was 1 KB of
        // the browser module. `count(distinct ...)`'s values go in one more
        // of them, each under its item's and its group's numbers.
        let mut index: crate::maps::Map<Vec<u8>, Vec<DocId>> = Default::default();
        let mut seen: crate::maps::Map<Vec<u8>, Vec<DocId>> = Default::default();
        let mut key_values: Vec<Value> = Vec::new();
        let mut folds: Vec<Fold> = Vec::new();
        let mut groups = 0usize;
        if nk == 0 {
            groups = 1;
            folds.extend(items.iter().map(|it| it.start.clone()));
        }
        #[cfg(not(target_arch = "wasm32"))]
        let walked = self.counted_by_index(
            c,
            sel,
            &keys,
            &terms,
            &items,
            &mut rd,
            &mut key_values,
            &mut folds,
        )?;
        #[cfg(target_arch = "wasm32")]
        let walked: Option<usize> = None;
        let ids = match walked {
            Some(_) => Vec::new(),
            None => self.matching_ids(&sel.collection, &sel.filter, params)?,
        };
        if let (Some(_), true) = (walked, nk > 0) {
            groups = key_values.len() / nk;
        }
        // In the browser module `walked` is always `None`.
        #[allow(clippy::unnecessary_literal_unwrap)]
        let read = walked.unwrap_or(ids.len());
        // Each item's slot, `count(*)`'s one holding a value that is no
        // null: one slot read a value whatever the item.
        rd.env.push(Value::Bool(true));
        let counted = rd.env.len() - 1;
        let args: Vec<usize> = items.iter().map(|it| it.arg.unwrap_or(counted)).collect();
        let (mut key, mut last_key, mut dkey) = (Vec::new(), Vec::new(), Vec::new());
        // The group the row before went to, and whether it was the one
        // before that's too: only then is a row's key asked of it first.
        let (mut last, mut streak) = (usize::MAX, false);
        for &id in &ids {
            if !rd.read(id, reg, clock)? {
                continue;
            }
            let g = match nk {
                0 => 0,
                _ => {
                    key.clear();
                    for t in &terms {
                        crate::codec::encode_value(&mut key, &rd.env[*t]);
                    }
                    // Rows in a time series come in the order they were
                    // written, a bucket's together: while they do, the
                    // group the row before went to is asked first. Asked
                    // every row, keys in no order took 8% longer.
                    if streak && key == last_key {
                        last
                    } else {
                        let g = match index.get(&key) {
                            Some(n) => n[0] as usize,
                            None => {
                                index.entry(key.clone()).or_default().push(groups as DocId);
                                for t in &terms {
                                    key_values.push(rd.env[*t].clone());
                                }
                                folds.extend(items.iter().map(|it| it.start.clone()));
                                groups += 1;
                                groups - 1
                            }
                        };
                        // Swapped, not copied: `key` is written over
                        // next row.
                        std::mem::swap(&mut key, &mut last_key);
                        streak = g == last;
                        last = g;
                        g
                    }
                }
            };
            let at = g * ni;
            for (i, (fold, &arg)) in folds[at..at + ni].iter_mut().zip(&args).enumerate() {
                let v = &rd.env[arg];
                if !fold.add(v)? {
                    let by = items[i].by.map(|b| &rd.env[b]);
                    fold.row_held(v, by, id, (i, g), &mut seen, &mut dkey)?
                }
            }
        }
        let n = groups;
        plan(|| {
            format!(
                "aggregate: {read} rows into {n} {}",
                if n == 1 { "group" } else { "groups" }
            )
        });

        let columns: Vec<String> = sel.aggregate.iter().map(|c| c.name.clone()).collect();
        let width = columns.len();
        let mut values: Vec<Value> = Vec::with_capacity(n * width);
        let mut env: Vec<Value> = Vec::with_capacity(nk + ni + params.len());
        let mut folds = folds.into_iter();
        for g in 0..n {
            env.clear();
            env.extend_from_slice(&key_values[g * nk..g * nk + nk]);
            env.extend(folds.by_ref().take(ni).map(Fold::value));
            env.extend_from_slice(params);
            for out in &outs {
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
        }
        // Groups come out by their keys unless `order` says otherwise, naming
        // the list's columns; the keys break what the order leaves tied.
        let mut order: Vec<OrderKey> = Vec::with_capacity(sel.order.len() + nk);
        let mut picked: Vec<usize> = Vec::with_capacity(sel.order.len());
        for s in &sel.order {
            let name = &s.field;
            let Some(at) = columns.iter().position(|c| c == name) else {
                return Err(Error::Query(format!(
                    "`order {name}`: not a column of this select"
                )));
            };
            // A key, `min`, `max`, `first` and `last` carry a field's
            // values, and order in its collation when the query names
            // none; `collate` orders only text.
            let field = carried_field(&sel.aggregate[at].expr).and_then(|f| c.schema.field(f));
            if let Some(coll) = s.collate {
                if !field.is_some_and(|f| collatable(&f.ty)) {
                    return Err(Error::Query(format!(
                        "`collate {}` orders text; `{name}` is not",
                        coll.name()
                    )));
                }
            }
            picked.push(at);
            order.push((
                None,
                s.asc,
                s.collate.or(field.and_then(|f| f.collate)),
                None,
            ));
        }
        for _ in 0..nk {
            order.push((None, true, None, None));
        }
        let w = order.len();
        let mut flat: Vec<Value> = Vec::with_capacity(n * w);
        for g in 0..n {
            for &at in &picked {
                flat.push(values[g * width + at].clone());
            }
            flat.extend_from_slice(&key_values[g * nk..g * nk + nk]);
        }
        let k = sel.limit.map_or(n, |l| l.saturating_add(sel.offset)).min(n);
        let mut rows = Vec::with_capacity(k.saturating_sub(sel.offset));
        for g in order_rows(&flat, w, n, &order, k)
            .into_iter()
            .skip(sel.offset)
        {
            rows.push(Row {
                id: 0,
                values: values[g * width..g * width + width].to_vec(),
                score: None,
            });
        }
        Ok(ResultSet {
            columns,
            rows,
            nested: None,
            facets: Vec::new(),
        })
    }
}
