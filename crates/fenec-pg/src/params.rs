//! The types of a statement's parameters, as `Describe` reports them and
//! Bind reads the values in.
//!
//! Reported unspecified (OID 0), a parameter sent tokio-postgres to look
//! the type up with a catalog query whose own `$1` was unspecified in turn,
//! until its stack ran out, and asyncpg into an introspection query of its
//! own. A driver told the type sends the value as that type -- tokio-postgres
//! and asyncpg in binary -- and Bind reads it by the same type.

use crate::binary::{
    EPOCH_2000_MS, OID_BPCHAR, OID_FLOAT4, OID_INT2, OID_INT4, OID_NAME, OID_OID, OID_VARCHAR,
};
use crate::proto::*;
use crate::server::{decode_param, pg_oid};
use fenec_core::prelude::{Database, Expr, Statement, Value};
use fenec_core::schema::Schema;
use fenec_core::value::DataType;

/// `timestamp`, which a client may declare for a parameter itself.
const OID_TIMESTAMP: i32 = 1114;
/// `unknown`: a literal's type before it is resolved.
const OID_UNKNOWN: i32 = 705;

/// The type each `$n` of `stmts` takes where its place names one: compared
/// with a field or given as one -- the field's type, and `id`'s -- the
/// vector of a `near` or a `rerank`, the text of a `match`. Text
/// everywhere else, which every driver sends and a pg text parameter is
/// read as.
pub fn types(db: &Database, stmts: &[Statement]) -> Vec<i32> {
    let n = stmts.iter().map(|s| s.max_param()).max().unwrap_or(0);
    let mut out = vec![None; n];
    for s in stmts {
        statement(db, s, &mut out);
    }
    out.into_iter().map(|t| t.unwrap_or(OID_TEXT)).collect()
}

/// The type `e` takes, when it is a parameter whose type is not taken yet.
fn set(out: &mut [Option<i32>], e: &Expr, oid: i32) {
    if let Expr::Param(i) = e {
        if let Some(slot) = out.get_mut(*i) {
            slot.get_or_insert(oid);
        }
    }
}

/// A field's type as a parameter given for it goes.
fn field(schema: Option<&Schema>, name: &str) -> Option<i32> {
    if name == "id" {
        return Some(OID_INT8);
    }
    schema?.field(name).map(|f| pg_oid(&f.ty))
}

fn filter(schema: Option<&Schema>, e: &Expr, out: &mut [Option<i32>]) {
    match e {
        Expr::Cmp(_, a, b) => {
            for (f, p) in [(a, b), (b, a)] {
                if let Expr::Field(name) = f.as_ref() {
                    if let Some(oid) = field(schema, name) {
                        set(out, p, oid);
                    }
                }
            }
            filter(schema, a, out);
            filter(schema, b, out);
        }
        Expr::Like(a, b) => {
            set(out, b, OID_TEXT);
            filter(schema, a, out);
            filter(schema, b, out);
        }
        // `tags has $1`: an element of the list.
        Expr::Has(a, b) => {
            if let Expr::Field(name) = a.as_ref() {
                if let Some(DataType::List(inner)) =
                    schema.and_then(|s| s.field(name)).map(|f| &f.ty)
                {
                    set(out, b, pg_oid(inner));
                }
            }
            filter(schema, a, out);
            filter(schema, b, out);
        }
        Expr::In(a, items) => {
            if let Expr::Field(name) = a.as_ref() {
                if let Some(oid) = field(schema, name) {
                    items.iter().for_each(|i| set(out, i, oid));
                }
            }
            filter(schema, a, out);
            items.iter().for_each(|i| filter(schema, i, out));
        }
        Expr::And(a, b) | Expr::Or(a, b) => {
            filter(schema, a, out);
            filter(schema, b, out);
        }
        Expr::Not(a) | Expr::IsNull(a) => filter(schema, a, out),
        Expr::Call(_, args) => args.iter().for_each(|a| filter(schema, a, out)),
        Expr::Param(_) | Expr::Field(_) | Expr::Lit(_) => {}
    }
}

fn statement(db: &Database, s: &Statement, out: &mut [Option<i32>]) {
    let schema = |c: &str| db.collection(c).ok().map(|c| &c.schema);
    let pairs = |sc: Option<&Schema>, pairs: &[(String, Expr)], out: &mut [Option<i32>]| {
        for (name, e) in pairs {
            if let Some(oid) = field(sc, name) {
                set(out, e, oid);
            }
        }
    };
    match s {
        Statement::Put { collection, docs } => {
            let sc = schema(collection);
            docs.iter().for_each(|d| pairs(sc, d, out));
        }
        Statement::Update {
            collection,
            set: to,
            filter: f,
        } => {
            let sc = schema(collection);
            pairs(sc, to, out);
            if let Some(f) = f {
                filter(sc, f, out);
            }
        }
        Statement::Delete {
            collection,
            filter: Some(f),
        } => filter(schema(collection), f, out),
        Statement::Select(sel) | Statement::Explain(sel) => {
            let sc = schema(&sel.collection);
            if let Some(f) = &sel.filter {
                filter(sc, f, out);
            }
            // A vector and a sparse vector travel as their text.
            if let Some(n) = &sel.near {
                set(out, &n.vector, OID_TEXT);
            }
            if let Some(m) = &sel.matcher {
                set(out, &m.query, OID_TEXT);
            }
            if let Some(r) = &sel.rerank {
                set(out, &r.vector, OID_TEXT);
            }
            if let Some(l) = &sel.lookup {
                for step in l.chain() {
                    if let Some(f) = &step.filter {
                        filter(schema(&step.collection), f, out);
                    }
                }
            }
        }
        _ => {}
    }
}

/// A parameter's bytes as a value: in text as a pg text parameter has
/// always been read -- a number, a vector's `[..]`, a boolean or text --
/// and in binary as its type `oid` sends it.
pub fn decode(raw: &[u8], binary: bool, oid: i32) -> Value {
    if !binary {
        return decode_param(raw, false);
    }
    let bytes = |n: usize| -> Option<[u8; 8]> {
        let mut b = [0u8; 8];
        (raw.len() == n).then(|| {
            b[8 - n..].copy_from_slice(raw);
            b
        })
    };
    let v =
        match oid {
            OID_INT8 => bytes(8).map(|b| Value::Int(i64::from_be_bytes(b))),
            OID_INT4 => (raw.len() == 4)
                .then(|| Value::Int(i32::from_be_bytes(raw.try_into().unwrap()) as i64)),
            OID_INT2 => (raw.len() == 2)
                .then(|| Value::Int(i16::from_be_bytes(raw.try_into().unwrap()) as i64)),
            OID_OID => bytes(4).map(|b| Value::Int(u64::from_be_bytes(b) as i64)),
            OID_FLOAT8 => bytes(8).map(|b| Value::Float(f64::from_be_bytes(b))),
            OID_FLOAT4 => (raw.len() == 4)
                .then(|| Value::Float(f32::from_be_bytes(raw.try_into().unwrap()) as f64)),
            OID_BOOL => (raw.len() == 1).then(|| Value::Bool(raw[0] != 0)),
            OID_TIMESTAMPTZ | OID_TIMESTAMP => bytes(8).map(|b| {
                let micros = i64::from_be_bytes(b);
                Value::Timestamp(micros.div_euclid(1000).saturating_add(EPOCH_2000_MS))
            }),
            OID_BYTEA => Some(Value::Bytes(raw.to_vec())),
            // A text type's binary form is its text.
            OID_TEXT | OID_VARCHAR | OID_NAME | OID_BPCHAR | OID_UNKNOWN => {
                Some(decode_param(raw, false))
            }
            _ => None,
        };
    // A type the client did not send as it was told: read as a binary
    // parameter always was, by its length.
    v.unwrap_or_else(|| decode_param(raw, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stmts(q: &str) -> Vec<Statement> {
        fenec_ql::parse(q).unwrap()
    }

    #[test]
    fn a_parameter_takes_the_type_its_place_names() {
        let mut db = Database::new();
        for q in [
            "create collection t (name text, n int, score float, ok bool, at timestamp, raw bytes, tags [text], e vector<2> @hnsw(cosine))",
            "create collection kids (parent int, age int)",
        ] {
            db.execute(&stmts(q)[0]).unwrap();
        }
        let of = |q: &str| types(&db, &stmts(q));
        assert_eq!(
            of("put t {name: $1, n: $2, score: $3, ok: $4, at: $5, raw: $6, e: $7, id: $8}"),
            [
                OID_TEXT,
                OID_INT8,
                OID_FLOAT8,
                OID_BOOL,
                OID_TIMESTAMPTZ,
                OID_BYTEA,
                OID_TEXT,
                OID_INT8
            ]
        );
        assert_eq!(
            of("get t where n > $1 and $2 = score or name ~ $3 or tags has $4 or id in [$5, $6]"),
            [OID_INT8, OID_FLOAT8, OID_TEXT, OID_TEXT, OID_INT8, OID_INT8]
        );
        assert_eq!(of("get t near e $1 limit 5"), [OID_TEXT]);
        assert_eq!(
            of("set t {ok: $1} where at < $2"),
            [OID_BOOL, OID_TIMESTAMPTZ]
        );
        assert_eq!(of("del t where n = $1"), [OID_INT8]);
        assert_eq!(
            of("get t where n = $1 lookup kids on parent = id where age > $2"),
            [OID_INT8, OID_INT8]
        );
        // Where nothing names a type, and a collection not made yet.
        assert_eq!(of("get t where $1 = $2"), [OID_TEXT, OID_TEXT]);
        assert_eq!(of("put nope {n: $1}"), [OID_TEXT]);
    }

    #[test]
    fn a_binary_parameter_is_read_as_its_type_sends_it() {
        assert_eq!(decode(&7i64.to_be_bytes(), true, OID_INT8), Value::Int(7));
        assert_eq!(
            decode(&(-3i32).to_be_bytes(), true, OID_INT4),
            Value::Int(-3)
        );
        assert_eq!(
            decode(&0.5f64.to_be_bytes(), true, OID_FLOAT8),
            Value::Float(0.5)
        );
        assert_eq!(
            decode(&0.5f32.to_be_bytes(), true, OID_FLOAT4),
            Value::Float(0.5)
        );
        assert_eq!(decode(&[1], true, OID_BOOL), Value::Bool(true));
        assert_eq!(
            decode(&1_000_000i64.to_be_bytes(), true, OID_TIMESTAMPTZ),
            Value::Timestamp(946_684_801_000)
        );
        // Before 2000, the microseconds round down.
        assert_eq!(
            decode(&(-1i64).to_be_bytes(), true, OID_TIMESTAMPTZ),
            Value::Timestamp(946_684_799_999)
        );
        assert_eq!(
            decode(&[0, 255], true, OID_BYTEA),
            Value::Bytes(vec![0, 255])
        );
        assert_eq!(
            decode(b"[1,2]", true, OID_TEXT),
            Value::Vector(vec![1.0, 2.0])
        );
        assert_eq!(decode(b"hi", true, OID_TEXT), Value::Text("hi".into()));
        // In text, as ever; and a length the type does not have, as before.
        assert_eq!(decode(b"7", false, OID_INT8), Value::Int(7));
        assert_eq!(decode(&[0, 0, 0, 9], true, OID_INT8), Value::Int(9));
    }
}
