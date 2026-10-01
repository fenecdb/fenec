//! The types of a statement's parameters, as `Describe` reports them and
//! Bind reads the values in.
//!
//! Reported unspecified (OID 0), a parameter sent tokio-postgres to look
//! the type up with a catalog query whose own `$1` was unspecified in turn,
//! until its stack ran out, and asyncpg into an introspection query of its
//! own. A driver told the type sends the value as that type -- tokio-postgres
//! and asyncpg in binary -- and Bind reads it by the same type.

use crate::binary::{
    self, EPOCH_2000_MS, OID_BPCHAR, OID_FLOAT4, OID_HALFVEC, OID_INT2, OID_INT4, OID_NAME,
    OID_OID, OID_SPARSEVEC, OID_VARCHAR, OID_VECTOR,
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
/// vector of a `near` or a `rerank`, its field's, the text of a `match`.
/// Text everywhere else, which every driver sends and a pg text parameter
/// is read as.
pub fn types(db: &Database, stmts: &[Statement]) -> Vec<i32> {
    places(db, stmts)
        .iter()
        .map(|t| t.as_ref().map_or(OID_TEXT, pg_oid))
        .collect()
}

/// The field type each `$n` of `stmts` stands in the place of, `None` where
/// its place names none: what [`types`] describes, and what a value sent as
/// text is read as ([`decode`]).
pub fn places(db: &Database, stmts: &[Statement]) -> Vec<Option<DataType>> {
    let n = stmts.iter().map(|s| s.max_param()).max().unwrap_or(0);
    let mut out = vec![None; n];
    for s in stmts {
        statement(db, s, &mut out);
    }
    out
}

/// The type `e` takes, when it is a parameter whose type is not taken yet.
fn set(out: &mut [Option<DataType>], e: &Expr, ty: DataType) {
    if let Expr::Param(i) = e {
        if let Some(slot) = out.get_mut(*i) {
            slot.get_or_insert(ty);
        }
    }
}

/// A field's type, `id`'s among them.
fn field(schema: Option<&Schema>, name: &str) -> Option<DataType> {
    if name == "id" {
        return Some(DataType::Int);
    }
    // A path's value is jsonb, as PostgreSQL types `meta->'lang'`: pgx
    // refused to send a number in a place described as text. A string
    // sent as text with no type named is still read by its look.
    let schema = schema?;
    if let Ok(Some(_)) = schema.path_of(name) {
        return Some(DataType::Json);
    }
    schema.field(name).map(|f| f.ty.clone())
}

fn filter(schema: Option<&Schema>, e: &Expr, out: &mut [Option<DataType>]) {
    match e {
        Expr::Cmp(_, a, b) => {
            for (f, p) in [(a, b), (b, a)] {
                if let Expr::Field(name) = f.as_ref() {
                    if let Some(ty) = field(schema, name) {
                        set(out, p, ty);
                    }
                }
            }
            filter(schema, a, out);
            filter(schema, b, out);
        }
        Expr::Like(a, b) => {
            set(out, b, DataType::Text);
            filter(schema, a, out);
            filter(schema, b, out);
        }
        // `tags has $1`: an element of the list.
        Expr::Has(a, b) => {
            if let Expr::Field(name) = a.as_ref() {
                if let Some(DataType::List(inner)) = field(schema, name) {
                    set(out, b, *inner);
                }
            }
            filter(schema, a, out);
            filter(schema, b, out);
        }
        Expr::In(a, items) => {
            if let Expr::Field(name) = a.as_ref() {
                if let Some(ty) = field(schema, name) {
                    items.iter().for_each(|i| set(out, i, ty.clone()));
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

fn statement(db: &Database, s: &Statement, out: &mut [Option<DataType>]) {
    let schema = |c: &str| db.collection(c).ok().map(|c| &c.schema);
    let pairs = |sc: Option<&Schema>, pairs: &[(String, Expr)], out: &mut [Option<DataType>]| {
        for (name, e) in pairs {
            if let Some(ty) = field(sc, name) {
                set(out, e, ty);
            }
        }
    };
    match s {
        Statement::Put {
            collection, docs, ..
        } => {
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
            // A query vector is of its field's type, as PostgreSQL types
            // the `$1` of `embedding <-> $1`: pgvector's clients send it in
            // that type's binary format.
            if let Some(n) = &sel.near {
                if let Some(ty) = field(sc, &n.field) {
                    set(out, &n.vector, ty);
                }
            }
            if let Some(m) = &sel.matcher {
                set(out, &m.query, DataType::Text);
            }
            if let Some(r) = &sel.rerank {
                if let Some(ty) = field(sc, &r.field) {
                    set(out, &r.vector, ty);
                }
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

/// A parameter's bytes as a value: in binary as its type `oid` sends it,
/// and in text as a value of the field its place names (`place`, from
/// [`places`]), read as COPY reads a cell of that field -- a text field's
/// as the text it is. Where no place names one, or the text is no value of
/// it, it is read as a pg text parameter always was, by its look: a
/// number, a vector's `[..]`, a boolean or text. Read that way alone, `"t"`
/// was a boolean and `"42"` a number, which a text field refused, from
/// psycopg and node-postgres, which name no type for a string, and from
/// pgx and JDBC, which name one the server never read by. A timestamp is
/// left to its look, which reads epoch milliseconds as the number they
/// are. Only a vector is refused, as pgvector refuses one: the rest are
/// read by their length where they are not as their type sends them, as a
/// binary parameter always was.
pub fn decode(
    raw: &[u8],
    binary: bool,
    oid: i32,
    place: Option<&DataType>,
) -> Result<Value, (&'static str, String)> {
    if !binary {
        let typed = place
            .filter(|t| **t != DataType::Timestamp)
            .zip(std::str::from_utf8(raw).ok())
            .and_then(|(t, s)| crate::copy::value(s, t).ok());
        return Ok(typed.unwrap_or_else(|| decode_param(raw, false)));
    }
    if let Some(elem) = binary::element_of(oid) {
        return Ok(array(raw, elem).unwrap_or_else(|| decode_param(raw, true)));
    }
    if let Some(v) = binary::json(raw, oid) {
        return v;
    }
    if matches!(oid, OID_VECTOR | OID_HALFVEC | OID_SPARSEVEC) {
        return match binary::vector(raw, oid) {
            Some(v) => v,
            // The text a vector went as before it had a binary format.
            None if !raw.contains(&0) => Ok(decode_param(raw, false)),
            None => Err((
                "22P03",
                format!("{} bytes are not a vector in its binary format", raw.len()),
            )),
        };
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
    Ok(v.unwrap_or_else(|| decode_param(raw, true)))
}

/// A one-dimensional array in the binary format -- its dimensions, a flag
/// for NULLs, its element type, its length and lower bound, then each
/// element's length and bytes -- as the list of its elements, each read as
/// its type sends it: asyncpg's `$1::oid[]`.
pub(crate) fn array(raw: &[u8], elem: i32) -> Option<Value> {
    let i32_at = |at: usize| {
        raw.get(at..at + 4)
            .map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    match i32_at(0)? {
        0 => return Some(Value::List(Vec::new())),
        1 => {}
        _ => return None,
    }
    let n = i32_at(12)?;
    let mut at = 20;
    let mut out = Vec::with_capacity(n.max(0) as usize);
    for _ in 0..n {
        let len = i32_at(at)?;
        at += 4;
        if len < 0 {
            out.push(Value::Null);
            continue;
        }
        let cell = raw.get(at..at + len as usize)?;
        at += len as usize;
        // An element of a text array is text: read by its look, as a
        // parameter no type names is, `"42"` was a number.
        out.push(match elem {
            OID_TEXT | OID_VARCHAR | OID_NAME | OID_BPCHAR => {
                Value::Text(std::str::from_utf8(cell).ok()?.to_string())
            }
            _ => decode(cell, true, elem, None).ok()?,
        });
    }
    (at == raw.len()).then_some(Value::List(out))
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
            "create collection t (name text, n int, score float, ok bool, at timestamp, raw bytes, tags [text], e vector<2> @hnsw(cosine), h vector<2, f16>, s sparse<5> @inverted)",
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
                OID_VECTOR,
                OID_INT8
            ]
        );
        assert_eq!(
            of("put t {h: $1, s: $2, tags: $3}"),
            // A list its array, which a driver binds a list of its own to.
            [OID_HALFVEC, OID_SPARSEVEC, 1009]
        );
        assert_eq!(
            of("get t where n > $1 and $2 = score or name ~ $3 or tags has $4 or id in [$5, $6]"),
            [OID_INT8, OID_FLOAT8, OID_TEXT, OID_TEXT, OID_INT8, OID_INT8]
        );
        // A query vector is its field's: pgvector types `embedding <-> $1`
        // so, and its clients send the vector in that type's format.
        assert_eq!(of("get t near e $1 limit 5"), [OID_VECTOR]);
        assert_eq!(of("get t near s $1 limit 5"), [OID_SPARSEVEC]);
        assert_eq!(
            of("get t match name $1 rerank h $2 limit 5"),
            [OID_TEXT, OID_HALFVEC]
        );
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
        let decode = |raw: &[u8], binary, oid| decode(raw, binary, oid, None).unwrap();
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
        // An array: asyncpg's `$1::oid[]`.
        let mut oids = Vec::new();
        for w in [1i32, 0, OID_OID, 2, 1, 4, 16_400, 4, 16_402] {
            oids.extend_from_slice(&w.to_be_bytes());
        }
        assert_eq!(
            decode(&oids, true, 1028),
            Value::List(vec![Value::Int(16_400), Value::Int(16_402)])
        );
        // In text, as ever; and a length the type does not have, as before.
        assert_eq!(decode(b"7", false, OID_INT8), Value::Int(7));
        assert_eq!(decode(&[0, 0, 0, 9], true, OID_INT8), Value::Int(9));
    }

    #[test]
    fn a_vector_parameter_is_read_as_pgvector_sends_it() {
        let decode = |raw: &[u8], binary, oid| decode(raw, binary, oid, None);
        let words = |w: &[&[u8]]| w.concat();
        // `vector_send`: the dimension, 0, then each f32.
        let v = words(&[
            &2u16.to_be_bytes(),
            &[0, 0],
            &1.5f32.to_be_bytes(),
            &(-2f32).to_be_bytes(),
        ]);
        assert_eq!(
            decode(&v, true, OID_VECTOR),
            Ok(Value::Vector(vec![1.5, -2.0]))
        );
        // `halfvec_send`: each component's binary16.
        let h = words(&[
            &2u16.to_be_bytes(),
            &[0, 0],
            &0x3e00u16.to_be_bytes(),
            &0xc000u16.to_be_bytes(),
        ]);
        assert_eq!(
            decode(&h, true, OID_HALFVEC),
            Ok(Value::Vector(vec![1.5, -2.0]))
        );
        // `sparsevec_send`: dimension, count, 0, the indices from 0, the weights.
        let s = words(&[
            &5u32.to_be_bytes(),
            &2u32.to_be_bytes(),
            &[0; 4],
            &1u32.to_be_bytes(),
            &3u32.to_be_bytes(),
            &0.5f32.to_be_bytes(),
            &0.25f32.to_be_bytes(),
        ]);
        assert_eq!(
            decode(&s, true, OID_SPARSEVEC),
            Ok(Value::Sparse(5, vec![(1, 0.5), (3, 0.25)]))
        );
        // Refused as pgvector refuses them.
        let nan = words(&[&1u16.to_be_bytes(), &[0, 0], &f32::NAN.to_be_bytes()]);
        assert_eq!(
            decode(&nan, true, OID_VECTOR),
            Err(("22000", "NaN not allowed in vector".to_string()))
        );
        let inf = words(&[&1u16.to_be_bytes(), &[0, 0], &0x7c00u16.to_be_bytes()]);
        assert_eq!(
            decode(&inf, true, OID_HALFVEC),
            Err(("22000", "infinite value not allowed in halfvec".to_string()))
        );
        // The text a vector went as before it had a binary format is read
        // as it was; bytes that are neither are refused.
        assert_eq!(
            decode(b"[1,2]", true, OID_VECTOR),
            Ok(Value::Vector(vec![1.0, 2.0]))
        );
        assert_eq!(
            decode(b"{2:1}/3", true, OID_SPARSEVEC),
            Ok(Value::Text("{2:1}/3".into()))
        );
        assert_eq!(decode(&v[..7], true, OID_VECTOR).unwrap_err().0, "22P03");
        // In text, a vector reads as it always has.
        assert_eq!(
            decode(b"[1,2]", false, OID_VECTOR),
            Ok(Value::Vector(vec![1.0, 2.0]))
        );
    }

    #[test]
    fn a_text_parameter_is_read_as_its_places_field() {
        let read = |raw: &str, place: Option<DataType>| {
            decode(raw.as_bytes(), false, 0, place.as_ref()).unwrap()
        };
        // A text field's value is the text it is: read by its look, these
        // were a boolean, a number and a vector, which the field refused.
        for t in ["t", "42", "1.5", "[1,2]"] {
            assert_eq!(read(t, Some(DataType::Text)), Value::Text(t.into()));
        }
        assert_eq!(read("42", Some(DataType::Int)), Value::Int(42));
        assert_eq!(read("42", Some(DataType::Float)), Value::Float(42.0));
        assert_eq!(read("1", Some(DataType::Bool)), Value::Bool(true));
        assert_eq!(
            read("\\x00ff", Some(DataType::Bytes)),
            Value::Bytes(vec![0, 255])
        );
        // node-postgres sends a JavaScript array as a PostgreSQL one.
        assert_eq!(
            read("{1,2}", Some(DataType::Vector(2, Default::default()))),
            Value::List(vec![Value::Float(1.0), Value::Float(2.0)])
        );
        assert_eq!(
            read(
                r#"["a","b"]"#,
                Some(DataType::List(Box::new(DataType::Text)))
            ),
            Value::List(vec![Value::Text("a".into()), Value::Text("b".into())])
        );
        // Epoch milliseconds stay the number they are, and text no value of
        // the field's type, or with no place, is read by its look.
        assert_eq!(
            read("1700000000000", Some(DataType::Timestamp)),
            Value::Int(1_700_000_000_000)
        );
        assert_eq!(read("abc", Some(DataType::Int)), Value::Text("abc".into()));
        assert_eq!(read("t", None), Value::Bool(true));
    }
}
