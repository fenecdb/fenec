//! `integrations/sync-scenarios.json` run against the sync core: each
//! scenario's server events fed to it in order, each request it asks for
//! answered as the script says, and what the replica holds, what was sent
//! and what was told checked after every step. `web/fenec.sync.scenarios.test.js`
//! runs the same file against the browser's `FenecSync`, so the two are
//! held to one behaviour; where they differ on purpose the file says so,
//! and each side checks its own expectation.
//!
//! Every scenario runs, or the test fails: the names that passed are
//! written to `target/sync-scenarios/rust.txt`, which
//! `integrations/sync-scenarios-check.mjs` holds to the file beside the
//! browser's list.

use fenec_abi::sync::shape::member;
use fenec_abi::sync::Sync;
use fenec_core::json;
use fenec_core::prelude::*;
use std::collections::{HashMap, VecDeque};

const URL: &str = "http://server";

type Check = std::result::Result<(), String>;

fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::Text(t)) => t.clone(),
        _ => String::new(),
    }
}

fn int(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Int(n)) => *n,
        Some(Value::Float(f)) => *f as i64,
        _ => 0,
    }
}

fn obj(members: Vec<(&str, Value)>) -> Value {
    let mut m: Vec<(String, Value)> = members
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    m.sort_by(|a, b| a.0.cmp(&b.0));
    Value::Object(m)
}

/// A scenario's write as the builders write it -- `integrations/builder-golden.json`'s
/// text -- its values parameters, numbered on across a batch's statements.
fn render(w: &Value, params: &mut Vec<Value>) -> String {
    let mut bind = |v: &Value, params: &mut Vec<Value>| {
        params.push(v.clone());
        format!("${}", params.len())
    };
    let doc = |d: &Value,
               params: &mut Vec<Value>,
               bind: &mut dyn FnMut(&Value, &mut Vec<Value>) -> String| {
        let Value::Object(m) = d else {
            panic!("a document is an object")
        };
        // `{"$inc": n}` is inc(n), as the builders write it.
        let parts: Vec<String> = m
            .iter()
            .map(|(k, v)| match v {
                Value::Object(o) if o.len() == 1 && o[0].0 == "$inc" => {
                    format!("{k}: coalesce({k}, 0) + {}", bind(&o[0].1, params))
                }
                v => format!("{k}: {}", bind(v, params)),
            })
            .collect();
        format!("{{{}}}", parts.join(", "))
    };
    let filter = |f: Option<&Value>,
                  params: &mut Vec<Value>,
                  bind: &mut dyn FnMut(&Value, &mut Vec<Value>) -> String| {
        match f {
            Some(Value::Object(m)) if !m.is_empty() => {
                let parts: Vec<String> = m
                    .iter()
                    .map(|(k, v)| format!("{k} = {}", bind(v, params)))
                    .collect();
                format!(" where {}", parts.join(" and "))
            }
            _ => String::new(),
        }
    };
    // `"require": n`: the builders' `{ require: n }`, last in the text.
    let required = match member(w, "require") {
        Some(Value::Int(n)) => format!(" require {n}"),
        _ => String::new(),
    };
    if let Some(Value::Text(c)) = member(w, "insert") {
        let Some(Value::List(docs)) = member(w, "docs") else {
            panic!("an insert has docs")
        };
        let body: Vec<String> = docs.iter().map(|d| doc(d, params, &mut bind)).collect();
        return match body.as_slice() {
            [one] => format!("put {c} {one}{required}"),
            _ => format!("put {c} [{}]{required}", body.join(", ")),
        };
    }
    if let Some(Value::Text(c)) = member(w, "update") {
        let set = doc(
            member(w, "set").expect("an update has set"),
            params,
            &mut bind,
        );
        let wh = filter(member(w, "where"), params, &mut bind);
        return format!("set {c} {set}{wh}{required}");
    }
    if let Some(Value::Text(c)) = member(w, "delete") {
        let wh = filter(member(w, "where"), params, &mut bind);
        return format!("del {c}{wh}{required}");
    }
    if let Some(Value::List(ws)) = member(w, "batch") {
        return ws
            .iter()
            .map(|w| render(w, params))
            .collect::<Vec<_>>()
            .join("; ");
    }
    panic!("a write of no kind: {}", json::to_string(w))
}

/// Expected against actual: an object's members a subset, a list whole, a
/// string `<name>` any value, the same each time the name comes again and
/// another than any other name's.
fn matches(exp: &Value, act: &Value, binds: &mut HashMap<String, String>, at: &str) -> Check {
    match (exp, act) {
        (Value::Text(t), _) if t.len() > 2 && t.starts_with('<') && t.ends_with('>') => {
            let got = json::to_string(act);
            match binds.get(t) {
                Some(b) if *b == got => Ok(()),
                Some(b) => Err(format!("{at}: {t} was {b}, now {got}")),
                None => {
                    if let Some((other, _)) = binds.iter().find(|(_, v)| **v == got) {
                        return Err(format!("{at}: {t} is {got}, which {other} already is"));
                    }
                    binds.insert(t.clone(), got);
                    Ok(())
                }
            }
        }
        (Value::Object(e), Value::Object(_)) => {
            for (k, v) in e {
                let a = member(act, k).cloned().unwrap_or(Value::Null);
                matches(v, &a, binds, &format!("{at}.{k}"))?;
            }
            Ok(())
        }
        (Value::List(e), Value::List(a)) => {
            if e.len() != a.len() {
                return Err(format!(
                    "{at}: {} items expected, {} came: {}",
                    e.len(),
                    a.len(),
                    json::to_string(act)
                ));
            }
            for (i, (x, y)) in e.iter().zip(a).enumerate() {
                matches(x, y, binds, &format!("{at}[{i}]"))?;
            }
            Ok(())
        }
        (Value::Int(x), Value::Float(y)) | (Value::Float(y), Value::Int(x)) if *x as f64 == *y => {
            Ok(())
        }
        (e, a) if e == a => Ok(()),
        (e, a) => Err(format!(
            "{at}: expected {}, got {}",
            json::to_string(e),
            json::to_string(a)
        )),
    }
}

/// A step's data with each fixture (`@name`) and each bound `<name>` put in.
fn resolve(v: &Value, fixtures: &Value, binds: &HashMap<String, String>) -> Value {
    match v {
        Value::Text(t) if t.starts_with('@') => member(fixtures, &t[1..])
            .unwrap_or_else(|| panic!("no fixture {t}"))
            .clone(),
        Value::Text(t) if t.starts_with('<') && t.ends_with('>') => match binds.get(t) {
            Some(b) => json::parse_json(b).unwrap(),
            None => panic!("{t} is used before it is bound"),
        },
        Value::List(l) => Value::List(l.iter().map(|x| resolve(x, fixtures, binds)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| {
                    let keep = k.starts_with("expect");
                    (
                        k.clone(),
                        if keep {
                            x.clone()
                        } else {
                            resolve(x, fixtures, binds)
                        },
                    )
                })
                .collect(),
        ),
        v => v.clone(),
    }
}

/// The app, its replica, and the network as the script plays it.
struct Run {
    db: Database,
    sync: Option<Sync>,
    shapes: Value,
    collections: String,
    up: bool,
    stream_status: Option<u16>,
    /// Open streams: id -> collection.
    streams: Vec<(u64, String)>,
    posts: VecDeque<u64>,
    timers: Vec<u64>,
    // What the step saw.
    requests: Vec<Value>,
    opened: Vec<Value>,
    refused: Vec<Value>,
    token_asked: bool,
    waits: Vec<Value>,
    error: Option<String>,
}

impl Run {
    fn config(&self) -> String {
        format!(
            r#"{{"url":"{URL}","token":"t0","seed":"0123456789abcdef0123456789abcdef","shapes":{}}}"#,
            json::to_string(&self.shapes)
        )
    }

    fn settle(&mut self) {
        for _ in 0..1000 {
            let Some(sync) = self.sync.as_mut() else {
                return;
            };
            let acts = sync.actions();
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
        panic!("the core never settled");
    }

    fn perform(&mut self, a: &Value) {
        let id = int(member(a, "id")) as u64;
        match text(member(a, "do")).as_str() {
            "request" => {
                let url = text(member(a, "url"));
                let path = url.strip_prefix(URL).unwrap().to_string();
                let headers = member(a, "headers").cloned().unwrap_or(Value::Null);
                let method = text(member(a, "method"));
                let mut r = vec![
                    ("method", Value::Text(method.clone())),
                    ("path", Value::Text(path)),
                    (
                        "auth",
                        member(&headers, "authorization")
                            .cloned()
                            .unwrap_or(Value::Null),
                    ),
                    (
                        "key",
                        member(&headers, "idempotency-key")
                            .cloned()
                            .unwrap_or(Value::Null),
                    ),
                ];
                if let Some(Value::Text(body)) = member(a, "body") {
                    let lines: Vec<Value> =
                        body.lines().map(|l| json::parse_json(l).unwrap()).collect();
                    r.push(("lines", Value::Int(lines.len() as i64)));
                    r.push(("body", Value::List(lines)));
                }
                self.requests.push(obj(r));
                if method == "GET" {
                    let sync = self.sync.as_mut().unwrap();
                    match self.up {
                        true => sync.response(&mut self.db, id, 200, 0, &self.collections),
                        false => sync.response(&mut self.db, id, 0, 0, "connection refused"),
                    }
                } else {
                    self.posts.push_back(id);
                }
            }
            "stream" => {
                let url = text(member(a, "url"));
                let path = url.strip_prefix(URL).unwrap().to_string();
                let auth = member(member(a, "headers").unwrap(), "authorization").cloned();
                self.opened.push(obj(vec![
                    ("path", Value::Text(path.clone())),
                    ("auth", auth.unwrap_or(Value::Null)),
                ]));
                let sync = self.sync.as_mut().unwrap();
                if !self.up {
                    return sync.closed(&mut self.db, id, "connection refused");
                }
                if let Some(s) = self.stream_status.take() {
                    return sync.opened(&mut self.db, id, s, "{\"error\":\"refused\"}");
                }
                let c = path
                    .trim_start_matches('/')
                    .split('/')
                    .next()
                    .unwrap()
                    .to_string();
                self.streams.push((id, c));
                self.sync
                    .as_mut()
                    .unwrap()
                    .opened(&mut self.db, id, 200, "");
            }
            "cancel" => self.streams.retain(|s| s.0 != id),
            "wait" => {
                self.timers.push(id);
                self.waits.push(Value::Int(int(member(a, "ms"))));
            }
            "refused" => self.refused.push(Value::Int(int(member(a, "status")))),
            "token" => self.token_asked = true,
            "changed" | "status" => {}
            other => panic!("an action of no kind: {other}"),
        }
    }

    fn start(&mut self) {
        self.streams.clear();
        self.posts.clear();
        self.timers.clear();
        let config = self.config();
        match Sync::start(&mut self.db, &config) {
            Ok(s) => self.sync = Some(s),
            Err(e) => {
                self.sync = None;
                self.error = Some(e.to_string());
            }
        }
    }

    /// An event on the stream of the shape over `collection`.
    fn event(&mut self, collection: &str, name: &str, data: &Value) {
        let id = self
            .streams
            .iter()
            .find(|s| s.1 == collection)
            .unwrap_or_else(|| panic!("no stream open for `{collection}`"))
            .0;
        let bytes = format!("event: {name}\ndata: {}\n\n", json::to_string(data)).into_bytes();
        // Cut as a network cuts it.
        for piece in bytes.chunks(7) {
            self.sync.as_mut().unwrap().bytes(&mut self.db, id, piece);
        }
    }

    fn step(&mut self, step: &Value) {
        self.requests.clear();
        self.opened.clear();
        self.refused.clear();
        self.waits.clear();
        self.token_asked = false;
        self.error = None;
        let shape_collection = |run: &Run| -> String {
            let Value::List(shapes) = &run.shapes else {
                unreachable!()
            };
            let n = int(member(step, "shape")) as usize;
            text(member(&shapes[n], "collection"))
        };
        match text(member(step, "do")).as_str() {
            "start" => {
                if let Some(s) = member(step, "shapes") {
                    self.shapes = s.clone();
                }
                self.start();
            }
            "restart" => {
                self.sync = None;
                if let Some(s) = member(step, "shapes") {
                    self.shapes = s.clone();
                }
                self.start();
            }
            "seed" | "change" => {
                let c = shape_collection(self);
                let mut data = vec![("seq", member(step, "seq").cloned().unwrap_or(Value::Int(0)))];
                for k in ["rows", "puts", "dels", "schema"] {
                    if let Some(v) = member(step, k) {
                        data.push((k, v.clone()));
                    }
                }
                let name = text(member(step, "do"));
                self.event(&c, &name, &obj(data));
            }
            "drop" => {
                let ids: Vec<u64> = self.streams.drain(..).map(|s| s.0).collect();
                for id in ids {
                    self.sync
                        .as_mut()
                        .unwrap()
                        .closed(&mut self.db, id, "connection reset");
                }
            }
            "fire" => {
                for id in std::mem::take(&mut self.timers) {
                    self.sync.as_mut().unwrap().timer(&mut self.db, id);
                }
            }
            "answer" => {
                let id = self
                    .posts
                    .pop_front()
                    .expect("no request waits for an answer");
                let status = int(member(step, "status")) as u16;
                let seq = int(member(step, "seq")) as u64;
                let body = match member(step, "body") {
                    Some(b) => json::to_string(b),
                    None if status == 0 => "connection reset".into(),
                    None => "{}".into(),
                };
                self.sync
                    .as_mut()
                    .unwrap()
                    .response(&mut self.db, id, status, seq, &body);
            }
            "write" => {
                let w = member(step, "write").expect("a write step has `write`");
                let mut params = Vec::new();
                let sql = render(w, &mut params);
                let ps = json::to_string(&Value::List(params));
                let r = fenec_abi::prepare(&sql, &ps, &[]).and_then(|p| {
                    let sync = self.sync.as_mut().unwrap();
                    assert!(sync.claims(&p.stmts)?, "a synced write: {sql}");
                    sync.write(&mut self.db, &p, &sql)
                });
                if let Err(e) = r {
                    self.error = Some(e.to_string());
                }
            }
            "net" => self.up = matches!(member(step, "up"), Some(Value::Bool(true))),
            "stream_status" => self.stream_status = Some(int(member(step, "status")) as u16),
            "online" => {
                let on = matches!(member(step, "online"), Some(Value::Bool(true)));
                self.sync
                    .as_mut()
                    .unwrap()
                    .signal(&mut self.db, &format!("{{\"online\":{on}}}"))
                    .unwrap();
            }
            "token" => {
                let t = text(member(step, "token"));
                self.sync
                    .as_mut()
                    .unwrap()
                    .signal(
                        &mut self.db,
                        &json::to_string(&obj(vec![("token", Value::Text(t))])),
                    )
                    .unwrap();
            }
            "schema" => {
                self.collections = json::to_string(member(step, "collections").unwrap());
            }
            other => panic!("a step of no kind: {other}"),
        }
        self.settle();
    }

    fn rows(&self, collection: &str) -> Value {
        let stmt = fenec_ql::parse_one(&format!("get {collection}")).unwrap();
        let Ok(Response::Rows(rs)) = self.db.query(&stmt, &[]) else {
            return Value::List(vec![]);
        };
        let mut out = String::new();
        json::rows_array_into(&mut out, &rs);
        let Value::List(mut rows) = json::parse_json(&out).unwrap() else {
            unreachable!()
        };
        rows.sort_by_key(|r| int(member(r, "id")));
        Value::List(rows)
    }

    fn check(&self, expect: &Value, binds: &mut HashMap<String, String>) -> Check {
        if let Some(e) = member(expect, "requests") {
            matches(e, &Value::List(self.requests.clone()), binds, "requests")?;
        }
        if let Some(e) = member(expect, "streams") {
            matches(e, &Value::List(self.opened.clone()), binds, "streams")?;
        }
        if let Some(Value::Object(cs)) = member(expect, "rows") {
            for (c, e) in cs {
                matches(e, &self.rows(c), binds, &format!("rows.{c}"))?;
            }
        }
        if let Some(e) = member(expect, "pending") {
            let st = json::parse_json(&self.sync.as_ref().ok_or("no sync")?.status()).unwrap();
            matches(e, member(&st, "pending").unwrap(), binds, "pending")?;
        }
        if let Some(e) = member(expect, "refused") {
            matches(e, &Value::List(self.refused.clone()), binds, "refused")?;
        }
        if let Some(e) = member(expect, "token_asked") {
            matches(e, &Value::Bool(self.token_asked), binds, "token_asked")?;
        }
        if let Some(Value::List(e)) = member(expect, "waits") {
            if e.len() != self.waits.len() {
                return Err(format!(
                    "waits: {} expected, {:?} asked",
                    e.len(),
                    self.waits
                ));
            }
            for (range, got) in e.iter().zip(&self.waits) {
                let Value::List(r) = range else {
                    panic!("a wait is [min, max]")
                };
                let (lo, hi, got) = (int(r.first()), int(r.get(1)), int(Some(got)));
                if got < lo || got > hi {
                    return Err(format!("waits: {got} ms is not within [{lo}, {hi}]"));
                }
            }
        }
        if let Some(e) = member(expect, "ready") {
            let st = json::parse_json(&self.sync.as_ref().ok_or("no sync")?.status()).unwrap();
            let ready = match member(&st, "shapes") {
                Some(Value::List(l)) => l
                    .iter()
                    .all(|s| matches!(member(s, "seeded"), Some(Value::Bool(true)))),
                _ => false,
            };
            matches(e, &Value::Bool(ready), binds, "ready")?;
        }
        match (member(expect, "error"), &self.error) {
            (Some(Value::Text(want)), Some(got)) if got.contains(want.as_str()) => {}
            (Some(Value::Text(want)), got) => {
                return Err(format!("error: expected one saying `{want}`, got {got:?}"))
            }
            (_, Some(got)) => return Err(format!("an error no expectation names: {got}")),
            _ => {}
        }
        Ok(())
    }
}

/// The step's expectation for this runner: the common one, with the
/// native side's own members over it where the platforms differ.
fn expectation(step: &Value) -> Value {
    let mut e = match member(step, "expect") {
        Some(Value::Object(m)) => m.clone(),
        _ => Vec::new(),
    };
    if let Some(Value::Object(own)) = member(step, "expect_native") {
        for (k, v) in own {
            e.retain(|(x, _)| x != k);
            e.push((k.clone(), v.clone()));
        }
    }
    Value::Object(e)
}

fn run(file: &Value, sc: &Value) -> Check {
    let mut r = Run {
        db: Database::new(),
        sync: None,
        shapes: member(sc, "shapes")
            .or_else(|| member(file, "shapes"))
            .cloned()
            .unwrap(),
        collections: json::to_string(
            member(sc, "collections")
                .or_else(|| member(file, "collections"))
                .unwrap(),
        ),
        up: true,
        stream_status: None,
        streams: vec![],
        posts: VecDeque::new(),
        timers: vec![],
        requests: vec![],
        opened: vec![],
        refused: vec![],
        token_asked: false,
        waits: vec![],
        error: None,
    };
    let mut binds = HashMap::new();
    let Some(Value::List(steps)) = member(sc, "steps") else {
        return Err("no steps".into());
    };
    let differs = member(sc, "differs").is_some();
    for (i, step) in steps.iter().enumerate() {
        if (member(step, "expect_native").is_some() || member(step, "expect_js").is_some())
            && !differs
        {
            return Err(format!(
                "step {i}: a platform's own expectation, and no `differs` saying why"
            ));
        }
        if matches!(member(step, "only"), Some(Value::Text(o)) if o != "native") {
            continue;
        }
        let step = resolve(
            step,
            member(file, "fixtures").unwrap_or(&Value::Null),
            &binds,
        );
        r.step(&step);
        r.check(&expectation(&step), &mut binds)
            .map_err(|e| format!("step {i} ({}): {e}", text(member(&step, "do"))))?;
    }
    Ok(())
}

#[test]
fn every_scenario_holds_for_the_core() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let src = std::fs::read_to_string(format!("{root}/integrations/sync-scenarios.json")).unwrap();
    let file = json::parse_json(&src).unwrap();
    let Some(Value::List(all)) = member(&file, "scenarios") else {
        panic!("no scenarios")
    };
    let mut failed = Vec::new();
    let mut passed = Vec::new();
    for sc in all {
        let name = text(member(sc, "name"));
        match run(&file, sc) {
            Ok(()) => passed.push(name),
            Err(e) => failed.push(format!("{name}: {e}")),
        }
    }
    let dir = format!("{root}/target/sync-scenarios");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(format!("{dir}/rust.txt"), passed.join("\n") + "\n").unwrap();
    assert!(
        failed.is_empty(),
        "{} of {} failed:\n{}",
        failed.len(),
        all.len(),
        failed.join("\n")
    );
}
