//! The flags that hold a token to whom it was issued for -- `--jwt-issuer`,
//! `--jwt-audience`, `--jwt-client` -- and `--jwt-role-claim`, on the binary
//! as a deployment runs it: a provider's token for this server taken, one
//! for another app refused with 401 naming the check, a role read from the
//! provider's own claim, a policy quoting a claim's name, `--mint-token`
//! stamping what the server asks for, and each flag given empty refused at
//! the start.

use crate::support::{start, tmp, try_start, Server};
use fenec_http::crypto::{b64url_decode, b64url_encode, hmac_sha256};

const SECRET: &str = "thirty-two bytes and a few more, for HS256";
const POLICY: &str = r#"notes  read,write  where owner = $jwt.sub
teams  read        where team in $jwt."cognito:groups"
board  read
board  write       for editors
"#;

/// An HS256 token for `claims` as given, an hour's `exp` added.
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
    let sig = hmac_sha256(SECRET.as_bytes(), format!("{head}.{body}").as_bytes());
    format!("{head}.{body}.{}", b64url_encode(&sig))
}

/// A server over a file of notes, teams and a board, keyed with the
/// secret, the policy above and `flags`.
fn serve(tag: &str, flags: &[&str]) -> Server {
    let dir = tmp("providers", tag);
    std::fs::create_dir_all(&dir).unwrap();
    let policy = dir.join("policy.txt");
    std::fs::write(&policy, POLICY).unwrap();
    let file = dir.join("data.fenec");
    let mut args = vec![
        "--file",
        file.to_str().unwrap(),
        "--http-token",
        "root",
        "--jwt-secret",
        SECRET,
        "--policy",
        policy.to_str().unwrap(),
        "--auth-delay",
        "0",
    ];
    args.extend_from_slice(flags);
    let s = start(&args);
    let mut root = s.http().with_token("root");
    for q in [
        "create collection notes (owner text @hash, title text)",
        "create collection teams (team text, name text)",
        "create collection board (msg text)",
        r#"put notes [{owner: "alice", title: "a1"}, {owner: "bob", title: "b1"}]"#,
        r#"put teams [{team: "t1", name: "one"}, {team: "t2", name: "two"}]"#,
    ] {
        root.run(q);
    }
    s
}

fn ask(s: &Server, token: &str, q: &str) -> (u16, String) {
    match s.http().with_token(token).query(q) {
        Ok(body) => (200, body),
        Err(refused) => refused,
    }
}

#[test]
fn a_token_is_held_to_the_issuer_audience_and_role_claim_given() {
    let s = serve(
        "told",
        &[
            "--jwt-issuer",
            "https://idp.example/",
            "--jwt-issuer",
            "https://cognito-idp.eu-west-1.amazonaws.com/eu-west-1_Pool",
            "--jwt-audience",
            "notes-api",
            "--jwt-role-claim",
            "cognito:groups",
        ],
    );
    // For this server: either issuer, `aud` a text or a list holding it.
    for claims in [
        r#"{"sub":"alice","iss":"https://idp.example/","aud":"notes-api"}"#,
        r#"{"sub":"alice","iss":"https://cognito-idp.eu-west-1.amazonaws.com/eu-west-1_Pool","aud":["x","notes-api"]}"#,
    ] {
        let (status, body) = ask(&s, &token(claims), "get notes");
        assert_eq!(status, 200, "{claims}: {body}");
        assert!(body.contains("a1") && !body.contains("b1"), "{body}");
    }
    // For another: refused, the check named, the value not.
    for (claims, check, value) in [
        (
            r#"{"sub":"alice","iss":"https://other.example/","aud":"notes-api"}"#,
            "issuer (iss)",
            "other.example",
        ),
        (
            r#"{"sub":"alice","aud":"notes-api"}"#,
            "issuer (iss)",
            "alice",
        ),
        (
            r#"{"sub":"alice","iss":"https://idp.example/","aud":["billing"]}"#,
            "audience (aud)",
            "billing",
        ),
        (
            r#"{"sub":"alice","iss":"https://idp.example/"}"#,
            "audience (aud)",
            "alice",
        ),
    ] {
        let (status, body) = ask(&s, &token(claims), "get notes");
        assert_eq!(status, 401, "{claims}: {body}");
        assert!(
            body.contains(check) && !body.contains(value),
            "{claims}: {body}"
        );
        // `/_whoami`, which the studio shows a token by, alike.
        let who = s
            .http()
            .with_token(&token(claims))
            .ask("GET", "/_whoami", "");
        assert_eq!(who.status, 401, "{}", who.body);
    }

    // The role from Cognito's groups, and the groups as a quoted claim.
    let editor = token(
        r#"{"sub":"alice","iss":"https://idp.example/","aud":"notes-api","cognito:groups":["t1","editors"]}"#,
    );
    let (status, body) = ask(&s, &editor, r#"put board {msg: "hi"}"#);
    assert_eq!(status, 200, "{body}");
    let (status, rows) = ask(&s, &editor, "get teams");
    assert_eq!(status, 200, "{rows}");
    assert!(rows.contains("one") && !rows.contains("two"), "{rows}");
    // `role` names nothing on this server.
    let role = token(
        r#"{"sub":"alice","iss":"https://idp.example/","aud":"notes-api","role":"editors","cognito:groups":["t2"]}"#,
    );
    let (status, body) = ask(&s, &role, r#"put board {msg: "no"}"#);
    assert_eq!(status, 403, "{body}");
}

#[test]
fn without_the_flags_a_token_is_taken_as_it_was() {
    let s = serve("open", &[]);
    for claims in [
        r#"{"sub":"alice"}"#,
        r#"{"sub":"alice","iss":"https://other.example/","aud":"billing"}"#,
    ] {
        let (status, body) = ask(&s, &token(claims), "get notes");
        assert_eq!(status, 200, "{claims}: {body}");
        assert!(body.contains("a1") && !body.contains("b1"), "{body}");
    }
    let role = token(r#"{"sub":"alice","role":["x","editors"]}"#);
    assert_eq!(ask(&s, &role, r#"put board {msg: "hi"}"#).0, 200);
    let groups = token(r#"{"sub":"alice","cognito:groups":"editors"}"#);
    assert_eq!(ask(&s, &groups, r#"put board {msg: "no"}"#).0, 403);
}

/// `--mint-token` on a server told an issuer, an audience and a client
/// stamps the first of each the claims leave out: the server takes the
/// token it minted.
#[test]
fn a_minted_token_names_what_the_server_asks_for() {
    let flags = [
        "--jwt-issuer",
        "https://idp.example/",
        "--jwt-audience",
        "notes-api",
        "--jwt-client",
        "app-1",
    ];
    let mut args = vec!["--jwt-secret", SECRET, "--mint-token", r#"{"sub":"alice"}"#];
    args.extend_from_slice(&flags);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fenec-server"))
        .args(&args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let minted = String::from_utf8(out.stdout).unwrap().trim().to_string();
    let body = minted.split('.').nth(1).unwrap();
    let claims = String::from_utf8(b64url_decode(body).unwrap()).unwrap();
    for c in [
        r#""iss":"https://idp.example/""#,
        r#""aud":"notes-api""#,
        r#""azp":"app-1""#,
        r#""exp":"#,
    ] {
        assert!(claims.contains(c), "{c}: {claims}");
    }
    let s = serve("minted", &flags);
    let (status, body) = ask(&s, &minted, "get notes");
    assert_eq!(status, 200, "{body}");
}

#[test]
fn a_flag_given_empty_or_a_nested_claim_is_refused_at_the_start() {
    let dir = tmp("providers", "refused");
    std::fs::create_dir_all(&dir).unwrap();
    let policy = dir.join("policy.txt");
    std::fs::write(&policy, POLICY).unwrap();
    let policy = policy.to_str().unwrap();
    let keyed = ["--jwt-secret", SECRET, "--policy", policy];
    // `--jwt-audience "$AUD"` with AUD unset: a server told "" would take
    // no token, one told nothing every one; a tenant claim named "" is
    // held by no token, which --jwt-unbound-tenants took for every tenant.
    for flag in [
        "--jwt-issuer",
        "--jwt-audience",
        "--jwt-client",
        "--jwt-role-claim",
        "--jwt-tenant-claim",
    ] {
        let mut args = keyed.to_vec();
        args.extend([flag, ""]);
        match try_start(&args) {
            Ok(s) => panic!("an empty {flag} was taken, the server on port {}", s.port),
            Err(log) => assert!(log.contains(&format!("{flag} is empty")), "{flag}: {log}"),
        }
    }
    // A path into a nested object: what a user may edit of themselves sits
    // a word from what only the app may.
    let nested = dir.join("nested.txt");
    std::fs::write(
        &nested,
        "notes read where owner = $jwt.app_metadata.owner\n",
    )
    .unwrap();
    let args = ["--jwt-secret", SECRET, "--policy", nested.to_str().unwrap()];
    match try_start(&args) {
        Ok(s) => panic!("a nested claim was taken, the server on port {}", s.port),
        Err(log) => assert!(log.contains("top-level claims alone"), "{log}"),
    }
}

/// A flag holding a JWT to something, given where no key verifies one, is
/// refused: taken and ignored, a server told `--jwt-audience` and no keys
/// looked held to it.
#[test]
fn a_jwt_flag_without_keys_is_refused_at_the_start() {
    let file = tmp("providers", "unkeyed.fenec");
    let file = file.to_str().unwrap();
    for flags in [
        &["--jwt-audience", "notes-api"][..],
        &["--jwt-issuer", "https://idp.example/"],
        &["--jwt-require-exp", "off"],
        &["--jwt-unbound-tenants"],
        &["--jwt-role-claim", "groups"],
    ] {
        let mut args = vec!["--file", file];
        args.extend_from_slice(flags);
        match try_start(&args) {
            Ok(s) => panic!("{flags:?} was taken with no keys, port {}", s.port),
            Err(log) => assert!(
                log.contains(&format!("{} holds JSON Web Tokens", flags[0]))
                    && log.contains("no --jwt-secret or --jwt-keys"),
                "{flags:?}: {log}"
            ),
        }
    }
}

/// `--jwt-keys` with no `--jwt-issuer` says at the start that it takes any
/// token the keys sign: Firebase's keys sign every project's.
#[test]
fn keys_with_no_issuer_are_warned_of() {
    let dir = tmp("providers", "keys");
    std::fs::create_dir_all(&dir).unwrap();
    let keys = dir.join("jwks.json");
    let k = b64url_encode(SECRET.as_bytes());
    std::fs::write(&keys, format!(r#"{{"keys":[{{"kty":"oct","k":"{k}"}}]}}"#)).unwrap();
    let policy = dir.join("policy.txt");
    std::fs::write(&policy, POLICY).unwrap();
    let base = [
        "--jwt-keys",
        keys.to_str().unwrap(),
        "--policy",
        policy.to_str().unwrap(),
    ];
    let warned = "warning: --jwt-keys without --jwt-issuer";
    let s = start(&base);
    let log = s.log.lock().unwrap().clone();
    assert!(log.contains(warned), "{log}");
    drop(s);
    let mut args = base.to_vec();
    args.extend(["--jwt-issuer", "https://idp.example/"]);
    let s = start(&args);
    let log = s.log.lock().unwrap().clone();
    assert!(!log.contains(warned), "{log}");
}

/// The audit line of a token refused for whom it was issued names the
/// check, never the token's value.
#[test]
fn a_refusal_for_another_audience_is_audited_by_its_check() {
    let dir = tmp("providers", "audited");
    std::fs::create_dir_all(&dir).unwrap();
    let audit = dir.join("audit.log");
    let s = serve(
        "audited-server",
        &[
            "--jwt-issuer",
            "https://idp.example/",
            "--jwt-audience",
            "notes-api",
            "--audit",
            audit.to_str().unwrap(),
        ],
    );
    let other = token(r#"{"sub":"alice","iss":"https://idp.example/","aud":"billing-api"}"#);
    assert_eq!(ask(&s, &other, "get notes").0, 401);
    // The line is written before the answer, which waits on nothing.
    let text = std::fs::read_to_string(&audit).unwrap();
    let line = text
        .lines()
        .find(|l| l.contains(r#""event":"refused""#))
        .unwrap_or_else(|| panic!("no refusal: {text}"));
    assert!(
        line.contains(r#""reason":"the token names no audience (aud) this server takes""#),
        "{line}"
    );
    assert!(
        !line.contains("billing") && !line.contains("alice"),
        "{line}"
    );
}

/// Under `--jwt-max-age` below an hour, `--mint-token` stamps that age: an
/// hour's was a token the same server refused.
#[test]
fn a_minted_token_lives_no_longer_than_the_max_age() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fenec-server"))
        .args([
            "--jwt-secret",
            SECRET,
            "--jwt-max-age",
            "600",
            "--mint-token",
            r#"{"sub":"alice"}"#,
        ])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    let minted = String::from_utf8(out.stdout).unwrap().trim().to_string();
    let s = serve("max-age", &["--jwt-max-age", "600"]);
    let (status, body) = ask(&s, &minted, "get notes");
    assert_eq!(status, 200, "{body}");
}
