//! The tools, each a statement or two run as `/query` runs them.
//!
//! What a model is told of each is in its definition ([`list_into`]): the
//! description is all it has to call it right, so it says what goes in,
//! what comes back and what is refused. Every argument is checked here
//! and a wrong one refused as the tool's result (`isError`), with what to
//! change: the 2025-11-25 revision moved input errors there, since a model
//! corrects from a result and gives up on a protocol error.

use super::{member, Ctx, Options};
use crate::api::{self, Response2};
use crate::http::{Method, Request};
use fenec_core::json;
use fenec_core::prelude::*;
use fenec_core::query::{Count, Select};
use fenec_core::schema::{IndexKind, Schema};
use fenec_core::value::DataType;
use std::sync::Arc;

type Args<'a> = &'a [(String, Value)];

/// The tools, in the order `tools/list` gives them -- the same every time,
/// so a client's cache and a model's prompt cache hold.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Tool {
    Schema,
    Search,
    Query,
    Get,
    Write,
    Remember,
    Recall,
    Forget,
}

pub(super) const COUNT: usize = 8;

impl Tool {
    pub(super) const ALL: [Tool; COUNT] = [
        Tool::Schema,
        Tool::Search,
        Tool::Query,
        Tool::Get,
        Tool::Write,
        Tool::Remember,
        Tool::Recall,
        Tool::Forget,
    ];

    pub(super) fn name(self) -> &'static str {
        match self {
            Tool::Schema => "schema",
            Tool::Search => "search",
            Tool::Query => "query",
            Tool::Get => "get",
            Tool::Write => "write",
            Tool::Remember => "remember",
            Tool::Recall => "recall",
            Tool::Forget => "forget",
        }
    }

    /// Whether the server's options list it.
    fn listed(self, opts: &Options) -> bool {
        match self {
            Tool::Write => opts.write,
            Tool::Remember | Tool::Recall | Tool::Forget => opts.memory.is_some(),
            _ => true,
        }
    }

    /// Whether it writes: the ones the audit log names.
    pub(super) fn writes(self) -> bool {
        matches!(self, Tool::Write | Tool::Remember | Tool::Forget)
    }

    /// The tool `name` names, if the server lists it.
    pub(super) fn named(name: &str, opts: &Options) -> Option<Tool> {
        Tool::ALL
            .into_iter()
            .find(|t| t.name() == name && t.listed(opts))
    }
}

/// What a tool answered: its structured content, a JSON object -- which
/// goes out as the text a model reads as well, the two the same as the
/// revision asks -- or why it refused, with the HTTP status `/query`
/// would have answered and, for a refusal to come back from, in how many
/// seconds: `retry_after`, as every 429 and 503 of the server says it; and
/// the `reason` a statement's `require <n> else '<reason>'` gave, as its
/// 412 says it, for a model to tell one refusal from another by.
pub(super) enum Outcome {
    Done(String),
    Refused {
        status: u16,
        why: String,
        retry_after: Option<u64>,
        reason: Option<String>,
    },
}

impl Outcome {
    pub(super) fn refused(status: u16, why: impl Into<String>) -> Outcome {
        Outcome::Refused {
            status,
            why: why.into(),
            retry_after: None,
            reason: None,
        }
    }

    /// A refusal to call again from in `secs` seconds.
    pub(super) fn come_back(status: u16, why: impl Into<String>, secs: u64) -> Outcome {
        Outcome::Refused {
            status,
            why: why.into(),
            retry_after: Some(secs),
            reason: None,
        }
    }

    pub(super) fn is_error(&self) -> bool {
        matches!(self, Outcome::Refused { .. })
    }

    /// The members of a `tools/call` result.
    pub(super) fn result_into(&self, out: &mut String) {
        match self {
            Outcome::Done(body) => {
                out.push_str("\"content\":[{\"type\":\"text\",\"text\":");
                json::escape_into(out, body);
                out.push_str("}],\"structuredContent\":");
                out.push_str(body);
                out.push_str(",\"isError\":false");
            }
            Outcome::Refused {
                status,
                why,
                retry_after,
                reason,
            } => {
                out.push_str("\"content\":[{\"type\":\"text\",\"text\":");
                json::escape_into(out, why);
                out.push_str("}],\"structuredContent\":{\"error\":");
                json::escape_into(out, why);
                out.push_str(",\"status\":");
                json::int_into(out, *status as i64);
                if let Some(secs) = retry_after {
                    out.push_str(",\"retry_after\":");
                    json::int_into(out, *secs as i64);
                }
                if let Some(r) = reason {
                    out.push_str(",\"reason\":");
                    json::escape_into(out, r);
                }
                out.push_str("},\"isError\":true");
            }
        }
    }
}

/// `outcome`, refused if its answer is past the bound of bytes. Measured
/// on the JSON the model would read; the rows' bound is each tool's, which
/// can stop a read before it fetches them all.
pub(super) fn bounded(outcome: Outcome, opts: &Options) -> Outcome {
    match outcome {
        Outcome::Done(body) if body.len() > opts.max_bytes => Outcome::refused(
            413,
            format!(
                "the answer is {} bytes, past this server's bound of {} (--mcp-max-bytes): refused \
                 rather than cut. Ask for fewer fields (`fields`, `select`), fewer rows (`limit`), \
                 or narrow the filter",
                body.len(),
                opts.max_bytes
            ),
        ),
        other => other,
    }
}

pub(super) fn run(ctx: &Ctx, tool: Tool, args: Args) -> Outcome {
    let done = match tool {
        Tool::Schema => schema(ctx, args),
        Tool::Search => search(ctx, args),
        Tool::Query => query(ctx, args),
        Tool::Get => get(ctx, args),
        Tool::Write => write(ctx, args),
        Tool::Remember => remember(ctx, args),
        Tool::Recall => recall(ctx, args),
        Tool::Forget => forget(ctx, args),
    };
    done.map_or_else(|refused| refused, Outcome::Done)
}

// ------------------------------------------------------------ definitions

const READS: &str =
    r#"{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}"#;

/// The tools the options list, as `tools/list` gives them.
pub(super) fn list_into(out: &mut String, opts: &Options) {
    let rows = opts.max_rows;
    let bytes = opts.max_bytes;
    let mut defs: Vec<String> = Vec::new();
    defs.push(format!(
        r#"{{"name":"schema","title":"Collections and fields","description":"The collections this token may read, each as the FenecQL statement that makes it: its fields, their types and indexes, and how many rows it holds. @text fields are searched by words and @hnsw vector fields by an embedding (search), @hash and @sorted fields filter fast. Call it before the other tools.","inputSchema":{{"type":"object","properties":{{"collection":{{"type":"string","description":"One collection; every one when absent"}}}},"additionalProperties":false}},"annotations":{READS}}}"#
    ));
    defs.push(format!(
        r#"{{"name":"search","title":"Search a collection","description":"Rows ranked best first: by words over an @text field (BM25), by an embedding over an @hnsw vector field, or both fused (reciprocal rank fusion). Each row has its id, its fields but the vectors, `_score`, and for a search by words a `snippet` with the words found in **bold**. Embeddings are yours to compute, with the model and dimension the collection was filled with; without one, search by words. At most {rows} rows."#
    ) + &format!(
        r#"","inputSchema":{{"type":"object","properties":{{"collection":{{"type":"string"}},"text":{{"type":"string","description":"The words to search for"}},"fuzzy":{{"type":"boolean","description":"Words within a typo of the index's match too -- one edit for words of 5 to 8 letters, two from 9 -- for words a user may have misspelt; the words as typed rank first"}},"prefix":{{"type":"boolean","description":"The last word also matches the words it begins, as a search box sends what is being typed"}},"vector":{{"type":"array","items":{{"type":"number"}},"description":"A query embedding, of the vector field's dimension"}},"filter":{{"type":"string","description":"A FenecQL condition every row must meet, its values as $1, $2 in params: year >= $1 and tags has $2"}},"params":{{"type":"array","description":"The filter's values, $1 first"}},"limit":{{"type":"integer","minimum":1,"maximum":{rows},"default":10}},"fields":{{"type":"array","items":{{"type":"string"}},"description":"The fields to return; every one but vectors when absent"}},"text_field":{{"type":"string","description":"The @text field to search, where there are several"}},"vector_field":{{"type":"string","description":"The vector field to search, where there are several"}}}},"required":["collection"],"additionalProperties":false}},"annotations":{READS}}}"#
    ));
    defs.push(format!(
        r#"{{"name":"query","title":"Run a FenecQL read","description":"Runs one FenecQL read and answers its rows: get <collection> [select a, b] [where <condition>] [order <field> [desc]] [limit n]; aggregates (get orders select status, count(*), sum(total) group status); get <collection> where ... count; lookup <collection> on <field> for related rows; explain get .... SQL's SELECT ... FROM ... WHERE, a JOIN on a key, IN (..), EXISTS, DISTINCT, BETWEEN and LIKE are read too; double quotes are text, so a field is written bare. Values go in params as $1, $2. An answer of more than {rows} rows or {bytes} bytes is refused, not cut: add a limit or narrow the where. Vector fields are left out unless include_vectors."#
    ) + &format!(
        r#"","inputSchema":{{"type":"object","properties":{{"query":{{"type":"string","description":"One FenecQL read"}},"params":{{"type":"array","description":"The values of $1, $2, ..."}},"include_vectors":{{"type":"boolean","default":false,"description":"Keep vector fields in the rows"}}}},"required":["query"],"additionalProperties":false}},"annotations":{READS}}}"#
    ));
    defs.push(format!(
        r#"{{"name":"get","title":"Rows by id","description":"Rows of a collection by their ids, which search, query and recall give. Ids with no row come back in `missing`.","inputSchema":{{"type":"object","properties":{{"collection":{{"type":"string"}},"ids":{{"type":"array","items":{{"type":"integer"}},"minItems":1,"maxItems":{rows}}},"fields":{{"type":"array","items":{{"type":"string"}},"description":"The fields to return; every one but vectors when absent"}}}},"required":["collection","ids"],"additionalProperties":false}},"annotations":{READS}}}"#
    ));
    if opts.write {
        defs.push(
            r#"{"name":"write","title":"Write rows","description":"Runs one FenecQL write and answers how many rows it touched: insert <collection> {field: $1, ...} makes a row; put <collection> {id: $1, ...} writes over that row; set <collection> {field: $2} where id = $1 changes fields; del <collection> where id = $1 deletes; put ... if absent else set {n: n + new.n} upserts; SQL's INSERT INTO <collection> (a, b) VALUES ($1, $2) [ON CONFLICT DO NOTHING], UPDATE ... SET a = $1 WHERE ... and DELETE FROM are read too. Values go in params. `require n` after it refuses the write, and writes nothing, unless it touched n rows. The token's rules hold every row written. Schema changes are not made here.","inputSchema":{"type":"object","properties":{"statement":{"type":"string","description":"One put, insert, set or del"},"params":{"type":"array","description":"The values of $1, $2, ..."}},"required":["statement"],"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false}}"#.into(),
        );
    }
    if let Some(m) = &opts.memory {
        let mut c = String::new();
        json::escape_into(&mut c, m);
        let c = c.trim_matches('"').to_string();
        defs.push(format!(
            r#"{{"name":"remember","title":"Remember a fact","description":"Keeps a memory in `{c}` for later conversations: a fact, a preference, a decision, written as a sentence that stands on its own. With `vector`, the text's embedding, recall finds it by meaning as well as by its words. `expires_in` (30m, 12h, 7d) lets it go after that long.","inputSchema":{{"type":"object","properties":{{"text":{{"type":"string"}},"vector":{{"type":"array","items":{{"type":"number"}},"description":"The text's embedding"}},"tags":{{"type":"array","items":{{"type":"string"}},"description":"Words to recall it by exactly: a project, a person"}},"expires_in":{{"type":"string","description":"How long it is kept: 30m, 12h, 7d; for good when absent"}}}},"required":["text"],"additionalProperties":false}},"annotations":{{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":false,"openWorldHint":false}}}}"#
        ));
        defs.push(format!(
            r#"{{"name":"recall","title":"Recall memories","description":"The memories kept with remember that best match `text` -- by their words, and by meaning too when `vector` is its embedding -- best first, each with its id and when it was kept. Without text or vector, the latest ones. Recall before answering from what you know of the user or the project.","inputSchema":{{"type":"object","properties":{{"text":{{"type":"string"}},"vector":{{"type":"array","items":{{"type":"number"}}}},"tags":{{"type":"array","items":{{"type":"string"}},"description":"Only memories holding every one of these"}},"limit":{{"type":"integer","minimum":1,"maximum":{rows},"default":5}}}},"additionalProperties":false}},"annotations":{READS}}}"#
        ));
        defs.push(
            r#"{"name":"forget","title":"Forget a memory","description":"Deletes the memory with this id, which recall gives: one found wrong or out of date.","inputSchema":{"type":"object","properties":{"id":{"type":"integer"}},"required":["id"],"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":false}}"#.into(),
        );
    }
    out.push('[');
    out.push_str(&defs.join(","));
    out.push(']');
}

// ------------------------------------------------------------ arguments

type Checked<T> = std::result::Result<T, Outcome>;

/// Refuses an argument the tool does not take: a model that guessed a
/// name learns the right one rather than have its value ignored.
fn known(args: Args, names: &[&str]) -> Checked<()> {
    match args.iter().find(|(k, _)| !names.contains(&k.as_str())) {
        None => Ok(()),
        Some((k, _)) => Err(Outcome::refused(
            400,
            format!("no argument `{k}`: this tool takes {}", names.join(", ")),
        )),
    }
}

/// An argument, `null` read as absent.
fn arg<'a>(args: Args<'a>, name: &str) -> Option<&'a Value> {
    member(args, name).filter(|v| !matches!(v, Value::Null))
}

fn text<'a>(args: Args<'a>, name: &str) -> Checked<Option<&'a str>> {
    match arg(args, name) {
        None => Ok(None),
        Some(Value::Text(t)) => Ok(Some(t)),
        Some(_) => Err(Outcome::refused(400, format!("`{name}` is a string"))),
    }
}

fn needed<'a>(args: Args<'a>, name: &str) -> Checked<&'a str> {
    text(args, name)?.ok_or_else(|| Outcome::refused(400, format!("`{name}` is required")))
}

fn int(args: Args, name: &str) -> Checked<Option<i64>> {
    match arg(args, name) {
        None => Ok(None),
        Some(Value::Int(n)) => Ok(Some(*n)),
        Some(Value::Float(f)) if f.fract() == 0.0 && f.abs() < 9.0e15 => Ok(Some(*f as i64)),
        Some(_) => Err(Outcome::refused(400, format!("`{name}` is an integer"))),
    }
}

fn texts(args: Args, name: &str) -> Checked<Option<Vec<String>>> {
    match arg(args, name) {
        None => Ok(None),
        Some(Value::List(items)) => items
            .iter()
            .map(|v| match v {
                Value::Text(t) => Ok(t.clone()),
                _ => Err(Outcome::refused(
                    400,
                    format!("`{name}` is a list of strings"),
                )),
            })
            .collect::<Checked<Vec<_>>>()
            .map(Some),
        Some(_) => Err(Outcome::refused(
            400,
            format!("`{name}` is a list of strings"),
        )),
    }
}

/// An embedding: a list of numbers, each made the `f32` a vector holds.
fn vector(args: Args, name: &str) -> Checked<Option<Vec<f32>>> {
    let bad = || Outcome::refused(400, format!("`{name}` is a list of numbers"));
    match arg(args, name) {
        None => Ok(None),
        Some(Value::List(items)) if !items.is_empty() => items
            .iter()
            .map(|v| match v {
                Value::Int(n) => Ok(*n as f32),
                Value::Float(f) if f.is_finite() => Ok(*f as f32),
                _ => Err(bad()),
            })
            .collect::<Checked<Vec<_>>>()
            .map(Some),
        Some(_) => Err(bad()),
    }
}

fn params(args: Args) -> Checked<Vec<Value>> {
    match arg(args, "params") {
        None => Ok(Vec::new()),
        Some(Value::List(items)) => Ok(items.clone()),
        Some(_) => Err(Outcome::refused(
            400,
            "`params` is a list: [2024, \"rust\"]",
        )),
    }
}

/// `n` when FenecQL can write it as a name -- a letter or `_`, then
/// letters, digits and `_` -- and refused otherwise: a name goes into a
/// statement's text, where a value never does.
fn name<'a>(n: &'a str, what: &str) -> Checked<&'a str> {
    let mut chars = n.chars();
    let ok = chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_');
    match ok {
        true => Ok(n),
        false => Err(Outcome::refused(
            400,
            format!("the {what} name `{n}` cannot be written in FenecQL"),
        )),
    }
}

/// An engine's refusal as a tool's, with the status `/query` answers it
/// with.
fn refusal(e: &Error) -> Outcome {
    let why = match e {
        Error::NotFound(m)
        | Error::Exists(m)
        | Error::Duplicate(m)
        | Error::Type(m)
        | Error::Query(m)
        | Error::Corrupt(m)
        | Error::Io(m)
        | Error::Plugin(m)
        | Error::ReadOnly(m)
        | Error::Denied(m)
        | Error::Unmet(m)
        | Error::Quota(m) => m.clone(),
    };
    Outcome::Refused {
        status: api::status_of(e),
        why,
        retry_after: None,
        reason: e.reason().map(String::from),
    }
}

/// `limit`, between 1 and the bound of rows.
fn limit(args: Args, opts: &Options, default: usize) -> Checked<usize> {
    match int(args, "limit")? {
        None => Ok(default.min(opts.max_rows)),
        Some(n) if n >= 1 && n as u64 <= opts.max_rows as u64 => Ok(n as usize),
        Some(n) => Err(Outcome::refused(
            400,
            format!("`limit` is between 1 and {}, not {n}", opts.max_rows),
        )),
    }
}

// ------------------------------------------------------------ running

/// The schema of `collection` as the token may see it: one it may not read
/// is answered as one that is not there, so a refusal names nothing, and
/// a field no read rule grants it is not among its fields.
fn schema_of(ctx: &Ctx, collection: &str) -> Checked<Schema> {
    let g = crate::held::read(ctx.db);
    let visible = ctx.who.scope().is_none_or(|s| s.readable(collection));
    match g.collection(collection) {
        Ok(c) if visible => Ok(shown_schema(ctx, &c.schema)),
        _ => Err(Outcome::refused(
            404,
            format!("no collection `{collection}`: `schema` lists the ones there are"),
        )),
    }
}

/// `schema` as the token is shown it (`Scope::shown_schema`).
fn shown_schema(ctx: &Ctx, schema: &Schema) -> Schema {
    ctx.who
        .scope()
        .and_then(|s| s.shown_schema(schema))
        .unwrap_or_else(|| schema.clone())
}

fn read(ctx: &Ctx, stmt: Statement, params: &[Value]) -> Checked<Response2> {
    (ctx.run.read)(ctx, stmt, params)
}

fn through_query(ctx: &Ctx, text: &str, params: &[Value]) -> crate::http::Response {
    (ctx.run.write)(ctx, text, params)
}

/// A read as `/query` runs one: held to the token's rules, pinned and run
/// with no lock held when it is long, under the read lock otherwise.
pub(super) fn read_served(ctx: &Ctx, stmt: Statement, params: &[Value]) -> Checked<Response2> {
    // A token whose rules read other collections is held to them as they
    // answer under each lock taken (`crate::scoped_under`).
    let unscoped = ctx.who.reaches_out().then(|| stmt.clone());
    let mut mask = None;
    let stmt = crate::scoped(ctx.who, stmt, params, &mut mask).map_err(|e| refusal(&e))?;
    let mut pin = true;
    loop {
        let guard = crate::held::read(ctx.db);
        let (rebound, rebound_mask, bound);
        let (stmt, mask, who) = match &unscoped {
            None => (&stmt, mask.as_ref(), ctx.who),
            Some(sent) => {
                let mut m = None;
                let (s, b) = crate::scoped_under(ctx.who, &guard, sent.clone(), params, &mut m)
                    .map_err(|e| refusal(&e))?;
                (rebound, rebound_mask, bound) = (s, m, b);
                (
                    &rebound,
                    rebound_mask.as_ref(),
                    bound.as_ref().unwrap_or(ctx.who),
                )
            }
        };
        let running = crate::trace::span("execute");
        let pinned = match pin {
            true => guard.pin(&[(stmt, params)]),
            false => None,
        };
        // Shown as the token is shown it, out of what answered.
        let shown = |r: Response2, db: &Database| crate::shown(who, mask, db, r);
        let answered = match &pinned {
            Some(p) => {
                drop(guard);
                running.attr("fenec.pinned", true);
                p.query(stmt, params)
                    .map(|r| r.map(|r| shown(r, p.database())))
            }
            None => Some(guard.query(stmt, params).map(|r| shown(r, &guard))),
        };
        match answered {
            Some(r) => {
                let r = r.map_err(|e| refusal(&e))?;
                crate::statements::rows(crate::counted(&r));
                return Ok(r);
            }
            // It reached for an index the pin left out: again under the lock.
            None => {
                running.forget();
                pin = false;
            }
        }
    }
}

/// A statement through `POST /query`'s own path, as the token: read-only
/// mode, a tenant's quotas and gate, the memory ceiling, the fsync and the
/// change it leaves for a replica's read all hold as they do for a client.
pub(super) fn write_served(ctx: &Ctx, text: &str, params: &[Value]) -> crate::http::Response {
    let req = Request {
        method: Method::Post,
        target: "/query".into(),
        path: "/query".into(),
        query: Vec::new(),
        headers: Vec::new(),
        body: query_body(text, params).into_bytes(),
        keep_alive: false,
    };
    crate::handle_query(ctx.db, ctx.cfg, &req, ctx.who)
}

/// The body `/query` would be sent, read back as it reads one.
fn query_body(text: &str, params: &[Value]) -> String {
    let mut body = String::from("{\"query\":");
    json::escape_into(&mut body, text);
    body.push_str(",\"params\":");
    json::value_into(&mut body, &Value::List(params.to_vec()));
    body.push('}');
    body
}

/// A read over a database this process holds for one caller (`fenec mcp
/// --file`): under the read lock, held to the token's rules all the same.
/// Nothing writes beside it, so nothing is pinned.
pub(super) fn read_local(ctx: &Ctx, stmt: Statement, params: &[Value]) -> Checked<Response2> {
    let mut mask = None;
    let g = crate::held::read(ctx.db);
    let (stmt, bound) =
        crate::scoped_under(ctx.who, &g, stmt, params, &mut mask).map_err(|e| refusal(&e))?;
    let r = g.query(&stmt, params).map_err(|e| refusal(&e))?;
    let who = bound.as_ref().unwrap_or(ctx.who);
    Ok(crate::shown(who, mask.as_ref(), &g, r))
}

/// A write over a database this process holds for one caller: run under
/// the write lock and on the disk before it is answered, where the
/// session's configuration asks (`sync_on_write`). What a server's write
/// path holds beside -- a tenant's quotas, an `Idempotency-Key`, a held
/// write, an index built beside the database -- is a server's, and left
/// out of the shell, where it was most of what `fenec mcp` added.
pub(super) fn write_local(ctx: &Ctx, text: &str, params: &[Value]) -> crate::http::Response {
    let body = query_body(text, params);
    let parsed = api::parse_query(&body).and_then(|(s, p)| api::exactly(ctx.db, &body, s, p));
    let (stmt, params) = match parsed {
        Ok(v) => v,
        Err(e) => return crate::error_response(&e),
    };
    let unscoped = ctx.who.reaches_out().then(|| Arc::clone(&stmt));
    let mut mask = None;
    let stmt = match crate::scoped(ctx.who, Arc::unwrap_or_clone(stmt), &params, &mut mask) {
        Ok(s) => s,
        Err(e) => return crate::error_response(&e),
    };
    if ctx.cfg.read_only && !stmt.is_read_only() {
        return crate::http::Response::error(403, "this session is read-only (--read-only)");
    }
    let mut g = crate::held::write(ctx.db);
    let r = match &unscoped {
        None => crate::access::within(ctx.who, || g.execute_with(&stmt, &params))
            .map(|r| crate::shown(ctx.who, mask.as_ref(), &g, r)),
        Some(sent) => {
            crate::run_bound(ctx.who, &mut g, sent, &params).map(|r| crate::visible(ctx.who, r))
        }
    };
    let durability = match (&r, ctx.cfg.sync_on_write) {
        (Ok(_), true) => match g.flush() {
            Ok(d) => d,
            Err(e) => return crate::error_response(&e),
        },
        _ => None,
    };
    drop(g);
    if let Some(durable) = durability {
        if let Err(e) = durable() {
            ctx.db.write().unwrap_or_else(|p| p.into_inner()).fail(&e);
            return crate::error_response(&e);
        }
    }
    match r {
        Ok(resp) => api::render_any(&resp, fenec_core::VERSION),
        Err(e) => crate::error_response(&e),
    }
}

/// `/query`'s answer as a tool's: its JSON, an object as it is and rows
/// under `rows`; an error as the refusal it says.
fn answered(resp: crate::http::Response) -> Checked<String> {
    let body = String::from_utf8(resp.body).unwrap_or_default();
    if resp.status >= 300 {
        // A tenant's quota (429) and a tenant being moved (503) say when
        // to come back, in the body as in `Retry-After`: kept for the model.
        let (why, retry_after, reason) = match json::parse_json(&body) {
            Ok(Value::Object(m)) => (
                match member(&m, "error") {
                    Some(Value::Text(t)) => t.clone(),
                    _ => body.clone(),
                },
                match member(&m, "retry_after") {
                    Some(Value::Int(n)) if *n >= 0 => Some(*n as u64),
                    _ => None,
                },
                match member(&m, "reason") {
                    Some(Value::Text(r)) => Some(r.clone()),
                    _ => None,
                },
            ),
            _ => (body.clone(), None, None),
        };
        return Err(Outcome::Refused {
            status: resp.status,
            why,
            retry_after,
            reason,
        });
    }
    Ok(match body.trim_start().starts_with('[') {
        true => format!("{{\"rows\":{body}}}"),
        false => body,
    })
}

/// The rows of a read as a tool answers them: `{"rows": [...], "count":
/// n}`, the facets beside them where asked for, vector fields left out
/// unless `vectors`, a column renamed where `rename` says.
fn rows(resp: Response2, vectors: bool, rename: &[(&str, &str)], extra: &str) -> Checked<String> {
    let mut rs = match resp {
        Response2::Rows(rs) => rs,
        Response2::Affected(n) => return Ok(format!("{{\"affected\":{n}}}")),
        Response2::Ok(msg) => {
            let mut out = String::from("{\"message\":");
            json::escape_into(&mut out, &msg);
            out.push('}');
            return Ok(out);
        }
        Response2::Schemas(list) => {
            let mut out = String::from("{\"collections\":[");
            for (i, s) in list.iter().filter(|s| !s.name.starts_with('_')).enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json::escape_into(&mut out, &s.name);
            }
            out.push_str("]}");
            return Ok(out);
        }
    };
    if !vectors {
        unvectored(&mut rs.columns, rs.rows.iter_mut().map(|r| &mut r.values));
        let mut level = rs.nested.as_mut();
        while let Some(n) = level {
            unvectored(
                &mut n.columns,
                n.groups.iter_mut().flatten().map(|r| &mut r.values),
            );
            level = n.nested.as_deref_mut();
        }
    }
    for (from, to) in rename {
        if let Some(c) = rs.columns.iter_mut().find(|c| c == from) {
            *c = to.to_string();
        }
    }
    // A SQL join's rows flat, and counted so.
    let rs = rs.answered();
    let mut out = String::from("{\"rows\":");
    json::rows_array_into(&mut out, &rs);
    json::facets_into(&mut out, &rs);
    out.push_str(",\"count\":");
    json::int_into(&mut out, rs.rows.len() as i64);
    out.push_str(extra);
    out.push('}');
    Ok(out)
}

/// Takes out the columns any row holds a vector in: a 768-dim embedding is
/// some 7 KB of JSON a row, which a model reads as noise and pays for.
fn unvectored<'a>(columns: &mut Vec<String>, rows: impl Iterator<Item = &'a mut Vec<Value>>) {
    let rows: Vec<&mut Vec<Value>> = rows.collect();
    let gone: Vec<usize> = (0..columns.len())
        .filter(|&c| {
            rows.iter()
                .any(|r| matches!(r.get(c), Some(Value::Vector(_) | Value::Sparse(..))))
        })
        .collect();
    for &c in gone.iter().rev() {
        columns.remove(c);
    }
    for r in rows {
        for &c in gone.iter().rev() {
            if c < r.len() {
                r.remove(c);
            }
        }
    }
}

/// The fields a row is answered with: those asked for, checked, or every
/// one but the vectors.
fn fields(args: Args, schema: &Schema) -> Checked<Vec<String>> {
    match texts(args, "fields")? {
        Some(list) if !list.is_empty() => {
            let mut out = Vec::with_capacity(list.len());
            for f in list {
                if f != "id" && schema.field(&f).is_none() {
                    return Err(Outcome::refused(
                        400,
                        format!("`{}` has no field `{f}`", schema.name),
                    ));
                }
                if f != "id" && !out.contains(&f) {
                    out.push(name(&f, "field")?.to_string());
                }
            }
            Ok(out)
        }
        _ => Ok(schema
            .fields
            .iter()
            .filter(|f| !matches!(f.ty, DataType::Vector(..) | DataType::Sparse(_)))
            .map(|f| f.name.clone())
            .collect()),
    }
}

/// A filter a model wrote, as the condition it is: parsed on its own, as
/// `get <collection> where <filter>`, and refused unless what was parsed is
/// a condition and nothing more -- no clause, no second statement, no
/// parenthesis that closes on the statement around it. The condition goes
/// into a statement built here, never its text, and its values are its own
/// `$1`, `$2`: those of `params`, ahead of the ones the tool adds.
fn filter(schema: &Schema, raw: Option<&str>, given: usize) -> Checked<Option<Expr>> {
    let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    let text = format!("get {} where {raw}", schema.name);
    let parsed = fenec_ql::parse_one(&text).and_then(|stmt| {
        match stmt.reads_vectors() && fenec_core::engine::exactly_for(schema, &stmt).text {
            true => fenec_ql::parse_exact(&text).map(|mut s| s.remove(0)),
            false => Ok(stmt),
        }
    });
    let refused = |why: String| {
        Outcome::refused(
            400,
            format!("the filter is a condition alone, such as `year >= $1 and tags has $2`: {why}"),
        )
    };
    let stmt = parsed.map_err(|e| refused(e.to_string()))?;
    let Statement::Select(mut sel) = stmt else {
        return Err(refused("it is not one".into()));
    };
    let cond = sel.filter.take();
    let bare = Select {
        collection: schema.name.clone(),
        ..Select::default()
    };
    if sel != bare || cond.is_none() {
        return Err(refused("it holds a clause past the condition".into()));
    }
    let named = highest_param(raw);
    if named > given {
        return Err(Outcome::refused(
            400,
            format!("the filter names ${named}, and `params` holds {given} values"),
        ));
    }
    Ok(cond)
}

/// The highest `$n` a text names outside its strings, as the studio counts
/// a typed clause's.
fn highest_param(text: &str) -> usize {
    let b = text.as_bytes();
    let (mut top, mut quote, mut i) = (0, None, 0);
    while i < b.len() {
        let c = b[i];
        match quote {
            Some(q) => {
                if c == b'\\' {
                    i += 1;
                } else if c == q {
                    quote = None;
                }
            }
            None if c == b'"' || c == b'\'' => quote = Some(c),
            None if c == b'$' => {
                let digits: String = b[i + 1..]
                    .iter()
                    .take_while(|d| d.is_ascii_digit())
                    .map(|&d| d as char)
                    .collect();
                top = top.max(digits.parse().unwrap_or(0));
            }
            None => {}
        }
        i += 1;
    }
    top
}

/// The `@text` field of a collection: the one named, or the only one.
fn text_field(schema: &Schema, asked: Option<&str>) -> Checked<String> {
    let texts: Vec<&str> = schema
        .fields
        .iter()
        .filter(|f| matches!(f.index, IndexKind::Text(_)))
        .map(|f| f.name.as_str())
        .collect();
    match (asked, texts.as_slice()) {
        (Some(a), _) if texts.contains(&a) => Ok(a.to_string()),
        (Some(a), _) => Err(Outcome::refused(
            400,
            format!("`{a}` is not an @text field of `{}`: {}", schema.name, listed(&texts)),
        )),
        (None, [one]) => Ok(one.to_string()),
        (None, []) => Err(Outcome::refused(
            400,
            format!(
                "`{}` has no @text field to search by words: search by `vector`, or read it with `query`",
                schema.name
            ),
        )),
        (None, many) => Err(Outcome::refused(
            400,
            format!("`{}` has several @text fields: name one in `text_field` ({})", schema.name, many.join(", ")),
        )),
    }
}

/// The `@hnsw` vector field of a collection and its dimension: the one
/// named, or the only one.
fn vector_field(schema: &Schema, asked: Option<&str>) -> Checked<(String, usize)> {
    let vectors: Vec<(&str, usize)> = schema
        .fields
        .iter()
        .filter(|f| matches!(f.index, IndexKind::Vector(_)))
        .filter_map(|f| match f.ty {
            DataType::Vector(dim, _) => Some((f.name.as_str(), dim)),
            _ => None,
        })
        .collect();
    let names: Vec<&str> = vectors.iter().map(|(n, _)| *n).collect();
    match (asked, vectors.as_slice()) {
        (Some(a), _) => vectors
            .iter()
            .find(|(n, _)| *n == a)
            .map(|(n, d)| (n.to_string(), *d))
            .ok_or_else(|| {
                Outcome::refused(
                    400,
                    format!(
                        "`{a}` is not an @hnsw vector field of `{}`: {}",
                        schema.name,
                        listed(&names)
                    ),
                )
            }),
        (None, [(n, d)]) => Ok((n.to_string(), *d)),
        (None, []) => Err(Outcome::refused(
            400,
            format!(
                "`{}` has no @hnsw vector field to search by an embedding: search by `text`",
                schema.name
            ),
        )),
        (None, _) => Err(Outcome::refused(
            400,
            format!(
                "`{}` has several vector fields: name one in `vector_field` ({})",
                schema.name,
                names.join(", ")
            ),
        )),
    }
}

fn listed(names: &[&str]) -> String {
    match names {
        [] => "it has none".into(),
        _ => format!("it has {}", names.join(", ")),
    }
}

fn dimension(v: &[f32], field: &str, dim: usize) -> Checked<()> {
    match v.len() == dim {
        true => Ok(()),
        false => Err(Outcome::refused(
            400,
            format!(
                "the vector holds {} numbers and `{field}` holds vector<{dim}>: embed with the model the collection was filled with",
                v.len()
            ),
        )),
    }
}

/// A statement built here, parsed: every name in it was checked and every
/// value is a parameter, so a parse that fails is this code's fault. From
/// `/query`'s cache of parsed texts, which a tool's texts -- the same few
/// shapes, their values apart -- hit as a client's do: parsed each time,
/// the `get` tool's row by id took 6.3 us in process against `/query`'s 1.3.
fn built(text: &str) -> Checked<Select> {
    crate::statements::text(text);
    match api::parsed(text).map(Arc::unwrap_or_clone) {
        Ok(Statement::Select(sel)) => Ok(sel),
        Ok(_) => Err(Outcome::refused(
            500,
            "a statement built for a read is not one",
        )),
        Err(e) => Err(Outcome::refused(
            500,
            format!("a statement built here: {e}"),
        )),
    }
}

// ------------------------------------------------------------ the tools

fn schema(ctx: &Ctx, args: Args) -> Checked<String> {
    known(args, &["collection"])?;
    let one = text(args, "collection")?;
    if let Some(c) = one {
        // One the token may not read is one that is not there.
        schema_of(ctx, c)?;
    }
    let g = crate::held::read(ctx.db);
    let names: Vec<String> = match one {
        Some(c) => vec![c.to_string()],
        // A collection of the server's own -- `_idempotency`, a sync's --
        // is noise to a model, and named by asking for it alone.
        None => g
            .collection_names()
            .into_iter()
            .filter(|n| !n.starts_with('_'))
            .filter(|n| ctx.who.scope().is_none_or(|s| s.readable(n)))
            .collect(),
    };
    let mut schemas = Vec::with_capacity(names.len());
    let mut out = String::from("{\"collections\":[");
    for (i, n) in names.iter().enumerate() {
        let Ok(c) = g.collection(n) else { continue };
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        json::escape_into(&mut out, n);
        // How many rows, to the server's token: a scoped one reads its own
        // alone, and the count of the rest is not its to know.
        if ctx.who.scope().is_none() {
            out.push_str(",\"rows\":");
            json::int_into(&mut out, c.stats().documents as i64);
        }
        out.push('}');
        schemas.push(shown_schema(ctx, &c.schema));
    }
    out.push_str("],\"fenecql\":");
    json::escape_into(&mut out, &crate::with_id_note(&schemas));
    out.push('}');
    Ok(out)
}

fn search(ctx: &Ctx, args: Args) -> Checked<String> {
    known(
        args,
        &[
            "collection",
            "text",
            "vector",
            "filter",
            "params",
            "limit",
            "fields",
            "text_field",
            "vector_field",
            "fuzzy",
            "prefix",
        ],
    )?;
    let collection = name(needed(args, "collection")?, "collection")?;
    let flag = |name: &str| match arg(args, name) {
        None | Some(Value::Bool(false)) => Ok(false),
        Some(Value::Bool(true)) => Ok(true),
        Some(_) => Err(Outcome::refused(400, format!("`{name}` is true or false"))),
    };
    let (fuzzy, prefix) = (flag("fuzzy")?, flag("prefix")?);
    let words = text(args, "text")?.map(str::trim).filter(|t| !t.is_empty());
    let embedding = vector(args, "vector")?;
    if words.is_none() && embedding.is_none() {
        return Err(Outcome::refused(
            400,
            "search takes `text`, `vector` or both: to read rows by a condition alone, use query",
        ));
    }
    let n = limit(args, ctx.opts, 10)?;
    let schema = schema_of(ctx, collection)?;
    let tf = match words {
        Some(_) => Some(text_field(&schema, text(args, "text_field")?)?),
        None => None,
    };
    let vf = match &embedding {
        Some(v) => {
            let (f, dim) = vector_field(&schema, text(args, "vector_field")?)?;
            dimension(v, &f, dim)?;
            Some(f)
        }
        None => None,
    };
    let wanted = fields(args, &schema)?;
    let mut params = params(args)?;
    let cond = filter(&schema, text(args, "filter")?, params.len())?;
    let mut sql = format!("get {collection} select id");
    for f in &wanted {
        sql.push_str(", ");
        sql.push_str(f);
    }
    let marked = tf
        .as_ref()
        .filter(|f| schema.field(f).is_some_and(|f| f.ty == DataType::Text));
    if let Some(f) = marked {
        // Thirty words around the most matches, the words found between
        // `**`: what a model reads as emphasis, and short.
        sql.push_str(&format!(", snippet({f}, 30, \"…\", \"**\", \"**\")"));
    }
    if let (Some(t), Some(f)) = (words, &tf) {
        params.push(Value::Text(t.to_string()));
        sql.push_str(&format!(" match {f} ${}", params.len()));
        if fuzzy {
            sql.push_str(" fuzzy");
        }
        if prefix {
            sql.push_str(" prefix");
        }
    }
    if let (Some(v), Some(f)) = (embedding, &vf) {
        params.push(Value::Vector(v));
        sql.push_str(&format!(" near {f} ${}", params.len()));
    }
    if tf.is_some() && vf.is_some() {
        sql.push_str(" fuse");
    }
    sql.push_str(&format!(" limit {n}"));
    let mut sel = built(&sql)?;
    sel.filter = cond;
    let resp = read(ctx, Statement::Select(sel), &params)?;
    let label = marked.map(|f| format!("snippet({f})"));
    let rename: Vec<(&str, &str)> = label.iter().map(|l| (l.as_str(), "snippet")).collect();
    rows(resp, true, &rename, "")
}

fn query(ctx: &Ctx, args: Args) -> Checked<String> {
    known(args, &["query", "params", "include_vectors"])?;
    let text = needed(args, "query")?;
    let given = params(args)?;
    let vectors = matches!(arg(args, "include_vectors"), Some(Value::Bool(true)));
    // The body `/query` would have been sent, read as it reads one: the
    // parameters' numbers as written where a json field needs them.
    let mut body = String::from("{\"query\":");
    json::escape_into(&mut body, text);
    body.push_str(",\"params\":");
    json::value_into(&mut body, &Value::List(given));
    body.push('}');
    let (stmt, params) = api::parse_query(&body)
        .and_then(|(s, p)| api::exactly(ctx.db, &body, s, p))
        .map_err(|e| refusal(&e))?;
    if !stmt.is_read_only() {
        return Err(Outcome::refused(
            400,
            match ctx.opts.write {
                true => "query runs reads: a put, insert, set or del goes through the write tool",
                false => "query runs reads, and this server takes no writes over MCP (--mcp-write)",
            },
        ));
    }
    let mut stmt = Arc::unwrap_or_clone(stmt);
    let max = ctx.opts.max_rows;
    // More rows than the bound would be fetched only to be refused: asked
    // for one past it, a read that has more says so with one row more.
    if let Statement::Select(sel) = &mut stmt {
        row_bound(sel, &params, max);
    }
    let resp = read(ctx, stmt, &params)?;
    if let Response2::Rows(rs) = &resp {
        if rs.answered().rows.len() > max {
            return Err(Outcome::refused(
                413,
                format!(
                    "more than {max} rows answer this query, this server's bound (--mcp-max-rows): \
                     refused rather than cut. Add `limit` (at most {max}), narrow the `where`, or \
                     count them (`count`, `group`)"
                ),
            ));
        }
    }
    rows(resp, vectors, &[], "")
}

/// `sel` asked for one row past `max` at most, so a read with more says so
/// with one row more and reads no more than that.
fn row_bound(sel: &mut Select, params: &[Value], max: usize) {
    let one_row = sel.count || (!sel.aggregate.is_empty() && sel.group.is_empty());
    let at = |c: Option<Count>| match c {
        None => None,
        Some(Count::N(n)) => Some(n),
        Some(Count::Param(i)) => match params.get(i) {
            Some(Value::Int(n)) if *n >= 0 => Some(*n as usize),
            _ => None,
        },
    };
    // A SQL join's rows are the flat ones, several a parent where a level
    // holds several: bounded there, and its parents to as many as that
    // page past its offset could need -- each parent kept answers a flat
    // row at least -- where an unbounded join probed every parent for an
    // 11-row page. A join read without flat rows, its `Flat` the names
    // alone, answers its parents' rows, bounded as any `get`'s.
    match sel.flat.as_deref_mut().filter(|f| !f.columns.is_empty()) {
        Some(f) => {
            if at(f.limit).or(at(sel.limit)).is_none_or(|n| n > max) {
                f.limit = Some(Count::N(max + 1));
            }
            let parents = (max + 1).saturating_add(at(Some(f.offset)).unwrap_or(0));
            if at(sel.limit).is_none_or(|n| n > parents) {
                sel.limit = Some(Count::N(parents));
            }
        }
        None => {
            if !one_row && at(sel.limit).is_none_or(|n| n > max) {
                sel.limit = Some(Count::N(max + 1));
            }
        }
    }
}

fn get(ctx: &Ctx, args: Args) -> Checked<String> {
    known(args, &["collection", "ids", "fields"])?;
    let collection = name(needed(args, "collection")?, "collection")?;
    let ids: Vec<i64> = match arg(args, "ids") {
        Some(Value::List(items)) if !items.is_empty() => items
            .iter()
            .map(|v| match v {
                Value::Int(n) if *n >= 0 => Ok(*n),
                _ => Err(Outcome::refused(
                    400,
                    "`ids` is a list of row ids, whole numbers",
                )),
            })
            .collect::<Checked<_>>()?,
        _ => {
            return Err(Outcome::refused(
                400,
                "`ids` is a list of row ids: [12, 40]",
            ))
        }
    };
    if ids.len() > ctx.opts.max_rows {
        return Err(Outcome::refused(
            413,
            format!(
                "{} ids, past this server's bound of {} rows (--mcp-max-rows): ask for fewer",
                ids.len(),
                ctx.opts.max_rows
            ),
        ));
    }
    let schema = schema_of(ctx, collection)?;
    let wanted = fields(args, &schema)?;
    let mut sql = format!("get {collection} select id");
    for f in &wanted {
        sql.push_str(", ");
        sql.push_str(f);
    }
    sql.push_str(" where id in [");
    for i in 0..ids.len() {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push_str(&format!("${}", i + 1));
    }
    sql.push_str(&format!("] limit {}", ids.len()));
    let sel = built(&sql)?;
    let params: Vec<Value> = ids.iter().map(|&n| Value::Int(n)).collect();
    let resp = read(ctx, Statement::Select(sel), &params)?;
    let found: Vec<u64> = match &resp {
        Response2::Rows(rs) => rs.rows.iter().map(|r| r.id).collect(),
        _ => Vec::new(),
    };
    let mut missing = String::from(",\"missing\":[");
    let mut first = true;
    for id in &ids {
        if !found.contains(&(*id as u64)) {
            if !first {
                missing.push(',');
            }
            first = false;
            json::int_into(&mut missing, *id);
        }
    }
    missing.push(']');
    rows(resp, true, &[], &missing)
}

fn write(ctx: &Ctx, args: Args) -> Checked<String> {
    known(args, &["statement", "params"])?;
    let text = needed(args, "statement")?;
    let params = params(args)?;
    // Read for its kind alone; `/query` reads it again, from its cache.
    let stmt = fenec_ql::parse_one(text).map_err(|e| Outcome::refused(400, e.to_string()))?;
    match stmt {
        Statement::Put { .. } | Statement::Update { .. } | Statement::Delete { .. } => {}
        // A named statement's tool is its own, `call_<name>`, once calls
        // run; nothing runs one yet.
        Statement::Call(_) => return Err(Outcome::refused(400, fenec_core::named::NOT_BUILT)),
        s if s.is_read_only() => {
            return Err(Outcome::refused(400, "write runs a put, insert, set or del: a read goes through query"))
        }
        // The schema is the operator's: `/_schema/apply` or the shell,
        // under the server's token and a person's eye.
        _ => {
            return Err(Outcome::refused(
                403,
                "schema changes and maintenance are not made over MCP: write runs a put, insert, set or del",
            ))
        }
    }
    answered(through_query(ctx, text, &params))
}

// ------------------------------------------------------------ memory

/// The memory collection, as `remember` makes it at its first memory: the
/// text searched by its words, the tags by `has`, `owner` for a token's
/// subject where a policy keeps each user's apart, `at` when it was kept,
/// `expires` when it goes -- `@ttl(1ms)`: a row is out of every read the
/// moment its time passes, and the sweeper deletes it -- and the vector
/// when the first memory comes with one.
pub fn memory_schema(collection: &str, dim: Option<usize>) -> String {
    let mut sql = format!(
        "create collection if not exists {collection} (text text required @text, tags [text], \
         owner text @hash, at timestamp @sorted, expires timestamp @ttl(1ms)"
    );
    if let Some(d) = dim {
        sql.push_str(&format!(", vector vector<{d}> @hnsw(cosine)"));
    }
    sql.push(')');
    sql
}

/// A duration as `@ttl` spells one: 30m, 12h, 7d, in milliseconds.
fn duration(text: &str) -> Option<i64> {
    let t = text.trim();
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    let n: i64 = t[..digits].parse().ok()?;
    let unit: i64 = match t[digits..].trim() {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 7 * 86_400_000,
        _ => return None,
    };
    n.checked_mul(unit).filter(|&ms| ms > 0)
}

fn memory<'a>(ctx: &'a Ctx) -> &'a str {
    ctx.opts.memory.as_deref().unwrap_or("memories")
}

fn remember(ctx: &Ctx, args: Args) -> Checked<String> {
    known(args, &["text", "vector", "tags", "expires_in"])?;
    let m = memory(ctx);
    let words = needed(args, "text")?.trim();
    if words.is_empty() {
        return Err(Outcome::refused(
            400,
            "`text` is the memory, and it is empty",
        ));
    }
    let embedding = vector(args, "vector")?;
    let tags = texts(args, "tags")?;
    let expires = match text(args, "expires_in")? {
        None => None,
        Some(d) => Some(duration(d).ok_or_else(|| {
            Outcome::refused(
                400,
                format!("`expires_in` is a duration such as 30m, 12h or 7d, not `{d}`"),
            )
        })?),
    };
    // The collection, made at the first memory, and its vector field at
    // the first memory with a vector.
    let held = {
        let g = crate::held::read(ctx.db);
        g.collection(m).ok().map(|c| c.schema.clone())
    };
    let make = |sql: &str| -> Checked<()> {
        answered(through_query(ctx, sql, &[]))
            .map(drop)
            .map_err(|e| match e {
                Outcome::Refused {
                    status,
                    why,
                    retry_after,
                    reason,
                } => Outcome::Refused {
                    status,
                    why: format!(
                        "{why}: the memory collection `{m}` cannot be made with this token; the \
                     server's operator makes it once with `{sql}`"
                    ),
                    retry_after,
                    reason,
                },
                done => done,
            })
    };
    let vector_field = match &held {
        None => {
            make(&memory_schema(m, embedding.as_ref().map(Vec::len)))?;
            embedding.as_ref().map(|_| "vector".to_string())
        }
        Some(schema) => match &embedding {
            None => None,
            Some(v) => match vector_field(schema, None) {
                Ok((f, dim)) => {
                    dimension(v, &f, dim)?;
                    Some(f)
                }
                Err(_) if schema.field("vector").is_none() => {
                    make(&format!(
                        "alter collection {m} add field vector vector<{}> @hnsw(cosine)",
                        v.len()
                    ))?;
                    Some("vector".to_string())
                }
                Err(e) => return Err(e),
            },
        },
    };
    let mut params = vec![
        Value::Text(words.to_string()),
        tags.map_or(Value::Null, |t| {
            Value::List(t.into_iter().map(Value::Text).collect())
        }),
        expires.map_or(Value::Null, Value::Int),
    ];
    let mut sql = format!("insert into {m} {{text: $1, tags: $2, at: now(), expires: now() + $3");
    if let (Some(v), Some(f)) = (embedding, vector_field) {
        params.push(Value::Vector(v));
        sql.push_str(&format!(", {f}: $4"));
    }
    sql.push('}');
    answered(through_query(ctx, &sql, &params))?;
    let mut out = String::from("{\"remembered\":1,\"collection\":");
    json::escape_into(&mut out, m);
    out.push('}');
    Ok(out)
}

fn recall(ctx: &Ctx, args: Args) -> Checked<String> {
    known(args, &["text", "vector", "tags", "limit"])?;
    let m = memory(ctx);
    let words = text(args, "text")?.map(str::trim).filter(|t| !t.is_empty());
    let embedding = vector(args, "vector")?;
    let tags = texts(args, "tags")?.unwrap_or_default();
    let n = limit(args, ctx.opts, 5)?;
    let held = {
        let g = crate::held::read(ctx.db);
        g.collection(m).ok().map(|c| c.schema.clone())
    };
    // Nothing remembered yet: the collection is made at the first memory.
    let Some(schema) = held else {
        return Ok("{\"memories\":[],\"count\":0}".into());
    };
    let schema = schema_of(ctx, &schema.name)?;
    let tf = match words {
        Some(_) => Some(text_field(&schema, None)?),
        None => None,
    };
    // A vector where the memories hold none is passed over: the words
    // still find them.
    let vf = match &embedding {
        Some(v) => match vector_field(&schema, None) {
            Ok((f, dim)) => {
                dimension(v, &f, dim)?;
                Some(f)
            }
            Err(_) => None,
        },
        None => None,
    };
    // The owner is the token's own, or null without a policy: noise to the
    // model either way.
    let mut wanted = fields(&[], &schema)?;
    wanted.retain(|f| f != "owner");
    let mut sql = format!("get {m} select id");
    for f in &wanted {
        sql.push_str(", ");
        sql.push_str(f);
    }
    let mut params: Vec<Value> = Vec::new();
    if !tags.is_empty() {
        if schema.field("tags").is_none() {
            return Err(Outcome::refused(400, format!("`{m}` has no `tags` field")));
        }
        for (i, t) in tags.into_iter().enumerate() {
            sql.push_str(if i == 0 { " where " } else { " and " });
            params.push(Value::Text(t));
            sql.push_str(&format!("tags has ${}", params.len()));
        }
    }
    if let (Some(t), Some(f)) = (words, &tf) {
        params.push(Value::Text(t.to_string()));
        sql.push_str(&format!(" match {f} ${}", params.len()));
    }
    if let (Some(v), Some(f)) = (embedding, &vf) {
        params.push(Value::Vector(v));
        sql.push_str(&format!(" near {f} ${}", params.len()));
    }
    match (tf.is_some(), vf.is_some()) {
        (true, true) => sql.push_str(" fuse"),
        // The latest first, where nothing ranks them, and the one kept
        // last of a millisecond first too: `at` alone left two memories of
        // one millisecond in ascending id, the older first, and the test
        // holding the order failed on a quick runner.
        (false, false) if schema.field("at").is_some() => sql.push_str(" order at desc, id desc"),
        (false, false) => sql.push_str(" order id desc"),
        _ => {}
    }
    sql.push_str(&format!(" limit {n}"));
    let sel = built(&sql)?;
    let resp = read(ctx, Statement::Select(sel), &params)?;
    let out = rows(resp, false, &[], "")?;
    // `{"rows": ...}` as `{"memories": ...}`.
    Ok(out.replacen("{\"rows\":", "{\"memories\":", 1))
}

fn forget(ctx: &Ctx, args: Args) -> Checked<String> {
    known(args, &["id"])?;
    let m = memory(ctx);
    let Some(id) = int(args, "id")?.filter(|n| *n >= 0) else {
        return Err(Outcome::refused(
            400,
            "`id` is a memory's id, which recall gives",
        ));
    };
    let sql = format!("del {m} where id = $1 require 1");
    match answered(through_query(ctx, &sql, &[Value::Int(id)])) {
        Ok(_) => Ok("{\"forgotten\":1}".into()),
        Err(Outcome::Refused {
            status: 412 | 404, ..
        }) => Err(Outcome::refused(
            404,
            format!("no memory {id}: recall gives the ids there are"),
        )),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A join bounded for `--mcp-max-rows` reads no more parents than its
    /// page could need -- each parent kept answers a flat row at least --
    /// where the flat rows alone were bounded and every parent probed; and
    /// a join read without flat rows is bounded as a `get` is.
    #[test]
    fn a_bounded_join_reads_as_many_parents_as_its_page_needs() {
        let mut db = Database::new();
        for sql in [
            "create collection users (n int)",
            "create collection orders (uid int @hash, k int)",
        ] {
            db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
        }
        for i in 0..50 {
            let put = format!("put users {{n: {i}}}");
            db.execute(&fenec_ql::parse_one(&put).unwrap()).unwrap();
            let put = format!(
                "put orders [{{uid: {}, k: 1}}, {{uid: {}, k: 2}}]",
                i + 1,
                i + 1
            );
            db.execute(&fenec_ql::parse_one(&put).unwrap()).unwrap();
        }
        let probed = |sql: &str| {
            let Statement::Select(mut sel) = fenec_ql::parse_one(sql).unwrap() else {
                panic!()
            };
            row_bound(&mut sel, &[], 10);
            let r = db.query(&Statement::Explain(sel), &[]).unwrap();
            let steps = format!("{:?}", r.rows().unwrap().rows);
            // "from the parent side, 11 of 50 parents probed in ..."
            let at = steps.find("parent side, ").expect(&steps) + "parent side, ".len();
            let n: String = steps[at..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            n.parse::<usize>().unwrap()
        };
        // A user's two orders: rows of the orders, many to a parent.
        assert_eq!(
            probed("SELECT u.n, o.k FROM users u JOIN orders o ON o.uid = u.id"),
            11
        );
        assert_eq!(
            probed("SELECT u.n, o.k FROM users u JOIN orders o ON o.uid = u.id LIMIT 20 OFFSET 3"),
            14
        );
        // An order's user, found by its `id`: the page is the parents'.
        assert_eq!(
            probed("SELECT o.k, u.n FROM orders o JOIN users u ON u.id = o.uid"),
            11
        );
        // The orders alone, the users a filter: a `get` bounded.
        let Statement::Select(mut sel) = fenec_ql::parse_one(
            "SELECT o.k FROM orders o JOIN users u ON u.id = o.uid WHERE u.n > 5",
        )
        .unwrap() else {
            panic!()
        };
        row_bound(&mut sel, &[], 10);
        assert_eq!(sel.limit, Some(Count::N(11)));
    }

    #[test]
    fn a_duration_reads_as_ttl_spells_one() {
        assert_eq!(duration("30m"), Some(1_800_000));
        assert_eq!(duration("7d"), Some(604_800_000));
        assert_eq!(duration("12 h"), Some(43_200_000));
        assert_eq!(duration("0s"), None);
        assert_eq!(duration("soon"), None);
        assert_eq!(duration("5y"), None);
    }

    #[test]
    fn the_highest_parameter_is_counted_outside_strings() {
        assert_eq!(highest_param("year >= $1 and tags has $2"), 2);
        assert_eq!(highest_param("title = '$9' and n = $3"), 3);
        assert_eq!(highest_param(r#"t = "a\"$7" or x = $1"#), 1);
        assert_eq!(highest_param("n = 1"), 0);
    }

    #[test]
    fn a_name_is_one_fenecql_writes() {
        assert!(name("notes", "c").is_ok());
        assert!(name("çiçek_2", "c").is_ok());
        assert!(name("_sync", "c").is_ok());
        for bad in ["", "2x", "a b", "a;del", "a)", "a--"] {
            assert!(name(bad, "c").is_err(), "{bad}");
        }
    }
}
