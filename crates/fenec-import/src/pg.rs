//! PostgreSQL source: turns a `COPY ... TO STDOUT` stream into rows.
//!
//! Type information comes from the OIDs in `RowDescription`; since
//! pgvector's OID belongs to an extension type it is not fixed and is
//! learned from `pg_type`.
//!
//! Why text format and not binary: a binary COPY needs a separate decoder
//! per type. In text format a single escape decoder suffices.

use crate::{Column, Source};
use fenec_core::error::{Error, Result};
use fenec_core::value::{DataType, Value, VecPrec};
use fenec_wire::client::{Client, CopyOut, FieldDesc};

/// The connection string. Passed through from `fenec_wire::client` as is.
pub use fenec_wire::client::Url;

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

const POINT: i32 = 600;

/// The OIDs of the pgvector and PostGIS types in this database: an
/// extension's types are numbered as it is installed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VectorOids {
    pub vector: Option<i32>,
    pub halfvec: Option<i32>,
    pub sparsevec: Option<i32>,
    pub geometry: Option<i32>,
    pub geography: Option<i32>,
}

/// How a column is decoded from COPY text.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Kind {
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
    /// `json` and `jsonb`: their text, read as JSON, every number as
    /// written.
    Json,
    /// PostgreSQL's own `point`, `(x,y)`: taken as `[lon, lat]`.
    Point,
    /// A PostGIS `geometry` or `geography` point, which COPY writes as
    /// hex EWKB: its byte order, its type -- a point, with or without an
    /// SRID, a Z or an M -- and its x and y.
    Ewkb,
}

/// The fenecdb type and decoder for an OID.
pub(crate) fn map_oid(f: &FieldDesc, oids: &VectorOids) -> (Column, Kind) {
    let name = f.name.as_str();
    let col = |ty: DataType, src: &str| Column::new(name, ty, src);

    if Some(f.oid) == oids.vector || Some(f.oid) == oids.halfvec {
        let prec = if Some(f.oid) == oids.halfvec {
            VecPrec::F16
        } else {
            VecPrec::F32
        };
        let tyname = if prec == VecPrec::F16 {
            "halfvec"
        } else {
            "vector"
        };
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

    // A `sparsevec` arrives in the text form `sparse<N>` reads,
    // `{1:0.5,3:0.25}/N`, so it is taken as text and read by the schema.
    if Some(f.oid) == oids.sparsevec {
        return if f.typmod > 0 {
            (
                col(
                    DataType::Sparse(f.typmod as usize),
                    &format!("sparsevec({})", f.typmod),
                ),
                Kind::Text,
            )
        } else {
            (
                Column::unsupported(
                    name,
                    "sparsevec",
                    "the dimension is not declared; supply it with `--cast <field>=sparse<N>`",
                ),
                Kind::Text,
            )
        };
    }

    // A PostGIS column is a point field when its typmod says it holds
    // points -- or says nothing, each row's own EWKB then checked -- in
    // longitude and latitude: SRID 4326, or none declared. Points of
    // another reference system are metres or feet on a plane, not degrees.
    if Some(f.oid) == oids.geometry || Some(f.oid) == oids.geography {
        let tyname = match Some(f.oid) == oids.geometry {
            true => "geometry",
            false => "geography",
        };
        // liblwgeom's TYPMOD_GET_TYPE and TYPMOD_GET_SRID.
        let (shape, srid) = match f.typmod {
            -1 => (0, 0),
            t => (
                (t & 0xFC) >> 2,
                ((t & 0x0FFF_FF00) - (t & 0x1000_0000)) >> 8,
            ),
        };
        let unsupported = |why: &str| (Column::unsupported(name, tyname, why), Kind::Ewkb);
        return match (shape, srid) {
            (0 | 1, 0 | 4326) => (col(DataType::Geo, tyname), Kind::Ewkb),
            (0 | 1, _) => unsupported(&format!(
                "SRID {srid} is not longitude and latitude; `ST_Transform(.., 4326)` it first"
            )),
            _ => unsupported("a `geo` field holds points, and this column other shapes"),
        };
    }

    match f.oid {
        POINT => (
            col(DataType::Geo, "point").note("point (x,y) taken as [lon, lat]"),
            Kind::Point,
        ),
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
            col(DataType::Text, "uuid")
                .note("uuid taken as `text`; it can be indexed with `@hash`"),
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
        JSON => (col(DataType::Json, "json"), Kind::Json),
        JSONB => (col(DataType::Json, "jsonb"), Kind::Json),
        other => (
            Column::unsupported(name, format!("oid {other}"), "unrecognised PostgreSQL type"),
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
        .map(|f| if f == b"\\N" { None } else { Some(unescape(f)) })
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

pub(crate) fn parse_cell(raw: &[u8], kind: &Kind, column: &str) -> Result<Value> {
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
        Kind::Json => fenec_core::json::parse_json(&text(raw)).map_err(|_| bad("JSON"))?,
        Kind::Point => {
            let s = text(raw);
            let (x, y) = s
                .trim()
                .strip_prefix('(')
                .and_then(|r| r.strip_suffix(')'))
                .and_then(|r| r.split_once(','))
                .ok_or_else(|| bad("a point"))?;
            let n = |t: &str| fenec_core::num::parse_f64(t.trim()).ok_or_else(|| bad("a point"));
            fenec_core::codec::geo_value((n(x)?, n(y)?))
        }
        Kind::Ewkb => match ewkb_point(raw) {
            Some(Some(p)) => fenec_core::codec::geo_value(p),
            Some(None) => Value::Null,
            None => return Err(bad("a point in longitude and latitude (SRID 4326)")),
        },
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
                // `str::parse`, and so still `core`'s 12 KB table of powers
                // of five -- which is why the default `fenec` binary carries
                // it while `fenec-server` and `make small` no longer do. Going
                // through `num::parse_f64` and narrowing would round twice,
                // and a component is not worth being slightly wrong about.
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

/// A point out of hex EWKB, as COPY writes a PostGIS value: `Some(None)`
/// for an empty point, `None` for anything that is not a point in
/// longitude and latitude -- another shape, an SRID other than 4326.
/// Both the extended form (an SRID, Z and M as flags of the type) and
/// ISO's (Z and M as thousands of it) are read; a Z or an M is passed
/// over.
fn ewkb_point(raw: &[u8]) -> Option<Option<(f64, f64)>> {
    let mut b = Vec::with_capacity(raw.len() / 2);
    for [hi, lo] in raw.as_chunks::<2>().0 {
        let hi = (*hi as char).to_digit(16)?;
        let lo = (*lo as char).to_digit(16)?;
        b.push((hi * 16 + lo) as u8);
    }
    let little = *b.first()? == 1;
    let word = |at: usize| -> Option<u32> {
        let w: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(w)
        } else {
            u32::from_be_bytes(w)
        })
    };
    let float = |at: usize| -> Option<f64> {
        let w: [u8; 8] = b.get(at..at + 8)?.try_into().ok()?;
        Some(if little {
            f64::from_le_bytes(w)
        } else {
            f64::from_be_bytes(w)
        })
    };
    let ty = word(1)?;
    if (ty & 0x0FFF_FFFF) % 1000 != 1 {
        return None;
    }
    let mut at = 5;
    if ty & 0x2000_0000 != 0 {
        if !matches!(word(at)?, 0 | 4326) {
            return None;
        }
        at += 4;
    }
    let (x, y) = (float(at)?, float(at + 8)?);
    // An empty point is written as two NaNs.
    Some((!x.is_nan() || !y.is_nan()).then_some((x, y)))
}

/// `\x48656c6c6f` -> bytes. This has been the default format since PostgreSQL 9.0.
fn hex_bytea(raw: &[u8]) -> Option<Vec<u8>> {
    let h = raw.strip_prefix(b"\\x")?;
    if h.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(h.len() / 2);
    for [hi, lo] in h.as_chunks::<2>().0 {
        let hi = (*hi as char).to_digit(16)?;
        let lo = (*lo as char).to_digit(16)?;
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
    let r = client.query(
        "select typname, oid from pg_type where typname in \
         ('vector', 'halfvec', 'sparsevec', 'geometry', 'geography')",
    )?;
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
            Some("sparsevec") => out.sparsevec = Some(oid),
            Some("geometry") => out.geometry = Some(oid),
            Some("geography") => out.geography = Some(oid),
            _ => {}
        }
    }
    Ok(out)
}

/// Quotes an identifier. `schema.table` is handled as two parts.
pub(crate) fn quote_ident(name: &str) -> Result<String> {
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
        assert_eq!(
            ty(INT4_ARRAY),
            Some(DataType::List(Box::new(DataType::Int)))
        );
        assert_eq!(
            ty(TEXT_ARRAY),
            Some(DataType::List(Box::new(DataType::Text)))
        );
        // A json column is a json field, its paths read into as PostgreSQL's
        // `->` reads them.
        assert_eq!(ty(JSONB), Some(DataType::Json));
        assert_eq!(ty(JSON), Some(DataType::Json));
        assert_eq!(
            parse_cell(br#"{"a": [1, 19.99]}"#, &Kind::Json, "meta").unwrap(),
            fenec_core::json::parse_json(r#"{"a":[1,19.99]}"#).unwrap()
        );
        assert!(parse_cell(b"{", &Kind::Json, "meta").is_err());
        // The ones with no counterpart require `--cast`.
        assert_eq!(ty(NUMERIC), None);
        assert_eq!(ty(9999), None);
    }

    /// A PostGIS point column, and PostgreSQL's own `point`, are a `geo`
    /// field; a point's hex EWKB, either byte order, with or without an
    /// SRID or a Z, is `[lon, lat]`; another shape or reference system is
    /// refused.
    #[test]
    fn postgis_points_are_geo_fields() {
        let o = VectorOids {
            geometry: Some(20_000),
            geography: Some(20_100),
            ..VectorOids::default()
        };
        // `geometry(Point, 4326)`: type 1 in bits 2-7, the SRID from bit 8.
        let point_4326 = (4326 << 8) | (1 << 2);
        for (oid, typmod) in [(20_000, point_4326), (20_100, -1), (20_000, 1 << 2)] {
            let (c, k) = map_oid(&f("loc", oid, typmod), &o);
            assert_eq!((c.ty, k), (Some(DataType::Geo), Kind::Ewkb), "{typmod}");
        }
        // Web Mercator is metres; a polygon is no point.
        assert_eq!(
            map_oid(&f("loc", 20_000, (3857 << 8) | (1 << 2)), &o).0.ty,
            None
        );
        assert_eq!(
            map_oid(&f("loc", 20_000, (4326 << 8) | (3 << 2)), &o).0.ty,
            None
        );
        assert_eq!(map_oid(&f("p", POINT, -1), &o).0.ty, Some(DataType::Geo));

        let hex = |bytes: &[u8]| -> Vec<u8> {
            bytes
                .iter()
                .flat_map(|b| format!("{b:02X}").into_bytes())
                .collect()
        };
        let (lon, lat) = (13.404954f64, 52.520008f64);
        let mut srid = vec![1, 1, 0, 0, 0x20, 0xE6, 0x10, 0, 0];
        srid.extend(lon.to_le_bytes());
        srid.extend(lat.to_le_bytes());
        let mut big = vec![0, 0, 0, 0, 1];
        big.extend(lon.to_be_bytes());
        big.extend(lat.to_be_bytes());
        let mut z = vec![1, 0xE9, 0x03, 0, 0];
        z.extend(lon.to_le_bytes());
        z.extend(lat.to_le_bytes());
        z.extend(7.0f64.to_le_bytes());
        let want = fenec_core::codec::geo_value((lon, lat));
        for wkb in [&srid, &big, &z] {
            assert_eq!(parse_cell(&hex(wkb), &Kind::Ewkb, "loc").unwrap(), want);
        }
        let mut mercator = srid.clone();
        mercator[5..9].copy_from_slice(&3857u32.to_le_bytes());
        assert!(parse_cell(&hex(&mercator), &Kind::Ewkb, "loc").is_err());
        let mut line = srid.clone();
        line[1] = 2;
        assert!(parse_cell(&hex(&line), &Kind::Ewkb, "loc").is_err());
        let mut empty = vec![1, 1, 0, 0, 0];
        empty.extend(f64::NAN.to_le_bytes());
        empty.extend(f64::NAN.to_le_bytes());
        assert_eq!(
            parse_cell(&hex(&empty), &Kind::Ewkb, "loc").unwrap(),
            Value::Null
        );
        assert_eq!(
            parse_cell(b"(13.404954,52.520008)", &Kind::Point, "p").unwrap(),
            want
        );
    }

    /// pgvector's OID is not fixed; the dimension comes from typmod.
    #[test]
    fn pgvector_dimension_comes_from_typmod() {
        let o = VectorOids {
            vector: Some(16385),
            halfvec: Some(16390),
            sparsevec: Some(16395),
            ..VectorOids::default()
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
        // A `sparsevec` is read in its text form by the schema.
        let (c, k) = map_oid(&f("splade", 16395, 30522), &o);
        assert_eq!(c.ty, Some(DataType::Sparse(30522)));
        assert_eq!(k, Kind::Text);
        let v = parse_cell(b"{1:0.5,3:0.25}/30522", &k, "splade").unwrap();
        assert_eq!(
            v.coerce(&DataType::Sparse(30522)).unwrap(),
            Value::Sparse(30522, vec![(0, 0.5), (2, 0.25)])
        );
        let (c, _) = map_oid(&f("splade", 16395, -1), &o);
        assert_eq!(c.ty, None);
        assert!(c.note.unwrap().contains("sparse<N>"));
    }

    #[test]
    fn copy_escapes_are_decoded() {
        let cells = split_line(b"a\\tb\tline\\nend\t\\\\back\t\\N\t");
        assert_eq!(cells[0].as_deref(), Some(&b"a\tb"[..]));
        assert_eq!(cells[1].as_deref(), Some(&b"line\nend"[..]));
        assert_eq!(cells[2].as_deref(), Some(&b"\\back"[..]));
        assert_eq!(cells[3], None, "\\N must be NULL");
        assert_eq!(
            cells[4].as_deref(),
            Some(&b""[..]),
            "an empty string is not NULL"
        );
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
        assert_eq!(
            p(b"\\x48690a", Kind::Bytea),
            Value::Bytes(vec![0x48, 0x69, 0x0a])
        );
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
        let e = parse_cell(b"abc", &Kind::Int, "score")
            .unwrap_err()
            .to_string();
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
