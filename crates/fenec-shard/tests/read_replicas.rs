//! Read replicas: a follower of a tenant's primary in another region, which
//! a router near that region answers the reads its clients mark from
//! (`Fenec-Read: replica`), every other request going home.
//!
//! What must hold: a marked read near a replica is answered there -- shown
//! by what only the replica could answer: the rows it last applied while
//! home holds newer, and an answer while home cannot be reached -- and is
//! never older than the client's own write it names (`Fenec-After`), going
//! home where the replica cannot catch up; a failover or a move of the
//! tenant's home leaves its read replicas following the new primary and
//! promotes none of them; a replica cut off from home serves its last
//! state and catches up once healed; and in regions keeping their own
//! directory, each region's router places, routes to and re-points the
//! read replicas on its nodes, whichever region the tenant lives in.

use fenec_http::replication::{fresh_id, Follower, Replication};
use fenec_http::tenants::refusals::{self, Stage};
use fenec_http::tenants::{Replicated, Tenants};
use fenec_shard::directory::Directory;
use fenec_shard::{Config, Router};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

const ROOT: &str = "routers-token";
const DIRS: &str = "directory-replication";
const REPL: &str = "tenant-replication";

struct Node {
    port: u16,
    dir: PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn node(tag: &str) -> Node {
    node_leased(tag, false)
}

/// A node that replicates nothing: no tenant of it follows anyone.
fn node_plain(tag: &str) -> Node {
    let dir = std::env::temp_dir().join(format!("fenec-rr-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = fenec_http::Config {
        addr: "127.0.0.1:0".into(),
        admin_token: Some(format!("adm-{tag}")),
        ..fenec_http::Config::default()
    };
    let server = fenec_http::Server::with_tenants(Arc::new(Tenants::new(&dir).unwrap()), cfg);
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node { port, dir }
}

/// A node; `leased`, taking writes only under its router's lease.
fn node_leased(tag: &str, leased: bool) -> Node {
    node_of(tag, leased, None)
}

/// A node whose tenants follow the node at `upstream`: its pair's standby.
fn node_standby(tag: &str, upstream: &str) -> Node {
    node_of(tag, false, Some(upstream.to_string()))
}

fn node_of(tag: &str, leased: bool, upstream: Option<String>) -> Node {
    let dir = std::env::temp_dir().join(format!("fenec-rr-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut tenants = Tenants::new(&dir).unwrap().with_replication(Replicated {
        token: REPL.into(),
        buffer: 8 << 20,
        upstream,
        sync_on_write: true,
    });
    if leased {
        tenants = tenants.with_lease();
    }
    let cfg = fenec_http::Config {
        addr: "127.0.0.1:0".into(),
        admin_token: Some(format!("adm-{tag}")),
        sync_on_write: true,
        ..fenec_http::Config::default()
    };
    let server = fenec_http::Server::with_tenants(Arc::new(tenants), cfg);
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node { port, dir }
}

/// A TCP proxy: cut, it drops what it carries and refuses every connection
/// after, until healed -- a node gone, as its router and its followers see
/// it.
struct Proxy {
    addr: String,
    target: String,
    cut: Arc<AtomicBool>,
    /// Hung, it takes connections and never answers on them: a node whose
    /// process stopped, as its peers see it.
    hung: Arc<AtomicBool>,
    open: Arc<Mutex<Vec<TcpStream>>>,
    /// The request line of each connection it took while hung: what the
    /// node that does not answer was asked.
    asked: Arc<Mutex<Vec<String>>>,
    /// Parking, it reads the first request of each connection it takes,
    /// and holds the ones whose request line holds one of these, not sent
    /// on, until let go ([`Proxy::unpark`]): a call to the node held at a
    /// moment the test chooses, then made.
    parking: Arc<Mutex<Vec<String>>>,
    /// The requests it holds: each one's bytes, and its client.
    parked: Parked,
    /// Every request it read while parking, held or passed on, as text.
    seen: Arc<Mutex<Vec<String>>>,
    /// Holding answers, it sends on each request whose line holds one of
    /// these and holds the node's answer, not sent back, until let go
    /// ([`Proxy::unpark_answers`]): what the node said at one moment,
    /// heard at a later one.
    answering: Arc<Mutex<Vec<String>>>,
    /// The answers it holds: the request's line, the answer's bytes, and
    /// the client it is for.
    answers: Answers,
    /// Cutting, it sends on each request whose line holds one of these
    /// and passes back the node's answer less its last bytes, the
    /// connection then closed: a node that broke off mid-body.
    cutting: Arc<Mutex<Vec<String>>>,
}

type Parked = Arc<Mutex<Vec<(Vec<u8>, TcpStream)>>>;
type Answers = Arc<Mutex<Vec<(String, Vec<u8>, TcpStream)>>>;

impl Proxy {
    fn to(port: u16) -> Proxy {
        let target = format!("127.0.0.1:{port}");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let cut = Arc::new(AtomicBool::new(false));
        let hung = Arc::new(AtomicBool::new(false));
        let open = Arc::new(Mutex::new(Vec::new()));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let parking: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let parked: Parked = Arc::new(Mutex::new(Vec::new()));
        let (flag, stopped, streams, lines) = (
            Arc::clone(&cut),
            Arc::clone(&hung),
            Arc::clone(&open),
            Arc::clone(&asked),
        );
        let (holding, held) = (Arc::clone(&parking), Arc::clone(&parked));
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let saw = Arc::clone(&seen);
        let answering: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let answers: Answers = Arc::new(Mutex::new(Vec::new()));
        let (holding_answers, held_answers) = (Arc::clone(&answering), Arc::clone(&answers));
        let cutting: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cuts = Arc::clone(&cutting);
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                if flag.load(Ordering::SeqCst) {
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                }
                if stopped.load(Ordering::SeqCst) {
                    streams.lock().unwrap().push(client.try_clone().unwrap());
                    let lines = Arc::clone(&lines);
                    std::thread::spawn(move || {
                        let mut line = String::new();
                        if std::io::BufRead::read_line(
                            &mut std::io::BufReader::new(client),
                            &mut line,
                        )
                        .is_ok()
                        {
                            lines.lock().unwrap().push(line);
                        }
                    });
                    continue;
                }
                if !holding.lock().unwrap().is_empty()
                    || !holding_answers.lock().unwrap().is_empty()
                    || !cuts.lock().unwrap().is_empty()
                {
                    let (holding, held, streams, target, saw) = (
                        Arc::clone(&holding),
                        Arc::clone(&held),
                        Arc::clone(&streams),
                        target.clone(),
                        Arc::clone(&saw),
                    );
                    let (holding_answers, held_answers) =
                        (Arc::clone(&holding_answers), Arc::clone(&held_answers));
                    let cuts = Arc::clone(&cuts);
                    std::thread::spawn(move || {
                        let Some(bytes) = first_request(&client) else {
                            return;
                        };
                        saw.lock()
                            .unwrap()
                            .push(String::from_utf8_lossy(&bytes).into_owned());
                        let line = String::from_utf8_lossy(&bytes)
                            .lines()
                            .next()
                            .unwrap_or("")
                            .to_string();
                        if holding.lock().unwrap().iter().any(|p| line.contains(p)) {
                            held.lock().unwrap().push((bytes, client));
                        } else if holding_answers
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|p| line.contains(p))
                        {
                            if let Some(answer) = asked_of(&target, &bytes) {
                                held_answers.lock().unwrap().push((line, answer, client));
                            }
                        } else if cuts.lock().unwrap().iter().any(|p| line.contains(p)) {
                            if let Some(answer) = asked_of(&target, &bytes) {
                                let mut client = client;
                                let _ = client.write_all(&answer[..answer.len().saturating_sub(3)]);
                                let _ = client.shutdown(Shutdown::Both);
                            }
                        } else {
                            pass_on(client, &target, &bytes, &streams);
                        }
                    });
                    continue;
                }
                let Ok(server) = TcpStream::connect(&target) else {
                    continue;
                };
                let mut held = streams.lock().unwrap();
                held.push(client.try_clone().unwrap());
                held.push(server.try_clone().unwrap());
                drop(held);
                let (a, b) = (client.try_clone().unwrap(), server.try_clone().unwrap());
                std::thread::spawn(move || copy(a, b));
                std::thread::spawn(move || copy(server, client));
            }
        });
        Proxy {
            addr,
            target: format!("127.0.0.1:{port}"),
            cut,
            hung,
            open,
            asked,
            parking,
            parked,
            seen,
            answering,
            answers,
            cutting,
        }
    }

    /// Cuts the node's answer to every request whose line holds `what`
    /// off mid-body from now on.
    fn cut_answers(&self, what: &str) {
        self.cutting.lock().unwrap().push(what.to_string());
        for s in self.open.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    /// Holds the node's answer to every request whose line holds `what`
    /// from now on.
    fn park_answers(&self, what: &str) {
        self.answering.lock().unwrap().push(what.to_string());
        for s in self.open.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    /// How many answers it holds to requests whose line holds `what`.
    fn answers_held(&self, what: &str) -> usize {
        self.answers
            .lock()
            .unwrap()
            .iter()
            .filter(|(l, _, _)| l.contains(what))
            .count()
    }

    /// Holds no more answers to requests whose line holds `what`, and
    /// sends back the ones it held.
    fn unpark_answers(&self, what: &str) {
        self.answering.lock().unwrap().retain(|p| p != what);
        let mut held = self.answers.lock().unwrap();
        let (go, stay): (Vec<_>, Vec<_>) = held.drain(..).partition(|(l, _, _)| l.contains(what));
        *held = stay;
        drop(held);
        for (_, answer, mut client) in go {
            let _ = client.write_all(&answer);
            let _ = client.shutdown(Shutdown::Both);
        }
    }

    /// The requests it read while parking whose line holds `what`, as text.
    fn seen(&self, what: &str) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|t| t.lines().next().is_some_and(|l| l.contains(what)))
            .cloned()
            .collect()
    }

    /// Holds every request whose line holds `what` from now on, its
    /// connections open until now closed so the next ones are read.
    fn park(&self, what: &str) {
        self.parking.lock().unwrap().push(what.to_string());
        for s in self.open.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    /// The requests held whose line holds `what`, as text.
    fn parked(&self, what: &str) -> Vec<String> {
        self.parked
            .lock()
            .unwrap()
            .iter()
            .map(|(b, _)| String::from_utf8_lossy(b).into_owned())
            .filter(|t| t.lines().next().is_some_and(|l| l.contains(what)))
            .collect()
    }

    /// Holds no more requests whose line holds `what`, and sends on the
    /// ones it held.
    fn unpark(&self, what: &str) {
        self.parking.lock().unwrap().retain(|p| p != what);
        let target = self.target.clone();
        let mut held = self.parked.lock().unwrap();
        let (go, stay): (Vec<_>, Vec<_>) = held.drain(..).partition(|(b, _)| {
            String::from_utf8_lossy(b)
                .lines()
                .next()
                .is_some_and(|l| l.contains(what))
        });
        *held = stay;
        drop(held);
        for (bytes, client) in go {
            pass_on(client, &target, &bytes, &self.open);
        }
    }

    fn hang(&self) {
        self.hung.store(true, Ordering::SeqCst);
        for s in self.open.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    /// How many requests it was sent, while hung, whose line holds `what`.
    fn asked(&self, what: &str) -> usize {
        self.asked
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.contains(what))
            .count()
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn cut(&self) {
        self.cut.store(true, Ordering::SeqCst);
        for s in self.open.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    fn heal(&self) {
        self.cut.store(false, Ordering::SeqCst);
        self.hung.store(false, Ordering::SeqCst);
        for s in self.open.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

/// The first request a client sends on a connection, whole: its head and
/// the body its `Content-Length` names.
fn first_request(client: &TcpStream) -> Option<Vec<u8>> {
    let mut client = client.try_clone().ok()?;
    client
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = client.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..n]);
        let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        if bytes.len() >= end + 4 + length {
            client.set_read_timeout(None).ok()?;
            return Some(bytes);
        }
    }
}

/// `client`'s connection carried on to `target`, the request it sent
/// first, `bytes`, written ahead -- asking the node to close it after: a
/// connection the router kept for its next call carried that call past
/// the parking, which reads the first request of each alone.
fn pass_on(client: TcpStream, target: &str, bytes: &[u8], open: &Mutex<Vec<TcpStream>>) {
    let Ok(mut server) = TcpStream::connect(target) else {
        let _ = client.shutdown(Shutdown::Both);
        return;
    };
    let line = bytes
        .windows(2)
        .position(|w| w == b"\r\n")
        .map_or(bytes.len(), |n| n + 2);
    let mut sent = bytes[..line].to_vec();
    sent.extend_from_slice(b"Connection: close\r\n");
    sent.extend_from_slice(&bytes[line..]);
    if server.write_all(&sent).is_err() {
        return;
    }
    let mut held = open.lock().unwrap();
    held.push(client.try_clone().unwrap());
    held.push(server.try_clone().unwrap());
    drop(held);
    let (a, b) = (client.try_clone().unwrap(), server.try_clone().unwrap());
    std::thread::spawn(move || copy(a, b));
    std::thread::spawn(move || copy(server, client));
}

/// The node at `target`'s whole answer to the request `bytes`, asked to
/// close after it.
fn asked_of(target: &str, bytes: &[u8]) -> Option<Vec<u8>> {
    let mut server = TcpStream::connect(target).ok()?;
    let line = bytes
        .windows(2)
        .position(|w| w == b"\r\n")
        .map_or(bytes.len(), |n| n + 2);
    let mut sent = bytes[..line].to_vec();
    sent.extend_from_slice(b"Connection: close\r\n");
    sent.extend_from_slice(&bytes[line..]);
    server.write_all(&sent).ok()?;
    let mut answer = Vec::new();
    server.read_to_end(&mut answer).ok()?;
    Some(answer)
}

fn copy(mut from: TcpStream, mut to: TcpStream) {
    let _ = std::io::copy(&mut from, &mut to);
    let _ = to.shutdown(Shutdown::Write);
}

struct Files(PathBuf);

impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn files(tag: &str) -> Files {
    let d = std::env::temp_dir().join(format!("fenec-rr-dirs-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Files(d)
}

/// A directory file a router serves to its standbys, or follows from
/// `upstream`: the database and its replication.
fn directory(
    path: &Path,
    upstream: Option<&str>,
) -> (Arc<RwLock<fenec_core::prelude::Database>>, Arc<Replication>) {
    let (mut db, feed) = fenec_http::replication::open(path.to_str().unwrap(), 8 << 20).unwrap();
    match upstream {
        Some(_) => {
            let lineage = db.history().lineage.clone();
            db.follow(lineage).unwrap();
        }
        None => {
            if db.history().lineage.is_empty() {
                db.fork(fresh_id()).unwrap();
                db.sync().unwrap();
            }
        }
    }
    let db = Arc::new(RwLock::new(db));
    let follower = upstream.map(|url| {
        let f = Follower::new(
            url,
            DIRS.into(),
            Arc::clone(&db),
            Some(Arc::clone(&feed)),
            true,
        )
        .unwrap();
        f.start("fenec-test-follower".into()).unwrap();
        f
    });
    let repl = Replication::new(Some(DIRS.into()), Some(feed), follower);
    (db, repl)
}

fn config(near: Option<&str>, region: Option<&str>, global: Option<String>) -> Config {
    Config {
        addr: "127.0.0.1:0".into(),
        token: Some(ROOT.into()),
        upstream_timeout: Duration::from_secs(60),
        replicas: true,
        near: near.map(str::to_string),
        region: region.map(str::to_string),
        global: global.into_iter().collect(),
        ..Config::default()
    }
}

fn serve(router: Arc<Router>) -> u16 {
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    port
}

/// A router of the global directory at `path` near `near`: its primary, or
/// with `follows` a standby of the one at that URL.
fn global_router(path: &Path, follows: Option<&str>, near: &str) -> u16 {
    global_router_leasing(path, follows, near, false)
}

/// [`global_router`], leasing its nodes where `leasing`: a lease long
/// enough that none lapses in a test, granted as a tenant is placed.
fn global_router_leasing(path: &Path, follows: Option<&str>, near: &str, leasing: bool) -> u16 {
    let mut cfg = config(Some(near), None, None);
    if leasing {
        cfg.auto_failover = Some(Duration::from_secs(600));
    }
    global_router_with(path, follows, cfg)
}

fn global_router_with(path: &Path, follows: Option<&str>, cfg: Config) -> u16 {
    let (db, repl) = directory(path, follows);
    let dir = Directory::load(db).unwrap();
    serve(Router::replicated(dir, cfg, repl))
}

/// Region `region`'s router over its own directory at `path`, with a copy
/// of the global directory followed from `global`.
fn region_router(region: &str, path: &Path, global: &str) -> u16 {
    region_router_with(
        path,
        global,
        config(None, Some(region), Some(global.to_string())),
    )
}

/// [`region_router`] configured as given.
fn region_router_with(path: &Path, global: &str, cfg: Config) -> u16 {
    let (db, repl) = directory(path, None);
    let dir = Directory::load(db).unwrap();
    let (gdb, _grepl) = directory(&path.with_extension("global.fenec"), Some(global));
    let gdir = Directory::load(gdb).unwrap();
    serve(Router::regional(dir, cfg, Some(repl), gdir).unwrap())
}

/// One request: its status, its head and its body.
fn request(
    port: u16,
    method: &str,
    target: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> (u16, String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    write!(
        s,
        "{head}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out)
        .unwrap_or_else(|e| panic!("{method} {target} on {port}, {headers:?}, {body}: {e}"));
    let status = out[9..12].parse().unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    (status, head.to_string(), body.to_string())
}

fn shard(port: u16, method: &str, target: &str, body: &str) -> (u16, String) {
    let auth = format!("Bearer {ROOT}");
    let (status, _, body) = request(port, method, target, body, &[("Authorization", &auth)]);
    (status, body)
}

fn admin(port: u16, tag: &str, method: &str, target: &str, body: &str) -> (u16, String) {
    let auth = format!("Bearer adm-{tag}");
    let (status, _, body) = request(port, method, target, body, &[("Authorization", &auth)]);
    (status, body)
}

/// An admin request of a body that is bytes -- an image -- and its answer's
/// status and bytes.
fn admin_bytes(port: u16, tag: &str, method: &str, target: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let head = format!(
        "{method} {target} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nAuthorization: Bearer \
         adm-{tag}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    let status = String::from_utf8_lossy(&out[9..12]).parse().unwrap();
    let end = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(out.len(), |n| n + 4);
    (status, out[end..].to_vec())
}

fn query_body(sql: &str) -> String {
    let mut body = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut body, sql);
    body.push('}');
    body
}

/// Where an answer left the tenant: `Fenec-Seq`, and `Fenec-History`, the
/// history that change is of, as a client sends them back.
#[derive(Clone, Debug)]
struct Pos {
    seq: u64,
    history: Option<String>,
}

/// A header of `head`, by its name.
fn header_of<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// A write through the router: where it left the tenant.
fn write(port: u16, tenant: &str, sql: &str) -> Pos {
    let (status, head, body) = request(
        port,
        "POST",
        &format!("/t/{tenant}/query"),
        &query_body(sql),
        &[],
    );
    assert_eq!(status, 200, "{sql}: {body}");
    Pos {
        seq: header_of(&head, "fenec-seq")
            .expect("a write answers Fenec-Seq")
            .parse()
            .unwrap(),
        // Every tenant of a node that replicates has a history of its own.
        history: Some(
            header_of(&head, "fenec-history")
                .expect("and Fenec-History")
                .into(),
        ),
    }
}

/// The headers of a read marked for a replica, after `after` where given.
fn mark(after: Option<&Pos>) -> Vec<(&'static str, String)> {
    let mut headers = vec![("Fenec-Read", "replica".to_string())];
    if let Some(p) = after {
        headers.push(("Fenec-After", p.seq.to_string()));
        if let Some(h) = &p.history {
            headers.push(("Fenec-History", h.clone()));
        }
    }
    headers
}

/// `request` with `headers` owned.
fn request_with(
    port: u16,
    target: &str,
    sql: &str,
    headers: &[(&'static str, String)],
) -> (u16, String, String) {
    let borrowed: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    request(port, "POST", target, &query_body(sql), &borrowed)
}

/// A request's status and body, its head left out.
trait TwoOfThree {
    fn map_two(self) -> (u16, String);
}

impl TwoOfThree for (u16, String, String) {
    fn map_two(self) -> (u16, String) {
        (self.0, self.2)
    }
}

/// A read through the router, unmarked: its status and body.
fn read(port: u16, tenant: &str, sql: &str) -> (u16, String) {
    let (status, _, body) = request(
        port,
        "POST",
        &format!("/t/{tenant}/query"),
        &query_body(sql),
        &[],
    );
    (status, body)
}

/// A read its client marked for a replica, after `after` where given: its
/// status and body.
fn marked(port: u16, tenant: &str, sql: &str, after: Option<&Pos>) -> (u16, String) {
    let (status, _, body) = request_with(port, &format!("/t/{tenant}/query"), sql, &mark(after));
    (status, body)
}

/// Waits until `check` holds, 60 s at most: a bound on something that must
/// happen, never on how soon.
fn until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !check() {
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The text after `"key":"` in a JSON text, to its closing quote.
fn text_of(json: &str, key: &str) -> Option<String> {
    let rest = json.split(&format!("\"{key}\":\"")).nth(1)?;
    Some(rest.split('"').next()?.to_string())
}

/// eu holds the tenants, us their read replicas, all in the global
/// directory: eu1 and eu2 in eu, us1 in us, each reached through a proxy;
/// the global router near eu, and its standby near us -- the router us's
/// clients ask.
struct OneDirectory {
    _files: Files,
    _nodes: [Node; 3],
    links: [Proxy; 3],
    primary: u16,
    us: u16,
}

impl OneDirectory {
    fn new(tag: &str) -> OneDirectory {
        OneDirectory::with(tag, false)
    }

    /// The same with every node taking writes under the primary router's
    /// lease, as a deployment failing over on its own does.
    fn with(tag: &str, leased: bool) -> OneDirectory {
        OneDirectory::with_us(tag, leased, config(Some("us"), None, None))
    }

    /// The same, us's router configured as `us`.
    fn with_us(tag: &str, leased: bool, us: Config) -> OneDirectory {
        let mut primary = config(Some("eu"), None, None);
        if leased {
            primary.auto_failover = Some(Duration::from_secs(600));
        }
        OneDirectory::build(tag, leased, primary, us)
    }

    /// The same, each router configured as given.
    fn build(tag: &str, leased: bool, primary: Config, us: Config) -> OneDirectory {
        let files = files(tag);
        let names = ["eu1", "eu2", "us1"];
        let nodes = names.map(|n| node_leased(&format!("{tag}-{n}"), leased));
        let links = [0, 1, 2].map(|i| Proxy::to(nodes[i].port));
        let primary = global_router_with(&files.0.join("global.fenec"), None, primary);
        let us = global_router_with(
            &files.0.join("standby.fenec"),
            Some(&format!("http://127.0.0.1:{primary}")),
            us,
        );
        for (i, (name, (region, zone))) in names
            .iter()
            .zip([("eu", "eu-a"), ("eu", "eu-b"), ("us", "us-a")])
            .enumerate()
        {
            let body = format!(
                r#"{{"addr":"{}","token":"adm-{tag}-{name}","region":"{region}","zone":"{zone}"}}"#,
                links[i].addr
            );
            let (status, answer) = shard(primary, "PUT", &format!("/_shard/nodes/{name}"), &body);
            assert_eq!(status, 201, "{name}: {answer}");
        }
        OneDirectory {
            _files: files,
            _nodes: nodes,
            links,
            primary,
            us,
        }
    }

    /// `tenant` made on eu1 with a collection, its replica on eu2 and its
    /// read replica on us1, which us's router knows of.
    fn tenant(&self, tenant: &str) {
        self.made(tenant);
        let (status, body) = shard(
            self.primary,
            "PUT",
            &format!("/_shard/tenants/{tenant}/read-replicas/us"),
            "",
        );
        assert_eq!(status, 201, "{body}");
        assert_eq!(text_of(&body, "node").as_deref(), Some("us1"), "{body}");
        assert_eq!(
            text_of(&body, "follows"),
            Some(self.links[0].url()),
            "{body}"
        );
        // Placed again, it is the one there.
        let (status, again) = shard(
            self.primary,
            "PUT",
            &format!("/_shard/tenants/{tenant}/read-replicas/us"),
            "",
        );
        assert_eq!((status, again.as_str()), (200, body.as_str()));
        until("us's router knows of the read replica", || {
            shard(
                self.us,
                "GET",
                &format!("/_shard/tenants/{tenant}/read-replicas"),
                "",
            )
            .1
            .contains(r#""node":"us1""#)
        });
    }

    /// `tenant` made on eu1 with a collection, its replica on eu2.
    fn made(&self, tenant: &str) {
        let (status, body) = shard(
            self.primary,
            "PUT",
            &format!("/_shard/tenants/{tenant}"),
            r#"{"node":"eu1","schema":"create collection notes (body text)"}"#,
        );
        assert_eq!(status, 201, "{body}");
        assert!(body.contains(r#""replica_on":"eu2""#), "{body}");
    }

    /// Waits until the read replica of `tenant` holds `what`, asked of its
    /// node: a marked read through the router may be answered home.
    fn holds(&self, tenant: &str, what: &str) {
        let us1 = self._nodes[2].port;
        until("the read replica holds the write", || {
            marked(us1, tenant, "get notes select body", None)
                .1
                .contains(what)
        });
    }

    /// Waits until the tenant's own replica on eu2 holds `what`: a failover
    /// promotes what it holds, and a write its stream had not yet carried
    /// there as eu1 was cut is lost with eu1 -- the collection with it.
    fn replicated(&self, tenant: &str, what: &str) {
        let eu2 = self._nodes[1].port;
        until("the tenant's replica on eu2 holds the write", || {
            marked(eu2, tenant, "get notes select body", None)
                .1
                .contains(what)
        });
    }

    /// Waits until the read replica of `tenant` follows the node behind
    /// `link`: a failover or a move has the router's next look over them
    /// come at once, and that look points it there.
    fn follows(&self, tenant: &str, link: &Proxy) {
        until("the read replica follows the tenant's new primary", || {
            let (_, list) = shard(
                self.primary,
                "GET",
                &format!("/_shard/tenants/{tenant}/read-replicas"),
                "",
            );
            text_of(&list, "follows") == Some(link.url())
        });
    }

    /// Waits until us's router, a standby, places `tenant` on `node` as
    /// the primary now does: its directory follows the primary's.
    fn settled(&self, tenant: &str, node: &str) {
        until(
            "us's router places the tenant where the primary does",
            || {
                let (_, shown) = shard(self.us, "GET", &format!("/_shard/tenants/{tenant}"), "");
                text_of(&shown, "node").as_deref() == Some(node)
            },
        );
    }
}

#[test]
fn a_marked_read_near_a_read_replica_is_answered_there_and_never_older_than_its_write() {
    let w = OneDirectory::new("near");
    w.tenant("acme");
    // None in the tenant's own region, nor in one with no node.
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/tenants/acme/read-replicas/eu",
        "",
    );
    assert_eq!(status, 409, "{body}");
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/tenants/acme/read-replicas/ap",
        "",
    );
    assert_eq!(status, 409, "{body}");
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/tenants/nosuch/read-replicas/us",
        "",
    );
    assert_eq!(status, 404, "{body}");

    // A write through us's router goes home; the read marked after it is
    // answered with it.
    let a = write(w.us, "acme", r#"put notes {body: "a"}"#);
    let (status, body) = marked(w.us, "acme", "get notes select body", Some(&a));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""a""#), "{body}");
    // Answered home, it may have been, the replica not caught up yet.
    w.holds("acme", r#""a""#);

    // The replica's own link to home cut -- us1 made to follow home through
    // a link of its own, answering once home has said hello over it, then
    // the link cut -- a write home is not on it. Marked and naming no
    // write, the read is answered with what the replica last applied,
    // which only the replica could answer: home holds the newer row.
    let us1 = w._nodes[2].port;
    let own = Proxy::to(w._nodes[0].port);
    let (status, body) = admin(
        us1,
        "near-us1",
        "POST",
        "/_admin/tenants/acme/follow",
        &format!(r#"{{"from":"{}"}}"#, own.url()),
    );
    assert_eq!(status, 200, "{body}");
    w.holds("acme", r#""a""#);
    own.cut();
    let b = write(w.us, "acme", r#"put notes {body: "b"}"#);
    let (status, body) = marked(w.us, "acme", "get notes select body", None);
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(r#""a""#) && !body.contains(r#""b""#),
        "{body}"
    );
    let (_, body) = read(w.us, "acme", "get notes select body");
    assert!(body.contains(r#""b""#), "{body}");
    // Naming the write, the replica cannot catch up in the wait, and the
    // read goes home: never older than the client's own write.
    let (status, _, body) = request(
        w.us,
        "POST",
        "/t/acme/query",
        &query_body("get notes select body"),
        &[
            ("Fenec-Read", "replica"),
            ("Fenec-After", &b.seq.to_string()),
            ("Fenec-History", b.history.as_deref().unwrap()),
            ("Fenec-Wait", "200"),
        ],
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""b""#), "{body}");

    // Healed, the replica catches up from where it stood, and is pointed
    // back at home's link, as the router records it.
    own.heal();
    w.holds("acme", r#""b""#);
    let (status, body) = admin(
        us1,
        "near-us1",
        "POST",
        "/_admin/tenants/acme/follow",
        &format!(r#"{{"from":"{}"}}"#, w.links[0].url()),
    );
    assert_eq!(status, 200, "{body}");
    w.holds("acme", r#""b""#);

    // Home out of reach: the marked read is still answered, by the
    // replica; an unmarked one is not; a write is refused at the replica,
    // marked as a read by mistake, and goes nowhere.
    w.links[0].cut();
    let (status, body) = marked(w.us, "acme", "get notes select body", None);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""b""#), "{body}");
    let (status, body) = read(w.us, "acme", "get notes select body");
    assert_eq!(status, 502, "{body}");
    let (status, body) = marked(w.us, "acme", r#"put notes {body: "c"}"#, None);
    assert_eq!(status, 403, "{body}");
    w.links[0].heal();

    // The router near the tenant's home sends a marked read home.
    let (status, body) = marked(w.primary, "acme", "get notes select body", Some(&b));
    assert_eq!(status, 200, "{body}");
    // So does one near a replica that does not answer.
    w.links[2].cut();
    let (status, body) = marked(w.us, "acme", "get notes select body", Some(&b));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""b""#), "{body}");
    w.links[2].heal();

    // Taken off, its region reads home, and the node holds the tenant no
    // more.
    let (status, body) = shard(
        w.primary,
        "DELETE",
        "/_shard/tenants/acme/read-replicas/us",
        "",
    );
    assert_eq!(status, 204, "{body}");
    let (_, names) = admin(us1, "near-us1", "GET", "/_admin/tenants", "");
    assert!(!names.contains("acme"), "{names}");
    let (status, body) = shard(
        w.primary,
        "DELETE",
        "/_shard/tenants/acme/read-replicas/us",
        "",
    );
    assert_eq!(status, 404, "{body}");
    until("us's router forgets it", || {
        !shard(w.us, "GET", "/_shard/tenants/acme/read-replicas", "")
            .1
            .contains("us1")
    });
    let (status, body) = marked(w.us, "acme", "get notes select body", Some(&b));
    assert_eq!(status, 200, "{body}");
}

#[test]
fn a_read_replica_follows_its_tenant_through_a_failover_and_a_move() {
    let w = OneDirectory::new("fail");
    w.tenant("acme");
    let a = write(w.us, "acme", r#"put notes {body: "a"}"#);
    assert_eq!(marked(w.us, "acme", "get notes count", Some(&a)).0, 200);

    // eu1 gone, its tenant fails over onto its replica on eu2: the read
    // replica follows eu2 at once, and is not what was promoted.
    w.links[0].cut();
    let (status, body) = shard(w.primary, "POST", "/_shard/nodes/eu1/failover", "");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""to":"eu2""#), "{body}");
    w.follows("acme", &w.links[1]);
    w.settled("acme", "eu2");
    let b = write(w.us, "acme", r#"put notes {body: "b"}"#);
    let (status, body) = marked(w.us, "acme", "get notes select body", Some(&b));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""b""#), "{body}");
    // Answered by the replica: home out of reach, it still reads. Held to
    // the replica first: the marked read above may have been answered
    // home, the replica still taking eu2's image.
    w.holds("acme", r#""b""#);
    w.links[1].cut();
    let (status, body) = marked(w.us, "acme", "get notes select body", None);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""b""#), "{body}");
    w.links[1].heal();
    // The replica takes no write: it was never promoted.
    let us1 = w._nodes[2].port;
    let (status, _, body) = request(
        us1,
        "POST",
        "/t/acme/query",
        &query_body(r#"put notes {body: "x"}"#),
        &[],
    );
    assert!(status == 403 || status == 503, "{status} {body}");

    // Back, eu1 rejoins as a replica; the tenant moves onto it, and the read
    // replica follows it there.
    w.links[0].heal();
    let (status, body) = shard(w.primary, "POST", "/_shard/replicas", "");
    assert_eq!(status, 200, "{body}");
    let (status, body) = shard(
        w.primary,
        "POST",
        "/_shard/tenants/acme/move",
        r#"{"to":"eu1"}"#,
    );
    assert_eq!(status, 200, "{body}");
    w.follows("acme", &w.links[0]);
    w.settled("acme", "eu1");
    let c = write(w.us, "acme", r#"put notes {body: "c"}"#);
    let (status, body) = marked(w.us, "acme", "get notes select body", Some(&c));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""c""#), "{body}");

    // Deleted, its read replicas go with it.
    let (status, body) = shard(w.primary, "DELETE", "/_shard/tenants/acme", "");
    assert_eq!(status, 204, "{body}");
    let (_, names) = admin(us1, "fail-us1", "GET", "/_admin/tenants", "");
    assert!(!names.contains("acme"), "{names}");
}

/// On leased nodes a read replica's copy is no primary the lease names, and
/// a write refused there is turned into 503 and `Retry-After`, as for a
/// tenant failing over: a write marked as a read by mistake was told to
/// come back, to the same replica, for good -- the router sent it home past
/// the 503, and paid a hop for nothing. It is refused 403 instead, saying
/// why, as anywhere; the marked reads are answered, the node holding no
/// lease at all.
#[test]
fn a_write_marked_as_a_read_is_refused_on_a_leased_replica_not_sent_back() {
    let w = OneDirectory::with("leased", true);
    w.tenant("acme");
    let a = write(w.us, "acme", r#"put notes {body: "a"}"#);
    let (status, body) = marked(w.us, "acme", "get notes select body", Some(&a));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""a""#), "{body}");
    // Asked of the replica's node itself, caught up: refused, not told to
    // come back.
    w.holds("acme", r#""a""#);
    let put = query_body(r#"put notes {body: "b"}"#);
    let us1 = w._nodes[2].port;
    let (status, _, body) = request(
        us1,
        "POST",
        "/t/acme/query",
        &put,
        &[("Fenec-Read", "replica")],
    );
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("only reads"), "{body}");
    // Through the router the same, and the write goes nowhere.
    let (status, body) = marked(w.us, "acme", r#"put notes {body: "b"}"#, None);
    assert_eq!(status, 403, "{body}");
    let (_, body) = read(w.us, "acme", "get notes count");
    assert!(body.contains(r#""count":1"#), "{body}");
    // Unmarked, the write goes home and lands.
    write(w.us, "acme", r#"put notes {body: "b"}"#);
}

/// The global router in eu over eu1 and eu2; us keeping its own directory,
/// with us1 and us2. A tenant at home in eu has a read replica in us,
/// placed and re-pointed by us's router; one at home in us has one in eu,
/// the global router's, which asks us's router where its primary is.
#[test]
fn regions_keeping_their_own_directory_place_and_repoint_their_read_replicas() {
    let tag = "regional";
    let files = files(tag);
    let names = ["eu1", "eu2", "us1", "us2"];
    let nodes = names.map(|n| node(&format!("{tag}-{n}")));
    let global = global_router(&files.0.join("global.fenec"), None, "eu");
    let us = region_router(
        "us",
        &files.0.join("us.fenec"),
        &format!("http://127.0.0.1:{global}"),
    );
    let (status, body) = shard(
        global,
        "PUT",
        "/_shard/regions/us",
        &format!(r#"{{"routers":["http://127.0.0.1:{us}"]}}"#),
    );
    assert_eq!(status, 200, "{body}");
    for (i, (name, region)) in names.iter().zip(["eu", "eu", "us", "us"]).enumerate() {
        let body = format!(
            r#"{{"addr":"127.0.0.1:{}","token":"adm-{tag}-{name}","region":"{region}"}}"#,
            nodes[i].port
        );
        let (status, answer) = shard(global, "PUT", &format!("/_shard/nodes/{name}"), &body);
        assert_eq!(status, 201, "{name}: {answer}");
    }
    until("us's copy of the global directory arrives", || {
        shard(us, "GET", "/_shard/regions", "")
            .1
            .contains(r#""name":"us""#)
    });

    // At home in eu, read near us: placed by us's router, asked of the
    // global one.
    let schema = r#""schema":"create collection notes (body text)""#;
    for (t, region) in [("ab", "eu"), ("ba", "us")] {
        let (status, body) = shard(
            global,
            "PUT",
            &format!("/_shard/tenants/{t}"),
            &format!(r#"{{"region":"{region}",{schema}}}"#),
        );
        assert_eq!(status, 201, "{t}: {body}");
    }
    until("us's copy holds ab", || {
        let (status, body) = shard(global, "PUT", "/_shard/tenants/ab/read-replicas/us", "");
        status == 201 || body.contains("us1") || body.contains("us2")
    });
    let (_, list) = shard(us, "GET", "/_shard/tenants/ab/read-replicas", "");
    assert!(list.contains(r#""region":"us""#), "{list}");
    let ab_on = text_of(&list, "node").unwrap();
    // At home in us, read near eu: the global router's, which asks us's
    // router where the primary is.
    let (status, body) = shard(global, "PUT", "/_shard/tenants/ba/read-replicas/eu", "");
    assert_eq!(status, 201, "{body}");
    let ba_on = text_of(&body, "node").unwrap();

    // Each region's router answers the marked reads of the other's tenant.
    let a = write(us, "ab", r#"put notes {body: "a"}"#);
    let (status, body) = marked(us, "ab", "get notes select body", Some(&a));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""a""#), "{body}");
    let b = write(global, "ba", r#"put notes {body: "b"}"#);
    let (status, body) = marked(global, "ba", "get notes select body", Some(&b));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""b""#), "{body}");

    // Each tenant's quota, kept by its home's directory, reaches its read
    // replica kept by the other's: ab's at us's router's next look, through
    // its copy of the global directory, ba's at the global router's, asked
    // of us's router with the placements.
    for (t, n) in [("ab", 5), ("ba", 7)] {
        let (status, body) = shard(
            global,
            "PUT",
            &format!("/_shard/tenants/{t}/quota"),
            &format!(r#"{{"streams": {n}}}"#),
        );
        assert_eq!(status, 200, "{t}: {body}");
    }
    for (t, on, n) in [("ab", &ab_on, 5), ("ba", &ba_on, 7)] {
        let i = names.iter().position(|x| x == on).unwrap();
        until("the quota reaches the read replica", || {
            admin(
                nodes[i].port,
                &format!("{tag}-{on}"),
                "GET",
                &format!("/_admin/tenants/{t}/quota"),
                "",
            )
            .1
            .contains(&format!(r#""streams":{n}"#))
        });
    }

    // ab fails over in eu, by the global router: us's router learns of it
    // at its next look, and points its read replica at the new primary.
    let (_, shown) = shard(global, "GET", "/_shard/tenants/ab", "");
    let from = text_of(&shown, "node").unwrap();
    let (status, body) = shard(
        global,
        "POST",
        &format!("/_shard/nodes/{from}/failover"),
        "",
    );
    assert_eq!(status, 200, "{body}");
    let (_, shown) = shard(global, "GET", "/_shard/tenants/ab", "");
    let to = text_of(&shown, "node").unwrap();
    assert_ne!(from, to);
    let to_port = nodes[names.iter().position(|n| *n == to).unwrap()].port;
    until("us's read replica of ab follows its new primary", || {
        shard(global, "GET", "/_shard/tenants/ab/read-replicas", "")
            .1
            .contains(&format!("127.0.0.1:{to_port}"))
    });
    let c = write(us, "ab", r#"put notes {body: "c"}"#);
    let (status, body) = marked(us, "ab", "get notes select body", Some(&c));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""c""#), "{body}");

    // Deleted through the global router, each tenant's read replica goes:
    // eu's at once, us's at its router's next look.
    for t in ["ab", "ba"] {
        let (status, body) = shard(global, "DELETE", &format!("/_shard/tenants/{t}"), "");
        assert_eq!(status, 204, "{t}: {body}");
    }
    until("the read replicas go with their tenants", || {
        ["ab", "ba"].iter().all(|t| {
            let (_, list) = shard(
                global,
                "GET",
                &format!("/_shard/tenants/{t}/read-replicas"),
                "",
            );
            list.contains(r#""read_replicas":[]"#)
        })
    });
    // And their files with them, a record going before its file. (The copy
    // a failover leaves on the node it failed over from stays, as it
    // always has.)
    for (t, on) in [("ab", &ab_on), ("ba", &ba_on)] {
        let i = names.iter().position(|n| n == on).unwrap();
        until("the read replica's file goes", || {
            let (_, held) = admin(
                nodes[i].port,
                &format!("{tag}-{on}"),
                "GET",
                "/_admin/tenants",
                "",
            );
            !held.contains(&format!("\"{t}\""))
        });
    }
}

/// A read replica just placed is an empty file -- a database of a history
/// of its own, holding no collection -- until its primary says hello and
/// it takes the image. It answered a marked read meanwhile: the tenant had
/// no collections. Now it answers 503, which sends the router home, until
/// its primary has said hello, and its `.follows` keeps that it has.
#[test]
fn a_read_replica_answers_no_marked_read_before_its_primary_has_said_hello() {
    let w = OneDirectory::new("hello");
    w.made("acme");
    let a = write(w.primary, "acme", r#"put notes {body: "a"}"#);
    // Home out of reach -- of us1's follower and of the routers alike: the
    // copy placed now can take no image.
    w.links[0].cut();
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/tenants/acme/read-replicas/us",
        "",
    );
    assert_eq!(status, 201, "{body}");
    let us1 = w._nodes[2].port;
    let (status, body) = marked(us1, "acme", "get notes select body", None);
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("has not caught up"), "{body}");
    until("us's router knows of the read replica", || {
        shard(w.us, "GET", "/_shard/tenants/acme/read-replicas", "")
            .1
            .contains("us1")
    });
    // Through us's router: sent home, out of reach -- never answered from
    // the empty copy.
    let (status, body) = marked(w.us, "acme", "get notes select body", None);
    assert_eq!(status, 502, "{body}");
    // Healed, the copy takes its image and answers.
    w.links[0].heal();
    w.holds("acme", r#""a""#);
    let (status, body) = marked(w.us, "acme", "get notes select body", Some(&a));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""a""#), "{body}");
    let follows = std::fs::read_to_string(w._nodes[2].dir.join("acme.follows")).unwrap();
    assert!(!follows.contains("pending"), "{follows}");
}

/// A failover forks the history where the copy promoted stood: a write the
/// old primary answered past that point is lost, and the new primary's
/// next write takes its change number. A read replica left on the old
/// branch -- not yet pointed at the new primary -- held a change of that
/// number and answered a read after the new write with the lost one. A
/// write names its history (`Fenec-History`), and a read sent after it is
/// answered by no copy whose history does not pass through it: the
/// replica waits, then the router asks home. Home, asked for the lost
/// write, says at once where it stands, for the client to take.
#[test]
fn a_read_after_a_write_waits_for_its_history_across_a_failover_not_its_number() {
    let w = OneDirectory::new("fork");
    w.tenant("acme");
    write(w.us, "acme", r#"put notes {body: "a"}"#);
    // eu2, the tenant's own replica, falls behind: what eu1 takes from here
    // is lost once eu2 is promoted.
    let eu2 = w._nodes[1].port;
    let (status, body) = admin(
        eu2,
        "fork-eu2",
        "POST",
        "/_admin/tenants/acme/follow",
        r#"{"from":"http://127.0.0.1:9"}"#,
    );
    assert_eq!(status, 200, "{body}");
    let lost = write(w.us, "acme", r#"put notes {body: "b-lost"}"#);
    w.holds("acme", "b-lost");
    // us1 kept from being pointed at the new primary: out of the routers'
    // reach, which the test goes round to ask it itself.
    w.links[2].cut();
    w.links[0].cut();
    let (status, body) = shard(w.primary, "POST", "/_shard/nodes/eu1/failover", "");
    assert_eq!(status, 200, "{body}");
    w.settled("acme", "eu2");
    let mine = write(w.us, "acme", r#"put notes {body: "d-mine"}"#);
    assert_eq!(mine.seq, lost.seq, "one change number, two histories");
    assert_ne!(mine.history, lost.history);

    // us1, on the branch the failover left, at that very number: asked
    // itself, it answers no read after the new write.
    let us1 = w._nodes[2].port;
    let mut after_mine = mark(Some(&mine));
    after_mine.push(("Fenec-Wait", "200".into()));
    let (status, _, body) =
        request_with(us1, "/t/acme/query", "get notes select body", &after_mine);
    assert_eq!(status, 504, "{body}");
    assert!(!body.contains("b-lost"), "{body}");
    // Through us's router: home answers it.
    let (status, _, body) =
        request_with(w.us, "/t/acme/query", "get notes select body", &after_mine);
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("d-mine") && !body.contains("b-lost"),
        "{body}"
    );

    // Home asked for the lost write: at once, not after the wait it is
    // given, it says where it stands and of what history.
    let mut after_lost = mark(Some(&lost));
    after_lost.push(("Fenec-Wait", "30000".into()));
    let started = Instant::now();
    let (status, _, body) =
        request_with(eu2, "/t/acme/query", "get notes select body", &after_lost);
    assert!(started.elapsed() < Duration::from_secs(15), "it waited");
    assert_eq!(status, 504, "{body}");
    assert!(
        body.contains(&format!(
            r#""history":"{}""#,
            mine.history.as_deref().unwrap()
        )),
        "{body}"
    );
    assert!(body.contains(&format!(r#""seq":{}"#, mine.seq)), "{body}");

    // The routers reaching us1 again, the next look points it at eu2,
    // whose image it takes: then it answers the read itself.
    w.links[2].heal();
    until("us1 answers the read after the new write", || {
        let (status, _, body) =
            request_with(us1, "/t/acme/query", "get notes select body", &after_mine);
        status == 200 && body.contains("d-mine")
    });
    let (_, _, body) = request_with(us1, "/t/acme/query", "get notes select body", &after_mine);
    assert!(!body.contains("b-lost"), "{body}");
}

/// A read replica's node that takes connections and never answers held
/// every marked read of its region for the router's upstream timeout, a
/// minute by default. A marked read gives a read replica its `Fenec-After`
/// wait and `--replica-timeout` past it, then goes home; and the replica
/// is let be, the reads after going home without asking it. Its rest is
/// longer than the test here, so the second read is passed over however
/// late it comes; and what the node was asked is told apart by its
/// request line, the primary router's check of the node asking it too.
#[test]
fn a_marked_read_passes_a_read_replica_that_does_not_answer_by_its_bound() {
    let mut us = config(Some("us"), None, None);
    us.upstream_timeout = Duration::from_secs(120);
    us.replica_timeout = Duration::from_millis(300);
    us.replica_rest = Duration::from_secs(600);
    let w = OneDirectory::with_us("hung", false, us);
    w.tenant("acme");
    let a = write(w.us, "acme", r#"put notes {body: "a"}"#);
    w.holds("acme", r#""a""#);
    w.links[2].hang();
    let started = Instant::now();
    let (status, body) = marked(w.us, "acme", "get notes select body", None);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""a""#), "{body}");
    // Against the upstream timeout's two minutes: half of it to spare.
    assert!(started.elapsed() < Duration::from_secs(60));
    until("the replica's node was asked the read", || {
        w.links[2].asked("/t/acme/query") == 1
    });
    // Let be: the next read goes home without asking the replica's node,
    // whose 300 ms its line would have arrived in.
    let (status, body) = marked(w.us, "acme", "get notes select body", Some(&a));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""a""#), "{body}");
    assert_eq!(
        w.links[2].asked("/t/acme/query"),
        1,
        "the replica was asked again"
    );
    w.links[2].heal();
}

/// A read replica's node without its file -- wiped, or the file deleted by
/// hand -- answered a marked read with its 404, which the client took for
/// the tenant's. The node says the tenant is absent, the router asks home,
/// and its next look makes the copy there again.
#[test]
fn a_marked_read_goes_home_where_its_read_replica_s_file_is_gone_and_it_is_made_again() {
    let w = OneDirectory::new("gone");
    w.tenant("acme");
    let a = write(w.us, "acme", r#"put notes {body: "a"}"#);
    w.holds("acme", r#""a""#);
    let us1 = w._nodes[2].port;
    let (status, body) = admin(us1, "gone-us1", "DELETE", "/_admin/tenants/acme", "");
    assert_eq!(status, 204, "{body}");
    let (status, body) = marked(w.us, "acme", "get notes select body", Some(&a));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""a""#), "{body}");
    w.holds("acme", r#""a""#);
}

/// A read replica's record goes only with its copy: a node that does not
/// take the copy off keeps it recorded, and a node holding one is not
/// removed -- either way a copy would follow its primary with nothing to
/// stop it. `?forget` lets go of the record of a node gone for good. A
/// placement whose copy will not follow takes off the file it made.
#[test]
fn a_read_replica_s_record_goes_only_with_its_copy() {
    let w = OneDirectory::new("drop");
    w.tenant("acme");
    let us1 = w._nodes[2].port;
    w.links[2].cut();
    let (status, body) = shard(
        w.primary,
        "DELETE",
        "/_shard/tenants/acme/read-replicas/us",
        "",
    );
    assert_eq!(status, 502, "{body}");
    assert!(body.contains("stays recorded"), "{body}");
    let (_, list) = shard(w.primary, "GET", "/_shard/tenants/acme/read-replicas", "");
    assert!(list.contains("us1"), "{list}");
    let (status, body) = shard(w.primary, "DELETE", "/_shard/nodes/us1", "");
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("read replica"), "{body}");
    w.links[2].heal();
    let (status, body) = shard(
        w.primary,
        "DELETE",
        "/_shard/tenants/acme/read-replicas/us",
        "",
    );
    assert_eq!(status, 204, "{body}");
    let (_, names) = admin(us1, "drop-us1", "GET", "/_admin/tenants", "");
    assert!(!names.contains("acme"), "{names}");

    // A node gone for good: its record forgotten, then the node removed.
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/tenants/acme/read-replicas/us",
        "",
    );
    assert_eq!(status, 201, "{body}");
    w.links[2].cut();
    let (status, body) = shard(
        w.primary,
        "DELETE",
        "/_shard/tenants/acme/read-replicas/us?forget",
        "",
    );
    assert_eq!(status, 204, "{body}");
    let (_, list) = shard(w.primary, "GET", "/_shard/tenants/acme/read-replicas", "");
    assert!(list.contains(r#""read_replicas":[]"#), "{list}");
    let (status, body) = shard(w.primary, "DELETE", "/_shard/nodes/us1", "");
    assert_eq!(status, 204, "{body}");

    // A node that follows nothing refuses to: the file the placement made
    // there goes again.
    let ap1 = node_plain("drop-ap1");
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/nodes/ap1",
        &format!(
            r#"{{"addr":"127.0.0.1:{}","token":"adm-drop-ap1","region":"ap"}}"#,
            ap1.port
        ),
    );
    assert_eq!(status, 201, "{body}");
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/tenants/acme/read-replicas/ap",
        "",
    );
    assert_eq!(status, 502, "{body}");
    let (_, names) = admin(ap1.port, "drop-ap1", "GET", "/_admin/tenants", "");
    assert!(!names.contains("acme"), "{names}");
}

/// A tenant deleted while a change of its read replica is under way waits
/// the change out, and takes the copy off before its 204: passed over, the
/// copy stayed recorded with the tenant gone, a marked read answered from
/// it until the next look took it off -- and the look's change holding it
/// as the delete came made the delete wait on itself (a CI run's 120 s).
/// The change here is the replica taken off by hand, its node silent; a
/// timeout of ten minutes keeps it under way until the node answers.
#[test]
fn a_tenant_deleted_beside_a_change_of_its_read_replica_waits_it_out() {
    let mut primary = config(Some("eu"), None, None);
    primary.reader_timeout = Duration::from_secs(600);
    let w = OneDirectory::build("waits", false, primary, config(Some("us"), None, None));
    w.tenant("acme");
    let us1 = w._nodes[2].port;
    w.links[2].hang();
    let at = w.primary;
    let dropping = std::thread::spawn(move || {
        shard(at, "DELETE", "/_shard/tenants/acme/read-replicas/us", "")
    });
    until("the replica's node was asked to take its copy off", || {
        w.links[2].asked("DELETE /_admin/tenants/acme") == 1
    });
    let deleting = std::thread::spawn(move || shard(at, "DELETE", "/_shard/tenants/acme", ""));
    // Waiting, the delete cannot answer while the change is under way; one
    // that answers meanwhile passed it over, and the copy must be gone.
    let started = Instant::now();
    while !deleting.is_finished() && started.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
    }
    if deleting.is_finished() {
        let (_, names) = admin(us1, "waits-us1", "GET", "/_admin/tenants", "");
        assert!(
            !names.contains("acme"),
            "the tenant was deleted, its read replica's copy kept: {names}"
        );
    }
    // Waiting, the delete holds no claim on the tenant: another operation
    // of it -- a failover among them -- goes on meanwhile.
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/tenants/acme/quota",
        r#"{"streams": 5}"#,
    );
    assert_eq!(status, 200, "{body}");
    w.links[2].heal();
    let (status, body) = dropping.join().unwrap();
    assert_eq!(status, 502, "{body}");
    let (status, body) = deleting.join().unwrap();
    assert_eq!(status, 204, "{body}");
    let (_, names) = admin(us1, "waits-us1", "GET", "/_admin/tenants", "");
    assert!(!names.contains("acme"), "{names}");
    let (_, list) = shard(w.primary, "GET", "/_shard/tenants/acme/read-replicas", "");
    assert!(list.contains(r#""read_replicas":[]"#), "{list}");
}

/// A tenant whose read replica's node does not take the copy off is kept,
/// 502, the record with it, as `DELETE .../read-replicas/<region>` keeps
/// the record: deleted regardless, the record outlived its tenant, and the
/// look that found the name made again -- another owner's tenant -- pointed
/// the old copy at it, which took the new tenant's image into a region
/// nobody had chosen.
#[test]
fn a_tenant_whose_read_replica_will_not_go_is_kept() {
    let mut primary = config(Some("eu"), None, None);
    primary.reader_timeout = Duration::from_secs(1);
    let w = OneDirectory::build("kept", false, primary, config(Some("us"), None, None));
    w.tenant("acme");
    let us1 = w._nodes[2].port;
    w.links[2].hang();
    let (status, body) = shard(w.primary, "DELETE", "/_shard/tenants/acme", "");
    assert_eq!(status, 502, "{body}");
    assert!(body.contains("is kept"), "{body}");
    let (status, body) = shard(w.primary, "GET", "/_shard/tenants/acme", "");
    assert_eq!(status, 200, "{body}");
    let (_, list) = shard(w.primary, "GET", "/_shard/tenants/acme/read-replicas", "");
    assert!(list.contains(r#""node":"us1""#), "{list}");
    write(w.primary, "acme", r#"put notes {body: "a"}"#);
    w.links[2].heal();
    let (status, body) = shard(w.primary, "DELETE", "/_shard/tenants/acme", "");
    assert_eq!(status, 204, "{body}");
    let (_, names) = admin(us1, "kept-us1", "GET", "/_admin/tenants", "");
    assert!(!names.contains("acme"), "{names}");
}

/// A name is not made again while a read replica of the tenant deleted
/// under it is kept -- by another region's directory, whose router takes
/// it off at its next look, its node silent here: made again, the name
/// was another owner's, and that look found the tenant placed and pointed
/// the old copy at it, which took the new tenant's image into a region
/// nobody had chosen. The global router asks each region keeping its own
/// directory; the name is free once the replica is gone.
#[test]
fn a_name_is_not_made_again_while_its_deleted_tenant_s_read_replica_is_kept() {
    let tag = "remade";
    let files = files(tag);
    let names = ["eu1", "eu2", "us1"];
    let nodes = names.map(|n| node(&format!("{tag}-{n}")));
    let link = Proxy::to(nodes[2].port);
    let global = global_router(&files.0.join("global.fenec"), None, "eu");
    let upstream = format!("http://127.0.0.1:{global}");
    let mut cfg = config(None, Some("us"), Some(upstream.clone()));
    // Its node silent, the look's taking the copy off waits until healed.
    cfg.reader_timeout = Duration::from_secs(600);
    let us = region_router_with(&files.0.join("us.fenec"), &upstream, cfg);
    let (status, body) = shard(
        global,
        "PUT",
        "/_shard/regions/us",
        &format!(r#"{{"routers":["http://127.0.0.1:{us}"]}}"#),
    );
    assert_eq!(status, 200, "{body}");
    for (i, (name, region)) in names.iter().zip(["eu", "eu", "us"]).enumerate() {
        let addr = match i {
            2 => link.addr.clone(),
            _ => format!("127.0.0.1:{}", nodes[i].port),
        };
        let body = format!(r#"{{"addr":"{addr}","token":"adm-{tag}-{name}","region":"{region}"}}"#);
        let (status, answer) = shard(global, "PUT", &format!("/_shard/nodes/{name}"), &body);
        assert_eq!(status, 201, "{name}: {answer}");
    }
    until("us's copy of the global directory arrives", || {
        shard(us, "GET", "/_shard/regions", "")
            .1
            .contains(r#""name":"us""#)
    });
    let schema = r#""region":"eu","schema":"create collection notes (body text)""#;
    let (status, body) = shard(
        global,
        "PUT",
        "/_shard/tenants/ab",
        &format!("{{{schema}}}"),
    );
    assert_eq!(status, 201, "{body}");
    until("us's router places ab's read replica", || {
        shard(global, "PUT", "/_shard/tenants/ab/read-replicas/us", "").0 == 201
    });
    link.hang();
    let (status, body) = shard(global, "DELETE", "/_shard/tenants/ab", "");
    assert_eq!(status, 204, "{body}");
    let again = r#"{"region":"eu","schema":"create collection secrets (x int)"}"#;
    let (status, body) = shard(global, "PUT", "/_shard/tenants/ab", again);
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("read replica"), "{body}");
    link.heal();
    until(
        "the name is free once us's router takes the replica off",
        || shard(global, "PUT", "/_shard/tenants/ab", again).0 == 201,
    );
    let (_, held) = admin(
        nodes[2].port,
        &format!("{tag}-us1"),
        "GET",
        "/_admin/tenants",
        "",
    );
    assert!(!held.contains(r#""ab""#), "{held}");
    let (_, list) = shard(global, "GET", "/_shard/tenants/ab/read-replicas", "");
    assert!(list.contains(r#""read_replicas":[]"#), "{list}");
}

/// A quota a read replica's node does not take holds up neither the quota
/// request nor the look: sent by the request and the look themselves, the
/// request waited for each such node, and every look after it was due
/// there again, a move of another tenant waiting 4.4 s behind it -- after
/// a router's restart, its sent quotas forgotten, every look, for good.
/// The node's sender waits for it, apart.
#[test]
fn a_quota_a_read_replica_s_node_does_not_take_holds_nothing_up() {
    let mut primary = config(Some("eu"), None, None);
    primary.reader_timeout = Duration::from_secs(600);
    let w = OneDirectory::build("quotas", false, primary, config(Some("us"), None, None));
    let ap1 = node("quotas-ap1");
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/nodes/ap1",
        &format!(
            r#"{{"addr":"127.0.0.1:{}","token":"adm-quotas-ap1","region":"ap"}}"#,
            ap1.port
        ),
    );
    assert_eq!(status, 201, "{body}");
    w.tenant("acme");
    w.made("beta");
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/tenants/beta/read-replicas/ap",
        "",
    );
    assert_eq!(status, 201, "{body}");
    w.links[2].hang();
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/tenants/acme/quota",
        r#"{"streams": 5}"#,
    );
    assert_eq!(status, 200, "{body}");
    w.links[0].cut();
    let (status, body) = shard(w.primary, "POST", "/_shard/nodes/eu1/failover", "");
    assert_eq!(status, 200, "{body}");
    until("beta's read replica follows its primary's failover", || {
        shard(w.primary, "GET", "/_shard/tenants/beta/read-replicas", "")
            .1
            .contains(&w.links[1].url())
    });
    w.links[2].heal();
    let us1 = w._nodes[2].port;
    until(
        "acme's quota reaches its read replica once its node answers",
        || {
            admin(us1, "quotas-us1", "GET", "/_admin/tenants/acme/quota", "")
                .1
                .contains(r#""streams":5"#)
        },
    );
}

/// A read replica is sent no quota older than one it was sent: a look's
/// own send, of the quota its round began with, landed after a quota
/// request's newer one when its change took long -- here its `follow`
/// held at the node's door across the request. One sender a node sends
/// what is wanted last.
#[test]
fn a_read_replica_is_sent_no_quota_older_than_one_it_was_sent() {
    let mut primary = config(Some("eu"), None, None);
    primary.reader_timeout = Duration::from_secs(600);
    let w = OneDirectory::build("order", false, primary, config(Some("us"), None, None));
    w.tenant("acme");
    let us1 = w._nodes[2].port;
    let holds = |n: u32| {
        admin(us1, "order-us1", "GET", "/_admin/tenants/acme/quota", "")
            .1
            .contains(&format!(r#""streams":{n}"#))
    };
    let quota = |n: u32| {
        let (status, body) = shard(
            w.primary,
            "PUT",
            "/_shard/tenants/acme/quota",
            &format!(r#"{{"streams": {n}}}"#),
        );
        assert_eq!(status, 200, "{body}");
    };
    quota(5);
    until("the read replica holds 5", || holds(5));
    w.links[2].park("/follow");
    w.links[0].cut();
    let (status, body) = shard(w.primary, "POST", "/_shard/nodes/eu1/failover", "");
    assert_eq!(status, 200, "{body}");
    until("the look's follow waits at us1's door", || {
        !w.links[2].parked("/follow").is_empty()
    });
    quota(9);
    until("the read replica holds 9", || holds(9));
    w.links[2].park("/quota");
    w.links[2].unpark("/follow");
    until("a quota is sent once the replica follows", || {
        !w.links[2].parked("/quota").is_empty()
    });
    let sent = w.links[2].parked("/quota").remove(0);
    assert!(sent.contains(r#""streams":9"#), "{sent}");
    w.links[2].unpark("/quota");
}

/// A move onto a read replica's node waits out a change of it under way,
/// and holds it until the directory places the tenant there: passed over,
/// the look's `follow` landing after the move installed the tenant on the
/// node made its new primary a follower of the node it had left, which the
/// move deleted -- and the tenant took no write again. Here the look's
/// `follow` is held at the node's door across the move.
#[test]
fn a_move_onto_a_read_replica_s_node_waits_out_a_change_of_it() {
    let mut primary = config(Some("eu"), None, None);
    primary.reader_timeout = Duration::from_secs(600);
    let w = OneDirectory::build("arrive", false, primary, config(Some("us"), None, None));
    w.tenant("acme");
    write(w.primary, "acme", r#"put notes {body: "a"}"#);
    w.replicated("acme", r#""a""#);
    w.links[2].park("/follow");
    w.links[0].cut();
    let (status, body) = shard(w.primary, "POST", "/_shard/nodes/eu1/failover", "");
    assert_eq!(status, 200, "{body}");
    until("the look's follow waits at us1's door", || {
        !w.links[2].parked("/follow").is_empty()
    });
    let at = w.primary;
    let moving = std::thread::spawn(move || {
        shard(
            at,
            "POST",
            "/_shard/tenants/acme/move",
            r#"{"to":"us1","region":"us"}"#,
        )
    });
    // Waiting, the move cannot answer while the change is under way; one
    // that answers meanwhile passed it over.
    let started = Instant::now();
    while !moving.is_finished() && started.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
    }
    w.links[2].unpark("/follow");
    let (status, body) = moving.join().unwrap();
    assert_eq!(status, 200, "{body}");
    until("no change of acme's read replicas is under way", || {
        shard(w.primary, "GET", "/_shard/tenants/acme/read-replicas", "")
            .1
            .contains(r#""changing":false"#)
    });
    let (status, _, body) = request(
        w.primary,
        "POST",
        "/t/acme/query",
        &query_body(r#"put notes {body: "b"}"#),
        &[],
    );
    assert_eq!(status, 200, "{body}");
}

/// A failover keeps the tenant's lineage -- its history id's top half --
/// and moves its term on; a tenant made again under the name starts
/// another, at term 1. A client orders positions by term within a lineage
/// alone: the made-again tenant's term 1, below the old one's, read as a
/// fenced primary's word on another branch, left every read near waiting
/// its whole wait for good.
#[test]
fn a_failover_keeps_the_tenant_s_lineage_and_a_tenant_made_again_starts_another() {
    let w = OneDirectory::new("lineage");
    w.made("acme");
    let parts = |p: &Pos| {
        let h = p.history.clone().unwrap();
        let (term, id) = h.split_once('-').unwrap();
        (term.parse::<u32>().unwrap(), id[..8].to_string())
    };
    let a = parts(&write(w.primary, "acme", r#"put notes {body: "a"}"#));
    w.replicated("acme", r#""a""#);
    w.links[0].cut();
    let (status, body) = shard(w.primary, "POST", "/_shard/nodes/eu1/failover", "");
    assert_eq!(status, 200, "{body}");
    let b = parts(&write(w.primary, "acme", r#"put notes {body: "b"}"#));
    assert_eq!(b.1, a.1, "the lineage the failover kept");
    assert!(b.0 > a.0, "{b:?} after {a:?}");
    let (status, body) = shard(w.primary, "DELETE", "/_shard/tenants/acme", "");
    assert_eq!(status, 204, "{body}");
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"node":"eu2","schema":"create collection notes (body text)"}"#,
    );
    assert_eq!(status, 201, "{body}");
    let c = parts(&write(w.primary, "acme", r#"put notes {body: "c"}"#));
    assert_eq!(c.0, 1, "{c:?}");
    assert_ne!(c.1, a.1, "a tenant made again is a lineage of its own");
}

/// A create asks only the regions that may keep a read replica of its
/// name -- those whose router said so placing one -- and one out of reach
/// refuses those names alone: every region asked, one after another, one
/// region's router out of reach refused every create in every region, a
/// partition's whole point lost. A name no region traced asks none; one a
/// region traced waits for it, 503 while it is out of reach and 409 while
/// it keeps the replica; and once the region keeps none, the name is free
/// and its trace gone, asking nothing again.
#[test]
fn a_create_asks_only_the_regions_that_may_keep_a_read_replica_of_its_name() {
    let tag = "traces";
    let files = files(tag);
    let names = ["eu1", "eu2", "us1"];
    let nodes = names.map(|n| node(&format!("{tag}-{n}")));
    let us1_link = Proxy::to(nodes[2].port);
    let global = global_router(&files.0.join("global.fenec"), None, "eu");
    let upstream = format!("http://127.0.0.1:{global}");
    let mut cfg = config(None, Some("us"), Some(upstream.clone()));
    // Its node silent, the look's taking the copy off waits until healed.
    cfg.reader_timeout = Duration::from_secs(600);
    let us = region_router_with(&files.0.join("us.fenec"), &upstream, cfg);
    let us_link = Proxy::to(us);
    let (status, body) = shard(
        global,
        "PUT",
        "/_shard/regions/us",
        &format!(r#"{{"routers":["{}"]}}"#, us_link.url()),
    );
    assert_eq!(status, 200, "{body}");
    for (i, (name, region)) in names.iter().zip(["eu", "eu", "us"]).enumerate() {
        let addr = match i {
            2 => us1_link.addr.clone(),
            _ => format!("127.0.0.1:{}", nodes[i].port),
        };
        let body = format!(r#"{{"addr":"{addr}","token":"adm-{tag}-{name}","region":"{region}"}}"#);
        let (status, answer) = shard(global, "PUT", &format!("/_shard/nodes/{name}"), &body);
        assert_eq!(status, 201, "{name}: {answer}");
    }
    until("us's copy of the global directory arrives", || {
        shard(us, "GET", "/_shard/regions", "")
            .1
            .contains(r#""name":"us""#)
    });
    let create = |t: &str| {
        let body = r#"{"region":"eu","schema":"create collection notes (body text)"}"#;
        shard(global, "PUT", &format!("/_shard/tenants/{t}"), body)
    };

    // us out of reach: a name no region traced is made all the same.
    us_link.cut();
    let (status, body) = create("cd");
    assert_eq!(status, 201, "{body}");
    us_link.heal();

    // ab's read replica placed in us, by us's router: traced there.
    let (status, body) = create("ab");
    assert_eq!(status, 201, "{body}");
    until("us's router places ab's read replica", || {
        shard(global, "PUT", "/_shard/tenants/ab/read-replicas/us", "").0 == 201
    });
    us1_link.hang();
    let (status, body) = shard(global, "DELETE", "/_shard/tenants/ab", "");
    assert_eq!(status, 204, "{body}");
    us_link.cut();
    let (status, body) = create("ab");
    assert_eq!(status, 503, "{body}");
    let (status, body) = create("ef");
    assert_eq!(status, 201, "{body}");
    us_link.heal();
    let (status, body) = create("ab");
    assert_eq!(status, 409, "{body}");

    // Its node back, us's router takes the replica off and says so.
    us1_link.heal();
    until("ab is made once us keeps no read replica of it", || {
        create("ab").0 == 201
    });
    let (status, body) = shard(global, "DELETE", "/_shard/tenants/ab", "");
    assert_eq!(status, 204, "{body}");
    us_link.cut();
    let (status, body) = create("ab");
    assert_eq!(status, 201, "the trace is gone: {body}");
}

/// A placement whose record the directory cannot write -- its disk failing
/// -- answers, the copy it made taken back off: the directory's write
/// guard was held into the error's block, whose taking the copy off reads
/// the directory, and the router waited on itself for good.
#[test]
fn a_placement_the_directory_cannot_record_answers_and_takes_its_copy_off() {
    let tag = "unrecorded";
    let files = files(tag);
    let names = ["eu1", "eu2", "us1"];
    let nodes = names.map(|n| node(&format!("{tag}-{n}")));
    let (db, repl) = directory(&files.0.join("global.fenec"), None);
    let dir = Directory::load(Arc::clone(&db)).unwrap();
    let router = serve(Router::replicated(
        dir,
        config(Some("eu"), None, None),
        repl,
    ));
    for (i, (name, region)) in names.iter().zip(["eu", "eu", "us"]).enumerate() {
        let body = format!(
            r#"{{"addr":"127.0.0.1:{}","token":"adm-{tag}-{name}","region":"{region}"}}"#,
            nodes[i].port
        );
        let (status, answer) = shard(router, "PUT", &format!("/_shard/nodes/{name}"), &body);
        assert_eq!(status, 201, "{name}: {answer}");
    }
    let (status, body) = shard(
        router,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"node":"eu1","schema":"create collection notes (body text)"}"#,
    );
    assert_eq!(status, 201, "{body}");
    db.write()
        .unwrap()
        .fail(&fenec_core::error::Error::Io("the disk is gone".into()));
    let (status, body) = shard(router, "PUT", "/_shard/tenants/acme/read-replicas/us", "");
    assert_eq!(status, 500, "{body}");
    let (_, held) = admin(
        nodes[2].port,
        &format!("{tag}-us1"),
        "GET",
        "/_admin/tenants",
        "",
    );
    assert!(!held.contains("acme"), "{held}");
    let (status, body) = shard(router, "GET", "/_shard/tenants/acme", "");
    assert_eq!(status, 200, "{body}");
}

/// A read replica's node that does not answer holds up no other's
/// re-pointing: the look waited for every call its round made, so a move
/// of one tenant -- its replica's node silent -- held the next tenant's
/// replica following the old primary for that node's whole timeout, ten
/// minutes here. A worker a node now, the look waiting for none.
#[test]
fn a_read_replica_s_silent_node_holds_up_no_other_s_re_pointing() {
    let mut primary = config(Some("eu"), None, None);
    primary.reader_timeout = Duration::from_secs(600);
    let w = OneDirectory::build("stall", false, primary, config(Some("us"), None, None));
    let ap1 = node("stall-ap1");
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/nodes/ap1",
        &format!(
            r#"{{"addr":"127.0.0.1:{}","token":"adm-stall-ap1","region":"ap"}}"#,
            ap1.port
        ),
    );
    assert_eq!(status, 201, "{body}");
    w.tenant("acme");
    w.made("beta");
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/tenants/beta/read-replicas/ap",
        "",
    );
    assert_eq!(status, 201, "{body}");
    w.links[2].hang();
    let (status, body) = shard(
        w.primary,
        "POST",
        "/_shard/tenants/acme/move",
        r#"{"to":"eu2"}"#,
    );
    assert_eq!(status, 200, "{body}");
    until(
        "the look asks us1, silent, to follow acme's new primary",
        || w.links[2].asked("/_admin/tenants/acme/follow") >= 1,
    );
    let (status, body) = shard(
        w.primary,
        "POST",
        "/_shard/tenants/beta/move",
        r#"{"to":"eu2"}"#,
    );
    assert_eq!(status, 200, "{body}");
    until(
        "beta's read replica follows it while us1 says nothing",
        || {
            shard(w.primary, "GET", "/_shard/tenants/beta/read-replicas", "")
                .1
                .contains(&w.links[1].url())
        },
    );
    w.links[2].heal();
}

/// A read replica's copy takes nothing from another tenant of its name,
/// and keeps none of the one it copied: a tenant deleted and made again on
/// the very node its copy followed -- the record forgotten by hand
/// (`?forget`, a node thought gone for good), or never written (a router
/// lost between the follow and the record, here a copy made and told to
/// follow by hand) -- had the copy's follower, reconnecting to the same
/// address, take the new one's image: another owner's data in a region
/// nobody chose for it. Told its tenant's lineage, the copy refuses a
/// primary of another; stopped and kept, it still answered the deleted
/// tenant's rows to anyone reading the node, so it deletes itself, file
/// and `.follows` (`fenec_copies_refused_total`).
#[test]
fn a_read_replica_s_copy_refusing_another_tenant_of_its_name_deletes_itself() {
    let w = OneDirectory::new("lineage-copy");
    w.tenant("acme");
    w.made("orphan");
    write(w.primary, "acme", r#"put notes {body: "old"}"#);
    write(w.primary, "orphan", r#"put notes {body: "old"}"#);
    w.holds("acme", r#""old""#);
    let us1 = w._nodes[2].port;
    // The orphan: a copy made and told to follow, with its tenant's
    // lineage, as a placement does before it records it.
    let (_, placed) = shard(
        w.primary,
        "POST",
        "/_shard/placements",
        r#"{"tenants":["orphan"]}"#,
    );
    let lineage = text_of(&placed, "lineage").expect("the router knows its lineage");
    let (status, body) = admin(us1, "lineage-copy-us1", "PUT", "/_admin/tenants/orphan", "");
    assert_eq!(status, 201, "{body}");
    let follow = format!(r#"{{"from":"{}","lineage":"{lineage}"}}"#, w.links[0].url());
    let (status, body) = admin(
        us1,
        "lineage-copy-us1",
        "POST",
        "/_admin/tenants/orphan/follow",
        &follow,
    );
    assert_eq!(status, 200, "{body}");
    until("the orphan holds the old tenant's rows", || {
        marked(us1, "orphan", "get notes select body", None)
            .1
            .contains(r#""old""#)
    });
    let (status, body) = shard(
        w.primary,
        "DELETE",
        "/_shard/tenants/acme/read-replicas/us?forget",
        "",
    );
    assert_eq!(status, 204, "{body}");
    for t in ["acme", "orphan"] {
        let (status, body) = shard(w.primary, "DELETE", &format!("/_shard/tenants/{t}"), "");
        assert_eq!(status, 204, "{body}");
        let (status, body) = shard(
            w.primary,
            "PUT",
            &format!("/_shard/tenants/{t}"),
            r#"{"node":"eu1","schema":"create collection notes (body text)"}"#,
        );
        assert_eq!(status, 201, "{body}");
        write(w.primary, t, r#"put notes {body: "another owner's"}"#);
    }
    for t in ["acme", "orphan"] {
        let mut last = (0, String::new());
        until("us1's copy deletes itself, or takes the new tenant", || {
            last = request(
                us1,
                "POST",
                &format!("/t/{t}/query"),
                &query_body("get notes select body"),
                &[],
            )
            .map_two();
            last.0 == 404 || last.1.contains("another owner's")
        });
        assert_eq!(last.0, 404, "{t}: {}", last.1);
        assert!(!w._nodes[2].dir.join(format!("{t}.follows")).exists());
    }
    let (_, held) = admin(us1, "lineage-copy-us1", "GET", "/_admin/tenants", "");
    assert!(!held.contains("acme") && !held.contains("orphan"), "{held}");
    let (_, metrics) = admin(us1, "lineage-copy-us1", "GET", "/_metrics", "");
    assert!(metrics.contains("fenec_copies_refused_total"), "{metrics}");
}

/// A read replica whose tenant's name was made again -- its record kept by
/// another region's directory, the trace let go of by hand, the new tenant
/// on the very node the old one was -- is taken off, never pointed at the
/// new one: the look found nothing to change, the address the same.
#[test]
fn a_read_replica_of_a_tenant_made_again_is_taken_off() {
    let tag = "remade-same";
    let files = files(tag);
    let names = ["eu1", "eu2", "us1"];
    let nodes = names.map(|n| node(&format!("{tag}-{n}")));
    let us1_link = Proxy::to(nodes[2].port);
    let global = global_router(&files.0.join("global.fenec"), None, "eu");
    let upstream = format!("http://127.0.0.1:{global}");
    let mut cfg = config(None, Some("us"), Some(upstream.clone()));
    cfg.reader_timeout = Duration::from_secs(600);
    let us = region_router_with(&files.0.join("us.fenec"), &upstream, cfg);
    let (status, body) = shard(
        global,
        "PUT",
        "/_shard/regions/us",
        &format!(r#"{{"routers":["http://127.0.0.1:{us}"]}}"#),
    );
    assert_eq!(status, 200, "{body}");
    for (i, (name, region)) in names.iter().zip(["eu", "eu", "us"]).enumerate() {
        let addr = match i {
            2 => us1_link.addr.clone(),
            _ => format!("127.0.0.1:{}", nodes[i].port),
        };
        let body = format!(r#"{{"addr":"{addr}","token":"adm-{tag}-{name}","region":"{region}"}}"#);
        let (status, answer) = shard(global, "PUT", &format!("/_shard/nodes/{name}"), &body);
        assert_eq!(status, 201, "{name}: {answer}");
    }
    until("us's copy of the global directory arrives", || {
        shard(us, "GET", "/_shard/regions", "")
            .1
            .contains(r#""name":"us""#)
    });
    let make = r#"{"node":"eu1","schema":"create collection notes (body text)"}"#;
    let (status, body) = shard(global, "PUT", "/_shard/tenants/ab", make);
    assert_eq!(status, 201, "{body}");
    until("us's router places ab's read replica", || {
        shard(global, "PUT", "/_shard/tenants/ab/read-replicas/us", "").0 == 201
    });
    us1_link.hang();
    let (status, body) = shard(global, "DELETE", "/_shard/tenants/ab", "");
    assert_eq!(status, 204, "{body}");
    // us out of reach for good, as far as the operator knows: its trace let
    // go of, and the name made again on the same node.
    let (status, body) = shard(global, "DELETE", "/_shard/tenants/ab/traces/us", "");
    assert_eq!(status, 204, "{body}");
    let (status, body) = shard(global, "PUT", "/_shard/tenants/ab", make);
    assert_eq!(status, 201, "{body}");
    write(global, "ab", r#"put notes {body: "another owner's"}"#);
    us1_link.heal();
    until(
        "us's router takes the old tenant's read replica off",
        || {
            let (_, list) = shard(us, "GET", "/_shard/tenants/ab/read-replicas", "");
            let (_, held) = admin(
                nodes[2].port,
                &format!("{tag}-us1"),
                "GET",
                "/_admin/tenants",
                "",
            );
            list.contains(r#""read_replicas":[]"#) && !held.contains(r#""ab""#)
        },
    );
}

/// A trace names a tenant the global directory holds: a region's router
/// whose copy of it still showed a tenant just deleted traced and placed a
/// replica of it as the name was made again.
#[test]
fn a_trace_of_a_name_no_tenant_holds_is_refused() {
    let w = OneDirectory::new("ghost");
    let (status, body) = shard(w.primary, "PUT", "/_shard/tenants/ghost/traces/us", "");
    assert_eq!(status, 404, "{body}");
    w.made("acme");
    let (status, body) = shard(w.primary, "PUT", "/_shard/tenants/acme/traces/us", "");
    assert_eq!(status, 204, "{body}");
}

/// A node's worker looks again where a tenant's primary is as it reaches
/// the tenant's replica: reached after a slow node's earlier calls, it
/// pointed the replica at the primary of the look's time -- the one a move
/// meanwhile had left. Here the worker is held at the node's door on one
/// tenant's `follow` while the other moves.
#[test]
fn a_look_s_worker_points_a_replica_at_its_primary_as_it_reaches_it() {
    let mut primary = config(Some("eu"), None, None);
    primary.reader_timeout = Duration::from_secs(600);
    let w = OneDirectory::build("late", false, primary, config(Some("us"), None, None));
    w.tenant("acme");
    w.made("beta");
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/tenants/beta/read-replicas/us",
        "",
    );
    assert_eq!(status, 201, "{body}");
    // Recorded once the tenants have their replicas, on eu2.
    let eu3 = node("late-eu3");
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/nodes/eu3",
        &format!(
            r#"{{"addr":"127.0.0.1:{}","token":"adm-late-eu3","region":"eu","zone":"eu-c"}}"#,
            eu3.port
        ),
    );
    assert_eq!(status, 201, "{body}");
    w.links[2].park("/follow");
    w.links[0].cut();
    let (status, body) = shard(w.primary, "POST", "/_shard/nodes/eu1/failover", "");
    assert_eq!(status, 200, "{body}");
    until("the worker waits at us1's door on acme's follow", || {
        !w.links[2].parked("acme/follow").is_empty()
    });
    let (status, body) = shard(
        w.primary,
        "POST",
        "/_shard/tenants/beta/move",
        r#"{"to":"eu3"}"#,
    );
    assert_eq!(status, 200, "{body}");
    w.links[2].unpark("acme/follow");
    until("the worker reaches beta", || {
        !w.links[2].parked("beta/follow").is_empty()
    });
    let sent = w.links[2].parked("beta/follow").remove(0);
    assert!(
        sent.contains(&format!("127.0.0.1:{}", eu3.port)),
        "beta's replica pointed elsewhere than eu3: {sent}"
    );
    w.links[2].unpark("/follow");
}

/// A tenant made before lineages were kept, and its read replica placed
/// then, are bound to its lineage once a router keeps them: unknown, the
/// replica was bound to nothing, and its copy took any tenant of the name.
/// The router asks the tenant's node for its lineage and records it, asks
/// the copy's node for the copy's, and where they agree records it with
/// the replica and tells the copy, which keeps it in `.follows`.
#[test]
fn a_tenant_and_its_read_replica_from_before_lineages_are_bound_to_one() {
    let tag = "legacy";
    let files = files(tag);
    let names = ["eu1", "us1"];
    let nodes = names.map(|n| node(&format!("{tag}-{n}")));
    let (eu1, us1) = (nodes[0].port, nodes[1].port);
    // The tenant and its copy, made on the nodes by hand, as a router from
    // before lineages made them: no lineage told the copy.
    let (status, body) = admin(eu1, "legacy-eu1", "PUT", "/_admin/tenants/acme", "");
    assert_eq!(status, 201, "{body}");
    for sql in [
        "create collection notes (body text)",
        r#"put notes {body: "old"}"#,
    ] {
        let (status, _, body) = request(eu1, "POST", "/t/acme/query", &query_body(sql), &[]);
        assert_eq!(status, 200, "{body}");
    }
    let (status, body) = admin(us1, "legacy-us1", "PUT", "/_admin/tenants/acme", "");
    assert_eq!(status, 201, "{body}");
    let follow = format!(r#"{{"from":"http://127.0.0.1:{eu1}"}}"#);
    let (status, body) = admin(
        us1,
        "legacy-us1",
        "POST",
        "/_admin/tenants/acme/follow",
        &follow,
    );
    assert_eq!(status, 200, "{body}");
    until("the copy holds the tenant's rows", || {
        marked(us1, "acme", "get notes select body", None)
            .1
            .contains(r#""old""#)
    });
    // The directory as such a router left it: the tenant and its read
    // replica recorded, no lineage of either.
    let (db, repl) = directory(&files.0.join("global.fenec"), None);
    let mut dir = Directory::load(Arc::clone(&db)).unwrap();
    for (name, region, port) in [("eu1", "eu", eu1), ("us1", "us", us1)] {
        let n = fenec_shard::directory::Node {
            addr: format!("127.0.0.1:{port}"),
            token: format!("adm-{tag}-{name}"),
            region: region.into(),
            ..Default::default()
        };
        dir.set_node(name, n).unwrap();
    }
    dir.place("acme", "eu1", fenec_shard::directory::State::Active)
        .unwrap();
    dir.set_reader("acme", "us1", &format!("http://127.0.0.1:{eu1}"), None)
        .unwrap();
    let router = serve(Router::replicated(
        dir,
        config(Some("eu"), None, None),
        repl,
    ));

    let mut lineage = None;
    until("the router has the tenant's lineage from its node", || {
        let (_, placed) = shard(
            router,
            "POST",
            "/_shard/placements",
            r#"{"tenants":["acme"]}"#,
        );
        lineage = text_of(&placed, "lineage");
        lineage.is_some()
    });
    let lineage = lineage.unwrap();
    until("the read replica is bound to it", || {
        shard(router, "GET", "/_shard/tenants/acme/read-replicas", "")
            .1
            .contains(&format!(r#""lineage":"{lineage}""#))
    });
    let follows = std::fs::read_to_string(nodes[1].dir.join("acme.follows")).unwrap();
    assert!(follows.contains(&format!("lineage {lineage}")), "{follows}");
}

/// A tenant's node and the router of a single directory near `us`, eu1
/// reached through a proxy and us1 holding read replicas: for the tests
/// of a primary of another lineage than the directory's.
struct Near {
    eu1: Node,
    us1: Node,
    link: Proxy,
    us_link: Proxy,
    router: Arc<Router>,
    port: u16,
}

impl Near {
    fn new(tag: &str, reader_timeout: Duration) -> Near {
        Near::with(tag, |cfg| cfg.reader_timeout = reader_timeout)
    }

    /// The same, the router configured by `set`.
    fn with(tag: &str, set: impl FnOnce(&mut Config)) -> Near {
        let eu1 = node(&format!("{tag}-eu1"));
        let us1 = node(&format!("{tag}-us1"));
        let link = Proxy::to(eu1.port);
        let us_link = Proxy::to(us1.port);
        let mut cfg = config(Some("us"), None, None);
        set(&mut cfg);
        let router = Router::new(Directory::in_memory(), cfg);
        let port = serve(Arc::clone(&router));
        for (name, addr, region) in [
            ("eu1", link.addr.clone(), "eu"),
            ("us1", us_link.addr.clone(), "us"),
        ] {
            let body =
                format!(r#"{{"addr":"{addr}","token":"adm-{tag}-{name}","region":"{region}"}}"#);
            let (status, answer) = shard(port, "PUT", &format!("/_shard/nodes/{name}"), &body);
            assert_eq!(status, 201, "{name}: {answer}");
        }
        Near {
            eu1,
            us1,
            link,
            us_link,
            router,
            port,
        }
    }

    /// `tenant` made on eu1, a row in it, and its read replica on us1
    /// holding the row.
    fn tenant(&self, tenant: &str) -> String {
        let make = r#"{"node":"eu1","schema":"create collection notes (body text)"}"#;
        let (status, body) = shard(self.port, "PUT", &format!("/_shard/tenants/{tenant}"), make);
        assert_eq!(status, 201, "{body}");
        write(self.eu1.port, tenant, r#"put notes {body: "before"}"#);
        let (status, body) = shard(
            self.port,
            "PUT",
            &format!("/_shard/tenants/{tenant}/read-replicas/us"),
            "",
        );
        assert_eq!(status, 201, "{body}");
        until("the read replica holds the row", || {
            marked(self.us1.port, tenant, "get notes select body", None)
                .1
                .contains(r#""before""#)
        });
        self.lineage(tenant)
            .expect("a lineage recorded at its create")
    }

    fn lineage(&self, tenant: &str) -> Option<String> {
        let body = format!(r#"{{"tenants":["{tenant}"]}}"#);
        text_of(
            &shard(self.port, "POST", "/_shard/placements", &body).1,
            "lineage",
        )
    }

    fn placed(&self, tenant: &str) -> String {
        let body = format!(r#"{{"tenants":["{tenant}"]}}"#);
        shard(self.port, "POST", "/_shard/placements", &body).1
    }

    fn listed(&self, tenant: &str) -> String {
        let target = format!("/_shard/tenants/{tenant}/read-replicas");
        shard(self.port, "GET", &target, "").1
    }

    /// `tenant` deleted on eu1 and made there again, behind the router:
    /// a file of another lineage in its place, as one restored or put
    /// there by hand is.
    fn swapped(&self, tag: &str, tenant: &str) {
        let tok = format!("{tag}-eu1");
        let (status, body) = admin(
            self.eu1.port,
            &tok,
            "DELETE",
            &format!("/_admin/tenants/{tenant}"),
            "",
        );
        assert_eq!(status, 204, "{body}");
        let (status, body) = admin(
            self.eu1.port,
            &tok,
            "PUT",
            &format!("/_admin/tenants/{tenant}"),
            "",
        );
        assert_eq!(status, 201, "{body}");
        write(self.eu1.port, tenant, "create collection notes (body text)");
        write(self.eu1.port, tenant, r#"put notes {body: "swapped"}"#);
    }
}

/// A copy's delete after it refused a primary of another lineage is held
/// to the copy that refused: one promoted meanwhile -- a primary now --
/// and a tenant installed under the name in its place, as a move installs
/// one, are kept. The delete asked whether the copy still followed before
/// it took the slot, and either landing between the two had the primary
/// deleted. Each refusal is held there here (`refusals::hold`) while the
/// promotion and the install land.
#[test]
fn a_refused_copy_promoted_or_installed_anew_before_its_delete_is_kept() {
    let tag = "refused-held";
    let eu1 = node(&format!("{tag}-eu1"));
    let us1 = node(&format!("{tag}-us1"));
    let (ea, ua) = (format!("{tag}-eu1"), format!("{tag}-us1"));
    let from = format!("http://127.0.0.1:{}", eu1.port);
    let names = ["refused-promoted", "refused-installed"];
    for t in names {
        let (status, body) = admin(eu1.port, &ea, "PUT", &format!("/_admin/tenants/{t}"), "");
        assert_eq!(status, 201, "{body}");
        write(eu1.port, t, "create collection notes (body text)");
        write(eu1.port, t, r#"put notes {body: "theirs"}"#);
        refusals::hold(t);
        let (status, body) = admin(us1.port, &ua, "PUT", &format!("/_admin/tenants/{t}"), "");
        assert_eq!(status, 201, "{body}");
        // Told a lineage eu1's tenant is not of: it refuses at the hello.
        let follow = format!(r#"{{"from":"{from}","lineage":"00000001"}}"#);
        let target = format!("/_admin/tenants/{t}/follow");
        let (status, body) = admin(us1.port, &ua, "POST", &target, &follow);
        assert_eq!(status, 200, "{body}");
        let held = refusals::wait_for(t, Stage::Waiting, Duration::from_secs(60));
        assert!(held, "{t}'s copy never refused");
    }
    // Promoted: a primary of its own now, taking writes.
    let target = "/_admin/tenants/refused-promoted/promote";
    let (status, body) = admin(us1.port, &ua, "POST", target, "");
    assert_eq!(status, 200, "{body}");
    write(
        us1.port,
        "refused-promoted",
        "create collection notes (body text)",
    );
    write(us1.port, "refused-promoted", r#"put notes {body: "kept"}"#);
    // Deleted, and installed again from eu1's image as a move installs one.
    let target = "/_admin/tenants/refused-installed";
    let (status, body) = admin(us1.port, &ua, "DELETE", target, "");
    assert_eq!(status, 204, "{body}");
    let file = "/_admin/tenants/refused-installed/file";
    let (status, image) = admin_bytes(eu1.port, &ea, "GET", file, b"");
    assert_eq!(status, 200);
    let (status, body) = admin_bytes(us1.port, &ua, "PUT", file, &image);
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    for t in names {
        refusals::release(t);
        assert!(
            refusals::wait_for(t, Stage::Done, Duration::from_secs(60)),
            "{t}"
        );
    }
    for (t, row) in [
        ("refused-promoted", "kept"),
        ("refused-installed", "theirs"),
    ] {
        let (status, _, body) = request(
            us1.port,
            "POST",
            &format!("/t/{t}/query"),
            &query_body("get notes select body"),
            &[],
        );
        assert_eq!(status, 200, "{t}: {body}");
        assert!(body.contains(&format!("\"{row}\"")), "{t}: {body}");
    }
}

/// A failover onto a standby that never took its tenant's first image
/// promotes a file of a lineage of its own: the promoted primary is the
/// tenant, recorded with the lineage the promotion answers, and its read
/// replica is bound to it and follows it, its copy refusing nothing. Bound
/// to the lineage before, its copy refused the promoted primary and deleted
/// itself, was made again by the next look and refused again: a delete
/// every look, for good.
#[test]
fn a_read_replica_follows_a_primary_promoted_from_a_copy_that_never_synced() {
    let tag = "never-synced";
    let eu1 = node(&format!("{tag}-eu1"));
    let to_standby = Proxy::to(eu1.port);
    let eu2 = node_standby(&format!("{tag}-eu2"), &to_standby.url());
    let us1 = node(&format!("{tag}-us1"));
    let link = Proxy::to(eu1.port);
    let port = serve(Router::new(
        Directory::in_memory(),
        config(Some("us"), None, None),
    ));
    for (name, body) in [
        (
            "eu2",
            format!(
                r#"{{"addr":"127.0.0.1:{}","token":"adm-{tag}-eu2","region":"eu"}}"#,
                eu2.port
            ),
        ),
        (
            "eu1",
            format!(
                r#"{{"addr":"{}","token":"adm-{tag}-eu1","region":"eu","standby":"eu2"}}"#,
                link.addr
            ),
        ),
        (
            "us1",
            format!(
                r#"{{"addr":"127.0.0.1:{}","token":"adm-{tag}-us1","region":"us"}}"#,
                us1.port
            ),
        ),
    ] {
        let (status, answer) = shard(port, "PUT", &format!("/_shard/nodes/{name}"), &body);
        assert_eq!(status, 201, "{name}: {answer}");
    }
    // The standby takes nothing of eu1's: its copy of the tenant never
    // has its first image. Any refusal of the read replica's copy held
    // where it would delete itself: none is to happen.
    to_standby.cut();
    refusals::hold("unsynced");
    let make = r#"{"node":"eu1","schema":"create collection notes (body text)"}"#;
    let (status, body) = shard(port, "PUT", "/_shard/tenants/unsynced", make);
    assert_eq!(status, 201, "{body}");
    let (status, body) = shard(port, "PUT", "/_shard/tenants/unsynced/read-replicas/us", "");
    assert_eq!(status, 201, "{body}");
    let placed = |port| {
        shard(
            port,
            "POST",
            "/_shard/placements",
            r#"{"tenants":["unsynced"]}"#,
        )
        .1
    };
    let before = text_of(&placed(port), "lineage").expect("its lineage");
    until("the read replica takes its tenant's image", || {
        let (status, body) = admin(
            us1.port,
            &format!("{tag}-us1"),
            "GET",
            "/_admin/tenants/unsynced/lineage",
            "",
        );
        status == 200
            && body.contains(&format!(r#""lineage":"{before}""#))
            && body.contains(r#""ready":true"#)
    });
    link.cut();
    let (status, body) = shard(port, "POST", "/_shard/nodes/eu1/failover", "");
    assert_eq!(status, 200, "{body}");
    let after = text_of(&placed(port), "lineage").expect("its lineage");
    assert_ne!(after, before, "the standby's own lineage");
    assert!(
        placed(port).contains(&format!(r#""was":["{before}"]"#)),
        "{}",
        placed(port)
    );
    let eu2_url = format!("http://127.0.0.1:{}", eu2.port);
    until(
        "the read replica follows the promoted primary, bound to its lineage",
        || {
            let listed = shard(port, "GET", "/_shard/tenants/unsynced/read-replicas", "").1;
            listed.contains(&format!(r#""lineage":"{after}""#)) && listed.contains(&eu2_url)
        },
    );
    until("its copy holds the promoted primary's history", || {
        let (status, body) = admin(
            us1.port,
            &format!("{tag}-us1"),
            "GET",
            "/_admin/tenants/unsynced/lineage",
            "",
        );
        status == 200
            && body.contains(&format!(r#""lineage":"{after}""#))
            && body.contains(r#""ready":true"#)
    });
    let refused = refusals::wait_for("unsynced", Stage::Waiting, Duration::ZERO);
    refusals::release("unsynced");
    assert!(
        !refused,
        "the read replica's copy refused the promoted primary"
    );
}

/// A tenant's file put in its place on its node by hand -- an older one
/// restored, another swapped in -- is the tenant: every read and write of
/// it goes there. The router asks the node which lineage it is of once
/// the read replica's copy is lost, records it, the one before kept as one
/// the tenant was, and binds the replica to it. Bound to the lineage
/// before, the copy refused the file and deleted itself at every look.
#[test]
fn a_read_replica_follows_its_tenant_s_file_put_in_place_by_hand() {
    let tag = "put-in-place";
    let w = Near::new(tag, readers_call());
    let before = w.tenant("acme");
    w.swapped(tag, "acme");
    until("the read replica holds the file put in place", || {
        // A marked read through the router finds the copy gone and has the
        // look come now, as the check every few seconds does.
        let _ = marked(w.port, "acme", "get notes select body", None);
        marked(w.us1.port, "acme", "get notes select body", None)
            .1
            .contains(r#""swapped""#)
    });
    let after = w.lineage("acme").expect("its lineage");
    assert_ne!(after, before);
    let placed = w.placed("acme");
    assert!(
        placed.contains(&format!(r#""was":["{before}"]"#)),
        "{placed}"
    );
    let listed = w.listed("acme");
    assert!(
        listed.contains(&format!(r#""lineage":"{after}""#)),
        "{listed}"
    );
}

/// A read replica whose copy is lost again and again -- refusing a
/// primary of a lineage the router cannot learn, its node not answering
/// what it is of -- is made again three times in a row, and then no more:
/// left recorded and without a copy, said in its list and counted, and
/// passed over by the marked reads of its region. Made again at every look,
/// its copy refused and deleted itself for good; given up on and still
/// asked, every marked read went to its node to be told the tenant was not
/// there.
#[test]
fn a_read_replica_whose_copy_keeps_refusing_is_given_up_on() {
    let tag = "given-up";
    let w = Near::new(tag, Duration::from_millis(300));
    w.tenant("acme");
    // What its file is of is asked of eu1 and never answered.
    w.link.park("acme/lineage");
    w.swapped(tag, "acme");
    until("the read replica is given up on", || {
        let _ = marked(w.port, "acme", "get notes select body", None);
        w.listed("acme").contains(r#""given_up":true"#)
    });
    let metrics = shard(w.port, "GET", "/_metrics", "").1;
    let given_up = metrics
        .lines()
        .find(|l| l.starts_with("fenec_router_read_replicas_given_up "))
        .and_then(|l| l.rsplit(' ').next()?.parse::<u64>().ok());
    assert!(given_up.is_some_and(|n| n >= 1), "{metrics}");
    // Its region's marked reads go home, its node not asked.
    w.us_link.park("a request no one sends");
    for _ in 0..5 {
        let (status, body) = marked(w.port, "acme", "get notes select body", None);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains(r#""swapped""#), "{body}");
    }
    assert!(
        w.us_link.seen("/t/acme/").is_empty(),
        "{:?}",
        w.us_link.seen("/t/acme/")
    );
    // Placed again by an operator, it is a replica as any.
    let (status, body) = shard(
        w.port,
        "DELETE",
        "/_shard/tenants/acme/read-replicas/us",
        "",
    );
    assert_eq!(status, 204, "{body}");
    assert!(!w.listed("acme").contains("given_up"));
    w.link.unpark("acme/lineage");
    drop(w.router);
}

/// A read replica placed before its tenant's lineage was known, whose copy
/// has not taken its first image as the lineage is backfilled, stays, and
/// is bound once it has: a copy not caught up is of a lineage of its own,
/// a new file's, and was taken for another tenant's -- a healthy replica
/// taken off as it caught up.
#[test]
fn a_read_replica_not_caught_up_is_bound_once_it_is() {
    let tag = "bind-unsynced";
    let eu1 = node(&format!("{tag}-eu1"));
    let us1 = node(&format!("{tag}-us1"));
    let (to_eu1, to_us1) = (Proxy::to(eu1.port), Proxy::to(us1.port));
    let (status, body) = admin(
        eu1.port,
        &format!("{tag}-eu1"),
        "PUT",
        "/_admin/tenants/acme",
        "",
    );
    assert_eq!(status, 201, "{body}");
    write(eu1.port, "acme", "create collection notes (body text)");
    let mut dir = Directory::in_memory();
    for (name, addr, region) in [("eu1", &to_eu1.addr, "eu"), ("us1", &to_us1.addr, "us")] {
        let n = fenec_shard::directory::Node {
            addr: addr.clone(),
            token: format!("adm-{tag}-{name}"),
            region: region.into(),
            ..Default::default()
        };
        dir.set_node(name, n).unwrap();
    }
    dir.place("acme", "eu1", fenec_shard::directory::State::Active)
        .unwrap();
    let port = serve(Router::new(dir, config(Some("us"), None, None)));
    // Its copy's stream held: it never takes its first image meanwhile.
    to_eu1.park("acme/_replication");
    to_us1.park("a request no one sends");
    let (status, body) = shard(port, "PUT", "/_shard/tenants/acme/read-replicas/us", "");
    assert_eq!(status, 201, "{body}");
    let mut lineage = None;
    until("the router has the tenant's lineage from its node", || {
        let placed = shard(
            port,
            "POST",
            "/_shard/placements",
            r#"{"tenants":["acme"]}"#,
        )
        .1;
        lineage = text_of(&placed, "lineage");
        lineage.is_some()
    });
    let lineage = lineage.unwrap();
    until("the router asks the copy what it is of", || {
        !to_us1.seen("acme/lineage").is_empty()
    });
    to_eu1.unpark("acme/_replication");
    until("the read replica is bound to its tenant's lineage", || {
        shard(port, "GET", "/_shard/tenants/acme/read-replicas", "")
            .1
            .contains(&format!(r#""lineage":"{lineage}""#))
    });
}

/// The lineage a node said is recorded only where the directory still
/// has none: a tenant deleted and made again while its node was asked
/// records its own at its create, which the answer, of the one before,
/// wrote over -- every read replica placed after bound to a lineage no
/// primary is of. Here the node's answer is held until the name is made
/// again.
#[test]
fn a_lineage_backfilled_late_writes_over_none_recorded_since() {
    let tag = "backfill-late";
    let eu1 = node(&format!("{tag}-eu1"));
    let link = Proxy::to(eu1.port);
    let (status, body) = admin(
        eu1.port,
        &format!("{tag}-eu1"),
        "PUT",
        "/_admin/tenants/acme",
        "",
    );
    assert_eq!(status, 201, "{body}");
    let mut dir = Directory::in_memory();
    let n = fenec_shard::directory::Node {
        addr: link.addr.clone(),
        token: format!("adm-{tag}-eu1"),
        region: "eu".into(),
        ..Default::default()
    };
    dir.set_node("eu1", n).unwrap();
    dir.place("acme", "eu1", fenec_shard::directory::State::Active)
        .unwrap();
    let router = Router::new(dir, config(Some("eu"), None, None));
    let port = serve(Arc::clone(&router));
    link.park_answers("acme/lineage");
    let placed = |port| {
        shard(
            port,
            "POST",
            "/_shard/placements",
            r#"{"tenants":["acme"]}"#,
        )
        .1
    };
    assert_eq!(text_of(&placed(port), "lineage"), None);
    until("the node's answer is held", || {
        link.answers_held("acme/lineage") > 0
    });
    let asked = router.lineages_asked();
    let (status, body) = shard(port, "DELETE", "/_shard/tenants/acme", "");
    assert_eq!(status, 204, "{body}");
    let (status, body) = shard(port, "PUT", "/_shard/tenants/acme", r#"{"node":"eu1"}"#);
    assert_eq!(status, 201, "{body}");
    let made = text_of(&placed(port), "lineage").expect("recorded at its create");
    link.unpark_answers("acme/lineage");
    until("the backfill takes the answer held", || {
        router.lineages_asked() > asked
    });
    assert_eq!(text_of(&placed(port), "lineage"), Some(made.clone()));
    let (_, body) = admin(
        eu1.port,
        &format!("{tag}-eu1"),
        "GET",
        "/_admin/tenants/acme/lineage",
        "",
    );
    assert!(body.contains(&format!(r#""lineage":"{made}""#)), "{body}");
}

/// The router's own bound on a call of a read replica's upkeep.
fn readers_call() -> Duration {
    fenec_shard::readers::CALL
}

/// A marked read slower at its read replica than `--replica-timeout`
/// lets that tenant's copy be, not the node: every other tenant's marked
/// reads of the node are answered by their replicas still. A replica's
/// head comes once its query has run, so a read slower than the timeout
/// looks just like a node that hangs, and resting the node sent every
/// tenant's marked reads of the region home -- kept so by one slow read
/// every few seconds. The node answers its `/_health`, asked on a thread
/// of the router's own once the read timed out, which tells the two
/// apart. Here the slow read's answer is held at the node's door past the
/// timeout.
#[test]
fn a_slow_marked_read_lets_its_tenant_s_copy_be_and_no_other() {
    let tag = "slow-read";
    let w = Near::with(tag, |cfg| {
        cfg.replica_timeout = Duration::from_secs(1);
        cfg.replica_rest = Duration::from_secs(600);
    });
    w.tenant("slow");
    w.tenant("other");
    w.us_link.park_answers("/t/slow/query");
    let (status, body) = marked(w.port, "slow", "get notes select body", None);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""before""#), "answered home: {body}");
    until("the node's health answered", || {
        w.router.health_questions() == 0
    });
    assert_eq!(w.us_link.seen("GET /_health").len(), 1, "its health asked");
    assert!(!w.router.replica_node_resting("us1"));
    for _ in 0..3 {
        let (status, body) = marked(w.port, "other", "get notes select body", None);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains(r#""before""#), "{body}");
    }
    assert_eq!(
        w.us_link.seen("/t/other/query").len(),
        3,
        "other's reads answered by its replica"
    );
    // The slow tenant's copy is let be: its next read goes home at once.
    let (status, body) = marked(w.port, "slow", "get notes select body", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(w.us_link.seen("/t/slow/query").len(), 1, "the copy let be");
    w.us_link.unpark_answers("/t/slow/query");
}

/// A replica node that takes connections and answers nothing -- stopped,
/// deadlocked: the kernel takes the request, and the head never comes --
/// is let be once the first read has waited it out: its `/_health`, asked
/// on a thread of the router's own while that read goes home, goes
/// unanswered too. Rested a tenant's copy at a time, every tenant with a
/// copy there waited `--replica-timeout` once a rest, for as long as the
/// node hung.
#[test]
fn a_hung_replica_node_is_let_be_by_the_first_read_that_waits_it_out() {
    let tag = "hung-node";
    let w = Near::with(tag, |cfg| {
        cfg.replica_timeout = Duration::from_secs(1);
        cfg.replica_rest = Duration::from_secs(600);
    });
    for t in ["a", "b"] {
        w.tenant(t);
    }
    w.us_link.hang();
    let (status, body) = marked(w.port, "a", "get notes select body", None);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""before""#), "answered home: {body}");
    assert_eq!(w.us_link.asked("/t/a/"), 1, "a waited it out");
    until("the node let be", || w.router.replica_node_resting("us1"));
    until("the node's health asked", || {
        w.us_link.asked("GET /_health") == 1
    });
    // Another tenant's read goes home without asking the node.
    let (status, body) = marked(w.port, "b", "get notes select body", None);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""before""#), "{body}");
    assert_eq!(w.us_link.asked("/t/b/"), 0, "the node let be");
    w.us_link.heal();
}

/// Two tenants' marked reads slower at their read replicas than
/// `--replica-timeout` let those two copies be, never the node: it answers
/// its `/_health`, so a third tenant's marked reads are answered by its
/// replica still. Counted by tenants -- the node rested once a second
/// tenant's copy was -- a client holding two tenants' tokens, or two
/// tenants' slow analytics, sent every tenant's marked reads of the node
/// home.
#[test]
fn two_tenants_slow_reads_leave_a_third_s_answered_by_its_replica() {
    let tag = "two-slow";
    let w = Near::with(tag, |cfg| {
        cfg.replica_timeout = Duration::from_secs(1);
        cfg.replica_rest = Duration::from_secs(600);
    });
    for t in ["slow1", "slow2", "third"] {
        w.tenant(t);
    }
    for t in ["slow1", "slow2"] {
        w.us_link.park_answers(&format!("/t/{t}/query"));
    }
    for t in ["slow1", "slow2"] {
        let (status, body) = marked(w.port, t, "get notes select body", None);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains(r#""before""#), "answered home: {body}");
    }
    until("the node's health answered", || {
        w.router.health_questions() == 0
    });
    assert!(
        !w.us_link.seen("GET /_health").is_empty(),
        "its health asked"
    );
    assert!(!w.router.replica_node_resting("us1"));
    for _ in 0..3 {
        let (status, body) = marked(w.port, "third", "get notes select body", None);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains(r#""before""#), "{body}");
    }
    assert_eq!(
        w.us_link.seen("/t/third/query").len(),
        3,
        "third's reads answered by its replica"
    );
    for t in ["slow1", "slow2"] {
        w.us_link.unpark_answers(&format!("/t/{t}/query"));
    }
}

/// Marked reads timing out together on a node that hangs ask its
/// `/_health` once: one question a node at a time, on a thread of the
/// router's own, and none once it rests. Asked on each read's path, every
/// one of them waited the question out past its timeout and asked it
/// again. The replica timeout is three seconds, so every read reaches the
/// node before the first times out.
#[test]
fn reads_timing_out_together_on_a_hung_node_ask_its_health_once() {
    const READS: usize = 8;
    let tag = "hung-many";
    let w = Near::with(tag, |cfg| {
        cfg.replica_timeout = Duration::from_secs(3);
        cfg.replica_rest = Duration::from_secs(600);
    });
    w.tenant("a");
    w.us_link.hang();
    let port = w.port;
    let reads: Vec<_> = (0..READS)
        .map(|_| std::thread::spawn(move || marked(port, "a", "get notes select body", None)))
        .collect();
    for r in reads {
        let (status, body) = r.join().unwrap();
        assert_eq!(status, 200, "{body}");
        assert!(body.contains(r#""before""#), "answered home: {body}");
    }
    assert_eq!(w.us_link.asked("/t/a/"), READS, "every read waited it out");
    until("the node let be", || {
        w.router.replica_node_resting("us1") && w.router.health_questions() == 0
    });
    until("the node's health asked", || {
        w.us_link.asked("GET /_health") >= 1
    });
    assert_eq!(w.us_link.asked("GET /_health"), 1, "asked once");
    w.us_link.heal();
}

/// A marked read whose body is past what the sockets between the router
/// and its replica hold, sent to a node that stopped reading, goes home by
/// `--replica-timeout`: its write was bounded by the pool's timeout alone
/// (the upstream timeout, a minute here), the connect and the head's reads
/// by the replica's.
#[test]
fn a_marked_read_whose_body_the_replica_does_not_read_goes_home_by_the_replica_timeout() {
    let tag = "unread-body";
    let w = Near::with(tag, |cfg| {
        cfg.replica_timeout = Duration::from_secs(1);
        cfg.replica_rest = Duration::from_secs(600);
    });
    w.tenant("a");
    w.us_link.hang();
    // 24 MB, past loopback's buffers on both ends; the node takes 64.
    let pad = "x".repeat(24 << 20);
    let body = format!(r#"{{"query":"get notes select body","pad":"{pad}"}}"#);
    // A client of its own: `request`'s refusal would print the body.
    let mut s = TcpStream::connect(("127.0.0.1", w.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let started = Instant::now();
    write!(
        s,
        "POST /t/a/query HTTP/1.1\r\nHost: x\r\nConnection: close\r\nFenec-Read: replica\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut answer = String::new();
    let read = s.read_to_string(&mut answer);
    let took = started.elapsed();
    assert!(read.is_ok(), "{read:?} after {took:?}");
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.contains(r#""before""#), "answered home: {answer}");
    assert_eq!(w.us_link.asked("/t/a/"), 1, "the replica asked");
    // Half the upstream timeout it would have waited.
    assert!(took < Duration::from_secs(30), "{took:?}");
    w.us_link.heal();
}

/// A read replica that breaks its answer off mid-body has the read go
/// home, as one that failed its head does: the answer is read whole
/// before a byte of it goes to the client, which was answered 502.
#[test]
fn a_replica_s_answer_broken_off_mid_body_goes_home() {
    let tag = "broken-body";
    let w = Near::new(tag, readers_call());
    w.tenant("acme");
    w.us_link.cut_answers("/t/acme/query");
    let (status, body) = marked(w.port, "acme", "get notes select body", None);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""before""#), "{body}");
    assert_eq!(
        w.us_link.seen("/t/acme/query").len(),
        1,
        "the replica asked"
    );
}

/// A tenant's quota reaches its read replicas, as its standby and its
/// replica: set before one is placed, and after.
#[test]
fn a_tenant_s_quota_reaches_its_read_replicas() {
    let w = OneDirectory::new("quota");
    w.made("acme");
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/tenants/acme/quota",
        r#"{"streams": 7}"#,
    );
    assert_eq!(status, 200, "{body}");
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/tenants/acme/read-replicas/us",
        "",
    );
    assert_eq!(status, 201, "{body}");
    let us1 = w._nodes[2].port;
    let (_, report) = admin(us1, "quota-us1", "GET", "/_admin/tenants/acme/quota", "");
    assert!(report.contains(r#""streams":7"#), "{report}");
    let (status, body) = shard(
        w.primary,
        "PUT",
        "/_shard/tenants/acme/quota",
        r#"{"streams": 9}"#,
    );
    assert_eq!(status, 200, "{body}");
    // Sent by the node's sender, the request waiting on none of them.
    until("the new quota reaches the read replica", || {
        admin(us1, "quota-us1", "GET", "/_admin/tenants/acme/quota", "")
            .1
            .contains(r#""streams":9"#)
    });
}

/// A copy is ready to answer marked reads once its primary has said hello
/// since it opened -- and from its promotion, whatever it heard. A node
/// pair's standby keeps no record of having heard from its primary, so
/// after a restart its copies wait for a hello; one promoted before it came
/// -- here, the standby never reaching its primary at all -- answered every
/// marked read 503 for as long as it stayed open, at the tenant's home.
#[test]
fn a_promoted_copy_answers_marked_reads_though_its_primary_never_said_hello() {
    let tag = "promoted";
    let files = files(tag);
    let eu1 = node(&format!("{tag}-eu1"));
    // The standby's own link to eu1, and the router's.
    let own = Proxy::to(eu1.port);
    let eu2 = node_standby(&format!("{tag}-eu2"), &own.url());
    let link = Proxy::to(eu1.port);
    let router = global_router(&files.0.join("global.fenec"), None, "eu");
    let body = format!(
        r#"{{"addr":"127.0.0.1:{}","token":"adm-{tag}-eu2","region":"eu"}}"#,
        eu2.port
    );
    let (status, answer) = shard(router, "PUT", "/_shard/nodes/eu2", &body);
    assert_eq!(status, 201, "{answer}");
    let body = format!(
        r#"{{"addr":"{}","token":"adm-{tag}-eu1","region":"eu","standby":"eu2"}}"#,
        link.addr
    );
    let (status, answer) = shard(router, "PUT", "/_shard/nodes/eu1", &body);
    assert_eq!(status, 201, "{answer}");
    own.cut();
    let (status, answer) = shard(
        router,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"node":"eu1","schema":"create collection notes (body text)"}"#,
    );
    assert_eq!(status, 201, "{answer}");
    link.cut();
    let (status, answer) = shard(router, "POST", "/_shard/nodes/eu1/failover", "");
    assert_eq!(status, 200, "{answer}");
    assert!(answer.contains(r#""to":"eu2""#), "{answer}");
    let (status, _, answer) = request(
        router,
        "POST",
        "/t/acme/query",
        &query_body("create collection if not exists notes (body text)"),
        &[],
    );
    assert_eq!(status, 200, "{answer}");
    let x = write(router, "acme", r#"put notes {body: "x"}"#);
    let (status, answer) = marked(router, "acme", "get notes select body", Some(&x));
    assert_eq!(status, 200, "{answer}");
    assert!(answer.contains(r#""x""#), "{answer}");
}

/// The router's check of what each read replica's node holds runs apart
/// from its looks: in the look, every node out of reach held each look --
/// the re-pointing after a failover among them -- for as long as its call
/// was allowed. Here that is ten minutes, a read replica's node in ap hangs
/// on the check, and the one in us is pointed at the new primary all the
/// same.
#[test]
fn a_read_replica_is_re_pointed_while_another_s_node_holds_up_the_check() {
    let mut primary = config(Some("eu"), None, None);
    primary.reader_timeout = Duration::from_secs(600);
    let w = OneDirectory::build("check", false, primary, config(Some("us"), None, None));
    w.tenant("acme");
    let ap1 = node("check-ap1");
    let ap = Proxy::to(ap1.port);
    let body = format!(
        r#"{{"addr":"{}","token":"adm-check-ap1","region":"ap"}}"#,
        ap.addr
    );
    let (status, answer) = shard(w.primary, "PUT", "/_shard/nodes/ap1", &body);
    assert_eq!(status, 201, "{answer}");
    let (status, answer) = shard(
        w.primary,
        "PUT",
        "/_shard/tenants/acme/read-replicas/ap",
        "",
    );
    assert_eq!(status, 201, "{answer}");
    ap.hang();
    until("the check is held up on ap1", || {
        ap.asked("GET /_admin/tenants") >= 1
    });
    w.links[0].cut();
    let (status, answer) = shard(w.primary, "POST", "/_shard/nodes/eu1/failover", "");
    assert_eq!(status, 200, "{answer}");
    let pointed = format!(r#""node":"us1","follows":"{}""#, w.links[1].url());
    until("us1's read replica follows the new primary", || {
        shard(w.primary, "GET", "/_shard/tenants/acme/read-replicas", "")
            .1
            .contains(&pointed)
    });
    ap.heal();
}

/// A copy pointed at its primary again -- the node it follows already --
/// answers no marked read until that node has said hello: the tenant there
/// may be another than the one it heard, deleted and made again while the
/// copy was cut off from it, and a router placing a read replica over a file
/// it found on the node asks for no more than this follow. It answered the
/// old tenant's rows to every read naming no write.
#[test]
fn a_copy_told_to_follow_again_answers_once_its_primary_has_said_hello() {
    let w = OneDirectory::new("again");
    w.tenant("acme");
    write(w.us, "acme", r#"put notes {body: "old"}"#);
    w.holds("acme", "old");
    // us1 follows home through a link of its own, then cut off from it.
    let us1 = w._nodes[2].port;
    let own = Proxy::to(w._nodes[0].port);
    let follow = format!(r#"{{"from":"{}"}}"#, own.url());
    let (status, body) = admin(
        us1,
        "again-us1",
        "POST",
        "/_admin/tenants/acme/follow",
        &follow,
    );
    assert_eq!(status, 200, "{body}");
    w.holds("acme", "old");
    own.cut();
    // The tenant made again at home, in place of the one us1 holds.
    let eu1 = w._nodes[0].port;
    let (status, body) = admin(eu1, "again-eu1", "DELETE", "/_admin/tenants/acme", "");
    assert_eq!(status, 204, "{body}");
    let (status, body) = admin(eu1, "again-eu1", "PUT", "/_admin/tenants/acme", "");
    assert_eq!(status, 201, "{body}");
    let (status, _, body) = request(
        eu1,
        "POST",
        "/t/acme/query",
        &query_body("create collection notes (body text)"),
        &[],
    );
    assert_eq!(status, 200, "{body}");
    let (status, _, body) = request(
        eu1,
        "POST",
        "/t/acme/query",
        &query_body(r#"put notes {body: "new"}"#),
        &[],
    );
    assert_eq!(status, 200, "{body}");
    // Told to follow the node it follows: not ready until it says hello.
    let (status, body) = admin(
        us1,
        "again-us1",
        "POST",
        "/_admin/tenants/acme/follow",
        &follow,
    );
    assert_eq!(status, 200, "{body}");
    let (status, body) = marked(us1, "acme", "get notes select body", None);
    assert_eq!(status, 503, "{body}");
    // Reached, it takes the new tenant's image.
    own.heal();
    until("us1 holds the tenant made again", || {
        let (status, body) = marked(us1, "acme", "get notes select body", None);
        status == 200 && body.contains("new") && !body.contains("old")
    });
}
