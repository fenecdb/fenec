//! A result's values in PostgreSQL's binary format, as its `typsend`
//! functions write them: what a driver asks Bind for -- tokio-postgres and
//! asyncpg for every column, pgx for every type it knows. Sent as text, a
//! `bigint` was one byte where they read eight: pgx refused the row,
//! tokio-postgres could not read the column, and asyncpg read past it into
//! the next message and misread every answer after it.

use crate::proto::*;
use fenec_core::codec::{f16_from_f32, f32_from_f16};
use fenec_core::prelude::Value;

pub use crate::catalog::{
    HALFVEC as OID_HALFVEC, SPARSEVEC as OID_SPARSEVEC, VECTOR as OID_VECTOR,
};

pub const OID_CHAR: i32 = 18;
pub const OID_NAME: i32 = 19;
pub const OID_INT2: i32 = 21;
pub const OID_INT4: i32 = 23;
pub const OID_REGPROC: i32 = 24;
pub const OID_OID: i32 = 26;
pub const OID_FLOAT4: i32 = 700;
pub const OID_BPCHAR: i32 = 1042;
pub const OID_VARCHAR: i32 = 1043;

/// PostgreSQL's timestamps count microseconds from 2000-01-01 UTC.
pub(crate) const EPOCH_2000_MS: i64 = 946_684_800_000;

/// Whether the `i`th column is asked for in the binary format: no code is
/// every column in text, one is every column in it, and more are one a
/// column.
pub fn binary_at(formats: &[i16], i: usize) -> bool {
    match formats {
        [] => false,
        [one] => *one == 1,
        many => many.get(i) == Some(&1),
    }
}

/// Whether any column is asked for in the binary format.
pub fn any_binary(formats: &[i16]) -> bool {
    formats.contains(&1)
}

/// An array type's element type.
pub fn element_of(oid: i32) -> Option<i32> {
    Some(match oid {
        1000 => OID_BOOL,
        1001 => OID_BYTEA,
        1003 => OID_NAME,
        1005 => OID_INT2,
        1007 => OID_INT4,
        1009 => OID_TEXT,
        1015 => OID_VARCHAR,
        1016 => OID_INT8,
        1021 => OID_FLOAT4,
        1022 => OID_FLOAT8,
        1028 => OID_OID,
        1185 => OID_TIMESTAMPTZ,
        _ => return None,
    })
}

/// The array type a list of `elem`s is sent as, one of the element types a
/// fenecdb list holds.
pub fn array_of(elem: i32) -> i32 {
    match elem {
        OID_BOOL => 1000,
        OID_BYTEA => 1001,
        OID_INT8 => 1016,
        OID_FLOAT8 => 1022,
        OID_TIMESTAMPTZ => 1185,
        _ => 1009,
    }
}

fn refused(oid: i32) -> String {
    format!("the binary format of type {oid} is not supported: ask for this column in text")
}

/// `v` as a column of type `oid` sends it in the binary format, `None` for
/// NULL. A type whose text is its binary form -- `text`, and a list, which
/// travels as text -- is sent as its text; a vector as pgvector sends one.
pub fn value(oid: i32, v: &Value) -> Result<Option<Vec<u8>>, String> {
    Ok(Some(match (oid, v) {
        (_, Value::Null) => return Ok(None),
        (oid, Value::List(items)) if element_of(oid).is_some() => {
            array(element_of(oid).unwrap_or(OID_TEXT), items)?
        }
        (OID_VECTOR, Value::Vector(x)) => dense(x, false)?,
        (OID_HALFVEC, Value::Vector(x)) => dense(x, true)?,
        (OID_SPARSEVEC, Value::Sparse(dim, entries)) => sparse(*dim, entries),
        (OID_INT8, Value::Int(i)) => i.to_be_bytes().to_vec(),
        (OID_FLOAT8, Value::Float(f)) => f.to_be_bytes().to_vec(),
        (OID_FLOAT8, Value::Int(i)) => (*i as f64).to_be_bytes().to_vec(),
        (OID_BOOL, Value::Bool(b)) => vec![*b as u8],
        (OID_BYTEA, Value::Bytes(b)) => b.clone(),
        (OID_TIMESTAMPTZ, Value::Timestamp(ms)) => {
            (ms.saturating_sub(EPOCH_2000_MS).saturating_mul(1000))
                .to_be_bytes()
                .to_vec()
        }
        (OID_TEXT | OID_VARCHAR | OID_NAME | OID_BPCHAR, v) => match crate::server::to_pg_text(v) {
            Some(s) => s.into_bytes(),
            None => return Ok(None),
        },
        // A value of another type than its column's -- a field holding a
        // value its schema does not name -- goes as the column's type reads
        // its text.
        (oid, v) => match crate::server::to_pg_text(v) {
            Some(s) => return text(oid, &s).map(Some),
            None => return Ok(None),
        },
    }))
}

/// `array_send`: one dimension, whether a NULL is among the elements, their
/// type, the length and the lower bound 1, then each element's length and
/// bytes as its type sends it; an empty list no dimension at all, as
/// PostgreSQL sends `'{}'`.
fn array(elem: i32, items: &[Value]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(20 + items.len() * 12);
    let dims = !items.is_empty() as i32;
    let nulls = items.iter().any(|v| matches!(v, Value::Null)) as i32;
    for w in [dims, nulls, elem] {
        out.extend_from_slice(&w.to_be_bytes());
    }
    if dims == 1 {
        out.extend_from_slice(&(items.len() as i32).to_be_bytes());
        out.extend_from_slice(&1i32.to_be_bytes());
    }
    for v in items {
        match value(elem, v)? {
            Some(bytes) => {
                out.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                out.extend_from_slice(&bytes);
            }
            None => out.extend_from_slice(&(-1i32).to_be_bytes()),
        }
    }
    Ok(out)
}

/// A cell answered as text -- the catalog's, a shim's -- in the binary
/// format of its column's type `oid`.
pub fn text(oid: i32, s: &str) -> Result<Vec<u8>, String> {
    let bad = || format!("`{s}` is not a value of type {oid}");
    Ok(match oid {
        OID_TEXT | OID_VARCHAR | OID_NAME | OID_BPCHAR => s.as_bytes().to_vec(),
        OID_BOOL => match s {
            "t" | "true" => vec![1],
            "f" | "false" => vec![0],
            _ => return Err(bad()),
        },
        OID_CHAR => s.as_bytes().first().map_or(vec![0], |c| vec![*c]),
        OID_INT2 => s.parse::<i16>().map_err(|_| bad())?.to_be_bytes().to_vec(),
        OID_INT4 => s.parse::<i32>().map_err(|_| bad())?.to_be_bytes().to_vec(),
        OID_INT8 => s.parse::<i64>().map_err(|_| bad())?.to_be_bytes().to_vec(),
        // An OID is unsigned; `regproc` is sent as the OID it names, which
        // the catalog writes as its number.
        OID_OID | OID_REGPROC => s.parse::<u32>().map_err(|_| bad())?.to_be_bytes().to_vec(),
        OID_FLOAT4 => fenec_core::num::parse_f64(s)
            .map(|f| (f as f32).to_be_bytes().to_vec())
            .ok_or_else(bad)?,
        OID_FLOAT8 => fenec_core::num::parse_f64(s)
            .map(|f| f.to_be_bytes().to_vec())
            .ok_or_else(bad)?,
        OID_TIMESTAMPTZ => fenec_core::time::parse(s)
            .map(|ms| {
                (ms.saturating_sub(EPOCH_2000_MS).saturating_mul(1000))
                    .to_be_bytes()
                    .to_vec()
            })
            .map_err(|_| bad())?,
        OID_BYTEA => match s.strip_prefix("\\x") {
            Some(hex) if hex.len() % 2 == 0 => (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(hex.get(i..i + 2).unwrap_or("zz"), 16))
                .collect::<Result<_, _>>()
                .map_err(|_| bad())?,
            _ => s.as_bytes().to_vec(),
        },
        other => return Err(refused(other)),
    })
}

/// pgvector's `vector_send`: the dimension and a word it leaves 0, 16 bits
/// each, then each component's `f32` -- `halfvec_send` each one's binary16,
/// which a `vector<N, f16>` gives back exactly, since it holds nothing
/// else.
fn dense(x: &[f32], half: bool) -> Result<Vec<u8>, String> {
    let dim = u16::try_from(x.len()).map_err(|_| {
        format!(
            "a vector of {} dimensions has no binary format: ask for this column in text",
            x.len()
        )
    })?;
    let mut out = Vec::with_capacity(4 + x.len() * if half { 2 } else { 4 });
    out.extend_from_slice(&dim.to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    for f in x {
        match half {
            true => out.extend_from_slice(&f16_from_f32(*f).to_be_bytes()),
            false => out.extend_from_slice(&f.to_be_bytes()),
        }
    }
    Ok(out)
}

/// pgvector's `sparsevec_send`: the dimension, the count of entries and a
/// word it leaves 0, 32 bits each, then every index -- from 0, as fenecdb
/// counts them too -- and every weight.
fn sparse(dim: u32, entries: &[(u32, f32)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + entries.len() * 8);
    for w in [dim, entries.len() as u32, 0] {
        out.extend_from_slice(&w.to_be_bytes());
    }
    for (i, _) in entries {
        out.extend_from_slice(&i.to_be_bytes());
    }
    for (_, w) in entries {
        out.extend_from_slice(&w.to_be_bytes());
    }
    out
}

/// A vector of type `oid` -- `vector`, `halfvec` or `sparsevec` -- as
/// pgvector's receive functions read it: `None` where `raw` is not in that
/// format. The text a vector went as before it had one never is: text
/// holds no zero byte, and the format's word after the dimension (after
/// the count of entries, for a sparse vector) is 0. A component that is not
/// finite is refused, as pgvector refuses it; a sparse vector is checked
/// and put in order where every sparse vector is (`sparse::normalise`).
pub fn vector(raw: &[u8], oid: i32) -> Option<Result<Value, (&'static str, String)>> {
    match oid {
        OID_VECTOR | OID_HALFVEC => {
            let half = oid == OID_HALFVEC;
            let dim = u16::from_be_bytes([*raw.first()?, *raw.get(1)?]) as usize;
            let width = if half { 2 } else { 4 };
            if raw.get(2..4)? != [0, 0] || raw.len() != 4 + dim * width {
                return None;
            }
            let x: Vec<f32> = match half {
                true => raw[4..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| f32_from_f16(u16::from_be_bytes(*c)))
                    .collect(),
                false => raw[4..]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| f32::from_be_bytes(*c))
                    .collect(),
            };
            if let Some(f) = x.iter().find(|f| !f.is_finite()) {
                let what = if f.is_nan() { "NaN" } else { "infinite value" };
                let ty = if half { "halfvec" } else { "vector" };
                return Some(Err(("22000", format!("{what} not allowed in {ty}"))));
            }
            Some(Ok(Value::Vector(x)))
        }
        OID_SPARSEVEC => {
            let word = |at: usize| {
                raw.get(at..at + 4)?
                    .first_chunk()
                    .map(|w| u32::from_be_bytes(*w))
            };
            let (dim, nnz) = (word(0)?, word(4)? as usize);
            if word(8)? != 0 || Some(raw.len()) != nnz.checked_mul(8)?.checked_add(12) {
                return None;
            }
            let (indices, weights) = raw[12..].split_at(nnz * 4);
            let entries = indices
                .as_chunks::<4>()
                .0
                .iter()
                .zip(weights.as_chunks::<4>().0)
                .map(|(i, w)| (u32::from_be_bytes(*i), f32::from_be_bytes(*w)))
                .collect();
            Some(Ok(Value::Sparse(dim, entries)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_go_as_their_types_send_them() {
        let v = |oid, v: Value| value(oid, &v).unwrap().unwrap();
        assert_eq!(v(OID_INT8, Value::Int(-2)), (-2i64).to_be_bytes());
        assert_eq!(v(OID_FLOAT8, Value::Float(0.5)), 0.5f64.to_be_bytes());
        assert_eq!(v(OID_FLOAT8, Value::Int(3)), 3f64.to_be_bytes());
        assert_eq!(v(OID_BOOL, Value::Bool(true)), [1]);
        assert_eq!(v(OID_BYTEA, Value::Bytes(vec![0, 255])), [0, 255]);
        // 2000-01-01T00:00:01Z is a million microseconds in.
        assert_eq!(
            v(OID_TIMESTAMPTZ, Value::Timestamp(946_684_801_000)),
            1_000_000i64.to_be_bytes()
        );
        assert_eq!(v(OID_TEXT, Value::Vector(vec![1.0, 0.5])), b"[1,0.5]");
        // pgvector's `vector_send`, `halfvec_send` and `sparsevec_send`,
        // which its receive functions -- and `vector` -- read back.
        let x = Value::Vector(vec![1.5, -2.0]);
        let sent = v(OID_VECTOR, x.clone());
        assert_eq!(
            sent,
            [
                &[0, 2, 0, 0][..],
                &1.5f32.to_be_bytes(),
                &(-2f32).to_be_bytes()
            ]
            .concat()
        );
        assert_eq!(vector(&sent, OID_VECTOR), Some(Ok(x.clone())));
        let half = v(OID_HALFVEC, x.clone());
        assert_eq!(half, [0, 2, 0, 0, 0x3e, 0, 0xc0, 0]);
        assert_eq!(vector(&half, OID_HALFVEC), Some(Ok(x)));
        let sp = Value::Sparse(5, vec![(1, 0.5), (3, 0.25)]);
        let sent = v(OID_SPARSEVEC, sp.clone());
        assert_eq!(sent[..12], [0, 0, 0, 5, 0, 0, 0, 2, 0, 0, 0, 0]);
        assert_eq!(vector(&sent, OID_SPARSEVEC), Some(Ok(sp)));
        // The format counts dimensions in 16 bits.
        assert!(value(OID_VECTOR, &Value::Vector(vec![0.0; 70_000])).is_err());
        assert_eq!(value(OID_INT8, &Value::Null).unwrap(), None);
        // A field holding what its schema does not name goes as the column
        // reads it, or is refused.
        assert_eq!(v(OID_INT8, Value::Text("7".into())), 7i64.to_be_bytes());
        assert!(value(OID_INT8, &Value::Text("seven".into())).is_err());
    }

    #[test]
    fn catalog_text_goes_as_its_column_type_sends_it() {
        assert_eq!(text(OID_INT2, "5").unwrap(), 5i16.to_be_bytes());
        assert_eq!(text(OID_INT4, "-1").unwrap(), (-1i32).to_be_bytes());
        assert_eq!(text(OID_OID, "4294967295").unwrap(), u32::MAX.to_be_bytes());
        assert_eq!(text(OID_NAME, "docs").unwrap(), b"docs");
        assert_eq!(text(OID_CHAR, "r").unwrap(), b"r");
        assert_eq!(text(OID_BOOL, "f").unwrap(), [0]);
        assert_eq!(text(OID_FLOAT4, "0.5").unwrap(), 0.5f32.to_be_bytes());
        assert_eq!(text(OID_BYTEA, "\\x00ff").unwrap(), [0, 255]);
        assert_eq!(
            text(OID_TIMESTAMPTZ, "2000-01-01 00:00:02+00").unwrap(),
            2_000_000i64.to_be_bytes()
        );
        assert!(text(1009, "{a,b}").is_err());
    }

    #[test]
    fn formats_name_the_columns_they_ask_for() {
        assert!(!binary_at(&[], 3));
        assert!(binary_at(&[1], 3));
        assert!(!binary_at(&[0], 0));
        assert!(binary_at(&[0, 1], 1) && !binary_at(&[0, 1], 0) && !binary_at(&[0, 1], 2));
    }
}
