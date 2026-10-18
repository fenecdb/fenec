//! SQL's spellings, read by the one parser into the statements FenecQL
//! writes: each held to its FenecQL by the `Statement` both parse to, run
//! against a database, and refused where it has no exact meaning here.

use fenec_core::prelude::*;
use fenec_ql::{parse, parse_one};

/// Each SQL text and the FenecQL it is: the same statement, to the last
/// expression.
const SAME: &[(&str, &str)] = &[
    // INSERT
    (
        "INSERT INTO t (a, b) VALUES (1, 'x')",
        r#"insert t {a: 1, b: "x"}"#,
    ),
    (
        "insert t (a, b) values (1, 'x'), ($1, $2)",
        r#"insert t [{a: 1, b: "x"}, {a: $1, b: $2}]"#,
    ),
    (
        r#"INSERT INTO "t" ("a", meta) VALUES (now(), {k: 'v'}) RETURNING id, a"#,
        r#"insert t {a: now(), meta: {k: "v"}} returning id, a"#,
    ),
    (
        "INSERT INTO t (e, n) VALUES ('a@x', 1) ON CONFLICT DO NOTHING RETURNING *",
        r#"insert t {e: "a@x", n: 1} if absent returning *"#,
    ),
    (
        "INSERT INTO t (e, n) VALUES ('a@x', 1) ON CONFLICT DO UPDATE SET n = t.n + EXCLUDED.n, \
         seen = now()",
        r#"insert t {e: "a@x", n: 1} if absent else set {n: n + new.n, seen: now()}"#,
    ),
    (
        "insert into t (n) values (1) on conflict do nothing require 1",
        "insert t {n: 1} if absent require 1",
    ),
    // UPDATE
    (
        "UPDATE t SET a = 5, b = b + 1 WHERE c = 'x'",
        r#"set t {a: 5, b: b + 1} where c = "x""#,
    ),
    (
        "update t set owner = $1, run_at = now() + 30000 where run_at <= now() order by run_at \
         limit 1 returning *",
        "set t {owner: $1, run_at: now() + 30000} where run_at <= now() order run_at limit 1 \
         returning *",
    ),
    ("UPDATE t SET meta.k = 1", "set t {meta.k: 1}"),
    // IN
    ("get t where x IN (1, 2, 3)", "get t where x in [1, 2, 3]"),
    ("get t where x in ()", "get t where x in []"),
    (
        "get t where x NOT IN ($1, 'b')",
        r#"get t where not (x in [$1, "b"])"#,
    ),
    (
        "SELECT a FROM t WHERE x IN (SELECT f FROM c WHERE g = 1)",
        "get t select a where x in (get c select f where g = 1)",
    ),
    (
        "get t where x not in (get c select f)",
        "get t where not (x in (get c select f))",
    ),
    // BETWEEN, its `and` its own
    (
        "get t where x BETWEEN 1 AND 3",
        "get t where x >= 1 and x <= 3",
    ),
    (
        "get t where x between 1 and 3 and y = 2",
        "get t where (x >= 1 and x <= 3) and y = 2",
    ),
    (
        "get t where a = 1 or x between $1 and $2 + 1 and y = 2",
        "get t where a = 1 or ((x >= $1 and x <= $2 + 1) and y = 2)",
    ),
    (
        "get t where x NOT BETWEEN 1 AND 3",
        "get t where not (x >= 1 and x <= 3)",
    ),
    (
        "get t where not x between 1 and 3",
        "get t where not (x >= 1 and x <= 3)",
    ),
    // LIKE, a call
    (
        "get t where name LIKE 'A%'",
        r#"get t where like(name, "A%")"#,
    ),
    (
        "get t where name not ilike '%!_%' escape '!'",
        r#"get t where not ilike(name, "%!_%", "!")"#,
    ),
    // Text
    ("get t where n = 'it''s'", r#"get t where n = "it's""#),
    ("get t where n = ''", r#"get t where n = """#),
    (
        "get t select a || ' ' || b as ab",
        r#"get t select a + " " + b as ab"#,
    ),
    // Names quoted where only a name stands
    (r#"SELECT * FROM "users""#, "get users"),
    (
        r#"DELETE FROM "users" WHERE id = 1"#,
        "del users where id = 1",
    ),
    (
        r#"UPDATE "users" SET "n" = 1 RETURNING "n""#,
        "set users {n: 1} returning n",
    ),
    (r#"get t facet "b""#, "get t facet b"),
    // Rows as objects, the brackets left out
    ("put t {a: 1}, {a: 2}", "put t [{a: 1}, {a: 2}]"),
    (
        "insert into t values {a: 1}, {a: 'x'}",
        r#"insert t [{a: 1}, {a: "x"}]"#,
    ),
    // SQL's `is`
    (
        "get t where a is distinct from $1 and b is not distinct from 2",
        "get t where a != $1 and b = 2",
    ),
    ("get t where done is true", "get t where done = true"),
    (
        "get t where done is not false",
        "get t where not (done = false)",
    ),
    (
        "get t where a is not null order a desc nulls last, b nulls first",
        "get t where a is not null order a desc, b",
    ),
    // A `~` of text that is no pattern
    (
        "get t where email ~ '@example.org'",
        r#"get t where email ~ "@example.org""#,
    ),
    (
        r#"create index on t ("a") @hash"#,
        "create index on t (a) @hash",
    ),
];

#[test]
fn sql_spellings_parse_to_the_fenecql_they_mean() {
    for (sql, fenecql) in SAME {
        let a = parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let b = parse_one(fenecql).unwrap_or_else(|e| panic!("{fenecql}: {e}"));
        assert_eq!(a, b, "{sql}");
    }
}

/// Each text refused, and what the refusal says.
const REFUSED: &[(&str, &str)] = &[
    // An insert that cannot say which field a value is
    ("insert into t values (1)", "an insert names its fields"),
    (
        "insert into t (a, b) values (1)",
        "a row of 1 values for 2 fields",
    ),
    (
        "insert into t (a) values (1), (2, 3)",
        "a row of 2 values for 1 fields",
    ),
    (
        "insert into t (a) select a from u",
        "not a query's rows: read them with `get`",
    ),
    (
        "insert into t select * from u",
        "not a query's rows: read them with `get`",
    ),
    // An upsert that does not map exactly
    (
        "insert into t (e) values (1) on conflict on constraint t_e do nothing",
        "an upsert names its unique field, or none",
    ),
    (
        "insert into t (e) values (1) on conflict do update set n = 1 where n < 3",
        "with no `where`",
    ),
    // The double-quoted name, text here
    (
        r#"select * from t where "name" = 'a'"#,
        r#"`name`, not "name": double quotes make text here, the same in every row"#,
    ),
    (r#"get t where "n" > $1"#, r#"`n`, not "n""#),
    (r#"get t where 'a' = "n""#, r#"`n`, not "n""#),
    (r#"get t where "n" = "m""#, r#"`n`, not "n""#),
    (r#"get t where "n" is not null"#, r#"`n`, not "n""#),
    (r#"get t where "n" in (1, 2)"#, r#"`n`, not "n""#),
    (r#"get t where "n" not in [$1]"#, r#"`n`, not "n""#),
    (r#"get t where "n" in (get u select m)"#, r#"`n`, not "n""#),
    (r#"get t where "n" like 'a%'"#, r#"`n`, not "n""#),
    (r#"get t where "n" between 1 and 2"#, r#"`n`, not "n""#),
    (r#"get t where "n" ~ 'a'"#, r#"`n`, not "n""#),
    (r#"get t where "n" has 'a'"#, r#"`n`, not "n""#),
    (r#"select "name" from t"#, r#"`name`, not "name""#),
    (r#"select "name" as n from t"#, r#"`name`, not "name""#),
    (r#"get t select a, "b""#, r#"`b`, not "b""#),
    (r#"get t order by "a" desc"#, r#"`a`, not "a""#),
    (r#"get t select count(*) group "a""#, r#"`a`, not "a""#),
    (r#"select max("at") from t"#, r#"`at`, not "at""#),
    (r#"select count(distinct "u") from t"#, r#"`u`, not "u""#),
    // SQL with no statement of its own here
    (
        "CREATE TABLE t (a int)",
        "FenecQL writes `create collection`",
    ),
    ("DROP TABLE t", "FenecQL writes `drop collection`"),
    (
        "ALTER TABLE t ADD COLUMN a int",
        "FenecQL writes `alter collection`",
    ),
    ("CREATE INDEX t_a ON t (a)", "an index is `create index on"),
    (
        "CREATE UNIQUE INDEX t_a ON t (a)",
        "an index is `create index on",
    ),
    ("create index on t (a)", "it names its kind"),
    (
        "SELECT * FROM a JOIN b ON a.id = b.a_id",
        "FenecQL has no `join`: `get orders where ... lookup items on order_id`",
    ),
    (
        "SELECT * FROM a x JOIN b y ON x.id = y.a_id",
        "FenecQL has no `join`: `get orders where ... lookup items on order_id`",
    ),
    (
        "SELECT * FROM users u WHERE u.id = 1",
        "unknown command `u`: an alias",
    ),
    (
        "SELECT * FROM items JOIN orders ON orders.id = items.order_id",
        "`get items where ... lookup orders on id = order_id`",
    ),
    (
        "SELECT * FROM users WHERE EXISTS (SELECT 1 FROM orders WHERE user_id = users.id)",
        "`get users lookup orders on user_id required`",
    ),
    (
        "SELECT * FROM t WHERE at = (SELECT max(at) FROM t)",
        "a query stands after `in`",
    ),
    (
        "SELECT * FROM docs WHERE MATCH(body) AGAINST ('rust')",
        "`get docs where ... match body $1`",
    ),
    (
        "SELECT * FROM docs WHERE to_tsvector(body) @@ to_tsquery('rust')",
        "full text is the `match` clause",
    ),
    (
        r"SELECT * FROM users WHERE email ~ '@example\.org$'",
        "not a regular expression",
    ),
    ("get users where name ~ '^Ad'", "not a regular expression"),
    ("get users where name ~ 'A.*a'", "not a regular expression"),
    (
        "SELECT DISTINCT city FROM users",
        "FenecQL has no `distinct`",
    ),
    (
        "get t where lower(a) between 'a' and 'm'",
        "`between` takes a field",
    ),
    ("get t order a nulls last", "a null comes first ascending"),
    ("TRUNCATE t", "`del <collection>`"),
    ("BEGIN", "a `/batch` of statements"),
    ("COMMIT", "a `/batch` of statements"),
];

#[test]
fn what_has_no_exact_meaning_is_refused_saying_what_to_write() {
    for (sql, says) in REFUSED {
        let e = match parse(sql) {
            Ok(s) => panic!("{sql} parsed: {s:?}"),
            Err(e) => e.to_string(),
        };
        assert!(e.contains(says), "{sql}: {e}");
    }
}

/// Where a double-quoted text is no column in SQL either, or stands beside
/// a field, it stays text: FenecQL as it was written before.
#[test]
fn double_quoted_text_beside_a_field_stays_text() {
    for q in [
        r#"get t where name = "a""#,
        r#"get t where "a" = name"#,
        r#"get t where $1 = "admin" or owner = $2"#,
        r#"get t where tags has "ai" and title ~ "rust""#,
        r#"get t where name in ["a", "b"]"#,
        r#"get t where name like "a%""#,
        "get t where 'a' = 'a'",
        "get t where 1 = 1",
        r#"get t select coalesce(n, "none") as n, bucket(at, 1d, "Europe/Berlin") as day"#,
        r#"get t select 'x' as k"#,
        r#"get t match body "rust" limit 5"#,
        r#"put t {a: "x"}"#,
        r#"set t {a: "x"} where b = "y""#,
        r#"get t select highlight(body, "<b>", "</b>") match body "x""#,
    ] {
        parse(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

fn run(db: &mut Database, q: &str) -> Response {
    let mut last = Response::Affected(0);
    for s in parse(q).unwrap_or_else(|e| panic!("{q}: {e}")) {
        last = db
            .execute_with(&s, &[])
            .unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    last
}

fn texts(db: &mut Database, q: &str) -> Vec<String> {
    let Response::Rows(rs) = run(db, q) else {
        panic!("{q}: no rows")
    };
    rs.rows
        .iter()
        .map(|r| match &r.values[0] {
            Value::Text(t) => t.clone(),
            v => fenec_core::json::to_string(v),
        })
        .collect()
}

fn people() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection p (name text, email text nocase @unique, city text collate tr, \
         age int @sorted, n int)",
    );
    run(
        &mut db,
        "INSERT INTO p (name, email, city, age) VALUES \
         ('Ada', 'Ada@X.com', 'İzmir', 36), \
         ('Bob', 'bob@x.com', 'Ankara', 25), \
         ('O''Brien', 'ob@x.com', 'istanbul', 52), \
         ('Straße', 'st@x.com', null, null), \
         ('50% off', 'off@x.com', 'Ankara', 7), \
         ('naïve_x', 'nv@x.com', 'Iğdır', 19)",
    );
    db
}

/// `like` against what SQL answers: `%` any run, `_` one character -- of
/// several bytes too -- an escape named, a `nocase` field as its `=`, a
/// collated one by its bytes, `ilike` by the case fold.
#[test]
fn like_matches_as_sql_reads_a_pattern() {
    let mut db = people();
    let names = |db: &mut Database, w: &str| {
        texts(db, &format!("SELECT name FROM p WHERE {w} ORDER BY id"))
    };
    assert_eq!(names(&mut db, "name LIKE 'A%'"), ["Ada"]);
    assert_eq!(names(&mut db, "name LIKE 'a%'"), Vec::<String>::new());
    assert_eq!(names(&mut db, "name ILIKE 'a%'"), ["Ada"]);
    assert_eq!(names(&mut db, "name LIKE '_o_'"), ["Bob"]);
    assert_eq!(
        names(&mut db, "name LIKE 'na_ve!_x' ESCAPE '!'"),
        ["naïve_x"]
    );
    assert_eq!(names(&mut db, "name LIKE 'na_ve_x'"), ["naïve_x"]);
    assert_eq!(names(&mut db, "name LIKE '%!%%' ESCAPE '!'"), ["50% off"]);
    assert_eq!(
        names(&mut db, "name LIKE '%\\\\%%' ESCAPE '\\\\'"),
        ["50% off"]
    );
    assert_eq!(names(&mut db, "name LIKE 'O''%'"), ["O'Brien"]);
    assert_eq!(names(&mut db, "name LIKE '%'"), names(&mut db, "true"));
    assert_eq!(names(&mut db, "name LIKE '%e%' AND age > 20"), ["O'Brien"]);
    // `ilike` folds: `ß` is `ss`.
    assert_eq!(names(&mut db, "name ILIKE 'STRASSE'"), ["Straße"]);
    // A `nocase` field compares by its fold, as its `=` does.
    assert_eq!(names(&mut db, "email LIKE 'ada@%'"), ["Ada"]);
    assert_eq!(names(&mut db, "email = 'ADA@x.COM'"), ["Ada"]);
    // A collated field by its bytes, as its `=` does: `İ` is not `i`.
    assert_eq!(names(&mut db, "city LIKE 'i%'"), ["O'Brien"]);
    assert_eq!(names(&mut db, "city = 'izmir'"), Vec::<String>::new());
    // A null is no text: `like` leaves it out, `not like` -- `not` over a
    // test FenecQL answers false -- takes it in.
    assert_eq!(names(&mut db, "city LIKE '%'").len(), 5);
    assert_eq!(names(&mut db, "city NOT LIKE '%'"), ["Straße"]);
    // In a select list too, a null for a null.
    let Response::Rows(rs) = run(&mut db, "SELECT city LIKE 'A%' AS a FROM p ORDER BY id") else {
        panic!()
    };
    let got: Vec<Value> = rs.rows.iter().map(|r| r.values[0].clone()).collect();
    use Value::{Bool, Null};
    assert_eq!(
        got,
        [
            Bool(false),
            Bool(true),
            Bool(false),
            Null,
            Bool(true),
            Bool(false)
        ]
    );
}

#[test]
fn like_refuses_what_it_cannot_read() {
    let mut db = people();
    for (q, says) in [
        ("get p where age like '1%'", "`like` takes text, not int"),
        (
            "get p where name like 'a!' escape '!'",
            "ends in its escape",
        ),
        (
            "get p where name like 'a' escape '!!'",
            "takes one character",
        ),
        ("get p where name ilike 'a' escape 'X'", "with no case"),
        ("get p where email like 'a' escape 'X'", "with no case"),
    ] {
        let st = parse_one(q).unwrap();
        let e = db.execute_with(&st, &[]).unwrap_err().to_string();
        assert!(e.contains(says), "{q}: {e}");
    }
    // Plain text takes a letter for its escape, before any character.
    let names = |db: &mut Database, w: &str| texts(db, &format!("get p select name where {w}"));
    assert_eq!(names(&mut db, "name like 'BXob' escape 'X'"), ["Bob"]);
    assert_eq!(
        names(&mut db, "name like 'BoX_' escape 'X'"),
        Vec::<String>::new()
    );
    assert_eq!(names(&mut db, "name like '%X%%' escape 'X'"), ["50% off"]);
}

/// The spellings run end to end, each answer what its FenecQL answers.
#[test]
fn sql_writes_and_reads_run() {
    let mut db = people();
    assert_eq!(
        texts(
            &mut db,
            "SELECT name FROM p WHERE age BETWEEN 25 AND 36 ORDER BY age DESC"
        ),
        ["Ada", "Bob"]
    );
    // Inclusive both ends, through the ordered index as through a scan.
    let Response::Rows(rs) = run(&mut db, "explain get p where age between 25 and 36") else {
        panic!()
    };
    let plan = format!("{:?}", rs.rows);
    assert!(plan.contains("the ordered index on age"), "{plan}");
    assert_eq!(
        texts(
            &mut db,
            "SELECT name FROM p WHERE age NOT BETWEEN 20 AND 40 ORDER BY id"
        ),
        ["O'Brien", "Straße", "50% off", "naïve_x"]
    );
    assert_eq!(
        texts(
            &mut db,
            "SELECT name FROM p WHERE age IN (7, 52) ORDER BY id"
        ),
        ["O'Brien", "50% off"]
    );
    assert_eq!(
        texts(
            &mut db,
            "SELECT name FROM p WHERE age NOT IN (7, 52, 25, 36, 19) ORDER BY id"
        ),
        ["Straße"]
    );
    assert_eq!(
        texts(
            &mut db,
            "SELECT name FROM p WHERE id IN (SELECT id FROM p WHERE city = 'Ankara') ORDER BY id"
        ),
        ["Bob", "50% off"]
    );
    // An upsert, `excluded` the row offered, the collection's name the row
    // held.
    let up = "INSERT INTO p (email, n) VALUES ('ADA@x.com', 2) \
              ON CONFLICT DO UPDATE SET n = coalesce(p.n, 0) + excluded.n RETURNING name, n";
    assert_eq!(texts(&mut db, up), ["Ada"]);
    run(&mut db, up);
    assert_eq!(texts(&mut db, "SELECT n FROM p WHERE name = 'Ada'"), ["4"]);
    assert!(matches!(
        run(
            &mut db,
            "INSERT INTO p (email) VALUES ('bob@X.com') ON CONFLICT DO NOTHING"
        ),
        Response::Affected(0)
    ));
    assert_eq!(
        texts(
            &mut db,
            "UPDATE p SET n = 1, name = name || '!' WHERE city = 'Ankara' RETURNING name"
        ),
        ["Bob!", "50% off!"]
    );
    assert!(matches!(
        run(&mut db, "DELETE FROM p WHERE age < 20"),
        Response::Affected(2)
    ));
    // A json field's list of numbers in a row, read as written through
    // `parse_for`, as a server reads a text.
    run(&mut db, "alter collection p add field meta json");
    let st = fenec_ql::parse_one_for(
        &db,
        "INSERT INTO p (email, meta) VALUES ('m@x', {k: [1.5, 2]}) RETURNING meta",
    )
    .unwrap();
    let Response::Rows(rs) = db.execute_with(&st, &[]).unwrap() else {
        panic!()
    };
    assert_eq!(
        fenec_core::json::to_string(&rs.rows[0].values[0]),
        r#"{"k":[1.5,2]}"#
    );
}

/// `count(x)` counts the rows where `x` is not null, as SQL's does; and
/// what an agent reaches for that is not here is refused naming what is.
#[test]
fn count_of_a_value_and_the_names_that_are_not_fields() {
    let mut db = people();
    let Response::Rows(rs) = run(
        &mut db,
        "SELECT count(*) AS all_rows, count(age), count(city), count(distinct city) FROM p",
    ) else {
        panic!()
    };
    assert_eq!(
        rs.rows[0].values,
        [Value::Int(6), Value::Int(5), Value::Int(5), Value::Int(4)]
    );
    assert_eq!(
        rs.columns,
        [
            "all_rows",
            "count(age)",
            "count(city)",
            "count(distinct city)"
        ]
    );
    let Response::Rows(rs) = run(
        &mut db,
        "SELECT city, count(age) AS n FROM p GROUP BY city HAVING count(age) > 1 ORDER BY city",
    ) else {
        panic!()
    };
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(
        rs.rows[0].values,
        [Value::Text("Ankara".into()), Value::Int(2)]
    );
    for (q, says) in [
        ("SELECT rowid FROM p", "field `rowid`: a row's id is `id`"),
        ("get p where _id = 1", "field `_id`: a row's id is `id`"),
        (
            "SELECT * FROM p WHERE p.age > 30",
            "`p.age` reads into a json field, and a field is named bare",
        ),
        (
            "get p where contains(name, 'a')",
            "text is matched by `like '%a%'`",
        ),
    ] {
        let st = parse_one(q).unwrap();
        let e = db.execute_with(&st, &[]).unwrap_err().to_string();
        assert!(e.contains(says), "{q}: {e}");
    }
}

/// A `like` in a filter is bound to its field (`Test::Pattern`), and
/// compared with `true` worked out by `eval`: over generated texts and
/// patterns, in a plain field, a `nocase` one, a Turkish `nocase` one and
/// a json one, with and without an escape, the two keep the same rows, and
/// a refusal is the same refusal.
#[test]
fn a_bound_like_answers_as_its_column_does() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection w (a text, b text nocase, c text collate tr nocase, j json)",
    );
    let pieces = [
        "a", "A", "ß", "SS", "İ", "i", "ı", "I", "ç", "%", "_", "!", "x", "𝔸",
    ];
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut pick = move |n: usize| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x % n as u64) as usize
    };
    let mut word = |wild: bool| {
        let mut w = String::new();
        for _ in 0..pick(5) {
            match (wild, pick(6)) {
                (true, 0) => w.push('%'),
                (true, 1) => w.push('_'),
                _ => w.push_str(pieces[pick(pieces.len())]),
            }
        }
        w
    };
    let put = parse_one("put w {a: $1, b: $2, c: $3, j: $1}").unwrap();
    for _ in 0..300 {
        let row = [word(false), word(false), word(false)].map(Value::Text);
        db.execute_with(&put, &row).unwrap();
    }
    db.execute(&parse_one("put w {a: null}").unwrap()).unwrap();
    db.execute(&parse_one("put w {j: 7}").unwrap()).unwrap();
    let mut checked = 0;
    for _ in 0..150 {
        let pat = [Value::Text(word(true))];
        for f in ["a", "b", "c", "j"] {
            for op in ["like", "ilike", "not like"] {
                for esc in ["", " escape '!'", " escape 'ß'"] {
                    let cond = format!("{f} {op} $1{esc}");
                    let bound = parse_one(&format!("get w select id where {cond}")).unwrap();
                    let evaluated =
                        parse_one(&format!("get w select id where ({cond}) = true")).unwrap();
                    match (db.query(&bound, &pat), db.query(&evaluated, &pat)) {
                        (Ok(Response::Rows(k)), Ok(Response::Rows(e))) => {
                            assert_eq!(k.rows, e.rows, "{cond} {pat:?}");
                            checked += 1;
                        }
                        (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{cond}"),
                        (a, b) => panic!("{cond} {pat:?}: {a:?} against {b:?}"),
                    }
                }
            }
        }
    }
    assert!(checked > 2_500, "{checked}");
}

/// What the agent eval found answered before the hint it needed, each
/// answered with that hint now.
#[test]
fn the_hint_an_agent_needs_comes_first() {
    let says = |q: &str, what: &str| {
        let e = parse(q)
            .map(|s| format!("{s:?}"))
            .unwrap_or_else(|e| e.to_string());
        assert!(e.contains(what), "{q}: {e}");
    };
    // A join is named before the grouping over its columns is refused,
    // after an alias, behind `distinct`, and as a comma between two.
    let join = "FenecQL has no `join`";
    says(
        "SELECT c.name, count(*) FROM orders o JOIN customers c ON o.cid = c.id GROUP BY c.name",
        join,
    );
    says(
        "SELECT * FROM orders JOIN customers ON orders.cid = customers.id",
        join,
    );
    says(
        "SELECT DISTINCT c.name FROM orders o JOIN customers c ON o.cid = c.id",
        join,
    );
    says(
        "SELECT * FROM orders o, customers c WHERE o.cid = c.id",
        join,
    );
    says("SELECT * FROM orders, customers", join);
    // Its `lookup` puts the rows above's `where` first.
    says(
        "SELECT * FROM a JOIN b",
        "`get items where ... lookup orders on id = order_id`",
    );
    // An alias alone, before the grouping over its columns.
    says(
        "SELECT u.city, count(*) FROM users u GROUP BY u.city",
        "unknown command `u`: an alias",
    );
    // PostgreSQL's `->>`.
    says(
        "SELECT * FROM t WHERE meta->>'source' = 'web'",
        "a json field's member is read by a path: `meta.source`",
    );
    // `distinct`'s hint names a group with its aggregate; in an `in`'s
    // query it changes nothing.
    says(
        "SELECT DISTINCT city FROM users",
        "a `group` needs an aggregate",
    );
    assert_eq!(
        parse_one("get t where x in (select distinct a from u where b = 1)").unwrap(),
        parse_one("get t where x in (get u select a where b = 1)").unwrap()
    );
}

/// `on conflict (k)` names the one unique key a row is found by, which the
/// engine holds it to: taken where it is the collection's one, refused
/// where the collection has another or a document names an id.
#[test]
fn a_conflict_target_is_the_one_unique_key() {
    let st = parse_one("INSERT INTO t (k) VALUES (1) ON CONFLICT (k) DO NOTHING").unwrap();
    let Statement::Put {
        pick, if_absent, ..
    } = &st
    else {
        panic!()
    };
    assert!(if_absent);
    assert_eq!(pick.as_ref().unwrap().conflict, Some(vec!["k".to_string()]));
    let mut db = Database::new();
    run(
        &mut db,
        "create collection p (sku text @unique, n int); create collection two (a text @unique, \
         b text @unique, n int); create collection plain (n int)",
    );
    let up = "INSERT INTO p (sku, n) VALUES ('x', 1) ON CONFLICT (sku) DO UPDATE SET n = p.n + \
              excluded.n RETURNING n";
    assert_eq!(texts(&mut db, up), ["1"]);
    assert_eq!(texts(&mut db, up), ["2"]);
    assert!(matches!(
        run(
            &mut db,
            "insert into plain (id, n) values (1, 1) on conflict (id) do nothing"
        ),
        Response::Affected(1)
    ));
    for (q, says) in [
        (
            "insert into p (sku) values ('y') on conflict (n) do nothing",
            "its rows are found by (sku)",
        ),
        (
            "insert into two (a, b) values ('a', 'b') on conflict (a) do nothing",
            "its rows are found by (a), (b)",
        ),
        (
            "insert into p (id, sku) values (9, 'z') on conflict (sku) do nothing",
            "(sku), (id)",
        ),
    ] {
        let e = db
            .execute_with(&parse_one(q).unwrap(), &[])
            .unwrap_err()
            .to_string();
        assert!(e.contains(says), "{q}: {e}");
    }
}

/// A field a `lookup` level does not hold says the clauses after it are
/// that collection's -- where agents put the rows above's `where`.
#[test]
fn a_field_after_lookup_is_the_looked_up_collections() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection orders (total int); create collection items (order_id int @hash, \
         qty int); put orders {total: 10}; put items {order_id: 1, qty: 3}",
    );
    for q in [
        "get items lookup orders on id = order_id where qty > 1",
        "get items lookup orders on id = order_id select qty",
    ] {
        let e = db
            .execute_with(&parse_one(q).unwrap(), &[])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("the clauses after `lookup` are the looked-up collection's"),
            "{q}: {e}"
        );
    }
    // And `json_extract`, the engine's to answer.
    let e = db
        .execute_with(
            &parse_one("get items where json_extract(qty, '$.a') = 1").unwrap(),
            &[],
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("`meta.source`"), "{e}");
}
