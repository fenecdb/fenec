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
    // The wait after a refusal is the process's, counted by address and
    // doubled to 5 s: every test here asks from 127.0.0.1, so the refusals
    // of the tests running at once held one's 401 past its read timeout.
    // tests/audit.rs and fenec-shard's tests/refusals.rs hold the wait.
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

/// A disjunctive facet leaves the query's own conditions on its field out,
/// and never the token's: `facet owner disjunctive` split after the rules
/// were ANDed in would have counted every user's rows. By `/query` and by
/// REST's `facet=`.
#[test]
fn a_disjunctive_facet_keeps_the_token_rules() {
    let n = start();
    for (owner, title) in [("alice", "a"), ("alice", "b"), ("bob", "c"), ("carol", "d")] {
        let sql = format!(r#"put notes {{owner: "{owner}", title: "{title}"}}"#);
        assert_eq!(n.query(ROOT, &sql).0, 200);
    }
    let alice = n.token(r#"{"sub":"alice"}"#);
    let (status, body) = n.query(
        &alice,
        r#"get notes where owner = "alice" and title = "a" count facet owner disjunctive, title disjunctive"#,
    );
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(r#""owner":[{"value":"alice","count":1}]"#),
        "{body}"
    );
    assert!(
        body.contains(r#""title":[{"value":"a","count":1},{"value":"b","count":1}]"#),
        "{body}"
    );
    assert!(!body.contains("bob") && !body.contains("carol"), "{body}");
    let (status, body) = n.call(
        Some(&alice),
        "GET",
        "/notes?owner=eq.alice&facet=owner%20disjunctive&limit=0",
        "",
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("alice"), "{body}");
    assert!(!body.contains("bob") && !body.contains("carol"), "{body}");
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

const REAPER: &str = "\
holds  read,insert,delete,expired              for app
holds  read                       where owner = $jwt.sub
";

/// `expired()` reads the rows past their `@ttl`, which every other read
/// leaves out: granted by `expired` alone -- never by `read`, `write` or
/// `*` -- so a reaper's token gives back a lapsed hold and a user's never
/// sees one again. A rule's own filter takes none.
#[test]
fn the_rows_past_their_time_are_the_expired_grants() {
    let n = start_with(
        REAPER,
        &[
            "create collection holds (owner text @hash, amount int, until timestamp @ttl(1ms))",
            "put holds [{owner: \"alice\", amount: 5, until: 1}, \
             {owner: \"alice\", amount: 7, until: \"2999-01-01\"}]",
        ],
    );
    let alice = n.token(r#"{"sub":"alice"}"#);
    let app = n.token(r#"{"sub":"ledger","role":"app"}"#);
    let (status, body) = n.query(&alice, "get holds where expired()");
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("`expired` grant"), "{body}");
    assert_eq!(n.query(&alice, "del holds where expired()").0, 403);
    let (status, body) = n.query(&alice, "get holds select amount");
    assert_eq!((status, count(&body, "\"amount\"")), (200, 1), "{body}");
    // Through an inner `get` too.
    let (status, body) = n.query(
        &alice,
        "get holds where owner in (get holds select owner where expired())",
    );
    assert_eq!(status, 403, "{body}");

    let (status, body) = n.query(&app, "get holds select amount where expired()");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("5") && !body.contains("7"), "{body}");
    let reap = line("del holds where expired() and amount = 5 require 1");
    let (status, body) = n.call(Some(&app), "POST", "/batch", &reap);
    assert_eq!(status, 200, "{body}");
    let (status, body) = n.call(Some(&app), "POST", "/batch", &reap);
    assert_eq!(status, 412, "given back once: {body}");

    // `*` and `write` grant no `expired`; a rule's filter takes none.
    let wide = start_with(
        "*  read,write\n",
        &["create collection holds (until timestamp @ttl(1ms))"],
    );
    let t = wide.token(r#"{"sub":"x"}"#);
    assert_eq!(wide.query(&t, "get holds where expired()").0, 403);
    assert!(Access::new(SECRET, "holds read where expired()").is_err());
    assert!(Access::new(SECRET, "holds read,expired\n").is_ok());
}

const BALANCES: &str = "\
accounts  read                                     for app
accounts  insert   where balance = 0               for app
accounts  update(balance, held, status)            for app
notes     read,insert,update,delete  where owner = $jwt.sub
notes     update(title)
";

fn balance(n: &Node) -> String {
    n.query(ROOT, "get accounts select kind, balance, held, status")
        .1
}

/// An `update(...)` grant names the fields a scoped write may change: the
/// app's token moves balances and changes nothing else of an account, by
/// any route -- `set`, REST `PATCH` and `PUT`, `/batch` -- where `update`
/// on `accounts` reached every field and `set accounts {balance: ...}`
/// could make money. A write is judged by the fields it changes: one
/// naming another field with the value it holds changes nothing there.
#[test]
fn an_update_grant_names_the_fields_it_may_change() {
    let n = start_with(
        BALANCES,
        &[
            "create collection accounts (ext text @unique, kind text, balance int, held int, status text)",
            "put accounts {ext: \"a\", kind: \"customer\", balance: 100, held: 0, status: \"open\"}",
            "create collection notes (owner text @hash, title text, body text)",
            "put notes [{owner: \"alice\", title: \"a\", body: \"x\"}, {owner: \"bob\", title: \"b\", body: \"y\"}]",
        ],
    );
    let app = n.token(r#"{"sub":"ledger","role":"app"}"#);
    let start = balance(&n);
    for sql in [
        "set accounts {kind: \"world\"} where ext = \"a\"",
        "set accounts {balance: balance + 1, kind: \"world\"} where ext = \"a\"",
        "set accounts {ext: \"b\"} where ext = \"a\"",
        "set accounts {kind: \"world\"} where ext = \"a\" require 1",
        "put accounts {id: 1, ext: \"a\", kind: \"world\", balance: 100, held: 0, status: \"open\"}",
    ] {
        let (status, body) = n.query(&app, sql);
        assert_eq!(status, 403, "{sql}: {body}");
    }
    let (_, body) = n.query(&app, "set accounts {kind: \"world\"} where ext = \"a\"");
    assert!(
        body.contains("may not change `kind` in `accounts`"),
        "{body}"
    );
    for (method, target, body) in [
        ("PATCH", "/accounts?ext=eq.a", r#"{"kind":"world"}"#),
        (
            "PUT",
            "/accounts?ext=eq.a",
            r#"{"kind":"world","balance":5}"#,
        ),
        ("PATCH", "/accounts/all", r#"{"status":"x","kind":"world"}"#),
    ] {
        let (status, answer) = n.call(Some(&app), method, target, body);
        assert_eq!(status, 403, "{method} {target}: {answer}");
    }
    // A batch holding one is refused whole: its balance move does not land.
    let batch = [
        line("set accounts {balance: balance + 500000} where ext = \"a\""),
        line("set accounts {kind: \"world\"} where ext = \"a\""),
    ];
    let (status, body) = n.call(Some(&app), "POST", "/batch", &batch.join("\n"));
    assert_eq!(status, 403, "{body}");
    assert_eq!(balance(&n), start);

    // The fields it names it changes, by every route.
    let (status, body) = n.query(
        &app,
        "set accounts {balance: balance - 10, held: held + 10} where ext = \"a\" require 1",
    );
    assert_eq!(status, 200, "{body}");
    let (status, body) = n.call(
        Some(&app),
        "PATCH",
        "/accounts?ext=eq.a",
        r#"{"status":"frozen"}"#,
    );
    assert_eq!(status, 200, "{body}");
    // Another field written with the value it holds changes nothing there.
    let (status, body) = n.query(
        &app,
        "set accounts {kind: kind, status: \"open\"} where ext = \"a\"",
    );
    assert_eq!(status, 200, "{body}");
    let (status, body) = n.call(
        Some(&app),
        "POST",
        "/batch",
        &line("set accounts {held: 0} where ext = \"a\" require 1"),
    );
    assert_eq!(status, 200, "{body}");
    let now = balance(&n);
    assert!(now.contains("\"customer\"") && now.contains("90"), "{now}");
    // The server's own token is not held to it.
    assert_eq!(
        n.query(ROOT, "set accounts {kind: \"world\"} where ext = \"a\"")
            .0,
        200
    );

    // Rules widen each other, each with its own fields and rows: alice
    // changes every field of her notes and the title of anyone's, the
    // owner of none but hers -- and gives none of hers away.
    let alice = n.token(r#"{"sub":"alice"}"#);
    let rows = |sql: &str| n.query(ROOT, sql).1;
    assert_eq!(
        n.query(&alice, "set notes {body: \"z\"} where owner = \"alice\"")
            .0,
        200
    );
    assert_eq!(n.query(&alice, "set notes {title: \"t\"}").0, 200);
    assert_eq!(count(&rows("get notes where title = \"t\" count"), "2"), 1);
    for sql in [
        "set notes {body: \"z\"} where owner = \"bob\"",
        "set notes {title: \"u\", body: \"z\"} where owner = \"bob\"",
        "set notes {owner: \"bob\"} where owner = \"alice\"",
    ] {
        let (status, body) = n.query(&alice, sql);
        assert_eq!(status, 403, "{sql}: {body}");
    }
    let all = rows("get notes select owner, body order id");
    assert!(all.contains("\"y\"") && all.contains("\"alice\""), "{all}");

    // The grammar: fields named, `id` never, a list closed.
    assert!(Access::new(SECRET, "a update(x, y_2) where x = 1 for r").is_ok());
    for bad in [
        "a update(",
        "a update(x y)",
        "a update()",
        "a update(id)",
        "a update((x))",
        "a update(x)\na append-only",
    ] {
        assert!(Access::new(SECRET, bad).is_err(), "{bad}");
    }
}

/// A subscription outlived its token: a stream opened with a token whose
/// `exp` was two seconds away still delivered a change written four seconds
/// later, while the same token's `get` was 401. It is ended at the `exp`
/// now, with an `error` event a client takes as a refusal (401).
#[test]
fn a_subscription_ends_when_its_token_expires() {
    let n = start();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let alice = n.token(&format!(r#"{{"sub":"alice","exp":{}}}"#, now + 2));
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
    assert!(heard.contains("event: seed"), "{heard}");
    // The `exp` passes: the stream says why and closes.
    listen(
        &mut s,
        &mut heard,
        &|h| h.contains("event: error"),
        Duration::from_secs(6),
    );
    assert!(
        heard.contains(
            r#"event: error
data: {"error":"the token has expired","status":401}"#
        ),
        "{heard}"
    );
    let ended = heard.len();
    // A write after it reaches no one: the connection is closed.
    let root = n.call(
        Some(ROOT),
        "POST",
        "/notes",
        r#"{"owner":"alice","title":"after"}"#,
    );
    assert_eq!(root.0, 201, "{}", root.1);
    let mut buf = [0u8; 64];
    let closed = loop {
        match s.read(&mut buf) {
            Ok(0) => break true,
            Ok(k) => heard.push_str(&String::from_utf8_lossy(&buf[..k])),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                break false
            }
            Err(_) => break true,
        }
    };
    assert!(closed, "the stream stayed open: {heard}");
    assert!(!heard[ended..].contains("after"), "{heard}");
    // And the token's own request is refused, as the stream was ended.
    assert_eq!(n.call(Some(&alice), "GET", "/notes", "").0, 401);
}

/// `FenecHttp.live` opens a stream of a shape that holds nothing
/// (`select=id&where=false`) and runs its query again at each change. Under
/// a scoped token that shape, ANDed with the token's filter, matched no row
/// and the stream heard nothing, ever. It hears of the writes to rows the
/// token may read now -- and of nothing else: not when another user writes,
/// nor which ids.
#[test]
fn a_scoped_stream_of_no_rows_hears_the_writes_to_rows_it_may_read() {
    let n = start();
    let alice = n.token(r#"{"sub":"alice"}"#);
    let bob = n.token(r#"{"sub":"bob"}"#);
    let mut s = TcpStream::connect(("127.0.0.1", n.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    write!(
        s,
        "GET /notes/changes?select=id&where=false HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {alice}\r\n\r\n"
    )
    .unwrap();
    let mut heard = String::new();
    listen(
        &mut s,
        &mut heard,
        &|h| h.contains("event: seed"),
        Duration::from_secs(3),
    );
    assert!(heard.contains(r#""rows":[]"#), "{heard}");

    // Bob writes and deletes: alice hears nothing of it.
    assert_eq!(
        n.call(Some(&bob), "POST", "/notes", r#"{"title":"b1"}"#).0,
        201
    );
    assert_eq!(n.query(&bob, r#"del notes where title = "b1""#).0, 200);
    let r = n.call(
        Some(ROOT),
        "POST",
        "/notes",
        r#"{"owner":"bob","title":"b2"}"#,
    );
    assert_eq!(r.0, 201, "{}", r.1);
    listen(&mut s, &mut heard, &|_| false, Duration::from_millis(300));
    assert!(!heard.contains("event: change"), "{heard}");

    // Her own write is a change; so is her row leaving her, by another hand.
    assert_eq!(
        n.call(Some(&alice), "POST", "/notes", r#"{"title":"a1"}"#)
            .0,
        201
    );
    listen(
        &mut s,
        &mut heard,
        &|h| count(h, "event: change") == 1,
        Duration::from_secs(3),
    );
    let r = n.query(ROOT, r#"set notes {owner: "bob"} where title = "a1""#);
    assert_eq!(r.0, 200, "{}", r.1);
    listen(
        &mut s,
        &mut heard,
        &|h| count(h, "event: change") == 2,
        Duration::from_secs(3),
    );
    // Bob's again, after: nothing more.
    assert_eq!(
        n.call(Some(&bob), "POST", "/notes", r#"{"title":"b3"}"#).0,
        201
    );
    listen(&mut s, &mut heard, &|_| false, Duration::from_millis(300));
    assert_eq!(count(&heard, "event: change"), 2, "{heard}");
    // No change names a row or an id.
    for e in heard.split("event: change").skip(1) {
        assert!(e.contains(r#""puts":[],"dels":[]"#), "{e}");
    }
}

/// A polled live query sends the tag of its last answer, and is answered
/// 304 while nothing it reads was written. A scoped token's tag was the
/// database's change counter: 304 against 200 told it when anyone wrote to
/// the collection, rows it may not read among them, and the tag how many
/// writes there had been. Its tag is its answer's own now.
#[test]
fn a_scoped_poll_learns_nothing_of_writes_it_may_not_read() {
    let n = start();
    let alice = n.token(r#"{"sub":"alice"}"#);
    let bob = n.token(r#"{"sub":"bob"}"#);
    assert_eq!(
        n.call(Some(&alice), "POST", "/notes", r#"{"title":"a1"}"#)
            .0,
        201
    );
    let ask = |token: &str, tag: &str| -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", n.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let body = r#"{"query":"get notes select title"}"#;
        write!(
            s,
            "POST /query HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\
             If-None-Match: {tag}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let status = out[9..12].parse().unwrap();
        let tag = out
            .lines()
            .find_map(|l| l.strip_prefix("ETag: "))
            .unwrap_or_default()
            .to_string();
        (status, tag)
    };
    let (status, tag) = ask(&alice, "\"0\"");
    assert_eq!(status, 200);
    assert!(
        tag.starts_with("\"c"),
        "a tag of the answer, not the counter: {tag}"
    );
    assert_eq!(ask(&alice, &tag), (304, tag.clone()));
    // Bob writes to the collection: the same answer, the same tag, 304.
    for t in ["b1", "b2", "b3"] {
        let body = format!(r#"{{"title":"{t}"}}"#);
        assert_eq!(n.call(Some(&bob), "POST", "/notes", &body).0, 201);
    }
    assert_eq!(ask(&alice, &tag), (304, tag.clone()));
    // Her own: another answer.
    assert_eq!(
        n.call(Some(&alice), "POST", "/notes", r#"{"title":"a2"}"#)
            .0,
        201
    );
    let (status, again) = ask(&alice, &tag);
    assert_eq!(status, 200);
    assert_ne!(again, tag);
    // The server's token keeps the counter's tag, answered unrun.
    let (status, root) = ask(ROOT, "\"0\"");
    assert_eq!(status, 200);
    assert!(!root.starts_with("\"c"), "{root}");
    assert_eq!(ask(ROOT, &root).0, 304);
}

const COUNTS: &str = "\
events   insert              where user = $jwt.sub
tallies  read,write          where owner = $jwt.sub
";

impl Node {
    fn query_with(&self, token: &str, sql: &str, params: &str) -> (u16, String) {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, sql);
        body.push_str(",\"params\":");
        body.push_str(params);
        body.push('}');
        self.call(Some(token), "POST", "/query", &body)
    }
}

/// A parameter's documents are each held to the token's rules, as written
/// ones are: its pinned field filled in, an `id` refused, another's value
/// refused. An upsert needs update as well as insert, and sets only a row
/// the token may update -- found by a value any row may hold.
#[test]
fn a_scoped_token_puts_a_parameter_s_documents_and_upserts_its_own_rows() {
    let n = start_with(
        COUNTS,
        &[
            "create collection events (user text, name text)",
            "create collection tallies (key text @unique, owner text, n int)",
            "put tallies {key: \"bob-k\", owner: \"bob\", n: 1}",
        ],
    );
    let alice = n.token(r#"{"sub":"alice"}"#);
    let rows = |sql: &str| n.query(ROOT, sql).1;
    // Insert alone, from a parameter: the user pinned in each.
    let (status, body) = n.query_with(
        &alice,
        "put events $1",
        r#"[[{"name": "view"}, {"name": "buy"}]]"#,
    );
    assert_eq!(status, 200, "{body}");
    let all = rows("get events select user order id");
    assert_eq!(count(&all, "\"alice\""), 2, "{all}");
    for docs in [
        r#"[[{"name": "x", "user": "bob"}]]"#,
        r#"[[{"name": "x", "id": 1}]]"#,
    ] {
        assert_eq!(n.query_with(&alice, "put events $1", docs).0, 403, "{docs}");
    }
    // An upsert into a collection the token only inserts into.
    let (status, _) = n.query_with(
        &alice,
        "put events $1 if absent else set {name: new.name}",
        r#"[[{"name": "y"}]]"#,
    );
    assert_eq!(status, 403);
    // Its own rows: made, then added to.
    let up = "put tallies $1 if absent else set {n: n + new.n}";
    for _ in 0..2 {
        let (status, body) = n.query_with(&alice, up, r#"[[{"key": "a-k", "n": 2}]]"#);
        assert_eq!(status, 200, "{body}");
    }
    let mine = rows("get tallies select key, owner, n where key = \"a-k\"");
    assert!(
        mine.contains("\"owner\":\"alice\"") && mine.contains("\"n\":4"),
        "{mine}"
    );
    // Bob's row, reached through its unique key: refused, and unchanged --
    // even setting the owner to alice's.
    for set in ["{n: n + new.n}", "{owner: \"alice\"}"] {
        let (status, body) = n.query_with(
            &alice,
            &format!("put tallies $1 if absent else set {set}"),
            r#"[[{"key": "bob-k", "n": 5}]]"#,
        );
        assert_eq!(status, 403, "{set}: {body}");
    }
    let theirs = rows("get tallies select owner, n where key = \"bob-k\"");
    assert!(
        theirs.contains("\"owner\":\"bob\"") && theirs.contains("\"n\":1"),
        "{theirs}"
    );
}

const QUEUES: &str = "\
jobs  read                               where queue = $jwt.queue and hidden = false
jobs  update(owner, run_at, attempts)    where queue = $jwt.queue          for worker
jobs  delete                             where queue = $jwt.queue          for worker
jobs  update                             where queue = $jwt.queue          for admin
";

/// A job queue's claim through a worker's token: it picks only among the
/// rows its rules let it update -- its own queue's -- and, answering them
/// (`returning`), among those it may read; a claim that changes a field the
/// grant does not name, or leaves a row its rules do not admit (`WITH
/// CHECK`), is refused whole, and nothing is claimed.
#[test]
fn a_worker_token_claims_through_its_rules() {
    let n = start_with(
        QUEUES,
        &[
            "create collection jobs (queue text, hidden bool, run_at timestamp @sorted, owner text, attempts int)",
            "put jobs [{queue: \"image\", hidden: false, run_at: 1, attempts: 0}, \
             {queue: \"mail\", hidden: true, run_at: 2, attempts: 0}, \
             {queue: \"mail\", hidden: false, run_at: 3, attempts: 0}, \
             {queue: \"mail\", hidden: false, run_at: 4, attempts: 0}]",
        ],
    );
    let mail = n.token(r#"{"sub":"w1","role":"worker","queue":"mail"}"#);
    let claim = "set jobs {owner: $1, run_at: now() + 60000, attempts: attempts + 1} \
                 where run_at <= now() order run_at limit 1 returning id, queue";
    // The oldest job is another queue's, the next one this worker may not
    // read: the claim answers the third.
    let (status, body) = n.query_with(&mail, claim, r#"["w1"]"#);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"[{"id":3,"queue":"mail"}]"#);
    // A field the grant does not name: refused, and the job left as it was.
    let (status, body) = n.query(
        &mail,
        "set jobs {owner: \"w1\", queue: \"image\"} where run_at <= now() order run_at \
         limit 1 returning id",
    );
    assert_eq!(status, 403, "{body}");
    // A row its rules would not admit after the write: refused as well.
    let admin = n.token(r#"{"sub":"a","role":"admin","queue":"mail"}"#);
    let (status, body) = n.query(
        &admin,
        "set jobs {queue: \"image\"} where run_at <= now() and hidden = false order run_at \
         limit 1 returning id",
    );
    assert_eq!(status, 403, "{body}");
    let (_, left) = n.query(ROOT, "get jobs select id where owner is null order id");
    assert_eq!(left, r#"[{"id":1},{"id":2},{"id":4}]"#);
    // The ack: its own job, and not another queue's.
    let ack = "del jobs where id = $1 and owner = $2 require 1";
    assert_eq!(n.query_with(&mail, ack, r#"[3, "w1"]"#).0, 200);
    let (status, body) = n.query_with(&mail, ack, r#"[1, null]"#);
    assert_eq!(status, 412, "{body}");
    // The image queue's worker takes the image job alone.
    let image = n.token(r#"{"sub":"w2","role":"worker","queue":"image"}"#);
    let (status, body) = n.query_with(
        &image,
        "set jobs {owner: $1} where run_at <= now() order run_at limit 10 returning id",
        r#"["w2"]"#,
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"[{"id":1}]"#);
}
