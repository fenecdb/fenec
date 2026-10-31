//! ES256 -- ECDSA over P-256 with SHA-256, written by hand
//! (`crypto::P256Key`) -- held to what others computed: every case of
//! Project Wycheproof's vectors in the encoding JWS uses, RFC 7515's
//! example, and tokens node:crypto signed (`es256/tokens.mjs`), which a
//! server takes beside an RSA key's and refuses once anything in them is
//! changed.

use fenec_core::prelude::*;
use fenec_http::access::Access;
use fenec_http::crypto::{b64url_decode, b64url_encode, P256Key};
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn member<'a>(o: &'a [(String, Value)], name: &str) -> &'a Value {
    &o.iter().find(|(k, _)| k == name).unwrap().1
}

fn text<'a>(o: &'a [(String, Value)], name: &str) -> &'a str {
    member(o, name).as_text().unwrap()
}

fn object(v: &Value) -> &[(String, Value)] {
    match v {
        Value::Object(o) => o,
        _ => panic!("not an object"),
    }
}

fn list(v: &Value) -> &[Value] {
    match v {
        Value::List(l) => l,
        _ => panic!("not a list"),
    }
}

/// Every case of Wycheproof's ECDSA P-256 SHA-256 vectors, their
/// signatures `R || S` as JWS has them (IEEE P1363): C2SP/wycheproof's
/// `testvectors_v1/ecdsa_secp256r1_sha256_p1363_test.json`, committed as it
/// is (es256/NOTICE). Each group's key is read from its JWK, as a JWKS file
/// gives it, where it has one; a `valid` case must verify, an `invalid` one must not, and an
/// `acceptable` one -- legal but weak, which this file has none of today --
/// may go either way.
#[test]
fn every_wycheproof_p256_sha256_case() {
    let doc =
        fenec_core::json::parse_json(include_str!("es256/ecdsa_secp256r1_sha256_p1363_test.json"))
            .unwrap();
    let doc = object(&doc);
    assert_eq!(text(doc, "algorithm"), "ECDSA");
    let (mut valid, mut invalid, mut acceptable) = (0i64, 0, 0);
    for group in list(member(doc, "testGroups")) {
        let group = object(group);
        assert_eq!(text(group, "type"), "EcdsaP1363Verify");
        assert_eq!(text(group, "sha"), "SHA-256");
        let (x, y) = match group.iter().find(|(k, _)| k == "publicKeyJwk") {
            Some((_, jwk)) => {
                let jwk = object(jwk);
                assert_eq!(text(jwk, "crv"), "P-256");
                let x = b64url_decode(text(jwk, "x")).unwrap();
                (x, b64url_decode(text(jwk, "y")).unwrap())
            }
            // A few groups, the ones about R's x and its reduction mod n,
            // give the key as hex alone: a sign byte's 00 before a high
            // bit, or fewer bytes than 32 for a small value.
            None => {
                let key = object(member(group, "publicKey"));
                assert_eq!(text(key, "curve"), "secp256r1");
                let to32 = |hex: &str| {
                    let b = unhex(hex);
                    let b = b.strip_prefix(&[0][..]).unwrap_or(&b).to_vec();
                    let mut out = vec![0u8; 32 - b.len()];
                    out.extend_from_slice(&b);
                    out
                };
                (to32(text(key, "wx")), to32(text(key, "wy")))
            }
        };
        let key = P256Key::new(&x, &y).expect("every Wycheproof key is a point of P-256");
        for case in list(member(group, "tests")) {
            let case = object(case);
            let id = member(case, "tcId");
            let (msg, sig) = (unhex(text(case, "msg")), unhex(text(case, "sig")));
            let ok = key.verify_sha256(&msg, &sig);
            match text(case, "result") {
                "valid" => {
                    assert!(ok, "case {id:?} ({}) is valid", text(case, "comment"));
                    valid += 1;
                }
                "invalid" => {
                    assert!(!ok, "case {id:?} ({}) is invalid", text(case, "comment"));
                    invalid += 1;
                }
                "acceptable" => acceptable += 1,
                other => panic!("case {id:?}: a result `{other}`"),
            }
        }
    }
    let Value::Int(total) = member(doc, "numberOfTests") else {
        panic!("numberOfTests")
    };
    assert_eq!(valid + invalid + acceptable, *total, "every case ran");
    assert_eq!((valid, invalid, acceptable), (173, 89, 0));
}

/// RFC 7515, A.3: the example JWS using ECDSA P-256 SHA-256, through a JWKS
/// file as the RFC's JWK writes the key (its private `d` beside, unread).
#[test]
fn rfc7515_a3_verifies() {
    const JWK: &str = r#"{"kty":"EC","crv":"P-256",
        "x":"f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU",
        "y":"x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0",
        "d":"jpsQnnGQmL-YBIffH1136cspYG6-0iY7X1fCE9-E9LI"}"#;
    let token = "eyJhbGciOiJFUzI1NiJ9\
        .eyJpc3MiOiJqb2UiLA0KICJleHAiOjEzMDA4MTkzODAsDQogImh0dHA6Ly9leGFt\
        cGxlLmNvbS9pc19yb290Ijp0cnVlfQ\
        .DtEhU3ljbEg8L38VWAfUAqOyKAM6-Xx-F4GawxaepmXFCgfTjDxw5djxLa8ISlSA\
        pmWQxfKTUJqPP3-Kg6NU1Q";
    let (signed, sig) = token.rsplit_once('.').unwrap();
    let sig = b64url_decode(sig).unwrap();
    // R and S as the RFC's table gives them.
    assert_eq!(sig[..4], [14, 209, 33, 83]);
    assert_eq!(sig[32..36], [197, 10, 7, 211]);
    let key = P256Key::new(
        &b64url_decode("f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU").unwrap(),
        &b64url_decode("x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0").unwrap(),
    )
    .unwrap();
    assert!(key.verify_sha256(signed.as_bytes(), &sig));
    let mut other = signed.as_bytes().to_vec();
    other[3] ^= 1;
    assert!(!key.verify_sha256(&other, &sig));

    let keys = Keys::write("rfc7515", &format!(r#"{{"keys":[{JWK}]}}"#));
    let a = Access::from_jwks(&keys.0, "notes read where owner = $jwt.sub").unwrap();
    // Its `exp` is 1300819380, March 2011.
    assert!(a.scope(token, 1_300_000_000).is_ok());
    assert_eq!(
        a.scope(token, 1_300_819_380).err(),
        Some("the token has expired")
    );
}

/// A JWKS file written for one test, removed with it.
struct Keys(PathBuf);

impl Keys {
    fn write(name: &str, jwks: &str) -> Keys {
        let dir = std::env::temp_dir().join(format!("fenec-es256-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keys.json");
        std::fs::write(&path, jwks).unwrap();
        Keys(path)
    }
}

impl Drop for Keys {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
    }
}

/// tokens.txt: the JWKS, then `<name> <token>` a line.
fn fixture() -> (&'static str, impl Fn(&str) -> String) {
    let text = include_str!("es256/tokens.txt");
    let mut lines = text.lines();
    let jwks = lines.next().unwrap();
    let tokens: Vec<(&str, &str)> = lines.map(|l| l.split_once(' ').unwrap()).collect();
    let token = move |name: &str| {
        tokens
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| t.to_string())
            .unwrap()
    };
    (jwks, token)
}

/// What a JWKS file holds beside its ES256 keys, and what it may not.
#[test]
fn a_jwks_takes_p256_keys_and_passes_over_other_curves() {
    let (jwks, token) = fixture();
    let keys = Keys::write("mixed", jwks);
    let a = Access::from_jwks(&keys.0, "notes read where owner = $jwt.sub").unwrap();
    let now = 1_800_000_000;
    let sub = |t: &str| {
        a.scope(t, now)
            .map(|s| s.subject().unwrap_or("").to_string())
    };
    assert_eq!(sub(&token("alice")).unwrap(), "alice", "named by its kid");
    assert_eq!(sub(&token("bob")).unwrap(), "bob", "no kid: each ES256 key");
    assert_eq!(
        sub(&token("carol")).unwrap(),
        "carol",
        "RS256 in the same file"
    );
    let refused = [
        ("other-kid", "the token's signature does not match"),
        ("rsa-kid", "the token's signature does not match"),
        ("der", "the token's signature does not match"),
        ("es384", "only HS256, RS256 and ES256 tokens are accepted"),
        ("eddsa", "only HS256, RS256 and ES256 tokens are accepted"),
    ];
    for (name, why) in refused {
        assert_eq!(sub(&token(name)).unwrap_err(), why, "{name}");
    }
    assert!(a.mint("{}").is_err(), "an ES256 key does not sign");

    // The keys this server takes: a P-256 key passes or the file is
    // refused, and one on another curve, or for ES384, is passed over.
    let doc = fenec_core::json::parse_json(jwks).unwrap();
    let all = list(member(object(&doc), "keys"));
    let e1 = object(&all[0]);
    let (x, y) = (text(e1, "x"), text(e1, "y"));
    let mut off = b64url_decode(y).unwrap();
    off[31] ^= 1;
    let off = b64url_encode(&off);
    let short = b64url_encode(&b64url_decode(x).unwrap()[1..]);
    let p384 = fenec_core::json::to_string(&all[3]);
    let ed = fenec_core::json::to_string(&all[4]);
    let ec = |x: &str, y: &str, extra: &str| {
        format!(r#"{{"keys":[{{"kty":"EC","crv":"P-256","x":"{x}","y":"{y}"{extra}}}]}}"#)
    };
    for (name, file, why) in [
        ("off", ec(x, &off, ""), "must be a point of P-256"),
        ("short", ec(&short, y, ""), "must be a point of P-256"),
        (
            "none",
            format!(r#"{{"keys":[{p384},{ed}]}}"#),
            "no HS256, RS256 or ES256 key",
        ),
        (
            "es384",
            ec(x, y, r#","alg":"ES384""#),
            "no HS256, RS256 or ES256 key",
        ),
    ] {
        let keys = Keys::write(name, &file);
        let err = Access::from_jwks(&keys.0, "").err().expect(name);
        assert!(err.contains(why), "{name}: {err}");
    }
    let keys = Keys::write("p256-alone", &ec(x, y, r#","kid":"e1""#));
    let a = Access::from_jwks(&keys.0, "").unwrap();
    assert!(a.scope(&token("alice"), now).is_ok(), "no alg, no use");
}

/// A server keeping the fixture's keys: each user's ES256 token reads
/// that user's rows over HTTP, and the same token changed anywhere is
/// refused 401.
#[test]
fn a_server_takes_es256_tokens_and_refuses_them_changed() {
    let (jwks, token) = fixture();
    let keys = Keys::write("server", jwks);
    // tests/audit.rs holds the wait after a refusal; here it would only
    // slow the refusals down.
    fenec_http::audit::set_delay(0);
    let access = Access::from_jwks(&keys.0, "notes read where owner = $jwt.sub").unwrap();
    let mut db = Database::new();
    for sql in [
        "create collection notes (owner text, title text)",
        r#"put notes [{owner: "alice", title: "a1"}, {owner: "bob", title: "b1"},
           {owner: "carol", title: "c1"}]"#,
    ] {
        db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
    }
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        token: Some("root".into()),
        access: Some(Arc::new(access)),
        ..Config::default()
    };
    let server = Server::new(Arc::new(RwLock::new(db)), cfg);
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    let get = |token: &str| -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let body = r#"{"query":"get notes"}"#;
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
    };
    for (name, title) in [("alice", "a1"), ("bob", "b1"), ("carol", "c1")] {
        let (status, body) = get(&token(name));
        assert_eq!(status, 200, "{name}: {body}");
        assert!(body.contains(title), "{name}: {body}");
        assert_eq!(body.matches("\"title\"").count(), 1, "{name}: {body}");
    }

    let alice = token("alice");
    let (head, rest) = alice.split_once('.').unwrap();
    let (body, sig) = rest.split_once('.').unwrap();
    let raw = b64url_decode(sig).unwrap();
    let flipped = |at: usize| {
        let mut s = raw.clone();
        s[at] ^= 0x80;
        format!("{head}.{body}.{}", b64url_encode(&s))
    };
    let bob_body = b64url_encode(br#"{"sub":"bob","exp":4102444800}"#);
    let rs_head = b64url_encode(br#"{"alg":"RS256","typ":"JWT","kid":"e1"}"#);
    let hs_head = b64url_encode(br#"{"alg":"HS256","typ":"JWT","kid":"e1"}"#);
    let none_head = b64url_encode(br#"{"alg":"none"}"#);
    let mut padded = raw.clone();
    padded.insert(0, 0);
    let tampered = [
        ("the claims", format!("{head}.{bob_body}.{sig}")),
        ("R's first bit", flipped(0)),
        ("R's last byte", flipped(31)),
        ("S's first bit", flipped(32)),
        ("S's last byte", flipped(63)),
        (
            "63 bytes",
            format!("{head}.{body}.{}", b64url_encode(&raw[..63])),
        ),
        (
            "65 bytes",
            format!("{head}.{body}.{}", b64url_encode(&padded)),
        ),
        ("RS256 named", format!("{rs_head}.{body}.{sig}")),
        ("HS256 named", format!("{hs_head}.{body}.{sig}")),
        ("alg none", format!("{none_head}.{body}.")),
        (
            "R = S = 0",
            format!("{head}.{body}.{}", b64url_encode(&[0; 64])),
        ),
        ("no signature", format!("{head}.{body}.")),
        ("signed elsewhere", token("other-kid")),
        ("DER", token("der")),
        ("ES384", token("es384")),
        ("EdDSA", token("eddsa")),
    ];
    for (what, t) in tampered {
        let (status, body) = get(&t);
        assert_eq!(status, 401, "{what}: {body}");
        assert!(!body.contains("a1"), "{what}: {body}");
    }
}
