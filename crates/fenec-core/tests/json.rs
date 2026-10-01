//! `json` fields: objects held whole, paths read into them in every place a
//! field is read, and `@hash`, `@unique` and `@sorted` on a path giving the
//! scan's answer row for row -- through writes, a reopen, a checkpoint, a
//! replica, a compact, an alter and a block put back.

use fenec_core::prelude::*;
use std::sync::{Arc, Mutex};

/// A file in memory, and the records it was handed.
#[derive(Clone)]
struct Tap {
    file: Arc<Mutex<Vec<u8>>>,
    records: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Tap {
    fn new() -> Tap {
        Tap {
            file: Arc::new(Mutex::new(fenec_core::engine::MAGIC.to_vec())),
            records: Arc::default(),
        }
    }
    fn database(&self) -> Database {
        Database::with_sink(Box::new(self.clone()))
    }
    fn reopen(&self) -> Database {
        let mut db = Database::with_sink(Box::new(self.clone()));
        let bytes = self.file.lock().unwrap().clone();
        let whole = db.load(&bytes).expect("reopen");
        self.file.lock().unwrap().truncate(whole);
        db
    }
}

impl Sink for Tap {
    fn append(&mut self, bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.file.lock().unwrap().extend_from_slice(bytes);
        Ok(())
    }
    fn record(&mut self, _seq: u64, bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.file.lock().unwrap().extend_from_slice(bytes);
        self.records.lock().unwrap().push(bytes.to_vec());
        Ok(())
    }
    fn rewrite(&mut self, bytes: &[u8]) -> fenec_core::error::Result<()> {
        *self.file.lock().unwrap() = bytes.to_vec();
        Ok(())
    }
}

fn run(db: &mut Database, q: &str) -> Result<Response> {
    db.execute(&fenec_ql::parse_one(q).unwrap_or_else(|e| panic!("{q}: {e}")))
}

fn ok(db: &mut Database, q: &str) {
    run(db, q).unwrap_or_else(|e| panic!("{q}: {e}"));
}

fn err(db: &mut Database, q: &str) -> String {
    match fenec_ql::parse_one(q) {
        Err(e) => e.to_string(),
        Ok(s) => db.execute(&s).expect_err(q).to_string(),
    }
}

fn rows(db: &Database, q: &str) -> Vec<(u64, Vec<Value>)> {
    rows_with(db, q, &[])
}

fn rows_with(db: &Database, q: &str, params: &[Value]) -> Vec<(u64, Vec<Value>)> {
    db.query(
        &fenec_ql::parse_one(q).unwrap_or_else(|e| panic!("{q}: {e}")),
        params,
    )
    .unwrap_or_else(|e| panic!("{q}: {e}"))
    .rows()
    .unwrap()
    .rows
    .iter()
    .map(|r| (r.id, r.values.clone()))
    .collect()
}

fn ids(db: &Database, q: &str) -> Vec<u64> {
    rows(db, q).into_iter().map(|r| r.0).collect()
}

fn json(s: &str) -> Value {
    fenec_core::json::parse_json(s).unwrap()
}

fn docs(db: &mut Database) {
    ok(db, "create collection docs (title text, meta json)");
    ok(
        db,
        r#"put docs [
            {title: "a", meta: {lang: "tr", source: {site: "x", rank: 3}}},
            {title: "b", meta: {lang: "en", source: {site: "y", rank: 1}, tags: ["ai", "db"]}},
            {title: "c", meta: {lang: "tr", source: {site: "z", rank: 7.5}}},
            {title: "d", meta: "just text"},
            {title: "e"},
            {title: "f", meta: {lang: null, source: {rank: "high"}}}
        ]"#,
    );
}

#[test]
fn an_object_is_kept_whole_sorted_and_read_back() {
    let mut db = Database::new();
    docs(&mut db);
    assert_eq!(
        rows(&db, "get docs select meta where id = 1"),
        [(
            1,
            vec![json(r#"{"lang":"tr","source":{"rank":3,"site":"x"}}"#)]
        )]
    );
    // Any value JSON holds: a text, a list of numbers kept as written.
    ok(&mut db, "put docs {id: 9, meta: [1, 2.5]}");
    ok(
        &mut db,
        "put docs {id: 10, meta: {n: [19.99, 12345678901]}}",
    );
    assert_eq!(
        rows(&db, "get docs select meta where id = 9")[0].1,
        [json("[1,2.5]")]
    );
    assert_eq!(
        rows(&db, "get docs select meta.n where id = 10")[0].1,
        [json("[19.99,12345678901]")]
    );
    assert_eq!(
        rows(&db, "get docs select meta where id = 4")[0].1,
        [json(r#""just text""#)]
    );
    // A key given twice is refused, not one of them kept.
    assert!(err(&mut db, "put docs {meta: {a: 1, a: 2}}").contains("twice"));
    assert!(fenec_core::json::parse(r#"{"a": 1, "a": 2}"#).is_err());
    // A typed field still refuses an object.
    ok(&mut db, "create collection typed (n int, t text)");
    assert!(err(&mut db, "put typed {n: {a: 1}}").contains("object"));
    assert!(err(&mut db, "put typed {t: {a: 1}}").contains("object"));
    // Bytes and numbers that are not finite have no JSON.
    assert!(db
        .execute_with(
            &fenec_ql::parse_one("put docs {meta: $1}").unwrap(),
            &[Value::Bytes(vec![1])]
        )
        .is_err());
    assert!(db
        .execute_with(
            &fenec_ql::parse_one("put docs {meta: $1}").unwrap(),
            &[Value::Float(f64::NAN)]
        )
        .is_err());
}

#[test]
fn a_path_reads_in_every_place_a_field_is_read() {
    let mut db = Database::new();
    docs(&mut db);
    let q = |db: &Database, w: &str| ids(db, &format!("get docs {w}"));
    assert_eq!(q(&db, r#"where meta.lang = "tr""#), [1, 3]);
    assert_eq!(q(&db, r#"where meta.lang != "tr""#), [2, 4, 5, 6]);
    assert_eq!(q(&db, "where meta.source.rank >= 2"), [1, 3, 6]);
    assert_eq!(q(&db, "where meta.source.rank < 5"), [1, 2]);
    assert_eq!(q(&db, "where meta.source.rank = 3.0"), [1]);
    assert_eq!(
        q(&db, r#"where meta.lang = "tr" and meta.source.rank >= 2"#),
        [1, 3]
    );
    assert_eq!(q(&db, r#"where meta.lang in ["en", "xx"]"#), [2]);
    assert_eq!(q(&db, r#"where meta.tags has "db""#), [2]);
    assert_eq!(q(&db, "where meta.lang is null"), [4, 5, 6]);
    assert_eq!(q(&db, "where meta.lang is not null"), [1, 2, 3]);
    assert_eq!(q(&db, r#"where meta.source.site ~ "Y""#), [2]);
    assert_eq!(
        q(&db, r#"where meta.lang = "tr" or meta.source.rank = 1"#),
        [1, 2, 3]
    );
    assert_eq!(q(&db, "where meta.nope = 1"), Vec::<u64>::new());
    // A parameter binds as it does against a field.
    assert_eq!(
        rows_with(
            &db,
            "get docs select id where meta.lang = $1",
            &[Value::Text("en".into())]
        ),
        [(2, vec![Value::Int(2)])]
    );
    // Select: under the path's text, null where it leads nowhere.
    let r = db
        .query(
            &fenec_ql::parse_one("get docs select title, meta.source.site where id <= 4").unwrap(),
            &[],
        )
        .unwrap();
    let rs = r.rows().unwrap();
    assert_eq!(rs.columns, ["title", "meta.source.site"]);
    assert_eq!(
        rs.rows
            .iter()
            .map(|r| r.values[1].clone())
            .collect::<Vec<_>>(),
        [
            Value::Text("x".into()),
            Value::Text("y".into()),
            Value::Text("z".into()),
            Value::Null
        ]
    );
    // Order: numbers, then text, null first ascending.
    assert_eq!(q(&db, "order meta.source.rank desc"), [6, 3, 1, 2, 4, 5]);
    assert_eq!(q(&db, "order meta.source.rank limit 3"), [4, 5, 2]);
    // A path into a field that is not json is refused, at the first row.
    let e = db
        .query(
            &fenec_ql::parse_one("get docs where title.x = 1").unwrap(),
            &[],
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("json"), "{e}");
    assert!(db
        .query(
            &fenec_ql::parse_one("get docs select title.x").unwrap(),
            &[]
        )
        .is_err());
    // An aggregate reads a field.
    assert!(db
        .query(
            &fenec_ql::parse_one("get docs select sum(meta.source.rank)").unwrap(),
            &[]
        )
        .unwrap_err()
        .to_string()
        .contains("path"));
}

#[test]
fn a_path_sets_one_key_and_keeps_the_rest() {
    let mut db = Database::new();
    docs(&mut db);
    ok(&mut db, r#"set docs {meta.lang: "en"} where id = 1"#);
    assert_eq!(
        rows(&db, "get docs select meta where id = 1")[0].1,
        [json(r#"{"lang":"en","source":{"rank":3,"site":"x"}}"#)]
    );
    // An object made where the path finds none, and a value worked out over
    // the document as it was.
    ok(
        &mut db,
        "set docs {meta.source.rank: 10, meta.copy: title} where id = 5",
    );
    assert_eq!(
        rows(&db, "get docs select meta where id = 5")[0].1,
        [json(r#"{"copy":"e","source":{"rank":10}}"#)]
    );
    // A value on the way that is not an object is refused, not replaced.
    let e = err(&mut db, r#"set docs {meta.lang: "x"} where id = 4"#);
    assert!(e.contains("not an object"), "{e}");
    let e = err(&mut db, r#"set docs {meta.lang.code: "x"} where id = 1"#);
    assert!(e.contains("`meta.lang` holds text"), "{e}");
    // A put names a path too.
    ok(&mut db, r#"put docs {id: 20, title: "g", meta.lang: "de"}"#);
    assert_eq!(
        rows(&db, "get docs select meta where id = 20")[0].1,
        [json(r#"{"lang":"de"}"#)]
    );
    // Into a field that is not json, a path is refused.
    assert!(err(&mut db, r#"set docs {title.x: 1}"#).contains("json"));
}

#[test]
fn nesting_and_paths_have_limits_that_refuse() {
    let mut db = Database::new();
    ok(&mut db, "create collection d (meta json)");
    let max = fenec_core::value::MAX_JSON_DEPTH;
    let nest = |n: usize| "{a: ".repeat(n) + "1" + &"}".repeat(n);
    ok(&mut db, &format!("put d {{meta: {}}}", nest(max)));
    let e = err(&mut db, &format!("put d {{meta: {}}}", nest(max + 1)));
    assert!(e.contains("64 levels"), "{e}");
    // Through a parameter too: a list nests as an object does.
    let mut deep = Value::Int(1);
    for _ in 0..=max {
        deep = Value::List(vec![deep]);
    }
    let e = db
        .execute_with(&fenec_ql::parse_one("put d {meta: $1}").unwrap(), &[deep])
        .unwrap_err()
        .to_string();
    assert!(e.contains("64 levels"), "{e}");
    // A path reaches as deep as a value can be, and no further.
    let path = |n: usize| format!("meta.{}", vec!["a"; n].join("."));
    assert_eq!(ids(&db, &format!("get d where {} = 1", path(max))), [1]);
    let e = db
        .query(
            &fenec_ql::parse_one(&format!("get d where {} = 1", path(max + 1))).unwrap(),
            &[],
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("at most 64 keys"), "{e}");
}

/// Few distinct values of every kind JSON has, so ties and mixed kinds are
/// everywhere: ints and floats equal to each other, ints past 2^53, text,
/// booleans, lists, objects, null and missing keys.
fn meta(r: &mut u64) -> String {
    let mut next = || {
        *r ^= *r << 13;
        *r ^= *r >> 7;
        *r ^= *r << 17;
        *r
    };
    let lang = ["\"tr\"", "\"en\"", "\"de\"", "null", "3", "\"\""];
    let rank = [
        "1", "2", "3", "3.0", "-0.0", "0", "2.5", "-7", "1e300", "\"high\"", "\"low\"", "null",
    ];
    let rare = ["true", "[1, 2]", "{x: 1}", "9007199254740993", "\"3\""];
    let mut parts = Vec::new();
    match next() % 8 {
        0 => {}
        _ => parts.push(format!(
            "lang: {}",
            lang[(next() % lang.len() as u64) as usize]
        )),
    }
    let mut src = Vec::new();
    match next() % 10 {
        0 => {}
        // A kind the ordered index holds apart, now and then.
        1 => src.push(format!(
            "rank: {}",
            rare[(next() % rare.len() as u64) as usize]
        )),
        _ => src.push(format!(
            "rank: {}",
            rank[(next() % rank.len() as u64) as usize]
        )),
    }
    if next() % 3 > 0 {
        src.push(format!("site: \"s{}\"", next() % 4));
    }
    parts.push(format!("source: {{{}}}", src.join(", ")));
    match next() % 12 {
        0 => "\"text\"".into(),
        1 => "null".into(),
        _ => format!("{{{}}}", parts.join(", ")),
    }
}

fn twins(n: usize, rare: bool) -> (Database, Database) {
    let mut ix = Database::new();
    let mut plain = Database::new();
    ok(&mut ix, "create collection c (k int, meta json)");
    ok(&mut ix, "create index on c (meta.lang) @hash");
    ok(&mut ix, "create index on c (meta.source.rank) @sorted");
    ok(&mut ix, "create index on c (meta.source.site) @sorted");
    ok(&mut plain, "create collection c (k int, meta json)");
    let mut r = 0x9E37_79B9_7F4A_7C15u64;
    let mut batch = String::from("put c [");
    for i in 0..n {
        let mut m = meta(&mut r);
        if !rare {
            m = m
                .replace("true", "2")
                .replace("[1, 2]", "2")
                .replace("{x: 1}", "\"x\"")
                .replace("9007199254740993", "4");
        }
        batch.push_str(&format!("{{k: {}, meta: {m}}},", i % 5));
    }
    batch.push(']');
    ok(&mut ix, &batch);
    ok(&mut plain, &batch);
    (ix, plain)
}

fn filters() -> Vec<String> {
    let mut out = vec![String::new()];
    let values = [
        (
            "meta.lang",
            vec!["\"tr\"", "\"\"", "3", "3.0", "null", "\"zz\""],
        ),
        (
            "meta.source.rank",
            vec![
                "3",
                "3.0",
                "2.5",
                "0",
                "-0.0",
                "\"high\"",
                "\"m\"",
                "9007199254740993",
                "true",
            ],
        ),
        ("meta.source.site", vec!["\"s1\"", "\"s\"", "2"]),
    ];
    for (field, vals) in &values {
        for v in vals {
            for op in ["=", "!=", "<", "<=", ">", ">="] {
                out.push(format!("where {field} {op} {v}"));
            }
            out.push(format!("where {v} < {field}"));
        }
    }
    out.extend(
        [
            "where meta.lang in [\"tr\", \"de\", 3]",
            "where meta.lang in [\"tr\", 9007199254740993]",
            "where meta.lang = \"tr\" and meta.source.rank >= 2",
            "where meta.source.rank >= 1 and meta.source.rank < 3",
            "where meta.source.rank >= 2 and meta.source.rank <= \"low\"",
            "where meta.source.rank > 1 and meta.source.rank > \"a\"",
            "where meta.source.rank < \"a\" and k = 2",
            "where meta.lang is null",
            "where meta.source.rank is not null and meta.source.site = \"s2\"",
            "where meta.lang = \"en\" or meta.source.rank = 3",
            "where not (meta.source.rank > 2)",
        ]
        .map(String::from),
    );
    out
}

fn check(ix: &Database, plain: &Database, stage: &str) {
    let orders = [
        "",
        "order meta.source.rank",
        "order meta.source.rank desc",
        "order meta.source.site desc",
        "order meta.lang, k desc",
    ];
    let pages = ["", "limit 1", "limit 5", "limit 7 offset 3"];
    for f in filters() {
        let count = format!("get c {f} count");
        assert_eq!(rows(ix, &count), rows(plain, &count), "{stage}: {count}");
        for o in orders {
            for p in pages {
                let q = format!("get c select k, meta.lang, meta.source.rank {f} {o} {p}");
                assert_eq!(rows(ix, &q), rows(plain, &q), "{stage}: {q}");
            }
        }
    }
}

#[test]
fn an_index_on_a_path_gives_the_scans_answer() {
    for rare in [false, true] {
        let (mut ix, mut plain) = twins(500, rare);
        let stage = if rare {
            "kinds held apart"
        } else {
            "numbers and text"
        };
        check(&ix, &plain, stage);
        for db in [&mut ix, &mut plain] {
            ok(db, r#"set c {meta.source.rank: 3} where meta.lang = "de""#);
            ok(
                db,
                r#"set c {meta.lang: "tr"} where k = 1 and meta.source is not null"#,
            );
            ok(db, "del c where meta.source.rank = 2.5");
            ok(
                db,
                r#"put c {id: 7, meta: {lang: "en", source: {rank: 1.5, site: "s3"}}}"#,
            );
        }
        check(&ix, &plain, &format!("{stage}, after writes"));
        let mut reopened = Database::new();
        reopened.load(&ix.snapshot()).unwrap();
        check(&reopened, &plain, &format!("{stage}, after a reopen"));
    }
}

#[test]
fn the_indexes_answer_where_they_can_and_the_scan_where_they_cannot() {
    let (ix, _) = twins(300, false);
    let plan = |q: &str| {
        rows(&ix, &format!("explain {q}"))
            .into_iter()
            .map(|r| format!("{:?}", r.1[0]))
            .collect::<Vec<_>>()
            .join(" | ")
    };
    let p = plan(r#"get c where meta.lang = "tr" count"#);
    assert!(p.contains("hash index on meta.lang"), "{p}");
    let p = plan("get c where meta.source.rank >= 2.5 count");
    assert!(p.contains("ordered index on meta.source.rank"), "{p}");
    let p = plan("get c order meta.source.rank desc limit 3");
    assert!(
        p.contains("walked the ordered index on meta.source.rank"),
        "{p}"
    );
    // An int past 2^53 has no exact bucket: the scan answers.
    let p = plan("get c where meta.lang = 9007199254740993 count");
    assert!(p.contains("full scan"), "{p}");
    // A kind held apart leaves the ordered index to the scan.
    let (ix, _) = twins(300, true);
    let p = rows(&ix, "explain get c where meta.source.rank >= 2.5 count")
        .into_iter()
        .map(|r| format!("{:?}", r.1[0]))
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(p.contains("cannot order"), "{p}");
}

#[test]
fn a_unique_path_refuses_a_value_held_twice() {
    let mut db = Database::new();
    docs(&mut db);
    ok(&mut db, "create index on docs (meta.source.site) @unique");
    let e = err(&mut db, r#"put docs {meta: {source: {site: "x"}}}"#);
    assert!(e.contains("unique") && e.contains("\"x\""), "{e}");
    // `3` and `3.0` are one value, as the scan finds them.
    ok(&mut db, "create collection u (meta json)");
    ok(&mut db, "create index on u (meta.n) @unique");
    ok(&mut db, "put u {meta: {n: 3}}");
    assert!(err(&mut db, "put u {meta: {n: 3.0}}").contains("unique"));
    ok(&mut db, "put u {meta: {n: 3.5}}");
    // Null is no value, and over values held twice the index is refused.
    ok(&mut db, "put u [{meta: {}}, {meta: {}}]");
    ok(&mut db, "create collection v (meta json)");
    ok(&mut db, "put v [{meta: {n: 1}}, {meta: {n: 1.0}}]");
    let e = err(&mut db, "create index on v (meta.n) @unique");
    assert!(e.contains("cannot be unique"), "{e}");
    assert!(db.collection("v").unwrap().schema.paths.is_empty());
}

#[test]
fn a_path_takes_equality_and_order_and_names_a_json_field() {
    let mut db = Database::new();
    docs(&mut db);
    assert!(err(&mut db, "create index on docs (meta.lang) @text").contains("path"));
    assert!(err(&mut db, "create index on docs (title.x) @hash").contains("json"));
    assert!(err(&mut db, "create index on docs (nope.x) @hash").contains("nope"));
    ok(&mut db, "create index on docs (meta.lang) @hash");
    assert!(err(&mut db, "create index on docs (meta.lang) @sorted").contains("already"));
    ok(
        &mut db,
        "create index if not exists on docs (meta.lang) @sorted",
    );
    // `describe` lists them after the fields.
    let d = fenec_core::json::response_to_string(&run(&mut db, "describe docs").unwrap());
    let want = r#"{"name":"meta","type":"json","index":"none"}],"paths":[{"name":"meta.lang","type":"json","index":"hash"}]"#;
    assert!(d.contains(want), "{d}");
    // A field is named without a dot.
    assert!(err(&mut db, "create collection x (a.b int)").contains("dot"));
    assert!(err(&mut db, "alter collection docs add field x.y int").contains("dot"));
    assert!(err(&mut db, "create collection x (l [json])").contains("list of json"));
}

fn indexed(db: &mut Database) {
    docs(db);
    ok(db, "create index on docs (meta.lang) @hash");
    ok(db, "create index on docs (meta.source.rank) @sorted");
}

fn answers(db: &Database) -> Vec<Vec<u64>> {
    [
        r#"get docs where meta.lang = "tr""#,
        "get docs where meta.source.rank >= 2",
        "get docs order meta.source.rank desc limit 4",
        "get docs select meta.source.site",
    ]
    .iter()
    .map(|q| ids(db, q))
    .collect()
}

#[test]
fn objects_and_path_indexes_come_back_after_an_open_a_checkpoint_and_a_compact() {
    let tap = Tap::new();
    let mut db = tap.database();
    indexed(&mut db);
    ok(&mut db, r#"set docs {meta.lang: "tr"} where id = 2"#);
    ok(&mut db, "del docs where id = 5");
    let want = answers(&db);
    let schema = db.collection("docs").unwrap().schema.clone();
    let everything = rows(&db, "get docs");
    drop(db);
    let mut db = tap.reopen();
    assert_eq!(db.collection("docs").unwrap().schema, schema);
    assert_eq!(answers(&db), want);
    assert_eq!(rows(&db, "get docs"), everything);
    db.checkpoint().unwrap();
    drop(db);
    let mut db = tap.reopen();
    assert_eq!(answers(&db), want);
    ok(&mut db, "compact");
    assert_eq!(answers(&db), want);
    assert_eq!(rows(&db, "get docs"), everything);
    drop(db);
    let db = tap.reopen();
    assert_eq!(db.collection("docs").unwrap().schema, schema);
    assert_eq!(answers(&db), want);
}

#[test]
fn a_replica_applies_objects_and_path_indexes() {
    let tap = Tap::new();
    let mut primary = tap.database();
    indexed(&mut primary);
    ok(&mut primary, r#"set docs {meta.lang: "de"} where id = 3"#);
    ok(
        &mut primary,
        "create index on docs (meta.source.site) @unique",
    );
    let replica_tap = Tap::new();
    let mut replica = replica_tap.database();
    for r in tap.records.lock().unwrap().iter() {
        replica.apply(r).unwrap();
    }
    assert_eq!(answers(&replica), answers(&primary));
    assert_eq!(rows(&replica, "get docs"), rows(&primary, "get docs"));
    assert_eq!(
        replica.collection("docs").unwrap().schema,
        primary.collection("docs").unwrap().schema
    );
    // The replica built the index it was sent: it answers through it.
    let plan = rows(&replica, r#"explain get docs where meta.lang = "de""#);
    assert!(
        format!("{plan:?}").contains("hash index on meta.lang"),
        "{plan:?}"
    );
    drop(replica);
    assert_eq!(answers(&replica_tap.reopen()), answers(&primary));
}

#[test]
fn a_block_puts_a_path_index_and_its_writes_back() {
    let mut db = Database::new();
    docs(&mut db);
    let before = answers(&db);
    let s1 = fenec_ql::parse_one("create index on docs (meta.lang) @hash").unwrap();
    let s2 = fenec_ql::parse_one(r#"put docs {meta: {lang: "tr"}}"#).unwrap();
    let s3 = fenec_ql::parse_one(r#"set docs {meta.lang: "xx"} where id = 1"#).unwrap();
    let s4 = fenec_ql::parse_one("create index on docs (nope.x) @hash").unwrap();
    assert!(db
        .execute_block(&[(&s1, &[]), (&s2, &[]), (&s3, &[]), (&s4, &[])])
        .is_err());
    assert!(db.collection("docs").unwrap().schema.paths.is_empty());
    assert!(db
        .collection("docs")
        .unwrap()
        .hashes
        .get("meta.lang")
        .is_none());
    assert_eq!(answers(&db), before);
    // Landed, the index answers what the scan does.
    db.execute_block(&[(&s1, &[]), (&s2, &[]), (&s3, &[])])
        .unwrap();
    assert_eq!(ids(&db, r#"get docs where meta.lang = "tr""#), [3, 7]);
    assert_eq!(ids(&db, r#"get docs where meta.lang = "xx""#), [1]);
}

#[test]
fn an_alter_takes_a_json_fields_path_indexes_with_it() {
    let tap = Tap::new();
    let mut db = tap.database();
    indexed(&mut db);
    ok(&mut db, "alter collection docs rename field meta to info");
    assert_eq!(
        db.collection("docs")
            .unwrap()
            .schema
            .paths
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>(),
        ["info.lang", "info.source.rank"]
    );
    let plan = rows(&db, r#"explain get docs where info.lang = "tr""#);
    assert!(
        format!("{plan:?}").contains("hash index on info.lang"),
        "{plan:?}"
    );
    assert_eq!(ids(&db, r#"get docs where info.lang = "tr""#), [1, 3]);
    assert!(db
        .query(
            &fenec_ql::parse_one(r#"get docs where meta.lang = "tr""#).unwrap(),
            &[]
        )
        .is_err());
    // A rename put back with its block puts the indexes back under the
    // old name.
    let rename = fenec_ql::parse_one("alter collection docs rename field info to meta").unwrap();
    let bad = fenec_ql::parse_one("put nope {a: 1}").unwrap();
    assert!(db.execute_block(&[(&rename, &[]), (&bad, &[])]).is_err());
    let plan = rows(&db, r#"explain get docs where info.lang = "tr""#);
    assert!(
        format!("{plan:?}").contains("hash index on info.lang"),
        "{plan:?}"
    );
    // A drop takes them off, and put back, they answer again.
    let drop_it = fenec_ql::parse_one("alter collection docs drop field info").unwrap();
    assert!(db.execute_block(&[(&drop_it, &[]), (&bad, &[])]).is_err());
    assert_eq!(ids(&db, r#"get docs where info.lang = "tr""#), [1, 3]);
    let plan = rows(&db, r#"explain get docs where info.source.rank > 2"#);
    assert!(
        format!("{plan:?}").contains("ordered index on info.source.rank"),
        "{plan:?}"
    );
    ok(&mut db, "alter collection docs drop field info");
    assert!(db.collection("docs").unwrap().schema.paths.is_empty());
    assert!(db.collection("docs").unwrap().sorted.is_empty());
    drop(db);
    let db = tap.reopen();
    assert!(db.collection("docs").unwrap().schema.paths.is_empty());
}

#[test]
fn a_server_builds_a_path_index_under_the_lock() {
    let mut db = Database::new();
    docs(&mut db);
    let lock = std::sync::RwLock::new(db);
    let stmt = fenec_ql::parse_one("create index on docs (meta.lang) @hash").unwrap();
    Database::maintain(&lock, &stmt).unwrap().unwrap();
    let db = lock.into_inner().unwrap();
    assert_eq!(ids(&db, r#"get docs where meta.lang = "tr""#), [1, 3]);
    assert!(db
        .collection("docs")
        .unwrap()
        .schema
        .path("meta.lang")
        .is_some());
}

#[test]
fn the_file_holds_tags_a_binary_from_before_refuses() {
    // A json field's type is tag 14 and an object's tag 13 -- neither 11
    // nor 12, which a schema writes for a collation and a dropped place --
    // and a binary from before refuses a tag it does not know rather than
    // read on: here, this one meeting tag 15.
    let tap = Tap::new();
    let mut db = tap.database();
    indexed(&mut db);
    let schema = db.collection("docs").unwrap().schema.encode();
    assert!(
        schema
            .windows(2)
            .any(|w| w == [b'a', fenec_core::codec::TAG_JSON]),
        "{schema:?}"
    );
    let file = tap.file.lock().unwrap().clone();
    assert!(file.contains(&fenec_core::codec::TAG_OBJECT));
    let at = file
        .windows(5)
        .position(|w| w == [b'm', b'e', b't', b'a', fenec_core::codec::TAG_JSON])
        .unwrap()
        + 4;
    let mut damaged = file.clone();
    damaged[at] = 15;
    let e = Database::new().load(&damaged).unwrap_err();
    assert!(matches!(e, Error::Corrupt(_)), "{e}");
}
