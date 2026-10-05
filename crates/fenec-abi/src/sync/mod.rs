//! A replica that syncs with a server, as a state machine with no I/O of
//! its own: what the native bindings' `Fenec.sync` runs, each over its
//! platform's HTTP client.
//!
//! ## Why the network is the binding's
//!
//! iOS (ATS) and Android refuse cleartext HTTP by default, and a server is
//! reached through TLS in front of it (Caddy, Cloudflare). fenecdb's own
//! HTTP client has no TLS and should not grow one: the platform's client
//! has the system's trust store, its proxies, its power management and its
//! certificate pinning. So the requests and the change stream are made by
//! `URLSession`, `HttpURLConnection` and `dart:io`'s `HttpClient`, and
//! everything else -- what to ask, what an answer means, what is written --
//! is here, once, with one suite of tests (`tests/sync.rs`).
//!
//! ## Events in, actions out
//!
//! A binding feeds what happened -- an answer to a request
//! ([`Sync::response`]), a stream's status, bytes and end ([`Sync::opened`],
//! [`Sync::bytes`], [`Sync::closed`]), a timer it was asked for
//! ([`Sync::timer`]), the network or a new token ([`Sync::signal`]) -- and
//! performs what [`Sync::actions`] hands back, a JSON array:
//!
//! - `{"do":"request","id":N,"method":"GET"|"POST","url":..,"headers":{..},"body":..}`
//! - `{"do":"stream","id":N,"url":..,"headers":{..}}`: an SSE stream, its
//!   bytes fed as they come
//! - `{"do":"cancel","id":N}`: the stream ends
//! - `{"do":"wait","id":N,"ms":M}`: a timer
//! - `{"do":"token"}`: the server said 401, a fresh token is wanted
//! - `{"do":"changed"}`: the replica changed, live queries look again
//! - `{"do":"refused","status":S,"message":..,"query":..}`: the server
//!   refused a write, which was put back
//! - `{"do":"status"}`: [`Sync::status`] has something new to say
//!
//! A write through `fenec_query` on a synced handle comes here
//! ([`Sync::write`]): applied to the replica at once, and sent after.
//!
//! ## What the file holds
//!
//! The replica's collections, and three of the sync's own, written in the
//! same blocks as what they describe, so a crash leaves the two agreeing:
//! `_sync_shapes` (each shape's cursor and whether it was seeded),
//! `_sync_queue` (the writes the server has not answered, each with its
//! idempotency key and what puts it back) and `_sync_temps` (the rows an
//! optimistic insert made under a temporary id, until the server's copy
//! arrives). A replica opened again resumes from its cursors, and sends its
//! queue first, under the keys it was written with.

pub mod shape;
pub mod sse;

use crate::Prepared;
use fenec_core::json;
use fenec_core::prelude::*;
use fenec_core::query::{eval, EvalCtx, RowAccess};

/// Where the temporary ids of optimistic rows start: far from a server's,
/// as `web/fenec.js` has it.
pub const TEMP_BASE: i64 = 1 << 52;

/// Backoff: from this, doubling, up to [`BACKOFF_MAX_MS`], with 30% jitter
/// so a server back up is not met by every client at once.
const BACKOFF_MS: u64 = 250;
const BACKOFF_MAX_MS: u64 = 15_000;

/// The sync's own collections, and the statements over them, parsed once.
const STATE: &[&str] = &[
    "create collection if not exists _sync_shapes (collection text, shape text, cursor int, seeded bool)",
    "create collection if not exists _sync_queue (path text, body text, key text, undo text, label text)",
    "create collection if not exists _sync_temps (collection text, key text, op int, until int)",
];
const PUT_SHAPE: usize = 0;
const PUT_OP: usize = 1;
const DEL_OP: usize = 2;
const PUT_TEMP: usize = 3;
const DEL_TEMP: usize = 4;
const STATEMENTS: &[&str] = &[
    "put _sync_shapes {id: $1, collection: $2, shape: $3, cursor: $4, seeded: $5}",
    "put _sync_queue {id: $1, path: $2, body: $3, key: $4, undo: $5, label: $6}",
    "del _sync_queue where id = $1",
    "put _sync_temps {id: $1, collection: $2, key: $3, op: $4, until: $5}",
    "del _sync_temps where id = $1",
];

/// Whether a collection is the sync's own.
fn own(name: &str) -> bool {
    matches!(name, "_sync_shapes" | "_sync_queue" | "_sync_temps")
}

struct Shape {
    spec: shape::Spec,
    /// The server, the filter and the projection: a replica opened with
    /// another holds rows it should not, and is seeded again.
    fingerprint: String,
    row: i64,
    cursor: u64,
    seeded: bool,
    stream: Option<u64>,
    connected: bool,
    attempt: u32,
    waiting: bool,
    frames: sse::Frames,
    /// The server's schema changed: the collection is made again from it
    /// before the stream opens.
    rebuild: bool,
    /// Opened without `since` though seeded: the whole shape again, after
    /// the collection was made anew.
    fresh: bool,
}

/// A write the server has not answered.
struct Op {
    n: i64,
    path: String,
    body: String,
    key: String,
    undo: String,
    label: String,
}

/// A row an optimistic insert made, under a temporary id.
struct Temp {
    collection: String,
    key: String,
    temp: i64,
    /// The write it came with while the server has not answered it; 0 once
    /// it has.
    op: i64,
    /// Once answered, the change the server's answer left the database at:
    /// a stream past it holds the server's copy, or would, and a temporary
    /// row still here then is one the shape does not hold.
    until: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum Timer {
    Stream(usize),
    Send,
    Schema,
}

struct Fault {
    message: String,
    status: u16,
    /// A connection's, cleared once the replica is back: a refusal stays.
    connection: bool,
}

/// A small generator for keys, seeded by the binding from the platform's
/// secure source: a business key decides which rows are one, so two
/// devices must not make the same (splitmix64, two lanes for 128 bits).
struct Rng(u64, u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0 ^ self.1.rotate_left(17);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        self.1 = self.1.wrapping_add(z | 1);
        z ^ (z >> 31)
    }

    /// A version-4 UUID's text, as `crypto.randomUUID` makes the JS layer's.
    fn uuid(&mut self) -> String {
        let (a, b) = (self.next(), self.next());
        let a = (a & !0xf000) | 0x4000;
        let b = (b & !(0xc << 60)) | (0x8 << 60);
        format!(
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            a >> 32,
            (a >> 16) & 0xffff,
            a & 0xffff,
            b >> 48,
            b & 0xffff_ffff_ffff
        )
    }
}

/// A replica's sync: its shapes, its queue and its connections.
pub struct Sync {
    url: String,
    token: Option<String>,
    shapes: Vec<Shape>,
    queue: Vec<Op>,
    temps: Vec<Temp>,
    next_temp: i64,
    next_n: i64,
    state: Vec<Statement>,
    rng: Rng,
    next_id: u64,
    outbox: Vec<String>,
    timers: Vec<(u64, Timer)>,
    /// The request under way: its id and the write it sends.
    sending: Option<(u64, i64)>,
    send_attempt: u32,
    send_waiting: bool,
    schema_req: Option<u64>,
    schema_attempt: u32,
    schema_waiting: bool,
    /// Writes left from before are sent before the streams open, so what
    /// they bring holds them; a write the server cannot take now lets the
    /// streams go ahead.
    flushing: bool,
    /// Told the network is gone (`{"online":false}`).
    offline: bool,
    /// Waiting for a token after a 401.
    paused: bool,
    stopped: bool,
    fault: Option<Fault>,
    last_state: &'static str,
}

/// An expression with no row: a value or a parameter.
struct NoRow;
impl RowAccess for NoRow {
    fn id(&self) -> DocId {
        0
    }
    fn field(&mut self, name: &str) -> Result<Value> {
        Err(Error::Query(format!(
            "field `{name}` cannot be accessed in this context"
        )))
    }
}

/// A list of the top-level members of a JSON object, each value's text as
/// it stands: a seed's rows are read where they lie, not parsed twice.
fn members(src: &str) -> Result<Vec<(String, &str)>> {
    let b = src.as_bytes();
    let bad = || Error::Query("malformed JSON from the server".into());
    let mut i = 0;
    let ws = |i: &mut usize| {
        while b.get(*i).is_some_and(|c| c.is_ascii_whitespace()) {
            *i += 1
        }
    };
    ws(&mut i);
    if b.get(i) != Some(&b'{') {
        return Err(bad());
    }
    i += 1;
    let mut out = Vec::new();
    loop {
        ws(&mut i);
        match b.get(i) {
            Some(b'}') => return Ok(out),
            Some(b',') => {
                i += 1;
                continue;
            }
            Some(b'"') => {}
            _ => return Err(bad()),
        }
        let start = i;
        i = skip(b, i).ok_or_else(bad)?;
        let key = match json::parse(&src[start..i])? {
            Value::Text(k) => k,
            _ => return Err(bad()),
        };
        ws(&mut i);
        if b.get(i) != Some(&b':') {
            return Err(bad());
        }
        i += 1;
        ws(&mut i);
        let start = i;
        i = skip(b, i).ok_or_else(bad)?;
        out.push((key, &src[start..i]));
    }
}

/// Past the JSON value at `i`.
fn skip(b: &[u8], mut i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => {
            i += 1;
            loop {
                match *b.get(i)? {
                    b'\\' => i += 2,
                    b'"' => return Some(i + 1),
                    _ => i += 1,
                }
            }
        }
        b'{' | b'[' => {
            let mut depth = 0usize;
            loop {
                match *b.get(i)? {
                    b'"' => {
                        i = skip(b, i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        _ => {
            while b
                .get(i)
                .is_some_and(|c| !matches!(c, b',' | b'}' | b']') && !c.is_ascii_whitespace())
            {
                i += 1;
            }
            Some(i)
        }
    }
}

fn field<'a>(m: &[(String, &'a str)], name: &str) -> Option<&'a str> {
    m.iter().find(|(k, _)| k == name).map(|(_, v)| *v)
}

/// The ids of a JSON array of them.
fn ids(src: &str) -> Result<Vec<DocId>> {
    match json::parse_json(src)? {
        Value::List(l) => l
            .iter()
            .map(|v| match v {
                Value::Int(n) if *n >= 0 => Ok(*n as DocId),
                _ => Err(Error::Query("an id that is not one".into())),
            })
            .collect(),
        _ => Err(Error::Query("ids that are not a list".into())),
    }
}

/// The json fields of a collection, which a document's JSON keeps as
/// written.
fn json_fields(db: &Database, collection: &str) -> Vec<String> {
    db.collection(collection)
        .map(|c| {
            c.schema
                .fields
                .iter()
                .filter(|f| f.ty == DataType::Json)
                .map(|f| f.name.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Documents as they came in JSON, each value a literal.
fn put_docs(collection: &str, docs: Vec<Vec<(String, Value)>>) -> Statement {
    Statement::Put {
        collection: collection.to_string(),
        docs: docs
            .into_iter()
            .map(|d| d.into_iter().map(|(k, v)| (k, Expr::Lit(v))).collect())
            .collect(),
        insert: false,
        if_absent: false,
        docs_param: None,
        else_set: None,
        require: None,
    }
}

fn del_ids(collection: &str, ids: &[DocId]) -> Statement {
    Statement::Delete {
        collection: collection.to_string(),
        filter: Some(Expr::In(
            Box::new(Expr::Field("id".into())),
            ids.iter()
                .map(|&i| Expr::Lit(Value::Int(i as i64)))
                .collect(),
        )),
        require: None,
    }
}

/// A key's text, as `String(k)` makes it in JS: a text as it is, anything
/// else as its JSON.
fn key_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Text(s) => Some(s.clone()),
        v => Some(json::to_string(v)),
    }
}

fn rows_json(rs: &ResultSet) -> String {
    let mut out = String::new();
    json::rows_array_into(&mut out, rs);
    out
}

fn text_of(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        _ => String::new(),
    }
}

fn int_of(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        Value::Float(f) => *f as i64,
        _ => 0,
    }
}

/// Whether every field `docs` name is one of the collection's.
fn fits(db: &Database, collection: &str, docs: &[Vec<(String, Value)>]) -> bool {
    let Ok(c) = db.collection(collection) else {
        return false;
    };
    docs.iter()
        .flatten()
        .all(|(k, _)| k == "id" || c.schema.fields.iter().any(|f| &f.name == k))
}

/// The collection made again by `create`, the server's schema, its rows
/// kept in the fields it still has: the replica reads as it did until the
/// seed writes the shape over it, and the rows of writes not yet answered
/// stay as a seed keeps them.
fn remake(db: &mut Database, collection: &str, create: &Statement) -> Result<()> {
    let held = match db.execute_with(
        &Statement::Select(Select {
            collection: collection.to_string(),
            ..Default::default()
        }),
        &[],
    )? {
        Response::Rows(rs) => rs,
        _ => ResultSet::default(),
    };
    db.execute_with(
        &fenec_ql::parse_one(&format!("drop collection {collection}"))?,
        &[],
    )?;
    db.execute_with(create, &[])?;
    let schema = db.collection(collection)?.schema.clone();
    let docs: Vec<Vec<(String, Value)>> = held
        .rows
        .iter()
        .map(|r| {
            let mut d = vec![("id".to_string(), Value::Int(r.id as i64))];
            for (k, v) in held.columns.iter().zip(&r.values) {
                if k != "id" && !v.is_null() && schema.fields.iter().any(|f| &f.name == k) {
                    d.push((k.clone(), v.clone()));
                }
            }
            d
        })
        .collect();
    if !docs.is_empty() {
        db.execute_with(&put_docs(collection, docs), &[])?;
    }
    Ok(())
}

/// `{"query":..,"params":[..]}`, a `POST /query` body and a `/batch` line.
fn query_body(text: &str, params: &[Value]) -> String {
    let mut out = String::from("{\"query\":");
    json::escape_into(&mut out, text);
    out.push_str(",\"params\":[");
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json::value_into(&mut out, p);
    }
    out.push_str("]}");
    out
}

/// `{a: $1, ...}` of fields whose values are known: every value a
/// parameter, so nothing of a value reaches the text.
fn render_doc(d: &[(String, Value)], params: &mut Vec<Value>) -> String {
    let mut text = String::from("{");
    for (j, (k, v)) in d.iter().enumerate() {
        if j > 0 {
            text.push_str(", ");
        }
        params.push(v.clone());
        text.push_str(&format!("{k}: ${}", params.len()));
    }
    text.push('}');
    text
}

/// `put c [{a: $1, ...}, ...]`.
fn render_put(
    collection: &str,
    insert: bool,
    docs: &[Vec<(String, Value)>],
) -> (String, Vec<Value>) {
    let mut params = Vec::new();
    let docs: Vec<String> = docs.iter().map(|d| render_doc(d, &mut params)).collect();
    let verb = if insert { "insert" } else { "put" };
    // As the builders write it, one document bare: the request is the same
    // text from every platform (`integrations/sync-scenarios.json`).
    let body = match docs.as_slice() {
        [one] => one.clone(),
        _ => format!("[{}]", docs.join(", ")),
    };
    (format!("{verb} {collection} {body}"), params)
}

/// A `get ... require` beside a synced write: the guard would be the
/// replica's count, and the server -- whose rows the write lands on --
/// never sent it. The browser's `batch()` refuses one in the same words.
const GUARDED: &str = "a `get ... require` counts the replica's rows and is not sent: a write \
                       guarded by a read goes to the server itself";

/// A write with `require` that reaches a row whose insert the server has not
/// answered: refused before anything is applied, as the browser's sync
/// refuses it (`integrations/sync-scenarios.json`).
/// An upsert into a synced collection whose documents do not each name
/// the shape's key or an id, refused before anything is applied, as the
/// browser's sync refuses it (`integrations/sync-scenarios.json`).
const UPSERT_KEY: &str = "an upsert into a synced collection names each document's key or id: \
                          the replica finds the row by it, and the server's copy is matched by it";

const REQUIRE_UNANSWERED: &str =
    "a write with `require` cannot reach a row whose insert the server has not answered yet";

/// What [`Sync::write`] made of one statement.
struct Applied {
    /// The lines the server is sent: a text and its parameters.
    lines: Vec<(String, Vec<Value>)>,
    /// What puts the replica back: `{"c":..,"del":[..],"put":[..]}`.
    undo: String,
    response: Response,
    temps: Vec<(String, i64)>,
}

impl Sync {
    /// Attaches a sync to `db`: `config` is
    /// `{"url":..,"token":..,"seed":"<hex>","shapes":[{collection,where?,select?,key?}]}`.
    /// Makes the sync's own collections and reads what they hold; asks
    /// for nothing yet -- [`Sync::actions`] says what to do first.
    pub fn start(db: &mut Database, config: &str) -> Result<Sync> {
        let cfg = json::parse_json(config)?;
        let url = match shape::member(&cfg, "url") {
            Some(Value::Text(u)) if !u.is_empty() => u.trim_end_matches('/').to_string(),
            _ => return Err(Error::Query("sync: `url` is required".into())),
        };
        let token = match shape::member(&cfg, "token") {
            Some(Value::Text(t)) if !t.is_empty() => Some(t.clone()),
            _ => None,
        };
        let seed = match shape::member(&cfg, "seed") {
            Some(Value::Text(h)) => h.clone(),
            _ => String::new(),
        };
        let lane = |s: &str| u64::from_str_radix(s, 16).unwrap_or(0);
        let rng = Rng(
            lane(seed.get(..16).unwrap_or(&seed)),
            lane(seed.get(16..32).unwrap_or("")) ^ 0x6a09_e667_f3bc_c908,
        );
        let mut shapes: Vec<Shape> = Vec::new();
        if let Some(Value::List(list)) = shape::member(&cfg, "shapes") {
            for s in list {
                let spec = shape::spec(s)?;
                if own(&spec.collection) {
                    return Err(Error::Query(format!(
                        "`{}` is the sync's own collection",
                        spec.collection
                    )));
                }
                if shapes.iter().any(|x| x.spec.collection == spec.collection) {
                    return Err(Error::Query(format!(
                        "two shapes for `{}`: one collection per shape (a seed has to say \
                         \"this is the whole collection\")",
                        spec.collection
                    )));
                }
                let mut fp = format!("{url}|");
                for (k, v) in &spec.params {
                    fp.push_str(&format!("{k}={v}&"));
                }
                if let Some(sel) = &spec.select {
                    fp.push_str(&format!("|{}", sel.join(",")));
                }
                shapes.push(Shape {
                    spec,
                    fingerprint: fp,
                    row: 0,
                    cursor: 0,
                    seeded: false,
                    stream: None,
                    connected: false,
                    attempt: 0,
                    waiting: false,
                    frames: Default::default(),
                    rebuild: false,
                    fresh: false,
                });
            }
        }
        if shapes.is_empty() {
            return Err(Error::Query("at least one shape is required".into()));
        }

        // The sync's collections, made in one block.
        db.begin()?;
        for text in STATE {
            if let Err(e) = db.execute_with(&fenec_ql::parse_one(text)?, &[]) {
                db.rollback();
                return Err(e);
            }
        }
        db.commit()?;
        let state = STATEMENTS
            .iter()
            .map(|s| fenec_ql::parse_one(s))
            .collect::<Result<Vec<_>>>()?;

        let mut sync = Sync {
            url,
            token,
            shapes,
            queue: Vec::new(),
            temps: Vec::new(),
            next_temp: TEMP_BASE,
            next_n: 1,
            state,
            rng,
            next_id: 1,
            outbox: Vec::new(),
            timers: Vec::new(),
            sending: None,
            send_attempt: 0,
            send_waiting: false,
            schema_req: None,
            schema_attempt: 0,
            schema_waiting: false,
            flushing: false,
            offline: false,
            paused: false,
            stopped: false,
            fault: None,
            last_state: "",
        };
        sync.load(db)?;
        sync.flushing = !sync.queue.is_empty();
        sync.kick(db);
        Ok(sync)
    }

    fn rows(db: &mut Database, text: &str) -> Result<ResultSet> {
        match db.execute_with(&fenec_ql::parse_one(text)?, &[])? {
            Response::Rows(rs) => Ok(rs),
            _ => Ok(ResultSet::default()),
        }
    }

    /// What the file kept: each shape's cursor, the queue and the
    /// temporary rows.
    fn load(&mut self, db: &mut Database) -> Result<()> {
        let rs = Self::rows(
            db,
            "get _sync_shapes select collection, shape, cursor, seeded",
        )?;
        let mut top = 0;
        for r in &rs.rows {
            top = top.max(r.id as i64);
            let c = text_of(&r.values[0]);
            if let Some(s) = self.shapes.iter_mut().find(|s| s.spec.collection == c) {
                s.row = r.id as i64;
                if text_of(&r.values[1]) == s.fingerprint {
                    s.cursor = int_of(&r.values[2]) as u64;
                    s.seeded = matches!(r.values[3], Value::Bool(true));
                }
            }
        }
        for s in &mut self.shapes {
            if s.row == 0 {
                top += 1;
                s.row = top;
            }
        }
        let rs = Self::rows(db, "get _sync_queue select path, body, key, undo, label")?;
        let mut ops: Vec<Op> = rs
            .rows
            .iter()
            .map(|r| Op {
                n: r.id as i64,
                path: text_of(&r.values[0]),
                body: text_of(&r.values[1]),
                key: text_of(&r.values[2]),
                undo: text_of(&r.values[3]),
                label: text_of(&r.values[4]),
            })
            .collect();
        ops.sort_unstable_by_key(|o| o.n);
        self.next_n = ops.last().map_or(1, |o| o.n + 1);
        self.queue = ops;
        let rs = Self::rows(db, "get _sync_temps select collection, key, op, until")?;
        self.temps = rs
            .rows
            .iter()
            .map(|r| Temp {
                collection: text_of(&r.values[0]),
                key: text_of(&r.values[1]),
                temp: r.id as i64,
                op: int_of(&r.values[2]),
                until: int_of(&r.values[3]) as u64,
            })
            .collect();
        self.next_temp = self
            .temps
            .iter()
            .map(|t| t.temp + 1)
            .max()
            .unwrap_or(TEMP_BASE)
            .max(TEMP_BASE);
        Ok(())
    }

    // ------------------------------------------------------------ actions

    /// The actions to perform, as a JSON array; the outbox is emptied.
    pub fn actions(&mut self) -> String {
        let state = self.state_name();
        if state != self.last_state {
            self.last_state = state;
            self.once("{\"do\":\"status\"}");
        }
        let mut out = String::from("[");
        out.push_str(&self.outbox.join(","));
        out.push(']');
        self.outbox.clear();
        out
    }

    fn once(&mut self, action: &str) {
        if !self.outbox.iter().any(|a| a == action) {
            self.outbox.push(action.to_string());
        }
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id - 1
    }

    fn headers(&self, extra: &[(&str, &str)]) -> String {
        let mut out = String::from("{");
        let mut first = true;
        let mut add = |k: &str, v: &str| {
            if !first {
                out.push(',');
            }
            first = false;
            json::escape_into(&mut out, k);
            out.push(':');
            json::escape_into(&mut out, v);
        };
        if let Some(t) = &self.token {
            add("authorization", &format!("Bearer {t}"));
        }
        for (k, v) in extra {
            add(k, v);
        }
        out.push('}');
        out
    }

    fn request(
        &mut self,
        method: &str,
        path: &str,
        extra: &[(&str, &str)],
        body: Option<&str>,
    ) -> u64 {
        let id = self.id();
        let mut a = format!("{{\"do\":\"request\",\"id\":{id},\"method\":\"{method}\",\"url\":");
        json::escape_into(&mut a, &format!("{}{path}", self.url));
        a.push_str(",\"headers\":");
        a.push_str(&self.headers(extra));
        a.push_str(",\"body\":");
        match body {
            Some(b) => json::escape_into(&mut a, b),
            None => a.push_str("null"),
        }
        a.push('}');
        self.outbox.push(a);
        id
    }

    fn wait(&mut self, what: Timer, attempt: u32) {
        let base = (BACKOFF_MS << attempt.min(10)).min(BACKOFF_MAX_MS);
        let ms = base + self.rng.next() % (base * 3 / 10 + 1);
        let id = self.id();
        self.timers.push((id, what));
        self.outbox
            .push(format!("{{\"do\":\"wait\",\"id\":{id},\"ms\":{ms}}}"));
    }

    fn fault(&mut self, message: impl Into<String>, status: u16, connection: bool) {
        self.fault = Some(Fault {
            message: message.into(),
            status,
            connection,
        });
        self.once("{\"do\":\"status\"}");
    }

    fn want_token(&mut self) {
        self.paused = true;
        self.fault(
            "the server refused the token (401): a fresh one is wanted",
            401,
            true,
        );
        self.once("{\"do\":\"token\"}");
    }

    // ------------------------------------------------------------ status

    fn state_name(&self) -> &'static str {
        if self.offline || self.stopped {
            return "offline";
        }
        let up = self.shapes.iter().all(|s| s.connected && s.seeded);
        if up && !self.flushing {
            return "online";
        }
        let failing = self.paused
            || self.schema_waiting
            || self.shapes.iter().any(|s| s.waiting && !s.connected);
        match failing {
            true => "offline",
            false => "catching_up",
        }
    }

    /// `{"state":"online"|"offline"|"catching_up","pending":N,"error":..,"shapes":[..]}`.
    pub fn status(&self) -> String {
        let mut out = format!(
            "{{\"state\":\"{}\",\"pending\":{},\"error\":",
            self.state_name(),
            self.queue.len()
        );
        match &self.fault {
            None => out.push_str("null"),
            Some(f) => {
                out.push_str("{\"message\":");
                json::escape_into(&mut out, &f.message);
                out.push_str(&format!(",\"status\":{}}}", f.status));
            }
        }
        out.push_str(",\"shapes\":[");
        for (i, s) in self.shapes.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"collection\":");
            json::escape_into(&mut out, &s.spec.collection);
            out.push_str(&format!(
                ",\"cursor\":{},\"seeded\":{},\"connected\":{}}}",
                s.cursor, s.seeded, s.connected
            ));
        }
        out.push_str("]}");
        out
    }

    /// Whether `collection` is one of the shapes'.
    pub fn synced(&self, collection: &str) -> bool {
        self.shapes.iter().any(|s| s.spec.collection == collection)
    }

    // ------------------------------------------------------------ driving

    /// Asks for whatever is due: the schemas, the queue's next write, the
    /// streams.
    fn kick(&mut self, db: &Database) {
        if self.stopped || self.offline {
            return;
        }
        let missing = self
            .shapes
            .iter()
            .any(|s| s.rebuild || db.collection(&s.spec.collection).is_err());
        if missing {
            if self.schema_req.is_none() && !self.schema_waiting && !self.paused {
                self.schema_req = Some(self.request("GET", "/collections", &[], None));
            }
            return;
        }
        self.pump();
        if self.paused || (self.flushing && !self.queue.is_empty()) {
            return;
        }
        for i in 0..self.shapes.len() {
            self.connect(i);
        }
    }

    fn connect(&mut self, i: usize) {
        let s = &self.shapes[i];
        if s.stream.is_some() || s.waiting || self.paused || self.offline || self.stopped {
            return;
        }
        let mut url = format!("{}/", self.url);
        shape::encode(&s.spec.collection, &mut url);
        url.push_str("/changes");
        let mut q = Vec::new();
        for (k, v) in &s.spec.params {
            let mut p = String::new();
            shape::encode(k, &mut p);
            p.push('=');
            shape::encode(v, &mut p);
            q.push(p);
        }
        if let Some(sel) = &s.spec.select {
            let mut p = String::from("select=");
            shape::encode(&sel.join(","), &mut p);
            q.push(p);
        }
        // Seeded once, it goes on from where it stopped: the server seeds
        // it again itself when the cursor is past its ring.
        if s.seeded && !s.fresh {
            q.push(format!("since={}", s.cursor));
        }
        if !q.is_empty() {
            url.push('?');
            url.push_str(&q.join("&"));
        }
        let id = self.id();
        let mut a = format!("{{\"do\":\"stream\",\"id\":{id},\"url\":");
        json::escape_into(&mut a, &url);
        a.push_str(",\"headers\":");
        a.push_str(&self.headers(&[("accept", "text/event-stream")]));
        a.push('}');
        self.outbox.push(a);
        let s = &mut self.shapes[i];
        s.stream = Some(id);
        s.frames = Default::default();
    }

    /// Sends the queue's next write, one at a time and in order: a write
    /// that lands after one written after it would leave the server holding
    /// the earlier.
    fn pump(&mut self) {
        if self.sending.is_some()
            || self.send_waiting
            || self.paused
            || self.offline
            || self.stopped
        {
            return;
        }
        let Some(op) = self.queue.first() else {
            return;
        };
        let (n, path, body, key) = (op.n, op.path.clone(), op.body.clone(), op.key.clone());
        let ty = match path.as_str() {
            "/batch" => "application/x-ndjson",
            _ => "application/json",
        };
        let id = self.request(
            "POST",
            &path,
            &[("content-type", ty), ("idempotency-key", &key)],
            Some(&body),
        );
        self.sending = Some((id, n));
    }

    fn drop_stream(&mut self, i: usize, cancel: bool) {
        if let Some(id) = self.shapes[i].stream.take() {
            if cancel {
                self.outbox
                    .push(format!("{{\"do\":\"cancel\",\"id\":{id}}}"));
            }
        }
        self.shapes[i].connected = false;
    }

    fn retry_stream(&mut self, i: usize) {
        if self.stopped || self.offline || self.paused {
            return;
        }
        let attempt = self.shapes[i].attempt;
        self.shapes[i].attempt += 1;
        self.shapes[i].waiting = true;
        self.wait(Timer::Stream(i), attempt);
    }

    // ------------------------------------------------------------- events

    /// A request's answer: its status (0 for none -- the network failed,
    /// `body` saying why), the `Fenec-Seq` it carried (0 for none), and its
    /// body.
    pub fn response(&mut self, db: &mut Database, id: u64, status: u16, seq: u64, body: &str) {
        if self.schema_req == Some(id) {
            self.schema_req = None;
            self.on_schemas(db, status, body);
        } else if let Some((_, n)) = self.sending.filter(|(r, _)| *r == id) {
            self.sending = None;
            self.on_answer(db, n, status, seq, body);
        }
        self.kick(db);
    }

    fn on_schemas(&mut self, db: &mut Database, status: u16, body: &str) {
        match status {
            200..=299 => {}
            401 => return self.want_token(),
            _ => {
                let why = message(status, body);
                self.fault(
                    format!("could not read the server's collections: {why}"),
                    status,
                    true,
                );
                self.schema_waiting = true;
                let a = self.schema_attempt;
                self.schema_attempt += 1;
                return self.wait(Timer::Schema, a);
            }
        }
        let made = (|| -> Result<()> {
            let Value::List(all) = json::parse_json(body)? else {
                return Err(Error::Query(
                    "the server's collections are not a list".into(),
                ));
            };
            let mut ddl = Vec::new();
            for s in &self.shapes {
                let c = &s.spec.collection;
                let held = db.collection(c).is_ok();
                if held && !s.rebuild {
                    continue;
                }
                let schema = all
                    .iter()
                    .find(|x| matches!(shape::member(x, "name"), Some(Value::Text(n)) if n == c))
                    .ok_or_else(|| {
                        Error::NotFound(format!("the server has no `{c}` collection"))
                    })?;
                ddl.push((
                    c.clone(),
                    held,
                    fenec_ql::parse_one(&shape::schema_ddl(schema)?)?,
                ));
            }
            db.begin()?;
            let r = (|| -> Result<()> {
                for (c, held, create) in &ddl {
                    match held {
                        true => remake(db, c, create)?,
                        false => {
                            db.execute_with(create, &[])?;
                        }
                    }
                }
                db.commit()
            })();
            if r.is_err() && db.in_block() {
                db.rollback();
            }
            r
        })();
        match made {
            Ok(()) => {
                self.schema_attempt = 0;
                for s in &mut self.shapes {
                    s.rebuild = false;
                }
            }
            Err(e) => {
                self.fault(e.to_string(), 0, true);
                self.schema_waiting = true;
                let a = self.schema_attempt;
                self.schema_attempt += 1;
                self.wait(Timer::Schema, a);
            }
        }
    }

    fn on_answer(&mut self, db: &mut Database, n: i64, status: u16, seq: u64, body: &str) {
        match status {
            200..=299 => {
                self.send_attempt = 0;
                if self.fault.as_ref().is_some_and(|f| f.connection) {
                    self.fault = None;
                }
                self.landed(db, n, seq);
            }
            401 => self.want_token(),
            // No answer, or one that says to come back: the write stays, and
            // goes again under its key, which the server answers as the
            // first time if the first reached it.
            0 | 408 | 429 | 500..=599 => {
                self.fault(message(status, body), status, true);
                self.flushing = false;
                self.send_waiting = true;
                let a = self.send_attempt;
                self.send_attempt += 1;
                self.wait(Timer::Send, a);
            }
            _ => self.refused(db, n, status, &message(status, body)),
        }
    }

    /// The server took write `n`: it leaves the queue, and the rows it made
    /// under temporary ids wait for the server's copies.
    fn landed(&mut self, db: &mut Database, n: i64, seq: u64) {
        let until = if seq == 0 { u64::MAX } else { seq };
        let r = (|| -> Result<()> {
            db.begin()?;
            self.exec(db, DEL_OP, &[Value::Int(n)])?;
            for i in 0..self.temps.len() {
                if self.temps[i].op == n {
                    self.temps[i].op = 0;
                    self.temps[i].until = until;
                    let t = &self.temps[i];
                    let p = [
                        Value::Int(t.temp),
                        Value::Text(t.collection.clone()),
                        Value::Text(t.key.clone()),
                        Value::Int(0),
                        Value::Int(until.min(i64::MAX as u64) as i64),
                    ];
                    self.exec(db, PUT_TEMP, &p)?;
                }
            }
            // A stream already past it: the server's copy came, or the shape
            // does not hold the row.
            for c in self
                .shapes
                .iter()
                .map(|s| (s.spec.collection.clone(), s.cursor))
                .collect::<Vec<_>>()
            {
                self.drop_passed(db, &c.0, c.1)?;
            }
            db.commit()
        })();
        if let Err(e) = r {
            db.rollback();
            self.fault(e.to_string(), 0, false);
        }
        self.queue.retain(|o| o.n != n);
        if self.queue.is_empty() {
            self.flushing = false;
        }
        self.once("{\"do\":\"changed\"}");
        self.once("{\"do\":\"status\"}");
    }

    /// The server refused write `n`: what it did to the replica is put
    /// back, the last statement's first, and the binding is told.
    fn refused(&mut self, db: &mut Database, n: i64, status: u16, why: &str) {
        let Some(at) = self.queue.iter().position(|o| o.n == n) else {
            return;
        };
        let op = self.queue.remove(at);
        let r = (|| -> Result<()> {
            db.begin()?;
            if let Value::List(steps) = json::parse_json(&op.undo)? {
                for step in steps.iter().rev() {
                    self.undo_step(db, step)?;
                }
            }
            self.exec(db, DEL_OP, &[Value::Int(n)])?;
            for t in self
                .temps
                .iter()
                .filter(|t| t.op == n)
                .map(|t| t.temp)
                .collect::<Vec<_>>()
            {
                self.exec(db, DEL_TEMP, &[Value::Int(t)])?;
            }
            db.commit()
        })();
        if let Err(e) = r {
            db.rollback();
            self.fault(
                format!("a refused write could not be put back: {e}"),
                0,
                false,
            );
        }
        self.temps.retain(|t| t.op != n);
        if self.queue.is_empty() {
            self.flushing = false;
        }
        self.fault(why.to_string(), status, false);
        let mut a = format!("{{\"do\":\"refused\",\"status\":{status},\"message\":");
        json::escape_into(&mut a, why);
        a.push_str(",\"query\":");
        json::escape_into(&mut a, &op.label);
        a.push('}');
        self.outbox.push(a);
        self.once("{\"do\":\"changed\"}");
    }

    fn undo_step(&self, db: &mut Database, step: &Value) -> Result<()> {
        let Some(Value::Text(c)) = shape::member(step, "c") else {
            return Ok(());
        };
        if let Some(Value::List(dels)) = shape::member(step, "del") {
            let ids: Vec<DocId> = dels.iter().map(|v| int_of(v) as DocId).collect();
            if !ids.is_empty() {
                db.execute_with(&del_ids(c, &ids), &[])?;
            }
        }
        if let Some(Value::Text(rows)) = shape::member(step, "put") {
            let fields = json_fields(db, c);
            let names: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();
            let docs = json::parse_documents_json(rows, &names)?;
            if !docs.is_empty() {
                db.execute_with(&put_docs(c, docs), &[])?;
            }
        }
        Ok(())
    }

    fn exec(&self, db: &mut Database, which: usize, params: &[Value]) -> Result<Response> {
        db.execute_with(&self.state[which], params)
    }

    /// The temporary rows of `collection` the server answered for at or
    /// before `cursor`: the server's copy is here by now, or the shape does
    /// not hold it.
    fn drop_passed(&mut self, db: &mut Database, collection: &str, cursor: u64) -> Result<()> {
        let gone: Vec<i64> = self
            .temps
            .iter()
            .filter(|t| t.collection == collection && t.op == 0 && t.until <= cursor)
            .map(|t| t.temp)
            .collect();
        if gone.is_empty() {
            return Ok(());
        }
        let ids: Vec<DocId> = gone.iter().map(|&t| t as DocId).collect();
        db.execute_with(&del_ids(collection, &ids), &[])?;
        for t in &gone {
            self.exec(db, DEL_TEMP, &[Value::Int(*t)])?;
        }
        self.temps.retain(|t| !gone.contains(&t.temp));
        Ok(())
    }

    fn shape_of(&self, stream: u64) -> Option<usize> {
        self.shapes.iter().position(|s| s.stream == Some(stream))
    }

    /// A stream's response began: `status`, and `body` when it is not 200.
    pub fn opened(&mut self, db: &mut Database, id: u64, status: u16, body: &str) {
        let Some(i) = self.shape_of(id) else {
            return;
        };
        match status {
            200 => {
                self.shapes[i].connected = true;
                self.shapes[i].attempt = 0;
                if self.fault.as_ref().is_some_and(|f| f.connection) {
                    self.fault = None;
                }
            }
            401 => {
                self.drop_stream(i, false);
                self.want_token();
            }
            _ => {
                self.drop_stream(i, false);
                let c = self.shapes[i].spec.collection.clone();
                self.fault(
                    format!(
                        "could not open the subscription to `{c}`: {}",
                        message(status, body)
                    ),
                    status,
                    true,
                );
                self.retry_stream(i);
            }
        }
        self.once("{\"do\":\"status\"}");
        self.kick(db);
    }

    /// A piece of a stream's body.
    pub fn bytes(&mut self, db: &mut Database, id: u64, bytes: &[u8]) {
        let Some(i) = self.shape_of(id) else {
            return;
        };
        let events = self.shapes[i].frames.push(bytes);
        for ev in events {
            // The schema changed: what the stream sent after is read again
            // from a fresh seed.
            if self.shapes[i].stream != Some(id) {
                break;
            }
            // The server ends a stream at its token's `exp` with a 401: the
            // token is refused as a request's would be, so a fresh one is
            // wanted before the stream opens again -- retried with the same
            // token, it was refused at every attempt.
            if ev.name == "error" && stream_status(&ev.data) == Some(401) {
                self.drop_stream(i, true);
                self.want_token();
                return;
            }
            let r = match ev.name.as_str() {
                "seed" => self.seed(db, i, &ev.data),
                "change" if self.shapes[i].seeded => self.change(db, i, &ev.data),
                "error" => Err(Error::Query(
                    members(&ev.data)
                        .ok()
                        .and_then(|m| {
                            field(&m, "error")
                                .map(|e| text_of(&json::parse(e).unwrap_or(Value::Null)))
                        })
                        .unwrap_or_else(|| "subscription error".into()),
                )),
                _ => Ok(()),
            };
            if let Err(e) = r {
                let c = self.shapes[i].spec.collection.clone();
                self.fault(format!("the subscription to `{c}` stopped: {e}"), 0, true);
                self.drop_stream(i, true);
                self.retry_stream(i);
                return;
            }
        }
        if self.shapes[i].rebuild {
            self.kick(db);
        }
    }

    /// A stream ended: `why` is empty for the server closing it.
    pub fn closed(&mut self, db: &mut Database, id: u64, why: &str) {
        let Some(i) = self.shape_of(id) else {
            return;
        };
        self.drop_stream(i, false);
        let c = self.shapes[i].spec.collection.clone();
        let why = if why.is_empty() {
            "the server closed it"
        } else {
            why
        };
        self.fault(format!("the subscription to `{c}` ended: {why}"), 0, true);
        self.retry_stream(i);
        self.kick(db);
    }

    /// A timer [`Sync::actions`] asked for ran out.
    pub fn timer(&mut self, db: &mut Database, id: u64) {
        let Some(at) = self.timers.iter().position(|(t, _)| *t == id) else {
            return;
        };
        let (_, what) = self.timers.remove(at);
        match what {
            Timer::Stream(i) => self.shapes[i].waiting = false,
            Timer::Send => self.send_waiting = false,
            Timer::Schema => self.schema_waiting = false,
        }
        self.kick(db);
    }

    /// `{"online":bool}`, `{"token":".."}` or `{"stop":true}`.
    pub fn signal(&mut self, db: &mut Database, json_text: &str) -> Result<()> {
        let v = json::parse_json(json_text)?;
        if let Some(Value::Text(t)) = shape::member(&v, "token") {
            self.token = Some(t.clone()).filter(|t| !t.is_empty());
            if self.paused {
                self.paused = false;
                self.fault = None;
            }
        }
        match shape::member(&v, "online") {
            Some(Value::Bool(false)) => {
                self.offline = true;
                for i in 0..self.shapes.len() {
                    self.drop_stream(i, true);
                }
            }
            Some(Value::Bool(true)) => {
                // Come back at once, the backoff forgotten: the platform
                // said the network is here.
                self.offline = false;
                self.timers.clear();
                self.send_waiting = false;
                self.schema_waiting = false;
                self.send_attempt = 0;
                self.schema_attempt = 0;
                for s in &mut self.shapes {
                    s.waiting = false;
                    s.attempt = 0;
                }
                self.flushing = !self.queue.is_empty();
            }
            _ => {}
        }
        if let Some(Value::Bool(true)) = shape::member(&v, "stop") {
            self.stopped = true;
            for i in 0..self.shapes.len() {
                self.drop_stream(i, true);
            }
            self.timers.clear();
        }
        self.once("{\"do\":\"status\"}");
        self.kick(db);
        Ok(())
    }

    // ----------------------------------------------------------- applying

    /// The seed: the whole shape. What the replica holds of the collection
    /// and the seed does not is deleted -- written over rather than cleared
    /// first, so a row that stayed keeps its vector's node -- except the
    /// rows of writes the server has not answered yet, whose copies come
    /// later. One block, the cursor in it.
    fn seed(&mut self, db: &mut Database, i: usize, data: &str) -> Result<()> {
        let m = members(data)?;
        let seq: u64 = field(&m, "seq").and_then(|s| s.parse().ok()).unwrap_or(0);
        let rows = field(&m, "rows").unwrap_or("[]");
        let c = self.shapes[i].spec.collection.clone();
        let fields = json_fields(db, &c);
        let names: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();
        let docs = json::parse_documents_json(rows, &names)?;
        if !fits(db, &c, &docs) {
            self.refresh(i);
            return Ok(());
        }
        db.begin()?;
        let r = (|| -> Result<()> {
            let held = match db.execute_with(
                &Statement::Select(Select {
                    collection: c.clone(),
                    project: Some(vec!["id".into()]),
                    ..Default::default()
                }),
                &[],
            )? {
                Response::Rows(rs) => rs.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            let came: std::collections::HashSet<DocId> = docs
                .iter()
                .filter_map(|d| {
                    d.iter()
                        .find(|(k, _)| k == "id")
                        .map(|(_, v)| int_of(v) as DocId)
                })
                .collect();
            self.reconcile(db, i, &docs)?;
            // Kept: an unanswered write's rows, and an answered one's the
            // seed is from before.
            let keep: Vec<DocId> = self
                .temps
                .iter()
                .filter(|t| t.collection == c && (t.op != 0 || t.until > seq))
                .map(|t| t.temp as DocId)
                .collect();
            let gone: Vec<DocId> = held
                .into_iter()
                .filter(|id| !came.contains(id) && !keep.contains(id))
                .collect();
            if !gone.is_empty() {
                db.execute_with(&del_ids(&c, &gone), &[])?;
            }
            let dropped: Vec<i64> = self
                .temps
                .iter()
                .filter(|t| t.collection == c && t.op == 0 && t.until <= seq)
                .map(|t| t.temp)
                .collect();
            for t in &dropped {
                self.exec(db, DEL_TEMP, &[Value::Int(*t)])?;
            }
            self.temps.retain(|t| !dropped.contains(&t.temp));
            if !docs.is_empty() {
                db.execute_with(&put_docs(&c, docs), &[])?;
            }
            self.shapes[i].cursor = seq;
            self.shapes[i].seeded = true;
            self.shapes[i].fresh = false;
            self.save_shape(db, i)
        })();
        self.finish(db, r)
    }

    /// A change: the rows that changed as they are now, and the ids that
    /// left the shape.
    fn change(&mut self, db: &mut Database, i: usize, data: &str) -> Result<()> {
        let m = members(data)?;
        let seq: u64 = field(&m, "seq").and_then(|s| s.parse().ok()).unwrap_or(0);
        let c = self.shapes[i].spec.collection.clone();
        let fields = json_fields(db, &c);
        let names: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();
        let docs = json::parse_documents_json(field(&m, "puts").unwrap_or("[]"), &names)?;
        if field(&m, "schema") == Some("true") || !fits(db, &c, &docs) {
            self.refresh(i);
            return Ok(());
        }
        let dels = ids(field(&m, "dels").unwrap_or("[]"))?;
        db.begin()?;
        let r = (|| -> Result<()> {
            self.reconcile(db, i, &docs)?;
            if !dels.is_empty() {
                db.execute_with(&del_ids(&c, &dels), &[])?;
            }
            if !docs.is_empty() {
                db.execute_with(&put_docs(&c, docs), &[])?;
            }
            self.drop_passed(db, &c, seq)?;
            self.shapes[i].cursor = seq.max(self.shapes[i].cursor);
            self.save_shape(db, i)
        })();
        self.finish(db, r)
    }

    fn finish(&mut self, db: &mut Database, r: Result<()>) -> Result<()> {
        match r.and_then(|_| db.commit()) {
            Ok(()) => {
                self.once("{\"do\":\"changed\"}");
                self.once("{\"do\":\"status\"}");
                Ok(())
            }
            Err(e) => {
                if db.in_block() {
                    db.rollback();
                }
                // The temporary rows as the file holds them again.
                self.load(db).ok();
                Err(e)
            }
        }
    }

    /// The server's schema is not the replica's: a change said so, or rows
    /// came with a field the replica has not got. The stream ends, the
    /// collection is made again from `GET /collections`, and the shape is
    /// sent whole -- a rename or a drop is not told apart from the rows.
    fn refresh(&mut self, i: usize) {
        self.drop_stream(i, true);
        self.shapes[i].rebuild = true;
        self.shapes[i].fresh = true;
    }

    fn save_shape(&self, db: &mut Database, i: usize) -> Result<()> {
        let s = &self.shapes[i];
        let p = [
            Value::Int(s.row),
            Value::Text(s.spec.collection.clone()),
            Value::Text(s.fingerprint.clone()),
            Value::Int(s.cursor as i64),
            Value::Bool(s.seeded),
        ];
        self.exec(db, PUT_SHAPE, &p).map(|_| ())
    }

    /// The server's copy of a row an optimistic insert made carries its
    /// key: the temporary row goes. A business key, since the ids are the
    /// server's to hand out.
    fn reconcile(
        &mut self,
        db: &mut Database,
        i: usize,
        docs: &[Vec<(String, Value)>],
    ) -> Result<()> {
        let Some(key) = self.shapes[i].spec.key.clone() else {
            return Ok(());
        };
        let c = &self.shapes[i].spec.collection;
        if !self.temps.iter().any(|t| &t.collection == c) {
            return Ok(());
        }
        let mut drop = Vec::new();
        for d in docs {
            let Some(k) = d
                .iter()
                .find(|(f, _)| *f == key)
                .and_then(|(_, v)| key_text(v))
            else {
                continue;
            };
            let id = d.iter().find(|(f, _)| f == "id").map(|(_, v)| int_of(v));
            if let Some(t) = self.temps.iter().find(|t| &t.collection == c && t.key == k) {
                if Some(t.temp) != id {
                    drop.push(t.temp);
                }
            }
        }
        if drop.is_empty() {
            return Ok(());
        }
        let ids: Vec<DocId> = drop.iter().map(|&t| t as DocId).collect();
        db.execute_with(&del_ids(c, &ids), &[])?;
        for t in &drop {
            self.exec(db, DEL_TEMP, &[Value::Int(*t)])?;
        }
        self.temps.retain(|t| !drop.contains(&t.temp));
        Ok(())
    }

    // ------------------------------------------------------------- writes

    /// Whether the statements are a synced write: `false` for statements
    /// over local collections alone, which run as they would without a
    /// sync, and an error for what a replica cannot take.
    pub fn claims(&self, stmts: &[Statement]) -> Result<bool> {
        let mut synced = 0;
        let mut other = 0;
        let mut guarded = false;
        for s in stmts {
            if let Statement::Select(sel) = s {
                guarded |= sel.require.is_some() && self.synced(&sel.collection);
            }
            let (c, write) = match s {
                Statement::Put { collection, .. }
                | Statement::Update { collection, .. }
                | Statement::Delete { collection, .. } => (Some(collection.as_str()), true),
                Statement::CreateCollection { schema, .. } => (Some(schema.name.as_str()), false),
                Statement::DropCollection { name, .. } => (Some(name.as_str()), false),
                Statement::CreateIndex { collection, .. }
                | Statement::AlterCollection { collection, .. } => {
                    (Some(collection.as_str()), false)
                }
                Statement::Compact(Some(c)) => (Some(c.as_str()), false),
                _ => (None, false),
            };
            let Some(c) = c else {
                other += 1;
                continue;
            };
            if own(c) {
                return Err(Error::ReadOnly(format!(
                    "`{c}` is the sync's own: it is written as the server answers"
                )));
            }
            match (self.synced(c), write) {
                (true, true) => synced += 1,
                (true, false) => {
                    return Err(Error::ReadOnly(format!(
                        "`{c}` is synced from the server, whose schema it has: \
                         change it there"
                    )))
                }
                (false, _) => other += 1,
            }
        }
        match (synced, other) {
            (0, _) => Ok(false),
            _ if guarded => Err(Error::Query(GUARDED.into())),
            (_, 0) => Ok(true),
            _ => Err(Error::Query(
                "a synced write goes to the server alone: send the statements over \
                 other collections, and the reads, apart"
                    .into(),
            )),
        }
    }

    /// A synced write: applied to the replica at once, each statement
    /// keeping what puts it back, and queued for the server with an
    /// idempotency key -- all in one block, so the file never holds the
    /// change without the write that sends it. The answer is the replica's.
    ///
    /// - An insert into a shape with a `key` gets a key where it has none
    ///   and a temporary id, and is matched with the server's copy by the
    ///   key; without a key it waits for the server, as in JS: with nothing
    ///   to match it by, the server's copy would leave two.
    /// - An update or a delete works by the filter, the rows it reaches read
    ///   first: putting them back is the undo. A row of an insert the server
    ///   has not answered is reached on the server by its key.
    pub fn write(&mut self, db: &mut Database, p: &Prepared, sql: &str) -> Result<Response> {
        let spans = fenec_ql::spans(sql)?;
        if spans.len() != p.stmts.len() {
            return Err(Error::Query(
                "the text's statements could not be told apart".into(),
            ));
        }
        db.begin()?;
        let n = self.next_n;
        let temp0 = self.next_temp;
        let r = (|| -> Result<(Vec<Applied>, Response)> {
            let mut all = Vec::new();
            let mut last = Response::Ok("empty".into());
            for (s, &(a, b)) in p.stmts.iter().zip(&spans) {
                let applied =
                    self.apply(db, s, sql[a..b].trim().trim_end_matches(';'), &p.params)?;
                last = applied.response.clone();
                all.push(applied);
            }
            Ok((all, last))
        })();
        let (applied, last) = match r {
            Ok(x) => x,
            Err(e) => {
                db.rollback();
                self.next_temp = temp0;
                return Err(e);
            }
        };
        let lines: Vec<(String, Vec<Value>)> =
            applied.iter().flat_map(|a| a.lines.clone()).collect();
        let (path, body) = match lines.as_slice() {
            [(t, p)] => ("/query".to_string(), query_body(t, p)),
            _ => (
                "/batch".to_string(),
                lines
                    .iter()
                    .map(|(t, p)| query_body(t, p))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        };
        let undo = format!(
            "[{}]",
            applied
                .iter()
                .map(|a| a.undo.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );
        let label: String = sql.chars().take(500).collect();
        let op = Op {
            n,
            path,
            body,
            key: format!("fenec-{}", self.rng.uuid()),
            undo,
            label,
        };
        let temps: Vec<Temp> = applied
            .iter()
            .zip(&p.stmts)
            .flat_map(|(a, s)| {
                let c = match s {
                    Statement::Put { collection, .. } => collection.clone(),
                    _ => String::new(),
                };
                a.temps.iter().map(move |(k, t)| Temp {
                    collection: c.clone(),
                    key: k.clone(),
                    temp: *t,
                    op: n,
                    until: 0,
                })
            })
            .collect();
        let saved = (|| -> Result<()> {
            let params = [
                Value::Int(op.n),
                Value::Text(op.path.clone()),
                Value::Text(op.body.clone()),
                Value::Text(op.key.clone()),
                Value::Text(op.undo.clone()),
                Value::Text(op.label.clone()),
            ];
            self.exec(db, PUT_OP, &params)?;
            for t in &temps {
                let params = [
                    Value::Int(t.temp),
                    Value::Text(t.collection.clone()),
                    Value::Text(t.key.clone()),
                    Value::Int(n),
                    Value::Int(0),
                ];
                self.exec(db, PUT_TEMP, &params)?;
            }
            db.commit()
        })();
        if let Err(e) = saved {
            if db.in_block() {
                db.rollback();
            }
            self.next_temp = temp0;
            return Err(e);
        }
        self.next_n += 1;
        self.queue.push(op);
        self.temps.extend(temps);
        self.once("{\"do\":\"status\"}");
        self.kick(db);
        Ok(last)
    }

    /// An upsert, sent as written: the server sets the row holding each
    /// document's `@unique` value, its set worked out over that row again.
    /// Here, where a replica's `@unique` is a plain hash, a document finds
    /// its row by its id or the shape's key -- each names one, or there is
    /// nothing to match the server's copy by -- and the replica runs the
    /// upsert by id: a row it holds set, the rest made under temporary ids,
    /// the rows set read first to put back. As the browser's sync does.
    #[allow(clippy::too_many_arguments)]
    fn upsert(
        &mut self,
        db: &mut Database,
        c: &str,
        key: Option<&str>,
        docs: Vec<Vec<(String, Value)>>,
        set: &[(String, Expr)],
        require: Option<u64>,
        (text, params): (&str, &[Value]),
        mut undo: String,
    ) -> Result<Applied> {
        let line = (text.to_string(), params.to_vec());
        let given = |d: &[(String, Value)], f: &str| {
            d.iter()
                .find(|(k, v)| k == f && !v.is_null())
                .map(|(_, v)| v.clone())
        };
        let named = |d: &[(String, Value)]| {
            given(d, "id").is_some() || key.is_some_and(|k| given(d, k).is_some())
        };
        if !docs.iter().all(|d| named(d)) {
            if key.is_some() {
                return Err(Error::Query(UPSERT_KEY.into()));
            }
            undo.push_str(",\"del\":[],\"put\":\"[]\"}");
            return Ok(Applied {
                lines: vec![line],
                undo,
                response: Response::Affected(docs.len()),
                temps: Vec::new(),
            });
        }
        let one = |db: &mut Database, field: &str, v: Value| -> Result<Option<i64>> {
            let sel = Select {
                collection: c.to_string(),
                filter: Some(Expr::Cmp(
                    CmpOp::Eq,
                    Box::new(Expr::Field(field.to_string())),
                    Box::new(Expr::Lit(v)),
                )),
                limit: Some(1),
                ..Default::default()
            };
            Ok(match db.execute_with(&Statement::Select(sel), &[])? {
                Response::Rows(rs) => rs.rows.first().map(|r| r.id as i64),
                _ => None,
            })
        };
        let (mut held, mut fresh, mut temps) = (Vec::new(), Vec::new(), Vec::new());
        // A key twice in the page: the second finds the row the first makes.
        let mut found: Vec<(String, i64)> = Vec::new();
        let mut local = Vec::with_capacity(docs.len());
        for mut d in docs {
            if let Some(v) = given(&d, "id") {
                let id = int_of(&v);
                match one(db, "id", v)? {
                    Some(_) => held.push(id),
                    None if !fresh.contains(&id) => fresh.push(id),
                    None => {}
                }
                local.push(d);
                continue;
            }
            let k = key.unwrap_or_default();
            let kv = given(&d, k).unwrap_or(Value::Null);
            let kt = key_text(&kv).unwrap_or_default();
            let id = match found.iter().find(|(t, _)| *t == kt) {
                Some((_, id)) => *id,
                None => {
                    let id = match one(db, k, kv)? {
                        Some(id) => {
                            held.push(id);
                            id
                        }
                        None => {
                            let t = self.next_temp;
                            self.next_temp += 1;
                            fresh.push(t);
                            temps.push((kt.clone(), t));
                            t
                        }
                    };
                    found.push((kt, id));
                    id
                }
            };
            d.retain(|(f, _)| f != "id");
            d.insert(0, ("id".into(), Value::Int(id)));
            local.push(d);
        }
        held.sort_unstable();
        held.dedup();
        let before = match held.is_empty() {
            true => ResultSet::default(),
            false => match db.execute_with(
                &Statement::Select(Select {
                    collection: c.to_string(),
                    filter: del_ids(c, &held.iter().map(|&x| x as DocId).collect::<Vec<_>>())
                        .filter_of(),
                    ..Default::default()
                }),
                &[],
            )? {
                Response::Rows(rs) => rs,
                _ => ResultSet::default(),
            },
        };
        let mut st = put_docs(c, local);
        if let Statement::Put {
            insert,
            if_absent,
            else_set,
            require: req,
            ..
        } = &mut st
        {
            (*insert, *if_absent) = (true, true);
            *else_set = Some(set.to_vec());
            *req = require;
        }
        // The set's values may read the text's parameters.
        let response = db.execute_with(&st, params)?;
        undo.push_str(",\"del\":[");
        undo.push_str(
            &fresh
                .iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(","),
        );
        undo.push_str("],\"put\":");
        json::escape_into(&mut undo, &rows_json(&before));
        undo.push('}');
        Ok(Applied {
            lines: vec![line],
            undo,
            response,
            temps,
        })
    }

    fn values(
        &self,
        db: &Database,
        pairs: &[(String, Expr)],
        params: &[Value],
    ) -> Result<Vec<(String, Value)>> {
        let ctx = EvalCtx {
            params,
            registry: db.registry(),
            clock: None,
        };
        pairs
            .iter()
            .map(|(k, e)| Ok((k.clone(), eval(e, &mut NoRow, &ctx)?)))
            .collect()
    }

    fn apply(
        &mut self,
        db: &mut Database,
        s: &Statement,
        text: &str,
        params: &[Value],
    ) -> Result<Applied> {
        let i = match s {
            Statement::Put { collection, .. }
            | Statement::Update { collection, .. }
            | Statement::Delete { collection, .. } => self
                .shapes
                .iter()
                .position(|x| &x.spec.collection == collection)
                .ok_or_else(|| Error::Query("not a synced collection".into()))?,
            _ => return Err(Error::Query("not a synced write".into())),
        };
        let key = self.shapes[i].spec.key.clone();
        let c = self.shapes[i].spec.collection.clone();
        let mut undo = String::from("{\"c\":");
        json::escape_into(&mut undo, &c);
        match s {
            Statement::Put {
                docs,
                docs_param,
                insert,
                if_absent,
                else_set,
                require,
                ..
            } => {
                let mut docs: Vec<Vec<(String, Value)>> = docs
                    .iter()
                    .map(|d| self.values(db, d, params))
                    .collect::<Result<_>>()?;
                // `put <c> $n`: the parameter's documents, as written ones.
                if let Some(i) = docs_param {
                    for m in fenec_core::query::documents_in(&c, *i, params)? {
                        docs.push(m.to_vec());
                    }
                }
                if let Some(set) = else_set {
                    let (key, set) = (key.as_deref(), set.as_slice());
                    return self.upsert(db, &c, key, docs, set, *require, (text, params), undo);
                }
                if let Some(k) = &key {
                    for d in &mut docs {
                        if !d.iter().any(|(f, v)| f == k && !v.is_null()) {
                            d.retain(|(f, _)| f != k);
                            d.push((k.clone(), Value::Text(self.rng.uuid())));
                        }
                    }
                }
                let (mut line, line_params) = render_put(&c, *insert, &docs);
                if *if_absent {
                    line.push_str(" if absent");
                }
                if let Some(n) = require {
                    line.push_str(&format!(" require {n}"));
                }
                let count = docs.len();
                let has_id =
                    |d: &Vec<(String, Value)>| d.iter().any(|(f, v)| f == "id" && !v.is_null());
                // Applied where the server's copy can be matched with it: by
                // its key, or by the id it names.
                if key.is_none() && !docs.iter().all(has_id) {
                    undo.push_str(",\"del\":[],\"put\":\"[]\"}");
                    return Ok(Applied {
                        lines: vec![(line, line_params)],
                        undo,
                        response: Response::Affected(count),
                        temps: Vec::new(),
                    });
                }
                let named: Vec<i64> = docs
                    .iter()
                    .filter_map(|d| d.iter().find(|(f, _)| f == "id").map(|(_, v)| int_of(v)))
                    .collect();
                let before = match named.is_empty() {
                    true => ResultSet::default(),
                    false => match db.execute_with(
                        &Statement::Select(Select {
                            collection: c.clone(),
                            filter: del_ids(
                                &c,
                                &named.iter().map(|&x| x as DocId).collect::<Vec<_>>(),
                            )
                            .filter_of(),
                            ..Default::default()
                        }),
                        &[],
                    )? {
                        Response::Rows(rs) => rs,
                        _ => ResultSet::default(),
                    },
                };
                let mut temps = Vec::new();
                let mut fresh = Vec::new();
                let local: Vec<Vec<(String, Value)>> = docs
                    .iter()
                    .map(|d| {
                        let mut d = d.clone();
                        if !has_id(&d) {
                            d.retain(|(f, _)| f != "id");
                            let t = self.next_temp;
                            self.next_temp += 1;
                            d.insert(0, ("id".into(), Value::Int(t)));
                            fresh.push(t);
                            if let Some(k) = &key {
                                let kv = d
                                    .iter()
                                    .find(|(f, _)| f == k)
                                    .and_then(|(_, v)| key_text(v));
                                temps.push((kv.unwrap_or_default(), t));
                            }
                        } else {
                            let id = d
                                .iter()
                                .find(|(f, _)| f == "id")
                                .map(|(_, v)| int_of(v))
                                .unwrap_or(0);
                            if !before.rows.iter().any(|r| r.id as i64 == id) {
                                fresh.push(id);
                            }
                        }
                        d
                    })
                    .collect();
                let mut st = put_docs(&c, local);
                if let Statement::Put {
                    insert: ins,
                    if_absent: absent,
                    require: req,
                    ..
                } = &mut st
                {
                    *ins = *insert;
                    *absent = *if_absent;
                    *req = *require;
                }
                db.execute_with(&st, &[])?;
                undo.push_str(",\"del\":[");
                undo.push_str(
                    &fresh
                        .iter()
                        .map(|x| x.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                );
                undo.push_str("],\"put\":");
                json::escape_into(&mut undo, &rows_json(&before));
                undo.push('}');
                Ok(Applied {
                    lines: vec![(line, line_params)],
                    undo,
                    response: Response::Affected(count),
                    temps,
                })
            }
            Statement::Update { .. } | Statement::Delete { .. } => {
                let set = match s {
                    Statement::Update { set, .. } => Some(set),
                    _ => None,
                };
                let before = match db.execute_with(
                    &Statement::Select(Select {
                        collection: c.clone(),
                        filter: s.filter_clone(),
                        ..Default::default()
                    }),
                    params,
                )? {
                    Response::Rows(rs) => rs,
                    _ => ResultSet::default(),
                };
                // `require` counts the rows the server's copy of the write
                // finds, and a row of an insert it has not answered is
                // reached there by a second line, its key: the count would
                // be split between two statements, and the first, naming a
                // temporary id the server never saw, would find none.
                let required = matches!(
                    s,
                    Statement::Update {
                        require: Some(_),
                        ..
                    } | Statement::Delete {
                        require: Some(_),
                        ..
                    }
                );
                if required
                    && key.is_some()
                    && self.temps.iter().any(|t| {
                        t.collection == c && before.rows.iter().any(|r| r.id as i64 == t.temp)
                    })
                {
                    return Err(Error::Query(REQUIRE_UNANSWERED.into()));
                }
                let response = db.execute_with(s, params)?;
                undo.push_str(",\"del\":[],\"put\":");
                json::escape_into(&mut undo, &rows_json(&before));
                undo.push('}');
                let mut lines = vec![(text.to_string(), params.to_vec())];
                // A row of an insert the server has not answered has a
                // temporary id the server never saw: it is reached there by
                // its key, after the insert in the queue's order.
                if let Some(k) = &key {
                    let keys: Vec<Value> = self
                        .temps
                        .iter()
                        .filter(|t| {
                            t.collection == c && before.rows.iter().any(|r| r.id as i64 == t.temp)
                        })
                        .map(|t| Value::Text(t.key.clone()))
                        .collect();
                    if !keys.is_empty() {
                        let mut lp = Vec::new();
                        let mut text = match set {
                            Some(pairs) => match self.values(db, pairs, params) {
                                Ok(vals) => format!("set {c} {}", render_doc(&vals, &mut lp)),
                                // A patch that reads the row: as the filter
                                // reaches it alone.
                                Err(_) => String::new(),
                            },
                            None => format!("del {c}"),
                        };
                        if !text.is_empty() {
                            let marks: Vec<String> = keys
                                .iter()
                                .enumerate()
                                .map(|(j, _)| format!("${}", lp.len() + j + 1))
                                .collect();
                            text.push_str(&format!(" where {k} in [{}]", marks.join(", ")));
                            lp.extend(keys);
                            lines.push((text, lp));
                        }
                    }
                }
                Ok(Applied {
                    lines,
                    undo,
                    response,
                    temps: Vec::new(),
                })
            }
            _ => Err(Error::Query("not a synced write".into())),
        }
    }
}

/// A refusal's words: the server's `{"error":..}`, or what the transport
/// said.
fn message(status: u16, body: &str) -> String {
    let from_json = members(body)
        .ok()
        .and_then(|m| field(&m, "error").map(|e| text_of(&json::parse(e).unwrap_or(Value::Null))))
        .filter(|s| !s.is_empty());
    match (from_json, status) {
        (Some(m), _) => m,
        (None, 0) if !body.is_empty() => body.to_string(),
        (None, 0) => "the server could not be reached".into(),
        (None, s) => format!("HTTP {s}: {}", body.chars().take(200).collect::<String>()),
    }
}

/// The `status` a stream's `error` event names, where it names one.
fn stream_status(data: &str) -> Option<u16> {
    let m = members(data).ok()?;
    field(&m, "status")?.trim().parse().ok()
}

/// A statement's filter, for the rows it reaches.
trait FilterOf {
    fn filter_of(self) -> Option<Expr>;
    fn filter_clone(&self) -> Option<Expr>;
}

impl FilterOf for Statement {
    fn filter_of(self) -> Option<Expr> {
        match self {
            Statement::Update { filter, .. } | Statement::Delete { filter, .. } => filter,
            _ => None,
        }
    }
    fn filter_clone(&self) -> Option<Expr> {
        match self {
            Statement::Update { filter, .. } | Statement::Delete { filter, .. } => filter.clone(),
            _ => None,
        }
    }
}
