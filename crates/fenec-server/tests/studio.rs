//! fenec studio (`--studio`): off unless asked for, every answer under its
//! path held to a strict content security policy, its files served as they
//! are in the repository -- and `/_whoami`, which it shows a token by.

use crate::support::{start, tmp};

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self'; \
                   font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; \
                   form-action 'none'; frame-ancestors 'none'";

#[test]
fn the_studio_is_off_unless_asked_for() {
    let s = start(&["--http-token", "root"]);
    let mut http = s.http();
    // No token: still 404, not 401 -- the path is no more than an unknown one.
    for target in ["/_studio/", "/_studio", "/_studio/app.js"] {
        let a = http.ask("GET", target, "");
        assert_eq!(a.status, 404, "{target}: {}", a.body);
        assert!(a.body.contains("--studio"), "{}", a.body);
        assert_eq!(a.header("content-security-policy"), None);
    }
    let a = http.with_token("root").ask("GET", "/_studio/", "");
    assert_eq!(a.status, 404);
}

#[test]
fn the_studio_is_served_with_its_headers() {
    let s = start(&["--studio", "--http-token", "root"]);
    let mut http = s.http();

    // The page, with no token: the token is pasted into it.
    let page = http.ask("GET", "/_studio/", "");
    assert_eq!(page.status, 200, "{}", page.body);
    assert_eq!(
        page.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(page.header("content-security-policy"), Some(CSP));
    assert_eq!(page.header("x-frame-options"), Some("DENY"));
    assert_eq!(page.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(page.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(page.header("cache-control"), Some("no-store"));
    assert_eq!(page.header("etag"), None);
    assert_eq!(page.body, include_str!("../../../studio/index.html"));
    // No inline script for the policy to have to allow.
    assert!(!page.body.contains("<script>") && !page.body.contains("<script type=\"module\">"));

    // `/_studio` is sent to the directory its imports resolve against.
    let bare = http.ask("GET", "/_studio", "");
    assert_eq!(bare.status, 308);
    assert_eq!(bare.header("location"), Some("/_studio/"));

    // Every module the page imports, the client's among them, as it is in
    // the repository, kept and asked about again.
    for (path, file, kind) in [
        (
            "app.js",
            include_str!("../../../studio/app.js"),
            "text/javascript; charset=utf-8",
        ),
        (
            "statements.js",
            include_str!("../../../studio/statements.js"),
            "text/javascript; charset=utf-8",
        ),
        (
            "app.css",
            include_str!("../../../studio/app.css"),
            "text/css; charset=utf-8",
        ),
        (
            "client.js",
            include_str!("../../../web/client.js"),
            "text/javascript; charset=utf-8",
        ),
        (
            "http.js",
            include_str!("../../../web/http.js"),
            "text/javascript; charset=utf-8",
        ),
        (
            "builder.js",
            include_str!("../../../web/builder.js"),
            "text/javascript; charset=utf-8",
        ),
    ] {
        let a = http.ask("GET", &format!("/_studio/{path}"), "");
        assert_eq!(a.status, 200, "{path}");
        assert_eq!(a.header("content-type"), Some(kind), "{path}");
        assert_eq!(a.header("cache-control"), Some("no-cache"), "{path}");
        assert_eq!(a.header("content-security-policy"), Some(CSP), "{path}");
        assert_eq!(a.body, file, "{path}");
        let tag = a.header("etag").expect("an etag").to_string();
        let again = http.ask_with(
            "GET",
            &format!("/_studio/{path}"),
            "",
            &[("If-None-Match", &tag)],
        );
        assert_eq!(again.status, 304, "{path}");
        assert_eq!(again.body, "");
    }
    let font = http.ask("GET", "/_studio/plex-mono-400.woff2", "");
    assert_eq!(
        (font.status, font.header("content-type")),
        (200, Some("font/woff2"))
    );

    // Nothing else is under the path, and nothing is written there.
    assert_eq!(http.ask("GET", "/_studio/../data.fenec", "").status, 404);
    assert_eq!(http.ask("GET", "/_studio/nope.js", "").status, 404);
    assert_eq!(http.ask("POST", "/_studio/app.js", "x").status, 405);
    // The data still asks for the token.
    assert_eq!(http.ask("GET", "/collections", "").status, 401);
}

#[test]
fn the_studio_may_also_reach_one_origin_given() {
    let s = start(&[
        "--studio",
        "--studio-connect",
        "https://router.example:8443",
    ]);
    let a = s.http().ask("GET", "/_studio/", "");
    let csp = a.header("content-security-policy").unwrap();
    assert!(
        csp.contains("connect-src 'self' https://router.example:8443;"),
        "{csp}"
    );

    // An origin is all the policy takes: a directive smuggled in is refused
    // at startup.
    let bad = crate::support::try_start(&[
        "--studio",
        "--studio-connect",
        "https://a.example; script-src *",
    ]);
    assert!(bad.is_err_and(|log| log.contains("--studio-connect takes an origin")));
    let alone = crate::support::try_start(&["--studio-connect", "https://a.example"]);
    assert!(alone.is_err_and(|log| log.contains("add --studio")));
}

#[test]
fn whoami_says_whom_the_server_takes_a_token_for() {
    let dir = tmp("studio", "whoami");
    std::fs::create_dir_all(&dir).unwrap();
    let policy = dir.join("policy.txt");
    std::fs::write(
        &policy,
        "notes read,write where owner = $jwt.sub\nboard read\nboard update(title) for mod\nlog insert\nlog append-only\n",
    )
    .unwrap();
    let secret = "thirty-two bytes and a few more, for HS256";
    let s = start(&[
        "--http-token",
        "root",
        "--jwt-secret",
        secret,
        "--policy",
        policy.to_str().unwrap(),
        "--auth-delay",
        "0",
    ]);
    let mut http = s.http();
    let full = http.ask_with("GET", "/_whoami", "", &[("Authorization", "Bearer root")]);
    assert_eq!(full.status, 200);
    assert_eq!(
        full.body,
        r#"{"node":"single","tenant":null,"read_only":false,"kind":"full"}"#
    );
    assert_eq!(http.ask("GET", "/_whoami", "").status, 401);

    let access = fenec_http::access::Access::new(secret.as_bytes(), "").unwrap();
    let alice = access.mint(r#"{"sub":"alice"}"#).unwrap();
    let a = http.ask_with(
        "GET",
        "/_whoami",
        "",
        &[("Authorization", &format!("Bearer {alice}"))],
    );
    assert_eq!(a.status, 200, "{}", a.body);
    assert_eq!(
        a.body,
        r#"{"node":"single","tenant":null,"read_only":false,"kind":"scoped","sub":"alice","tenants":null,"unbound":false,"append_only":["log"],"rules":[{"collection":"notes","grants":["read","insert","update","delete"],"fields":null,"rows":"owner = $jwt.sub"},{"collection":"board","grants":["read"],"fields":null,"rows":null},{"collection":"log","grants":["insert"],"fields":null,"rows":null}]}"#
    );
    // A rule for a role the token does not hold is not shown to it.
    assert!(!a.body.contains("title"));
    let m = access.mint(r#"{"sub":"bo","role":"mod"}"#).unwrap();
    let b = http.ask_with(
        "GET",
        "/_whoami",
        "",
        &[("Authorization", &format!("Bearer {m}"))],
    );
    assert!(
        b.body.contains(
            r#"{"collection":"board","grants":["update"],"fields":["title"],"rows":null}"#
        ),
        "{}",
        b.body
    );
    let _ = std::fs::remove_dir_all(dir.parent().unwrap());
}

#[test]
fn whoami_on_a_tenant_node_names_the_tenants_a_token_reaches() {
    let dir = tmp("studio-node", "tenants");
    std::fs::create_dir_all(&dir).unwrap();
    let policy = dir.with_extension("policy");
    std::fs::write(&policy, "notes read\n").unwrap();
    let secret = "thirty-two bytes and a few more, for HS256";
    let s = start(&[
        "--dir",
        dir.to_str().unwrap(),
        "--admin-token",
        "admin",
        "--http-token",
        "root",
        "--jwt-secret",
        secret,
        "--policy",
        policy.to_str().unwrap(),
        "--auth-delay",
        "0",
    ]);
    let mut http = s.http();
    for t in ["acme", "globex"] {
        let a = http.ask_with(
            "PUT",
            &format!("/_admin/tenants/{t}"),
            "",
            &[("Authorization", "Bearer admin")],
        );
        assert_eq!(a.status, 201, "{}", a.body);
    }
    let access = fenec_http::access::Access::new(secret.as_bytes(), "").unwrap();
    let alice = access.mint(r#"{"sub":"alice","tenant":"acme"}"#).unwrap();
    let auth = format!("Bearer {alice}");
    let root = http.ask_with("GET", "/_whoami", "", &[("Authorization", &auth)]);
    assert_eq!(root.status, 200, "{}", root.body);
    assert!(root.body.starts_with(r#"{"node":"tenants","tenant":null,"read_only":false,"kind":"scoped","sub":"alice","tenants":["acme"]"#), "{}", root.body);
    let here = http.ask_with("GET", "/t/acme/_whoami", "", &[("Authorization", &auth)]);
    assert!(
        here.body
            .starts_with(r#"{"node":"tenants","tenant":"acme","#),
        "{}",
        here.body
    );
    // Another tenant's is refused as every route under its prefix is.
    assert_eq!(
        http.ask_with("GET", "/t/globex/_whoami", "", &[("Authorization", &auth)])
            .status,
        403
    );
    let _ = std::fs::remove_dir_all(dir.parent().unwrap());
}
