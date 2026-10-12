//! Time zones, for `bucket(at, 1d, 'Europe/Berlin')`: a fixed offset
//! written out (`'+03:00'`), or one of IANA's zones by its name, read from
//! a table of their rules since 2000 (`tz/zones.bin`, written by
//! `tools/tz/gen.py` out of the system's compiled tzdata).
//!
//! A zone is the offset it had at the start of 1999, the transitions since
//! that its current rule does not make, and that rule -- the POSIX TZ
//! string a TZif file ends with -- from the year it took over: 2026c's
//! 597 names are 196 tables, 30.6 KB, 10.1 KB gzip, 8.5 KB brotli. Native
//! builds carry it; the browser module is handed it as it is handed the
//! collation data (`add_chunk`), since a page that buckets by a zone is
//! rare and the table would have been 8.5 KB brotli of every page: a
//! statement naming a zone it has not got is refused with a note
//! (`take_missing`), and runs again once fetched.
//!
//! A zone with daylight saving makes a day of 23 or 25 hours, and so a
//! bucket in local time is defined by the zone's clock, not by a length:
//!
//! - a day, a week, a month or a year starts at the first instant whose
//!   clock reads its first moment or later -- local midnight, or where a
//!   clock skips midnight the instant it jumps past it;
//! - a shorter interval starts at the last instant, at or before the time,
//!   at which the clock read the interval's start or jumped past it: the
//!   hour a clock shows twice is two buckets, and one it skips is none.
//!
//! Each is a walk over the zone's pieces -- stretches of one offset --
//! near the time, a handful at most.

// Built whatever the features, so that a binding asks one name whether
// the module was handed the table; without `analytics` no `bucket` reads
// a zone, and the rest is code the compiler drops.
#![cfg_attr(not(feature = "analytics"), allow(dead_code))]

use crate::error::{Error, Result};
use crate::query::Interval;
use crate::time::{civil_from_days, days_from_civil};
use crate::value::Value;

const DAY: i64 = 86_400_000;

/// What `bucket` reads local time by.
#[derive(Debug, Clone)]
pub enum Zone {
    /// A fixed offset east of UTC, in milliseconds.
    Fixed(i64),
    Rules(Rules),
}

/// A zone of the table: its transitions since the start of 1999, and the
/// rule after them.
#[derive(Debug, Clone)]
pub struct Rules {
    /// The first instant answered for: a time before it is refused rather
    /// than answered in rules the table does not hold.
    from: i64,
    /// The offset before the first transition.
    init: i64,
    /// Each transition's instant and the offset from it, ascending.
    trans: Vec<(i64, i64)>,
    rule: Option<Rule>,
    /// Where the rule takes over from the transitions.
    rule_from: i64,
    /// The piece a time was last asked of: rows bucketed in time order
    /// ask one piece for months of them. Worked out for each, a rule's
    /// three years of changes a time, a week's hourly counts by Berlin's
    /// clock off a million rows' index took 252 ms against 22 in UTC.
    last: std::cell::Cell<Piece>,
}

/// A POSIX TZ string's rule: standard and daylight offsets, and when
/// daylight time starts (in standard time) and ends (in daylight time).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rule {
    std: i64,
    dst: i64,
    start: When,
    end: When,
}

/// `Mm.w.d/t`: the `w`th (5 the last) weekday `d` (0 Sunday) of month `m`,
/// at `t` past its midnight -- milliseconds, which may pass the day.
#[derive(Debug, Clone, Copy, PartialEq)]
struct When {
    month: u32,
    week: u32,
    wday: i64,
    at: i64,
}

impl When {
    /// The instant it names in `year`, read in a clock `off` east of UTC.
    fn instant(self, year: i64, off: i64) -> i64 {
        let first = days_from_civil(year, self.month, 1);
        // 1970-01-01 was a Thursday.
        let wday = (first + 4).rem_euclid(7);
        let mut day = (self.wday - wday).rem_euclid(7) + 7 * (self.week as i64 - 1);
        let (ny, nm) = match self.month {
            12 => (year + 1, 1),
            m => (year, m + 1),
        };
        let len = days_from_civil(ny, nm, 1) - first;
        while day >= len {
            day -= 7;
        }
        (first + day) * DAY + self.at - off
    }
}

impl Rule {
    /// This year's two changes, in UTC: daylight time's start and end.
    fn changes(&self, year: i64) -> (i64, i64) {
        (
            self.start.instant(year, self.std),
            self.end.instant(year, self.dst),
        )
    }

    fn offset_at(&self, t: i64) -> i64 {
        let (on, off) = self.changes(civil_from_days(t.div_euclid(DAY)).0);
        let daylight = match on < off {
            true => on <= t && t < off,
            false => !(off <= t && t < on),
        };
        if daylight {
            self.dst
        } else {
            self.std
        }
    }
}

/// A stretch of time of one offset: from `start` to `end`, exclusive.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Piece {
    start: i64,
    end: i64,
    off: i64,
}

impl Zone {
    /// The offset east of UTC at `t`, in milliseconds.
    pub fn offset_at(&self, t: i64) -> i64 {
        match self {
            Zone::Fixed(o) => *o,
            Zone::Rules(_) => self.piece(t).off,
        }
    }

    /// The piece `t` is in.
    fn piece(&self, t: i64) -> Piece {
        match self {
            Zone::Fixed(o) => Piece {
                start: i64::MIN,
                end: i64::MAX,
                off: *o,
            },
            Zone::Rules(r) => {
                let held = r.last.get();
                if held.start <= t && t < held.end {
                    return held;
                }
                let p = Piece {
                    start: r.last_change(t),
                    end: r.next_change(t),
                    off: r.offset_at(t),
                };
                r.last.set(p);
                p
            }
        }
    }

    /// The start of the bucket of `interval` `t` falls in, by this zone's
    /// clock (the module's doc says how).
    pub fn bucket(&self, interval: Interval, t: i64) -> Result<i64> {
        if let Zone::Rules(r) = self {
            if t < r.from {
                return Err(Error::Query(format!(
                    "`bucket` knows a zone's rules from {} on: give an offset, such as '+03:00', \
                     for a time before",
                    crate::time::format_iso(r.from)
                )));
            }
        }
        let w = interval.start(t.saturating_add(self.offset_at(t)));
        Ok(match interval.whole_days() {
            true => self.first_at_or_after(w),
            false => self.last_arrival(w, t),
        })
    }

    /// The first instant whose clock reads `w` or later. Every offset is
    /// within a day of UTC, so the pieces from a day before `w` hold it:
    /// in the first of them that reaches `w`, where it does or where the
    /// piece starts.
    fn first_at_or_after(&self, w: i64) -> i64 {
        let mut p = self.piece(w.saturating_sub(DAY + DAY / 12));
        // A handful of pieces at most; bounded, should a table be bad.
        for _ in 0..16 {
            let at = p.start.max(w.saturating_sub(p.off));
            if at < p.end {
                return at;
            }
            p = self.piece(p.end);
        }
        w
    }

    /// The last instant at or before `t` at which the clock read `w`, or
    /// jumped past it. Going back a piece at a time from `t`'s: in a piece
    /// the clock reads `w` once, at `w - off`; where a piece's clock starts
    /// past `w`, the one before it either stayed below `w` -- the clock
    /// jumped past it where this one starts -- or read it.
    fn last_arrival(&self, w: i64, t: i64) -> i64 {
        let mut p = self.piece(t);
        for _ in 0..16 {
            let at = w.saturating_sub(p.off);
            if at >= p.start {
                return at;
            }
            let q = self.piece(p.start - 1);
            if (p.start - 1).saturating_add(q.off) < w {
                return p.start;
            }
            p = q;
        }
        w
    }
}

impl Rules {
    fn offset_at(&self, t: i64) -> i64 {
        let i = self.trans.partition_point(|&(u, _)| u <= t);
        match (&self.rule, i == self.trans.len() && t >= self.rule_from) {
            (Some(r), true) => r.offset_at(t),
            _ if i == 0 => self.init,
            _ => self.trans[i - 1].1,
        }
    }

    /// The rule's changes near `t`: the years around it, those it answers.
    fn rule_changes(&self, t: i64) -> [i64; 6] {
        let mut out = [i64::MIN; 6];
        if let Some(r) = &self.rule {
            let y = civil_from_days(t.div_euclid(DAY)).0;
            // The rule's own from where it takes over -- its first change
            // is one -- and after the last transition kept.
            let last = self.trans.last().map_or(i64::MIN, |l| l.0);
            let ours = |c: i64| match c >= self.rule_from && c > last {
                true => c,
                false => i64::MIN,
            };
            for (k, year) in [y - 1, y, y + 1].into_iter().enumerate() {
                let (a, b) = r.changes(year);
                (out[2 * k], out[2 * k + 1]) = (ours(a), ours(b));
            }
        }
        out
    }

    /// The last change at or before `t`, `i64::MIN` where there is none.
    fn last_change(&self, t: i64) -> i64 {
        let i = self.trans.partition_point(|&(u, _)| u <= t);
        let mut last = if i == 0 {
            i64::MIN
        } else {
            self.trans[i - 1].0
        };
        if i == self.trans.len() {
            for c in self.rule_changes(t) {
                if c <= t && c > last {
                    last = c;
                }
            }
        }
        last
    }

    /// The first change after `t`, `i64::MAX` where there is none.
    fn next_change(&self, t: i64) -> i64 {
        let i = self.trans.partition_point(|&(u, _)| u <= t);
        if let Some(&(u, _)) = self.trans.get(i) {
            return u;
        }
        // Before the rule takes over its first change is the next, however
        // many years off: Egypt kept no daylight time from 2015 to 2022.
        if self.rule.is_some() && t < self.rule_from {
            return self.rule_from;
        }
        let mut next = i64::MAX;
        for c in self.rule_changes(t) {
            if c > t && c < next {
                next = c;
            }
        }
        next
    }
}

/// The zone `v` names: an offset -- `'+03:00'`, `'-0530'`, `'+03'`, `'Z'`,
/// `'UTC'` -- or an IANA name the table holds, any case.
pub fn zone_of(v: &Value) -> Result<Zone> {
    let Value::Text(name) = v else {
        return Err(refused(v.type_name()));
    };
    if let Some(off) = fixed(name) {
        return Ok(Zone::Fixed(off));
    }
    match table() {
        Some(t) => t.find(name).ok_or_else(|| refused(&format!("`{name}`"))),
        None => {
            MISSING.store(true, std::sync::atomic::Ordering::Relaxed);
            Err(Error::NotFound(
                "the time zone data zones: hand it to the module (`collation`) and run the \
                 statement again"
                    .into(),
            ))
        }
    }
}

thread_local! {
    /// The zone a call named last, by its name: `bucket(at, 1d, $3)` in a
    /// `where` names it once a row, and a lookup reads the names and an
    /// entry, some microseconds.
    static LAST: std::cell::RefCell<Option<(String, std::rc::Rc<Zone>)>> =
        const { std::cell::RefCell::new(None) };
}

/// [`zone_of`] for a call made a row: the zone the last call named kept.
pub fn cached(v: &Value) -> Result<std::rc::Rc<Zone>> {
    if let Value::Text(name) = v {
        let hit = LAST.with(|l| {
            (l.borrow().as_ref())
                .filter(|(n, _)| n == name)
                .map(|(_, z)| z.clone())
        });
        if let Some(z) = hit {
            return Ok(z);
        }
    }
    let z = std::rc::Rc::new(zone_of(v)?);
    if let Value::Text(name) = v {
        LAST.with(|l| *l.borrow_mut() = Some((name.clone(), z.clone())));
    }
    Ok(z)
}

#[cold]
fn refused(what: &str) -> Error {
    Error::Query(format!(
        "`bucket`'s zone is an IANA name, such as 'Europe/Istanbul', or an offset, such as \
         '+03:00'; found {what}"
    ))
}

/// `+03:00`, `-0530`, `+03`, `Z` or `UTC`: an offset in milliseconds.
fn fixed(s: &str) -> Option<i64> {
    if s == "Z" || s.eq_ignore_ascii_case("utc") {
        return Some(0);
    }
    let sign = match s.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let rest = s.get(1..)?;
    let (h, m) = match rest.len() {
        2 => (rest, "00"),
        4 => (rest.get(..2)?, rest.get(2..)?),
        5 if rest.as_bytes()[2] == b':' => (rest.get(..2)?, rest.get(3..)?),
        _ => return None,
    };
    let two = |d: &str| -> Option<i64> {
        let b = d.as_bytes();
        match (b.first()?, b.get(1)?) {
            (x @ b'0'..=b'9', y @ b'0'..=b'9') => Some(((x - b'0') * 10 + (y - b'0')) as i64),
            _ => None,
        }
    };
    let (h, m) = (two(h)?, two(m)?);
    if h > 18 || m > 59 {
        return None;
    }
    Some(sign * (h * 3_600_000 + m * 60_000))
}

// --------------------------------------------------------------- the table

/// "FNTZ", read little-endian.
const MAGIC: u32 = 0x5A54_4E46;

/// The table's bytes as the build has them.
#[cfg(not(target_arch = "wasm32"))]
fn bytes() -> Option<&'static [u8]> {
    Some(include_bytes!("tz/zones.bin"))
}

#[cfg(target_arch = "wasm32")]
static HANDED: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

#[cfg(target_arch = "wasm32")]
fn bytes() -> Option<&'static [u8]> {
    HANDED.get().map(|b| b.as_slice())
}

static MISSING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the build may lack the table: the browser module, handed it,
/// where a `bucket` reads a zone at all.
pub const PARTIAL: bool = cfg!(all(target_arch = "wasm32", feature = "analytics"));

/// Whether a statement since the last call named a zone the build had no
/// table for; clears the note.
pub fn take_missing() -> bool {
    MISSING.swap(false, std::sync::atomic::Ordering::Relaxed)
}

/// Whether the build has the table.
pub fn loaded() -> bool {
    bytes().is_some()
}

/// Hands the browser module the table, the bytes of `zones.bin`: false
/// for bytes that are not one, another version's among them.
pub fn add_chunk(b: &[u8]) -> bool {
    if !PARTIAL || Table::parse(b).is_none() {
        return false;
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = HANDED.set(b.to_vec());
        true
    }
    #[cfg(not(target_arch = "wasm32"))]
    true
}

static TABLE: std::sync::OnceLock<Option<Table<'static>>> = std::sync::OnceLock::new();

fn table() -> Option<&'static Table<'static>> {
    // Read once there are bytes: a statement asked before the module was
    // handed them keeps nothing that would outlast their coming.
    let b = bytes()?;
    TABLE.get_or_init(|| Table::parse(b)).as_ref()
}

/// The table's header: where each part of it starts. Offsets and rules are
/// records of a fixed size, each zone's entry found through an index, so a
/// lookup reads where the bytes lie and nothing is parsed into lists first.
struct Table<'a> {
    bytes: &'a [u8],
    from: i64,
    kept: i64,
    names: usize,
    offsets: usize,
    rules: usize,
    index: usize,
}

/// The `len` bytes at `at`, `None` past the end.
fn at(b: &[u8], at: usize, len: usize) -> Option<&[u8]> {
    b.get(at..at.checked_add(len)?)
}

fn le(b: &[u8]) -> i64 {
    let mut v = 0i64;
    for (i, x) in b.iter().enumerate() {
        v |= (*x as i64) << (8 * i);
    }
    v
}

/// A LEB128 number at `*p`, moving it past.
fn varint(b: &[u8], p: &mut usize) -> Option<i64> {
    let mut v = 0i64;
    for shift in (0..63).step_by(7) {
        let x = *b.get(*p)?;
        *p += 1;
        v |= ((x & 0x7F) as i64) << shift;
        if x & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

impl<'a> Table<'a> {
    fn parse(b: &'a [u8]) -> Option<Table<'a>> {
        if le(at(b, 0, 4)?) != MAGIC as i64 {
            return None;
        }
        let h = 5 + *b.get(4)? as usize;
        let offsets = h + 21;
        let rules = offsets + 2 * *b.get(h + 20)? as usize + 1;
        let index = rules + 16 * *b.get(rules - 1)? as usize + 2;
        Some(Table {
            bytes: b,
            from: le(at(b, h, 8)?),
            kept: le(at(b, h + 8, 8)?),
            names: le(at(b, h + 16, 4)?) as usize,
            offsets,
            rules,
            index,
        })
    }

    /// Offset `k`, in milliseconds.
    fn offset(&self, k: u8) -> Option<i64> {
        let m = le(at(self.bytes, self.offsets + 2 * k as usize, 2)?) as i16;
        Some(m as i64 * 60_000)
    }

    /// Rule `k`: its offsets, and when daylight time starts and ends.
    fn rule(&self, k: u8) -> Option<Rule> {
        let r = at(self.bytes, self.rules + 16 * k as usize, 16)?;
        let when = |w: &[u8]| When {
            month: w[0] as u32,
            week: w[1] as u32,
            wday: w[2] as i64,
            at: le(&w[3..7]) as i32 as i64 * 1000,
        };
        Some(Rule {
            std: self.offset(r[0])?,
            dst: self.offset(r[1])?,
            start: when(&r[2..9]),
            end: when(&r[9..16]),
        })
    }

    /// The zone named `name`, in any case.
    fn find(&self, name: &str) -> Option<Zone> {
        let b = self.bytes;
        let mut p = self.names + 2;
        for _ in 0..le(at(b, self.names, 2)?) {
            let len = *b.get(p)? as usize;
            if at(b, p + 1, len)?.eq_ignore_ascii_case(name.as_bytes()) {
                return self.zone(le(at(b, p + 1 + len, 2)?) as usize);
            }
            p += len + 3;
        }
        None
    }

    fn zone(&self, entry: usize) -> Option<Zone> {
        let b = self.bytes;
        let mut p = le(at(b, self.index + 4 * entry, 4)?) as usize;
        let init = self.offset(*b.get(p)?)?;
        let (rule, rule_from) = match *b.get(p + 1)? {
            255 => (None, i64::MAX),
            k => (
                Some(self.rule(k)?),
                self.kept + varint(b, &mut { p + 2 })? * 1000,
            ),
        };
        p += 2;
        if rule.is_some() {
            varint(b, &mut p)?;
        }
        let mut trans = Vec::new();
        let mut t = self.kept;
        for _ in 0..varint(b, &mut p)? {
            t += varint(b, &mut p)? * 1000;
            trans.push((t, self.offset(*b.get(p)?)?));
            p += 1;
        }
        Some(Zone::Rules(Rules {
            from: self.from,
            init,
            trans,
            rule,
            rule_from,
            last: std::cell::Cell::new(Piece {
                start: 0,
                end: 0,
                off: 0,
            }),
        }))
    }
}

impl Interval {
    /// Whether it counts whole days: a day, a week, a month, a year --
    /// which a zone's clock starts at its midnight.
    pub fn whole_days(self) -> bool {
        match self {
            Interval::Ms(n, _) => n % DAY == 0,
            Interval::Months(_) => true,
        }
    }
}
