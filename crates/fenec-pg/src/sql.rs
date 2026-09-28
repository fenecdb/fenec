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
    } else {
        loop {
            columns.push(ident(t.next()?)?);
            match t.next()? {
                Tok::Punct(',') => {}
                Tok::Word(w) if w == "from" => break,
                _ => return None,
            }
        }
    }
    if columns.is_empty() && t.next()? != Tok::Word("from".into()) {
        return None;
    }
    let mut collection = ident(t.next()?)?;
    if t.peek() == Some(&Tok::Punct('.')) {
        if collection != "public" {
            return None;
        }
        t.next();
        collection = ident(t.next()?)?;
    }
    let mut q = format!("get {collection}");
    if !columns.is_empty() {
        q.push_str(" select ");
        q.push_str(&columns.join(", "));
    }
    match t.next() {
        None => {}
        Some(Tok::Word(w)) if w == "limit" => match t.next()? {
            Tok::Word(n) if n.bytes().all(|b| b.is_ascii_digit()) => {
                q.push_str(" limit ");
                q.push_str(&n);
            }
            _ => return None,
        },
        _ => return None,
    }
    t.next().is_none().then_some(q)
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
