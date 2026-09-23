//! Time: epoch milliseconds <-> calendar.
//!
//! `timestamp` values are stored as **UTC epoch milliseconds** (i64).
//! Milliseconds because that is the browser side's natural unit
//! (`Date.now()`), and an i64 spans +-2.9e8 years -- no sub-unit overflow.
//!
//! The calendar conversion is the method from Howard Hinnant's
//! "chrono-Compatible Low-Level Date Algorithms": integer arithmetic,
//! leap-year and century rules included, no tables and no dependencies.

use crate::error::{Error, Result};

/// Day count (1970-01-01 = 0) -> (year, month, day).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) -> day count (1970-01-01 = 0).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Epoch ms -> (day, ms within the day). Correct for negative times too:
/// Rust's `%` can leave a negative remainder, which is corrected here.
fn split(ms: i64) -> (i64, i64) {
    let day = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    (day, rem)
}

fn parts(ms: i64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let (day, rem) = split(ms);
    let (y, mo, d) = civil_from_days(day);
    (
        y,
        mo,
        d,
        (rem / 3_600_000) as u32,
        (rem / 60_000 % 60) as u32,
        (rem / 1000 % 60) as u32,
        (rem % 1000) as u32,
    )
}

/// ISO-8601, UTC: `2026-09-19T12:34:56.789Z`. Milliseconds omitted when zero.
pub fn format_iso(ms: i64) -> String {
    let (y, mo, d, h, mi, s, milli) = parts(ms);
    if milli == 0 {
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
    } else {
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
    }
}

/// PostgreSQL's `DateStyle = ISO` output format: `2026-09-19 12:34:56.789+00`.
///
/// This is the format used on the wire, not ISO-8601: the psycopg and pgjdbc
/// parsers assume input in the server's own format and may reject text that
/// uses `T` and `Z`.
pub fn format_pg(ms: i64) -> String {
    let (y, mo, d, h, mi, s, milli) = parts(ms);
    if milli == 0 {
        format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}+00")
    } else {
        format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{milli:03}+00")
    }
}

fn num(s: &str) -> Result<i64> {
    s.parse::<i64>()
        .map_err(|_| Error::Type(format!("expected a number in the date: `{s}`")))
}

/// Epoch ms from text. Accepted forms:
/// `2026-09-19`, `2026-09-19T12:34:56`, `... 12:34:56.789`, trailing `Z`,
/// `+03:00`, `+0300` or `+03`. UTC is assumed when no time zone is given.
pub fn parse(text: &str) -> Result<i64> {
    let t = text.trim();
    let bad = || Error::Type(format!("invalid timestamp: `{text}`"));

    // Time zone suffix.
    let (body, offset_min) = if let Some(rest) = t.strip_suffix('Z').or(t.strip_suffix('z')) {
        (rest, 0i64)
    } else {
        // Only characters past the 10th are scanned, so the `-` inside the
        // date part is not mistaken for a sign.
        let idx = t
            .char_indices()
            .skip(10)
            .find(|(_, c)| *c == '+' || *c == '-')
            .map(|(i, _)| i);
        match idx {
            Some(i) => {
                let (b, off) = t.split_at(i);
                let sign = if off.starts_with('-') { -1 } else { 1 };
                let digits: String = off[1..].chars().filter(|c| c.is_ascii_digit()).collect();
                let m = match digits.len() {
                    2 => num(&digits)? * 60,
                    4 => num(&digits[..2])? * 60 + num(&digits[2..])?,
                    _ => return Err(bad()),
                };
                (b, sign * m)
            }
            None => (t, 0),
        }
    };

    let body = body.trim();
    let (date, time) = match body.split_once(['T', 't', ' ']) {
        Some((d, t)) => (d, t.trim()),
        None => (body, ""),
    };

    let mut dp = date.split('-');
    let (y, mo, d) = match (dp.next(), dp.next(), dp.next(), dp.next()) {
        (Some(y), Some(m), Some(d), None) => (num(y)?, num(m)? as u32, num(d)? as u32),
        _ => return Err(bad()),
    };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return Err(bad());
    }

    let (mut h, mut mi, mut s, mut milli) = (0i64, 0i64, 0i64, 0i64);
    if !time.is_empty() {
        let mut tp = time.split(':');
        h = num(tp.next().ok_or_else(bad)?)?;
        if let Some(v) = tp.next() {
            mi = num(v)?;
        }
        if let Some(v) = tp.next() {
            match v.split_once('.') {
                Some((sec, frac)) => {
                    s = num(sec)?;
                    // The fraction is padded/truncated to 3 digits (ms resolution).
                    let f: String = frac.chars().take(3).collect();
                    milli = num(&f)? * 10i64.pow(3 - f.len() as u32);
                }
                None => s = num(v)?,
            }
        }
        if tp.next().is_some() || h > 23 || mi > 59 || s > 60 {
            return Err(bad());
        }
    }

    let day = days_from_civil(y, mo, d);
    // A day that is not on the calendar (like Feb 31) round-trips differently.
    if civil_from_days(day) != (y, mo, d) {
        return Err(bad());
    }
    Ok((day * 86_400_000) + h * 3_600_000 + mi * 60_000 + s * 1000 + milli - offset_min * 60_000)
}

/// Current time (epoch ms).
///
/// There is no clock on `wasm32-unknown-unknown` -- `SystemTime::now()` panics.
/// On that target `now()` returns an error; in the browser the caller passes
/// the time in (as a parameter, via `Date.now()`).
#[cfg(not(target_arch = "wasm32"))]
pub fn now_ms() -> Result<i64> {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => Ok(d.as_millis() as i64),
        Err(e) => Ok(-(e.duration().as_millis() as i64)),
    }
}

#[cfg(target_arch = "wasm32")]
pub fn now_ms() -> Result<i64> {
    Err(Error::Query(
        "now() is unavailable on this target: pass the time in as a parameter (Date.now())".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_roundtrip_covers_leap_rules() {
        // 1600 (leap), 1700/1800/1900 (not), 2000 (leap), 2400 (leap)
        for (y, m, d) in [
            (1600, 2, 29),
            (1900, 2, 28),
            (2000, 2, 29),
            (2024, 2, 29),
            (2026, 9, 19),
            (1969, 12, 31),
            (1970, 1, 1),
            (2400, 2, 29),
        ] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(days), (y, m, d), "{y}-{m}-{d}");
        }
        // Consecutive days must be consecutive numbers.
        let mut prev = days_from_civil(1999, 12, 31);
        for (y, m, d) in [(2000, 1, 1), (2000, 1, 2)] {
            let cur = days_from_civil(y, m, d);
            assert_eq!(cur, prev + 1);
            prev = cur;
        }
    }

    #[test]
    fn parse_and_format_roundtrip() {
        for s in [
            "1970-01-01T00:00:00Z",
            "2026-09-19T12:34:56Z",
            "2026-09-19T12:34:56.789Z",
            "1969-07-20T20:17:40Z",
        ] {
            assert_eq!(format_iso(parse(s).unwrap()), s);
        }
        // Equivalent spellings land on the same instant.
        let t = parse("2026-09-19T12:34:56Z").unwrap();
        assert_eq!(parse("2026-09-19 12:34:56").unwrap(), t);
        assert_eq!(parse("2026-09-19T15:34:56+03:00").unwrap(), t);
        assert_eq!(parse("2026-09-19T15:34:56+0300").unwrap(), t);
        assert_eq!(parse("2026-09-19T09:34:56-03").unwrap(), t);
        assert_eq!(
            parse("2026-09-19").unwrap(),
            t - 12 * 3_600_000 - 34 * 60_000 - 56_000
        );
        // Fractional seconds are aligned to 3 digits.
        assert_eq!(parse("2026-09-19T00:00:00.5Z").unwrap() % 1000, 500);
        assert_eq!(parse("2026-09-19T00:00:00.123456Z").unwrap() % 1000, 123);
    }

    #[test]
    fn invalid_timestamps_are_rejected() {
        for s in [
            "2026-02-31T00:00:00Z", // not on the calendar
            "2026-13-01",
            "2026-09-19T25:00:00Z",
            "yesterday",
            "2026/09/19",
            "2026-09-19T12:34:56+9",
        ] {
            assert!(parse(s).is_err(), "`{s}` was accepted");
        }
    }

    #[test]
    fn negative_epoch_formats_correctly() {
        // 1969-12-31T23:59:59.999Z -> -1 ms
        assert_eq!(parse("1969-12-31T23:59:59.999Z").unwrap(), -1);
        assert_eq!(format_iso(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(format_pg(0), "1970-01-01 00:00:00+00");
    }
}
