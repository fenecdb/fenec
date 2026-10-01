//! fenec-server against PostgreSQL + pgvector at scale, both over the pg wire:
//! `make scale-bench`.
//!
//! ```text
//! cargo run --release -p fenec-bench --bin scale -- [N] [DIM] [--rank R] [--queries Q]
//!     [--clients C] [--only fenec|pg] [--after] [--compact] [--efs 40,100,200]
//!     [--copy-rows ROWS]
//! ```
//!
//! N vectors of DIM dimensions -- the generator `quant` uses, 64 centres
//! spread along R directions (32 unless given), as embeddings vary along
//! far fewer directions than they have -- go into each server by binary
//! COPYs of ROWS rows (50 000; 0, every row in one) through the same client,
//! the `postgres` crate with pgvector-rust's `Vector`:
//!
//!   * fenec-server, started here over an empty file, its HNSW index kept as
//!     the rows land -- or with `--after` built by a `create index` once
//!     they are in;
//!   * PostgreSQL + pgvector, the container `make pgvector-up` starts
//!     (skipped without it), its index built after the COPY as pgvector
//!     advises, by as many processes as the container has cores
//!     (`max_parallel_maintenance_workers`) and in memory
//!     (`maintenance_work_mem`), then read into its buffers (`pg_prewarm`).
//!
//! Each row also carries a `tag`, 0 to 99, and its `quarter`, the tag's
//! remainder by 4, from a hash of its own -- no cluster's -- each with an
//! index: a filter on the tag keeps 1% of the rows, on the quarter 25%.
//!
//! Both indexes take m = 16 and ef_construction = 64, pgvector's defaults.
//! Each server is then asked Q held-out queries at beams of 40, 100 and 200
//! (`ef`, `hnsw.ef_search`; `--efs` names others) by one client, once to warm and once measured,
//! for recall@10 against the exact ten -- found once, by brute force over
//! the vectors -- and latency, unfiltered and with each filter, the exact
//! ten then those of the rows it keeps (pgvector searching with
//! `hnsw.iterative_scan = relaxed_order`, as it advises for a filter); and
//! at a beam of 100 by C clients (8) for
//! 10 s, for throughput. PostgreSQL answers from inside Docker's virtual
//! machine, whose network carries every byte, and fenec-server on the host, so
//! the round trip of an empty query is measured for each. The space on
//! disk is fenec-server's file and the table with its index. Memory is what
//! each server holds of its own, which the kernel cannot take back without
//! swapping it out -- fenec-server's physical footprint, the dirty and
//! compressed pages of `vmmap -summary` (macOS; Linux's anonymous resident
//! pages and swap), and the container's anonymous memory and the shared
//! memory PostgreSQL's buffers are -- and beside it the pages of their files
//! each keeps in memory, clean, which the kernel takes back under pressure:
//! fenec-server's mapped file's, and the container's page cache; and the most
//! each held of its own while the rows went in and its index was built
//! (`Peak`), which a COPY of every row in one shows. Then what
//! fenec-server's engine counts it holds (`fenec_memory_bytes`), and each
//! resident set as its tools count it: fenec-server's, the pages of its file
//! it touched included, and what `docker stats` counts of the container.

#[path = "../wire.rs"]
#[allow(dead_code)]
mod wire;

use pgvector::Vector;
use postgres::binary_copy::BinaryCopyInWriter;
use postgres::types::{Kind, Type};
use postgres::{Client, NoTls};
use std::time::{Duration, Instant};

const PG: &str = "host=127.0.0.1 port=55432 user=postgres password=fenec dbname=fenecbench";
const M: usize = 16;
const EF_CONSTRUCTION: usize = 64;

// ------------------------------------------------------------------ the data
//
// `quant`'s generator (crates/fenec-core/examples/quant.rs), so the numbers
// sit beside its: a vector is made from its own seed, and none is kept.

struct Rng(u64);
impl Rng {
    fn f32(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32) / ((1u32 << 24) as f32)
    }
    fn gauss(&mut self) -> f32 {
        (0..6).map(|_| self.f32()).sum::<f32>() - 3.0
    }
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

struct Data {
    centres: Vec<Vec<f32>>,
    basis: Vec<Vec<f32>>,
}

impl Data {
    fn new(dim: usize, rank: usize) -> Data {
        let mut rng = Rng(0xDEAD_BEEF);
        let centres = (0..64)
            .map(|_| (0..dim).map(|_| rng.gauss()).collect())
            .collect();
        let basis = (0..rank)
            .map(|_| (0..dim).map(|_| rng.gauss()).collect())
            .collect();
        Data { centres, basis }
    }

    fn vector(&self, i: u64) -> Vec<f32> {
        let mut rng = Rng(splitmix(i) | 1);
        let c = &self.centres[(i % 64) as usize];
        if self.basis.is_empty() {
            return c.iter().map(|x| x + rng.gauss() * 0.35).collect();
        }
        // Along the basis, and a twentieth of that beside it.
        let scale = 0.35 * (c.len() as f32 / self.basis.len() as f32).sqrt();
        let mut out: Vec<f32> = c.iter().map(|x| x + rng.gauss() * 0.0175).collect();
        for b in &self.basis {
            let z = rng.gauss() * scale / (c.len() as f32).sqrt();
            for (o, x) in out.iter_mut().zip(b) {
                *o += z * x;
            }
        }
        out
    }
}

/// Held-out queries are made from seeds past every row's.
const QUERY_BASE: u64 = 1 << 40;

/// A row's tag, 0 to 99, from a hash of its own: no cluster's and no
/// direction's, so a filter on it keeps rows of every region.
fn tag(i: u64) -> i64 {
    (splitmix(i ^ 0x5EED_5EED) % 100) as i64
}

/// The filters a query `j` is asked with: its tag, 1% of the rows, and its
/// quarter, 25%.
fn filters(j: usize) -> (i64, i64) {
    ((j % 100) as i64, (j % 4) as i64)
}

/// A query's nearest so far, most similar first: of every row, of its
/// tag's, of its quarter's.
type Tens = [Vec<(f32, i64)>; 3];

/// The exact ten of every query: of all the rows, of the rows of its tag,
/// and of the rows of its quarter.
struct Truth {
    all: Vec<Vec<i64>>,
    tag: Vec<Vec<i64>>,
    quarter: Vec<Vec<i64>>,
}

fn unit(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter().map(|x| x / n).collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let ((ca, ra), (cb, rb)) = (a.as_chunks::<8>(), b.as_chunks::<8>());
    for (x, y) in ca.iter().zip(cb) {
        for k in 0..8 {
            acc[k] += x[k] * y[k];
        }
    }
    acc.iter().sum::<f32>() + ra.iter().zip(rb).map(|(x, y)| x * y).sum::<f32>()
}

/// The exact ten of each query by cosine, their ids from 1 -- of every row,
/// of its tag's and of its quarter's: a range of the rows to a thread, each
/// keeping its own tens, merged at the end.
fn truth(data: &Data, n: u64, queries: &[Vec<f32>]) -> Truth {
    let qs: Vec<Vec<f32>> = queries.iter().map(|q| unit(q)).collect();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()) as u64;
    let per = n.div_ceil(threads);
    let keep = |b: &mut Vec<(f32, i64)>, sim: f32, id: i64| {
        if b.len() < 10 || sim > b[9].0 {
            b.push((sim, id));
            b.sort_by(|x, y| y.0.total_cmp(&x.0));
            b.truncate(10);
        }
    };
    let parts: Vec<Vec<Tens>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let qs = &qs;
                s.spawn(move || {
                    let mut best: Vec<Tens> = vec![Default::default(); qs.len()];
                    for i in t * per..((t + 1) * per).min(n) {
                        let v = data.vector(i);
                        let norm = dot(&v, &v).sqrt();
                        let row_tag = tag(i);
                        for (j, (b, q)) in best.iter_mut().zip(qs).enumerate() {
                            let sim = dot(q, &v) / norm;
                            let id = i as i64 + 1;
                            let (qt, qq) = filters(j);
                            keep(&mut b[0], sim, id);
                            if row_tag == qt {
                                keep(&mut b[1], sim, id);
                            }
                            if row_tag % 4 == qq {
                                keep(&mut b[2], sim, id);
                            }
                        }
                    }
                    best
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let merged = |k: usize| -> Vec<Vec<i64>> {
        (0..queries.len())
            .map(|q| {
                let mut all: Vec<(f32, i64)> = parts.iter().flat_map(|p| p[q][k].clone()).collect();
                all.sort_by(|x, y| y.0.total_cmp(&x.0));
                all.iter().take(10).map(|x| x.1).collect()
            })
            .collect()
    };
    Truth {
        all: merged(0),
        tag: merged(1),
        quarter: merged(2),
    }
}

// ---------------------------------------------------------------- a server

#[derive(Clone, Copy, PartialEq)]
enum Engine {
    Fenec,
    Pg,
}

struct Server {
    engine: Engine,
    dsn: String,
    /// fenec-server's process, and the file it keeps.
    fenec: Option<(wire::Server, std::path::PathBuf)>,
    /// fenec-server's HTTP port, for its `/_metrics`.
    http: u16,
}

/// A size as `vmmap` writes it -- `1633K`, `1.2G` -- in bytes.
fn vmmap_size(s: &str) -> Option<u64> {
    let (num, unit) = s.split_at(s.find(|c: char| c.is_ascii_alphabetic())?);
    let n: f64 = num.parse().ok()?;
    let m = match unit {
        "K" => 1u64 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        _ => 1,
    };
    Some((n * m as f64) as u64)
}

/// What fenec-server holds of its own -- the dirty and compressed pages it
/// cannot give back, macOS's physical footprint (`vmmap -summary`), or
/// Linux's anonymous resident pages and swap -- and its mapped file's
/// resident pages.
fn footprint(pid: u32) -> Option<(u64, u64)> {
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        let kb = |key: &str| -> u64 {
            status
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
                .unwrap_or(0)
                * 1024
        };
        return Some((kb("RssAnon:") + kb("VmSwap:"), kb("RssFile:")));
    }
    let out = std::process::Command::new("vmmap")
        .args(["-summary", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let phys = text
        .lines()
        .find_map(|l| l.strip_prefix("Physical footprint:"))
        .and_then(|v| vmmap_size(v.trim()))?;
    // `mapped file` and its virtual, then resident size.
    let mapped = text
        .lines()
        .find(|l| l.starts_with("mapped file "))
        .and_then(|l| l.split_whitespace().nth(3))
        .and_then(vmmap_size)
        .unwrap_or(0);
    Some((phys, mapped))
}

/// The most a server holds of its own while the rows go in: fenec-server's
/// physical footprint at its peak, which macOS keeps (`vmmap`'s `Physical
/// footprint (peak)`; Linux's peak resident set, `VmHWM`, whose mapped pages
/// are few while the rows go in); PostgreSQL's, polled from its container
/// every half second, the cgroup keeping no peak of it alone.
struct Peak {
    fenec: Option<u32>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    polled: Option<std::thread::JoinHandle<u64>>,
}

impl Peak {
    fn watch(server: &Server) -> Peak {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fenec = server.fenec.as_ref().map(|(s, _)| s.pid());
        let polled = fenec.is_none().then(|| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut most = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    most = most.max(pg_memory().map_or(0, |m| m.0));
                    std::thread::sleep(Duration::from_millis(500));
                }
                most
            })
        });
        Peak {
            fenec,
            stop,
            polled,
        }
    }

    fn most(self) -> Option<u64> {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        match (self.fenec, self.polled) {
            (Some(pid), _) => peak_footprint(pid),
            (None, Some(polled)) => polled.join().ok().filter(|&m| m > 0),
            _ => None,
        }
    }
}

/// fenec-server's physical footprint at its peak so far; see [`Peak`].
fn peak_footprint(pid: u32) -> Option<u64> {
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        return status
            .lines()
            .find_map(|l| l.strip_prefix("VmHWM:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .map(|kb| kb * 1024);
    }
    let out = std::process::Command::new("vmmap")
        .args(["-summary", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("Physical footprint (peak):"))
        .and_then(|v| vmmap_size(v.trim()))
}

/// What PostgreSQL's container holds of its own -- its processes'
/// anonymous memory and the shared memory its buffers are, which it cannot
/// give back -- and the page cache of its files, from the container's
/// cgroup (`memory.stat`: `file` counts shared memory as well).
fn pg_memory() -> Option<(u64, u64)> {
    let out = std::process::Command::new("docker")
        .args(["exec", "fenecbench-pg", "cat", "/sys/fs/cgroup/memory.stat"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let get = |key: &str| -> Option<u64> {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(' '))
            .and_then(|v| v.trim().parse().ok())
    };
    let (anon, shmem, file) = (get("anon")?, get("shmem")?, get("file")?);
    Some((anon + shmem, file.saturating_sub(shmem)))
}

/// One of fenec-server's `/_metrics` without labels, `fenec_memory_bytes`
/// among them: what the engine counts it holds.
fn metric(http: u16, name: &str) -> Option<u64> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", http)).ok()?;
    s.write_all(b"GET /_metrics HTTP/1.1\r\nHost: bench\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut body = String::new();
    s.read_to_string(&mut body).ok()?;
    body.lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(' '))
        .and_then(|v| v.trim().parse().ok())
}

impl Server {
    /// A session whose notices are shown: pgvector says so when its
    /// graph outgrows `maintenance_work_mem` and the build goes on disk.
    fn connect(&self) -> Client {
        let mut cfg: postgres::Config = self.dsn.parse().unwrap();
        cfg.notice_callback(|n| eprintln!("  notice: {}", n.message()));
        cfg.connect(NoTls).unwrap()
    }

    /// The statement a query of the ten nearest is -- of the rows whose
    /// `filter` field equals `$2`, where one is named -- and what a session
    /// runs first to search with a beam of `ef`.
    fn query(&self, ef: usize, filter: Option<&str>) -> (Option<String>, String) {
        match (self.engine, filter) {
            (Engine::Fenec, None) => (
                None,
                format!("get items select id near embed $1 ef {ef} limit 10"),
            ),
            (Engine::Fenec, Some(f)) => (
                None,
                format!("get items select id where {f} = $2 near embed $1 ef {ef} limit 10"),
            ),
            (Engine::Pg, None) => (
                Some(format!("SET hnsw.ef_search = {ef}")),
                "SELECT id FROM items ORDER BY embed <=> $1 LIMIT 10".into(),
            ),
            (Engine::Pg, Some(f)) => (
                Some(format!(
                    "SET hnsw.ef_search = {ef}; SET hnsw.iterative_scan = relaxed_order"
                )),
                format!("SELECT id FROM items WHERE {f} = $2 ORDER BY embed <=> $1 LIMIT 10"),
            ),
        }
    }

    /// Resident memory in bytes: fenec-server's process, the container's.
    fn memory(&self) -> u64 {
        match &self.fenec {
            Some((s, _)) => {
                let out = std::process::Command::new("ps")
                    .args(["-o", "rss=", "-p", &s.pid().to_string()])
                    .output()
                    .unwrap();
                String::from_utf8_lossy(&out.stdout)
                    .trim()
                    .parse::<u64>()
                    .unwrap_or(0)
                    * 1024
            }
            None => {
                let out = std::process::Command::new("docker")
                    .args([
                        "stats",
                        "--no-stream",
                        "--format",
                        "{{.MemUsage}}",
                        "fenecbench-pg",
                    ])
                    .output()
                    .unwrap();
                let used = String::from_utf8_lossy(&out.stdout);
                let used = used.split('/').next().unwrap_or("").trim();
                let (num, unit) =
                    used.split_at(used.find(|c: char| c.is_alphabetic()).unwrap_or(used.len()));
                let f: f64 = num.parse().unwrap_or(0.0);
                let mult = match unit {
                    "GiB" => 1u64 << 30,
                    "MiB" => 1 << 20,
                    "KiB" => 1 << 10,
                    _ => 1,
                };
                (f * mult as f64) as u64
            }
        }
    }

    /// Bytes on disk: fenec-server's file, the table with its index.
    fn disk(&self, c: &mut Client) -> u64 {
        match &self.fenec {
            Some((_, file)) => std::fs::metadata(file).map(|m| m.len()).unwrap_or(0),
            None => c
                .query_one("SELECT pg_total_relation_size('items')", &[])
                .unwrap()
                .get::<_, i64>(0) as u64,
        }
    }
}

/// What every server is sent and asked.
struct Workload<'a> {
    data: &'a Data,
    n: u64,
    dim: usize,
    /// fenec-server's index built once the rows are in, rather than as they land.
    after: bool,
    queries: &'a [Vec<f32>],
    truth: &'a Truth,
    clients: usize,
    /// The beams searched with.
    efs: &'a [usize],
    /// The rows a COPY sends.
    copy_rows: u64,
}

/// fenec-server killed and started again: waits until no vector is left to link, asks
/// every query once unfiltered and once with each filter, as the run did,
/// and measures its memory.
fn reopen(server: &Server, w: &Workload, started: Instant) -> Option<(f64, u64, u64, u64, u64)> {
    while metric(server.http, "fenec_vectors_unlinked") != Some(0) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let secs = started.elapsed().as_secs_f64();
    let mut c = server.connect();
    let qs: Vec<Vector> = w.queries.iter().map(|q| Vector::from(q.clone())).collect();
    for filter in [None, Some("tag"), Some("quarter")] {
        let (_, sql) = server.query(100, filter);
        let stmt = c.prepare(&sql).ok()?;
        for (j, q) in qs.iter().enumerate() {
            let (t, qq) = filters(j);
            let v = if filter == Some("tag") { t } else { qq };
            match filter {
                None => c.query(&stmt, &[q]).ok()?,
                Some(_) => c.query(&stmt, &[q, &v]).ok()?,
            };
        }
    }
    let (s, _) = server.fenec.as_ref()?;
    let (phys, mapped) = footprint(s.pid())?;
    Some((
        secs,
        server.memory(),
        phys,
        mapped,
        metric(server.http, "fenec_memory_bytes").unwrap_or(0),
    ))
}

/// What one server measured.
struct Measured {
    load: f64,
    build: f64,
    memory: u64,
    disk: u64,
    round_trip: f64,
    /// What the server holds of its own, and the pages of its files it
    /// keeps in memory besides, which the kernel takes back under pressure
    /// ([`footprint`], [`pg_memory`]); and what fenec-server's engine counts it
    /// holds. Bytes.
    breakdown: Option<(u64, u64, Option<u64>)>,
    /// The most the server held of its own while the rows went in and its
    /// index was built ([`Peak`]).
    peak: Option<u64>,
    /// fenec-server killed and started again over its file, its documents in
    /// the file rather than written since the open: the seconds until
    /// every vector was linked, and the resident set, footprint, mapped
    /// pages and engine count after every query was asked again.
    reopened: Option<(f64, u64, u64, u64, u64)>,
    /// With `--compact`, the same after a compact of the collection: its
    /// seconds and the memory then.
    compacted: Option<(f64, u64, u64, u64, u64)>,
    /// Recall@10, p50 and p99 in ms, a beam each: unfiltered, then with
    /// the tag's filter and the quarter's.
    searches: [Vec<(usize, f64, f64, f64)>; 3],
    qps: f64,
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

fn run(server: &Server, w: &Workload) -> Measured {
    let Workload {
        data,
        n,
        dim,
        after,
        queries,
        truth,
        clients,
        efs,
        copy_rows,
    } = *w;
    let mut c = server.connect();
    let index = format!("@hnsw(cosine, m={M}, ef_construction={EF_CONSTRUCTION})");
    match server.engine {
        Engine::Fenec => {
            let kept = if after { "" } else { index.as_str() };
            c.batch_execute(&format!(
                "create collection items (tag int @hash, quarter int @hash, embed vector<{dim}> {kept})"
            ))
            .unwrap();
        }
        Engine::Pg => {
            c.batch_execute(&format!(
                "CREATE EXTENSION IF NOT EXISTS vector; DROP TABLE IF EXISTS items; \
                 CREATE TABLE items (id bigint, tag bigint, quarter bigint, embed vector({dim}))"
            ))
            .unwrap();
        }
    }
    // pgvector-rust's bulk load: the type found by name, then a binary COPY.
    let found = c
        .query_one(
            "SELECT pg_type.oid, nspname AS schema FROM pg_type \
             INNER JOIN pg_namespace ON pg_namespace.oid = pg_type.typnamespace \
             WHERE typname = $1",
            &[&"vector"],
        )
        .unwrap();
    let vector = Type::new(
        "vector".into(),
        found.get("oid"),
        Kind::Simple,
        found.get("schema"),
    );
    // A COPY of `copy_rows` rows at a time: one of 250 000 768-dim rows,
    // 770 MB, stalled in Docker's port forwarding, the server waiting for
    // bytes the client could not write.
    let peak = Peak::watch(server);
    let t = Instant::now();
    for from in (0..n).step_by(copy_rows as usize) {
        let sink = c
            .copy_in("COPY items (id, tag, quarter, embed) FROM STDIN WITH (FORMAT BINARY)")
            .unwrap();
        let mut writer =
            BinaryCopyInWriter::new(sink, &[Type::INT8, Type::INT8, Type::INT8, vector.clone()]);
        for i in from..(from + copy_rows).min(n) {
            let v = Vector::from(data.vector(i));
            let t = tag(i);
            writer.write(&[&(i as i64 + 1), &t, &(t % 4), &v]).unwrap();
        }
        writer.finish().unwrap();
        let done = (from + copy_rows).min(n);
        if done % 100_000 == 0 || done == n {
            eprintln!("  {done} rows in {:.1} s", t.elapsed().as_secs_f64());
        }
    }
    let load = t.elapsed().as_secs_f64();
    let t = Instant::now();
    match server.engine {
        Engine::Fenec if after => c
            .batch_execute(&format!("create index on items (embed) {index}"))
            .unwrap(),
        Engine::Fenec => {}
        // Every core the container has: PostgreSQL plans workers by the
        // size of the table, and 768-dim vectors live out of it, in TOAST,
        // which left a build of them to one process; `parallel_workers`
        // settles the number.
        Engine::Pg => {
            let workers = std::thread::available_parallelism().map_or(4, |n| n.get()) - 1;
            c.batch_execute(&format!(
                "SET maintenance_work_mem = '1800MB'; \
                 SET max_parallel_maintenance_workers = {workers}; \
                 ALTER TABLE items SET (parallel_workers = {workers}); \
                 CREATE INDEX ON items USING hnsw (embed vector_cosine_ops) \
                 WITH (m = {M}, ef_construction = {EF_CONSTRUCTION}); \
                 CREATE INDEX ON items (tag); CREATE INDEX ON items (quarter)"
            ))
            .unwrap();
        }
    }
    let build = t.elapsed().as_secs_f64();
    let peak = peak.most();
    eprintln!("  loaded in {load:.1} s, index {build:.1} s more");
    // PostgreSQL's index read into its buffers, as far as they hold it:
    // fenec-server holds its graph in memory.
    if server.engine == Engine::Pg {
        c.batch_execute(
            "CREATE EXTENSION IF NOT EXISTS pg_prewarm; SELECT pg_prewarm('items_embed_idx')",
        )
        .unwrap();
    }

    // The empty round trip.
    let mut rtt: Vec<f64> = (0..200)
        .map(|_| {
            let t = Instant::now();
            c.simple_query("SELECT 1").unwrap();
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    let round_trip = pct(&mut rtt, 0.5);

    let qs: Vec<Vector> = queries.iter().map(|q| Vector::from(q.clone())).collect();
    let kinds: [(Option<&str>, &[Vec<i64>]); 3] = [
        (None, &truth.all),
        (Some("tag"), &truth.tag),
        (Some("quarter"), &truth.quarter),
    ];
    let mut searches: [Vec<(usize, f64, f64, f64)>; 3] = Default::default();
    for (k, (filter, exact_tens)) in kinds.iter().enumerate() {
        for &ef in efs {
            let (prelude, sql) = server.query(ef, *filter);
            if let Some(p) = &prelude {
                c.batch_execute(p).unwrap();
            }
            let stmt = c.prepare(&sql).unwrap();
            // The filter's value, $2: the query's tag or quarter.
            let value = |j: usize| -> i64 {
                let (t, q) = filters(j);
                if *filter == Some("tag") {
                    t
                } else {
                    q
                }
            };
            let ask = |c: &mut Client, j: usize| match filter {
                None => c.query(&stmt, &[&qs[j]]).unwrap(),
                Some(_) => c.query(&stmt, &[&qs[j], &value(j)]).unwrap(),
            };
            for j in 0..qs.len() {
                ask(&mut c, j);
            }
            let mut hits = 0usize;
            // Queries that found none of their ten: a region of the graph
            // the walk never reached, which no beam makes up for.
            let mut lost = 0usize;
            let mut lat = Vec::with_capacity(qs.len());
            for (j, exact) in exact_tens.iter().enumerate() {
                let t = Instant::now();
                let rows = ask(&mut c, j);
                lat.push(t.elapsed().as_secs_f64() * 1e3);
                let found = rows
                    .iter()
                    .filter(|r| exact.contains(&r.get::<_, i64>(0)))
                    .count();
                hits += found;
                lost += (found == 0) as usize;
            }
            let recall = hits as f64 / (10 * qs.len()) as f64;
            let (p50, p99) = (pct(&mut lat, 0.5), pct(&mut lat, 0.99));
            eprintln!(
                "  {}ef {ef}: recall {:.1}%, p50 {p50:.3} ms, p99 {p99:.3} ms, {lost} queries found none",
                filter.map_or(String::new(), |f| format!("{f} filter, ")),
                recall * 100.0
            );
            searches[k].push((ef, recall, p50, p99));
        }
    }
    if server.engine == Engine::Pg {
        c.batch_execute("RESET hnsw.iterative_scan").unwrap();
    }

    // Throughput: `clients` sessions at a beam of 100 for 10 s.
    let (prelude, sql) = server.query(100, None);
    let deadline = Instant::now() + Duration::from_secs(10);
    let started = Instant::now();
    let done: usize = std::thread::scope(|s| {
        let handles: Vec<_> = (0..clients)
            .map(|k| {
                let (prelude, sql) = (&prelude, &sql);
                s.spawn(move || {
                    let mut c = server.connect();
                    if let Some(p) = prelude {
                        c.batch_execute(p).unwrap();
                    }
                    let stmt = c.prepare(sql).unwrap();
                    let qs: Vec<Vector> = queries.iter().map(|q| Vector::from(q.clone())).collect();
                    let mut count = 0;
                    let mut j = k;
                    while Instant::now() < deadline {
                        c.query(&stmt, &[&qs[j % qs.len()]]).unwrap();
                        count += 1;
                        j += clients;
                    }
                    count
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).sum()
    });
    let qps = done as f64 / started.elapsed().as_secs_f64();
    eprintln!("  {clients} clients at ef 100: {qps:.0} queries/s");

    Measured {
        load,
        build,
        memory: server.memory(),
        breakdown: match &server.fenec {
            Some((s, _)) => footprint(s.pid())
                .map(|(held, mapped)| (held, mapped, metric(server.http, "fenec_memory_bytes"))),
            None => pg_memory().map(|(held, cache)| (held, cache, None)),
        },
        peak,
        reopened: None,
        compacted: None,
        disk: server.disk(&mut c),
        round_trip,
        searches,
        qps,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    // N and DIM are the arguments no flag takes; `--after` alone takes no
    // value.
    let mut plain = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--after" | "--compact" => i += 1,
            a if a.starts_with("--") => i += 2,
            a => {
                plain.push(a.to_string());
                i += 1;
            }
        }
    }
    let n: u64 = plain.first().map_or(1_000_000, |a| a.parse().unwrap());
    let dim: usize = plain.get(1).map_or(128, |a| a.parse().unwrap());
    let rank: usize = flag("--rank").map_or(32, |a| a.parse().unwrap());
    let nq: usize = flag("--queries").map_or(1000, |a| a.parse().unwrap());
    let clients: usize = flag("--clients").map_or(8, |a| a.parse().unwrap());
    let only = flag("--only");
    let after = args.iter().any(|a| a == "--after");
    let compact = args.iter().any(|a| a == "--compact");
    let copy_rows: u64 = flag("--copy-rows").map_or(50_000, |a| a.parse().unwrap());
    let efs: Vec<usize> = flag("--efs").map_or(vec![40, 100, 200], |a| {
        a.split(',').map(|x| x.parse().unwrap()).collect()
    });

    let data = Data::new(dim, rank);
    let queries: Vec<Vec<f32>> = (0..nq as u64)
        .map(|j| data.vector(QUERY_BASE + j))
        .collect();
    let t = Instant::now();
    let truth = truth(&data, n, &queries);
    eprintln!(
        "the exact ten of {nq} queries in {:.1} s",
        t.elapsed().as_secs_f64()
    );

    let work = Workload {
        data: &data,
        n,
        dim,
        after,
        queries: &queries,
        truth: &truth,
        clients,
        efs: &efs,
        copy_rows: if copy_rows == 0 { n } else { copy_rows },
    };
    let mut results: Vec<(&str, Measured)> = Vec::new();
    if only.as_deref() != Some("pg") {
        let dir = std::env::temp_dir().join(format!("fenec-scale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("scale.fenec");
        let (pg, http) = (wire::free_port(), wire::free_port());
        let s = wire::start_fenec(&file, pg, http, "scale-bench");
        let server = Server {
            engine: Engine::Fenec,
            dsn: format!("host=127.0.0.1 port={pg} user=fenec dbname=fenec"),
            fenec: Some((s, file)),
            http,
        };
        eprintln!("fenec-server:");
        let mut r = run(&server, &work);
        // With `--compact`, the memory after a compact, which writes the
        // documents into a new image the stores then read through the map.
        if compact {
            let mut c = server.connect();
            let t = Instant::now();
            c.simple_query("compact items").unwrap();
            let secs = t.elapsed().as_secs_f64();
            let (s, _) = server.fenec.as_ref().unwrap();
            r.compacted = footprint(s.pid()).map(|(phys, mapped)| {
                (
                    secs,
                    server.memory(),
                    phys,
                    mapped,
                    metric(server.http, "fenec_memory_bytes").unwrap_or(0),
                )
            });
        }
        // Killed and started again over the same file: the documents come
        // from the mapped file now, where every one was written since the
        // open before.
        let Server { fenec, .. } = server;
        let (process, file) = fenec.unwrap();
        drop(process);
        let (pg, http) = (wire::free_port(), wire::free_port());
        let t = Instant::now();
        let again = wire::start_fenec(&file, pg, http, "scale-bench");
        let server = Server {
            engine: Engine::Fenec,
            dsn: format!("host=127.0.0.1 port={pg} user=fenec dbname=fenec"),
            fenec: Some((again, file)),
            http,
        };
        r.reopened = reopen(&server, &work, t);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
        results.push(("fenec-server", r));
    }
    if only.as_deref() != Some("fenec") {
        match Client::connect(PG, NoTls) {
            Ok(_) => {
                let server = Server {
                    engine: Engine::Pg,
                    dsn: PG.into(),
                    fenec: None,
                    http: 0,
                };
                eprintln!("PostgreSQL + pgvector:");
                let r = run(&server, &work);
                results.push(("PostgreSQL + pgvector", r));
            }
            Err(e) => eprintln!("PostgreSQL is not reachable ({e}): make pgvector-up"),
        }
    }

    println!(
        "\n{n} x {dim}, along {rank} directions; m {M}, ef_construction {EF_CONSTRUCTION}; {nq} queries\n"
    );
    print!("| |");
    for (name, _) in &results {
        print!(" {name} |");
    }
    println!();
    print!("|---|");
    for _ in &results {
        print!("---:|");
    }
    println!();
    let row = |label: &str, f: &dyn Fn(&Measured) -> String| {
        print!("| {label} |");
        for (_, r) in &results {
            print!(" {} |", f(r));
        }
        println!();
    };
    row("load (binary COPY)", &|r| format!("{:.1} s", r.load));
    row("index after it", &|r| format!("{:.1} s", r.build));
    row("load and index", &|r| {
        format!(
            "{:.1} s, {:.0} rows/s",
            r.load + r.build,
            n as f64 / (r.load + r.build)
        )
    });
    let mb = |b: Option<u64>| b.map_or("--".to_string(), |b| format!("{:.0} MB", b as f64 / 1e6));
    row("memory the server holds", &|r| mb(r.breakdown.map(|b| b.0)));
    row("  the most, while loading", &|r| mb(r.peak));
    row("  its files' pages besides", &|r| {
        mb(r.breakdown.map(|b| b.1))
    });
    row("  what fenec-server's engine counts", &|r| {
        mb(r.breakdown.and_then(|b| b.2))
    });
    row("resident, as counted", &|r| mb(Some(r.memory)));
    if let Some((_, r)) = results.iter().find(|(_, r)| r.compacted.is_some()) {
        let (secs, rss, phys, mapped, engine) = r.compacted.unwrap();
        println!(
            "\nfenec-server after a compact of {secs:.1} s: resident {:.0} MB, physical footprint {:.0} MB, \
             the mapped file's resident pages {:.0} MB, the engine's count {:.0} MB",
            rss as f64 / 1e6,
            phys as f64 / 1e6,
            mapped as f64 / 1e6,
            engine as f64 / 1e6
        );
    }
    if let Some((_, r)) = results.iter().find(|(_, r)| r.reopened.is_some()) {
        let (secs, rss, phys, mapped, engine) = r.reopened.unwrap();
        println!(
            "\nfenec-server killed and started again over its file: every vector linked after {secs:.1} s; \
             resident {:.0} MB, physical footprint {:.0} MB, the mapped file's resident pages {:.0} MB, \
             the engine's count {:.0} MB",
            rss as f64 / 1e6,
            phys as f64 / 1e6,
            mapped as f64 / 1e6,
            engine as f64 / 1e6
        );
    }
    row("disk", &|r| format!("{:.0} MB", r.disk as f64 / 1e6));
    row("empty round trip p50", &|r| {
        format!("{:.3} ms", r.round_trip)
    });
    for (k, kind) in ["", "tag (1%), ", "quarter (25%), "].iter().enumerate() {
        for (i, ef) in efs.iter().enumerate() {
            row(&format!("{kind}ef {ef}: recall@10"), &|r| {
                format!("{:.1}%", r.searches[k][i].1 * 100.0)
            });
            row(&format!("{kind}ef {ef}: p50, p99"), &|r| {
                format!("{:.3} ms, {:.3} ms", r.searches[k][i].2, r.searches[k][i].3)
            });
        }
    }
    row(&format!("{clients} clients at ef 100"), &|r| {
        format!("{:.0} queries/s", r.qps)
    });
}
