//! Scoped access: two users of one collection, each with a JSON Web Token,
//! may not see each other's rows -- not through any route, not through raw
//! FenecQL, and not as changed ids on a subscription -- nor write outside
//! their rules.

use fenec_core::prelude::*;
use fenec_http::access::Access;
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const SECRET: &[u8] = b"thirty-two bytes and a few more, for HS256";
const ROOT: &str = "root-token";
const POLICY: &str = "\
notes  read,write  where owner = $jwt.sub
board  read
board  write                                  for moderator
";

struct Node {
    port: u16,
    access: Arc<Access>,
}

fn start() -> Node {
    start_with(
        POLICY,
        &[
            "create collection notes (owner text @hash, title text)",
            "create collection board (msg text)",
            "create collection secrets (x int)",
            "put secrets {x: 42}",
        ],
    )
}

fn start_with(policy: &str, setup: &[&str]) -> Node {
    let access = Arc::new(Access::new(SECRET, policy).unwrap());
    let mut db = Database::new();
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

    fn call(&self, token: Option<&str>, method: &str, target: &str, body: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
        write!(
            s,
            "{method} {target} HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
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

    fn query(&self, token: &str, sql: &str) -> (u16, String) {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, sql);
        body.push('}');
        self.call(Some(token), "POST", "/query", &body)
    }
}

/// Reads the stream into `heard` until `until` holds or `budget` passes.
fn listen(s: &mut TcpStream, heard: &mut String, until: &dyn Fn(&str) -> bool, budget: Duration) {
    let deadline = Instant::now() + budget;
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline && !until(heard) {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => heard.push_str(&String::from_utf8_lossy(&buf[..k])),
            Err(_) => {}
        }
    }
}

/// An HS256 token for `claims` exactly as given -- `mint` stamps an `exp`
/// on claims that name none.
fn signed(secret: &[u8], claims: &str) -> String {
    use fenec_http::crypto::{b64url_encode, hmac_sha256};
    let head = b64url_encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let body = b64url_encode(claims.as_bytes());
    let sig = hmac_sha256(secret, format!("{head}.{body}").as_bytes());
    format!("{head}.{body}.{}", b64url_encode(&sig))
}

/// How many times `needle` appears in `hay`.
fn count(hay: &str, needle: &str) -> usize {
    hay.matches(needle).count()
}

#[test]
fn two_users_see_and_change_only_their_own_rows() {
    let n = start();
    let (alice, bob) = (n.token(r#"{"sub":"alice"}"#), n.token(r#"{"sub":"bob"}"#));
    // A put that leaves `owner` out gets its token's subject.
    assert_eq!(
        n.call(Some(&alice), "POST", "/notes", r#"{"title":"a1"}"#)
            .0,
        201
    );
    assert_eq!(
        n.call(Some(&alice), "POST", "/notes", r#"{"title":"a2"}"#)
            .0,
        201
    );
    assert_eq!(
        n.call(Some(&bob), "POST", "/notes", r#"{"title":"b1"}"#).0,
        201
    );

    let (status, rows) = n.call(Some(&alice), "GET", "/notes", "");
    assert_eq!(status, 200);
    assert_eq!(
        (count(&rows, "\"alice\""), count(&rows, "\"bob\"")),
        (2, 0),
        "{rows}"
    );
    let (_, rows) = n.call(Some(&bob), "GET", "/notes", "");
    assert_eq!(
        (count(&rows, "\"alice\""), count(&rows, "\"bob\"")),
        (0, 1),
        "{rows}"
    );
    let (_, rows) = n.call(Some(ROOT), "GET", "/notes", "");
    assert_eq!(
        (count(&rows, "\"alice\""), count(&rows, "\"bob\"")),
        (2, 1),
        "{rows}"
    );

    // Raw FenecQL is held to the same rows, whatever it asks for.
    let (_, body) = n.query(&alice, "get notes count");
    assert!(body.contains("\"count\":2"), "{body}");
    let (_, body) = n.query(&alice, r#"get notes where owner = "bob""#);
    assert_eq!(count(&body, "\"bob\""), 0, "{body}");
    let (_, body) = n.query(&alice, r#"explain get notes where owner = "bob""#);
    assert!(!body.is_empty());

    // A set or del reaches only the token's own rows.
    let (status, body) = n.call(Some(&alice), "PATCH", "/notes/all", r#"{"title":"mine"}"#);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"updated\":2"), "{body}");
    let (_, rows) = n.call(Some(ROOT), "GET", "/notes?owner=eq.bob", "");
    assert!(rows.contains("\"b1\""), "{rows}");
    let (_, body) = n.query(&bob, "del notes");
    assert!(body.contains("\"affected\":1"), "{body}");
    let (_, rows) = n.call(Some(ROOT), "GET", "/notes", "");
    assert_eq!(
        (count(&rows, "\"alice\""), count(&rows, "\"bob\"")),
        (2, 0),
        "{rows}"
    );
}

#[test]
fn a_write_outside_the_rules_is_refused() {
    let n = start();
    let alice = n.token(r#"{"sub":"alice"}"#);
    let bob = n.token(r#"{"sub":"bob"}"#);
    assert_eq!(
        n.call(Some(&bob), "POST", "/notes", r#"{"title":"b1"}"#).0,
        201
    );
    assert_eq!(
        n.call(Some(&alice), "POST", "/notes", r#"{"title":"a1"}"#)
            .0,
        201
    );

    // Writing someone else's row: as its owner, over its id, or by moving
    // one's own over.
    let (status, body) = n.call(
        Some(&alice),
        "POST",
        "/notes",
        r#"{"owner":"bob","title":"spoof"}"#,
    );
    assert_eq!(status, 403, "{body}");
    assert_eq!(
        n.query(&alice, r#"put notes {id: 1, title: "mine now"}"#).0,
        403
    );
    assert_eq!(n.query(&alice, r#"set notes {owner: "bob"}"#).0, 403);
    let (_, rows) = n.call(Some(ROOT), "GET", "/notes?owner=eq.bob", "");
    assert_eq!(count(&rows, "\"title\""), 1, "{rows}");
    assert!(
        !rows.contains("spoof") && !rows.contains("mine now"),
        "{rows}"
    );

    // No schema, no maintenance.
    for sql in [
        "create collection mine (x int)",
        "drop collection notes",
        "create index on notes (title) @sorted",
        "compact",
    ] {
        assert_eq!(n.query(&alice, sql).0, 403, "{sql}");
    }

    // Read but not write, for all but one role.
    assert_eq!(n.call(Some(&alice), "GET", "/board", "").0, 200);
    assert_eq!(
        n.call(Some(&alice), "POST", "/board", r#"{"msg":"hi"}"#).0,
        403
    );
    let moderator = n.token(r#"{"sub":"m","role":"moderator"}"#);
    assert_eq!(
        n.call(Some(&moderator), "POST", "/board", r#"{"msg":"hi"}"#)
            .0,
        201
    );

    // A collection no rule lets it read does not exist for the token.
    assert_eq!(n.call(Some(&alice), "GET", "/secrets", "").0, 404);
    assert_eq!(n.query(&alice, "get secrets").0, 404);
    assert_eq!(n.query(&alice, "get notes lookup secrets on x = id").0, 404);
    let (_, list) = n.call(Some(&alice), "GET", "/collections", "");
    assert!(
        list.contains("notes") && !list.contains("secrets"),
        "{list}"
    );
    let (_, list) = n.query(&alice, "collections");
    assert!(!list.contains("secrets"), "{list}");
}

#[test]
fn tokens_that_do_not_verify_are_refused() {
    let n = start();
    let expired = n.token(r#"{"sub":"alice","exp":1000}"#);
    let (status, body) = n.call(Some(&expired), "GET", "/notes", "");
    assert_eq!(status, 401);
    assert!(body.contains("expired"), "{body}");
    let other = Access::new(&[9u8; 40], POLICY).unwrap();
    let forged = other.mint(r#"{"sub":"alice"}"#).unwrap();
    assert_eq!(n.call(Some(&forged), "GET", "/notes", "").0, 401);
    // Signed with the server's own secret but naming no `exp`: good for
    // ever, so refused (`--jwt-require-exp`).
    let forever = signed(SECRET, r#"{"sub":"alice"}"#);
    let (status, body) = n.call(Some(&forever), "GET", "/notes", "");
    assert_eq!(status, 401, "{body}");
    assert!(body.contains("no exp"), "{body}");
    assert_eq!(n.call(None, "GET", "/notes", "").0, 401);
    assert_eq!(n.call(Some("guess"), "GET", "/notes", "").0, 401);
    // The server's own token is everything.
    assert_eq!(n.call(Some(ROOT), "GET", "/secrets", "").0, 200);
}

/// A subscription to the notes, as alice: the rows other users write are
/// neither sent nor mentioned -- not even as deletions.
#[test]
fn a_subscription_hears_nothing_of_other_users_rows() {
    let n = start();
    let alice = n.token(r#"{"sub":"alice"}"#);
    let bob = n.token(r#"{"sub":"bob"}"#);
    assert_eq!(
        n.call(Some(&alice), "POST", "/notes", r#"{"title":"a1"}"#)
            .0,
        201
    );

    let mut s = TcpStream::connect(("127.0.0.1", n.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    write!(
        s,
        "GET /notes/changes HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {alice}\r\n\r\n"
    )
    .unwrap();
    let mut heard = String::new();
    listen(
        &mut s,
        &mut heard,
        &|h| h.contains("event: seed"),
        Duration::from_secs(3),
    );
    assert!(heard.contains("\"a1\""), "{heard}");

    // Bob writes three notes and deletes one; then alice writes hers.
    for t in ["b1", "b2", "b3"] {
        let body = format!(r#"{{"title":"{t}"}}"#);
        assert_eq!(n.call(Some(&bob), "POST", "/notes", &body).0, 201);
    }
    assert_eq!(n.query(&bob, r#"del notes where title = "b2""#).0, 200);
    assert_eq!(
        n.call(Some(&alice), "POST", "/notes", r#"{"title":"a2"}"#)
            .0,
        201
    );
    listen(
        &mut s,
        &mut heard,
        &|h| h.contains("\"a2\""),
        Duration::from_secs(3),
    );
    // A moment more, for anything that would have followed.
    listen(&mut s, &mut heard, &|_| false, Duration::from_millis(300));

    assert!(heard.contains("\"a2\""), "{heard}");
    // Bob's rows had ids 2, 3 and 4: no event names them, as a row or as a
    // deletion.
    let events: Vec<&str> = heard.split("event: ").skip(1).collect();
    for e in &events {
        assert!(!e.contains("\"bob\"") && !e.contains("\"b2\""), "{e}");
        if let Some(dels) = e.split("\"dels\":[").nth(1) {
            let dels = dels.split(']').next().unwrap();
            assert!(dels.is_empty(), "a deletion of someone else's row: {e}");
        }
    }
}

/// A rule over a field in a collation checks a write in that order, as it
/// filters a read: `çay` is before `d` in Turkish and after it in bytes, so
/// a token held to `name < "d"` may write `çay`, which it is shown, and not
/// `zeytin`, which it is not.
#[test]
fn a_rule_over_a_collated_field_checks_writes_in_its_order() {
    let n = start_with(
        "people  read,write  where name < \"d\"\n",
        &["create collection people (name text collate tr)"],
    );
    let t = n.token(r#"{"sub":"a"}"#);
    let (status, body) = n.query(&t, r#"put people {name: "çay"}"#);
    assert_eq!(status, 200, "{body}");
    assert_eq!(n.query(&t, r#"put people {name: "zeytin"}"#).0, 403);
    let (_, rows) = n.query(&t, "get people select name");
    assert!(rows.contains("çay"), "{rows}");
}

/// An inner `get` reads its collection as any `get` does: held to the
/// token's rules, so the list it makes holds only rows the token may read,
/// and a token learns nothing of the others through what the outer query
/// finds by it -- not in a read, a count or a write.
#[test]
fn an_inner_get_reads_only_what_the_token_may() {
    let n = start_with(
        &format!("{POLICY}board  read,write  for poster\n"),
        &[
            "create collection notes (owner text @hash, title text)",
            "create collection board (msg text)",
            "create collection secrets (x int)",
            "put secrets {x: 42}",
            r#"put notes [{owner: "alice", title: "a1"}, {owner: "alice", title: "a2"},
                         {owner: "bob", title: "b1"}, {owner: "bob", title: "b2"}]"#,
            r#"put board [{msg: "a1"}, {msg: "b1"}, {msg: "b2"}, {msg: "42"}]"#,
        ],
    );
    let alice = n.token(r#"{"sub":"alice"}"#);
    let poster = n.token(r#"{"sub":"alice","role":"poster"}"#);

    // The board is everyone's, the notes' titles each owner's: the list is
    // alice's titles, whatever the inner `where` asks for.
    let (status, body) = n.query(&alice, "get board where msg in (get notes select title)");
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        (
            count(&body, "\"a1\""),
            count(&body, "\"b1\""),
            count(&body, "\"b2\"")
        ),
        (1, 0, 0),
        "{body}"
    );
    for sql in [
        r#"get board where msg in (get notes select title where owner = "bob") count"#,
        r#"get board where msg in (get notes select title where owner = "bob" or owner = "alice") and msg ~ "b" count"#,
        "get board where not msg in (get notes select title) and msg ~ \"b\" count",
    ] {
        let (status, body) = n.query(&alice, sql);
        assert_eq!(status, 200, "{sql}: {body}");
        let want = if sql.contains("not msg") { 2 } else { 0 };
        assert!(body.contains(&format!("\"count\":{want}")), "{sql}: {body}");
    }
    // Unscoped, the same text finds bob's.
    let (_, body) = n.query(
        ROOT,
        r#"get board where msg in (get notes select title where owner = "bob") count"#,
    );
    assert!(body.contains("\"count\":2"), "{body}");

    // Nested, each level held to the rules.
    let (_, body) = n.query(
        &alice,
        r#"get board where msg in (get notes select title where owner in
             (get notes select owner where title = "b1")) count"#,
    );
    assert!(body.contains("\"count\":0"), "{body}");

    // A collection the token may not read is not there for it, inside too.
    let (status, body) = n.query(&alice, "get board where msg in (get secrets select x)");
    assert_eq!(status, 404, "{body}");

    // A write by an inner `get` reaches only what the list the token may
    // make holds: the poster deletes nothing by bob's titles.
    let (status, body) = n.query(
        &poster,
        r#"del board where msg in (get notes select title where owner = "bob")"#,
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"affected\":0"), "{body}");
    let (_, body) = n.query(ROOT, "get board count");
    assert!(body.contains("\"count\":4"), "{body}");

    // A rule takes none: tested against a document on its own, no query
    // would answer it.
    let Err(e) = Access::new(SECRET, "notes read where owner in (get board select msg)") else {
        panic!("a rule holding `in (get ...)` was taken");
    };
    assert!(e.contains("in (get ...)"), "{e}");
}

/// A `@unique` clash told a scoped token that another user's row holds the
/// value, naming the row's id and echoing the value: alice, who may not read
/// bob's profile, learned that it exists and where by trying his email.
/// A scoped token is told the field alone, by every route; the server's own
/// token is told the row and the value as before.
#[test]
fn a_unique_clash_tells_a_scoped_token_nothing_of_the_other_row() {
    let n = start_with(
        "profiles  read,write  where owner = $jwt.sub\n",
        &[
            "create collection profiles (owner text @hash, email text @unique)",
            r#"put profiles {owner: "bob", email: "bob@x.io"}"#,
        ],
    );
    let alice = n.token(r#"{"sub":"alice"}"#);
    let tries = [
        n.query(&alice, r#"insert profiles {email: "bob@x.io"}"#),
        n.query(&alice, r#"put profiles {email: "bob@x.io"}"#),
        n.call(Some(&alice), "POST", "/profiles", r#"{"email":"bob@x.io"}"#),
        n.call(
            Some(&alice),
            "POST",
            "/batch",
            r#"{"query":"put profiles {email: \"bob@x.io\"}"}"#,
        ),
    ];
    for (status, body) in tries {
        assert_eq!(status, 409, "{body}");
        assert!(body.contains("`profiles.email` is unique"), "{body}");
        assert!(!body.contains("bob"), "the value was told: {body}");
        assert!(!body.contains("document"), "the row was told: {body}");
    }
    // Her own row, then a `set` of it onto bob's value: the same.
    assert_eq!(
        n.query(&alice, r#"put profiles {email: "alice@x.io"}"#).0,
        200
    );
    let (status, body) = n.query(&alice, r#"set profiles {email: "bob@x.io"}"#);
    assert_eq!(status, 409, "{body}");
    assert!(
        !body.contains("bob") && !body.contains("document"),
        "{body}"
    );

    // The server's own token is told which row and what value.
    let (status, body) = n.query(ROOT, r#"insert profiles {email: "bob@x.io"}"#);
    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains("document 1") && body.contains("bob@x.io"),
        "{body}"
    );
}

const LEDGER: &str = "\
journal  read,write  where owner = $jwt.sub
journal  append-only
events   insert      where user = $jwt.sub
";

/// A `/batch` line.
fn line(query: &str) -> String {
    let mut out = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut out, query);
    out.push('}');
    out
}

/// An `append-only` journal: a scoped token with `write` inserts into it,
/// and changes nothing in it by any route -- `/query`, REST, `/batch`, a
/// `require` -- while the server's own token still does. An `insert`
/// grant alone writes a collection the token cannot read.
#[test]
fn an_append_only_journal_takes_inserts_and_nothing_else() {
    let n = start_with(
        LEDGER,
        &[
            "create collection journal (owner text @hash, amount int)",
            "create collection events (user text, name text)",
        ],
    );
    let alice = n.token(r#"{"sub":"alice"}"#);
    let rows = |sql: &str| n.query(ROOT, sql).1;

    assert_eq!(
        n.call(Some(&alice), "POST", "/journal", r#"{"amount":10}"#)
            .0,
        201
    );
    assert_eq!(
        n.query(&alice, "insert journal {amount: 20} require 1").0,
        200
    );
    assert_eq!(n.query(&alice, "put journal {amount: 30}").0, 200);
    let batch = [
        line("insert journal {amount: 40}"),
        line("insert journal {amount: 50}"),
    ];
    let (status, body) = n.call(Some(&alice), "POST", "/batch", &batch.join("\n"));
    assert_eq!(status, 200, "{body}");
    let before = rows("get journal select amount order id");
    assert_eq!(count(&before, "\"amount\""), 5, "{before}");

    for sql in [
        "set journal {amount: 1}",
        "set journal {amount: 1} where id = 1 require 1",
        "del journal",
        "del journal where id = 1 require 1",
        "put journal {id: 1, amount: 1}",
    ] {
        let (status, body) = n.query(&alice, sql);
        assert_eq!(status, 403, "{sql}: {body}");
    }
    let (status, body) = n.query(&alice, "set journal {amount: 1}");
    assert!(body.contains("append-only"), "{status} {body}");
    for (method, target, body) in [
        ("PATCH", "/journal?id=eq.1", r#"{"amount":1}"#),
        ("PATCH", "/journal/all", r#"{"amount":1}"#),
        ("DELETE", "/journal?id=eq.1", ""),
        ("DELETE", "/journal/all", ""),
    ] {
        let (status, answer) = n.call(Some(&alice), method, target, body);
        assert_eq!(status, 403, "{method} {target}: {answer}");
    }
    // A batch holding one is refused whole: its insert does not land.
    let batch = [
        line("insert journal {amount: 60}"),
        line("del journal where id = 1"),
    ];
    let (status, body) = n.call(Some(&alice), "POST", "/batch", &batch.join("\n"));
    assert_eq!(status, 403, "{body}");
    assert_eq!(rows("get journal select amount order id"), before);

    // The server's own token is not held to it.
    assert_eq!(
        n.query(ROOT, "set journal {amount: 11} where id = 1").0,
        200
    );

    // Insert alone: written, never read, and only as the token's own.
    assert_eq!(
        n.call(Some(&alice), "POST", "/events", r#"{"name":"view"}"#)
            .0,
        201
    );
    assert_eq!(n.query(&alice, "insert events {name: 'buy'}").0, 200);
    assert_eq!(n.call(Some(&alice), "GET", "/events", "").0, 403);
    assert_eq!(n.query(&alice, "get events count").0, 403);
    assert_eq!(
        n.query(&alice, "insert events {user: 'bob', name: 'x'}").0,
        403
    );
    assert_eq!(n.query(&alice, "set events {name: 'x'}").0, 403);
    let (_, list) = n.call(Some(&alice), "GET", "/collections", "");
    assert!(!list.contains("events"), "{list}");
    let all = rows("get events select user, name order id");
    assert_eq!(count(&all, "\"alice\""), 2, "{all}");
}

const ROOMS: &str = "\
messages  read,insert  where room in $jwt.rooms
";

/// Room membership as a list claim: alice, in rooms r1 and r2, reads,
/// writes and hears only those -- a post into r3 is refused -- and a token
/// listing more rooms than a policy takes is refused.
#[test]
fn a_list_claim_scopes_reads_writes_and_subscriptions() {
    let n = start_with(
        ROOMS,
        &["create collection messages (room text @hash, body text)"],
    );
    let alice = n.token(r#"{"sub":"alice","rooms":["r1","r2"]}"#);
    let carol = n.token(r#"{"sub":"carol","rooms":["r3"]}"#);
    let mut s = TcpStream::connect(("127.0.0.1", n.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    write!(
        s,
        "GET /messages/changes HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {alice}\r\n\r\n"
    )
    .unwrap();
    let mut heard = String::new();
    listen(
        &mut s,
        &mut heard,
        &|h| h.contains("event: seed"),
        Duration::from_secs(3),
    );

    for (room, who) in [("r1", &alice), ("r2", &alice), ("r3", &carol)] {
        let body = format!(r#"{{"room":"{room}","body":"in {room}"}}"#);
        let (status, answer) = n.call(Some(who), "POST", "/messages", &body);
        assert_eq!(status, 201, "{answer}");
    }
    let (status, body) = n.call(
        Some(&alice),
        "POST",
        "/messages",
        r#"{"room":"r3","body":"spoof"}"#,
    );
    assert_eq!(status, 403, "{body}");
    assert_eq!(
        n.query(&alice, "insert messages {room: 'r3', body: 'spoof'}")
            .0,
        403
    );
    // A post naming no room is in none of hers.
    assert_eq!(n.query(&alice, "insert messages {body: 'nowhere'}").0, 403);

    let (_, mine) = n.call(Some(&alice), "GET", "/messages", "");
    assert!(
        mine.contains("in r1") && mine.contains("in r2") && !mine.contains("in r3"),
        "{mine}"
    );
    let (_, counted) = n.query(&alice, "get messages count");
    assert!(counted.contains(":2"), "{counted}");

    listen(
        &mut s,
        &mut heard,
        &|h| h.contains("in r2"),
        Duration::from_secs(3),
    );
    // A moment more, for anything that would have followed.
    listen(&mut s, &mut heard, &|_| false, Duration::from_millis(300));
    assert!(
        heard.contains("in r1") && heard.contains("in r2"),
        "{heard}"
    );
    assert!(
        !heard.contains("in r3") && !heard.contains("spoof"),
        "{heard}"
    );

    let rooms: Vec<String> = (0..=fenec_http::access::MAX_CLAIM_VALUES)
        .map(|i| format!("\"r{i}\""))
        .collect();
    let many = n.token(&format!(r#"{{"sub":"m","rooms":[{}]}}"#, rooms.join(",")));
    let (status, body) = n.call(Some(&many), "GET", "/messages", "");
    assert_eq!(status, 401, "{body}");
    assert!(body.contains("1 000 values"), "{body}");
}

/// The `_score` of the first row an answer holds.
fn first_score(body: &str) -> f64 {
    let at = body.find("\"_score\":").expect(body) + "\"_score\":".len();
    let digits: String = body[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | 'e' | 'E'))
        .collect();
    digits.parse().unwrap()
}

/// BM25 over the rows a token may read: alice's own memo scores what it
/// scored before bob wrote 200 private memos holding the same word, where
/// over the collection it went 9.87 -> 4.79 and told her how many of his
/// held it. The server's own token still scores over the collection.
#[test]
fn a_scoped_match_scores_nothing_of_rows_the_token_cannot_read() {
    let n = start_with(
        "memos  read,write  where owner = $jwt.sub\n",
        &["create collection memos (owner text @hash, body text @text)"],
    );
    let (alice, bob) = (n.token(r#"{"sub":"alice"}"#), n.token(r#"{"sub":"bob"}"#));
    for body in [
        "the initech memo",
        "lunch on friday",
        "a plan for the quarter",
    ] {
        let doc = format!(r#"{{"body":"{body}"}}"#);
        assert_eq!(n.call(Some(&alice), "POST", "/memos", &doc).0, 201);
    }
    let ask = r#"get memos match body "initech" limit 5"#;
    let (status, before) = n.query(&alice, ask);
    assert_eq!(status, 200, "{before}");
    let mine = r#"get memos where owner = "alice" match body "initech" limit 5"#;
    let (_, root_before) = n.query(ROOT, mine);

    let memos: Vec<String> = (0..200)
        .map(|i| format!(r#"{{"body":"initech deal {i}, private"}}"#))
        .collect();
    let (status, body) = n.call(
        Some(&bob),
        "POST",
        "/memos",
        &format!("[{}]", memos.join(",")),
    );
    assert_eq!(status, 201, "{body}");

    let (_, after) = n.query(&alice, ask);
    assert_eq!(count(&after, "\"_score\""), 1, "{after}");
    assert_eq!(
        first_score(&after),
        first_score(&before),
        "{before} {after}"
    );
    // Over the collection the same memo's score fell: what alice no longer
    // learns.
    let (_, root_after) = n.query(ROOT, mine);
    assert!(
        first_score(&root_after) < first_score(&root_before),
        "{root_before} {root_after}"
    );
}
