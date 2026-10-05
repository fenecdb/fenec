//! `facet`: each value a field holds over every row a query matches, and
//! how many rows hold it -- held to `group ... count` over the same filter,
//! row for row, through a hash index and through the scan, and to counts
//! worked out by hand where `group` has no answer: a path into a json
//! field, a list's values, the rows `match` selects.

use fenec_core::prelude::*;
use fenec_core::query::FacetValues;
use std::collections::BTreeMap;

fn exec(db: &mut Database, sql: &str, params: &[Value]) {
    db.execute_with(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn answer(db: &Database, sql: &str, params: &[Value]) -> ResultSet {
    let r = db
        .query(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows().unwrap().clone()
}

fn error(db: &Database, sql: &str) -> String {
    match fenec_ql::parse_one(sql) {
        Err(e) => e.to_string(),
        Ok(stmt) => db.query(&stmt, &[]).expect_err(sql).to_string(),
    }
}

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

const BRANDS: [&str; 7] = ["acme", "zeta", "Acme", "orbit", "nova", "çam", "ışık"];
const COLORS: [&str; 5] = ["red", "blue", "green", "black", "white"];
const TAGS: [&str; 6] = ["new", "sale", "eco", "gift", "bulk", "rare"];
const WORDS: [&str; 8] = [
    "phone", "case", "cable", "charger", "screen", "battery", "lens", "stand",
];

/// One product: brand, color, year, tags, the json `meta.origin`, its text.
#[derive(Clone)]
struct P {
    brand: Option<&'static str>,
    color: Option<&'static str>,
    year: i64,
    tags: Vec<&'static str>,
    origin: Option<Value>,
    body: String,
}

/// `p` hashes its brand and year, `q` holds the same rows with no index
/// at all: a facet through the buckets and through the scan, side by side.
fn products(n: usize) -> (Database, Vec<P>) {
    let mut db = Database::new();
    for (name, idx) in [("p", "@hash"), ("q", "")] {
        exec(
            &mut db,
            &format!(
                "create collection {name} (brand text {idx}, color text, year int {idx}, \
                 tags [text], meta json, body text @text, price float)"
            ),
            &[],
        );
    }
    let mut rng = Rng(0x2545f4914f6cdd1d);
    let mut all = Vec::new();
    for i in 0..n {
        let brand = (rng.below(9) != 0).then(|| BRANDS[rng.below(7) as usize]);
        let color = (rng.below(6) != 0).then(|| COLORS[rng.below(5) as usize]);
        let year = 2020 + rng.below(5) as i64;
        let mut tags = Vec::new();
        for _ in 0..rng.below(4) {
            tags.push(TAGS[rng.below(6) as usize]);
        }
        let origin = match rng.below(5) {
            0 => None,
            1 => Some(Value::Int(rng.below(3) as i64)),
            2 => Some(Value::Bool(true)),
            _ => Some(Value::Text(
                ["tr", "de", "jp"][rng.below(3) as usize].into(),
            )),
        };
        let body: Vec<&str> = (0..4).map(|_| WORDS[rng.below(8) as usize]).collect();
        let p = P {
            brand,
            color,
            year,
            tags,
            origin,
            body: body.join(" "),
        };
        let meta = match &p.origin {
            Some(v) => Value::object(vec![("origin".into(), v.clone())]).unwrap(),
            None => Value::Null,
        };
        let opt = |s: Option<&str>| s.map_or(Value::Null, |s| Value::Text(s.into()));
        let params = [
            opt(p.brand),
            opt(p.color),
            Value::Int(p.year),
            Value::List(p.tags.iter().map(|t| Value::Text(t.to_string())).collect()),
            meta,
            Value::Text(p.body.clone()),
            Value::Float(i as f64),
        ];
        for name in ["p", "q"] {
            exec(
                &mut db,
                &format!(
                    "put {name} {{brand: $1, color: $2, year: $3, tags: $4, meta: $5, \
                     body: $6, price: $7}}"
                ),
                &params,
            );
        }
        all.push(p);
    }
    (db, all)
}

fn facet<'a>(rs: &'a ResultSet, field: &str) -> &'a FacetValues {
    rs.facets
        .iter()
        .find(|f| f.field == field)
        .unwrap_or_else(|| panic!("no facet {field}"))
}

/// What a facet should be: the counts, most first, then by value.
fn ordered(counts: BTreeMap<Key, u64>) -> Vec<(Value, u64)> {
    let mut v: Vec<(Value, u64)> = counts.into_iter().map(|(k, n)| (k.0, n)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp_value(&b.0)));
    v
}

/// A value as a map key, by the engine's own order.
#[derive(Clone, Debug)]
struct Key(Value);
impl PartialEq for Key {
    fn eq(&self, o: &Self) -> bool {
        self.0.cmp_value(&o.0).is_eq() && self.0.type_name() == o.0.type_name()
    }
}
impl Eq for Key {}
impl PartialOrd for Key {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Key {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.0
            .cmp_value(&o.0)
            .then_with(|| self.0.type_name().cmp(o.0.type_name()))
    }
}

/// `group <field> count` over the same filter, as a facet orders it.
fn grouped(db: &Database, coll: &str, field: &str, filter: &str) -> Vec<(Value, u64)> {
    let w = if filter.is_empty() {
        String::new()
    } else {
        format!(" where {filter}")
    };
    let rs = answer(
        db,
        &format!("get {coll} select {field}, count(*){w} group {field}"),
        &[],
    );
    let mut counts = BTreeMap::new();
    for r in &rs.rows {
        let Value::Int(n) = r.values[1] else { panic!() };
        counts.insert(Key(r.values[0].clone()), n as u64);
    }
    ordered(counts)
}

/// Which products a filter keeps.
type Keep = fn(&P) -> bool;

const FILTERS: [(&str, Keep); 6] = [
    ("", |_| true),
    ("year >= 2022", |p| p.year >= 2022),
    ("color = \"red\"", |p| p.color == Some("red")),
    ("brand = \"acme\" or color is null", |p| {
        p.brand == Some("acme") || p.color.is_none()
    }),
    ("tags has \"sale\"", |p| p.tags.contains(&"sale")),
    ("year = 1999", |_| false),
];

#[test]
fn facets_equal_group_count_through_the_buckets_and_the_scan() {
    let (db, all) = products(3_000);
    for (filter, keep) in FILTERS {
        let w = if filter.is_empty() {
            String::new()
        } else {
            format!(" where {filter}")
        };
        for coll in ["p", "q"] {
            let rs = answer(
                &db,
                &format!(
                    "get {coll} select id{w} facet brand, color, year, tags, meta.origin limit 5"
                ),
                &[],
            );
            // The page is the page; the counts are every matched row's.
            assert!(rs.rows.len() <= 5);
            for field in ["brand", "color", "year"] {
                assert_eq!(
                    facet(&rs, field).values,
                    grouped(&db, coll, field, filter),
                    "{coll} {field} {filter:?}"
                );
            }
            // A list: once a row for each value it holds.
            let mut tags = BTreeMap::new();
            let mut origin = BTreeMap::new();
            for p in all.iter().filter(|p| keep(p)) {
                let mut seen = Vec::new();
                for t in &p.tags {
                    if !seen.contains(t) {
                        seen.push(*t);
                        *tags.entry(Key(Value::Text(t.to_string()))).or_insert(0) += 1;
                    }
                }
                let o = p.origin.clone().unwrap_or(Value::Null);
                *origin.entry(Key(o)).or_insert(0) += 1;
            }
            assert_eq!(
                facet(&rs, "tags").values,
                ordered(tags),
                "{coll} tags {filter:?}"
            );
            assert_eq!(
                facet(&rs, "meta.origin").values,
                ordered(origin),
                "{coll} meta.origin {filter:?}"
            );
        }
    }
}

#[test]
fn a_facet_counts_the_rows_match_selects_and_ignores_the_page() {
    let (db, all) = products(2_000);
    for (query, filter, keep) in [
        ("phone", "", (|_| true) as Keep),
        ("lens cable", "year < 2023", |p| p.year < 2023),
        ("screen", "brand = \"nova\"", |p| p.brand == Some("nova")),
        ("nothing", "", |_| true),
    ] {
        let words: Vec<&str> = query.split(' ').collect();
        let w = if filter.is_empty() {
            String::new()
        } else {
            format!(" where {filter}")
        };
        for coll in ["p", "q"] {
            let rs = answer(
                &db,
                &format!(
                    "get {coll} select id{w} match body $1 limit 3 offset 1 \
                     facet brand top 3, tags"
                ),
                &[Value::Text(query.into())],
            );
            let mut brands = BTreeMap::new();
            let mut tags = BTreeMap::new();
            for p in all
                .iter()
                .filter(|p| keep(p) && p.body.split(' ').any(|b| words.contains(&b)))
            {
                let b = p.brand.map_or(Value::Null, |b| Value::Text(b.into()));
                *brands.entry(Key(b)).or_insert(0) += 1;
                let mut seen = Vec::new();
                for t in &p.tags {
                    if !seen.contains(t) {
                        seen.push(*t);
                        *tags.entry(Key(Value::Text(t.to_string()))).or_insert(0) += 1;
                    }
                }
            }
            let mut want = ordered(brands);
            want.truncate(3);
            assert_eq!(facet(&rs, "brand").values, want, "{coll} {query} {filter}");
            assert_eq!(facet(&rs, "tags").values, ordered(tags), "{coll} {query}");
        }
    }
}

#[test]
fn facets_go_beside_a_count_an_order_and_a_required_lookup() {
    let (mut db, _) = products(1_000);
    let rs = answer(&db, "get p where year >= 2023 count facet color", &[]);
    assert_eq!(rs.columns, ["count"]);
    assert_eq!(
        facet(&rs, "color").values,
        grouped(&db, "p", "color", "year >= 2023")
    );
    // An order walked through no index, and one that reads every row.
    let rs = answer(
        &db,
        "get p select id where year >= 2021 order price desc limit 2 facet year",
        &[],
    );
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(
        facet(&rs, "year").values,
        grouped(&db, "p", "year", "year >= 2021")
    );
    // `top` keeps the commonest, ties by value.
    let rs = answer(&db, "get p facet brand top 2, color top 1 limit 0", &[]);
    let full = grouped(&db, "p", "brand", "");
    assert_eq!(facet(&rs, "brand").values, full[..2]);
    assert_eq!(rs.rows.len(), 0);
    // A required lookup decides who is counted.
    exec(
        &mut db,
        "create collection r (pid int @hash, stars int)",
        &[],
    );
    for pid in [1, 2, 3, 3, 5, 8, 13] {
        exec(&mut db, "put r {pid: $1, stars: 5}", &[Value::Int(pid)]);
    }
    let rs = answer(
        &db,
        "get p select id facet year lookup r on pid required",
        &[],
    );
    assert_eq!(rs.rows.len(), 6);
    let counted: u64 = facet(&rs, "year").values.iter().map(|v| v.1).sum();
    assert_eq!(counted, 6);
}

#[test]
fn the_json_carries_facets_beside_the_rows() {
    let mut db = Database::new();
    exec(&mut db, "create collection t (k text @hash, n int)", &[]);
    for (k, n) in [("a", 1), ("b", 1), ("a", 2), ("c", 3)] {
        exec(
            &mut db,
            "put t {k: $1, n: $2}",
            &[Value::Text(k.into()), Value::Int(n)],
        );
    }
    exec(&mut db, "put t {n: 4}", &[]);
    let r = db
        .query(
            &fenec_ql::parse_one("get t select n where n < 4 limit 1 facet k, n top 1").unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(
        fenec_core::json::response_to_string(&r),
        "{\"kind\":\"rows\",\"result\":{\"columns\":[\"n\"],\"rows\":[{\"n\":1}],\
         \"facets\":{\"k\":[{\"value\":\"a\",\"count\":2},{\"value\":\"b\",\"count\":1},\
         {\"value\":\"c\",\"count\":1}],\"n\":[{\"value\":1,\"count\":2}]}}}"
    );
    // Without a filter the buckets alone count: the null row too.
    let rs = answer(&db, "get t facet k", &[]);
    assert_eq!(
        facet(&rs, "k").values,
        [
            (Value::Text("a".into()), 2),
            (Value::Null, 1),
            (Value::Text("b".into()), 1),
            (Value::Text("c".into()), 1)
        ]
    );
}

#[test]
fn what_facets_cannot_answer_is_refused() {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection v (k text, e vector<2> @hnsw(cosine), body text @text, n int)",
        &[],
    );
    for i in 0..30 {
        exec(
            &mut db,
            "put v {k: $1, e: [1, 0], body: \"x\", n: $2}",
            &[Value::Text(format!("k{i}")), Value::Int(i)],
        );
    }
    for (sql, want) in [
        ("get v near e [1, 0] facet k", "ranks every row"),
        (
            "get v match body \"x\" near e [1, 0] fuse facet k",
            "ranks every row",
        ),
        ("get v select count(*) facet k", "aggregates"),
        ("get v facet k, k", "asked twice"),
        ("get v facet k top 0", "top 0"),
        ("get v facet k top 10001", "at most 10000"),
        ("get v facet nope", "nope"),
        ("get v facet n top 3 ranges [0, 1]", "no `top`"),
        ("get v facet n ranges [0]", "2 to 10 001 numbers"),
        ("get v facet n ranges [5, 5]", "above the one"),
        ("get v facet n ranges [5, 1]", "above the one"),
        ("get v facet n ranges [0, \"a\"]", "numbers"),
        ("get v facet n ranges $1", "brackets"),
        ("get v facet k ranges [0, 1]", "counts numbers"),
        (
            "get v where k = \"a\" or n = 1 facet k disjunctive",
            "reads another field",
        ),
    ] {
        let e = error(&db, sql);
        assert!(e.contains(want), "{sql}: {e}");
    }
}

/// `disjunctive`: a facet counted over the query's rows with its own
/// conditions on the field left out is the facet of the query written
/// without them -- through the buckets and the scan, beside `match`, a
/// count, a page and a facet that is not disjunctive.
#[test]
fn a_disjunctive_facet_counts_as_the_filter_without_its_own_conditions() {
    let (db, _) = products(3_000);
    // Each: the filter, and what it is without the brand's conditions.
    let cases = [
        (r#"brand = "acme""#, ""),
        (
            r#"brand in ["acme", "nova"] and year >= 2022"#,
            "year >= 2022",
        ),
        (
            r#"year >= 2021 and (brand = "zeta" or brand is null) and color = "red""#,
            r#"year >= 2021 and color = "red""#,
        ),
        (
            r#"not brand = "orbit" and tags has "sale""#,
            r#"tags has "sale""#,
        ),
        ("year = 2023", "year = 2023"),
        ("", ""),
    ];
    for coll in ["p", "q"] {
        for (filter, rest) in cases {
            let w = |f: &str| match f {
                "" => String::new(),
                f => format!(" where {f}"),
            };
            for tail in [
                "limit 5 offset 2",
                "count",
                "match body \"phone lens\" limit 3",
            ] {
                let got = answer(
                    &db,
                    &format!(
                        "get {coll}{} {tail} facet brand disjunctive, color, year disjunctive",
                        w(filter)
                    ),
                    &[],
                );
                let want = answer(
                    &db,
                    &format!("get {coll}{} {tail} facet brand, color", w(rest)),
                    &[],
                );
                let plain = answer(
                    &db,
                    &format!("get {coll}{} {tail} facet color, year", w(filter)),
                    &[],
                );
                let at = format!("{coll} {filter} | {tail}");
                assert_eq!(facet(&got, "brand"), facet(&want, "brand"), "{at}");
                assert_eq!(facet(&got, "color"), facet(&plain, "color"), "{at}");
                assert_eq!(got.rows, plain.rows, "{at}: the page is the query's");
                // With no condition of its own the year's disjunctive count
                // is the plain one: the brand's conditions stay in it.
                if !filter.contains("year") {
                    assert_eq!(facet(&got, "year"), facet(&plain, "year"), "{at}");
                }
            }
        }
    }
}

/// A row past its time is out of a disjunctive facet's count too, though
/// the facet is of the field it expires by.
#[test]
fn a_disjunctive_facet_leaves_expired_rows_out() {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection s (at timestamp @ttl(1s), k text)",
        &[],
    );
    for (at, k) in [(0, "old"), (5_000, "new"), (6_000, "new")] {
        exec(
            &mut db,
            "put s {at: $1, k: $2}",
            &[Value::Timestamp(at), Value::Text(k.into())],
        );
    }
    db.set_clock(Some(5_500));
    let rs = answer(
        &db,
        "get s where at >= 5500 count facet at disjunctive, k",
        &[],
    );
    assert_eq!(facet(&rs, "at").values.len(), 2, "{:?}", rs.facets);
    assert_eq!(facet(&rs, "k").values, [(Value::Text("new".into()), 1)]);
}

/// `ranges`: how many rows hold a number in each range, held to a count by
/// hand -- through an ordered index and by reading the field, over every
/// row and over a filter's, a list once a row a range, nulls, a float
/// field, bounds the index cannot key, and a path into a json field.
#[test]
fn range_facets_count_each_range_through_the_index_and_the_read() {
    let mut db = Database::new();
    for (name, idx) in [("r", "@sorted"), ("u", "")] {
        exec(
            &mut db,
            &format!(
                "create collection {name} (price int {idx}, w float {idx}, at timestamp {idx}, \
                 sizes [int], meta json, kind text @hash)"
            ),
            &[],
        );
    }
    let mut rng = Rng(0x9e3779b97f4a7c15);
    let mut rows = Vec::new();
    for _ in 0..4_000 {
        let price = (rng.below(10) != 0).then(|| rng.below(12_000) as i64 - 500);
        let w = rng.below(1000) as f64 / 7.0;
        let sizes: Vec<i64> = (0..rng.below(4)).map(|_| rng.below(50) as i64).collect();
        let kind = ["a", "b", "c"][rng.below(3) as usize];
        rows.push((price, w, sizes.clone(), kind));
        let params = [
            price.map_or(Value::Null, Value::Int),
            Value::Float(w),
            price.map_or(Value::Null, |p| Value::Timestamp(p * 1000)),
            Value::List(sizes.iter().map(|s| Value::Int(*s)).collect()),
            match price {
                Some(p) => Value::object(vec![("p".into(), Value::Int(p))]).unwrap(),
                None => Value::Null,
            },
            Value::Text(kind.into()),
        ];
        for name in ["r", "u"] {
            exec(
                &mut db,
                &format!("put {name} {{price: $1, w: $2, at: $3, sizes: $4, meta: $5, kind: $6}}"),
                &params,
            );
        }
    }
    let count = |bounds: &[f64], vals: &mut dyn Iterator<Item = Vec<f64>>| {
        let mut n = vec![0u64; bounds.len() - 1];
        for row in vals {
            let mut hit = vec![false; n.len()];
            for v in row {
                if let Some(i) = (0..n.len()).find(|&i| bounds[i] <= v && v < bounds[i + 1]) {
                    hit[i] = true;
                }
            }
            for (i, h) in hit.iter().enumerate() {
                n[i] += *h as u64;
            }
        }
        n
    };
    let as_values = |bounds: &[Value], n: Vec<u64>| -> Vec<(Value, u64)> {
        n.into_iter()
            .enumerate()
            .map(|(i, c)| {
                (
                    Value::List(vec![bounds[i].clone(), bounds[i + 1].clone()]),
                    c,
                )
            })
            .collect()
    };
    for (filter, keep) in [
        ("", (|_: &str| true) as fn(&str) -> bool),
        ("where kind = \"b\"", |k| k == "b"),
        ("where kind in [\"a\", \"c\"]", |k| k != "b"),
    ] {
        for coll in ["r", "u"] {
            for (field, text, bounds) in [
                (
                    "price",
                    "[0, 2500, 5000, 10000]",
                    vec![0.0, 2500.0, 5000.0, 10000.0],
                ),
                ("price", "[-1000, 0.5, 11000]", vec![-1000.0, 0.5, 11000.0]),
                ("w", "[0, 10, 50.5, 200]", vec![0.0, 10.0, 50.5, 200.0]),
                (
                    "at",
                    "[0, 2500000, 9000000]",
                    vec![0.0, 2_500_000.0, 9_000_000.0],
                ),
                ("sizes", "[0, 10, 20, 49]", vec![0.0, 10.0, 20.0, 49.0]),
                ("meta.p", "[0, 2500, 5000]", vec![0.0, 2500.0, 5000.0]),
            ] {
                let rs = answer(
                    &db,
                    &format!("get {coll} {filter} limit 1 facet {field} ranges {text}"),
                    &[],
                );
                let mut vals = rows.iter().filter(|r| keep(r.3)).map(|r| match field {
                    "price" | "meta.p" => r.0.iter().map(|p| *p as f64).collect(),
                    "at" => r.0.iter().map(|p| (*p * 1000) as f64).collect(),
                    "w" => vec![r.1],
                    _ => r.2.iter().map(|s| *s as f64).collect(),
                });
                let bound_values: Vec<Value> =
                    match fenec_ql::parse_one(&format!("get {coll} facet {field} ranges {text}"))
                        .unwrap()
                    {
                        Statement::Select(s) => s.facets[0].ranges.clone().unwrap(),
                        _ => unreachable!(),
                    };
                assert_eq!(
                    facet(&rs, field).values,
                    as_values(&bound_values, count(&bounds, &mut vals)),
                    "{coll} {field} {text} {filter}"
                );
            }
        }
    }
    // The index answers where it keys every bound, and the read where not.
    let plan = |sql: &str| {
        answer(&db, &format!("explain {sql}"), &[])
            .rows
            .iter()
            .map(|r| format!("{:?}", r.values[0]))
            .collect::<String>()
    };
    assert!(
        plan("get r where kind = \"b\" facet price ranges [0, 5000]").contains("ordered index")
    );
    assert!(plan("get r facet price ranges [0, 0.5]").contains("read"));
    assert!(plan("get u facet price ranges [0, 5000]").contains("read"));
    // A disjunctive range facet leaves its own range condition out.
    let rs = answer(
        &db,
        "get r where price >= 5000 and kind = \"a\" count facet price ranges [0, 5000, 20000] disjunctive",
        &[],
    );
    let want = answer(
        &db,
        "get r where kind = \"a\" count facet price ranges [0, 5000, 20000]",
        &[],
    );
    assert_eq!(rs.facets, want.facets);
}
