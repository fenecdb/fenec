//! One client's round trip taken apart: YCSB's read by key (a 1 KB record
//! of ten fields), its update of one field and E's scan of N records from a
//! key (`scanN`), against fenec-server -- natively and in a container --
//! and PostgreSQL 17 in a container, as `ycsb` measures them (`make
//! roundtrip-bench`).
//!
//! In a container the answer's bytes and packets are counted at its
//! network card -- what Docker's forwarding carried, fenec-server's JSON
//! against PostgreSQL's binary rows -- and fenec-server's page faults, from
//! `/proc/1/stat`: musl's allocator unmaps a freed group of blocks, and
//! each answer faulted its pages in again.
//!
//! Each side reports what it can see. The client: each operation's whole
//! round trip, and for fenec-server its parts -- the request's write, the
//! wait for the answer's first bytes, the rest read and parsed -- with the
//! process's CPU time, socket messages and context switches an operation
//! (`getrusage`). The server: fenec-server built with `--features timing`
//! answers `GET /_timing` with the mean of each phase inside it, from the
//! request's first bytes to its answer's `writev`; PostgreSQL's
//! `log_min_duration_statement = 0` logs each bind and execute message's
//! time, in a pass of its own since the logging costs time. What is left
//! of the round trip is the kernel's and the network's, Docker's proxy
//! included.
//!
//!     roundtrip [--systems server,server-docker,pg] [--modes always,250]
//!               [--records 100000] [--ops 20000] [--image fenecdb-timing]
//!               [--measure read,update,scan1,scan50,scan100]

#[path = "../http.rs"]
mod http;

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const FIELDS: usize = 10;
const FIELD_LEN: usize = 100;
const PG_URL: &str = "host=127.0.0.1 port=55434 user=postgres password=fenec dbname=ycsb";
const DOCKER_SERVER: &str = "fenecrt-server";
const DOCKER_PG: &str = "fenecrt-pg";

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        ((self.next() as u128 * n as u128) >> 64) as u64
    }
    fn value(&mut self) -> String {
        const ABC: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        (0..FIELD_LEN)
            .map(|_| ABC[self.below(62) as usize] as char)
            .collect()
    }
}

// ---- the process's own counts

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Timeval {
    sec: i64,
    usec: i64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Rusage {
    utime: Timeval,
    stime: Timeval,
    maxrss: i64,
    ixrss: i64,
    idrss: i64,
    isrss: i64,
    minflt: i64,
    majflt: i64,
    nswap: i64,
    inblock: i64,
    oublock: i64,
    msgsnd: i64,
    msgrcv: i64,
    nsignals: i64,
    nvcsw: i64,
    nivcsw: i64,
}

extern "C" {
    fn getrusage(who: i32, usage: *mut Rusage) -> i32;
}

fn rusage() -> Rusage {
    let mut r = Rusage::default();
    unsafe { getrusage(0, &mut r) };
    r
}

/// CPU us, socket messages sent and received, context switches: per
/// operation between two counts.
fn per_op(a: &Rusage, b: &Rusage, ops: usize) -> [f64; 4] {
    let us = |t: &Timeval| t.sec as f64 * 1e6 + t.usec as f64;
    let n = ops as f64;
    [
        (us(&b.utime) + us(&b.stime) - us(&a.utime) - us(&a.stime)) / n,
        (b.msgsnd - a.msgsnd) as f64 / n,
        (b.msgrcv - a.msgrcv) as f64 / n,
        (b.nvcsw - a.nvcsw + b.nivcsw - a.nivcsw) as f64 / n,
    ]
}

fn pct(v: &mut [u64], p: f64) -> f64 {
    v.sort_unstable();
    v[((v.len() - 1) as f64 * p) as usize] as f64 / 1e3
}

fn mean(v: &[u64]) -> f64 {
    v.iter().sum::<u64>() as f64 / v.len() as f64 / 1e3
}

fn docker(args: &[&str]) -> bool {
    std::process::Command::new("docker")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

// ---- fenec-server

struct Fenec {
    port: u16,
    file: PathBuf,
    docker: Option<String>,
    proc: Option<http::Server>,
}

impl Fenec {
    fn start(&mut self, sync: &str) {
        self.stop();
        let Some(image) = &self.docker else {
            let bin = std::env::var("FENEC_SERVER").unwrap_or_default();
            let child = std::process::Command::new(if bin.is_empty() {
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../target/timing/release/fenec-server"
                )
            } else {
                &bin
            })
            .args(["--http", &format!("127.0.0.1:{}", self.port)])
            .args(["--file", self.file.to_str().unwrap(), "--no-checkpoint"])
            .args(["--sync", sync])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("fenec-server built with --features timing (make roundtrip-bench)");
            self.proc = Some(http::Server::from_child(child));
            self.wait();
            return;
        };
        assert!(docker(&[
            "run",
            "-d",
            "--name",
            DOCKER_SERVER,
            "-p",
            &format!("127.0.0.1:{}:8080", self.port),
            "-v",
            &format!("{DOCKER_SERVER}:/data"),
            image,
            "--http",
            "0.0.0.0:8080",
            "--insecure",
            "--no-checkpoint",
            "--file",
            "/data/ycsb.fenec",
            "--sync",
            sync,
        ]));
        self.wait();
    }

    fn wait(&self) {
        let until = Instant::now() + Duration::from_secs(120);
        loop {
            if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", self.port)) {
                // Docker's proxy takes the connection before the server
                // listens: ask for something.
                let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
                let _ = s.write_all(b"GET /_health HTTP/1.1\r\nHost: x\r\n\r\n");
                let mut b = [0u8; 16];
                if matches!(std::io::Read::read(&mut s, &mut b), Ok(n) if n > 0) {
                    return;
                }
            }
            assert!(Instant::now() < until, "fenec-server did not start");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn stop(&mut self) {
        if let Some(p) = self.proc.take() {
            p.terminate();
        }
        if self.docker.is_some() {
            docker(&["stop", "-t", "60", DOCKER_SERVER]);
            docker(&["rm", "-f", DOCKER_SERVER]);
        }
    }

    fn load(&mut self, records: u64) {
        self.stop();
        if self.docker.is_some() {
            docker(&["volume", "rm", "-f", DOCKER_SERVER]);
        } else {
            let _ = std::fs::remove_file(&self.file);
        }
        self.start("250");
        let mut c = http::Http::connect(&format!("127.0.0.1:{}", self.port));
        let fields: Vec<String> = (0..FIELDS).map(|i| format!("field{i} text")).collect();
        c.query(
            &format!("create collection usertable ({})", fields.join(", ")),
            "",
        );
        let mut rng = Rng(7);
        let mut key = 1;
        let mut body = String::new();
        while key <= records {
            let end = (key + 1000).min(records + 1);
            body.clear();
            body.push('[');
            for k in key..end {
                if k > key {
                    body.push(',');
                }
                write!(body, "{{\"id\":{k}").unwrap();
                for i in 0..FIELDS {
                    write!(body, ",\"field{i}\":\"{}\"", rng.value()).unwrap();
                }
                body.push('}');
            }
            body.push(']');
            c.post("/usertable", "application/json", body.as_bytes());
            key = end;
        }
    }
}

impl Drop for Fenec {
    fn drop(&mut self) {
        self.stop();
        if self.docker.is_some() {
            docker(&["volume", "rm", "-f", DOCKER_SERVER]);
        }
    }
}

/// The server's phases since the last reset, in us, and the reset.
fn server_phases(c: &mut http::Http) -> Vec<(String, f64)> {
    let body = String::from_utf8_lossy(c.get("/_timing")).into_owned();
    let (_, _) = c.request("DELETE", "/_timing", "text/plain", b"");
    body.trim_matches(|c| c == '{' || c == '}')
        .split(',')
        .filter_map(|kv| {
            let (k, v) = kv.split_once(':')?;
            Some((k.trim_matches('"').to_string(), v.parse().ok()?))
        })
        .collect()
}

fn run_fenec(name: &str, f: &mut Fenec, cfg: &Config, sync: &str) {
    f.start(sync);
    let mut c = http::Http::connect(&format!("127.0.0.1:{}", f.port));
    let mut rng = Rng(11);
    let read = "get usertable where id = $1";
    let set = "set usertable {field3: $2} where id = $1";
    // Warm: the statements parsed and cached, the pages in.
    for _ in 0..cfg.ops / 4 {
        let k = 1 + rng.below(cfg.records);
        c.query(read, &k.to_string());
    }
    for op in &cfg.ops_list {
        let op = op.as_str();
        let scan = op
            .strip_prefix("scan")
            .map(|n| format!("get usertable where id >= $1 limit {n}"));
        if let Some(scan) = &scan {
            for _ in 0..cfg.ops / 20 {
                let k = 1 + rng.below(cfg.records - 100);
                c.query(scan, &k.to_string());
            }
        }
        let _ = server_phases(&mut c);
        let net = f.docker.as_ref().map(|_| net_counts(DOCKER_SERVER));
        let mut parts: [Vec<u64>; 4] = Default::default();
        let mut params = String::new();
        let values: Vec<String> = (0..256).map(|_| rng.value()).collect();
        let before = rusage();
        for i in 0..cfg.ops {
            let k = 1 + rng.below(cfg.records);
            params.clear();
            let t = Instant::now();
            let p = if let Some(scan) = &scan {
                write!(params, "{}", k.min(cfg.records - 100)).unwrap();
                c.query_timed(scan, &params)
            } else if op == "read" {
                write!(params, "{k}").unwrap();
                c.query_timed(read, &params)
            } else {
                write!(params, "{k},\"{}\"", values[i % values.len()]).unwrap();
                c.query_timed(set, &params)
            };
            parts[3].push(t.elapsed().as_nanos() as u64);
            for (v, x) in parts.iter_mut().zip(p) {
                v.push(x);
            }
        }
        let after = rusage();
        let bytes = c.body().len();
        let net = net.map(|a| net_per_op(&a, &net_counts(DOCKER_SERVER), cfg.ops));
        let server = server_phases(&mut c);
        let [w, wait, r, mut total] = parts;
        report(
            name,
            sync,
            op,
            &mut total,
            Some([&w, &wait, &r]),
            per_op(&before, &after, cfg.ops),
            &server,
        );
        println!(
            "{name}\t{sync}\t{op}\tlast body {bytes} bytes{}",
            net.unwrap_or_default()
        );
        if cfg.strace && f.docker.is_some() && scan.is_none() {
            let n = 2000;
            let calls = strace(DOCKER_SERVER, "1", n, || {
                for i in 0..n {
                    let k = 1 + rng.below(cfg.records);
                    params.clear();
                    if op == "read" {
                        write!(params, "{k}").unwrap();
                        c.query(read, &params);
                    } else {
                        write!(params, "{k},\"{}\"", values[i % values.len()]).unwrap();
                        c.query(set, &params);
                    }
                }
            });
            println!("{name}\t{sync}\t{op}\tserver syscalls an op: {calls}");
        }
    }
}

/// The system calls a process in the container `target` makes over `ops`
/// operations `run` sends: strace attached to each of its threads from a
/// container sharing its process namespace (`fenec-strace`, alpine and
/// strace: `make roundtrip-bench` builds it). Those of at least one in ten
/// operations, a mean an operation.
fn strace(target: &str, pid: &str, ops: usize, run: impl FnOnce()) -> String {
    const NAME: &str = "fenecrt-strace";
    docker(&["rm", "-f", NAME]);
    assert!(docker(&[
        "run",
        "-d",
        "--name",
        NAME,
        &format!("--pid=container:{target}"),
        "--cap-add",
        "SYS_PTRACE",
        "fenec-strace",
        "sh",
        "-c",
        &format!("exec strace -c -f $(for t in /proc/{pid}/task/*; do echo -p ${{t##*/}}; done)"),
    ]));
    std::thread::sleep(Duration::from_secs(2));
    run();
    docker(&["kill", "-s", "INT", NAME]);
    docker(&["wait", NAME]);
    let out = std::process::Command::new("docker")
        .args(["logs", NAME])
        .output()
        .unwrap();
    docker(&["rm", "-f", NAME]);
    let text = String::from_utf8_lossy(&out.stderr).into_owned();
    let mut calls = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // % time, seconds, usecs/call, calls, [errors,] syscall
        if cols.len() < 5 || cols[0].parse::<f64>().is_err() {
            continue;
        }
        let Ok(n) = cols[3].parse::<usize>() else {
            continue;
        };
        let name = cols[cols.len() - 1];
        if name != "total" && n * 10 >= ops {
            calls.push(format!("{name} {:.2}", n as f64 / ops as f64));
        }
    }
    calls.join(", ")
}

/// A container's network counters: bytes and packets out and in, from
/// its `eth0` -- what crossed into Docker's forwarding, TCP and IP heads
/// included -- and its first process's minor page faults. Read from a
/// container of `fenec-strace` in its network and process namespaces,
/// since fenec-server's image holds no `cat`. A packet is one the virtual
/// network card was handed, which may be cut into several on the way.
fn net_counts(container: &str) -> [u64; 5] {
    let out = std::process::Command::new("docker")
        .args(["run", "--rm", &format!("--network=container:{container}")])
        .args([&format!("--pid=container:{container}"), "fenec-strace"])
        .args([
            "sh",
            "-c",
            "cd /sys/class/net/eth0/statistics && cat tx_bytes tx_packets rx_bytes rx_packets \
             && cut -d' ' -f10 /proc/1/stat",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let mut v = [0u64; 5];
    for (o, l) in v.iter_mut().zip(text.lines()) {
        *o = l.trim().parse().unwrap_or(0);
    }
    v
}

fn net_per_op(a: &[u64; 5], b: &[u64; 5], ops: usize) -> String {
    let n = ops as f64;
    let d = |i: usize| b[i].saturating_sub(a[i]) as f64 / n;
    format!(
        "\tthe container sent {:.0} bytes in {:.2} packets, received {:.0} in {:.2}, \
         its first process took {:.2} page faults, an op",
        d(0),
        d(1),
        d(2),
        d(3),
        d(4)
    )
}

fn report(
    name: &str,
    sync: &str,
    op: &str,
    total: &mut [u64],
    client: Option<[&Vec<u64>; 3]>,
    usage: [f64; 4],
    server: &[(String, f64)],
) {
    let mut line = format!(
        "{name}\t{sync}\t{op}\ttotal p50 {:.1} mean {:.1} p99 {:.1} us",
        pct(total, 0.5),
        mean(total),
        pct(total, 0.99)
    );
    if let Some([w, wait, r]) = client {
        write!(
            line,
            "\tclient write {:.1} | wait {:.1} | read+parse {:.1} (means)",
            mean(w),
            mean(wait),
            mean(r)
        )
        .unwrap();
    }
    write!(
        line,
        "\tclient process: cpu {:.1} us, sent {:.2} recv {:.2} msgs, {:.2} switches an op",
        usage[0], usage[1], usage[2], usage[3]
    )
    .unwrap();
    let mut inside = 0.0;
    let mut phases = String::new();
    for (k, v) in server {
        if k == "requests" {
            write!(phases, " n={v}").unwrap();
            continue;
        }
        inside += v;
        write!(phases, " {k}={v:.2}").unwrap();
    }
    if !server.is_empty() {
        write!(line, "\tserver {inside:.1} us:{phases}").unwrap();
    }
    println!("{line}");
}

// ---- PostgreSQL

fn pg_start(records: u64) {
    docker(&["rm", "-f", "-v", DOCKER_PG]);
    docker(&["volume", "rm", "-f", DOCKER_PG]);
    assert!(docker(&[
        "run",
        "-d",
        "--name",
        DOCKER_PG,
        "-e",
        "POSTGRES_PASSWORD=fenec",
        "-e",
        "POSTGRES_DB=ycsb",
        "-p",
        "127.0.0.1:55434:5432",
        "--shm-size=1g",
        "-v",
        &format!("{DOCKER_PG}:/var/lib/postgresql/data"),
        "postgres:17",
        "-c",
        "shared_buffers=1GB",
        "-c",
        "effective_cache_size=2GB",
        "-c",
        "max_wal_size=4GB",
        "-c",
        "shared_preload_libraries=pg_stat_statements",
    ]));
    let until = Instant::now() + Duration::from_secs(120);
    let mut c = loop {
        // The image's entrypoint starts the server twice: wait for the
        // second, the one that stays.
        std::thread::sleep(Duration::from_secs(1));
        if let Ok(mut c) = postgres::Client::connect(PG_URL, postgres::NoTls) {
            if c.simple_query("select 1").is_ok() {
                std::thread::sleep(Duration::from_secs(3));
                if let Ok(c) = postgres::Client::connect(PG_URL, postgres::NoTls) {
                    break c;
                }
            }
        }
        assert!(Instant::now() < until, "postgres did not start");
    };
    let fields: Vec<String> = (0..FIELDS).map(|i| format!("field{i} text")).collect();
    c.batch_execute(&format!(
        "CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
         CREATE TABLE usertable (ycsb_key bigint PRIMARY KEY, {});",
        fields.join(", ")
    ))
    .unwrap();
    let mut rng = Rng(7);
    let mut w = c.copy_in("COPY usertable FROM STDIN").unwrap();
    let mut line = String::new();
    for k in 1..=records {
        line.clear();
        write!(line, "{k}").unwrap();
        for _ in 0..FIELDS {
            write!(line, "\t{}", rng.value()).unwrap();
        }
        line.push('\n');
        w.write_all(line.as_bytes()).unwrap();
    }
    w.finish().unwrap();
    c.batch_execute("VACUUM ANALYZE usertable").unwrap();
    c.batch_execute("CHECKPOINT").unwrap();
}

fn run_pg(cfg: &Config, sync: &str) {
    let durable = sync == "always";
    let mut c = postgres::Client::connect(PG_URL, postgres::NoTls).unwrap();
    c.batch_execute(if durable {
        "SET synchronous_commit = on"
    } else {
        "SET synchronous_commit = off"
    })
    .unwrap();
    let read = c
        .prepare("SELECT * FROM usertable WHERE ycsb_key = $1")
        .unwrap();
    let set = c
        .prepare("UPDATE usertable SET field3 = $2 WHERE ycsb_key = $1")
        .unwrap();
    let scan_stmt = c
        .prepare("SELECT * FROM usertable WHERE ycsb_key >= $1 ORDER BY ycsb_key LIMIT $2")
        .unwrap();
    let mut rng = Rng(11);
    for _ in 0..cfg.ops / 4 {
        let k = (1 + rng.below(cfg.records)) as i64;
        c.query_one(&read, &[&k]).unwrap();
    }
    let values: Vec<String> = (0..256).map(|_| rng.value()).collect();
    let mut admin = postgres::Client::connect(PG_URL, postgres::NoTls).unwrap();
    for op in &cfg.ops_list {
        let op = op.as_str();
        let scan: Option<i64> = op.strip_prefix("scan").map(|n| n.parse().unwrap());
        // The round trip, nothing logged.
        let mut total = Vec::with_capacity(cfg.ops);
        let one = |c: &mut postgres::Client, i: usize, rng: &mut Rng| {
            let k = (1 + rng.below(cfg.records)) as i64;
            if let Some(n) = scan {
                let k = k.min(cfg.records as i64 - 100);
                let rows = c.query(&scan_stmt, &[&k, &n]).unwrap();
                assert_eq!(rows.len(), n as usize);
                for r in &rows {
                    let s: String = r.get(4);
                    assert_eq!(s.len(), FIELD_LEN);
                }
            } else if op == "read" {
                let r = c.query_one(&read, &[&k]).unwrap();
                let s: String = r.get(4);
                assert_eq!(s.len(), FIELD_LEN);
            } else {
                let v = &values[i % values.len()];
                assert_eq!(c.execute(&set, &[&k, v]).unwrap(), 1);
            }
        };
        admin
            .batch_execute("SELECT pg_stat_statements_reset()")
            .unwrap();
        if scan.is_some() {
            for i in 0..cfg.ops / 20 {
                one(&mut c, i, &mut rng);
            }
        }
        let net = net_counts(DOCKER_PG);
        let before = rusage();
        for i in 0..cfg.ops {
            let t = Instant::now();
            one(&mut c, i, &mut rng);
            total.push(t.elapsed().as_nanos() as u64);
        }
        let after = rusage();
        let net = net_per_op(&net, &net_counts(DOCKER_PG), cfg.ops);
        let mut server = Vec::new();
        for row in admin
            .query(
                "SELECT calls, mean_exec_time FROM pg_stat_statements
                 WHERE query LIKE $1
                 ORDER BY calls DESC LIMIT 1",
                &[&match (scan, op) {
                    (Some(_), _) => "SELECT * FROM usertable WHERE ycsb_key >=%",
                    (None, "read") => "SELECT * FROM usertable WHERE ycsb_key =%",
                    _ => "UPDATE usertable%",
                }],
            )
            .unwrap()
        {
            let calls: i64 = row.get(0);
            let ms: f64 = row.get(1);
            server.push(("requests".to_string(), calls as f64));
            server.push(("executor".to_string(), ms * 1e3));
        }
        // Each message's time as the server logs it, in a pass of its own.
        c.batch_execute("SET log_min_duration_statement = 0")
            .unwrap();
        let since = chrono_now();
        let logged = cfg.ops.min(5000);
        for i in 0..logged {
            one(&mut c, i, &mut rng);
        }
        c.batch_execute("SET log_min_duration_statement = -1")
            .unwrap();
        let (bind, exec) = pg_logged(&since);
        server.push(("bind".to_string(), bind));
        server.push(("execute".to_string(), exec));
        report(
            "pg",
            sync,
            op,
            &mut total,
            None,
            per_op(&before, &after, cfg.ops),
            &server,
        );
        println!("pg\t{sync}\t{op}{net}");
        if cfg.strace && scan.is_none() {
            let pid: i32 = c.query_one("SELECT pg_backend_pid()", &[]).unwrap().get(0);
            let n = 2000;
            let calls = strace(DOCKER_PG, &pid.to_string(), n, || {
                for i in 0..n {
                    one(&mut c, i, &mut rng);
                }
            });
            println!("pg\t{sync}\t{op}\tserver syscalls an op: {calls}");
        }
    }
}

/// `docker logs --since`'s idea of now.
fn chrono_now() -> String {
    let out = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The mean of the logged bind and execute durations, in us.
fn pg_logged(since: &str) -> (f64, f64) {
    let out = std::process::Command::new("docker")
        .args(["logs", "--since", since, DOCKER_PG])
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
    let (mut b, mut nb, mut e, mut ne) = (0.0, 0, 0.0, 0);
    for line in text.lines() {
        let Some(at) = line.find("duration: ") else {
            continue;
        };
        let rest = &line[at + 10..];
        let Some((ms, what)) = rest.split_once(" ms") else {
            continue;
        };
        let Ok(ms) = ms.parse::<f64>() else { continue };
        if !what.contains("usertable") {
            continue;
        }
        if what.trim_start().starts_with("bind") {
            b += ms;
            nb += 1;
        } else if what.trim_start().starts_with("execute") {
            e += ms;
            ne += 1;
        }
    }
    (b * 1e3 / nb.max(1) as f64, e * 1e3 / ne.max(1) as f64)
}

struct Config {
    records: u64,
    ops: usize,
    /// Count the server's system calls an operation, in a pass of its own.
    strace: bool,
    /// What is measured: `read`, `update`, `scan<n>` (n records from a key).
    ops_list: Vec<String>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut systems = vec!["server".to_string(), "server-docker".into(), "pg".into()];
    let mut modes = vec!["always".to_string(), "250".into()];
    let mut image = "fenecdb-timing".to_string();
    let mut cfg = Config {
        records: 100_000,
        ops: 20_000,
        strace: false,
        ops_list: vec!["read".into(), "update".into()],
    };
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--strace" {
            cfg.strace = true;
            i += 1;
            continue;
        }
        let v = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--systems" => systems = v.split(',').map(str::to_string).collect(),
            "--modes" => modes = v.split(',').map(str::to_string).collect(),
            "--records" => cfg.records = v.parse().unwrap(),
            "--ops" => cfg.ops = v.parse().unwrap(),
            "--image" => image = v,
            "--measure" => cfg.ops_list = v.split(',').map(str::to_string).collect(),
            other => panic!("unknown argument {other}"),
        }
        i += 2;
    }
    for sys in &systems {
        match sys.as_str() {
            "server" | "server-docker" => {
                let mut f = Fenec {
                    port: http::free_port(),
                    file: std::env::temp_dir()
                        .join(format!("roundtrip-{}.fenec", std::process::id())),
                    docker: (sys == "server-docker").then(|| image.clone()),
                    proc: None,
                };
                f.load(cfg.records);
                for m in &modes {
                    run_fenec(sys, &mut f, &cfg, m);
                }
                drop(f);
            }
            "pg" => {
                pg_start(cfg.records);
                for m in &modes {
                    run_pg(&cfg, m);
                }
                docker(&["rm", "-f", "-v", DOCKER_PG]);
                docker(&["volume", "rm", "-f", DOCKER_PG]);
            }
            other => panic!("no system {other}: server, server-docker, pg"),
        }
    }
}
