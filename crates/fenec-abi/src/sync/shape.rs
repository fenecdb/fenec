//! A shape -- the rows of one collection a replica keeps -- as a binding
//! hands it over, and what is made of it: the subscription's query string
//! and the local collection's text. `web/fenec.js`'s `normalizeShape`,
//! `shapeParams` and `schemaDDL`, written once for every native binding.

use fenec_core::error::{Error, Result};
use fenec_core::json;
use fenec_core::value::Value;

/// A member of a JSON object, by name.
pub fn member<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    match v {
        Value::Object(m) => m.iter().find(|(k, _)| k == name).map(|(_, v)| v),
        _ => None,
    }
}

/// FenecQL's identifier rule, as the builders and the lexer read it.
pub fn ident(name: &str, what: &str) -> Result<String> {
    let mut chars = name.chars();
    let ok = matches!(chars.next(), Some(c) if c == '_' || c.is_alphabetic())
        && chars.all(|c| c == '_' || c.is_alphanumeric());
    match ok {
        true => Ok(name.to_string()),
        false => Err(Error::Query(format!(
            "invalid {what} name: {}",
            json::to_string(&Value::Text(name.into()))
        ))),
    }
}

pub struct Spec {
    pub collection: String,
    pub key: Option<String>,
    pub select: Option<Vec<String>>,
    /// The filter as the subscription's query string takes it,
    /// `field=op.value`.
    pub params: Vec<(String, String)>,
}

/// `{collection, where?, select?, key?}`.
pub fn spec(v: &Value) -> Result<Spec> {
    let collection = match member(v, "collection") {
        Some(Value::Text(c)) => ident(c, "collection")?,
        _ => {
            return Err(Error::Query(
                "a shape must be `{ collection, where?, select?, key? }`".into(),
            ))
        }
    };
    let key = match member(v, "key") {
        None | Some(Value::Null) => None,
        Some(Value::Text(k)) => Some(ident(k, "field")?),
        Some(_) => return Err(Error::Query("a shape's `key` is a field's name".into())),
    };
    let select = match member(v, "select") {
        None | Some(Value::Null) => None,
        Some(Value::List(cols)) => {
            let mut out = vec!["id".to_string()];
            for c in cols {
                let Value::Text(c) = c else {
                    return Err(Error::Query("a shape's `select` lists fields".into()));
                };
                let c = ident(c, "field")?;
                if !out.contains(&c) {
                    out.push(c);
                }
            }
            Some(out)
        }
        Some(_) => return Err(Error::Query("a shape's `select` lists fields".into())),
    };
    let params = match member(v, "where") {
        None | Some(Value::Null) => Vec::new(),
        Some(w @ Value::Object(_)) => where_params(w)?,
        Some(_) => return Err(Error::Query("a shape condition must be an object".into())),
    };
    Ok(Spec {
        collection,
        key,
        select,
        params,
    })
}

/// The JS sync layer's operator names, to the REST surface's.
fn rest_op(k: &str) -> Option<&'static str> {
    Some(match k {
        "=" | "eq" => "eq",
        "!=" | "ne" | "neq" => "neq",
        "<" | "lt" => "lt",
        "<=" | "lte" | "le" => "lte",
        ">" | "gt" => "gt",
        ">=" | "gte" | "ge" => "gte",
        "~" | "like" | "contains" => "like",
        "has" => "has",
        "in" => "in",
        _ => return None,
    })
}

/// `{status: 'open', priority: {gte: 3}}` as `status=eq.open&priority=gte.3`:
/// no `or`, no call, the escaping the transport's and the types the
/// server's, as a shape is in JS.
fn where_params(w: &Value) -> Result<Vec<(String, String)>> {
    let Value::Object(fields) = w else {
        unreachable!()
    };
    let mut out = Vec::new();
    for (field, spec) in fields {
        let field = ident(field, "field")?;
        match spec {
            Value::Null => out.push((field, "is.null".into())),
            Value::Object(ops) => {
                for (k, v) in ops {
                    if k == "not" {
                        let inner = match v {
                            Value::Null => "is.null".to_string(),
                            Value::Object(o) if o.len() == 1 => op(&o[0].0, &o[0].1, &field)?,
                            Value::Object(_) => {
                                return Err(Error::Query("`not` takes a single condition".into()))
                            }
                            v => op("eq", v, &field)?,
                        };
                        out.push((field.clone(), format!("not.{inner}")));
                    } else {
                        out.push((field.clone(), op(k, v, &field)?));
                    }
                }
            }
            v => out.push((field.clone(), format!("eq.{}", scalar(v, &field)?))),
        }
    }
    Ok(out)
}

fn op(k: &str, v: &Value, field: &str) -> Result<String> {
    let op = rest_op(k)
        .ok_or_else(|| Error::Query(format!("unknown operator `{k}` in shape (field: {field})")))?;
    if op != "in" {
        return Ok(format!("{op}.{}", scalar(v, field)?));
    }
    let items: Vec<Value> = match v {
        Value::List(l) => l.clone(),
        Value::Vector(f) => f.iter().map(|x| Value::Float(*x as f64)).collect(),
        _ => Vec::new(),
    };
    if items.is_empty() {
        return Err(Error::Query(format!(
            "`in` expects a non-empty array (field: {field})"
        )));
    }
    let parts: Result<Vec<String>> = items.iter().map(|x| scalar(x, field)).collect();
    Ok(format!("in.({})", parts?.join(",")))
}

fn scalar(v: &Value, field: &str) -> Result<String> {
    match v {
        Value::Null => Err(Error::Query(format!(
            "a shape value cannot be empty (field: {field})"
        ))),
        Value::Text(s) => Ok(s.clone()),
        Value::Bool(_) | Value::Int(_) | Value::Float(_) => Ok(json::to_string(v)),
        _ => Err(Error::Query(format!(
            "a shape value must be a scalar (field: {field})"
        ))),
    }
}

/// Percent-encoding for a query string: everything but RFC 3986's
/// unreserved characters.
pub fn encode(s: &str, out: &mut String) {
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
}

/// `create collection if not exists` text from a schema of `GET
/// /collections`, field order, types, collations and indexes as the server
/// has them: a document is its values in field order, and a replica whose
/// fields drifted would read rows wrong. `@unique` is a plain hash, as on a
/// replica: a batch lands rows' last states one at a time, and a value
/// moved between two collided.
pub fn schema_ddl(schema: &Value) -> Result<String> {
    let name = match member(schema, "name") {
        Some(Value::Text(n)) => ident(n, "collection")?,
        _ => return Err(Error::Query("a schema with no name".into())),
    };
    let mut fields = Vec::new();
    if let Some(Value::List(list)) = member(schema, "fields") {
        for f in list {
            let (Some(Value::Text(n)), Some(Value::Text(ty))) =
                (member(f, "name"), member(f, "type"))
            else {
                return Err(Error::Query(format!(
                    "a field of `{name}` with no name or type"
                )));
            };
            let mut text = format!("{} {ty}", ident(n, "field")?);
            if let Some(Value::Text(c)) = member(f, "collate") {
                text.push_str(&format!(" collate {c}"));
            }
            if let Some(Value::Bool(true)) = member(f, "required") {
                text.push_str(" required");
            }
            if let Some(Value::Text(ix)) = member(f, "index") {
                let ix = if ix == "unique" { "hash" } else { ix };
                if ix != "none" {
                    text.push_str(&format!(" @{ix}"));
                }
            }
            fields.push(text);
        }
    }
    Ok(format!(
        "create collection if not exists {name} ({})",
        fields.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(src: &str) -> Vec<(String, String)> {
        spec(&json::parse_json(src).unwrap()).unwrap().params
    }

    #[test]
    fn a_shape_condition_reads_as_the_js_one() {
        assert_eq!(
            params(
                r#"{"collection":"t","where":{"status":"open","p":{"gte":3,"lt":9},"x":null,"y":{"not":{"in":[1,2]}}}}"#
            ),
            [
                ("p".into(), "gte.3".into()),
                ("p".into(), "lt.9".into()),
                ("status".into(), "eq.open".into()),
                ("x".into(), "is.null".into()),
                ("y".into(), "not.in.(1,2)".into()),
            ]
        );
        for bad in [
            r#"{"collection":"t","where":{"a":{"nope":1}}}"#,
            r#"{"collection":"t","where":{"a":{"in":[]}}}"#,
            r#"{"collection":"t","where":{"a b":1}}"#,
            r#"{"collection":"t","where":{"a":{"eq":[1,"x"]}}}"#,
            r#"{"where":{}}"#,
        ] {
            assert!(spec(&json::parse_json(bad).unwrap()).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_server_schema_is_made_as_it_is() {
        let s = json::parse_json(
            r#"{"name":"t","fields":[{"name":"k","type":"text","index":"unique","required":true},{"name":"n","type":"text","index":null,"required":false,"collate":"tr"},{"name":"e","type":"vector<2>","index":"hnsw(cosine, m=16, ef_search=100)","required":false}]}"#,
        )
        .unwrap();
        let ddl = schema_ddl(&s).unwrap();
        assert_eq!(
            ddl,
            "create collection if not exists t (k text required @hash, n text collate tr, e vector<2> @hnsw(cosine, m=16, ef_search=100))"
        );
        fenec_ql::parse(&ddl).unwrap();
    }
}
