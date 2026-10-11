//! `limit $n`, `offset $n` and a facet's `top $n` answer what the numbers
//! written in the text answer, row for row, through every path that reads
//! a page: the scan that stops at `offset + limit`, an `@sorted` walk, an
//! ordering that keeps only the page, `near`, `match`, `fuse`, a `lookup`
//! level's children, an inner `get`, a facet, groups, a write that picks
//! its rows, `explain`, a plain `get` written as JSON and a read pinned.
//! A parameter is refused as the number it holds would be, in the same
//! words.

use fenec_core::prelude::*;

fn exec(db: &mut Database, sql: &str) {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn stmt(sql: &str) -> Statement {
    fenec_ql::parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// 600 rows of `t` -- an ordered `a`, a few values of `b` so ties are
/// everywhere, a hashed tag, a text and a vector -- and children in `c`,
/// from none to four a row.
fn fixture() -> Database {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection t (a int @sorted, b int, tag text @hash, body text @text, \
         v vector<4> @hnsw(cosine, m=8))",
    );
    exec(&mut db, "create collection c (k int @hash, s int)");
    let put = stmt("put t {a: $1, b: $2, tag: $3, body: $4, v: $5}");
    let child = stmt("put c {k: $1, s: $2}");
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for i in 0..600u64 {
        let r = next();
        let v: Vec<f32> = (0..4)
            .map(|j| ((r >> (j * 8)) & 0xff) as f32 - 127.5)
            .collect();
        db.execute_with(
            &put,
            &[
                Value::Int((r % 97) as i64),
                Value::Int((r % 7) as i64),
                Value::Text(["red", "blue", "green", "black"][(r % 4) as usize].into()),
                Value::Text(format!("word{} common w{}", r % 13, i % 5)),
                Value::Vector(v),
            ],
        )
        .unwrap();
        for s in 0..(i % 5) {
            db.execute_with(
                &child,
                &[Value::Int(i as i64 + 1), Value::Int((r % 9 + s) as i64)],
            )
            .unwrap();
        }
    }
    db
}

/// `template` with each `#` written as the number it stands for, and with
/// each a parameter after `base`'s: the two texts and the parameters of
/// the second.
fn pair(template: &str, base: &[Value], counts: &[i64]) -> (String, String, Vec<Value>) {
    let (mut lit, mut par) = (String::new(), String::new());
    let mut params = base.to_vec();
    let mut n = counts.iter();
    for (i, part) in template.split('#').enumerate() {
        if i > 0 {
            let c = *n.next().expect("a count for each #");
            lit.push_str(&c.to_string());
            params.push(Value::Int(c));
            par.push('$');
            par.push_str(&params.len().to_string());
        }
        lit.push_str(part);
        par.push_str(part);
    }
    assert!(n.next().is_none(), "a # for each count");
    (lit, par, params)
}

fn query(db: &Database, sql: &str, params: &[Value]) -> Result<Response> {
    db.query(&fenec_ql::parse_one(sql)?, params)
}

/// The words of an error, its position in the text left out: a literal
/// is refused by the parser, which says where -- and, for what the
/// select's check refuses, says "query error" twice.
fn words(e: &Error) -> String {
    let s = e.to_string();
    let s = match s.find("position ") {
        Some(at) => {
            let rest = &s[at..];
            let colon = rest.find(": ").map_or(0, |c| c + 2);
            format!("{}{}", &s[..at], &rest[colon..])
        }
        None => s,
    };
    s.replace("query error: query error: ", "query error: ")
}

fn vector() -> Value {
    Value::Vector(vec![10.0, -20.0, 30.0, 5.0])
}

/// A read's template, the parameters before its counts, and the counts
/// it is asked with.
type Read = (&'static str, Vec<Value>, Vec<Vec<i64>>);

/// Each read, written with its counts and with them as parameters.
fn reads() -> Vec<Read> {
    let pages = |v: &[(i64, i64)]| v.iter().map(|&(l, o)| vec![l, o]).collect::<Vec<_>>();
    vec![
        // The scan that stops at `offset + limit` matches.
        (
            "get t where b > 3 limit # offset #",
            vec![],
            pages(&[(0, 0), (1, 0), (7, 2), (50, 590), (1000, 0), (5, 5000)]),
        ),
        // An `@sorted` walk, which stops at the page.
        (
            "get t where b > 1 order a desc limit # offset #",
            vec![],
            pages(&[(1, 0), (10, 3), (100, 400), (3, 0)]),
        ),
        // An ordering that keeps only the page.
        (
            "get t order b, tag desc limit # offset #",
            vec![],
            pages(&[(1, 0), (9, 4), (300, 299)]),
        ),
        // A plain `get`, which a server writes as JSON from the documents.
        (
            "get t select id, a, b limit # offset #",
            vec![],
            pages(&[(5, 0), (20, 100), (0, 3)]),
        ),
        (
            "get t near v $1 limit # offset #",
            vec![vector()],
            pages(&[(5, 0), (10, 3), (1, 50)]),
        ),
        ("get t near v $1 exact limit #", vec![vector()], vec![vec![7]]),
        (
            "get t where tag = $1 near v $2 limit # offset #",
            vec![Value::Text("red".into()), vector()],
            pages(&[(4, 0), (12, 6)]),
        ),
        (
            "get t match body $1 limit # offset #",
            vec![Value::Text("word3 common".into())],
            pages(&[(5, 0), (10, 10), (100, 0)]),
        ),
        (
            "get t near v $1 match body $2 fuse limit # offset #",
            vec![vector(), Value::Text("word5".into())],
            pages(&[(5, 0), (8, 4)]),
        ),
        // A `lookup` level's children, counted per parent.
        (
            "get t where b = 2 order a limit # lookup c on k order s desc limit # offset #",
            vec![],
            vec![vec![10, 2, 1], vec![30, 0, 0], vec![5, 3, 2]],
        ),
        (
            "get t order a limit # offset # lookup c on k required limit #",
            vec![],
            vec![vec![10, 5, 1], vec![40, 0, 3]],
        ),
        // An inner `get`, run once with the statement's parameters.
        (
            "get t select id where id in (get c select k where s > 3 order s limit # offset #) limit #",
            vec![],
            vec![vec![20, 5, 100], vec![3, 0, 2], vec![500, 0, 1000]],
        ),
        (
            "get t where b < 5 limit # facet tag top #, b top #",
            vec![],
            vec![vec![10, 2, 3], vec![0, 1, 7], vec![3, 4, 1]],
        ),
        (
            "get t select b, count(*) group b order count(*) desc, b limit # offset #",
            vec![],
            pages(&[(3, 0), (2, 2), (10, 0)]),
        ),
        (
            "get t where b = 1 order a limit # require 3",
            vec![],
            vec![vec![3]],
        ),
    ]
}

#[test]
fn a_parameter_answers_what_the_number_written_answers() {
    let db = fixture();
    for (template, base, cases) in reads() {
        for counts in cases {
            let (lit, par, params) = pair(template, &base, &counts);
            let want = query(&db, &lit, &base).unwrap_or_else(|e| panic!("{lit}: {e}"));
            let got = query(&db, &par, &params).unwrap_or_else(|e| panic!("{par}: {e}"));
            assert_eq!(got, want, "{par} {counts:?}");
            // A count that is a whole float is the number it is.
            let floats: Vec<Value> = (params.iter().enumerate())
                .map(|(i, v)| match v {
                    Value::Int(n) if i >= base.len() => Value::Float(*n as f64),
                    other => other.clone(),
                })
                .collect();
            assert_eq!(query(&db, &par, &floats).unwrap(), want, "{par} as floats");
            // `explain` names the numbers the plan ran with.
            let plan = |sql: &str, p: &[Value]| query(&db, &format!("explain {sql}"), p).unwrap();
            assert_eq!(plan(&par, &params), plan(&lit, &base), "explain {par}");
        }
    }
    // `require` counts what the page answers.
    let (_, par, params) = pair("get t where b = 1 order a limit # require 3", &[], &[2]);
    let e = query(&db, &par, &params).unwrap_err();
    assert!(matches!(e, Error::Unmet(_)), "{e}");
}

/// What `query_json` writes of a plain `get` and what a pinned read
/// answers are what the lock's answer is, the counts parameters.
#[test]
#[cfg(not(target_arch = "wasm32"))]
fn the_json_path_and_a_pin_read_the_numbers_handed() {
    let mut db = fixture();
    db.set_pin_at(0);
    let (_, par, params) = pair(
        "get t select id, a, b where b > 2 limit # offset #",
        &[],
        &[9, 4],
    );
    let lit = "get t select id, a, b where b > 2 limit 9 offset 4";
    let mut fast = String::new();
    let n = db.query_json(&stmt(&par), &params, &mut fast).unwrap();
    assert_eq!(n, Some(9), "{fast}");
    let mut want = String::new();
    db.query_json(&stmt(lit), &[], &mut want).unwrap();
    assert_eq!(fast, want);
    let s = stmt(&par);
    let pinned = db
        .pin(&[(&s, &params[..])])
        .expect("a scan of 600 rows pins at 0");
    let got = pinned.query(&s, &params).expect("no index").unwrap();
    assert_eq!(got, query(&db, lit, &[]).unwrap());
    // A count past what memory holds is every row, never wrapped.
    let all = query(&db, "get t select id limit $1", &[Value::Int(i64::MAX)]).unwrap();
    assert_eq!(all.rows().unwrap().rows.len(), 600);
}

/// A `set` and a `del` that pick their rows by `order` and `limit` write
/// the rows the number written picks.
#[test]
fn a_write_picks_the_rows_the_number_written_picks() {
    for (template, counts) in [
        (
            "set t {b: b + 100} where b < 3 order a limit # returning *",
            7,
        ),
        ("del t where b = 4 order a desc limit # returning id", 5),
        ("set t {tag: \"x\"} order b, a limit #", 40),
        ("del t limit #", 11),
    ] {
        let (lit, par, params) = pair(template, &[], &[counts]);
        let (mut a, mut b) = (fixture(), fixture());
        let want = a.execute(&stmt(&lit)).unwrap();
        let got = b.execute_with(&stmt(&par), &params).unwrap();
        assert_eq!(got, want, "{par}");
        let all = "get t select id, b, tag order id";
        assert_eq!(query(&b, all, &[]).unwrap(), query(&a, all, &[]).unwrap());
    }
}

/// A parameter is refused as the number it holds, written in the text,
/// would be -- the same words -- and so past a bound the literal meets:
/// `near`'s and `match`'s 10 000 rows, a facet's 1 to 10 000 values.
#[test]
fn a_parameter_is_refused_as_the_number_written_is() {
    let mut db = fixture();
    let v = vector();
    let cases: [(&str, Vec<Value>, Value, &str); 9] = [
        ("get t limit #", vec![], Value::Int(-1), "-1"),
        ("get t offset #", vec![], Value::Float(2.5), "2.5"),
        (
            "get t limit #",
            vec![],
            Value::Text("ten".into()),
            "\"ten\"",
        ),
        ("get t lookup c on k limit #", vec![], Value::Int(-3), "-3"),
        (
            "get t near v $1 limit #",
            vec![v.clone()],
            Value::Int(10_001),
            "10001",
        ),
        (
            "get t near v $1 limit 9999 offset #",
            vec![v.clone()],
            Value::Int(2),
            "2",
        ),
        (
            "get t match body $1 limit #",
            vec![Value::Text("word1".into())],
            Value::Int(10_001),
            "10001",
        ),
        ("get t facet tag top #", vec![], Value::Int(0), "0"),
        ("get t facet tag top #", vec![], Value::Int(10_001), "10001"),
    ];
    for (template, base, value, written) in cases {
        let lit = template.replace('#', written);
        let par = template.replace('#', &format!("${}", base.len() + 1));
        let mut params = base.clone();
        params.push(value);
        let want = query(&db, &lit, &base).expect_err(&lit);
        let got = query(&db, &par, &params).expect_err(&par);
        assert_eq!(words(&got), words(&want), "{par}");
    }
    // What has no number written for it is refused in the literal's words.
    for (value, says) in [
        (Value::Null, "`limit` takes a whole number from 0, not null"),
        (
            Value::Bool(true),
            "`limit` takes a whole number from 0, not true",
        ),
        (
            Value::Float(f64::NAN),
            "`limit` takes a whole number from 0, not ",
        ),
        (
            Value::Float(f64::INFINITY),
            "`limit` takes a whole number from 0, not ",
        ),
        (
            Value::Vector(vec![1.0]),
            "`limit` takes a whole number from 0, not [1]",
        ),
    ] {
        let e = query(&db, "get t limit $1", &[value])
            .unwrap_err()
            .to_string();
        assert!(e.contains(says), "{e}");
    }
    let e = query(&db, "get t limit $1 offset $2", &[Value::Int(1)]).unwrap_err();
    assert!(e.to_string().contains("parameter $2 is not bound"), "{e}");
    // A write is refused before it writes, and its block put back.
    let before = query(&db, "get t select id, b order id", &[]).unwrap();
    let e = db
        .execute_with(&stmt("del t order a limit $1"), &[Value::Int(-1)])
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "query error: `limit` takes a whole number from 0, not -1"
    );
    assert_eq!(
        query(&db, "get t select id, b order id", &[]).unwrap(),
        before
    );
}
