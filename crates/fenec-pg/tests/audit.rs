//! Logins over the pg wire in the audit log, and the wait after a failed
//! one. Apart from the other tests: the log and the count of failures are
//! the process's own.

use fenec_core::prelude::Database;
use fenec_pg::client::{Client, Url};
use fenec_pg::server::Auth;
use fenec_pg::{Config, PgPlugin, Server};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

fn url(port: u16, password: &str) -> Url {
    Url {
        user: "fenec".into(),
        password: Some(password.into()),
        host: "127.0.0.1".into(),
        port,
        database: "fenec".into(),
    }
}

#[test]
fn a_failed_login_waits_and_logins_are_logged() {
    let dir = std::env::temp_dir().join(format!("fenec-pg-audit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("audit.log");
    fenec_http::audit::open(&log).unwrap();
    fenec_http::audit::set_delay(50);

    let mut db = Database::new();
    db.install_plugin(&PgPlugin).unwrap();
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        auth: Auth::parse("scram", "right-password").unwrap(),
        ..Config::default()
    };
    let server = Server::new(Arc::new(RwLock::new(db)), cfg);
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });

    for least in [50, 100] {
        let t = Instant::now();
        let e = Client::connect(&url(port, "wrong"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("28P01") || e.contains("password"), "{e}");
        assert!(
            t.elapsed() >= Duration::from_millis(least),
            "{:?}",
            t.elapsed()
        );
    }
    // The right password waits for nothing and starts the count again: the
    // next failure from the address waits the first wait. (A clock bound on
    // the login itself fails on a slow runner, SCRAM being slow unoptimised.)
    let mut c = Client::connect(&url(port, "right-password")).unwrap();
    let local = Some("127.0.0.1".parse().unwrap());
    assert_eq!(fenec_http::audit::failed(local), Duration::from_millis(50));
    c.query("create collection notes (title text)").unwrap();
    c.query(r#"put notes {title: "not in the log"}"#).unwrap();
    drop(c);

    let text = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
    for (l, event) in lines
        .iter()
        .zip(["login_failed", "login_failed", "login", "schema"])
    {
        assert!(l.contains(&format!(r#""event":"{event}""#)), "{l}");
        assert!(
            l.contains(r#""proto":"pg""#) && l.contains(r#""peer":"127.0.0.1:"#),
            "{l}"
        );
    }
    assert!(lines[0].contains(r#""as":"fenec""#) && lines[0].contains(r#""reason":"#));
    assert!(lines[2].contains(r#""user":"fenec""#) && lines[2].contains(r#""database":"fenec""#));
    assert!(lines[3].contains(r#""user":"fenec""#), "{}", lines[3]);
    assert!(lines[3].contains(r#""statement":"create collection notes (title text)""#));
    assert!(!text.contains("not in the log"));
    std::fs::remove_dir_all(&dir).unwrap();
}
