//! The SQL a client sends about the catalog: a lexer and a parser for the
//! part of `SELECT` that psql, JDBC and DBeaver use to look around --
//! joins, `CASE`, casts, scalar subqueries, `UNION`, `ANY` and regular
//! expressions -- and nothing past it. A query outside it is not an error to
//! the client: it is answered as it was before this module, empty.

use std::fmt;

/// Why a query could not be read. Never shown to the client.
#[derive(Debug, Clone, PartialEq)]
pub struct Unsupported(pub String);

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub type Parsed<T> = std::result::Result<T, Unsupported>;

fn no<T>(what: impl Into<String>) -> Parsed<T> {
    Err(Unsupported(what.into()))
}

// -------------------------------------------------------------------- lexer

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// An unquoted name, folded to lower case as PostgreSQL does.
    Word(String),
    /// A `"quoted"` name, as written.
    Quoted(String),
    Str(String),
    Num(String),
    Param(usize),
    Op(&'static str),
    End,
}

const OPS: [&str; 31] = [
    "::", "<>", "!=", "<=", ">=", "!~*", "!~", "~*", "||", "<<", ">>", "(", ")", ",", ".", ";",
    "=", "<", ">", "~", "+", "-", "*", "/", "%", "[", "]", ":", "!", "&", "|",
];

pub fn lex(src: &str) -> Parsed<Vec<Tok>> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    'outer: while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // Comments.
        if b[i..].starts_with(b"--") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b[i..].starts_with(b"/*") {
            match src[i + 2..].find("*/") {
                Some(n) => i += n + 4,
                None => return no("an unclosed comment"),
            }
            continue;
        }
        // E'...' with backslash escapes; '...' with doubled quotes.
        let escaped = (c == b'E' || c == b'e') && b.get(i + 1) == Some(&b'\'');
        if c == b'\'' || escaped {
            i += if escaped { 2 } else { 1 };
            let mut s = String::new();
            let mut chars = src[i..].char_indices();
            while let Some((k, ch)) = chars.next() {
                match ch {
                    '\'' if src[i + k + 1..].starts_with('\'') => {
                        s.push('\'');
                        chars.next();
                    }
                    '\'' => {
                        i += k + 1;
                        out.push(Tok::Str(s));
                        continue 'outer;
                    }
                    '\\' if escaped => match chars.next() {
                        Some((_, 'n')) => s.push('\n'),
                        Some((_, 't')) => s.push('\t'),
                        Some((_, 'r')) => s.push('\r'),
                        Some((_, other)) => s.push(other),
                        None => break,
                    },
                    other => s.push(other),
                }
            }
            return no("an unclosed string");
        }
        if c == b'"' {
            match src[i + 1..].find('"') {
                Some(n) => {
                    out.push(Tok::Quoted(src[i + 1..i + 1 + n].to_string()));
                    i += n + 2;
                }
                None => return no("an unclosed quoted name"),
            }
            continue;
        }
        if c == b'$' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            let start = i + 1;
            i = start;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let n = src[start..i]
                .parse()
                .map_err(|_| Unsupported("a parameter number".into()))?;
            out.push(Tok::Param(n));
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            out.push(Tok::Num(src[start..i].to_string()));
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            let start = i;
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80)
            {
                i += 1;
            }
            out.push(Tok::Word(src[start..i].to_lowercase()));
            continue;
        }
        for op in OPS {
            if b[i..].starts_with(op.as_bytes()) {
                out.push(Tok::Op(op));
                i += op.len();
                continue 'outer;
            }
        }
        return no(format!("the character `{}`", c as char));
    }
    out.push(Tok::End);
    Ok(out)
}

// ---------------------------------------------------------------------- AST

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    Param(usize),
    /// `qualifier.name`, or `name` alone.
    Column(Option<String>, String),
    /// `name(args)`, schema dropped; `star` for `count(*)`.
    Call {
        name: String,
        args: Vec<Expr>,
        star: bool,
        distinct: bool,
    },
    /// `CASE [operand] WHEN .. THEN .. [ELSE ..] END`.
    Case {
        operand: Option<Box<Expr>>,
        arms: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
    Cast(Box<Expr>, TypeName),
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    IsNull(Box<Expr>, bool),
    InList(Box<Expr>, Vec<Expr>, bool),
    InQuery(Box<Expr>, Box<Query>, bool),
    /// `x op ANY (array)` / `x op ALL (array)`.
    Quantified(&'static str, Box<Expr>, Box<Expr>, bool),
    Like(Box<Expr>, Box<Expr>, bool, bool),
    Between(Box<Expr>, Box<Expr>, Box<Expr>, bool),
    Subquery(Box<Query>),
    Exists(Box<Query>),
    /// `ARRAY(subquery)`.
    ArrayQuery(Box<Query>),
    /// `ARRAY[a, b]`.
    Array(Vec<Expr>),
    Index(Box<Expr>, Box<Expr>),
    /// `(composite).field`.
    Field(Box<Expr>, String),
    /// `name(args) OVER (PARTITION BY .. ORDER BY ..)`.
    Window {
        name: String,
        partition: Vec<Expr>,
        order: Vec<Order>,
    },
    /// A value computed for the row beforehand -- a window function's, or
    /// one of a set-returning function's -- put in the expression's place
    /// by the evaluator; never parsed.
    Slot(usize),
}

/// A type in a cast: its last name part, and whether it is an array.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeName {
    pub name: String,
    pub array: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Star(Option<String>),
    Expr(Expr, Option<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Table {
        schema: Option<String>,
        name: String,
        alias: Option<String>,
    },
    Query {
        query: Box<Query>,
        alias: String,
    },
    /// `generate_series(a, b) [AS] s[(column)]` and the like.
    Function {
        name: String,
        args: Vec<Expr>,
        alias: Option<String>,
        columns: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JoinKind {
    Inner,
    Left,
    Cross,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    pub source: Source,
    pub on: Option<Expr>,
    /// `USING (a, b)`: the columns both sides share.
    pub using: Vec<String>,
}

/// One `FROM` item and what joins onto it.
#[derive(Debug, Clone, PartialEq)]
pub struct From {
    pub first: Source,
    pub joins: Vec<Join>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub distinct: bool,
    pub items: Vec<Item>,
    pub from: Vec<From>,
    pub filter: Option<Expr>,
    pub group: Vec<Expr>,
    pub having: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Order {
    pub expr: Expr,
    pub desc: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    /// The first select, then each one `UNION`ed on and whether with `ALL`.
    pub first: Select,
    pub unions: Vec<(bool, Select)>,
    pub order: Vec<Order>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

// ------------------------------------------------------------------- parser

pub struct Parser {
    toks: Vec<Tok>,
    at: usize,
    /// The highest `$n` seen: how many parameters the statement takes.
    pub params: usize,
}

/// Words that end an expression or a select item where they stand, so none
/// of them is taken for a bare alias.
const STOP: [&str; 26] = [
    "from", "where", "group", "having", "order", "limit", "offset", "union", "on", "join", "left",
    "right", "inner", "cross", "full", "outer", "and", "or", "not", "then", "else", "end", "when",
    "as", "asc", "desc",
];

impl Parser {
    pub fn new(src: &str) -> Parsed<Parser> {
        Ok(Parser {
            toks: lex(src)?,
            at: 0,
            params: 0,
        })
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.at.min(self.toks.len() - 1)]
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.at + n).min(self.toks.len() - 1)]
    }

    fn next(&mut self) -> Tok {
        let t = self.peek().clone();
        if self.at < self.toks.len() {
            self.at += 1;
        }
        t
    }

    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Tok::Word(x) if x == w)
    }

    fn eat_word(&mut self, w: &str) -> bool {
        if self.is_word(w) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if matches!(self.peek(), Tok::Op(x) if *x == op) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect_op(&mut self, op: &str) -> Parsed<()> {
        if self.eat_op(op) {
            Ok(())
        } else {
            no(format!("expected `{op}`, found {:?}", self.peek()))
        }
    }

    fn expect_word(&mut self, w: &str) -> Parsed<()> {
        if self.eat_word(w) {
            Ok(())
        } else {
            no(format!("expected `{w}`, found {:?}", self.peek()))
        }
    }

    /// A name: a word or a quoted name.
    fn name(&mut self) -> Parsed<String> {
        match self.next() {
            Tok::Word(w) | Tok::Quoted(w) => Ok(w),
            other => no(format!("expected a name, found {other:?}")),
        }
    }

    /// The whole input as one query, a trailing `;` allowed.
    pub fn statement(mut self) -> Parsed<(Query, usize)> {
        let q = self.query()?;
        self.eat_op(";");
        if *self.peek() != Tok::End {
            return no(format!("text after the query: {:?}", self.peek()));
        }
        Ok((q, self.params))
    }

    pub fn query(&mut self) -> Parsed<Query> {
        // A parenthesised query as the whole: `(SELECT ...) UNION ...`.
        let first = self.select_core()?;
        let mut unions = Vec::new();
        while self.eat_word("union") {
            let all = self.eat_word("all");
            if !all {
                self.eat_word("distinct");
            }
            unions.push((all, self.select_core()?));
        }
        let mut order = Vec::new();
        if self.eat_word("order") {
            self.expect_word("by")?;
            loop {
                let expr = self.expr()?;
                let desc = if self.eat_word("desc") {
                    true
                } else {
                    self.eat_word("asc");
                    false
                };
                if self.eat_word("nulls") && !self.eat_word("first") {
                    self.expect_word("last")?;
                }
                order.push(Order { expr, desc });
                if !self.eat_op(",") {
                    break;
                }
            }
        }
        let (mut limit, mut offset) = (None, None);
        loop {
            if self.eat_word("limit") {
                limit = Some(self.expr()?);
            } else if self.eat_word("offset") {
                offset = Some(self.expr()?);
                self.eat_word("rows");
            } else {
                break;
            }
        }
        Ok(Query {
            first,
            unions,
            order,
            limit,
            offset,
        })
    }

    fn select_core(&mut self) -> Parsed<Select> {
        if self.eat_op("(") {
            let q = self.query()?;
            self.expect_op(")")?;
            if q.unions.is_empty() && q.order.is_empty() && q.limit.is_none() {
                return Ok(q.first);
            }
            return no("a parenthesised union inside a union");
        }
        self.expect_word("select")?;
        let distinct = self.eat_word("distinct");
        if !distinct {
            self.eat_word("all");
        }
        let mut items = Vec::new();
        loop {
            items.push(self.item()?);
            if !self.eat_op(",") {
                break;
            }
        }
        let mut from = Vec::new();
        if self.eat_word("from") {
            loop {
                from.push(self.joined_source()?);
                if !self.eat_op(",") {
                    break;
                }
            }
        }
        let filter = if self.eat_word("where") {
            Some(self.expr()?)
        } else {
            None
        };
        let mut group = Vec::new();
        if self.eat_word("group") {
            self.expect_word("by")?;
            loop {
                group.push(self.expr()?);
                if !self.eat_op(",") {
                    break;
                }
            }
        }
        let having = if self.eat_word("having") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Select {
            distinct,
            items,
            from,
            filter,
            group,
            having,
        })
    }

    fn item(&mut self) -> Parsed<Item> {
        if self.eat_op("*") {
            return Ok(Item::Star(None));
        }
        // `alias.*`
        if let (Tok::Word(q) | Tok::Quoted(q), Tok::Op("."), Tok::Op("*")) = (
            self.peek().clone(),
            self.peek_at(1).clone(),
            self.peek_at(2).clone(),
        ) {
            self.at += 3;
            return Ok(Item::Star(Some(q)));
        }
        let e = self.expr()?;
        Ok(Item::Expr(e, self.alias()?))
    }

    /// `AS name`, or a bare name that is not a keyword.
    fn alias(&mut self) -> Parsed<Option<String>> {
        if self.eat_word("as") {
            return Ok(Some(self.name()?));
        }
        match self.peek().clone() {
            Tok::Quoted(q) => {
                self.at += 1;
                Ok(Some(q))
            }
            Tok::Word(w) if !STOP.contains(&w.as_str()) && w != "using" => {
                self.at += 1;
                Ok(Some(w))
            }
            _ => Ok(None),
        }
    }

    fn joined_source(&mut self) -> Parsed<From> {
        let first = self.source()?;
        let mut joins = Vec::new();
        loop {
            let kind = if self.eat_word("left") {
                self.eat_word("outer");
                JoinKind::Left
            } else if self.eat_word("inner") {
                JoinKind::Inner
            } else if self.eat_word("cross") {
                JoinKind::Cross
            } else if self.is_word("join") {
                JoinKind::Inner
            } else if self.is_word("right") || self.is_word("full") {
                return no("a right or full join");
            } else {
                break;
            };
            self.expect_word("join")?;
            let source = self.source()?;
            let mut using = Vec::new();
            let on = if kind == JoinKind::Cross {
                None
            } else if self.eat_word("on") {
                Some(self.expr()?)
            } else if self.eat_word("using") {
                self.expect_op("(")?;
                loop {
                    using.push(self.name()?);
                    if self.eat_op(")") {
                        break;
                    }
                    self.expect_op(",")?;
                }
                None
            } else {
                return no("a join without ON");
            };
            joins.push(Join {
                kind,
                source,
                on,
                using,
            });
        }
        Ok(From { first, joins })
    }

    fn source(&mut self) -> Parsed<Source> {
        if self.eat_op("(") {
            let query = self.query()?;
            self.expect_op(")")?;
            let alias = self.alias()?.unwrap_or_else(|| "subquery".into());
            return Ok(Source::Query {
                query: Box::new(query),
                alias,
            });
        }
        let mut name = self.name()?;
        let mut schema = None;
        if self.eat_op(".") {
            schema = Some(name);
            name = self.name()?;
        }
        if self.eat_op("(") {
            let args = self.args()?;
            let alias = self.alias()?;
            // A column list after a function's alias: `s(i)`.
            let mut columns = Vec::new();
            if alias.is_some() && self.eat_op("(") {
                loop {
                    columns.push(self.name()?);
                    if self.eat_op(")") {
                        break;
                    }
                    self.expect_op(",")?;
                }
            }
            return Ok(Source::Function {
                name,
                args,
                alias,
                columns,
            });
        }
        let alias = self.alias()?;
        Ok(Source::Table {
            schema,
            name,
            alias,
        })
    }

    /// Arguments up to and including the closing parenthesis.
    fn args(&mut self) -> Parsed<Vec<Expr>> {
        let mut args = Vec::new();
        if self.eat_op(")") {
            return Ok(args);
        }
        loop {
            args.push(self.expr()?);
            if self.eat_op(")") {
                return Ok(args);
            }
            self.expect_op(",")?;
        }
    }

    pub fn expr(&mut self) -> Parsed<Expr> {
        self.or()
    }

    fn or(&mut self) -> Parsed<Expr> {
        let mut e = self.and()?;
        while self.eat_word("or") {
            e = Expr::Or(Box::new(e), Box::new(self.and()?));
        }
        Ok(e)
    }

    fn and(&mut self) -> Parsed<Expr> {
        let mut e = self.not()?;
        while self.eat_word("and") {
            e = Expr::And(Box::new(e), Box::new(self.not()?));
        }
        Ok(e)
    }

    fn not(&mut self) -> Parsed<Expr> {
        if self.eat_word("not") {
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Parsed<Expr> {
        let left = self.concat()?;
        // `x IS [NOT] NULL`, `x IS [NOT] TRUE|FALSE`
        if self.eat_word("is") {
            let negated = self.eat_word("not");
            if self.eat_word("null") {
                return Ok(Expr::IsNull(Box::new(left), negated));
            }
            let truth = if self.eat_word("true") {
                true
            } else if self.eat_word("false") {
                false
            } else if self.eat_word("distinct") {
                self.expect_word("from")?;
                let right = self.concat()?;
                let differ = Expr::Binary("<>", Box::new(left), Box::new(right));
                return Ok(if negated {
                    Expr::Not(Box::new(differ))
                } else {
                    differ
                });
            } else {
                return no("IS followed by something else");
            };
            let test = Expr::Binary("=", Box::new(left), Box::new(Expr::Bool(truth)));
            return Ok(if negated {
                Expr::Not(Box::new(test))
            } else {
                test
            });
        }
        let negated = self.eat_word("not");
        if self.eat_word("in") {
            self.expect_op("(")?;
            if self.is_word("select") {
                let q = self.query()?;
                self.expect_op(")")?;
                return Ok(Expr::InQuery(Box::new(left), Box::new(q), negated));
            }
            let list = self.args()?;
            return Ok(Expr::InList(Box::new(left), list, negated));
        }
        if self.eat_word("like") || self.eat_word("ilike") {
            let insensitive = matches!(&self.toks[self.at - 1], Tok::Word(w) if w == "ilike");
            let right = self.concat()?;
            return Ok(Expr::Like(
                Box::new(left),
                Box::new(right),
                insensitive,
                negated,
            ));
        }
        if self.eat_word("between") {
            let low = self.concat()?;
            self.expect_word("and")?;
            let high = self.concat()?;
            return Ok(Expr::Between(
                Box::new(left),
                Box::new(low),
                Box::new(high),
                negated,
            ));
        }
        if negated {
            return no("NOT inside a comparison");
        }
        let op = match self.peek() {
            Tok::Op(
                op @ ("=" | "<>" | "!=" | "<" | "<=" | ">" | ">=" | "~" | "!~" | "~*" | "!~*"),
            ) => {
                let op: &'static str = op;
                self.at += 1;
                op
            }
            // `OPERATOR(pg_catalog.~)`
            Tok::Word(w) if w == "operator" => {
                self.at += 1;
                self.expect_op("(")?;
                let mut op = None;
                loop {
                    match self.next() {
                        Tok::Op(")") => break,
                        Tok::Op(o) if o != "." => op = Some(o),
                        Tok::End => return no("an unclosed OPERATOR()"),
                        _ => {}
                    }
                }
                op.ok_or_else(|| Unsupported("an empty OPERATOR()".into()))?
            }
            _ => return Ok(left),
        };
        let op = if op == "!=" { "<>" } else { op };
        // `x = ANY (array)`
        if self.is_word("any") || self.is_word("some") || self.is_word("all") {
            let all = self.is_word("all");
            self.at += 1;
            self.expect_op("(")?;
            let right = if self.is_word("select") {
                Expr::ArrayQuery(Box::new(self.query()?))
            } else {
                self.expr()?
            };
            self.expect_op(")")?;
            return Ok(Expr::Quantified(op, Box::new(left), Box::new(right), all));
        }
        let right = self.concat()?;
        Ok(Expr::Binary(op, Box::new(left), Box::new(right)))
    }

    /// `||` and PostgreSQL's other operators, which bind looser than
    /// arithmetic and tighter than comparison.
    fn concat(&mut self) -> Parsed<Expr> {
        let mut e = self.additive()?;
        loop {
            let op = match self.peek() {
                Tok::Op(op @ ("||" | "&" | "|" | "<<" | ">>")) => *op,
                _ => return Ok(e),
            };
            self.at += 1;
            e = Expr::Binary(op, Box::new(e), Box::new(self.additive()?));
        }
    }

    fn additive(&mut self) -> Parsed<Expr> {
        let mut e = self.multiplicative()?;
        loop {
            let op = if self.eat_op("+") {
                "+"
            } else if self.eat_op("-") {
                "-"
            } else {
                return Ok(e);
            };
            e = Expr::Binary(op, Box::new(e), Box::new(self.multiplicative()?));
        }
    }

    fn multiplicative(&mut self) -> Parsed<Expr> {
        let mut e = self.unary()?;
        loop {
            let op = if self.eat_op("*") {
                "*"
            } else if self.eat_op("/") {
                "/"
            } else if self.eat_op("%") {
                "%"
            } else {
                return Ok(e);
            };
            e = Expr::Binary(op, Box::new(e), Box::new(self.unary()?));
        }
    }

    fn unary(&mut self) -> Parsed<Expr> {
        if self.eat_op("-") {
            return Ok(Expr::Unary("-", Box::new(self.unary()?)));
        }
        if self.eat_op("+") {
            return self.unary();
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Parsed<Expr> {
        let mut e = self.primary()?;
        loop {
            if self.eat_op("::") {
                e = Expr::Cast(Box::new(e), self.type_name()?);
            } else if self.eat_op("[") {
                let i = self.expr()?;
                self.expect_op("]")?;
                e = Expr::Index(Box::new(e), Box::new(i));
            } else if self.eat_word("collate") {
                // The collation changes nothing a catalog is sorted by here.
                self.name()?;
                if self.eat_op(".") {
                    self.name()?;
                }
            } else {
                return Ok(e);
            }
        }
    }

    fn type_name(&mut self) -> Parsed<TypeName> {
        let mut name = self.name()?;
        while self.eat_op(".") {
            name = self.name()?;
        }
        // Two-word types.
        for (first, second, whole) in [
            ("double", "precision", "float8"),
            ("character", "varying", "varchar"),
        ] {
            if name == first && self.eat_word(second) {
                name = whole.into();
            }
        }
        // `varchar(64)`, `numeric(10, 2)`
        if self.eat_op("(") {
            while !self.eat_op(")") {
                if self.next() == Tok::End {
                    return no("an unclosed type modifier");
                }
            }
        }
        let mut array = false;
        while self.eat_op("[") {
            self.expect_op("]")?;
            array = true;
        }
        Ok(TypeName { name, array })
    }

    fn primary(&mut self) -> Parsed<Expr> {
        match self.next() {
            Tok::Num(n) => Ok(match n.parse::<i64>() {
                Ok(i) => Expr::Int(i),
                // Decimals do not come up in the catalog; they stay text.
                Err(_) => Expr::Str(n),
            }),
            Tok::Str(s) => Ok(Expr::Str(s)),
            Tok::Param(n) => {
                self.params = self.params.max(n);
                Ok(Expr::Param(n))
            }
            Tok::Op("(") => {
                if self.is_word("select") {
                    let q = self.query()?;
                    self.expect_op(")")?;
                    return Ok(Expr::Subquery(Box::new(q)));
                }
                let e = self.expr()?;
                // A row constructor is not something the catalog needs.
                if self.eat_op(",") {
                    return no("a row constructor");
                }
                self.expect_op(")")?;
                // `(composite).field`
                if matches!(self.peek(), Tok::Op(".")) {
                    self.at += 1;
                    return Ok(Expr::Field(Box::new(e), self.name()?));
                }
                Ok(e)
            }
            Tok::Quoted(q) => self.column_or_call(q),
            Tok::Word(w) => match w.as_str() {
                "null" => Ok(Expr::Null),
                "true" => Ok(Expr::Bool(true)),
                "false" => Ok(Expr::Bool(false)),
                "case" => self.case(),
                "cast" => {
                    self.expect_op("(")?;
                    let e = self.expr()?;
                    self.expect_word("as")?;
                    let t = self.type_name()?;
                    self.expect_op(")")?;
                    Ok(Expr::Cast(Box::new(e), t))
                }
                // `trim([both|leading|trailing] [chars] from text)`
                "trim" if matches!(self.peek(), Tok::Op("(")) => {
                    self.at += 1;
                    let side = ["both", "leading", "trailing"]
                        .into_iter()
                        .find(|w| self.eat_word(w))
                        .unwrap_or("both");
                    let first = if self.is_word("from") {
                        None
                    } else {
                        Some(self.expr()?)
                    };
                    let (chars, text) = if self.eat_word("from") {
                        (first, self.expr()?)
                    } else {
                        (
                            None,
                            first.ok_or_else(|| Unsupported("an empty trim()".into()))?,
                        )
                    };
                    self.expect_op(")")?;
                    let name = match side {
                        "leading" => "ltrim",
                        "trailing" => "rtrim",
                        _ => "btrim",
                    };
                    let mut args = vec![text];
                    args.extend(chars);
                    Ok(Expr::Call {
                        name: name.into(),
                        args,
                        star: false,
                        distinct: false,
                    })
                }
                "exists" => {
                    self.expect_op("(")?;
                    let q = self.query()?;
                    self.expect_op(")")?;
                    Ok(Expr::Exists(Box::new(q)))
                }
                "array" if self.eat_op("(") => {
                    let q = self.query()?;
                    self.expect_op(")")?;
                    Ok(Expr::ArrayQuery(Box::new(q)))
                }
                "array" if self.eat_op("[") => {
                    let mut items = Vec::new();
                    if !self.eat_op("]") {
                        loop {
                            items.push(self.expr()?);
                            if self.eat_op("]") {
                                break;
                            }
                            self.expect_op(",")?;
                        }
                    }
                    Ok(Expr::Array(items))
                }
                // `current_user` and the like take no parentheses.
                "current_user" | "session_user" | "user" | "current_catalog" | "current_schema"
                    if !matches!(self.peek(), Tok::Op("(")) =>
                {
                    Ok(Expr::Call {
                        name: w,
                        args: Vec::new(),
                        star: false,
                        distinct: false,
                    })
                }
                // A typed literal: `regclass 'docs'`, `name 'x'`.
                t if matches!(self.peek(), Tok::Str(_))
                    && ["regclass", "regtype", "name", "text", "oid", "date"].contains(&t) =>
                {
                    let Tok::Str(s) = self.next() else {
                        unreachable!()
                    };
                    Ok(Expr::Cast(
                        Box::new(Expr::Str(s)),
                        TypeName {
                            name: w,
                            array: false,
                        },
                    ))
                }
                _ => self.column_or_call(w),
            },
            other => no(format!("unexpected {other:?}")),
        }
    }

    fn column_or_call(&mut self, first: String) -> Parsed<Expr> {
        let mut parts = vec![first];
        while matches!(self.peek(), Tok::Op(".")) {
            self.at += 1;
            parts.push(self.name()?);
        }
        if self.eat_op("(") {
            let name = parts.pop().unwrap_or_default();
            if self.eat_op("*") {
                self.expect_op(")")?;
                return Ok(Expr::Call {
                    name,
                    args: Vec::new(),
                    star: true,
                    distinct: false,
                });
            }
            let distinct = self.eat_word("distinct");
            let args = self.args()?;
            if self.eat_word("over") {
                return self.window(name);
            }
            return Ok(Expr::Call {
                name,
                args,
                star: false,
                distinct,
            });
        }
        let name = parts.pop().unwrap_or_default();
        // `schema.table.column` keeps only the table as the qualifier.
        Ok(Expr::Column(parts.pop(), name))
    }

    /// `OVER ( [PARTITION BY ..] [ORDER BY ..] )`, after the function.
    fn window(&mut self, name: String) -> Parsed<Expr> {
        self.expect_op("(")?;
        let mut partition = Vec::new();
        if self.eat_word("partition") {
            self.expect_word("by")?;
            loop {
                partition.push(self.expr()?);
                if !self.eat_op(",") {
                    break;
                }
            }
        }
        let mut order = Vec::new();
        if self.eat_word("order") {
            self.expect_word("by")?;
            loop {
                let expr = self.expr()?;
                let desc = if self.eat_word("desc") {
                    true
                } else {
                    self.eat_word("asc");
                    false
                };
                order.push(Order { expr, desc });
                if !self.eat_op(",") {
                    break;
                }
            }
        }
        self.expect_op(")")?;
        Ok(Expr::Window {
            name,
            partition,
            order,
        })
    }

    fn case(&mut self) -> Parsed<Expr> {
        let operand = if self.is_word("when") {
            None
        } else {
            Some(Box::new(self.expr()?))
        };
        let mut arms = Vec::new();
        while self.eat_word("when") {
            let when = self.expr()?;
            self.expect_word("then")?;
            arms.push((when, self.expr()?));
        }
        let otherwise = if self.eat_word("else") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        self.expect_word("end")?;
        Ok(Expr::Case {
            operand,
            arms,
            otherwise,
        })
    }
}

/// Parses a whole statement: the query, and how many parameters it takes.
pub fn parse(src: &str) -> Parsed<(Query, usize)> {
    Parser::new(src)?.statement()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psql_describe_parses() {
        for q in [
            "SELECT n.nspname as \"Schema\", c.relname as \"Name\", CASE c.relkind WHEN 'r' THEN 'table' END as \"Type\" \
             FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r','p','') AND n.nspname !~ '^pg_toast' AND pg_catalog.pg_table_is_visible(c.oid) ORDER BY 1,2;",
            "SELECT c.oid, n.nspname, c.relname FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname OPERATOR(pg_catalog.~) '^(docs)$' COLLATE pg_catalog.default AND pg_catalog.pg_table_is_visible(c.oid) ORDER BY 2, 3;",
            "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true) \
             FROM pg_catalog.pg_attrdef d WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef), a.attnotnull \
             FROM pg_catalog.pg_attribute a WHERE a.attrelid = '38147' AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum;",
            "SELECT c.oid::pg_catalog.regclass, c.relkind FROM pg_catalog.pg_class c, pg_catalog.pg_inherits i \
             WHERE c.oid = i.inhrelid AND i.inhparent = '38147' ORDER BY pg_catalog.pg_get_expr(c.relpartbound, c.oid) = 'DEFAULT', c.oid::pg_catalog.regclass::pg_catalog.text;",
            "SELECT pubname, NULL, NULL FROM pg_catalog.pg_publication p WHERE p.puballtables UNION SELECT pubname, NULL, NULL FROM pg_catalog.pg_publication p ORDER BY 1;",
            "SELECT CASE WHEN pol.polroles = '{0}' THEN NULL ELSE pg_catalog.array_to_string(array(select rolname from pg_catalog.pg_roles where oid = any (pol.polroles) order by 1),',') END FROM pg_catalog.pg_policy pol",
            "select 'd' = any(stxkind) AS ndist_enabled, E'\\n' from pg_catalog.pg_statistic_ext where stxrelid = $1",
        ] {
            parse(q).unwrap_or_else(|e| panic!("{q}\n{e}"));
        }
        assert_eq!(parse("select $3, $1").unwrap().1, 3);
    }

    #[test]
    fn what_is_not_read_says_so() {
        for q in [
            "select 1 from a right join b on true",
            "insert into t values (1)",
            "select 'x",
        ] {
            assert!(parse(q).is_err(), "{q}");
        }
    }
}
