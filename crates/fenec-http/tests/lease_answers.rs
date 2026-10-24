//! A tenant's answer asked of its lease as it is ready, over HTTP on one
//! keep-alive connection: what a write whose lease lapsed before its answer
//! is told, and that a request is asked of its own writes alone.

use fenec_core::value::Value;
use fenec_http::lease::Lease;
use fenec_http::tenants::Tenants;
use fenec_http::{Config, Server};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

/// `lapse()`: the lease runs out as a statement works it out -- the race a
/// stop, a suspend or a slow durability makes, made certain.
struct Lapse(Arc<Lease>);

impl fenec_core::plugin::ScalarFn for Lapse {
    fn call(&self, _: &[Value]) -> fenec_core::error::Result<Value> {
        self.0.grant(0, "e1", None).ok();
        Ok(Value::Int(1))
    }
}

struct Node {
    port: u16,
    lease: Arc<Lease>,
    dir: std::path::PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Node {
    fn start(tag: &str) -> Node {
        let dir =
            std::env::temp_dir().join(format!("fenec-lease-answers-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tenants = Tenants::new(&dir).unwrap().with_lease();
        let lease = Arc::clone(tenants.lease().unwrap());
        let held = Arc::clone(&lease);
        let tenants = Arc::new(tenants.with_setup(move |db| {
            db.registry_mut()
                .register_fn("lapse", Arc::new(Lapse(Arc::clone(&held))))
        }));
        let mut node = Node {
            port: 0,
            lease,
            dir,
        };
        node.grant();
        tenants.create("acme").unwrap();
        let cfg = Config {
            addr: "127.0.0.1:0".into(),
            admin_token: Some("adm".into()),
            ..Config::default()
        };
        let server = Server::with_tenants(tenants, cfg);
        let listener = server.bind().unwrap();
        node.port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let _ = server.serve_on(listener);
        });
        node
    }

    fn grant(&self) {
        assert!(self
            .lease
            .grant(60_000, "e1", Some(vec!["acme".into()]))
            .is_ok());
    }
}

/// One keep-alive connection, requests one after another, as a router's
/// pooled connection carries them.
struct Conn {
    reader: BufReader<TcpStream>,
    out: TcpStream,
}

struct Answer {
    status: u16,
    headers: Vec<String>,
    body: String,
}

impl Conn {
    fn open(port: u16) -> Conn {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        Conn {
            out: s.try_clone().unwrap(),
            reader: BufReader::new(s),
        }
    }

    fn send(&mut self, method: &str, target: &str, admin: bool, body: &str) -> Answer {
        let auth = match admin {
            true => "Authorization: Bearer adm\r\n",
            false => "",
        };
        let head = format!(
            "{method} {target} HTTP/1.1\r\nHost: n\r\n{auth}Content-Length: {}\r\n\r\n",
            body.len()
        );
        self.out.write_all(head.as_bytes()).unwrap();
        self.out.write_all(body.as_bytes()).unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        let status = line.split_whitespace().nth(1).unwrap().parse().unwrap();
        let mut headers = Vec::new();
        let mut len = 0;
        loop {
            let mut h = String::new();
            self.reader.read_line(&mut h).unwrap();
            let h = h.trim_end().to_string();
            if h.is_empty() {
                break;
            }
            if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
            headers.push(h);
        }
        let mut body = vec![0u8; len];
        self.reader.read_exact(&mut body).unwrap();
        Answer {
            status,
            headers,
            body: String::from_utf8(body).unwrap(),
        }
    }

    fn query(&mut self, q: &str) -> Answer {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, q);
        body.push('}');
        self.send("POST", "/t/acme/query", false, &body)
    }

    fn batch(&mut self, lines: &[&str]) -> Answer {
        let lines: Vec<String> = lines
            .iter()
            .map(|q| {
                let mut line = String::from("{\"query\":");
                fenec_core::json::escape_into(&mut line, q);
                line.push('}');
                line
            })
            .collect();
        self.send("POST", "/t/acme/batch", false, &lines.join("\n"))
    }
}

/// A `/batch` holding a compact runs each statement on its own: a put
/// lands, the read after it lapses the lease, and the put after that is
/// refused by the fence. Told to come back (503, `Retry-After`) as if
/// nothing had landed, and with no `completed` or `at`, a client sent the
/// batch again; it is answered as a write whose outcome is unknown, where
/// it got to with it, and where the write left the database in the body --
/// not in `Fenec-Seq`, which a client takes as its own position.
#[test]
fn a_batch_refused_after_a_write_landed_says_the_outcome_is_unknown() {
    let n = Node::start("batch");
    let mut c = Conn::open(n.port);
    assert_eq!(c.query("create collection notes (title text)").status, 200);
    let late = c.batch(&[
        "compact notes",
        "put notes {title: \"b\"}",
        "get notes select lapse() as l limit 1",
        "put notes {title: \"c\"}",
    ]);
    assert_eq!(late.status, 500, "{}", late.body);
    for part in [
        r#""outcome":"unknown""#,
        r#""completed":3"#,
        r#""at":3"#,
        r#""seq":"#,
        "a lease and a tenth",
    ] {
        assert!(late.body.contains(part), "{part} in {}", late.body);
    }
    assert!(
        !late
            .headers
            .iter()
            .any(|h| h.to_ascii_lowercase().starts_with("fenec-seq")),
        "{:?}",
        late.headers
    );
    // b is in the file, c is not.
    n.grant();
    let b = c.query("get notes where title = \"b\" count");
    assert!(b.body.contains(r#""count":1"#), "{}", b.body);
    let none = c.query("get notes where title = \"c\" count");
    assert!(none.body.contains(r#""count":0"#), "{}", none.body);
}

/// A request is asked of its own writes alone: the schema a router applies
/// through the admin route is answered on a path that counts no statement,
/// which cleared what a request wrote, so the read after it on the same
/// connection -- a router's pooled one -- was taken for a write once its
/// lease lapsed under it, and answered 500 as one that landed.
#[test]
fn a_read_after_an_admin_schema_on_one_connection_is_a_read() {
    let n = Node::start("schema");
    let mut c = Conn::open(n.port);
    assert_eq!(c.query("create collection notes (title text)").status, 200);
    assert_eq!(c.query("put notes {title: \"a\"}").status, 200);
    let schema = c.send(
        "POST",
        "/_admin/tenants/acme/schema",
        true,
        r#"{"format":1,"fenecql":"create collection other (x int)"}"#,
    );
    assert_eq!(schema.status, 200, "{}", schema.body);
    let read = c.query("get notes select lapse() as l limit 1");
    assert_eq!(read.status, 200, "{}", read.body);
}
