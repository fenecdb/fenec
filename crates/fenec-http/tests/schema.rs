//! `/_schema`: a schema declared in code against a server's database --
//! described, compared as a client that follows it, and applied by what
//! owns it, never by a scoped token.

use fenec_core::prelude::*;
use fenec_http::access::Access;
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::Duration;

const SECRET: &[u8] = b"thirty-two bytes and a few more, for HS256";
const ROOT: &str = "root-token";

struct Node {
    port: u16,
    access: Arc<Access>,
}

fn start() -> Node {
    let access = Arc::new(Access::new(SECRET, "todos  read,write\n").unwrap());
    let mut db = Database::new();
    for sql in [
        "create collection todos (title text required, done bool @hash)",
        "create collection hidden (x int)",
        "put todos {title: \"a\", done: false}",
    ] {
        db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
    }
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        token: Some(ROOT.into()),
        access: Some(Arc::clone(&access)),
        ..Config::default()
    };
    let server = Server::new(Arc::new(RwLock::new(db)), cfg);
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node { port, access }
}

impl Node {
    fn call(&self, token: &str, method: &str, target: &str, body: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            s,
            "{method} {target} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let body = out
            .split_once("\r\n\r\n")
            .map_or("", |(_, b)| b)
            .to_string();
        (out[9..12].parse().unwrap(), body)
    }
}

const TODOS: &str = r#"{"format":1,"collections":[{"name":"todos","fields":[{"name":"title","type":"text","required":true},{"name":"done","type":"bool","index":{"kind":"hash"}}]}]}"#;

#[test]
fn the_schema_is_described_compared_and_applied_by_its_owner() {
    let n = start();
    let user = n.access.mint(r#"{"sub":"u1"}"#).unwrap();

    // Described, as a client pulls it; a scoped token sees what it reads.
    let (status, all) = n.call(ROOT, "GET", "/_schema", "");
    assert_eq!(status, 200);
    assert!(all.contains(r#""name":"hidden""#), "{all}");
    let (_, mine) = n.call(&user, "GET", "/_schema", "");
    assert_eq!(
        mine,
        r#"{"format":1,"collections":[{"name":"todos","fields":[{"name":"title","type":"text","required":true},{"name":"done","type":"bool","index":{"kind":"hash"}}]}]}"#
    );

    // Followed: the server holds what the code declares, and more.
    let (status, f) = n.call(&user, "POST", "/_schema/plan?mode=follow", TODOS);
    assert_eq!(status, 200, "{f}");
    assert!(f.contains(r#""refusals":[]"#), "{f}");
    let more = TODOS.replace(
        r#"{"name":"done""#,
        r#"{"name":"due","type":"timestamp"},{"name":"done""#,
    );
    let (_, f) = n.call(&user, "POST", "/_schema/plan?mode=follow", &more);
    assert!(f.contains(r#""kind":"field_missing""#), "{f}");

    // A scoped token neither plans an apply nor applies.
    for path in ["/_schema/plan", "/_schema/apply"] {
        let (status, _) = n.call(&user, "POST", path, &more);
        assert_eq!(status, 403, "{path}");
    }

    // Owned: `title` is not in the code and `name` is new -- a rename, or
    // a drop and an add? Not guessed: nothing is applied ...
    let renamed = more.replace(r#""name":"title""#, r#""name":"name""#);
    let (status, a) = n.call(ROOT, "POST", "/_schema/apply", &renamed);
    assert_eq!(status, 409, "{a}");
    assert!(
        a.contains(r#""kind":"field_not_declared","collection":"todos","field":"title""#),
        "{a}"
    );
    let (_, rows) = n.call(ROOT, "GET", "/todos?select=title", "");
    assert_eq!(rows, r#"[{"title":"a"}]"#);
    // ... until a migration says, run once and recorded. `hidden`, which
    // the code does not declare, is left as it is.
    let migrated = format!(
        r#"{},"migrations":["alter collection todos rename field title to name"]}}"#,
        &renamed[..renamed.len() - 1]
    );
    let (status, plan) = n.call(ROOT, "POST", "/_schema/plan", &migrated);
    assert_eq!(status, 200, "{plan}");
    assert!(
        plan.contains(r#""applied":false,"ran":true,"migrations":[1]"#),
        "{plan}"
    );
    let (status, a) = n.call(ROOT, "POST", "/_schema/apply", &migrated);
    assert_eq!(status, 200, "{a}");
    assert!(
        a.contains(r#""statements":["alter collection todos add field due timestamp"]"#),
        "{a}"
    );
    let (status, again) = n.call(ROOT, "POST", "/_schema/apply", &migrated);
    assert_eq!(status, 200);
    assert!(
        again.contains(r#""applied":false"#) && again.contains(r#""migrations":[]"#),
        "{again}"
    );
    let (_, rows) = n.call(ROOT, "GET", "/_migrations?select=n,text", "");
    assert_eq!(
        rows,
        r#"[{"n":1,"text":"alter collection todos rename field title to name"}]"#
    );
    let (_, hidden) = n.call(ROOT, "GET", "/hidden?count", "");
    assert_eq!(hidden, r#"{"count":0}"#);

    let (status, e) = n.call(ROOT, "POST", "/_schema/apply", "{\"format\":7}");
    assert_eq!(status, 400, "{e}");
}
