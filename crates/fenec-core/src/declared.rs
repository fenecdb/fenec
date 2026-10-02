//! A schema declared in code: the description every SDK's declarations
//! compile to, and the plan that brings a database to it.
//!
//! An app that makes its own database declares its collections in its own
//! language -- `fenecTable(...)` in TypeScript, and the other SDKs in theirs
//! -- and each declaration compiles to one JSON description, written down
//! here, so that the comparison with the database and what it leads to is
//! written once rather than once a language:
//!
//! ```text
//! {"format": 1,
//!  "collections": [
//!    {"name": "todos", "fields": [
//!      {"name": "title", "type": "text", "required": true, "collate": "tr",
//!       "index": {"kind": "text", "prefix": 6}},
//!      {"name": "meta", "type": "json",
//!       "paths": [{"path": "lang", "index": {"kind": "hash"}}]}]}],
//!  "migrations": ["alter collection todos rename field name to title",
//!                 {"rebuild": {"collection": "todos", "field": "done"}}]}
//! ```
//!
//! A description holds what a declaration says and no more: an option left
//! out takes the engine's default, so the defaults live here and in the
//! parser alone, not in every SDK. [`describe`] writes a database's schema
//! the same way, every option spelled out.
//!
//! [`plan`] compares a database's schemas with a description. What only
//! adds -- a collection, a field, an index on a field or a path that has
//! none -- comes back as the statements that make it; what would lose data
//! or could mean two things -- a field the code lacks (dropped, or renamed?),
//! a type, a collation or a `required` changed, an index changed or taken
//! off -- comes back as a refusal saying what differs and how to resolve it.
//! The resolution is a migration: FenecQL, run once and recorded in
//! `_migrations` by whoever applies the plan (`fenec-abi`), since running a
//! statement takes the parser, which this crate does not have.

use crate::collate::Collation;
use crate::error::{Error, Result};
use crate::json;
use crate::schema::{
    split_path, ttl_text, Field, IndexKind, Metric, Quant, Schema, TextIndexSpec, VectorIndexSpec,
};
use crate::value::{DataType, Value, VecPrec};

/// The description's format. A description says which it is written in, and
/// one of another is refused rather than read as this one.
pub const FORMAT: i64 = 1;

/// Where the migrations applied are recorded, one row each: its number,
/// its text and when it was applied.
pub const MIGRATIONS: &str = "_migrations";

/// The collection [`MIGRATIONS`] names, as it is made.
pub const MIGRATIONS_DDL: &str =
    "create collection if not exists _migrations (n int @unique, text text, at timestamp)";

/// A description read: the collections declared, and the migrations.
#[derive(Debug, Clone, PartialEq)]
pub struct Declared {
    pub collections: Vec<Schema>,
    /// The collections as FenecQL text (`"fenecql"`), for the caller that
    /// has the parser to read them into `collections`
    /// (`fenec_ql::schema_text`).
    pub text: Option<String>,
    pub migrations: Vec<Migration>,
}

/// One migration: run once, in order, and recorded.
#[derive(Debug, Clone, PartialEq)]
pub enum Migration {
    /// FenecQL statements, run as written.
    Text(String),
    /// A field made again as the description declares it -- its type,
    /// collation and index -- and its values copied over: what resolves a
    /// changed index or collation, which no statement changes in place.
    Rebuild { collection: String, field: String },
}

/// How [`plan`] reads a difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The code owns the schema: what it adds is applied, and the database
    /// holds nothing it does not declare.
    Apply,
    /// The database's owner is elsewhere -- a server a replica follows, or
    /// one a client reaches without the right to change it: nothing is
    /// applied, everything the code declares must be there as declared, and
    /// what the database holds beyond it is its owner's business.
    Follow,
}

/// What a database needs to hold what the code declares.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// FenecQL that adds what is missing, in order; empty under
    /// [`Mode::Follow`].
    pub statements: Vec<String>,
    /// What cannot be applied as it stands, each with its resolution.
    pub refusals: Vec<Refusal>,
}

/// A difference the plan does not apply.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    /// What kind: `field_not_declared`,
    /// `type_changed`, `required_changed`, `required_added`,
    /// `collate_changed`, `index_changed`, `index_removed`, and under
    /// [`Mode::Follow`] `collection_missing`, `field_missing`,
    /// `index_missing`.
    pub kind: &'static str,
    pub collection: String,
    /// The field, or the path into a json field (`meta.lang`); `None` for
    /// the collection itself.
    pub field: Option<String>,
    /// What differs.
    pub message: String,
    /// How to resolve it.
    pub fix: String,
}

// ------------------------------------------------------------------ reading
//
// Written for the browser module's size: every refusal goes through one
// `bad`, a place and a reason, and text is joined by `cat` rather than
// `format!` -- each `format!` is its own code -- and nothing here slices a
// `str` in a way that can panic, which brings a `char`'s formatting back
// (`make size-report`).

/// Reads a description.
pub fn parse(src: &str) -> Result<Declared> {
    let top = json::parse_json(src)?;
    let top = object(&top, "the description")?;
    only(
        top,
        &["format", "collections", "fenecql", "migrations"],
        "the description",
    )?;
    if !matches!(member(top, "format"), Some(Value::Int(FORMAT))) {
        return Err(bad(
            "the description",
            "is not in format 1, the one this build reads",
        ));
    }
    let text = match member(top, "fenecql") {
        None => None,
        Some(Value::Text(t)) if member(top, "collections").is_none() => Some(t.clone()),
        Some(_) => {
            return Err(bad(
                "the description",
                "holds its collections once: `collections` or `fenecql`",
            ))
        }
    };
    // The browser module reads a schema as FenecQL alone: it carries the
    // parser already, where reading the collections' JSON was 8 KB of it.
    #[cfg(target_arch = "wasm32")]
    let collections = match member(top, "collections") {
        Some(_) => {
            return Err(bad(
                "the description",
                "reaches the browser module as `fenecql`",
            ))
        }
        None => Vec::new(),
    };
    #[cfg(not(target_arch = "wasm32"))]
    let collections = collections(list(member(top, "collections"), "collections")?)?;
    let mut migrations = Vec::new();
    for m in list(member(top, "migrations"), "migrations")? {
        migrations.push(match m {
            Value::Text(t) if !blank(t) => Migration::Text(t.clone()),
            Value::Object(o) if only(o, &["rebuild"], "a migration").is_ok() => {
                let r = object(member(o, "rebuild").unwrap_or(&Value::Null), "a rebuild")?;
                only(r, &["collection", "field"], "a rebuild")?;
                Migration::Rebuild {
                    collection: name(r, "collection", "a rebuild")?,
                    field: name(r, "field", "a rebuild")?,
                }
            }
            _ => {
                return Err(bad(
                    "a migration",
                    "is FenecQL text or {\"rebuild\": {collection, field}}",
                ))
            }
        });
    }
    Ok(Declared {
        collections,
        text,
        migrations,
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn collections(list: &[Value]) -> Result<Vec<Schema>> {
    let mut out: Vec<Schema> = Vec::new();
    for c in list {
        let s = collection(c)?;
        if out.iter().any(|o| o.name == s.name) {
            return Err(bad(&s.name, "is declared twice"));
        }
        out.push(s);
    }
    Ok(out)
}

fn blank(t: &str) -> bool {
    t.bytes().all(|b| b.is_ascii_whitespace())
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn collection(v: &Value) -> Result<Schema> {
    let o = object(v, "a collection")?;
    only(o, &["name", "fields"], "a collection")?;
    let cname = name(o, "name", "a collection")?;
    let mut fields = Vec::new();
    let mut paths = Vec::new();
    let declared = list(member(o, "fields"), "fields")?;
    if declared.is_empty() {
        return Err(bad(&cname, "declares no field"));
    }
    for f in declared {
        let (field, its) = field(&cname, f)?;
        fields.push(field);
        paths.extend(its);
    }
    let mut schema = Schema::new(cname.clone(), fields)?;
    for p in paths {
        let at = cat(&[&cname, ".", &p.name]);
        schema.path_of(&p.name)?;
        if schema.path(&p.name).is_some() {
            return Err(bad(&at, "is declared twice"));
        }
        p.index.check(&p.name, &p.ty)?;
        schema.add_path(p);
    }
    Ok(schema)
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn field(collection: &str, v: &Value) -> Result<(Field, Vec<Field>)> {
    let o = object(v, "a field")?;
    only(
        o,
        &["name", "type", "required", "collate", "index", "paths"],
        "a field",
    )?;
    let fname = name(o, "name", "a field")?;
    let at = cat(&[collection, ".", &fname]);
    let ty = match member(o, "type") {
        Some(Value::Text(t)) => type_named(t).ok_or_else(|| bad(&at, "has a type there is not"))?,
        _ => return Err(bad(&at, "has no `type`")),
    };
    let mut f = Field::new(fname.clone(), ty);
    if flag(o, "required", &at)? {
        f = f.required();
    }
    match member(o, "collate") {
        None | Some(Value::Null) => {}
        Some(Value::Text(c)) if c == "tr" || c == "und" => {
            f = f.collated(Collation::named(c).unwrap_or(Collation::Root))
        }
        Some(_) => return Err(bad(&at, "has a collation there is not: tr or und")),
    }
    if let Some(ix) = member(o, "index").filter(|v| !matches!(v, Value::Null)) {
        f = f.indexed(index(ix, &at)?);
    }
    let mut paths = Vec::new();
    for p in list(member(o, "paths"), "paths")? {
        let po = object(p, "a path")?;
        only(po, &["path", "index"], "a path")?;
        let whole = match member(po, "path") {
            Some(Value::Text(k)) if !k.is_empty() => cat(&[&fname, ".", k]),
            _ => return Err(bad(&at, "has a path that names no key")),
        };
        let kind = index(member(po, "index").unwrap_or(&Value::Null), &at)?;
        paths.push(Field::new(whole, DataType::Json).indexed(kind));
    }
    Ok((f, paths))
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn index(v: &Value, at: &str) -> Result<IndexKind> {
    let o = object(v, "an index")?;
    let keys: &[&str] = match member(o, "kind") {
        Some(Value::Text(k)) => match k.as_str() {
            "hash" | "unique" | "sorted" | "inverted" => &["kind"],
            "ttl" => &["kind", "ms"],
            "text" => &["kind", "k1", "b", "prefix", "prefix_min", "chars"],
            "hnsw" => &[
                "kind",
                "metric",
                "m",
                "ef_construction",
                "ef_search",
                "quant",
            ],
            _ => &[],
        },
        _ => &[],
    };
    let Some(Value::Text(kind)) = member(o, "kind").filter(|_| !keys.is_empty()) else {
        return Err(bad(
            at,
            "has an index of no kind there is: hash, unique, sorted, ttl, text, hnsw or inverted",
        ));
    };
    only(o, keys, at)?;
    Ok(match kind.as_str() {
        "hash" => IndexKind::HASH,
        "unique" => IndexKind::UNIQUE,
        "sorted" => IndexKind::SORTED,
        "inverted" => IndexKind::Inverted,
        "ttl" => match whole(o, "ms", at)? {
            Some(ms) if ms > 0 => IndexKind::Sorted {
                ttl: Some(ms as u64),
            },
            _ => {
                return Err(bad(
                    at,
                    "has a ttl that is not `ms`, milliseconds past zero",
                ))
            }
        },
        "text" => {
            let mut spec = TextIndexSpec::default();
            for (key, pct) in [("k1", &mut spec.k1_pct), ("b", &mut spec.b_pct)] {
                if let Some(v) = member(o, key) {
                    match v.as_f64().filter(|x| (0.0..=100.0).contains(x)) {
                        Some(x) => *pct = (x * 100.0 + 0.5) as u16,
                        None => return Err(bad(at, "has a k1 or b that is not 0 to 100")),
                    }
                }
            }
            for (key, n) in [
                ("prefix", &mut spec.prefix_max),
                ("prefix_min", &mut spec.prefix_min),
            ] {
                match whole(o, key, at)? {
                    None => {}
                    Some(x) if x <= 64 => *n = x as u8,
                    Some(_) => return Err(bad(at, "has a prefix that is not 0 to 64")),
                }
            }
            // Without prefixes the shortest one means nothing, and the
            // index as written leaves it out: kept, the declaration would
            // never equal the index it made.
            if spec.prefix_max == 0 {
                spec.prefix_min = TextIndexSpec::default().prefix_min;
            }
            spec.chars = flag(o, "chars", at)?;
            IndexKind::Text(spec)
        }
        _ => {
            let mut spec = VectorIndexSpec::default();
            match member(o, "metric") {
                None => {}
                Some(Value::Text(m)) if Metric::parse(m).is_some_and(|p| p.name() == m) => {
                    spec.metric = Metric::parse(m).unwrap_or(Metric::Cosine)
                }
                Some(_) => return Err(bad(at, "has a metric there is not: cosine, l2 or dot")),
            }
            // As the parser bounds them, so the declaration equals the index.
            if let Some(m) = whole(o, "m", at)? {
                spec.m = (m as usize).max(2);
            }
            if let Some(n) = whole(o, "ef_construction", at)? {
                spec.ef_construction = (n as usize).max(8);
            }
            if let Some(n) = whole(o, "ef_search", at)? {
                spec.ef_search = (n as usize).max(1);
            }
            match member(o, "quant") {
                None => {}
                Some(Value::Text(q)) if Quant::parse(q).is_some() => {
                    spec.quant = Quant::parse(q).unwrap_or(Quant::None)
                }
                Some(_) => return Err(bad(at, "has a quant there is not: none, int8 or bit")),
            }
            IndexKind::Vector(spec.resolved())
        }
    })
}

/// A type as the description spells it: as `DataType::name` writes it.
pub fn type_named(s: &str) -> Option<DataType> {
    let b = s.as_bytes();
    if let [b'[', inner @ .., b']'] = b {
        let inner = std::str::from_utf8(inner).ok()?;
        return Some(DataType::List(Box::new(type_named(inner)?)));
    }
    let dim = |head: &[u8], tail: &[u8]| -> Option<usize> {
        let rest = b.strip_prefix(head)?.strip_suffix(tail)?;
        if rest.is_empty() || rest.len() > 10 || !rest.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let n = rest
            .iter()
            .fold(0usize, |n, d| n * 10 + (d - b'0') as usize);
        (n > 0).then_some(n)
    };
    if let Some(n) = dim(b"vector<", b", f16>") {
        return Some(DataType::Vector(n, VecPrec::F16));
    }
    if let Some(n) = dim(b"vector<", b">") {
        return Some(DataType::Vector(n, VecPrec::F32));
    }
    if let Some(n) = dim(b"sparse<", b">").filter(|&n| n <= crate::sparse::MAX_DIM) {
        return Some(DataType::Sparse(n));
    }
    Some(match s {
        "bool" => DataType::Bool,
        "int" => DataType::Int,
        "float" => DataType::Float,
        "text" => DataType::Text,
        "bytes" => DataType::Bytes,
        "timestamp" => DataType::Timestamp,
        "json" => DataType::Json,
        _ => return None,
    })
}

/// Text joined: one loop, where each `format!` is code of its own.
fn cat(parts: &[&str]) -> String {
    let mut s = String::new();
    for p in parts {
        s.push_str(p);
    }
    s
}

fn bad(at: &str, why: &str) -> Error {
    Error::Query(cat(&["schema description: ", at, " ", why]))
}

fn object<'a>(v: &'a Value, what: &str) -> Result<&'a [(String, Value)]> {
    match v {
        Value::Object(o) => Ok(o),
        _ => Err(bad(what, "is not an object")),
    }
}

fn member<'a>(o: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
    o.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// A key no reader takes is refused: a misspelt option would otherwise be
/// an index built without it.
fn only(o: &[(String, Value)], keys: &[&str], what: &str) -> Result<()> {
    match o.iter().find(|(k, _)| !keys.contains(&k.as_str())) {
        Some((k, _)) => Err(bad(what, &cat(&["takes no `", k, "`"]))),
        None => Ok(()),
    }
}

fn list<'a>(v: Option<&'a Value>, what: &str) -> Result<&'a [Value]> {
    match v {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::List(l)) => Ok(l),
        Some(_) => Err(bad(what, "is not a list")),
    }
}

/// A collection's or a field's name: an identifier, as FenecQL reads one,
/// since it goes into the statements unquoted.
fn name(o: &[(String, Value)], key: &str, what: &str) -> Result<String> {
    match member(o, key) {
        Some(Value::Text(n)) if is_name(n) => Ok(n.clone()),
        _ => Err(bad(what, &cat(&["has no `", key, "` that is a name"]))),
    }
}

fn is_name(n: &str) -> bool {
    let mut chars = n.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

fn flag(o: &[(String, Value)], key: &str, at: &str) -> Result<bool> {
    match member(o, key) {
        None | Some(Value::Null) | Some(Value::Bool(false)) => Ok(false),
        Some(Value::Bool(true)) => Ok(true),
        Some(_) => Err(bad(
            at,
            &cat(&["has a `", key, "` that is not true or false"]),
        )),
    }
}

fn whole(o: &[(String, Value)], key: &str, at: &str) -> Result<Option<i64>> {
    match member(o, key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Int(n)) if *n >= 0 => Ok(Some(*n)),
        Some(_) => Err(bad(
            at,
            &cat(&["has a `", key, "` that is not a whole number"]),
        )),
    }
}

// ------------------------------------------------------------------ writing

/// A database's schemas as a description, every option spelled out. The
/// collections whose names start with `_` are the database's own --
/// `_migrations`, a server's `_idempotency` and `_consumers` -- and are left
/// out.
pub fn describe(schemas: &[Schema]) -> String {
    let mut out = format!("{{\"format\":{FORMAT},\"collections\":[");
    for (i, s) in schemas.iter().filter(|s| !own(&s.name)).enumerate() {
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
            if f.required {
                out.push_str(",\"required\":true");
            }
            if let Some(c) = f.collate {
                out.push_str(",\"collate\":");
                json::escape_into(&mut out, c.name());
            }
            if f.index != IndexKind::None {
                out.push_str(",\"index\":");
                index_json(&mut out, &f.index);
            }
            let mut paths = paths_of(s, &f.name).peekable();
            if paths.peek().is_some() {
                out.push_str(",\"paths\":[");
                for (k, (keys, p)) in paths.enumerate() {
                    if k > 0 {
                        out.push(',');
                    }
                    out.push_str("{\"path\":");
                    json::escape_into(&mut out, keys);
                    out.push_str(",\"index\":");
                    index_json(&mut out, &p.index);
                    out.push('}');
                }
                out.push(']');
            }
            out.push('}');
        }
        out.push_str("]}");
    }
    out.push_str("]}");
    out
}

fn index_json(out: &mut String, k: &IndexKind) {
    match k {
        IndexKind::None => out.push_str("null"),
        IndexKind::Hash { unique: false } => out.push_str("{\"kind\":\"hash\"}"),
        IndexKind::Hash { unique: true } => out.push_str("{\"kind\":\"unique\"}"),
        IndexKind::Sorted { ttl: None } => out.push_str("{\"kind\":\"sorted\"}"),
        IndexKind::Sorted { ttl: Some(ms) } => {
            out.push_str(&format!("{{\"kind\":\"ttl\",\"ms\":{ms}}}"))
        }
        IndexKind::Inverted => out.push_str("{\"kind\":\"inverted\"}"),
        IndexKind::Vector(s) => {
            out.push_str(&format!(
                "{{\"kind\":\"hnsw\",\"metric\":\"{}\",\"m\":{},\"ef_construction\":{},\"ef_search\":{}",
                s.metric.name(),
                s.m,
                s.ef_construction,
                s.ef_search
            ));
            if s.quant != Quant::None {
                out.push_str(&format!(",\"quant\":\"{}\"", s.quant.name()));
            }
            out.push('}');
        }
        IndexKind::Text(s) => {
            out.push_str("{\"kind\":\"text\",\"k1\":");
            crate::num::f32_into(out, s.k1());
            out.push_str(",\"b\":");
            crate::num::f32_into(out, s.b());
            if s.prefix_max != 0 {
                out.push_str(&format!(",\"prefix\":{}", s.prefix_max));
                if s.prefix_min != TextIndexSpec::default().prefix_min {
                    out.push_str(&format!(",\"prefix_min\":{}", s.prefix_min));
                }
            }
            if s.chars {
                out.push_str(",\"chars\":true");
            }
            out.push('}');
        }
    }
}

/// The indexes on paths into `field`, each with its keys past the field.
fn paths_of<'a>(s: &'a Schema, field: &'a str) -> impl Iterator<Item = (&'a str, &'a Field)> {
    s.paths
        .iter()
        .filter_map(move |p| match split_path(&p.name) {
            Some((f, keys)) if f == field => Some((keys, p)),
            _ => None,
        })
}

/// A database's schemas as FenecQL -- a `schema.fenecql` file: each
/// collection's `create collection` and the `create index` of each path,
/// every option written. Read back by `fenec_ql::schema_text`, it is the
/// same schemas, and so the same description.
pub fn fenecql(schemas: &[Schema]) -> String {
    let mut out = String::new();
    for s in schemas.iter().filter(|s| !own(&s.name)) {
        for statement in create(s) {
            out.push_str(&statement);
            out.push('\n');
        }
    }
    out
}

/// Whether a collection is the database's own, by its name.
pub fn own(name: &str) -> bool {
    name.starts_with('_')
}

/// An index as FenecQL declares it, every option written: `@hnsw(cosine,
/// m=16, ef_construction=200, ef_search=100)`.
pub fn index_text(k: &IndexKind) -> Option<String> {
    Some(match k {
        IndexKind::None => return None,
        IndexKind::Hash { unique: false } => "@hash".into(),
        IndexKind::Hash { unique: true } => "@unique".into(),
        IndexKind::Sorted { ttl: None } => "@sorted".into(),
        IndexKind::Sorted { ttl: Some(ms) } => format!("@ttl({})", ttl_text(*ms)),
        IndexKind::Inverted => "@inverted".into(),
        IndexKind::Vector(s) => format!(
            "@hnsw({}, m={}, ef_construction={}, ef_search={}{})",
            s.metric.name(),
            s.m,
            s.ef_construction,
            s.ef_search,
            s.quant_arg()
        ),
        IndexKind::Text(s) => format!("@text({})", s.args()),
    })
}

/// A field as `create collection` and `add field` declare it; `required`
/// says whether its `required` is written.
pub fn field_text(f: &Field, required: bool) -> String {
    let mut s = cat(&[&f.name, " ", &f.ty.name()]);
    if required && f.required {
        s.push_str(" required");
    }
    if let Some(c) = f.collate {
        s.push_str(" collate ");
        s.push_str(c.name());
    }
    if let Some(ix) = index_text(&f.index) {
        s.push(' ');
        s.push_str(&ix);
    }
    s
}

/// The statements that make a collection as declared: the collection, then
/// an index on each path.
pub fn create(s: &Schema) -> Vec<String> {
    let mut c = cat(&["create collection ", &s.name, " ("]);
    for (i, f) in s.fields.iter().enumerate() {
        if i > 0 {
            c.push_str(", ");
        }
        c.push_str(&field_text(f, true));
    }
    c.push(')');
    let mut out = vec![c];
    out.extend(s.paths.iter().map(|p| path_index(&s.name, p)));
    out
}

fn path_index(collection: &str, p: &Field) -> String {
    let ix = index_text(&p.index).unwrap_or_default();
    cat(&["create index on ", collection, " (", &p.name, ") ", &ix])
}

// --------------------------------------------------------------------- plan

/// What `db` needs to hold what `declared` declares, read as `mode` says.
/// `db` is every collection the database holds: one the code does not
/// declare is left alone.
#[inline(always)]
pub fn plan(db: &[Schema], declared: &[Schema], mode: Mode) -> Plan {
    // Two copies, so that a caller that only applies -- the browser module
    // -- carries none of what only following says.
    match mode {
        Mode::Apply => planned::<false>(db, declared),
        Mode::Follow => planned::<true>(db, declared),
    }
}

fn planned<const FOLLOW: bool>(db: &[Schema], declared: &[Schema]) -> Plan {
    let mut p = Plan::default();
    for d in declared {
        match db.iter().find(|s| s.name == d.name) {
            None if !FOLLOW => p.statements.extend(create(d)),
            None => p.refuse(
                "collection_missing",
                &d.name,
                None,
                cat(&["`", &d.name, "` is in the code and not in the database"]),
                cat(&[
                    "its owner makes it (",
                    &create(d).join("; "),
                    "), or the code leaves it out",
                ]),
            ),
            Some(s) => collection_plan::<FOLLOW>(&mut p, s, d),
        }
    }
    // A collection the code does not declare is left as it is: nothing of
    // it is lost, and FenecQL renames no collection, so it cannot be one the
    // code renamed. Code that means it gone drops it in a migration.
    p
}

fn collection_plan<const FOLLOW: bool>(p: &mut Plan, s: &Schema, d: &Schema) {
    let c = &d.name;
    let alter = cat(&["alter collection ", c, " "]);
    for f in &d.fields {
        let at = cat(&["`", c, ".", &f.name, "`"]);
        let Some(g) = s.field(&f.name) else {
            let add = cat(&[&alter, "add field ", &field_text(f, false)]);
            if FOLLOW {
                p.refuse(
                    "field_missing",
                    c,
                    Some(&f.name),
                    cat(&[&at, " is in the code and not in the database"]),
                    cat(&[
                        "its owner adds it (`",
                        &add,
                        "`), or the code leaves it out",
                    ]),
                );
            } else if f.required {
                p.refuse(
                    "required_added",
                    c,
                    Some(&f.name),
                    cat(&[
                        &at,
                        " is required and new: the documents in `",
                        c,
                        "` hold no value for it",
                    ]),
                    "declare it not required: a field added to a collection that exists cannot be"
                        .into(),
                );
            } else {
                p.statements.push(add);
                p.statements.extend(
                    d.paths
                        .iter()
                        .filter(|q| under(q, &f.name))
                        .map(|q| path_index(c, q)),
                );
            }
            continue;
        };
        if g.ty != f.ty {
            p.refuse(
                "type_changed",
                c,
                Some(&f.name),
                cat(&[&at, " is ", &g.ty.name(), " in the database and ", &f.ty.name(), " in the code"]),
                match FOLLOW {
                    true => AS_IT_IS.into(),
                    false => cat(&[
                        "a type does not change in place: a migration adds a field of the new type, sets it from `",
                        &f.name,
                        "` and drops `",
                        &f.name,
                        "`",
                    ]),
                },
            );
            continue;
        }
        if g.required != f.required {
            // Loosened, no value is lost, and a rebuild -- whose field is
            // never required -- makes it so; tightened, the documents there
            // may hold none, and no statement makes a field required.
            let (yes, no, fix) = match g.required {
                true => (
                    "the database",
                    "the code",
                    fix_rebuild::<FOLLOW>(c, &f.name),
                ),
                false => (
                    "the code",
                    "the database",
                    cat(&[
                        AS_IT_IS,
                        ": a field becomes required only as its collection is made",
                    ]),
                ),
            };
            p.refuse(
                "required_changed",
                c,
                Some(&f.name),
                cat(&[&at, " is required in ", yes, " and not in ", no]),
                fix,
            );
        }
        if g.collate != f.collate {
            let name = |c: Option<Collation>| c.map_or("no collation", |c| c.name());
            p.refuse(
                "collate_changed",
                c,
                Some(&f.name),
                cat(&[
                    &at,
                    " is in ",
                    name(g.collate),
                    " in the database and in ",
                    name(f.collate),
                    " in the code",
                ]),
                fix_rebuild::<FOLLOW>(c, &f.name),
            );
        }
        index_plan::<FOLLOW>(p, c, &f.name, &g.index, &f.index);
        // The paths into a json field, each as a field of its own.
        for q in d.paths.iter().filter(|q| under(q, &f.name)) {
            let had = s.path(&q.name).map_or(&IndexKind::None, |h| &h.index);
            index_plan::<FOLLOW>(p, c, &q.name, had, &q.index);
        }
        if !FOLLOW {
            for h in s
                .paths
                .iter()
                .filter(|h| under(h, &f.name) && d.path(&h.name).is_none())
            {
                index_plan::<FOLLOW>(p, c, &h.name, &h.index, &IndexKind::None);
            }
        }
    }
    if FOLLOW {
        return;
    }
    for g in s.fields.iter().filter(|g| d.field(&g.name).is_none()) {
        // A field the database lacks and one the code lacks, of one type,
        // may be one field renamed: said beside the second, never guessed.
        let mut twins = d
            .fields
            .iter()
            .filter(|f| f.ty == g.ty && s.field(&f.name).is_none());
        let drop = cat(&["`", &alter, "drop field ", &g.name, "`"]);
        let fix = match (twins.next(), twins.next()) {
            (Some(f), None) => cat(&[
                "if it was renamed to `",
                &f.name,
                "`, the migration `",
                &alter,
                "rename field ",
                &g.name,
                " to ",
                &f.name,
                "`; if it goes, ",
                &drop,
            ]),
            _ => cat(&[
                "the migration ",
                &drop,
                ", or `",
                &alter,
                "rename field ",
                &g.name,
                " to <its new name>` if it was renamed",
            ]),
        };
        p.refuse(
            "field_not_declared",
            c,
            Some(&g.name),
            cat(&[
                "`",
                c,
                ".",
                &g.name,
                "` is in the database and not in the code",
            ]),
            fix,
        );
    }
}

const AS_IT_IS: &str = "declare it as the database has it";

/// Whether a path's index reads into `field`.
fn under(p: &Field, field: &str) -> bool {
    split_path(&p.name).is_some_and(|(f, _)| f == field)
}

fn index_plan<const FOLLOW: bool>(
    p: &mut Plan,
    c: &str,
    f: &str,
    had: &IndexKind,
    want: &IndexKind,
) {
    if had == want {
        return;
    }
    let at = cat(&["`", c, ".", f, "` has "]);
    let show = |k: &IndexKind| index_text(k).unwrap_or_else(|| "no index".into());
    let create = cat(&["create index on ", c, " (", f, ") ", &show(want)]);
    match (had, want) {
        (IndexKind::None, _) if !FOLLOW => p.statements.push(create),
        // The database's owner may index what the code does not ask for.
        (_, IndexKind::None) if FOLLOW => {}
        (IndexKind::None, _) => p.refuse(
            "index_missing",
            c,
            Some(f),
            cat(&[
                &at,
                &show(want),
                " in the code and no index in the database",
            ]),
            cat(&[
                "its owner builds it (`",
                &create,
                "`), or the code leaves it out",
            ]),
        ),
        (_, IndexKind::None) => p.refuse(
            "index_removed",
            c,
            Some(f),
            cat(&[&at, &show(had), " in the database and no index in the code"]),
            fix_rebuild::<FOLLOW>(c, root(f)),
        ),
        _ => {
            // An expiry and an ordered index are one index, which `alter
            // field` changes in place.
            let fix = match (had, want) {
                (IndexKind::Sorted { .. }, IndexKind::Sorted { .. }) if !FOLLOW => cat(&[
                    "the migration `alter collection ",
                    c,
                    " alter field ",
                    f,
                    " ",
                    &show(want),
                    "`",
                ]),
                _ => fix_rebuild::<FOLLOW>(c, root(f)),
            };
            p.refuse(
                "index_changed",
                c,
                Some(f),
                cat(&[
                    &at,
                    &show(had),
                    " in the database and ",
                    &show(want),
                    " in the code",
                ]),
                fix,
            )
        }
    }
}

/// The field a path reads into, or the field itself.
fn root(f: &str) -> &str {
    split_path(f).map_or(f, |(h, _)| h)
}

fn fix_rebuild<const FOLLOW: bool>(c: &str, f: &str) -> String {
    match FOLLOW {
        true => AS_IT_IS.into(),
        false => cat(&[
            "the migration {\"rebuild\": {\"collection\": \"",
            c,
            "\", \"field\": \"",
            f,
            "\"}} (rebuild in the SDKs): `",
            f,
            "` made again as declared, its values copied",
        ]),
    }
}

impl Plan {
    fn refuse(
        &mut self,
        kind: &'static str,
        collection: &str,
        field: Option<&str>,
        message: String,
        fix: String,
    ) {
        self.refusals.push(Refusal {
            kind,
            collection: collection.into(),
            field: field.map(Into::into),
            message,
            fix,
        });
    }

    /// `"statements":[...],"refusals":[{"kind","collection","field","message","fix"}]`
    /// -- without braces, for a caller to put more beside.
    pub fn json_into(&self, out: &mut String) {
        out.push_str("\"statements\":[");
        for (i, s) in self.statements.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            json::escape_into(out, s);
        }
        out.push_str("],\"refusals\":[");
        for (i, r) in self.refusals.iter().enumerate() {
            out.push_str(if i > 0 { ",{\"kind\":" } else { "{\"kind\":" });
            json::escape_into(out, r.kind);
            out.push_str(",\"collection\":");
            json::escape_into(out, &r.collection);
            out.push_str(",\"field\":");
            match &r.field {
                Some(f) => json::escape_into(out, f),
                None => out.push_str("null"),
            }
            out.push_str(",\"message\":");
            json::escape_into(out, &r.message);
            out.push_str(",\"fix\":");
            json::escape_into(out, &r.fix);
            out.push('}');
        }
        out.push(']');
    }
}

// --------------------------------------------------------------- migrations

/// Each migration as the FenecQL it runs, a rebuild written out against the
/// declaration: what is run, and what is recorded.
pub fn migration_texts(d: &Declared) -> Result<Vec<String>> {
    d.migrations
        .iter()
        .map(|m| match m {
            Migration::Text(t) => Ok(t.clone()),
            Migration::Rebuild { collection, field } => d
                .collections
                .iter()
                .find(|s| s.name == *collection)
                .and_then(|s| rebuild(s, field))
                .ok_or_else(|| {
                    bad(
                        &cat(&[collection, ".", field]),
                        "is rebuilt and not declared",
                    )
                }),
        })
        .collect()
}

/// A field made again as `s` declares it: a field of that declaration under
/// another name, the values copied into it, the old one dropped and the new
/// one named as the old -- each a statement that rewrites no document but
/// the copy. Not required: a field added cannot be.
pub fn rebuild(s: &Schema, field: &str) -> Option<String> {
    let f = s.field(field)?;
    let alter = cat(&["alter collection ", &s.name, " "]);
    let tmp = Field {
        name: cat(&[field, "__rebuilt"]),
        ..f.clone()
    };
    let mut out = cat(&[
        &alter,
        "add field ",
        &field_text(&tmp, false),
        "; set ",
        &s.name,
        " {",
        &tmp.name,
        ": ",
        field,
        "}; ",
        &alter,
        "drop field ",
        field,
        "; ",
        &alter,
        "rename field ",
        &tmp.name,
        " to ",
        field,
    ]);
    for p in s.paths.iter().filter(|p| under(p, field)) {
        out.push_str("; ");
        out.push_str(&path_index(&s.name, p));
    }
    Some(out)
}

/// How many of `texts` the database has applied, given what `_migrations`
/// holds -- `(n, text)`, ascending -- and so where the pending ones start.
/// A migration is known by its place in the list and held to its text: one
/// changed after it ran, or a database holding more than the code lists, is
/// refused, as the list only grows.
pub fn applied(recorded: &[(i64, String)], texts: &[String]) -> Result<usize> {
    if recorded.len() > texts.len() {
        return Err(Error::Query(
            "the database has applied more migrations than the code lists: the code is older than the database"
                .into(),
        ));
    }
    for (i, ((n, text), want)) in recorded.iter().zip(texts).enumerate() {
        if *n != i as i64 + 1 || text != want {
            return Err(Error::Query(cat(&[
                "migration ",
                &(i + 1).to_string(),
                " is not the one the database applied: the list only grows, so a change goes in a new \
                 migration (it ran `",
                text,
                "`)",
            ])));
        }
    }
    Ok(recorded.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(src: &str) -> Declared {
        parse(src).unwrap_or_else(|e| panic!("{e}: {src}"))
    }

    #[test]
    fn a_description_reads_every_type_and_index() {
        let d = declared(
            r#"{"format":1,"collections":[{"name":"t","fields":[
                {"name":"a","type":"text","required":true,"collate":"tr","index":{"kind":"text","prefix":6,"chars":true}},
                {"name":"b","type":"int","index":{"kind":"hash"}},
                {"name":"c","type":"float","index":{"kind":"sorted"}},
                {"name":"d","type":"bool"},
                {"name":"e","type":"timestamp","index":{"kind":"ttl","ms":5400000}},
                {"name":"f","type":"bytes"},
                {"name":"g","type":"json","paths":[{"path":"lang","index":{"kind":"hash"}},{"path":"src.rank","index":{"kind":"sorted"}}]},
                {"name":"h","type":"vector<4, f16>","index":{"kind":"hnsw","metric":"l2","m":8,"ef_construction":50,"ef_search":33,"quant":"int8"}},
                {"name":"i","type":"sparse<100>","index":{"kind":"inverted"}},
                {"name":"j","type":"[text]","index":{"kind":"hash"}},
                {"name":"u","type":"text","index":{"kind":"unique"}},
                {"name":"v","type":"vector<3>","index":{"kind":"hnsw"}}]}]}"#,
        );
        let s = &d.collections[0];
        assert_eq!(s.fields.len(), 12);
        assert_eq!(s.paths.len(), 2);
        let made = create(s);
        assert_eq!(
            made[0],
            "create collection t (a text required collate tr @text(k1=0.9, b=0.4, prefix=6, chars), \
             b int @hash, c float @sorted, d bool, e timestamp @ttl(90m), f bytes, g json, \
             h vector<4, f16> @hnsw(l2, m=8, ef_construction=50, ef_search=33, quant=int8), \
             i sparse<100> @inverted, j [text] @hash, u text @unique, \
             v vector<3> @hnsw(cosine, m=16, ef_construction=200, ef_search=100))"
        );
        assert_eq!(made[1], "create index on t (g.lang) @hash");
        assert_eq!(made[2], "create index on t (g.src.rank) @sorted");
        // Described again, it reads back as itself.
        let again = declared(&describe(&d.collections));
        assert_eq!(again.collections, d.collections);
    }

    #[test]
    fn a_description_is_refused_what_it_does_not_say_right() {
        for (src, why) in [
            (r#"{"collections":[]}"#, "not in format 1"),
            (r#"{"format":2,"collections":[]}"#, "not in format 1"),
            (
                r#"{"format":1,"collections":[{"name":"t","fields":[{"name":"a","type":"txt"}]}]}"#,
                "a type there is not",
            ),
            (
                r#"{"format":1,"collections":[{"name":"t","fields":[{"name":"a","type":"text","index":{"kind":"hash","m":3}}]}]}"#,
                "takes no `m`",
            ),
            (
                r#"{"format":1,"collections":[{"name":"t","fields":[{"name":"a","type":"int","index":{"kind":"text"}}]}]}"#,
                "not text",
            ),
            (
                r#"{"format":1,"collections":[{"name":"t","fields":[{"name":"a b","type":"int"}]}]}"#,
                "that is a name",
            ),
            (
                r#"{"format":1,"collections":[{"name":"t","fields":[{"name":"a","type":"int","nullable":true}]}]}"#,
                "takes no `nullable`",
            ),
            (
                r#"{"format":1,"collections":[{"name":"t","fields":[{"name":"a","type":"int","paths":[{"path":"x","index":{"kind":"hash"}}]}]}]}"#,
                "json field",
            ),
            (
                r#"{"format":1,"collections":[{"name":"t","fields":[{"name":"id","type":"int"}]}]}"#,
                "reserved",
            ),
            (
                r#"{"format":1,"collections":[{"name":"t","fields":[{"name":"a","type":"int"}]},{"name":"t","fields":[{"name":"a","type":"int"}]}]}"#,
                "twice",
            ),
        ] {
            let e = parse(src).unwrap_err().to_string();
            assert!(e.contains(why), "{e} / {src}");
        }
    }

    fn schemas(src: &str) -> Vec<Schema> {
        declared(&format!(r#"{{"format":1,"collections":{src}}}"#)).collections
    }

    #[test]
    fn what_only_adds_is_planned_and_the_rest_refused() {
        let db = schemas(
            r#"[{"name":"t","fields":[{"name":"a","type":"int","index":{"kind":"hash"}},{"name":"old","type":"text"}]},
                {"name":"gone","fields":[{"name":"x","type":"int"}]},
                {"name":"_migrations","fields":[{"name":"n","type":"int"}]}]"#,
        );
        let code = schemas(
            r#"[{"name":"t","fields":[{"name":"a","type":"int","index":{"kind":"sorted"}},{"name":"new","type":"text"},{"name":"b","type":"bool","index":{"kind":"hash"}}]},
                {"name":"u","fields":[{"name":"y","type":"int"}]}]"#,
        );
        let p = plan(&db, &code, Mode::Apply);
        assert_eq!(
            p.statements,
            [
                "alter collection t add field new text",
                "alter collection t add field b bool @hash",
                "create collection u (y int)"
            ]
        );
        let kinds: Vec<_> = p
            .refusals
            .iter()
            .map(|r| (r.kind, r.field.as_deref()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("index_changed", Some("a")),
                ("field_not_declared", Some("old")),
            ]
        );
        // One field gone and one new of the same type: perhaps a rename.
        assert!(
            p.refusals[1].fix.contains("rename field old to new"),
            "{:?}",
            p.refusals[1]
        );
        // Followed, the extras are the owner's, the additions refusals.
        let f = plan(&db, &code, Mode::Follow);
        assert!(f.statements.is_empty());
        let kinds: Vec<_> = f.refusals.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                "index_changed",
                "field_missing",
                "field_missing",
                "collection_missing"
            ]
        );
        // The same schema plans nothing.
        assert_eq!(plan(&code, &code, Mode::Apply), Plan::default());
    }

    #[test]
    fn migrations_are_held_to_their_texts() {
        let texts = vec!["a".to_string(), "b".to_string()];
        assert_eq!(applied(&[], &texts).unwrap(), 0);
        assert_eq!(applied(&[(1, "a".into())], &texts).unwrap(), 1);
        assert!(applied(&[(1, "x".into())], &texts).is_err());
        assert!(applied(&[(1, "a".into()), (2, "b".into()), (3, "c".into())], &texts).is_err());
        assert!(applied(&[(2, "a".into())], &texts).is_err());
    }
}
