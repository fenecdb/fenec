//! Named statements: a FenecQL statement, or a block of them, defined once
//! in a database and called by name with typed arguments
//! (`docs/design/named-statements.md`).
//!
//! What this holds today is the tree and what the tree alone decides: a
//! definition's signature and body as the parser reads them, the refusals
//! a body meets at its definition that need nothing but its text, and the
//! canonical tree's digest. Nothing runs one yet: the engine and the
//! server refuse every `create statement`, `drop statement` and `call` as
//! not built ([`NOT_BUILT`]), so no statement is ever defined with nothing
//! to check it, nor called with nothing to scope it. `statements` answers
//! none, and `describe statement` finds none.

use crate::codec;
use crate::error::{Error, Result};
use crate::query::{
    ArithOp, CmpOp, Column, Count, Expr, Facet, Flat, Fuse, Lookup, Mark, Match, Near, Pick,
    Require, Rerank, Select, Sort, Statement,
};
use crate::value::{DataType, Value};

/// What the engine and the server answer a `create statement`, a `drop
/// statement` and a `call` with, every form of each: the words of their
/// own a client tells this refusal by, as the sync's refusals are told by
/// theirs. A query error (400 over HTTP), never a panic, and refused before
/// anything of the statement runs: no block is opened for it.
pub const NOT_BUILT: &str = "named statements are not built yet: `create statement`, `drop \
                             statement` and `call` parse, and nothing runs them";

/// The most statements a body holds.
pub const MAX_BODY: usize = 64;
/// The most parameters a signature declares.
pub const MAX_PARAMS: usize = 32;
/// The most claims a signature declares.
pub const MAX_CLAIMS: usize = 16;
/// The longest name, in bytes: `call_<name>`, an MCP tool's, then fits the
/// 64 characters a model's API takes for a tool's name.
pub const MAX_NAME: usize = 59;

/// `create [or replace] statement <name>(<parameters>) [claims (...)]
/// [public] [scans] [comment '<text>'] [check <expr>] as <body>`.
#[derive(Debug, Clone, PartialEq)]
pub struct Definition {
    pub name: String,
    /// `create or replace`: what the definition does to a name held, which
    /// is no part of the statement defined -- its digest leaves it out.
    pub replace: bool,
    /// In the signature's order: `$1` in the body is the first.
    pub params: Vec<Param>,
    /// The token's claims the body reads, numbered after the parameters:
    /// with two parameters, `$jwt.sub` is `Expr::Param(2)` when `sub` is
    /// the first claim declared.
    pub claims: Vec<Claim>,
    /// `public`: a body that writes and reads no claim, which writes the
    /// same rows the same way for every token granted it, says so.
    pub public: bool,
    /// `scans`: a writing body whose plan walks a collection no index
    /// narrows, acknowledged (checked against the schema in a later slice).
    pub scans: bool,
    /// `comment '<text>'`: what `describe`, the MCP tool and generated types
    /// say of it.
    pub comment: Option<String>,
    /// `check <expr>`: a condition over the parameters, the claims and
    /// `now()` alone, asked before a call takes any lock.
    pub check: Option<Expr>,
    /// One statement, or the statements of `begin ... end` in order: the
    /// last is what a call answers.
    pub body: Vec<Statement>,
    /// Written `begin ... end`: the text closes it, so no line after it
    /// goes on with it, which the shell reads (fenec-cli's `kind`). How it
    /// was written, not what it means: the digest leaves it out.
    pub block: bool,
}

/// `$name type [= literal]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: DataType,
    /// `= <literal>`: what it is when a call leaves it out; `Some(Null)`
    /// for `= null`. None: a call must give it, never null.
    pub default: Option<Value>,
}

/// `name type [from "<claim>"]`: a claim of the caller's token, read as
/// `$jwt.<name>`.
#[derive(Debug, Clone, PartialEq)]
pub struct Claim {
    pub name: String,
    pub ty: DataType,
    /// `from "cognito:groups"`: the token's claim, where FenecQL does not
    /// write its name as a name. None: the claim is `name` itself -- the
    /// parser writes `from "sub"` on a claim named `sub` as none.
    pub from: Option<String>,
}

impl Claim {
    /// The token's claim this reads.
    pub fn claim(&self) -> &str {
        self.from.as_deref().unwrap_or(&self.name)
    }
}

/// `call <name> [<arguments>] [as <claims>]`, and SQL's `CALL <name>(...)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub name: String,
    pub args: Args,
    /// `as {sub: 'alice'}` or `as $2`: the claims the server's token calls
    /// a statement reading claims with. `Named` or `Param` alone.
    pub claims: Option<Args>,
}

/// A call's arguments, matched to the signature by the engine, since the
/// parser reads no schema.
#[derive(Debug, Clone, PartialEq)]
pub enum Args {
    /// `call purge`, `CALL purge()`.
    None,
    /// `call transfer {from: 1, to: 2}`, and SQL's `CALL transfer(from =>
    /// 1, to => 2)`, which is the same call.
    Named(Vec<(String, Expr)>),
    /// `call transfer $1`: a parameter's object, its members by name.
    Param(usize),
    /// SQL's `CALL transfer(1, 2, 300)`: by place, then any by name
    /// (`CALL transfer(1, to => 2, amount => 300)`), as PostgreSQL's.
    Placed(Vec<Expr>, Vec<(String, Expr)>),
}

impl Args {
    /// Every expression it holds.
    pub fn exprs(&self) -> impl Iterator<Item = &Expr> {
        let (placed, named): (&[Expr], &[(String, Expr)]) = match self {
            Args::None | Args::Param(_) => (&[], &[]),
            Args::Named(n) => (&[], n),
            Args::Placed(p, n) => (p, n),
        };
        placed.iter().chain(named.iter().map(|(_, e)| e))
    }

    /// The highest `$n` it reads, one-based as [`Expr::max_param`]'s.
    pub fn max_param(&self) -> usize {
        match self {
            Args::Param(i) => i + 1,
            _ => self.exprs().map(Expr::max_param).max().unwrap_or(0),
        }
    }
}

/// Whether `name` is one a named statement may have: ASCII letters, digits
/// and `_`, starting with a letter, at most [`MAX_NAME`] bytes -- a path
/// segment with nothing to escape, and an identifier in every SDK.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= MAX_NAME
        && b[0].is_ascii_alphabetic()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// What a named statement's name is refused with.
pub fn name_refused(name: &str) -> String {
    format!(
        "`{name}` is no statement's name: ASCII letters, digits and `_`, starting with a \
         letter, at most {MAX_NAME} bytes"
    )
}

impl Definition {
    /// Whether the body writes: an `insert`, a `set` or a `del` among it.
    pub fn writes(&self) -> bool {
        self.body.iter().any(|s| !s.is_read_only())
    }

    /// Whether the body or the `check` reads a claim: a parameter numbered
    /// past the declared parameters.
    pub fn reads_claims(&self) -> bool {
        let n = self.params.len();
        self.body.iter().any(|s| s.max_param() > n)
            || self.check.as_ref().is_some_and(|c| c.max_param() > n)
    }

    /// What its tree alone refuses at its definition: the statement of the
    /// body it is about (`None` for the signature) and why. The checks
    /// against the schema -- names, indexes, the plan behind `scans` --
    /// are the engine's, at the definition and at each schema change's
    /// commit.
    pub fn refused(&self) -> Option<(Option<usize>, String)> {
        let at = |i: usize, why: String| Some((Some(i), why));
        if self.body.is_empty() {
            return Some((None, "a body holds at least one statement".into()));
        }
        if self.body.len() > MAX_BODY {
            return at(
                MAX_BODY,
                format!("a body holds at most {MAX_BODY} statements"),
            );
        }
        if let Some(why) = self.check.as_ref().and_then(check_refused) {
            return Some((None, why));
        }
        for (i, s) in self.body.iter().enumerate() {
            if let Some(why) = body_refused(s) {
                return at(i, why.into());
            }
            let mut own = None;
            each_collection(s, &mut |c| {
                if own.is_none() && crate::declared::own(c) {
                    own = Some(c.to_string());
                }
            });
            if let Some(c) = own {
                return at(
                    i,
                    format!(
                        "`{c}` is the database's own: a body names no collection whose name \
                         starts with `_`"
                    ),
                );
            }
        }
        let last = self.body.len() - 1;
        if let Some(why) = unnamed_answer(&self.body[last]) {
            return at(last, why.into());
        }
        if self.public && !self.claims.is_empty() {
            return Some((
                None,
                "`public` says the body writes on behalf of nobody in particular, and the \
                 statement declares claims: one or the other"
                    .into(),
            ));
        }
        if self.writes() && !self.public && !self.reads_claims() {
            return Some((
                None,
                format!(
                    "statement `{}` writes and reads no claim, so every token granted it \
                     writes the same rows the same way: read the caller's claims \
                     (`claims (sub text)`, `$jwt.sub`), or say so with `public`",
                    self.name
                ),
            ));
        }
        None
    }

    /// The canonical tree's digest: [`crate::hll::hash`] of
    /// [`Self::canonical`]. Equal for every text that parses to the same
    /// statement -- white space, comments, a keyword's case, SQL's spelling
    /// of a FenecQL form, `as <statement>` or `as begin <statement> end`,
    /// `create` or `create or replace` -- and another wherever the meaning
    /// differs, a literal's bits, a name, a type, a marker. A body statement
    /// that is no body's (a schema change, a `call`) has none.
    pub fn digest(&self) -> Result<u64> {
        Ok(crate::hll::hash(&self.canonical()?))
    }

    /// The canonical tree, as bytes. A node is its kind's byte, then each
    /// field that holds something -- not `None`, `false`, empty or `0`
    /// (an `offset` of 0, an ascending key) -- as its tag and its value, in
    /// the order of their tags, then a 0. A text is its length and its
    /// UTF-8 bytes, a count a varint, a literal and a type as the file
    /// writes them (`codec::encode_value`, `codec::encode_type`), an
    /// expression its variant's byte and its operands. So a field added to
    /// the tree that holds nothing in a statement leaves that statement's
    /// digest as it was: only a statement using it reads otherwise. A
    /// tag is never reused nor renumbered; a change that writes an old
    /// tree otherwise raises fenec-ql's `GRAMMAR_EPOCH`.
    pub fn canonical(&self) -> Result<Vec<u8>> {
        let Definition {
            name,
            replace: _,
            params,
            claims,
            public,
            scans,
            comment,
            check,
            body,
            block: _,
        } = self;
        let mut c = Canon(Vec::new());
        c.node(1);
        c.text(1, name);
        c.list(2, params, |c, p| {
            c.node(2);
            c.text(1, &p.name);
            c.ty(2, &p.ty);
            if let Some(v) = &p.default {
                c.tag(3);
                c.value(v);
            }
            c.end();
        });
        c.list(3, claims, |c, k| {
            c.node(3);
            c.text(1, &k.name);
            c.ty(2, &k.ty);
            c.opt_text(3, k.from.as_deref());
            c.end();
        });
        c.flag(4, *public);
        c.flag(5, *scans);
        c.opt_text(6, comment.as_deref());
        c.opt_expr(7, check);
        if !body.is_empty() {
            c.tag(8);
            c.uint(body.len() as u64);
            for s in body {
                c.statement(s)?;
            }
        }
        c.end();
        Ok(c.0)
    }
}

/// Why `check` is refused, if it is: it reads the parameters, the claims
/// and `now()` alone, before a call reads any row.
pub fn check_refused(e: &Expr) -> Option<String> {
    let mut why = None;
    let mut walk = |e: &Expr| {
        if why.is_some() {
            return;
        }
        why = match e {
            Expr::Field(f) => Some(format!(
                "`check` reads the parameters, the claims and `now()`, and no field: `{f}`"
            )),
            Expr::InSelect(..) => Some("`check` reads no collection: no `in (get ...)`".into()),
            e if e.aggregate_name().is_some() => Some("`check` folds no rows".into()),
            e if e.asks_expired() => Some("`check` reads no row: no `expired()`".into()),
            _ => None,
        };
    };
    each_expr(e, &mut walk);
    why
}

/// Why a body refuses `s`, if it does: the statement it holds is a `get`,
/// an `insert` with its documents in the text, or a `set` or a `del` that
/// says which rows it writes.
fn body_refused(s: &Statement) -> Option<&'static str> {
    Some(match s {
        Statement::Select(_) => return None,
        Statement::Put {
            insert,
            docs_param,
            else_set,
            ..
        } => {
            if docs_param.is_some() {
                "`insert <collection> $n` writes the caller's documents -- fields, ids and \
                 values -- past every guard the text holds: a body writes its documents in \
                 the text, `insert entries {from: $from, by: $jwt.sub}`"
            } else if else_set.is_some() {
                "`if absent else set` finds its row by a key the caller's document names: a \
                 body writes `insert ... if absent`, or `set ... where` the row it means"
            } else if !insert {
                "a `put` naming an id writes over whatever row holds it, and an argument could \
                 name anyone's: a body writes `insert`, which never writes over a row, or `set \
                 ... where` the row it means"
            } else {
                return None;
            }
        }
        Statement::Update { filter, pick, .. } | Statement::Delete { filter, pick, .. } => {
            if filter.is_none() && !pick.as_ref().is_some_and(|p| p.picks()) {
                "a body's `set` and `del` say which rows they write: a `where`, or `order` and \
                 `limit` -- `where true` where every row is meant"
            } else {
                return None;
            }
        }
        Statement::CreateCollection { .. }
        | Statement::DropCollection { .. }
        | Statement::CreateIndex { .. }
        | Statement::CreateComposite { .. }
        | Statement::DropComposite { .. }
        | Statement::AlterCollection { .. }
        | Statement::CreateStatement(_)
        | Statement::DropStatement { .. } => {
            "a body changes no schema: that is the server token's, and a statement holding it \
             would hand it to every caller"
        }
        Statement::Compact(_) => "a body compacts nothing: `compact` is the server token's",
        Statement::Call(_) => {
            "a body calls no statement: it is read whole where it is defined, with no \
             recursion to bound"
        }
        Statement::Explain(_)
        | Statement::ListCollections
        | Statement::Describe(_)
        | Statement::ListStatements
        | Statement::DescribeStatement(_) => {
            "a body holds `get`, `insert`, `set ... where` and `del ... where`"
        }
    })
}

/// Why the last statement's answer is refused, if it is: it names every
/// field it answers, since a statement runs with an authority of its own,
/// and `*` would start answering a field added after it was defined.
fn unnamed_answer(s: &Statement) -> Option<&'static str> {
    const STAR: &str = "the last statement is what a call answers, and names every field it \
                        answers: `select a, b`, `returning a, b` -- `*`, or a `get` with no \
                        list, would answer a field added after it was defined";
    match s {
        Statement::Select(sel) => {
            let named = |project: &Option<Vec<String>>, folds: bool| project.is_some() || folds;
            let mut ok = named(&sel.project, !sel.aggregate.is_empty()) || sel.count;
            if let Some(f) = &sel.flat {
                ok &= !f.columns.iter().any(|c| c.column.is_empty());
            }
            let mut level = sel.lookup.as_ref();
            while let (true, Some(l)) = (ok, level) {
                ok = named(&l.project, !l.aggregate.is_empty());
                level = l.next.as_deref();
            }
            (!ok).then_some(STAR)
        }
        _ => match s.pick().and_then(|p| p.returning.as_ref()) {
            Some(None) => Some(STAR),
            _ => None,
        },
    }
}

/// Calls `f` on `e` and on every expression under it, an inner `get`'s
/// among them.
fn each_expr<'a>(e: &'a Expr, f: &mut dyn FnMut(&'a Expr)) {
    f(e);
    if let Expr::InSelect(_, sel) = e {
        select_exprs(sel, f);
    }
    e.each_child(&mut |c| each_expr(c, f));
}

/// Calls `f` on every expression of a `get` -- each clause's, each
/// `lookup` level's, each inner `get`'s -- and on what is under each.
fn select_exprs<'a>(s: &'a Select, f: &mut dyn FnMut(&'a Expr)) {
    let mut top: Vec<&'a Expr> = Vec::new();
    top.extend(&s.filter);
    top.extend(&s.having);
    top.extend(&s.group);
    top.extend(s.aggregate.iter().chain(&s.computed).map(|c| &c.expr));
    top.extend(s.order.iter().filter_map(|o| o.expr.as_ref()));
    top.extend(s.near.as_ref().map(|n| &n.vector));
    top.extend(s.matcher.as_ref().map(|m| &m.query));
    top.extend(s.rerank.as_ref().map(|r| &r.vector));
    top.extend(s.marks.iter().flat_map(|m| &m.args));
    for l in s.lookup.iter().flat_map(Lookup::chain) {
        top.extend(&l.filter);
        top.extend(l.list_exprs());
        top.extend(l.order.iter().filter_map(|o| o.expr.as_ref()));
    }
    for e in top {
        each_expr(e, f);
    }
}

/// Calls `f` on every collection `s` names: its own, a `lookup` level's,
/// an inner `get`'s.
fn each_collection<'a>(s: &'a Statement, f: &mut dyn FnMut(&str)) {
    let mut gets: Vec<&'a Select> = Vec::new();
    let mut exprs: Vec<&'a Expr> = Vec::new();
    match s {
        Statement::Select(sel) | Statement::Explain(sel) => gets.push(sel),
        Statement::Put {
            collection,
            docs,
            else_set,
            pick,
            ..
        } => {
            f(collection);
            exprs.extend(docs.iter().flatten().map(|(_, e)| e));
            exprs.extend(else_set.iter().flatten().map(|(_, e)| e));
            exprs.extend(
                pick.iter()
                    .flat_map(|p| &p.order)
                    .filter_map(|o| o.expr.as_ref()),
            );
        }
        Statement::Update {
            collection,
            set,
            filter,
            pick,
            ..
        } => {
            f(collection);
            exprs.extend(set.iter().map(|(_, e)| e));
            exprs.extend(filter);
            exprs.extend(
                pick.iter()
                    .flat_map(|p| &p.order)
                    .filter_map(|o| o.expr.as_ref()),
            );
        }
        Statement::Delete {
            collection,
            filter,
            pick,
            ..
        } => {
            f(collection);
            exprs.extend(filter);
            exprs.extend(
                pick.iter()
                    .flat_map(|p| &p.order)
                    .filter_map(|o| o.expr.as_ref()),
            );
        }
        _ => {}
    }
    // Every inner `get`, however deep: `each_expr` walks into each.
    let mut inner: Vec<&'a Select> = Vec::new();
    let mut found = |e: &'a Expr| {
        if let Expr::InSelect(_, sel) = e {
            inner.push(sel);
        }
    };
    for e in exprs {
        each_expr(e, &mut found);
    }
    for sel in &gets {
        select_exprs(sel, &mut found);
    }
    for sel in gets.iter().chain(&inner) {
        f(&sel.collection);
        for l in sel.lookup.iter().flat_map(Lookup::chain) {
            f(&l.collection);
        }
    }
}

/// An operator's byte in the canonical tree: fixed here, never the
/// variant's place in `CmpOp`, so reordering the enum or adding to it moves
/// no stored digest.
fn cmp_byte(op: CmpOp) -> u64 {
    match op {
        CmpOp::Eq => 0,
        CmpOp::Ne => 1,
        CmpOp::Lt => 2,
        CmpOp::Le => 3,
        CmpOp::Gt => 4,
        CmpOp::Ge => 5,
    }
}

/// As [`cmp_byte`], for `ArithOp`.
fn arith_byte(op: ArithOp) -> u64 {
    match op {
        ArithOp::Add => 0,
        ArithOp::Sub => 1,
        ArithOp::Mul => 2,
        ArithOp::Div => 3,
    }
}

/// The canonical tree's writer ([`Definition::canonical`]).
struct Canon(Vec<u8>);

impl Canon {
    fn tag(&mut self, t: u8) {
        self.0.push(t);
    }

    /// A node's kind.
    fn node(&mut self, kind: u8) {
        self.0.push(kind);
    }

    /// A node's end.
    fn end(&mut self) {
        self.0.push(0);
    }

    fn uint(&mut self, n: u64) {
        codec::put_uvarint(&mut self.0, n);
    }

    fn str(&mut self, s: &str) {
        codec::encode_str(&mut self.0, s);
    }

    fn value(&mut self, v: &Value) {
        codec::encode_value(&mut self.0, v);
    }

    fn text(&mut self, tag: u8, s: &str) {
        if !s.is_empty() {
            self.tag(tag);
            self.str(s);
        }
    }

    fn opt_text(&mut self, tag: u8, s: Option<&str>) {
        if let Some(s) = s {
            self.tag(tag);
            self.str(s);
        }
    }

    fn flag(&mut self, tag: u8, b: bool) {
        if b {
            self.tag(tag);
        }
    }

    fn num(&mut self, tag: u8, n: u64) {
        if n != 0 {
            self.tag(tag);
            self.uint(n);
        }
    }

    fn opt_num(&mut self, tag: u8, n: Option<u64>) {
        if let Some(n) = n {
            self.tag(tag);
            self.uint(n);
        }
    }

    /// `require <n> [else '<reason>']`: the count under `tag`, and the
    /// reason, a part of the API it answers, under `reason` where one is
    /// given -- a field of its own, so a guard without one writes what it
    /// wrote before reasons were read.
    fn require(&mut self, tag: u8, reason: u8, r: &Option<Require>) {
        if let Some(r) = r {
            self.tag(tag);
            self.uint(r.n);
            self.text(reason, &r.reason);
        }
    }

    fn ty(&mut self, tag: u8, t: &DataType) {
        self.tag(tag);
        codec::encode_type(&mut self.0, t);
    }

    fn names(&mut self, tag: u8, v: &[String]) {
        if !v.is_empty() {
            self.tag(tag);
            self.uint(v.len() as u64);
            for s in v {
                self.str(s);
            }
        }
    }

    /// `None` writes nothing; `Some` the list, empty or not, which differ:
    /// a `project` of none is every field, of an empty list none.
    fn opt_names(&mut self, tag: u8, v: &Option<Vec<String>>) {
        if let Some(v) = v {
            self.tag(tag);
            self.uint(v.len() as u64);
            for s in v {
                self.str(s);
            }
        }
    }

    fn list<T>(&mut self, tag: u8, v: &[T], mut each: impl FnMut(&mut Canon, &T)) {
        if !v.is_empty() {
            self.tag(tag);
            self.uint(v.len() as u64);
            for x in v {
                each(self, x);
            }
        }
    }

    fn count(&mut self, c: Count) {
        match c {
            Count::N(n) => {
                self.uint(0);
                self.uint(n as u64);
            }
            Count::Param(i) => {
                self.uint(1);
                self.uint(i as u64);
            }
        }
    }

    fn opt_count(&mut self, tag: u8, c: Option<Count>) {
        if let Some(c) = c {
            self.tag(tag);
            self.count(c);
        }
    }

    /// An `offset`: `0` written is none.
    fn offset(&mut self, tag: u8, c: Count) {
        if !c.is_zero() {
            self.tag(tag);
            self.count(c);
        }
    }

    fn opt_expr(&mut self, tag: u8, e: &Option<Expr>) {
        if let Some(e) = e {
            self.tag(tag);
            self.expr(e);
        }
    }

    fn exprs(&mut self, tag: u8, v: &[Expr]) {
        self.list(tag, v, |c, e| c.expr(e));
    }

    fn pairs(&mut self, tag: u8, v: &[(String, Expr)]) {
        self.list(tag, v, |c, (k, e)| {
            c.str(k);
            c.expr(e);
        });
    }

    fn columns(&mut self, tag: u8, v: &[Column]) {
        self.list(tag, v, |c, Column { expr, name }| {
            c.str(name);
            c.expr(expr);
        });
    }

    fn sorts(&mut self, tag: u8, v: &[Sort]) {
        self.list(tag, v, |c, s| {
            let Sort {
                field,
                asc,
                collate,
                expr,
            } = s;
            c.node(1);
            c.text(1, field);
            c.flag(2, !asc);
            c.opt_text(3, collate.map(|k| k.name()));
            c.opt_expr(4, expr);
            c.end();
        });
    }

    fn expr(&mut self, e: &Expr) {
        match e {
            Expr::Field(f) => {
                self.node(1);
                self.str(f);
            }
            Expr::Lit(v) => {
                self.node(2);
                self.value(v);
            }
            Expr::Param(i) => {
                self.node(3);
                self.uint(*i as u64);
            }
            Expr::And(a, b) => self.two(4, a, b),
            Expr::Or(a, b) => self.two(5, a, b),
            Expr::Not(a) => {
                self.node(6);
                self.expr(a);
            }
            Expr::Cmp(op, a, b) => {
                self.node(7);
                self.uint(cmp_byte(*op));
                self.expr(a);
                self.expr(b);
            }
            Expr::Like(a, b) => self.two(8, a, b),
            Expr::Has(a, b) => self.two(9, a, b),
            Expr::In(a, items) => {
                self.node(10);
                self.expr(a);
                self.uint(items.len() as u64);
                for i in items {
                    self.expr(i);
                }
            }
            Expr::InSelect(a, sel) => {
                self.node(11);
                self.expr(a);
                self.select(sel);
            }
            Expr::IsNull(a) => {
                self.node(12);
                self.expr(a);
            }
            Expr::Call(name, args) => {
                self.node(13);
                self.str(name);
                self.uint(args.len() as u64);
                for a in args {
                    self.expr(a);
                }
            }
            Expr::Arith(op, a, b) => {
                self.node(14);
                self.uint(arith_byte(*op));
                self.expr(a);
                self.expr(b);
            }
        }
    }

    fn two(&mut self, kind: u8, a: &Expr, b: &Expr) {
        self.node(kind);
        self.expr(a);
        self.expr(b);
    }

    fn select(&mut self, s: &Select) {
        let Select {
            collection,
            project,
            filter,
            near,
            matcher,
            rerank,
            order,
            limit,
            offset,
            count,
            lookup,
            aggregate,
            group,
            having,
            computed,
            fuse,
            marks,
            facets,
            require,
            flat,
        } = s;
        self.node(1);
        self.text(1, collection);
        self.opt_names(2, project);
        self.opt_expr(3, filter);
        if let Some(Near {
            field,
            vector,
            ef,
            exact,
            metric,
        }) = near
        {
            self.tag(4);
            self.node(1);
            self.text(1, field);
            self.tag(2);
            self.expr(vector);
            self.opt_num(3, ef.map(|n| n as u64));
            self.flag(4, *exact);
            self.opt_text(5, metric.map(|m| m.name()));
            self.end();
        }
        if let Some(Match {
            field,
            query,
            within,
            fuzzy,
            prefix,
        }) = matcher
        {
            self.tag(5);
            self.node(1);
            self.text(1, field);
            self.tag(2);
            self.expr(query);
            self.flag(3, *within);
            self.num(4, *fuzzy as u64);
            self.flag(5, *prefix);
            self.end();
        }
        if let Some(Rerank {
            field,
            vector,
            candidates,
        }) = rerank
        {
            self.tag(6);
            self.node(1);
            self.text(1, field);
            self.tag(2);
            self.expr(vector);
            self.opt_num(3, candidates.map(|n| n as u64));
            self.end();
        }
        self.sorts(7, order);
        self.opt_count(8, *limit);
        self.offset(9, *offset);
        self.flag(10, *count);
        if let Some(l) = lookup {
            self.tag(11);
            self.lookup(l);
        }
        self.columns(12, aggregate);
        self.exprs(13, group);
        self.opt_expr(14, having);
        self.columns(15, computed);
        if let Some(Fuse { k, candidates }) = fuse {
            self.tag(16);
            self.node(1);
            self.opt_num(1, k.map(u64::from));
            self.opt_num(2, candidates.map(|n| n as u64));
            self.end();
        }
        self.list(17, marks, |c, m| {
            let Mark {
                field,
                snippet,
                args,
            } = m;
            c.node(1);
            c.text(1, field);
            c.opt_num(2, snippet.map(|n| n as u64));
            c.exprs(3, args);
            c.end();
        });
        self.list(18, facets, |c, f| {
            // `rest` is the filter split for a disjunctive facet, worked
            // out of the filter and the facets by the parser.
            let Facet {
                field,
                top,
                ranges,
                disjunctive,
                rest: _,
            } = f;
            c.node(1);
            c.text(1, field);
            c.opt_count(2, *top);
            if let Some(r) = ranges {
                c.tag(3);
                c.uint(r.len() as u64);
                for v in r {
                    c.value(v);
                }
            }
            c.flag(4, *disjunctive);
            c.end();
        });
        self.require(19, 21, require);
        if let Some(f) = flat {
            self.tag(20);
            self.flat(f);
        }
        self.end();
    }

    fn flat(&mut self, f: &Flat) {
        let Flat {
            columns,
            offset,
            limit,
            shown,
            names,
            probe,
        } = f;
        self.node(1);
        self.list(1, columns, |c, col| {
            c.uint(col.level as u64);
            c.str(&col.column);
            c.str(&col.name);
        });
        self.offset(2, *offset);
        self.opt_count(3, *limit);
        self.list(4, shown, |c, (level, fields)| {
            c.uint(*level as u64);
            c.uint(fields.len() as u64);
            for s in fields {
                c.str(s);
            }
        });
        if let Some(n) = names {
            self.tag(5);
            self.uint(n.tables.len() as u64);
            for (alias, collection) in &n.tables {
                self.str(alias);
                self.str(collection);
            }
            self.uint(n.names.len() as u64);
            for (table, name, bare) in &n.names {
                self.uint(*table as u64);
                self.str(name);
                self.uint(u64::from(*bare));
            }
        }
        self.flag(6, *probe);
        self.end();
    }

    fn lookup(&mut self, l: &Lookup) {
        let Lookup {
            collection,
            on,
            parent,
            project,
            aggregate,
            group,
            having,
            filter,
            order,
            limit,
            offset,
            required,
            next,
        } = l;
        self.node(1);
        self.text(1, collection);
        self.names(2, on);
        self.names(3, parent);
        self.opt_names(4, project);
        self.columns(5, aggregate);
        self.exprs(6, group);
        self.opt_expr(7, having);
        self.opt_expr(8, filter);
        self.sorts(9, order);
        self.opt_count(10, *limit);
        self.offset(11, *offset);
        self.flag(12, *required);
        if let Some(n) = next {
            self.tag(13);
            self.lookup(n);
        }
        self.end();
    }

    fn pick(&mut self, tag: u8, p: &Option<Box<Pick>>) {
        let Some(p) = p else { return };
        let Pick {
            order,
            limit,
            returning,
            conflict,
        } = &**p;
        self.tag(tag);
        self.node(1);
        self.sorts(1, order);
        self.opt_count(2, *limit);
        if let Some(r) = returning {
            self.tag(3);
            match r {
                None => self.uint(0),
                Some(names) => {
                    self.uint(1);
                    self.uint(names.len() as u64);
                    for s in names {
                        self.str(s);
                    }
                }
            }
        }
        self.opt_names(4, conflict);
        self.end();
    }

    /// A body's statement: a `get`, an `insert`, a `set` or a `del`.
    fn statement(&mut self, s: &Statement) -> Result<()> {
        match s {
            Statement::Select(sel) => {
                self.node(1);
                self.select(sel);
            }
            Statement::Put {
                collection,
                docs,
                docs_param,
                insert,
                if_absent,
                else_set,
                require,
                pick,
            } => {
                self.node(2);
                self.text(1, collection);
                self.list(2, docs, |c, d| {
                    c.uint(d.len() as u64);
                    for (k, e) in d {
                        c.str(k);
                        c.expr(e);
                    }
                });
                self.opt_num(3, docs_param.map(|i| i as u64));
                self.flag(4, *insert);
                self.flag(5, *if_absent);
                if let Some(set) = else_set {
                    self.tag(6);
                    self.uint(set.len() as u64);
                    for (k, e) in set {
                        self.str(k);
                        self.expr(e);
                    }
                }
                self.require(7, 9, require);
                self.pick(8, pick);
                self.end();
            }
            Statement::Update {
                collection,
                set,
                filter,
                require,
                pick,
            } => {
                self.node(3);
                self.text(1, collection);
                self.pairs(2, set);
                self.opt_expr(3, filter);
                self.require(4, 6, require);
                self.pick(5, pick);
                self.end();
            }
            Statement::Delete {
                collection,
                filter,
                require,
                pick,
            } => {
                self.node(4);
                self.text(1, collection);
                self.opt_expr(2, filter);
                self.require(3, 5, require);
                self.pick(4, pick);
                self.end();
            }
            _ => {
                return Err(Error::Query(
                    "a body holds `get`, `insert`, `set ... where` and `del ... where`".into(),
                ))
            }
        }
        Ok(())
    }
}
