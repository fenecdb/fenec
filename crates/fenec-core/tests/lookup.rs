//! `lookup`: children of another collection, attached per parent row.
//!
//! The clause is sugar over a plan an application can already write — a page
//! query plus one indexed query per row — so the spine of this file is a
//! differential test that says exactly that: the nested result must equal the
//! same result assembled by hand. What it adds on top is a `limit` that counts
//! children *per parent*, which is the thing no join expresses, so that gets
//! its own test rather than being left to the differential one.
//!
//! The single-level plans here are built in Rust rather than parsed, which
//! is also the path `Select::check` exists for; the chained ones are written
//! as queries, because a chain is as much a grammar question as a planner
//! one.

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
    l.order = vec![Sort::new("stars", false)];
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
        full.order = vec![Sort::new("k", asc)];
        full.limit = Some(10_000);
        let reference = ids(full);
        assert_eq!(reference.len(), 400);

        for (offset, limit) in [(0, 1), (0, 3), (0, 57), (0, 58), (5, 10), (57, 3), (399, 5)] {
            let mut l = lookup_c();
            l.order = vec![Sort::new("k", asc)];
            l.offset = offset;
            l.limit = Some(limit);
            let want: Vec<u64> = reference.iter().copied().skip(offset).take(limit).collect();
            assert_eq!(ids(l), want, "asc={asc} offset={offset} limit={limit}");
        }
    }

    // Two keys, the second breaking ties of the first.
    let mut full = lookup_c();
    full.order = vec![Sort::new("k", false), Sort::new("tag", true)];
    full.limit = Some(10_000);
    let reference = ids(full);
    let mut l = lookup_c();
    l.order = vec![Sort::new("k", false), Sort::new("tag", true)];
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
    assert!(all_sizes.contains(&0), "the fixture must exercise both");
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

// ---------------------------------------------------------------- chaining

/// shops 1..3 with 2, 1 and 0 orders; orders 1, 2 and 4 with 2, 1 and 1
/// lines, order 3 with none. Order 4 hangs off a shop that does not exist,
/// so it can only ever be reached through the orphan path.
///
/// The shape is deliberate: the first shop has *two* orders, so the second
/// one's lines sit at index 1 of the flattened child list while sitting at
/// index 0 of their own group. Anything that confuses the two indexes gives
/// order 3 order 1's lines, and every level below inherits the mistake.
fn chain_fixture() -> Database {
    let mut db = Database::new();
    run(&mut db, "create collection shops (name text)");
    run(
        &mut db,
        "create collection orders (shop_id int @hash, code text, buyer_id int)",
    );
    run(
        &mut db,
        "create collection lines (order_id int @hash, item text, qty int)",
    );
    run(&mut db, "create collection buyers (name text)");
    run(
        &mut db,
        r#"put shops [{name: "Merkez"}, {name: "Sube"}, {name: "Depo"}]"#,
    );
    run(
        &mut db,
        r#"put orders [
             {shop_id: 1, code: "A", buyer_id: 1},
             {shop_id: 1, code: "B", buyer_id: 2},
             {shop_id: 2, code: "C", buyer_id: 1},
             {shop_id: 9, code: "Z", buyer_id: 1}
           ]"#,
    );
    run(
        &mut db,
        r#"put lines [
             {order_id: 1, item: "kahve",  qty: 2},
             {order_id: 1, item: "demlik", qty: 1},
             {order_id: 2, item: "kupa",   qty: 5},
             {order_id: 4, item: "filtre", qty: 1}
           ]"#,
    );
    run(&mut db, r#"put buyers [{name: "Ada"}, {name: "Bora"}]"#);
    db
}

/// The ids of a result's rows, for comparing against a query written out by
/// hand.
fn ids(rs: &ResultSet) -> Vec<DocId> {
    rs.rows.iter().map(|r| r.id).collect()
}

/// Same claim as the single-level differential test, one level deeper: a
/// chain must equal the page query plus one indexed query per parent plus
/// one per child. If it ever does not, the clause has stopped being sugar
/// over a plan an application could write itself.
#[test]
fn a_chain_equals_a_query_per_row_at_every_level() {
    let db = chain_fixture();
    let rs = rows(
        &db,
        "get shops lookup orders on shop_id lookup lines on order_id",
    );
    let n = rs.nested.as_ref().expect("orders");
    let below = n.nested.as_deref().expect("lines");

    let parents = rows(&db, "get shops");
    assert_eq!(ids(&rs), ids(&parents));

    // One group of orders per shop, and one group of lines per order --
    // counted over every order the level above emitted, in that order.
    let mut seen = 0;
    for (i, shop) in parents.rows.iter().enumerate() {
        let want = rows(&db, &format!("get orders where shop_id = {}", shop.id));
        assert_eq!(ids_of(n.group(i)), ids(&want), "shop {}", shop.id);
        for order in n.group(i) {
            let kids = rows(&db, &format!("get lines where order_id = {}", order.id));
            assert_eq!(ids_of(below.group(seen)), ids(&kids), "order {}", order.id);
            seen += 1;
        }
    }
    assert_eq!(
        seen,
        below.groups.len(),
        "a group per row of the level above"
    );
}

fn ids_of(rows: &[Row]) -> Vec<DocId> {
    rows.iter().map(|r| r.id).collect()
}

/// The alignment rule in isolation, because getting it wrong is silent: a
/// level's groups are keyed by the position of the owning row among *all*
/// the rows above it, not by its position inside its own group.
///
/// Shop 2's only order is the third order overall, so a level that keyed by
/// the within-group index would hand it the first order's lines and look
/// entirely plausible doing it.
#[test]
fn a_level_is_keyed_by_the_row_above_across_groups() {
    let db = chain_fixture();
    let rs = rows(
        &db,
        "get shops lookup orders on shop_id lookup lines on order_id",
    );
    let n = rs.nested.as_ref().unwrap();
    let below = n.nested.as_deref().unwrap();

    // shop 1 -> orders 1, 2; shop 2 -> order 3; shop 3 -> none.
    assert_eq!(ids_of(n.group(0)), vec![1, 2]);
    assert_eq!(ids_of(n.group(1)), vec![3]);
    assert!(n.group(2).is_empty());

    // Flattened, the orders are [1, 2, 3], so the line groups are theirs in
    // that order: two, one, none.
    assert_eq!(ids_of(below.group(0)), vec![1, 2]);
    assert_eq!(ids_of(below.group(1)), vec![3]);
    assert!(
        below.group(2).is_empty(),
        "order 3 has no lines; it must not inherit another order's"
    );
    assert_eq!(below.groups.len(), 3);
}

/// `limit` counts children per parent at every level, which is the whole
/// reason the clause exists -- and the reason a chain cannot be rendered as
/// a chain of joins and left at that.
#[test]
fn limit_counts_per_parent_at_every_level() {
    let db = chain_fixture();
    let rs = rows(
        &db,
        "get shops lookup orders on shop_id limit 1 lookup lines on order_id limit 1",
    );
    let n = rs.nested.as_ref().unwrap();
    let below = n.nested.as_deref().unwrap();
    // One order per shop, not one order in total.
    assert_eq!(ids_of(n.group(0)), vec![1]);
    assert_eq!(ids_of(n.group(1)), vec![3]);
    // And one line per order, over the orders that survived the limit above.
    assert_eq!(below.groups.len(), 2);
    assert_eq!(ids_of(below.group(0)), vec![1]);
    assert!(below.group(1).is_empty());
}

/// `required` is a statement about its own level: it drops rows of the level
/// immediately above it and stops there.
///
/// Requiring lines drops the *orders* that have none. It must not also drop
/// the shops those orders belonged to -- that is what requiring orders says,
/// and it was not said here. A shop left with nothing keeps its row and an
/// empty group, exactly as a childless parent always has.
#[test]
fn required_below_drops_the_row_above_and_stops_there() {
    let db = chain_fixture();
    let rs = rows(
        &db,
        "get shops lookup orders on shop_id lookup lines on order_id required",
    );
    let n = rs.nested.as_ref().unwrap();

    assert_eq!(ids(&rs), vec![1, 2, 3], "every shop keeps its row");
    // Order 3 has no lines, so shop 2's group empties -- but shop 2 stays.
    assert_eq!(ids_of(n.group(0)), vec![1, 2]);
    assert!(n.group(1).is_empty(), "order 3 has no lines");
    assert!(n.group(2).is_empty(), "shop 3 never had an order");
}

/// Said at both levels, it composes: the shops that survive are the ones
/// with an order that has a line. The reference is the same query without
/// the flags, keeping the parents whose group came back non-empty -- the
/// same reference the single-level test uses, applied twice.
#[test]
fn required_composes_down_the_chain() {
    let db = chain_fixture();
    let rs = rows(
        &db,
        "get shops lookup orders on shop_id required lookup lines on order_id required",
    );
    assert_eq!(ids(&rs), vec![1], "only shop 1 has an order with a line");

    let loose = rows(
        &db,
        "get shops lookup orders on shop_id lookup lines on order_id required",
    );
    let n = loose.nested.as_ref().unwrap();
    let want: Vec<DocId> = loose
        .rows
        .iter()
        .enumerate()
        .filter(|(i, _)| !n.group(*i).is_empty())
        .map(|(_, r)| r.id)
        .collect();
    assert_eq!(ids(&rs), want);
}

/// `count` with `required` at the top asks how many parents have a match,
/// and a `required` level further down is part of what "a match" means.
#[test]
fn count_sees_the_whole_required_chain() {
    let db = chain_fixture();
    let rs = rows(
        &db,
        "get shops count lookup orders on shop_id required lookup lines on order_id required",
    );
    assert_eq!(rs.rows[0].values[0], Value::Int(1));
}

/// The key of a deeper level comes out of the store, not out of what the
/// level above projected: `select code` on the orders must not take
/// `buyer_id` away from the level below it.
#[test]
fn a_deeper_level_reads_its_key_even_when_the_level_above_hid_it() {
    let db = chain_fixture();
    let rs = rows(
        &db,
        "get shops lookup orders on shop_id select code lookup buyers on id = buyer_id",
    );
    let n = rs.nested.as_ref().unwrap();
    assert_eq!(n.columns, vec!["code".to_string()]);
    let below = n.nested.as_deref().unwrap();
    // Orders 1, 2, 3 were bought by 1, 2, 1.
    assert_eq!(ids_of(below.group(0)), vec![1]);
    assert_eq!(ids_of(below.group(1)), vec![2]);
    assert_eq!(ids_of(below.group(2)), vec![1]);
}

/// Flattened, a chain is one row per root-to-leaf path, and a level that ran
/// out fills its own columns and every column below with nulls. That is what
/// the PostgreSQL wire and the terminal table get; the per-level `limit` is
/// the part only the nested transports keep.
#[test]
fn a_chain_flattens_to_one_row_per_path() {
    let db = chain_fixture();
    let rs = rows(
        &db,
        "get shops select name lookup orders on shop_id select code lookup lines on order_id select item",
    );
    let flat = rs.flatten();
    assert_eq!(flat.columns, vec!["name", "orders.code", "lines.item"]);

    let cell = |r: &Row, i: usize| match &r.values[i] {
        Value::Text(s) => s.clone(),
        Value::Null => "-".to_string(),
        v => panic!("unexpected {v:?}"),
    };
    let table: Vec<(String, String, String)> = flat
        .rows
        .iter()
        .map(|r| (cell(r, 0), cell(r, 1), cell(r, 2)))
        .collect();
    assert_eq!(
        table,
        vec![
            ("Merkez".into(), "A".into(), "kahve".into()),
            ("Merkez".into(), "A".into(), "demlik".into()),
            ("Merkez".into(), "B".into(), "kupa".into()),
            // Order C has no lines, so the leaf columns go null.
            ("Sube".into(), "C".into(), "-".into()),
            // Depo has no orders at all: both blocks below it go null.
            ("Depo".into(), "-".into(), "-".into()),
        ]
    );
}

/// JSON nests all the way down, and a row with nothing under it still gets
/// the key with an empty array -- a missing one would read as "not asked
/// for" rather than "nothing matched".
#[test]
fn a_chain_nests_all_the_way_down_in_json() {
    let db = chain_fixture();
    let rs = rows(
        &db,
        "get shops select name lookup orders on shop_id select code lookup lines on order_id select item",
    );
    let mut out = String::new();
    fenec_core::json::rows_array_into(&mut out, &rs);
    assert_eq!(
        out,
        r#"[{"name":"Merkez","orders":[{"code":"A","lines":[{"item":"kahve"},{"item":"demlik"}]},{"code":"B","lines":[{"item":"kupa"}]}]},{"name":"Sube","orders":[{"code":"C","lines":[]}]},{"name":"Depo","orders":[]}]"#
    );
}

/// The refusals a chain adds. A collection may appear once in a query: put
/// it back in scope two levels down and `on child = parent` reaches a level
/// that could be either of them, which is the ambiguity the single-level
/// self-lookup rule already refuses. The depth cap is a bound on the stack
/// -- the parser, `check` and the engine all recurse per level -- and it is
/// an error rather than a truncation, because a chain quietly cut short is a
/// wrong answer believed right.
#[test]
fn refuses_a_repeated_collection_and_a_chain_too_deep() {
    let db = chain_fixture();
    let bad = |sql: &str| -> String {
        let stmt = match fenec_ql::parse_one(sql) {
            Ok(s) => s,
            Err(e) => return e.to_string(),
        };
        db.query(&stmt, &[]).expect_err("must refuse").to_string()
    };

    let e = bad("get shops lookup orders on shop_id lookup shops on id = shop_id");
    assert!(e.contains("shops") && e.contains("look itself up"), "{e}");

    let e = bad("get orders lookup lines on order_id lookup lines on order_id");
    assert!(e.contains("lines") && e.contains("look itself up"), "{e}");

    let deep = "get shops".to_string() + &" lookup orders on shop_id".repeat(64);
    let e = bad(&deep);
    assert!(e.contains("too deep"), "{e}");
}
