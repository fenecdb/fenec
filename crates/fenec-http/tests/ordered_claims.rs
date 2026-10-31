//! An ordered comparison against a claim holds the other side to the
//! claim's kind. Values of two kinds order by kind (`cmp_value`), every
//! number below every text, so `level <= $jwt.clearance` given the text
//! `"top"` read every row of an `int` level; held to the claim's kind, a
//! value of another kind is neither below the claim nor above it, under a
//! `not` too, a time field orders against a claim that reads as a time by
//! time, and an ordered index's range stays exact. A claim inside a call
//! or a sum, whose kind the rule cannot know, and a comparison used as a
//! value are refused at startup. Several are the review of #408's (r1, r2,
//! r3, r6, r8, r9, r10).

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

fn start(policy: &str, setup: &[&str]) -> Node {
    // The wait after a refusal is the process's (tests/access.rs).
    fenec_http::audit::set_delay(0);
    let access = Arc::new(Access::new(SECRET, policy).unwrap());
    let mut db = fenec_core::prelude::Database::new();
    for sql in setup {
        db.execute(&fenec_ql::parse_one(sql).unwrap())
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
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

    fn query(&self, token: &str, sql: &str) -> (u16, String) {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, sql);
        body.push('}');
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            s,
            "POST /query HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\
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

    /// The answer a token with `claims` is given, which must be a 200.
    fn rows(&self, claims: &str, sql: &str) -> String {
        let t = self.token(claims);
        let (status, body) = self.query(&t, sql);
        assert_eq!(status, 200, "{claims} {sql}: {body}");
        body
    }
}

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
    let files = "get files select name order name";
    assert_eq!(
        n.rows(r#"{"clearance":5}"#, files),
        r#"[{"name":"a"},{"name":"b"}]"#
    );
    assert_eq!(n.rows(r#"{"clearance":"top"}"#, files), "[]");
    assert_eq!(n.rows(r#"{"clearance":true}"#, files), "[]");
    let people = "get people select name order name";
    assert_eq!(
        n.rows(r#"{"from":"b"}"#, people),
        r#"[{"name":"bob"},{"name":"carol"}]"#
    );
    assert_eq!(n.rows(r#"{"from":5}"#, people), "[]");
    // A field of every kind, the claim on the left.
    let grades = "get grades select score order id";
    assert_eq!(n.rows(r#"{"at_least":5}"#, grades), r#"[{"score":8}]"#);
    assert_eq!(n.rows(r#"{"at_least":"B"}"#, grades), "[]");
    assert_eq!(n.rows(r#"{"at_least":"A"}"#, grades), r#"[{"score":"A"}]"#);
    let flags = "get flags select state order id";
    assert_eq!(n.rows(r#"{"flag":true}"#, flags), r#"[{"state":true}]"#);
    assert_eq!(
        n.rows(r#"{"flag":false}"#, flags),
        r#"[{"state":true},{"state":false}]"#
    );
}

/// A claim inside a call or a sum has a kind the rule cannot know until it
/// runs, and given a claim of another kind admitted every row: `lower(5)`
/// over a text field, `least(level, "top")` the level itself (r1). A
/// comparison with a claim used as a value is held nowhere. Each is
/// refused at startup, naming the claim.
#[test]
fn a_claim_inside_a_call_a_sum_or_a_value_is_refused_at_startup() {
    for (policy, says) in [
        (
            "files read where level <= lower($jwt.clearance)",
            "`$jwt.clearance` inside `lower(...)`",
        ),
        (
            "people read where name >= lower($jwt.from)",
            "`$jwt.from` inside `lower(...)`",
        ),
        (
            "boxes read where level <= coalesce($jwt.c, 0)",
            "`$jwt.c` inside `coalesce(...)`",
        ),
        (
            "crates read where least(level, $jwt.c) = level",
            "`$jwt.c` inside `least(...)`",
        ),
        (
            "crates read where greatest(level, $jwt.c) = level",
            "`$jwt.c` inside `greatest(...)`",
        ),
        (
            "notes read where owner = upper($jwt.sub)",
            "`$jwt.sub` inside `upper(...)`",
        ),
        (
            "files read where level <= $jwt.c + 1",
            "`$jwt.c` in arithmetic",
        ),
        (
            "files read where created >= $jwt.iat * 1000",
            "`$jwt.iat` in arithmetic",
        ),
        (
            "files read where (level > $jwt.c) = false",
            "`$jwt.c` is compared in order inside a value",
        ),
        (
            "files read where (level > $jwt.c) is null",
            "`$jwt.c` is compared in order inside a value",
        ),
        (
            "files read where case when level <= $jwt.c then true else false end",
            "`$jwt.c` inside `case(...)`",
        ),
        (
            "files read where id in (get other select x where lower($jwt.c) = y)",
            "`$jwt.c` inside `lower(...)`",
        ),
    ] {
        let why = Access::new(SECRET, policy)
            .err()
            .unwrap_or_else(|| panic!("{policy}"));
        assert!(why.contains(says), "{policy}: {why}");
    }
    // A bare claim, in order or not, under `not` too, and in an inner get.
    for policy in [
        "files read where level <= $jwt.c and not (level < $jwt.floor)",
        "notes read where owner = $jwt.sub or owner ~ $jwt.pattern",
        "rooms read where id in (get members select room where user = $jwt.sub and rank >= $jwt.rank)",
        "posts read where $jwt.rooms has room",
    ] {
        assert!(Access::new(SECRET, policy).is_ok(), "{policy}");
    }
}

/// A time field asked to be below or above a claim that reads as a time is
/// compared by time, as `cmp_value` compares a time with a text (r2): the
/// lower edge of a text claim was `>= ""`, which no time passes.
#[test]
fn a_time_field_orders_against_a_claim_that_reads_as_a_time() {
    let times = "[{at: \"2026-01-01T00:00:00Z\"}, {at: \"2026-06-01T00:00:00Z\"}, \
                 {at: \"2027-01-01T00:00:00Z\"}]";
    let n = start(
        "early read where at <= $jwt.until\n\
         late  read where at >= $jwt.since\n\
         live  read where now() <= $jwt.until\n\
         dated read where at <= $jwt.until\n\
         days  read where day <= $jwt.until\n",
        &[
            "create collection early (at timestamp)",
            &format!("put early {times}"),
            "create collection late (at timestamp)",
            &format!("put late {times}"),
            "create collection live (x int)",
            "put live {x: 1}",
            "create collection dated (at timestamp @sorted)",
            &format!("put dated {times}"),
            "create collection days (day text)",
            "put days [{day: \"2026-01-01\"}, {day: \"2026-09-01\"}, {day: \"soon\"}]",
        ],
    );
    let july = r#"{"until":"2026-07-01T00:00:00Z"}"#;
    assert_eq!(n.rows(july, "get early count"), r#"[{"count":2}]"#);
    assert_eq!(
        n.rows(r#"{"since":"2026-03-01T00:00:00Z"}"#, "get late count"),
        r#"[{"count":2}]"#
    );
    assert_eq!(
        n.rows(r#"{"until":"2999-01-01T00:00:00Z"}"#, "get live count"),
        r#"[{"count":1}]"#
    );
    // The same over an ordered index, and the range exact.
    assert_eq!(n.rows(july, "get dated count"), r#"[{"count":2}]"#);
    // A text field compares as text, a number by kind not at all; a text
    // that is no time is no time.
    assert_eq!(
        n.rows(july, "get days select day order id"),
        r#"[{"day":"2026-01-01"}]"#
    );
    assert_eq!(
        n.rows(r#"{"until":"later"}"#, "get early count"),
        r#"[{"count":0}]"#
    );
    assert_eq!(
        n.rows(r#"{"until":1767225600000}"#, "get early count"),
        r#"[{"count":1}]"#
    );
}

/// The guard's edges over a json `@sorted` field, a `nocase` and a
/// collated `@sorted` text field give the scan's answer: a twin collection
/// without the index, the same rule (r3).
#[test]
fn the_guard_over_json_nocase_and_collated_indexes_gives_the_scans_answer() {
    let kinds: &[(&str, &str, &str, &str)] = &[
        (
            "grades",
            "$jwt.at_least <= score",
            "score json",
            "[{score: 3}, {score: 8}, {score: \"A\"}, {score: true}, {score: [\"x\"]}]",
        ),
        (
            "marks",
            "score <= $jwt.at_most",
            "score json",
            "[{score: 3}, {score: 8}, {score: \"A\"}, {score: true}, {score: [\"x\"]}, {score: false}]",
        ),
        (
            "flags",
            "state >= $jwt.flag",
            "state json",
            "[{state: true}, {state: false}, {state: 1}, {state: \"yes\"}]",
        ),
        (
            "negated",
            "not (score > $jwt.at_most)",
            "score json",
            "[{score: 3}, {score: 8}, {score: \"A\"}, {score: true}, {score: [\"x\"]}, {score: false}]",
        ),
        (
            "names",
            "name >= $jwt.from",
            "name text nocase",
            "[{name: \"alice\"}, {name: \"Bob\"}, {name: \"carol\"}]",
        ),
        (
            "words",
            "name >= $jwt.from",
            "name text collate und",
            "[{name: \"apple\"}, {name: \"Banana\"}, {name: \"cherry\"}, {name: \"\u{e9}clair\"}]",
        ),
        (
            "tr",
            "name < $jwt.to",
            "name text collate tr nocase",
            "[{name: \"ilk\"}, {name: \"Irmak\"}, {name: \"\u{130}stanbul\"}, {name: \"zeytin\"}]",
        ),
        (
            "levels",
            "not (level > $jwt.at_most)",
            "level int",
            "[{level: 1}, {level: 5}, {level: 9}]",
        ),
    ];
    let mut policy = String::new();
    let mut setup = Vec::new();
    for (c, rule, field, docs) in kinds {
        for (suffix, ix) in [("", " @sorted"), ("_scan", "")] {
            policy.push_str(&format!("{c}{suffix} read where {rule}\n"));
            setup.push(format!("create collection {c}{suffix} ({field}{ix})"));
            setup.push(format!("put {c}{suffix} {docs}"));
        }
    }
    let setup: Vec<&str> = setup.iter().map(String::as_str).collect();
    let n = start(&policy, &setup);
    let mut differ = Vec::new();
    for (claims, c, sel) in [
        (r#"{"at_least":5}"#, "grades", "score"),
        (r#"{"at_least":"A"}"#, "grades", "score"),
        (r#"{"at_least":"B"}"#, "grades", "score"),
        (r#"{"at_least":false}"#, "grades", "score"),
        (r#"{"at_most":5}"#, "marks", "score"),
        (r#"{"at_most":"Z"}"#, "marks", "score"),
        (r#"{"at_most":true}"#, "marks", "score"),
        (r#"{"flag":true}"#, "flags", "state"),
        (r#"{"flag":false}"#, "flags", "state"),
        (r#"{"flag":0}"#, "flags", "state"),
        (r#"{"at_most":5}"#, "negated", "score"),
        (r#"{"at_most":"Z"}"#, "negated", "score"),
        (r#"{"at_most":true}"#, "negated", "score"),
        (r#"{"from":"b"}"#, "names", "name"),
        (r#"{"from":"B"}"#, "names", "name"),
        (r#"{"from":5}"#, "names", "name"),
        (r#"{"from":"b"}"#, "words", "name"),
        (r#"{"from":"d"}"#, "words", "name"),
        (r#"{"to":"j"}"#, "tr", "name"),
        (r#"{"at_most":5}"#, "levels", "level"),
        (r#"{"at_most":"top"}"#, "levels", "level"),
    ] {
        let a = n.rows(claims, &format!("get {c} select {sel} order id"));
        let b = n.rows(claims, &format!("get {c}_scan select {sel} order id"));
        if a != b {
            differ.push(format!("{claims} {c}: index {a} scan {b}"));
        }
    }
    assert!(differ.is_empty(), "{differ:#?}");
}

/// What a scoped read over an ordered index plans is what the server's own
/// read with the claim written in plans (r6): the guard's edges, true of
/// every value of a typed field, are no bound and leave the range exact.
#[test]
fn a_guarded_read_plans_as_the_server_tokens() {
    let files: Vec<String> = (0..2000)
        .map(|i| format!("{{name: \"n{i:05}\", level: {i}}}"))
        .collect();
    let people: Vec<String> = (0..2000)
        .map(|i| format!("{{name: \"n{i:05}\"}}"))
        .collect();
    let put_files = format!("put files [{}]", files.join(","));
    let put_people = format!("put people [{}]", people.join(","));
    let n = start(
        "files read where level <= $jwt.clearance\n\
         people read where name >= $jwt.from\n",
        &[
            "create collection files (name text, level int @sorted)",
            &put_files,
            "create collection people (name text @sorted)",
            &put_people,
        ],
    );
    let plan = |token: &str, sql: &str| {
        let (status, body) = n.query(token, sql);
        assert_eq!(status, 200, "{sql}: {body}");
        body
    };
    let clearance = n.token(r#"{"clearance":5}"#);
    let from = n.token(r#"{"from":"n01995"}"#);
    for (root, scoped, token) in [
        (
            "explain get files where level <= 5",
            "explain get files",
            &clearance,
        ),
        (
            "explain get files where level <= 5 count",
            "explain get files count",
            &clearance,
        ),
        (
            "explain get people where name >= \"n01995\"",
            "explain get people",
            &from,
        ),
    ] {
        let (root, scoped) = (plan(ROOT, root), plan(token, scoped));
        assert!(root.contains("the ordered index on"), "{root}");
        assert_eq!(scoped, root, "{scoped}\n{root}");
    }
}

/// A scoped count over an ordered index is answered from the range as the
/// server's own is (r8), with no row read: its time is `make
/// policy-bench`'s.
#[test]
fn a_guarded_count_over_an_ordered_index_reads_no_row() {
    let mut setup = vec!["create collection files (name text, level int @sorted)".to_string()];
    for chunk in 0..4 {
        let docs: Vec<String> = (0..10_000)
            .map(|i| format!("{{name: \"n\", level: {}}}", chunk * 10_000 + i))
            .collect();
        setup.push(format!("put files [{}]", docs.join(",")));
    }
    let setup: Vec<&str> = setup.iter().map(String::as_str).collect();
    let n = start("files read where level <= $jwt.clearance\n", &setup);
    let t = n.token(r#"{"clearance":4000}"#);
    assert_eq!(n.query(&t, "get files count").1, r#"[{"count":4001}]"#);
    let (_, root) = n.query(ROOT, "explain get files where level <= 4000 count");
    let (_, scoped) = n.query(&t, "explain get files count");
    assert_eq!(scoped, root);
}

/// Under a `not` a value of another kind than the claim passes neither
/// the comparison nor its negation (r9): the guard ANDed in became an `or`
/// there, and `not (score > 5)` admitted every text and list.
#[test]
fn a_negated_ordered_claim_admits_no_value_of_another_kind() {
    let n = start(
        "grades read where not (score > $jwt.max)\n\
         files  read where not (level > $jwt.c)\n\
         twice  read where not (not (level <= $jwt.c))\n\
         inner  read where id in (get files_seen select at where not (level > $jwt.c))\n\
         outer  read where at not in (get files_seen select at where level > $jwt.c)\n\
         files_seen read\n",
        &[
            "create collection grades (score json)",
            "put grades [{score: 3}, {score: 8}, {score: \"A\"}, {score: true}, {score: [\"x\"]}]",
            "create collection files (name text, level int)",
            "put files [{name: \"a\", level: 1}, {name: \"b\", level: 5}, {name: \"c\", level: 9}]",
            "create collection twice (name text, level int)",
            "put twice [{name: \"a\", level: 1}, {name: \"b\", level: 5}, {name: \"c\", level: 9}]",
            "create collection files_seen (at int, level json)",
            "put files_seen [{at: 1, level: 1}, {at: 2, level: 9}, {at: 3, level: \"x\"}]",
            "create collection inner (x int)",
            "put inner [{x: 1}, {x: 2}, {x: 3}]",
            "create collection outer (at int)",
            "put outer [{at: 1}, {at: 2}, {at: 3}]",
        ],
    );
    let grades = n.rows(r#"{"max":5}"#, "get grades select score order id");
    assert_eq!(grades, r#"[{"score":3}]"#, "not (score > 5)");
    let files = "get files select name order name";
    assert_eq!(
        n.rows(r#"{"c":"top"}"#, files),
        "[]",
        "not (level > \"top\")"
    );
    assert_eq!(
        n.rows(r#"{"c":5}"#, files),
        r#"[{"name":"a"},{"name":"b"}]"#
    );
    let twice = "get twice select name order name";
    assert_eq!(n.rows(r#"{"c":"top"}"#, twice), "[]");
    assert_eq!(
        n.rows(r#"{"c":5}"#, twice),
        r#"[{"name":"a"},{"name":"b"}]"#
    );
    // An inner `get`'s filter stands where its `in` does: the row of
    // another kind is in no list a token is shown through, nor out of one
    // a `not in` holds rows out by.
    assert_eq!(
        n.rows(r#"{"c":5}"#, "get inner select x order id"),
        r#"[{"x":1}]"#
    );
    assert_eq!(
        n.rows(r#"{"c":5}"#, "get outer select at order id"),
        r#"[{"at":1}]"#
    );
}

/// A deny list read through a rule the token's claim leaves out is empty,
/// and its `not in` admits every row (r10) -- as it did to a token without
/// the claim, nothing this changed. Named in the rule holding the `not in`,
/// the claim fails closed.
#[test]
fn a_deny_list_names_its_claim_where_its_not_in_is() {
    let n = start(
        "posts  read where author not in (get bans select author)\n\
         closed read where author not in (get bans select author where viewer = $jwt.sub)\n\
         bans   read where viewer = $jwt.sub\n",
        &[
            "create collection posts (author text, body text)",
            "put posts [{author: \"spam\", body: \"buy\"}, {author: \"ok\", body: \"hello\"}]",
            "create collection closed (author text, body text)",
            "put closed [{author: \"spam\", body: \"buy\"}, {author: \"ok\", body: \"hello\"}]",
            "create collection bans (author text, viewer text)",
            "put bans [{author: \"spam\", viewer: \"alice\"}]",
        ],
    );
    let posts = "get posts select body order id";
    let both = r#"[{"body":"buy"},{"body":"hello"}]"#;
    assert_eq!(n.rows(r#"{"sub":null}"#, posts), both);
    assert_eq!(n.rows(r#"{"x":1}"#, posts), both);
    assert_eq!(n.rows(r#"{"sub":"alice"}"#, posts), r#"[{"body":"hello"}]"#);
    let closed = "get closed select body order id";
    for claims in [r#"{"sub":null}"#, r#"{"x":1}"#] {
        let (status, body) = n.query(&n.token(claims), closed);
        assert_eq!(status, 404, "{claims}: {body}");
    }
    assert_eq!(
        n.rows(r#"{"sub":"alice"}"#, closed),
        r#"[{"body":"hello"}]"#
    );
}

/// An index over several fields plans a scoped read as it plans the server
/// token's with the claims written in: the guard's `level > true` beside
/// `level <= $jwt.c` is true of every int, no bound, where it was left to
/// be tested row by row and a count read every row of its span.
#[test]
fn a_guarded_read_over_an_index_of_several_fields_plans_as_the_server_tokens() {
    let docs: Vec<String> = (0..3000)
        .map(|i| format!("{{team: \"t{}\", level: {i}}}", i % 3))
        .collect();
    let put = format!("put files [{}]", docs.join(","));
    let n = start(
        "files read where team = $jwt.team and level <= $jwt.c\n",
        &[
            "create collection files (team text, level int)",
            "create index on files (team, level) @sorted",
            &put,
        ],
    );
    let t = n.token(r#"{"team":"t1","c":2000}"#);
    for (scoped, root) in [
        (
            "get files count",
            "get files where team = \"t1\" and level <= 2000 count",
        ),
        (
            "get files order level desc limit 5",
            "get files where team = \"t1\" and level <= 2000 order level desc limit 5",
        ),
    ] {
        let (_, plan) = n.query(&t, &format!("explain {scoped}"));
        let (_, want) = n.query(ROOT, &format!("explain {root}"));
        assert!(want.contains("the index on (team, level)"), "{want}");
        assert_eq!(plan, want, "{scoped}");
        assert_eq!(n.query(&t, scoped).1, n.query(ROOT, root).1, "{scoped}");
    }
    // A claim of another kind: no span, and no row.
    let top = n.token(r#"{"team":"t1","c":"top"}"#);
    assert_eq!(n.query(&top, "get files count").1, r#"[{"count":0}]"#);
    let (_, plan) = n.query(&top, "explain get files count");
    assert!(plan.contains("the index on (team, level)"), "{plan}");
}

/// SQL's `like`, `ilike` and `not like` over a claim compare nothing in
/// order: the claim as the pattern, or the escape, starts as any bare
/// claim does, and matches as the pattern written out.
#[test]
fn a_claim_as_a_like_pattern_starts_and_matches() {
    let n = start(
        "files  read where path like $jwt.home\n\
         people read where email ilike $jwt.domain\n\
         others read where path not like $jwt.home escape $jwt.esc\n",
        &[
            "create collection files (path text)",
            "put files [{path: \"/home/a/x\"}, {path: \"/home/b/y\"}]",
            "create collection people (email text)",
            "put people [{email: \"A@Corp.com\"}, {email: \"b@else.org\"}]",
            "create collection others (path text)",
            "put others [{path: \"/home/a_x\"}, {path: \"/home/abx\"}]",
        ],
    );
    assert_eq!(
        n.rows(r#"{"home":"/home/a/%"}"#, "get files select path"),
        r#"[{"path":"/home/a/x"}]"#
    );
    assert_eq!(
        n.rows(r#"{"domain":"%@corp.com"}"#, "get people select email"),
        r#"[{"email":"A@Corp.com"}]"#
    );
    assert_eq!(
        n.rows(
            r#"{"home":"/home/a!_x","esc":"!"}"#,
            "get others select path"
        ),
        r#"[{"path":"/home/abx"}]"#
    );
}

/// A text claim that reads as a time holds a text field as text: `day <=
/// "2026-01-01"` answers the rows the server token's does, `""`, `"!x"`
/// and `"-1"` among them, which a time's lower edge, `"0000-01-01"`, held
/// out over every field. A time field orders against it by time, a json
/// field's texts as texts and its numbers not at all, and an ordered index
/// plans it as the server token's read.
#[test]
fn a_claim_that_reads_as_a_time_holds_a_text_field_as_text() {
    let days =
        "[{day: \"\"}, {day: \"!x\"}, {day: \"-1\"}, {day: \"1999\"}, {day: \"2026-06-01\"}]";
    // Days past the claim, for the range to be the index's to count.
    let later: Vec<String> = (0..400)
        .map(|i| format!("{{day: \"2030-{i:04}\"}}"))
        .collect();
    let later = format!("put sdays [{}]", later.join(","));
    let n = start(
        "days   read where day <= $jwt.until\n\
         sdays  read where day <= $jwt.until\n\
         vers   read where ver <= $jwt.max\n\
         nvers  read where not (ver > $jwt.max)\n\
         stamps read where at <= $jwt.until\n\
         meta   read where m.day <= $jwt.until\n",
        &[
            "create collection days (day text)",
            &format!("put days {days}"),
            "create collection sdays (day text @sorted)",
            &format!("put sdays {days}"),
            &later,
            "create collection vers (ver text)",
            "put vers [{ver: \"0.9\"}, {ver: \"1-0-0\"}, {ver: \"2-0-0\"}]",
            "create collection nvers (ver text, n int)",
            "put nvers [{ver: \"0.9\"}, {ver: \"1-0-0\"}, {ver: \"2-0-0\"}, {n: 1}]",
            "create collection stamps (at timestamp @sorted)",
            "put stamps [{at: 0}, {at: \"2026-01-01T00:00:00Z\"}, {at: \"2027-01-01T00:00:00Z\"}]",
            "create collection meta (m json)",
            "put meta [{m: {day: \"\"}}, {m: {day: \"-1\"}}, {m: {day: 5}}, {m: {day: true}}, \
             {m: {day: \"2026-06-01\"}}]",
        ],
    );
    let until = r#"{"until":"2026-01-01"}"#;
    for (c, field) in [("days", "day"), ("sdays", "day")] {
        let scoped = n.rows(until, &format!("get {c} select {field} order id"));
        let (_, root) = n.query(
            ROOT,
            &format!("get {c} select {field} where {field} <= \"2026-01-01\" order id"),
        );
        assert_eq!(scoped, root, "{c}");
        assert!(scoped.contains(r#"{"day":""}"#), "{scoped}");
    }
    let (_, plan) = n.query(&n.token(until), "explain get sdays count");
    let (_, want) = n.query(ROOT, "explain get sdays where day <= \"2026-01-01\" count");
    assert!(want.contains("which is the answer"), "{want}");
    assert_eq!(plan, want);
    let max = r#"{"max":"1-1-1"}"#;
    assert_eq!(
        n.rows(max, "get vers select ver order id"),
        r#"[{"ver":"0.9"},{"ver":"1-0-0"}]"#
    );
    assert_eq!(
        n.rows(max, "get nvers select ver order id"),
        r#"[{"ver":"0.9"},{"ver":"1-0-0"},{"ver":null}]"#
    );
    assert_eq!(n.rows(until, "get stamps count"), r#"[{"count":2}]"#);
    let (_, plan) = n.query(&n.token(until), "explain get stamps count");
    assert!(plan.contains("the ordered index on at"), "{plan}");
    assert_eq!(
        n.rows(until, "get meta select m order id"),
        r#"[{"m":{"day":""}},{"m":{"day":"-1"}}]"#
    );
}

/// Under a `not`, a row with no value answers as the rule written out:
/// `not (level > $jwt.c)` admits the rows with no `level`, as main did and
/// the server token's `not (level > 5)` does, while a value of another
/// kind passes neither the comparison nor its negation.
#[test]
fn a_row_with_no_value_answers_a_negated_rule_as_the_rule_written_out() {
    let n = start(
        "files read where not (level > $jwt.c)\n\
         paths read where not (meta.level > $jwt.c)\n",
        &[
            "create collection files (name text, level int)",
            "put files [{name: \"a\", level: 1}, {name: \"b\", level: 9}, {name: \"c\"}]",
            "create collection paths (meta json)",
            "put paths [{meta: {level: 1}}, {meta: {level: \"x\"}}, {meta: {level: true}}, \
             {meta: {}}, {meta: {level: null}}]",
        ],
    );
    let scoped = n.rows(r#"{"c":5}"#, "get files select name order id");
    let (_, root) = n.query(ROOT, "get files select name where not (level > 5) order id");
    assert_eq!(scoped, root);
    assert_eq!(scoped, r#"[{"name":"a"},{"name":"c"}]"#);
    assert_eq!(
        n.rows(r#"{"c":5}"#, "get paths select meta order id"),
        r#"[{"meta":{"level":1}},{"meta":{}},{"meta":{"level":null}}]"#
    );
    assert_eq!(
        n.rows(r#"{"c":"top"}"#, "get files select name order id"),
        r#"[{"name":"c"}]"#
    );
}

/// A deny list over a collection whose read rule orders by a claim holds
/// out what main held out: the rule is ANDed into the `not ... in`'s list
/// bound for the `not`, so a ban of another kind than the claim stays in
/// the list, where bound for a place standing alone it left it.
#[test]
fn a_deny_list_over_an_ordered_rule_holds_out_what_main_did() {
    let n = start(
        "posts read where author not in (get bans select author)\n\
         shown read where author in (get bans select author)\n\
         bans  read where severity >= $jwt.min\n",
        &[
            "create collection posts (author text, body text)",
            "put posts [{author: \"spam\", body: \"buy\"}, {author: \"ok\", body: \"hello\"}, \
             {author: \"low\", body: \"meh\"}, {author: \"bad\", body: \"no\"}]",
            "create collection shown (author text, body text)",
            "put shown [{author: \"spam\", body: \"buy\"}, {author: \"ok\", body: \"hello\"}, \
             {author: \"low\", body: \"meh\"}, {author: \"bad\", body: \"no\"}]",
            "create collection bans (author text, severity json)",
            "put bans [{author: \"spam\", severity: \"high\"}, {author: \"low\", severity: 3}, \
             {author: \"bad\", severity: 7}]",
        ],
    );
    let min = r#"{"min":5}"#;
    assert_eq!(
        n.rows(min, "get posts select body order id"),
        r#"[{"body":"hello"},{"body":"meh"}]"#
    );
    // The same rule under an `in` holds the ban of another kind out of the
    // list, and its post out of the answer.
    assert_eq!(
        n.rows(min, "get shown select body order id"),
        r#"[{"body":"no"}]"#
    );
}

/// Forms that hold no row of another kind: a claim against a claim,
/// `between` and `not between`, the claim on the left under a `not`, a
/// path whose value's kind is a row's, a bool field, De Morgan, and two
/// `not in`s deep.
#[test]
fn forms_that_admit_no_row_of_another_kind() {
    let n = start(
        "cc     read where $jwt.a < $jwt.b\n\
         ncc    read where not ($jwt.a < $jwt.b)\n\
         btw    read where level between $jwt.lo and $jwt.hi\n\
         nbtw   read where level not between $jwt.lo and $jwt.hi\n\
         left   read where not ($jwt.c < level)\n\
         paths  read where meta.level <= $jwt.c\n\
         flags  read where not (on > $jwt.c)\n\
         dm     read where not (level > $jwt.c and name < $jwt.d)\n\
         deep   read where x not in (get mid select x where y not in (get leaf select y where level > $jwt.c))\n\
         mid    read\n\
         leaf   read\n",
        &[
            "create collection cc (x int)",
            "put cc [{x: 1}]",
            "create collection ncc (x int)",
            "put ncc [{x: 1}]",
            "create collection btw (level int)",
            "put btw [{level: 1}, {level: 5}]",
            "create collection nbtw (level int)",
            "put nbtw [{level: 1}, {level: 5}]",
            "create collection left (level int)",
            "put left [{level: 1}, {level: 9}]",
            "create collection paths (meta json)",
            "put paths [{meta: {level: 1}}, {meta: {level: \"x\"}}, {meta: {level: true}}, \
             {meta: {level: [1]}}, {meta: {}}]",
            "create collection flags (on bool)",
            "put flags [{on: true}, {on: false}]",
            "create collection dm (level int, name text)",
            "put dm [{level: 1, name: \"a\"}, {level: 9, name: \"z\"}]",
            "create collection deep (x int)",
            "put deep [{x: 1}, {x: 2}]",
            "create collection mid (x int, y int)",
            "put mid [{x: 1, y: 10}, {x: 2, y: 20}]",
            "create collection leaf (y int, level json)",
            "put leaf [{y: 10, level: 9}, {y: 20, level: \"x\"}]",
        ],
    );
    let count = |claims: &str, c: &str| n.rows(claims, &format!("get {c} count"));
    assert_eq!(count(r#"{"a":5,"b":"top"}"#, "cc"), r#"[{"count":0}]"#);
    assert_eq!(count(r#"{"a":"x","b":5}"#, "ncc"), r#"[{"count":0}]"#);
    assert_eq!(count(r#"{"a":1,"b":5}"#, "cc"), r#"[{"count":1}]"#);
    assert_eq!(count(r#"{"lo":"a","hi":"z"}"#, "btw"), r#"[{"count":0}]"#);
    assert_eq!(count(r#"{"lo":"a","hi":"z"}"#, "nbtw"), r#"[{"count":0}]"#);
    assert_eq!(count(r#"{"lo":2,"hi":9}"#, "nbtw"), r#"[{"count":1}]"#);
    assert_eq!(count(r#"{"c":"top"}"#, "left"), r#"[{"count":0}]"#);
    assert_eq!(count(r#"{"c":5}"#, "left"), r#"[{"count":1}]"#);
    let p = "get paths select meta order id";
    assert_eq!(n.rows(r#"{"c":5}"#, p), r#"[{"meta":{"level":1}}]"#);
    assert_eq!(n.rows(r#"{"c":"z"}"#, p), r#"[{"meta":{"level":"x"}}]"#);
    assert_eq!(n.rows(r#"{"c":true}"#, p), r#"[{"meta":{"level":true}}]"#);
    assert_eq!(count(r#"{"c":5}"#, "flags"), r#"[{"count":0}]"#);
    assert_eq!(count(r#"{"c":"top","d":5}"#, "dm"), r#"[{"count":0}]"#);
    // The leaf's rows over 5 held to numbers: y 10. The mid rows whose y
    // is not 10: x 2. The deep rows whose x is not 2: x 1.
    assert_eq!(
        n.rows(r#"{"c":5}"#, "get deep select x order id"),
        r#"[{"x":1}]"#
    );
}
