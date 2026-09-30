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
    if !p.filters.is_empty() {
        return None;
    }
    let (q, _) = p.fenecql();
    Some((q, p.columns.iter().map(|c| c.1).collect()))
}

/// A `SELECT` of columns from one collection, as DuckDB's postgres
/// extension reads a table: its columns, each cast to text or not, the
/// conditions it pushes down -- a column compared with a literal, `IS [NOT]
/// NULL`, `IN (...)` -- joined by `AND`, and a `LIMIT`.
#[derive(Debug, PartialEq)]
pub struct Plain {
    pub collection: String,
    /// Empty: `*`.
    pub columns: Vec<(String, bool)>,
    pub filters: Vec<Filter>,
    pub limit: Option<u64>,
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
}

/// A literal: a string, or a number or boolean written bare.
fn literal(t: &mut std::iter::Peekable<impl Iterator<Item = Tok>>) -> Option<String> {
    match t.next()? {
        Tok::Str(s) => Some(s),
        Tok::Word(w) => Some(w),
        Tok::Punct('-') => match t.next()? {
            Tok::Word(w) if w.bytes().all(|b| b.is_ascii_digit()) => Some(format!("-{w}")),
            _ => None,
        },
        _ => None,
    }
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
            let name = ident(t.next()?)?;
            let as_text = match cast(&mut t)?.as_str() {
                "" => false,
                "varchar" | "text" => true,
                _ => return None,
            };
            columns.push((name, as_text));
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
    let mut filters = Vec::new();
    if t.peek() == Some(&Tok::Word("where".into())) {
        t.next();
        loop {
            let col = ident(t.next()?)?;
            match t.next()? {
                // DuckDB splits a table into tasks by `ctid`, and asks a
                // table it reads whole for every page: that range is every
                // row. fenecdb has no row addresses, and part of a table
                // named by them is refused rather than read wrong.
                Tok::Word(w) if w == "between" && col == "ctid" => {
                    let from = literal(&mut t)?;
                    (cast(&mut t)? == "tid").then_some(())?;
                    (t.next()? == Tok::Word("and".into())).then_some(())?;
                    let to = literal(&mut t)?;
                    (cast(&mut t)? == "tid").then_some(())?;
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
                        lits.push(literal(&mut t)?);
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
                    let lit = literal(&mut t)?;
                    // A string's cast to its own type, and the byte order
                    // fenecdb compares text in anyway.
                    let _ = cast(&mut t)?;
                    if t.peek() == Some(&Tok::Word("collate".into())) {
                        t.next();
                        match t.next()? {
                            Tok::Name(c) | Tok::Word(c) if c == "C" || c == "c" => {}
                            _ => return None,
                        }
                    }
                    filters.push(Filter::Cmp(col, op, lit));
                }
                _ => return None,
            }
            if t.peek() == Some(&Tok::Word("and".into())) {
                t.next();
            } else {
                break;
            }
        }
    }
    let mut limit = None;
    if t.peek() == Some(&Tok::Word("limit".into())) {
        t.next();
        match t.next()? {
            Tok::Word(n) if n.bytes().all(|b| b.is_ascii_digit()) => limit = Some(n.parse().ok()?),
            _ => return None,
        }
    }
    t.next().is_none().then_some(Plain {
        collection,
        columns,
        filters,
        limit,
    })
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
                ("id".into(), false),
                ("n".into(), false),
                ("e".into(), true)
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
        for other in [
            "select name from docs where n = 1",
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
}
