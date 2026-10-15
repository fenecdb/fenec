//! What archiving every tenant of a node costs, each way:
//! `cargo run --release -p fenec-http --example node_archive -- [TENANTS] [ACTIVE] [SECONDS] [SYNC] [WAY] [BIG_MB]`
//! (`make node-archive-bench`; it runs `target/release/fenec-server`, or
//! the one `FENEC_SERVER` names -- another build's, to compare).
//!
//! A `fenec-server --dir` node of TENANTS tenants (1 000), each a
//! collection of ten rows, and one of BIG_MB (64) whose image is the
//! largest the archiver takes, all closed once idle for two seconds, in a
//! process of its own; ACTIVE of them (10) written ten times a second each
//! from before the archiver connects -- the writes' round trips counted
//! apart while it takes its first images, the large one among them, and
//! after -- then SECONDS (20) more under `--sync SYNC` (always). Three ways,
//! a fresh node each:
//!
//!   none     no archiver: what the writes cost on their own
//!   streams  a tenant's own stream each (`/t/<tenant>/_replication`), all
//!            from this process, as `fenec archive` would one at a time
//!   node     the node's stream of every tenant (`fenec archive --tenants`)
//!
//! For each: how long until every tenant's archive has its first image;
//! the node's resident memory, threads, open tenants and CPU through the
//! writes; this process's (the archiver's) memory, threads, descriptors and
//! CPU; the writes' round trips; and, the node restarted and the archiver
//! connecting again, what the node holds a few seconds after -- and for the
//! node's stream, how long its first pass over the closed tenants took (its
//! log line).

use fenec_http::archive::Archive;
use fenec_http::node_archive::{self, NodeArchive};
use fenec_http::replication::Upstream;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REPL: &str = "r";
const ADMIN: &str = "a";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Way {
    None,
    Streams,
    Node,
}

struct Node {
    child: Child,
    addr: String,
    log: Arc<Mutex<Vec<String>>>,
}

fn server() -> PathBuf {
    if let Ok(path) = std::env::var("FENEC_SERVER") {
        return PathBuf::from(path);
    }
    let exe = std::env::current_exe().unwrap();
    let release = exe.parent().unwrap().parent().unwrap();
    let path = release.join("fenec-server");
    assert!(
        path.exists(),
        "{} is missing: cargo build --release -p fenec-server",
        path.display()
    );
    path
}

impl Node {
    fn start(dir: &Path, sync: &str) -> Node {
        let mut child = Command::new(server())
            .args(["--dir", dir.to_str().unwrap(), "--http", "127.0.0.1:0"])
            .args(["--replication-token", REPL, "--admin-token", ADMIN])
            .args([
                "--max-connections",
                "0",
                "--idle-close",
                "2",
                "--sync",
                sync,
            ])
            .args(["--auth-delay", "0", "--warm", "off"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let err = child.stderr.take().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = std::sync::mpsc::channel();
        let kept = Arc::clone(&log);
        std::thread::spawn(move || {
            let mut tx = Some(tx);
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                if let Some(at) = line.find("listening on: http://") {
                    let addr = line[at + 21..]
                        .split_whitespace()
                        .next()
                        .unwrap()
                        .to_string();
                    if let Some(tx) = tx.take() {
                        let _ = tx.send(addr);
                    }
                }
                kept.lock().unwrap().push(line);
            }
        });
        let addr = rx.recv_timeout(Duration::from_secs(30)).unwrap();
        Node { child, addr, log }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn stop(mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let _ = self.child.wait();
    }

    fn open_tenants(&self) -> usize {
        let (_, body) = call(&self.addr, "GET", "/_admin/stats", "", Some(ADMIN));
        let at = body.find("\"open\":").unwrap() + 7;
        body[at..]
            .trim_end_matches('}')
            .split(',')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }
}

/// One request on a connection of its own.
fn call(addr: &str, method: &str, path: &str, body: &str, auth: Option<&str>) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    let auth = auth.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out[9..12].parse().unwrap();
    (
        status,
        out.split_once("\r\n\r\n").map_or("", |x| x.1).to_string(),
    )
}

/// A keep-alive connection for the writers.
struct Client {
    r: BufReader<TcpStream>,
}

impl Client {
    fn new(addr: &str) -> Client {
        let s = TcpStream::connect(addr).unwrap();
        s.set_nodelay(true).unwrap();
        Client {
            r: BufReader::new(s),
        }
    }

    fn query(&mut self, tenant: &str, q: &str) -> u16 {
        let body = format!("{{\"query\":\"{}\"}}", q.replace('"', "\\\""));
        let req = format!(
            "POST /t/{tenant}/query HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        self.r.get_mut().write_all(req.as_bytes()).unwrap();
        let mut line = String::new();
        self.r.read_line(&mut line).unwrap();
        let status = line[9..12].parse().unwrap();
        let mut len = 0;
        loop {
            line.clear();
            self.r.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; len];
        self.r.read_exact(&mut body).unwrap();
        status
    }
}

/// A process's resident memory (MB), threads and CPU seconds, as `ps` has
/// them.
fn ps(pid: u32) -> (f64, usize, f64) {
    let out = Command::new("ps")
        .args(["-o", "rss=,time=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.split_whitespace();
    let rss: f64 = it.next().unwrap().parse().unwrap();
    let cpu = it
        .next()
        .unwrap()
        .split(':')
        .rev()
        .enumerate()
        .map(|(i, p)| p.parse::<f64>().unwrap() * 60f64.powi(i as i32))
        .sum();
    let threads = if cfg!(target_os = "macos") {
        let out = Command::new("ps")
            .args(["-M", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).lines().count() - 1
    } else {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("Threads:"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    (rss / 1024.0, threads, cpu)
}

fn descriptors(pid: u32) -> usize {
    let out = Command::new("lsof")
        .args(["-n", "-P", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .count()
        .saturating_sub(1)
}

fn images(dir: &Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |d| {
        d.filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.file_name().to_string_lossy().starts_with("image-"))
        })
        .count()
    })
}

/// The archiver, one way or the other, on threads of this process.
struct Archiver {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Archiver {
    fn start(way: Way, url: &str, root: &Path, names: &[String]) -> Archiver {
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        match way {
            Way::None => {}
            Way::Streams => {
                for name in names {
                    let (stop, url, dir) = (
                        Arc::clone(&stop),
                        format!("{url}/t/{name}"),
                        root.join(name),
                    );
                    threads.push(
                        std::thread::Builder::new()
                            .stack_size(256 << 10)
                            .spawn(move || {
                                let upstream = Upstream::new(&url, REPL.into()).unwrap();
                                let _ =
                                    Archive::new(&dir)
                                        .unwrap()
                                        .follow(&upstream, &stop, &|_| {});
                            })
                            .unwrap(),
                    );
                }
            }
            Way::Node => {
                let (stop, url, root) = (Arc::clone(&stop), url.to_string(), root.to_path_buf());
                threads.push(std::thread::spawn(move || {
                    let upstream = node_archive::node_upstream(&url, REPL.into()).unwrap();
                    let _ =
                        NodeArchive::new(&root, None)
                            .unwrap()
                            .follow(&upstream, &stop, &|_| {});
                }));
            }
        }
        Archiver { stop, threads }
    }

    fn finish(self) {
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads {
            let _ = t.join();
        }
    }
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let tenants: usize = args.first().map_or(1000, |v| v.parse().unwrap());
    let active: usize = args.get(1).map_or(10, |v| v.parse().unwrap());
    let seconds: u64 = args.get(2).map_or(20, |v| v.parse().unwrap());
    let sync = args.get(3).cloned().unwrap_or_else(|| "always".into());
    let ways: Vec<Way> = match args.get(4).map(String::as_str) {
        Some("none") => vec![Way::None],
        Some("streams") => vec![Way::Streams],
        Some("node") => vec![Way::Node],
        _ => vec![Way::None, Way::Streams, Way::Node],
    };
    let big_mb: usize = args.get(5).map_or(64, |v| v.parse().unwrap());
    println!(
        "{tenants} tenants and one of {big_mb} MB, {active} written 10 times a second each, \
         --sync {sync}, {}\n",
        server().display()
    );
    for way in ways {
        run(way, tenants, active, seconds, &sync, big_mb);
    }
}

fn stats(lat: &[f64]) -> String {
    let mut lat = lat.to_vec();
    lat.sort_by(f64::total_cmp);
    format!(
        "{} at p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
        lat.len(),
        pct(&lat, 0.5),
        pct(&lat, 0.99),
        lat.last().copied().unwrap_or(0.0)
    )
}

fn run(way: Way, tenants: usize, active: usize, seconds: u64, sync: &str, big_mb: usize) {
    let d = std::env::temp_dir().join(format!("fenec-node-archive-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let node = Node::start(&d.join("node"), sync);
    let mut names: Vec<String> = (0..tenants).map(|i| format!("t{i:05}")).collect();
    for name in &names {
        let (s, _) = call(
            &node.addr,
            "PUT",
            &format!("/_admin/tenants/{name}"),
            "",
            Some(ADMIN),
        );
        assert_eq!(s, 201);
        let mut c = Client::new(&node.addr);
        assert_eq!(
            c.query(name, "create collection notes (title text, n int @hash)"),
            200
        );
        let docs: Vec<String> = (0..10)
            .map(|i| format!("{{title: \"note {i}\", n: {i}}}"))
            .collect();
        assert_eq!(
            c.query(name, &format!("put notes [{}]", docs.join(","))),
            200
        );
    }
    // The large one, first in the stream's pass: a thousand rows of a KB a
    // statement.
    let (s, _) = call(&node.addr, "PUT", "/_admin/tenants/big", "", Some(ADMIN));
    assert_eq!(s, 201);
    let mut c = Client::new(&node.addr);
    assert_eq!(
        c.query("big", "create collection notes (title text, n int @hash)"),
        200
    );
    let text = "x".repeat(1000);
    for k in 0..big_mb {
        let docs: Vec<String> = (0..1000)
            .map(|i| format!("{{title: \"{text}\", n: {}}}", k * 1000 + i))
            .collect();
        assert_eq!(
            c.query("big", &format!("put notes [{}]", docs.join(","))),
            200
        );
    }
    drop(c);
    names.push("big".into());
    // Every tenant idle and closed, as most of a node's are.
    let t = Instant::now();
    while node.open_tenants() > 0 {
        assert!(
            t.elapsed() < Duration::from_secs(60),
            "the tenants did not close"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let pid = node.child.id();
    let me = std::process::id();
    let (node_rss0, node_threads0, _) = ps(pid);
    let (me_rss0, me_threads0, _) = ps(me);

    // The writers, from before the archiver connects: each round trip
    // with the phase it fell in.
    let phase = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let lat: Arc<Mutex<Vec<(u8, f64)>>> = Arc::new(Mutex::new(Vec::new()));
    let writers: Vec<_> = names[..active]
        .iter()
        .map(|name| {
            let (name, addr, stop, lat, phase) = (
                name.clone(),
                node.addr.clone(),
                Arc::clone(&stop),
                Arc::clone(&lat),
                Arc::clone(&phase),
            );
            std::thread::spawn(move || {
                let mut c = Client::new(&addr);
                let mut i = 0;
                let mut next = Instant::now();
                while !stop.load(Ordering::SeqCst) {
                    let p = phase.load(Ordering::SeqCst);
                    let t = Instant::now();
                    let s = c.query(&name, &format!("put notes {{title: \"w {i}\", n: {i}}}"));
                    assert_eq!(s, 200);
                    lat.lock()
                        .unwrap()
                        .push((p, t.elapsed().as_secs_f64() * 1e3));
                    i += 1;
                    next += Duration::from_millis(100);
                    std::thread::sleep(next.saturating_duration_since(Instant::now()));
                }
            })
        })
        .collect();
    std::thread::sleep(Duration::from_secs(2));

    // The archiver connects: until every tenant's archive has its image.
    phase.store(1, Ordering::SeqCst);
    let root = d.join("root");
    let t0 = Instant::now();
    let arch = Archiver::start(way, &node.url(), &root, &names);
    if way != Way::None {
        for name in &names {
            while images(&root.join(name)) == 0 {
                assert!(
                    t0.elapsed() < Duration::from_secs(600),
                    "{name} took no image"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    } else {
        std::thread::sleep(Duration::from_secs(5));
    }
    let first_images = t0.elapsed();
    // Settled: the node's idle close has had its turn.
    phase.store(2, Ordering::SeqCst);
    std::thread::sleep(Duration::from_secs(3));
    let settled_open = node.open_tenants();

    // The steady writes.
    let (_, _, node_cpu0) = ps(pid);
    let (_, _, me_cpu0) = ps(me);
    phase.store(3, Ordering::SeqCst);
    let t = Instant::now();
    let (mut node_rss, mut node_threads, mut open) = (0f64, 0usize, 0usize);
    while t.elapsed() < Duration::from_secs(seconds) {
        std::thread::sleep(Duration::from_secs(1));
        let (rss, threads, _) = ps(pid);
        node_rss = node_rss.max(rss);
        node_threads = node_threads.max(threads);
        open = open.max(node.open_tenants());
    }
    let wall = t.elapsed().as_secs_f64();
    let (_, _, node_cpu1) = ps(pid);
    let (me_rss, me_threads, me_cpu1) = ps(me);
    stop.store(true, Ordering::SeqCst);
    for w in writers {
        w.join().unwrap();
    }
    let fds = descriptors(me);
    let node_fds = descriptors(pid);
    let lat = lat.lock().unwrap().clone();
    let of = |p: u8| -> Vec<f64> { lat.iter().filter(|x| x.0 == p).map(|x| x.1).collect() };

    println!("== {way:?}");
    println!("  node before:  {node_rss0:.1} MB, {node_threads0} threads, 0 tenants open");
    match way {
        Way::None => println!("  writes, the same 5 s: {}", stats(&of(1))),
        _ => {
            println!(
                "  first images of all {}: {:.2} s; {settled_open} tenants open 3 s after",
                names.len(),
                first_images.as_secs_f64()
            );
            println!("  writes as they were taken: {}", stats(&of(1)));
        }
    }
    println!("  writes before the archiver: {}", stats(&of(0)));
    println!(
        "  node writing: {node_rss:.1} MB, {node_threads} threads, {node_fds} descriptors, {open} tenants open at most, CPU {:.1}% of a core",
        (node_cpu1 - node_cpu0) / wall * 100.0
    );
    println!(
        "  archiver:     {me_rss:.1} MB (+{:.1}), {me_threads} threads (+{}), {fds} descriptors, CPU {:.1}% of a core",
        me_rss - me_rss0,
        me_threads.saturating_sub(me_threads0),
        (me_cpu1 - me_cpu0) / wall * 100.0
    );
    println!("  writes, steady: {}", stats(&of(3)));

    // The node restarted, the archiver connecting again.
    arch.finish();
    let dir = d.join("node");
    node.stop();
    let node = Node::start(&dir, sync);
    let pid = node.child.id();
    if way != Way::None {
        let t0 = Instant::now();
        let arch = Archiver::start(way, &node.url(), &root, &names);
        if way == Way::Node {
            // From the archiver's start -- reading where each archive stands
            // -- to the node's log saying it greeted every tenant.
            while !node
                .log
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains("said hello to in"))
            {
                assert!(t0.elapsed() < Duration::from_secs(300), "no first pass");
                std::thread::sleep(Duration::from_millis(5));
            }
            println!(
                "  reconnected:  every tenant greeted {:.2} s after the archiver started",
                t0.elapsed().as_secs_f64()
            );
        }
        std::thread::sleep(Duration::from_secs(5).saturating_sub(t0.elapsed()));
        let (rss, threads, cpu) = ps(pid);
        println!(
            "  restarted:    5 s after the archiver connected again: {rss:.1} MB, {threads} threads, \
             {} tenants open, {cpu:.2} s of CPU since the start",
            node.open_tenants()
        );
        if way == Way::Node {
            let log = node.log.lock().unwrap().clone();
            if let Some(line) = log.iter().find(|l| l.contains("said hello to in")) {
                println!(
                    "  node's log:   {}",
                    line.split(" request_id=").next().unwrap()
                );
            }
        }
        arch.finish();
    }
    node.stop();
    println!();
    let _ = std::fs::remove_dir_all(&d);
}
