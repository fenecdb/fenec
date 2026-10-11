//! An index over several fields never changes an answer.
//!
//! Every query here runs twice: on a collection carrying indexes over
//! several fields and on a twin without them, holding the same documents.
//! The twin is the scan, so the two have to agree row for row -- order, ties
//! and page included -- through writes, deletes, updates of a key's part, a
//! block put back, a reopen, an index made after the fact, a field renamed
//! and one dropped.

use fenec_core::prelude::*;

const FIELDS: &str = "owner int, status text, created timestamp, score float, flag bool, \
     title text, name text collate tr, meta json";

/// The indexes the twin with them carries: an equality prefix and an order,
/// three parts with a float in the middle, a bool and a collated text, a
/// json path and a descending int.
const INDEXES: &[&str] = &[
    "@sorted(owner, created desc)",
    "@sorted(status, score, created)",
    "@sorted(flag, name desc)",
    "@sorted(meta.k, owner desc)",
];

fn exec(db: &mut Database, sql: &str, params: &[Value]) -> Response {
    db.execute_with(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn rows(db: &Database, sql: &str, params: &[Value]) -> Vec<(u64, String)> {
    let r = db
        .query(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    // Values by their text: a NaN is not equal to itself.
    r.rows()
        .expect("rows")
        .rows
        .iter()
        .map(|r| (r.id, format!("{:?}", r.values)))
        .collect()
}

fn plan(db: &Database, sql: &str) -> String {
    rows(db, &format!("explain {sql}"), &[])
        .into_iter()
        .map(|(_, s)| s)
        .collect::<Vec<_>>()
        .join("\n")
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<T: Clone>(&mut self, xs: &[T]) -> T {
        xs[(self.next() % xs.len() as u64) as usize].clone()
    }
}

/// Few distinct values, so ties are everywhere; nulls, negatives, `-0.0`,
/// a zero byte in a text and texts a collation orders apart from their
/// bytes among them.
fn document(r: &mut Rng) -> Vec<(String, Expr)> {
    let owner = r.pick(&[
        Value::Null,
        Value::Int(-3),
        Value::Int(1),
        Value::Int(2),
        Value::Int(2),
        Value::Int(7),
    ]);
    let status = r.pick(&[
        Value::Null,
        Value::Text("open".into()),
        Value::Text("open".into()),
        Value::Text("done".into()),
        Value::Text("".into()),
        Value::Text("a\0b".into()),
    ]);
    let created = r.pick(&[
        Value::Null,
        Value::Text("2025-06-01T00:00:00Z".into()),
        Value::Text("2026-01-01T00:00:00Z".into()),
        Value::Text("2026-01-01T00:00:00Z".into()),
        Value::Text("2026-09-01T12:00:00Z".into()),
    ]);
    let score = r.pick(&[
        Value::Null,
        Value::Float(-2.5),
        Value::Float(-0.0),
        Value::Float(0.0),
        Value::Float(1.5),
        Value::Float(1e300),
    ]);
    let flag = r.pick(&[Value::Null, Value::Bool(false), Value::Bool(true)]);
    let title = r.pick(&["x", "y", "z"]);
    let name = r.pick(&[
        Value::Null,
        Value::Text("Ağaç".into()),
        Value::Text("ağaç".into()),
        Value::Text("çay".into()),
        Value::Text("cay".into()),
        Value::Text("ılık".into()),
        Value::Text("ilik".into()),
        Value::Text("zeytin".into()),
    ]);
    let meta = r.pick(&[
        "null",
        "{}",
        "{\"k\": 1}",
        "{\"k\": 2.5}",
        "{\"k\": 2}",
        "{\"k\": \"a\"}",
        "{\"k\": \"b\"}",
    ]);
    let mut doc = Vec::new();
    for (k, v) in [
        ("owner", owner),
        ("status", status),
        ("created", created),
        ("score", score),
        ("flag", flag),
        ("name", name),
    ] {
        if v != Value::Null {
            doc.push((k.to_string(), Expr::Lit(v)));
        }
    }
    doc.push(("title".into(), Expr::Lit(Value::Text(title.into()))));
    let meta = fenec_core::json::parse_json(meta).unwrap();
    if meta != Value::Null {
        doc.push(("meta".into(), Expr::Lit(meta)));
    }
    doc
}

/// Two databases with the same documents: `ix` with the indexes, `plain`
/// without.
fn twins(docs: usize) -> (Database, Database) {
    let mut ix = Database::new();
    let mut plain = Database::new();
    exec(
        &mut ix,
        &format!("create collection c ({FIELDS}, {})", INDEXES.join(", ")),
        &[],
    );
    exec(&mut plain, &format!("create collection c ({FIELDS})"), &[]);
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
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

fn filters() -> Vec<String> {
    let mut out = vec![String::new()];
    for owner in ["1", "2", "-3", "9", "2.0", "\"2\""] {
        out.push(format!("where owner = {owner}"));
        for c in [
            "created >= \"2026-01-01\"",
            "created < \"2026-01-01\"",
            "created > \"2025-06-01\" and created <= \"2026-09-01T12:00:00Z\"",
            "created = \"2026-01-01T00:00:00Z\"",
            "created > \"garbage\"",
            "created is null",
            "title = \"x\"",
            "created >= \"2026-01-01\" and title != \"y\"",
        ] {
            out.push(format!("where owner = {owner} and {c}"));
        }
    }
    out.extend(
        [
            "where owner in [1, 2]",
            "where owner in [2, 7, 2.0] and created >= \"2026-01-01\"",
            "where owner in [1, \"x\"]",
            "where owner > 1",
            "where owner >= -3 and owner < 7",
            "where owner = 2 or owner = 1",
            "where not (owner = 2)",
            "where owner = 2 and owner = 1",
            "where owner = 2 and owner >= 2",
            "where owner is null",
            "where status = \"open\"",
            "where status = \"open\" and score = 0",
            "where status = \"open\" and score = -0.0 and created >= \"2026-01-01\"",
            "where status = \"open\" and score > -1",
            "where status = \"\" and score <= 1.5",
            "where status = \"a\\u0000b\"",
            "where status >= \"a\" and status < \"e\"",
            "where status in [\"open\", \"done\"] and score >= 0",
            "where status = \"open\" and created >= \"2026-01-01\"",
            "where flag = true",
            "where flag = false and name >= \"b\"",
            "where flag = true and name < \"d\"",
            "where flag = true and name = \"ağaç\"",
            "where flag in [true, false] and name > \"b\"",
            "where meta.k in [2, \"a\", 1]",
            "where flag = 1",
            "where meta.k = 2",
            "where meta.k = 2.0",
            "where meta.k >= 2",
            "where meta.k < \"b\"",
            "where meta.k = \"a\" and owner > 1",
            "where meta.k in [1, \"b\"]",
            "where title ~ \"x\"",
        ]
        .map(String::from),
    );
    out
}

fn check(ix: &Database, plain: &Database, stage: &str) {
    let orders = [
        "",
        "order created desc",
        "order created",
        "order owner, created desc",
        "order owner desc, created",
        "order owner, created",
        "order owner",
        "order owner desc",
        "order score, created",
        "order score desc, created desc",
        "order status, score, created",
        "order name desc",
        "order name",
        "order name collate und",
        "order flag, name desc",
        "order meta.k, owner desc",
        "order meta.k desc, owner",
        "order created desc, owner",
    ];
    // A page's size and place written, and handed as parameters, which
    // the walk reads where it reads the number.
    let pages: [(&str, &[Value]); 7] = [
        ("", &[]),
        ("limit 1", &[]),
        ("limit 5", &[]),
        ("limit 7 offset 3", &[]),
        ("limit 50 offset 40", &[]),
        ("limit $1", &[Value::Int(4)]),
        ("limit $1 offset $2", &[Value::Int(6), Value::Int(2)]),
    ];
    for f in filters() {
        let count = format!("get c {f} count");
        assert_eq!(
            rows(ix, &count, &[]),
            rows(plain, &count, &[]),
            "{stage}: {count}"
        );
        for o in orders {
            for (p, params) in pages {
                let q = format!("get c select owner, status, created, score, name {f} {o} {p}");
                assert_eq!(
                    rows(ix, &q, params),
                    rows(plain, &q, params),
                    "{stage}: {q}"
                );
            }
        }
    }
}

#[test]
fn an_index_over_several_fields_gives_the_scans_answer() {
    let (mut ix, mut plain) = twins(400);
    check(&ix, &plain, "after the load");

    // Writes: a key's part updated to and from null, a whole row written
    // over, deletes; the index has to follow each.
    for db in [&mut ix, &mut plain] {
        exec(
            db,
            "set c {created: \"2026-09-01T12:00:00Z\"} where owner = 2 and title = \"x\"",
            &[],
        );
        exec(
            db,
            "set c {owner: null} where owner = 7 and title = \"y\"",
            &[],
        );
        exec(
            db,
            "set c {owner: 7, status: \"done\"} where owner is null and title = \"z\"",
            &[],
        );
        exec(
            db,
            "set c {name: \"Çay\", flag: true} where name = \"cay\"",
            &[],
        );
        exec(db, "set c {meta: {k: 1}} where meta.k = \"b\"", &[]);
        exec(db, "del c where status = \"done\" and owner = 1", &[]);
        exec(
            db,
            "put c {id: 7, owner: 2, created: \"2026-01-01\", title: \"x\"}",
            &[],
        );
    }
    check(&ix, &plain, "after writes");

    // A block put back leaves the index as it was.
    for db in [&mut ix, &mut plain] {
        db.begin().unwrap();
        exec(db, "set c {owner: 1} where owner = 2", &[]);
        exec(db, "del c where status = \"open\"", &[]);
        exec(
            db,
            "put c {owner: 1, created: \"2027-01-01\", title: \"x\"}",
            &[],
        );
        db.rollback();
    }
    check(&ix, &plain, "after a block put back");

    // A reopen builds the indexes from the documents.
    let mut reopened = Database::new();
    reopened.load(&ix.snapshot()).unwrap();
    check(&reopened, &plain, "after a reopen");
}

/// An index created over data that is already there is the one kept on
/// write.
#[test]
fn an_index_created_later_gives_the_same_answers() {
    let (_, mut plain) = twins(300);
    let mut later = Database::new();
    later.load(&plain.snapshot()).unwrap();
    for ix in INDEXES {
        let (kind, keys) = ix.trim_start_matches('@').split_once('(').unwrap();
        exec(
            &mut later,
            &format!("create index on c ({keys} @{kind}"),
            &[],
        );
    }
    check(&later, &plain, "index created later");
    exec(&mut plain, "put c {owner: 2, created: \"2026-02-02\"}", &[]);
    exec(&mut later, "put c {owner: 2, created: \"2026-02-02\"}", &[]);
    check(&later, &plain, "index created later, then a write");
}

/// A part renamed keeps the index, under the field's new name; a part
/// dropped takes it off, and the scan answers.
#[test]
fn a_part_renamed_or_dropped() {
    let (mut ix, mut plain) = twins(200);
    for db in [&mut ix, &mut plain] {
        exec(db, "alter collection c rename field created to at", &[]);
    }
    let q = "get c select owner, at where owner = 2 order at desc limit 5";
    assert_eq!(rows(&ix, q, &[]), rows(&plain, q, &[]));
    assert!(
        plan(&ix, q).contains("walked the index on (owner, at desc)"),
        "{}",
        plan(&ix, q)
    );
    let d = ix.query(&Statement::Describe("c".into()), &[]).unwrap();
    let Response::Schemas(s) = d else { panic!() };
    assert!(s[0].composites.iter().any(|c| c.keys[1].0 == "at"));

    for db in [&mut ix, &mut plain] {
        exec(db, "alter collection c drop field at", &[]);
    }
    let Response::Schemas(s) = ix.query(&Statement::Describe("c".into()), &[]).unwrap() else {
        panic!()
    };
    assert_eq!(s[0].composites.len(), 2, "{:?}", s[0].composites);
    let q = "get c select owner where owner = 2 limit 5";
    assert_eq!(rows(&ix, q, &[]), rows(&plain, q, &[]));
}

/// The page shapes the index is for take it, and say so.
#[test]
fn explain_names_the_index_the_prefix_and_the_range() {
    let (ix, _) = twins(400);
    let p = plan(&ix, "get c where owner = 2 order created desc limit 5");
    assert!(
        p.contains("order: walked the index on (owner, created desc), owner fixed"),
        "{p}"
    );
    let p = plan(&ix, "get c where owner = 2 order created limit 5");
    assert!(p.contains("backwards"), "{p}");
    let p = rows(
        &ix,
        "explain get c where owner = $1 order created desc limit $2",
        &[Value::Int(2), Value::Int(3)],
    );
    assert!(
        format!("{p:?}").contains("walked the index on (owner, created desc), owner fixed, 3 of"),
        "{p:?}"
    );
    let p = plan(&ix, "get c where flag = true order name desc limit 5");
    assert!(
        p.contains("walked the index on (flag, name desc), flag fixed"),
        "{p}"
    );
    let p = plan(&ix, "get c where meta.k = 2 order owner desc limit 5");
    assert!(
        p.contains("walked the index on (meta.k, owner desc), meta.k fixed"),
        "{p}"
    );
    let p = plan(&ix, "get c order owner, created desc limit 5");
    assert!(
        p.contains("walked the index on (owner, created desc)"),
        "{p}"
    );
    let p = plan(
        &ix,
        "get c where status = \"open\" and score >= 0 order score limit 5",
    );
    assert!(
        p.contains("the index on (status, score, created), status fixed, a range on score"),
        "{p}"
    );
    let p = plan(
        &ix,
        "get c where status = \"open\" and score = 0 and created > \"2026-01-01\" count",
    );
    assert!(
        p.contains(
            "the index on (status, score, created), status, score fixed, a range on created"
        ),
        "{p}"
    );
    let p = plan(
        &ix,
        "get c where owner in [1, 2] and created >= \"2026-01-01\"",
    );
    assert!(
        p.contains("owner fixed, 2 spans, a range on created"),
        "{p}"
    );
    // A feed: the spans an `in` makes, walked together by the order.
    let p = plan(
        &ix,
        "get c where owner in [1, 2, 7] order created desc limit 5",
    );
    assert!(
        p.contains("order: walked the index on (owner, created desc), owner fixed, 3 spans, 5 of"),
        "{p}"
    );
    let p = plan(
        &ix,
        "get c where owner in [1, 2] order owner, created desc limit 5",
    );
    assert!(
        !p.contains("order: walked"),
        "an `in`'s part is not one value: {p}"
    );
}

/// `NaN` compares equal to everything: a row holding one is apart, the
/// walks step aside while it is there, and a range's rows take it to test.
#[test]
fn nan_rows_match_what_the_scan_says_they_match() {
    let (mut ix, mut plain) = twins(150);
    for db in [&mut ix, &mut plain] {
        exec(
            db,
            "put c {status: \"open\", score: $1, created: \"2026-01-01\"}",
            &[Value::Float(f64::NAN)],
        );
    }
    check(&ix, &plain, "with a NaN");
    let p = plan(&ix, "get c where status = \"open\" order score limit 5");
    assert!(
        p.contains("not walked, it holds a value it cannot order"),
        "{p}"
    );
}

fn refused(db: &mut Database, sql: &str) -> Error {
    match fenec_ql::parse_one(sql) {
        Err(e) => e,
        Ok(stmt) => db.execute(&stmt).expect_err(sql),
    }
}

#[test]
fn a_unique_index_over_several_fields_refuses_a_second_holder() {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection pages (org text, slug text, n int, @unique(org, slug))",
        &[],
    );
    exec(
        &mut db,
        "put pages {org: \"acme\", slug: \"home\", n: 1}",
        &[],
    );
    exec(
        &mut db,
        "put pages {org: \"acme\", slug: \"about\", n: 2}",
        &[],
    );
    exec(
        &mut db,
        "put pages {org: \"zeta\", slug: \"home\", n: 3}",
        &[],
    );
    // A null in a part is no value.
    exec(&mut db, "put pages {org: \"acme\", n: 4}", &[]);
    exec(&mut db, "put pages {org: \"acme\", n: 5}", &[]);
    let e = refused(&mut db, "put pages {org: \"acme\", slug: \"home\", n: 6}");
    assert!(matches!(e, Error::Duplicate(_)), "{e}");
    assert_eq!(
        e.to_string(),
        "duplicate: `pages (org, slug)` is unique, and document 1 holds [\"acme\", \"home\"] already"
    );
    // A set onto a held key, and one keeping its own.
    let e = refused(&mut db, "set pages {slug: \"home\"} where n = 2");
    assert!(matches!(e, Error::Duplicate(_)), "{e}");
    exec(&mut db, "set pages {n: 10} where n = 1", &[]);
    exec(&mut db, "set pages {slug: \"home\"} where n = 1", &[]);
    // `if absent` passes over it, and `else set` sets the row holding it.
    let r = exec(
        &mut db,
        "put pages {org: \"acme\", slug: \"home\", n: 7} if absent",
        &[],
    );
    assert_eq!(r, Response::Affected(0));
    exec(
        &mut db,
        "put pages {org: \"acme\", slug: \"home\", n: 7} if absent else set {n: n + new.n}",
        &[],
    );
    assert_eq!(
        rows(
            &db,
            "get pages select n where org = \"acme\" and slug = \"home\"",
            &[]
        ),
        vec![(1, "[Int(17)]".into())]
    );
    // Freed by a delete, taken again within one block.
    db.begin().unwrap();
    exec(&mut db, "del pages where n = 17", &[]);
    exec(
        &mut db,
        "put pages {org: \"acme\", slug: \"home\", n: 8}",
        &[],
    );
    db.commit().unwrap();
    // A unique index over documents holding a key twice is refused, naming
    // two of them, and taken back.
    exec(&mut db, "create collection t (a int, b int)", &[]);
    exec(
        &mut db,
        "put t [{a: 1, b: 2}, {a: 1, b: 3}, {a: 1, b: 2}]",
        &[],
    );
    let e = refused(&mut db, "create index on t (a, b) @unique");
    assert_eq!(
        e.to_string(),
        "duplicate: `t (a, b)` cannot be unique: documents 1 and 3 both hold [1, 2] already"
            .replace(" already", "")
    );
    exec(&mut db, "put t {a: 1, b: 2}", &[]);
    exec(&mut db, "create index on t (a, b) @sorted", &[]);
    exec(&mut db, "drop index on t (a, b)", &[]);
    exec(&mut db, "del t where id = 3", &[]);
    exec(&mut db, "del t where id = 4", &[]);
    exec(&mut db, "create index on t (a, b) @unique", &[]);
    let e = refused(&mut db, "put t {a: 1, b: 3}");
    assert!(matches!(e, Error::Duplicate(_)), "{e}");
    // It survives a reopen.
    let mut reopened = Database::new();
    reopened.load(&db.snapshot()).unwrap();
    let e = refused(&mut reopened, "put t {a: 1, b: 3}");
    assert!(matches!(e, Error::Duplicate(_)), "{e}");
}

#[test]
fn what_an_index_over_several_fields_refuses() {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection x (a int, b text, v vector<3>, l [int], m json)",
        &[],
    );
    for (sql, why) in [
        ("create index on x (a, a) @sorted", "named twice"),
        ("create index on x (a, v) @sorted", "orders bool, int"),
        ("create index on x (a, l) @sorted", "orders bool, int"),
        ("create index on x (a, nope) @sorted", "nope"),
        ("create index on x (a, b) @hash", "@sorted or @unique"),
        ("create index on x (a desc) @sorted", "walked both ways"),
        (
            "create index on x (a, b, a, b, a, b, a, b, a) @sorted",
            "at most 8 fields",
        ),
        ("drop index on x (a, b)", "an index on `x` (a, b)"),
        ("drop index on x (a)", "a rebuild of the field"),
        ("create collection y (a int, @sorted(a))", "one field"),
    ] {
        let e = refused(&mut db, sql).to_string();
        assert!(e.contains(why), "{sql}: {e}");
    }
    exec(&mut db, "create index on x (a, m.k) @sorted", &[]);
    let e = refused(&mut db, "create index on x (a, m.k desc) @unique").to_string();
    assert!(e.contains("exists"), "{e}");
    exec(
        &mut db,
        "create index if not exists on x (a, m.k) @sorted",
        &[],
    );
    exec(&mut db, "drop index if exists on x (b, a)", &[]);
}

/// Built beside the database, with writes landing during the build -- a
/// key's part changed, a row deleted, rows put -- the index comes out as
/// the one built under the write lock, and a unique one is refused for a
/// second holder that came in meanwhile.
#[test]
fn an_index_built_beside_the_database_takes_the_writes_made_meanwhile() {
    use std::sync::RwLock;
    let meanwhile = |db: &mut Database| {
        exec(
            db,
            "set c {created: \"2027-01-01\"} where owner = 2 and title = \"x\"",
            &[],
        );
        exec(db, "del c where owner = 1 and title = \"y\"", &[]);
        exec(
            db,
            "put c {owner: 2, created: \"2024-01-01\", title: \"q\"}",
            &[],
        );
        exec(
            db,
            "set c {owner: 1} where owner = 7 and title = \"z\"",
            &[],
        );
    };
    for ix in INDEXES {
        let (kind, keys) = ix.trim_start_matches('@').split_once('(').unwrap();
        let stmt = fenec_ql::parse_one(&format!("create index on c ({keys} @{kind}")).unwrap();
        let (_, plain) = twins(250);
        let mut start = Database::new();
        start.load(&plain.snapshot()).unwrap();
        let db = RwLock::new(start);
        let r = Database::maintain_with(&db, &stmt, &mut || meanwhile(&mut db.write().unwrap()));
        r.unwrap().unwrap();
        let mut twin = Database::new();
        twin.load(&plain.snapshot()).unwrap();
        meanwhile(&mut twin);
        let db = db.into_inner().unwrap();
        assert_eq!(db.change_seq(), twin.change_seq() + 1, "{ix}");
        check(&db, &twin, &format!("{ix} built beside"));
    }

    let mut db = Database::new();
    exec(&mut db, "create collection t (a int, b int)", &[]);
    exec(&mut db, "put t [{a: 1, b: 2}, {a: 1, b: 3}]", &[]);
    let db = RwLock::new(db);
    let stmt = fenec_ql::parse_one("create index on t (a, b) @unique").unwrap();
    let r = Database::maintain_with(&db, &stmt, &mut || {
        exec(&mut db.write().unwrap(), "put t {a: 1, b: 2}", &[]);
    });
    let e = r.unwrap().unwrap_err().to_string();
    assert!(e.contains("documents 1 and 3 both hold [1, 2]"), "{e}");
    let db = db.into_inner().unwrap();
    assert!(db.collection("t").unwrap().schema.composites.is_empty());
}

/// Made and dropped in a block put back, the index is as it was: gone with
/// the rows written beside it, and back with what it held.
#[test]
fn a_block_put_back_takes_an_index_made_and_brings_one_dropped_back() {
    let (mut ix, plain) = twins(200);
    ix.begin().unwrap();
    exec(
        &mut ix,
        "create index on c (title, owner desc) @sorted",
        &[],
    );
    exec(
        &mut ix,
        "put c {owner: 2, title: \"x\", created: \"2030-01-01\"}",
        &[],
    );
    let p = plan(&ix, "get c where title = \"x\" order owner desc limit 3");
    assert!(p.contains("walked the index on (title, owner desc)"), "{p}");
    ix.rollback();
    let schema = |db: &Database| db.collection("c").unwrap().schema.composites.clone();
    assert_eq!(schema(&ix).len(), INDEXES.len());
    check(&ix, &plain, "an index made in a block put back");

    ix.begin().unwrap();
    exec(&mut ix, "drop index on c (owner, created)", &[]);
    exec(&mut ix, "del c where owner = 2", &[]);
    assert_eq!(schema(&ix).len(), INDEXES.len() - 1);
    ix.rollback();
    assert_eq!(schema(&ix).len(), INDEXES.len());
    assert_eq!(schema(&ix)[0].keys[0].0, "owner");
    let p = plan(&ix, "get c where owner = 2 order created desc limit 3");
    assert!(
        p.contains("walked the index on (owner, created desc)"),
        "{p}"
    );
    check(&ix, &plain, "an index dropped in a block put back");
}

/// A replica applies the records of a primary that makes, drops and keeps
/// indexes over several fields, and answers as it does, through them.
#[test]
fn a_replica_takes_the_indexes_its_primary_makes() {
    use std::sync::{Arc, Mutex};
    #[derive(Clone, Default)]
    struct Tap(Arc<Mutex<Vec<u8>>>);
    impl Sink for Tap {
        fn append(&mut self, _: &[u8]) -> fenec_core::error::Result<()> {
            Ok(())
        }
        fn record(&mut self, _: u64, bytes: &[u8]) -> fenec_core::error::Result<()> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(())
        }
        fn rewrite(&mut self, _: &[u8]) -> fenec_core::error::Result<()> {
            Ok(())
        }
    }
    let tap = Tap::default();
    let mut primary = Database::with_sink(Box::new(tap.clone()));
    exec(
        &mut primary,
        "create collection p (owner int, at int, slug text, @unique(owner, slug))",
        &[],
    );
    for i in 0..300 {
        let put = format!(
            "put p {{owner: {}, at: {}, slug: \"s{}\"}}",
            i % 7,
            i * 3 % 100,
            i
        );
        exec(&mut primary, &put, &[]);
    }
    exec(
        &mut primary,
        "create index on p (owner, at desc) @sorted",
        &[],
    );
    exec(&mut primary, "create index on p (at, slug) @sorted", &[]);
    exec(&mut primary, "drop index on p (at, slug)", &[]);
    exec(&mut primary, "alter collection p rename field at to t", &[]);
    exec(&mut primary, "set p {t: 99} where owner = 3", &[]);
    exec(&mut primary, "del p where owner = 5", &[]);
    let mut replica = Database::new();
    replica.follow(vec![(7, 0)]).unwrap();
    let records = tap.0.lock().unwrap().clone();
    replica.apply(&records).unwrap();
    assert_eq!(replica.change_seq(), primary.change_seq());
    let schema = |db: &Database| db.collection("p").unwrap().schema.composites.clone();
    assert_eq!(schema(&replica), schema(&primary));
    for q in [
        "get p select owner, t, slug where owner = 3 order t desc limit 5",
        "get p select owner, t where owner in [1, 2] order t limit 7",
        "get p where owner = 1 and slug = \"s8\" count",
    ] {
        assert_eq!(rows(&replica, q, &[]), rows(&primary, q, &[]), "{q}");
    }
    let p = plan(&replica, "get p where owner = 3 order t desc limit 5");
    assert!(p.contains("walked the index on (owner, t desc)"), "{p}");
}

/// A schema holding one is written with the composites' tag past its last
/// place, which a binary from before meets as a type it does not know.
#[test]
fn a_binary_from_before_refuses_the_schema() {
    let stmt =
        fenec_ql::parse_one("create collection s (a int, b text, @sorted(a, b desc))").unwrap();
    let Statement::CreateCollection { schema, .. } = stmt else {
        panic!()
    };
    let bytes = schema.encode();
    let at = bytes
        .windows(2)
        .position(|w| w == [0, fenec_core::codec::TAG_COMPOSITES])
        .expect("a nameless place behind the tag");
    assert!(fenec_core::codec::decode_type(&bytes[at + 1..], &mut 0).is_err());
    assert_eq!(
        fenec_core::schema::Schema::decode(&bytes, &mut 0).unwrap(),
        schema
    );
}

/// Over a field whose rows expire, the walk leaves out the rows past their
/// time as the scan does: every read tests the expiry, which no index says.
#[test]
fn rows_past_their_time_are_out_of_the_walk() {
    let mut ix = Database::new();
    let mut plain = Database::new();
    let at = |m: i64| Value::Timestamp(1_760_000_000_000 + m * 60_000);
    for (db, extra) in [(&mut ix, ", @sorted(user, seen desc)"), (&mut plain, "")] {
        db.set_clock(Some(1_760_000_000_000 + 100 * 60_000));
        exec(
            db,
            &format!("create collection s (user int, seen timestamp @ttl(30m){extra})"),
            &[],
        );
        for i in 0..200i64 {
            exec(
                db,
                "put s {user: $1, seen: $2}",
                &[Value::Int(i % 4), at(i % 120)],
            );
        }
    }
    for q in [
        "get s select user, seen where user = 1 order seen desc limit 5",
        "get s select user, seen where user = 2 order seen limit 50",
        "get s where user = 3 count",
        "get s select seen where user in [0, 1] order seen desc limit 7",
    ] {
        assert_eq!(rows(&ix, q, &[]), rows(&plain, q, &[]), "{q}");
    }
    let p = plan(&ix, "get s where user = 1 order seen desc limit 5");
    assert!(
        p.contains("walked the index on (user, seen desc)") && p.contains("tested"),
        "{p}"
    );
}
