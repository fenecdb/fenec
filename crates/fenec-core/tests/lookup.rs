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

/// The bounded selection must never change the answer, only the work.
///
/// `lookup` carries its own `limit`, so it can stop ordering once it has
/// `offset + limit` children -- but a partial order is only allowed if it
/// picks exactly the rows a full one would. Ties are where that breaks, so
/// this deliberately generates a lot of them, and the reference is the same
/// query with the limit lifted above the group.
#[test]
fn a_bounded_child_order_agrees_with_a_full_sort() {
    let mut db = Database::new();
    run(&mut db, "create collection p (name text)");
    run(
        &mut db,
        "create collection c (pid int @hash, k int, tag text)",
    );
    run(&mut db, r#"put p {name: "one"}"#);

    // 400 children over 7 distinct keys: every key is a tie of ~57 rows, so
    // the cut lands inside a tie for most limits.
    let docs: Vec<String> = (0..400)
        .map(|i| format!(r#"{{pid: 1, k: {}, tag: "t{i}"}}"#, i % 7))
        .collect();
    run(&mut db, &format!("put c [{}]", docs.join(",")));

    let ids = |l: Lookup| -> Vec<u64> {
        let (_, n) = nested(&db, &plan_for("p", l));
        n.groups[0].iter().map(|r| r.id).collect()
    };

    for asc in [true, false] {
        let mut full = lookup_c();
        full.order = vec![("k".into(), asc)];
        full.limit = Some(10_000);
        let reference = ids(full);
        assert_eq!(reference.len(), 400);

        for (offset, limit) in [(0, 1), (0, 3), (0, 57), (0, 58), (5, 10), (57, 3), (399, 5)] {
            let mut l = lookup_c();
            l.order = vec![("k".into(), asc)];
            l.offset = offset;
            l.limit = Some(limit);
            let want: Vec<u64> = reference.iter().copied().skip(offset).take(limit).collect();
            assert_eq!(ids(l), want, "asc={asc} offset={offset} limit={limit}");
        }
    }

    // Two keys, the second breaking ties of the first.
    let mut full = lookup_c();
    full.order = vec![("k".into(), false), ("tag".into(), true)];
    full.limit = Some(10_000);
    let reference = ids(full);
    let mut l = lookup_c();
    l.order = vec![("k".into(), false), ("tag".into(), true)];
    l.limit = Some(9);
    assert_eq!(ids(l), reference[..9].to_vec());
}

fn lookup_c() -> Lookup {
    Lookup {
        collection: "c".into(),
        child_field: "pid".into(),
        parent_field: "id".into(),
        ..Default::default()
    }
}

fn plan_for(parent: &str, l: Lookup) -> Select {
    Select {
        collection: parent.into(),
        lookup: Some(l),
        ..Default::default()
    }
}

/// `required` turns the children from something attached into something that
/// decides. The reference is the same query without it, keeping the parents
/// whose group came back non-empty -- the two must agree, or the flag means
/// something other than what it says.
#[test]
fn required_keeps_only_the_parents_a_child_matches() {
    let db = fixture();
    let filtered = |required: bool| -> (Vec<u64>, Vec<usize>) {
        let mut l = lookup("product_id", "id");
        l.filter = Some(Expr::Cmp(
            CmpOp::Ge,
            Box::new(Expr::Field("stars".into())),
            Box::new(Expr::Lit(Value::Int(4))),
        ));
        l.required = required;
        let (parents, n) = nested(&db, &plan(l));
        (
            parents.rows.iter().map(|r| r.id).collect(),
            n.groups.iter().map(|g| g.len()).collect(),
        )
    };

    let (all_ids, all_sizes) = filtered(false);
    let (kept_ids, kept_sizes) = filtered(true);

    let want: Vec<u64> = all_ids
        .iter()
        .zip(&all_sizes)
        .filter(|(_, n)| **n > 0)
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(kept_ids, want);
    assert!(kept_sizes.iter().all(|n| *n > 0));
    assert!(
        all_sizes.iter().any(|n| *n == 0),
        "the fixture must exercise both"
    );
}

/// `required` decides who is on the page, so it has to run before `limit`:
/// asking for two rows must give two parents that have children, not two
/// candidates of which some were dropped afterwards.
#[test]
fn required_fills_the_page_before_limit_applies() {
    let mut db = fixture();
    // Products 2 and 3 have no five-star review; 1 and 4 do.
    run(
        &mut db,
        r#"put products {sku: "e", name: "Fincan", price: 3000}"#,
    );
    run(
        &mut db,
        r#"put reviews {product_id: 5, sku: "e", stars: 5, body: "sade"}"#,
    );

    let mut l = lookup("product_id", "id");
    l.filter = Some(Expr::Cmp(
        CmpOp::Eq,
        Box::new(Expr::Field("stars".into())),
        Box::new(Expr::Lit(Value::Int(5))),
    ));
    l.required = true;
    let mut sel = plan(l);
    sel.limit = Some(2);
    let (parents, n) = nested(&db, &sel);
    assert_eq!(parents.rows.len(), 2, "the page must be full");
    assert_eq!(
        parents.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![1, 4]
    );
    assert!(n.groups.iter().all(|g| !g.is_empty()));
}

/// A child `offset` that skips past every match must not drop the parent: it
/// has matches, the page simply walked past them.
#[test]
fn a_child_offset_does_not_undo_required() {
    let db = fixture();
    let mut l = lookup("product_id", "id");
    l.required = true;
    l.offset = 99;
    let (parents, n) = nested(&db, &plan(l));
    // Products 1, 2 and 4 have reviews; 3 does not.
    assert_eq!(
        parents.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![1, 2, 4]
    );
    assert!(
        n.groups.iter().all(|g| g.is_empty()),
        "offset skipped them all"
    );
}

/// `count` with `required` asks how many parents have a match, which is a
/// question with an answer; without it there is nothing to attach children
/// to and it is refused.
#[test]
fn count_works_with_required_and_is_refused_without() {
    let db = fixture();
    let mut l = lookup("product_id", "id");
    l.required = true;
    let mut sel = plan(l);
    sel.count = true;
    let Response::Rows(rs) = db
        .query(&Statement::Select(sel.clone()), &[])
        .expect("query")
    else {
        panic!("expected rows");
    };
    assert_eq!(rs.rows[0].values[0], Value::Int(3));

    sel.lookup.as_mut().unwrap().required = false;
    let e = err(&db, &sel);
    assert!(e.contains("required"), "{e}");
}

/// `required` has two plans -- walk the parents probing each, or read the
/// children a `@hash` equality names and take their parents. They are not
/// allowed to disagree, whichever the planner picks, so this pins the answer
/// over the cases where they could: a bucket that does not exist, a filter
/// the index only half covers, an equality over a field with no index beside
/// one that has it, NULL keys, dead children, duplicates.
#[test]
fn both_required_plans_give_the_same_answer() {
    let mut db = Database::new();
    run(&mut db, "create collection p (name text)");
    run(
        &mut db,
        "create collection c (pid int @hash, stars int @hash, tag text)",
    );
    run(
        &mut db,
        r#"put p [{name: "a"}, {name: "b"}, {name: "c"}, {name: "d"}]"#,
    );
    run(
        &mut db,
        r#"put c [
             {pid: 1, stars: 5, tag: "x"},
             {pid: 1, stars: 3, tag: "y"},
             {pid: 2, stars: 5, tag: "y"},
             {pid: 3, stars: 1, tag: "x"},
             {pid: 9, stars: 5, tag: "x"},
             {stars: 5, tag: "z"}
           ]"#,
    );
    run(&mut db, "del c where id = 3");
    run(&mut db, r#"put c {id: 1, pid: 1, stars: 5, tag: "x"}"#);

    // The reference is the plan that needs no index at all: keep a parent
    // when the unindexed `lookup` gave it a non-empty group.
    let reference = |db: &Database, filter: Option<Expr>| -> Vec<u64> {
        let mut l = lookup_p();
        l.filter = filter;
        let (parents, n) = nested(db, &plan_for("p", l));
        parents
            .rows
            .iter()
            .zip(&n.groups)
            .filter(|(_, g)| !g.is_empty())
            .map(|(r, _)| r.id)
            .collect()
    };

    for filter in [
        None,
        Some(eq("stars", 5)),
        Some(eq("stars", 1)),
        Some(eq("stars", 99)), // a bucket that does not exist
        Some(Expr::And(
            Box::new(eq("stars", 5)),
            Box::new(Expr::Like(
                Box::new(Expr::Field("tag".into())),
                Box::new(Expr::Lit(Value::Text("x".into()))),
            )),
        )), // the index covers half of it
        Some(Expr::And(
            Box::new(eq("stars", 5)),
            Box::new(Expr::Cmp(
                CmpOp::Eq,
                Box::new(Expr::Field("tag".into())),
                Box::new(Expr::Lit(Value::Text("x".into()))),
            )),
        )), // an equality with no index beside one that has it
        Some(Expr::Cmp(
            CmpOp::Ge,
            Box::new(Expr::Field("stars".into())),
            Box::new(Expr::Lit(Value::Int(3))),
        )), // no equality: no child-side plan exists
    ] {
        let want = reference(&db, filter.clone());
        let mut l = lookup_p();
        l.filter = filter.clone();
        l.required = true;
        let (parents, _) = nested(&db, &plan_for("p", l));
        let got: Vec<u64> = parents.rows.iter().map(|r| r.id).collect();
        assert_eq!(got, want, "{filter:?}");
    }
}

fn lookup_p() -> Lookup {
    Lookup {
        collection: "c".into(),
        child_field: "pid".into(),
        parent_field: "id".into(),
        ..Default::default()
    }
}

fn eq(field: &str, v: i64) -> Expr {
    Expr::Cmp(
        CmpOp::Eq,
        Box::new(Expr::Field(field.into())),
        Box::new(Expr::Lit(Value::Int(v))),
    )
}

/// A named parent key takes the parent-driven plan, because the child-driven
/// one was measured there and lost -- mapping values back means encoding and
/// sorting byte strings rather than integers. It still has to give the same
/// answer as the reference that uses no index, indexed key field or not.
#[test]
fn required_on_a_named_parent_key_matches_the_reference() {
    let mut db = Database::new();
    // `ph` indexes the key field, `pn` does not; same rows, same question.
    run(&mut db, "create collection ph (sku text @hash, name text)");
    run(&mut db, "create collection pn (sku text, name text)");
    run(
        &mut db,
        "create collection rv (sku text @hash, stars int @hash)",
    );
    for c in ["ph", "pn"] {
        run(
            &mut db,
            &format!(
                r#"put {c} [{{sku: "a", name: "1"}}, {{sku: "b", name: "2"}},
                            {{sku: "c", name: "3"}}, {{name: "no key"}}]"#
            ),
        );
    }
    run(
        &mut db,
        r#"put rv [
             {sku: "a", stars: 5}, {sku: "a", stars: 1},
             {sku: "b", stars: 5}, {sku: "c", stars: 2},
             {sku: "zz", stars: 5}, {stars: 5}
           ]"#,
    );

    let named = |coll: &str, required: bool, filter: Option<Expr>| -> Vec<u64> {
        let l = Lookup {
            collection: "rv".into(),
            child_field: "sku".into(),
            parent_field: "sku".into(),
            filter,
            required,
            ..Default::default()
        };
        let (parents, n) = nested(&db, &plan_for(coll, l));
        parents
            .rows
            .iter()
            .zip(&n.groups)
            .filter(|(_, g)| required || !g.is_empty())
            .map(|(r, _)| r.id)
            .collect()
    };

    for filter in [
        None,
        Some(eq("stars", 5)),
        Some(eq("stars", 2)),
        Some(eq("stars", 99)),
        Some(Expr::Cmp(
            CmpOp::Ge,
            Box::new(Expr::Field("stars".into())),
            Box::new(Expr::Lit(Value::Int(2))),
        )),
    ] {
        // The reference: no `required`, keep the parents with a non-empty
        // group. It reads every child and uses no index to decide.
        let want = named("pn", false, filter.clone());
        for coll in ["ph", "pn"] {
            assert_eq!(named(coll, true, filter.clone()), want, "{coll} {filter:?}");
        }
    }
}
