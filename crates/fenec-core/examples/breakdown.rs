//! Breaks down where the time goes: storage or indexing?
//! `cargo run --release -p fenec-core --example breakdown -- [N] [DIM]`

use std::time::Instant;
use fenec_core::prelude::*;
use fenec_core::vector::VectorIndex;

struct Rng(u64);
impl Rng {
    fn f32(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x >> 12; x ^= x << 25; x ^= x >> 27;
        self.0 = x;
        ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32) / ((1u32 << 24) as f32)
    }
    fn gauss(&mut self) -> f32 { (0..6).map(|_| self.f32()).sum::<f32>() - 3.0 }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let dim: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(128);

    let mut rng = Rng(0xC0FFEE);
    let centers: Vec<Vec<f32>> = (0..64).map(|_| (0..dim).map(|_| rng.gauss()).collect()).collect();
    let vecs: Vec<Vec<f32>> = (0..n)
        .map(|i| centers[i % 64].iter().map(|x| x + rng.gauss() * 0.35).collect())
        .collect();

    // --- 1) storage only (NO vector index)
    let mut db = Database::new();
    db.execute(&Statement::CreateCollection {
        schema: Schema::new("s", vec![
            Field::new("category", DataType::Text).indexed(IndexKind::Hash),
            Field::new("embed", DataType::Vector(dim, VecPrec::F32)),
        ]).unwrap(),
        if_not_exists: false,
    }).unwrap();
    let t = Instant::now();
    for chunk in vecs.chunks(2000) {
        let docs: Vec<Vec<(String, Expr)>> = chunk.iter().map(|v| vec![
            ("category".into(), Expr::Lit(Value::Text("a".into()))),
            ("embed".into(), Expr::Lit(Value::Vector(v.clone()))),
        ]).collect();
        db.execute(&Statement::Put { collection: "s".into(), docs }).unwrap();
    }
    let store_only = t.elapsed().as_secs_f64();
    println!("storage only           {:.2} s   ({:.0} docs/s)", store_only, n as f64 / store_only);

    // --- 2) HNSW only (no storage), with the current batch limit
    let items: Vec<(u64, Vec<f32>)> = vecs.iter().enumerate().map(|(i, v)| (i as u64 + 1, v.clone())).collect();
    let mut ix = VectorIndex::new(dim, VectorIndexSpec::default());
    ix.reserve(n);
    let t = Instant::now();
    ix.insert_batch(&items);
    let hnsw_bulk = t.elapsed().as_secs_f64();
    println!("HNSW only (bulk)       {:.2} s   ({:.0} vectors/s)", hnsw_bulk, n as f64 / hnsw_bulk);

    // --- 3) HNSW in chunks of 2000 (the engine's path today)
    let mut ix2 = VectorIndex::new(dim, VectorIndexSpec::default());
    ix2.reserve(n);
    let t = Instant::now();
    for c in items.chunks(2000) { ix2.insert_batch(c); }
    let hnsw_chunked = t.elapsed().as_secs_f64();
    println!("HNSW only (2000-chunk) {:.2} s ({:.0} vectors/s)", hnsw_chunked, n as f64 / hnsw_chunked);

    println!("\n=> the index is ~{:.0}% of the write time", hnsw_chunked / (store_only + hnsw_chunked) * 100.0);

    // --- 4) graph size
    let g = ix.serialize_graph();
    println!("\ngraph (serialised)     {:.1} MB", g.len() as f64 / 1e6);

    // --- 5) reopen breakdown
    let img = {
        let mut d2 = Database::new();
        d2.execute(&Statement::CreateCollection {
            schema: Schema::new("v", vec![
                Field::new("embed", DataType::Vector(dim, VecPrec::F32))
                    .indexed(IndexKind::Vector(VectorIndexSpec::default())),
            ]).unwrap(),
            if_not_exists: false,
        }).unwrap();
        for chunk in vecs.chunks(2000) {
            let docs: Vec<Vec<(String, Expr)>> = chunk.iter()
                .map(|v| vec![("embed".to_string(), Expr::Lit(Value::Vector(v.clone())))]).collect();
            d2.execute(&Statement::Put { collection: "v".into(), docs }).unwrap();
        }
        d2.snapshot()
    };
    println!("image                  {:.1} MB", img.len() as f64 / 1e6);

    let t = Instant::now();
    let mut d3 = Database::new();
    d3.load(&img).unwrap();
    println!("full load              {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);

    // Storage-only replay (skip the graph record, and there is no index either)
    let store_img = {
        let mut d = Database::new();
        d.execute(&Statement::CreateCollection {
            schema: Schema::new("v", vec![Field::new("embed", DataType::Vector(dim, VecPrec::F32))]).unwrap(),
            if_not_exists: false,
        }).unwrap();
        for chunk in vecs.chunks(2000) {
            let docs: Vec<Vec<(String, Expr)>> = chunk.iter()
                .map(|v| vec![("embed".to_string(), Expr::Lit(Value::Vector(v.clone())))]).collect();
            d.execute(&Statement::Put { collection: "v".into(), docs }).unwrap();
        }
        d.snapshot()
    };
    let t = Instant::now();
    let mut d5 = Database::new();
    d5.load(&store_img).unwrap();
    let replay_ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("  -> storage only      {:.0} ms", replay_ms);
    let _ = d5.collection_names();

    // graphless image = storage-only replay + index rebuild
    // The head is MAGIC + the fixed-width counter header REC_SEQ (1+8+8);
    // that record does not have the [cid][len] shape, so the walk starts after it.
    const HEAD: usize = fenec_core::engine::MAGIC.len() + 1 + 8 + 8;
    let mut nograph = Vec::from(&img[..HEAD]);
    let mut pos = HEAD;
    while pos < img.len() {
        let rec = img[pos];
        let start = pos;
        pos += 1;
        let mut p2 = pos;
        // [rec][cid varint][len varint][payload]
        let read_uv = |p: &mut usize| { let mut r=0u64; let mut sh=0; loop { let b=img[*p]; *p+=1; r|=((b&0x7f) as u64)<<sh; if b&0x80==0 {break} sh+=7; } r };
        let _cid = read_uv(&mut p2);
        let len = read_uv(&mut p2) as usize;
        let end = p2 + len;
        if rec != 4 { nograph.extend_from_slice(&img[start..end]); }
        pos = end;
    }
    // The header carries the body length; dropping the graph records shortens it.
    let body_len = (nograph.len() - HEAD) as u64;
    // The body length is the last 8 bytes of that fixed-width header.
    nograph[HEAD - 8..HEAD].copy_from_slice(&body_len.to_le_bytes());
    println!("graphless image        {:.1} MB", nograph.len() as f64 / 1e6);
    let t = Instant::now();
    let mut d4 = Database::new();
    d4.load(&nograph).unwrap();
    println!("graphless load         {:.0} ms  (the index is rebuilt)", t.elapsed().as_secs_f64() * 1000.0);
    let _ = (d3.collection_names(), d4.collection_names());
}
