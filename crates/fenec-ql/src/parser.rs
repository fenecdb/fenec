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
//! select o.id, c.name from orders o join customers c on c.id = o.customer_id
//!                                        -- a JOIN on a key, read as a lookup (`join.rs`)
//! put    <name> { ... } if absent        -- a document whose id or @unique value is held is passed over
//! put    <name> { ... } if absent else set { n: n + new.n }   -- or sets the row holding it
//! put    <name> $1                       -- the documents a parameter holds: an object or a list
//! put | insert ... [returning * | a, b]  -- the rows written, ids and all, in the documents' order
//! set    <name> { k: v, ... } [where <expr>] -- v may read the row: {n: n + 1, at: now()}
//! <expr> + - * / <expr>                  -- numbers, or a timestamp and milliseconds
//! del    <name> [where <expr>]
//! set | del ... [order <key> [desc], ...] [limit N] [returning * | a, b]
//!                                        -- the rows a get would page to, answered as written
//! put | set | del | get ... require N [else '<reason>']
//!                                        -- N rows written or answered, or refused (412), saying why
//! collections | describe <name> | compact [<name>]
//! create [or replace] statement <name>($p <type> [= <literal>], ...) [claims (<c> <type> [from "<claim>"], ...)]
//!        [public] [scans] [comment '<text>'] [check <expr>] as <statement> | as begin <statement>; ... end
//!                                        -- a named statement: `$p` and `$jwt.<c>` in its body, parsed, not yet run
//! drop statement [if exists] <name> | statements | describe statement <name>
//! call <name> [{p: v, ...} | $1] [as {c: v, ...} | as $2]   -- and SQL's CALL <name>(v, ..., p => v)
//! ```

use crate::join::{From, Join, Table};
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

/// What an index is written as, said where SQL's `create index` is not it.
const INDEX_FORM: &str = "an index is `create index on <collection> (<field>, ...) @hash`, \
                          `@sorted` or `@unique`: it names its kind, and has no name";

/// What SQL's `join` is answered with: both ways a key runs, and the
/// filter by another collection's rows.
const JOIN: &str = "FenecQL has no `join`: `get orders where ... lookup items on order_id` \
                    attaches each order's items, `get items where ... lookup orders on id = \
                    order_id` each item's order -- the clauses after `lookup` are the looked-up \
                    collection's -- and `where order_id in (get orders select id where ...)` \
                    filters by another collection";

/// What a column's or a table's rule fenecdb holds no rule of is refused
/// with: dropped, the collection would take writes SQL refuses.
const NO_RULE: &str = "a field holds its type, `not null` (`required`) and `unique` \
                       (`@unique`), and no `default`, `references`, `check` or computed value: \
                       a write names its values";

/// What an insert that names no fields is answered with.
const NAMED: &str = "an insert names its fields: `insert into <collection> (<field>, ...) values \
                     (...)`, or `insert <collection> {<field>: <value>}`";

/// How a `require`'s reason is written, which each refusal of one shows.
const REASON_EMPTY: &str = "a `require`'s reason is empty: say why, \
                            `require 1 else 'no such account'`";
const REASON: &str = "`else` after `require <n>` takes its reason in quotes: \
                      `require 1 else 'no such account'`";
const REASON_PARAM: &str = "a `require`'s reason is written in the statement, never a parameter, \
                            as its count is: `require 1 else 'no such account'`";
const REASON_TICK: &str = "a `require`'s reason holds no backtick, which its refusal quotes the \
                           statement with: `require 1 else 'no such account'`";

/// The words a clause of a `get` or a join begins with, which no alias SQL
/// writes without `as` is.
const TAKEN: [&str; 27] = [
    "select",
    "facet",
    "group",
    "having",
    "where",
    "near",
    "match",
    "fuse",
    "rerank",
    "order",
    "count",
    "limit",
    "offset",
    "require",
    "lookup",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "cross",
    "natural",
    "on",
    "using",
    "union",
    "returning",
    "from",
];

/// The words a statement begins with, and SQL's that `Parser::statement`
/// answers: one a `get` stops at is the next statement's, any other an
/// alias or a mistake. Named statements' `call` and `statements` are not
/// among them ([`NAMED_COMMANDS`]): reserved, they took `select a from t
/// call` and `count(*) statements` from SQL, which reserves neither
/// (PostgreSQL's `CALL` is a word any alias takes).
const COMMANDS: [&str; 20] = [
    "get",
    "select",
    "put",
    "insert",
    "set",
    "update",
    "del",
    "delete",
    "create",
    "drop",
    "alter",
    "explain",
    "collections",
    "describe",
    "compact",
    "truncate",
    "begin",
    "start",
    "commit",
    "rollback",
];

/// The words named statements begin with, which an alias still takes: a
/// text reads `call` and `statements` as a statement only where one may
/// begin, and a statement's name is neither.
#[cfg(feature = "statements")]
const NAMED_COMMANDS: [&str; 2] = ["call", "statements"];

/// What a definition or a call is refused with where this build has no
/// named statements (fenec-ql's `statements` feature): the browser module.
#[cfg(not(feature = "statements"))]
pub(crate) const UNBUILT: &str =
    "named statements are a server's; this module is built without them";

/// What SQL's routine, `create function` or `create procedure`, is refused
/// with: run as its caller unless told, it would change who may do what
/// read as a named statement, which runs with its own text's authority.
#[cfg(feature = "statements")]
const ROUTINE: &str = "a routine here is a named statement, `create statement <name>($a int) as \
                       <statement>`, called with `call <name> {a: 1}`: it runs with its own \
                       text's authority, where SQL's function runs as its caller unless told";

/// What PostgreSQL's `->>` is answered with, and `json_extract()` by the
/// engine in its words.
const JSON_PATH: &str = "a json field's member is read by a path: `meta.source`, not \
                         `meta->>'source'`";

/// What full text in a filter is answered with: SQL's `match(...)`,
/// PostgreSQL's `@@`.
const FULLTEXT: &str = "full text is the `match` clause over an `@text` field: \
                        `get docs where ... match body $1`";

/// What a test of a double-quoted text alone is refused with, after the
/// text and its name (`Parser::quoted`): SQL's quoted column, FenecQL's
/// text, so the test answers the same for every row.
const QUOTED: &str = "double quotes make text here, the same in every row";

/// What pgvector's distance outside an `order by` is refused with:
/// `near`, and what a row's `_score` is of the distance. One text, so that
/// `cmp_expr`'s frame holds no `format!` -- with one, 511 parentheses took
/// 1 106 KiB of stack against 1 090.
const DISTANCE: &str = "pgvector's distance ranks rows: `order by emb <-> $1 limit 10` is `near \
                        emb $1 limit 10`, `_score` the L2 distance for `<->`, 1 minus it for \
                        `<=>`, minus it for `<#>`";

/// What a query in a select list's place is refused with: a row of another
/// collection beside each row is a join, or a `lookup`.
const ITEM_QUERY: &str = "another collection's row beside each row is a join, `from l join p \
                          on p.id = l.product_id`, or a `lookup`";

/// The tables a correlated `exists` reads the row around it in: a join's,
/// or the one collection the token at a place names -- kept as the place,
/// since a copy of the name for every statement took a read by id's parse
/// 768 -> 831 ns.
enum Around {
    Tables(Vec<Table>),
    At(usize),
}

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
    /// The tables of the statement whose clauses are being read: what
    /// SQL's `exists (select ... where o.customer_id = c.id)` names the
    /// row around it by.
    outer: Around,
    /// Inside a named statement's definition, its `check` and its body:
    /// the names its `$from` and `$jwt.sub` are numbered by. None
    /// anywhere else, where a name after `$` is refused.
    def: Option<Box<Names>>,
}

/// What a definition's `$from` and `$jwt.sub` are numbered by: its
/// parameters first, in the signature's order, then its claims in theirs.
/// Never made without named statements, whose definitions alone make one.
#[cfg_attr(not(feature = "statements"), allow(dead_code))]
struct Names {
    params: Vec<String>,
    claims: Vec<String>,
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
        outer: Around::At(usize::MAX),
        def: None,
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
    /// Whether the text's end decided which statement the last one is
    /// (`left_open`): `describe statement` is the collection `statement`'s
    /// description alone and a named statement's with a name on the next
    /// line. A reader a line at a time waits for the next line before it
    /// runs one, whatever its kind.
    pub open: bool,
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
                open: false,
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
        outer: Around::At(usize::MAX),
        def: None,
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
    let mut open = false;
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
                Some((at, s, cut, _)) => {
                    open = cut && left_open(&s);
                    Rest::Last(at, Box::new(s))
                }
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
        open,
    }
}

/// Parses a single statement; errors when there is more than one.
pub fn parse_one(src: &str) -> Result<Statement> {
    one(parse(src)?)
}

/// A value the row cannot change: a literal, or a parameter.
fn fixed(e: &Expr) -> bool {
    matches!(e, Expr::Lit(_) | Expr::Param(_))
}

/// Whether `s`, read whole at the text's end with the end marked
/// (`Parser::cut`), is another statement with a word more: `describe
/// statement` is the collection `statement`'s description, and a named
/// statement's with its name on the next line. Not every mark: most are
/// left by a reading tried and dropped (`get notes count` tries SQL's list
/// first), and a read the next line may go on with runs at Enter all the
/// same -- its `where` begins no statement, and says so.
fn left_open(s: &Statement) -> bool {
    cfg!(feature = "statements")
        && matches!(s, Statement::Describe(n) if n.eq_ignore_ascii_case("statement"))
}

/// Whether a statement begins with `w` ([`COMMANDS`], [`NAMED_COMMANDS`]).
#[cfg(feature = "statements")]
fn is_command(w: &str) -> bool {
    COMMANDS
        .iter()
        .chain(&NAMED_COMMANDS)
        .any(|c| w.eq_ignore_ascii_case(c))
}

/// Why a call's argument is no value, if it is not: it reads no row, so it
/// names no field and holds no query.
#[cfg(feature = "statements")]
fn not_a_value(e: &Expr) -> Option<String> {
    let mut fields = Vec::new();
    e.referenced_fields(&mut fields);
    if let Some(f) = fields.first() {
        return Some(format!(
            "an argument is a value -- a literal, a parameter, an expression of them -- and \
             `{f}` names a field: a text in quotes"
        ));
    }
    e.has_subquery()
        .then(|| "an argument is a value, and reads no collection".to_string())
}

/// Whether a text `~` is handed reads as a regular expression: anchored
/// (`^a`, `a$`) or holding `.*` or `.+`, which a text searched for as
/// written seldom does. Narrow on purpose: `[draft]` and `a.b` are text
/// people search for.
fn regex_like(t: &str) -> bool {
    let b = t.as_bytes();
    (b.len() > 1 && (b[0] == b'^' || b[b.len() - 1] == b'$'))
        || b.windows(2)
            .any(|w| w[0] == b'.' && matches!(w[1], b'*' | b'+'))
}

/// `select city group city`: a list of the keys alone, each group's set of
/// values once -- SQL's `group by city` with no aggregate, and `select
/// distinct city`. Its items are the aggregating list's, held to the keys
/// as one is.
#[inline(never)]
fn keys_alone(sel: &mut Select) {
    if sel.group.is_empty() || !sel.aggregate.is_empty() || !sel.marks.is_empty() {
        return;
    }
    let Some(names) = sel.project.take() else {
        return;
    };
    for name in names {
        let expr = match sel.computed.iter().position(|c| c.name == name) {
            Some(i) => sel.computed.remove(i).expr,
            None => Expr::Field(name.clone()),
        };
        sel.aggregate.push(Column { expr, name });
    }
}

/// pgvector's text of a vector, `[1, 2.5, 3]`.
#[inline(never)]
fn pgvector_text(t: &str) -> Option<Vec<f32>> {
    let inner = t.trim_ascii().strip_prefix('[')?.strip_suffix(']')?;
    let mut out = Vec::new();
    for n in inner.split(',') {
        out.push(fenec_core::num::parse_f64(n.trim_ascii())? as f32);
    }
    Some(out)
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
        outer: Around::At(usize::MAX),
        def: None,
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
        outer: Around::At(usize::MAX),
        def: None,
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
        outer: Around::At(usize::MAX),
        def: None,
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
    /// `"Users"` is a name where only a name can stand, as SQL quotes one:
    /// that was an error before, so no text that parsed reads otherwise.
    fn ident(&mut self) -> Result<String> {
        match self.next() {
            Tok::Ident(s) | Tok::Str(s) if s.contains('.') => {
                self.i -= 1;
                self.err(format!("`{s}` is a path; a name holds no dot"))
            }
            Tok::Ident(s) | Tok::Str(s) => Ok(s),
            other => self.err(format!("expected a name, found {}", other.describe())),
        }
    }

    /// A field, or a path into a json field (`meta.source.rank`): what a
    /// `select`, an `order` and a `create index` take. Double-quoted too,
    /// as [`Self::ident`].
    fn path(&mut self) -> Result<String> {
        match self.next() {
            Tok::Ident(s) | Tok::Str(s) => Ok(s),
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
            #[cfg(feature = "statements")]
            "statements" => {
                self.next();
                Ok(Statement::ListStatements)
            }
            #[cfg(not(feature = "statements"))]
            "statements" => self.err(UNBUILT),
            "call" => self.call_statement(),
            "describe" => {
                self.next();
                // `describe statement <name>`: a name after `statement`,
                // and no word a statement begins with -- `describe
                // statement` alone stays the collection named `statement`'s
                // description, a statement after it as ever.
                #[cfg(feature = "statements")]
                let named = self.peek_kw("statement")
                    && matches!(self.toks.get(self.i + 1).map(|t| &t.tok),
                        Some(Tok::Ident(w) | Tok::Str(w)) if !is_command(w));
                // `describe statement` at the text's end: alone it is the
                // collection `statement`'s, and a name on the next line
                // makes it a named statement's.
                #[cfg(feature = "statements")]
                {
                    self.cut |= self.peek_kw("statement")
                        && matches!(self.toks.get(self.i + 1).map(|t| &t.tok), Some(Tok::Eof));
                }
                #[cfg(feature = "statements")]
                if named {
                    self.next();
                    return Ok(Statement::DescribeStatement(self.statement_name()?));
                }
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
            // SQL's words with no statement of their own here, each
            // answered with what FenecQL writes: an error that teaches,
            // where an agent writing SQL by habit would retry blind.
            "join" | "inner" | "left" | "right" | "full" | "cross" => self.err(JOIN),
            // `from users u join orders o`: the alias, then the join.
            _ if matches!(self.toks.get(self.i + 1).map(|t| &t.tok),
                Some(Tok::Ident(k)) if k.eq_ignore_ascii_case("join")) =>
            {
                self.err(JOIN)
            }
            "truncate" => self.err("FenecQL deletes every row with `del <collection>`"),
            "begin" | "start" | "commit" | "rollback" => self.err(
                "FenecQL has no open transaction: a `/batch` of statements (in the page, a `run` \
                 of several) lands whole or not at all",
            ),
            // `from users u where ...`: a word after a name that ends a
            // statement, as SQL's alias does. (One `format!` for both was 20
            // bytes smaller raw and 322 larger brotli.)
            other if self.i > 0 && matches!(self.toks[self.i - 1].tok, Tok::Ident(_)) => {
                Err(self.alias(other))
            }
            other => self.err(format!("unknown command `{other}`")),
        }
    }

    /// `msg` and what was found where it is refused: at the text's end
    /// "end of input", which a next line moves -- so the shell, reading a
    /// line at a time, holds a statement it refuses there as open.
    #[inline(never)]
    fn taught(&self, msg: &str) -> Error {
        self.error(&format!("{msg}; found {}", self.peek().describe()))
    }

    /// Refused where one more subquery would nest past the depth they
    /// nest to.
    fn deeper(&self) -> Result<()> {
        match self.subqueries >= MAX_SUBQUERY_DEPTH {
            true => self.err(format!(
                "`in (get ...)` nested too deep: at most {MAX_SUBQUERY_DEPTH} levels"
            )),
            false => Ok(()),
        }
    }

    /// `other`, a word that begins no statement after a name that ends
    /// one: SQL's alias, if it is one.
    #[inline(never)]
    fn alias(&self, other: &str) -> Error {
        self.error(&format!(
            "unknown command `{other}`: an alias, if it is one, FenecQL does not take -- a \
             statement's one collection names its fields bare"
        ))
    }

    /// `table` after `create`, `drop` or `alter`: SQL's, refused with what
    /// FenecQL writes.
    fn no_table(&mut self, what: &str) -> Result<()> {
        match self.peek_kw("table") {
            true => self.err(format!("FenecQL writes `{what} collection`")),
            false => Ok(()),
        }
    }

    fn create(&mut self) -> Result<Statement> {
        self.expect_kw("create")?;
        if self.peek_kw("statement") {
            return self.create_statement(false);
        }
        #[cfg(feature = "statements")]
        if self.peek_kw("or") {
            self.next();
            self.expect_kw("replace")?;
            if self.peek_kw("statement") {
                return self.create_statement(true);
            }
            if self.peek_kw("function") || self.peek_kw("procedure") {
                return self.err(ROUTINE);
            }
            return Err(self.taught(
                "`create or replace` defines a named statement: `create or replace statement \
                 <name>(...) as ...`",
            ));
        }
        // The module reads SQL's `create or replace` and routines as main
        // did, and only a named statement's definition in words of its own.
        #[cfg(not(feature = "statements"))]
        if self.peek_kw("or")
            && matches!(self.toks.get(self.i + 2).map(|t| &t.tok),
                Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("statement"))
        {
            return self.err(UNBUILT);
        }
        #[cfg(feature = "statements")]
        if self.peek_kw("function") || self.peek_kw("procedure") {
            return self.err(ROUTINE);
        }
        if self.peek_kw("index") {
            return self.create_index(false);
        }
        if self.eat_kw("unique") {
            return self.create_index(true);
        }
        if self.peek_kw("table") {
            return self.create_table();
        }
        if ["temp", "temporary", "unlogged"]
            .iter()
            .any(|w| self.peek_kw(w))
        {
            return self.err("a collection is kept in the file, whole: `create table`");
        }
        if self.peek_kw("extension") {
            return self.err("vectors and full text are fenecdb's own: no extension to create");
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

    /// `create index [if not exists] on <collection> (<field>) @hnsw(...)`,
    /// and SQL's `create [unique] index [<name>] on <collection> [using
    /// btree | hash | hnsw | gin] (<column> [<opclass>] [desc], ...)
    /// [with (m = 16, ...)]`: `@sorted` or an index over several fields,
    /// `@unique`, `@hash`, `@hnsw` measuring its operator class's distance
    /// -- `vector_l2_ops` (pgvector's default), `vector_cosine_ops`,
    /// `vector_ip_ops` -- and `@text` for `gin (to_tsvector(..., body))`.
    /// An index has no name here, and one SQL names is not kept.
    fn create_index(&mut self, unique: bool) -> Result<Statement> {
        self.expect_kw("index")?;
        self.eat_kw("concurrently");
        let mut if_not_exists = false;
        if self.eat_kw("if") {
            self.expect_kw("not")?;
            self.expect_kw("exists")?;
            if_not_exists = true;
        }
        // SQL's name: `create index users_email on users (email)`.
        let named = !self.peek_kw("on") && matches!(self.peek(), Tok::Ident(_));
        if named {
            self.next();
        }
        if !self.eat_kw("on") {
            return Err(self.taught(INDEX_FORM));
        }
        self.eat_kw("only");
        let collection = self.ident()?;
        let method = match self.eat_kw("using") {
            true => Some(self.ident()?.to_ascii_lowercase()),
            false => None,
        };
        let (keys, sql) = self.sql_index_keys()?;
        if !matches!(self.peek(), Tok::At) || unique || named || method.is_some() || sql.is_some() {
            return self.sql_index(collection, keys, sql, unique, method, if_not_exists);
        }
        let mut keys = keys;
        self.next();
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

    /// An index's keys as SQL writes them -- `(<column> [<opclass>] [asc
    /// | desc] [nulls first | last], ...)`, or `(to_tsvector('english',
    /// body))` -- or as FenecQL does, `(owner, created desc)`: the fields
    /// and their directions, and the operator class SQL named the first
    /// with, or `to_tsvector` for full text.
    #[inline(never)]
    #[allow(clippy::type_complexity)]
    fn sql_index_keys(&mut self) -> Result<(Vec<(String, bool)>, Option<String>)> {
        self.expect(Tok::LParen)?;
        let mut keys = Vec::new();
        let mut op = None;
        loop {
            // `to_tsvector('english', body)`: the field, its text indexed.
            if matches!(self.toks.get(self.i + 1).map(|t| &t.tok), Some(Tok::LParen)) {
                if !self.eat_kw("to_tsvector") {
                    return self.err(
                        "an index is over fields, a json field's paths among them, and full \
                         text over `to_tsvector(body)`",
                    );
                }
                self.next();
                if matches!(self.peek(), Tok::Single(_) | Tok::Str(_)) {
                    self.next();
                    self.expect(Tok::Comma)?;
                }
                keys.push((self.path()?, false));
                self.expect(Tok::RParen)?;
                op.get_or_insert_with(|| "to_tsvector".to_string());
            } else {
                let k = self.path()?;
                if let Tok::Ident(w) = self.peek() {
                    if w.to_ascii_lowercase().ends_with("_ops") {
                        let w = w.to_ascii_lowercase();
                        self.next();
                        op.get_or_insert(w);
                    }
                }
                let desc = match self.eat_kw("desc") {
                    true => true,
                    false => {
                        self.eat_kw("asc");
                        false
                    }
                };
                // Where a null goes: no order reads an index's.
                if self.eat_kw("nulls") && !(self.eat_kw("first") || self.eat_kw("last")) {
                    return Err(self.taught("`nulls` is `nulls first` or `nulls last`"));
                }
                keys.push((k, desc));
            }
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        self.expect(Tok::RParen)?;
        Ok((keys, op))
    }

    /// SQL's `create index`, its keys read ([`Self::create_index`]).
    #[inline(never)]
    fn sql_index(
        &mut self,
        collection: String,
        mut keys: Vec<(String, bool)>,
        op: Option<String>,
        unique: bool,
        method: Option<String>,
        if_not_exists: bool,
    ) -> Result<Statement> {
        if matches!(self.peek(), Tok::At) {
            return self.err(
                "an index is written as SQL's, `create unique index on t (a)`, or as FenecQL's, \
                 `create index on t (a) @unique`: not both",
            );
        }
        if self.peek_kw("where") || self.peek_kw("include") {
            return self.err(
                "an index holds every row and its keys alone: a partial index's `where` and \
                 `include` have none here",
            );
        }
        let mut with = Vec::new();
        if self.eat_kw("with") {
            self.expect(Tok::LParen)?;
            loop {
                let k = self.ident()?.to_ascii_lowercase();
                self.expect(Tok::Eq)?;
                with.push((k, self.int()?));
                if !matches!(self.peek(), Tok::Comma) {
                    break;
                }
                self.next();
            }
            self.expect(Tok::RParen)?;
        }
        let text = op.as_deref() == Some("to_tsvector");
        let one = keys.len() == 1;
        let method = method.unwrap_or_else(|| "btree".to_string());
        let kind = match method.as_str() {
            "btree" if !text && !one => {
                return Ok(Statement::CreateComposite {
                    collection,
                    index: Composite { keys, unique },
                    if_not_exists,
                })
            }
            "btree" if !text => match unique {
                true => IndexKind::UNIQUE,
                false => IndexKind::SORTED,
            },
            "hash" if one && !text => match unique {
                true => IndexKind::UNIQUE,
                false => IndexKind::HASH,
            },
            "hnsw" if one && !unique => {
                let class = op.as_deref().unwrap_or("vector_l2_ops");
                let metric = if class.starts_with("sparsevec_ip") {
                    None
                } else if class.ends_with("_l2_ops") {
                    Some(Metric::L2)
                } else if class.ends_with("_cosine_ops") {
                    Some(Metric::Cosine)
                } else if class.ends_with("_ip_ops") {
                    Some(Metric::Dot)
                } else {
                    return self.err(format!(
                        "`{class}`: a vector's index measures `vector_l2_ops`, \
                         `vector_cosine_ops` or `vector_ip_ops`"
                    ));
                };
                let mut spec = VectorIndexSpec::default();
                // pgvector's bounds, refused past as pgvector refuses them:
                // clamped, `m = 1` built another index than it asked for.
                const BOUNDS: &str = "pgvector's `m` is from 2 to 100 and `ef_construction` \
                                      from 4 to 1000, at least twice `m`";
                let mut ef = false;
                for (k, v) in std::mem::take(&mut with) {
                    match k.as_str() {
                        "m" if (2..=100).contains(&v) => spec.m = v as usize,
                        "ef_construction" if (4..=1000).contains(&v) => {
                            spec.ef_construction = v as usize;
                            ef = true;
                        }
                        "m" | "ef_construction" => return self.err(BOUNDS),
                        _ => return self.err(format!("unknown hnsw parameter `{k}`")),
                    }
                }
                if ef && spec.ef_construction < 2 * spec.m {
                    return self.err(BOUNDS);
                }
                match metric {
                    Some(m) => {
                        spec.metric = m;
                        IndexKind::Vector(spec.resolved())
                    }
                    None => IndexKind::Inverted,
                }
            }
            "gin" if one && text && !unique => IndexKind::Text(TextIndexSpec::default()),
            "ivfflat" => {
                return self
                    .err("fenecdb's vector index is a graph: `using hnsw (emb vector_cosine_ops)`")
            }
            _ => {
                return self.err(
                    "an index is `using btree`, `hash`, `hnsw (emb vector_cosine_ops)` or `gin \
                     (to_tsvector(body))`, and `unique` over btree or hash",
                )
            }
        };
        if !with.is_empty() {
            return self.err("`with` sets an `hnsw` index's `m` and `ef_construction`");
        }
        Ok(Statement::CreateIndex {
            collection,
            field: keys.remove(0).0,
            kind,
            if_not_exists,
        })
    }

    /// SQL's `create table [if not exists] <name> (<column> <type>
    /// [constraints], ..., [table constraints])`, `table` next: `create
    /// collection`, each type as `fenec import` maps it (`sqltypes`). The
    /// row's `id` is fenecdb's: a column `id` of an integer type that is
    /// the primary key is it, and taken out. A primary key on other columns
    /// is what its rules are, `@unique` and `required`, beside the row's
    /// `id`; `not null` is `required`, `unique` `@unique`, over several
    /// columns an index over them. What fenecdb has no rule of --
    /// `default`, `references`, `check`, a sequence on another column --
    /// is refused rather than dropped, which would take writes SQL refuses.
    #[inline(never)]
    fn create_table(&mut self) -> Result<Statement> {
        self.expect_kw("table")?;
        let mut if_not_exists = false;
        if self.eat_kw("if") {
            self.expect_kw("not")?;
            self.expect_kw("exists")?;
            if_not_exists = true;
        }
        let name = self.ident()?;
        if !matches!(self.peek(), Tok::LParen) {
            return Err(self.taught(
                "a table is made of its columns, `create table t (a int, ...)`: a query's rows \
                 go in with `insert t $1`",
            ));
        }
        self.next();
        let mut fields: Vec<Field> = Vec::new();
        let mut id = None;
        let mut primary: Option<Vec<String>> = None;
        let mut uniques: Vec<Vec<String>> = Vec::new();
        let mut keys = 0;
        loop {
            if self.eat_kw("constraint") {
                self.ident()?;
            }
            if self.eat_kw("primary") {
                self.expect_kw("key")?;
                primary = Some(self.names()?);
                keys += 1;
            } else if self.peek_kw("unique") {
                self.next();
                uniques.push(self.names()?);
            } else if ["foreign", "check", "exclude"]
                .iter()
                .any(|w| self.peek_kw(w))
            {
                return self.err(NO_RULE);
            } else {
                let (field, pk, serial) = self.sql_column()?;
                keys += pk as usize;
                if field.name.eq_ignore_ascii_case("id") {
                    id = Some((pk, serial || matches!(field.ty, DataType::Int)));
                } else {
                    if serial {
                        return self.err(
                            "a sequence counts a row's `id` alone: a column `id`, the primary \
                             key, is it",
                        );
                    }
                    if pk {
                        primary = Some(vec![field.name.clone()]);
                    }
                    fields.push(field);
                }
            }
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        self.expect(Tok::RParen)?;
        if keys > 1 {
            return self
                .err("a table has one primary key: over several columns, `primary key (a, b)`");
        }
        let pk_id = matches!(&primary, Some(k) if k.len() == 1 && k[0].eq_ignore_ascii_case("id"));
        match id {
            Some((pk, int)) if !(pk || pk_id) || !int => {
                return self.err(
                    "a row's `id` is an int fenecdb makes, a column `id` an integer primary key; \
                     another key is a field, `code text primary key`",
                )
            }
            None if pk_id => {
                return self.err("`primary key (id)` names a column `id`, which the table declares")
            }
            _ => {}
        }
        let mut composites = Vec::new();
        if let Some(keys) = primary.filter(|_| !pk_id) {
            for f in fields.iter_mut().filter(|f| keys.contains(&f.name)) {
                f.required = true;
            }
            uniques.push(keys);
        }
        for keys in uniques {
            match &keys[..] {
                [one] => match fields.iter_mut().find(|f| f.name == *one) {
                    Some(f) => f.index = IndexKind::UNIQUE,
                    None => return self.err(format!("`unique ({one})` names no column")),
                },
                _ => composites.push(Composite {
                    keys: keys.into_iter().map(|k| (k, false)).collect(),
                    unique: true,
                }),
            }
        }
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

    /// A column of SQL's `create table`: its field, whether it is the
    /// primary key, and whether a sequence counts it (`serial`,
    /// `autoincrement`, `generated ... as identity`).
    #[inline(never)]
    fn sql_column(&mut self) -> Result<(Field, bool, bool)> {
        let name = self.ident()?;
        // A FenecQL type, `vector<384>` or `[text]`, as FenecQL reads it.
        let fenec = matches!(self.peek(), Tok::LBracket)
            || matches!(self.toks.get(self.i + 1).map(|t| &t.tok), Some(Tok::Lt));
        let (mut ty, mut nocase, mut serial): (DataType, bool, bool);
        (nocase, serial) = (false, false);
        if fenec {
            ty = self.data_type()?;
        } else {
            const ENDS: [&str; 13] = [
                "primary",
                "array",
                "not",
                "null",
                "unique",
                "check",
                "default",
                "collate",
                "references",
                "generated",
                "constraint",
                "autoincrement",
                "auto_increment",
            ];
            let (mut words, mut args) = (String::new(), Vec::new());
            loop {
                match self.peek().clone() {
                    Tok::Ident(w) if !ENDS.iter().any(|k| w.eq_ignore_ascii_case(k)) => {
                        words.push_str(if words.is_empty() { "" } else { " " });
                        words.push_str(&w.to_ascii_lowercase());
                        self.next();
                    }
                    Tok::LParen if !words.is_empty() => {
                        self.next();
                        while !matches!(self.peek(), Tok::RParen | Tok::Eof) {
                            if let Tok::Int(n) = self.next() {
                                args.push(n);
                            }
                        }
                        self.expect(Tok::RParen)?;
                    }
                    _ => break,
                }
            }
            if words.is_empty() {
                return Err(self.taught("a column has a type: `<name> <type>`"));
            }
            serial = words.ends_with("serial");
            let mapped = match serial {
                true => crate::sqltypes::Mapped::Type(DataType::Int, false),
                false => crate::sqltypes::postgres(&words, &args)
                    .unwrap_or_else(|| crate::sqltypes::sqlite(&words)),
            };
            (ty, nocase) = match mapped {
                crate::sqltypes::Mapped::Type(t, n) => (t, n),
                crate::sqltypes::Mapped::Not(why) => {
                    use crate::sqltypes::Why;
                    return self.err(match why {
                        Why::Decimal => format!(
                            "`{words}`: fenecdb has no decimal -- `int` of the smallest unit, \
                             cents, keeps it exact, `float` rounds it"
                        ),
                        Why::NoDimension => format!("`{words}` has its dimension: `vector(384)`"),
                        Why::Untyped => format!(
                            "`{words}` is no type here: `int`, `float`, `text`, `bool`, \
                             `timestamp`, `bytea`, `jsonb`, `vector(n)`, an array `text[]`"
                        ),
                    });
                }
            };
            // `text[]`, `integer array`, `int array[3]`: a list, its length
            // not held, as PostgreSQL holds none.
            let array = self.eat_kw("array");
            // `[3]` is a vector of one to the lexer.
            if matches!(self.peek(), Tok::Vector(v) if v.len() == 1) {
                self.next();
                ty = DataType::List(Box::new(ty));
            } else if matches!(self.peek(), Tok::LBracket) {
                self.next();
                if matches!(self.peek(), Tok::Int(_)) {
                    self.next();
                }
                self.expect(Tok::RBracket)?;
                ty = DataType::List(Box::new(ty));
            } else if array {
                ty = DataType::List(Box::new(ty));
            }
        }
        let mut field = Field::new(name, ty);
        if nocase {
            field = field.nocase();
        }
        let mut pk = false;
        loop {
            if self.eat_kw("primary") {
                self.expect_kw("key")?;
                pk = true;
                if !self.eat_kw("asc") {
                    self.eat_kw("desc");
                }
            } else if self.eat_kw("not") {
                self.expect_kw("null")?;
                field.required = true;
            } else if self.eat_kw("null") {
            } else if self.eat_kw("unique") {
                field.index = IndexKind::UNIQUE;
            } else if self.eat_kw("autoincrement") || self.eat_kw("auto_increment") {
                serial = true;
            } else if self.eat_kw("constraint") {
                self.ident()?;
            } else if self.eat_kw("generated") {
                // `generated always as identity`, `by default as identity`:
                // a sequence; `generated always as (expr) stored`, none.
                while !self.peek_kw("as") && !matches!(self.peek(), Tok::Eof) {
                    self.next();
                }
                self.expect_kw("as")?;
                if !self.eat_kw("identity") {
                    return self.err(NO_RULE);
                }
                serial = true;
            } else if self.eat_kw("collate") {
                let c = self.ident()?;
                match c.to_ascii_lowercase().as_str() {
                    "nocase" => field = field.nocase(),
                    "c" | "binary" | "posix" => {}
                    _ => match Collation::named(&c) {
                        Some(c) => field = field.collated(c),
                        None => {
                            return self.err(format!(
                                "unknown collation `{c}`: a text compares by its bytes, \
                                 `nocase` by its case fold, and `und` and `tr` by Unicode's \
                                 order and Turkish's"
                            ))
                        }
                    },
                }
            } else if ["default", "references", "check"]
                .iter()
                .any(|w| self.peek_kw(w))
            {
                return self.err(NO_RULE);
            } else {
                break;
            }
        }
        Ok((field, pk, serial))
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
        #[cfg(not(feature = "statements"))]
        if self.peek_kw("statement") {
            return self.err(UNBUILT);
        }
        #[cfg(feature = "statements")]
        if self.eat_kw("statement") {
            let mut if_exists = false;
            if self.eat_kw("if") {
                self.expect_kw("exists")?;
                if_exists = true;
            }
            return Ok(Statement::DropStatement {
                name: self.statement_name()?,
                if_exists,
            });
        }
        #[cfg(feature = "statements")]
        if self.peek_kw("function") || self.peek_kw("procedure") {
            return self.err("a named statement is dropped with `drop statement <name>`");
        }
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
        self.no_table("drop")?;
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
        self.no_table("alter")?;
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
        } else if self.named_documents(&mut docs_param)? {
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
        } else if insert && !matches!(self.peek(), Tok::LBrace) {
            docs = self.values()?;
        } else {
            docs.push(self.object()?);
            // consecutive objects are accepted too: put docs {..} {..}, and
            // with commas between them, a list without its brackets.
            loop {
                match (self.peek(), self.toks.get(self.i + 1).map(|t| &t.tok)) {
                    (Tok::LBrace, _) => {}
                    (Tok::Comma, Some(Tok::LBrace)) => self.i += 1,
                    _ => break,
                }
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
        let mut if_absent = self.eat_kw("if");
        let mut else_set = None;
        if if_absent {
            self.expect_kw("absent")?;
            if self.eat_kw("else") {
                self.expect_kw("set")?;
                else_set = Some(self.object()?);
            }
        }
        let mut conflict = None;
        if !if_absent && self.eat_kw("on") {
            if_absent = true;
            (else_set, conflict) = self.on_conflict(&collection)?;
        }
        // `require` and `returning`, either first.
        let mut require = None;
        let mut returning = None;
        loop {
            if self.peek_kw("require") {
                require = self.require()?;
            } else if self.eat_kw("returning") {
                returning = Some(self.projection_list()?);
            } else {
                break;
            }
        }
        let pick = (returning.is_some() || conflict.is_some()).then(|| {
            Box::new(Pick {
                returning,
                conflict,
                ..Pick::default()
            })
        });
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

    /// SQL's `(<a>, <b>) values (..), (..)` after `insert into <name>`: a
    /// document a row, each value its column's -- `insert <name> {a: ..,
    /// b: ..}`. The columns are named: the parser reads no schema, and a
    /// collection's fields have no order a statement could lean on, `id`
    /// not among them and a field added going last.
    fn values(&mut self) -> Result<Vec<Vec<(String, Expr)>>> {
        // `values {a: 1}, {a: 2}`: documents, which name their fields. Each
        // refusal here says what it found (`taught`), so one at the text's
        // end is one a next line moves, and the shell reads it as open.
        if self.eat_kw("values") {
            if !matches!(self.peek(), Tok::LBrace) {
                return Err(self.taught(NAMED));
            }
            let mut docs = vec![self.object()?];
            while matches!(self.peek(), Tok::Comma) {
                self.next();
                docs.push(self.object()?);
            }
            return Ok(docs);
        }
        let cols = match self.peek() {
            Tok::LParen => self.names()?,
            _ if self.peek_kw("select") || self.peek_kw("get") => vec![],
            _ => return Err(self.taught(NAMED)),
        };
        if self.peek_kw("select") || self.peek_kw("get") {
            return self.err(
                "FenecQL inserts documents, not a query's rows: read them with `get`, and write \
                 them with `insert <collection> $1`, a parameter holding them",
            );
        }
        self.expect_kw("values")?;
        let mut docs = Vec::new();
        loop {
            self.expect(Tok::LParen)?;
            let mut doc = Vec::with_capacity(cols.len());
            loop {
                let v = self.expr()?;
                doc.push((cols.get(doc.len()).cloned().unwrap_or_default(), v));
                if !matches!(self.peek(), Tok::Comma) {
                    break;
                }
                self.next();
            }
            if doc.len() != cols.len() {
                return self.err(format!(
                    "a row of {} values for {} fields",
                    doc.len(),
                    cols.len()
                ));
            }
            self.expect(Tok::RParen)?;
            docs.push(doc);
            if !matches!(self.peek(), Tok::Comma) {
                return Ok(docs);
            }
            self.next();
        }
    }

    /// SQL's `on conflict [(<key>, ...)] do nothing` and `do update set
    /// ...`, `on` read: FenecQL's `if absent` and `if absent else set
    /// {...}` -- the pairs the `else set` holds -- and the target, which
    /// the engine holds to the one unique key the collection's rows can
    /// be found by (`Schema::arbiter`): `if absent` is held by the id or
    /// any `@unique` value, and with two the target and it would part.
    /// With no target, SQLite's reading, they map as they are.
    /// `excluded.f` is `new.f`, the document's `f`, and `<name>.f` the
    /// row's `f`.
    #[allow(clippy::type_complexity)]
    fn on_conflict(
        &mut self,
        collection: &str,
    ) -> Result<(Option<Vec<(String, Expr)>>, Option<Vec<String>>)> {
        self.expect_kw("conflict")?;
        let target = match self.peek() {
            Tok::LParen => Some(self.names()?),
            _ => None,
        };
        if !self.eat_kw("do") {
            return Err(self.taught(
                "an upsert names its unique field, or none: `on conflict (email) do ...`, or \
                 `on conflict do nothing`",
            ));
        }
        if self.eat_kw("nothing") {
            return Ok((None, target));
        }
        self.expect_kw("update")?;
        self.expect_kw("set")?;
        let mut set = self.assignments()?;
        if self.peek_kw("where") {
            return self.err("FenecQL's upsert sets the row it finds held, with no `where`");
        }
        for (_, e) in &mut set {
            let _ = e.rewrite(&mut |e| {
                if let Expr::Field(f) = e {
                    match f.split_once('.') {
                        Some((t, rest)) if t.eq_ignore_ascii_case("excluded") => {
                            *f = format!("new.{rest}")
                        }
                        Some((t, rest)) if t == collection => *f = rest.to_string(),
                        _ => {}
                    }
                }
                Ok(false)
            });
        }
        Ok((Some(set), target))
    }

    /// SQL's `a = <expr>, b = <expr>` after `set`: an update's pairs, as
    /// `{a: <expr>, b: <expr>}` holds them.
    fn assignments(&mut self) -> Result<Vec<(String, Expr)>> {
        let mut pairs = Vec::new();
        loop {
            let key = self.path()?;
            self.expect(Tok::Eq)?;
            pairs.push((key, self.expr()?));
            if !matches!(self.peek(), Tok::Comma) {
                return Ok(pairs);
            }
            self.next();
        }
    }

    /// `require <n>` after a write: it must write exactly `n` rows, or it is
    /// refused and its block put back. A clause of the write rather than an
    /// `assert` statement of its own: the count is the write's, known under
    /// the lock that wrote it, with no second read to race or to scope.
    ///
    /// `else '<reason>'` after it says why, in the refusal's words and its
    /// own `reason`: a text written in the statement, never a parameter --
    /// as `n` is never one, it is the statement's contract, which a caller
    /// tells one refusal from another by, and a value bound at each run
    /// would let whoever binds them say what refused. It is parsed with the
    /// text, so a statement the parse cache holds holds its reason.
    /// `require 1 else` was "unknown command `else`" before, so no text
    /// that parsed reads otherwise.
    fn require(&mut self) -> Result<Option<Require>> {
        if !self.eat_kw("require") {
            return Ok(None);
        }
        let n = match self.int()? {
            n if n >= 0 => n as u64,
            n => return self.err(format!("`require` takes a count of rows, not {n}")),
        };
        if !self.eat_kw("else") {
            return Ok(Some(Require {
                n,
                reason: Box::default(),
            }));
        }
        let reason = match self.peek() {
            Tok::Str(r) | Tok::Single(r) => r.clone(),
            Tok::Param(_) => return self.err(REASON_PARAM),
            // At the text's end, "end of input": a next line moves it.
            _ => return Err(self.taught(REASON)),
        };
        if reason.is_empty() {
            return self.err(REASON_EMPTY);
        }
        if reason.len() > MAX_REASON {
            return self.err(format!(
                "a `require`'s reason is at most {MAX_REASON} bytes, not {}",
                reason.len()
            ));
        }
        // The refusal quotes its statement with backticks after the reason,
        // and `Error::reason` reads the reason back as the message up to the
        // first `` (` ``: one in the reason would end it there. A quoted
        // collection name may hold one, so the count's words cannot be
        // where the reason's end is looked for.
        if reason.bytes().any(|b| b == b'`') {
            return self.err(REASON_TICK);
        }
        self.next();
        Ok(Some(Require {
            n,
            reason: reason.into_boxed_str(),
        }))
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
                Tok::Ident(s) | Tok::Str(s) | Tok::Single(s) => s,
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
        // SQL's `select distinct a, b`: each row of the list once, a group
        // of its items (`distinct`). In `in (select distinct ...)` it
        // changes nothing, and is passed over.
        let mut distinct = false;
        if self.peek_kw("distinct")
            && matches!(&self.toks[self.i - 1].tok, Tok::Ident(w) if w.eq_ignore_ascii_case("select"))
            && matches!(
                self.toks.get(self.i + 1).map(|t| &t.tok),
                Some(Tok::Ident(_) | Tok::Star)
            )
        {
            self.next();
            distinct = self.subqueries == 0;
            // Read as a list of one field named `distinct` before SQL was,
            // which SQL refuses.
            if self.peek_kw("from") {
                return self.err(
                    "`distinct` makes each row of the list once, and takes the list: \
                     `select distinct city from users`",
                );
            }
            if self.peek_kw("on") {
                return self.err(
                    "`distinct on` is a group: `select city, first(name by signup) as name ... \
                     group by city`",
                );
            }
        }
        let mut sql = false;
        if !self.peek_kw("from") {
            let save = self.i;
            match self.projection_before_from()? {
                Some(l) => (list, sql) = (l, true),
                None => self.i = save,
            }
        }
        sql |= self.eat_kw("from");
        let at = self.i;
        let collection = self.ident()?;
        // SQL's `select transfer(...)`, a routine's value in a row, and
        // `select * from transfer(...)`, a table it makes: a collection's
        // name takes no arguments, and a named statement runs with `call`.
        // Refused at the name, which is not consumed. With no `from`, the
        // name may be a function of the list, `select count(*)` with its
        // collection left out: both are said.
        #[cfg(feature = "statements")]
        if matches!(self.peek(), Tok::LParen) {
            self.i = at;
            return Err(self.routine_read(sql, &collection));
        }
        // SQL's `from orders o join customers c on ...`, or an alias alone.
        let from = match sql {
            true => self.tables_after(&collection)?,
            false => None,
        };
        let around = match &from {
            Some(f) => Around::Tables(f.tables.clone()),
            None => Around::At(at),
        };
        let outer = std::mem::replace(&mut self.outer, around);
        let sel = self.get_clauses(collection, list, from, distinct, sql);
        self.outer = outer;
        sel
    }

    /// SQL's `order by 2` and `group by 1`: the list's second item and its
    /// first, where SQL's order names the list -- a number there was a
    /// key the same in every row, which ordered nothing.
    #[inline(never)]
    fn ordinals(&self, sel: &mut Select, (group_by, order_by): (bool, bool)) -> Result<()> {
        let names: Vec<String> = match (&sel.project, sel.aggregate.is_empty()) {
            (_, false) => sel.aggregate.iter().map(|c| c.name.clone()).collect(),
            (Some(p), true) => p.clone(),
            (None, true) => Vec::new(),
        };
        let item = |k: &Expr| match k {
            Expr::Lit(Value::Int(n)) => Some(*n),
            _ => None,
        };
        let at = |n: i64| -> Result<String> {
            match names.get((n as usize).wrapping_sub(1)).filter(|_| n > 0) {
                Some(name) => Ok(name.clone()),
                None if names.is_empty() => self.err(format!(
                    "a key `{n}` names an item of the list by its place, and `*` names none: \
                     name the columns, or order by the field"
                )),
                None => self.err(format!(
                    "a key `{n}` names an item of the list by its place, and the list holds {}",
                    names.len()
                )),
            }
        };
        for g in sel.group.iter_mut().filter(|_| group_by) {
            if let Some(n) = item(g) {
                *g = Expr::Field(at(n)?);
            }
        }
        let grouped = !sel.aggregate.is_empty() || !sel.group.is_empty();
        for s in sel.order.iter_mut().filter(|_| order_by) {
            let Some(n) = s.expr.as_ref().and_then(item) else {
                continue;
            };
            let name = at(n)?;
            // Over rows, a column worked out is ordered by what it works out.
            s.expr = match sel.computed.iter().find(|c| c.name == name) {
                Some(c) if !grouped => Some(c.expr.duplicate()),
                _ => None,
            };
            s.field = match s.expr {
                Some(_) => String::new(),
                None => name,
            };
        }
        Ok(())
    }

    /// The rest of a `get` from its collection on, `from` SQL's tables
    /// where the statement has them: its clauses, a join read as the
    /// statement it means.
    fn get_clauses(
        &mut self,
        collection: String,
        list: SelectList,
        from: Option<From>,
        distinct: bool,
        sql: bool,
    ) -> Result<Statement> {
        let mut sel = Select {
            collection,
            project: list.project,
            aggregate: list.aggregate,
            marks: list.marks,
            computed: list.computed,
            ..Default::default()
        };

        // Whether the `group` and the `order` were written with SQL's `by`.
        let mut by = (false, false);
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
                // SQL's `group by 1` names the list's first item; FenecQL's
                // `group 1`, written before SQL was read, keeps its tree.
                by.0 |= self.peek_kw("by");
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
                    metric: None,
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
                by.1 |= self.peek_kw("by");
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
        // SQL's join, where the clauses stop -- `join`, an alias before it,
        // a comma between two collections -- and an alias alone, answered
        // before the check, whose refusal of a grouping over the join's
        // columns came first and hid the hint (the agent eval: 11 of 32
        // joins).
        if self.joined() {
            return self.err(JOIN);
        }
        // An alias only where SQL's clause follows it (`from users u group
        // by u.city`): a word at the end of the text may be a statement
        // still being typed, which the shell holds back as one beginning
        // no statement, refused where it stands.
        if let (Tok::Ident(w), Some(Tok::Ident(k))) =
            (self.peek(), self.toks.get(self.i + 1).map(|t| &t.tok))
        {
            if !COMMANDS.iter().any(|c| w.eq_ignore_ascii_case(c))
                && ["where", "group", "order", "limit", "having"]
                    .iter()
                    .any(|c| k.eq_ignore_ascii_case(c))
            {
                return Err(self.alias(&w.to_ascii_lowercase()));
            }
        }
        if sql {
            self.ordinals(&mut sel, by)?;
        }
        if let Some(from) = from {
            sel = match crate::join::relate(sel, from, distinct) {
                Ok(s) => s,
                Err(e) => return self.err(e),
            };
        }
        if distinct {
            self.distinct(&mut sel)?;
        }
        keys_alone(&mut sel);
        if !sel.order.is_empty() {
            self.distance(&mut sel)?;
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

    /// After SQL's `from <collection>`: its alias, and the tables joined to
    /// it -- `join u [as] b on ...`, `left [outer] join`, `join u using
    /// (k)`, `, u` -- or `None` where there is neither, a statement read as
    /// it was before SQL's joins were.
    ///
    /// This and the other readings of SQL's forms are out of line: inlined
    /// into `get` and `create`, which every statement passes through, they
    /// left the browser module 0.4 KB brotli larger.
    #[inline(never)]
    fn tables_after(&mut self, collection: &str) -> Result<Option<From>> {
        let mut tables = vec![Table {
            collection: collection.to_string(),
            alias: self.table_alias()?,
        }];
        let mut joins = Vec::new();
        loop {
            let left = if self.peek_kw("join") || self.peek_kw("inner") {
                self.eat_kw("inner");
                false
            } else if self.peek_kw("left") {
                self.next();
                self.eat_kw("outer");
                true
            } else if matches!(self.peek(), Tok::Comma) {
                self.next();
                let collection = self.ident()?;
                let alias = self.table_alias()?;
                tables.push(Table { collection, alias });
                joins.push(Join {
                    left: false,
                    on: None,
                });
                continue;
            } else if ["right", "full", "cross", "natural"]
                .iter()
                .any(|w| self.peek_kw(w))
            {
                return self.err(
                    "a join is an inner JOIN or a LEFT JOIN, a RIGHT JOIN a LEFT JOIN with its \
                     tables the other way round",
                );
            } else {
                break;
            };
            self.expect_kw("join")?;
            let collection = self.ident()?;
            let alias = self.table_alias()?;
            let on = if self.eat_kw("on") {
                let on = self.expr()?;
                // `on o.customer_id` where the text ends: its `=` may be on
                // the next line, which a refusal of what it found moves.
                if self.at_eof() && !matches!(&on, Expr::Cmp(CmpOp::Eq, ..) | Expr::And(..)) {
                    return Err(self.taught("a JOIN pairs its columns by `=`"));
                }
                on
            } else if self.eat_kw("using") {
                self.using(tables.last().unwrap_or(&tables[0]), &collection, &alias)?
            } else {
                return Err(self.taught(
                    "a JOIN names the columns it pairs: `join customers c on c.id = \
                     o.customer_id`",
                ));
            };
            tables.push(Table { collection, alias });
            joins.push(Join { left, on: Some(on) });
        }
        if tables.len() == 1 && tables[0].alias.is_none() {
            return Ok(None);
        }
        Ok(Some(From { tables, joins }))
    }

    /// `using (a, b)`, its word read: `prev.a = this.a and prev.b =
    /// this.b`, each table named as the statement names it.
    #[inline(never)]
    fn using(&mut self, prev: &Table, name: &str, alias: &Option<String>) -> Result<Expr> {
        let at = |t: &Table| t.alias.clone().unwrap_or_else(|| t.collection.clone());
        let (a, b) = (at(prev), alias.clone().unwrap_or_else(|| name.to_string()));
        let mut out: Option<Expr> = None;
        for k in self.names()? {
            let eq = Expr::Cmp(
                CmpOp::Eq,
                Box::new(Expr::Field(format!("{a}.{k}"))),
                Box::new(Expr::Field(format!("{b}.{k}"))),
            );
            out = Some(match out {
                None => eq,
                Some(e) => Expr::And(Box::new(e), Box::new(eq)),
            });
        }
        out.ok_or_else(|| self.error(&"`using` names the columns the tables share"))
    }

    /// A table's alias after its name in SQL's `from`: `[as] <name>`,
    /// where the word is no clause, command or join's.
    fn table_alias(&mut self) -> Result<Option<String>> {
        if self.eat_kw("as") {
            return self.ident().map(Some);
        }
        Ok(self.bare_alias())
    }

    /// SQL's `count(*) n`, a word next: a name without `as` where the list
    /// goes on or ends at `from` after it -- at the text's end the next
    /// line says which (`cut`).
    #[inline(never)]
    fn item_alias(&mut self) -> Option<String> {
        let after = self.toks.get(self.i + 1).map(|t| &t.tok);
        self.cut |= matches!(after, Some(Tok::Eof));
        let listed = matches!(after, Some(Tok::Comma))
            || matches!(after, Some(Tok::Ident(w)) if w.eq_ignore_ascii_case("from"));
        match listed {
            true => self.bare_alias(),
            false => None,
        }
    }

    /// A name SQL writes without `as` -- `from orders o`, `count(*) n` --
    /// next: a word no clause or statement begins with.
    fn bare_alias(&mut self) -> Option<String> {
        match self.peek() {
            Tok::Ident(w)
                if !w.contains('.')
                    && !TAKEN
                        .iter()
                        .chain(&COMMANDS)
                        .any(|k| w.eq_ignore_ascii_case(k))
                    // In a body, `end` closes it.
                    && !(self.defining() && w.eq_ignore_ascii_case("end")) =>
            {
                let a = w.clone();
                self.next();
                Some(a)
            }
            _ => None,
        }
    }

    /// SQL's `select distinct`: each row once -- a group of the list's
    /// items, which then answers each set of their values once; over `*`,
    /// whose every row is its own by its `id`, the rows as they are; over
    /// an aggregating list, its rows, already one a group where the list
    /// holds every key.
    #[inline(never)]
    fn distinct(&self, sel: &mut Select) -> Result<()> {
        if !sel.aggregate.is_empty() || !sel.group.is_empty() {
            let listed = |k: &Expr| {
                sel.aggregate
                    .iter()
                    .any(|c| is_key(k, &c.expr) || matches!(k, Expr::Field(f) if *f == c.name))
                    || sel
                        .project
                        .iter()
                        .flatten()
                        .any(|p| matches!(k, Expr::Field(f) if f == p))
            };
            return match sel.group.iter().all(listed) {
                true => Ok(()),
                false => self.err("`distinct` over groups: list their keys"),
            };
        }
        let Some(names) = sel.project.take() else {
            return Ok(());
        };
        for name in names {
            sel.group.push(Expr::Field(name.clone()));
            let expr = match sel.computed.iter().position(|c| c.name == name) {
                Some(i) => sel.computed.remove(i).expr,
                None => Expr::Field(name.clone()),
            };
            sel.aggregate.push(Column { expr, name });
        }
        Ok(())
    }

    /// SQL's `order by emb <-> $1 limit 10` -- pgvector's distance, the
    /// one key -- as `near emb $1 limit 10`: the field's index must measure
    /// the operator's distance, which the engine holds it to
    /// ([`Near::metric`]). The query vector may be pgvector's text, `'[1,
    /// 2, 3]'`.
    #[inline(never)]
    fn distance(&self, sel: &mut Select) -> Result<()> {
        let Some(at) = sel.order.iter().position(crate::join::sort_is_distance) else {
            return Ok(());
        };
        if sel.order.len() > 1 || !sel.order[at].asc || sel.near.is_some() || sel.matcher.is_some()
        {
            return self.err(
                "pgvector's distance orders alone, nearest first: `order by emb <-> $1 limit 10`",
            );
        }
        let Some(Expr::Call(op, args)) = sel.order.remove(0).expr else {
            return Ok(());
        };
        let metric = match op.as_str() {
            "<->" => Metric::L2,
            "<=>" => Metric::Cosine,
            _ => Metric::Dot,
        };
        let mut args = args.into_iter();
        let (Some(Expr::Field(field)), Some(mut vector)) = (args.next(), args.next()) else {
            return self.err("pgvector's distance is between a field and a vector: `emb <-> $1`");
        };
        // A sparse vector's text, `'{1:0.5}/30522'`, is read as it is.
        if let Expr::Lit(Value::Text(t)) = &vector {
            if !t.trim_ascii_start().starts_with('{') {
                vector = Expr::Lit(Value::Vector(pgvector_text(t).ok_or_else(|| {
                    self.error(&"a vector written as text is pgvector's `'[1, 2, 3]'`")
                })?));
            }
        }
        sel.near = Some(Near {
            field,
            vector,
            ef: None,
            exact: false,
            metric: Some(metric),
        });
        Ok(())
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
            let ti = self.i;
            let e = match self.distance_key()? {
                Some(e) => e,
                None => self.expr()?,
            };
            if self.alone(ti) {
                return Err(self.quoted(ti));
            }
            let (field, expr) = match e {
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
            // SQL's `nulls first` and `nulls last`, where they say what
            // the order does here: nulls before the values ascending,
            // after them descending.
            if self.eat_kw("nulls") {
                let nulls = if asc { "first" } else { "last" };
                if !self.eat_kw(nulls) {
                    return Err(self.taught(
                        "a null comes first ascending and last descending: to turn that round, \
                         order by `<key> is null` first",
                    ));
                }
            }
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

    /// pgvector's `emb <-> $1` as an order's key, the field on either side:
    /// a call named by the operator, which `distance` reads as `near` --
    /// and which nothing else runs ([`crate::join::sort_is_distance`]).
    #[inline(never)]
    fn distance_key(&mut self) -> Result<Option<Expr>> {
        let at = |k: usize| self.toks.get(self.i + k).map(|t| t.tok.clone());
        let Some(Tok::Distance(op)) = at(1) else {
            return Ok(None);
        };
        let (first, after) = (at(0), at(2));
        let (field, vector) = match (first, after) {
            (Some(Tok::Ident(f)), _) => {
                self.i += 2;
                (f, self.arith()?)
            }
            (_, Some(Tok::Ident(f))) => {
                let v = self.arith()?;
                self.i += 2;
                (f, v)
            }
            _ => return Ok(None),
        };
        Ok(Some(Expr::Call(
            op.to_string(),
            vec![Expr::Field(field), vector],
        )))
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
                if l.order.iter().any(crate::join::sort_is_distance) {
                    return self.err("a `lookup` level orders its children by fields");
                }
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
        // `select product group product`: the keys alone, as a `get`'s.
        if !l.group.is_empty() && l.aggregate.is_empty() {
            for name in l.project.take().into_iter().flatten() {
                let expr = Expr::Field(name.clone());
                l.aggregate.push(Column { expr, name });
            }
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

    /// `Ok(None)` where the words after `select` are no list followed by
    /// `from`; a double-quoted name among them is refused there, which a
    /// collection's name after `select` never is.
    fn projection_before_from(&mut self) -> Result<Option<SelectList>> {
        let list = self.select_list();
        self.cut |= self.at_eof();
        match list {
            Ok(list) => Ok(self.eat_kw("from").then_some(list)),
            Err(Error::Query(m))
                if m.ends_with(QUOTED) || m.ends_with(ITEM_QUERY) || m.ends_with(DISTANCE) =>
            {
                Err(Error::Query(m))
            }
            // A `from` ahead, outside brackets: SQL's list, whose own
            // refusal says what is wrong with it. Read as FenecQL's
            // `select <collection>`, an unnamed `sum(case ...)` or `count(*)
            // n` was answered that FenecQL has no `join` (the agent eval).
            Err(e) if self.sees_from() => Err(e),
            Err(_) => Ok(None),
        }
    }

    /// Whether a `from` follows, at the depth of the next token.
    fn sees_from(&self) -> bool {
        let mut depth = 0usize;
        for t in &self.toks[self.i..] {
            match &t.tok {
                Tok::LParen | Tok::LBracket => depth += 1,
                Tok::RParen | Tok::RBracket if depth == 0 => return false,
                Tok::RParen | Tok::RBracket => depth -= 1,
                Tok::Ident(w) if depth == 0 && w.eq_ignore_ascii_case("from") => return true,
                Tok::Semi => return false,
                _ => {}
            }
        }
        false
    }

    /// `group symbol, bucket(at, 1m)`: the keys, each an expression over the
    /// row or a name the select list gives an item.
    fn group_list(&mut self) -> Result<Vec<Expr>> {
        self.eat_kw("by");
        let mut out = Vec::new();
        loop {
            let ti = self.i;
            out.push(self.expr()?);
            if self.alone(ti) {
                return Err(self.quoted(ti));
            }
            if !matches!(self.peek(), Tok::Comma) {
                return Ok(out);
            }
            self.next();
        }
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
                // SQL's `select (select name from products where id =
                // product_id) as name`: another collection's row beside
                // each, a join's.
                if self.scalar_ahead() {
                    return self.err(ITEM_QUERY);
                }
                let ti = self.i;
                let e = self.expr()?;
                // `select "name"`: SQL's column, text here.
                if self.alone(ti) {
                    return Err(self.quoted(ti));
                }
                let name = match matches!(self.peek(), Tok::Ident(_)) {
                    true if self.eat_kw("as") => Some(self.ident()?),
                    true => self.item_alias(),
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
                    // At the text's end, an `as` on the next line names it.
                    self.cut |= self.at_eof();
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
        let at = self.i;
        let collection = self.ident()?;
        // SQL's `update <name> set a = 1, b = b + 1`, the pairs FenecQL's
        // object holds.
        let set = match self.eat_kw("set") && !matches!(self.peek(), Tok::LBrace) {
            true => self.assignments()?,
            false => self.object()?,
        };
        let (filter, require, pick) = self.write_clauses(at)?;
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
        let at = self.i;
        let collection = self.ident()?;
        let (filter, require, pick) = self.write_clauses(at)?;
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
    fn write_clauses(
        &mut self,
        at: usize,
    ) -> Result<(Option<Expr>, Option<Require>, Option<Box<Pick>>)> {
        let (mut filter, mut require, mut pick) = (None, None, Pick::default());
        loop {
            if self.eat_kw("where") {
                // The row an `exists` reads around it is the collection's.
                let outer = std::mem::replace(&mut self.outer, Around::At(at));
                let e = self.expr();
                self.outer = outer;
                filter = Some(e?);
                continue;
            }
            if self.eat_kw("order") {
                self.order_list(&mut pick.order)?;
                if pick.order.iter().any(crate::join::sort_is_distance) {
                    return self.err(
                        "pgvector's distance ranks a `get`: `where id in (get ... near <field> \
                         <vector> limit 1)`",
                    );
                }
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

    // ---------------------------------------------------- named statements

    /// Whether a definition's `check` or body is being read: never
    /// without named statements, so the module's parser asks nothing.
    fn defining(&self) -> bool {
        cfg!(feature = "statements") && self.def.is_some()
    }

    /// `put <c> $docs` in a definition's body: a parameter's documents,
    /// which the body refuses by its tree. Whether it read one.
    #[cfg(feature = "statements")]
    fn named_documents(&mut self, docs_param: &mut Option<usize>) -> Result<bool> {
        if !matches!(self.peek(), Tok::Named(_)) {
            return Ok(false);
        }
        *docs_param = Some(self.named_documents_param()?);
        Ok(true)
    }

    #[cfg(feature = "statements")]
    #[cold]
    #[inline(never)]
    fn named_documents_param(&mut self) -> Result<usize> {
        let Tok::Named(n) = self.peek().clone() else {
            return self.err("a parameter's documents");
        };
        let i = self.named(&n)?;
        self.next();
        Ok(i)
    }

    /// `$from` read as a value: the parameter it names. Out of line, as
    /// every path only a definition takes: inside `primary`, which every
    /// value of every text passes, it weighed on the texts that hold none.
    #[cfg(feature = "statements")]
    #[cold]
    #[inline(never)]
    fn named_param(&mut self, n: &str) -> Result<Expr> {
        let i = self.named(n)?;
        self.next();
        Ok(Expr::Param(i))
    }

    /// `$<i>` in a definition past the parameters it declares.
    #[cfg(feature = "statements")]
    #[cold]
    #[inline(never)]
    fn past_params(&self, i: usize, declared: usize) -> Error {
        self.error(&format!(
            "`${}`: the statement declares {declared} parameters, read by name or by place, and \
             its claims by name, `$jwt.<claim>`",
            i + 1
        ))
    }

    /// SQL's `select transfer(...)` and `select * from transfer(...)`,
    /// refused: `sql` where the list and its `from` were read.
    #[cfg(feature = "statements")]
    #[cold]
    #[inline(never)]
    fn routine_read(&self, sql: bool, c: &str) -> Error {
        self.error(&match sql {
            true => format!(
                "`{c}(...)` names no collection: a routine's rows are no table here, and a named \
                 statement runs with `call {c}(...)`"
            ),
            false => format!(
                "`{c}(...)` names no collection: a function reads a collection's rows, `select \
                 {c}(...) from <collection>`, and a named statement runs with `call {c}(...)`"
            ),
        })
    }

    #[cfg(not(feature = "statements"))]
    fn named_documents(&mut self, _: &mut Option<usize>) -> Result<bool> {
        Ok(false)
    }

    /// A named statement's name, next: one `call_<name>` fits a model's
    /// tool name with, and no word a statement begins with.
    #[cfg(feature = "statements")]
    fn statement_name(&mut self) -> Result<String> {
        let at = self.i;
        let name = self.ident()?;
        if !fenec_core::named::valid_name(&name) || is_command(&name) {
            self.i = at;
            return self.err(fenec_core::named::name_refused(&name));
        }
        Ok(name)
    }

    /// The parameter, or the claim, `$<n>` names: its place among the
    /// definition's parameters, or past them among its claims. Refused
    /// outside a definition, where parameters are numbered.
    #[cfg(feature = "statements")]
    fn named(&self, n: &str) -> Result<usize> {
        let Some(d) = self.def.as_deref() else {
            return self.err(format!(
                "`${n}` names a named statement's parameter; a statement sent alone numbers \
                 them, `$1`"
            ));
        };
        self.named_in(d, n)
    }

    #[cfg(feature = "statements")]
    fn named_in(&self, d: &Names, n: &str) -> Result<usize> {
        if n == "jwt" {
            return self
                .err("`$jwt` is the caller's token: a body reads a claim it declares, `$jwt.sub`");
        }
        if let Some(claim) = n.strip_prefix("jwt.") {
            if let Some((top, _)) = claim.split_once('.') {
                return self.err(format!(
                    "`${n}`: a claim is read whole, `$jwt.{top}`, and no path into it"
                ));
            }
            return match d.claims.iter().position(|c| c == claim) {
                Some(k) => Ok(d.params.len() + k),
                None => self.err(format!(
                    "`${n}` names no claim the statement declares: `claims ({claim} text)`"
                )),
            };
        }
        if n.contains('.') {
            return self.err(format!(
                "`${n}`: a parameter is read whole, and no path into it"
            ));
        }
        match d.params.iter().position(|p| p == n) {
            Some(i) => Ok(i),
            None => self.err(format!("`${n}` names no parameter the statement declares")),
        }
    }

    /// `create [or replace] statement ...`, at `statement`: the signature,
    /// then `as` and the body, which is held to what a body may hold
    /// ([`Definition::refused`]). The definition is the text's: no schema
    /// is read here, and nothing runs it yet.
    #[cfg(feature = "statements")]
    #[inline(never)]
    fn create_statement(&mut self, replace: bool) -> Result<Statement> {
        use fenec_core::named::*;
        if self.def.is_some() {
            return self.err(
                "a body defines no statement: a schema change is the server token's, and a \
                 statement holding one would hand it to every caller",
            );
        }
        self.next();
        let at = self.i;
        let name = self.statement_name()?;
        if !matches!(self.peek(), Tok::LParen) {
            return Err(self.taught(
                "a statement's parameters follow its name in parentheses, `()` for none: \
                 `create statement purge() ...`",
            ));
        }
        self.next();
        let mut params: Vec<Param> = Vec::new();
        while !matches!(self.peek(), Tok::RParen) {
            if params.len() == MAX_PARAMS {
                return self.err(format!(
                    "a statement declares at most {MAX_PARAMS} parameters"
                ));
            }
            let at = self.i;
            let p = self.param()?;
            if params.iter().any(|q| q.name == p.name) {
                self.i = at;
                return self.err(format!("`${}` is declared twice", p.name));
            }
            params.push(p);
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        self.expect(Tok::RParen)?;
        let (mut claims, mut public, mut scans) = (None, false, false);
        let (mut comment, mut check) = (None, None);
        let twice = |p: &Parser, what: &str| p.err(format!("`{what}` is written once"));
        loop {
            if self.peek_kw("claims") {
                if claims.is_some() {
                    return twice(self, "claims");
                }
                if check.is_some() {
                    return self.err("`claims` goes before `check`, which reads them");
                }
                self.next();
                claims = Some(self.claims()?);
            } else if self.peek_kw("public") {
                if public {
                    return twice(self, "public");
                }
                self.next();
                public = true;
            } else if self.peek_kw("scans") {
                if scans {
                    return twice(self, "scans");
                }
                self.next();
                scans = true;
            } else if self.peek_kw("comment") {
                if comment.is_some() {
                    return twice(self, "comment");
                }
                self.next();
                comment = Some(self.quoted_text(
                    "`comment` takes what the statement does, in quotes: `comment 'Moves an \
                     amount between two accounts'`",
                )?);
            } else if self.peek_kw("check") {
                if check.is_some() {
                    return twice(self, "check");
                }
                self.next();
                self.def = Some(Box::new(Names {
                    params: params.iter().map(|p| p.name.clone()).collect(),
                    claims: claims
                        .iter()
                        .flatten()
                        .map(|c: &Claim| c.name.clone())
                        .collect(),
                }));
                let at = self.i;
                let e = self.expr();
                self.def = None;
                let e = e?;
                if let Some(why) = check_refused(&e) {
                    self.i = at;
                    return self.err(why);
                }
                check = Some(e);
            } else {
                break;
            }
        }
        let claims = claims.unwrap_or_default();
        if !self.eat_kw("as") {
            return Err(self.taught(
                "a statement's body follows `as`: `as get ...`, or `as begin <statement>; ... \
                 end`",
            ));
        }
        self.def = Some(Box::new(Names {
            params: params.iter().map(|p| p.name.clone()).collect(),
            claims: claims.iter().map(|c| c.name.clone()).collect(),
        }));
        let body = self.body();
        self.def = None;
        let (body, starts, block) = body?;
        let def = Definition {
            name,
            replace,
            params,
            claims,
            public,
            scans,
            comment,
            check,
            body,
            block,
        };
        if let Some((which, why)) = def.refused() {
            // A body the text's end cut short may yet read: a `where`, a
            // list, a claim on the next line.
            self.cut |= self.at_eof();
            self.i = which.and_then(|k| starts.get(k).copied()).unwrap_or(at);
            return self.err(why);
        }
        Ok(Statement::CreateStatement(Box::new(def)))
    }

    #[cfg(not(feature = "statements"))]
    fn create_statement(&mut self, _replace: bool) -> Result<Statement> {
        self.err(UNBUILT)
    }

    /// `$name type [= <literal>]`.
    #[cfg(feature = "statements")]
    fn param(&mut self) -> Result<fenec_core::named::Param> {
        let name = match self.peek() {
            Tok::Named(n) if n == "jwt" || n.starts_with("jwt.") => {
                return self.err(
                    "`$jwt` is the caller's token: a claim is declared in `claims (sub text)`",
                )
            }
            Tok::Named(n) if fenec_core::named::valid_name(n) => n.clone(),
            Tok::Named(n) => {
                let n = n.clone();
                return self.err(format!(
                    "`${n}` is no parameter's name: ASCII letters, digits and `_`, at most {} \
                     bytes",
                    fenec_core::named::MAX_NAME
                ));
            }
            Tok::Ident(n) => {
                let n = n.clone();
                return self.err(format!("a parameter is named with its `$`: `${n} <type>`"));
            }
            _ => {
                return Err(self.taught(
                    "a parameter is `$<name> <type>`, read in the body as `$<name>` or by its \
                     place, `$1`",
                ))
            }
        };
        self.next();
        let ty = self.data_type()?;
        let mut default = None;
        if matches!(self.peek(), Tok::Eq) {
            self.next();
            let (mark, at) = (self.depth, self.i);
            let e = self.operand();
            self.depth = mark;
            default = match e? {
                Expr::Lit(v) => Some(v),
                _ => {
                    self.i = at;
                    return self.err("a parameter's default is a literal: `$n int = 10`");
                }
            };
        }
        Ok(fenec_core::named::Param { name, ty, default })
    }

    /// `(<name> <type> [from "<claim>"], ...)` after `claims`: the token's
    /// claims the body reads as `$jwt.<name>`.
    #[cfg(feature = "statements")]
    fn claims(&mut self) -> Result<Vec<fenec_core::named::Claim>> {
        use fenec_core::named::{valid_name, Claim, MAX_CLAIMS};
        self.expect(Tok::LParen)?;
        if matches!(self.peek(), Tok::RParen) {
            return self.err("`claims` declares the claims the body reads: `claims (sub text)`");
        }
        let mut out: Vec<Claim> = Vec::new();
        loop {
            if out.len() == MAX_CLAIMS {
                return self.err(format!("a statement declares at most {MAX_CLAIMS} claims"));
            }
            let at = self.i;
            let name = self.ident()?;
            if !valid_name(&name) {
                self.i = at;
                return self.err(format!(
                    "`{name}` is no claim's name here: give it one, `groups [text] from \
                     \"{name}\"`"
                ));
            }
            let ty = self.data_type()?;
            let mut from = None;
            if self.eat_kw("from") {
                from = Some(self.quoted_text(
                    "`from` names the token's claim in quotes: `from \"cognito:groups\"`",
                )?);
            }
            let claim = Claim {
                from: from.filter(|f| *f != name),
                name,
                ty,
            };
            if let Some(c) = out
                .iter()
                .find(|c| c.name == claim.name || c.claim() == claim.claim())
            {
                self.i = at;
                return self.err(format!("the claim `{}` is declared twice", c.claim()));
            }
            out.push(claim);
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        self.expect(Tok::RParen)?;
        Ok(out)
    }

    /// A text in quotes, not empty, next; refused with `why` otherwise --
    /// at the text's end as cut short, which a next line moves.
    #[cfg(feature = "statements")]
    fn quoted_text(&mut self, why: &str) -> Result<String> {
        match self.peek() {
            Tok::Single(t) | Tok::Str(t) if !t.is_empty() => {
                let t = t.clone();
                self.next();
                Ok(t)
            }
            _ => Err(self.taught(why)),
        }
    }

    /// After `as`: one statement, or `begin`, statements cut by `;`, and
    /// `end` -- with where each began, and whether it was `begin ... end`.
    /// At the text's end before `end`, refused as cut short, which the
    /// shell reads as "go on": no bracket of the lexer's holds it open.
    #[cfg(feature = "statements")]
    #[allow(clippy::type_complexity)]
    fn body(&mut self) -> Result<(Vec<Statement>, Vec<usize>, bool)> {
        if !self.eat_kw("begin") {
            let at = self.i;
            return Ok((vec![self.statement()?], vec![at], false));
        }
        let (mut out, mut starts) = (Vec::new(), Vec::new());
        loop {
            self.semis();
            if self.eat_kw("end") {
                return Ok((out, starts, true));
            }
            if self.at_eof() {
                return Err(self.taught("a body begun with `begin` ends with `end`"));
            }
            if out.len() == fenec_core::named::MAX_BODY {
                return self.err(format!(
                    "a body holds at most {} statements",
                    fenec_core::named::MAX_BODY
                ));
            }
            starts.push(self.i);
            out.push(self.statement()?);
            if !matches!(self.peek(), Tok::Semi) && !self.peek_kw("end") {
                return Err(self.taught("a body's statements are cut by `;`, and `end` closes it"));
            }
        }
    }

    /// `call <name> [{...} | $n] [as {...} | as $n]`, and SQL's `CALL
    /// <name>(<arguments>)`: the arguments are values, matched to the
    /// signature by the engine, since the parser reads no schema.
    #[cfg(feature = "statements")]
    #[inline(never)]
    fn call_statement(&mut self) -> Result<Statement> {
        use fenec_core::named::{Args, Call};
        let at = self.i;
        self.next();
        let name = match self.statement_name() {
            Ok(name) => name,
            // `call` at the text's end: a name to come, or the alias of the
            // statement before it (`select a call`, with `from t` to come),
            // which reserves no word. Not consumed, and cut short, so the
            // reading holds the two together until the next line says.
            Err(e) if self.at_eof() => {
                self.i = at;
                self.cut = true;
                return Err(e);
            }
            Err(e) => return Err(e),
        };
        // `call transfer` at a line's end: its document may come on the
        // next.
        self.cut |= self.at_eof();
        let args = match self.peek() {
            Tok::LBrace => Args::Named(self.call_document()?),
            Tok::Param(i) => {
                let i = *i;
                self.next();
                Args::Param(i)
            }
            Tok::Named(n) => {
                let n = n.clone();
                let i = self.named(&n)?;
                self.next();
                Args::Param(i)
            }
            Tok::LParen => self.sql_arguments()?,
            _ => Args::None,
        };
        let claims = match self.eat_kw("as") {
            false => None,
            true => Some(match self.peek() {
                Tok::LBrace => Args::Named(self.call_document()?),
                Tok::Param(i) => {
                    let i = *i;
                    self.next();
                    Args::Param(i)
                }
                _ => {
                    return Err(self.taught(
                        "`as` takes the claims a call is made with: `as {sub: 'alice'}`, or a \
                         parameter holding them, `as $2`",
                    ))
                }
            }),
        };
        Ok(Statement::Call(Box::new(Call { name, args, claims })))
    }

    #[cfg(not(feature = "statements"))]
    fn call_statement(&mut self) -> Result<Statement> {
        self.err(UNBUILT)
    }

    /// `{from: 1, to: $2}`: a call's arguments, or its claims, by name --
    /// each a value, none twice.
    #[cfg(feature = "statements")]
    fn call_document(&mut self) -> Result<Vec<(String, Expr)>> {
        let at = self.i;
        let pairs = self.object()?;
        for (k, (name, e)) in pairs.iter().enumerate() {
            let why = if name.contains('.') {
                format!("`{name}`: an argument is named by its parameter, and no path")
            } else if pairs[..k].iter().any(|(n, _)| n == name) {
                format!("`{name}` is given twice")
            } else if let Some(why) = not_a_value(e) {
                why
            } else {
                continue;
            };
            self.i = at;
            return self.err(why);
        }
        Ok(pairs)
    }

    /// SQL's `(1, 2, amount => 300)` after `CALL <name>`: by place, then
    /// any by name, as PostgreSQL's -- all by name the call `{...}` makes,
    /// none the call with no arguments.
    #[cfg(feature = "statements")]
    fn sql_arguments(&mut self) -> Result<fenec_core::named::Args> {
        use fenec_core::named::Args;
        self.expect(Tok::LParen)?;
        let (mut placed, mut named) = (Vec::new(), Vec::<(String, Expr)>::new());
        while !matches!(self.peek(), Tok::RParen) {
            let at = self.i;
            let by_name = matches!(self.peek(), Tok::Ident(_))
                && matches!(self.toks.get(self.i + 1).map(|t| &t.tok), Some(Tok::Eq))
                && matches!(self.toks.get(self.i + 2).map(|t| &t.tok), Some(Tok::Gt));
            let key = match by_name {
                true => {
                    let k = self.ident()?;
                    self.i += 2;
                    Some(k)
                }
                false => None,
            };
            let e = self.expr()?;
            if let Some(why) = not_a_value(&e) {
                self.i = at;
                return self.err(why);
            }
            match key {
                Some(k) if named.iter().any(|(n, _)| *n == k) => {
                    self.i = at;
                    return self.err(format!("`{k}` is given twice"));
                }
                Some(k) => named.push((k, e)),
                None if !named.is_empty() => {
                    self.i = at;
                    return self.err(
                        "an argument by place goes before those by name: `CALL transfer(1, 2, \
                         amount => 300)`",
                    );
                }
                None => placed.push(e),
            }
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        self.expect(Tok::RParen)?;
        Ok(match (placed.is_empty(), named.is_empty()) {
            (true, true) => Args::None,
            (true, false) => Args::Named(named),
            _ => Args::Placed(placed, named),
        })
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
        let at = self.i;
        // SQL's `(select max(at) from t) = x`.
        if self.scalar_ahead() {
            return self.scalar(None);
        }
        let left = self.arith()?;
        // A double-quoted text alone on the left: SQL's quoted column,
        // FenecQL's text. Against values alone the test answers the same
        // for every row -- `where "name" = 'a'` all of them or none -- and
        // is refused (`quoted`). A parameter on the left with text on the
        // right stays, a policy's claim tested: `$jwt.role = "admin"`.
        let lq = self.alone(at);
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
            if op == CmpOp::Eq && self.scalar_ahead() {
                return self.scalar(Some(left));
            }
            let r = self.i;
            let right = self.arith()?;
            if lq && fixed(&right) {
                return Err(self.quoted(at));
            }
            if matches!(left, Expr::Lit(_)) && self.alone(r) {
                return Err(self.quoted(r));
            }
            return Ok(Expr::Cmp(op, Box::new(left), Box::new(right)));
        }
        if matches!(self.peek(), Tok::Tilde) {
            self.next();
            let right = self.arith()?;
            if lq && fixed(&right) {
                return Err(self.quoted(at));
            }
            // PostgreSQL's `~` is a regular expression, FenecQL's text
            // contained: `email ~ '@example\.org$'` ran, matched nothing,
            // and agents told their users no such rows existed.
            if matches!(&right, Expr::Lit(Value::Text(t)) if regex_like(t)) {
                return self.err(
                    "`~` finds the text as written, not a regular expression: `like 'abc%'` \
                     for a start, `like '%abc'` for an end, `like 'a%b'` for a gap",
                );
            }
            return Ok(Expr::Like(Box::new(left), Box::new(right)));
        }
        // A word a test is named by -- `not`, `has`, `in`, `is`, `like`,
        // `ilike`, `between` -- by its first letter, before the call: the
        // words that follow an operand most, `and`, `or`, `where`, `order`
        // and `as`, called it for nothing, a parse of a filtered `get` 2%
        // slower.
        match self.peek() {
            Tok::Ident(w)
                if matches!(
                    w.as_bytes().first().map(|b| b | 0x20),
                    Some(b'n' | b'h' | b'i' | b'l' | b'b')
                ) =>
            {
                self.worded(at, lq, left)
            }
            // PostgreSQL's `@@`.
            Tok::At => self.err(FULLTEXT),
            // pgvector's distance, outside the `order by` it ranks.
            Tok::Distance(_) => self.err(DISTANCE),
            _ => Ok(left),
        }
    }

    /// Whether SQL's scalar subquery, `(select ...)`, comes next: asked
    /// of every comparison, so in line -- out of line, a call each, a read
    /// by id's parse was 1-2% slower.
    #[inline(always)]
    fn scalar_ahead(&self) -> bool {
        matches!(self.peek(), Tok::LParen)
            && matches!(self.toks.get(self.i + 1).map(|t| &t.tok),
                Some(Tok::Ident(k)) if k.eq_ignore_ascii_case("select") || k.eq_ignore_ascii_case("get"))
    }

    /// SQL's `x = (select max(at) from t)`, the `(` next: `x in (get t
    /// select max(at))`, where the query answers one row at most -- an
    /// aggregate with no group, or `limit 1` -- as SQL's comparison takes
    /// one. A query that may answer more is refused: SQL refuses it at
    /// the second row. `left` None: the query is on the left, the value
    /// after its `=`.
    #[inline(never)]
    fn scalar(&mut self, left: Option<Expr>) -> Result<Expr> {
        self.next();
        let sel = self.inner_get()?;
        self.expect(Tok::RParen)?;
        let one = (!sel.aggregate.is_empty() && sel.group.is_empty())
            || matches!(sel.limit, Some(Count::N(0 | 1)));
        if !one {
            return self.err(
                "`= (select ...)` compares one value, and the query may answer several rows: \
                 `x in (select ...)` takes each, or `limit 1` answers one",
            );
        }
        let left = match left {
            Some(l) => l,
            None => {
                self.expect(Tok::Eq)?;
                self.arith()?
            }
        };
        Ok(Expr::InSelect(Box::new(left), Box::new(sel)))
    }

    /// The `get` of a subquery, its `get` or `select` next: held to the
    /// depth subqueries nest to, and to the one column it answers.
    fn inner_get(&mut self) -> Result<Select> {
        self.deeper()?;
        self.subqueries += 1;
        let inner = self.get();
        self.subqueries -= 1;
        let Statement::Select(sel) = inner? else {
            unreachable!("`get` parses a select")
        };
        if let Err(e) = sel.check_subquery() {
            return self.err(e);
        }
        Ok(sel)
    }

    /// SQL's `exists (select ... from orders o where o.customer_id =
    /// c.id and ...)`, its `(` read: `id in (get orders select customer_id
    /// where ...)`, the one equality between a column of the query's table
    /// and one of the row around it the list, the rest of its `where` the
    /// query's. `not exists` is `not` over it, and keeps a row whose key
    /// is null, as SQL's does.
    #[inline(never)]
    fn exists(&mut self) -> Result<Expr> {
        if !(self.peek_kw("select") || self.peek_kw("get")) {
            return Err(self.taught("`exists` takes a query: `exists (select 1 from orders o where o.customer_id = c.id)`"));
        }
        self.deeper()?;
        self.next();
        // The list, which `exists` reads nothing of: `select 1`, `*`.
        let mut depth = 0usize;
        while depth > 0 || !self.peek_kw("from") {
            match self.next() {
                Tok::LParen => depth += 1,
                Tok::RParen if depth > 0 => depth -= 1,
                Tok::RParen | Tok::Eof => {
                    return Err(self.taught("an `exists` query reads `from` a collection"))
                }
                _ => {}
            }
        }
        self.next();
        let collection = self.ident()?;
        let inner = Table {
            collection: collection.clone(),
            alias: self.table_alias()?,
        };
        let outer = std::mem::replace(&mut self.outer, Around::Tables(vec![inner]));
        self.subqueries += 1;
        let filter = match self.eat_kw("where") {
            true => self.expr().map(Some),
            false => Ok(None),
        };
        self.subqueries -= 1;
        let Around::Tables(here) = std::mem::replace(&mut self.outer, outer) else {
            unreachable!("set just before")
        };
        let filter = filter?;
        // Whether a row exists asks no page: a `limit` of 1 or more changes
        // nothing, and one of 0 -- or a parameter bound to it -- asks of
        // no row, false where read as the list.
        if self.eat_kw("limit") {
            if !matches!(self.peek(), Tok::Int(n) if *n >= 1) {
                return self.err(
                    "an `exists` query's `limit` is a number from 1, which changes nothing: \
                     `limit 0` asks of no row",
                );
            }
            self.next();
        }
        if !matches!(self.peek(), Tok::RParen) {
            return Err(self.taught("an `exists` query is `select 1 from <collection> where ...`"));
        }
        self.next();
        let inner = From {
            tables: here,
            joins: Vec::new(),
        };
        let tables = match &self.outer {
            Around::Tables(t) => t.clone(),
            Around::At(at) => match self.toks.get(*at).map(|t| &t.tok) {
                Some(Tok::Ident(c) | Tok::Str(c)) => vec![Table {
                    collection: c.clone(),
                    alias: None,
                }],
                _ => Vec::new(),
            },
        };
        let around = From {
            tables,
            joins: Vec::new(),
        };
        match crate::join::correlate(&inner, &around, filter) {
            Ok(e) => Ok(e),
            Err(e) => self.err(e),
        }
    }

    /// A test named by a word after its operand `left`, read from `at`:
    /// `has`, `in`, `is [not] null`, and SQL's `like`, `ilike` and
    /// `between`, `not` before any of the last three. Out of line, so that
    /// `cmp_expr`, a frame every level of parentheses holds, stays the
    /// size it was: inlined, the depth test's 512 levels outgrew its 8 MiB
    /// in a debug build.
    #[inline(never)]
    fn worded(&mut self, at: usize, lq: bool, left: Expr) -> Result<Expr> {
        let Tok::Ident(w) = self.peek() else {
            return Ok(left);
        };
        // SQL's `x not in (..)`, `not like`, `not between`: the test, `not`
        // over it -- which no FenecQL text had after an operand.
        let negated = w.eq_ignore_ascii_case("not")
            && matches!(self.toks.get(self.i + 1).map(|t| &t.tok),
                Some(Tok::Ident(k)) if ["in", "like", "ilike", "between"]
                    .iter().any(|w| k.eq_ignore_ascii_case(w)));
        // `x not` where the text ends: a test a next line may name, which
        // the shell reads as open where a word of no statement stood.
        let open = w.eq_ignore_ascii_case("not")
            && matches!(self.toks.get(self.i + 1).map(|t| &t.tok), Some(Tok::Eof));
        if negated || open {
            self.next();
        }
        if open {
            return self.err("`not` after a value takes `in`, `like`, `ilike` or `between`");
        }
        let e = if self.eat_kw("has") {
            let right = self.arith()?;
            if lq && fixed(&right) {
                return Err(self.quoted(at));
            }
            Expr::Has(Box::new(left), Box::new(right))
        } else if self.eat_kw("in") {
            let e = self.in_list(left)?;
            let fixed_list = match &e {
                Expr::In(_, items) => items.iter().all(fixed),
                _ => true,
            };
            if lq && fixed_list {
                return Err(self.quoted(at));
            }
            e
        } else if self.peek_kw("like") || self.peek_kw("ilike") {
            // SQL's patterns, `%` and `_`: a call, as `case` is
            // (`query::sql_like`), so no walk over an expression grew a
            // variant.
            let name = match self.next() {
                Tok::Ident(w) => w.to_ascii_lowercase(),
                _ => String::new(),
            };
            let mut args = vec![left, self.arith()?];
            if self.eat_kw("escape") {
                args.push(self.arith()?);
            }
            if lq && args[1..].iter().all(fixed) {
                return Err(self.quoted(at));
            }
            Expr::Call(name, args)
        } else if self.eat_kw("between") {
            // SQL's bounds are both in: `>= low and <= high`, the `and`
            // its own before the boolean's, an operand on each side.
            let low = self.arith()?;
            self.expect_kw("and")?;
            let high = self.arith()?;
            if lq && fixed(&low) && fixed(&high) {
                return Err(self.quoted(at));
            }
            // The operand twice: a field or a value, copied as one -- a
            // copy of any expression was `Expr`'s whole clone, 2 KB of the
            // browser module.
            let twice = match &left {
                Expr::Field(f) => Expr::Field(f.clone()),
                Expr::Param(i) => Expr::Param(*i),
                _ => {
                    self.i = at;
                    return self.err("`between` takes a field: write `x >= a and x <= b`");
                }
            };
            Expr::And(
                Box::new(Expr::Cmp(CmpOp::Ge, Box::new(twice), Box::new(low))),
                Box::new(Expr::Cmp(CmpOp::Le, Box::new(left), Box::new(high))),
            )
        } else if self.eat_kw("is") {
            if lq {
                return Err(self.quoted(at));
            }
            let negated = self.eat_kw("not");
            // SQL's `is true` and `is false` are `= true` and `= false`, a
            // null neither; `is distinct from` is `!=`, which a null is of
            // any value but null -- FenecQL's `=` and `!=` take a null as
            // a value, as those do.
            let e = if self.eat_kw("distinct") {
                self.expect_kw("from")?;
                let right = self.arith()?;
                let op = if negated { CmpOp::Eq } else { CmpOp::Ne };
                return Ok(Expr::Cmp(op, Box::new(left), Box::new(right)));
            } else if self.peek_kw("true") || self.peek_kw("false") {
                let right = self.primary()?;
                Expr::Cmp(CmpOp::Eq, Box::new(left), Box::new(right))
            } else {
                self.expect_kw("null")?;
                Expr::IsNull(Box::new(left))
            };
            return Ok(if negated { Expr::Not(Box::new(e)) } else { e });
        } else {
            return Ok(left);
        };
        Ok(match negated {
            true => Expr::Not(Box::new(e)),
            false => e,
        })
    }

    /// After `in`: `[a, b]`, FenecQL's list; `(get ...)` or `(select ...)`,
    /// a subquery's; or `(a, b)`, SQL's list, read as `[a, b]` -- which no
    /// FenecQL text had, a `(` there taking a subquery alone.
    fn in_list(&mut self, left: Expr) -> Result<Expr> {
        let (open, close) = match self.peek() {
            Tok::LParen => (Tok::LParen, Tok::RParen),
            _ => (Tok::LBracket, Tok::RBracket),
        };
        if open == Tok::LParen
            && matches!(self.toks.get(self.i + 1).map(|t| &t.tok),
                Some(Tok::Ident(k)) if k.eq_ignore_ascii_case("get") || k.eq_ignore_ascii_case("select"))
        {
            return self.subquery(left);
        }
        self.expect(open)?;
        let mut items = Vec::new();
        loop {
            if self.peek() == &close {
                break;
            }
            items.push(self.expr()?);
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        self.expect(close)?;
        Ok(Expr::In(Box::new(left), items))
    }

    /// Refuses the double-quoted text at `at` (SQL's quoted column), which
    /// stands where only a field could make the answer depend on the row:
    /// alone in a test against values, as an item of a select list, a key
    /// of `order` or `group`, an aggregate's argument.
    #[inline(never)]
    fn quoted(&mut self, at: usize) -> Error {
        self.i = at;
        let name = match &self.toks[at].tok {
            Tok::Str(s) => s.as_str(),
            _ => "",
        };
        self.error(&format!("`{name}`, not \"{name}\": {QUOTED}"))
    }

    /// Whether a `get`'s clauses stopped at SQL's join: `join` or a word
    /// that leads one, an alias before either or before a comma, or a
    /// comma between collections. None begins a statement, so no text that
    /// parsed reads otherwise.
    fn joined(&self) -> bool {
        let word = |t: Option<&Token>| {
            matches!(t.map(|t| &t.tok), Some(Tok::Ident(w))
                if ["join", "inner", "left", "right", "full", "cross"]
                    .iter().any(|k| w.eq_ignore_ascii_case(k)))
        };
        let (a, b) = (self.toks.get(self.i), self.toks.get(self.i + 1));
        match a.map(|t| &t.tok) {
            Some(Tok::Comma) => true,
            Some(Tok::Ident(_)) => {
                word(a) || word(b) || matches!(b.map(|t| &t.tok), Some(Tok::Comma))
            }
            _ => false,
        }
    }

    /// Whether the tokens from `at` to here are a double-quoted text alone.
    fn alone(&self, at: usize) -> bool {
        self.i == at + 1 && matches!(self.toks[at].tok, Tok::Str(_))
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
        // SQL's `exists (select ...)` and MySQL's `match (f) against (..)`.
        match name.as_str() {
            "exists" => return self.exists(),
            "match" => return self.err(FULLTEXT),
            _ => {}
        }
        let mut args = Vec::new();
        // `max("at")`: SQL's column, text here, folded over every row the
        // same.
        let folds = AGGREGATES.contains(&name.as_str());
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
            let ti = self.i;
            args.push(Expr::Call("distinct".into(), vec![self.expr()?]));
            if self.alone(ti) {
                return Err(self.quoted(ti));
            }
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
                None => {
                    let ti = self.i;
                    args.push(self.expr()?);
                    if folds && self.alone(ti) {
                        return Err(self.quoted(ti));
                    }
                }
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
        // SQL's `count(*) filter (where c)`: the aggregate of what `case
        // when c then .. end` leaves -- `count(case when c then 1 end)` --
        // since every aggregate passes a null over and `count(*)` alone
        // counts rows.
        if matches!(name.as_str(), "count" | "sum" | "avg" | "min" | "max")
            && self.peek_kw("filter")
            && matches!(self.toks.get(self.i + 1).map(|t| &t.tok), Some(Tok::LParen))
        {
            self.i += 2;
            self.expect_kw("where")?;
            let when = self.expr()?;
            self.expect(Tok::RParen)?;
            let case = |v| Expr::Call("case".into(), vec![when, v]);
            args = match args.pop() {
                None => vec![case(Expr::Lit(Value::Int(1)))],
                Some(Expr::Call(d, mut v)) if d == "distinct" => {
                    let x = v.pop().unwrap_or(Expr::Lit(Value::Null));
                    vec![Expr::Call(d, vec![case(x)])]
                }
                Some(x) => vec![case(x)],
            };
        }
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
    /// A `get` or a `select` follows the `(` ([`Self::in_list`]).
    fn subquery(&mut self, left: Expr) -> Result<Expr> {
        self.next();
        let sel = self.inner_get()?;
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
                // SQL's scalar subquery, `= (select max(at) from t)`.
                if self.peek_kw("get") || self.peek_kw("select") {
                    return self.err(
                        "a query stands after `in`: `where x in (get <collection> select \
                         <field> ...)`; one value is read by a statement of its own",
                    );
                }
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
            Tok::Str(s) | Tok::Single(s) => {
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
                // In a definition, `$1` is its first parameter, and no
                // claim is read by its place.
                #[cfg(feature = "statements")]
                if let Some(d) = &self.def {
                    if i >= d.params.len() {
                        return Err(self.past_params(i, d.params.len()));
                    }
                }
                self.next();
                Ok(Expr::Param(i))
            }
            #[cfg(feature = "statements")]
            Tok::Named(n) => self.named_param(&n),
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
            // PostgreSQL's `meta->>'source'`, lexed `-` and `>`.
            Tok::Gt if self.toks[self.i - 1].tok == Tok::Minus => self.err(JSON_PATH),
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
        // A guard that says why: its `else` on the next line goes on with it.
        "set accounts {balance: balance - $1} where id = $2 and balance >= $1 require 1 else 'insufficient funds'",
        "get accounts where id = $1 limit 1 require 1 else \"no such account\"",
        "insert likes {post: $1, user: $2} if absent require 1 else 'it''s liked already' returning id",
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
        // SQL's spellings, the same statements.
        "insert into notes (body, topic) values ('a', 3), ($1, $2) on conflict (topic) do update set body = excluded.body returning id",
        "insert into notes values {body: 'a'}, {body: 'b'} on conflict do nothing",
        "update notes set topic = 4, body = body || '!' where id = 3 returning *",
        "get notes where topic not in (1, 2) and posted between $1 and $2 and body like 'a%' escape '!' and body is distinct from 'it''s'",
        "select topic, count(body) from notes where body ilike '%x%' and id in (select distinct n from tops) group by topic order by topic desc nulls last",
        // SQL's joins, subqueries and schema, the same statements as their
        // FenecQL.
        "select o.id, c.name as who from orders o join customers c on c.id = o.customer_id where c.country = 'IT' and o.total > 1 order by o.placed desc limit 3 offset 1",
        "select c.name, o.total from customers as c left outer join orders o on o.customer_id = c.id and o.status = 'paid' order by c.id",
        "select l.qty, o.status, c.name from lines l inner join orders o on o.id = l.order_id left join customers c on c.id = o.customer_id",
        "select o.* from customers c, orders o where o.customer_id = c.id and c.country = $1",
        "select c.id, count(o.id) as n from customers c join orders o on o.customer_id = c.id where c.country = 'IT' group by c.id",
        "select distinct city from users where age > 3 order by city",
        "select status s, count(*) n from orders group by status",
        "select count(*) filter (where tier is null) as nulls, sum(t) filter (where k = 1) as s from users",
        "select email from customers c where not exists (select 1 from orders o where o.customer_id = c.id and o.total > 2)",
        "update orders set status = 'paid' where id = (select id from orders where status = 'open' order by placed limit 1) returning id",
        "create table if not exists users (id serial primary key, name varchar(80) not null, email text unique, tags text[], emb vector(3), at timestamp with time zone, unique (name, email))",
        "create unique index users_email on users using btree (email)",
        "create index on docs using hnsw (emb vector_cosine_ops) with (m = 16)",
        "select id from docs where k = 1 order by emb <=> $1 limit 5",
        // Named statements: a body over lines reads as one definition, an
        // unended `begin` reading on to its `end`.
        "create statement transfer($from int, $to int, $amount int) claims (sub text) comment 'Moves an amount' check $amount > 0 and $from != $to as begin get accounts select id where id = $from and owner = $jwt.sub limit 1 require 1; set accounts {balance: balance - $amount} where id = $from and balance >= $amount require 1; set accounts {balance: balance + $amount} where id = $to require 1; insert entries {from: $from, to: $to, amount: $amount, by: $jwt.sub, at: now()} returning id, at end",
        "create or replace statement claim($queue text) claims (sub text, groups [text] from \"cognito:groups\") as set jobs {owner: $jwt.sub, run_at: now() + 30000, attempts: attempts + 1} where queue = $queue and run_at <= now() order run_at limit 1 returning id, payload",
        "create statement purge($n int = 10) public scans as del sessions where seen < now() - 86400000",
        "drop statement if exists transfer",
        "statements",
        "describe statement transfer",
        "call transfer {from: 1, to: 2, amount: 300} as {sub: 'alice'}",
        "call transfer $1 as $2",
        "CALL transfer(1, 2, amount => 300)",
        "call purge",
        // `call` and `statements` are aliases still, in a table's place and
        // a column's.
        "select a from t call",
        "select a call from t",
        "select count(*) statements from t",
        "select c.a, call.b from t c join calls call on call.id = c.id",
        "select a from t statements",
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
                // The SQL order's list again, of the spellings' statement.
                "select topic,",
                "select topic, count(body)",
                // And of the joins', its tables' names before their
                // `from`.
                "select o.id,",
                "select o.id, c.name",
                "select o.id, c.name as",
                "select o.id, c.name as who",
                "select c.name,",
                "select c.name, o.total",
                "select l.qty,",
                "select l.qty, o.status,",
                "select l.qty, o.status, c.name",
                "select o.*",
                "select c.id,",
                "select c.id, count(o.id)",
                "select c.id, count(o.id) as",
                "select c.id, count(o.id) as n",
                "select status s,",
                "select status s, count(*)",
                "select status s, count(*) n",
                "select count(*)",
                "select count(*) filter",
                "select count(*) filter (where tier is null)",
                "select count(*) filter (where tier is null) as",
                "select count(*) filter (where tier is null) as nulls,",
                "select count(*) filter (where tier is null) as nulls, sum(t)",
                "select count(*) filter (where tier is null) as nulls, sum(t) filter",
                "select count(*) filter (where tier is null) as nulls, sum(t) filter (where k = 1)",
                "select count(*) filter (where tier is null) as nulls, sum(t) filter (where k = 1) as",
                "select count(*) filter (where tier is null) as nulls, sum(t) filter (where k = 1) as s",
                // And of the aliases named statements' words still are.
                "select count(*)",
                "select count(*) statements",
                "select c.a,",
                "select c.a, call.b",
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
            outer: Around::At(usize::MAX),
            def: None,
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
