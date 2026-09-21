//! `lookup`: children of another collection, attached per parent row.
//!
//! The clause is sugar over a plan an application can already write — a page
//! query plus one indexed query per row — so the spine of this file is a
//! differential test that says exactly that: the nested result must equal the
//! same result assembled by hand. What it adds on top is a `limit` that counts
//! children *per parent*, which is the thing no join expresses, so that gets
//! its own test rather than being left to the differential one.
//!
//! The parser does not speak `lookup` yet, so the plans here are built in
//! Rust. That is also the path `Select::check` exists for.

use fenec_core::prelude::*;

fn run(db: &mut Database, sql: &str) {
    for stmt in fenec_ql::parse(sql).expect("parse") {
        db.execute(&stmt).expect("execute");
    }
}

fn rows(db: &Database, sql: &str) -> ResultSet {
    let stmt = fenec_ql::parse_one(sql).expect("parse");
    let Response::Rows(rs) = db.query(&stmt, &[]).expect("query") else {
        panic!("expected rows");
    };
    rs
}

/// products 1..4 with 3, 1, 0 and 2 reviews; one review points at no product
/// and one product carries a NULL key.
fn fixture() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection products (sku text @hash, name text, price int)",
    );
    run(
        &mut db,
        "create collection reviews (product_id int @hash, sku text @hash, stars int, body text)",
    );
    run(
        &mut db,
        r#"put products [
             {sku: "a", name: "Kahve",  price: 12000},
             {sku: "b", name: "Demlik", price: 34000},
             {sku: "c", name: "Kupa",   price:  6000},
             {sku: "d", name: "Filtre", price:  4500}
           ]"#,
    );
    run(
        &mut db,
        r#"put reviews [
             {product_id: 1, sku: "a", stars: 5, body: "guzel"},
             {product_id: 1, sku: "a", stars: 3, body: "idare eder"},
             {product_id: 1, sku: "a", stars: 4, body: "hizli kargo"},
             {product_id: 2, sku: "b", stars: 2, body: "kirik geldi"},
             {product_id: 4, sku: "d", stars: 5, body: "tavsiye"},
             {product_id: 4, sku: "d", stars: 1, body: "gelmedi"},
             {product_id: 9, sku: "z", stars: 5, body: "oksuz"}
           ]"#,
    );
    db
}

fn lookup(child_field: &str, parent_field: &str) -> Lookup {
    Lookup {
        collection: "reviews".into(),
        child_field: child_field.into(),
        parent_field: parent_field.into(),
        ..Default::default()
    }
}

fn plan(l: Lookup) -> Select {
    Select {
        collection: "products".into(),
        lookup: Some(l),
        ..Default::default()
    }
}

fn nested(db: &Database, sel: &Select) -> (ResultSet, Nested) {
    let Response::Rows(rs) = db
        .query(&Statement::Select(sel.clone()), &[])
        .expect("query")
    else {
        panic!("expected rows");
    };
    let n = rs.nested.clone().expect("nested");
    (rs, n)
}

/// The clause must be nothing but sugar: whatever it returns, the same thing
/// has to come out of one query for the parents plus one indexed query per
/// parent, merged by hand. That is the plan it replaces, and the only claim
/// it makes over it is round trips.
#[test]
fn lookup_equals_a_page_query_plus_one_query_per_row() {
    let db = fixture();
    let (parents, n) = nested(&db, &plan(lookup("product_id", "id")));

    let by_hand = rows(&db, "get products");
    assert_eq!(parents.rows, by_hand.rows);
    assert_eq!(n.groups.len(), by_hand.rows.len());

    for (row, group) in by_hand.rows.iter().zip(&n.groups) {
        let want = rows(&db, &format!("get reviews where product_id = {}", row.id));
        assert_eq!(n.columns, want.columns, "columns for product {}", row.id);
        assert_eq!(group, &want.rows, "children of product {}", row.id);
    }
}

/// The same relation reached through `id` and through a `@hash` text field
/// must give the same children. An index is an accelerator; it is never
/// allowed to be the reason an answer differs.
#[test]
fn the_key_can_be_an_id_or_a_hash_field() {
    let db = fixture();
    let (_, by_id) = nested(&db, &plan(lookup("product_id", "id")));
    let (_, by_sku) = nested(&db, &plan(lookup("sku", "sku")));
    assert_eq!(by_id.groups, by_sku.groups);
}

/// A parent with no children keeps its row and gets an empty group. This is
/// the visible difference from an inner join, and it is the behaviour the
/// whole clause is built around: the page is the parents, always.
#[test]
fn a_childless_parent_keeps_its_row() {
    let db = fixture();
    let (parents, n) = nested(&db, &plan(lookup("product_id", "id")));
    assert_eq!(parents.rows.len(), 4);
    assert_eq!(
        n.groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
        vec![3, 1, 0, 2]
    );
}

/// `limit` counts children per parent. A join's limit counts pairs, so one
/// parent with many children consumes the whole page -- the failure this
/// clause exists to make impossible.
#[test]
fn limit_and_offset_are_per_parent() {
    let db = fixture();
    let mut l = lookup("product_id", "id");
    l.limit = Some(2);
    let (_, n) = nested(&db, &plan(l));
    assert_eq!(
        n.groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
        vec![2, 1, 0, 2]
    );

    let mut l = lookup("product_id", "id");
    l.offset = 1;
    l.limit = Some(1);
    let (_, n) = nested(&db, &plan(l));
    assert_eq!(
        n.groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
        vec![1, 0, 0, 1]
    );
    // Skipping one of product 1's three reviews leaves the second.
    assert_eq!(n.groups[0][0].id, 2);
}

/// A child `where` filters the group, not the page: a parent whose children
/// all fail it keeps its row with nothing under it.
#[test]
fn a_child_filter_empties_a_group_without_dropping_the_parent() {
    let db = fixture();
    let mut l = lookup("product_id", "id");
    l.filter = Some(Expr::Cmp(
        CmpOp::Ge,
        Box::new(Expr::Field("stars".into())),
        Box::new(Expr::Lit(Value::Int(4))),
    ));
    let (parents, n) = nested(&db, &plan(l));
    assert_eq!(parents.rows.len(), 4);
    assert_eq!(
        n.groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
        vec![2, 0, 0, 1]
    );
}

/// Child ordering, against a full sort of the same group.
#[test]
fn children_can_be_ordered() {
    let db = fixture();
    let mut l = lookup("product_id", "id");
    l.order = vec![("stars".into(), false)];
    let (_, n) = nested(&db, &plan(l));

    let stars = |g: &Vec<Row>| {
        g.iter()
            .map(|r| match &r.values[3] {
                Value::Int(i) => *i,
                v => panic!("stars was {v:?}"),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(stars(&n.groups[0]), vec![5, 4, 3]);
    assert_eq!(stars(&n.groups[3]), vec![5, 1]);
}

/// A NULL key matches nothing, deliberately unlike `eval`, where
/// `NULL = NULL` is true. Without the rule every document missing the key
/// would attach to every other one.
#[test]
fn a_null_key_never_matches() {
    let mut db = fixture();
    run(&mut db, r#"put products {name: "Kayip", price: 100}"#);
    run(&mut db, r#"put reviews {stars: 5, body: "sahipsiz"}"#);

    let (parents, n) = nested(&db, &plan(lookup("sku", "sku")));
    assert_eq!(parents.rows.len(), 5);
    // The NULL-keyed product gets nothing, and the NULL-keyed review is not
    // handed to anybody.
    assert!(n.groups[4].is_empty());
    for g in &n.groups {
        assert!(
            g.iter().all(|r| r.values[2] != Value::Null),
            "a null-keyed review was attached"
        );
    }
}

/// A deleted child must leave no trace: a hash bucket can still hold its id.
#[test]
fn a_deleted_child_leaves_no_row_behind() {
    let mut db = fixture();
    run(&mut db, "del reviews where id = 2");
    let (_, n) = nested(&db, &plan(lookup("product_id", "id")));
    assert_eq!(
        n.groups[0].iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![1, 3]
    );
}

/// An upsert removes the id from its bucket and pushes it back, so the same
/// child must still appear exactly once and the order must not drift.
#[test]
fn an_upserted_child_appears_once() {
    let mut db = fixture();
    run(
        &mut db,
        r#"put reviews {id: 1, product_id: 1, sku: "a", stars: 5, body: "duzeltildi"}"#,
    );
    let (_, n) = nested(&db, &plan(lookup("product_id", "id")));
    assert_eq!(
        n.groups[0].iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

/// The refusals. Each one is a case where answering would mean inventing a
/// meaning: a rank over a parent plus its children, a count of rows the
/// children hang from, a probe with no bucket to probe, or a key comparison
/// whose two rulebooks disagree.
#[test]
fn refuses_what_it_cannot_answer() {
    let mut db = fixture();
    run(
        &mut db,
        "create collection plain (product_id int, note text)",
    );

    let mut sel = plan(lookup("product_id", "id"));
    sel.count = true;
    assert!(err(&db, &sel).contains("count"));

    let mut sel = plan(lookup("product_id", "id"));
    sel.lookup.as_mut().unwrap().collection = "products".into();
    assert!(err(&db, &sel).contains("itself"));

    // No `@hash` on the child key: the message has to name the fix.
    let mut sel = plan(lookup("product_id", "id"));
    sel.lookup.as_mut().unwrap().collection = "plain".into();
    let e = err(&db, &sel);
    assert!(e.contains("@hash"), "{e}");

    // `text` parent key against an `int` child key.
    let e = err(&db, &plan(lookup("product_id", "sku")));
    assert!(e.contains("do not match"), "{e}");

    // Fields that are not there.
    assert!(err(&db, &plan(lookup("nope", "id"))).contains("nope"));
    assert!(err(&db, &plan(lookup("product_id", "nope"))).contains("nope"));
}

fn err(db: &Database, sel: &Select) -> String {
    db.query(&Statement::Select(sel.clone()), &[])
        .expect_err("should have been refused")
        .to_string()
}

/// The children have to survive a round trip through the file: they are read
/// back out of the same store and the same hash index the rebuild produces.
#[test]
fn lookup_works_after_a_reload() {
    let db = fixture();
    let (_, before) = nested(&db, &plan(lookup("product_id", "id")));

    let mut fresh = Database::new();
    fresh.load(&db.snapshot()).expect("load");
    let (_, after) = nested(&fresh, &plan(lookup("product_id", "id")));

    assert_eq!(before.groups, after.groups);
    assert_eq!(before.columns, after.columns);
}
