//! Tokens as an identity provider issues them. A provider's keys sign
//! every token it issues, for every app of a tenant, a team or a pool, so a
//! server told an issuer and an audience takes only the tokens issued for
//! it -- on every route a token reaches: `/query`, REST, `/batch`, a
//! subscription, MCP and `/_whoami` -- and refuses the rest with 401
//! naming the check and not the token's value. A rule's `for` reads the
//! role claim the server names, and a policy names a claim outside letters,
//! digits and `_` in quotes. Told none of it, a server takes what it took.

use fenec_core::prelude::*;
use fenec_http::access::{Access, Demands};
use fenec_http::crypto::{b64url_encode, hmac_sha256};
use fenec_http::mcp::Options;
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const SECRET: &[u8] = b"thirty-two bytes and a few more, for HS256";
const ISSUER: &str = "https://idp.example/";
const AUDIENCE: &str = "notes-api";
/// Where a provider puts what a rule reads, by its own names.
const POLICY: &str = r#"
notes  read,write  where owner = $jwt.sub
teams  read        where team in $jwt."cognito:groups"
posts  read,write  where author = $jwt."https://example.com/user"
board  read
board  write                                 for moderator
"#;
const SETUP: &[&str] = &[
    "create collection notes (owner text @hash, title text)",
    "create collection teams (team text, name text)",
    "create collection posts (author text, title text)",
    "create collection board (msg text)",
    r#"put notes [{owner: "alice", title: "a1"}, {owner: "bob", title: "b1"}]"#,
    r#"put teams [{team: "t1", name: "one"}, {team: "t2", name: "two"}, {team: "t3", name: "three"}]"#,
    r#"put posts [{author: "u-alice", title: "mine"}, {author: "u-bob", title: "theirs"}]"#,
];

struct Node {
    port: u16,
}

fn start(demands: Demands) -> Node {
    // The wait after a refusal is the process's, counted by address
    // (tests/audit.rs holds it): every test here asks from 127.0.0.1.
    fenec_http::audit::set_delay(0);
    let access = Arc::new(Access::new(SECRET, POLICY).unwrap().demanding(demands));
    let mut db = Database::new();
    for sql in SETUP {
        db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
    }
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        token: Some("root-token".into()),
        access: Some(access),
        // MCP's calls a second are counted by subject, the process's.
        mcp: Some(Arc::new(Options {
            calls_per_sec: 0,
            ..Options::default()
        })),
        ..Config::default()
    };
    let server = Server::new(Arc::new(RwLock::new(db)), cfg);
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node { port }
}

/// The server told the issuer and the audience above, and the role claim a
/// provider names by a URL.
fn told() -> Node {
    start(Demands {
        issuers: vec![ISSUER.into(), "https://other-idp.example".into()],
        audiences: vec![AUDIENCE.into()],
        role_claim: "https://example.com/roles".into(),
        ..Demands::default()
    })
}

/// An HS256 token for `claims` as given, an hour's `exp` added: `mint`
/// stamps the issuer and audience a server was told on claims naming none.
fn token(claims: &str) -> String {
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let open = claims.strip_suffix('}').expect("a JSON object");
    let claims = format!("{open},\"exp\":{exp}}}");
    let head = b64url_encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let body = b64url_encode(claims.as_bytes());
    let sig = hmac_sha256(SECRET, format!("{head}.{body}").as_bytes());
    format!("{head}.{body}.{}", b64url_encode(&sig))
}

/// Alice's token, issued for this server: the provider's issuer, and an
/// `aud` listing another API beside this one.
fn alice() -> String {
    token(&format!(
        r#"{{"sub":"alice","iss":"{ISSUER}","aud":["https://idp.example/userinfo","{AUDIENCE}"]}}"#
    ))
}

impl Node {
    fn call(
        &self,
        token: &str,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut head = format!(
            "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {token}\r\n",
            self.port
        );
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str(&format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ));
        s.write_all(head.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let body = out
            .split_once("\r\n\r\n")
            .map_or("", |(_, b)| b)
            .to_string();
        (out[9..12].parse().unwrap(), body)
    }

    fn query(&self, token: &str, q: &str) -> (u16, String) {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, q);
        body.push('}');
        self.call(token, "POST", "/query", &[], &body)
    }

    /// A subscription to the notes: its status, and what it said up to the
    /// seed -- or, refused, its answer.
    fn subscribe(&self, token: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        write!(
            s,
            "GET /notes/changes HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\r\n"
        )
        .unwrap();
        let mut heard = String::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 4096];
        let done = |h: &str| {
            h.contains("event: seed")
                || h.split_once("\r\n\r\n")
                    .is_some_and(|(head, body)| !head.contains(" 200 ") && body.contains('}'))
        };
        while Instant::now() < deadline && !done(&heard) {
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(k) => heard.push_str(&String::from_utf8_lossy(&buf[..k])),
                Err(_) => {}
            }
        }
        let status = heard.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0);
        (status, heard)
    }

    /// MCP's `query` tool.
    fn mcp(&self, token: &str, q: &str) -> (u16, String) {
        let mut args = String::new();
        fenec_core::json::escape_into(&mut args, q);
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"query","arguments":{{"query":{args}}}}}}}"#
        );
        let headers = [
            ("MCP-Protocol-Version", "2025-06-18"),
            ("Content-Type", "application/json"),
            ("Accept", "application/json, text/event-stream"),
        ];
        self.call(token, "POST", "/_mcp", &headers, &body)
    }

    /// Every route a token reaches, each with its status: `/query`, REST,
    /// `/batch`, a subscription, MCP and `/_whoami`.
    fn every_route(&self, token: &str) -> Vec<(&'static str, u16, String)> {
        let batch = r#"{"query":"get notes"}"#;
        let (q, qb) = self.query(token, "get notes");
        let (r, rb) = self.call(token, "GET", "/notes", &[], "");
        let (b, bb) = self.call(token, "POST", "/batch", &[], batch);
        let (s, sb) = self.subscribe(token);
        let (m, mb) = self.mcp(token, "get notes");
        let (w, wb) = self.call(token, "GET", "/_whoami", &[], "");
        vec![
            ("/query", q, qb),
            ("REST", r, rb),
            ("/batch", b, bb),
            ("a subscription", s, sb),
            ("MCP", m, mb),
            ("/_whoami", w, wb),
        ]
    }
}

#[test]
fn a_token_issued_for_this_server_is_taken_on_every_route() {
    let n = told();
    for (route, status, body) in n.every_route(&alice()) {
        assert_eq!(status, 200, "{route}: {body}");
        if route != "/_whoami" {
            assert!(
                body.contains("a1") && !body.contains("b1"),
                "{route}: {body}"
            );
        }
    }
    // `aud` as a text, and the other issuer told.
    let one = token(&format!(
        r#"{{"sub":"alice","iss":"https://other-idp.example","aud":"{AUDIENCE}"}}"#
    ));
    let (status, body) = n.query(&one, "get notes");
    assert!(status == 200 && body.contains("a1"), "{status} {body}");
}

#[test]
fn a_token_for_another_issuer_or_audience_is_refused_on_every_route() {
    let n = told();
    let refused = [
        // Another issuer: its keys are this provider's, its app another's.
        (
            format!(r#"{{"sub":"alice","iss":"https://evil.example/","aud":"{AUDIENCE}"}}"#),
            "issuer (iss)",
            "evil.example",
        ),
        // The issuer as written, but for its trailing `/`.
        (
            format!(r#"{{"sub":"alice","iss":"https://idp.example","aud":"{AUDIENCE}"}}"#),
            "issuer (iss)",
            "https://idp.example",
        ),
        // No issuer at all.
        (
            format!(r#"{{"sub":"alice","aud":"{AUDIENCE}"}}"#),
            "issuer (iss)",
            "\"sub\"",
        ),
        // Another app's audience, as a text and as a list.
        (
            format!(r#"{{"sub":"alice","iss":"{ISSUER}","aud":"billing-api"}}"#),
            "audience (aud)",
            "billing-api",
        ),
        (
            format!(r#"{{"sub":"alice","iss":"{ISSUER}","aud":["billing-api","admin"]}}"#),
            "audience (aud)",
            "billing-api",
        ),
        // No audience, where one is asked for.
        (
            format!(r#"{{"sub":"alice","iss":"{ISSUER}"}}"#),
            "audience (aud)",
            "\"sub\"",
        ),
    ];
    for (claims, check, value) in &refused {
        let t = token(claims);
        for (route, status, body) in n.every_route(&t) {
            assert_eq!(status, 401, "{route}, {claims}: {body}");
            assert!(body.contains(check), "{route}, {claims}: {body}");
            assert!(
                !body.contains(value),
                "{route} told the token's value: {body}"
            );
        }
    }
}

#[test]
fn a_client_is_its_azp_or_its_client_id() {
    let n = start(Demands {
        issuers: vec![ISSUER.into()],
        clients: vec!["app-1".into()],
        ..Demands::default()
    });
    // Cognito's access token: `client_id`, no `aud`.
    let cognito = token(&format!(
        r#"{{"sub":"alice","iss":"{ISSUER}","client_id":"app-1","token_use":"access"}}"#
    ));
    let (status, body) = n.query(&cognito, "get notes");
    assert!(status == 200 && body.contains("a1"), "{status} {body}");
    // Auth0's, Keycloak's and Clerk's: `azp`.
    let azp = token(&format!(
        r#"{{"sub":"alice","iss":"{ISSUER}","azp":"app-1"}}"#
    ));
    assert_eq!(n.query(&azp, "get notes").0, 200);
    for claims in [
        format!(r#"{{"sub":"alice","iss":"{ISSUER}","azp":"app-2","client_id":"app-1"}}"#),
        format!(r#"{{"sub":"alice","iss":"{ISSUER}","client_id":"app-2"}}"#),
        format!(r#"{{"sub":"alice","iss":"{ISSUER}","aud":"app-1"}}"#),
    ] {
        let (status, body) = n.query(&token(&claims), "get notes");
        assert_eq!(status, 401, "{claims}: {body}");
        assert!(body.contains("client (azp"), "{body}");
        assert!(!body.contains("app-2"), "{body}");
    }
}

#[test]
fn a_rule_for_a_role_reads_the_claim_the_server_names() {
    let n = told();
    let with = |extra: &str| {
        token(&format!(
            r#"{{"sub":"alice","iss":"{ISSUER}","aud":"{AUDIENCE}"{extra}}}"#
        ))
    };
    // A list holding the role, in the claim named.
    let listed = with(r#","https://example.com/roles":["reader","moderator"]"#);
    assert_eq!(n.query(&listed, r#"put board {msg: "hi"}"#).0, 200);
    // A text naming it.
    let text = with(r#","https://example.com/roles":"moderator""#);
    assert_eq!(n.query(&text, r#"put board {msg: "again"}"#).0, 200);
    // `role` is a claim like any other there, and a list without the role
    // grants nothing.
    for t in [
        with(r#","role":"moderator""#),
        with(r#","https://example.com/roles":["reader"]"#),
        with(r#","https://example.com/roles":{"moderator":true}"#),
    ] {
        let (status, body) = n.query(&t, r#"put board {msg: "no"}"#);
        assert_eq!(status, 403, "{body}");
    }
    let (_, rows) = n.query(&alice(), "get board");
    assert!(
        rows.contains("hi") && rows.contains("again") && !rows.contains("no"),
        "{rows}"
    );
}

#[test]
fn a_quoted_claim_name_is_read_in_read_and_write_rules() {
    let n = told();
    let t = token(&format!(
        r#"{{"sub":"alice","iss":"{ISSUER}","aud":"{AUDIENCE}","cognito:groups":["t1","t3"],"https://example.com/user":"u-alice"}}"#
    ));
    // A list claim by its provider's name, spread into `in`.
    let (status, rows) = n.query(&t, "get teams order team");
    assert_eq!(status, 200, "{rows}");
    assert!(
        rows.contains("one") && rows.contains("three") && !rows.contains("two"),
        "{rows}"
    );
    // A text claim named by a URL: reads, an insert stamped with it, and a
    // write naming another author refused.
    let (_, rows) = n.query(&t, "get posts");
    assert!(rows.contains("mine") && !rows.contains("theirs"), "{rows}");
    let (status, body) = n.query(&t, r#"put posts {title: "new"}"#);
    assert_eq!(status, 200, "{body}");
    let (status, body) = n.query(&t, r#"put posts {author: "u-bob", title: "forged"}"#);
    assert_eq!(status, 403, "{body}");
    let (status, body) = n.query(&t, r#"set posts {author: "u-bob"} where title = "mine""#);
    assert_eq!(status, 403, "{body}");
    let (_, rows) = n.query(
        "root-token",
        "get posts where author = \"u-alice\" order title",
    );
    assert!(rows.contains("mine") && rows.contains("new"), "{rows}");
    let (_, rows) = n.query("root-token", "get posts where author = \"u-bob\"");
    assert!(
        !rows.contains("forged") && rows.contains("theirs"),
        "{rows}"
    );
    // `/_whoami` shows the rule as the policy writes it.
    let (_, who) = n.call(&t, "GET", "/_whoami", &[], "");
    assert!(who.contains(r#"$jwt.\"cognito:groups\""#), "{who}");
    // A token lacking the claim: the rule does not apply, and a
    // collection no rule grants it is none it can tell of.
    let (status, body) = n.query(&alice(), "get teams");
    assert_eq!(status, 404, "{body}");
}

/// Told no issuer, audience, client or role claim, a server takes every
/// token its keys sign, and `for` reads `role`, as it did.
#[test]
fn without_the_flags_nothing_changes() {
    let n = start(Demands::default());
    for claims in [
        r#"{"sub":"alice"}"#,
        r#"{"sub":"alice","iss":"https://evil.example/","aud":"billing-api"}"#,
        r#"{"sub":"alice","iss":5,"aud":{"x":1},"azp":["y"]}"#,
    ] {
        for (route, status, body) in n.every_route(&token(claims)) {
            assert_eq!(status, 200, "{route}, {claims}: {body}");
        }
    }
    let role = token(r#"{"sub":"alice","role":["reader","moderator"]}"#);
    assert_eq!(n.query(&role, r#"put board {msg: "hi"}"#).0, 200);
    let other = token(r#"{"sub":"alice","https://example.com/roles":"moderator"}"#);
    assert_eq!(n.query(&other, r#"put board {msg: "no"}"#).0, 403);
}
