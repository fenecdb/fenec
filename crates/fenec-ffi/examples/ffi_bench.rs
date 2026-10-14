//! What a call through the native library costs, against the same work
//! through fenec-server's HTTP handler in process (`fenec_http::handle`,
//! no socket): an open, a put, a `near`. `make ffi-bench`.
//!
//!     cargo run --release -p fenec-ffi --example ffi_bench [rows] [dim]
//!
//! The file is made through the library, checkpointed, and opened by both:
//! the library as an app opens it (`fenec_open`, `fs::open`), the server as
//! it serves one (`fs::open_serving`). A put is a row with its vector, the
//! library handed the vector as `f32`s as the bindings hand it and as JSON
//! as a page's JSON would, the server a `POST /query` body; each durable
//! (an fsync a write, the library's default and `--sync always`) and not.
//!
//! Then an app reading by id on one thread while another writes 1 000 rows
//! of 128 dimensions a text, back to back, buffered: what the reads wait,
//! and how long the reads that waited out a write took -- each a hold of
//! the write lock, the reader asking again within microseconds of its last
//! answer. `ffi_bench beside` runs that part alone, three times (`beside
//! durable` with every text fsynced), and `ffi_bench put` a buffered put of
//! one row alone, 20 000 of them.

use fenec_core::prelude::*;
use fenec_ffi::*;
use std::ffi::c_char;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

fn main() {
    let dir = std::env::temp_dir().join(format!("fenec-ffi-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    if std::env::args().nth(1).as_deref() == Some("beside") {
        let durable = std::env::args().nth(2).as_deref() == Some("durable");
        for _ in 0..3 {
            beside(&dir, durable);
        }
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    // `put`: a buffered put of a row of 128 dimensions alone, 20 000 of
    // them, the vector as `f32`s: what a write of a few documents pays for
    // being asked whether it is worth building beside the readers.
    if std::env::args().nth(1).as_deref() == Some("put") {
        let path = dir.join("put.fenec");
        let h = open(path.to_str().unwrap(), FENEC_OPEN_NO_SYNC);
        ok(h, "create collection t (n int, e vector<128>)", "[]", &[]);
        let v: Vec<f32> = (0..128).map(|i| i as f32 / 128.0).collect();
        let bytes = apart(0, &v);
        let mut us: Vec<f64> = (0..20_000)
            .map(|_| {
                let t = Instant::now();
                ok(h, "put t {n: 0, e: $1}", "[null]", &bytes);
                t.elapsed().as_secs_f64() * 1e6
            })
            .collect();
        call(fenec_close, h);
        let mean = us.iter().sum::<f64>() / us.len() as f64;
        us.sort_by(f64::total_cmp);
        println!(
            "put alone buffered library {:.2} us p50, {:.2} mean (of 20 000)",
            us[us.len() / 2],
            mean
        );
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let args: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let rows = args.first().copied().unwrap_or(20_000);
    let dim = args.get(1).copied().unwrap_or(128);
    let path = dir.join("bench.fenec");
    let p = path.to_str().unwrap();
    let mut rng = 0x2545_f491_4f6c_dd1du64;
    let mut vector = move || -> Vec<f32> {
        (0..dim)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                (rng >> 40) as f32 / (1u64 << 24) as f32 - 0.5
            })
            .collect()
    };

    // The file, made through the library.
    let h = open(p, FENEC_OPEN_NO_SYNC);
    ok(
        h,
        &format!("create collection t (n int, e vector<{dim}> @hnsw(cosine))"),
        "[]",
        &[],
    );
    let t = Instant::now();
    for start in (0..rows).step_by(1000) {
        let mut text = String::from("put t [");
        let mut bytes = Vec::new();
        let n = 1000.min(rows - start);
        for i in 0..n {
            if i > 0 {
                text.push_str(", ");
            }
            text.push_str(&format!("{{n: {}, e: ${}}}", start + i, i + 1));
            bytes.extend(apart(i as u32, &vector()));
        }
        text.push(']');
        let params = format!("[{}]", vec!["null"; n].join(","));
        ok(h, &text, &params, &bytes);
    }
    println!(
        "load      {rows} x {dim} through the library: {:.2} s",
        t.elapsed().as_secs_f64()
    );
    call(fenec_checkpoint, h);
    call(fenec_close, h);
    println!(
        "file      {:.1} MB, checkpointed",
        std::fs::metadata(&path).unwrap().len() as f64 / 1e6
    );

    // An open, five times each.
    let opens: Vec<f64> = (0..5)
        .map(|_| {
            let t = Instant::now();
            let h = open(p, 0);
            let ms = ms(t);
            call(fenec_close, h);
            ms
        })
        .collect();
    let served: Vec<f64> = (0..5)
        .map(|_| {
            let t = Instant::now();
            let db = fenec_core::fs::open_serving(&path, true, Box::new(Ok)).unwrap();
            let ms = ms(t);
            drop(db);
            ms
        })
        .collect();
    println!(
        "open      library {:.1} ms   server {:.1} ms   (median of 5)",
        median(opens),
        median(served)
    );

    let queries: Vec<Vec<f32>> = (0..1000).map(|_| vector()).collect();
    let puts: Vec<Vec<f32>> = (0..1000).map(|_| vector()).collect();
    for durable in [true, false] {
        let h = open(p, if durable { 0 } else { FENEC_OPEN_NO_SYNC });
        let apart_ = times(&puts, |v| {
            ok(h, "put t {n: 0, e: $1}", "[null]", &apart(0, v))
        });
        let json = times(&puts, |v| {
            ok(h, "put t {n: 0, e: $1}", &format!("[{}]", list(v)), &[])
        });
        let near = (!durable).then(|| {
            times(&queries, |v| {
                ok(
                    h,
                    "get t select n near e $1 limit 10",
                    "[null]",
                    &apart(0, v),
                )
            })
        });
        call(fenec_close, h);

        let db = Arc::new(RwLock::new(
            fenec_core::fs::open_serving(&path, true, Box::new(Ok)).unwrap(),
        ));
        let cfg = fenec_http::Config {
            sync_on_write: durable,
            ..Default::default()
        };
        let server = times(&puts, |v| {
            post(&db, &cfg, "put t {n: 0, e: $1}", &list(v));
        });
        let served_near = (!durable).then(|| {
            times(&queries, |v| {
                post(&db, &cfg, "get t select n near e $1 limit 10", &list(v))
            })
        });
        drop(db);
        let what = if durable { "durable" } else { "buffered" };
        println!(
            "put       {what:8} library {:.1} us (f32s apart), {:.1} us (JSON)   server {:.1} us   (p50 of 1000)",
            apart_, json, server
        );
        if let (Some(a), Some(b)) = (near, served_near) {
            println!("near      limit 10  library {a:.1} us   server {b:.1} us   (p50 of 1000)");
        }
    }
    beside(&dir, false);
    let _ = std::fs::remove_dir_all(&dir);
}

/// One thread reading a row by id in a loop, beside another writing 1 000
/// rows of a text, an int and a 128-dim vector a text -- the vectors handed
/// over as `f32`s, as the bindings hand them -- back to back for two
/// seconds, buffered or each text fsynced.
fn beside(dir: &std::path::Path, durable: bool) {
    const DIM: usize = 128;
    const SEEDED: u64 = 10_000;
    let path = dir.join("beside.fenec");
    let _ = std::fs::remove_file(&path);
    let h = open(
        path.to_str().unwrap(),
        if durable { 0 } else { FENEC_OPEN_NO_SYNC },
    );
    ok(
        h,
        &format!("create collection r (k text, n int, e vector<{DIM}>)"),
        "[]",
        &[],
    );
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    let mut vector = move || -> Vec<f32> {
        (0..DIM)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                (rng >> 40) as f32 / (1u64 << 24) as f32 - 0.5
            })
            .collect()
    };
    // The texts made before the run, so the writer is the library alone.
    let texts: Vec<(String, String, Vec<u8>)> = (0..40)
        .map(|t| {
            let (mut text, mut bytes) = (String::from("put r ["), Vec::new());
            for i in 0..1000 {
                if i > 0 {
                    text.push_str(", ");
                }
                text.push_str(&format!("{{k: \"w\", n: {}, e: ${}}}", t * 1000 + i, i + 1));
                bytes.extend(apart(i as u32, &vector()));
            }
            text.push(']');
            (text, format!("[{}]", vec!["null"; 1000].join(",")), bytes)
        })
        .collect();
    for (text, params, bytes) in &texts[..(SEEDED / 1000) as usize] {
        ok(h, text, params, bytes);
    }
    let (stop, written) = (AtomicBool::new(false), AtomicU64::new(0));
    let run = Duration::from_secs(2);
    let mut reads = std::thread::scope(|s| {
        let reader = s.spawn(|| {
            let (mut id, mut us) = (0u64, Vec::new());
            while !stop.load(Ordering::Relaxed) {
                id = (id * 7919 + 13) % SEEDED + 1;
                let params = format!("[{id}]");
                let t = Instant::now();
                ok(h, "get r select k, n where id = $1", &params, &[]);
                us.push(t.elapsed().as_secs_f64() * 1e6);
            }
            us
        });
        let end = Instant::now() + run;
        let mut i = 0;
        while Instant::now() < end {
            let (text, params, bytes) = &texts[i % texts.len()];
            ok(h, text, params, bytes);
            written.fetch_add(1000, Ordering::Relaxed);
            i += 1;
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap()
    });
    call(fenec_close, h);
    // A read that waited out a write: past 50 us, ten times a read alone.
    let mut waits: Vec<f64> = reads.iter().copied().filter(|&u| u > 50.0).collect();
    reads.sort_by(f64::total_cmp);
    waits.sort_by(f64::total_cmp);
    let at = |v: &[f64], p: f64| match v.is_empty() {
        true => 0.0,
        false => v[((v.len() - 1) as f64 * p) as usize],
    };
    println!(
        "beside    {} reads {:.0}k/s {:.1}/{:.1} us p50/p99, {:.2} ms at most   \
         {} waited out a write: {:.2}/{:.2}/{:.2} ms p50/p99/max   writer {:.0} rows/s",
        if durable { "durable" } else { "buffered" },
        reads.len() as f64 / run.as_secs_f64() / 1e3,
        at(&reads, 0.5),
        at(&reads, 0.99),
        at(&reads, 1.0) / 1e3,
        waits.len(),
        at(&waits, 0.5) / 1e3,
        at(&waits, 0.99) / 1e3,
        at(&waits, 1.0) / 1e3,
        written.load(Ordering::Relaxed) as f64 / run.as_secs_f64()
    );
}

fn open(p: &str, flags: u32) -> u64 {
    let (mut h, mut out) = (0, std::ptr::null_mut());
    let code = unsafe {
        fenec_open(
            p.as_ptr(),
            p.len(),
            flags,
            &mut h,
            &mut out,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(code, 0, "{}", taken(out));
    h
}

fn ok(h: u64, sql: &str, params: &str, vectors: &[u8]) {
    let mut out = std::ptr::null_mut();
    let code = unsafe {
        fenec_query(
            h,
            sql.as_ptr(),
            sql.len(),
            params.as_ptr(),
            params.len(),
            vectors.as_ptr(),
            vectors.len(),
            &mut out,
            std::ptr::null_mut(),
        )
    };
    let text = taken(out);
    assert_eq!(code, 0, "{text}");
}

fn call(f: unsafe extern "C" fn(u64, *mut *mut c_char, *mut usize) -> i32, h: u64) {
    let mut out = std::ptr::null_mut();
    let code = unsafe { f(h, &mut out, std::ptr::null_mut()) };
    assert_eq!(code, 0, "{}", taken(out));
}

fn post(db: &Arc<RwLock<Database>>, cfg: &fenec_http::Config, sql: &str, vector: &str) {
    let body = format!("{{\"query\":\"{sql}\",\"params\":[{vector}]}}");
    let req = fenec_http::http::Request {
        method: fenec_http::http::Method::Post,
        target: "/query".into(),
        path: "/query".into(),
        query: Vec::new(),
        headers: Vec::new(),
        body: body.into_bytes(),
        keep_alive: true,
    };
    let resp = fenec_http::handle(db, cfg, &req);
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
}

fn taken(out: *mut c_char) -> String {
    if out.is_null() {
        return String::new();
    }
    let s = unsafe { std::ffi::CStr::from_ptr(out) }
        .to_string_lossy()
        .into_owned();
    unsafe { fenec_free_string(out) };
    s
}

/// A vector as the bindings hand it over: its place, its length, its `f32`s.
fn apart(at: u32, v: &[f32]) -> Vec<u8> {
    let mut b = [at.to_le_bytes(), (v.len() as u32).to_le_bytes()].concat();
    v.iter().for_each(|x| b.extend(x.to_le_bytes()));
    b
}

fn list(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

/// The median of each call's time, in microseconds.
fn times(items: &[Vec<f32>], mut f: impl FnMut(&[f32])) -> f64 {
    let mut us: Vec<f64> = items
        .iter()
        .map(|v| {
            let t = Instant::now();
            f(v);
            t.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    us.sort_by(f64::total_cmp);
    us[us.len() / 2]
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}
