//! A build made without the indexes (`Cargo.toml`'s `indexes`): a
//! collection that declares them is made and opened all the same, its
//! documents read and written, its `@sorted` field's comparisons and orders
//! answered by the scan, a `near` over a vector or a sparse vector by
//! measuring every one, as `exact` does, and what needs the others --
//! `match`, an index made -- refused, the feature named. `web/fenec.test.js`
//! holds the module without the graph to the full one's `near ... exact`,
//! row for row and score for score.
//!
//! And a build made without the rest of the language as well (`language`):
//! a file declaring what it lacks -- `tests/language.fenec`, which only a
//! build with it can write -- opens, its other collections answer, and a
//! statement that needs what is missing is refused, the feature named. Run
//! both ways, each test asking for what it needs:
//!
//!     cargo test -p fenec-core --no-default-features --features std-fs,language --test features
//!     cargo test -p fenec-core --no-default-features --features std-fs --test features

#![cfg(not(any(
    feature = "vector",
    feature = "text",
    feature = "sparse",
    feature = "sorted"
)))]

use fenec_core::prelude::*;

/// The statements of `sql`, each parsed once the ones before it ran: a
/// refusal a statement's own check makes comes at its parse.
fn run(db: &mut Database, sql: &str) -> Result<Response> {
    let mut last = Response::Affected(0);
    for text in sql.split(';').filter(|t| !t.trim().is_empty()) {
        for s in fenec_ql::parse_for(db, text)? {
            last = db.execute_with(&s, &[])?;
        }
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

/// Facets need no index: the scan counts them, over the rows a filter
/// keeps, beside the page.
#[cfg(feature = "facets")]
#[test]
fn facets_count_without_the_indexes() {
    let mut db = declared();
    run(&mut db, r#"put d {year: 2021, title: "more"}"#).unwrap();
    let r = run(
        &mut db,
        "get d select year where year > 2000 limit 1 facet year",
    )
    .unwrap();
    let rs = r.rows().unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(
        rs.facets[0].values,
        [(Value::Int(2021), 2), (Value::Int(2010), 1)]
    );
}

#[test]
fn what_needs_a_missing_index_is_refused() {
    let mut db = declared();
    run(&mut db, "alter collection d add field other vector<3>").unwrap();
    // The marks are asked of before the `match` they mark is run.
    let marks = match cfg!(feature = "highlight") {
        true => "`text`",
        false => "`highlight`",
    };
    for (sql, feature) in [
        (r#"get d match title "rust""#, "`text`"),
        (r#"get d match title "rsut" fuzzy prefix"#, "`text`"),
        (r#"get d select highlight(title) match title "rust""#, marks),
        (
            r#"get d select snippet(title, 3) match title "rust" facet year"#,
            marks,
        ),
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

/// A point field declaring `@geo`, which needs the `sorted` feature: a
/// build without it opens the file, answers a radius, a box and `near` by
/// measuring every point -- what the index's answers are held to -- and
/// refuses to make one.
#[cfg(feature = "geo")]
#[test]
fn a_point_index_declared_is_answered_by_the_scan() {
    let mut db = Database::new();
    run(&mut db, "create collection p (n int, loc geo @geo)").unwrap();
    run(
        &mut db,
        "put p [{n: 1, loc: [13.4, 52.5]}, {n: 2, loc: [13.41, 52.5]}, \
         {n: 3, loc: [2.35, 48.86]}, {n: 4}, {n: 5, loc: [179.99, 0]}]",
    )
    .unwrap();
    let mut back = Database::new();
    back.load(&db.snapshot()).expect("the image opens");
    for d in [&mut db, &mut back] {
        let r = run(
            d,
            "get p select n where distance(loc, [13.4, 52.5]) <= 1000",
        )
        .unwrap();
        assert_eq!(ints(&r, 0), [1, 2]);
        let r = run(d, "get p select n where within(loc, [170, -1, -170, 1])").unwrap();
        assert_eq!(ints(&r, 0), [5]);
        let r = run(d, "get p select n near loc [2.0, 48.0] limit 3").unwrap();
        assert_eq!(ints(&r, 0), [3, 1, 2]);
    }
    run(&mut back, "alter collection p add field other geo").unwrap();
    let e = run(&mut back, "create index on p (other) @geo")
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

/// An index over several fields declared: its queries are the scan's, a
/// unique one is asked by reading the rows, and one made is refused.
#[test]
fn an_index_over_several_fields_declared_is_answered_by_the_scan() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection p (owner int, at int, slug text, @sorted(owner, at desc), \
         @unique(owner, slug))",
    )
    .expect("a collection declaring one is made");
    for (owner, at, slug) in [(1, 5, "a"), (2, 7, "b"), (1, 9, "c"), (1, 3, "d")] {
        run(
            &mut db,
            &format!(r#"put p {{owner: {owner}, at: {at}, slug: "{slug}"}}"#),
        )
        .unwrap();
    }
    let r = run(
        &mut db,
        "get p select at where owner = 1 order at desc limit 2",
    )
    .unwrap();
    assert_eq!(ints(&r, 0), [9, 5]);
    let e = run(&mut db, r#"put p {owner: 1, at: 1, slug: "c"}"#).unwrap_err();
    assert!(matches!(e, Error::Duplicate(_)), "{e}");
    run(&mut db, r#"put p {owner: 2, at: 1, slug: "c"}"#).unwrap();
    let e = run(&mut db, "create index on p (slug, at) @sorted").unwrap_err();
    assert!(e.to_string().contains("`composite`"), "{e}");
    // A `lookup` the index answers -- a key over several fields, or one
    // field it leads with no `@hash` -- is refused naming it.
    run(&mut db, "create collection o (owner int)").unwrap();
    run(&mut db, "put o {owner: 1}").unwrap();
    for sql in [
        "get o lookup p on (owner, at) = (owner, id)",
        "get o lookup p on owner = owner order at desc limit 1",
    ] {
        let e = run(&mut db, sql).unwrap_err();
        assert!(e.to_string().contains("`composite`"), "{sql}: {e}");
    }
    let mut back = Database::new();
    back.load(&db.snapshot()).expect("the image opens");
    let r = run(&mut back, "get p select at where owner = 1 order at desc").unwrap();
    assert_eq!(ints(&r, 0), [9, 5, 3]);
    let e = run(&mut back, r#"put p {owner: 2, at: 4, slug: "b"}"#).unwrap_err();
    assert!(matches!(e, Error::Duplicate(_)), "{e}");
    run(&mut back, "drop index on p (owner, slug)").unwrap();
    run(&mut back, r#"put p {owner: 2, at: 4, slug: "b"}"#).unwrap();
}

#[cfg(feature = "nocase")]
#[test]
fn a_nocase_field_is_its_fold_without_the_indexes() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection u (email text nocase @unique, name text nocase @sorted, n int, \
         org text nocase, @unique(org, n))",
    )
    .expect("a collection declaring them is made");
    for (i, (e, name)) in [
        ("Alice@X.com", "Straße"),
        ("bob@x.com", "ömer"),
        ("CAROL@x.com", "STRASSE"),
        ("dave@x.com", "Ömer"),
    ]
    .iter()
    .enumerate()
    {
        run(
            &mut db,
            &format!(r#"put u {{email: "{e}", name: "{name}", n: {i}, org: "ACME"}}"#),
        )
        .unwrap();
    }
    let r = run(&mut db, r#"get u select n where email = "alice@x.COM""#).unwrap();
    assert_eq!(ints(&r, 0), [0]);
    let r = run(&mut db, r#"get u select n where name = "strasse" order n"#).unwrap();
    assert_eq!(ints(&r, 0), [0, 2]);
    // The scan orders by the fold, ties by id: `ömer` and `Ömer` together,
    // `Straße` and `STRASSE` together.
    let r = run(&mut db, "get u select n order name, id").unwrap();
    assert_eq!(ints(&r, 0), [0, 2, 1, 3]);
    let e = run(&mut db, r#"put u {email: "ALICE@x.com", n: 9}"#).unwrap_err();
    assert!(matches!(e, Error::Duplicate(_)), "{e}");
    let e = run(&mut db, r#"put u {email: "eve@x.com", n: 1, org: "acme"}"#).unwrap_err();
    assert!(matches!(e, Error::Duplicate(_)), "{e}");
    let mut back = Database::new();
    back.load(&db.snapshot()).expect("the image opens");
    let r = run(&mut back, r#"get u select n where email = "carol@X.COM""#).unwrap();
    assert_eq!(ints(&r, 0), [2]);
}

/// Asserts `sql` is refused naming `feature` as one this build lacks.
#[cfg(not(all(
    feature = "nocase",
    feature = "collate",
    feature = "geo",
    feature = "facets",
    feature = "analytics"
)))]
fn refused(db: &mut Database, sql: &str, feature: &str) {
    let e = run(db, sql).unwrap_err().to_string();
    assert!(
        e.contains(&format!(
            "`{feature}` feature, which this build was made without"
        )),
        "{sql}: {e}"
    );
}

/// `tests/language.fenec`, which a build with every feature wrote
/// (`tests/language_file.rs`): a `nocase` field's collection, a collated
/// one's, points under `@geo`, and a collection declaring none of them.
#[cfg(not(all(feature = "nocase", feature = "collate", feature = "geo")))]
fn language_file() -> Database {
    let mut db = Database::new();
    db.load(include_bytes!("language.fenec"))
        .expect("a file declaring what the build lacks opens");
    db
}

/// The file's collection that declares nothing missing answers as ever,
/// and an image of the file written again opens again.
#[cfg(not(all(feature = "nocase", feature = "collate", feature = "geo")))]
#[test]
fn a_file_declaring_what_the_build_lacks_opens() {
    let mut db = language_file();
    let r = run(&mut db, "get plain select n order n").unwrap();
    assert_eq!(ints(&r, 0), [1, 2]);
    run(&mut db, r#"put plain {n: 3, tag: "c"}"#).unwrap();
    let r = run(&mut db, r#"get plain select n where tag = "c""#).unwrap();
    assert_eq!(ints(&r, 0), [3]);
    let mut back = Database::new();
    back.load(&db.snapshot()).expect("its image opens");
    let r = run(&mut back, "get plain select n order n").unwrap();
    assert_eq!(ints(&r, 0), [1, 2, 3]);
}

/// A `nocase` field's text compared as written would find what its fold
/// does not and miss what it does: every statement over its collection is
/// refused, an inner `get` too, and so is declaring one. Dropped, the rest
/// goes on.
#[cfg(not(feature = "nocase"))]
#[test]
fn a_nocase_field_s_collection_is_refused() {
    let mut db = language_file();
    for sql in [
        r#"get users select n where email = "alice@x.com""#,
        "get users count",
        r#"put users {email: "carol@x.com", n: 3}"#,
        "set users {n: 9} where n = 1",
        "del users where n = 2",
        "get plain where n in (get users select n)",
        "get plain lookup users on id = n",
        "create collection more (email text nocase)",
        "alter collection plain add field email text nocase",
    ] {
        refused(&mut db, sql, "nocase");
    }
    run(&mut db, "drop collection users").unwrap();
    assert_eq!(
        ints(&run(&mut db, "get plain select n").unwrap(), 0),
        [1, 2]
    );
}

/// A collated field's order needs ICU's tables: a statement over its
/// collection is refused, as `order ... collate` and declaring one are.
#[cfg(not(feature = "collate"))]
#[test]
fn a_collated_field_s_collection_is_refused() {
    let mut db = language_file();
    for sql in [
        "get people select n order name",
        r#"put people {name: "ana", n: 3}"#,
        "get plain select n order tag collate und",
        "set plain {n: 1} order tag collate tr limit 1",
        "create collection more (name text collate tr)",
    ] {
        refused(&mut db, sql, "collate");
    }
}

/// A point is read, written and checked as one; what measures one -- a
/// radius, a box, `near` -- and its index are refused.
#[cfg(not(feature = "geo"))]
#[test]
fn points_are_kept_and_their_questions_refused() {
    let mut db = language_file();
    run(&mut db, "put places {n: 3, loc: [-73.98, 40.75]}").unwrap();
    let r = run(&mut db, "get places select n order n").unwrap();
    assert_eq!(ints(&r, 0), [1, 2, 3]);
    let e = run(&mut db, "put places {n: 4, loc: [200, 0]}").unwrap_err();
    assert!(e.to_string().contains("180"), "a point is checked: {e}");
    for sql in [
        "get places select n where distance(loc, [13.4, 52.5]) < 1000",
        "get places select n where within(loc, [0, 0, 20, 60])",
        "get places select n near loc [2.0, 48.0] limit 1",
        "alter collection places add field to geo; create index on places (to) @geo",
    ] {
        refused(&mut db, sql, "geo");
    }
}

/// `facet` counts beside the page; a build without it refuses one.
#[cfg(not(feature = "facets"))]
#[test]
fn facets_are_refused() {
    let mut db = declared();
    refused(&mut db, "get d select year facet year", "facets");
    refused(
        &mut db,
        "get d select year where year > 2000 limit 1 facet year",
        "facets",
    );
    refused(&mut db, "get d facet year ranges [0, 2000, 3000]", "facets");
}

/// `count(distinct ...)`, `first`, `last` and `having` are standard and in
/// every build; `bucket` and the sketches are refused by name without
/// `analytics`.
#[cfg(not(feature = "analytics"))]
#[test]
fn the_sketches_and_buckets_are_refused_and_the_standard_aggregates_answer() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection e (user text, n int, at timestamp)",
    )
    .unwrap();
    run(
        &mut db,
        r#"put e [{user: "a", n: 1, at: 1000}, {user: "a", n: 2, at: 2000}, {user: "b", n: 4, at: 3000}]"#,
    )
    .unwrap();
    let r = run(
        &mut db,
        "get e select sum(n) as s, count() as c, min(n) as lo, max(n) as hi, avg(n) as a, user \
         group user order user",
    )
    .unwrap();
    assert_eq!(ints(&r, 0), [3, 4]);
    let r = run(&mut db, "get e select count(distinct user) as u").unwrap();
    assert_eq!(ints(&r, 0), [2]);
    let r = run(
        &mut db,
        "get e select first(n by at) as f, last(n by at) as l, user group user order user",
    )
    .unwrap();
    assert_eq!((ints(&r, 0), ints(&r, 1)), (vec![1, 4], vec![2, 4]));
    let r = run(
        &mut db,
        "get e select user, count() as c group user having c > 1",
    )
    .unwrap();
    assert_eq!(ints(&r, 1), [2]);
    for sql in [
        "get e select approx_count_distinct(user)",
        "get e select hll_accumulate(user)",
        "get e select hll_combine(user)",
        "get e select bucket(at, 1h) as h, count() group h",
        "get e select bucket(at, 1d, 'Europe/Berlin') as h, count() group h",
        "get e select n where hll_estimate(null) = 0",
    ] {
        refused(&mut db, sql, "analytics");
    }
    // A level that folds is the aggregates' and the lookup's, in every
    // build: each user's rows of `e` folded beside them.
    run(&mut db, "create collection u (name text)").unwrap();
    run(&mut db, "create collection v (user text @hash, n int)").unwrap();
    run(&mut db, r#"put u [{name: "a"}, {name: "b"}, {name: "c"}]"#).unwrap();
    run(
        &mut db,
        r#"put v [{user: "a", n: 1}, {user: "a", n: 2}, {user: "b", n: 4}]"#,
    )
    .unwrap();
    let r = run(
        &mut db,
        "get u select name lookup v on user = name select count() as c, sum(n) as s",
    )
    .unwrap();
    let rs = r.rows().unwrap();
    let n = rs.nested.as_ref().unwrap();
    let folds: Vec<Vec<Value>> = (0..3).map(|i| n.group(i)[0].values.clone()).collect();
    assert_eq!(
        folds,
        [
            vec![Value::Int(2), Value::Int(3)],
            vec![Value::Int(1), Value::Int(4)],
            vec![Value::Int(0), Value::Null]
        ]
    );
}
