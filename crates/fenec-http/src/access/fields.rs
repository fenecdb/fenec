//! Field grants: `read(a, b)` in a rule, the fields of the rows it admits
//! that a read shows.
//!
//! ```text
//! orders  read(id, pickup, pickup_name, run_at, status)  where run_at <= now()     for courier
//! orders  read                                           where courier = $jwt.sub  for courier
//! ```
//!
//! A row shows the union of the fields of the read rules that admit it, and
//! `id` always: a courier sees a ready order's pickup to claim it, and the
//! customer's door once the order is theirs. The rest of a row answers
//! `null`. A field no read rule grants is not there for the token: `select
//! *` leaves it out, `\d`, `/_schema` and the MCP `schema` tool do not list
//! it, and naming it anywhere is refused (403) whether or not the
//! collection has it, so a name tried says nothing of the schema.
//!
//! **Masking the rows is not enough.** A field withheld from a row must not
//! show through what a statement does with it: which rows a `where` keeps,
//! how an `order` puts them, a `group`'s keys, a `facet`'s counts, an
//! aggregate, `match`'s and `near`'s ranking, which children a `lookup`'s
//! key attaches, a `set`'s values -- written into a field the token reads,
//! or failing with an error that says what they met. So:
//!
//! - A `where` term reading a field `f` some rule withholds becomes
//!   `(showing_f and term)`, `showing_f` the `or` of the filters of the read
//!   rules granting `f`: a row that does not show `f` passes none of its
//!   terms, whatever it holds, and `not` over one says nothing either. `and`
//!   is asked left to right and stops at the first false, so the term is
//!   never worked out over such a row -- `10 / n > 1` divides by no `n` it
//!   may not see. In a `set`'s and a `del`'s `where`, in a `lookup` level's,
//!   a disjunctive facet's and an inner `get`'s.
//! - `order`, `group`, `having`, a `facet`, an aggregate, `match`, `near`,
//!   `rerank`, a highlight, a `lookup` key (both sides), a `set`'s values
//!   and an upsert's `else set` may read `f` only where every row they
//!   reach shows it -- [`View::open`] -- and are refused (403, naming the
//!   field) otherwise. The server knows that in two ways: every read rule
//!   of the collection grants `f`, so every row a read reaches shows it; or
//!   one rule granting `f` admits every row the statement reaches, having
//!   no filter or one whose `and` terms are each an `and` term of the
//!   statement's own `where`, its parameters bound -- the courier's `get
//!   orders ... where courier = $1 order delivered_at desc`, `$1` their
//!   `sub`, reaches the orders the second rule above admits alone. Nothing
//!   finer is proved, and nothing guessed.
//! - A computed column reading `f` is `case when showing_f then <it> end`,
//!   null where the row does not show it and never worked out there.
//! - An inner `get`'s column of `f` is made of the rows showing `f`: its
//!   `where` takes `showing_f`, so the list holds no value it may not see.
//!
//! A write's rows are found as they were (an update grant says what it may
//! change, the read rules what it may see): the terms of its `where` are
//! held as a read's, so a `del ... where dropoff = $1` tells nothing by the
//! rows it counts, and its `returning` is masked as a read's rows are. An
//! update rule wider than the read rules reaches rows no read rule shows,
//! and a field every read rule grants is held there as every field was
//! before field grants: what field lists take away, they take away there
//! too, and nothing more.
//!
//! **Against PostgreSQL.** Column privileges (`GRANT SELECT (a, b)`) are
//! the role's and the same on every row, and a statement naming a column
//! the role lacks is refused whole -- `SELECT *` included. Here what a row
//! shows is decided by the rules admitting it, which PostgreSQL says with a
//! view of `CASE WHEN <policy> THEN col END` columns over a security
//! barrier; its `WHERE` over those columns then reads `NULL` where this
//! reads the term as false -- a `dropoff is null` there finds every order
//! not claimed. A `select` of a field shown on some rows answers `null` on
//! the rest rather than refusing, since that is what the rule says the row
//! is to this token.
//!
//! **Cost.** A token none of whose rules name fields takes the path it took
//! before: [`Scope::fielded`] is asked once a statement, and `None` is its
//! mask. One whose do pays, per answered row, the filters of the rules that
//! could show it more, worked out over the row's own columns -- the fields
//! they read appended to the projection where it leaves them out, and taken
//! off after -- and its plain `get`s are not written straight from the
//! stored bytes where a column is masked (`query_json`).

use super::{denied, order_of, Bound, Scope, READ};
use fenec_core::prelude::*;
use fenec_core::query::{eval, truthy, EvalCtx, Pick, RowAccess};
use std::sync::OnceLock;

/// What an answer must not show a token whose rules name fields: a level
/// of rows each -- the statement's own or a write's `returning`, then each
/// `lookup` level's -- by the collection its rows are of.
#[derive(Debug)]
pub struct Mask {
    levels: Vec<Level>,
}

#[derive(Debug)]
struct Level {
    collection: String,
    /// Columns appended for the rules alone -- the fields their filters
    /// read, where the list leaves them out -- taken off after.
    added: usize,
    /// Columns that are no field's: a computed column, which the engine
    /// masked itself (`case`), and a highlight, of a field shown on every
    /// row.
    own: Vec<String>,
    /// Nothing to do: rows of groups, a count, or a collection none of
    /// whose rules name fields.
    idle: bool,
}

impl Level {
    fn idle(collection: &str) -> Level {
        Level {
            collection: collection.to_string(),
            added: 0,
            own: Vec::new(),
            idle: true,
        }
    }
}

/// A field's name, or the field a path reads into.
fn base(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

/// The fields `e` reads, outside an inner `get` -- whose are its own
/// collection's -- leaving out `id`, shown on every row.
fn fields_of(e: &Expr) -> Vec<String> {
    read_by(e, false)
}

/// [`fields_of`], and where `upsert`, an upsert's `else set` value: its
/// `new.` names the document written, the token's own, and is no field of
/// the row.
fn read_by(e: &Expr, upsert: bool) -> Vec<String> {
    fn walk(e: &Expr, upsert: bool, out: &mut Vec<String>) {
        if let Expr::Field(name) = e {
            let b = base(name);
            let new = upsert && name.starts_with(super::NEW);
            if b != "id" && !new && !out.iter().any(|f| f == b) {
                out.push(b.to_string());
            }
            return;
        }
        e.each_child(&mut |c| walk(c, upsert, out));
    }
    let mut out = Vec::new();
    walk(e, upsert, &mut out);
    out
}

/// The terms of `e`'s top-level `and` chain.
fn and_terms<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match e {
        Expr::And(a, b) => {
            and_terms(a, out);
            and_terms(b, out);
        }
        e => out.push(e),
    }
}

fn all_of(mut terms: Vec<Expr>) -> Option<Expr> {
    let last = terms.pop()?;
    Some(
        terms
            .into_iter()
            .rev()
            .fold(last, |t, s| Expr::And(Box::new(s), Box::new(t))),
    )
}

/// The rows a statement level reaches, as far as the server proves it: the
/// `and` terms of its own `where`, its parameters bound, against which a
/// rule's terms are matched one by one.
struct Reach {
    terms: Vec<Expr>,
}

impl Reach {
    fn of(filter: Option<&Expr>, params: &[Value]) -> Reach {
        let mut refs = Vec::new();
        if let Some(f) = filter {
            and_terms(f, &mut refs);
        }
        let terms = refs
            .into_iter()
            .map(|t| {
                let mut t = t.clone();
                let _ = t.rewrite(&mut |x| {
                    if let Expr::Param(i) = x {
                        if let Some(v) = params.get(*i) {
                            *x = Expr::Lit(v.clone());
                        }
                        return Ok(true);
                    }
                    Ok(false)
                });
                t
            })
            .collect();
        Reach { terms }
    }

    /// Whether every row reached passes `filter`: each of its `and` terms
    /// is one of the `where`'s.
    fn implies(&self, filter: &Expr) -> bool {
        let mut terms = Vec::new();
        and_terms(filter, &mut terms);
        terms.iter().all(|t| self.terms.iter().any(|w| w == *t))
    }
}

/// A token's read rules over one collection, one of them naming the fields
/// it shows.
pub(super) struct View<'a> {
    collection: String,
    rules: Vec<&'a Bound>,
}

impl<'a> View<'a> {
    /// `None` where no read rule of the token over `collection` names
    /// fields: every field shows, as before field grants.
    pub(super) fn of(scope: &'a Scope, collection: &str) -> Option<View<'a>> {
        if !scope.fielded {
            return None;
        }
        let rules: Vec<&Bound> = scope
            .rules
            .iter()
            .filter(|r| {
                r.ops & READ != 0 && r.collection.as_deref().is_none_or(|c| c == collection)
            })
            .collect();
        if rules.iter().all(|r| r.read_fields.is_none()) {
            return None;
        }
        Some(View {
            collection: collection.to_string(),
            rules,
        })
    }

    fn grants(r: &Bound, f: &str) -> bool {
        f == "id"
            || r.read_fields
                .as_ref()
                .is_none_or(|list| list.iter().any(|x| x == f))
    }

    /// Whether some row may show `f`.
    fn granted(&self, f: &str) -> bool {
        self.rules.iter().any(|r| Self::grants(r, f))
    }

    /// Whether every row the token reads shows `f`.
    fn everywhere(&self, f: &str) -> bool {
        self.rules.iter().all(|r| Self::grants(r, f))
    }

    /// Whether every row `reach` holds shows `f`: every read rule grants
    /// it, or one granting it admits every such row.
    fn open(&self, f: &str, reach: &Reach) -> bool {
        self.everywhere(f)
            || self.rules.iter().any(|r| {
                Self::grants(r, f) && r.filter.as_ref().is_none_or(|flt| reach.implies(flt))
            })
    }

    /// The rows that show `f`: the `or` of the filters of the rules
    /// granting it, `None` for every row.
    fn showing(&self, f: &str) -> Option<Expr> {
        let mut either: Option<Expr> = None;
        for r in self.rules.iter().filter(|r| Self::grants(r, f)) {
            let flt = r.filter.as_ref()?;
            either = Some(match either {
                None => flt.clone(),
                Some(o) => Expr::Or(Box::new(o), Box::new(flt.clone())),
            });
        }
        either
    }

    fn unread(&self, f: &str) -> Error {
        denied(format!(
            "this token may not read `{f}` of `{}`",
            self.collection
        ))
    }

    /// A field named where it is no field's own value: granted to some row.
    fn named(&self, f: &str) -> Result<()> {
        match self.granted(base(f)) {
            true => Ok(()),
            false => Err(self.unread(base(f))),
        }
    }

    /// `f` read by `what`, which says something of every row it reaches:
    /// shown on each of them, or refused.
    fn reached(&self, f: &str, reach: &Reach, what: &str) -> Result<()> {
        let f = base(f);
        if !self.granted(f) {
            return Err(self.unread(f));
        }
        if self.open(f, reach) {
            return Ok(());
        }
        Err(denied(format!(
            "this token sees `{f}` of `{}` on some rows alone, and {what} would tell it of \
             the rest: a `where` holding the filter of a rule granting `{f}` reaches rows \
             that all show it",
            self.collection
        )))
    }

    /// The filters that show each of `fields` where a row may not.
    fn needed(&self, fields: &[String], reach: &Reach) -> Result<Vec<Expr>> {
        let mut need: Vec<Expr> = Vec::new();
        for f in fields {
            if !self.granted(f) {
                return Err(self.unread(f));
            }
            if self.open(f, reach) {
                continue;
            }
            if let Some(s) = self.showing(f) {
                if !need.contains(&s) {
                    need.push(s);
                }
            }
        }
        Ok(need)
    }

    /// `e`, a `where`, its terms over a field some row does not show held
    /// to the rows that do: `(showing_f and term)`.
    fn guard(&self, e: Expr, reach: &Reach) -> Result<Expr> {
        Ok(match e {
            Expr::And(a, b) => Expr::And(
                Box::new(self.guard(*a, reach)?),
                Box::new(self.guard(*b, reach)?),
            ),
            Expr::Or(a, b) => Expr::Or(
                Box::new(self.guard(*a, reach)?),
                Box::new(self.guard(*b, reach)?),
            ),
            Expr::Not(a) => Expr::Not(Box::new(self.guard(*a, reach)?)),
            term => {
                let need = self.needed(&fields_of(&term), reach)?;
                need.into_iter()
                    .rev()
                    .fold(term, |t, s| Expr::And(Box::new(s), Box::new(t)))
            }
        })
    }

    /// The level a list `project` answers -- `None` for every field -- its
    /// columns `own` no field's: the names checked, and where a column may
    /// be masked, the fields the rules read appended.
    fn answer(&self, project: &mut Option<Vec<String>>, own: Vec<String>) -> Result<Level> {
        let mut masked = project.is_none();
        if let Some(list) = project.as_deref() {
            for n in list {
                if n == "id" || own.iter().any(|o| o == n) {
                    continue;
                }
                self.named(n)?;
                masked = masked || !self.everywhere(base(n));
            }
        }
        if !masked {
            return Ok(Level::idle(&self.collection));
        }
        let mut added = 0;
        if let Some(list) = project.as_mut() {
            for r in &self.rules {
                let Some(f) = &r.filter else { continue };
                for name in fields_of(f) {
                    if !list.contains(&name) {
                        list.push(name);
                        added += 1;
                    }
                }
            }
        }
        Ok(Level {
            collection: self.collection.clone(),
            added,
            own,
            idle: false,
        })
    }

    /// An `order` key: a field, the list's column (`labels`), or an
    /// expression.
    fn ordered(&self, s: &Sort, reach: &Reach, labels: &[(String, Vec<String>)]) -> Result<()> {
        const WHAT: &str = "an `order` by it";
        if let Some(e) = &s.expr {
            for f in fields_of(e) {
                if !labels.iter().any(|(l, _)| *l == f) {
                    self.reached(&f, reach, WHAT)?;
                }
            }
            return Ok(());
        }
        match labels.iter().find(|(l, _)| *l == s.field) {
            Some((_, reads)) => reads.iter().try_for_each(|f| self.reached(f, reach, WHAT)),
            None => self.reached(&s.field, reach, WHAT),
        }
    }

    /// What a level's list, `group`, `having` and `order` read: an
    /// aggregate's fields shown on every row it folds, and a key's.
    fn folded(
        &self,
        aggregate: &[Column],
        group: &[Expr],
        having: Option<&Expr>,
        order: &[Sort],
        reach: &Reach,
        mut labels: Vec<(String, Vec<String>)>,
    ) -> Result<()> {
        for c in aggregate {
            let reads = fields_of(&c.expr);
            for f in &reads {
                self.reached(f, reach, "an aggregate over it")?;
            }
            labels.push((c.name.clone(), reads));
        }
        let label = |f: &str| labels.iter().any(|(l, _)| l == f);
        for g in group {
            for f in fields_of(g).iter().filter(|f| !label(f)) {
                self.reached(f, reach, "a `group` by it")?;
            }
        }
        for f in having.map(fields_of).unwrap_or_default() {
            if !label(&f) {
                self.reached(&f, reach, "a `having` over it")?;
            }
        }
        for s in order {
            self.ordered(s, reach, &labels)?;
        }
        Ok(())
    }
}

/// A read of `sel` as a token whose rules name fields may run it: what it
/// does with a field held to the rows showing it, refused where it cannot
/// be, and what its rows must not show (`None`: nothing). `inner`: an inner
/// `get`, whose column becomes a list -- made of the rows showing it -- and
/// is answered nowhere.
pub(super) fn hold(
    scope: &Scope,
    sel: &mut Select,
    params: &[Value],
    inner: bool,
) -> Result<Option<Mask>> {
    let mut levels = Vec::new();
    let top = View::of(scope, &sel.collection);
    let reach = Reach::of(sel.filter.as_ref(), params);
    let folds = !sel.aggregate.is_empty();
    let mut own: Vec<String> = sel.marks.iter().map(|m| m.label()).collect();
    if let Some(v) = &top {
        // The list's items by name, each with what it reads, for an
        // `order` naming one.
        let labels: Vec<(String, Vec<String>)> = sel
            .computed
            .iter()
            .map(|c| (c.name.clone(), fields_of(&c.expr)))
            .collect();
        if let Some(list) = &sel.project {
            for n in list {
                let label = labels.iter().any(|(l, _)| l == n) || own.iter().any(|o| o == n);
                if n != "id" && !label && !sel.aggregate.iter().any(|c| c.name == *n) {
                    v.named(n)?;
                }
            }
        }
        v.folded(
            &sel.aggregate,
            &sel.group,
            sel.having.as_ref(),
            &sel.order,
            &reach,
            labels,
        )?;
        for f in &sel.facets {
            v.reached(&f.field, &reach, "a `facet` of it")?;
        }
        if let Some(m) = &sel.matcher {
            v.reached(&m.field, &reach, "`match` over it")?;
        }
        if let Some(n) = &sel.near {
            v.reached(&n.field, &reach, "`near` over it")?;
        }
        if let Some(r) = &sel.rerank {
            v.reached(&r.field, &reach, "`rerank` by it")?;
        }
        for m in &sel.marks {
            v.reached(&m.field, &reach, "a highlight of it")?;
        }
        // A computed column over a field some row does not show is null
        // there, and never worked out: `case` is, lazily.
        for c in &mut sel.computed {
            if let Some(show) = all_of(v.needed(&fields_of(&c.expr), &reach)?) {
                let e = std::mem::replace(&mut c.expr, Expr::Lit(Value::Null));
                c.expr = Expr::Call("case".into(), vec![show, e]);
            }
            own.push(c.name.clone());
        }
        if let Some(f) = sel.filter.take() {
            sel.filter = Some(v.guard(f, &reach)?);
        }
        for rest in sel.facets.iter_mut().filter_map(|f| f.rest.as_mut()) {
            if let Some(f) = rest.take() {
                let r = Reach::of(Some(&f), params);
                *rest = Some(v.guard(f, &r)?);
            }
        }
        if inner {
            if let Some([f]) = sel.project.as_deref() {
                let show = v.needed(&[base(f).to_string()], &reach)?;
                if let Some(show) = all_of(show) {
                    sel.filter = Some(match sel.filter.take() {
                        Some(f) => Expr::And(Box::new(f), Box::new(show)),
                        None => show,
                    });
                }
            }
        }
    }
    if !inner {
        levels.push(match &top {
            Some(v) if !folds && !sel.count => v.answer(&mut sel.project, own)?,
            _ => Level::idle(&sel.collection),
        });
    }
    let (mut above, mut above_reach, mut above_folds) = (top, reach, folds);
    let mut level = sel.lookup.as_mut();
    while let Some(l) = level {
        let v = View::of(scope, &l.collection);
        let reach = Reach::of(l.filter.as_ref(), params);
        // Which children a key attaches tells the parent's value and the
        // child's: shown on every row of either side, or refused. Over
        // groups the parent's names are its list's columns, checked there.
        if let (Some(a), false) = (&above, above_folds) {
            for p in &l.parent {
                a.reached(p, &above_reach, "a `lookup` keyed by it")?;
            }
        }
        let folds = !l.aggregate.is_empty();
        if let Some(v) = &v {
            for k in &l.on {
                v.reached(k, &reach, "a `lookup` keyed by it")?;
            }
            if let Some(list) = &l.project {
                for n in list.iter().filter(|n| *n != "id") {
                    if !l.aggregate.iter().any(|c| c.name == *n) {
                        v.named(n)?;
                    }
                }
            }
            v.folded(
                &l.aggregate,
                &l.group,
                l.having.as_ref(),
                &l.order,
                &reach,
                Vec::new(),
            )?;
            if let Some(f) = l.filter.take() {
                l.filter = Some(v.guard(f, &reach)?);
            }
        }
        if !inner {
            levels.push(match &v {
                Some(v) if !folds => v.answer(&mut l.project, Vec::new())?,
                _ => Level::idle(&l.collection),
            });
        }
        (above, above_reach, above_folds) = (v, reach, folds);
        level = l.next.as_deref_mut();
    }
    Ok(levels.iter().any(|l| !l.idle).then_some(Mask { levels }))
}

/// A write as a token whose rules name fields may make it: its `where`
/// held as a read's, a `set`'s values and a pick's `order` reading only
/// fields every row it reaches shows -- written into a field the token
/// reads, or failing on a value, they would tell it the rest -- and what
/// its `returning` must not show. An upsert's `else set` (`upsert`, no
/// `where`) reaches any row holding the value, so reads what every row
/// shows.
pub(super) fn write(
    scope: &Scope,
    collection: &str,
    filter: &mut Option<Expr>,
    (set, upsert): (Option<&[(String, Expr)]>, bool),
    pick: &mut Option<Box<Pick>>,
    params: &[Value],
) -> Result<Option<Mask>> {
    let Some(v) = View::of(scope, collection) else {
        return Ok(None);
    };
    // An upsert's reach is empty: it finds its rows by the value they
    // hold, whatever else they hold.
    let reach = Reach::of(filter.as_ref(), params);
    for (_, e) in set.unwrap_or_default() {
        for f in read_by(e, upsert) {
            v.reached(&f, &reach, "a `set` reading it")?;
        }
    }
    if let Some(p) = pick.as_deref() {
        for s in &p.order {
            v.ordered(s, &reach, &[])?;
        }
    }
    if let Some(f) = filter.take() {
        *filter = Some(v.guard(f, &reach)?);
    }
    let Some(Some(returning)) = pick.as_deref_mut().map(|p| &mut p.returning) else {
        return Ok(None);
    };
    let level = v.answer(returning, Vec::new())?;
    Ok((!level.idle).then(|| Mask {
        levels: vec![level],
    }))
}

/// The builtins a rule's filter may call, as the write hook has them.
fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::with_builtins)
}

/// A row of an answer, read as a rule's filter reads a stored one: a field
/// by its column, a path into the json one, in the field's collation.
struct Answered<'a> {
    columns: &'a [String],
    values: &'a [Value],
    id: DocId,
    schema: Option<&'a Schema>,
}

impl RowAccess for Answered<'_> {
    fn id(&self) -> DocId {
        self.id
    }
    fn field(&mut self, name: &str) -> Result<Value> {
        let at = |c: &str| self.columns.iter().position(|x| x == c);
        Ok(match name.split_once('.') {
            None => at(name).map_or(Value::Null, |j| self.values[j].clone()),
            Some((field, keys)) => match at(field) {
                Some(j) => self.values[j].at_path(keys).clone(),
                None => at(name).map_or(Value::Null, |j| self.values[j].clone()),
            },
        })
    }
    fn text_order(&self, name: &str) -> fenec_core::collate::TextOrder {
        self.schema
            .map_or(fenec_core::collate::TextOrder::BYTES, |s| order_of(s, name))
    }
}

/// What a column of a masked level is.
#[derive(Clone, Copy)]
enum Col {
    Shown,
    Gone,
    /// The field at this place of the level's fields a rule may withhold.
    If(usize),
}

impl Scope {
    /// `resp` as this token is shown it: [`Self::shown_rows`] of its rows.
    pub fn shown(&self, mask: &Mask, db: &Database, resp: &mut Response) {
        if let Response::Rows(rs) = resp {
            self.shown_rows(mask, db, rs);
        }
    }

    /// Each row of `rs` with the fields of the read rules admitting it, the
    /// rest `null`; the fields no rule grants and those read for the rules
    /// alone taken off. `db` holds the schemas, for the collation a rule's
    /// comparison takes.
    pub fn shown_rows(&self, mask: &Mask, db: &Database, rs: &mut ResultSet) {
        let mut levels = mask.levels.iter();
        if let Some(l) = levels.next() {
            self.mask_level(l, db, &mut rs.columns, rs.rows.iter_mut().collect());
        }
        let mut nested = rs.nested.as_mut();
        for l in levels {
            let Some(n) = nested else { break };
            let rows = n.groups.iter_mut().flatten().collect();
            self.mask_level(l, db, &mut n.columns, rows);
            nested = n.nested.as_deref_mut();
        }
    }

    fn mask_level(
        &self,
        l: &Level,
        db: &Database,
        columns: &mut Vec<String>,
        mut rows: Vec<&mut Row>,
    ) {
        if l.idle {
            return;
        }
        let Some(v) = View::of(self, &l.collection) else {
            return;
        };
        let schema = db.collection(&l.collection).ok().map(|c| &c.schema);
        let kept = columns.len().saturating_sub(l.added);
        // The fields a row may not show, a place each.
        let mut maybe: Vec<String> = Vec::new();
        let cols: Vec<Col> = columns
            .iter()
            .enumerate()
            .map(|(j, c)| {
                if j >= kept {
                    return Col::Gone;
                }
                if c == "id" || l.own.iter().any(|o| o == c) {
                    return Col::Shown;
                }
                let b = base(c);
                if !v.granted(b) {
                    Col::Gone
                } else if v.everywhere(b) {
                    Col::Shown
                } else {
                    let k = match maybe.iter().position(|m| m == b) {
                        Some(k) => k,
                        None => {
                            maybe.push(b.to_string());
                            maybe.len() - 1
                        }
                    };
                    Col::If(k)
                }
            })
            .collect();
        if !maybe.is_empty() {
            let ctx = EvalCtx {
                params: &[],
                registry: registry(),
                clock: None,
            };
            let mut shows = vec![false; maybe.len()];
            for row in rows.iter_mut() {
                shows.fill(false);
                let mut every = false;
                let mut at = Answered {
                    columns,
                    values: &row.values,
                    id: row.id,
                    schema,
                };
                for r in &v.rules {
                    // A rule that would show nothing more is not asked.
                    let more = match &r.read_fields {
                        None => true,
                        Some(list) => maybe
                            .iter()
                            .zip(&shows)
                            .any(|(f, s)| !s && list.iter().any(|x| x == f)),
                    };
                    if !more {
                        continue;
                    }
                    // A filter that fails on the row admits nothing.
                    let admits = r
                        .filter
                        .as_ref()
                        .is_none_or(|f| eval(f, &mut at, &ctx).is_ok_and(|v| truthy(&v)));
                    if !admits {
                        continue;
                    }
                    match &r.read_fields {
                        None => {
                            every = true;
                            break;
                        }
                        Some(list) => {
                            for (k, f) in maybe.iter().enumerate() {
                                shows[k] = shows[k] || list.iter().any(|x| x == f);
                            }
                        }
                    }
                }
                if every {
                    continue;
                }
                for (j, c) in cols.iter().enumerate() {
                    if let Col::If(k) = c {
                        if !shows[*k] {
                            row.values[j] = Value::Null;
                        }
                    }
                }
            }
        }
        if cols.iter().any(|c| matches!(c, Col::Gone)) {
            let keep = |j: usize| !matches!(cols[j], Col::Gone);
            let mut j = 0;
            columns.retain(|_| {
                j += 1;
                keep(j - 1)
            });
            for row in rows.iter_mut() {
                let mut j = 0;
                row.values.retain(|_| {
                    j += 1;
                    keep(j - 1)
                });
            }
        }
    }

    /// `schema` as this token is shown it -- by `\d`, `/collections`,
    /// `/_schema`, the MCP `schema` tool: the fields no read rule grants
    /// it taken out, and the indexes over them. `None` where it is shown
    /// whole.
    pub fn shown_schema(&self, schema: &Schema) -> Option<Schema> {
        let v = View::of(self, &schema.name)?;
        let mut out = schema.clone();
        out.fields.retain(|f| v.granted(&f.name));
        out.paths.retain(|p| v.granted(base(&p.name)));
        out.composites
            .retain(|c| c.keys.iter().all(|(k, _)| v.granted(base(k))));
        // Where a dropped field's values lie is the file's, not the
        // token's: a place among fields it is not shown.
        out.dropped.clear();
        Some(out)
    }

    /// A subscription's shape -- its `where` and its list -- as this token
    /// may hold it, before its read rules are ANDed in, and what the rows
    /// it is sent must not show.
    pub fn shaped(
        &self,
        collection: &str,
        filter: Option<Expr>,
        project: &mut Option<Vec<String>>,
    ) -> Result<(Option<Expr>, Option<Mask>)> {
        if !self.fielded {
            return Ok((filter, None));
        }
        let mut sel = Select {
            collection: collection.to_string(),
            filter,
            project: project.take(),
            ..Select::default()
        };
        let mask = hold(self, &mut sel, &[], false)?;
        *project = sel.project;
        Ok((sel.filter, mask))
    }
}
