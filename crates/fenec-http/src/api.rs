//! A REST surface derived from the schema.
//!
//! Paths are collection names and the query string is the filter --
//! PostgREST's `?field=op.value` pattern. The translation always ends in a
//! [`Statement`]: the HTTP endpoint opens no separate execution path, it
//! arrives where FenecQL arrives.
//!
//! ```text
//! GET    /                       version + collection list
//! GET    /collections            schemas
//! GET    /<name>?select=a,b&year=gte.2024&order=year.desc&limit=10
//! GET    /<name>?...&count       number of matching rows
//! POST   /<name>                 body: {...} or [{...}]
//! PATCH  /<name>?<filter>        body: {...}
//! PATCH  /<name>/all             body: {...}   (unfiltered, deliberately)
//! DELETE /<name>?<filter>
//! DELETE /<name>/all
//! POST   /<name>/near            body: {"field":..,"vector":[..],"limit":..}
//! POST   /query                  govde: {"query":"<FenecQL>","params":[..]}
//! ```
//!
//! **Why is a vector a POST?** A 768-dimensional embedding does not fit in a
//! query string; squeezing it into a URL (base64, truncation) is both
//! unreadable and runs into URL ceilings in proxies and servers. Since
//! PostgREST's model does not cover fenecdb's main feature, `near` is its own
//! endpoint.

use crate::http::{Method, Request, Response};
use fenec_core::json;
use fenec_core::prelude::*;

/// Query keys that are read as clauses rather than as filters. A field with
/// the same name cannot be filtered over HTTP (the FenecQL and `fenec-pg` paths
/// are unaffected).
const RESERVED: [&str; 8] = [
    "select", "order", "limit", "offset", "count", "where", "lookup", "group",
];

/// On the subscription endpoint `since` is a clause as well. It is a
/// separate list so that a field named `since` stays filterable on the
/// **other** endpoints.
const RESERVED_STREAM: [&str; 8] = [
    "select", "order", "limit", "offset", "count", "where", "since", "lookup",
];

pub struct Routed {
    pub statement: Statement,
    /// The shape of the response body becomes clear during translation.
    pub shape: Shape,
}

pub enum Shape {
    /// Row array: `[{...}, {...}]`
    Rows,
    /// `{"count": N}`
    Count,
    /// `{"<name>": N}` -- write counter
    Affected(&'static str, u16),
    /// `{"fenecdb": "...", "collections": [...]}`
    Info,
    /// Schema list
    Schemas,
}

/// Turns a request into a statement. It only consults the database for schemas.
pub fn route(db: &Database, req: &Request) -> Result<Routed> {
    let seg = req.segments();
    match (req.method, seg.as_slice()) {
        (Method::Get | Method::Head, []) => Ok(Routed {
            statement: Statement::ListCollections,
            shape: Shape::Info,
        }),
        (Method::Get | Method::Head, ["collections"]) => Ok(Routed {
            statement: Statement::ListCollections,
            shape: Shape::Schemas,
        }),
        (Method::Get | Method::Head, [name]) => {
            let schema = &db.collection(name)?.schema;
            let sel = select_from_query(db, schema, req)?;
            let shape = if sel.count { Shape::Count } else { Shape::Rows };
            Ok(Routed {
                statement: Statement::Select(sel),
                shape,
            })
        }
        (Method::Post, [name, "near"]) => {
            let schema = &db.collection(name)?.schema;
            Ok(Routed {
                statement: Statement::Select(near_from_body(schema, req)?),
                shape: Shape::Rows,
            })
        }
        (Method::Post, [name]) => {
            let schema = &db.collection(name)?.schema;
            Ok(Routed {
                statement: put_from_body(schema, req)?,
                shape: Shape::Affected("inserted", 201),
            })
        }
        (Method::Patch | Method::Put, [name]) | (Method::Patch | Method::Put, [name, "all"]) => {
            let schema = &db.collection(name)?.schema;
            let all = seg.len() == 2;
            let filter = require_filter(schema, req, all, "an update")?;
            let set = document(schema, req)?;
            if set.is_empty() {
                return Err(Error::Query("empty body: no field to update".into()));
            }
            Ok(Routed {
                statement: Statement::Update {
                    collection: name.to_string(),
                    set,
                    filter,
                },
                shape: Shape::Affected("updated", 200),
            })
        }
        (Method::Delete, [name]) | (Method::Delete, [name, "all"]) => {
            let schema = &db.collection(name)?.schema;
            let all = seg.len() == 2;
            let filter = require_filter(schema, req, all, "a delete")?;
            Ok(Routed {
                statement: Statement::Delete {
                    collection: name.to_string(),
                    filter,
                },
                shape: Shape::Affected("deleted", 200),
            })
        }
        _ => Err(Error::NotFound(format!("path `{}`", req.path))),
    }
}

/// An unfiltered `PATCH`/`DELETE` covers the whole collection. It is very
/// easy to do by accident and impossible to undo: we require an explicit
/// path (`/<name>/all`). It is a separate *path* because a key like
/// `?all=true` would clash with a field named `all`.
fn require_filter(schema: &Schema, req: &Request, all: bool, verb: &str) -> Result<Option<Expr>> {
    let filter = filter_from_query(schema, req)?;
    match (&filter, all) {
        (Some(_), true) => Err(Error::Query(format!(
            "`/all` takes no filter: either give a filter or use `/all` ({verb})"
        ))),
        (None, false) => Err(Error::Query(format!(
            "{verb} without a filter covers the whole collection; if deliberate, `/{}/all`",
            schema.name
        ))),
        _ => Ok(filter),
    }
}

// --------------------------------------------------------------- reading

fn select_from_query(db: &Database, schema: &Schema, req: &Request) -> Result<Select> {
    let lookup = lookup_from_query(db, schema, req)?;
    // Every level's prefix is reserved, not just the first: a chained key
    // read as a condition on the parent would be a silent wrong answer.
    let skip: Vec<String> = lookup
        .iter()
        .flat_map(|l| l.chain())
        .map(|s| format!("{}.", s.collection))
        .collect();
    let mut sel = Select {
        collection: schema.name.clone(),
        filter: filter_with(schema, req, &RESERVED, &skip)?,
        lookup,
        ..Default::default()
    };
    for (k, v) in &req.query {
        match k.as_str() {
            // `select=status,sum(total),count(*)`: a list with an aggregate
            // in it aggregates, as FenecQL's does.
            "select" if v.contains('(') => sel.aggregate = aggregates(schema, v)?,
            "select" => sel.project = Some(projection(schema, v)?),
            "group" => sel.group = Some(field_of(schema, v)?),
            "order" => sel.order = order(schema, v)?,
            "limit" => sel.limit = Some(number(v, "limit")?),
            "offset" => sel.offset = number(v, "offset")?,
            "count" => sel.count = truthy(v),
            _ => {}
        }
    }
    sel.check()?;
    Ok(sel)
}

/// `?lookup=reviews&reviews.on=product_id&reviews.limit=3&reviews.stars=gte.4`
///
/// The collection is named once and everything prefixed with it configures
/// the clause -- `on`, `parent`, `select`, `order`, `limit`, `offset`,
/// `required`, `where`, and any field name as a condition. Prefixing keeps
/// the two sides apart in a query string the way the clause's position does
/// in FenecQL, and it is the shape PostgREST already uses for an embedded
/// resource's filters.
///
/// `?lookup=orders,lines&orders.on=shop_id&lines.on=order_id` chains: the
/// list is read left to right, each name binding to the one before it, and
/// each level keeps its own prefix. A comma-separated list is the same
/// ordering FenecQL gets from position, written where a query string has no
/// position to use.
fn lookup_from_query(db: &Database, parent: &Schema, req: &Request) -> Result<Option<Lookup>> {
    let Some((_, spec)) = req.query.iter().find(|(k, _)| k == "lookup") else {
        return Ok(None);
    };
    let names: Vec<&str> = spec
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .collect();
    if names.is_empty() {
        return Err(Error::Query("`lookup` is empty".into()));
    }
    // Built innermost-last, then linked from the back, so each level owns the
    // one below it.
    let mut levels = Vec::with_capacity(names.len());
    let mut above = parent;
    for name in &names {
        let child = &db.collection(name)?.schema;
        levels.push(lookup_level(name, above, child, req)?);
        above = child;
    }
    let mut chain: Option<Box<Lookup>> = None;
    for mut l in levels.into_iter().rev() {
        l.next = chain;
        chain = Some(Box::new(l));
    }
    Ok(chain.map(|b| *b))
}

/// One level of the chain: every key prefixed with its collection's name.
fn lookup_level(name: &str, parent: &Schema, child: &Schema, req: &Request) -> Result<Lookup> {
    let prefix = format!("{name}.");
    let mut l = Lookup {
        collection: name.to_string(),
        parent_field: "id".to_string(),
        ..Default::default()
    };
    let mut parts: Vec<Expr> = Vec::new();
    for (key, raw) in &req.query {
        let Some(k) = key.strip_prefix(&prefix) else {
            continue;
        };
        match k {
            "on" => l.child_field = field_of(child, raw)?,
            "parent" => l.parent_field = field_of(parent, raw)?,
            "select" => l.project = Some(projection(child, raw)?),
            "order" => l.order = order(child, raw)?,
            "limit" => l.limit = Some(number(raw, "limit")?),
            "offset" => l.offset = number(raw, "offset")?,
            "required" => l.required = truthy(raw),
            "where" => parts.push(parse_expr(name, raw)?),
            other => parts.push(condition(child, other, raw)?),
        }
    }
    if l.child_field.is_empty() {
        return Err(Error::Query(format!(
            "`lookup={name}` needs `{name}.on=<field>`: the child field holding the key"
        )));
    }
    l.filter = parts
        .into_iter()
        .reduce(|a, b| Expr::And(Box::new(a), Box::new(b)));
    Ok(l)
}

/// A field name from the query string, checked against the schema so an
/// unknown one is an error here rather than at execution. `id` is not a
/// declared field but is a legal key on either side.
fn field_of(schema: &Schema, raw: &str) -> Result<String> {
    let name = raw.trim();
    if name == "id" || schema.field(name).is_some() {
        return Ok(name.to_string());
    }
    Err(Error::NotFound(format!("field `{}.{name}`", schema.name)))
}

fn projection(schema: &Schema, raw: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for part in raw.split(',') {
        let name = part.trim();
        if name.is_empty() {
            continue;
        }
        field(schema, name)?;
        out.push(name.to_string());
    }
    if out.is_empty() {
        return Err(Error::Query("`select` is empty".into()));
    }
    Ok(out)
}

/// `select=status,sum(total),count(*)` -> the fields and aggregates, in
/// order. The list is FenecQL's, parsed by FenecQL's parser.
fn aggregates(schema: &Schema, raw: &str) -> Result<Vec<Agg>> {
    let (_, list) =
        fenec_ql::parse_select_list(raw).map_err(|e| Error::Query(format!("`select`: {e}")))?;
    for f in list.iter().filter_map(Agg::field) {
        field(schema, f)?;
    }
    Ok(list)
}

/// `order=year.desc,title` -> `order year desc, title`. A collation's name
/// among the modifiers puts a text field in its language's order:
/// `order=name.tr.desc` is `order name collate tr desc`. Over groups a key
/// may name an aggregate of the list, `order=sum(total).desc`; the engine
/// checks it against the list.
fn order(schema: &Schema, raw: &str) -> Result<Vec<Sort>> {
    let mut out = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // No name holds a dot -- an aggregate's parentheses neither -- so
        // everything after the first one is a modifier.
        let mut words = part.split('.');
        let name = words.next().unwrap_or_default();
        let (mut asc, mut collate) = (true, None);
        for w in words {
            match w {
                "asc" => asc = true,
                "desc" => asc = false,
                w => {
                    collate = Some(Collation::named(w).ok_or_else(|| {
                        Error::Query(format!(
                            "`order`: `{w}` in `{part}` is not asc, desc or a collation (tr)"
                        ))
                    })?)
                }
            }
        }
        let name = if let Some((f, rest)) = name.split_once('(') {
            // The function's name folds as FenecQL folds it; the field's does not.
            match (f.to_ascii_lowercase().as_str(), rest) {
                ("count", "*)" | ")") => "count".to_string(),
                (f, rest) => format!("{f}({rest}"),
            }
        } else {
            field(schema, name)?;
            name.to_string()
        };
        out.push(Sort {
            field: name,
            asc,
            collate,
        });
    }
    Ok(out)
}

fn number(raw: &str, what: &str) -> Result<usize> {
    raw.trim()
        .parse()
        .map_err(|_| Error::Query(format!("`{what}` expects a number, got `{raw}`")))
}

/// `?count`, `?count=true`, `?count=1` -- all of them mean on.
fn truthy(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "" | "true" | "1" | "yes" | "exact"
    )
}

// ---------------------------------------------------------------- filter

/// Joins every condition in the query string with `and`.
fn filter_from_query(schema: &Schema, req: &Request) -> Result<Option<Expr>> {
    filter_with(schema, req, &RESERVED, &[])
}

/// `skip` holds each `lookup` level's `<collection>.` prefix: those keys
/// configure a child and must not be read as conditions on the parent.
fn filter_with(
    schema: &Schema,
    req: &Request,
    reserved: &[&str],
    skip: &[String],
) -> Result<Option<Expr>> {
    let mut parts: Vec<Expr> = Vec::new();
    for (key, raw) in &req.query {
        if skip.iter().any(|p| key.starts_with(p)) {
            continue;
        }
        if reserved.contains(&key.as_str()) {
            if key == "where" {
                parts.push(parse_expr(&schema.name, raw)?);
            }
            continue;
        }
        parts.push(condition(schema, key, raw)?);
    }
    Ok(parts
        .into_iter()
        .reduce(|a, b| Expr::And(Box::new(a), Box::new(b))))
}

/// `where=` is a free FenecQL expression, so that conditions which do not fit
/// the query-string pattern (function calls, `or` groups) can be expressed
/// too. Only the condition part is taken; no other clause is accepted.
fn parse_expr(collection: &str, raw: &str) -> Result<Expr> {
    let stmt = fenec_ql::parse_one(&format!("get {collection} where {raw}"))
        .map_err(|e| Error::Query(format!("`where` could not be parsed: {e}")))?;
    let Statement::Select(sel) = stmt else {
        return Err(Error::Query(
            "`where` must be a condition expression".into(),
        ));
    };
    if sel.project.is_some()
        || sel.near.is_some()
        || !sel.order.is_empty()
        || sel.limit.is_some()
        || sel.offset != 0
        || sel.count
    {
        return Err(Error::Query(
            "`where` takes only a condition, no clauses".into(),
        ));
    }
    sel.filter
        .ok_or_else(|| Error::Query("`where` is empty".into()))
}

/// `field=op.value` -- when the op is not recognised the whole value counts
/// as an equality. (Someone writing `?name=v1.2` is not looking for a `v1`
/// operator.)
fn condition(schema: &Schema, name: &str, raw: &str) -> Result<Expr> {
    let ty = field(schema, name)?;
    let (op, rest) = split_op(raw);
    if op == "not" {
        let inner = condition(schema, name, rest)?;
        return Ok(Expr::Not(Box::new(inner)));
    }
    let f = || Box::new(Expr::Field(name.to_string()));

    if op == "is" {
        return match rest.trim().to_ascii_lowercase().as_str() {
            "null" => Ok(Expr::IsNull(f())),
            other => Err(Error::Query(format!(
                "`is` only takes `null`, got `{other}`"
            ))),
        };
    }

    if matches!(ty, DataType::List(_)) && !matches!(op, "has" | "in") {
        return Err(Error::Query(format!(
            "`{name}` is a list: use `{name}=has.<value>` to look for an element"
        )));
    }

    Ok(match op {
        "eq" => Expr::Cmp(CmpOp::Eq, f(), boxed(lit(rest, ty, name)?)),
        "neq" | "ne" => Expr::Cmp(CmpOp::Ne, f(), boxed(lit(rest, ty, name)?)),
        "lt" => Expr::Cmp(CmpOp::Lt, f(), boxed(lit(rest, ty, name)?)),
        "lte" | "le" => Expr::Cmp(CmpOp::Le, f(), boxed(lit(rest, ty, name)?)),
        "gt" => Expr::Cmp(CmpOp::Gt, f(), boxed(lit(rest, ty, name)?)),
        "gte" | "ge" => Expr::Cmp(CmpOp::Ge, f(), boxed(lit(rest, ty, name)?)),
        "like" => Expr::Like(f(), Box::new(Expr::Lit(Value::Text(rest.to_string())))),
        "has" => Expr::Has(f(), boxed(lit(rest, ty, name)?)),
        "in" => {
            let items = rest.trim().trim_start_matches('(').trim_end_matches(')');
            let mut out = Vec::new();
            for item in items.split(',') {
                out.push(Expr::Lit(lit(item.trim(), ty, name)?));
            }
            if out.is_empty() {
                return Err(Error::Query(format!("`{name}=in.` empty list")));
            }
            Expr::In(f(), out)
        }
        other => {
            return Err(Error::Query(format!(
                "unknown operator `{other}` (field: {name})"
            )))
        }
    })
}

fn boxed(v: Value) -> Box<Expr> {
    Box::new(Expr::Lit(v))
}

const OPS: [&str; 13] = [
    "eq", "neq", "ne", "lt", "lte", "le", "gt", "gte", "ge", "like", "has", "in", "is",
];

/// If the part up to the first dot is a recognised operator, it is split
/// off; otherwise the operator is `eq` and the whole value is preserved.
fn split_op(raw: &str) -> (&str, &str) {
    match raw.split_once('.') {
        Some(("not", rest)) => ("not", rest),
        Some((head, rest)) if OPS.contains(&head) => (head, rest),
        _ => ("eq", raw),
    }
}

/// The query string is always text: the value is parsed according to the
/// field's type. `coerce` does not go from text to number (and must not --
/// we want no silent conversion on the write path), so the parse is
/// explicit here.
fn lit(raw: &str, ty: &DataType, name: &str) -> Result<Value> {
    let bad = |what: &str| Error::Query(format!("`{name}` expects {what}, got `{raw}`"));
    let v = match ty {
        DataType::Bool => match raw.to_ascii_lowercase().as_str() {
            "true" | "t" | "1" => Value::Bool(true),
            "false" | "f" | "0" => Value::Bool(false),
            _ => return Err(bad("a bool")),
        },
        DataType::Int => Value::Int(raw.parse().map_err(|_| bad("an integer"))?),
        // `num::parse_f64`, the same parser the JSON body and FenecQL use,
        // rather than `str::parse`. It is stricter by exactly one thing --
        // it has no spelling for infinity -- which makes the query string
        // agree with the JSON body, where `inf` was never a number either.
        // It also keeps `core`'s 12 KB table of powers of five out of every
        // binary that links this crate, `fenec-pg` included.
        DataType::Float => {
            Value::Float(fenec_core::num::parse_f64(raw).ok_or_else(|| bad("a number"))?)
        }
        DataType::Text => Value::Text(raw.to_string()),
        DataType::Bytes => Value::Bytes(raw.as_bytes().to_vec()),
        // Both ISO-8601 and epoch milliseconds are accepted.
        DataType::Timestamp => match raw.parse::<i64>() {
            Ok(ms) => Value::Timestamp(ms),
            Err(_) => Value::Text(raw.to_string()).coerce(&DataType::Timestamp)?,
        },
        DataType::List(inner) => lit(raw, inner, name)?,
        DataType::Vector(..) | DataType::Sparse(_) => {
            return Err(Error::Query(format!(
                "`{name}` is a vector: it cannot be filtered in the query string, use `POST /<collection>/near`"
            )))
        }
    };
    Ok(v)
}

/// Field type; `id` is not in the schema but is queryable.
fn field<'a>(schema: &'a Schema, name: &str) -> Result<&'a DataType> {
    if name == "id" {
        return Ok(&DataType::Int);
    }
    schema
        .field(name)
        .map(|f| &f.ty)
        .ok_or_else(|| Error::NotFound(format!("field `{name}` in collection `{}`", schema.name)))
}

// --------------------------------------------------------------- writing

fn body_str(req: &Request) -> Result<&str> {
    std::str::from_utf8(&req.body).map_err(|_| Error::Query("the body is not UTF-8".into()))
}

fn put_from_body(schema: &Schema, req: &Request) -> Result<Statement> {
    let docs = json::parse_documents(body_str(req)?)?;
    if docs.is_empty() {
        return Err(Error::Query("empty body: no document to write".into()));
    }
    let mut out = Vec::with_capacity(docs.len());
    for doc in docs {
        out.push(check_fields(schema, doc)?);
    }
    Ok(Statement::Put {
        collection: schema.name.clone(),
        docs: out,
    })
}

fn document(schema: &Schema, req: &Request) -> Result<Vec<(String, Expr)>> {
    check_fields(schema, json::parse_object(body_str(req)?)?)
}

/// Field names are validated against the schema: the engine validates them
/// too, but the error has to be a 400 here rather than a 404, and the
/// message has to carry the field name.
fn check_fields(schema: &Schema, doc: Vec<(String, Value)>) -> Result<Vec<(String, Expr)>> {
    let mut out = Vec::with_capacity(doc.len());
    for (k, v) in doc {
        if k != "id" && schema.field(&k).is_none() {
            return Err(Error::Query(format!(
                "collection `{}` has no field `{k}`",
                schema.name
            )));
        }
        out.push((k, Expr::Lit(v)));
    }
    Ok(out)
}

// ------------------------------------------------------------------ near

fn near_from_body(schema: &Schema, req: &Request) -> Result<Select> {
    let body = json::parse_object(body_str(req)?)?;
    let get = |name: &str| body.iter().find(|(k, _)| k == name).map(|(_, v)| v);

    let field_name = match get("field") {
        Some(Value::Text(s)) => s.clone(),
        Some(_) => return Err(Error::Query("`field` must be text".into())),
        None => default_vector_field(schema)?,
    };
    let sparse = match field(schema, &field_name)? {
        DataType::Vector(..) => false,
        DataType::Sparse(_) => true,
        other => {
            return Err(Error::Query(format!(
                "`{field_name}` is not a vector ({})",
                other.name()
            )))
        }
    };

    let vector = match get("vector") {
        Some(Value::Vector(v)) if !sparse => Value::Vector(v.clone()),
        // A sparse field's is pgvector's text form: `{1:0.5,3:0.25}/30522`.
        Some(Value::Text(t)) if sparse => Value::Text(t.clone()),
        Some(Value::List(items)) if items.is_empty() => {
            return Err(Error::Query("`vector` is empty".into()))
        }
        Some(_) if sparse => {
            return Err(Error::Query(
                "`vector` must be a sparse vector as text: {index:value,...}/dimension".into(),
            ))
        }
        Some(_) => return Err(Error::Query("`vector` must be an array of numbers".into())),
        None => return Err(Error::Query("`vector` is required".into())),
    };

    let as_usize = |name: &str| -> Result<Option<usize>> {
        match get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Int(i)) if *i >= 0 => Ok(Some(*i as usize)),
            Some(_) => Err(Error::Query(format!(
                "`{name}` must be a non-negative integer"
            ))),
        }
    };

    let mut sel = Select {
        collection: schema.name.clone(),
        near: Some(Near {
            field: field_name,
            vector: Expr::Lit(vector),
            ef: as_usize("ef")?,
            exact: matches!(get("exact"), Some(Value::Bool(true))),
        }),
        limit: as_usize("limit")?,
        offset: as_usize("offset")?.unwrap_or(0),
        ..Default::default()
    };

    // `"match": "words"` makes it hybrid: BM25 over the text field ranks
    // too, and the two rankings are fused. Flat keys, since a JSON body
    // here holds no nested objects.
    match get("match") {
        None | Some(Value::Null) => {}
        Some(Value::Text(q)) => {
            let text_field = match get("match_field") {
                Some(Value::Text(f)) => f.clone(),
                Some(_) => return Err(Error::Query("`match_field` must be text".into())),
                None => default_text_field(schema)?,
            };
            sel.matcher = Some(Match {
                field: text_field,
                query: Expr::Lit(Value::Text(q.clone())),
            });
            sel.fuse = Some(Fuse {
                k: as_usize("fuse_k")?.map(|k| k.min(u32::MAX as usize) as u32),
                candidates: as_usize("candidates")?,
            });
        }
        Some(_) => return Err(Error::Query("`match` must be text".into())),
    }

    if let Some(v) = get("select") {
        let cols = match v {
            Value::List(items) => items
                .iter()
                .map(|i| match i {
                    Value::Text(s) => Ok(s.clone()),
                    _ => Err(Error::Query("`select` must be an array of text".into())),
                })
                .collect::<Result<Vec<_>>>()?,
            Value::Text(s) => vec![s.clone()],
            _ => return Err(Error::Query("`select` must be an array of text".into())),
        };
        for c in &cols {
            field(schema, c)?;
        }
        sel.project = Some(cols);
    }

    // The filter can come from two places: the `where` expression in the
    // body and the `?field=op.value` pairs in the query string. When both
    // are present they are `and`ed.
    let mut filter = filter_from_query(schema, req)?;
    if let Some(Value::Text(expr)) = get("where") {
        let parsed = parse_expr(&schema.name, expr)?;
        filter = Some(match filter {
            Some(f) => Expr::And(Box::new(f), Box::new(parsed)),
            None => parsed,
        });
    }
    sel.filter = filter;
    Ok(sel)
}

/// When the collection has a single `@text` field there is no need to write
/// `match_field`.
fn default_text_field(schema: &Schema) -> Result<String> {
    let mut texts = schema
        .fields
        .iter()
        .filter(|f| matches!(f.index, IndexKind::Text(_)));
    match (texts.next(), texts.next()) {
        (Some(f), None) => Ok(f.name.clone()),
        (None, _) => Err(Error::Query(format!(
            "`{}` has no @text field to match",
            schema.name
        ))),
        (Some(_), Some(_)) => Err(Error::Query(
            "the collection has several @text fields: name one with `match_field`".into(),
        )),
    }
}

/// When the collection has a single vector field there is no need to write `field`.
fn default_vector_field(schema: &Schema) -> Result<String> {
    let mut found = None;
    for f in &schema.fields {
        if matches!(f.ty, DataType::Vector(..)) {
            if found.is_some() {
                return Err(Error::Query(
                    "there is more than one vector field: specify `field`".into(),
                ));
            }
            found = Some(f.name.clone());
        }
    }
    found.ok_or_else(|| Error::Query(format!("collection `{}` has no vector field", schema.name)))
}

// ---------------------------------------------------------- subscription

/// Parsing of `GET /<name>/changes`.
///
/// The shape is a **set**, not a window: `order`, `limit`, `offset` and
/// `count` are deliberately rejected. A subscription like "the last 100
/// rows" looks right but is not -- when a new row arrives the oldest has to
/// drop out, and an incremental diff cannot say that. An explicit error
/// beats a silently wrong stream.
pub struct Subscription {
    pub collection: String,
    pub filter: Option<Expr>,
    pub project: Option<Vec<String>>,
    /// Absent: seed from scratch. Present: continue from this point.
    pub since: Option<u64>,
}

pub fn subscription(db: &Database, req: &Request) -> Result<Subscription> {
    let seg = req.segments();
    let [name, "changes"] = seg.as_slice() else {
        return Err(Error::NotFound(format!("path `{}`", req.path)));
    };
    let schema = &db.collection(name)?.schema;

    let mut since = None;
    let mut project = None;
    for (k, v) in &req.query {
        match k.as_str() {
            "since" => {
                since =
                    Some(v.trim().parse::<u64>().map_err(|_| {
                        Error::Query(format!("`since` expects a number, got `{v}`"))
                    })?)
            }
            "select" => {
                // The id is always carried: deletions are applied by id, and
                // the projection cannot leave it out.
                let mut cols = projection(schema, v)?;
                if !cols.iter().any(|c| c == "id") {
                    cols.insert(0, "id".to_string());
                }
                project = Some(cols);
            }
            "order" | "limit" | "offset" | "count" => {
                return Err(Error::Query(format!(
                    "`{k}` cannot be used on a subscription: the shape is a set, not a window"
                )))
            }
            _ => {}
        }
    }

    Ok(Subscription {
        collection: name.to_string(),
        filter: filter_with(schema, req, &RESERVED_STREAM, &[])?,
        project,
        since,
    })
}

/// The seed query: the **whole** shape, with the same filter and projection.
pub fn seed_select(sub: &Subscription) -> Select {
    Select {
        collection: sub.collection.clone(),
        filter: sub.filter.clone(),
        project: sub.project.clone(),
        ..Default::default()
    }
}

// ----------------------------------------------------------- raw query

/// The body of `POST /query`: `{"query": "...", "params": [...]}`.
///
/// The REST surface is derived from the schema and is deliberately narrow
/// (no DDL, no `or` groups). Raw input lifts that wall: so the query builder
/// in the browser can hand the same text to wasm and to this endpoint alike
/// -- one piece of query code, two transports.
pub fn parse_query(body: &str) -> Result<(Statement, Vec<Value>)> {
    let obj = json::parse_object(body)?;
    let get = |name: &str| obj.iter().find(|(k, _)| k == name).map(|(_, v)| v);
    let sql = match get("query").or_else(|| get("sql")) {
        Some(Value::Text(s)) => s.clone(),
        Some(_) => return Err(Error::Query("`query` must be text".into())),
        None => return Err(Error::Query("`query` is required".into())),
    };
    let params = match get("params") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::List(items)) => items.clone(),
        // An all-numeric array is parsed as a vector on the JSON side; it is
        // unpacked back into a parameter list (a single vector parameter is `[[..]]`).
        Some(Value::Vector(v)) => v.iter().map(|f| Value::Float(*f as f64)).collect(),
        Some(other) => vec![other.clone()],
    };
    let stmt = fenec_ql::parse_one(&sql).map_err(|e| Error::Query(e.to_string()))?;
    Ok((stmt, params))
}

/// The body of `POST /batch`: **one per line**, each a `POST /query` body
/// (NDJSON).
///
/// ```text
/// {"query":"put tasks {title: $1}","params":["a"]}
/// {"query":"set tasks {status: $1} where id = $2","params":["open", 7]}
/// ```
///
/// Why NDJSON: fenecdb's JSON parser does not accept nested objects
/// (deliberately -- see Limits), so `{"statements":[{...}]}` could not be
/// parsed anyway. Rather than writing a second parser, the boundary is put
/// at the end of the line: every line is *exactly* the body of the single
/// query endpoint and goes down that same path. Escaping rules mean no JSON
/// encoder can write a bare `\n` into the body, so the split is unambiguous.
///
/// **It is not a transaction.** fenecdb has no transactions (single-writer
/// model) and this endpoint does not invent one: the statements run in
/// order, **under a single write lock**, and stop at the first error. The
/// gain is not atomicity but two things: one round trip instead of N, and no
/// other writer slipping in between. On an error the response says how many
/// were applied -- nothing is rolled back, because nothing can be.
pub fn parse_batch(body: &str) -> Result<Vec<(Statement, Vec<Value>)>> {
    let mut out = Vec::new();
    for (i, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(
            parse_query(line).map_err(|e| Error::Query(format!("batch line {}: {e}", i + 1)))?,
        );
    }
    if out.is_empty() {
        return Err(Error::Query("empty batch".into()));
    }
    Ok(out)
}

/// A single item of the batch response.
fn result_json(out: &mut String, resp: &Response2) {
    match resp {
        Response2::Rows(rs) => {
            out.push_str("{\"rows\":");
            out.push_str(&rows_json(rs));
            out.push('}');
        }
        Response2::Affected(n) => out.push_str(&format!("{{\"affected\":{n}}}")),
        Response2::Ok(msg) => {
            out.push_str("{\"message\":");
            json::escape_into(out, msg);
            out.push('}');
        }
        Response2::Schemas(list) => {
            out.push_str("{\"collections\":");
            out.push_str(&schemas_json(list));
            out.push('}');
        }
    }
}

/// `{"ok":N,"results":[...]}`
pub fn render_batch(results: &[Response2], version: &str) -> Response {
    let mut out = format!("{{\"ok\":{},\"results\":[", results.len());
    for (i, r) in results.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        result_json(&mut out, r);
    }
    out.push_str("]}");
    Response::json(200, out).header("X-Fenecdb-Version", version)
}

/// A batch that stopped halfway: we **have to say** how many were applied.
/// A silent error would permanently separate the client's optimistic local
/// state from the server.
pub fn render_batch_error(e: &Error, completed: usize, version: &str) -> Response {
    render_batch_stop(status_of(e), &e.to_string(), completed, version)
}

/// A batch stopped at statement `completed` with `status` and `why`: an
/// error, or a write the data ceiling refused.
pub fn render_batch_stop(status: u16, why: &str, completed: usize, version: &str) -> Response {
    let mut out = String::from("{\"error\":");
    json::escape_into(&mut out, why);
    out.push_str(&format!(",\"completed\":{completed}}}"));
    Response::json(status, out).header("X-Fenecdb-Version", version)
}

/// The JSON form of a raw query response: the shape follows the statement.
pub fn render_any(resp: &Response2, version: &str) -> Response {
    match resp {
        Response2::Rows(rs) => Response::json(200, rows_json(rs)),
        Response2::Affected(n) => Response::json(200, format!("{{\"affected\":{n}}}")),
        Response2::Ok(msg) => {
            let mut out = String::from("{\"message\":");
            json::escape_into(&mut out, msg);
            out.push('}');
            Response::json(200, out)
        }
        Response2::Schemas(list) => Response::json(200, schemas_json(list)),
    }
    .header("X-Fenecdb-Version", version)
}

// --------------------------------------------------------------- response

pub fn render(resp: &Response2, shape: &Shape, version: &str) -> Response {
    match (shape, resp) {
        (Shape::Rows, Response2::Rows(rs)) => Response::json(200, rows_json(rs)),
        (Shape::Count, Response2::Rows(rs)) => {
            let n = rs
                .rows
                .first()
                .and_then(|r| r.values.first())
                .map(json::to_string)
                .unwrap_or_else(|| "0".into());
            Response::json(200, format!("{{\"count\":{n}}}"))
        }
        (Shape::Affected(name, status), Response2::Affected(n)) => {
            Response::json(*status, format!("{{\"{name}\":{n}}}"))
        }
        (Shape::Info, Response2::Schemas(list)) => {
            let mut out = String::from("{\"fenecdb\":");
            json::escape_into(&mut out, version);
            out.push_str(",\"collections\":[");
            for (i, s) in list.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json::escape_into(&mut out, &s.name);
            }
            out.push_str("]}");
            Response::json(200, out)
        }
        (Shape::Schemas, Response2::Schemas(list)) => Response::json(200, schemas_json(list)),
        // If the engine did not return what was expected, this is a programming bug.
        _ => Response::error(500, "unexpected response shape"),
    }
}

/// `fenec_core::query::Response` -- renamed so it does not clash with `http::Response`.
pub use fenec_core::query::Response as Response2;

pub fn rows_json(rs: &ResultSet) -> String {
    let mut out = String::new();
    json::rows_array_into(&mut out, rs);
    out
}

fn schemas_json(list: &[Schema]) -> String {
    let mut out = String::from("[");
    for (i, s) in list.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        json::escape_into(&mut out, &s.name);
        out.push_str(",\"fields\":[");
        for (j, f) in s.fields.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            json::escape_into(&mut out, &f.name);
            out.push_str(",\"type\":");
            json::escape_into(&mut out, &f.ty.name());
            out.push_str(",\"index\":");
            match &f.index {
                IndexKind::None => out.push_str("null"),
                IndexKind::Hash => json::escape_into(&mut out, "hash"),
                IndexKind::Sorted => json::escape_into(&mut out, "sorted"),
                IndexKind::Vector(spec) => json::escape_into(
                    &mut out,
                    &format!(
                        "hnsw({}, m={}, ef_search={}{})",
                        spec.metric.name(),
                        spec.m,
                        spec.ef_search,
                        spec.quant_arg()
                    ),
                ),
                IndexKind::Text(spec) => {
                    json::escape_into(&mut out, &format!("text(k1={}, b={})", spec.k1(), spec.b()))
                }
                IndexKind::Inverted => json::escape_into(&mut out, "inverted"),
            }
            out.push_str(",\"required\":");
            out.push_str(if f.required { "true" } else { "false" });
            // Only where there is one, so a schema without stays as it was.
            if let Some(c) = f.collate {
                out.push_str(",\"collate\":");
                json::escape_into(&mut out, c.name());
            }
            out.push('}');
        }
        out.push_str("]}");
    }
    out.push(']');
    out
}

/// Turns an engine error into an HTTP status.
pub fn status_of(e: &Error) -> u16 {
    match e {
        Error::NotFound(_) => 404,
        Error::Exists(_) => 409,
        Error::Type(_) | Error::Query(_) => 400,
        Error::Corrupt(_) | Error::Io(_) | Error::Plugin(_) => 500,
        // As `--http-read-only` answers: the write is not this server's to take.
        Error::ReadOnly(_) | Error::Denied(_) => 403,
    }
}
