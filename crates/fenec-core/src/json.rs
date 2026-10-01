//! Minimal JSON encoder/parser.
//!
//! Why our own JSON: the only data exchange format at the WASM boundary is
//! JSON, and serde_json adds ~100KB. The version here supports only the
//! subset fenecdb needs.

use crate::error::{Error, Result};
use crate::query::{Nested, Response, ResultSet, Row};
use crate::value::Value;

// ---------------------------------------------------------------- writing

pub fn escape_into(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn num_into(out: &mut String, f: f64) {
    if f.is_finite() {
        if f.fract() == 0.0 && f.abs() < 1e15 {
            out.push_str(&format!("{}", f as i64));
        } else {
            crate::num::f64_into(out, f);
        }
    } else {
        out.push_str("null");
    }
}

/// `7.038531e-26`, the one `f32` whose shortest text an `f64` reader rounds
/// to another: read as an `f64`, it lands exactly between it and the `f32`
/// above, and a tie goes to the even one.
const TIE: u32 = 0x15ae_43fd;

/// An `f32` -- a vector's component, a row's score -- as the shortest text
/// that reads back as it: `0.1`, where the `f64` it widens to wrote
/// `0.10000000149011612`, as the pg wire and pgvector write a vector and a
/// sparse vector's weights. A page of 200 768-dim vectors is 43% shorter.
/// JavaScript reads it as an `f64` and a `Float32Array` rounds that again,
/// which gives every `f32` back but [`TIE`], so that one keeps its `f64`'s
/// text, which reads back exact (`every_f32_reads_back_through_an_f64`).
fn num32_into(out: &mut String, x: f32) {
    if !x.is_finite() {
        out.push_str("null");
    } else if x.abs().to_bits() == TIE {
        crate::num::f64_into(out, x as f64);
    } else {
        crate::num::f32_into(out, x);
    }
}

pub fn value_into(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => out.push_str(&i.to_string()),
        // ISO-8601 for the browser side: `new Date(x)` works directly.
        Value::Timestamp(ms) => escape_into(out, &crate::time::format_iso(*ms)),
        Value::Float(f) => num_into(out, *f),
        Value::Text(s) => escape_into(out, s),
        Value::Bytes(b) => {
            // byte array as a list of numbers
            out.push('[');
            for (i, x) in b.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&x.to_string());
            }
            out.push(']');
        }
        Value::Vector(v) => {
            out.push('[');
            for (i, x) in v.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                num32_into(out, *x);
            }
            out.push(']');
        }
        Value::List(items) => {
            out.push('[');
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                value_into(out, x);
            }
            out.push(']');
        }
        Value::Object(members) => {
            out.push('{');
            for (i, (k, v)) in members.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                escape_into(out, k);
                out.push(':');
                value_into(out, v);
            }
            out.push('}');
        }
        // pgvector's text form, as a string: the one form every transport
        // takes back.
        Value::Sparse(dim, entries) => {
            out.push('"');
            crate::sparse::format_into(out, *dim, entries);
            out.push('"');
        }
    }
}

pub fn to_string(v: &Value) -> String {
    let mut s = String::new();
    value_into(&mut s, v);
    s
}

/// A row's group at one level of a `lookup` chain, and the levels below it.
///
/// `group` is the index the owning row has among the rows of the level
/// *above*, counted across groups -- the alignment `Nested` documents.
pub struct Children<'a> {
    level: &'a Nested,
    group: usize,
}

/// The rows of a result as a JSON array, children and grandchildren nested
/// inside the row they hang from.
///
/// This loop had two copies before -- here and in the HTTP endpoint's
/// `rows_json` -- and `lookup` would have made a third place to forget, so
/// both now come through here.
pub fn rows_array_into(out: &mut String, rs: &ResultSet) {
    // One cursor per nested level: the index of the next row to be emitted
    // there. A level's groups are keyed by the position of the owning row in
    // the level above, and emission walks rows in exactly that order, so a
    // running counter *is* that position -- no prefix sums, and no second
    // pass to keep in step with.
    let mut depth = 0;
    let mut level = rs.nested.as_ref();
    while let Some(n) = level {
        depth += 1;
        level = n.nested.as_deref();
    }
    let mut cursors = vec![0usize; depth];

    out.push('[');
    for (i, row) in rs.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let children = rs.nested.as_ref().map(|n| Children { level: n, group: i });
        row_object_into(out, &rs.columns, row, children, &mut cursors, 0);
    }
    out.push(']');
}

/// One row as a JSON object: its columns, the score when `near` produced
/// one, and the children `lookup` attached -- each of which carries its own
/// children, all the way down the chain.
pub fn row_object_into(
    out: &mut String,
    columns: &[String],
    row: &Row,
    children: Option<Children<'_>>,
    cursors: &mut [usize],
    depth: usize,
) {
    out.push('{');
    for (j, c) in columns.iter().enumerate() {
        if j > 0 {
            out.push(',');
        }
        escape_into(out, c);
        out.push(':');
        value_into(out, &row.values[j]);
    }
    if let Some(s) = row.score {
        out.push_str(",\"_score\":");
        num32_into(out, s);
    }
    // A row with no matches still gets the key, holding an empty array: a
    // missing one would read as "not asked for" rather than "nothing
    // matched".
    if let Some(c) = children {
        out.push(',');
        escape_into(out, &c.level.name);
        out.push_str(":[");
        for (k, child) in c.level.group(c.group).iter().enumerate() {
            if k > 0 {
                out.push(',');
            }
            let pos = cursors[depth];
            cursors[depth] += 1;
            let below = c.level.nested.as_deref().map(|n| Children {
                level: n,
                group: pos,
            });
            row_object_into(out, &c.level.columns, child, below, cursors, depth + 1);
        }
        out.push(']');
    }
    out.push('}');
}

pub fn result_set_into(out: &mut String, rs: &ResultSet) {
    out.push_str("{\"columns\":[");
    for (i, c) in rs.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        escape_into(out, c);
    }
    out.push_str("],\"rows\":");
    rows_array_into(out, rs);
    out.push('}');
}

pub fn response_to_string(r: &Response) -> String {
    let mut out = String::new();
    match r {
        Response::Rows(rs) => {
            out.push_str("{\"kind\":\"rows\",\"result\":");
            result_set_into(&mut out, rs);
            out.push('}');
        }
        Response::Affected(n) => {
            out.push_str(&format!("{{\"kind\":\"affected\",\"count\":{n}}}"));
        }
        Response::Ok(msg) => {
            out.push_str("{\"kind\":\"ok\",\"message\":");
            escape_into(&mut out, msg);
            out.push('}');
        }
        Response::Schemas(schemas) => {
            out.push_str("{\"kind\":\"schemas\",\"collections\":[");
            for (i, s) in schemas.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str("{\"name\":");
                escape_into(&mut out, &s.name);
                out.push_str(",\"fields\":[");
                // The indexes on paths into json fields after the fields,
                // under `paths`, and only where there are any.
                for (j, f) in s.fields.iter().chain(&s.paths).enumerate() {
                    if j == s.fields.len() {
                        out.push_str("],\"paths\":[");
                    } else if j > 0 {
                        out.push(',');
                    }
                    out.push_str("{\"name\":");
                    escape_into(&mut out, &f.name);
                    out.push_str(",\"type\":");
                    escape_into(&mut out, &f.ty.name());
                    out.push_str(",\"index\":");
                    escape_into(
                        &mut out,
                        match &f.index {
                            crate::schema::IndexKind::None => "none".to_string(),
                            crate::schema::IndexKind::Hash { unique: false } => "hash".to_string(),
                            crate::schema::IndexKind::Hash { unique: true } => "unique".to_string(),
                            crate::schema::IndexKind::Sorted => "sorted".to_string(),
                            crate::schema::IndexKind::Inverted => "inverted".to_string(),
                            crate::schema::IndexKind::Vector(spec) => {
                                format!(
                                    "hnsw({}, m={}{})",
                                    spec.metric.name(),
                                    spec.m,
                                    spec.quant_arg()
                                )
                            }
                            crate::schema::IndexKind::Text(spec) => {
                                format!("text({})", spec.args())
                            }
                        }
                        .as_str(),
                    );
                    out.push('}');
                }
                out.push_str("]}");
            }
            out.push_str("]}");
        }
    }
    out
}

pub fn error_to_string(e: &Error) -> String {
    let mut out = String::from("{\"kind\":\"error\",\"message\":");
    escape_into(&mut out, &e.to_string());
    out.push('}');
    out
}

// ---------------------------------------------------------------- reading
//
// The reader walks the text's bytes. JSON's structure is ASCII, so a string
// is copied a run at a time between its quotes and escapes, and a number is
// parsed where it stands. Collected into `char`s first -- four bytes a
// character, and a pass of its own -- with each number gathered into a
// `String`, a page of 200 768-dim vectors took 51.9 ms to read in the
// browser module, against 23.7.

pub fn parse(src: &str) -> Result<Value> {
    let mut i = 0;
    let v = parse_value(src, &mut i)?;
    skip_ws(src, &mut i);
    if i != src.len() {
        return Err(Error::Query("trailing characters after JSON".into()));
    }
    Ok(v)
}

/// A `json` field's value from its JSON text, every number as it is
/// written: a list of numbers alone is a list here, not the vector [`parse`]
/// makes of one at the top. What jsonb carries over the pg wire, a COPY's
/// cell, and a document's member for a json field (`parse_documents_json`).
pub fn parse_json(src: &str) -> Result<Value> {
    let mut i = 0;
    let v = parse_exact_at(src, &mut i, 0)?;
    skip_ws(src, &mut i);
    if i != src.len() {
        return Err(Error::Query("trailing characters after JSON".into()));
    }
    Ok(v)
}

/// [`parse_value_at`], an array at the top of the value read as a list.
fn parse_exact_at(s: &str, i: &mut usize, depth: usize) -> Result<Value> {
    skip_ws(s, i);
    match s.as_bytes().get(*i) {
        Some(b'[') if depth < crate::value::MAX_JSON_DEPTH => {
            Ok(Value::List(parse_array_at(s, i, depth + 1)?))
        }
        _ => parse_value_at(s, i, depth),
    }
}

/// The character starting at byte `i`.
fn char_at(s: &str, i: usize) -> Option<char> {
    s.get(i..).and_then(|t| t.chars().next())
}

fn skip_ws(s: &str, i: &mut usize) {
    while let Some(&b) = s.as_bytes().get(*i) {
        // A byte past ASCII starts a character, which may be a space too.
        let c = match b < 0x80 {
            true => b as char,
            false => match char_at(s, *i) {
                Some(c) => c,
                None => return,
            },
        };
        if !c.is_whitespace() {
            return;
        }
        *i += c.len_utf8();
    }
}

fn parse_value(s: &str, i: &mut usize) -> Result<Value> {
    parse_value_at(s, i, 0)
}

/// A value at `depth` containers down: an array of numbers alone is a
/// vector at the top of a value (`depth` 0), as an embedding travels, and
/// a list of numbers below it -- inside an object, which only a `json`
/// field holds, or another array -- each number as it was written, an
/// integer an integer: read into `f32`s there, `[19.99]` in a document's
/// metadata came back as 19.989999771118164. Past
/// [`crate::value::MAX_JSON_DEPTH`] it is refused, where the recursion
/// would otherwise take the stack down with a deep enough text.
fn parse_value_at(s: &str, i: &mut usize, depth: usize) -> Result<Value> {
    skip_ws(s, i);
    let b = s.as_bytes();
    let c = *b
        .get(*i)
        .ok_or_else(|| Error::Query("unexpected end of JSON".into()))?;
    if matches!(c, b'[' | b'{') && depth >= crate::value::MAX_JSON_DEPTH {
        return Err(Error::Query(format!(
            "JSON nested deeper than {} levels",
            crate::value::MAX_JSON_DEPTH
        )));
    }
    match c {
        b'n' => {
            expect_word(s, i, "null")?;
            Ok(Value::Null)
        }
        b't' => {
            expect_word(s, i, "true")?;
            Ok(Value::Bool(true))
        }
        b'f' => {
            expect_word(s, i, "false")?;
            Ok(Value::Bool(false))
        }
        b'"' => Ok(Value::Text(parse_string(s, i)?)),
        b'[' => {
            if depth > 0 {
                return Ok(Value::List(parse_array_at(s, i, depth + 1)?));
            }
            // Numbers alone are a vector, read straight into its `f32`s --
            // natively: a page's vectors come into the browser module as
            // `f32`s already (`vectorsApart`), and there the reader was 474
            // bytes brotli for nothing.
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(v) = numbers(s, i) {
                return Ok(Value::Vector(v));
            }
            let items = parse_array_at(s, i, depth + 1)?;
            // If every item is a number, read it as a vector (embedding transfer)
            if !items.is_empty()
                && items
                    .iter()
                    .all(|v| matches!(v, Value::Int(_) | Value::Float(_)))
            {
                return Ok(Value::Vector(
                    items.iter().map(|v| v.as_f64().unwrap() as f32).collect(),
                ));
            }
            Ok(Value::List(items))
        }
        // An object is a value of a `json` field: its members sorted, a key
        // given twice refused (`Value::object`).
        b'{' => Value::object(parse_members(s, i, "", &[], depth + 1)?)
            .map_err(|e| Error::Query(e.to_string())),
        b'-' | b'0'..=b'9' => {
            let (text, is_float) = number_at(s, i);
            if is_float {
                crate::num::parse_f64(text)
                    .map(Value::Float)
                    .ok_or_else(|| Error::Query(format!("invalid number `{text}`")))
            } else {
                text.parse::<i64>()
                    .map(Value::Int)
                    .map_err(|_| Error::Query(format!("invalid number `{text}`")))
            }
        }
        _ => {
            let other = char_at(s, *i).unwrap_or(char::REPLACEMENT_CHARACTER);
            Err(Error::Query(format!("unexpected JSON character `{other}`")))
        }
    }
}

/// The number at `i`, which starts with `-` or a digit: its text, and
/// whether it is written as a float.
fn number_at<'s>(s: &'s str, i: &mut usize) -> (&'s str, bool) {
    let b = s.as_bytes();
    let start = *i;
    if b.get(*i) == Some(&b'-') {
        *i += 1;
    }
    let mut is_float = false;
    while let Some(&d) = b.get(*i) {
        match d {
            b'0'..=b'9' | b'+' | b'-' => {}
            b'.' | b'e' | b'E' => is_float = true,
            _ => break,
        }
        *i += 1;
    }
    (s.get(start..*i).unwrap_or(""), is_float)
}

/// The array at `i` when it holds numbers alone, each the `f32` the vector
/// shortcut makes of it -- an integer through `i64` and `f64`, as its
/// `Value::Int` went -- and `i` past it. `None`, `i` where it was, for
/// anything else, which the general path reads, or refuses, as it did.
/// Read a `Value` at a time and converted, each number's text found and
/// then read twice over, a 128-dim vector took 5.26 us and a 768-dim one
/// 30.1; read here, 2.05 and 11.1, and a COPY of 128-dim rows without an
/// index went 120k -> 196k rows/s.
#[cfg(not(target_arch = "wasm32"))]
fn numbers(s: &str, i: &mut usize) -> Option<Vec<f32>> {
    let b = s.as_bytes();
    let mut j = *i + 1;
    let mut out = Vec::new();
    // A space is looked for only where a byte could start one: a call a
    // side was an eighth of a vector's reading.
    let space = |j: &mut usize| {
        if b.get(*j).is_some_and(|&c| c <= b' ' || c >= 0x80) {
            skip_ws(s, j);
        }
    };
    loop {
        space(&mut j);
        if !matches!(b.get(j), Some(b'-' | b'0'..=b'9')) {
            return None;
        }
        let x = match crate::num::clinger(b, &mut j) {
            Some(x) => x,
            None => {
                let (text, is_float) = number_at(s, &mut j);
                match is_float {
                    true => crate::num::parse_f64(text)?,
                    false => text.parse::<i64>().ok()? as f64,
                }
            }
        };
        out.push(x as f32);
        space(&mut j);
        match b.get(j) {
            Some(b',') => j += 1,
            Some(b']') => {
                *i = j + 1;
                return Some(out);
            }
            _ => return None,
        }
    }
}

/// Parses an array starting at `[` element by element; it does *not* apply
/// the vector shortcut. That shortcut only makes sense in value position.
fn parse_array(s: &str, i: &mut usize) -> Result<Vec<Value>> {
    // Its elements are values of their own -- a query's parameters, a
    // listed member -- each read as at the top: `[[0.1, 0.2], 7]` hands a
    // vector and an integer.
    parse_array_at(s, i, 0)
}

/// [`parse_array`], its elements `depth` containers down.
fn parse_array_at(s: &str, i: &mut usize, depth: usize) -> Result<Vec<Value>> {
    let b = s.as_bytes();
    *i += 1; // `[`
    let mut items = Vec::new();
    loop {
        skip_ws(s, i);
        if b.get(*i) == Some(&b']') {
            *i += 1;
            break;
        }
        items.push(parse_value_at(s, i, depth)?);
        skip_ws(s, i);
        match b.get(*i) {
            Some(b',') => *i += 1,
            Some(b']') => {
                *i += 1;
                break;
            }
            _ => return Err(Error::Query("expected `,` or `]` in JSON array".into())),
        }
    }
    Ok(items)
}

fn expect_word(s: &str, i: &mut usize, w: &str) -> Result<()> {
    if s.as_bytes().get(*i..*i + w.len()) != Some(w.as_bytes()) {
        return Err(Error::Query(format!("expected `{w}`")));
    }
    *i += w.len();
    Ok(())
}

fn parse_string(s: &str, i: &mut usize) -> Result<String> {
    let b = s.as_bytes();
    *i += 1; // opening quote
    let mut out = String::new();
    loop {
        // Up to the next quote or escape, whole: both are ASCII, so the
        // run ends where a character does.
        let run = *i;
        while *i < b.len() && b[*i] != b'"' && b[*i] != b'\\' {
            *i += 1;
        }
        out.push_str(s.get(run..*i).unwrap_or(""));
        let c = *b
            .get(*i)
            .ok_or_else(|| Error::Query("unterminated JSON string".into()))?;
        *i += 1;
        if c == b'"' {
            return Ok(out);
        }
        let e = char_at(s, *i).ok_or_else(|| Error::Query("truncated escape sequence".into()))?;
        *i += e.len_utf8();
        match e {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'u' => {
                if *i + 4 > b.len() {
                    return Err(Error::Query("truncated \\u escape".into()));
                }
                let code = s
                    .get(*i..*i + 4)
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .ok_or_else(|| Error::Query("invalid \\u escape".into()))?;
                *i += 4;
                out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
            }
            other => out.push(other),
        }
    }
}

/// Parses a JSON object into field-value pairs.
///
/// A **document** is an object: the HTTP body and internal imports come
/// through this entry, each member a field. A member that is an object is
/// a value for a `json` field, which the schema then takes or refuses.
pub fn parse_object(src: &str) -> Result<Vec<(String, Value)>> {
    parse_object_listing(src, "")
}

/// An object as [`parse_object`] reads one, but the member named `list` read
/// as [`parse_params`] reads a list: each element keeps its type, where an
/// array of numbers read as a value is a vector. A query's parameters are
/// that list -- read as a vector, `[123456789]` handed the query the `f32`
/// 123456792, and `[19.99]` 19.989999771118164.
pub fn parse_object_listing(src: &str, list: &str) -> Result<Vec<(String, Value)>> {
    let s = src.trim();
    let mut i = 0;
    let out = parse_object_at(s, &mut i, list)?;
    skip_ws(s, &mut i);
    if i != s.len() {
        return Err(Error::Query("trailing characters after JSON".into()));
    }
    Ok(out)
}

/// Object or array of objects -> list of documents.
pub fn parse_documents(src: &str) -> Result<Vec<Vec<(String, Value)>>> {
    parse_documents_json(src, &[])
}

/// [`parse_documents`], the members named in `json` -- a collection's json
/// fields -- read as [`parse_json`] reads a value: their numbers as written,
/// where any other member's array of numbers alone is the vector an
/// embedding travels as.
pub fn parse_documents_json(src: &str, json: &[&str]) -> Result<Vec<Vec<(String, Value)>>> {
    let s = src.trim();
    let b = s.as_bytes();
    let mut i = 0;
    skip_ws(s, &mut i);
    let out = match b.get(i) {
        Some(b'[') => {
            i += 1;
            let mut docs = Vec::new();
            loop {
                skip_ws(s, &mut i);
                if b.get(i) == Some(&b']') {
                    i += 1;
                    break;
                }
                docs.push(document(s, &mut i, "", json)?);
                skip_ws(s, &mut i);
                match b.get(i) {
                    Some(b',') => i += 1,
                    Some(b']') => {
                        i += 1;
                        break;
                    }
                    _ => return Err(Error::Query("expected `,` or `]` in JSON array".into())),
                }
            }
            docs
        }
        Some(b'{') => vec![document(s, &mut i, "", json)?],
        _ => {
            return Err(Error::Query(
                "expected a JSON object or array of objects".into(),
            ))
        }
    };
    skip_ws(s, &mut i);
    if i != s.len() {
        return Err(Error::Query("trailing characters after JSON".into()));
    }
    Ok(out)
}

fn parse_object_at(s: &str, i: &mut usize, list: &str) -> Result<Vec<(String, Value)>> {
    document(s, i, list, &[])
}

/// A document at `i`: the member named `list` a parameter list, and those
/// named in `json` read as [`parse_json`] reads a value.
fn document(s: &str, i: &mut usize, list: &str, json: &[&str]) -> Result<Vec<(String, Value)>> {
    let out = parse_members(s, i, list, json, 0)?;
    // A repeated key is not silently overwritten: which one wins depends
    // on the parser, and that is an invisible difference. Asked here of a
    // document's fields, in the order they came; an object's members are
    // asked as it is sorted (`Value::object`).
    for (n, (k, _)) in out.iter().enumerate() {
        if out[..n].iter().any(|(x, _)| x == k) {
            return Err(Error::Query(format!("field `{k}` was given twice")));
        }
    }
    Ok(out)
}

/// The members of the object at `i`, in the order written, each value
/// `depth` containers down -- a document's fields at 0.
fn parse_members(
    s: &str,
    i: &mut usize,
    list: &str,
    json: &[&str],
    depth: usize,
) -> Result<Vec<(String, Value)>> {
    let b = s.as_bytes();
    skip_ws(s, i);
    if b.get(*i) != Some(&b'{') {
        return Err(Error::Query("expected a JSON object".into()));
    }
    *i += 1;
    let mut out: Vec<(String, Value)> = Vec::new();
    loop {
        skip_ws(s, i);
        if b.get(*i) == Some(&b'}') {
            *i += 1;
            break;
        }
        if b.get(*i) != Some(&b'"') {
            return Err(Error::Query(
                "expected a field name in the JSON object".into(),
            ));
        }
        let key = parse_string(s, i)?;
        skip_ws(s, i);
        if b.get(*i) != Some(&b':') {
            return Err(Error::Query("expected `:` in the JSON object".into()));
        }
        *i += 1;
        skip_ws(s, i);
        // A listed member keeps its elements' types, a parameter list's;
        // a json field's, every number.
        let value = if depth == 0 && key == list && b.get(*i) == Some(&b'[') {
            Value::List(parse_array(s, i)?)
        } else if depth == 0 && json.contains(&key.as_str()) {
            parse_exact_at(s, i, 0)?
        } else {
            parse_value_at(s, i, depth)?
        };
        out.push((key, value));
        skip_ws(s, i);
        match b.get(*i) {
            Some(b',') => *i += 1,
            Some(b'}') => {
                *i += 1;
                break;
            }
            _ => {
                return Err(Error::Query(
                    "expected `,` or `}` in the JSON object".into(),
                ))
            }
        }
    }
    Ok(out)
}

/// Turns a JSON array into a parameter list.
///
/// A top-level array is a parameter *list*, not a vector: here `parse`'s
/// "all numbers means vector" shortcut would lose types (`[1999, 2023]` ->
/// two `float`s). That was not a silent bug but a visible wrong answer:
/// `year in [$1, $2]` returned no rows at all. The shortcut stays valid
/// inside nested arrays, so embedding transfer is unaffected.
pub fn parse_params(src: &str) -> Result<Vec<Value>> {
    let t = src.trim();
    if t.is_empty() {
        return Ok(Vec::new());
    }
    let mut i = 0;
    skip_ws(t, &mut i);
    if t.as_bytes().get(i) != Some(&b'[') {
        // A single value is accepted too.
        return Ok(vec![parse(t)?]);
    }
    let items = parse_array(t, &mut i)?;
    skip_ws(t, &mut i);
    if i != t.len() {
        return Err(Error::Query("trailing characters after JSON".into()));
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An array as the general path reads it, a value at a time: a vector
    /// when every item is a number, as `parse_value` read every array
    /// before the one-pass reader.
    fn a_value_at_a_time(s: &str) -> Result<Value> {
        let mut i = 0;
        skip_ws(s, &mut i);
        let items = parse_array(s, &mut i)?;
        skip_ws(s, &mut i);
        if i != s.len() {
            return Err(Error::Query("trailing characters after JSON".into()));
        }
        Ok(
            match !items.is_empty()
                && items
                    .iter()
                    .all(|v| matches!(v, Value::Int(_) | Value::Float(_)))
            {
                true => Value::Vector(items.iter().map(|v| v.as_f64().unwrap() as f32).collect()),
                false => Value::List(items),
            },
        )
    }

    fn bits(v: &Result<Value>) -> Option<Vec<u32>> {
        match v {
            Ok(Value::Vector(v)) => Some(v.iter().map(|x| x.to_bits()).collect()),
            _ => None,
        }
    }

    /// The vector shortcut reads its numbers in one pass where Clinger's
    /// path takes them, and every vector -- every refusal, every list -- is
    /// the one read a value at a time, to the bit: -0 and -0.0, 19 digits
    /// and 20, exponents near and past 22, integers past 2^53 and `i64`,
    /// and the runs of `[0-9.eE+-]` that are no number at all.
    #[test]
    fn a_vector_read_in_one_pass_is_the_one_read_a_value_at_a_time() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let digits = |next: &mut dyn FnMut(u64) -> u64, n: u64| -> String {
            (0..n).map(|_| char::from(b'0' + next(10) as u8)).collect()
        };
        let fixed = [
            "0",
            "-0",
            "0.0",
            "-0.0",
            "00",
            "-01.5",
            "1.",
            "1.e5",
            "1e",
            "1e+",
            "1e-",
            "1.2.3",
            "1-2",
            "--1",
            "1e5e5",
            "9007199254740992",
            "9007199254740993",
            "-9007199254740993",
            "9223372036854775807",
            "9223372036854775808",
            "-9223372036854775808",
            "99999999999999999999",
            "1e22",
            "1e23",
            "1.5e22",
            "1e-22",
            "1e-23",
            "1E+05",
            "4e0400",
            "0.1",
            "0.30000000000000004",
            "1234567890123456789",
            "12345678901234567890",
            "0.0000000000000000000001234",
            "1e-400",
            "1e400",
            "-1e-400",
            "7.038531e-26",
        ];
        let mut texts: Vec<String> = fixed.iter().map(|t| format!("[{t}]")).collect();
        texts.push(format!("[{}]", fixed.join(", ")));
        for _ in 0..40_000 {
            let n = 1 + next(12);
            let items: Vec<String> = (0..n)
                .map(|_| {
                    let mut t = String::new();
                    if next(3) == 0 {
                        t.push('-');
                    }
                    let zeros = next(4);
                    t.push_str(&"0".repeat(zeros as usize));
                    let int = next(22);
                    t.push_str(&digits(&mut next, int));
                    if t.is_empty() || t == "-" {
                        t.push('0');
                    }
                    if next(3) > 0 {
                        t.push('.');
                        let frac = 1 + next(22);
                        t.push_str(&digits(&mut next, frac));
                    }
                    if next(4) == 0 {
                        t.push(['e', 'E'][next(2) as usize]);
                        match next(3) {
                            0 => t.push('-'),
                            1 => t.push('+'),
                            _ => {}
                        }
                        let e = 1 + next(3);
                        t.push_str(&digits(&mut next, e));
                    }
                    if next(200) == 0 {
                        t.push(['.', 'e', '-', '+', '1'][next(5) as usize]);
                    }
                    t
                })
                .collect();
            let sep = [",", ", ", " ,\n"][next(3) as usize];
            texts.push(format!("[{}]", items.join(sep)));
        }
        let (mut vectors, mut refused) = (0, 0);
        for t in &texts {
            let (one, each) = (parse(t), a_value_at_a_time(t));
            assert_eq!(bits(&one), bits(&each), "{t}");
            assert_eq!(one.is_err(), each.is_err(), "{t}");
            if !matches!(one, Ok(Value::Vector(_))) {
                assert_eq!(format!("{one:?}"), format!("{each:?}"), "{t}");
            }
            vectors += bits(&one).is_some() as u32;
            refused += one.is_err() as u32;
        }
        // Both kinds were tried, many times over.
        assert!(vectors > 30_000 && refused > 100, "{vectors} {refused}");
    }

    #[test]
    fn roundtrip() {
        let v = Value::List(vec![
            Value::Int(1),
            Value::Text("a\"b\n".into()),
            Value::Bool(false),
            Value::Null,
        ]);
        let s = to_string(&v);
        assert_eq!(parse(&s).unwrap(), v);
    }

    /// The reader walks bytes, and reads what it read walking `char`s: a
    /// space of any script between tokens, a string of any script whole,
    /// escapes of every kind, and the errors, named as they were.
    #[test]
    fn the_reader_reads_every_script() {
        assert_eq!(
            parse("\u{a0}[1,\u{2003}2.5]\u{3000}").unwrap(),
            Value::Vector(vec![1.0, 2.5])
        );
        assert_eq!(
            parse(r#""a\u00e9b ü \"q\" \\ \/ \n\t 日本語""#).unwrap(),
            Value::Text("aéb ü \"q\" \\ / \n\t 日本語".into())
        );
        // An unknown escape keeps the character it escapes, whole.
        assert_eq!(parse(r#""\é\x""#).unwrap(), Value::Text("éx".into()));
        assert_eq!(
            parse(r#""\ud800""#).unwrap(),
            Value::Text("\u{fffd}".into())
        );
        let o = parse_object("{\"é\": \"ü\",\u{a0}\"n\": -1.5e3, \"i\": -7}").unwrap();
        assert_eq!(
            o,
            vec![
                ("é".to_string(), Value::Text("ü".into())),
                ("n".to_string(), Value::Float(-1500.0)),
                ("i".to_string(), Value::Int(-7)),
            ]
        );
        let err = |src: &str| parse(src).unwrap_err().to_string();
        assert!(err("é").contains("unexpected JSON character `é`"));
        assert!(err(r#""\u12""#).contains("truncated \\u escape"));
        assert!(err(r#""\u12é""#).contains("invalid \\u escape"));
        assert!(err(r#""abc"#).contains("unterminated JSON string"));
        assert!(err("[1 2]").contains("expected `,` or `]`"));
        assert!(err("nul").contains("expected `null`"));
        assert!(err("1.2.3").contains("invalid number `1.2.3`"));
        assert!(err("[1] x").contains("trailing characters"));
    }

    #[test]
    fn numeric_array_becomes_vector() {
        assert_eq!(
            parse("[0.1, 0.2, 3]").unwrap(),
            Value::Vector(vec![0.1, 0.2, 3.0])
        );
    }

    /// A top-level array is a parameter list: every element keeps its type.
    /// When the vector shortcut leaked in here, `[1999, 2023]` became two
    /// `float`s and `year in [$1, $2]` returned no rows at all.
    #[test]
    fn top_level_params_keep_their_type() {
        let p = parse_params("[1999, 2023]").unwrap();
        assert_eq!(p, vec![Value::Int(1999), Value::Int(2023)]);
        // A single parameter is not an array either.
        assert_eq!(parse_params("[7]").unwrap(), vec![Value::Int(7)]);
        // The shortcut stays valid in a nested array: embedding transfer is fine.
        assert_eq!(
            parse_params("[[0.1, 0.2]]").unwrap(),
            vec![Value::Vector(vec![0.1, 0.2])]
        );
        assert_eq!(parse_params("[]").unwrap(), Vec::<Value>::new());
        assert_eq!(parse_params("  ").unwrap(), Vec::<Value>::new());
        assert!(parse_params("[1] 2").is_err());
    }

    #[test]
    fn objects_become_documents() {
        let d = parse_object(r#"{"a": 1, "b": "x", "c": [0.1, 0.2], "d": null}"#).unwrap();
        assert_eq!(d.len(), 4);
        assert_eq!(d[0], ("a".to_string(), Value::Int(1)));
        assert_eq!(d[2].1, Value::Vector(vec![0.1, 0.2]));
        assert_eq!(d[3].1, Value::Null);

        let docs = parse_documents(r#"[{"a": 1}, {"a": 2}]"#).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(parse_documents(r#"{"a": 1}"#).unwrap().len(), 1);
        assert_eq!(parse_documents("[]").unwrap().len(), 0);

        // A nested object is a `json` field's value, its members sorted and
        // its lists exact; a key twice in it is refused.
        let d = parse_object(r#"{"a": {"z": [1, 2.5], "b": {"c": null}}}"#).unwrap();
        assert_eq!(
            d[0].1,
            Value::Object(vec![
                ("b".into(), Value::Object(vec![("c".into(), Value::Null)])),
                (
                    "z".into(),
                    Value::List(vec![Value::Int(1), Value::Float(2.5)])
                ),
            ])
        );
        assert!(parse_object(r#"{"a": {"b": 1, "b": 2}}"#).is_err());
        // A repeated key does not silently pick a winner.
        assert!(parse_object(r#"{"a": 1, "a": 2}"#).is_err());
        assert!(parse_object("[]").is_err());
        assert!(parse_documents("5").is_err());
        assert!(parse_object(r#"{"a": 1} x"#).is_err());
    }

    /// A query body's parameters are a list whose elements keep their types.
    #[test]
    fn an_object_goes_out_as_it_came_in() {
        let text = r#"{"a":[1,2.5,"x",[true]],"b":{"c":null,"d":-7},"e":0.1}"#;
        let v = parse(text).unwrap();
        assert_eq!(to_string(&v), text);
        // Sorted by key on the way in, whatever order it was written in.
        assert_eq!(
            to_string(&parse(r#"{"b":1,"a":2}"#).unwrap()),
            r#"{"a":2,"b":1}"#
        );
    }

    #[test]
    fn json_nests_no_deeper_than_the_limit() {
        let deep = |n: usize| "[".repeat(n) + &"]".repeat(n);
        assert!(parse(&deep(crate::value::MAX_JSON_DEPTH)).is_ok());
        assert!(parse(&deep(crate::value::MAX_JSON_DEPTH + 1)).is_err());
        let objs = |n: usize| r#"{"a":"#.repeat(n) + "1" + &"}".repeat(n);
        assert!(parse(&objs(crate::value::MAX_JSON_DEPTH)).is_ok());
        assert!(parse(&objs(crate::value::MAX_JSON_DEPTH + 1)).is_err());
        // Deep enough to take the stack down, it is refused instead.
        assert!(parse(&deep(100_000)).is_err());
    }

    #[test]
    fn a_listed_member_keeps_its_numbers() {
        let body = r#"{"params": [123456789, 19.99], "v": [1, 2]}"#;
        let o = parse_object_listing(body, "params").unwrap();
        assert_eq!(
            o[0].1,
            Value::List(vec![Value::Int(123_456_789), Value::Float(19.99)])
        );
        // Another member's array of numbers is still a vector.
        assert_eq!(o[1].1, Value::Vector(vec![1.0, 2.0]));
        // A vector parameter is an array inside the list.
        let o = parse_object_listing(r#"{"params": [[0.1, 0.2], 7]}"#, "params").unwrap();
        assert_eq!(
            o[0].1,
            Value::List(vec![Value::Vector(vec![0.1, 0.2]), Value::Int(7)])
        );
        let o = parse_object_listing(r#"{"params": []}"#, "params").unwrap();
        assert_eq!(o[0].1, Value::List(vec![]));
    }

    #[test]
    fn params_mixed() {
        let p = parse_params(r#"[[0.1,0.2], "abc", 7]"#).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p[0], Value::Vector(vec![0.1, 0.2]));
        assert_eq!(p[2], Value::Int(7));
    }

    #[test]
    fn a_vector_is_written_as_its_f32s() {
        let tie = f32::from_bits(TIE);
        let v = [0.1, 0.25, -1.5e-7, 1.0, -0.0, 16_777_217.0, 3e38, tie];
        let want = v
            .iter()
            .map(|x| match *x == tie {
                true => format!("{}", *x as f64),
                false => format!("{x}"),
            })
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(to_string(&Value::Vector(v.to_vec())), format!("[{want}]"));
        assert_eq!(
            to_string(&Value::Vector(vec![0.1, -0.5])),
            "[0.1,-0.5]",
            "not the 0.10000000149011612 its f64 writes"
        );
        assert_eq!(
            to_string(&Value::Vector(vec![f32::NAN, f32::INFINITY])),
            "[null,null]"
        );
    }

    /// What JavaScript makes of an `f32`'s text: the `f64` nearest it,
    /// then the `f32` nearest that (`Float32Array`).
    #[track_caller]
    fn reads_back_through_an_f64(x: f32) {
        let mut text = String::new();
        num32_into(&mut text, x);
        let back = text.parse::<f64>().unwrap() as f32;
        assert_eq!(back.to_bits(), x.to_bits(), "{text}");
    }

    #[test]
    fn f32s_read_back_through_an_f64() {
        // Its shortest text alone does not.
        let tie = f32::from_bits(TIE);
        assert_eq!(tie, 7.038_531e-26);
        let shortest = format!("{tie}").parse::<f64>().unwrap() as f32;
        assert_eq!(shortest.to_bits(), TIE + 1);
        reads_back_through_an_f64(tie);
        reads_back_through_an_f64(-tie);
        // One in 4 099, a spread over every exponent; the test below takes
        // all of them.
        for bits in (0..=u32::MAX).step_by(4099) {
            let x = f32::from_bits(bits);
            if x.is_finite() {
                reads_back_through_an_f64(x);
            }
        }
    }

    /// Every `f32`, which is how [`TIE`] was found to be the only one: `cargo
    /// test -p fenec-core --release --lib every_f32_reads -- --ignored`.
    #[test]
    #[ignore]
    fn every_f32_reads_back_through_an_f64() {
        let threads = std::thread::available_parallelism().map_or(8, |n| n.get()) as u64;
        std::thread::scope(|scope| {
            for t in 0..threads {
                scope.spawn(move || {
                    let span = (1u64 << 32) / threads;
                    let end = if t == threads - 1 {
                        1 << 32
                    } else {
                        (t + 1) * span
                    };
                    for bits in t * span..end {
                        let x = f32::from_bits(bits as u32);
                        if x.is_finite() {
                            reads_back_through_an_f64(x);
                        }
                    }
                });
            }
        });
    }
}
