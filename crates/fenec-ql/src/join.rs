//! SQL's `from orders o join customers c on c.id = o.customer_id`, read as
//! the FenecQL it means: a `lookup` from each row of the first table to its
//! matches in the next, answered flat as SQL answers it ([`Flat`]); or,
//! where only one table's columns are answered and each row of it is
//! answered once, that table's `get` with the other's rows a filter, `x in
//! (get ...)`. Whatever would answer otherwise than SQL does is refused
//! saying what to write.
//!
//! The parser reads no schema, so a name SQL would place by its schema --
//! `total`, with no table before it -- is read as the first table's, where
//! SQL would have refused it if another table had one too; and the key a
//! table is read by is its column in `on`, which the engine holds to an
//! index as a `lookup`'s.

use fenec_core::query::MAX_LOOKUP_DEPTH;
use fenec_core::query::{
    label_of, CmpOp, Count, Expr, Flat, FlatColumn, JoinNames, Lookup, Select, Sort,
};
use fenec_core::value::Value;
use std::sync::Arc;

/// A table of `from`, and the name the statement calls it by.
#[derive(Clone)]
pub(crate) struct Table {
    pub collection: String,
    pub alias: Option<String>,
}

/// The tables of SQL's `from`, and how each after the first is joined:
/// `joins[k]` joins `tables[k + 1]` to the one before it.
pub(crate) struct From {
    pub tables: Vec<Table>,
    pub joins: Vec<Join>,
}

/// `[inner] join`, `left [outer] join`, or a comma, whose key is in the
/// `where` (`on` None).
pub(crate) struct Join {
    pub left: bool,
    pub on: Option<Expr>,
}

/// What a join's statement is refused with: the clauses only FenecQL has.
const CLAUSES: &str = "a join takes SQL's clauses; `near`, `match`, `facet` and `lookup` read \
                       one collection";

/// What a join's `on` that pairs no column of each table is refused with.
const KEY: &str = "a join pairs a column of each table in `on` by `=`: `join customers c on \
                   c.id = o.customer_id`";

/// What an aggregate or `distinct` reading both tables is refused with.
const FOLDED: &str = "an aggregate or `distinct` over a join reads one table's columns, the \
                      other's rows a filter; another collection's row beside a group is a \
                      `lookup` after it: `get o select k, count(*) as n group k lookup c on id \
                      = k select name`";

impl From {
    /// The table `name` qualifies, and the name in it: `o.total` is
    /// `total` of the table `o` names, `customers.email` of `customers`.
    /// `None` for a name no table's qualifies -- written bare, or a path
    /// into a json field -- which is the first table's.
    fn owner<'a>(&self, name: &'a str) -> (Option<usize>, &'a str) {
        if let Some((q, rest)) = name.split_once('.') {
            // A table aliased is named by its alias alone, as in SQL.
            for (i, t) in self.tables.iter().enumerate() {
                if t.alias
                    .as_deref()
                    .unwrap_or(&t.collection)
                    .eq_ignore_ascii_case(q)
                {
                    return (Some(i), rest);
                }
            }
        }
        (None, name)
    }

    /// The tables an expression reads, a bit each.
    fn levels(&self, e: &Expr) -> u32 {
        let mut names = Vec::new();
        e.referenced_fields(&mut names);
        names
            .iter()
            .fold(0, |m, n| m | 1 << self.owner(n).0.unwrap_or(0))
    }

    /// `e` with each name its table qualifies written bare, and where
    /// `key` says, the first table's key read as the second's: an inner
    /// join's rows hold the same value in both.
    fn strip(&self, e: &mut Expr, key: Option<(&str, &str)>) {
        let _ = e.rewrite(&mut |e| {
            if let Expr::Field(f) = e {
                let (t, bare) = self.owner(f);
                let bare = match key {
                    Some((a, b)) if t.unwrap_or(0) == 0 && bare == a => b,
                    _ => bare,
                };
                if bare != f {
                    *f = bare.to_string();
                }
            }
            Ok(false)
        });
    }

    /// A column's name as written bare: `o.total` is `total`, and an
    /// aggregate's label `count(o.id)` is `count(id)`, as the list names
    /// the item written with no table.
    fn label(&self, name: &str) -> String {
        match name.split_once('(') {
            Some((call, rest)) => {
                let inner = rest.strip_suffix(')').unwrap_or(rest);
                let (head, field) = match inner.strip_prefix("distinct ") {
                    Some(f) => ("distinct ", f),
                    None => ("", inner),
                };
                format!("{call}({head}{})", self.owner(field).1)
            }
            None => self.owner(name).1.to_string(),
        }
    }

    /// The key pair a term of join `j`'s `on` is -- `c.id = o.customer_id`
    /// -- as the joined table's column and the one before's, each with
    /// whether it was written with no table. A side with no table is the
    /// other side's other: SQL places it by the schema.
    #[allow(clippy::type_complexity)]
    fn key(
        &self,
        t: &Expr,
        j: usize,
    ) -> Result<Option<((String, bool), (String, bool))>, &'static str> {
        let Expr::Cmp(CmpOp::Eq, a, b) = t else {
            return Ok(None);
        };
        let (Expr::Field(a), Expr::Field(b)) = (&**a, &**b) else {
            return Ok(None);
        };
        let ((ta, ra), (tb, rb)) = (self.owner(a), self.owner(b));
        let (a, b) = ((ra, ta.is_none()), (rb, tb.is_none()));
        let pair = match (ta, tb) {
            (Some(x), Some(y)) if x == j && y + 1 == j => (a, b),
            (Some(x), Some(y)) if y == j && x + 1 == j => (b, a),
            (Some(x), None) if x == j => (a, b),
            (None, Some(y)) if y == j => (b, a),
            (Some(x), None) if x + 1 == j => (b, a),
            (None, Some(y)) if y + 1 == j => (a, b),
            (Some(x), Some(y)) if (x == j) != (y == j) => {
                return Err(
                    "a join hangs off the table just before it: name the tables in \
                            the order their keys lead",
                )
            }
            _ => return Ok(None),
        };
        Ok(Some((
            (pair.0 .0.to_string(), pair.0 .1),
            (pair.1 .0.to_string(), pair.1 .1),
        )))
    }
}

/// Every term of an `and` chain.
fn terms(e: Option<Expr>) -> Vec<Expr> {
    let mut out = Vec::new();
    let mut todo: Vec<Expr> = Vec::new();
    if let Some(e) = e {
        todo.push(e);
    }
    while let Some(e) = todo.pop() {
        match e {
            Expr::And(a, b) => {
                todo.push(*b);
                todo.push(*a);
            }
            e => out.push(e),
        }
    }
    out
}

/// The terms joined by `and` again, in order.
fn joined(terms: Vec<Expr>) -> Option<Expr> {
    let mut out = None;
    for t in terms {
        out = Some(match out {
            None => t,
            Some(e) => Expr::And(Box::new(e), Box::new(t)),
        });
    }
    out
}

/// An item of SQL's list: its expression as written, its name, and for
/// `o.*` the table whose every column it is.
struct Item {
    expr: Expr,
    name: String,
    star: Option<usize>,
}

/// `sel`, read off SQL's `from`: an alias alone written away, a join the
/// statement it means ([`From`]). `distinct`: SQL's `select distinct`,
/// which the caller makes a group of; here it says a row answered twice is
/// answered once.
#[inline(never)]
pub(crate) fn relate(
    mut sel: Select,
    mut from: From,
    distinct: bool,
) -> Result<Select, &'static str> {
    let n = from.tables.len();
    if n == 1 {
        bare(&mut sel, &from, None);
        return Ok(sel);
    }
    if sel.near.is_some()
        || sel.matcher.is_some()
        || sel.rerank.is_some()
        || sel.fuse.is_some()
        || !sel.facets.is_empty()
        || !sel.marks.is_empty()
        || sel.lookup.is_some()
        || sel.count
        || sel.require.is_some()
        || sel.order.iter().any(sort_is_distance)
    {
        return Err(CLAUSES);
    }
    const _: () = assert!(MAX_LOOKUP_DEPTH == 8);
    if n > MAX_LOOKUP_DEPTH + 1 {
        return Err("a join reads at most 9 tables");
    }
    // Each table's own terms, and each join's key.
    let mut filters: Vec<Vec<Expr>> = Vec::new();
    let mut keys: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    // Every column the statement names, with its table ([`Flat::names`]).
    let mut names: Vec<(usize, String, bool)> = Vec::new();
    for _ in 0..n {
        filters.push(Vec::new());
        keys.push((Vec::new(), Vec::new()));
    }
    let mut rest = terms(sel.filter.take());
    for j in 1..n {
        let left = from.joins[j - 1].left;
        let on = match from.joins[j - 1].on.take() {
            Some(e) => terms(Some(e)),
            // A comma: the key is among the `where`'s terms.
            None => {
                let mut on = Vec::new();
                for t in std::mem::take(&mut rest) {
                    match from.key(&t, j)? {
                        Some(_) => on.push(t),
                        None => rest.push(t),
                    }
                }
                on
            }
        };
        for t in on {
            if let Some(((child, cb), (parent, pb))) = from.key(&t, j)? {
                names.push((j, child.clone(), cb));
                names.push((j - 1, parent.clone(), pb));
                keys[j].0.push(child);
                keys[j].1.push(parent);
                continue;
            }
            let m = from.levels(&t);
            if m == 1 << j {
                filters[j].push(t);
            } else if m & 1 << j != 0 {
                return Err(KEY);
            } else if !left {
                rest.push(t);
            } else {
                return Err(
                    "a LEFT JOIN's `on` reads its own table's columns beside the key: a \
                     condition there on a table before it keeps that table's rows and drops \
                     only their matches, which no `lookup` says -- in the `where` it would \
                     drop the rows",
                );
            }
        }
        if keys[j].0.is_empty() {
            return Err(KEY);
        }
        if left && j + 1 < n && !from.joins[j].left {
            return Err(
                "an inner JOIN after a LEFT JOIN drops the rows it kept with nulls: \
                        write the inner joins first",
            );
        }
    }
    // `left join orders o on .. where o.id is null`: the rows with none, the
    // one condition in the `where` a LEFT JOIN's table takes. Any other
    // reads the rows it kept with nulls; one no such row holds makes it an
    // inner join, which is refused to be written so: telling which
    // conditions hold for no null row was code for a spelling SQL's own
    // style guides warn of.
    let mut none = false;
    for t in rest {
        let m = from.levels(&t);
        if m.count_ones() > 1 {
            return Err(
                "a condition reads one table's columns; the columns a join pairs go in its `on`",
            );
        }
        let l = m.trailing_zeros() as usize % 32;
        if l == 0 || !from.joins[l - 1].left {
            filters[l].push(t);
            continue;
        }
        let key = match &t {
            Expr::IsNull(f) => match &**f {
                Expr::Field(f) => Some(from.owner(f).1),
                _ => None,
            },
            _ => None,
        };
        match key {
            Some(k)
                if n == 2 && !none && keys[1].0.len() == 1 && (k == keys[1].0[0] || k == "id") =>
            {
                none = true
            }
            _ => {
                return Err(
                    "a LEFT JOIN's table in the `where` reads the rows it kept with \
                            nulls: in `on` a condition filters its rows, an inner JOIN drops them",
                )
            }
        }
    }
    let from = &from;
    let items = items(&mut sel, from);
    // What the answer reads beyond each table's filters: the list, the
    // keys, `having` and the order. A name the list gives an item stands
    // for it there.
    let mut read = Vec::new();
    let mut reads = 0u32;
    for i in &items {
        match i.star {
            Some(t) => reads |= 1 << t,
            None => i.expr.referenced_fields(&mut read),
        }
    }
    // A key, `having` or the order naming an item reads what it does.
    let mut keys_read = Vec::new();
    for g in &sel.group {
        g.referenced_fields(&mut keys_read);
    }
    if let Some(h) = &sel.having {
        h.referenced_fields(&mut keys_read);
    }
    for s in &sel.order {
        match &s.expr {
            Some(e) => e.referenced_fields(&mut keys_read),
            None if s.field.contains('(') => {}
            None => keys_read.push(s.field.clone()),
        }
    }
    for f in keys_read {
        if named(from, &items, &f).is_none() {
            read.push(f);
        }
    }
    // Whether the first table is read for more than its key, where one
    // key pairs the two tables: `*`, or a column besides it.
    let one_key = n == 2 && keys[1].0.len() == 1;
    let mut first_more = reads & 1 != 0;
    for f in &read {
        let (t, bare) = from.owner(f);
        let t = t.unwrap_or(0);
        reads |= 1 << t;
        first_more |= t == 0 && !(one_key && bare == keys[1].1[0]);
    }
    // The rest of the names the statement writes, each its table's: the
    // engine holds them to the tables' fields ([`Flat::names`]).
    let mut written = read.clone();
    for f in &filters {
        for t in f {
            t.referenced_fields(&mut written);
        }
    }
    for f in written {
        let (t, name) = from.owner(&f);
        names.push((t.unwrap_or(0), name.to_string(), t.is_none()));
    }
    // Each once: the engine asks each of every run of the statement.
    let mut each = Vec::with_capacity(names.len());
    for n in names {
        if n.1 != "id" && !n.1.contains('(') && !each.contains(&n) {
            each.push(n);
        }
    }
    let names = each;
    let tables = (from.tables.iter())
        .map(|t| {
            (
                t.alias.clone().unwrap_or_else(|| t.collection.clone()),
                t.collection.clone(),
            )
        })
        .collect();
    let names = Some(Arc::new(JoinNames { tables, names }));
    let folds = items_fold(&items) || !sel.group.is_empty() || sel.having.is_some();
    let inner: Vec<bool> = from.joins.iter().map(|j| !j.left).collect();
    if one_key {
        let (child, parent) = (&keys[1].0[0], &keys[1].1[0]);
        let once = distinct && !folds;
        if none && reads & !1 != 0 {
            return Err(
                "the rows a LEFT JOIN finds no match for hold none of its columns: list \
                        the first table's",
            );
        }
        // The first table's rows answered, the joined table's a filter:
        // each answered once where the joined table holds one match a
        // row, by its `id`, or where `distinct` counts a row once -- or,
        // for `where o.id is null`, the rows it holds none for.
        //
        // Where the joined table is found by its `id` and the rows are
        // the first table's as they come, the same join may be read
        // through the `lookup`'s probe ([`Flat::probe`]): the list answers
        // a small joined table through the first table's index, and past
        // its 100 000 values the engine reads the probe, a row's match
        // looked up by its id, where the plain join was refused. Read as
        // the probe alone, a join to three rows of `vip` looked up each of
        // 100 001 users.
        if none || reads & !1 == 0 && (child == "id" || once) {
            let probe = child == "id" && inner[0] && !none && !folds && !distinct;
            if inner[0] || none {
                let some = semi(from, 1, child, parent, &mut filters[1]);
                filters[0].push(match none {
                    true => Expr::Not(Box::new(some)),
                    false => some,
                });
            }
            sel.flat = join_flat(names, probe);
            return Ok(driving(sel, from, items, filters.swap_remove(0), None));
        }
        // The joined table's, the first table's a filter -- its key read
        // as the joined table's, which an inner join's rows hold too. Its
        // probe drives the joined table, the first table a `required`
        // level found by its `id`.
        if inner[0] && reads & !3 == 0 && !first_more && (parent == "id" || once) {
            let probe = parent == "id" && !folds && !distinct;
            let some = semi(from, 0, parent, child, &mut filters[0]);
            filters[1].push(some);
            let own = filters.swap_remove(1);
            sel.flat = join_flat(names, probe);
            return Ok(driving(sel, from, items, own, Some((parent, child))));
        }
    }
    if folds || distinct {
        return Err(FOLDED);
    }
    flat(sel, from, items, filters, keys, &inner, names)
}

/// What a statement read off a join without flat rows carries: the names
/// it writes ([`Flat::names`]) and whether its list may be probed.
fn join_flat(names: Option<Arc<JoinNames>>, probe: bool) -> Option<Box<Flat>> {
    Some(Box::new(Flat {
        names,
        probe,
        ..Flat::default()
    }))
}

/// The item a name with no table names -- by its `as`, or its label
/// written bare, `y` for `b.y` -- as SQL's `order by`, `group by` and
/// `having` take an item's name.
#[inline(never)]
fn named<'a>(from: &From, items: &'a [Item], f: &str) -> Option<&'a Item> {
    if f.contains('.') {
        return None;
    }
    items
        .iter()
        .find(|i| i.star.is_none() && (i.name == f || from.label(&i.name) == f))
}

/// The keys of an order over rows that name an item, as the item reads:
/// its field, or what it works out.
#[inline(never)]
fn order_by_items(sel: &mut Select, from: &From, items: &[Item]) {
    for s in &mut sel.order {
        if s.expr.is_some() {
            continue;
        }
        if let Some(i) = named(from, items, &s.field) {
            match &i.expr {
                Expr::Field(f) => s.field = f.clone(),
                e if i.star.is_none() => {
                    s.field.clear();
                    s.expr = Some(e.duplicate());
                }
                _ => {}
            }
        }
    }
}

/// The list of `sel` as SQL's items, its places in `sel` emptied.
#[inline(never)]
fn items(sel: &mut Select, from: &From) -> Vec<Item> {
    let mut out = Vec::new();
    if !sel.aggregate.is_empty() {
        for c in std::mem::take(&mut sel.aggregate) {
            out.push(Item {
                expr: c.expr,
                name: c.name,
                star: None,
            });
        }
        return out;
    }
    let computed = std::mem::take(&mut sel.computed);
    match sel.project.take() {
        None => out.extend((0..from.tables.len()).map(|t| Item {
            expr: Expr::Lit(Value::Null),
            name: String::new(),
            star: Some(t),
        })),
        Some(names) => {
            for name in names {
                if let Some(c) = computed.iter().find(|c| c.name == name) {
                    out.push(Item {
                        expr: c.expr.duplicate(),
                        name,
                        star: None,
                    });
                    continue;
                }
                let star = match from.owner(&name) {
                    (Some(t), "*") => Some(t),
                    _ => None,
                };
                out.push(Item {
                    expr: Expr::Field(name.clone()),
                    name,
                    star,
                });
            }
        }
    }
    out
}

/// `key in (get <table> select <col> where <its terms>)`: the rows of the
/// other table a filter, its terms taken from `terms`.
fn semi(from: &From, table: usize, col: &str, key: &str, terms: &mut Vec<Expr>) -> Expr {
    // Qualified, so that the driving table's strip places it.
    let t = &from.tables[1 - table];
    let at = t.alias.as_deref().unwrap_or(&t.collection);
    in_get(
        format!("{at}.{key}"),
        from,
        table,
        col.to_string(),
        std::mem::take(terms),
    )
}

/// `left in (get <from's table> select <col> where <terms>)`, the terms'
/// names written bare.
fn in_get(left: String, from: &From, table: usize, col: String, terms: Vec<Expr>) -> Expr {
    let mut filter = joined(terms);
    if let Some(f) = &mut filter {
        from.strip(f, None);
    }
    let inner = Select {
        collection: from.tables[table].collection.clone(),
        project: Some(vec![col]),
        filter,
        ..Select::default()
    };
    Expr::InSelect(Box::new(Expr::Field(left)), Box::new(inner))
}

/// `sel` as the `get` of one table -- the first, or with `key` the second,
/// the first's key read as its own -- its filter `terms`.
fn driving(
    mut sel: Select,
    from: &From,
    items: Vec<Item>,
    terms: Vec<Expr>,
    key: Option<(&str, &str)>,
) -> Select {
    let at = key.map_or(0, |_| 1);
    sel.collection = from.tables[at].collection.clone();
    sel.filter = joined(terms);
    // An item `o.*` alone is every field; beside others it is refused as
    // a field named `*`, by the engine.
    let star = items.len() == 1 && items[0].star.is_some();
    let folds = !sel.group.is_empty() || sel.having.is_some() || items_fold(&items);
    if !folds {
        order_by_items(&mut sel, from, &items);
    }
    for mut i in items.into_iter().filter(|_| !star) {
        let derived = Some(&i.name) == label_of(&i.expr).as_ref();
        if derived {
            i.name = from.label(&i.name);
        }
        from.strip(&mut i.expr, key);
        let field = matches!(&i.expr, Expr::Field(f) if *f == i.name);
        match (folds, field) {
            (true, _) => sel.aggregate.push(fenec_core::query::Column {
                expr: i.expr,
                name: i.name,
            }),
            (false, true) => sel.project.get_or_insert_with(Vec::new).push(i.name),
            (false, false) => {
                sel.project
                    .get_or_insert_with(Vec::new)
                    .push(i.name.clone());
                sel.computed.push(fenec_core::query::Column {
                    expr: i.expr,
                    name: i.name,
                });
            }
        }
    }
    bare(&mut sel, from, key);
    sel
}

/// Whether any item folds rows: an aggregating list.
#[inline(never)]
fn items_fold(items: &[Item]) -> bool {
    items.iter().any(|i| i.expr.has_aggregate())
}

/// Every name of `sel` but its list written bare: its filter, keys,
/// `having` and order, and for an alias alone its list too.
fn bare(sel: &mut Select, from: &From, key: Option<(&str, &str)>) {
    if let Some(f) = &mut sel.filter {
        from.strip(f, key);
    }
    for g in &mut sel.group {
        from.strip(g, key);
    }
    if let Some(h) = &mut sel.having {
        from.strip(h, key);
    }
    let grouped = !sel.aggregate.is_empty() || !sel.group.is_empty();
    for s in &mut sel.order {
        match &mut s.expr {
            Some(e) => from.strip(e, key),
            // Over groups a key names a column, by its label.
            None if grouped => s.field = from.label(&s.field),
            None => {
                let mut f = Expr::Field(std::mem::take(&mut s.field));
                from.strip(&mut f, key);
                if let Expr::Field(f) = f {
                    s.field = f;
                }
            }
        }
    }
    // An alias alone: the list's names too.
    if from.tables.len() == 1 {
        for c in sel.aggregate.iter_mut().chain(&mut sel.computed) {
            if Some(&c.name) == label_of(&c.expr).as_ref() {
                c.name = from.label(&c.name);
            }
            from.strip(&mut c.expr, None);
        }
        let computed: Vec<String> = sel.computed.iter().map(|c| c.name.clone()).collect();
        for p in sel.project.iter_mut().flatten() {
            if !computed.contains(p) {
                *p = from.owner(p).1.to_string();
            }
        }
        if let Some(p) = &sel.project {
            if let [one] = &p[..] {
                if one == "*" {
                    sel.project = None;
                }
            }
        }
        for m in &mut sel.marks {
            m.field = from.owner(&m.field).1.to_string();
        }
        for f in &mut sel.facets {
            f.field = from.owner(&f.field).1.to_string();
        }
    }
}

/// The join's rows as SQL answers them: a `lookup` from each table to the
/// next, flat ([`Flat`]). The order is the first table's, whose rows the
/// chain hangs off -- a joined table's would order each row's children
/// alone, which no SQL order is -- and the page is the first table's where
/// every join finds a row by its `id`, at most one a row, and else cut
/// from the flat rows, the first table read as far as the page could need.
#[inline(never)]
fn flat(
    mut sel: Select,
    from: &From,
    items: Vec<Item>,
    mut filters: Vec<Vec<Expr>>,
    keys: Vec<(Vec<String>, Vec<String>)>,
    inner: &[bool],
    names: Option<Arc<JoinNames>>,
) -> Result<Select, &'static str> {
    let n = from.tables.len();
    order_by_items(&mut sel, from, &items);
    for s in &sel.order {
        let at = match &s.expr {
            Some(e) => from.levels(e),
            None => 1 << from.owner(&s.field).0.unwrap_or(0),
        };
        if at & !1 != 0 {
            return Err(
                "a join's rows are put in order by the first table's columns: name \
                        the table it orders by first",
            );
        }
    }
    let mut projects: Vec<Option<Vec<String>>> = (0..n).map(|_| Some(Vec::new())).collect();
    let mut columns = Vec::new();
    for mut i in items {
        if let Some(t) = i.star {
            projects[t] = None;
            columns.push(FlatColumn {
                level: t,
                column: String::new(),
                name: String::new(),
            });
            continue;
        }
        if Some(&i.name) == label_of(&i.expr).as_ref() {
            i.name = from.label(&i.name);
        }
        let column = match &i.expr {
            Expr::Field(f) => {
                let (t, bare) = from.owner(f);
                let t = t.unwrap_or(0);
                if let Some(p) = &mut projects[t] {
                    if !p.iter().any(|c| c == bare) {
                        p.push(bare.to_string());
                    }
                }
                FlatColumn {
                    level: t,
                    column: bare.to_string(),
                    name: i.name,
                }
            }
            e => {
                if from.levels(e) & !1 != 0 {
                    return Err(
                        "a column worked out of a row reads the first table's columns; \
                                a joined table's are listed as they are",
                    );
                }
                let mut e = i.expr.duplicate();
                from.strip(&mut e, None);
                if projects[0].is_none() {
                    return Err("`o.*` beside a column worked out of the row: name the fields");
                }
                if let Some(p) = &mut projects[0] {
                    p.push(i.name.clone());
                }
                sel.computed.push(fenec_core::query::Column {
                    expr: e,
                    name: i.name.clone(),
                });
                FlatColumn {
                    level: 0,
                    column: i.name.clone(),
                    name: i.name,
                }
            }
        };
        columns.push(column);
    }
    // The chain, the last level first.
    let mut next: Option<Box<Lookup>> = None;
    for j in (1..n).rev() {
        let mut filter = joined(std::mem::take(&mut filters[j]));
        if let Some(f) = &mut filter {
            from.strip(f, None);
        }
        let (on, parent) = keys[j].clone();
        next = Some(Box::new(Lookup {
            collection: from.tables[j].collection.clone(),
            on,
            parent,
            project: projects[j].take(),
            filter,
            required: inner[j - 1],
            next,
            ..Lookup::default()
        }));
    }
    sel.lookup = next.map(|l| *l);
    sel.project = projects[0].take();
    sel.filter = joined(std::mem::take(&mut filters[0]));
    bare(&mut sel, from, None);
    // A row of each table at most, by `id`: the first table's page is the
    // flat rows'.
    let by_id = keys[1..].iter().all(|(c, _)| c.len() == 1 && c[0] == "id");
    let mut page = Flat {
        columns,
        names,
        ..Flat::default()
    };
    if !by_id {
        page.offset = std::mem::take(&mut sel.offset);
        page.limit = sel.limit.take();
        sel.limit = match (page.limit, page.offset) {
            (Some(Count::N(l)), Count::N(o)) => Some(Count::N(l.saturating_add(o))),
            _ => None,
        };
    }
    sel.flat = Some(Box::new(page));
    Ok(sel)
}

/// SQL's correlated `exists (select 1 from <inner> where <filter>)`: the
/// one equality of `filter` between a column of `inner` and one of a table
/// `around` names -- `o.customer_id = c.id` -- as `c.id in (get orders
/// select customer_id where <the rest>)`. A name with no table, or the
/// query's own, is the query's, as SQL reads it first. The column around
/// is written bare where the statement reads one collection, and as
/// written in a join's, which places it.
#[inline(never)]
pub(crate) fn correlate(
    inner: &From,
    around: &From,
    filter: Option<Expr>,
) -> Result<Expr, &'static str> {
    const ONE: &str = "an `exists` reads the row around it by one equality, `where \
                       o.customer_id = c.id`";
    let outside = |f: &str| inner.owner(f).0.is_none() && around.owner(f).0.is_some();
    let mut own = Vec::new();
    let mut key: Option<(String, String)> = None;
    for t in terms(filter) {
        let mut names = Vec::new();
        t.referenced_fields(&mut names);
        if !names.iter().any(|f| outside(f)) {
            own.push(t);
            continue;
        }
        let pair = match &t {
            Expr::Cmp(CmpOp::Eq, a, b) => match (&**a, &**b) {
                (Expr::Field(a), Expr::Field(b)) if outside(b) && !outside(a) => Some((a, b)),
                (Expr::Field(a), Expr::Field(b)) if outside(a) && !outside(b) => Some((b, a)),
                _ => None,
            },
            _ => None,
        };
        match (pair, &key) {
            (Some((here, there)), None) => {
                key = Some((inner.owner(here).1.to_string(), there.clone()));
            }
            _ => return Err(ONE),
        }
    }
    let Some((here, there)) = key else {
        return Err(ONE);
    };
    let there = match around.tables.len() {
        1 => around.owner(&there).1.to_string(),
        _ => there,
    };
    Ok(in_get(there, inner, 0, here, own))
}

/// Whether a sort key is SQL's distance operator, which `near` reads.
#[inline(never)]
pub(crate) fn sort_is_distance(s: &Sort) -> bool {
    matches!(&s.expr, Some(Expr::Call(name, _)) if name.starts_with('<'))
}
