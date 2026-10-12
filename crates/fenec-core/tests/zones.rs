//! `bucket(at, 1d, 'Europe/Berlin')`: buckets by a zone's clock, held to
//! zoneinfo's (`zones.txt`, written by `tools/tz/gen.py` from Python's
//! zoneinfo over the same tzdata the table was made of): every zone's
//! offsets, sampled over 45 years and hashed, and the start of each
//! interval around the transitions of twelve zones -- daylight time,
//! midnight skipped (Havana, Santiago, Beirut), a day skipped (Apia, 2011),
//! half an hour (Lord Howe), daylight time in winter (Dublin), Gaza's rules
//! -- through every way a bucket is worked out: a select list's, a `where`'s
//! and a group counted off an ordered index.

use fenec_core::prelude::*;
use fenec_core::query::Interval;
use fenec_core::tz;

const INTERVALS: [&str; 7] = ["15m", "40m", "1h", "3h", "1d", "1w", "1mo"];

fn exec(db: &mut Database, sql: &str) {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn rows(db: &Database, sql: &str, params: &[Value]) -> Vec<Vec<Value>> {
    let r = db
        .query(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| r.values.clone())
        .collect()
}

fn error(db: &Database, sql: &str, params: &[Value]) -> String {
    match fenec_ql::parse_one(sql) {
        Err(e) => e.to_string(),
        Ok(stmt) => db.query(&stmt, params).expect_err(sql).to_string(),
    }
}

fn reference() -> &'static str {
    include_str!("zones.txt")
}

/// A zone by its name, through the table.
fn zone(name: &str) -> tz::Zone {
    tz::zone_of(&Value::Text(name.into())).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// Every zone's offsets, as zoneinfo gives them, at 2000-01-01 and every
/// 11h 13m 7s after it up to 2045 -- a time of day that walks round the
/// clock -- hashed as the generator hashed them.
#[test]
fn every_zone_has_zoneinfos_offsets() {
    let from = 946_684_800_000i64;
    let end = 2_366_841_600_000i64;
    let step = (11 * 3600 + 13 * 60 + 7) * 1000;
    let mut names = 0;
    for line in reference().lines() {
        let mut parts = line.split(' ');
        if parts.next() != Some("offsets") {
            continue;
        }
        let (name, want) = (parts.next().unwrap(), parts.next().unwrap());
        let z = zone(name);
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut t = from;
        while t < end {
            let minutes = (z.offset_at(t) / 60_000) as i16;
            for b in minutes.to_le_bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
            t += step;
        }
        assert_eq!(format!("{h:016x}"), want, "{name}");
        names += 1;
    }
    assert!(names > 500, "{names} zones");
}

/// Each `bucket` line: a zone, an instant, and for each interval how far
/// past its bucket's start the instant is.
fn buckets() -> Vec<(&'static str, i64, Vec<i64>)> {
    let mut out = Vec::new();
    for line in reference().lines() {
        let mut parts = line.split(' ');
        if parts.next() != Some("bucket") {
            continue;
        }
        let name = parts.next().unwrap();
        let t: i64 = parts.next().unwrap().parse().unwrap();
        let back: Vec<i64> = parts.map(|p| p.parse().unwrap()).collect();
        out.push((name, t, back));
    }
    out
}

#[test]
fn a_bucket_starts_where_zoneinfo_says() {
    let lines = buckets();
    assert!(lines.len() > 500, "{} lines", lines.len());
    for (name, t, back) in &lines {
        let z = zone(name);
        for (iv, back) in INTERVALS.iter().zip(back) {
            let interval = Interval::of(&Value::Text((*iv).into())).unwrap();
            let got = z.bucket(interval, *t).unwrap();
            assert_eq!(t - got, *back, "{name} {iv} at {t}");
        }
    }
}

/// The same starts through FenecQL: a select list's `bucket` (bound once
/// a query), a `where`'s (the registry's, a call a row), and a count by
/// bucket read off an ordered index against the rows read.
#[test]
fn every_way_of_working_a_bucket_out_agrees() {
    type Points<'a> = Vec<(i64, &'a Vec<i64>)>;
    let lines = buckets();
    let mut by_zone: Vec<(&str, Points)> = Vec::new();
    for (name, t, back) in &lines {
        match by_zone.iter_mut().find(|(n, _)| n == name) {
            Some((_, v)) => v.push((*t, back)),
            None => by_zone.push((name, vec![(*t, back)])),
        }
    }
    for (name, points) in &by_zone {
        let (mut ix, mut plain) = (Database::new(), Database::new());
        exec(
            &mut ix,
            "create collection ev (at timestamp @sorted, n int)",
        );
        exec(&mut plain, "create collection ev (at timestamp, n int)");
        for (i, (t, _)) in points.iter().enumerate() {
            for db in [&mut ix, &mut plain] {
                exec(db, &format!("put ev {{at: {t}, n: {i}}}"));
            }
        }
        let zone = Value::Text((*name).into());
        for (k, iv) in INTERVALS.iter().enumerate() {
            let interval = Value::Text((*iv).into());
            // A select list's, each row's against the reference.
            let got = rows(
                &plain,
                "get ev select n, bucket(at, $1, $2) as b order n",
                &[interval.clone(), zone.clone()],
            );
            for (row, (t, back)) in got.iter().zip(points) {
                assert_eq!(
                    row[1],
                    Value::Timestamp(t - back[k]),
                    "{name} {iv} at {t}, a select list's"
                );
            }
            // A `where`'s: the rows in the bucket of the first point.
            let (t0, back0) = points[0];
            let start = t0 - back0[k];
            let want: Vec<Value> = (points.iter().enumerate())
                .filter(|(_, (t, b))| t - b[k] == start)
                .map(|(i, _)| Value::Int(i as i64))
                .collect();
            let got: Vec<Value> = rows(
                &plain,
                "get ev select n where bucket(at, $1, $2) = $3 order n",
                &[interval.clone(), zone.clone(), Value::Timestamp(start)],
            )
            .into_iter()
            .map(|r| r[0].clone())
            .collect();
            assert_eq!(got, want, "{name} {iv}, a where's");
            // Counted by bucket over a range of the index, against the rows
            // read: the same groups, the same counts.
            let sql = "get ev select bucket(at, $1, $2) as b, count() where at >= $3 and at < $4 \
                       group b";
            let range = [
                interval.clone(),
                zone.clone(),
                Value::Timestamp(946_684_800_000),
                Value::Timestamp(4_102_444_800_000),
            ];
            let walked = rows(&ix, sql, &range);
            let read = rows(&plain, sql, &range);
            assert_eq!(walked, read, "{name} {iv}, counted off the index");
            let plan = rows(&ix, &format!("explain {sql}"), &range);
            assert!(
                format!("{plan:?}").contains("no row read"),
                "{name} {iv}: {plan:?}"
            );
        }
    }
}

/// Merrow keeps UTC+3 the year round: a local day by its zone is the day
/// the offset written into every statement made, and an offset written
/// out is a zone too.
#[test]
fn a_zone_without_daylight_time_is_its_offset() {
    let mut db = Database::new();
    exec(&mut db, "create collection ev (at timestamp)");
    for t in [
        "\"2026-03-29T20:59:59Z\"",
        "\"2026-03-29T21:00:00Z\"",
        "\"2026-10-25T00:30:00Z\"",
        "\"2026-12-31T21:00:00Z\"",
    ] {
        exec(&mut db, &format!("put ev {{at: {t}}}"));
    }
    let named = rows(
        &db,
        "get ev select bucket(at, 1d, 'Europe/Istanbul') as d, bucket(at, 1mo, \"+03:00\") as m order id",
        &[],
    );
    let written = rows(
        &db,
        "get ev select bucket(at + 10800000, 1d) - 10800000 as d order id",
        &[],
    );
    for (n, w) in named.iter().zip(&written) {
        assert_eq!(n[0], w[0]);
    }
    // 2026-12-31 21:00 UTC is January's first moment in Istanbul.
    assert_eq!(named[3][1], Value::Timestamp(1_798_750_800_000));
    for (a, b) in [
        ("+05:30", "Asia/Kolkata"),
        ("-0300", "America/Sao_Paulo"),
        ("+00", "UTC"),
        ("Z", "Etc/UTC"),
        ("utc", "europe/london"),
    ] {
        let got = rows(
            &db,
            "get ev select bucket(at, 1d, $1) as a, bucket(at, 1d, $2) as b order id",
            &[Value::Text(a.into()), Value::Text(b.into())],
        );
        // London is UTC in winter; these instants are not in its summer
        // but the one of March 29, which is.
        for (i, r) in got.iter().enumerate() {
            if b == "europe/london" && i < 3 {
                continue;
            }
            assert_eq!(r[0], r[1], "{a} against {b}, row {i}");
        }
    }
}

/// A day by Berlin's clock is 23 hours long in March and 25 in October.
#[test]
fn a_day_by_a_zones_clock_is_the_days_length() {
    let mut db = Database::new();
    exec(&mut db, "create collection ev (at timestamp)");
    let mut t = 1_774_656_000_000i64; // 2026-03-28T00:00Z
    while t < 1_774_656_000_000 + 4 * 86_400_000 {
        exec(&mut db, &format!("put ev {{at: {t}}}"));
        t += 3_600_000;
    }
    let mut t = 1_792_800_000_000i64; // 2026-10-24T00:00Z
    while t < 1_792_800_000_000 + 4 * 86_400_000 {
        exec(&mut db, &format!("put ev {{at: {t}}}"));
        t += 3_600_000;
    }
    let got = rows(
        &db,
        "get ev select bucket(at, 1d, 'Europe/Berlin') as day, count() as hours group day order day",
        &[],
    );
    let hours = |day: i64| {
        let row = got.iter().find(|r| r[0] == Value::Timestamp(day));
        row.map(|r| r[1].clone())
    };
    // Each run's first and last days are cut by the run; between them
    // 2026-03-29, from 23:00 UTC the day before, has 23 hours, and
    // 2026-10-25, from 22:00, has 25.
    assert_eq!(hours(1_774_738_800_000), Some(Value::Int(23)), "{got:?}");
    assert_eq!(hours(1_774_821_600_000), Some(Value::Int(24)), "{got:?}");
    assert_eq!(hours(1_792_879_200_000), Some(Value::Int(25)), "{got:?}");
}

#[test]
fn what_is_no_zone_is_refused() {
    let mut db = Database::new();
    exec(&mut db, "create collection ev (at timestamp)");
    exec(&mut db, "put ev {at: \"1999-12-31T12:00:00Z\"}");
    for (zone, says) in [
        ("Mars/Phobos", "an IANA name"),
        ("+3", "an IANA name"),
        ("+25:00", "an IANA name"),
        ("+03:60", "an IANA name"),
    ] {
        let e = error(
            &db,
            "get ev select bucket(at, 1d, $1) as d",
            &[Value::Text(zone.into())],
        );
        assert!(e.contains(says), "{zone}: {e}");
    }
    let e = error(&db, "get ev select bucket(at, 1d, 3) as d", &[]);
    assert!(e.contains("an IANA name"), "{e}");
    // A time before the table's first is refused, not answered by rules it
    // does not hold; an offset written out answers.
    let e = error(
        &db,
        "get ev select bucket(at, 1d, 'Europe/Berlin') as d",
        &[],
    );
    assert!(e.contains("from 2000"), "{e}");
    let got = rows(&db, "get ev select bucket(at, 1d, '+01:00') as d", &[]);
    assert_eq!(got[0][0], Value::Timestamp(946_594_800_000));
}
