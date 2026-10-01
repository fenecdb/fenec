//! A plain `SELECT` of columns from one collection, read as the FenecQL
//! `get` it is: what a driver asks before a COPY, to learn the columns'
//! types -- asyncpg's `SELECT "a", "b" FROM "t" LIMIT 1`, pgx's `select
//! "a", "b" from "t"` -- and what a person types into psql first. A
//! `WHERE`, a join, an expression or a function is none of it, and goes on
//! to FenecQL's parser and its error.

use crate::copy::{tokens, Tok};

/// The FenecQL of `text` when it is a `SELECT` of columns, or `*`, from
/// one collection, with a `LIMIT` or none.
pub fn select(text: &str) -> Option<String> {
    select_as(text).map(|(q, _)| q)
}

/// [`select`], and which of its columns are cast to text -- `"e"::VARCHAR`,
/// as DuckDB asks for a type it has no reader for, a pgvector vector --
/// to go as their text in every format.
pub fn select_as(text: &str) -> Option<(String, Vec<bool>)> {
    let p = plain(text)?;
    if p.columns.iter().any(|c| c.0.is_empty()) {
        return None;
    }
    Some((p.inline()?, p.columns.iter().map(|c| c.1).collect()))
}

/// `SELECT 1 FROM t [WHERE ...]`: a constant a matching row, as Spark
/// counts a table's rows -- the FenecQL that counts them, and the
/// constants a row holds.
pub fn constants(text: &str) -> Option<(String, Vec<String>)> {
    let mut p = plain(text)?;
    if p.columns.is_empty() || p.columns.iter().any(|c| !c.0.is_empty()) {
        return None;
    }
    let values = p
        .columns
        .iter()
        .map(|c| c.2.clone().unwrap_or_default())
        .collect();
    p.columns.clear();
    let q = p.inline()?;
    // The count, in place of the rows: `limit 0` stays a count of none.
    Some((format!("{q} count"), values))
}

/// A `SELECT` of columns from one collection, as DuckDB's postgres
/// extension reads a table: its columns, each cast to text or not, the
/// conditions it pushes down -- a column compared with a literal, `IS [NOT]
/// NULL`, `IN (...)` -- joined by `AND`, and a `LIMIT`.
#[derive(Debug, PartialEq)]
pub struct Plain {
    pub collection: String,
    /// Empty: `*`. A column's name, whether it is cast to text, and for a
    /// constant (`SELECT 1`, the name empty) its literal.
    pub columns: Vec<(String, bool, Option<String>)>,
    pub filters: Vec<Filter>,
    pub limit: Option<u64>,
    /// Bare literals, as Spark writes a number: `n < 10`. Quoted is text.
    bare: Vec<bool>,
}

/// A condition, its literals as the text they came in: each is read as its
/// column's type when the collection's schema is at hand -- DuckDB quotes
/// an int's `100` as `'100'`, which by its look alone a text field would
/// have been compared with as a number.
#[derive(Debug, PartialEq)]
pub enum Filter {
    Cmp(String, &'static str, String),
    Null(String, bool),
    In(String, Vec<String>),
}

impl Plain {
    /// The FenecQL `get`, each literal a parameter in order: `(column,
    /// literal)` a parameter.
    pub fn fenecql(&self) -> (String, Vec<(String, String)>) {
        let mut q = format!("get {}", self.collection);
        if !self.columns.is_empty() {
            let names: Vec<&str> = self.columns.iter().map(|c| c.0.as_str()).collect();
            q.push_str(" select ");
            q.push_str(&names.join(", "));
        }
        let mut params = Vec::new();
        let mut terms = Vec::new();
        for f in &self.filters {
            match f {
                Filter::Cmp(col, op, lit) => {
                    params.push((col.clone(), lit.clone()));
                    terms.push(format!("{col} {op} ${}", params.len()));
                }
                Filter::Null(col, not) => {
                    terms.push(format!("{col} is {}null", if *not { "not " } else { "" }));
                }
                Filter::In(col, lits) => {
                    let mut marks = Vec::new();
                    for lit in lits {
                        params.push((col.clone(), lit.clone()));
                        marks.push(format!("${}", params.len()));
                    }
                    terms.push(format!("{col} in [{}]", marks.join(", ")));
                }
            }
        }
        if !terms.is_empty() {
            q.push_str(" where ");
            q.push_str(&terms.join(" and "));
        }
        if let Some(n) = self.limit {
            q.push_str(&format!(" limit {n}"));
        }
        (q, params)
    }

    /// The `get` with each literal written into it as its look says: a bare
    /// number a number, a quoted one text -- how Spark writes them, over a
    /// path with no schema at hand. `None` for a literal FenecQL could not
    /// hold.
    pub fn inline(&self) -> Option<String> {
        let (mut q, params) = self.fenecql();
        // Last first, so `$1` does not take the front of `$10`.
        for (i, (_, lit)) in params.iter().enumerate().rev() {
            let text = if self.bare.get(i).copied().unwrap_or(false) {
                let number = lit.parse::<f64>().ok().filter(|n| n.is_finite());
                if lit != "true" && lit != "false" && number.is_none() {
                    return None;
                }
                lit.clone()
            } else {
                let mut s = String::from("\"");
                for c in lit.chars() {
                    match c {
                        '"' => s.push_str("\\\""),
                        '\\' => s.push_str("\\\\"),
                        '\n' => s.push_str("\\n"),
                        c => s.push(c),
                    }
                }
                s.push('"');
                s
            };
            q = q.replace(&format!("${}", i + 1), &text);
        }
        Some(q)
    }
}

/// A literal, and whether it was written bare: a string, or a number or
/// boolean written as it is -- `12.5` comes as three tokens.
fn literal(t: &mut std::iter::Peekable<impl Iterator<Item = Tok>>) -> Option<(String, bool)> {
    let (sign, first) = match t.next()? {
        Tok::Str(s) => return Some((s, false)),
        Tok::Punct('-') => ("-", t.next()?),
        other => ("", other),
    };
    let Tok::Word(mut w) = first else {
        return None;
    };
    if w.bytes().all(|b| b.is_ascii_digit()) && t.peek() == Some(&Tok::Punct('.')) {
        t.next();
        match t.next()? {
            Tok::Word(f) if f.bytes().all(|b| b.is_ascii_digit()) => w = format!("{w}.{f}"),
            _ => return None,
        }
    }
    Some((format!("{sign}{w}"), true))
}

/// `::type`, the type's name.
fn cast(t: &mut std::iter::Peekable<impl Iterator<Item = Tok>>) -> Option<String> {
    if t.peek() != Some(&Tok::Punct(':')) {
        return Some(String::new());
    }
    t.next();
    if t.next()? != Tok::Punct(':') {
        return None;
    }
    match t.next()? {
        Tok::Word(ty) | Tok::Name(ty) => Some(ty),
        _ => None,
    }
}

/// [`Plain`] when `text` is one.
pub fn plain(text: &str) -> Option<Plain> {
    let head = text.trim_start().as_bytes();
    if !head
        .get(..6)
        .is_some_and(|w| w.eq_ignore_ascii_case(b"select"))
    {
        return None;
    }
    let mut t = tokens(text);
    if t.first() != Some(&Tok::Word("select".into())) {
        return None;
    }
    while t.last() == Some(&Tok::Punct(';')) {
        t.pop();
    }
    let mut t = t.into_iter().skip(1).peekable();
    let mut columns = Vec::new();
    if t.peek() == Some(&Tok::Punct('*')) {
        t.next();
        if t.next()? != Tok::Word("from".into()) {
            return None;
        }
    } else {
        loop {
            // A constant, as Spark counts rows with `SELECT 1`, or a column.
            let column = match t.peek()? {
                Tok::Str(_) => (String::new(), false, Some(literal(&mut t)?.0)),
                Tok::Word(w) if w.bytes().all(|b| b.is_ascii_digit()) => {
                    (String::new(), false, Some(literal(&mut t)?.0))
                }
                _ => {
                    let name = ident(t.next()?)?;
                    let as_text = match cast(&mut t)?.as_str() {
                        "" => false,
                        "varchar" | "text" => true,
                        _ => return None,
                    };
                    (name, as_text, None)
                }
            };
            columns.push(column);
            match t.next()? {
                Tok::Punct(',') => {}
                Tok::Word(w) if w == "from" => break,
                _ => return None,
            }
        }
    }
    let mut collection = ident(t.next()?)?;
    if t.peek() == Some(&Tok::Punct('.')) {
        if collection != "public" {
            return None;
        }
        t.next();
        collection = ident(t.next()?)?;
    }
    let (mut filters, mut bare) = (Vec::new(), Vec::new());
    let mut never = false;
    if t.peek() == Some(&Tok::Word("where".into())) {
        t.next();
        loop {
            // Spark writes each condition in parentheses.
            let wrapped = t.peek() == Some(&Tok::Punct('('));
            if wrapped {
                t.next();
            }
            match t.peek()? {
                // A literal compared with a literal: `1=0`, which Spark asks
                // a table's columns with, is no row; `1=1` every one.
                Tok::Word(w) if w.bytes().all(|b| b.is_ascii_digit()) => {
                    let (a, _) = literal(&mut t)?;
                    let equal = match (t.next()?, t.peek()) {
                        (Tok::Punct('='), _) => true,
                        (Tok::Punct('<'), Some(Tok::Punct('>')))
                        | (Tok::Punct('!'), Some(Tok::Punct('='))) => {
                            t.next();
                            false
                        }
                        _ => return None,
                    };
                    let (b, _) = literal(&mut t)?;
                    never |= (a == b) != equal;
                }
                _ => condition(&mut t, &mut filters, &mut bare)?,
            }
            if wrapped && t.next()? != Tok::Punct(')') {
                return None;
            }
            if t.peek() == Some(&Tok::Word("and".into())) {
                t.next();
            } else {
                break;
            }
        }
    }
    let mut limit = never.then_some(0);
    if t.peek() == Some(&Tok::Word("limit".into())) {
        t.next();
        match t.next()? {
            Tok::Word(n) if n.bytes().all(|b| b.is_ascii_digit()) => {
                let n: u64 = n.parse().ok()?;
                limit = Some(limit.map_or(n, |l: u64| l.min(n)));
            }
            _ => return None,
        }
    }
    t.next().is_none().then_some(Plain {
        collection,
        columns,
        filters,
        limit,
        bare,
    })
}

/// A column's condition, onto `filters`, each of its literals' bareness
/// onto `bare` in the order `Plain::fenecql` numbers them.
fn condition(
    t: &mut std::iter::Peekable<impl Iterator<Item = Tok>>,
    filters: &mut Vec<Filter>,
    bare: &mut Vec<bool>,
) -> Option<()> {
    let col = ident(t.next()?)?;
    match t.next()? {
        // DuckDB splits a table into tasks by `ctid`, and asks a table it
        // reads whole for every page: that range is every row. fenecdb has
        // no row addresses, and part of a table named by them is refused
        // rather than read wrong.
        Tok::Word(w) if w == "between" && col == "ctid" => {
            let (from, _) = literal(t)?;
            (cast(t)? == "tid").then_some(())?;
            (t.next()? == Tok::Word("and".into())).then_some(())?;
            let (to, _) = literal(t)?;
            (cast(t)? == "tid").then_some(())?;
            (from == "(0,0)" && to.starts_with("(4294967295,")).then_some(())?;
        }
        Tok::Word(w) if w == "is" => {
            let not = t.peek() == Some(&Tok::Word("not".into()));
            if not {
                t.next();
            }
            (t.next()? == Tok::Word("null".into())).then_some(())?;
            filters.push(Filter::Null(col, not));
        }
        Tok::Word(w) if w == "in" => {
            (t.next()? == Tok::Punct('(')).then_some(())?;
            let mut lits = Vec::new();
            loop {
                let (lit, b) = literal(t)?;
                lits.push(lit);
                bare.push(b);
                match t.next()? {
                    Tok::Punct(',') => {}
                    Tok::Punct(')') => break,
                    _ => return None,
                }
            }
            filters.push(Filter::In(col, lits));
        }
        Tok::Punct(c) => {
            let op = match (c, t.peek()) {
                ('<', Some(Tok::Punct('>'))) | ('!', Some(Tok::Punct('='))) => {
                    t.next();
                    "!="
                }
                ('<', Some(Tok::Punct('='))) => {
                    t.next();
                    "<="
                }
                ('>', Some(Tok::Punct('='))) => {
                    t.next();
                    ">="
                }
                ('=', _) => "=",
                ('<', _) => "<",
                ('>', _) => ">",
                _ => return None,
            };
            let (lit, b) = literal(t)?;
            // A string's cast to its own type, and the byte order fenecdb
            // compares text in anyway.
            let _ = cast(t)?;
            if t.peek() == Some(&Tok::Word("collate".into())) {
                t.next();
                match t.next()? {
                    Tok::Name(c) | Tok::Word(c) if c == "C" || c == "c" => {}
                    _ => return None,
                }
            }
            filters.push(Filter::Cmp(col, op, lit));
            bare.push(b);
        }
        _ => return None,
    }
    Some(())
}

/// A name FenecQL writes bare.
fn ident(t: Tok) -> Option<String> {
    let (Tok::Word(w) | Tok::Name(w)) = t else {
        return None;
    };
    let mut b = w.bytes();
    let first = b.next()?;
    ((first.is_ascii_alphabetic() || first == b'_')
        && b.all(|c| c.is_ascii_alphanumeric() || c == b'_'))
    .then_some(w)
}

/// What a refused `ALTER TABLE` is answered with: its SQLSTATE and why.
pub type Refusal = (&'static str, String);

/// PostgreSQL's `ALTER TABLE t ADD [COLUMN] c type`, `DROP [COLUMN] c` and
/// `RENAME [COLUMN] a TO b`, which an ORM's migrations and a migration tool
/// send, as the FenecQL `alter collection` it is; `None` for a text that is
/// no `ALTER TABLE`. Only a type fenecdb has, and a change that rewrites no
/// document: a new type, a `NOT NULL`, a `DEFAULT` -- each a value every
/// row already there would need -- or several changes at once are refused
/// (`0A000`), as a name FenecQL cannot write is (`42602`).
pub fn alter(text: &str) -> Option<std::result::Result<String, Refusal>> {
    let head = text.trim_start().as_bytes();
    if !head
        .get(..5)
        .is_some_and(|w| w.eq_ignore_ascii_case(b"alter"))
    {
        return None;
    }
    let mut t = tokens(text);
    while t.last() == Some(&Tok::Punct(';')) {
        t.pop();
    }
    let word = |w: &str| Tok::Word(w.into());
    if t.len() < 2 || t[0] != word("alter") || t[1] != word("table") {
        return None;
    }
    Some(alter_table(&t[2..]))
}

/// The tokens of an `ALTER TABLE`, as its parts are read.
type Toks<'a> = std::iter::Peekable<std::iter::Cloned<std::slice::Iter<'a, Tok>>>;

/// Takes the word `w` if it is next.
fn eat(t: &mut Toks<'_>, w: &str) -> bool {
    let hit = matches!(t.peek(), Some(Tok::Word(x)) if x == w);
    if hit {
        t.next();
    }
    hit
}

fn alter_table(t: &[Tok]) -> std::result::Result<String, Refusal> {
    let refuse = |what: &str| Err(("0A000", format!("ALTER TABLE {what} is not supported")));
    let mut t = t.iter().cloned().peekable();
    if eat(&mut t, "if") {
        return refuse("IF EXISTS");
    }
    eat(&mut t, "only");
    let mut table = name(t.next())?;
    if t.peek() == Some(&Tok::Punct('.')) {
        if table != "public" {
            return Err((
                "3F000",
                format!("schema \"{table}\" does not exist; collections are in public"),
            ));
        }
        t.next();
        table = name(t.next())?;
    }
    if t.peek() == Some(&Tok::Punct('*')) {
        t.next();
    }
    let action = match t.next() {
        Some(Tok::Word(w)) => w,
        _ => return refuse("without an action"),
    };
    let fenecql = match action.as_str() {
        "add" => {
            if eat(&mut t, "constraint") {
                return refuse(
                    "ADD CONSTRAINT: a unique column is `create index on t (c) @unique` \
                     in FenecQL",
                );
            }
            eat(&mut t, "column");
            if eat(&mut t, "if") {
                return refuse("ADD COLUMN IF NOT EXISTS");
            }
            let column = name(t.next())?;
            let ty = column_type(&mut t)?;
            let mut extra = String::new();
            loop {
                match t.next() {
                    None => break,
                    Some(Tok::Word(w)) => match w.as_str() {
                        // Nullable is what an added field is.
                        "null" => {}
                        "unique" => extra.push_str(" @unique"),
                        "collate" => {
                            let c = match t.next() {
                                Some(Tok::Word(c) | Tok::Name(c)) => c,
                                _ => return refuse("COLLATE without a collation"),
                            };
                            match c.as_str() {
                                "und-x-icu" => extra.push_str(" collate und"),
                                "tr-x-icu" => extra.push_str(" collate tr"),
                                _ => {
                                    return Err((
                                        "42704",
                                        format!(
                                            "collation \"{c}\" does not exist; fenecdb has \
                                             und-x-icu and tr-x-icu"
                                        ),
                                    ))
                                }
                            }
                        }
                        "not" | "default" => {
                            return refuse(
                                "ADD COLUMN with NOT NULL or DEFAULT: the rows already there \
                                 hold no value for the column, and none is written for them",
                            )
                        }
                        other => {
                            return refuse(&format!(
                                "ADD COLUMN with {}",
                                other.to_ascii_uppercase()
                            ))
                        }
                    },
                    Some(Tok::Punct(',')) => return refuse("with several actions"),
                    Some(_) => return refuse("ADD COLUMN with what follows the type"),
                }
            }
            format!("alter collection {table} add field {column} {ty}{extra}")
        }
        "drop" => {
            if eat(&mut t, "constraint") {
                return refuse("DROP CONSTRAINT");
            }
            eat(&mut t, "column");
            if eat(&mut t, "if") {
                return refuse("DROP COLUMN IF EXISTS");
            }
            let column = name(t.next())?;
            // Nothing depends on a column: both are what is done anyway.
            if !eat(&mut t, "cascade") {
                eat(&mut t, "restrict");
            }
            if t.next().is_some() {
                return refuse("with several actions");
            }
            format!("alter collection {table} drop field {column}")
        }
        "rename" => {
            if eat(&mut t, "to") {
                return refuse("RENAME TO: a collection keeps its name");
            }
            if eat(&mut t, "constraint") {
                return refuse("RENAME CONSTRAINT");
            }
            eat(&mut t, "column");
            let from = name(t.next())?;
            if !eat(&mut t, "to") {
                return refuse("RENAME without TO");
            }
            let to = name(t.next())?;
            if t.next().is_some() {
                return refuse("with what follows RENAME");
            }
            format!("alter collection {table} rename field {from} to {to}")
        }
        "alter" => {
            return refuse(
                "ALTER COLUMN: a column's type, default and nullability do not change in \
                 place; add a column, update it from the old one, and drop the old one",
            )
        }
        other => return refuse(&other.to_ascii_uppercase()),
    };
    Ok(fenecql)
}

/// A name FenecQL can write, or the refusal of one it cannot.
fn name(t: Option<Tok>) -> std::result::Result<String, Refusal> {
    let shown = match &t {
        Some(Tok::Word(w) | Tok::Name(w)) => w.clone(),
        _ => String::new(),
    };
    t.and_then(ident).ok_or_else(|| {
        (
            "42602",
            format!("invalid name \"{shown}\": fenecdb names are letters, digits and _"),
        )
    })
}

/// `(n)` after a type: a vector's dimension, or a length text does not
/// keep; empty for one that is no number.
fn size(t: &mut Toks<'_>) -> Option<String> {
    if t.peek() != Some(&Tok::Punct('(')) {
        return None;
    }
    t.next();
    let n = match t.next() {
        Some(Tok::Word(n)) if n.bytes().all(|b| b.is_ascii_digit()) => n,
        _ => String::new(),
    };
    while !matches!(t.next(), Some(Tok::Punct(')')) | None) {}
    Some(n)
}

/// The FenecQL type of a PostgreSQL column type, for the types fenecdb
/// has: the ones the wire describes its own as, and their spellings.
fn column_type(t: &mut Toks<'_>) -> std::result::Result<String, Refusal> {
    let unknown = |ty: &str| {
        Err((
            "0A000",
            format!(
                "fenecdb has no {ty} type: bool, bigint, double precision, text, bytea, \
                     timestamptz, vector, halfvec, sparsevec, jsonb and arrays of them are its \
                     types"
            ),
        ))
    };
    let first = match t.next() {
        Some(Tok::Word(w) | Tok::Name(w)) => w,
        _ => return unknown("such"),
    };
    let mut ty = match first.as_str() {
        "text" | "varchar" => {
            size(t);
            "text".to_string()
        }
        "character" => {
            if t.peek() != Some(&Tok::Word("varying".into())) {
                return unknown("character(n)");
            }
            t.next();
            size(t);
            "text".to_string()
        }
        "int" | "integer" | "int2" | "int4" | "int8" | "bigint" | "smallint" => "int".into(),
        "real" | "float" | "float4" | "float8" => {
            size(t);
            "float".into()
        }
        "double" => {
            if t.next() != Some(Tok::Word("precision".into())) {
                return unknown("double");
            }
            "float".into()
        }
        "bool" | "boolean" => "bool".into(),
        "bytea" => "bytes".into(),
        "timestamptz" => "timestamp".into(),
        // A `json` field is jsonb on the wire; `json` is taken for it too.
        "jsonb" | "json" => "json".into(),
        "timestamp" => {
            size(t);
            // `with time zone` or `without`: a timestamp is UTC either way.
            if matches!(t.peek(), Some(Tok::Word(w)) if w == "with" || w == "without") {
                t.next();
                for w in ["time", "zone"] {
                    if t.next() != Some(Tok::Word(w.into())) {
                        return unknown("timestamp");
                    }
                }
            }
            "timestamp".into()
        }
        "vector" | "halfvec" | "sparsevec" => match size(t).filter(|n| !n.is_empty()) {
            Some(n) => match first.as_str() {
                "vector" => format!("vector<{n}>"),
                "halfvec" => format!("vector<{n}, f16>"),
                _ => format!("sparse<{n}>"),
            },
            None => return unknown(&format!("{first} without a dimension")),
        },
        other => return unknown(other),
    };
    // `text[]`: a list of them.
    if t.peek() == Some(&Tok::Punct('[')) {
        t.next();
        if t.next() != Some(Tok::Punct(']')) {
            return unknown("array of a size");
        }
        ty = format!("[{ty}]");
    }
    Ok(ty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duckdbs_reads_are_plain_selects() {
        // What DuckDB's postgres extension sends inside its COPY: the
        // columns, a vector cast to text, the whole table's ctid range and
        // the conditions it pushed down.
        let p = plain(
            r#"SELECT "id", "n", "e"::VARCHAR FROM "public"."big" WHERE ctid BETWEEN '(0,0)'::tid AND '(4294967295,0)'::tid AND "n" < '100' AND "g" = 'g3' COLLATE "C" AND "x" IS NOT NULL AND "t" IN ('a', 'b') AND "y" <> '-2'"#,
        )
        .unwrap();
        assert_eq!(p.collection, "big");
        assert_eq!(
            p.columns,
            [
                ("id".into(), false, None),
                ("n".into(), false, None),
                ("e".into(), true, None)
            ]
        );
        let (q, params) = p.fenecql();
        assert_eq!(
            q,
            "get big select id, n, e where n < $1 and g = $2 and x is not null and t in [$3, $4] and y != $5"
        );
        assert_eq!(
            params,
            [
                ("n", "100"),
                ("g", "g3"),
                ("t", "a"),
                ("t", "b"),
                ("y", "-2")
            ]
            .map(|(c, v)| (c.to_string(), v.to_string()))
        );
        assert_eq!(
            plain(r#"SELECT * FROM "t" WHERE "n" >= '1' AND "n" <= '9' LIMIT 5"#)
                .unwrap()
                .fenecql()
                .0,
            "get t where n >= $1 and n <= $2 limit 5"
        );
        // Part of a table by its row addresses, which fenecdb has none of,
        // and anything else a plain select is not.
        for other in [
            r#"SELECT "n" FROM "t" WHERE ctid BETWEEN '(0,0)'::tid AND '(1000,0)'::tid"#,
            r#"SELECT "n" FROM "t" WHERE "n" < '1' OR "n" > '2'"#,
            r#"SELECT "n"::int FROM "t""#,
            r#"SELECT "n" FROM "t" WHERE "g" = 'x' COLLATE "tr-x-icu""#,
            r#"SELECT "n" FROM "t" WHERE lower("g") = 'x'"#,
        ] {
            assert!(plain(other).is_none(), "{other}");
        }
    }

    #[test]
    fn sparks_reads_are_plain_selects() {
        // What Spark's JDBC reader sends: the columns, then each pushed-down
        // condition in parentheses, a number bare and text quoted.
        assert_eq!(
            select(r#"SELECT "n","g" FROM docs WHERE ("n" IS NOT NULL) AND ("g" IS NOT NULL) AND ("n" < 10) AND ("g" = 'g1')"#)
                .as_deref(),
            Some(r#"get docs select n, g where n is not null and g is not null and n < 10 and g = "g1""#)
        );
        assert_eq!(
            select(r#"SELECT "x" FROM docs WHERE ("g" IN ('g1','g2')) AND ("x" >= -2.5)"#)
                .as_deref(),
            Some(r#"get docs select x where g in ["g1", "g2"] and x >= -2.5"#)
        );
        // Its question for a table's columns, and its count.
        assert_eq!(
            select("SELECT * FROM docs WHERE 1=0").as_deref(),
            Some("get docs limit 0")
        );
        assert_eq!(
            select("SELECT * FROM docs WHERE 1=1 LIMIT 5").as_deref(),
            Some("get docs limit 5")
        );
        assert_eq!(
            constants(r#"SELECT 1 FROM docs WHERE ("n" > 5)"#),
            Some(("get docs where n > 5 count".into(), vec!["1".into()]))
        );
        assert_eq!(
            constants("SELECT 1 FROM docs"),
            Some(("get docs count".into(), vec!["1".into()]))
        );
        assert!(
            select("SELECT 1 FROM docs").is_none(),
            "a constant is not a column"
        );
        // Text written into the statement is escaped as FenecQL's.
        assert_eq!(
            select(r#"SELECT "n" FROM t WHERE ("g" = 'say "hi" \ there')"#).as_deref(),
            Some(r#"get t select n where g = "say \"hi\" \\ there""#)
        );
        assert!(
            select(r#"SELECT "n" FROM t WHERE ("g" = abc)"#).is_none(),
            "a bare word is no literal"
        );
    }

    #[test]
    fn a_select_of_columns_is_a_get() {
        assert_eq!(
            select(r#"SELECT "name", "n" FROM "docs" LIMIT 1"#).as_deref(),
            Some("get docs select name, n limit 1")
        );
        assert_eq!(
            select(r#"select "name" from "public"."Docs""#).as_deref(),
            Some("get Docs select name")
        );
        assert_eq!(select("select * from docs;").as_deref(), Some("get docs"));
        assert_eq!(
            select("select name from docs where n = 1").as_deref(),
            Some("get docs select name where n = 1")
        );
        for other in [
            "select count(*) from docs",
            "select a.name from docs a",
            "select name from other.docs",
            r#"select "a b" from docs"#,
            "select name from docs limit $1",
            "get docs",
            "selectx from docs",
        ] {
            assert_eq!(select(other), None, "{other}");
        }
    }

    /// What Django, Rails, Alembic and Prisma send for a column added,
    /// dropped or renamed, and what they get.
    #[test]
    fn alter_table_is_the_alter_collection_it_is() {
        for (sql, want) in [
            (
                r#"ALTER TABLE "orders" ADD COLUMN "note" varchar(100) NULL"#,
                "alter collection orders add field note text",
            ),
            (
                r#"ALTER TABLE "orders" ADD "paid_at" timestamp with time zone;"#,
                "alter collection orders add field paid_at timestamp",
            ),
            (
                "alter table public.orders add column qty integer",
                "alter collection orders add field qty int",
            ),
            (
                "ALTER TABLE ONLY orders ADD COLUMN email TEXT UNIQUE",
                "alter collection orders add field email text @unique",
            ),
            (
                "ALTER TABLE orders ADD COLUMN embedding vector(384)",
                "alter collection orders add field embedding vector<384>",
            ),
            (
                "ALTER TABLE orders ADD COLUMN h halfvec(8)",
                "alter collection orders add field h vector<8, f16>",
            ),
            (
                "ALTER TABLE orders ADD COLUMN s sparsevec(30522)",
                "alter collection orders add field s sparse<30522>",
            ),
            (
                "ALTER TABLE orders ADD COLUMN tags text[]",
                "alter collection orders add field tags [text]",
            ),
            (
                "ALTER TABLE orders ADD COLUMN meta jsonb",
                "alter collection orders add field meta json",
            ),
            (
                "ALTER TABLE orders ADD COLUMN w double precision",
                "alter collection orders add field w float",
            ),
            (
                r#"ALTER TABLE orders ADD COLUMN name text COLLATE "und-x-icu""#,
                "alter collection orders add field name text collate und",
            ),
            (
                r#"ALTER TABLE "orders" DROP COLUMN "note" CASCADE"#,
                "alter collection orders drop field note",
            ),
            (
                "ALTER TABLE orders DROP qty",
                "alter collection orders drop field qty",
            ),
            (
                r#"ALTER TABLE "orders" RENAME COLUMN "total" TO "amount""#,
                "alter collection orders rename field total to amount",
            ),
            (
                "ALTER TABLE orders RENAME total TO amount",
                "alter collection orders rename field total to amount",
            ),
        ] {
            assert_eq!(alter(sql), Some(Ok(want.to_string())), "{sql}");
        }
        for (sql, code) in [
            ("ALTER TABLE orders ALTER COLUMN total TYPE text", "0A000"),
            (
                "ALTER TABLE orders ALTER COLUMN total SET NOT NULL",
                "0A000",
            ),
            ("ALTER TABLE orders ADD COLUMN n int NOT NULL", "0A000"),
            ("ALTER TABLE orders ADD COLUMN n int DEFAULT 0", "0A000"),
            ("ALTER TABLE orders ADD COLUMN n numeric(10, 2)", "0A000"),
            ("ALTER TABLE orders ADD COLUMN n xml", "0A000"),
            (
                "ALTER TABLE orders ADD COLUMN n int, ADD COLUMN m int",
                "0A000",
            ),
            ("ALTER TABLE orders ADD COLUMN IF NOT EXISTS n int", "0A000"),
            ("ALTER TABLE orders RENAME TO purchases", "0A000"),
            ("ALTER TABLE orders ADD CONSTRAINT c UNIQUE (n)", "0A000"),
            (r#"ALTER TABLE orders ADD COLUMN "a b" int"#, "42602"),
            ("ALTER TABLE other.orders DROP COLUMN n", "3F000"),
            (
                r#"ALTER TABLE orders ADD COLUMN n text COLLATE "C""#,
                "42704",
            ),
        ] {
            match alter(sql) {
                Some(Err((c, _))) => assert_eq!(c, code, "{sql}"),
                other => panic!("{sql}: {other:?}"),
            }
        }
        assert_eq!(alter("ALTER INDEX x RENAME TO y"), None);
        assert_eq!(alter("select 1"), None);
    }
}
