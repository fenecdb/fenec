//! A claim a token holds as `null`, as `""` or as a value its rule cannot
//! compare is no claim: the rule naming it does not apply, as when the
//! token lacks it, and grants nothing. Bound as it came, `"sub": null`
//! made `owner = $jwt.sub` the filter `owner = null`, which every row with
//! no owner matched -- every row written before `alter collection notes
//! add field owner text` -- to read, `set` and `del` by every route, and a
//! claim of another kind than its field, ordered by kind, passed every row.

use fenec_http::access::Access;
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const SECRET: &[u8] = b"thirty-two bytes and a few more, for HS256";
const ROOT: &str = "root-token";

struct Node {
    port: u16,
    access: Arc<Access>,
}

fn start(policy: &str, setup: &[&str]) -> Node {
    // The wait after a refusal is the process's (tests/access.rs).
    fenec_http::audit::set_delay(0);
    let access = Arc::new(Access::new(SECRET, policy).unwrap());
    let mut db = fenec_core::prelude::Database::new();
    for sql in setup {
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
    fn token(&self, claims: &str) -> String {
        self.access.mint(claims).unwrap()
    }

    fn send(
        &self,
        token: &str,
        method: &str,
        target: &str,
        headers: &str,
        body: &str,
    ) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            s,
            "{method} {target} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\
             {headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

    fn call(&self, token: &str, method: &str, target: &str, body: &str) -> (u16, String) {
        self.send(token, method, target, "", body)
    }

    fn query(&self, token: &str, sql: &str) -> (u16, String) {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, sql);
        body.push('}');
        self.call(token, "POST", "/query", &body)
    }

    /// The status a subscription opens with, and what it sent within a
    /// moment: a stream a server refuses is answered and closed, one it
    /// takes stays open, its seed first.
    fn subscribe(&self, token: &str, target: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        write!(
            s,
            "GET {target} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\
             Connection: close\r\n\r\n"
        )
        .unwrap();
        let mut heard = String::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut buf = [0u8; 4096];
        let refused = |h: &str| h.contains("\r\n\r\n") && !h.starts_with("HTTP/1.1 200");
        while Instant::now() < deadline && !heard.contains("event: seed") && !refused(&heard) {
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(k) => heard.push_str(&String::from_utf8_lossy(&buf[..k])),
                Err(_) => {}
            }
        }
        (heard[9..12].parse().unwrap(), heard)
    }
}

const NOTES: &str = "\
notes  read,write     where owner = $jwt.sub
memos  read(title)    where owner = $jwt.sub
board  read
";

/// Rows written before the collection had owners, and one of alice's.
const OWNERLESS: &[&str] = &[
    "create collection notes (title text)",
    "put notes [{title: \"before owners\"}, {title: \"imported\"}]",
    "alter collection notes add field owner text",
    "put notes {title: \"alice's\", owner: \"alice\"}",
    "create collection memos (title text, body text, owner text)",
    "put memos {title: \"ownerless memo\", body: \"secret\"}",
    "create collection board (msg text)",
    "put board {msg: \"hello\"}",
];

/// Every way to read and write `notes` that a token with no subject must
/// not have: the collection does not exist for it, as for a token without
/// the claim.
fn refused_everywhere(n: &Node, token: &str) {
    let (status, body) = n.call(token, "GET", "/notes", "");
    assert_eq!(status, 404, "{body}");
    assert!(!body.contains("before owners"), "{body}");
    for sql in [
        "get notes",
        "get notes count",
        "get notes where title = \"imported\"",
        "set notes {title: \"taken\"} where title = \"before owners\"",
        "del notes where title = \"imported\"",
        "insert notes {title: \"mine now\"}",
        "get memos",
    ] {
        let (status, body) = n.query(token, sql);
        assert_eq!(status, 404, "{sql}: {body}");
        assert!(
            !body.contains("before owners") && !body.contains("ownerless"),
            "{body}"
        );
    }
    let (status, body) = n.call(token, "POST", "/notes", r#"{"title":"posted"}"#);
    assert_eq!(status, 404, "{body}");
    let (status, body) = n.call(token, "PATCH", "/notes/1", r#"{"title":"patched"}"#);
    assert_eq!(status, 404, "{body}");
    let (status, body) = n.call(token, "DELETE", "/notes/2", "");
    assert_eq!(status, 404, "{body}");
    let batch = "{\"query\":\"get notes\"}\n{\"query\":\"del notes\"}\n";
    let (status, body) = n.call(token, "POST", "/batch", batch);
    assert!(!body.contains("before owners"), "{status}: {body}");
    // A subscription is opened on the same rules.
    let (status, heard) = n.subscribe(token, "/notes/changes");
    assert_eq!(status, 404, "{heard}");
    assert!(!heard.contains("before owners"), "{heard}");
    // A rule naming no claim still applies.
    let (status, body) = n.call(token, "GET", "/board", "");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("hello"), "{body}");
    // And `/_whoami` shows it alone.
    let (status, body) = n.call(token, "GET", "/_whoami", "");
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("\"board\"") && !body.contains("\"notes\""),
        "{body}"
    );
}

/// Nothing the refused token sent changed a row.
fn untouched(n: &Node) {
    let (_, body) = n.query(ROOT, "get notes select title order id");
    assert_eq!(
        body,
        r#"[{"title":"before owners"},{"title":"imported"},{"title":"alice's"}]"#
    );
}

#[test]
fn a_null_subject_reads_and_writes_no_ones_rows() {
    let n = start(NOTES, OWNERLESS);
    let anonymous = n.token(r#"{"sub":null}"#);
    refused_everywhere(&n, &anonymous);
    untouched(&n);
    // Alice reads hers and none of the rows with no owner.
    let alice = n.token(r#"{"sub":"alice"}"#);
    let (status, body) = n.call(&alice, "GET", "/notes", "");
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("alice's") && !body.contains("before owners"),
        "{body}"
    );
}

/// An app that stores an anonymous user's rows with `owner: ""` and mints
/// `"sub": ""` for them gave every anonymous user every such row; and a
/// subject of `""` kept its Idempotency-Keys under the server token's.
#[test]
fn an_empty_subject_reads_no_rows_and_keeps_no_keys() {
    let mut setup = OWNERLESS.to_vec();
    setup.push("put notes {title: \"blank owner\", owner: \"\"}");
    setup.push("create collection secrets (x int)");
    setup.push("put secrets {x: 42}");
    let n = start(NOTES, &setup);
    let blank = n.token(r#"{"sub":""}"#);
    let (status, body) = n.call(&blank, "GET", "/notes", "");
    assert_eq!(status, 404, "{body}");
    assert!(!body.contains("blank owner"), "{body}");
    let (status, body) = n.query(&blank, "del notes where title = \"blank owner\"");
    assert_eq!(status, 404, "{body}");

    // The server's token writes under a key, and answers with a secret.
    let write = r#"{"query":"set secrets {x: 43} where x = 42 returning x"}"#;
    let keyed = "Idempotency-Key: k-1\r\n";
    let (status, body) = n.send(ROOT, "POST", "/query", keyed, write);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("43"), "{body}");
    // The same request under the same key from a subject of `""` is not
    // the server token's to be answered.
    let (status, body) = n.send(&blank, "POST", "/query", keyed, write);
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("subject"), "{body}");
    assert!(!body.contains("43"), "{body}");
}

/// A claim of a kind its rule cannot compare is no claim: an object, a
/// list where one value is read, a list holding a `null`, a `""` or a
/// list; a number and a bool are values a field may hold.
#[test]
fn a_claim_its_rule_cannot_compare_is_no_claim() {
    let n = start(NOTES, OWNERLESS);
    for claims in [
        r#"{"sub":{"id":"alice"}}"#,
        r#"{"sub":["alice"]}"#,
        r#"{"sub":[]}"#,
        r#"{"sub":[null]}"#,
    ] {
        let t = n.token(claims);
        refused_everywhere(&n, &t);
    }
    untouched(&n);
    // A number is a value: no text owner equals it.
    let seven = n.token(r#"{"sub":7}"#);
    let (status, body) = n.query(&seven, "get notes");
    assert_eq!((status, body.as_str()), (200, "[]"));
}

const ROOMS: &str = "messages  read,insert  where room in $jwt.rooms\n";

#[test]
fn a_list_claim_holding_no_one_is_no_claim() {
    let n = start(
        ROOMS,
        &[
            "create collection messages (room text, body text)",
            "put messages [{room: \"r1\", body: \"in r1\"}, {body: \"in no room\"}, \
             {room: \"\", body: \"in a blank room\"}, {room: \"r2\", body: \"in r2\"}]",
        ],
    );
    for claims in [
        r#"{"sub":"a","rooms":["r1",null]}"#,
        r#"{"sub":"a","rooms":["r1",""]}"#,
        r#"{"sub":"a","rooms":["r1",["r2"]]}"#,
        r#"{"sub":"a","rooms":["r1",{"r":2}]}"#,
        r#"{"sub":"a","rooms":null}"#,
        r#"{"sub":"a","rooms":""}"#,
    ] {
        let t = n.token(claims);
        let (status, body) = n.call(&t, "GET", "/messages", "");
        assert_eq!(status, 404, "{claims}: {body}");
        assert!(
            !body.contains("in no room") && !body.contains("blank room"),
            "{body}"
        );
        // A post naming no room was in the list's `null`.
        let (status, body) = n.query(&t, "insert messages {body: \"nowhere\"}");
        assert_eq!(status, 404, "{claims}: {body}");
        let (status, heard) = n.subscribe(&t, "/messages/changes");
        assert_eq!(status, 404, "{claims}: {heard}");
    }
    let (_, count) = n.query(ROOT, "get messages count");
    assert_eq!(count, r#"[{"count":4}]"#);
    // A list of rooms, a text, and a list of none are what they say.
    let one = n.token(r#"{"sub":"a","rooms":["r1"]}"#);
    let (status, body) = n.query(&one, "get messages select body");
    assert_eq!((status, body.as_str()), (200, r#"[{"body":"in r1"}]"#));
    let text = n.token(r#"{"sub":"a","rooms":"r2"}"#);
    let (status, body) = n.query(&text, "get messages select body");
    assert_eq!((status, body.as_str()), (200, r#"[{"body":"in r2"}]"#));
    let none = n.token(r#"{"sub":"a","rooms":[]}"#);
    let (status, body) = n.query(&none, "get messages");
    assert_eq!((status, body.as_str()), (200, "[]"));
}

/// A claim compared by order holds the other side to its kind: values of
/// two kinds order by kind, every number below every text, so a text
/// clearance admitted every level, and a number every name.
#[test]
fn an_ordered_claim_holds_its_field_to_the_claims_kind() {
    let n = start(
        "files  read  where level <= $jwt.clearance\n\
         people read  where name >= $jwt.from\n\
         grades read  where $jwt.at_least <= score\n\
         flags  read  where state >= $jwt.flag\n",
        &[
            "create collection files (name text, level int @sorted)",
            "put files [{name: \"a\", level: 1}, {name: \"b\", level: 5}, {name: \"c\", level: 9}]",
            "create collection people (name text @sorted)",
            "put people [{name: \"alice\"}, {name: \"bob\"}, {name: \"carol\"}]",
            "create collection grades (score json)",
            "put grades [{score: 3}, {score: 8}, {score: \"A\"}, {score: true}]",
            "create collection flags (state json)",
            "put flags [{state: true}, {state: false}, {state: 1}, {state: \"yes\"}]",
        ],
    );
    let rows = |claims: &str, sql: &str| {
        let t = n.token(claims);
        let (status, body) = n.query(&t, sql);
        assert_eq!(status, 200, "{claims}: {body}");
        body
    };
    let files = "get files select name order name";
    assert_eq!(
        rows(r#"{"clearance":5}"#, files),
        r#"[{"name":"a"},{"name":"b"}]"#
    );
    assert_eq!(rows(r#"{"clearance":"top"}"#, files), "[]");
    assert_eq!(rows(r#"{"clearance":true}"#, files), "[]");
    let people = "get people select name order name";
    assert_eq!(
        rows(r#"{"from":"b"}"#, people),
        r#"[{"name":"bob"},{"name":"carol"}]"#
    );
    assert_eq!(rows(r#"{"from":5}"#, people), "[]");
    // A field of every kind, the claim on the left.
    let grades = "get grades select score order id";
    assert_eq!(rows(r#"{"at_least":5}"#, grades), r#"[{"score":8}]"#);
    assert_eq!(rows(r#"{"at_least":"B"}"#, grades), "[]");
    assert_eq!(rows(r#"{"at_least":"A"}"#, grades), r#"[{"score":"A"}]"#);
    let flags = "get flags select state order id";
    assert_eq!(rows(r#"{"flag":true}"#, flags), r#"[{"state":true}]"#);
    assert_eq!(
        rows(r#"{"flag":false}"#, flags),
        r#"[{"state":true},{"state":false}]"#
    );
}

/// The rules of a transition, of a field list, and of one reading another
/// collection, each naming `$jwt.sub`: none of them applies to a subject
/// of `null`.
#[test]
fn a_null_subject_takes_no_transition_and_reaches_no_other_collection() {
    let n = start(
        "orders     read                where customer = $jwt.sub\n\
         orders     update(status)      where customer = $jwt.sub and status = \"ready\" and new.status = \"cancelled\"\n\
         positions  read                where courier in (get orders select courier where customer = $jwt.sub)\n",
        &[
            "create collection orders (customer text, courier text, status text)",
            "put orders [{courier: \"c1\", status: \"ready\"}, \
             {customer: \"alice\", courier: \"c2\", status: \"ready\"}]",
            "create collection positions (courier text, at text)",
            "put positions [{courier: \"c1\", at: \"the docks\"}, {courier: \"c2\", at: \"the square\"}]",
        ],
    );
    let anonymous = n.token(r#"{"sub":null}"#);
    let (status, body) = n.query(
        &anonymous,
        "set orders {status: \"cancelled\"} where status = \"ready\"",
    );
    assert_eq!(status, 404, "{body}");
    let (status, body) = n.query(&anonymous, "get positions");
    assert_eq!(status, 404, "{body}");
    assert!(!body.contains("docks"), "{body}");
    let (_, body) = n.query(ROOT, "get orders select status order id");
    assert_eq!(body, r#"[{"status":"ready"},{"status":"ready"}]"#);
    // Alice cancels hers and follows her courier alone.
    let alice = n.token(r#"{"sub":"alice"}"#);
    let (status, body) = n.query(
        &alice,
        "set orders {status: \"cancelled\"} where status = \"ready\"",
    );
    assert_eq!(status, 200, "{body}");
    let (_, body) = n.query(ROOT, "get orders select status order id");
    assert_eq!(body, r#"[{"status":"ready"},{"status":"cancelled"}]"#);
    let (_, body) = n.query(&alice, "get positions select at");
    assert_eq!(body, r#"[{"at":"the square"}]"#);
}
