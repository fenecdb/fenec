//! A REST insert's body read before the write lock and its documents built
//! under the read lock, beside the readers (`early_put`,
//! `Database::prepare`), writes what the same insert routed and built under
//! the write lock writes: every answer, every byte of the file -- inserts
//! of many documents and of a few, `?returning=`, json fields read exact,
//! named ids, an id taken and a `@unique` value held twice put back, a
//! type refused, a field or a collection the body names wrongly. A scoped
//! token's documents are held to its rules one by one as before, whether
//! its rules read only the document or another collection.

use fenec_core::prelude::*;
use fenec_http::access::Access;
use fenec_http::api;
use fenec_http::http::{Method, Request};
use fenec_http::{Config, Server};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fenecdb-http-prepared-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{tag}.fenec"));
    let _ = std::fs::remove_file(&path);
    path
}

fn run(db: &mut Database, sql: &str) {
    for s in fenec_ql::parse(sql).expect("parse") {
        db.execute(&s).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

fn rows(db: &Database, sql: &str) -> ResultSet {
    db.query(&fenec_ql::parse_one(sql).unwrap(), &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows()
        .expect("rows")
        .clone()
}

/// A request as the connection reads it: `target` its path and query.
fn request(method: Method, target: &str, token: Option<&str>, body: String) -> Request {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k.to_string(), v.to_string())
        })
        .collect();
    let headers = token
        .map(|t| vec![("Authorization".to_string(), format!("Bearer {t}"))])
        .unwrap_or_default();
    Request {
        method,
        target: target.into(),
        path: path.into(),
        query,
        headers,
        body: body.into_bytes(),
        keep_alive: true,
    }
}

fn post(target: &str, docs: &[String]) -> Request {
    request(Method::Post, target, None, format!("[{}]", docs.join(",")))
}

/// A document of `docs`: a json field whose numbers a body keeps as
/// written, a vector, a `nocase` `@unique` code.
fn doc(i: i64) -> String {
    format!(
        r#"{{"title":"t{i}","kind":"{}","n":{i},"code":"C{i}","meta":{{"lang":"{}","scores":[0.1,0.30000000000000004,1e-7,{i}.5,123456789012]}},"v":[{}.25,0.5,-0.125,1]}}"#,
        ["a", "b", "c"][i as usize % 3],
        ["tr", "en"][i as usize % 2],
        i % 7
    )
}

/// Each request of the workload: inserts of many documents and of a few,
/// some put back, some refused before anything runs.
fn workload() -> Vec<Request> {
    let docs = |from: i64, n: i64| (from..from + n).map(doc).collect::<Vec<_>>();
    let mut out = vec![post("/docs", &docs(0, 40))];
    out.push(post("/docs?returning=id,title,meta", &docs(100, 20)));
    out.push(post("/docs?returning=*", &docs(150, 30)));
    // Below the documents worth building before the lock.
    out.push(post("/docs", &docs(200, 5)));
    // An id the collection holds, after 29 documents: put back whole.
    let mut d = docs(300, 29);
    d.push(r#"{"id":1,"title":"taken"}"#.into());
    out.push(post("/docs", &d));
    // A `@unique` value twice, as `nocase` folds it: put back whole.
    let mut d = docs(400, 24);
    d.push(r#"{"title":"one","code":"dup"}"#.into());
    d.push(r#"{"title":"two","code":"DUP"}"#.into());
    out.push(post("/docs", &d));
    // A value its field's type refuses, after 19 documents.
    let mut d = docs(500, 19);
    d.push(r#"{"title":"typed","n":"x"}"#.into());
    out.push(post("/docs", &d));
    // A field the collection does not have, and a collection not here.
    let mut d = docs(600, 19);
    d.push(r#"{"title":"nope","nope":1}"#.into());
    out.push(post("/docs", &d));
    out.push(post("/nosuch", &docs(700, 20)));
    // Ids named, and documents with fields left out.
    let named: Vec<String> = (0..60)
        .map(|i| match i % 3 {
            0 => format!(r#"{{"id":{},"title":"named {i}","n":{i}}}"#, 5000 + i),
            1 => format!(
                r#"{{"id":{},"meta":{{"deep":{{"list":[1,2.5,[3]]}}}}}}"#,
                5000 + i
            ),
            _ => format!(r#"{{"id":{},"code":"N{i}","v":[1,2,3,4]}}"#, 5000 + i),
        })
        .collect();
    out.push(post("/docs", &named));
    // Another collection, between.
    let ev: Vec<String> = (0..30)
        .map(|i| format!(r#"{{"k":"k{}","n":{i}}}"#, i % 4))
        .collect();
    out.push(post("/ev", &ev));
    out.push(post("/docs", &docs(800, 50)));
    out
}

fn schema(db: &mut Database) {
    run(
        db,
        "create collection docs (title text, kind text @hash, n int @sorted, \
         code text nocase @unique, meta json, v vector<4>)",
    );
    run(db, "create collection ev (k text, n int)");
}

/// What a request was answered: its status, and its body where it wrote.
type Answer = (u16, Vec<u8>);

/// The request as the write lock routed and built it before:
/// `api::route` under it and the statement run there.
fn under_the_lock(db: &mut Database, req: &Request) -> Answer {
    let routed = match api::route(db, req) {
        Ok(r) => r,
        Err(e) => return (api::status_of(&e), Vec::new()),
    };
    match db.execute_with(&routed.statement, &[]) {
        Ok(r) => {
            let resp = api::render(&r, &routed.shape, fenec_core::VERSION);
            (resp.status, resp.body)
        }
        Err(e) => (api::status_of(&e), Vec::new()),
    }
}

fn served(db: &Arc<RwLock<Database>>, cfg: &Config, req: &Request) -> Answer {
    let resp = fenec_http::handle(db, cfg, req);
    match resp.status < 300 {
        true => (resp.status, resp.body),
        false => (resp.status, Vec::new()),
    }
}

fn answers(db: &Database) -> Vec<ResultSet> {
    [
        "get docs order id",
        "get docs where kind = \"b\" order n desc",
        "get docs where meta.lang = \"tr\" order id",
        "get docs where code = \"c7\"",
        "get ev order id",
    ]
    .iter()
    .map(|q| rows(db, q))
    .collect()
}

#[test]
fn a_rest_insert_built_beside_the_readers_writes_what_one_built_under_the_lock_writes() {
    let (a_path, b_path) = (tmp("rest-a"), tmp("rest-b"));
    let mut a = fenec_core::fs::open_mapped(&a_path).unwrap();
    let mut b = fenec_core::fs::open_mapped(&b_path).unwrap();
    schema(&mut a);
    schema(&mut b);
    let b = Arc::new(RwLock::new(b));
    let cfg = Config::default();
    let mut statuses = Vec::new();
    for req in workload() {
        let x = under_the_lock(&mut a, &req);
        let y = served(&b, &cfg, &req);
        assert_eq!(
            x,
            y,
            "{} {}: {}",
            req.target,
            x.0,
            String::from_utf8_lossy(&y.1)
        );
        statuses.push(x.0);
    }
    assert_eq!(
        statuses,
        [201, 201, 201, 201, 409, 409, 400, 400, 404, 201, 201, 201]
    );
    let b = Arc::into_inner(b).unwrap().into_inner().unwrap();
    assert_eq!(answers(&a), answers(&b));
    assert_eq!(a.change_seq(), b.change_seq());
    drop((a, b));
    let (x, y) = (
        std::fs::read(&a_path).unwrap(),
        std::fs::read(&b_path).unwrap(),
    );
    assert!(x == y, "{} and {} bytes differ", x.len(), y.len());
}

const SECRET: &[u8] = b"thirty-two bytes and a few more, for HS256";
const ROOT: &str = "root-token";

/// A scoped token's insert of many documents is held to its rules by the
/// `Check` hook, each document: one outside them refuses the whole, as
/// built under the lock. A token none of whose rules reads another
/// collection has the documents built before the lock; one whose rules do
/// is bound under it, so its insert is built there as before.
#[test]
fn a_scoped_rest_insert_is_held_to_its_rules_document_by_document() {
    let notes = "notes  read,insert  where owner = $jwt.sub\n";
    let posts =
        "posts  read,insert  where room in (get members select room where user = $jwt.sub)\n\
                 members  read  where user = $jwt.sub\n";
    for policy in [notes.to_string(), format!("{notes}{posts}")] {
        let access = Arc::new(Access::new(SECRET, &policy).unwrap());
        let mut db = Database::new();
        run(
            &mut db,
            "create collection notes (owner text, t text, n int); \
             create collection posts (room text @hash, author text, t text); \
             create collection members (user text, room text)",
        );
        run(&mut db, r#"put members [{user: "alice", room: "r1"}]"#);
        let cfg = Config {
            token: Some(ROOT.into()),
            access: Some(Arc::clone(&access)),
            ..Config::default()
        };
        let db = Arc::new(RwLock::new(db));
        // Installs the `Check` hook, as a server over the database does.
        let _server = Server::new(Arc::clone(&db), cfg.clone());
        let alice = access.mint(r#"{"sub":"alice"}"#).unwrap();
        let send = |target: &str, docs: Vec<String>| {
            let body = format!("[{}]", docs.join(","));
            let req = request(Method::Post, target, Some(&alice), body);
            fenec_http::handle(&db, &cfg, &req).status
        };
        let note = |i: i64, owner: &str| format!(r#"{{"owner":"{owner}","t":"n{i}","n":{i}}}"#);
        let post =
            |i: i64, room: &str| format!(r#"{{"room":"{room}","author":"alice","t":"p{i}"}}"#);

        assert_eq!(
            send("/notes", (0..30).map(|i| note(i, "alice")).collect()),
            201
        );
        let mut d: Vec<String> = (30..59).map(|i| note(i, "alice")).collect();
        d.push(note(59, "bob"));
        assert_eq!(send("/notes", d), 403, "{policy}");
        let reaches = policy.contains("posts");
        if reaches {
            assert_eq!(
                send("/posts", (0..20).map(|i| post(i, "r1")).collect()),
                201
            );
            let mut d: Vec<String> = (20..39).map(|i| post(i, "r1")).collect();
            d.push(post(39, "r2"));
            assert_eq!(send("/posts", d), 403);
        }

        let g = db.read().unwrap();
        let count = |sql: &str| rows(&g, sql).rows[0].values[0].clone();
        assert_eq!(count("get notes count"), Value::Int(30));
        assert_eq!(
            count("get notes where owner = \"alice\" count"),
            Value::Int(30)
        );
        let posted = if reaches { 20 } else { 0 };
        assert_eq!(count("get posts count"), Value::Int(posted));
    }
}
