//! The tokens a deployment keeps off the command line -- `ps` shows every
//! process's arguments to every user of the machine -- read from the
//! environment, as deploy/'s systemd units hand them over
//! (`EnvironmentFile=`): the admin token from FENEC_ADMIN_TOKEN. A variable
//! set and empty, as an environment file leaves a line it does not use, is
//! no token; an empty one given as a flag is refused at the start, since
//! `/_admin/` reads a request with no `Authorization` as an empty token.

use crate::support::{start_env, tmp, try_start, Http};

#[test]
fn the_admin_token_is_read_from_the_environment() {
    let dir = tmp("tokens", "node");
    let server = start_env(
        &["--dir", dir.to_str().unwrap(), "--auth-delay", "0"],
        &[("FENEC_ADMIN_TOKEN", "adm-from-env")],
    );
    assert_eq!(
        Http::open(server.port)
            .ask("GET", "/_admin/stats", "")
            .status,
        401
    );
    let mut admin = Http::open(server.port).with_token("adm-from-env");
    assert_eq!(admin.ask("GET", "/_admin/stats", "").status, 200);
    let made = admin.ask("PUT", "/_admin/tenants/acme", "");
    assert!(made.status / 100 == 2, "{} {}", made.status, made.body);
    assert!(
        admin
            .ask("GET", "/_admin/tenants", "")
            .body
            .contains("acme"),
        "the tenant the admin token made"
    );
}

#[test]
fn an_admin_token_set_empty_is_none() {
    // From the environment: /_admin/ is off, as with no token, never open.
    let dir = tmp("tokens", "empty-env");
    let server = start_env(
        &["--dir", dir.to_str().unwrap(), "--auth-delay", "0"],
        &[("FENEC_ADMIN_TOKEN", "")],
    );
    let off = Http::open(server.port).ask("GET", "/_admin/stats", "");
    assert_eq!(off.status, 404, "{}", off.body);
    drop(server);

    // Given as a flag: refused before anything listens.
    let dir = tmp("tokens", "empty-flag");
    let log = match try_start(&["--dir", dir.to_str().unwrap(), "--admin-token", ""]) {
        Ok(server) => panic!(
            "an empty --admin-token was taken, /_admin/ open on port {}",
            server.port
        ),
        Err(log) => log,
    };
    assert!(log.contains("--admin-token is empty"), "{log}");
}

#[test]
fn every_token_given_empty_as_a_flag_is_refused() {
    // `--http-token "$TOKEN"` with TOKEN unset: an empty data token passed
    // the check that keeps a server off a public address and answered
    // nothing, an empty replication token turned replication off unsaid.
    let file = tmp("tokens", "flags.fenec");
    let file = file.to_str().unwrap();
    for flag in [
        "--http-token",
        "--replication-token",
        "--jwt-secret",
        "--jwt-keys",
    ] {
        let log = match try_start(&["--file", file, flag, ""]) {
            Ok(server) => panic!(
                "an empty {flag} was taken, the server on port {}",
                server.port
            ),
            Err(log) => log,
        };
        assert!(log.contains(&format!("{flag} is empty")), "{flag}: {log}");
    }
    // A secret's file holding nothing, likewise.
    let secret = tmp("tokens", "jwt.secret");
    std::fs::write(&secret, "\n").unwrap();
    let log = match try_start(&[
        "--file",
        file,
        "--jwt-secret-file",
        secret.to_str().unwrap(),
    ]) {
        Ok(server) => panic!("an empty secret's file was taken, port {}", server.port),
        Err(log) => log,
    };
    assert!(log.contains("is empty"), "{log}");
}

#[test]
fn a_secret_set_empty_in_the_environment_is_none() {
    // Left empty, FENEC_JWT_SECRET and FENEC_JWT_KEYS were taken for keys,
    // and the server refused to start for want of a --policy; unset, it
    // starts with no keys, no data token and no replication.
    let file = tmp("tokens", "env.fenec");
    let server = start_env(
        &["--file", file.to_str().unwrap()],
        &[
            ("FENEC_JWT_SECRET", ""),
            ("FENEC_JWT_KEYS", ""),
            ("FENEC_REPLICATION_TOKEN", ""),
            ("FENEC_HTTP_TOKEN", ""),
        ],
    );
    let status = Http::open(server.port)
        .ask("GET", "/_replication/status", "")
        .status;
    assert_ne!(status, 200, "replication on with an empty token");
}
