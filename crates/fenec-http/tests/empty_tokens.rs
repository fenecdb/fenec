//! An empty token configured is no token, whoever configures it -- the
//! binaries refuse one given as a flag and take one left empty in the
//! environment for none, and an embedder's `Config` or `Replication` is
//! held to the same. Every check reads a request carrying no
//! `Authorization` as the empty token: an empty admin token let anyone
//! into `/_admin/` (create, delete, every tenant's image), an empty
//! replication token anyone onto every tenant's stream, and an empty data
//! token passed the check that keeps a server off a public address.

use fenec_core::engine::Database;
use fenec_http::replication::Replication;
use fenec_http::tenants::{Replicated, Tenants};
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fenec-empty-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn serve(server: Server) -> u16 {
    fenec_http::audit::set_delay(0);
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    port
}

/// The status of a request carrying no `Authorization`.
fn status(port: u16, method: &str, target: &str) -> u16 {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    )
    .unwrap();
    let mut head = [0u8; 12];
    s.read_exact(&mut head).unwrap();
    String::from_utf8_lossy(&head[9..12]).parse().unwrap()
}

fn loopback(cfg: Config) -> Config {
    Config {
        addr: "127.0.0.1:0".into(),
        ..cfg
    }
}

#[test]
fn an_empty_admin_token_leaves_admin_off() {
    let tenants = Arc::new(Tenants::new(scratch("admin")).unwrap());
    let port = serve(Server::with_tenants(
        tenants,
        loopback(Config {
            admin_token: Some(String::new()),
            ..Config::default()
        }),
    ));
    // Off, as with no admin token: 404, where it answered 200.
    assert_eq!(status(port, "GET", "/_admin/stats"), 404);
    assert_eq!(status(port, "PUT", "/_admin/tenants/acme"), 404);
}

#[test]
fn an_empty_replication_token_opens_no_stream() {
    // A single database's /_replication.
    let db = Arc::new(RwLock::new(Database::new()));
    let port = serve(
        Server::new(db, loopback(Config::default())).with_replication(Replication::new(
            Some(String::new()),
            None,
            None,
        )),
    );
    assert_eq!(status(port, "GET", "/_replication/status"), 401);

    // A node's: a tenant's stream and the node's stream of every tenant.
    let tenants = Tenants::new(scratch("repl"))
        .unwrap()
        .with_replication(Replicated {
            token: String::new(),
            buffer: 1 << 20,
            upstream: None,
            sync_on_write: true,
        });
    tenants.create("acme").unwrap();
    let port = serve(Server::with_tenants(
        Arc::new(tenants),
        loopback(Config::default()),
    ));
    assert_eq!(status(port, "GET", "/t/acme/_replication?since=0"), 401);
    assert_eq!(status(port, "POST", "/_replication/tenants"), 401);
}

#[test]
fn an_empty_data_token_keeps_a_server_off_a_public_address() {
    // As no token: a non-loopback address is refused without --insecure.
    let db = Arc::new(RwLock::new(Database::new()));
    let server = Server::new(
        db,
        Config {
            addr: "0.0.0.0:0".into(),
            token: Some(String::new()),
            ..Config::default()
        },
    );
    let bound = server.bind();
    assert!(
        bound.is_err(),
        "an empty token listened on {:?}",
        bound.map(|l| l.local_addr())
    );
}
