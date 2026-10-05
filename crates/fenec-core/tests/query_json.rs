//! `Database::query_json` writes a plain `get`'s rows straight from the
//! stored documents; it must write, byte for byte, what `query`'s rows
//! written out by `json::rows_array_into` are -- over every type a field
//! holds, a field added after a document was written, a dropped one, a
//! mapped file and the rows written since -- and leave every other
//! statement to `query`.

use fenec_core::prelude::*;

fn run(db: &mut Database, sql: &str) {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// Both ways, with `params`; `None` when `query_json` leaves it to `query`.
fn both(db: &Database, sql: &str, params: &[Value]) -> Option<(String, String)> {
    let stmt = fenec_ql::parse_one(sql).expect("parse");
    let mut fast = String::from("unchanged");
    let n = db
        .query_json(&stmt, params, &mut fast)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let resp = db
        .query(&stmt, params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let rs = resp.rows().expect("rows");
    let Some(n) = n else {
        assert_eq!(fast, "unchanged", "{sql}: written to and refused");
        return None;
    };
    assert_eq!(n, rs.rows.len(), "{sql}");
    let mut slow = String::from("unchanged");
    fenec_core::json::rows_array_into(&mut slow, rs);
    Some((fast, slow))
}

fn same(db: &Database, sql: &str, params: &[Value]) {
    let (fast, slow) = both(db, sql, params).unwrap_or_else(|| panic!("{sql}: not taken"));
    assert_eq!(fast, slow, "{sql}");
}

fn fixture(db: &mut Database) {
    run(
        db,
        "create collection t (title text, n int, x float, ok bool, at timestamp, \
         v vector<3> @hnsw(cosine), meta json, tags [text], sp sparse<8>)",
    );
    run(db, "create index on t (n) @sorted");
    for i in 0..60i64 {
        let title = match i % 5 {
            0 => "plain".to_string(),
            1 => "quote \" and \\ slash".to_string(),
            2 => "line\nbreak\ttab\u{1}ctl".to_string(),
            3 => "ünïcødé 😀 şğı".repeat(3),
            _ => format!("a longer title, number {i}, past a word or two"),
        };
        let title = fenec_core::json::to_string(&Value::Text(title));
        let doc = if i % 7 == 0 {
            format!("{{id: {}, title: {title}}}", i + 1)
        } else {
            format!(
                "{{id: {}, title: {title}, n: {}, x: {}, ok: {}, at: {}, v: [{}, 0.5, -1], \
                 meta: {{\"a\": {i}, \"b\": [1, \"two\", null]}}, tags: [\"t{i}\", \"u\"], \
                 sp: '{{1:0.5,3:{i}}}/8'}}",
                i + 1,
                i * 3 - 40,
                i as f64 / 3.0,
                i % 2 == 0,
                1_700_000_000_000i64 + i * 1000,
                i as f32 / 7.0,
            )
        };
        run(db, &format!("insert t {doc}"));
    }
}

fn cases(db: &Database) {
    same(db, "get t", &[]);
    same(db, "get t where id >= $1 limit 7", &[Value::Int(20)]);
    same(db, "get t where id >= 59 limit 50", &[]);
    same(db, "get t where id >= 1000 limit 5", &[]);
    same(db, "get t where n > 0 order n desc limit 9 offset 2", &[]);
    same(db, "get t where title ~ 'plain' order id desc", &[]);
    same(db, "get t limit 0", &[]);
    same(db, "get t offset 55", &[]);
    same(db, "get t where id = 3", &[]);
    same(db, "get t where id = 4000", &[]);
    same(
        db,
        "get t select id, title, at where ok = true limit 5",
        &[],
    );
    same(db, "get t select title, n limit 5", &[]);
    same(db, "get t select id, meta, tags, sp limit 9", &[]);
    // Not in the payload's order, a path, the id twice: `query`'s.
    for sql in [
        "get t select n, title limit 3",
        "get t select meta.a limit 3",
        "get t select id, id, title limit 3",
        "get t count",
        "get t select count(*)",
        "get t near v [0.1, 0.5, -1] limit 3",
        "get t where id <= 3 facet ok",
    ] {
        assert!(both(db, sql, &[]).is_none(), "{sql}: taken");
    }
}

#[test]
fn a_plain_get_is_written_as_its_rows_would_be() {
    let mut db = Database::new();
    fixture(&mut db);
    cases(&db);
    // A field added after the documents ends past them, and reads `null`.
    run(&mut db, "alter collection t add field late text");
    run(&mut db, "insert t {id: 100, title: 'late', late: 'here'}");
    cases(&db);
    // A dropped field's place is passed over.
    run(&mut db, "alter collection t drop field x");
    cases(&db);
    same(&db, "get t select id, ok, late where id >= 95", &[]);
}

#[test]
fn a_mapped_file_and_the_rows_since_are_written_alike() {
    let dir = std::env::temp_dir().join(format!("fenec-query-json-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("q.fenec");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = fenec_core::fs::open(&path).unwrap();
        fixture(&mut db);
        db.sync().unwrap();
    }
    let mut db = fenec_core::fs::open(&path).unwrap();
    cases(&db);
    run(
        &mut db,
        "set t {title: 'rewritten \"here\"'} where id <= 10",
    );
    cases(&db);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rows_past_their_time_are_left_out_as_query_leaves_them() {
    const NOW: i64 = 1_800_000_000_000;
    let mut db = Database::new();
    db.set_clock(Some(NOW));
    run(
        &mut db,
        "create collection s (name text, seen timestamp @ttl(30m))",
    );
    for i in 0..40i64 {
        // Every third row an hour old, past its time.
        let seen = NOW - if i % 3 == 0 { 3_600_000 } else { 60_000 };
        run(
            &mut db,
            &format!("insert s {{id: {}, name: 'n{i}', seen: {seen}}}", i + 1),
        );
    }
    let (fast, _) = both(&db, "get s", &[]).unwrap();
    assert!(
        !fast.contains("\"n0\"") && fast.contains("\"n1\""),
        "{fast}"
    );
    same(&db, "get s", &[]);
    same(&db, "get s where id >= 5 limit 10", &[]);
    same(&db, "get s where id = 4", &[]);
}
