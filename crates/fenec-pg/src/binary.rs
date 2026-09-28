//! A result's values in PostgreSQL's binary format, as its `typsend`
//! functions write them: what a driver asks Bind for -- tokio-postgres and
//! asyncpg for every column, pgx for every type it knows. Sent as text, a
//! `bigint` was one byte where they read eight: pgx refused the row,
//! tokio-postgres could not read the column, and asyncpg read past it into
//! the next message and misread every answer after it.

use crate::proto::*;
use fenec_core::prelude::Value;

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
const EPOCH_2000_MS: i64 = 946_684_800_000;

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

fn refused(oid: i32) -> String {
    format!("the binary format of type {oid} is not supported: ask for this column in text")
}

/// `v` as a column of type `oid` sends it in the binary format, `None` for
/// NULL. A type whose text is its binary form -- `text`, and a vector, a
/// list or a sparse vector, which travel as text -- is sent as its text.
pub fn value(oid: i32, v: &Value) -> Result<Option<Vec<u8>>, String> {
    Ok(Some(match (oid, v) {
        (_, Value::Null) => return Ok(None),
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
