//! The sync core against a server in process answering as fenec-http
//! does -- `/collections`, `/query` and `/batch` under an
//! `Idempotency-Key`, the change stream's seeds and changes from the
//! engine's own ring -- and a network the test cuts, slows and loses
//! answers on. Nothing here does I/O: every action the core asks for is
//! performed by the test, as a binding performs it.
//!
//! What a replica does is `integrations/sync-scenarios.json`, which
//! `scenarios.rs` runs here and the browser's tests run against
//! `FenecSync`; what stays here needs the engine on the other side -- a
//! write made once though sent twice, a replica in a file reopened -- or is
//! the native API's alone.

use fenec_abi::sync::{Sync, TEMP_BASE};
use fenec_core::json;
use fenec_core::prelude::*;
use std::collections::HashMap;

const URL: &str = "http://server";

/// fenec-http as the sync meets it.
struct Server {
    db: Database,
    /// Each collection's shape, as the subscription's query string asks it.
    filters: HashMap<String, Option<Expr>>,
    /// `Idempotency-Key` -> the answer it was given.
    keys: HashMap<String, (u16, u64, String)>,
    /// The next write is refused so, whatever it is.
    refuse: Option<(u16, String)>,
    /// Writes executed: a write sent twice under one key counts once.
    runs: usize,
    streams: Vec<(u64, String, u64)>,
}

impl Server {
    fn new() -> Server {
        let mut db = Database::new();
        for s in [
            "create collection tasks (key text @unique, title text, status text @hash, priority int)",
            r#"put tasks [{key: "a", title: "one", status: "open", priority: 1},
                          {key: "b", title: "two", status: "open", priority: 5},
                          {key: "c", title: "three", status: "closed", priority: 3}]"#,
        ] {
            db.execute(&fenec_ql::parse_one(s).unwrap()).unwrap();
        }
        let mut filters = HashMap::new();
        filters.insert("tasks".to_string(), Some(filter(r#"status = "open""#)));
        Server {
            db,
            filters,
            keys: HashMap::new(),
            refuse: None,
            runs: 0,
            streams: Vec::new(),
        }
    }

    fn rows(&self, sql: &str) -> Vec<Row> {
        match self
            .db
            .query(&fenec_ql::parse_one(sql).unwrap(), &[])
            .unwrap()
        {
            Response::Rows(rs) => rs.rows,
            _ => vec![],
        }
    }

    fn status(e: &Error) -> u16 {
        match e {
            Error::NotFound(_) => 404,
            Error::Exists(_) | Error::Duplicate(_) => 409,
            Error::ReadOnly(_) | Error::Denied(_) => 403,
            Error::Type(_) | Error::Query(_) => 400,
            _ => 500,
        }
    }

    fn error(e: &str) -> String {
        let mut s = String::from("{\"error\":");
        json::escape_into(&mut s, e);
        s.push('}');
        s
    }

    fn statement(line: &str) -> Result<(Statement, Vec<Value>)> {
        let obj = json::parse_object_listing(line, "params")?;
        let get = |k: &str| obj.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let Some(Value::Text(q)) = get("query") else {
            return Err(Error::Query("`query` is required".into()));
        };
        let params = match get("params") {
            Some(Value::List(l)) => l,
            _ => vec![],
        };
        Ok((fenec_ql::parse_one(&q)?, params))
    }

    /// `POST /query` and `/batch`, a block each, under their key.
    fn post(&mut self, path: &str, key: Option<&str>, body: &str) -> (u16, u64, String) {
        if let Some(k) = key {
            if let Some((s, q, b)) = self.keys.get(k) {
                return (*s, *q, b.clone());
            }
        }
        if let Some((s, why)) = self.refuse.take() {
            return (s, 0, Self::error(&why));
        }
        let lines: Vec<&str> = match path {
            "/batch" => body.lines().collect(),
            _ => vec![body],
        };
        let r = (|| -> Result<usize> {
            let stmts = lines
                .iter()
                .map(|l| Self::statement(l))
                .collect::<Result<Vec<_>>>()?;
            self.db.begin()?;
            let mut n = 0;
            for (s, p) in &stmts {
                match self.db.execute_with(s, p) {
                    Ok(Response::Affected(k)) => n = k,
                    Ok(_) => {}
                    Err(e) => {
                        self.db.rollback();
                        return Err(e);
                    }
                }
            }
            self.db.commit()?;
            Ok(n)
        })();
        let answer = match r {
            Ok(n) => {
                self.runs += 1;
                (200, self.db.change_seq(), format!("{{\"affected\":{n}}}"))
            }
            Err(e) => (Self::status(&e), 0, Self::error(&e.to_string())),
        };
        if let (Some(k), 200) = (key, answer.0) {
            self.keys.insert(k.to_string(), answer.clone());
        }
        answer
    }

    fn collections(&self) -> String {
        let mut out = String::from("[");
        let names = ["tasks"];
        for (i, n) in names.iter().enumerate() {
            let Ok(c) = self.db.collection(n) else {
                continue;
            };
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!("{{\"name\":\"{n}\",\"fields\":["));
            for (j, f) in c.schema.fields.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                let index = match &f.index {
                    IndexKind::None => "null".to_string(),
                    IndexKind::Hash { unique: true } => "\"unique\"".into(),
                    IndexKind::Hash { unique: false } => "\"hash\"".into(),
                    IndexKind::Sorted { .. } => "\"sorted\"".into(),
                    _ => "null".into(),
                };
                out.push_str(&format!(
                    "{{\"name\":\"{}\",\"type\":\"{}\",\"index\":{index},\"required\":false}}",
                    f.name,
                    f.ty.name()
                ));
            }
            out.push_str("]}");
        }
        out.push(']');
        out
    }

    /// A stream opened: its seed, or nothing where it resumes inside the
    /// ring, as `sse::serve` answers.
    fn open(&mut self, id: u64, url: &str) -> Vec<u8> {
        let rest = url.strip_prefix(URL).unwrap();
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        let collection = path
            .trim_start_matches('/')
            .trim_end_matches("/changes")
            .to_string();
        let since = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("since="))
            .map(|v| v.parse::<u64>().unwrap());
        let (cursor, bytes) = match since {
            Some(n) => (n, Vec::new()),
            None => self.seed(&collection),
        };
        self.streams.push((id, collection, cursor));
        bytes
    }

    fn seed(&self, collection: &str) -> (u64, Vec<u8>) {
        let filter = self.filters[collection].clone();
        let stmt = Statement::Select(Select {
            collection: collection.into(),
            filter,
            ..Default::default()
        });
        let Response::Rows(rs) = self.db.query(&stmt, &[]).unwrap() else {
            unreachable!()
        };
        let mut rows = String::new();
        json::rows_array_into(&mut rows, &rs);
        let seq = self.db.change_seq();
        (
            seq,
            format!("event: seed\ndata: {{\"seq\":{seq},\"rows\":{rows}}}\n\n").into_bytes(),
        )
    }

    /// What each open stream is sent now.
    fn changes(&mut self) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::new();
        for i in 0..self.streams.len() {
            let (id, c, cursor) = self.streams[i].clone();
            let filter = self.filters[&c].clone();
            match self
                .db
                .changes_since(&c, cursor, filter.as_ref(), None, &[])
                .unwrap()
            {
                Changes::Reseed => {
                    let (seq, bytes) = self.seed(&c);
                    self.streams[i].2 = seq;
                    out.push((id, bytes));
                }
                Changes::Batch(b) => {
                    self.streams[i].2 = b.seq;
                    if b.puts.rows.is_empty() && b.dels.is_empty() {
                        continue;
                    }
                    let mut puts = String::new();
                    json::rows_array_into(&mut puts, &b.puts);
                    let dels: Vec<String> = b.dels.iter().map(|d| d.to_string()).collect();
                    out.push((
                        id,
                        format!(
                            "event: change\ndata: {{\"seq\":{},\"puts\":{puts},\"dels\":[{}],\"schema\":false}}\n\n",
                            b.seq,
                            dels.join(",")
                        )
                        .into_bytes(),
                    ));
                }
            }
        }
        out
    }
}

fn filter(text: &str) -> Expr {
    match fenec_ql::parse_one(&format!("get t where {text}")).unwrap() {
        Statement::Select(s) => s.filter.unwrap(),
        _ => unreachable!(),
    }
}

/// A test's world: the server, the network between, the app's replica.
struct World {
    server: Server,
    client: Database,
    sync: Sync,
    /// Whether the network reaches the server.
    up: bool,
    /// Requests the server takes and whose answers are lost.
    lose: bool,
    timers: Vec<u64>,
    refused: Vec<(u16, String)>,
    tokens: usize,
    streams: Vec<String>,
    requests: Vec<String>,
}

const SHAPES: &str = r#"[{"collection":"tasks","where":{"status":"open"},"key":"key"}]"#;

fn config(shapes: &str) -> String {
    format!(
        r#"{{"url":"{URL}","token":"t0","seed":"0123456789abcdef0123456789abcdef","shapes":{shapes}}}"#
    )
}

impl World {
    fn new() -> World {
        Self::with(Server::new(), Database::new())
    }

    fn with(server: Server, mut client: Database) -> World {
        let sync = Sync::start(&mut client, &config(SHAPES)).unwrap();
        let mut w = World {
            server,
            client,
            sync,
            up: true,
            lose: false,
            timers: vec![],
            refused: vec![],
            tokens: 0,
            streams: vec![],
            requests: vec![],
        };
        w.settle();
        w
    }

    /// Performs every action due, and what they bring, until none is.
    fn settle(&mut self) {
        loop {
            let acts = self.sync.actions();
            let Value::List(acts) = json::parse_json(&acts).unwrap() else {
                panic!("{acts}")
            };
            if acts.is_empty() {
                return;
            }
            for a in acts {
                self.perform(&a);
            }
        }
    }

    fn perform(&mut self, a: &Value) {
        let get = |k: &str| {
            fenec_abi::sync::shape::member(a, k)
                .cloned()
                .unwrap_or(Value::Null)
        };
        let id = match get("id") {
            Value::Int(n) => n as u64,
            _ => 0,
        };
        let text = |v: Value| match v {
            Value::Text(t) => t,
            _ => String::new(),
        };
        match text(get("do")).as_str() {
            "request" => {
                let url = text(get("url"));
                let headers = get("headers");
                let header = |k: &str| match fenec_abi::sync::shape::member(&headers, k) {
                    Some(Value::Text(t)) => Some(t.clone()),
                    _ => None,
                };
                assert_eq!(header("authorization").as_deref(), Some("Bearer t0"));
                self.requests.push(url.clone());
                if !self.up {
                    return self
                        .sync
                        .response(&mut self.client, id, 0, 0, "connection refused");
                }
                let path = url.strip_prefix(URL).unwrap().to_string();
                let (status, seq, body) = match text(get("method")).as_str() {
                    "GET" => (200, 0, self.server.collections()),
                    _ => {
                        let key = header("idempotency-key");
                        assert!(key.is_some(), "a write goes with a key");
                        self.server.post(&path, key.as_deref(), &text(get("body")))
                    }
                };
                match self.lose {
                    true => self.sync.response(&mut self.client, id, 0, 0, "timed out"),
                    false => self.sync.response(&mut self.client, id, status, seq, &body),
                }
            }
            "stream" => {
                let url = text(get("url"));
                self.streams.push(url.clone());
                if !self.up {
                    return self.sync.closed(&mut self.client, id, "connection refused");
                }
                let bytes = self.server.open(id, &url);
                self.sync.opened(&mut self.client, id, 200, "");
                if !bytes.is_empty() {
                    self.sync.bytes(&mut self.client, id, &bytes);
                }
            }
            "cancel" => self.server.streams.retain(|s| s.0 != id),
            "wait" => self.timers.push(id),
            "refused" => {
                let status = match get("status") {
                    Value::Int(n) => n as u16,
                    _ => 0,
                };
                self.refused.push((status, text(get("message"))));
            }
            "token" => self.tokens += 1,
            "changed" | "status" => {}
            other => panic!("an action of no kind: {other}"),
        }
    }

    /// The server's changes reach the open streams, cut into pieces as a
    /// network cuts them.
    fn deliver(&mut self) {
        for (id, bytes) in self.server.changes() {
            for piece in bytes.chunks(7) {
                self.sync.bytes(&mut self.client, id, piece);
            }
        }
        self.settle();
    }

    /// Every connection drops.
    fn drop_connections(&mut self) {
        let ids: Vec<u64> = self.server.streams.drain(..).map(|s| s.0).collect();
        for id in ids {
            self.sync.closed(&mut self.client, id, "connection reset");
        }
        self.settle();
    }

    /// The timers the core asked for run out.
    fn fire(&mut self) {
        for id in std::mem::take(&mut self.timers) {
            self.sync.timer(&mut self.client, id);
        }
        self.settle();
    }

    /// A write through the replica, as `fenec_query` routes it.
    fn write(&mut self, sql: &str, params: &str) -> Result<Response> {
        let p = fenec_abi::prepare(sql, params, &[])?;
        assert!(self.sync.claims(&p.stmts)?, "a synced write: {sql}");
        let r = self.sync.write(&mut self.client, &p, sql);
        self.settle();
        r
    }

    fn local(&self, sql: &str) -> Vec<Row> {
        match self
            .client
            .query(&fenec_ql::parse_one(sql).unwrap(), &[])
            .unwrap()
        {
            Response::Rows(rs) => rs.rows,
            _ => vec![],
        }
    }

    fn titles(&self) -> Vec<String> {
        let rows = self.local("get tasks select title order title");
        rows.iter()
            .map(|r| match &r.values[0] {
                Value::Text(t) => t.clone(),
                _ => String::new(),
            })
            .collect()
    }

    fn status(&self) -> Value {
        json::parse_json(&self.sync.status()).unwrap()
    }

    fn state(&self) -> String {
        match fenec_abi::sync::shape::member(&self.status(), "state") {
            Some(Value::Text(t)) => t.clone(),
            _ => String::new(),
        }
    }

    fn pending(&self) -> i64 {
        match fenec_abi::sync::shape::member(&self.status(), "pending") {
            Some(Value::Int(n)) => *n,
            _ => -1,
        }
    }
}

#[test]
fn an_offline_write_lands_once_its_answer_lost() {
    let mut w = World::new();
    w.up = false;
    w.drop_connections();
    w.write(r#"put tasks {title: "offline", status: "open"}"#, "")
        .unwrap();
    assert_eq!(w.titles(), ["offline", "one", "two"]);
    assert_eq!(w.state(), "offline");
    assert_eq!(w.pending(), 1);
    // Back, the write reaches the server and its answer is lost: sent
    // again under the same key, it is answered and not made again.
    w.up = true;
    w.lose = true;
    w.fire();
    assert_eq!(w.server.runs, 1);
    w.lose = false;
    w.fire();
    assert_eq!(w.server.runs, 1, "the second send is the first's answer");
    assert_eq!(
        w.server.rows(r#"get tasks where title = "offline""#).len(),
        1
    );
    w.fire();
    w.deliver();
    assert_eq!(w.titles(), ["offline", "one", "two"]);
    assert!(w.local(r#"get tasks where title = "offline""#)[0].id < TEMP_BASE as u64);
    assert_eq!(w.pending(), 0);
    assert_eq!(w.state(), "online");
}

#[test]
fn a_restart_keeps_its_pending_writes_and_its_cursor() {
    let dir = std::env::temp_dir().join(format!("fenec-sync-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("replica.fenec");
    let _ = std::fs::remove_file(&path);
    let mut w = World::with(Server::new(), fenec_core::fs::open(&path).unwrap());
    let cursor = w.server.db.change_seq();
    w.up = false;
    w.drop_connections();
    w.write(r#"put tasks {title: "kept", status: "open"}"#, "")
        .unwrap();
    w.write(r#"set tasks {title: "ONE"} where key = "a""#, "")
        .unwrap();
    assert_eq!(w.pending(), 2);
    let World {
        server, mut client, ..
    } = w;
    client.sync().unwrap();
    drop(client);

    // Opened again: the replica, its queue and its cursor as they were.
    let mut client = fenec_core::fs::open(&path).unwrap();
    let sync = Sync::start(&mut client, &config(SHAPES)).unwrap();
    let mut w = World {
        server,
        client,
        sync,
        up: true,
        lose: false,
        timers: vec![],
        refused: vec![],
        tokens: 0,
        streams: vec![],
        requests: vec![],
    };
    assert_eq!(w.titles(), ["ONE", "kept", "two"]);
    assert_eq!(w.pending(), 2);
    w.settle();
    // Sent first, in order, then the stream from where it stopped.
    assert_eq!(w.requests, [format!("{URL}/query"), format!("{URL}/query")]);
    assert_eq!(
        w.streams,
        [format!("{URL}/tasks/changes?status=eq.open&since={cursor}")]
    );
    w.deliver();
    assert_eq!(w.titles(), ["ONE", "kept", "two"]);
    assert_eq!(w.pending(), 0);
    assert_eq!(w.server.runs, 2);
    assert!(w.local("get tasks").iter().all(|r| r.id < TEMP_BASE as u64));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn what_a_replica_cannot_take_is_refused() {
    let mut w = World::new();
    for sql in [
        "drop collection tasks",
        "create index on tasks (title) @hash",
        r#"put _sync_queue {path: "x"}"#,
    ] {
        let p = fenec_abi::prepare(sql, "", &[]).unwrap();
        assert!(w.sync.claims(&p.stmts).is_err(), "{sql}");
    }
    // A local collection beside the replica is the app's own.
    w.client
        .execute(&fenec_ql::parse_one("create collection notes (t text)").unwrap())
        .unwrap();
    let p = fenec_abi::prepare(r#"put notes {t: "x"}"#, "", &[]).unwrap();
    assert!(!w.sync.claims(&p.stmts).unwrap());
    let p = fenec_abi::prepare(r#"put notes {t: "x"}; put tasks {title: "y"}"#, "", &[]).unwrap();
    assert!(w.sync.claims(&p.stmts).is_err());
    w.settle();
}
