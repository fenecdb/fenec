//! Minimal JSON encoder/parser.
//!
//! Why our own JSON: the only data exchange format at the WASM boundary is
//! JSON, and serde_json adds ~100KB. The version here supports only the
//! subset fenecdb needs.

use crate::error::{Error, Result};
use crate::query::{Response, ResultSet};
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
            out.push_str(&format!("{f}"));
        }
    } else {
        out.push_str("null");
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
                num_into(out, *x as f64);
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
    }
}

pub fn to_string(v: &Value) -> String {
    let mut s = String::new();
    value_into(&mut s, v);
    s
}

pub fn result_set_into(out: &mut String, rs: &ResultSet) {
    out.push_str("{\"columns\":[");
    for (i, c) in rs.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        escape_into(out, c);
    }
    out.push_str("],\"rows\":[");
    for (i, row) in rs.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        for (j, c) in rs.columns.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            escape_into(out, c);
            out.push(':');
            value_into(out, &row.values[j]);
        }
        if let Some(s) = row.score {
            out.push_str(",\"_score\":");
            num_into(out, s as f64);
        }
        out.push('}');
    }
    out.push_str("]}");
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
                for (j, f) in s.fields.iter().enumerate() {
                    if j > 0 {
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
                            crate::schema::IndexKind::Hash => "hash".to_string(),
                            crate::schema::IndexKind::Vector(spec) => {
                                format!("hnsw({}, m={})", spec.metric.name(), spec.m)
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

pub fn parse(src: &str) -> Result<Value> {
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    let v = parse_value(&b, &mut i)?;
    skip_ws(&b, &mut i);
    if i != b.len() {
        return Err(Error::Query("trailing characters after JSON".into()));
    }
    Ok(v)
}

fn skip_ws(b: &[char], i: &mut usize) {
    while *i < b.len() && b[*i].is_whitespace() {
        *i += 1;
    }
}

fn parse_value(b: &[char], i: &mut usize) -> Result<Value> {
    skip_ws(b, i);
    let c = *b
        .get(*i)
        .ok_or_else(|| Error::Query("unexpected end of JSON".into()))?;
    match c {
        'n' => {
            expect_word(b, i, "null")?;
            Ok(Value::Null)
        }
        't' => {
            expect_word(b, i, "true")?;
            Ok(Value::Bool(true))
        }
        'f' => {
            expect_word(b, i, "false")?;
            Ok(Value::Bool(false))
        }
        '"' => Ok(Value::Text(parse_string(b, i)?)),
        '[' => {
            let items = parse_array(b, i)?;
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
        '{' => {
            // Objects are not supported as list-of-pairs nor silently skipped:
            // the fenecdb value model has no object. Error out so it is visible.
            Err(Error::Query(
                "a JSON object is not supported as a fenecdb value".into(),
            ))
        }
        c if c == '-' || c.is_ascii_digit() => {
            let s = *i;
            if b[*i] == '-' {
                *i += 1;
            }
            let mut is_float = false;
            while *i < b.len()
                && (b[*i].is_ascii_digit()
                    || b[*i] == '.'
                    || b[*i] == 'e'
                    || b[*i] == 'E'
                    || b[*i] == '+'
                    || b[*i] == '-')
            {
                if b[*i] == '.' || b[*i] == 'e' || b[*i] == 'E' {
                    is_float = true;
                }
                *i += 1;
            }
            let text: String = b[s..*i].iter().collect();
            if is_float {
                text.parse::<f64>()
                    .map(Value::Float)
                    .map_err(|_| Error::Query(format!("invalid number `{text}`")))
            } else {
                text.parse::<i64>()
                    .map(Value::Int)
                    .map_err(|_| Error::Query(format!("invalid number `{text}`")))
            }
        }
        other => Err(Error::Query(format!("unexpected JSON character `{other}`"))),
    }
}

/// Parses an array starting at `[` element by element; it does *not* apply
/// the vector shortcut. That shortcut only makes sense in value position.
fn parse_array(b: &[char], i: &mut usize) -> Result<Vec<Value>> {
    *i += 1; // `[`
    let mut items = Vec::new();
    loop {
        skip_ws(b, i);
        if b.get(*i) == Some(&']') {
            *i += 1;
            break;
        }
        items.push(parse_value(b, i)?);
        skip_ws(b, i);
        match b.get(*i) {
            Some(',') => *i += 1,
            Some(']') => {
                *i += 1;
                break;
            }
            _ => return Err(Error::Query("expected `,` or `]` in JSON array".into())),
        }
    }
    Ok(items)
}

fn expect_word(b: &[char], i: &mut usize, w: &str) -> Result<()> {
    for c in w.chars() {
        if b.get(*i) != Some(&c) {
            return Err(Error::Query(format!("expected `{w}`")));
        }
        *i += 1;
    }
    Ok(())
}

fn parse_string(b: &[char], i: &mut usize) -> Result<String> {
    *i += 1; // opening quote
    let mut s = String::new();
    loop {
        let c = *b
            .get(*i)
            .ok_or_else(|| Error::Query("unterminated JSON string".into()))?;
        *i += 1;
        match c {
            '"' => return Ok(s),
            '\\' => {
                let e = *b
                    .get(*i)
                    .ok_or_else(|| Error::Query("truncated escape sequence".into()))?;
                *i += 1;
                match e {
                    'n' => s.push('\n'),
                    't' => s.push('\t'),
                    'r' => s.push('\r'),
                    'b' => s.push('\u{8}'),
                    'f' => s.push('\u{c}'),
                    'u' => {
                        let hex: String = b
                            .get(*i..*i + 4)
                            .map(|c| c.iter().collect())
                            .ok_or_else(|| Error::Query("truncated \\u escape".into()))?;
                        *i += 4;
                        let code = u32::from_str_radix(&hex, 16)
                            .map_err(|_| Error::Query("invalid \\u escape".into()))?;
                        s.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                    }
                    other => s.push(other),
                }
            }
            c => s.push(c),
        }
    }
}

/// Parses a JSON object into field-value pairs.
///
/// `parse` rejects an object as a *value* -- the fenecdb value model has no
/// object. But a **document** is an object: the HTTP body and internal
/// imports come through this entry. A nested object is still rejected,
/// because it cannot be a field value.
pub fn parse_object(src: &str) -> Result<Vec<(String, Value)>> {
    let b: Vec<char> = src.trim().chars().collect();
    let mut i = 0;
    let out = parse_object_at(&b, &mut i)?;
    skip_ws(&b, &mut i);
    if i != b.len() {
        return Err(Error::Query("trailing characters after JSON".into()));
    }
    Ok(out)
}

/// Object or array of objects -> list of documents.
pub fn parse_documents(src: &str) -> Result<Vec<Vec<(String, Value)>>> {
    let b: Vec<char> = src.trim().chars().collect();
    let mut i = 0;
    skip_ws(&b, &mut i);
    let out = match b.get(i) {
        Some('[') => {
            i += 1;
            let mut docs = Vec::new();
            loop {
                skip_ws(&b, &mut i);
                if b.get(i) == Some(&']') {
                    i += 1;
                    break;
                }
                docs.push(parse_object_at(&b, &mut i)?);
                skip_ws(&b, &mut i);
                match b.get(i) {
                    Some(',') => i += 1,
                    Some(']') => {
                        i += 1;
                        break;
                    }
                    _ => return Err(Error::Query("expected `,` or `]` in JSON array".into())),
                }
            }
            docs
        }
        Some('{') => vec![parse_object_at(&b, &mut i)?],
        _ => {
            return Err(Error::Query(
                "expected a JSON object or array of objects".into(),
            ))
        }
    };
    skip_ws(&b, &mut i);
    if i != b.len() {
        return Err(Error::Query("trailing characters after JSON".into()));
    }
    Ok(out)
}

fn parse_object_at(b: &[char], i: &mut usize) -> Result<Vec<(String, Value)>> {
    skip_ws(b, i);
    if b.get(*i) != Some(&'{') {
        return Err(Error::Query("expected a JSON object".into()));
    }
    *i += 1;
    let mut out: Vec<(String, Value)> = Vec::new();
    loop {
        skip_ws(b, i);
        if b.get(*i) == Some(&'}') {
            *i += 1;
            break;
        }
        if b.get(*i) != Some(&'"') {
            return Err(Error::Query(
                "expected a field name in the JSON object".into(),
            ));
        }
        let key = parse_string(b, i)?;
        skip_ws(b, i);
        if b.get(*i) != Some(&':') {
            return Err(Error::Query("expected `:` in the JSON object".into()));
        }
        *i += 1;
        let value = parse_value(b, i)?;
        // A repeated key is not silently overwritten: which one wins depends
        // on the parser, and that is an invisible difference.
        if out.iter().any(|(k, _)| k == &key) {
            return Err(Error::Query(format!("field `{key}` was given twice")));
        }
        out.push((key, value));
        skip_ws(b, i);
        match b.get(*i) {
            Some(',') => *i += 1,
            Some('}') => {
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
    let b: Vec<char> = t.chars().collect();
    let mut i = 0;
    skip_ws(&b, &mut i);
    if b.get(i) != Some(&'[') {
        // A single value is accepted too.
        return Ok(vec![parse(t)?]);
    }
    let items = parse_array(&b, &mut i)?;
    skip_ws(&b, &mut i);
    if i != b.len() {
        return Err(Error::Query("trailing characters after JSON".into()));
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // A nested object cannot be a field value.
        assert!(parse_object(r#"{"a": {"b": 1}}"#).is_err());
        // A repeated key does not silently pick a winner.
        assert!(parse_object(r#"{"a": 1, "a": 2}"#).is_err());
        assert!(parse_object("[]").is_err());
        assert!(parse_documents("5").is_err());
        assert!(parse_object(r#"{"a": 1} x"#).is_err());
    }

    #[test]
    fn params_mixed() {
        let p = parse_params(r#"[[0.1,0.2], "abc", 7]"#).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p[0], Value::Vector(vec![0.1, 0.2]));
        assert_eq!(p[2], Value::Int(7));
    }
}
