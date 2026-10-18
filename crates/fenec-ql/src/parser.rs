//! FenecQL parser: token stream -> `fenec_core::query::Statement`.
//!
//! Language summary
//! ```text
//! create collection [if not exists] <name> ( <field> <type> [required] [collate und|tr] [nocase] [@index], ... )
//!        type:  bool int float text bytes timestamp vector<N[, f16]> sparse<N> [type] json
//!        index: @hash @unique @sorted @ttl(30m) @hnsw(..) @text(..) @inverted (a sparse<N> field's)
//!        and among the fields: @sorted(<field> [desc], ...) | @unique(<field>, ...)
//! create index [if not exists] on <name> (<field> [desc], <field> [desc], ...) @sorted | @unique
//! drop   index [if exists] on <name> (<field>, <field>, ...)
//! drop   collection [if exists] <name>
//! put    <name> { k: v, ... }            -- or [ {...}, {...} ]; v may be {..}, a json value
//! a.b.c                                  -- a path into a json field, where a field goes
//! get    <name> [select a, b] [where <expr>] [near <field> <vector> [ef N] [exact]]
//!            [match <field> <text>] [rerank <field> <vector> [candidates N]]
//!            [fuse [k N] [candidates N]]     -- match and near, by reciprocal rank
//!            [order <key> [collate und|tr] [asc|desc], ...] [limit N] [offset N] [count]
//!            -- a key: a field, or any expression over the row, `distance(loc, $1)`, `a * b`
//!            [facet <field> [top N], ...]   -- value counts over every matched row
//!            [lookup <name> on <child> [= <parent>] [required] <clauses...>]
//!            [lookup <name> on (<child>, ...) = (<parent>, ...) [required] <clauses...>]
//! get    <name> select a, highlight(f [, pre, post]), snippet(f, N [, ellipsis [, pre, post]])
//!            match <field> <text> ...       -- the spans the match's terms were read from
//! where  <field> in (get <name> select <field> ...)  -- the inner get's one column, run once
//! alter  collection <name> alter field <field> @ttl(<duration>) | @sorted
//! get    <name> select [<key>,] count(*) | count(distinct e) | sum(e) | avg(e) | min(e)
//!            | max(e) | first(e [by k]) | last(e [by k]) [as <name>], ...
//!            [where <expr>] [group <key>, ... [having <expr>] [order <column> [desc]] [limit N] [offset N] [count]]
//!            -- also approx_count_distinct(e), hll_accumulate(e), hll_combine(sketch)
//! get    <name> select a, <expr> as <name>   -- a column worked out over each row
//! bucket(at, 15m) | greatest(a, b) | least(a, b) | case when <c> then <v> ... [else <v>] end
//! select a, b from <name> ...            -- the classic SQL order works too
//! put    <name> { ... } if absent         -- a document whose id or @unique value is held is passed over
//! put    <name> { ... } if absent else set { n: n + new.n }   -- or sets the row holding it
//! put    <name> $1                       -- the documents a parameter holds: an object or a list
//! put | insert ... [returning * | a, b]  -- the rows written, ids and all, in the documents' order
//! set    <name> { k: v, ... } [where <expr>] -- v may read the row: {n: n + 1, at: now()}
//! <expr> + - * / <expr>                  -- numbers, or a timestamp and milliseconds
//! del    <name> [where <expr>]
//! set | del ... [order <key> [desc], ...] [limit N] [returning * | a, b]
//!                                        -- the rows a get would page to, answered as written
//! collections | describe <name> | compact [<name>]
//! ```

use crate::lexer::{tokenize_vectors as tokenize, Tok, Token};
use fenec_core::collate::Collation;
use fenec_core::engine::Database;
use fenec_core::error::{Error, Result};
use fenec_core::query::*;
use fenec_core::schema::{
    Composite, Field, IndexKind, Metric, Quant, Schema, TextIndexSpec, VectorIndexSpec,
};
use fenec_core::value::{DataType, Value, VecPrec};

/// The maximum nesting level of an expression.
///
/// Both the parser and the evaluator are recursive: depth is stack depth
/// directly. Without a limit, `((((...))))` or a long `or` chain overflows
/// the stack -- and a stack overflow is not a catchable panic but an
/// `abort` of the process; in `fenec-server` a single query would take the whole
/// server down.
///
/// The cost of the limit was measured (511 levels, worst shape = parentheses):
///
/// | build | stack required |
/// |---|---|
/// | release | ~750 KiB |
/// | debug   | ~5 MiB   |
///
/// So `thread::spawn`'s 2 MiB default leaves ~2.7x headroom in release and
/// is *not enough* in a debug build. 512 is therefore not safe on its own:
/// whoever runs it sizes its stack explicitly (`SESSION_STACK` in `fenec-server`,
/// 8 MiB in tests). The value stayed at 512 because generated `or` chains
/// easily reach hundreds of terms; a lower ceiling would cut off legitimate
/// queries.
pub const MAX_EXPR_DEPTH: usize = 512;

pub struct Parser {
    toks: Vec<Token>,
    i: usize,
    /// Stack depth of the expression currently being built.
    depth: usize,
    /// How many `in (get ...)` the parser is inside.
    subqueries: usize,
    /// Inside an object literal, whose lists keep their numbers as written
    /// rather than become a vector: only a `json` field holds an object,
    /// and `[19.99]` in it is two decimals, not an `f32`.
    exact: bool,
    /// The end of the text decided how a statement was read: the SQL
    /// order's list ran to it and was read again as a collection's name
    /// for want of its `from`, or `alter field f` found it where an `@`
    /// would have chosen `@ttl(..)`. What [`reading`] tells a reader a line
    /// at a time, whose next line may hold the `from` or the `@`.
    cut: bool,
}

pub fn parse(src: &str) -> Result<Vec<Statement>> {
    statements(tokenize(src)?, false)
}

/// [`parse`], every list of numbers kept as written -- integers integers,
/// decimals the `f64`s they say -- rather than read into a vector's `f32`s:
/// what a json field is handed. A vector field takes such a list a number
/// at a time, which is slower than the vector [`parse`] makes, so a text is
/// read so only where [`parse_for`] finds a json field given one.
pub fn parse_exact(src: &str) -> Result<Vec<Statement>> {
    statements(crate::lexer::tokenize(src)?, true)
}

/// `src` parsed for `db`: [`parse`], read again by [`parse_exact`] where a
/// statement hands a json field of `db` a list of numbers, or compares one
/// with a path into it (`Database::exactly`). A statement with no list of
/// numbers costs the walk of its literals, and one into a collection with
/// no json field a look at its fields.
pub fn parse_for(db: &Database, src: &str) -> Result<Vec<Statement>> {
    let stmts = parse(src)?;
    match stmts
        .iter()
        .any(|s| s.reads_vectors() && db.exactly(s).text)
    {
        true => parse_exact(src),
        false => Ok(stmts),
    }
}

/// [`parse_for`], of a single statement.
pub fn parse_one_for(db: &Database, src: &str) -> Result<Statement> {
    one(parse_for(db, src)?)
}

/// Inlined into each caller natively: called out of line, a `put` of 1 000
/// 128-dim rows parsed 1.4% slower (8.27 -> 8.39 ms) than with the body in
/// `parse`; the browser module keeps one copy.
#[cfg_attr(not(target_arch = "wasm32"), inline(always))]
fn statements(toks: Vec<Token>, exact: bool) -> Result<Vec<Statement>> {
    let mut p = Parser {
        toks,
        i: 0,
        depth: 0,
        subqueries: 0,
        exact,
        cut: false,
    };
    let mut out = Vec::new();
    while !p.ended() {
        out.push(p.statement()?);
    }
    if out.is_empty() {
        return Err(Error::Query("empty query".into()));
    }
    Ok(out)
}

/// Where each statement of `src` is, in bytes: its text runs from its
/// first token to the next statement's, a `;` between them included. A
/// sync replica sends a server each write of a text on its own line of a
/// `/batch`, as written -- the parser's own reading of where one ends,
/// rather than a split on `;`, which a string holding one would cut. In
/// characters, as a token's position counts, a text holding a letter
/// outside ASCII had its second statement sliced a byte early and its
/// first cut short. Unused by the browser module, whose link leaves it
/// out.
pub fn spans(src: &str) -> Result<Vec<(usize, usize)>> {
    let r = reading(src);
    let mut out = r.whole;
    match r.rest {
        Rest::Nothing => {}
        Rest::Last(at, _) => out.push((at, src.len())),
        Rest::Refused(r) => return Err(r.error),
    }
    Ok(out)
}

/// A text as a reader taking it a line at a time finds it -- the shell,
/// which runs a statement once nothing after it can still go on with it.
/// A statement has no end of its own: `get notes select body` is whole,
/// and so is it with `where topic = 4` on the next line, so a statement
/// is whole once a `;` or another statement follows it, and the last may
/// yet take more. Unused by the browser module, whose link leaves it out.
#[derive(Debug, PartialEq)]
pub struct Reading {
    /// The statements read whole, each the bytes from its first token to
    /// the next statement's, as [`spans`] gives them.
    pub whole: Vec<(usize, usize)>,
    /// What follows them.
    pub rest: Rest,
}

/// What a [`Reading`] ends in.
#[derive(Debug, PartialEq)]
pub enum Rest {
    /// White space, comments and `;`s alone.
    Nothing,
    /// A statement read whole up to the end of the text, from this byte:
    /// a next line may go on with it -- `where`, `order`, `and` -- or
    /// begin another, which says it was whole.
    Last(usize, Box<Statement>),
    /// A statement refused, so far as the text goes.
    Refused(Refusal),
}

/// A statement refused, so far as the text goes: whether the refusal
/// stands is for what comes after to say ([`Refusal::stands`]).
#[derive(Debug, PartialEq)]
pub struct Refusal {
    /// Where the text refused begins, in bytes. Where what was refused
    /// begins no statement -- an unknown command, `where` on a line of its
    /// own, a clause written past a `;` -- it may go on with the statement
    /// before it: that statement is refused with it, since run alone it
    /// could be a write wider than the one written (`del notes`, then
    /// `wher id = 3`; `del notes; where id = 3`).
    pub at: usize,
    /// Where what begins no statement is, in bytes, when that is what was
    /// refused.
    pub begins: Option<usize>,
    /// Where the parser stood as it refused, in bytes: the place the
    /// error's `position` names, which a reader that lets go of the text
    /// before `at` moves with the rest, as the error's words cannot be.
    pub place: usize,
    pub error: Error,
    /// Refused at the end of the text: what comes after may make it read.
    pub end: bool,
}

impl Refusal {
    /// Whether this refusal stands whatever follows: `later`, the same
    /// text read with more after it, is refused where this one was, for
    /// the same reason, and not at its end. A refusal at the end is placed
    /// at the token after the one refused (a `limit "x"` past the `"x"`),
    /// which more text moves, so its words are compared without the
    /// position they lead with; one before the end keeps its place, or the
    /// same check refusing the next item would stand for the one before --
    /// `select topic + 1` answers under no name until `as a`, and then
    /// `topic + 2` until `as b`, each line a refusal of the same words. A
    /// refusal at the end of `later` never stands, since a line more may
    /// go on with it.
    pub fn stands(&self, later: &Refusal) -> bool {
        let same = match (&self.error, &later.error) {
            (Error::Query(a), Error::Query(b)) => unplaced(a) == unplaced(b),
            (a, b) => a == b,
        };
        same && !later.end
            && self.at == later.at
            && self.begins == later.begins
            && (self.end || self.place == later.place)
    }

    /// Whether this refusal of `src` stands with no next line to ask -- a
    /// line typed, the shell waiting on its user. One before the end does.
    /// One at the end stands where a line more that most statements go on
    /// with, `limit 1`, leaves it standing ([`Self::stands`]): what the end
    /// refused moves with it -- a clause not finished, a check of the whole
    /// statement such as `match` and `near` with no `fuse` -- and a refusal
    /// of what was read, placed past it at the end, does not (`limit "x"`,
    /// `@foo`). A check a next line could satisfy and `limit` cannot -- an
    /// item with no `as` yet, `select a, b` before its `from` -- stands, as
    /// it did when the shell ran each line.
    pub fn stands_alone(&self, src: &str) -> bool {
        if !self.end {
            return true;
        }
        match reading(&format!("{src}\nlimit 1")).rest {
            Rest::Refused(later) => self.stands(&later),
            _ => false,
        }
    }
}

/// An error's words past the `position N: ` the parser leads them with.
/// Its digits counted rather than split at `: `, which took the searcher
/// for a pattern of several bytes, 1 KB of the shell.
fn unplaced(m: &str) -> &str {
    let Some(r) = m.strip_prefix("position ") else {
        return m;
    };
    let n = r.bytes().take_while(u8::is_ascii_digit).count();
    match r.get(n..).and_then(|w| w.strip_prefix(": ")) {
        Some(w) if n > 0 => w,
        _ => m,
    }
}

/// What [`Reading`] finds `src` to be.
pub fn reading(src: &str) -> Reading {
    let toks = match tokenize(src) {
        Ok(t) => t,
        // Refused whole: a text the lexer refuses has no statements to
        // tell apart. Only an open string is cut short by its end.
        Err(error) => {
            let end = matches!(&error, Error::Query(m) if m.ends_with(crate::lexer::UNTERMINATED));
            let r = Refusal {
                at: 0,
                begins: None,
                place: 0,
                error,
                end,
            };
            return Reading {
                whole: Vec::new(),
                rest: Rest::Refused(r),
            };
        }
    };
    let mut p = Parser {
        toks,
        i: 0,
        depth: 0,
        subqueries: 0,
        exact: false,
        cut: false,
    };
    // In characters, as tokens are placed: each statement read whole, by
    // where it starts, and the one just read, which what follows decides,
    // with whether the end of the text cut its SQL list short and whether
    // a `;` ended it. Ended, it is whole where the text ends or a statement
    // begins after it, and is held back still by a word that begins none:
    // `del notes; where id = 3` on one line is refused, as `-c` and
    // `POST /query` refuse it, where pushed as whole at its `;` it was
    // every row's delete.
    let mut whole: Vec<usize> = Vec::new();
    let mut last: Option<(usize, Statement, bool, bool)> = None;
    let rest = loop {
        if p.semis() {
            if let Some((.., ended)) = &mut last {
                *ended = true;
            }
        }
        if p.at_eof() {
            break match last {
                Some((at, _, _, true)) => {
                    whole.push(at);
                    Rest::Nothing
                }
                Some((at, s, ..)) => Rest::Last(at, Box::new(s)),
                None => Rest::Nothing,
            };
        }
        let (start, first) = (p.pos(), p.i);
        p.cut = false;
        match p.statement() {
            Ok(s) => whole.extend(last.replace((start, s, p.cut, false)).map(|(at, ..)| at)),
            Err(error) => {
                let mut end = p.at_eof() || p.cut;
                let begins = (p.i == first).then_some(start);
                let at = match (begins, last.take()) {
                    (Some(_), Some((at, _, cut, _))) => {
                        end |= cut;
                        at
                    }
                    (_, before) => {
                        whole.extend(before.map(|(at, ..)| at));
                        start
                    }
                };
                break Rest::Refused(Refusal {
                    at,
                    begins,
                    place: p.pos(),
                    error,
                    end,
                });
            }
        }
    };
    // The places in bytes, which a caller slices the text by: walked once,
    // in order, a character counted at each byte that begins one.
    let b = src.as_bytes();
    let (mut chars, mut to) = (0usize, 0usize);
    let mut bytes = |at: usize| -> usize {
        while to < b.len() && (chars < at || b[to] & 0xC0 == 0x80) {
            chars += usize::from(b[to] & 0xC0 != 0x80);
            to += 1;
        }
        to
    };
    let whole: Vec<usize> = whole.into_iter().map(&mut bytes).collect();
    let rest = match rest {
        Rest::Nothing => Rest::Nothing,
        Rest::Last(at, s) => Rest::Last(bytes(at), s),
        Rest::Refused(r) => {
            let at = bytes(r.at);
            let begins = r.begins.map(&mut bytes);
            let place = bytes(r.place);
            Rest::Refused(Refusal {
                at,
                begins,
                place,
                ..r
            })
        }
    };
    let next = match &rest {
        Rest::Nothing => src.len(),
        Rest::Last(at, _) => *at,
        Rest::Refused(r) => r.at,
    };
    let ends = whole.iter().skip(1).copied().chain([next]);
    Reading {
        whole: whole.iter().copied().zip(ends).collect(),
        rest,
    }
}

/// Parses a single statement; errors when there is more than one.
pub fn parse_one(src: &str) -> Result<Statement> {
    one(parse(src)?)
}

fn one(mut s: Vec<Statement>) -> Result<Statement> {
    if s.len() != 1 {
        return Err(Error::Query(format!(
            "expected a single statement, found {}",
            s.len()
        )));
    }
    Ok(s.remove(0))
}

/// A select list as `get ... select` reads one.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SelectList {
    /// The columns, a mark's and a computed one's under its name; `None`
    /// for `*`.
    pub project: Option<Vec<String>>,
    /// Every item, when the list has an aggregate: then there are no columns.
    pub aggregate: Vec<Column>,
    /// `highlight()` and `snippet()`, in the order written.
    pub marks: Vec<Mark>,
    /// The items worked out over each row, `px * qty as notional`.
    pub computed: Vec<Column>,
}

/// A select list on its own -- `status, sum(total), count(*)` -- as `get
/// ... select` reads one: the fields, or the aggregates when it has any.
/// For a caller that assembles a select from parts, such as a query string.
/// A `;` after it reads as it did while a `;` was white space: a query
/// string's list ends no statement (and so do `group=` and `facet=`'s).
pub fn parse_select_list(src: &str) -> Result<SelectList> {
    let mut p = Parser {
        toks: tokenize(src)?,
        i: 0,
        depth: 0,
        subqueries: 0,
        exact: false,
        cut: false,
    };
    let list = p.select_list()?;
    if !p.ended() {
        return p.err("a select list ends where the text does");
    }
    Ok(list)
}

/// A group list on its own -- `symbol, bucket(at, 1m)` -- as `get ...
/// group` reads one, for a query string's `group=`.
pub fn parse_group_list(src: &str) -> Result<Vec<Expr>> {
    let mut p = Parser {
        toks: tokenize(src)?,
        i: 0,
        depth: 0,
        subqueries: 0,
        exact: false,
        cut: false,
    };
    let list = p.group_list()?;
    if !p.ended() {
        return p.err("a group list ends where the text does");
    }
    Ok(list)
}

/// A facet list on its own -- `brand top 5, color` -- as `get ... facet`
/// reads one, for a query string's `facet=`.
pub fn parse_facet_list(src: &str) -> Result<Vec<Facet>> {
    let mut p = Parser {
        toks: tokenize(src)?,
        i: 0,
        depth: 0,
        subqueries: 0,
        exact: false,
        cut: false,
    };
    let list = p.facet_list()?;
    if !p.ended() {
        return p.err("a facet list ends where the text does");
    }
    Ok(list)
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.i].tok
    }
    fn at_eof(&self) -> bool {
        matches!(self.peek(), Tok::Eof)
    }
    /// Steps past the `;`s here, and says whether there were any.
    fn semis(&mut self) -> bool {
        let at = self.i;
        while matches!(self.peek(), Tok::Semi) {
            self.next();
        }
        self.i > at
    }
    /// Whether the statements are over: nothing but `;`s is left.
    fn ended(&mut self) -> bool {
        self.semis();
        self.at_eof()
    }
    fn next(&mut self) -> Tok {
        let t = self.toks[self.i].tok.clone();
        if !matches!(t, Tok::Eof) {
            self.i += 1;
        }
        t
    }
    fn pos(&self) -> usize {
        self.toks[self.i].pos
    }

    fn err<T>(&self, msg: impl std::fmt::Display) -> Result<T> {
        Err(self.error(&msg))
    }

    /// What [`Self::err`] answers with, once: generic over its message and
    /// what it answers, `err` was ten copies of the formatting.
    #[inline(never)]
    fn error(&self, msg: &dyn std::fmt::Display) -> Error {
        Error::Query(format!("position {}: {msg}", self.pos()))
    }

    fn expect(&mut self, t: Tok) -> Result<()> {
        if self.peek() == &t {
            self.next();
            Ok(())
        } else {
            self.err(format!(
                "expected {}, found {}",
                t.describe(),
                self.peek().describe()
            ))
        }
    }

    /// Consumes a keyword (case insensitive).
    fn eat_kw(&mut self, kw: &str) -> bool {
        if let Tok::Ident(s) = self.peek() {
            if s.eq_ignore_ascii_case(kw) {
                self.next();
                return true;
            }
        }
        false
    }

    fn peek_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Ident(s) if s.eq_ignore_ascii_case(kw))
    }

    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            self.err(format!("expected `{kw}`, found {}", self.peek().describe()))
        }
    }

    /// At a `[`: the numbers up to the `]` that closes it as `f32`s, and the
    /// tokens stepped past, when there are any and each is followed by a
    /// `,` or that `]`; `None`, and nothing stepped past, otherwise.
    fn numbers_in_brackets(&mut self) -> Option<Vec<f32>> {
        let mut j = self.i + 1;
        let mut v = Vec::new();
        loop {
            v.push(match self.toks.get(j)?.tok {
                Tok::Int(n) => n as f32,
                Tok::Float(f) => f as f32,
                _ => return None,
            });
            match self.toks.get(j + 1)?.tok {
                Tok::Comma => j += 2,
                Tok::RBracket => {
                    self.i = j + 2;
                    return Some(v);
                }
                _ => return None,
            }
        }
    }

    /// A name: a collection's, a field's where one is declared or named
    /// alone. A path is no name, and is refused rather than taken for one.
    fn ident(&mut self) -> Result<String> {
        match self.next() {
            Tok::Ident(s) if s.contains('.') => {
                self.i -= 1;
                self.err(format!("`{s}` is a path; a name holds no dot"))
            }
            Tok::Ident(s) => Ok(s),
            other => self.err(format!("expected a name, found {}", other.describe())),
        }
    }

    /// A field, or a path into a json field (`meta.source.rank`): what a
    /// `select`, an `order` and a `create index` take.
    fn path(&mut self) -> Result<String> {
        match self.next() {
            Tok::Ident(s) => Ok(s),
            other => self.err(format!("expected a name, found {}", other.describe())),
        }
    }

    fn int(&mut self) -> Result<i64> {
        match self.next() {
            Tok::Int(i) => Ok(i),
            other => self.err(format!("expected an integer, found {}", other.describe())),
        }
    }

    /// A `limit`, an `offset` or a facet's `top`: a whole number from 0, or
    /// a parameter, `limit $2`, read as the statement runs -- so a client
    /// paging through a list sends one text whatever the page. Read as an
    /// operand, which folds `-1` and takes `"x"`, so a number written is
    /// refused in the words a parameter's value is (`count_refused`).
    fn count(&mut self, what: &str) -> Result<Count> {
        let mark = self.depth;
        let e = self.operand();
        self.depth = mark;
        let v = match e? {
            Expr::Param(i) => return Ok(Count::Param(i)),
            Expr::Lit(v) => v,
            _ => {
                return self.err(format!(
                    "`{what}` takes a whole number from 0, or a parameter"
                ))
            }
        };
        match as_count(&v) {
            Some(n) => Ok(Count::N(n)),
            None => self.err(count_refused(what, &v)),
        }
    }

    /// A number that may be written with a decimal point. Only the BM25 knobs
    /// need it -- `k1 = 0.9` reads the way the literature writes it, and the
    /// schema stores it in hundredths.
    fn number(&mut self) -> Result<f64> {
        match self.next() {
            Tok::Int(i) => Ok(i as f64),
            Tok::Float(f) => Ok(f),
            other => self.err(format!("expected a number, found {}", other.describe())),
        }
    }

    // ---------------------------------------------------------- statements

    fn statement(&mut self) -> Result<Statement> {
        let kw = match self.peek() {
            Tok::Ident(s) => s.to_ascii_lowercase(),
            other => return self.err(format!("expected a command, found {}", other.describe())),
        };
        match kw.as_str() {
            "create" => self.create(),
            "drop" => self.drop(),
            "alter" => self.alter(),
            "put" | "insert" => self.put(),
            "get" | "select" => self.get(),
            "explain" => {
                self.next();
                let Statement::Select(sel) = self.statement()? else {
                    return self.err("`explain` takes a `get` or `select`");
                };
                Ok(Statement::Explain(sel))
            }
            "set" | "update" => self.set(),
            "del" | "delete" => self.del(),
            "collections" => {
                self.next();
                Ok(Statement::ListCollections)
            }
            "describe" => {
                self.next();
                Ok(Statement::Describe(self.ident()?))
            }
            "compact" => {
                self.next();
                let which = match self.peek() {
                    Tok::Ident(_) => Some(self.ident()?),
                    _ => None,
                };
                Ok(Statement::Compact(which))
            }
            other => self.err(format!("unknown command `{other}`")),
        }
    }

    fn create(&mut self) -> Result<Statement> {
        self.expect_kw("create")?;
        if self.peek_kw("index") {
            return self.create_index();
        }
        self.expect_kw("collection")?;
        let mut if_not_exists = false;
        if self.eat_kw("if") {
            self.expect_kw("not")?;
            self.expect_kw("exists")?;
            if_not_exists = true;
        }
        let name = self.ident()?;
        self.expect(Tok::LParen)?;
        let mut fields = Vec::new();
        let mut composites = Vec::new();
        loop {
            if self.peek() == &Tok::RParen {
                break;
            }
            // `@sorted(owner, created desc)`: an index over several fields,
            // a line of the collection's own among its fields.
            match self.peek() {
                Tok::At => composites.push(self.composite_line()?),
                _ => fields.push(self.field_def()?),
            }
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        self.expect(Tok::RParen)?;
        if fields.is_empty() {
            return self.err("a collection must contain at least one field");
        }
        let mut schema = Schema::new(name, fields)?;
        for c in composites {
            schema.add_composite(c)?;
        }
        Ok(Statement::CreateCollection {
            schema,
            if_not_exists,
        })
    }

    /// `@sorted(owner, created desc)` or `@unique(org, slug)` in a `create
    /// collection`.
    fn composite_line(&mut self) -> Result<Composite> {
        self.expect(Tok::At)?;
        let unique = self.composite_kind()?;
        let keys = self.index_keys()?;
        Ok(Composite { keys, unique })
    }

    /// The kind of an index over several fields: `false` for `@sorted`,
    /// `true` for `@unique`, the `@` already read.
    fn composite_kind(&mut self) -> Result<bool> {
        match self.ident()?.to_ascii_lowercase().as_str() {
            "sorted" => Ok(false),
            "unique" => Ok(true),
            other => self.err(format!(
                "an index over several fields is @sorted or @unique, not @{other}: its keys \
                 are ordered part by part"
            )),
        }
    }

    /// `(owner, created desc)`: an index's fields -- or paths into json
    /// ones -- each ascending unless `desc` follows it.
    fn index_keys(&mut self) -> Result<Vec<(String, bool)>> {
        self.expect(Tok::LParen)?;
        let mut keys = Vec::new();
        loop {
            let k = self.path()?;
            let desc = match self.eat_kw("desc") {
                true => true,
                false => {
                    self.eat_kw("asc");
                    false
                }
            };
            keys.push((k, desc));
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        self.expect(Tok::RParen)?;
        Ok(keys)
    }

    /// `create index [if not exists] on <collection> (<field>) @hnsw(...)`
    fn create_index(&mut self) -> Result<Statement> {
        self.expect_kw("index")?;
        let mut if_not_exists = false;
        if self.eat_kw("if") {
            self.expect_kw("not")?;
            self.expect_kw("exists")?;
            if_not_exists = true;
        }
        self.expect_kw("on")?;
        let collection = self.ident()?;
        let mut keys = self.index_keys()?;
        self.expect(Tok::At)?;
        // Several fields: an index over them, ordered part by part.
        if keys.len() > 1 {
            let unique = self.composite_kind()?;
            return Ok(Statement::CreateComposite {
                collection,
                index: Composite { keys, unique },
                if_not_exists,
            });
        }
        let (field, desc) = keys.remove(0);
        if desc {
            return self.err(format!(
                "`{field} desc`: an index of one field is walked both ways, and `desc` orders \
                 a part of an index over several"
            ));
        }
        let kind = match self.ident()?.to_ascii_lowercase().as_str() {
            "hash" => IndexKind::HASH,
            "unique" => IndexKind::UNIQUE,
            "sorted" => IndexKind::SORTED,
            "ttl" => IndexKind::Sorted {
                ttl: Some(self.ttl_args()?),
            },
            "hnsw" | "vector" => IndexKind::Vector(self.hnsw_args()?),
            "text" | "bm25" => IndexKind::Text(self.text_args()?),
            "inverted" => IndexKind::Inverted,
            "geo" => IndexKind::Geo,
            other => return self.err(format!("unknown index `{other}`")),
        };
        Ok(Statement::CreateIndex {
            collection,
            field,
            kind,
            if_not_exists,
        })
    }

    fn field_def(&mut self) -> Result<Field> {
        let name = self.ident()?;
        let ty = self.data_type()?;
        let mut field = Field::new(name, ty.clone());
        loop {
            if self.eat_kw("required") {
                field = field.required();
                continue;
            }
            // `name text collate tr`: the field's text orders in its
            // collation wherever it is compared, as SQL's column collation
            // does.
            if let Some(c) = self.collate()? {
                field = field.collated(c);
                continue;
            }
            // `email text nocase`: compared by its case fold wherever it is
            // compared, as PostgreSQL's `citext` is, and kept as written.
            if self.eat_kw("nocase") {
                field = field.nocase();
                continue;
            }
            if matches!(self.peek(), Tok::At) {
                self.next();
                let kind = self.ident()?.to_ascii_lowercase();
                match kind.as_str() {
                    "hash" => field = field.indexed(IndexKind::HASH),
                    "unique" => field = field.indexed(IndexKind::UNIQUE),
                    "sorted" => field = field.indexed(IndexKind::SORTED),
                    // An ordered index whose rows expire: the sweep walks
                    // it rather than scan for them.
                    "ttl" => {
                        let ttl = Some(self.ttl_args()?);
                        field = field.indexed(IndexKind::Sorted { ttl });
                    }
                    "hnsw" | "vector" => {
                        let spec = self.hnsw_args()?;
                        field = field.indexed(IndexKind::Vector(spec));
                    }
                    "text" | "bm25" => {
                        let spec = self.text_args()?;
                        field = field.indexed(IndexKind::Text(spec));
                    }
                    "inverted" => field = field.indexed(IndexKind::Inverted),
                    "geo" => field = field.indexed(IndexKind::Geo),
                    other => return self.err(format!("unknown index `{other}`")),
                }
                continue;
            }
            break;
        }
        Ok(field)
    }

    /// `(30m)`: how long a row lives past its timestamp, in milliseconds --
    /// a whole number of milliseconds, seconds, minutes, hours or days.
    /// FenecQL had no duration of its own to follow.
    fn ttl_args(&mut self) -> Result<u64> {
        self.expect(Tok::LParen)?;
        let n = self.int()?;
        let unit = match self.peek() {
            Tok::Ident(u) => u.to_ascii_lowercase(),
            other => {
                return self.err(format!(
                    "@ttl takes a duration such as 30s, 30m, 12h or 7d, not {}",
                    other.describe()
                ))
            }
        };
        let ms: u64 = match unit.as_str() {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            other => return self.err(format!("unknown unit `{other}` in @ttl: ms, s, m, h or d")),
        };
        self.next();
        self.expect(Tok::RParen)?;
        match u64::try_from(n)
            .ok()
            .filter(|&n| n > 0)
            .and_then(|n| n.checked_mul(ms))
        {
            Some(ttl) if ttl <= i64::MAX as u64 => Ok(ttl),
            _ => self.err(format!("@ttl takes a duration past zero, not {n}{unit}")),
        }
    }

    fn text_args(&mut self) -> Result<TextIndexSpec> {
        let mut spec = TextIndexSpec::default();
        if !matches!(self.peek(), Tok::LParen) {
            return Ok(spec);
        }
        self.next();
        loop {
            if matches!(self.peek(), Tok::RParen) {
                break;
            }
            let key = self.ident()?;
            let lowered = key.to_ascii_lowercase();
            // `chars` is a flag: each character of a script written without
            // spaces indexed as well as each pair.
            if lowered == "chars" {
                spec.chars = true;
                if !matches!(self.peek(), Tok::Comma) {
                    break;
                }
                self.next();
                continue;
            }
            self.expect(Tok::Eq)?;
            let v = self.number()?;
            match lowered.as_str() {
                "k1" | "b" => {
                    if !(0.0..=100.0).contains(&v) {
                        let mut got = String::new();
                        fenec_core::num::f64_into(&mut got, v);
                        return self.err(format!("`{key}` must be between 0 and 100, got {got}"));
                    }
                    let pct = (v * 100.0).round() as u16;
                    if lowered == "k1" {
                        spec.k1_pct = pct;
                    } else {
                        spec.b_pct = pct;
                    }
                }
                // `prefix` is the longest prefix indexed; 0 is off.
                "prefix" | "prefix_max" | "prefix_min" => {
                    if !(0.0..=64.0).contains(&v) || v.fract() != 0.0 {
                        let mut got = String::new();
                        fenec_core::num::f64_into(&mut got, v);
                        return self
                            .err(format!("`{key}` must be a whole number 0..64, got {got}"));
                    }
                    if lowered == "prefix_min" {
                        spec.prefix_min = v as u8;
                    } else {
                        spec.prefix_max = v as u8;
                    }
                }
                other => return self.err(format!("unknown text parameter `{other}`")),
            }
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        self.expect(Tok::RParen)?;
        Ok(spec)
    }

    fn hnsw_args(&mut self) -> Result<VectorIndexSpec> {
        let mut spec = VectorIndexSpec::default();
        if !matches!(self.peek(), Tok::LParen) {
            return Ok(spec.resolved());
        }
        self.next();
        loop {
            if matches!(self.peek(), Tok::RParen) {
                break;
            }
            let key = self.ident()?;
            if matches!(self.peek(), Tok::Eq) {
                self.next();
                if key.eq_ignore_ascii_case("quant") {
                    let q = self.ident()?;
                    match Quant::parse(&q) {
                        Some(q) => spec.quant = q,
                        None => {
                            return self
                                .err(format!("unknown quantization `{q}`: int8, bit or none"))
                        }
                    }
                } else {
                    let v = self.int()? as usize;
                    match key.to_ascii_lowercase().as_str() {
                        "m" => spec.m = v.max(2),
                        "ef_construction" | "ef_c" => spec.ef_construction = v.max(8),
                        "ef_search" | "ef" => spec.ef_search = v.max(1),
                        other => return self.err(format!("unknown hnsw parameter `{other}`")),
                    }
                }
            } else {
                // positional: the metric name
                match Metric::parse(&key) {
                    Some(m) => spec.metric = m,
                    None => return self.err(format!("unknown metric `{key}`")),
                }
            }
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        self.expect(Tok::RParen)?;
        Ok(spec.resolved())
    }

    fn data_type(&mut self) -> Result<DataType> {
        // [type] -> list
        if matches!(self.peek(), Tok::LBracket) {
            self.next();
            let inner = self.data_type()?;
            self.expect(Tok::RBracket)?;
            return Ok(DataType::List(Box::new(inner)));
        }
        let name = self.ident()?.to_ascii_lowercase();
        Ok(match name.as_str() {
            "bool" => DataType::Bool,
            "int" => DataType::Int,
            "float" => DataType::Float,
            "text" | "string" => DataType::Text,
            "bytes" | "blob" => DataType::Bytes,
            "timestamp" | "timestamptz" => DataType::Timestamp,
            "json" | "jsonb" => DataType::Json,
            "geo" => DataType::Geo,
            "vector" | "vec" => {
                self.expect(Tok::Lt)?;
                let dim = self.int()?;
                if dim <= 0 {
                    return self.err("the vector dimension must be positive");
                }
                // Optional storage precision: `vector<768, f16>`.
                let prec = if matches!(self.peek(), Tok::Comma) {
                    self.next();
                    match self.ident()?.to_ascii_lowercase().as_str() {
                        "f32" => VecPrec::F32,
                        "f16" => VecPrec::F16,
                        other => {
                            return self.err(format!(
                                "vector precision must be `f32` or `f16`, found `{other}`"
                            ))
                        }
                    }
                } else {
                    VecPrec::F32
                };
                self.expect(Tok::Gt)?;
                DataType::Vector(dim as usize, prec)
            }
            // `sparse<30522>`: a vector of that many dimensions of which only
            // the non-zero entries are kept -- a SPLADE vocabulary's size.
            "sparse" | "sparsevec" => {
                self.expect(Tok::Lt)?;
                let dim = self.int()?;
                if dim <= 0 || dim as u64 > fenec_core::sparse::MAX_DIM as u64 {
                    return self.err(format!(
                        "a sparse vector's dimension is 1 to {}",
                        fenec_core::sparse::MAX_DIM
                    ));
                }
                self.expect(Tok::Gt)?;
                DataType::Sparse(dim as usize)
            }
            other => return self.err(format!("unknown type `{other}`")),
        })
    }

    fn drop(&mut self) -> Result<Statement> {
        self.expect_kw("drop")?;
        // `drop index [if exists] on posts (owner, created)`: an index over
        // several fields, named by its fields.
        if self.eat_kw("index") {
            let mut if_exists = false;
            if self.eat_kw("if") {
                self.expect_kw("exists")?;
                if_exists = true;
            }
            self.expect_kw("on")?;
            let collection = self.ident()?;
            let keys = self.index_keys()?;
            if keys.len() < 2 {
                return self.err(
                    "`drop index` takes an index over several fields: a field's own index goes \
                     with a rebuild of the field",
                );
            }
            return Ok(Statement::DropComposite {
                collection,
                keys: keys.into_iter().map(|(k, _)| k).collect(),
                if_exists,
            });
        }
        self.expect_kw("collection")?;
        let mut if_exists = false;
        if self.eat_kw("if") {
            self.expect_kw("exists")?;
            if_exists = true;
        }
        Ok(Statement::DropCollection {
            name: self.ident()?,
            if_exists,
        })
    }

    /// `alter collection <name> add [field] <field> <type> ... | drop
    /// [field] <field> | rename [field] <field> to <name>`.
    fn alter(&mut self) -> Result<Statement> {
        self.expect_kw("alter")?;
        self.expect_kw("collection")?;
        let collection = self.ident()?;
        let change = match self.ident()?.to_ascii_lowercase().as_str() {
            "add" => {
                self.eat_kw("field");
                Alter::AddField(self.field_def()?)
            }
            "drop" => {
                self.eat_kw("field");
                Alter::DropField(self.ident()?)
            }
            "rename" => {
                self.eat_kw("field");
                let from = self.ident()?;
                self.expect_kw("to")?;
                Alter::RenameField(from, self.ident()?)
            }
            // A field's expiry, set or taken off: `@ttl(..)`, or `@sorted`
            // for none. Its ordered index stays.
            "alter"
                if {
                    self.eat_kw("field");
                    let next = self.toks.get(self.i + 1).map(|t| &t.tok);
                    let at = matches!(next, Some(Tok::At));
                    self.cut |= matches!(next, Some(Tok::Eof));
                    at
                } =>
            {
                let field = self.ident()?;
                self.expect(Tok::At)?;
                match self.ident()?.to_ascii_lowercase().as_str() {
                    "ttl" => Alter::Ttl(field, Some(self.ttl_args()?)),
                    "sorted" => Alter::Ttl(field, None),
                    other => {
                        return self.err(format!(
                            "`alter field` sets @ttl(..) or @sorted, not @{other}: an index \
                             is made by `create index`"
                        ))
                    }
                }
            }
            // Every document would be rewritten under the write lock, which
            // the changes `alter` makes are chosen not to need.
            "alter" | "modify" | "set" | "change" | "retype" => {
                return self.err(
                    "a field's type does not change in place: add a field of the new type, \
                     `set` it from the old one, and drop the old one",
                )
            }
            other => {
                return self.err(format!(
                    "`alter collection` takes `add field`, `drop field` or `rename field`, \
                     not `{other}`"
                ))
            }
        };
        Ok(Statement::AlterCollection { collection, change })
    }

    fn put(&mut self) -> Result<Statement> {
        let insert = matches!(self.next(), Tok::Ident(w) if w.eq_ignore_ascii_case("insert"));
        self.eat_kw("into");
        let collection = self.ident()?;
        let mut docs = Vec::new();
        let mut docs_param = None;
        if let Tok::Param(i) = self.peek() {
            // `put events $1`: the documents are the parameter's, an object
            // or a list of them, so a page of any size is one text.
            docs_param = Some(*i);
            self.next();
        } else if matches!(self.peek(), Tok::LBracket) {
            self.next();
            loop {
                if matches!(self.peek(), Tok::RBracket) {
                    break;
                }
                docs.push(self.object()?);
                if !matches!(self.peek(), Tok::Comma) {
                    break;
                }
                self.next();
            }
            self.expect(Tok::RBracket)?;
        } else {
            docs.push(self.object()?);
            // consecutive objects are accepted too: put docs {..} {..}
            while matches!(self.peek(), Tok::LBrace) {
                docs.push(self.object()?);
            }
        }
        if docs.is_empty() && docs_param.is_none() {
            return self.err("put expects at least one document");
        }
        // `if absent`: a document whose id or `@unique` value is held is
        // passed over, and the answer counts what was written. `else set
        // {..}` sets the row holding it instead -- an upsert, `new.f`
        // reading the document's `f`.
        let if_absent = self.eat_kw("if");
        let mut else_set = None;
        if if_absent {
            self.expect_kw("absent")?;
            if self.eat_kw("else") {
                self.expect_kw("set")?;
                else_set = Some(self.object()?);
            }
        }
        // `require` and `returning`, either first.
        let (mut require, mut pick) = (None, None);
        loop {
            if self.peek_kw("require") {
                require = self.require()?;
            } else if self.eat_kw("returning") {
                pick = Some(Box::new(Pick {
                    returning: Some(self.projection_list()?),
                    ..Pick::default()
                }));
            } else {
                break;
            }
        }
        Ok(Statement::Put {
            collection,
            docs,
            docs_param,
            insert: insert || if_absent,
            if_absent,
            else_set,
            require,
            pick,
        })
    }

    /// `require <n>` after a write: it must write exactly `n` rows, or it is
    /// refused and its block put back. A clause of the write rather than an
    /// `assert` statement of its own: the count is the write's, known under
    /// the lock that wrote it, with no second read to race or to scope.
    fn require(&mut self) -> Result<Option<u64>> {
        if !self.eat_kw("require") {
            return Ok(None);
        }
        match self.int()? {
            n if n >= 0 => Ok(Some(n as u64)),
            n => self.err(format!("`require` takes a count of rows, not {n}")),
        }
    }

    /// `{k: v, ...}`: a document's fields, a `set`'s, or an object's
    /// members. A key may be a path (`meta.lang: "en"`), which sets a key
    /// inside a json field.
    fn object(&mut self) -> Result<Vec<(String, Expr)>> {
        self.expect(Tok::LBrace)?;
        let mut pairs = Vec::new();
        loop {
            if matches!(self.peek(), Tok::RBrace) {
                break;
            }
            let key = match self.next() {
                Tok::Ident(s) => s,
                Tok::Str(s) => s,
                other => {
                    return self.err(format!("expected a field name, found {}", other.describe()))
                }
            };
            self.expect(Tok::Colon)?;
            pairs.push((key, self.expr()?));
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        self.expect(Tok::RBrace)?;
        Ok(pairs)
    }

    fn get(&mut self) -> Result<Statement> {
        self.next(); // get / select
                     // The classic SQL order: `select title, year from articles`. fenecdb's
                     // own order is valid in the same position (`get articles select
                     // title`), so the decision is made by backtracking: if the name list
                     // is followed by `from` it is a projection, otherwise the first name
                     // is the collection.
        let mut list = SelectList::default();
        if !self.peek_kw("from") {
            let save = self.i;
            match self.projection_before_from() {
                Some(l) => list = l,
                None => self.i = save,
            }
        }
        self.eat_kw("from");
        let collection = self.ident()?;
        let mut sel = Select {
            collection,
            project: list.project,
            aggregate: list.aggregate,
            marks: list.marks,
            computed: list.computed,
            ..Default::default()
        };

        loop {
            if self.eat_kw("select") {
                let l = self.select_list()?;
                (sel.project, sel.aggregate, sel.marks) = (l.project, l.aggregate, l.marks);
                sel.computed = l.computed;
                continue;
            }
            if self.eat_kw("facet") {
                sel.facets = self.facet_list()?;
                continue;
            }
            if self.eat_kw("group") {
                sel.group = self.group_list()?;
                continue;
            }
            if self.eat_kw("having") {
                sel.having = Some(self.expr()?);
                continue;
            }
            if self.eat_kw("where") {
                sel.filter = Some(self.expr()?);
                continue;
            }
            if self.eat_kw("near") {
                let field = self.ident()?;
                let vector = self.expr()?;
                let mut ef = None;
                let mut exact = false;
                loop {
                    if self.eat_kw("ef") {
                        ef = Some(self.int()?.max(1) as usize);
                        continue;
                    }
                    if self.eat_kw("exact") {
                        exact = true;
                        continue;
                    }
                    break;
                }
                sel.near = Some(Near {
                    field,
                    vector,
                    ef,
                    exact,
                });
                continue;
            }
            if self.eat_kw("match") {
                let field = self.ident()?;
                let query = self.expr()?;
                // `fuzzy [N]` and `prefix`, in either order: neither can
                // continue the text's expression, so where it ends is not
                // in doubt.
                let (mut fuzzy, mut prefix) = (0, false);
                loop {
                    if self.eat_kw("fuzzy") {
                        fuzzy = MAX_FUZZY;
                        if matches!(self.peek(), Tok::Int(_) | Tok::Minus) {
                            match self.int() {
                                Ok(n @ 0..=2) => fuzzy = n as u8,
                                _ => return self.err(FUZZY_RANGE),
                            }
                        }
                        continue;
                    }
                    if self.eat_kw("prefix") {
                        prefix = true;
                        continue;
                    }
                    break;
                }
                sel.matcher = Some(Match {
                    field,
                    query,
                    within: false,
                    fuzzy,
                    prefix,
                });
                continue;
            }
            if self.eat_kw("fuse") {
                let mut f = Fuse {
                    k: None,
                    candidates: None,
                };
                loop {
                    if self.eat_kw("k") {
                        f.k = Some(self.int()?.clamp(0, u32::MAX as i64) as u32);
                        continue;
                    }
                    if self.eat_kw("candidates") {
                        f.candidates = Some(self.int()?.max(0) as usize);
                        continue;
                    }
                    break;
                }
                sel.fuse = Some(f);
                continue;
            }
            if self.eat_kw("rerank") {
                let field = self.ident()?;
                let vector = self.expr()?;
                let mut candidates = None;
                if self.eat_kw("candidates") {
                    candidates = Some(self.int()?.max(1) as usize);
                }
                sel.rerank = Some(Rerank {
                    field,
                    vector,
                    candidates,
                });
                continue;
            }
            if self.eat_kw("order") {
                self.order_list(&mut sel.order)?;
                continue;
            }
            if self.eat_kw("count") {
                sel.count = true;
                continue;
            }
            if self.eat_kw("limit") {
                sel.limit = Some(self.count("limit")?);
                continue;
            }
            if self.eat_kw("offset") {
                sel.offset = self.count("offset")?;
                continue;
            }
            // `require <n>`: the rows answered must number `n` -- before a
            // `lookup`, whose clauses are the children's.
            if self.peek_kw("require") {
                sel.require = self.require()?;
                continue;
            }
            // `lookup` is terminal: every clause after it binds to the child.
            // Scoping by position is what keeps qualified names out of the
            // language -- `where` means on either side exactly what it always
            // meant, and neither side needs a prefix to say which it is. The
            // cost is that a parent clause cannot follow the child ones, which
            // reads the way the query runs anyway.
            if self.eat_kw("lookup") {
                sel.lookup = Some(self.lookup_clause(1)?);
                break;
            }
            break;
        }
        // The semantic check also runs here so the error carries a position.
        // The engine repeats the same check, because plan structures can be
        // built independently of FenecQL.
        if let Err(e) = sel.check() {
            return self.err(e);
        }
        // Split here, from the filter as written: a token's rules and an
        // expiry are ANDed in later, into the split filters too.
        if let Err(e) = sel.split_facets() {
            return self.err(e);
        }
        Ok(Statement::Select(sel))
    }

    /// Tries the "name list + `from`" pattern after `select`.
    ///
    /// An outer `Some` means "that was a projection and `from` was consumed";
    /// on `None` the caller rewinds the position. An inner `None` means
    /// `select *`: all fields.
    /// `select a, b` or `select *`. `None` means every field.
    fn projection_list(&mut self) -> Result<Option<Vec<String>>> {
        if matches!(self.peek(), Tok::Star) {
            self.next();
            return Ok(None);
        }
        let mut cols = Vec::new();
        loop {
            cols.push(self.path()?);
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        Ok(Some(cols))
    }

    /// `order year desc, title asc` -- keys in priority order. Over groups a
    /// key may be an aggregate, `order sum(total) desc`, named by its column.
    /// Any other expression is a key worked out of each row: `order
    /// distance(loc, $1)`, `order price * qty desc`.
    fn order_list(&mut self, out: &mut Vec<Sort>) -> Result<()> {
        self.eat_kw("by");
        loop {
            // A field, or an aggregate of one, by the name it answers
            // under -- what a key was before it could be an expression.
            let (field, expr) = match self.expr()? {
                Expr::Field(f) => (f, None),
                e @ Expr::Call(..) if e.aggregate_name().is_some() => match label_of(&e) {
                    Some(label) => (label, None),
                    None => (String::new(), Some(e)),
                },
                e => (String::new(), Some(e)),
            };
            // `collate` comes before the direction, as in SQL; written
            // after it, it reads just as well and is taken there too.
            let mut collate = self.collate()?;
            let asc = if self.eat_kw("desc") {
                false
            } else {
                self.eat_kw("asc");
                true
            };
            if collate.is_none() {
                collate = self.collate()?;
            }
            out.push(Sort {
                field,
                asc,
                collate,
                expr,
            });
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        Ok(())
    }

    /// `collate <name>`, if it comes next.
    fn collate(&mut self) -> Result<Option<Collation>> {
        if !self.eat_kw("collate") {
            return Ok(None);
        }
        let name = self.ident()?;
        match Collation::named(&name) {
            Some(c) => Ok(Some(c)),
            None => self.err(format!(
                "unknown collation `{name}`: there are `und`, Unicode's order for every language, and `tr`"
            )),
        }
    }

    /// `lookup <name> on <child> [= <parent>]`, or `on (<child>, ...) =
    /// (<parent>, ...)` for a key over several fields, and the clauses that
    /// follow, all of which belong to the child collection.
    ///
    /// Written without the second half of `on`, the parent key is `id`: that
    /// is the foreign-key-to-primary-key shape, which is most of them, and
    /// spelling it out every time would be noise.
    ///
    /// A second `lookup` inside the first chains it -- `products lookup
    /// reviews on product_id lookup authors on id = author_id`. Position
    /// keeps scoping the same way, one level deeper: what follows a `lookup`
    /// belongs to *its* collection, and `on child = parent` names a field of
    /// the level immediately above. Exactly one collection is in scope at
    /// any point in the query, and it is the last one named, which is what
    /// keeps qualified names out of the language at any depth.
    ///
    /// `depth` is counted here as well as in `Select::check` because this
    /// function recurses: the check would run after a pathological query had
    /// already taken the stack down with it.
    fn lookup_clause(&mut self, depth: usize) -> Result<Lookup> {
        if depth > MAX_LOOKUP_DEPTH {
            return self.err(Error::Query(format!(
                "`lookup` chained too deep: at most {MAX_LOOKUP_DEPTH} levels"
            )));
        }
        let collection = self.ident()?;
        self.expect_kw("on")?;
        // `on (org, order) = (org, id)` is a key over several fields, the
        // parent's side written out, since `id` alone pairs with one child
        // field; `Select::check` holds the two sides to as many.
        let on = self.names()?;
        let parent = if matches!(self.peek(), Tok::Eq) {
            self.next();
            self.names()?
        } else {
            vec!["id".to_string()]
        };
        let mut l = Lookup {
            collection,
            on,
            parent,
            ..Default::default()
        };
        loop {
            // Fields, or a list that aggregates: each parent's children
            // folded, `select count() as n, sum(total) as revenue`.
            if self.eat_kw("select") {
                let at = self.i;
                let list = self.select_list()?;
                if !list.marks.is_empty() || !list.computed.is_empty() {
                    self.i = at;
                    return self.err(
                        "a `lookup` level selects its children's fields, or aggregates of them",
                    );
                }
                (l.project, l.aggregate) = (list.project, list.aggregate);
                continue;
            }
            if self.eat_kw("group") {
                l.group = self.group_list()?;
                continue;
            }
            if self.eat_kw("having") {
                l.having = Some(self.expr()?);
                continue;
            }
            if self.eat_kw("where") {
                l.filter = Some(self.expr()?);
                continue;
            }
            if self.eat_kw("order") {
                self.order_list(&mut l.order)?;
                continue;
            }
            if self.eat_kw("limit") {
                l.limit = Some(self.count("limit")?);
                continue;
            }
            if self.eat_kw("offset") {
                l.offset = self.count("offset")?;
                continue;
            }
            // A bare flag among the keyword clauses, the way `exact` sits
            // inside `near`.
            if self.eat_kw("required") {
                l.required = true;
                continue;
            }
            if self.eat_kw("require") {
                return self
                    .err("`require` counts the rows of the `get`, and goes before its `lookup`");
            }
            // Terminal here for the same reason it is terminal up there: the
            // clauses after it bind to the next collection down, so nothing
            // belonging to this one can follow.
            if self.eat_kw("lookup") {
                l.next = Some(Box::new(self.lookup_clause(depth + 1)?));
                break;
            }
            break;
        }
        Ok(l)
    }

    /// `order`, or `(org, order)`: a name, or names in parentheses. One
    /// call of `ident` for both, which is inlined where it is called.
    fn names(&mut self) -> Result<Vec<String>> {
        let listed = matches!(self.peek(), Tok::LParen);
        if listed {
            self.next();
        }
        let mut out = Vec::new();
        loop {
            out.push(self.ident()?);
            if !listed || !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        if listed {
            self.expect(Tok::RParen)?;
        }
        Ok(out)
    }

    /// `[0, 2500, 5000]`: a facet's range bounds, numbers as written --
    /// literals and never parameters: a sidebar's buckets are its code's,
    /// one text every request, where a page's `limit` and `offset` -- and a
    /// "show more" growing a `top` -- change with each.
    /// A list literal, read as any is: what the bounds are -- numbers,
    /// rising -- `Select::check` says.
    fn bounds(&mut self) -> Result<Vec<Value>> {
        // As written, as `in [..]`'s: not the `f32`s of a vector.
        let was = std::mem::replace(&mut self.exact, true);
        let e = self.primary();
        self.exact = was;
        match e? {
            Expr::Lit(Value::List(v)) => Ok(v),
            _ => self.err("`ranges` takes its bounds in brackets: `ranges [0, 100, 500]`"),
        }
    }

    fn projection_before_from(&mut self) -> Option<SelectList> {
        let list = self.select_list();
        self.cut |= self.at_eof();
        let list = list.ok()?;
        self.eat_kw("from").then_some(list)
    }

    /// `group symbol, bucket(at, 1m)`: the keys, each an expression over the
    /// row or a name the select list gives an item.
    fn group_list(&mut self) -> Result<Vec<Expr>> {
        self.eat_kw("by");
        let mut out = vec![self.expr()?];
        while matches!(self.peek(), Tok::Comma) {
            self.next();
            out.push(self.expr()?);
        }
        Ok(out)
    }

    /// `brand top 5 disjunctive, price ranges [0, 2500, 5000], color`: each
    /// field or path, how many of its commonest values or which ranges of
    /// its numbers, and whether the filter's own conditions on it count.
    fn facet_list(&mut self) -> Result<Vec<Facet>> {
        let mut out = Vec::new();
        loop {
            let mut f = Facet::new(self.path()?);
            // Both read, so the check names what they cannot do together.
            if self.eat_kw("top") {
                f.top = Some(self.count("top")?);
            }
            if self.eat_kw("ranges") {
                f.ranges = Some(self.bounds()?);
            }
            f.disjunctive = self.eat_kw("disjunctive");
            out.push(f);
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        Ok(out)
    }

    /// `highlight(body [, pre, post])` or `snippet(body, words [, ellipsis
    /// [, pre, post]])`, its `(` read: the field, a snippet's words, and
    /// the values -- each a literal or a parameter -- that go around a
    /// mark and stand for what a snippet leaves out.
    /// The arguments' count is `Select::check`'s to judge, with the rest.
    fn mark(&mut self, snippet: bool) -> Result<Mark> {
        let field = self.ident()?;
        let mut words = None;
        if snippet {
            self.expect(Tok::Comma)?;
            words = Some(self.int()?.max(0) as usize);
        }
        let mut args = Vec::new();
        while matches!(self.peek(), Tok::Comma) {
            self.next();
            args.push(self.expr()?);
        }
        self.expect(Tok::RParen)?;
        Ok(Mark {
            field,
            snippet: words,
            args,
        })
    }

    /// A select list: `*`, fields, or -- once any item calls an aggregate
    /// -- an aggregating list, whose items are folded over the rows or are
    /// the group's keys. An item is an expression, named with `as`; a
    /// field, or an aggregate of one, answers under the text it was
    /// written as when it is not named (`label_of`), and anything else
    /// needs `as`. `count` needs its parentheses there: bare, it is a field
    /// of that name. `highlight()` and `snippet()` go among the fields, or
    /// after a `*`.
    fn select_list(&mut self) -> Result<SelectList> {
        let mut out = SelectList::default();
        let star = matches!(self.peek(), Tok::Star);
        if star {
            self.next();
            if !matches!(self.peek(), Tok::Comma) {
                return Ok(out);
            }
            self.next();
        }
        // Each item as written, and where; a mark's is its label, at `MAX`.
        let mut items: Vec<(Expr, Option<String>, usize)> = Vec::new();
        let mut names = Vec::new();
        loop {
            let at = self.pos();
            let mark = match (self.peek(), self.toks.get(self.i + 1).map(|t| &t.tok)) {
                (Tok::Ident(n), Some(Tok::LParen)) => {
                    let low = n.to_ascii_lowercase();
                    matches!(low.as_str(), "highlight" | "snippet").then_some(low)
                }
                _ => None,
            };
            if let Some(low) = mark {
                self.i += 2;
                let mark = self.mark(low == "snippet")?;
                items.push((Expr::Lit(Value::Null), Some(mark.label()), usize::MAX));
                out.marks.push(mark);
            } else if star {
                return self.err("after `*` the list takes `highlight()` and `snippet()` alone");
            } else {
                let e = self.expr()?;
                let name = match self.eat_kw("as") {
                    true => Some(self.ident()?),
                    false => None,
                };
                items.push((e, name, at));
            }
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        let aggregates = items.iter().any(|(e, ..)| e.has_aggregate());
        for (e, name, at) in items {
            if let (Some(label), usize::MAX) = (&name, at) {
                names.push(label.clone());
                continue;
            }
            let plain = matches!((&e, &name), (Expr::Field(_), None));
            let name = match name.or_else(|| label_of(&e)) {
                Some(n) => n,
                None => {
                    self.i = self.toks.iter().position(|t| t.pos >= at).unwrap_or(self.i);
                    return self.err(
                        "an item that is not a field, or an aggregate of one -- count, sum, \
                         avg, min, max, first, last -- answers under a name: `<expr> as <name>`",
                    );
                }
            };
            if aggregates {
                out.aggregate.push(Column { expr: e, name });
                continue;
            }
            names.push(name.clone());
            if !plain {
                out.computed.push(Column { expr: e, name });
            }
        }
        if aggregates {
            return Ok(out);
        }
        if !star {
            out.project = Some(names);
        }
        Ok(out)
    }

    fn set(&mut self) -> Result<Statement> {
        self.next(); // set / update
        let collection = self.ident()?;
        self.eat_kw("set");
        let set = self.object()?;
        let (filter, require, pick) = self.write_clauses()?;
        Ok(Statement::Update {
            collection,
            set,
            filter,
            require,
            pick,
        })
    }

    fn del(&mut self) -> Result<Statement> {
        self.next(); // del / delete
        self.eat_kw("from");
        let collection = self.ident()?;
        let (filter, require, pick) = self.write_clauses()?;
        Ok(Statement::Delete {
            collection,
            filter,
            require,
            pick,
        })
    }

    /// The clauses of a `set` after its object and of a `del` after its
    /// collection, in any order as a `get`'s are: `where`, `require`, and
    /// the `order`, `limit` and `returning` that make it pick its rows and
    /// answer them -- a job queue's claim,
    /// `set jobs {owner: $1, run_at: now() + 30000} where run_at <= now()
    /// order run_at limit 10 returning *`.
    #[allow(clippy::type_complexity)]
    fn write_clauses(&mut self) -> Result<(Option<Expr>, Option<u64>, Option<Box<Pick>>)> {
        let (mut filter, mut require, mut pick) = (None, None, Pick::default());
        loop {
            if self.eat_kw("where") {
                filter = Some(self.expr()?);
                continue;
            }
            if self.eat_kw("order") {
                self.order_list(&mut pick.order)?;
                continue;
            }
            if self.eat_kw("limit") {
                pick.limit = Some(self.count("limit")?);
                continue;
            }
            if self.eat_kw("returning") {
                pick.returning = Some(self.projection_list()?);
                continue;
            }
            if self.peek_kw("require") {
                require = self.require()?;
                continue;
            }
            // `near` ranks a `get`'s rows, scores and all; a write picks
            // its rows by `order`, which takes the distance as a key.
            if self.peek_kw("near") {
                return self.err(
                    "a write picks its rows with `order`: `order distance(<field>, <point>) \
                     limit 1` for the nearest, or `where id in (get ... near <field> <vector> \
                     limit 1)`",
                );
            }
            break;
        }
        let picks = pick.picks() || pick.returning.is_some();
        Ok((filter, require, picks.then(|| Box::new(pick))))
    }

    // -------------------------------------------------------- expression grammar
    // expr    := or
    // or      := and ( "or" and )*
    // and     := not ( "and" not )*
    // not     := "not" not | cmp
    // cmp     := arith ( op arith )?   (arith below)
    // primary := literal | ident | call | "(" expr ")" | list

    /// Errors when the depth counter exceeds `MAX_EXPR_DEPTH`.
    fn deepen(&mut self) -> Result<()> {
        if self.depth >= MAX_EXPR_DEPTH {
            return Err(Error::Query(format!(
                "expression too deep: at most {MAX_EXPR_DEPTH} levels"
            )));
        }
        self.depth += 1;
        Ok(())
    }

    fn expr(&mut self) -> Result<Expr> {
        self.deepen()?;
        let out = self.or_expr();
        self.depth -= 1;
        out
    }

    // `a or b or c` leans left: `Or(Or(a, b), c)`. Because parsing is a loop
    // the stack does not grow here, but the *tree* gets one level deeper on
    // every turn, and evaluation recurses to that depth. Chain length is
    // therefore counted as depth; the counter is given back once the
    // expression ends, since sibling branches do not overlap.
    fn or_expr(&mut self) -> Result<Expr> {
        let mut left = self.and_expr()?;
        let mut spine = 0;
        while self.eat_kw("or") {
            if let Err(e) = self.deepen() {
                self.depth -= spine;
                return Err(e);
            }
            spine += 1;
            let right = match self.and_expr() {
                Ok(r) => r,
                Err(e) => {
                    self.depth -= spine;
                    return Err(e);
                }
            };
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        self.depth -= spine;
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut left = self.not_expr()?;
        let mut spine = 0;
        while self.eat_kw("and") {
            if let Err(e) = self.deepen() {
                self.depth -= spine;
                return Err(e);
            }
            spine += 1;
            let right = match self.not_expr() {
                Ok(r) => r,
                Err(e) => {
                    self.depth -= spine;
                    return Err(e);
                }
            };
            left = Expr::And(Box::new(left), Box::new(right));
        }
        self.depth -= spine;
        Ok(left)
    }

    fn not_expr(&mut self) -> Result<Expr> {
        if self.eat_kw("not") {
            self.deepen()?;
            let out = self.not_expr().map(|e| Expr::Not(Box::new(e)));
            self.depth -= 1;
            return out;
        }
        self.cmp_expr()
    }

    fn cmp_expr(&mut self) -> Result<Expr> {
        let left = self.arith()?;

        let op = match self.peek() {
            Tok::Eq => Some(CmpOp::Eq),
            Tok::Ne => Some(CmpOp::Ne),
            Tok::Lt => Some(CmpOp::Lt),
            Tok::Le => Some(CmpOp::Le),
            Tok::Gt => Some(CmpOp::Gt),
            Tok::Ge => Some(CmpOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.next();
            let right = self.arith()?;
            return Ok(Expr::Cmp(op, Box::new(left), Box::new(right)));
        }
        if matches!(self.peek(), Tok::Tilde) {
            self.next();
            let right = self.arith()?;
            return Ok(Expr::Like(Box::new(left), Box::new(right)));
        }
        if self.peek_kw("has") {
            self.next();
            let right = self.arith()?;
            return Ok(Expr::Has(Box::new(left), Box::new(right)));
        }
        if self.peek_kw("in") {
            self.next();
            if matches!(self.peek(), Tok::LParen) {
                return self.subquery(left);
            }
            self.expect(Tok::LBracket)?;
            let mut items = Vec::new();
            loop {
                if matches!(self.peek(), Tok::RBracket) {
                    break;
                }
                items.push(self.expr()?);
                if !matches!(self.peek(), Tok::Comma) {
                    break;
                }
                self.next();
            }
            self.expect(Tok::RBracket)?;
            return Ok(Expr::In(Box::new(left), items));
        }
        if self.peek_kw("is") {
            self.next();
            let negated = self.eat_kw("not");
            self.expect_kw("null")?;
            let e = Expr::IsNull(Box::new(left));
            return Ok(if negated { Expr::Not(Box::new(e)) } else { e });
        }
        Ok(left)
    }

    // arith   := term ( ("+" | "-") term )*
    // term    := operand ( ("*" | "/") operand )*
    // operand := "-"* primary
    //
    // One function for both levels (`arith_level`), a sum's frame calling a
    // product's, so a level of parentheses is two frames on its way to
    // `primary`: depth is stack depth (see `MAX_EXPR_DEPTH`), and a frame
    // more a level would be more stack for the same 512 levels. Every
    // operator counts as a level, as `or` does, since evaluation recurses
    // down the tree it makes.

    /// `n + 1`, `price * (1 - $1)`: the operands a comparison takes, or a
    /// value in a `set`. Without an operator, the operand alone.
    fn arith(&mut self) -> Result<Expr> {
        let mark = self.depth;
        let out = self.arith_level(false);
        self.depth = mark;
        out
    }

    /// A sum of products (`product` false), or a product of operands.
    fn arith_level(&mut self, product: bool) -> Result<Expr> {
        let mut left = match product {
            true => self.operand()?,
            false => self.arith_level(true)?,
        };
        loop {
            let op = match (self.peek(), product) {
                (Tok::Plus, false) => ArithOp::Add,
                (Tok::Minus, false) => ArithOp::Sub,
                (Tok::Star, true) => ArithOp::Mul,
                (Tok::Slash, true) => ArithOp::Div,
                _ => return Ok(left),
            };
            self.next();
            self.deepen()?;
            let right = match product {
                true => self.operand()?,
                false => self.arith_level(true)?,
            };
            left = Expr::Arith(op, Box::new(left), Box::new(right));
        }
    }

    /// A primary, after any `-`s: a number's folded into it, as the lexer
    /// would have read `-1` where nothing before it ends a value. Out of
    /// line, so that `primary` is in one place.
    #[inline(never)]
    fn operand(&mut self) -> Result<Expr> {
        let mut minus = 0;
        while matches!(self.peek(), Tok::Minus) {
            self.next();
            self.deepen()?;
            minus += 1;
        }
        let mut e = self.primary()?;
        for _ in 0..minus {
            e = match e {
                Expr::Lit(Value::Int(i)) if i != i64::MIN => Expr::Lit(Value::Int(-i)),
                Expr::Lit(Value::Float(f)) => Expr::Lit(Value::Float(-f)),
                e => Expr::Arith(
                    ArithOp::Mul,
                    Box::new(Expr::Lit(Value::Int(-1))),
                    Box::new(e),
                ),
            };
        }
        Ok(e)
    }

    /// A call's arguments, its `(` read. Four shapes are a call's too:
    /// `count(*)`, a call of nothing; `count(distinct e)`, whose argument is
    /// a call of `distinct`; `first(e by k)`, of two; and `bucket(at, 15m)`,
    /// whose duration is the text `15m`.
    #[inline(never)]
    fn call(&mut self, name: String) -> Result<Expr> {
        let mut args = Vec::new();
        if name == "count" && matches!(self.peek(), Tok::Star) {
            self.next();
        } else if name == "count"
            && self.peek_kw("distinct")
            && !matches!(
                self.toks.get(self.i + 1).map(|t| &t.tok),
                Some(Tok::RParen | Tok::Comma)
            )
        {
            self.next();
            args.push(Expr::Call("distinct".into(), vec![self.expr()?]));
        }
        loop {
            if matches!(self.peek(), Tok::RParen) || !args.is_empty() && name == "count" {
                break;
            }
            let duration = match (self.peek(), self.toks.get(self.i + 1).map(|t| &t.tok)) {
                (Tok::Int(n), Some(Tok::Ident(u))) if name == "bucket" && args.len() == 1 => {
                    Some(format!("{n}{}", u.to_ascii_lowercase()))
                }
                _ => None,
            };
            match duration {
                Some(d) => {
                    self.i += 2;
                    args.push(Expr::Lit(Value::Text(d)));
                }
                None => args.push(self.expr()?),
            }
            if args.len() == 1 && matches!(name.as_str(), "first" | "last") && self.eat_kw("by") {
                args.push(self.expr()?);
            }
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        self.expect(Tok::RParen)?;
        Ok(Expr::Call(name, args))
    }

    /// `case when <cond> then <value> ... [else <value>] end`, `case` read:
    /// a call of its conditions and values in turn and the `else` last,
    /// which `eval` works out lazily.
    #[inline(never)]
    fn case(&mut self) -> Result<Expr> {
        let mut args = Vec::new();
        while self.eat_kw("when") {
            args.push(self.expr()?);
            self.expect_kw("then")?;
            args.push(self.expr()?);
        }
        if self.eat_kw("else") {
            args.push(self.expr()?);
        }
        self.expect_kw("end")?;
        Ok(Expr::Call("case".into(), args))
    }

    /// `(get <collection> select <field> ...)` after `in`: a query whose one
    /// column is the list, run before the query around it.
    fn subquery(&mut self, left: Expr) -> Result<Expr> {
        self.expect(Tok::LParen)?;
        if !(self.peek_kw("get") || self.peek_kw("select")) {
            return self
                .err("`in (...)` takes a `get`: `in (get <collection> select <field> ...)`");
        }
        if self.subqueries >= MAX_SUBQUERY_DEPTH {
            return self.err(format!(
                "`in (get ...)` nested too deep: at most {MAX_SUBQUERY_DEPTH} levels"
            ));
        }
        self.subqueries += 1;
        let inner = self.get();
        self.subqueries -= 1;
        let Statement::Select(sel) = inner? else {
            unreachable!("`get` parses a select")
        };
        if let Err(e) = sel.check_subquery() {
            return self.err(e);
        }
        self.expect(Tok::RParen)?;
        Ok(Expr::InSelect(Box::new(left), Box::new(sel)))
    }

    fn primary(&mut self) -> Result<Expr> {
        // Taken rather than cloned, as `next` would.
        if let Tok::Vector(v) = &mut self.toks[self.i].tok {
            let v = std::mem::take(v);
            self.i += 1;
            return Ok(Expr::Lit(Value::Vector(v)));
        }
        match self.peek().clone() {
            Tok::LParen => {
                self.next();
                let e = self.expr()?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            // An object, a `json` field's value: constants alone, as in a
            // list, its keys names or strings, sorted and none twice.
            Tok::LBrace => {
                let was = std::mem::replace(&mut self.exact, true);
                let pairs = self.object();
                self.exact = was;
                let mut members = Vec::new();
                for (k, e) in pairs? {
                    match e {
                        Expr::Lit(v) => members.push((k, v)),
                        _ => return self.err("only constant values can be used inside an object"),
                    }
                }
                match Value::object(members) {
                    Ok(v) => Ok(Expr::Lit(v)),
                    Err(e) => self.err(e),
                }
            }
            Tok::LBracket => {
                // A vector written out, numbers alone between the brackets,
                // is read into its `f32`s at once, as the loop below would
                // make it: each number through `expr` was most of what a
                // query holding one took to parse.
                if !self.exact {
                    if let Some(v) = self.numbers_in_brackets() {
                        return Ok(Expr::Lit(Value::Vector(v)));
                    }
                }
                self.next();
                let mut items = Vec::new();
                loop {
                    if matches!(self.peek(), Tok::RBracket) {
                        break;
                    }
                    items.push(self.expr()?);
                    if !matches!(self.peek(), Tok::Comma) {
                        break;
                    }
                    self.next();
                }
                self.expect(Tok::RBracket)?;
                // Constants alone, moved out in one pass: through iterator
                // chains, the vector's and the list's, this was 1.2 KB of the
                // browser module.
                let mut vals = Vec::with_capacity(items.len());
                let mut numbers = !self.exact && !items.is_empty();
                for e in items {
                    let Expr::Lit(v) = e else {
                        return self.err("only constant values can be used inside a list");
                    };
                    numbers &= matches!(v, Value::Int(_) | Value::Float(_));
                    vals.push(v);
                }
                // Every element a constant number: a vector, so embedding
                // lists become Value::Vector in one go.
                if numbers {
                    let mut v = Vec::with_capacity(vals.len());
                    for x in &vals {
                        v.push(match x {
                            Value::Int(i) => *i as f32,
                            Value::Float(f) => *f as f32,
                            _ => continue,
                        });
                    }
                    return Ok(Expr::Lit(Value::Vector(v)));
                }
                Ok(Expr::Lit(Value::List(vals)))
            }
            Tok::Str(s) => {
                self.next();
                Ok(Expr::Lit(Value::Text(s)))
            }
            Tok::Int(i) => {
                self.next();
                Ok(Expr::Lit(Value::Int(i)))
            }
            Tok::Float(f) => {
                self.next();
                Ok(Expr::Lit(Value::Float(f)))
            }
            Tok::Param(i) => {
                self.next();
                Ok(Expr::Param(i))
            }
            Tok::Ident(name) => {
                let lower = name.to_ascii_lowercase();
                match lower.as_str() {
                    "true" => {
                        self.next();
                        return Ok(Expr::Lit(Value::Bool(true)));
                    }
                    "false" => {
                        self.next();
                        return Ok(Expr::Lit(Value::Bool(false)));
                    }
                    "null" => {
                        self.next();
                        return Ok(Expr::Lit(Value::Null));
                    }
                    _ => {}
                }
                self.next();
                if lower == "case" && self.peek_kw("when") {
                    return self.case();
                }
                if matches!(self.peek(), Tok::LParen) {
                    self.next();
                    return self.call(lower);
                }
                Ok(Expr::Field(name))
            }
            other => self.err(format!("expected a value, found {}", other.describe())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(r: &Reading) -> &Refusal {
        match &r.rest {
            Rest::Refused(r) => r,
            other => panic!("not refused: {other:?}"),
        }
    }

    /// What a text reads as, a line at a time: whole where a `;` or a
    /// statement follows, the last left to what comes after, and a word
    /// that begins no statement holding back the statement before it.
    #[test]
    fn a_text_reads_as_whole_statements_and_what_is_left() {
        let r = reading("");
        assert_eq!((r.whole.len(), &r.rest), (0, &Rest::Nothing));
        let r = reading("get a");
        assert!(r.whole.is_empty() && matches!(r.rest, Rest::Last(0, _)));
        let r = reading("get a;  -- done\n");
        assert_eq!((r.whole, r.rest), (vec![(0, 16)], Rest::Nothing));
        let r = reading("get a\nget b");
        assert_eq!(r.whole, [(0, 6)]);
        assert!(matches!(r.rest, Rest::Last(6, _)));
        // One statement over three lines.
        let r = reading("get notes select body\n  where topic = 4\n  order posted desc\n");
        assert!(r.whole.is_empty() && matches!(r.rest, Rest::Last(0, _)));
        let r = reading("del notes\n  where id = 3");
        let Rest::Last(0, s) = r.rest else { panic!() };
        assert!(matches!(
            *s,
            Statement::Delete {
                filter: Some(_),
                ..
            }
        ));
        // A `;` ends a statement: no clause goes on past it, and one written
        // past it holds the statement back, as `-c` refuses the two.
        let r = reading("del notes; where id = 3");
        assert!(r.whole.is_empty());
        assert_eq!((refusal(&r).at, refusal(&r).begins), (0, Some(11)));
        assert!(parse("del notes; where id = 3").is_err());
        let r = reading("del notes;; get notes");
        assert_eq!(r.whole, [(0, 12)]);
        assert!(matches!(r.rest, Rest::Last(12, _)));
        // Inside brackets a `;` is refused; and `select`, a clause and a
        // statement, begins a statement after one.
        assert!(parse("get a where x in (get b select y;)").is_err());
        let two = parse("get a lookup b on k; select z").unwrap();
        assert!(
            matches!(&two[..], [Statement::Select(_), Statement::Select(s)] if s.collection == "z")
        );
        // A query string's list keeps the `;` after it, and refuses one
        // inside it.
        assert!(parse_select_list("a, b;").is_ok());
        assert!(parse_select_list("a; b").is_err());
        assert!(parse_group_list("a;").is_ok() && parse_facet_list("a top 3;").is_ok());
        // `wher` begins no statement: it may go on with `del notes`, which
        // is refused with it rather than run as every row's delete.
        let r = reading("del notes\nwher id = 3");
        assert!(r.whole.is_empty());
        let f = refusal(&r);
        assert_eq!((f.at, f.begins, f.end), (0, Some(10), false));
        assert!(f.error.to_string().contains("unknown command `wher`"));
        // Refused inside a statement that began as one: the one before is whole.
        let r = reading("put a {x: 1}\nget b limit \"x\"");
        assert_eq!(r.whole, [(0, 13)]);
        assert_eq!((refusal(&r).at, refusal(&r).begins), (13, None));
        // Cut short by the end, and by an open string.
        assert!(refusal(&reading("get a where")).end);
        assert!(refusal(&reading("put a {s: \"it's")).end);
        assert!(!refusal(&reading("get a where x = 1 !")).end);
        // A `;`, a keyword or a quote in a string or a comment is neither.
        for text in [
            "put notes {body: \"a; where id = 3\"}",
            "put notes {body: 'del notes; -- no'}",
            "get notes -- it's; del notes\n",
            "get notes # \"where\n  where id = 3",
        ] {
            let r = reading(text);
            assert!(
                r.whole.is_empty() && matches!(r.rest, Rest::Last(0, _)),
                "{text}"
            );
        }
    }

    /// The places are bytes, which a caller slices the text by: in
    /// characters, as tokens count them, a statement after a `ç` was sliced
    /// a byte early.
    #[test]
    fn a_reading_is_placed_in_bytes() {
        let text = "put a {b: \"çay\"}\nput a {b: \"ş\"}; get a";
        let r = reading(text);
        let got: Vec<&str> = r.whole.iter().map(|&(a, b)| &text[a..b]).collect();
        assert_eq!(got, ["put a {b: \"çay\"}\n", "put a {b: \"ş\"}; "]);
        let Rest::Last(at, _) = r.rest else { panic!() };
        assert_eq!(&text[at..], "get a");
        let spans = spans(text).unwrap();
        let got: Vec<&str> = spans.iter().map(|&(a, b)| &text[a..b]).collect();
        assert_eq!(got, ["put a {b: \"çay\"}\n", "put a {b: \"ş\"}; ", "get a"]);
        let r = reading("put a {b: \"ü\"}\nwher x");
        assert_eq!((refusal(&r).at, refusal(&r).begins), (0, Some(16)));
        // Letters of three bytes and four, and a statement after each.
        let text = "put a {b: \"名前\"} put a {b: \"😀😀\"};get a -- ü\nget b";
        let r = reading(text);
        let got: Vec<&str> = r.whole.iter().map(|&(a, b)| &text[a..b]).collect();
        assert_eq!(
            got,
            [
                "put a {b: \"名前\"} ",
                "put a {b: \"😀😀\"};",
                "get a -- ü\n"
            ]
        );
        let Rest::Last(at, _) = r.rest else { panic!() };
        assert_eq!(&text[at..], "get b");
    }

    /// Statements of every kind, written whole.
    const WRITTEN: &[&str] = &[
        "get notes select body, posted where topic = 3 and posted > 1 order posted desc limit 10 offset 2",
        "get docs where year >= 2020 near embed [0.1, 0.2] limit 5",
        "get docs select title near embed $1 ef 64 exact limit 5",
        "get products match title $1 fuzzy prefix limit 10",
        "get notes select body, highlight(body) match body \"oasys stars\" fuzzy limit 10",
        "get docs match body $1 near embed $2 fuse k 60 candidates 40 limit 10",
        "get docs match body $1 rerank embed $2 candidates 50 limit 10",
        "get notes where topic in (get topics select id where name ~ \"x\") limit 3",
        "get notes where topic in [1, 2, 3] and not (body is null) or posted < now() - 1000",
        "get notes select topic, count(*) as n group topic having n > 1 order n desc limit 5",
        "get e select bucket(at, 1d, 'Europe/Berlin') as day, count(*) group day",
        "get notes select a + b as total, case when a > 1 then 'x' else 'y' end as k",
        "get notes select topic + 1 as a, topic + 2 as b",
        "get orders lookup lines on order_id required select sku limit 3",
        "get shops lookup orders on (org, shop) = (org, id) order at desc limit 3",
        "get products facet brand top 5 disjunctive, price ranges [0, 2500, 5000] limit 20",
        "get notes order distance(loc, $1) limit 1",
        "get notes where meta.source.rank > 2 order body collate tr limit 1 require 1",
        "select a, b from notes where a = 1 order b limit 2",
        "explain get notes where topic = 3 order posted desc",
        "put notes {body: \"a night\", topic: 3, posted: now()}",
        "put notes [{body: \"a\", embed: [0.1, -0.2]}, {body: \"b\", meta: {k: [1, 2.5]}}]",
        "put locks {name: \"nightly\", owner: $1, at: now()} if absent",
        "put hits {key: $1, n: 1} if absent else set {n: n + new.n} require 1 returning id",
        "insert into notes {body: $1} returning *",
        "put ev $1",
        "set jobs {owner: $1, run_at: now() + 30000, attempts: attempts + 1} where run_at <= now() order run_at limit 10 returning *",
        "set docs {body: $1, version: version + 1} where id = $2 and version = $3 require 1",
        "update notes set {topic: 4} where id = 3",
        "del notes where id = 3",
        "delete from notes where topic = 4 order posted limit 1 returning id",
        "create collection notes (body text @text(prefix=3), topic int @hash, posted timestamp @sorted, embed vector<384> @hnsw(cosine, m=16), @sorted(topic, posted desc))",
        "create collection if not exists users (email text nocase @unique, name text collate tr required)",
        "create index on notes (embed) @hnsw(cosine, quant=int8)",
        "create index if not exists on notes (topic, posted desc) @sorted",
        "drop index on notes (topic, posted)",
        "drop collection if exists notes",
        "alter collection notes add field mood text",
        "alter collection notes rename field mood to feel",
        "alter collection notes alter field posted @ttl(30d)",
        "collections",
        "describe notes",
        "compact notes",
    ];

    /// The words of a text, cut where the white space between them is.
    fn cuts(text: &str) -> Vec<usize> {
        let mut out = Vec::new();
        let mut quote = None;
        for (i, c) in text.char_indices() {
            match (quote, c) {
                (None, '"' | '\'') => quote = Some(c),
                (Some(q), _) if c == q => quote = None,
                (None, ' ') => out.push(i),
                _ => {}
            }
        }
        out
    }

    /// Every statement, written over lines cut between any of its words
    /// and read a line at a time as the shell reads one: never read as two,
    /// never refused for good before its last line -- each refusal is one a
    /// next line moves (`Refusal::stands`) -- and read whole at the end as
    /// the statement written on one line is.
    #[test]
    fn a_statement_written_over_lines_reads_as_one() {
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        for text in WRITTEN {
            let want = parse_one(text).unwrap();
            let cuts = cuts(text);
            for round in 0..200 {
                // Every single cut first, then cuts at random.
                let chosen: Vec<usize> = match round < cuts.len() {
                    true => vec![cuts[round]],
                    false => cuts
                        .iter()
                        .copied()
                        .filter(|_| {
                            x ^= x << 13;
                            x ^= x >> 7;
                            x ^= x << 17;
                            x.is_multiple_of(3)
                        })
                        .collect(),
                };
                let mut lines = Vec::new();
                let mut from = 0;
                for c in chosen.iter().copied().chain([text.len()]) {
                    lines.push(format!("{}\n", &text[from..c]));
                    from = c;
                }
                let mut buf = String::new();
                let mut held: Option<Refusal> = None;
                for line in &lines {
                    buf.push_str(line);
                    let r = reading(&buf);
                    assert!(r.whole.is_empty(), "{lines:?} read as two");
                    match r.rest {
                        Rest::Refused(now) => {
                            if let Some(before) = &held {
                                assert!(!before.stands(&now), "{lines:?}: {now:?}");
                            }
                            held = Some(now);
                        }
                        _ => held = None,
                    }
                }
                let r = reading(&buf);
                assert!(r.whole.is_empty());
                let Rest::Last(0, got) = r.rest else {
                    panic!("{lines:?}: {:?}", r.rest)
                };
                assert_eq!(*got, want, "{lines:?}");
            }
        }
    }

    /// A line typed, with no next line to ask: of every statement cut after
    /// any of its words, with no string or bracket left open (which the
    /// shell reads on past), a refusal stands alone only where a check a
    /// next line could satisfy is one `limit` cannot reach -- the ones
    /// below, each refused as it was when the shell ran every line as it
    /// came. Pasted or piped, the next line is there to tell.
    #[test]
    fn a_statement_typed_up_to_a_word_is_left_open() {
        let mut stood = Vec::new();
        for text in WRITTEN {
            for c in cuts(text) {
                let prefix = &text[..c];
                let mut gate = crate::lexer::Unclosed::default();
                gate.feed(prefix);
                if gate.is_open() {
                    continue;
                }
                let r = reading(prefix);
                assert!(r.whole.is_empty(), "{prefix} read as two");
                if let Rest::Refused(f) = r.rest {
                    if f.stands_alone(prefix) {
                        stood.push(prefix);
                    }
                }
            }
        }
        assert_eq!(
            stood,
            [
                // An item with no name yet, which `as` would give it.
                "get e select bucket(at, 1d, 'Europe/Berlin')",
                "get notes select a + b",
                "get notes select a + b as total, case when a > 1 then 'x' else 'y' end",
                "get notes select topic + 1",
                "get notes select topic + 1 as a, topic + 2",
                // The SQL order's list, before its `from`.
                "select a,",
                "select a, b",
                // `@ttl(..)` decides the arm, a token past the field.
                "alter collection notes alter",
                "alter collection notes alter field",
                "alter collection notes alter field posted",
            ]
        );
        // A refusal of what was read stands at once, wherever it is placed.
        for text in [
            "get notes limit \"x\"",
            "create index on notes (f) @foo",
            "put notes {a: 1} require \"x\"",
            "create collection x ()",
        ] {
            let Rest::Refused(f) = reading(text).rest else {
                panic!("{text}")
            };
            assert!(f.stands_alone(text), "{text}");
        }
    }

    /// What the parser made of a text before the lexer read a list of
    /// numbers as one token.
    fn parse_plain(src: &str) -> Result<Vec<Statement>> {
        let mut p = Parser {
            toks: crate::lexer::tokenize(src)?,
            i: 0,
            depth: 0,
            subqueries: 0,
            exact: false,
            cut: false,
        };
        let mut out = Vec::new();
        while !p.at_eof() {
            out.push(p.statement()?);
        }
        Ok(out)
    }

    /// A list of numbers read at once parses to the statement its tokens
    /// one by one did, and a text one refused the other refuses: over
    /// numbers of every shape the lexer reads, and some it does not, in
    /// every place a list goes.
    #[test]
    fn a_list_read_as_a_vector_parses_as_its_tokens_did() {
        let items = [
            "0",
            "-0",
            "7",
            "-123",
            "0.5",
            "-0.0",
            "1e5",
            "1E-3",
            "2.5e+2",
            "-4.25E1",
            "9007199254740993",
            "-9007199254740993",
            "9223372036854775807",
            "99999999999999999999",
            "3.14159265358979323846264338",
            "1e",
            "1.5e1234",
            "1e-400",
            ".5",
            "1.",
            "1_000",
            "0.1_5",
            "\"a\"",
            "$1",
            "[1]",
            "[]",
            "-",
            "- 1",
            "1e05",
            "0.000001",
            "123456789.123456789",
            "-- c\n1",
            "1 # c\n",
        ];
        let seps = [", ", ",", " , ", ",\n", "\t,\t"];
        let mut x = 0x2545f4914f6cdd1du64;
        let mut rnd = |n: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n as u64) as usize
        };
        for _ in 0..20_000 {
            let n = rnd(6);
            // Mostly numbers, as a vector is written.
            let mut list = String::from(["[", "[ ", "[\n"][rnd(3)]);
            for k in 0..n {
                if k > 0 {
                    list.push_str(seps[rnd(seps.len())]);
                }
                let it = match rnd(4) {
                    0 => items[rnd(items.len())],
                    _ => items[rnd(10)],
                };
                list.push_str(it);
            }
            if rnd(8) == 0 {
                list.push(',');
            }
            list.push_str(["]", " ]", "\n]"][rnd(3)]);
            let text = match rnd(6) {
                0 => format!("put t {{v: {list}}}"),
                1 => format!("get t where x in {list}"),
                2 => format!("get t where x IN {list} and y = 1"),
                3 => format!("get t near v {list} limit 3"),
                4 => format!("put t [{{v: {list}}}, {{w: {list}}}]"),
                _ => format!("get t where x has {list}"),
            };
            match (parse(&text), parse_plain(&text)) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "{text}"),
                (Err(_), Err(_)) => {}
                (a, b) => panic!("{text}: {a:?} against {b:?}"),
            }
        }
    }
}
