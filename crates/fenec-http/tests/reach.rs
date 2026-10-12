//! Rules that read another collection: a customer reads the position of
//! the courier carrying their order, a member writes to the rooms they
//! belong to -- through every read path, every write the `Check` hook
//! judges, a `/batch` whose line changes what the next one may do, a
//! stream whose rows come and go as the other collection is written, and a
//! policy whose rules would read each other refused at startup.

use fenec_core::prelude::*;
use fenec_http::access::{Access, MAX_CLAIM_VALUES};
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
    server: Arc<Server>,
}

fn start_with(policy: &str, setup: &[&str]) -> Node {
    // Every test here asks from 127.0.0.1; the wait after a refusal is the
    // process's (tests/audit.rs holds it).
    fenec_http::audit::set_delay(0);
    let access = Arc::new(Access::new(SECRET, policy).unwrap());
    let mut db = Database::new();
    for sql in setup {
        db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
    }
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        token: Some(ROOT.into()),
        access: Some(Arc::clone(&access)),
        mcp: Some(Arc::new(fenec_http::mcp::Options {
            calls_per_sec: 0,
            ..Default::default()
        })),
        ..Config::default()
    };
    let server = Arc::new(Server::new(Arc::new(RwLock::new(db)), cfg));
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    let s = Arc::clone(&server);
    std::thread::spawn(move || {
        let _ = s.serve_on(listener);
    });
    Node {
        port,
        access,
        server,
    }
}

impl Node {
    fn token(&self, claims: &str) -> String {
        self.access.mint(claims).unwrap()
    }

    fn call_with(
        &self,
        token: Option<&str>,
        method: &str,
        target: &str,
        headers: &str,
        body: &str,
    ) -> (u16, String, String) {
        request(self.port, token, method, target, headers, body)
    }

    fn call(&self, token: Option<&str>, method: &str, target: &str, body: &str) -> (u16, String) {
        let (status, _, body) = self.call_with(token, method, target, "", body);
        (status, body)
    }

    fn query(&self, token: &str, sql: &str) -> (u16, String) {
        self.query_with(token, sql, "[]")
    }

    fn query_with(&self, token: &str, sql: &str, params: &str) -> (u16, String) {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, sql);
        body.push_str(",\"params\":");
        body.push_str(params);
        body.push('}');
        self.call(Some(token), "POST", "/query", &body)
    }

    fn batch(&self, token: &str, lines: &[&str]) -> (u16, String) {
        // A statement a line.
        let mut body = String::new();
        for l in lines {
            body.push_str("{\"query\":");
            fenec_core::json::escape_into(&mut body, l);
            body.push_str("}\n");
        }
        self.call(Some(token), "POST", "/batch", &body)
    }

    /// A stream of `target`, its head and seed read.
    fn stream(&self, token: &str, target: &str) -> (TcpStream, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        write!(
            s,
            "GET {target} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\r\n"
        )
        .unwrap();
        let mut heard = String::new();
        listen(&mut s, &mut heard, &|h| h.contains("event: seed"));
        (s, heard)
    }
}

/// A request, its status, head and body: what a client thread sends.
fn request(
    port: u16,
    token: Option<&str>,
    method: &str,
    target: &str,
    headers: &str,
    body: &str,
) -> (u16, String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{auth}{headers}Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    (
        head[9..12].parse().unwrap(),
        head.to_string(),
        body.to_string(),
    )
}

/// Reads the stream into `heard` until `until` holds: a bound on a wait
/// for something that must come, never on whether it comes.
fn listen(s: &mut TcpStream, heard: &mut String, until: &dyn Fn(&str) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut buf = [0u8; 4096];
    while !until(heard) {
        assert!(Instant::now() < deadline, "never came: {heard}");
        match s.read(&mut buf) {
            Ok(0) => panic!("the stream ended: {heard}"),
            Ok(k) => heard.push_str(&String::from_utf8_lossy(&buf[..k])),
            Err(_) => {}
        }
    }
}

/// The change events heard, in order.
fn changes(heard: &str) -> Vec<&str> {
    heard
        .split("event: change\ndata: ")
        .skip(1)
        .map(|e| e.split("\n\n").next().unwrap_or(""))
        .collect()
}

const DISPATCH: &str = "\
orders     read   where customer = $jwt.sub                                       for customer
orders     read   where courier = $jwt.sub                                        for courier
positions  read   where courier in (get orders select courier where customer = $jwt.sub and (status = \"assigned\" or status = \"picked\"))  for customer
positions  read,insert,update(loc, note)  where courier = $jwt.sub                for courier
customers  read   where name in (get orders select customer where courier = $jwt.sub and status = \"assigned\")  for courier
orders     update(courier, status)  where status = \"ready\" and new.courier = $jwt.sub and new.status = \"assigned\"  for courier
orders     read   where status = \"ready\"                                        for courier
";

fn dispatch() -> Node {
    start_with(
        DISPATCH,
        &[
            "create collection orders (customer text @hash, courier text @hash, status text @hash)",
            "create collection positions (courier text @unique, loc int, note text @text)",
            "create collection customers (name text @unique, door text)",
            r#"put positions [{courier: "c1", loc: 1, note: "fast bike"}, {courier: "c2", loc: 2, note: "fast car"}, {courier: "c3", loc: 3, note: "fast feet"}]"#,
            r#"put orders [{customer: "alice", courier: "c1", status: "assigned"},
                           {customer: "alice", courier: "c2", status: "delivered"},
                           {customer: "bob", courier: "c3", status: "picked"},
                           {customer: "carol", status: "ready"}]"#,
            r#"put customers [{name: "alice", door: "1A"}, {name: "bob", door: "2B"}, {name: "carol", door: "3C"}]"#,
        ],
    )
}

/// The couriers named in `body`, in order.
fn couriers(body: &str) -> Vec<String> {
    body.split("\"courier\":\"")
        .skip(1)
        .map(|s| s.split('"').next().unwrap().to_string())
        .collect()
}

/// A customer reads the position of the courier carrying their order, and
/// no other, by every way a read is sent; the server's token reads all.
#[test]
fn a_rule_reaches_another_collection_on_every_read_path() {
    let n = dispatch();
    let alice = n.token(r#"{"sub":"alice","role":"customer"}"#);
    let bob = n.token(r#"{"sub":"bob","role":"customer"}"#);
    let dave = n.token(r#"{"sub":"dave","role":"customer"}"#);

    let (status, body) = n.query(&alice, "get positions select courier");
    assert_eq!(
        (status, couriers(&body)),
        (200, vec!["c1".to_string()]),
        "{body}"
    );
    let (_, body) = n.query(&bob, "get positions select courier");
    assert_eq!(couriers(&body), ["c3"], "{body}");
    let (_, body) = n.query(&dave, "get positions select courier");
    assert_eq!(body, "[]");
    let (_, body) = n.query(ROOT, "get positions select courier");
    assert_eq!(couriers(&body), ["c1", "c2", "c3"]);

    // REST, a count, a facet, `match` and its score's statistics.
    let (status, body) = n.call(Some(&alice), "GET", "/positions?select=courier", "");
    assert_eq!(
        (status, couriers(&body)),
        (200, vec!["c1".to_string()]),
        "{body}"
    );
    assert_eq!(n.query(&alice, "get positions count").1, r#"[{"count":1}]"#);
    let (_, body) = n.query(&alice, "get positions select courier facet courier");
    assert!(
        body.contains(r#""facets""#) && !body.contains("c3"),
        "{body}"
    );
    let (_, body) = n.query(&alice, r#"get positions match note "fast" select courier"#);
    assert_eq!(couriers(&body), ["c1"], "{body}");

    // A `lookup` level, and an inner `get` of the token's own.
    let (_, body) = n.query(
        &alice,
        "get orders select id, status lookup positions on courier = courier select courier",
    );
    assert_eq!(couriers(&body), ["c1"], "{body}");
    let (_, body) = n.query(
        &alice,
        "get orders select courier where courier in (get positions select courier)",
    );
    assert_eq!(couriers(&body), ["c1"], "{body}");

    // A `/batch` of reads, one snapshot.
    let (status, body) = n.batch(
        &alice,
        &["get positions select courier", "get positions count"],
    );
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(r#""courier":"c1""#) && body.contains(r#"[{"count":1}]"#),
        "{body}"
    );
    assert!(!body.contains("c3") && !body.contains("c2"), "{body}");

    // A scoped poll: its tag is its answer's, and the answer moves with
    // the orders, which no write to `positions` says.
    let poll = |tag: &str| {
        let body = r#"{"query":"get positions select courier"}"#;
        let (status, head, _) = n.call_with(
            Some(&alice),
            "POST",
            "/query",
            &format!("If-None-Match: {tag}\r\n"),
            body,
        );
        let tag = head
            .lines()
            .find_map(|l| l.strip_prefix("ETag: "))
            .unwrap_or_default()
            .to_string();
        (status, tag)
    };
    let (status, tag) = poll("\"0\"");
    assert_eq!(status, 200);
    assert_eq!(poll(&tag), (304, tag.clone()));
    let r = n.query(
        ROOT,
        r#"set orders {customer: "alice"} where courier = "c3""#,
    );
    assert_eq!(r.0, 200, "{}", r.1);
    let (status, moved) = poll(&tag);
    assert_eq!(status, 200);
    assert_ne!(moved, tag);
    let (_, body) = n.query(&alice, "get positions select courier");
    assert_eq!(couriers(&body), ["c1", "c3"], "{body}");

    // The rule as the policy writes it.
    let (status, body) = n.call(Some(&alice), "GET", "/_whoami", "");
    assert_eq!(status, 200);
    assert!(
        body.contains(
            r#""rows":"courier in (get orders select courier where customer = $jwt.sub and (status = \"assigned\" or status = \"picked\"))""#
        ),
        "{body}"
    );
}

/// A courier reads the door of the customer whose order they carry: a
/// claim and the read in one `/batch` find it, the read first finds
/// nothing -- each line held to the rules as they answer when it runs.
#[test]
fn a_batch_holds_each_line_to_the_rules_as_the_lines_before_left_them() {
    let n = dispatch();
    let c4 = n.token(r#"{"sub":"c4","role":"courier"}"#);
    let claim = r#"set orders {courier: "c4", status: "assigned"} where customer = "carol" and status = "ready" require 1"#;
    let door = r#"get customers select door"#;

    // Read first: nothing; the claim after it lands.
    let (status, body) = n.batch(&c4, &[door, claim, door]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        r#"{"ok":3,"results":[{"rows":[]},{"affected":1},{"rows":[{"door":"3C"}]}]}"#
    );
    let (_, body) = n.query(&c4, door);
    assert_eq!(body, r#"[{"door":"3C"}]"#);
}

const ROOMS: &str = "\
members   read     where user = $jwt.sub
members   insert   where user = $jwt.sub
messages  read,insert  where room in (get members select room where user = $jwt.sub)
messages  update(text)  where author = $jwt.sub and room in (get members select room where user = $jwt.sub)
messages  update(room)  where author = $jwt.sub and new.room in (get members select room where user = $jwt.sub)
messages  delete   where author = $jwt.sub and room in (get members select room where user = $jwt.sub)
";

fn rooms() -> Node {
    start_with(
        ROOMS,
        &[
            "create collection members (room text @hash, user text @hash)",
            "create collection messages (room text @hash, author text, text text, key text @unique)",
            r#"put members [{room: "r1", user: "alice"}, {room: "r1", user: "bob"}, {room: "r3", user: "alice"}]"#,
            r#"put messages [{room: "r1", author: "bob", text: "hi", key: "k1"}, {room: "r2", author: "carol", text: "secret", key: "k2"}]"#,
        ],
    )
}

/// Every write the rules judge sees them answered: an insert's document,
/// an update's row before and after, a transition's pair, an upsert's row
/// written over, `returning`'s rows, a delete's pick -- and a membership
/// taken away takes the room with it.
#[test]
fn writes_are_held_to_rules_that_read_another_collection() {
    let n = rooms();
    let alice = n.token(r#"{"sub":"alice"}"#);
    let insert = |room: &str, key: &str| {
        n.query(
            &alice,
            &format!(r#"insert messages {{room: "{room}", author: "alice", text: "x", key: "{key}"}} returning id, room"#),
        )
    };
    let (status, body) = insert("r1", "a1");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""room":"r1""#), "{body}");
    let (status, body) = insert("r2", "a2");
    assert_eq!(status, 403, "a room she is no member of: {body}");
    // REST's insert too.
    let (status, body) = n.call(
        Some(&alice),
        "POST",
        "/messages",
        r#"{"room":"r2","author":"alice","text":"x","key":"a3"}"#,
    );
    assert_eq!(status, 403, "{body}");

    // A `set` picks among the rows its rules reach, and answers them.
    let (status, body) = n.query(
        &alice,
        r#"set messages {text: "edited"} where key = "a1" require 1 returning text"#,
    );
    assert_eq!((status, body.as_str()), (200, r#"[{"text":"edited"}]"#));
    let (status, body) = n.query(&alice, r#"set messages {text: "mine"} where key = "k1""#);
    assert_eq!(
        (status, body.as_str()),
        (200, r#"{"affected":0}"#),
        "bob's row"
    );
    // A transition reaching `members` for the row after: to a room of
    // hers, and not to one she is no member of.
    let (status, body) = n.query(
        &alice,
        r#"set messages {room: "r3"} where key = "a1" require 1"#,
    );
    assert_eq!(status, 200, "{body}");
    let (status, body) = n.query(&alice, r#"set messages {room: "r2"} where key = "a1""#);
    assert_eq!(status, 403, "{body}");
    // An upsert sets a row only where its update rules admit it: bob's
    // row by its key is not hers.
    let (status, body) = n.query(
        &alice,
        r#"put messages {room: "r1", author: "alice", text: "y", key: "k1"} if absent else set {text: new.text}"#,
    );
    assert_eq!(status, 403, "{body}");
    let (status, body) = n.query(
        &alice,
        r#"put messages {room: "r3", author: "alice", text: "z", key: "a1"} if absent else set {text: new.text}"#,
    );
    assert_eq!(status, 200, "{body}");
    let (_, body) = n.query(ROOT, r#"get messages select text where key = "a1""#);
    assert_eq!(body, r#"[{"text":"z"}]"#);

    // She leaves `r3`: its rows are gone from her reads and her writes.
    let r = n.query(ROOT, r#"del members where user = "alice" and room = "r3""#);
    assert_eq!(r.0, 200, "{}", r.1);
    let (_, body) = n.query(&alice, "get messages select key order key");
    assert_eq!(body, r#"[{"key":"k1"}]"#);
    let (status, body) = n.query(&alice, r#"del messages where key = "a1""#);
    assert_eq!((status, body.as_str()), (200, r#"{"affected":0}"#));
    assert_eq!(insert("r3", "a4").0, 403);
    // The server's token is held to none of it.
    let (_, body) = n.query(ROOT, "get messages count");
    assert_eq!(body, r#"[{"count":3}]"#);
}

/// Joining a room and writing to it in one `/batch` lands: the second line
/// is held to the rules as the first left them. In the other order the
/// write is refused, and nothing of the batch stays.
#[test]
fn a_batch_answers_the_rules_again_for_each_line() {
    let n = rooms();
    let alice = n.token(r#"{"sub":"alice"}"#);
    let join = r#"insert members {room: "r2", user: "alice"}"#;
    let write = r#"insert messages {room: "r2", author: "alice", text: "hello", key: "a9"}"#;
    let (status, body) = n.batch(&alice, &[write, join]);
    assert_eq!(status, 403, "{body}");
    assert!(body.contains(r#""at":0"#), "{body}");
    assert_eq!(n.query(ROOT, "get members count").1, r#"[{"count":3}]"#);
    let (status, body) = n.batch(&alice, &[join, write, "get messages select key order key"]);
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(r#""key":"a9""#) && body.contains(r#""key":"k2""#),
        "{body}"
    );
}

/// A rule's list is held to a list claim's bound and refused past it,
/// never cut: 1 000 rooms are read, 1 001 are a query error naming the
/// bound -- a read and a write alike; the same room listed twice is one.
#[test]
fn a_rule_s_list_past_the_bound_is_refused() {
    let n = rooms();
    let alice = n.token(r#"{"sub":"alice"}"#);
    // 1 000 rooms in all, `r1` and `r3` among them, and `r1` again.
    let more: Vec<String> = (0..MAX_CLAIM_VALUES - 2)
        .map(|i| format!(r#"{{room: "x{i}", user: "alice"}}"#))
        .chain([r#"{room: "r1", user: "alice"}"#.to_string()])
        .collect();
    let r = n.query(ROOT, &format!("put members [{}]", more.join(", ")));
    assert_eq!(r.0, 200, "{}", r.1);
    let (status, body) = n.query(&alice, "get messages count");
    assert_eq!((status, body.as_str()), (200, r#"[{"count":1}]"#));
    let r = n.query(ROOT, r#"insert members {room: "x-last", user: "alice"}"#);
    assert_eq!(r.0, 200, "{}", r.1);
    let (status, body) = n.query(&alice, "get messages count");
    assert_eq!(status, 400, "{body}");
    assert!(
        body.contains("1000") && body.contains("get members"),
        "{body}"
    );
    let (status, body) = n.query(
        &alice,
        r#"insert messages {room: "r1", author: "alice", text: "x", key: "b1"}"#,
    );
    assert_eq!(status, 400, "{body}");
    // Bob's list is his own: 1 500 rows of one room, read again grouped by
    // room once they fill the page, are a list of one.
    let bob = n.token(r#"{"sub":"bob"}"#);
    let again: Vec<&str> = (0..1_500).map(|_| r#"{room: "r1", user: "bob"}"#).collect();
    let r = n.query(ROOT, &format!("put members [{}]", again.join(", ")));
    assert_eq!(r.0, 200, "{}", r.1);
    assert_eq!(n.query(&bob, "get messages count").1, r#"[{"count":1}]"#);
}

/// Rules that read each other's collections, at any depth, are refused at
/// startup, and so is what an inner `get` may not say; a chain is not a
/// ring.
#[test]
fn rules_reading_each_other_are_refused_at_startup() {
    for bad in [
        "positions read where courier in (get orders select courier where customer = $jwt.sub)\n\
         orders read where id in (get positions select job)",
        "a read where x in (get b select y)\nb read where y in (get c select z)\n\
         c read where z in (get a select x) for admin",
        "a read where x in (get a select y)",
        "* read where x in (get b select y)",
        "a read where x in (get b select y, z)",
        "a read where x in (get b select y limit $jwt.n)",
        "a read where x in (get b select y where expired())",
        "a read where x in (get b select y where new.y = 1)",
    ] {
        let e = Access::new(SECRET, bad).err();
        assert!(e.is_some(), "{bad}");
    }
    let e = Access::new(
        SECRET,
        "positions read where courier in (get orders select courier)\n\
         orders read where id in (get positions select job)",
    )
    .err()
    .unwrap();
    assert!(
        e.contains("`positions` -> `orders` -> `positions`") && e.contains("lines 1, 2"),
        "{e}"
    );
    // A chain: `a` reads `b`, `b` reads `c`.
    Access::new(
        SECRET,
        "a read where x in (get b select y)\nb read where y in (get c select z)\nc read",
    )
    .unwrap();
}

/// A rule reading a collection the token has no rule for reads none of its
/// rows -- no error, and nothing of what it holds.
#[test]
fn a_rule_reading_a_collection_the_token_may_not_read_admits_nothing() {
    let n = start_with(
        "positions read where courier in (get orders select courier where customer = $jwt.sub)",
        &[
            "create collection orders (customer text, courier text)",
            "create collection positions (courier text)",
            r#"put orders {customer: "alice", courier: "c1"}"#,
            r#"put positions {courier: "c1"}"#,
        ],
    );
    let alice = n.token(r#"{"sub":"alice"}"#);
    assert_eq!(n.query(&alice, "get positions").1, "[]");
    let (status, _) = n.query(&alice, "get orders");
    assert_eq!(status, 404);
}

/// A customer's stream of positions gains their courier as an order is
/// claimed and loses them as it is delivered, with no write to `positions`
/// between -- and hears nothing of other couriers, nor of theirs once gone.
#[test]
fn a_stream_gains_and_loses_rows_as_the_other_collection_moves() {
    let n = dispatch();
    let dave = n.token(r#"{"sub":"dave","role":"customer"}"#);
    let c1 = n.token(r#"{"sub":"c1","role":"courier"}"#);
    let c3 = n.token(r#"{"sub":"c3","role":"courier"}"#);
    let (mut s, mut heard) = n.stream(&dave, "/positions/changes");
    assert!(heard.contains(r#""rows":[]"#), "{heard}");
    let (mut q, mut quiet) = n.stream(&dave, "/positions/changes?select=id&where=false");

    // Dave's order is taken by c1: c1 enters, and the quiet stream hears
    // that something changed, naming nothing.
    let r = n.query(
        ROOT,
        r#"insert orders {customer: "dave", courier: "c1", status: "assigned"}"#,
    );
    assert_eq!(r.0, 200, "{}", r.1);
    listen(&mut s, &mut heard, &|h| changes(h).len() == 1);
    assert!(changes(&heard)[0].contains(r#""courier":"c1""#), "{heard}");
    listen(&mut q, &mut quiet, &|h| changes(h).len() == 1);
    assert!(
        changes(&quiet)[0].contains(r#""puts":[],"dels":[]"#),
        "{quiet}"
    );

    // Another courier moves, then c1: only c1 is heard -- the first would
    // have come before it.
    let mv = |token: &str, loc: i64| {
        let r = n.query(
            token,
            &format!("set positions {{loc: {loc}}} where loc >= 0"),
        );
        assert_eq!(r.0, 200, "{}", r.1);
    };
    mv(&c3, 30);
    mv(&c1, 10);
    listen(&mut s, &mut heard, &|h| changes(h).len() == 2);
    assert!(changes(&heard)[1].contains(r#""loc":10"#), "{heard}");
    // Asked of the events: the head's request id is hex, `c3` among it.
    let named = |c: &str| changes(&heard).iter().any(|e| e.contains(c));
    assert!(
        !named(r#""courier":"c3""#) && !named(r#""id":3"#),
        "{heard}"
    );

    // Delivered: c1 leaves, a deletion of the row it was sent.
    let r = n.query(
        ROOT,
        r#"set orders {status: "delivered"} where customer = "dave""#,
    );
    assert_eq!(r.0, 200, "{}", r.1);
    listen(&mut s, &mut heard, &|h| changes(h).len() == 3);
    assert!(changes(&heard)[2].contains(r#""dels":[1]"#), "{heard}");
    // c1 moves again, then dave's next order goes to c3: c3 enters, and
    // nothing of c1 came between.
    mv(&c1, 11);
    let r = n.query(
        ROOT,
        r#"insert orders {customer: "dave", courier: "c3", status: "picked"}"#,
    );
    assert_eq!(r.0, 200, "{}", r.1);
    listen(&mut s, &mut heard, &|h| changes(h).len() == 4);
    let last = changes(&heard)[3];
    assert!(
        last.contains(r#""courier":"c3""#) && !last.contains(r#""courier":"c1""#),
        "{heard}"
    );
    assert!(!heard.contains(r#""loc":11"#), "{heard}");
}

/// An agent's MCP `query` tool sees what the token's `/query` does.
#[test]
fn an_agent_s_tool_reads_through_the_rule() {
    let n = dispatch();
    let alice = n.token(r#"{"sub":"alice","role":"customer"}"#);
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"query","arguments":{"query":"get positions select courier"},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"t","version":"1"}}}}"#;
    let (status, _, out) = n.call_with(
        Some(&alice),
        "POST",
        "/_mcp",
        "Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\n\
         MCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/call\r\nMcp-Name: query\r\n",
        body,
    );
    assert_eq!(status, 200, "{out}");
    assert!(
        out.contains(r#"\"courier\":\"c1\""#) || out.contains(r#""courier":"c1""#),
        "{out}"
    );
    assert!(!out.contains("c2") && !out.contains("c3"), "{out}");
}

/// A courier's held claim reads the jobs of their zone, which a rule finds
/// in `couriers`: a job of another zone leaves it held, and the courier
/// moved to that zone -- a write to `couriers` alone -- wakes it to take
/// the job, as the claim's look reads what its rules read.
#[test]
fn a_held_claim_is_woken_by_a_write_to_what_its_rules_read() {
    let n = start_with(
        "couriers  read  where user = $jwt.sub\n\
         jobs      read,update(owner)  where zone in (get couriers select zone where user = $jwt.sub)\n",
        &[
            "create collection couriers (user text @unique, zone text)",
            "create collection jobs (zone text @hash, owner text)",
            r#"put couriers {user: "w1", zone: "z1"}"#,
        ],
    );
    let w1 = n.token(r#"{"sub":"w1"}"#);
    let port = n.port;
    let held = std::thread::spawn(move || {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(
            &mut body,
            "set jobs {owner: $1} where owner is null order id limit 1 returning id, zone",
        );
        body.push_str(",\"params\":[\"w1\"]}");
        request(
            port,
            Some(&w1),
            "POST",
            "/query",
            "Fenec-Wait: 20000\r\n",
            &body,
        )
    });
    let t = Instant::now();
    while n.server.waits().held < 1 {
        assert!(t.elapsed() < Duration::from_secs(20), "never held");
        std::thread::sleep(Duration::from_millis(1));
    }
    let r = n.query(ROOT, r#"insert jobs {zone: "z2"}"#);
    assert_eq!(r.0, 200, "{}", r.1);
    let r = n.query(ROOT, r#"set couriers {zone: "z2"} where user = "w1""#);
    assert_eq!(r.0, 200, "{}", r.1);
    let (status, _, body) = held.join().unwrap();
    assert_eq!((status, body.as_str()), (200, r#"[{"id":1,"zone":"z2"}]"#));
}

/// A rule that reads another collection and names the fields it shows: a
/// customer reads the position, time and point of the courier carrying
/// their order, and the name alone of an idle one -- each row masked by the
/// rules as their lists answer, in `/query`, a `lookup` level and a stream,
/// where answered no rule admits a row asked of alone and the carrying
/// courier's point would read `null`.
#[test]
fn a_rule_reaching_another_collection_shows_the_fields_it_names() {
    let n = start_with(
        "orders     read                   where customer = $jwt.sub\n\
         positions  read(courier, at, loc)  where courier in (get orders select courier where customer = $jwt.sub and status = \"picked\")\n\
         positions  read(courier)           where state = \"idle\"\n",
        &[
            "create collection orders (customer text @hash, courier text @hash, status text)",
            "create collection positions (courier text @unique, at int, loc int, state text, phone text)",
            r#"put positions [{courier: "k1", at: 10, loc: 1, state: "dropoff", phone: "111"},
                              {courier: "k2", at: 20, loc: 5, state: "idle", phone: "222"},
                              {courier: "k3", at: 30, loc: 9, state: "dropoff", phone: "333"}]"#,
            r#"put orders [{customer: "alice", courier: "k1", status: "picked"},
                           {customer: "bob", courier: "k3", status: "picked"}]"#,
        ],
    );
    let alice = n.token(r#"{"sub":"alice"}"#);
    // Her courier's point; the idle one's name alone; nothing of bob's, nor
    // anyone's phone or state, which no rule of hers shows.
    let (status, body) = n.query(&alice, "get positions order id");
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        r#"[{"id":1,"courier":"k1","at":10,"loc":1},{"id":2,"courier":"k2","at":null,"loc":null}]"#
    );
    // A `where` over a field a row may not show holds where it shows it.
    let (_, body) = n.query(
        &alice,
        "get positions select courier where loc >= 1 order id",
    );
    assert_eq!(body, r#"[{"courier":"k1"}]"#);
    // A `lookup` level, by its own collection's rules.
    let (status, body) = n.query(
        &alice,
        "get orders select id lookup positions on courier = courier select courier, loc",
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""courier":"k1","loc":1"#), "{body}");
    // A `/batch` of reads, bound once for the batch.
    let (status, body) = n.batch(
        &alice,
        &["get positions select loc order id", "get positions count"],
    );
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(r#"{"rows":[{"loc":1},{"loc":null}]}"#),
        "{body}"
    );

    // A stream: its rows masked as the lists answer at each look.
    let (mut s, mut heard) = n.stream(&alice, "/positions/changes");
    assert!(
        heard.contains(r#""rows":[{"id":1,"courier":"k1","at":10,"loc":1},{"id":2,"courier":"k2","at":null,"loc":null}]"#),
        "{heard}"
    );
    // Her courier moves: heard. The idle one moves: shows what it showed,
    // and is not sent.
    let mv = |courier: &str, loc: i64| {
        let r = n.query(
            ROOT,
            &format!(r#"set positions {{loc: {loc}}} where courier = "{courier}""#),
        );
        assert_eq!(r.0, 200, "{}", r.1);
    };
    mv("k1", 2);
    listen(&mut s, &mut heard, &|h| changes(h).len() == 1);
    assert!(
        changes(&heard)[0].contains(r#""courier":"k1","at":10,"loc":2"#),
        "{heard}"
    );
    mv("k2", 6);
    // The idle courier takes her next order, with no write to `positions`:
    // their row shows its point now, the first change after the one before.
    let r = n.query(
        ROOT,
        r#"insert orders {customer: "alice", courier: "k2", status: "picked"}"#,
    );
    assert_eq!(r.0, 200, "{}", r.1);
    listen(&mut s, &mut heard, &|h| changes(h).len() == 2);
    assert!(
        changes(&heard)[1].contains(r#""courier":"k2","at":20,"loc":6"#),
        "{heard}"
    );
    // Delivered: the first courier, no longer carrying it nor idle, leaves.
    let r = n.query(
        ROOT,
        r#"set orders {status: "delivered"} where courier = "k1""#,
    );
    assert_eq!(r.0, 200, "{}", r.1);
    listen(&mut s, &mut heard, &|h| changes(h).len() == 3);
    assert!(changes(&heard)[2].contains(r#""dels":[1]"#), "{heard}");
    assert!(!heard.contains("phone") && !heard.contains("k3"), "{heard}");
}
