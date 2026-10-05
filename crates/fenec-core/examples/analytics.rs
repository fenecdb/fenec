//! What the analytics and market-data aggregates cost: `make analytics-bench`.
//!
//! A million events (a user, an event name, a time over the last seven days
//! under `@ttl(30d)`, a country, a json of properties) and a million ticks
//! (50 symbols 20 ms apart, about 5.5 hours, a price and a quantity), the
//! shapes of the real-world spikes. Each question is timed as one query and
//! as the workaround it took before -- the queries a client sent instead,
//! in process here, and what it folded itself -- with the same answer
//! checked between them. Then the fixed aggregates of before, over a million
//! rows in 100 groups, so a run on another commit (`old`, which asks
//! nothing new) holds them to it. The median of `RUNS`, after one run to
//! build the indexes.
//!
//!     cargo run --release -p fenec-core --example analytics [old | writes]

use fenec_core::prelude::*;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

const RUNS: usize = 11;
const N: usize = 1_000_000;
const MIN: i64 = 60_000;
const HOUR: i64 = 3_600_000;
const DAY: i64 = 86_400_000;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn exec(db: &mut Database, sql: &str) {
    for s in fenec_ql::parse(sql).expect("parse") {
        db.execute(&s).expect("execute");
    }
}

fn put(db: &mut Database, collection: &str, docs: Vec<Vec<(String, Expr)>>) {
    for chunk in docs.chunks(10_000) {
        db.execute(&Statement::Put {
            collection: collection.into(),
            docs: chunk.to_vec(),
            insert: false,
            if_absent: false,
            docs_param: None,
            else_set: None,
            require: None,
        })
        .expect("put");
    }
}

fn rows(db: &Database, sql: &str, params: &[Value]) -> Vec<Vec<Value>> {
    let stmt = fenec_ql::parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    match db.query(&stmt, params) {
        Ok(Response::Rows(rs)) => rs.rows.into_iter().map(|r| r.values).collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The median of `RUNS` of `f`, in ms, after one run, and what it answered.
fn median<T>(mut f: impl FnMut() -> T) -> (f64, T) {
    let first = f();
    let mut t: Vec<f64> = (0..RUNS)
        .map(|_| {
            let s = Instant::now();
            std::hint::black_box(f());
            s.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    t.sort_by(|a, b| a.total_cmp(b));
    (t[RUNS / 2], first)
}

fn line(what: &str, ms: f64, n: usize) {
    println!("  {what:<58} {ms:>9.2} ms  ({n})");
}

fn main() {
    let old = std::env::args().any(|a| a == "old");
    if std::env::args().any(|a| a == "writes") {
        writes();
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let lit = |v: Value| Expr::Lit(v);

    // The fixed aggregates of before, over a million rows in 100 groups.
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection t (cat text @hash, price float, n int @sorted)",
    );
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let docs = (0..N)
        .map(|i| {
            vec![
                (
                    "cat".into(),
                    lit(Value::Text(format!("c{:02}", rng.below(100)))),
                ),
                ("price".into(), lit(Value::Float(rng.unit() * 100.0))),
                ("n".into(), lit(Value::Int(i as i64))),
            ]
        })
        .collect();
    put(&mut db, "t", docs);
    if std::env::args().any(|a| a == "spin") {
        for _ in 0..300 {
            rows(
                &db,
                "get t select count(*), sum(price), min(price), max(price)",
                &[],
            );
        }
        return;
    }
    println!("aggregates of before, {N} rows:");
    for sql in [
        "get t select cat, count(*), avg(price) group cat",
        "get t select count(*), sum(price), min(price), max(price)",
        "get t select cat, count(*), avg(price) where n >= 900000 group cat",
    ] {
        let (ms, r) = median(|| rows(&db, sql, &[]));
        line(sql, ms, r.len());
    }
    if old {
        return;
    }
    drop(db);

    // Events.
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection events (user int @hash, name text @hash, at timestamp @ttl(30d), \
         minute int, hour int, day int @hash, country text, props json)",
    );
    let names = [
        "view", "click", "signup", "cart", "buy", "share", "search", "logout",
    ];
    let countries = ["TR", "US", "DE", "FR", "JP"];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut events = Vec::with_capacity(N);
    let docs = (0..N)
        .map(|_| {
            let at = now - rng.below(7 * DAY as u64) as i64;
            // Pareto(1.2) over 50 000 users, as the spike drew them.
            let user = ((1.0 - rng.unit()).powf(-1.0 / 1.2) as i64) % 50_000;
            let name = names[rng.below(8) as usize];
            events.push((user, at));
            let props = Value::object(vec![
                (
                    "plan".into(),
                    Value::Text(["free", "pro"][rng.below(2) as usize].into()),
                ),
                ("v".into(), Value::Int(1 + rng.below(9) as i64)),
            ])
            .unwrap();
            vec![
                ("user".into(), lit(Value::Int(user))),
                ("name".into(), lit(Value::Text(name.into()))),
                ("at".into(), lit(Value::Timestamp(at))),
                ("minute".into(), lit(Value::Int(at / MIN))),
                ("hour".into(), lit(Value::Int(at / HOUR))),
                ("day".into(), lit(Value::Int(at / DAY))),
                (
                    "country".into(),
                    lit(Value::Text(countries[rng.below(5) as usize].into())),
                ),
                ("props".into(), lit(props)),
            ]
        })
        .collect();
    put(&mut db, "events", docs);
    println!("\n{N} events, the last seven days:");
    let day_ago = [Value::Timestamp(now - DAY)];

    let q = "get events select bucket(at, 1m) as minute, count(*) where at >= $1 group minute";
    let (ms, one) = median(|| rows(&db, q, &day_ago));
    line(
        "per-minute counts over a day, bucket(at, 1m)",
        ms,
        one.len(),
    );
    let q = "get events select minute, count(*) where at >= $1 group minute";
    let (ms, stored) = median(|| rows(&db, q, &day_ago));
    line(
        "  by a minute field stored at ingest (the workaround)",
        ms,
        stored.len(),
    );
    // The stored minute starts where the bucket does, a minute's ms apart.
    let firsts = |r: &[Vec<Value>]| r.iter().map(|r| r[1].clone()).collect::<Vec<_>>();
    assert_eq!(firsts(&one), firsts(&stored));

    let q = "get events select bucket(at, 1h) as hour, count(distinct user) group hour";
    let (ms, distinct) = median(|| rows(&db, q, &[]));
    line(
        "distinct users per hour, 7 days, count(distinct user)",
        ms,
        distinct.len(),
    );
    // Before: a group of users an hour, shipped and counted by the client,
    // one query an hour of a stored hour field.
    let hours: Vec<i64> = {
        let mut h: Vec<i64> = events.iter().map(|e| e.1 / HOUR).collect();
        h.sort_unstable();
        h.dedup();
        h
    };
    let q = "get events select user, count(*) where hour = $1 group user";
    let (ms, counted) = median(|| {
        hours
            .iter()
            .map(|h| rows(&db, q, &[Value::Int(*h)]).len() as i64)
            .collect::<Vec<_>>()
    });
    line(
        "  a query an hour, the groups counted (the workaround)",
        ms,
        counted.len(),
    );
    let want: Vec<i64> = distinct
        .iter()
        .map(|r| match r[1] {
            Value::Int(n) => n,
            _ => 0,
        })
        .collect();
    assert_eq!(want, counted);
    let mut by_hand: HashMap<i64, HashSet<i64>> = HashMap::new();
    for e in &events {
        by_hand.entry(e.1 / HOUR).or_default().insert(e.0);
    }
    assert_eq!(by_hand.len(), distinct.len());

    let q = "get events select bucket(at, 1h) as hour, approx_count_distinct(user) group hour";
    let (ms, approx) = median(|| rows(&db, q, &[]));
    line(
        "  the same, approx_count_distinct(user), a sketch an hour",
        ms,
        approx.len(),
    );
    let worst = distinct
        .iter()
        .zip(&approx)
        .map(|(e, a)| match (&e[1], &a[1]) {
            (Value::Int(e), Value::Int(a)) => (a - e).abs() as f64 / *e as f64,
            _ => 0.0,
        })
        .fold(0.0, f64::max);
    println!(
        "    the furthest of {} hours off: {:.2}%",
        approx.len(),
        worst * 100.0
    );
    let q = "get events select count(distinct user)";
    let (ms, exact) = median(|| rows(&db, q, &[]));
    line("distinct users of the week, count(distinct user)", ms, 1);
    let q = "get events select approx_count_distinct(user)";
    let (ms, est) = median(|| rows(&db, q, &[]));
    line("  approx_count_distinct(user)", ms, 1);
    println!("    {:?} against {:?}", exact[0][0], est[0][0]);

    // The ordered funnel: users whose first buy is no earlier than their
    // first signup.
    let firsts = "get events select user, min(case when name = 'signup' then at end) as a, \
                  min(case when name = 'buy' then at end) as b \
                  where name in ['signup', 'buy'] group user";
    let q = format!("{firsts} having b >= a count");
    let (ms, funnel) = median(|| rows(&db, &q, &[]));
    line("ordered funnel, group user having b >= a count", ms, 1);
    let (ms, by_client) = median(|| {
        rows(&db, firsts, &[])
            .iter()
            .filter(|r| match (&r[1], &r[2]) {
                (Value::Timestamp(a), Value::Timestamp(b)) => b >= a,
                _ => false,
            })
            .count()
    });
    line(
        "  a row a user, compared by the client (the workaround)",
        ms,
        by_client,
    );
    assert_eq!(funnel[0][0], Value::Int(by_client as i64));

    let q = "get events select name, country, count(*) group name, country";
    let (ms, r) = median(|| rows(&db, q, &[]));
    line(
        "events by name and country, group name, country",
        ms,
        r.len(),
    );
    let q = "get events select props.plan, sum(props.v) group props.plan";
    let (ms, r) = median(|| rows(&db, q, &[]));
    line("a json path summed per plan, sum(props.v)", ms, r.len());
    drop(db);

    // Ticks.
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection ticks (sym text @hash, at timestamp @ttl(7d), px float, qty int, \
         minute int)",
    );
    let syms: Vec<String> = (0..50).map(|i| format!("S{i:02}")).collect();
    let mut px = vec![100.0f64; 50];
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    let docs = (0..N)
        .map(|k| {
            let s = k % 50;
            // A walk of about 0.1% a tick.
            px[s] = (px[s] * (1.0 + (rng.unit() - 0.5) * 0.0035) * 1e4).round() / 1e4;
            let at = now - (N - k) as i64 * 20;
            vec![
                ("sym".into(), lit(Value::Text(syms[s].clone()))),
                ("at".into(), lit(Value::Timestamp(at))),
                ("px".into(), lit(Value::Float(px[s]))),
                ("qty".into(), lit(Value::Int(1 + rng.below(500) as i64))),
                ("minute".into(), lit(Value::Int(at / MIN))),
            ]
        })
        .collect();
    put(&mut db, "ticks", docs);
    println!("\n{N} ticks over 50 symbols:");
    let hour_ago = now - HOUR;
    let q = "get ticks select bucket(at, 1m) as bar, first(px by at) as open, max(px) as high, \
             min(px) as low, last(px by at) as close, sum(qty) as volume \
             where sym = 'S07' and at >= $1 group bar";
    let (ms, bars) = median(|| rows(&db, q, &[Value::Timestamp(hour_ago)]));
    line(
        "60 one-minute OHLC bars of one symbol, one query",
        ms,
        bars.len(),
    );
    let open = "get ticks select px where sym = 'S07' and minute = $1 order at limit 1";
    let close = "get ticks select px where sym = 'S07' and minute = $1 order at desc limit 1";
    let (ms, oc) = median(|| {
        let mut out = Vec::new();
        for m in hour_ago / MIN..=now / MIN {
            let o = rows(&db, open, &[Value::Int(m)]);
            let c = rows(&db, close, &[Value::Int(m)]);
            if let (Some(o), Some(c)) = (o.first(), c.first()) {
                out.push((o[0].clone(), c[0].clone()));
            }
        }
        out
    });
    line(
        "  open and close as 2 queries a bar (the workaround)",
        ms,
        oc.len(),
    );
    // The first bar is cut by the hour's start: the rest are whole minutes.
    for (bar, (o, c)) in bars.iter().skip(1).zip(oc.iter().skip(1)) {
        assert_eq!((&bar[1], &bar[4]), (o, c));
    }

    let q = "get ticks select sym, sum(px * qty) / sum(qty) as vwap group sym";
    let (ms, vwap) = median(|| rows(&db, q, &[]));
    line(
        "VWAP of every symbol, sum(px * qty) / sum(qty)",
        ms,
        vwap.len(),
    );
    let q = "get ticks select sym, px, qty";
    let (ms, folded) = median(|| {
        let mut sums: HashMap<String, (f64, f64)> = HashMap::new();
        for r in rows(&db, q, &[]) {
            if let [Value::Text(s), Value::Float(p), Value::Int(n)] = &r[..] {
                let e = sums.entry(s.clone()).or_default();
                e.0 += p * *n as f64;
                e.1 += *n as f64;
            }
        }
        sums.len()
    });
    line(
        "  every tick read out and folded by the client (the workaround)",
        ms,
        folded,
    );

    let q = "get ticks select sym, last(px by at) as px group sym";
    let (ms, latest) = median(|| rows(&db, q, &[]));
    line(
        "the latest price of every symbol, last(px by at)",
        ms,
        latest.len(),
    );
    let one = "get ticks select px where sym = $1 order at desc limit 1";
    let (ms, each) = median(|| {
        syms.iter()
            .map(|s| rows(&db, one, &[Value::Text(s.clone())])[0][0].clone())
            .collect::<Vec<_>>()
    });
    line(
        "  a query a symbol, order at desc limit 1 (the workaround)",
        ms,
        each.len(),
    );
    let got: Vec<Value> = latest.iter().map(|r| r[1].clone()).collect();
    assert_eq!(got, each);
    drop(db);
    writes();
}

/// A beacon's page of 1 000 events, written out and as a parameter, and a
/// rollup's page of 1 000 counts -- half of them new -- as one upsert and as
/// the read and the two writes it took before, each parsed from its text
/// and JSON as a server parses them.
fn writes() {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection ev (eid text @unique, name text, user text, path text, \
         at timestamp, props json);
         create collection m (key text @unique, n int)",
    );
    println!("\nwrites, a page of 1 000:");
    // An event's fields, as FenecQL writes a document (`q`) and as JSON.
    let event = |i: usize, q: bool| {
        let (eid, user, path) = (
            format!("e{i}"),
            format!("u{}", i % 977),
            format!("/p/{}", i % 13),
        );
        let at = 1_800_000_000_000i64 + i as i64;
        let v = i % 9;
        match q {
            true => format!(
                "{{eid: \"{eid}\", name: \"view\", user: \"{user}\", path: \"{path}\", at: {at}, \
                 props: {{plan: \"pro\", v: {v}}}}}"
            ),
            false => format!(
                "{{\"eid\": \"{eid}\", \"name\": \"view\", \"user\": \"{user}\", \"path\": \
                 \"{path}\", \"at\": {at}, \"props\": {{\"plan\": \"pro\", \"v\": {v}}}}}"
            ),
        }
    };
    let mut next = 0usize;
    let (ms, n) = median(|| {
        let docs: Vec<String> = (0..1000).map(|j| event(next + j, true)).collect();
        next += 1000;
        let text = format!("put ev [{}] if absent", docs.join(", "));
        let stmt = fenec_ql::parse_for(&db, &text).expect("parse");
        affected(db.execute(&stmt[0]))
    });
    line("put ev [1 000 documents written out] if absent", ms, n);
    let (ms, n) = median(|| {
        let docs: Vec<String> = (0..1000).map(|j| event(next + j, false)).collect();
        next += 1000;
        let params =
            fenec_core::json::parse_params(&format!("[[{}]]", docs.join(", "))).expect("params");
        let stmt = fenec_ql::parse_one("put ev $1 if absent").expect("parse");
        affected(db.execute_with(&stmt, &params))
    });
    line("put ev $1 if absent, the 1 000 as a parameter", ms, n);

    // A rollup of 20 000 keys, a page adding to 500 of them and making 500.
    let keys: Vec<String> = (0..20_000)
        .map(|i| format!("{{key: \"k{i}\", n: 1}}"))
        .collect();
    exec(&mut db, &format!("put m [{}]", keys.join(", ")));
    let mut round = 0usize;
    let mut fresh = 20_000usize;
    let mut page_of = || {
        round += 1;
        let mut out: Vec<(String, i64)> = (0..500)
            .map(|j| {
                (
                    format!("k{}", (round * 997 + j * 31) % 20_000),
                    1 + (j % 3) as i64,
                )
            })
            .collect();
        for _ in 0..500 {
            out.push((format!("k{fresh}"), 2));
            fresh += 1;
        }
        out
    };
    let upsert = fenec_ql::parse_one("put m $1 if absent else set {n: n + new.n}").unwrap();
    let (ms, n) = median(|| {
        let p = page_of();
        let json: Vec<String> = p
            .iter()
            .map(|(k, n)| format!("{{\"key\": \"{k}\", \"n\": {n}}}"))
            .collect();
        let params = fenec_core::json::parse_params(&format!("[[{}]]", json.join(", "))).unwrap();
        affected(db.execute_with(&upsert, &params))
    });
    line(
        "put m $1 if absent else set {n: n + new.n}, one statement",
        ms,
        n,
    );
    let (ms, n) = median(|| {
        let p = page_of();
        // Which keys are held, then the new rows and the old ones over by id.
        let list: Vec<String> = (1..=p.len()).map(|j| format!("${j}")).collect();
        let read = fenec_ql::parse_one(&format!(
            "get m select id, key, n where key in [{}] limit {}",
            list.join(", "),
            p.len()
        ))
        .unwrap();
        let keys: Vec<Value> = p.iter().map(|(k, _)| Value::Text(k.clone())).collect();
        let held: HashMap<String, (i64, i64)> = match db.query(&read, &keys) {
            Ok(Response::Rows(rs)) => rs
                .rows
                .into_iter()
                .map(|r| match &r.values[..] {
                    [Value::Int(id), Value::Text(k), Value::Int(n)] => (k.clone(), (*id, *n)),
                    other => panic!("{other:?}"),
                })
                .collect(),
            other => panic!("{other:?}"),
        };
        let (mut made, mut over) = (Vec::new(), Vec::new());
        for (k, n) in &p {
            match held.get(k) {
                Some((id, was)) => over.push(format!("{{id: {id}, key: \"{k}\", n: {}}}", was + n)),
                None => made.push(format!("{{key: \"{k}\", n: {n}}}")),
            }
        }
        let mut n = 0;
        for docs in [made, over] {
            let stmt = fenec_ql::parse_one(&format!("put m [{}]", docs.join(", "))).unwrap();
            n += affected(db.execute(&stmt));
        }
        n
    });
    line(
        "  a read, then the new rows and the old by id (before)",
        ms,
        n,
    );
}

fn affected(r: Result<Response>) -> usize {
    match r.expect("write") {
        Response::Affected(n) => n,
        other => panic!("{other:?}"),
    }
}
