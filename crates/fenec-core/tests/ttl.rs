//! `@ttl`: a row is gone that long after its timestamp -- at once for every
//! read, and from the file when the sweep deletes it.
//!
//! The spine is a twin: `s` declares `seen timestamp @ttl(30m)`, `t` holds
//! the same documents under a plain `@sorted`, and every query over `s` is
//! asked of `t` with the test written out by hand, `not (seen <= now -
//! 30m)`. The two must agree row for row, through every read a filter goes
//! into -- `get`, `count`, an aggregate, `near`, `match`, an ordered walk, a
//! `lookup` level, an inner `get` -- and through `set` and `del`, before
//! and after a sweep, and across a reopen.

use fenec_core::prelude::*;
use fenec_core::schema::IndexKind;
use std::sync::{Arc, Mutex};

/// The time every read here is answered at.
const NOW: i64 = 1_800_000_000_000;
const TTL: i64 = 30 * 60 * 1000;

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
    fn reopen(&self) -> Database {
        let mut db = Database::with_sink(Box::new(self.clone()));
        let bytes = self.file.lock().unwrap().clone();
        db.load(&bytes).expect("reopen");
        db.set_clock(Some(NOW));
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
}

fn run(db: &mut Database, sql: &str, params: &[Value]) -> Response {
    let mut last = None;
    for stmt in fenec_ql::parse(sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        last = Some(
            db.execute_with(&stmt, params)
                .unwrap_or_else(|e| panic!("{sql}: {e}")),
        );
    }
    last.unwrap()
}

fn rows(db: &Database, sql: &str, params: &[Value]) -> ResultSet {
    let stmt = fenec_ql::parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    match db
        .query(&stmt, params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
    {
        // A twin's children answer under their collection's name.
        Response::Rows(mut rs) => {
            if let Some(n) = &mut rs.nested {
                n.name = "s".into();
            }
            rs
        }
        other => panic!("{sql}: {other:?}"),
    }
}

fn count(db: &Database, sql: &str) -> i64 {
    match rows(db, sql, &[]).rows[0].values[0] {
        Value::Int(n) => n,
        ref v => panic!("{v:?}"),
    }
}

/// `s` and its twin `t`, the same documents in each: times on both sides
/// of the cutoff and on it, and some with none. `u` is looked up from and
/// into.
fn fixture(db: &mut Database) {
    db.set_clock(Some(NOW));
    run(
        db,
        "create collection s (user text @hash, seen timestamp @ttl(30m), n int,
                              v vector<3> @hnsw(cosine), body text @text);
         create collection t (user text @hash, seen timestamp @sorted, n int,
                              v vector<3> @hnsw(cosine), body text @text);
         create collection u (name text @hash, team int)",
        &[],
    );
    let mut r = Rng(0x77);
    let words = ["kahve", "demlik", "kupa", "kahve kupa"];
    let mut docs = Vec::new();
    for i in 0..1500 {
        let seen = match r.below(12) {
            0 => "null".to_string(),
            // Exactly at the cutoff: gone.
            1 => (NOW - TTL).to_string(),
            2 => (NOW - TTL + 1).to_string(),
            _ => (NOW - 2 * TTL + r.below(3 * TTL as u64) as i64).to_string(),
        };
        docs.push(format!(
            "{{user: \"u{}\", seen: {seen}, n: {}, v: [{}, {}, 1], body: \"{}\"}}",
            r.below(40),
            i % 17,
            r.below(9),
            r.below(9),
            words[r.below(4) as usize]
        ));
    }
    let docs = docs.join(",");
    run(db, &format!("put s [{docs}]; put t [{docs}]"), &[]);
    let users: Vec<String> = (0..50)
        .map(|i| format!("{{name: \"u{i}\", team: {}}}", i % 4))
        .collect();
    run(db, &format!("put u [{}]", users.join(",")), &[]);
}

/// `template` over `s`, and over `t` with the expiry written out: `{c}`
/// the collection, `{w}` a `where` that the test is ANDed into, `{and}`
/// where a filter already holds conditions, `{lw}` / `{land}` the same in a
/// `lookup` level over either.
fn twins(template: &str) -> [String; 2] {
    let alive = format!("not (seen <= {})", NOW - TTL);
    let s = template
        .replace("{c}", "s")
        .replace("{w}", "")
        .replace("{and}", "")
        .replace("{lw}", "")
        .replace("{land}", "");
    let t = template
        .replace("{c}", "t")
        .replace("{w}", &format!("where {alive}"))
        .replace("{and}", &format!("{alive} and "))
        .replace("{lw}", &format!("where {alive}"))
        .replace("{land}", &format!("{alive} and "));
    [s, t]
}

const READS: &[&str] = &[
    "get {c} {w} order id",
    "get {c} {w} count",
    "get {c} where {and} n > 5 count",
    "get {c} where {and} user = \"u7\" order id",
    "get {c} where {and} id in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10] order id",
    "get {c} {w} order seen desc limit 20",
    "get {c} where {and} seen >= 1799999000000 order seen limit 20",
    "get {c} {w} limit 30",
    "get {c} where {and} seen is null order id",
    "get {c} select user, count(*), max(seen) where {and} n < 9 group user",
    "get {c} select count(*), min(seen) {w}",
    "get {c} {w} near v [1, 2, 1] limit 10",
    "get {c} where {and} n = 3 near v [3, 1, 1] limit 5",
    "get {c} {w} match body \"kahve\" limit 10",
    "get u where name in (get {c} select user where {and} n = 4) order id",
    "get u order id limit 10 lookup {c} on user = name {lw} order id limit 3",
    "get u order id lookup {c} on user = name required where {land} n = 11 order id",
    "get u count lookup {c} on user = name required {lw}",
];

#[test]
fn a_read_leaves_out_what_the_test_written_out_does() {
    let mut db = Database::new();
    fixture(&mut db);
    for template in READS {
        let [s, t] = twins(template);
        assert_eq!(rows(&db, &s, &[]), rows(&db, &t, &[]), "\n{s}\n{t}");
    }
    // Something was left out, and not everything.
    let (all, alive) = (count(&db, "get t count"), count(&db, "get s count"));
    assert!(0 < alive && alive < all, "{alive} of {all}");
    // A row with no time has none to expire from.
    assert_eq!(
        count(&db, "get s where seen is null count"),
        count(&db, "get t where seen is null count")
    );
    assert!(count(&db, "get s where seen is null count") > 0);
}

#[test]
fn time_moves_what_a_read_finds() {
    let mut db = Database::new();
    fixture(&mut db);
    let before = count(&db, "get s count");
    db.set_clock(Some(NOW + TTL));
    let later = count(&db, "get s count");
    assert!(later < before, "{later} >= {before}");
    assert_eq!(
        later,
        count(&db, &format!("get t where not (seen <= {}) count", NOW))
    );
}

#[test]
fn set_and_del_reach_only_the_living() {
    for template in [
        "set {c} {n: 100} {w}",
        "set {c} {n: 101} where {and} n < 4",
        "del {c} where {and} user = \"u3\"",
        "del {c} {w}",
    ] {
        let mut db = Database::new();
        fixture(&mut db);
        let [s, t] = twins(template);
        let (a, b) = (run(&mut db, &s, &[]), run(&mut db, &t, &[]));
        assert_eq!(a, b, "{s}");
        for read in ["get {c} {w} order id", "get {c} {w} count"] {
            let [x, y] = twins(read);
            assert_eq!(rows(&db, &x, &[]), rows(&db, &y, &[]), "{s}: {x}");
        }
    }
}

/// The sweep deletes what reads already left out, and nothing else: the
/// reads answer as before it, and the rows it took are gone from the
/// file, as ordinary deletes -- which a replica applies and the change
/// ring reports.
#[test]
fn a_sweep_deletes_what_reads_left_out() {
    let tap = Tap::new();
    let mut db = Database::with_sink(Box::new(tap.clone()));
    fixture(&mut db);
    let before: Vec<ResultSet> = READS.iter().map(|q| rows(&db, &twins(q)[0], &[])).collect();
    let expired = count(&db, &format!("get t where seen <= {} count", NOW - TTL)) as usize;
    assert!(expired > 0);
    let seq = db.change_seq();
    assert_eq!(db.expiring(), vec!["s".to_string()]);
    let mut swept = 0;
    loop {
        let ids = db.expired("s", NOW, 100).unwrap();
        assert!(ids.len() <= 100);
        if ids.is_empty() {
            break;
        }
        swept += db.sweep("s", NOW, &ids).unwrap();
    }
    assert_eq!(swept, expired);
    assert_eq!(db.change_seq(), seq + expired as u64, "a delete a row");
    let after: Vec<ResultSet> = READS.iter().map(|q| rows(&db, &twins(q)[0], &[])).collect();
    for ((q, b), a) in READS.iter().zip(&before).zip(&after) {
        // BM25 counts a row in its statistics until it is swept, as it
        // counts any row until it is deleted: the same rows match, and
        // score a little otherwise once the sweep took the rest away.
        if q.contains("match") {
            let ids = |rs: &ResultSet| {
                let mut ids: Vec<u64> = rs.rows.iter().map(|r| r.id).collect();
                ids.sort_unstable();
                ids
            };
            assert_eq!(ids(b), ids(a), "{q}");
            continue;
        }
        assert_eq!(b, a, "{q}");
    }
    // Gone from the store, the nulls kept.
    db.set_clock(Some(0));
    assert_eq!(
        count(&db, "get s count") as usize,
        count(&db, "get t count") as usize - expired
    );
    assert_eq!(
        count(&db, "get s where seen is null count"),
        count(&db, "get t where seen is null count")
    );
    db.set_clock(Some(NOW));
    // The change ring hands the deletes to a subscriber as deletes.
    let Changes::Batch(b) = db.changes_since("s", seq, None, None, &[]).unwrap() else {
        panic!("reseed");
    };
    assert_eq!(b.dels.len(), expired);
    // A replica applies them as any delete, and a reopen holds them.
    let mut replica = Database::new();
    replica.set_clock(Some(0));
    for r in tap.records.lock().unwrap().iter() {
        replica.apply(r).unwrap();
    }
    db.set_clock(Some(0));
    assert_eq!(count(&replica, "get s count"), count(&db, "get s count"));
    assert_eq!(
        rows(&replica, "get s order id", &[]),
        rows(&db, "get s order id", &[])
    );
}

#[test]
fn a_row_written_again_since_it_was_found_is_not_swept() {
    let mut db = Database::new();
    fixture(&mut db);
    let ids = db.expired("s", NOW, 10).unwrap();
    assert_eq!(ids.len(), 10);
    // One is seen again before the sweep reaches it.
    run(
        &mut db,
        &format!("put s {{id: {}, user: \"u1\", seen: {NOW}}}", ids[0]),
        &[],
    );
    assert_eq!(db.sweep("s", NOW, &ids).unwrap(), 9);
    assert_eq!(count(&db, &format!("get s where id = {} count", ids[0])), 1);
}

#[test]
fn a_subscriber_is_told_an_expired_row_is_gone() {
    let mut db = Database::new();
    fixture(&mut db);
    let seq = db.change_seq();
    run(
        &mut db,
        &format!(
            "put s [{{id: 1, user: \"a\", seen: {NOW}}}, {{id: 2, user: \"b\", seen: {}}}]",
            NOW - TTL
        ),
        &[],
    );
    let Changes::Batch(b) = db.changes_since("s", seq, None, None, &[]).unwrap() else {
        panic!("reseed");
    };
    assert_eq!(
        b.puts.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(b.dels, vec![2]);
    // Time passes: the row it was handed is gone at its next change.
    db.set_clock(Some(NOW + TTL));
    let Changes::Batch(b) = db.changes_since("s", seq, None, None, &[]).unwrap() else {
        panic!("reseed");
    };
    assert!(b.puts.rows.is_empty());
    assert_eq!(b.dels, vec![1, 2]);
}

#[test]
fn the_expiry_is_in_the_file_and_an_index_kind_of_its_own() {
    let tap = Tap::new();
    let mut db = Database::with_sink(Box::new(tap.clone()));
    fixture(&mut db);
    let again = tap.reopen();
    let field = |db: &Database| {
        db.collection("s")
            .unwrap()
            .schema
            .field("seen")
            .unwrap()
            .index
            .clone()
    };
    assert_eq!(
        field(&again),
        IndexKind::Sorted {
            ttl: Some(TTL as u64)
        }
    );
    for template in READS {
        let [s, _] = twins(template);
        assert_eq!(rows(&db, &s, &[]), rows(&again, &s, &[]), "{s}");
    }
    // Kind 9 and the milliseconds behind it, where `@sorted` is kind 4: a
    // binary from before refuses the file rather than hand out the rows
    // past their time.
    let enc = |kind| {
        Schema::new(
            "x",
            vec![fenec_core::schema::Field::new("t", DataType::Timestamp).indexed(kind)],
        )
        .unwrap()
        .encode()
    };
    let (plain, ttl) = (
        enc(IndexKind::SORTED),
        enc(IndexKind::Sorted { ttl: Some(60_000) }),
    );
    assert_eq!(plain.last(), Some(&4));
    assert_eq!(&ttl[ttl.len() - 4..], &[9, 0xe0, 0xd4, 0x03]);
}

#[test]
fn a_ttl_takes_a_timestamp_and_a_duration() {
    for (sql, why) in [
        ("create collection x (t int @ttl(30m))", "timestamp"),
        ("create collection x (t text @ttl(1h))", "timestamp"),
        ("create collection x (t timestamp @ttl(0s))", "past zero"),
        ("create collection x (t timestamp @ttl(30))", "duration"),
        (
            "create collection x (t timestamp @ttl(30y))",
            "unknown unit",
        ),
        ("create collection x (t timestamp @ttl)", "expected"),
    ] {
        let e = fenec_ql::parse_one(sql).unwrap_err();
        assert!(e.to_string().contains(why), "{sql}: {e}");
    }
    for (d, ms) in [
        ("45s", 45_000),
        ("30m", 1_800_000),
        ("12h", 43_200_000),
        ("7d", 604_800_000),
    ] {
        let Statement::CreateCollection { schema, .. } =
            fenec_ql::parse_one(&format!("create collection x (t timestamp @ttl({d}))")).unwrap()
        else {
            panic!()
        };
        assert_eq!(schema.fields[0].index.ttl(), Some(ms));
        assert_eq!(fenec_core::schema::ttl_text(ms), d);
    }
    // A row expires by one time.
    let e = fenec_ql::parse_one("create collection x (a timestamp @ttl(1h), b timestamp @ttl(2h))")
        .unwrap_err();
    assert!(e.to_string().contains("expire by one field"), "{e}");
    let mut two = Database::new();
    run(
        &mut two,
        "create collection x (a timestamp @ttl(1h), b timestamp @sorted, c timestamp)",
        &[],
    );
    for sql in [
        "create index on x (c) @ttl(1h)",
        "alter collection x alter field b @ttl(1h)",
    ] {
        let e = two.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap_err();
        assert!(e.to_string().contains("expire by one field"), "{sql}: {e}");
    }
    // On a path, refused as any index but `@hash` and `@sorted` is.
    let mut db = Database::new();
    run(&mut db, "create collection j (meta json)", &[]);
    let e = db
        .execute(&fenec_ql::parse_one("create index on j (meta.at) @ttl(1h)").unwrap())
        .unwrap_err();
    assert!(e.to_string().contains("timestamp"), "{e}");
}

#[test]
fn an_index_or_an_alter_sets_the_expiry_and_a_block_puts_it_back() {
    let mut db = Database::new();
    db.set_clock(Some(NOW));
    run(
        &mut db,
        &format!(
            "create collection k (at timestamp, n int);
             put k [{{at: {NOW}}}, {{at: {}}}, {{n: 1}}]",
            NOW - 2 * TTL
        ),
        &[],
    );
    assert_eq!(count(&db, "get k count"), 3);
    // A plain timestamp field takes one through `create index`.
    run(&mut db, "create index on k (at) @ttl(30m)", &[]);
    assert_eq!(count(&db, "get k count"), 2);
    assert_eq!(db.expired("k", NOW, 10).unwrap(), vec![2]);
    // `alter field` moves it, and takes it off with `@sorted`.
    run(&mut db, "alter collection k alter field at @ttl(3h)", &[]);
    assert_eq!(count(&db, "get k count"), 3);
    run(&mut db, "alter collection k alter field at @ttl(1h)", &[]);
    assert_eq!(count(&db, "get k count"), 2);
    run(&mut db, "alter collection k alter field at @sorted", &[]);
    assert_eq!(count(&db, "get k count"), 3);
    assert!(db.expiring().is_empty());
    // An alter in a block that does not land is put back with it, the
    // ordered index kept.
    run(&mut db, "alter collection k alter field at @ttl(1h)", &[]);
    db.begin().unwrap();
    db.execute(&fenec_ql::parse_one("alter collection k alter field at @sorted").unwrap())
        .unwrap();
    assert_eq!(count(&db, "get k count"), 3);
    db.rollback();
    assert_eq!(count(&db, "get k count"), 2);
    assert_eq!(
        count(&db, &format!("get k where at >= {} count", NOW - 1)),
        1,
        "the ordered index answers as before"
    );
    // A field with no ordered index, or of another kind, is refused.
    for (sql, why) in [
        (
            "alter collection k alter field n @ttl(1h)",
            "no ordered index",
        ),
        (
            "alter collection k alter field at @hash",
            "@ttl(..) or @sorted",
        ),
        (
            "alter collection k alter field at int",
            "does not change in place",
        ),
    ] {
        let e = fenec_ql::parse_one(sql)
            .and_then(|s| db.execute(&s))
            .unwrap_err();
        assert!(e.to_string().contains(why), "{sql}: {e}");
    }
}

#[test]
fn an_alter_reaches_a_replica_and_a_reopen() {
    let tap = Tap::new();
    let mut db = Database::with_sink(Box::new(tap.clone()));
    fixture(&mut db);
    run(&mut db, "alter collection s alter field seen @ttl(1h)", &[]);
    run(&mut db, "alter collection t alter field seen @ttl(1h)", &[]);
    let mut replica = Database::new();
    replica.set_clock(Some(NOW));
    for r in tap.records.lock().unwrap().iter() {
        replica.apply(r).unwrap();
    }
    let again = tap.reopen();
    for d in [&replica, &again] {
        assert_eq!(
            d.collection("s").unwrap().schema,
            db.collection("s").unwrap().schema
        );
        assert_eq!(count(d, "get s count"), count(&db, "get s count"));
        assert_eq!(count(d, "get s count"), count(&db, "get t count"));
    }
}

/// The browser module has no clock: a read of a collection whose rows
/// expire is answered only at a time it is handed. Natively the system's
/// clock answers when none is.
#[test]
fn a_read_needs_a_time() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection k (at timestamp @ttl(1m)); put k [{at: 1}, {at: \"2999-01-01\"}]",
        &[],
    );
    db.set_clock(None);
    assert_eq!(count(&db, "get k count"), 1);
    db.set_clock(Some(0));
    assert_eq!(count(&db, "get k count"), 2);
    // Written at the time it is, a row lives its minute.
    db.set_clock(None);
    run(&mut db, "put k {at: now()}", &[]);
    assert_eq!(count(&db, "get k count"), 2);
}

/// `expired()` reads the rows past their time, which every other read
/// leaves out: a reaper's way to give back what a lapsed hold reserved
/// before the row goes. It is the test written out, `seen <= now - ttl`,
/// wherever a filter over the collection holds it -- `get`, `count`, a
/// `lookup` level, an inner `get`, `set` and `del` -- and `not expired()`
/// is what a read finds without it.
#[test]
fn expired_reads_what_every_other_read_leaves_out() {
    let mut db = Database::new();
    fixture(&mut db);
    let past = format!("seen <= {}", NOW - TTL);
    let alive = format!("not (seen <= {})", NOW - TTL);
    for (s, t) in [
        (
            "get s where expired() order id".to_string(),
            format!("get t where {past} order id"),
        ),
        (
            "get s where expired() count".into(),
            format!("get t where {past} count"),
        ),
        (
            "get s where expired() and user = \"u7\" order seen desc limit 5".into(),
            format!("get t where {past} and user = \"u7\" order seen desc limit 5"),
        ),
        (
            "get s where not expired() order id".into(),
            "get s order id".into(),
        ),
        (
            "get s select user, count(*) where expired() or n = 3 group user".into(),
            format!("get t select user, count(*) where {past} or n = 3 group user"),
        ),
        (
            "get u order id lookup s on user = name where expired() order id limit 3".into(),
            format!("get u order id lookup t on user = name where {past} order id limit 3"),
        ),
        (
            "get u where name in (get s select user where expired() and n = 4) order id".into(),
            format!("get u where name in (get t select user where {past} and n = 4) order id"),
        ),
        // An inner `get` of a collection whose rows expire, under an outer
        // `expired()`: the inner one leaves them out as any read does.
        (
            "get s where expired() and user in (get s select user where n = 4) count".into(),
            format!(
                "get t where {past} and user in (get t select user where {alive} and n = 4) count"
            ),
        ),
    ] {
        assert_eq!(rows(&db, &s, &[]), rows(&db, &t, &[]), "\n{s}\n{t}");
    }
    let expired = count(&db, "get s where expired() count");
    assert!(expired > 0);
    // A range of the ordered index, not a scan.
    let plan = rows(&db, "explain get s where expired()", &[]);
    assert!(
        format!("{plan:?}").contains("the ordered index on seen"),
        "{plan:?}"
    );

    // `set` and `del` reach them by it; the living are left alone.
    let living = count(&db, "get s count");
    run(&mut db, "set s {n: 1000} where expired() and n = 2", &[]);
    assert_eq!(
        count(&db, "get s where expired() and n = 1000 count"),
        count(&db, &format!("get t where {past} and n = 2 count"))
    );
    run(&mut db, "del s where expired()", &[]);
    assert_eq!(count(&db, "get s where expired() count"), 0);
    assert_eq!(count(&db, "get s count"), living);

    // A collection with no `@ttl` has nothing past its time.
    let stmt = fenec_ql::parse_one("get u where expired()").unwrap();
    let e = db.query(&stmt, &[]).unwrap_err();
    assert!(e.to_string().contains("has no `@ttl`"), "{e}");
}

/// The reaper: a hold lapsed is read with `expired()`, its money given back
/// and the hold deleted in one block, the delete required -- so of two
/// reapers, or a reaper and a capture racing it, one gives it back and the
/// other is refused whole.
#[test]
fn a_reaper_gives_back_once() {
    let mut db = Database::new();
    db.set_clock(Some(NOW));
    run(
        &mut db,
        "create collection accounts (ext text @unique, balance int, held int);
         create collection holds (account text @hash, amount int, until timestamp @ttl(1ms));
         put accounts {ext: \"a\", balance: 100, held: 30};
         put holds [{account: \"a\", amount: 10, until: $1}, {account: \"a\", amount: 20, until: $2}]",
        &[Value::Timestamp(NOW - 2), Value::Timestamp(NOW + 60_000)],
    );
    // The lapsed one is out of every read, and `expired()` finds it.
    assert_eq!(count(&db, "get holds count"), 1);
    let lapsed = rows(
        &db,
        "get holds select id, account, amount where expired()",
        &[],
    );
    assert_eq!(lapsed.rows.len(), 1);
    let (id, amount) = (
        lapsed.rows[0].values[0].clone(),
        lapsed.rows[0].values[2].clone(),
    );
    assert_eq!(amount, Value::Int(10));
    let reap = fenec_ql::parse(
        "del holds where expired() and id = $1 require 1;
         set accounts {held: held - $2} where ext = $3 and held >= $2 require 1",
    )
    .unwrap();
    let params = [id, amount, Value::Text("a".into())];
    let give_back = |db: &mut Database| -> fenec_core::error::Result<()> {
        db.begin()?;
        for s in &reap {
            if let Err(e) = db.execute_with(s, &params) {
                db.rollback();
                return Err(e);
            }
        }
        db.commit()
    };
    give_back(&mut db).unwrap();
    let e = give_back(&mut db).unwrap_err();
    assert!(matches!(e, Error::Unmet(_)), "{e}");
    assert_eq!(
        rows(&db, "get accounts select held", &[]).rows[0].values[0],
        Value::Int(20)
    );
    db.set_clock(Some(0));
    assert_eq!(count(&db, "get holds count"), 1, "the lapsed hold is gone");
}
