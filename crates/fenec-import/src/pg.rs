//! PostgreSQL source: turns a `COPY ... TO STDOUT` stream into rows.
//!
//! Type information comes from the OIDs in `RowDescription`; since
//! pgvector's OID belongs to an extension type it is not fixed and is
//! learned from `pg_type`.
//!
//! Why text format and not binary: a binary COPY needs a separate decoder
//! per type. In text format a single escape decoder suffices, and it is
//! consistent with `fenec-pg`'s own choice (see `fenec_pg::proto`).

use crate::{Column, Source};
use fenec_core::error::{Error, Result};
use fenec_core::value::{DataType, VecPrec, Value};
use fenec_pg::client::{Client, CopyOut, FieldDesc};

/// The connection string. Passed through from `fenec_pg::client` as is.
pub use fenec_pg::client::Url;

// PostgreSQL builtin type OIDs.
const BOOL: i32 = 16;
const BYTEA: i32 = 17;
const INT8: i32 = 20;
const INT2: i32 = 21;
const INT4: i32 = 23;
const TEXT: i32 = 25;
const JSON: i32 = 114;
const FLOAT4: i32 = 700;
const FLOAT8: i32 = 701;
const BPCHAR: i32 = 1042;
const VARCHAR: i32 = 1043;
const DATE: i32 = 1082;
const TIMESTAMP: i32 = 1114;
const TIMESTAMPTZ: i32 = 1184;
const NUMERIC: i32 = 1700;
const UUID: i32 = 2950;
const JSONB: i32 = 3802;

const INT2_ARRAY: i32 = 1005;
const INT4_ARRAY: i32 = 1007;
const TEXT_ARRAY: i32 = 1009;
const INT8_ARRAY: i32 = 1016;
const FLOAT4_ARRAY: i32 = 1021;
const FLOAT8_ARRAY: i32 = 1022;

/// The OIDs of the pgvector types in this database.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VectorOids {
    pub vector: Option<i32>,
    pub halfvec: Option<i32>,
}

/// How a column is decoded from COPY text.
#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Bool,
    Int,
    Float,
    /// Text, timestamps, and anything with no counterpart that can still be
    /// taken with `--cast text`. A timestamp is parsed by `Value::coerce`.
    Text,
    /// `\x…` hexadecimal notation.
    Bytea,
    /// pgvector `[1,2,3]`.
    Vector,
    /// A PostgreSQL array `{1,2,3}`.
    Array(Box<Kind>),
}

/// The fenecdb type and decoder for an OID.
fn map_oid(f: &FieldDesc, oids: &VectorOids) -> (Column, Kind) {
    let name = f.name.as_str();
    let col = |ty: DataType, src: &str| Column::new(name, ty, src);

    if Some(f.oid) == oids.vector || Some(f.oid) == oids.halfvec {
        let prec = if Some(f.oid) == oids.halfvec {
            VecPrec::F16
        } else {
            VecPrec::F32
        };
        let tyname = if prec == VecPrec::F16 { "halfvec" } else { "vector" };
        // pgvector carries the dimension in typmod; when it is not declared
        // the column is defined as `vector` and the dimension can vary per row.
        return if f.typmod > 0 {
            (
                col(
                    DataType::Vector(f.typmod as usize, prec),
                    &format!("{tyname}({})", f.typmod),
                ),
                Kind::Vector,
            )
        } else {
            (
                Column::unsupported(
                    name,
                    tyname,
                    "the dimension is not declared; supply it with `--cast <field>=vector<N>`",
                ),
                Kind::Vector,
            )
        };
    }

    match f.oid {
        BOOL => (col(DataType::Bool, "bool"), Kind::Bool),
        INT2 => (col(DataType::Int, "int2"), Kind::Int),
        INT4 => (col(DataType::Int, "int4"), Kind::Int),
        INT8 => (col(DataType::Int, "int8"), Kind::Int),
        FLOAT4 => (col(DataType::Float, "float4"), Kind::Float),
        FLOAT8 => (col(DataType::Float, "float8"), Kind::Float),
        TEXT => (col(DataType::Text, "text"), Kind::Text),
        BPCHAR => (col(DataType::Text, "bpchar"), Kind::Text),
        VARCHAR => (col(DataType::Text, "varchar"), Kind::Text),
        BYTEA => (col(DataType::Bytes, "bytea"), Kind::Bytea),
        DATE => (col(DataType::Timestamp, "date"), Kind::Text),
        TIMESTAMP => (col(DataType::Timestamp, "timestamp"), Kind::Text),
        TIMESTAMPTZ => (col(DataType::Timestamp, "timestamptz"), Kind::Text),
        UUID => (
            col(DataType::Text, "uuid").note("uuid taken as `text`; it can be indexed with `@hash`"),
            Kind::Text,
        ),
        INT2_ARRAY | INT4_ARRAY | INT8_ARRAY => (
            col(DataType::List(Box::new(DataType::Int)), "int[]"),
            Kind::Array(Box::new(Kind::Int)),
        ),
        FLOAT4_ARRAY | FLOAT8_ARRAY => (
            col(DataType::List(Box::new(DataType::Float)), "float[]"),
            Kind::Array(Box::new(Kind::Float)),
        ),
        TEXT_ARRAY => (
            col(DataType::List(Box::new(DataType::Text)), "text[]"),
            Kind::Array(Box::new(Kind::Text)),
        ),
        NUMERIC => (
            Column::unsupported(
                name,
                "numeric",
                "fenecdb has no decimal; `text` keeps the exact value, `float` rounds it",
            ),
            Kind::Text,
        ),
        JSON | JSONB => (
            Column::unsupported(
                name,
                if f.oid == JSON { "json" } else { "jsonb" },
                "fenecdb has no nested objects; a field to filter on has to be its own column",
            ),
            Kind::Text,
        ),
        other => (
            Column::unsupported(
                name,
                format!("oid {other}"),
                "unrecognised PostgreSQL type",
            ),
            Kind::Text,
        ),
    }
}

// ------------------------------------------------------- COPY text format

/// Splits one line of COPY text format into cells.
///
/// Columns are tab separated; since a tab inside a value is escaped as `\t`,
/// splitting on a raw tab is safe. `\N` means NULL.
fn split_line(line: &[u8]) -> Vec<Option<Vec<u8>>> {
    line.split(|&b| b == b'\t')
        .map(|f| {
            if f == b"\\N" {
                None
            } else {
                Some(unescape(f))
            }
        })
        .collect()
}

/// Decodes the COPY text escapes.
fn unescape(f: &[u8]) -> Vec<u8> {
    if !f.contains(&b'\\') {
        return f.to_vec();
    }
    let mut out = Vec::with_capacity(f.len());
    let mut i = 0;
    while i < f.len() {
        if f[i] != b'\\' {
            out.push(f[i]);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&c) = f.get(i) else {
            out.push(b'\\');
            break;
        };
        i += 1;
        out.push(match c {
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            b'0'..=b'7' => {
                let mut v = (c - b'0') as u32;
                let mut taken = 1;
                while taken < 3 {
                    match f.get(i) {
                        Some(&d @ b'0'..=b'7') => {
                            v = v * 8 + (d - b'0') as u32;
                            i += 1;
                            taken += 1;
                        }
                        _ => break,
                    }
                }
                v as u8
            }
            b'x' => {
                let mut v = 0u32;
                let mut taken = 0;
                while taken < 2 {
                    match f.get(i).and_then(|d| (*d as char).to_digit(16)) {
                        Some(d) => {
                            v = v * 16 + d;
                            i += 1;
                            taken += 1;
                        }
                        None => break,
                    }
                }
                // `\x` on its own is not an escape; it is left as is.
                if taken == 0 {
                    b'x'
                } else {
                    v as u8
                }
            }
            // `\\` and undefined escapes: the character itself.
            other => other,
        });
    }
    out
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn parse_cell(raw: &[u8], kind: &Kind, column: &str) -> Result<Value> {
    let bad = |want: &str| {
        Error::Type(format!(
            "`{column}` expected {want}, could not parse `{}`",
            text(raw)
        ))
    };
    Ok(match kind {
        Kind::Bool => match raw {
            b"t" => Value::Bool(true),
            b"f" => Value::Bool(false),
            _ => return Err(bad("a bool")),
        },
        Kind::Int => Value::Int(text(raw).trim().parse().map_err(|_| bad("an integer"))?),
        Kind::Float => {
            // PostgreSQL can produce `NaN`, `Infinity` and `-Infinity`.
            let s = text(raw);
            let v = match s.trim() {
                "NaN" => f64::NAN,
                "Infinity" => f64::INFINITY,
                "-Infinity" => f64::NEG_INFINITY,
                other => other.parse().map_err(|_| bad("a decimal"))?,
            };
            Value::Float(v)
        }
        Kind::Text => Value::Text(text(raw)),
        Kind::Bytea => Value::Bytes(hex_bytea(raw).ok_or_else(|| bad("a bytea"))?),
        Kind::Vector => {
            let s = text(raw);
            let inner = s
                .trim()
                .strip_prefix('[')
                .and_then(|r| r.strip_suffix(']'))
                .ok_or_else(|| bad("a vector"))?;
            if inner.trim().is_empty() {
                return Ok(Value::Vector(Vec::new()));
            }
            let mut v = Vec::new();
            for part in inner.split(',') {
                v.push(part.trim().parse::<f32>().map_err(|_| bad("a vector"))?);
            }
            Value::Vector(v)
        }
        Kind::Array(inner) => {
            let items = split_array(raw).ok_or_else(|| bad("an array"))?;
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(match item {
                    None => Value::Null,
                    Some(b) => parse_cell(&b, inner, column)?,
                });
            }
            Value::List(out)
        }
    })
}

/// `\x48656c6c6f` -> bytes. This has been the default format since PostgreSQL 9.0.
fn hex_bytea(raw: &[u8]) -> Option<Vec<u8>> {
    let h = raw.strip_prefix(b"\\x")?;
    if h.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(h.len() / 2);
    for pair in h.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// `{1,2,3}` or `{"a","b",NULL}` -> items. Inside a quoted item `\` escapes.
fn split_array(raw: &[u8]) -> Option<Vec<Option<Vec<u8>>>> {
    let s = raw.strip_prefix(b"{")?.strip_suffix(b"}")?;
    if s.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    let mut cur = Vec::new();
    let mut quoted = false;
    let mut was_quoted = false;
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if quoted {
            match c {
                b'\\' if i + 1 < s.len() => {
                    cur.push(s[i + 1]);
                    i += 2;
                    continue;
                }
                b'"' => quoted = false,
                _ => cur.push(c),
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => {
                quoted = true;
                was_quoted = true;
            }
            b',' => {
                out.push(finish_item(&cur, was_quoted));
                cur.clear();
                was_quoted = false;
            }
            _ => cur.push(c),
        }
        i += 1;
    }
    out.push(finish_item(&cur, was_quoted));
    Some(out)
}

/// An unquoted `NULL` means an absent value; a quoted one is the text `"NULL"`.
fn finish_item(cur: &[u8], was_quoted: bool) -> Option<Vec<u8>> {
    if !was_quoted && cur.eq_ignore_ascii_case(b"NULL") {
        None
    } else {
        Some(cur.to_vec())
    }
}

// ------------------------------------------------------------------ reader

/// The source table and its narrowing options.
#[derive(Debug, Clone, Default)]
pub struct Query {
    /// Table name; `schema.table` is allowed too.
    pub table: String,
    /// The `--where` expression, passed through as SQL.
    pub filter: Option<String>,
    /// `--limit`
    pub limit: Option<u64>,
}

impl Query {
    pub fn table(name: impl Into<String>) -> Query {
        Query {
            table: name.into(),
            ..Default::default()
        }
    }
}

/// A source that reads a PostgreSQL table from a `COPY` stream.
pub struct Reader {
    copy: CopyOut,
    columns: Vec<Column>,
    kinds: Vec<Kind>,
}

impl Reader {
    /// Connects, learns the column types and starts the COPY stream.
    pub fn open(url: &Url, query: &Query) -> Result<Reader> {
        let mut client = Client::connect(url)?;
        let oids = vector_oids(&mut client)?;
        let table = quote_ident(&query.table)?;

        // Types are learned first: once COPY starts, no query can run on the
        // same connection.
        let desc = client.query(&format!("select * from {table} limit 0"))?;
        if desc.columns.is_empty() {
            return Err(Error::NotFound(format!(
                "no column was found in table `{}`",
                query.table
            )));
        }
        let (columns, kinds): (Vec<Column>, Vec<Kind>) =
            desc.columns.iter().map(|f| map_oid(f, &oids)).unzip();

        let mut sql = format!("copy (select * from {table}");
        if let Some(w) = &query.filter {
            sql.push_str(" where ");
            sql.push_str(w);
        }
        if let Some(n) = query.limit {
            sql.push_str(&format!(" limit {n}"));
        }
        sql.push_str(") to stdout");

        Ok(Reader {
            copy: client.copy_out(&sql)?,
            columns,
            kinds,
        })
    }
}

impl Source for Reader {
    fn columns(&mut self) -> Result<Vec<Column>> {
        Ok(self.columns.clone())
    }

    fn next_row(&mut self) -> Result<Option<Vec<Value>>> {
        let Some(line) = self.copy.next_line()? else {
            return Ok(None);
        };
        let cells = split_line(&line);
        if cells.len() != self.kinds.len() {
            return Err(Error::Corrupt(format!(
                "the COPY row has {} cells, {} columns were expected",
                cells.len(),
                self.kinds.len()
            )));
        }
        let mut out = Vec::with_capacity(cells.len());
        for ((cell, kind), col) in cells.into_iter().zip(&self.kinds).zip(&self.columns) {
            out.push(match cell {
                None => Value::Null,
                Some(raw) => parse_cell(&raw, kind, &col.name)?,
            });
        }
        Ok(Some(out))
    }
}

/// Counts the rows in the table.
///
/// It is not cheap: `count(*)` does a full scan. Since no query can run on
/// the same connection once the COPY stream has started, this opens its own.
pub fn count_rows(url: &Url, query: &Query) -> Result<u64> {
    let table = quote_ident(&query.table)?;
    let mut inner = format!("select 1 from {table}");
    if let Some(w) = &query.filter {
        inner.push_str(" where ");
        inner.push_str(w);
    }
    if let Some(n) = query.limit {
        inner.push_str(&format!(" limit {n}"));
    }
    let mut client = Client::connect(url)?;
    let r = client.query(&format!("select count(*) from ({inner}) s"))?;
    r.rows
        .first()
        .and_then(|row| row.first())
        .and_then(|c| c.as_ref())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::Corrupt("postgres: count(*) did not return a number".into()))
}

/// Learns the OIDs of the pgvector types when the extension is installed.
pub fn vector_oids(client: &mut Client) -> Result<VectorOids> {
    let r = client.query("select typname, oid from pg_type where typname in ('vector', 'halfvec')")?;
    let mut out = VectorOids::default();
    for row in &r.rows {
        let (Some(name), Some(oid)) = (row.first(), row.get(1)) else {
            continue;
        };
        let Some(oid) = oid.as_ref().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        match name.as_deref() {
            Some("vector") => out.vector = Some(oid),
            Some("halfvec") => out.halfvec = Some(oid),
            _ => {}
        }
    }
    Ok(out)
}

/// Quotes an identifier. `schema.table` is handled as two parts.
fn quote_ident(name: &str) -> Result<String> {
    if name.trim().is_empty() {
        return Err(Error::Query("the table name is empty".into()));
    }
    let mut out = String::new();
    for (i, part) in name.split('.').enumerate() {
        if part.contains('"') {
            return Err(Error::Query(format!(
                "a table name cannot contain a double quote: `{name}`"
            )));
        }
        if part.is_empty() {
            return Err(Error::Query(format!("invalid table name: `{name}`")));
        }
        if i > 0 {
            out.push('.');
        }
        out.push('"');
        out.push_str(part);
        out.push('"');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(name: &str, oid: i32, typmod: i32) -> FieldDesc {
        FieldDesc {
            name: name.into(),
            oid,
            typmod,
        }
    }

    #[test]
    fn oids_map_to_fenecdb_types() {
        let o = VectorOids::default();
        let ty = |oid| map_oid(&f("x", oid, -1), &o).0.ty;
        assert_eq!(ty(BOOL), Some(DataType::Bool));
        assert_eq!(ty(INT2), Some(DataType::Int));
        assert_eq!(ty(INT8), Some(DataType::Int));
        assert_eq!(ty(FLOAT4), Some(DataType::Float));
        assert_eq!(ty(VARCHAR), Some(DataType::Text));
        assert_eq!(ty(BYTEA), Some(DataType::Bytes));
        assert_eq!(ty(TIMESTAMPTZ), Some(DataType::Timestamp));
        assert_eq!(ty(UUID), Some(DataType::Text));
        assert_eq!(ty(INT4_ARRAY), Some(DataType::List(Box::new(DataType::Int))));
        assert_eq!(
            ty(TEXT_ARRAY),
            Some(DataType::List(Box::new(DataType::Text)))
        );
        // The ones with no counterpart require `--cast`.
        assert_eq!(ty(NUMERIC), None);
        assert_eq!(ty(JSONB), None);
        assert_eq!(ty(9999), None);
    }

    /// pgvector's OID is not fixed; the dimension comes from typmod.
    #[test]
    fn pgvector_dimension_comes_from_typmod() {
        let o = VectorOids {
            vector: Some(16385),
            halfvec: Some(16390),
        };
        let (c, k) = map_oid(&f("embed", 16385, 384), &o);
        assert_eq!(c.ty, Some(DataType::Vector(384, VecPrec::F32)));
        assert_eq!(k, Kind::Vector);
        let (c, _) = map_oid(&f("embed", 16390, 768), &o);
        assert_eq!(c.ty, Some(DataType::Vector(768, VecPrec::F16)));
        // A `vector` column with no dimension: never guessed silently.
        let (c, _) = map_oid(&f("embed", 16385, -1), &o);
        assert_eq!(c.ty, None);
        assert!(c.note.unwrap().contains("vector<N>"));
    }

    #[test]
    fn copy_escapes_are_decoded() {
        let cells = split_line(b"a\\tb\tline\\nend\t\\\\back\t\\N\t");
        assert_eq!(cells[0].as_deref(), Some(&b"a\tb"[..]));
        assert_eq!(cells[1].as_deref(), Some(&b"line\nend"[..]));
        assert_eq!(cells[2].as_deref(), Some(&b"\\back"[..]));
        assert_eq!(cells[3], None, "\\N must be NULL");
        assert_eq!(cells[4].as_deref(), Some(&b""[..]), "an empty string is not NULL");
    }

    #[test]
    fn octal_and_hex_escapes() {
        assert_eq!(unescape(b"\\101\\102"), b"AB");
        assert_eq!(unescape(b"\\x41\\x42"), b"AB");
        // No more than three digits may be read.
        assert_eq!(unescape(b"\\1011"), b"A1");
    }

    #[test]
    fn cells_parse_by_kind() {
        let p = |raw: &[u8], k: Kind| parse_cell(raw, &k, "x").unwrap();
        assert_eq!(p(b"t", Kind::Bool), Value::Bool(true));
        assert_eq!(p(b"f", Kind::Bool), Value::Bool(false));
        assert_eq!(p(b"-42", Kind::Int), Value::Int(-42));
        assert_eq!(p(b"2.5", Kind::Float), Value::Float(2.5));
        assert_eq!(p(b"\\x48690a", Kind::Bytea), Value::Bytes(vec![0x48, 0x69, 0x0a]));
        assert_eq!(
            p(b"[0.5,-1,2]", Kind::Vector),
            Value::Vector(vec![0.5, -1.0, 2.0])
        );
        // A timestamp passes through as text; `coerce` parses it.
        assert_eq!(
            p(b"2026-09-19 12:34:56.789+00", Kind::Text),
            Value::Text("2026-09-19 12:34:56.789+00".into())
        );
    }

    /// PostgreSQL can produce `NaN`/`Infinity`; those are not errors.
    #[test]
    fn special_floats_survive() {
        let Value::Float(v) = parse_cell(b"NaN", &Kind::Float, "x").unwrap() else {
            panic!()
        };
        assert!(v.is_nan());
        assert_eq!(
            parse_cell(b"-Infinity", &Kind::Float, "x").unwrap(),
            Value::Float(f64::NEG_INFINITY)
        );
    }

    #[test]
    fn arrays_become_lists() {
        let k = Kind::Array(Box::new(Kind::Int));
        assert_eq!(
            parse_cell(b"{1,2,3}", &k, "x").unwrap(),
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
        );
        assert_eq!(parse_cell(b"{}", &k, "x").unwrap(), Value::List(vec![]));

        let k = Kind::Array(Box::new(Kind::Text));
        assert_eq!(
            parse_cell(br#"{"a,b","c\"d",NULL,"NULL"}"#, &k, "x").unwrap(),
            Value::List(vec![
                Value::Text("a,b".into()),
                Value::Text("c\"d".into()),
                Value::Null,
                Value::Text("NULL".into()),
            ]),
            "an unquoted NULL is an absent value, a quoted one is text"
        );
    }

    #[test]
    fn bad_cells_name_the_column() {
        let e = parse_cell(b"abc", &Kind::Int, "score").unwrap_err().to_string();
        assert!(e.contains("`score`"), "{e}");
    }

    #[test]
    fn identifiers_are_quoted() {
        assert_eq!(quote_ident("docs").unwrap(), "\"docs\"");
        assert_eq!(quote_ident("public.docs").unwrap(), "\"public\".\"docs\"");
        assert!(quote_ident("a\"; drop table x --").is_err());
        assert!(quote_ident("").is_err());
        assert!(quote_ident("a..b").is_err());
    }
}
