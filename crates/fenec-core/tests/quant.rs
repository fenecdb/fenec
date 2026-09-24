//! A quantized index (`@hnsw(..., quant=int8|bit)`) holds codes rather than
//! vectors. The codes find the candidates and the documents' own vectors put
//! them in order, so `near` answers in exact distances; an int8 code reads
//! only the candidates that can still make the page, `exact` reads every
//! vector, a filtered set under the ANN budget is ranked by its codes and
//! reads the beam's worth, and a checkpoint keeps a graph over codes as it
//! keeps one over vectors.

use fenec_core::prelude::*;

fn run(db: &mut Database, sql: &str) {
    for stmt in fenec_ql::parse(sql).expect("parse") {
        db.execute(&stmt).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

fn hits(db: &Database, sql: &str, params: &[Value]) -> Vec<(u64, f32)> {
    let r = db
        .query(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| (r.id, r.score.unwrap_or(f32::NAN)))
        .collect()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f32 {
        (self.next() % 20_001) as f32 / 10_000.0 - 1.0
    }
}

const DIM: usize = 64;
const ROWS: usize = 3000;

/// Vectors around forty centres, as embeddings gather around topics. The
/// centres sit away from the origin, so most signs are a cluster's own:
/// hard on bit codes, which is what the beam they default to is for.
fn vectors(n: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng(0x5eed);
    let centres: Vec<Vec<f32>> = (0..40)
        .map(|_| (0..DIM).map(|_| rng.unit()).collect())
        .collect();
    let mut rng = Rng(seed);
    (0..n)
        .map(|_| {
            let c = &centres[(rng.next() % 40) as usize];
            c.iter().map(|x| x + 0.35 * rng.unit()).collect()
        })
        .collect()
}

/// `docs` over the same rows under `index`; a row's kind is its id's
/// remainder by three, `a` for ids 1, 4, 7 ...
fn filled(index: &str) -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        &format!("create collection docs (kind text @hash, v vector<{DIM}> {index})"),
    );
    let put = fenec_ql::parse_one("put docs {kind: $1, v: $2}").unwrap();
    for (i, v) in vectors(ROWS, 7).into_iter().enumerate() {
        db.execute_with(
            &put,
            &[Value::Text(["a", "b", "c"][i % 3].into()), Value::Vector(v)],
        )
        .unwrap();
    }
    db
}

/// The three indexes the read-only tests share, each built once: a build is
/// most of these tests' time.
fn shared(index: &'static str) -> &'static Database {
    use std::sync::OnceLock;
    static PLAIN: OnceLock<Database> = OnceLock::new();
    static INT8: OnceLock<Database> = OnceLock::new();
    static BIT: OnceLock<Database> = OnceLock::new();
    let cell = match index {
        "@hnsw(cosine)" => &PLAIN,
        "@hnsw(cosine, quant=int8)" => &INT8,
        "@hnsw(cosine, quant=bit)" => &BIT,
        other => panic!("not shared: {other}"),
    };
    cell.get_or_init(|| filled(index))
}

fn queries() -> Vec<[Value; 1]> {
    vectors(100, 99)
        .into_iter()
        .map(|v| [Value::Vector(v)])
        .collect()
}

/// How many documents' vectors `sql` read, and of how many candidates, as
/// `explain` states it: "near: 12 of the 100 candidates of the codes read".
fn reads(db: &Database, sql: &str, params: &[Value]) -> (usize, usize) {
    let stmt = fenec_ql::parse_one(&format!("explain {sql}")).expect("parse");
    let r = db.query(&stmt, params).unwrap();
    for row in &r.rows().unwrap().rows {
        let [Value::Text(step)] = &row.values[..] else {
            continue;
        };
        let Some((read, rest)) = step
            .strip_prefix("near: ")
            .and_then(|s| s.split_once(" of the "))
        else {
            continue;
        };
        if let Some((of, _)) = rest.split_once(" candidates of the codes read") {
            return (read.parse().unwrap(), of.parse().unwrap());
        }
    }
    panic!("{sql}: no step read the documents' vectors");
}

/// The share of `exact`'s rows `got` found; every score `got` gave for a row
/// both hold must be the exact one, since the documents' vectors ordered it.
fn found(got: &[(u64, f32)], exact: &[(u64, f32)]) -> usize {
    let mut n = 0;
    for (id, score) in got {
        if let Some((_, want)) = exact.iter().find(|e| e.0 == *id) {
            assert!(
                (score - want).abs() < 1e-5,
                "row {id}: {score} against {want}"
            );
            n += 1;
        }
    }
    n
}

#[test]
fn near_over_codes_finds_the_exact_ten_in_exact_order() {
    let plain = shared("@hnsw(cosine)");
    for (index, least) in [
        // Measured 1.000.
        ("@hnsw(cosine, quant=int8)", 0.98),
        // Measured 1.000, at the default beam of 200 bit codes take.
        ("@hnsw(cosine, quant=bit)", 0.93),
    ] {
        let db = shared(index);
        let (mut hit, mut total) = (0, 0);
        for q in queries() {
            let exact = hits(plain, "get docs near v $1 exact limit 10", &q);
            let got = hits(db, "get docs near v $1 limit 10", &q);
            assert!(
                got.windows(2).all(|w| w[0].1 >= w[1].1),
                "{index}: out of order"
            );
            hit += found(&got, &exact);
            total += exact.len();
        }
        let recall = hit as f64 / total as f64;
        assert!(recall >= least, "{index}: recall@10 {recall}");
    }
}

#[test]
fn exact_over_codes_reads_every_vector() {
    let plain = shared("@hnsw(cosine)");
    for index in ["@hnsw(cosine, quant=int8)", "@hnsw(cosine, quant=bit)"] {
        let db = shared(index);
        for q in queries().iter().take(20) {
            let sql = "get docs near v $1 exact limit 10";
            let (want, got) = (hits(plain, sql, q), hits(db, sql, q));
            assert_eq!(found(&got, &want), want.len(), "{index}");
        }
    }
}

#[test]
fn a_filtered_near_over_codes() {
    let plain = shared("@hnsw(cosine)");
    let db = shared("@hnsw(cosine, quant=int8)");
    let a = |id: u64| id % 3 == 1;
    for q in queries().iter().take(20) {
        let exact = hits(
            plain,
            r#"get docs where kind = "a" near v $1 exact limit 10"#,
            q,
        );
        // A thousand rows, under the budget of 100 x 32: ranked by their
        // codes, and what can make the page read.
        let got = hits(db, r#"get docs where kind = "a" near v $1 limit 10"#, q);
        assert_eq!(found(&got, &exact), 10);
        // A beam of 10 makes the budget 320: the ANN, each candidate tested,
        // in the order the documents' vectors give.
        let got = hits(
            db,
            r#"get docs where kind = "a" near v $1 ef 10 limit 10"#,
            q,
        );
        assert!(got.iter().all(|(id, _)| a(*id)));
        assert!(got.windows(2).all(|w| w[0].1 >= w[1].1));
        let scores = hits(
            plain,
            r#"get docs where kind = "a" near v $1 exact limit 1000"#,
            q,
        );
        assert_eq!(found(&got, &scores), got.len());
    }
}

/// An int8 code knows how far off its estimate can be, so a candidate of
/// the beam is read only while it can still make the page, nearest estimate
/// first. What is left unread changes nothing: a page as long as the beam
/// reads all of it, and its first ten are the ten -- under every metric the
/// codes take. Bit codes bound nothing and read the whole beam.
#[test]
fn a_code_reads_only_what_can_make_the_page() {
    for (index, ef, most) in [
        // Measured: 1 421, 1 157 and 1 148 of the 10 000 candidates of a
        // hundred queries.
        ("@hnsw(cosine, quant=int8)", 100, 0.25),
        ("@hnsw(l2, quant=int8)", 100, 0.25),
        ("@hnsw(dot, quant=int8)", 100, 0.25),
        ("@hnsw(cosine, quant=bit)", 200, 1.0),
    ] {
        let own;
        let db = match index {
            "@hnsw(cosine, quant=int8)" | "@hnsw(cosine, quant=bit)" => shared(index),
            _ => {
                own = filled(index);
                &own
            }
        };
        let (mut read_all, mut beam_all) = (0, 0);
        for q in queries() {
            let page = "get docs near v $1 limit 10";
            let beam = &format!("get docs near v $1 limit {ef}");
            let whole = hits(db, beam, &q);
            assert_eq!(hits(db, page, &q), whole[..10], "{index}");
            let (read, of) = reads(db, page, &q);
            assert_eq!(reads(db, beam, &q), (of, of), "{index}");
            read_all += read;
            beam_all += of;
        }
        assert!(
            read_all as f64 <= most * beam_all as f64,
            "{index}: {read_all} of {beam_all} candidates read"
        );
    }
}

/// A filtered set under the ANN budget is ranked by its codes, every row of
/// it, and the beam's worth of the nearest put in order as a walk's are:
/// the thousand rows of a kind read at most a beam, not the thousand.
#[test]
fn a_small_set_reads_the_beam_rather_than_the_set() {
    let plain = shared("@hnsw(cosine)");
    // Measured: every one of the exact 200 found both ways, int8 codes
    // reading 239 vectors over the twenty queries, bit codes 4 000.
    for (index, beam, least, most) in [
        ("@hnsw(cosine, quant=int8)", 100, 200, 600),
        ("@hnsw(cosine, quant=bit)", 200, 190, 4000),
    ] {
        let db = shared(index);
        let sql = r#"get docs where kind = "a" near v $1 limit 10"#;
        let (mut hit, mut read_all) = (0, 0);
        for q in queries().iter().take(20) {
            let exact = hits(
                plain,
                r#"get docs where kind = "a" near v $1 exact limit 10"#,
                q,
            );
            hit += found(&hits(db, sql, q), &exact);
            let (read, of) = reads(db, sql, q);
            assert_eq!(of, beam, "{index}");
            read_all += read;
        }
        assert!(hit >= least, "{index}: {hit} of the exact 200");
        assert!(read_all <= most, "{index}: {read_all} read");
    }
}

/// Each index's arena: bytes, and nodes per the codes' own size.
fn arena(db: &Database) -> usize {
    db.stats()
        .iter()
        .find(|s| s.name == "docs")
        .unwrap()
        .vector_indexes[0]
        .arena_bytes
}

#[test]
fn a_checkpoint_keeps_the_graph_over_codes() {
    // A restored graph keeps its tombstones, as the live one does; a rebuilt
    // one holds the live rows alone -- which is how the arena tells them
    // apart. A byte a component and a scale; or a bit a component, the
    // centre and two factors, beside 128 centres learned from the first
    // 2 048 vectors, each with half its squared length.
    for (index, per_node, centres) in [
        ("@hnsw(cosine, quant=int8)", DIM + 4, 0),
        ("@hnsw(cosine, quant=bit)", DIM / 8 + 9, 128 * (DIM + 1) * 4),
    ] {
        let mut db = filled(index);
        run(&mut db, "del docs where id > 2700");
        assert_eq!(arena(&db), ROWS * per_node + centres, "{index}");
        let before: Vec<_> = queries()
            .iter()
            .take(20)
            .map(|q| hits(&db, "get docs near v $1 limit 10", q))
            .collect();

        let mut back = Database::new();
        back.load(&db.snapshot()).unwrap();
        assert_eq!(
            arena(&back),
            ROWS * per_node + centres,
            "{index}: the graph was rebuilt"
        );
        let after: Vec<_> = queries()
            .iter()
            .take(20)
            .map(|q| hits(&back, "get docs near v $1 limit 10", q))
            .collect();
        assert_eq!(before, after, "{index}");
    }
}

#[test]
fn the_quantization_is_part_of_the_index() {
    let spec = |db: &mut Database| {
        let Response::Schemas(s) = db
            .execute(&fenec_ql::parse_one("collections").unwrap())
            .unwrap()
        else {
            panic!("schemas");
        };
        let IndexKind::Vector(spec) = s[0].field("v").unwrap().index else {
            panic!("a vector index");
        };
        spec
    };
    let mut db = Database::new();
    run(
        &mut db,
        "create collection docs (v vector<8> @hnsw(cosine, quant=bit))",
    );
    let s = spec(&mut db);
    assert_eq!((s.quant, s.ef_search), (Quant::Bit, 200));
    let mut back = Database::new();
    back.load(&db.snapshot()).unwrap();
    assert_eq!(spec(&mut back), s);

    // A beam named is the beam.
    let mut db = Database::new();
    run(
        &mut db,
        "create collection docs (v vector<8> @hnsw(cosine, ef=64, quant=bit))",
    );
    assert_eq!(spec(&mut db).ef_search, 64);

    // Signs are a unit vector's: no other metric.
    let err = fenec_ql::parse_one("create collection docs (v vector<8> @hnsw(l2, quant=bit))")
        .unwrap_err();
    assert!(err.to_string().contains("cosine"), "{err}");
    assert!(fenec_ql::parse_one("create collection d (v vector<8> @hnsw(quant=int4))").is_err());
    // Nor when the index comes after the collection, under the lock or
    // beside the database: the rule was `create collection`'s alone.
    for metric in ["l2", "dot"] {
        let stmt = fenec_ql::parse_one(&format!(
            "create index on docs (v) @hnsw({metric}, quant=bit)"
        ))
        .unwrap();
        let mut db = filled("");
        let err = db.execute(&stmt).unwrap_err();
        assert!(err.to_string().contains("cosine"), "{metric}: {err}");
        let db = std::sync::RwLock::new(filled(""));
        let err = Database::maintain(&db, &stmt).unwrap().unwrap_err();
        assert!(err.to_string().contains("cosine"), "{metric}: {err}");
        let g = db.read().unwrap();
        let schema = &g.collection("docs").unwrap().schema;
        assert_eq!(schema.field("v").unwrap().index, IndexKind::None);
    }

    // A quantized index is built after the fact as any other is.
    let mut db = filled("");
    run(
        &mut db,
        "create index on docs (v) @hnsw(cosine, quant=int8)",
    );
    assert_eq!(spec(&mut db).quant, Quant::Int8);
    assert_eq!(arena(&db), ROWS * (DIM + 4));
    let plain = shared("@hnsw(cosine)");
    for q in queries().iter().take(10) {
        let exact = hits(plain, "get docs near v $1 exact limit 10", q);
        let got = hits(&db, "get docs near v $1 limit 10", q);
        assert!(found(&got, &exact) >= 9);
    }
}

/// A bit index built through the Rust API takes the beam FenecQL gives it.
/// Only the parser knew the bit beam, and the Rust API's searched 100:
/// 82.5% of the true ten over a million vectors, where 400 held 98.4%. The
/// core settles `ef_search` now, wherever a spec comes in, and a beam named
/// is kept.
#[test]
fn the_rust_api_gives_a_bit_index_the_beam_fenecql_does() {
    use fenec_core::schema::{BIT_EF_SEARCH, DEFAULT_EF_SEARCH};
    let spec = |quant| VectorIndexSpec {
        metric: Metric::Cosine,
        quant,
        ..VectorIndexSpec::default()
    };
    let field = |name: &str, spec| {
        Field::new(name, DataType::Vector(8, VecPrec::F32)).indexed(IndexKind::Vector(spec))
    };
    let schema = Schema::new(
        "d",
        vec![
            field("b", spec(Quant::Bit)),
            field("i", spec(Quant::Int8)),
            field(
                "n",
                VectorIndexSpec {
                    ef_search: 64,
                    ..spec(Quant::Bit)
                },
            ),
        ],
    )
    .unwrap();
    let beams = |s: &Schema| -> Vec<usize> {
        s.fields
            .iter()
            .map(|f| match f.index {
                IndexKind::Vector(v) => v.ef_search,
                _ => 0,
            })
            .collect()
    };
    assert_eq!(beams(&schema), [BIT_EF_SEARCH, DEFAULT_EF_SEARCH, 64]);
    let Statement::CreateCollection { schema: parsed, .. } = fenec_ql::parse_one(
        "create collection d (b vector<8> @hnsw(cosine, quant=bit), \
         i vector<8> @hnsw(cosine, quant=int8), n vector<8> @hnsw(cosine, quant=bit, ef=64))",
    )
    .unwrap() else {
        panic!("not a create collection")
    };
    assert_eq!(parsed, schema);

    // A `create index` settles it too, under the lock and beside the
    // database, and so does an index made on its own.
    let mut db = Database::new();
    run(&mut db, "create collection e (v vector<8>, w vector<8>)");
    for field in ["v", "w"] {
        let stmt = Statement::CreateIndex {
            collection: "e".into(),
            field: field.into(),
            kind: IndexKind::Vector(spec(Quant::Bit)),
            if_not_exists: false,
        };
        match field {
            "v" => {
                db.execute(&stmt).unwrap();
            }
            _ => {
                let lock = std::sync::RwLock::new(db);
                Database::maintain(&lock, &stmt)
                    .expect("a maintenance")
                    .unwrap();
                db = lock.into_inner().unwrap();
            }
        }
    }
    let Response::Schemas(schemas) = db.execute(&Statement::ListCollections).unwrap() else {
        panic!("no schemas")
    };
    assert_eq!(beams(&schemas[0]), [BIT_EF_SEARCH, BIT_EF_SEARCH]);
    let ix = fenec_core::vector::VectorIndex::new(8, spec(Quant::Bit));
    assert_eq!(ix.spec.ef_search, BIT_EF_SEARCH);
}
