//! What checking a JSON Web Token costs, by algorithm: `make jwt-bench`
//! (`cargo run --release -p fenec-http --example jwt -- [ROUNDS]`).
//!
//! For HS256, RS256 (2048 bits) and ES256 (P-256): the signature's check
//! alone; a token's first `Access::scope` -- its parse, the check, the
//! claims bound to the policy -- on a server that has not seen it; and a
//! later one, the token found among those verified (`Access::verified`).
//! The RS256 and ES256 tokens are node:crypto's (tests/es256/tokens.txt).
//! The algorithms take turns in each round, which goes first changing
//! every round, so a machine slowing down slows each alike; each figure is
//! the median of its rounds.

use fenec_core::prelude::*;
use fenec_http::access::Access;
use fenec_http::crypto::{b64url_decode, P256Key, RsaKey};
use std::time::Instant;

const SECRET: &[u8] = b"a secret of more than thirty-two bytes, for HS256";
const POLICY: &str = "notes read where owner = $jwt.sub";
/// After every token's `exp`, before 2100.
const NOW: u64 = 1_800_000_000;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

struct Side {
    name: &'static str,
    token: String,
    /// A server's keys and policy, read again for each first check.
    fresh: Box<dyn Fn() -> Access>,
    /// The signature's check alone.
    check: Box<dyn Fn() -> bool>,
}

fn main() {
    let rounds: usize = std::env::args()
        .nth(1)
        .map_or(200, |a| a.parse().expect("ROUNDS"));
    let fixture = include_str!("../tests/es256/tokens.txt");
    let mut lines = fixture.lines();
    let jwks = lines.next().unwrap();
    let token = |name: &str| {
        fixture
            .lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(' '))
            .unwrap()
            .to_string()
    };
    let path = std::env::temp_dir().join(format!("fenec-jwt-bench-{}.json", std::process::id()));
    std::fs::write(&path, jwks).unwrap();

    let keys = match fenec_core::json::parse_json(jwks).unwrap() {
        Value::Object(o) => o,
        _ => panic!("a JWKS"),
    };
    let Some((_, Value::List(keys))) = keys.into_iter().find(|(k, _)| k == "keys") else {
        panic!("keys")
    };
    let member = |i: usize, name: &str| match &keys[i] {
        Value::Object(o) => b64url_decode(
            o.iter()
                .find(|(k, _)| k == name)
                .unwrap()
                .1
                .as_text()
                .unwrap(),
        )
        .unwrap(),
        _ => panic!("a key"),
    };
    let ec = P256Key::new(&member(0, "x"), &member(0, "y")).unwrap();
    let rsa = RsaKey::new(&member(2, "n"), &member(2, "e")).unwrap();
    let split = |t: &str| {
        let (signed, sig) = t.rsplit_once('.').unwrap();
        (signed.to_string(), b64url_decode(sig).unwrap())
    };
    let (es_signed, es_sig) = split(&token("alice"));
    let (rs_signed, rs_sig) = split(&token("carol"));
    let hs = Access::new(SECRET, POLICY).unwrap();
    let hs_token = hs.mint(r#"{"sub":"alice","exp":4102444800}"#).unwrap();
    let (hs_signed, hs_sig) = split(&hs_token);

    let jwks_access = {
        let path = path.clone();
        move || Access::from_jwks(&path, POLICY).unwrap()
    };
    let sides = [
        Side {
            name: "HS256",
            token: hs_token.clone(),
            fresh: Box::new(|| Access::new(SECRET, POLICY).unwrap()),
            check: Box::new(move || {
                fenec_http::crypto::ct_eq(
                    &hs_sig,
                    &fenec_http::crypto::hmac_sha256(SECRET, hs_signed.as_bytes()),
                )
            }),
        },
        Side {
            name: "RS256",
            token: token("carol"),
            fresh: Box::new(jwks_access.clone()),
            check: Box::new(move || rsa.verify_sha256(rs_signed.as_bytes(), &rs_sig)),
        },
        Side {
            name: "ES256",
            token: token("alice"),
            fresh: Box::new(jwks_access),
            check: Box::new(move || ec.verify_sha256(es_signed.as_bytes(), &es_sig)),
        },
    ];

    // Per side: the check alone, the first scope, a kept one -- us.
    let mut took: Vec<[Vec<f64>; 3]> = sides.iter().map(|_| Default::default()).collect();
    for round in 0..rounds {
        for i in 0..sides.len() {
            let at = (i + round) % sides.len();
            let side = &sides[at];
            const CHECKS: u32 = 20;
            let t = Instant::now();
            for _ in 0..CHECKS {
                assert!((side.check)());
            }
            took[at][0].push(t.elapsed().as_secs_f64() * 1e6 / CHECKS as f64);
            let access = (side.fresh)();
            // Its first look at the JWKS file, which a server takes once a
            // second rather than once a token, before the clock starts.
            assert!(access.scope("not a token", NOW).is_err());
            let t = Instant::now();
            assert!(access.scope(&side.token, NOW).is_ok());
            took[at][1].push(t.elapsed().as_secs_f64() * 1e6);
            const KEPT: u32 = 1000;
            let t = Instant::now();
            for _ in 0..KEPT {
                assert!(access.scope(&side.token, NOW).is_ok());
            }
            took[at][2].push(t.elapsed().as_secs_f64() * 1e6 / KEPT as f64);
        }
    }
    let _ = std::fs::remove_file(&path);
    println!("{rounds} rounds, median us a token");
    println!(
        "{:<8}{:>12}{:>14}{:>14}",
        "", "signature", "first scope", "kept scope"
    );
    for (side, t) in sides.iter().zip(took) {
        let [check, first, kept] = t.map(median);
        println!("{:<8}{check:>12.2}{first:>14.2}{kept:>14.2}", side.name);
    }
}
