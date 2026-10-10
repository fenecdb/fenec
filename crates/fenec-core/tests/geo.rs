//! Points, `distance`, `within`, `near` over a point, and `@geo`, which
//! never changes an answer.
//!
//! Every query of the twin tests runs twice: on a collection whose point
//! field carries `@geo` and on a twin without it, holding the same
//! documents -- the twin is the scan, so the two have to agree row for
//! row, scores, ties and pages included -- through writes, deletes, a
//! reopen, an index created after the fact and a block put back.

use fenec_core::prelude::*;

fn exec(db: &mut Database, sql: &str, params: &[Value]) -> Response {
    let stmt = fenec_ql::parse_one_for(db, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    db.execute_with(&stmt, params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

type Rows = Vec<(u64, Vec<Value>, Option<f32>)>;

fn rows(db: &Database, sql: &str, params: &[Value]) -> Rows {
    let stmt = fenec_ql::parse_one_for(db, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let r = db
        .query(&stmt, params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows()
        .expect("rows")
        .rows
        .iter()
        .map(|r| (r.id, r.values.clone(), r.score))
        .collect()
}

fn pt(lon: f64, lat: f64) -> Value {
    Value::List(vec![Value::Float(lon), Value::Float(lat)])
}

fn bx(w: f64, s: f64, e: f64, n: f64) -> Value {
    Value::List([w, s, e, n].map(Value::Float).to_vec())
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn pick<T: Clone>(&mut self, xs: &[T]) -> T {
        xs[(self.next() % xs.len() as u64) as usize].clone()
    }
}

/// Places points crowd round, the poles and either side of the
/// antimeridian among them.
const CITIES: [(f64, f64); 6] = [
    (13.404954, 52.520008),
    (139.6917, 35.6895),
    (-73.9857, 40.7484),
    (179.9, -17.7),
    (0.0, 90.0),
    (45.0, -89.95),
];

/// A point: round a city, anywhere, on an edge of the map, or one of a few
/// written again and again, so that distances tie.
fn point(r: &mut Rng) -> Option<(f64, f64)> {
    let p = match r.next() % 10 {
        0 => return None,
        1..=4 => {
            let (lon, lat) = r.pick(&CITIES);
            let spread = r.pick(&[0.001, 0.01, 0.2, 2.0]);
            let lon = lon + (r.unit() - 0.5) * spread;
            let lat = lat + (r.unit() - 0.5) * spread;
            (
                ((lon + 180.0).rem_euclid(360.0)) - 180.0,
                lat.clamp(-90.0, 90.0),
            )
        }
        5 | 6 => (r.unit() * 360.0 - 180.0, r.unit() * 180.0 - 90.0),
        7 => r.pick(&[
            (180.0, 0.0),
            (-180.0, 0.0),
            (180.0, -17.7),
            (0.0, -90.0),
            (-0.0, 0.0),
            (13.0, 52.0),
        ]),
        _ => r.pick(&[(13.4, 52.5), (13.401, 52.5), (13.4, 52.501)]),
    };
    Some(p)
}

fn document(r: &mut Rng) -> Vec<(String, Expr)> {
    let mut doc = vec![
        (
            "n".to_string(),
            Expr::Lit(Value::Int((r.next() % 50) as i64)),
        ),
        (
            "tag".to_string(),
            Expr::Lit(Value::Text(r.pick(&["a", "b", "c"]).into())),
        ),
    ];
    if let Some((lon, lat)) = point(r) {
        doc.push(("loc".into(), Expr::Lit(pt(lon, lat))));
    }
    doc
}

const SCHEMA: &str = "(n int, tag text @hash, loc geo)";

fn twins(docs: usize, seed: u64) -> (Database, Database) {
    let mut ix = Database::new();
    let mut plain = Database::new();
    exec(
        &mut ix,
        &format!("create collection c {}", SCHEMA.replace("geo", "geo @geo")),
        &[],
    );
    exec(&mut plain, &format!("create collection c {SCHEMA}"), &[]);
    let mut r = Rng(seed);
    let batch: Vec<_> = (0..docs).map(|_| document(&mut r)).collect();
    for db in [&mut ix, &mut plain] {
        db.execute(&Statement::Put {
            collection: "c".into(),
            docs: batch.clone(),
            insert: false,
            if_absent: false,
            docs_param: None,
            else_set: None,
            require: None,
        })
        .unwrap();
    }
    (ix, plain)
}

/// The centres, radii and boxes asked about: a city's, the antimeridian's
/// and the poles', a radius of nothing to half the earth, a box across
/// the antimeridian, round a pole, along a meridian and over everything.
fn shapes(r: &mut Rng) -> Vec<(Value, Value, Value)> {
    let mut centres: Vec<(f64, f64)> = CITIES.to_vec();
    centres.extend([(-179.95, -17.7), (13.4, 52.5), (180.0, 0.0), (0.0, -90.0)]);
    for _ in 0..4 {
        centres.push((r.unit() * 360.0 - 180.0, r.unit() * 180.0 - 90.0));
    }
    let radii = [0.0, 1.0, 120.0, 1_500.0, 40_000.0, 900_000.0, 2.1e7];
    let boxes = [
        bx(13.3, 52.4, 13.5, 52.6),
        bx(179.0, -19.0, -179.0, -16.0),
        bx(-180.0, 89.0, 180.0, 90.0),
        bx(13.4, -90.0, 13.4, 90.0),
        bx(-180.0, -90.0, 180.0, 90.0),
        bx(170.0, -90.0, -170.0, -60.0),
    ];
    let mut out = Vec::new();
    for (i, c) in centres.iter().enumerate() {
        for (j, rad) in radii.iter().enumerate() {
            if (i + j) % 3 == 0 {
                let b = boxes[(i + j) % boxes.len()].clone();
                out.push((pt(c.0, c.1), Value::Float(*rad), b));
            }
        }
    }
    out
}

const FILTERS: [&str; 18] = [
    "where distance(loc, $1) <= $2",
    "where distance(loc, $1) < $2",
    "where $2 >= distance(loc, $1)",
    "where distance($1, loc) <= $2",
    "where distance(loc, $1) <= $2 and n > 25",
    "where tag = \"b\" and distance(loc, $1) <= $2",
    "where distance(loc, $1) <= $2 or n = 3",
    "where not (distance(loc, $1) <= $2)",
    "where distance(loc, $1) > $2",
    "where distance(loc, $1) = 0",
    "where within(loc, $3)",
    "where within(loc, $3) and distance(loc, $1) <= $2",
    "where within(loc, $3) and tag in [\"a\", \"c\"]",
    "where not within(loc, $3)",
    "where loc is null",
    "where distance(loc, $1) <= $2 and within(loc, $3) and n < 40",
    // Evaluated as a call, not bound: the bound test, its floor by
    // latitude among it, answers as `eval` does.
    "where distance(loc, $1) + 0 <= $2",
    "where distance(loc, $1) * 1 > $2",
];

fn check(ix: &Database, plain: &Database, stage: &str, seed: u64) {
    let mut r = Rng(seed);
    let mut asked = 0;
    for (c, rad, b) in shapes(&mut r) {
        let params = [c, rad, b];
        for f in FILTERS {
            let count = format!("get c {f} count");
            assert_eq!(
                rows(ix, &count, &params),
                rows(plain, &count, &params),
                "{stage}: {count} {params:?}"
            );
            for o in ["", "order n desc", "limit 5", "limit 7 offset 3"] {
                let q = format!("get c select n, loc {f} {o}");
                assert_eq!(
                    rows(ix, &q, &params),
                    rows(plain, &q, &params),
                    "{stage}: {q} {params:?}"
                );
                asked += 1;
            }
        }
        // The bound test -- a row ruled out by its latitude alone among it
        // -- answers as the call evaluated a row at a time does.
        for (bound, called) in [
            ("distance(loc, $1) <= $2", "distance(loc, $1) + 0 <= $2"),
            ("distance($1, loc) > $2", "distance($1, loc) * 1 > $2"),
            ("$2 >= distance(loc, $1)", "$2 + 0 >= distance(loc, $1)"),
        ] {
            let q = |f: &str| format!("get c select n where {f}");
            assert_eq!(
                rows(plain, &q(bound), &params),
                rows(plain, &q(called), &params),
                "{stage}: {bound} {params:?}"
            );
        }
        for f in [
            "",
            "where distance(loc, $1) <= $2",
            "where tag = \"b\"",
            "where n >= 48",
            "where within(loc, $3)",
            "where distance(loc, $1) < $2 and tag = \"a\"",
        ] {
            for p in ["limit 10", "limit 3 offset 4", "limit 1", ""] {
                let q = format!("get c select n, distance(loc, $1) as d {f} near loc $1 {p}");
                let want = rows(plain, &q, &params);
                assert_eq!(rows(ix, &q, &params), want, "{stage}: {q} {params:?}");
                let exact = q.replace("near loc $1", "near loc $1 exact");
                assert_eq!(rows(ix, &exact, &params), want, "{stage}: {exact}");
                // The score is the distance, as an `f32`.
                for (_, v, s) in &want {
                    match &v[1] {
                        Value::Float(d) => assert_eq!(*s, Some(*d as f32), "{q}"),
                        other => panic!("{q}: {other:?}"),
                    }
                }
                asked += 1;
            }
        }
    }
    assert!(asked > 2_000, "{asked}");
}

#[test]
fn a_point_index_gives_the_scans_answer() {
    let (mut ix, mut plain) = twins(2_000, 0x9E37_79B9_7F4A_7C15);
    check(&ix, &plain, "after the load", 1);

    // Writes: points moved, taken away and given, rows deleted.
    for db in [&mut ix, &mut plain] {
        exec(db, "set c {loc: $1} where n = 7", &[pt(13.4, 52.5)]);
        exec(db, "set c {loc: null} where n = 8", &[]);
        exec(
            db,
            "set c {loc: [179.99, -17.7]} where loc is null and n < 20",
            &[],
        );
        exec(db, "del c where tag = \"c\" and n > 40", &[]);
        exec(db, "put c {id: 9, n: 1, tag: \"a\", loc: [0, 90]}", &[]);
    }
    check(&ix, &plain, "after writes", 2);

    // A reopen builds the index from the documents.
    let mut reopened = Database::new();
    reopened.load(&ix.snapshot()).unwrap();
    check(&reopened, &plain, "after a reopen", 3);
}

/// An index created over data that is already there is the one kept up on
/// write; and a block put back takes its writes out of it.
#[test]
fn an_index_created_later_and_a_block_put_back_give_the_same_answers() {
    let (_, mut plain) = twins(1_000, 77);
    let mut later = Database::new();
    later.load(&plain.snapshot()).unwrap();
    exec(&mut later, "create index on c (loc) @geo", &[]);
    check(&later, &plain, "index created later", 4);

    // A block that fails is put back whole, the index with it.
    let put =
        fenec_ql::parse_one_for(&later, "put c {n: 1, tag: \"z\", loc: [13.4, 52.5]}").unwrap();
    let moved = fenec_ql::parse_one_for(&later, "set c {loc: [0, 0]} where n < 30").unwrap();
    let bad = fenec_ql::parse_one_for(&later, "put c {loc: [0, 91]}").unwrap();
    let none: &[Value] = &[];
    assert!(later
        .execute_block(&[(&put, none), (&moved, none), (&bad, none)])
        .is_err());
    check(&later, &plain, "a block put back", 5);
    exec(&mut plain, "put c {n: 1, loc: [13.4001, 52.5]}", &[]);
    exec(&mut later, "put c {n: 1, loc: [13.4001, 52.5]}", &[]);
    check(&later, &plain, "index created later, then a write", 6);
}

#[test]
fn a_point_is_written_and_read_as_lon_lat() {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection p (name text, loc geo @geo)",
        &[],
    );
    exec(
        &mut db,
        "put p {name: \"berlin\", loc: [13.404954, 52.520008]}",
        &[],
    );
    exec(&mut db, "put p {name: \"zero\", loc: [0, 0]}", &[]);
    exec(&mut db, "put p {name: \"none\"}", &[]);
    exec(
        &mut db,
        "put p {name: \"param\", loc: $1}",
        &[pt(-73.9857, 40.7484)],
    );
    let got = rows(&db, "get p select name, loc", &[]);
    assert_eq!(got[0].1[1], pt(13.404954, 52.520008));
    assert_eq!(got[1].1[1], pt(0.0, 0.0));
    assert_eq!(got[2].1[1], Value::Null);
    let r = db
        .query(&fenec_ql::parse_one("get p select loc").unwrap(), &[])
        .unwrap();
    let json = fenec_core::json::response_to_string(&r);
    assert!(
        json.contains("[13.404954,52.520008]") && json.contains("[0,0]"),
        "{json}"
    );
    // The plain read written as JSON from the bytes writes the same.
    let mut out = String::new();
    db.query_json(
        &fenec_ql::parse_one("get p select loc").unwrap(),
        &[],
        &mut out,
    )
    .unwrap()
    .unwrap();
    assert!(out.contains("{\"loc\":[13.404954,52.520008]}"), "{out}");

    // Refused, never wrapped or cut: a latitude past 90, a longitude past
    // 180, a list of three, text, a vector's f32s.
    for bad in [
        "put p {loc: [0, 91]}",
        "put p {loc: [181, 0]}",
        "put p {loc: [1, 2, 3]}",
        "put p {loc: \"13.4,52.5\"}",
        "put p {loc: [\"13.4\", 52.5]}",
    ] {
        let stmt = fenec_ql::parse_one_for(&db, bad).unwrap();
        assert!(db.execute(&stmt).is_err(), "{bad}");
    }
    let v = Value::Vector(vec![13.4, 52.5]);
    let stmt = fenec_ql::parse_one("put p {loc: $1}").unwrap();
    let e = db.execute_with(&stmt, &[v]).unwrap_err().to_string();
    assert!(e.contains("as written"), "{e}");

    // A point's index is `@geo`, and `@geo` a point's.
    for bad in [
        "create collection x (loc geo @hash)",
        "create collection x (loc geo @sorted)",
        "create collection x (n int @geo)",
    ] {
        assert!(fenec_ql::parse_one(bad).is_err(), "{bad}");
    }
}

/// The literals of a text are read as written where they reach a point:
/// read the quick way, 13.404954 would be the `f32` 13.404954 and a
/// distance a metre off at the antimeridian.
#[test]
fn a_point_in_a_text_or_a_parameter_is_read_as_written() {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection p (loc geo @geo, e vector<2>)",
        &[],
    );
    exec(
        &mut db,
        "put p {loc: [179.123456789, 0.000000001], e: [1, 2]}",
        &[],
    );
    let got = rows(&db, "get p select loc", &[]);
    assert_eq!(got[0].1[0], pt(179.123456789, 0.000000001));
    let d = rows(
        &db,
        "get p select distance(loc, [179.123456789, 0.000000001]) as d",
        &[],
    );
    assert_eq!(d[0].1[0], Value::Float(0.0));
    let near = rows(
        &db,
        "get p near loc [179.123456789, 0.000000001] limit 1",
        &[],
    );
    assert_eq!(near[0].2, Some(0.0));
    // A parameter read the quick way is refused, and the statement says
    // which ones a reader must read as written.
    let stmt = fenec_ql::parse_one("get p where distance(loc, $1) <= $2").unwrap();
    assert_eq!(db.exactly(&stmt).params, vec![0]);
    let stmt = fenec_ql::parse_one("get p where within(loc, $3) or loc = $1").unwrap();
    assert_eq!(db.exactly(&stmt).params, vec![2, 0]);
    let stmt = fenec_ql::parse_one("get p near loc $2").unwrap();
    assert_eq!(db.exactly(&stmt).params, vec![1]);
    let exact = fenec_core::json::parse_params_exact("[[179.123456789, 0.000000001], 0]").unwrap();
    let stmt = fenec_ql::parse_one("get p where distance(loc, $1) <= $2").unwrap();
    assert_eq!(
        db.query(&stmt, &exact).unwrap().rows().unwrap().rows.len(),
        1
    );
    let quick = fenec_core::json::parse_params("[[179.123456789, 0.000000001], 0]").unwrap();
    assert!(db.query(&stmt, &quick).is_err());
}

/// Redis's own examples (GEOADD Sicily ...): its distances, and the order
/// GEOSEARCH answers in.
#[test]
fn geosearch_answers_as_redis() {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection sicily (name text, loc geo @geo)",
        &[],
    );
    for (name, lon, lat) in [
        ("Palermo", 13.361389, 38.115556),
        ("Catania", 15.087269, 37.502669),
        ("edge1", 12.758489, 38.788135),
        ("edge2", 17.241510, 38.788135),
    ] {
        exec(
            &mut db,
            "put sicily {name: $1, loc: $2}",
            &[Value::Text(name.into()), pt(lon, lat)],
        );
    }
    // GEODIST Sicily Palermo Catania -> 166274.1516 (Redis keeps each in a
    // cell of 0.6 m by 0.3 m; the coordinates as written are within it).
    let d = rows(
        &db,
        "get sicily select distance(loc, $1) as d where name = \"Palermo\"",
        &[pt(15.087269, 37.502669)],
    );
    let Value::Float(d) = d[0].1[0] else { panic!() };
    assert!((d - 166274.1516).abs() < 1.0, "{d}");
    // GEOSEARCH Sicily FROMLONLAT 15 37 BYRADIUS 200 km ASC WITHDIST:
    // Catania 56.4413, Palermo 190.4424.
    let got = rows(
        &db,
        "get sicily select name where distance(loc, $1) <= 200000 near loc $1",
        &[pt(15.0, 37.0)],
    );
    let names: Vec<_> = got.iter().map(|r| r.1[0].clone()).collect();
    assert_eq!(
        names,
        [Value::Text("Catania".into()), Value::Text("Palermo".into())]
    );
    assert!((got[0].2.unwrap() - 56441.3).abs() < 1.0);
    assert!((got[1].2.unwrap() - 190442.4).abs() < 1.0);
    // ... COUNT 1: the nearest alone.
    let one = rows(
        &db,
        "get sicily select name near loc $1 limit 1",
        &[pt(15.0, 37.0)],
    );
    assert_eq!(one[0].1[0], Value::Text("Catania".into()));
    // The bounding box the four lie in, and one across nothing.
    let n = rows(
        &db,
        "get sicily where within(loc, [12, 37, 18, 39]) count",
        &[],
    );
    assert_eq!(n[0].1[0], Value::Int(4));
}

/// `explain` names the index a radius or a box was narrowed by, and the
/// walk `near` took.
#[test]
fn explain_names_the_point_index() {
    let (ix, _) = twins(2_000, 5);
    let plan = |q: &str, params: &[Value]| -> String {
        rows(&ix, &format!("explain {q}"), params)
            .into_iter()
            .map(|r| format!("{:?}\n", r.1[0]))
            .collect()
    };
    let p = plan("get c where distance(loc, $1) <= 1000", &[pt(13.4, 52.5)]);
    assert!(p.contains("the point index on loc"), "{p}");
    let p = plan("get c where within(loc, [13.3, 52.4, 13.5, 52.6])", &[]);
    assert!(p.contains("the point index on loc"), "{p}");
    let p = plan("get c near loc $1 limit 5", &[pt(13.4, 52.5)]);
    assert!(p.contains("walked the point index on loc"), "{p}");
    let p = plan("get c near loc $1 exact limit 5", &[pt(13.4, 52.5)]);
    assert!(p.contains("every point measured"), "{p}");
}

/// What `near` over a point refuses: `ef`, which only a vector search
/// has, a null point, and a point that is none.
#[test]
fn near_over_a_point_refuses_what_it_cannot_answer() {
    let (ix, _) = twins(50, 9);
    for (q, params) in [
        ("get c near loc $1 ef 50 limit 3", vec![pt(1.0, 1.0)]),
        ("get c near loc $1 limit 3", vec![Value::Null]),
        ("get c near loc $1 limit 3", vec![pt(1.0, 99.0)]),
        ("get c near loc $1 limit 3", vec![Value::Int(4)]),
    ] {
        let stmt = fenec_ql::parse_one(q).unwrap();
        assert!(ix.query(&stmt, &params).is_err(), "{q} {params:?}");
    }
}

/// A point's conditions compose with what a filter composes with: a
/// facet counted over the rows a radius selects, a `lookup` hanging
/// children off them, an expiry leaving rows out, a field added and
/// indexed after the rows, and an index built beside the database.
#[test]
fn a_radius_composes_with_facets_lookups_expiry_and_alters() {
    let (ix, plain) = twins(1_500, 21);
    let c = [pt(13.4, 52.5), Value::Float(30_000.0)];
    for q in [
        "get c where distance(loc, $1) <= $2 count facet tag",
        "get c select n where distance(loc, $1) <= $2 facet n top 3 limit 4",
        "get c select tag, count(*) as k where distance(loc, $1) <= $2 group tag",
        "get c where distance(loc, $1) <= $2 and tag in [\"a\", \"b\"] count",
    ] {
        let a = ix.query(&fenec_ql::parse_one(q).unwrap(), &c).unwrap();
        let b = plain.query(&fenec_ql::parse_one(q).unwrap(), &c).unwrap();
        assert_eq!(a, b, "{q}");
    }

    // A lookup's parents chosen by a radius, its children by a point too.
    let mut db = Database::new();
    for sql in [
        "create collection shops (name text, loc geo @geo)",
        "create collection visits (shop int @hash, at geo)",
        "put shops [{name: \"near\", loc: [13.4, 52.5]}, {name: \"far\", loc: [2.35, 48.86]}]",
        "put visits [{shop: 1, at: [13.4001, 52.5]}, {shop: 1, at: [0, 0]}, {shop: 2}]",
    ] {
        exec(&mut db, sql, &[]);
    }
    let got = rows(
        &db,
        "get shops select name where distance(loc, [13.4, 52.5]) < 1000 \
         lookup visits on shop = id where distance(at, [13.4, 52.5]) < 100 required",
        &[],
    );
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1[0], Value::Text("near".into()));

    // Rows past their time are out of a radius and of `near` at once.
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection pings (at timestamp @ttl(1m), loc geo @geo)",
        &[],
    );
    db.set_clock(Some(1_000_000));
    exec(
        &mut db,
        "put pings [{at: 999000, loc: [1, 1]}, {at: 900000, loc: [1, 1.0001]}]",
        &[],
    );
    let alive = rows(
        &db,
        "get pings where distance(loc, [1, 1]) < 100 count",
        &[],
    );
    assert_eq!(alive[0].1[0], Value::Int(1));
    assert_eq!(rows(&db, "get pings near loc [1, 1]", &[]).len(), 1);

    // A point added to a collection holding rows, then indexed beside it.
    let mut db = Database::new();
    exec(&mut db, "create collection t (n int)", &[]);
    exec(&mut db, "put t [{n: 1}, {n: 2}]", &[]);
    exec(&mut db, "alter collection t add field loc geo", &[]);
    exec(&mut db, "set t {loc: [10, 10]} where n = 2", &[]);
    let lock = std::sync::RwLock::new(db);
    let stmt = fenec_ql::parse_one("create index on t (loc) @geo").unwrap();
    Database::maintain(&lock, &stmt).unwrap().unwrap();
    let mut db = lock.into_inner().unwrap();
    exec(&mut db, "put t {n: 3, loc: [10.0001, 10]}", &[]);
    let got = rows(&db, "get t select n where within(loc, [9, 9, 11, 11])", &[]);
    let ns: Vec<Value> = got.iter().map(|r| r.1[0].clone()).collect();
    assert_eq!(ns, [Value::Int(2), Value::Int(3)]);
    let p = rows(
        &db,
        "explain get t where distance(loc, [10, 10]) <= 50",
        &[],
    );
    assert!(format!("{p:?}").contains("the point index on loc"), "{p:?}");
    exec(&mut db, "alter collection t rename field loc to place", &[]);
    let got = rows(&db, "get t select n near place [10, 10] limit 1", &[]);
    assert_eq!(got[0].1[0], Value::Int(2));
    exec(&mut db, "alter collection t drop field place", &[]);
    let gone = fenec_ql::parse_one("get t near place [10, 10]").unwrap();
    assert!(db.query(&gone, &[]).is_err());
}

/// A read the point index answers runs under the lock, as every indexed
/// read does: `pin` declines it, and one let through meets an index that
/// refuses and is answered `None`.
#[test]
fn a_read_the_point_index_answers_is_not_pinned() {
    let (mut ix, _) = twins(600, 3);
    ix.set_pin_at(10);
    let c = [pt(13.4, 52.5), Value::Float(30_000.0)];
    let p = ix.pin_every_collection();
    for sql in [
        "get c where distance(loc, $1) <= $2 count",
        "get c where within(loc, [13, 52, 14, 53]) count",
        "get c near loc $1 limit 3",
    ] {
        let s = fenec_ql::parse_one_for(&ix, sql).unwrap();
        assert!(ix.pin(&[(&s, &c[..])]).is_none(), "{sql}");
        assert_eq!(p.query(&s, &c), None, "{sql}");
    }
    // Without the index the same radius is a scan, and is pinned.
    let s = fenec_ql::parse_one("get c where distance(loc, $1) > $2 count").unwrap();
    assert!(ix.pin(&[(&s, &c[..])]).is_some());
}
