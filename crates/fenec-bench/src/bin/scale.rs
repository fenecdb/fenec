//! fenec-pg against PostgreSQL + pgvector at scale, both over the pg wire:
//! `make scale-bench`.
//!
//! ```text
//! cargo run --release -p fenec-bench --bin scale -- [N] [DIM] [--rank R] [--queries Q]
//!     [--clients C] [--only fenec|pg] [--after] [--efs 40,100,200]
//! ```
//!
//! N vectors of DIM dimensions -- the generator `quant` uses, 64 centres
//! spread along R directions (32 unless given), as embeddings vary along
//! far fewer directions than they have -- go into each server by a binary
//! COPY through the same client, the `postgres` crate with pgvector-rust's
//! `Vector`:
//!
//!   * fenec-pg, started here over an empty file, its HNSW index kept as
//!     the rows land -- or with `--after` built by a `create index` once
//!     they are in;
//!   * PostgreSQL + pgvector, the container `make pgvector-up` starts
//!     (skipped without it), its index built after the COPY as pgvector
//!     advises, by as many processes as the container has cores
//!     (`max_parallel_maintenance_workers`) and in memory
//!     (`maintenance_work_mem`), then read into its buffers (`pg_prewarm`).
//!
//! Both indexes take m = 16 and ef_construction = 64, pgvector's defaults.
//! Each server is then asked Q held-out queries at beams of 40, 100 and 200
//! (`ef`, `hnsw.ef_search`; `--efs` names others) by one client, once to warm and once measured,
//! for recall@10 against the exact ten -- found once, by brute force over
//! the vectors -- and latency; and at a beam of 100 by C clients (8) for
//! 10 s, for throughput. PostgreSQL answers from inside Docker's virtual
//! machine, whose network carries every byte, and fenec-pg on the host, so
//! the round trip of an empty query is measured for each. Memory is
//! fenec-pg's resident set and the container's use, and the space on disk
//! fenec-pg's file and the table with its index.

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
const COPY_ROWS: usize = 50_000;

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

/// The exact ten of each query by cosine, their ids from 1: a range of the
/// rows to a thread, each keeping its own ten, merged at the end.
fn truth(data: &Data, n: u64, queries: &[Vec<f32>]) -> Vec<Vec<i64>> {
    let qs: Vec<Vec<f32>> = queries.iter().map(|q| unit(q)).collect();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()) as u64;
    let per = n.div_ceil(threads);
    let parts: Vec<Vec<Vec<(f32, i64)>>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let qs = &qs;
                s.spawn(move || {
                    let mut best: Vec<Vec<(f32, i64)>> = vec![Vec::new(); qs.len()];
                    for i in t * per..((t + 1) * per).min(n) {
                        let v = data.vector(i);
                        let norm = dot(&v, &v).sqrt();
                        for (b, q) in best.iter_mut().zip(qs) {
                            let sim = dot(q, &v) / norm;
                            if b.len() < 10 || sim > b[9].0 {
                                b.push((sim, i as i64 + 1));
                                b.sort_by(|x, y| y.0.total_cmp(&x.0));
                                b.truncate(10);
                            }
                        }
                    }
                    best
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    (0..queries.len())
        .map(|q| {
            let mut all: Vec<(f32, i64)> = parts.iter().flat_map(|p| p[q].clone()).collect();
            all.sort_by(|x, y| y.0.total_cmp(&x.0));
            all.iter().take(10).map(|x| x.1).collect()
        })
        .collect()
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
    /// fenec-pg's process, and the file it keeps.
    fenec: Option<(wire::Server, std::path::PathBuf)>,
}

impl Server {
    /// A session whose notices are shown: pgvector says so when its
    /// graph outgrows `maintenance_work_mem` and the build goes on disk.
    fn connect(&self) -> Client {
        let mut cfg: postgres::Config = self.dsn.parse().unwrap();
        cfg.notice_callback(|n| eprintln!("  notice: {}", n.message()));
        cfg.connect(NoTls).unwrap()
    }

    /// The statement a query of the ten nearest is, and what a session
    /// runs first to search with a beam of `ef`.
    fn query(&self, ef: usize) -> (Option<String>, String) {
        match self.engine {
            Engine::Fenec => (
                None,
                format!("get items select id near embed $1 ef {ef} limit 10"),
            ),
            Engine::Pg => (
                Some(format!("SET hnsw.ef_search = {ef}")),
                "SELECT id FROM items ORDER BY embed <=> $1 LIMIT 10".into(),
            ),
        }
    }

    /// Resident memory in bytes: fenec-pg's process, the container's.
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

    /// Bytes on disk: fenec-pg's file, the table with its index.
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
    /// fenec-pg's index built once the rows are in, rather than as they land.
    after: bool,
    queries: &'a [Vec<f32>],
    truth: &'a [Vec<i64>],
    clients: usize,
    /// The beams searched with.
    efs: &'a [usize],
}

/// What one server measured.
struct Measured {
    load: f64,
    build: f64,
    memory: u64,
    disk: u64,
    round_trip: f64,
    /// Recall@10, p50 and p99 in ms, a beam each.
    searches: Vec<(usize, f64, f64, f64)>,
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
    } = *w;
    let mut c = server.connect();
    let index = format!("@hnsw(cosine, m={M}, ef_construction={EF_CONSTRUCTION})");
    match server.engine {
        Engine::Fenec => {
            let kept = if after { "" } else { index.as_str() };
            c.batch_execute(&format!(
                "create collection items (embed vector<{dim}> {kept})"
            ))
            .unwrap();
        }
        Engine::Pg => {
            c.batch_execute(&format!(
                "CREATE EXTENSION IF NOT EXISTS vector; DROP TABLE IF EXISTS items; \
                 CREATE TABLE items (id bigint, embed vector({dim}))"
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
    // A COPY of `COPY_ROWS` rows at a time: one of 250 000 768-dim rows,
    // 770 MB, stalled in Docker's port forwarding, the server waiting for
    // bytes the client could not write.
    let t = Instant::now();
    for from in (0..n).step_by(COPY_ROWS) {
        let sink = c
            .copy_in("COPY items (id, embed) FROM STDIN WITH (FORMAT BINARY)")
            .unwrap();
        let mut writer = BinaryCopyInWriter::new(sink, &[Type::INT8, vector.clone()]);
        for i in from..(from + COPY_ROWS as u64).min(n) {
            let v = Vector::from(data.vector(i));
            writer.write(&[&(i as i64 + 1), &v]).unwrap();
        }
        writer.finish().unwrap();
        let done = (from + COPY_ROWS as u64).min(n);
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
                 WITH (m = {M}, ef_construction = {EF_CONSTRUCTION})"
            ))
            .unwrap();
        }
    }
    let build = t.elapsed().as_secs_f64();
    eprintln!("  loaded in {load:.1} s, index {build:.1} s more");
    // PostgreSQL's index read into its buffers, as far as they hold it:
    // fenec-pg holds its graph in memory.
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

    let mut searches = Vec::new();
    for &ef in efs {
        let (prelude, sql) = server.query(ef);
        if let Some(p) = &prelude {
            c.batch_execute(p).unwrap();
        }
        let stmt = c.prepare(&sql).unwrap();
        let qs: Vec<Vector> = queries.iter().map(|q| Vector::from(q.clone())).collect();
        for q in &qs {
            c.query(&stmt, &[q]).unwrap();
        }
        let mut hits = 0usize;
        // Queries that found none of their ten: a region of the graph the
        // walk never reached, which no beam makes up for.
        let mut lost = 0usize;
        let mut lat = Vec::with_capacity(qs.len());
        for (q, exact) in qs.iter().zip(truth) {
            let t = Instant::now();
            let rows = c.query(&stmt, &[q]).unwrap();
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
            "  ef {ef}: recall {:.1}%, p50 {p50:.3} ms, p99 {p99:.3} ms, {lost} queries found none",
            recall * 100.0
        );
        searches.push((ef, recall, p50, p99));
    }

    // Throughput: `clients` sessions at a beam of 100 for 10 s.
    let (prelude, sql) = server.query(100);
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
            "--after" => i += 1,
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
        };
        eprintln!("fenec-pg:");
        let r = run(&server, &work);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
        results.push(("fenec-pg", r));
    }
    if only.as_deref() != Some("fenec") {
        match Client::connect(PG, NoTls) {
            Ok(_) => {
                let server = Server {
                    engine: Engine::Pg,
                    dsn: PG.into(),
                    fenec: None,
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
    row("memory", &|r| format!("{:.0} MB", r.memory as f64 / 1e6));
    row("disk", &|r| format!("{:.0} MB", r.disk as f64 / 1e6));
    row("empty round trip p50", &|r| {
        format!("{:.3} ms", r.round_trip)
    });
    for (i, ef) in efs.iter().enumerate() {
        row(&format!("ef {ef}: recall@10"), &|r| {
            format!("{:.1}%", r.searches[i].1 * 100.0)
        });
        row(&format!("ef {ef}: p50, p99"), &|r| {
            format!("{:.3} ms, {:.3} ms", r.searches[i].2, r.searches[i].3)
        });
    }
    row(&format!("{clients} clients at ef 100"), &|r| {
        format!("{:.0} queries/s", r.qps)
    });
}
