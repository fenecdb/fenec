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
    ] {
        let e = error(&db, sql);
        assert!(e.contains(want), "{sql}: {e}");
    }
}
