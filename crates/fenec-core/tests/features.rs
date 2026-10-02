//! A build made without the indexes (`Cargo.toml`'s `vector`, `text`,
//! `sparse` and `sorted`): a collection that declares them is made and
//! opened all the same, its documents read and written, its `@sorted`
//! field's comparisons and orders answered by the scan, a `near` over a
//! vector or a sparse vector by measuring every one, as `exact` does, and
//! what needs the others -- `match`, an index made -- refused, the feature
//! named. `web/fenec.test.js` holds the module without the graph to the
//! full one's `near ... exact`, row for row and score for score.
//!
//!     cargo test -p fenec-core --no-default-features --features std-fs --test features

#![cfg(not(any(
    feature = "vector",
    feature = "text",
    feature = "sparse",
    feature = "sorted"
)))]

use fenec_core::prelude::*;

fn run(db: &mut Database, sql: &str) -> Result<Response> {
    let mut last = Response::Affected(0);
    for s in fenec_ql::parse(sql).expect("parse") {
        last = db.execute_with(&s, &[])?;
    }
    Ok(last)
}

fn ints(r: &Response, col: usize) -> Vec<i64> {
    r.rows()
        .expect("rows")
        .rows
        .iter()
        .map(|row| match &row.values[col] {
            Value::Int(n) => *n,
            other => panic!("not an int: {other:?}"),
        })
        .collect()
}

fn declared() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection d (year int @sorted, title text @text, \
         embed vector<3> @hnsw(cosine), s sparse<10> @inverted)",
    )
    .expect("a collection declaring every index is made");
    for (year, title) in [(2021, "rust"), (1999, "wasm"), (2010, "search")] {
        run(
            &mut db,
            &format!(
                r#"put d {{year: {year}, title: "{title}", embed: [1.0, 0.0, 0.0], s: "{{1:0.5}}/10"}}"#
            ),
        )
        .expect("put");
    }
    db
}

#[test]
fn documents_read_and_write_and_the_scan_orders_them() {
    let mut db = declared();
    let r = run(&mut db, "get d select year order year").unwrap();
    assert_eq!(ints(&r, 0), [1999, 2010, 2021]);
    let r = run(
        &mut db,
        "get d select year where year > 2000 order year desc",
    )
    .unwrap();
    assert_eq!(ints(&r, 0), [2021, 2010]);
    run(&mut db, "set d {year: 2030} where year = 1999").unwrap();
    run(&mut db, "del d where year = 2010").unwrap();
    let mut back = Database::new();
    back.load(&db.snapshot()).expect("the image opens");
    let r = run(&mut back, "get d select year order year").unwrap();
    assert_eq!(ints(&r, 0), [2021, 2030]);
}

#[test]
fn what_needs_a_missing_index_is_refused() {
    let mut db = declared();
    run(&mut db, "alter collection d add field other vector<3>").unwrap();
    for (sql, feature) in [
        (r#"get d match title "rust""#, "`text`"),
        ("create index on d (title) @sorted", "`sorted`"),
        ("create index on d (other) @hnsw(cosine)", "`vector`"),
    ] {
        let e = run(&mut db, sql).unwrap_err().to_string();
        assert!(
            e.contains(feature) && e.contains("this build was made without"),
            "{sql}: {e}"
        );
    }
    // Nothing was built, so nothing is half-built: the collection answers.
    assert_eq!(
        ints(&run(&mut db, "get d select year order year").unwrap(), 0).len(),
        3
    );
}

/// A path's ordered index is declared in the file as a field's is: a build
/// without `sorted` opens it, answers its comparisons and its order by the
/// scan, and refuses to make one.
#[test]
fn a_path_declaring_an_ordered_index_is_scanned() {
    let mut db = Database::new();
    let mut schema = Schema::new("j", vec![Field::new("meta", DataType::Json)]).unwrap();
    schema.add_path(Field::new("meta.n", DataType::Json).indexed(IndexKind::SORTED));
    db.execute(&Statement::CreateCollection {
        schema,
        if_not_exists: false,
    })
    .unwrap();
    run(
        &mut db,
        "put j [{meta: {n: 3}}, {meta: {n: 1}}, {meta: {n: 2.5}}]",
    )
    .unwrap();
    let mut back = Database::new();
    back.load(&db.snapshot()).expect("the image opens");
    for d in [&mut db, &mut back] {
        let r = run(d, "get j select id where meta.n >= 2 order meta.n desc").unwrap();
        assert_eq!(ints(&r, 0), [1, 3]);
    }
    let e = run(&mut back, "create index on j (meta.m) @sorted")
        .unwrap_err()
        .to_string();
    assert!(e.contains("`sorted`"), "{e}");
}

/// A small generator, so a failure meets the same case again.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn unit(&mut self) -> f32 {
        (self.next() % 20_001) as f32 / 10_000.0 - 1.0
    }
}

fn scored(r: &Response) -> Vec<(u64, Option<f32>)> {
    r.rows()
        .expect("rows")
        .rows
        .iter()
        .map(|row| (row.id, row.score))
        .collect()
}

/// The ids a page of `want` keeps: the nearest by a reference in `f64`.
fn nearest_by(decl: &str, vectors: &[Vec<f32>], q: &[f32], keep: impl Fn(u64) -> bool) -> Vec<u64> {
    let len = |v: &[f32]| v.iter().map(|a| (*a as f64).powi(2)).sum::<f64>().sqrt();
    let mut all: Vec<(u64, f64)> = (1..=vectors.len() as u64)
        .filter(|&id| keep(id))
        .map(|id| {
            let v = &vectors[id as usize - 1];
            let dot: f64 = v.iter().zip(q).map(|(a, b)| *a as f64 * *b as f64).sum();
            let l2: f64 = v
                .iter()
                .zip(q)
                .map(|(a, b)| (*a as f64 - *b as f64).powi(2))
                .sum();
            let d = match decl {
                d if d.contains("l2") => l2,
                d if d.contains("dot") => -dot,
                _ => 1.0 - dot / (len(v) * len(q)),
            };
            (id, d)
        })
        .collect();
    all.sort_by(|a, b| a.1.total_cmp(&b.1));
    all.into_iter().map(|a| a.0).collect()
}

/// Every vector measured: `near` with no graph is `near ... exact`, and
/// both are the nearest by the metric -- each declaration's, over codes
/// and halves too -- the filter's rows only, paged by `limit` and
/// `offset`, a row with no vector and a deleted one left out, and the page
/// past `near`'s ceiling refused as it is with a graph.
#[test]
fn near_without_the_graph_measures_every_vector() {
    let mut g = Lcg(7);
    for decl in [
        "vector<8> @hnsw(cosine)",
        "vector<8> @hnsw(l2)",
        "vector<8> @hnsw(dot)",
        "vector<8, f16> @hnsw(cosine)",
        "vector<8> @hnsw(cosine, quant=int8)",
        "vector<8> @hnsw(l2, quant=int8)",
        "vector<8> @hnsw(cosine, quant=bit)",
    ] {
        let mut db = Database::new();
        run(&mut db, &format!("create collection v (k int, e {decl})")).unwrap();
        let mut vectors = Vec::new();
        for n in 0..300 {
            let v: Vec<f32> = (0..8).map(|_| g.unit()).collect();
            let text: Vec<String> = v.iter().map(|x| format!("{x:?}")).collect();
            let put = format!("put v {{k: {}, e: [{}]}}", n % 7, text.join(", "));
            run(&mut db, &put).unwrap();
            vectors.push(v);
        }
        run(&mut db, "put v {k: 1}").unwrap();
        run(&mut db, "del v where id = 5").unwrap();
        let q: Vec<f32> = (0..8).map(|_| g.unit()).collect();
        let q_text: Vec<String> = q.iter().map(|x| format!("{x:?}")).collect();
        let q_text = q_text.join(", ");
        for (tail, offset, limit) in [
            ("limit 10", 0, 10),
            ("where k = 3 limit 5", 0, 5),
            ("where k > 1 limit 7 offset 4", 4, 7),
            ("limit 400", 0, 400),
        ] {
            let near = scored(&run(&mut db, &format!("get v near e [{q_text}] {tail}")).unwrap());
            let exact = format!("get v near e [{q_text}] exact {tail}");
            assert_eq!(
                near,
                scored(&run(&mut db, &exact).unwrap()),
                "{decl} {tail}"
            );
            let keep = |id: u64| {
                let k = (id - 1) % 7;
                id != 5
                    && match tail {
                        t if t.starts_with("where k = 3") => k == 3,
                        t if t.starts_with("where k > 1") => k > 1,
                        _ => true,
                    }
            };
            let want: Vec<u64> = nearest_by(decl, &vectors, &q, keep)
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect();
            let got: Vec<u64> = near.iter().map(|r| r.0).collect();
            // Halves round each component: two vectors closer than that
            // may change places.
            match decl.contains("f16") {
                false => assert_eq!(got, want, "{decl} {tail}"),
                true => assert_eq!(got.len(), want.len(), "{decl} {tail}"),
            }
            // A distance under l2, a similarity otherwise.
            let l2 = decl.contains("l2");
            assert!(
                near.windows(2)
                    .all(|w| (w[0].1 >= w[1].1) != l2 || w[0].1 == w[1].1),
                "{decl} {tail}: the scores in order"
            );
        }
        let e = run(&mut db, &format!("get v near e [{q_text}] limit 10001")).unwrap_err();
        assert!(e.to_string().contains("at most 10000 rows"), "{e}");
        let e = run(&mut db, "get v near e [1.0, 2.0] limit 1").unwrap_err();
        assert!(e.to_string().contains("8 dimensions"), "{e}");
        let mut back = Database::new();
        back.load(&db.snapshot()).expect("the image opens");
        let page = format!("get v near e [{q_text}] limit 10");
        let (a, b) = (run(&mut db, &page).unwrap(), run(&mut back, &page).unwrap());
        assert_eq!(scored(&a), scored(&b), "{decl}: reopened");
    }
}

/// A sparse `near` with no inverted index scores every document, as
/// `exact` does with one: by the dot product, a document sharing no
/// dimension with the query left out; `ef` refused as it is with one.
#[test]
fn sparse_near_without_the_index_scores_every_document() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection s (k int, w sparse<6> @inverted)",
    )
    .unwrap();
    run(
        &mut db,
        r#"put s [{k: 1, w: "{1:0.5,3:1}/6"}, {k: 2, w: "{2:2}/6"}, {k: 1, w: "{1:1,6:0.25}/6"}, {k: 2, w: "{4:3}/6"}, {k: 1}]"#,
    )
    .unwrap();
    let r = run(&mut db, r#"get s near w "{1:1,3:0.5}/6" limit 10"#).unwrap();
    assert_eq!(scored(&r), [(1, Some(1.0)), (3, Some(1.0))]);
    let r = run(&mut db, r#"get s near w "{1:1,6:4}/6" where k = 1 limit 1"#).unwrap();
    assert_eq!(scored(&r), [(3, Some(2.0))]);
    let e = run(&mut db, r#"get s near w "{1:1}/6" ef 10 limit 1"#).unwrap_err();
    assert!(e.to_string().contains("`ef`"), "{e}");
}
