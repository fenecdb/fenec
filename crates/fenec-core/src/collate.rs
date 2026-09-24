//! `collate`: text in the order a language expects rather than the order of
//! its bytes.
//!
//! Byte order is what `order` gives text, and it is wrong for people in
//! every language: every capitalised word comes before every lowercase one,
//! an accented letter lands after `z`, and every script past ASCII falls
//! where its code points do. `collate und` gives Unicode's root order
//! instead, the one every language's starts from -- CLDR's root collation,
//! which PostgreSQL calls `und-x-icu`: every script in a run of its own,
//! its letters in their order, accents and case deciding only between words
//! whose letters are the same. A language whose letters sort otherwise is a
//! tailoring over it: `collate tr` is Turkish, where `ç ğ ı ö ş ü` are
//! letters of their own, `ı` before `i`, `I` the capital of `ı` and `İ` the
//! capital of `i`.
//!
//! It compares as ICU does, one level at a time over the whole string: the
//! letters, then their accents (`kar` < `kâr` < `kara`), then their case
//! (`ince` < `İnce`). What all three leave equal is ordered by its bytes, as
//! under a deterministic PostgreSQL collation: the order is total, and two
//! different strings never tie.
//!
//! The weights are ICU's own, dumped from macOS's libicucore by
//! `tools/collate/gen.py` for every assigned code point and cut into chunks
//! by script (`collate/*.bin`): Han ideographs in the radical-and-stroke
//! order ICU's root gives them, those of the other planes among them, and
//! Hangul syllables decomposed into their jamo as ICU does. Two or three
//! characters ICU sorts as one are sorted as one: a character and a mark it
//! composes with (`и` and U+0306 as `й`, under `tr` `c` and U+0327 as `ç`),
//! a vowel sign written in two parts, and a Thai or Lao vowel written before
//! the consonant it follows in speech, which sorts after it. An unassigned
//! or private code point goes where ICU puts one, by code point.
//! `web/fenec.test.js` holds the order to `Intl.Collator`. What it does not
//! do is ICU's normalisation: a mark that composes with a letter across
//! another mark (`и`, U+0323, U+0306) sorts as a mark of its own.
//!
//! A native build carries every chunk. The browser module carries `latin`
//! and is handed the others as a page first needs them: a comparison that
//! meets a code point whose chunk is not there sorts it as unassigned and
//! notes the chunk ([`take_missing`]), and the statement is refused, the
//! chunks named, rather than answered in that order -- the client adds them
//! ([`add_chunk`]) and runs it again.

use crate::value::Value;
use std::cmp::Ordering;
use std::str::Chars;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::OnceLock;

mod layout;
pub use layout::CHUNKS;
use layout::{
    BLOCKS, COMMON_S, COMMON_T, CONTINUES, EMBEDDED, IMPLICIT, OTHER, SMP, STAMP, TAILORED,
    TAILORINGS,
};

/// A text order other than the bytes'.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collation {
    /// `tr`: Turkish, as ICU orders it.
    Turkish,
    /// `und`: Unicode's root order, for every language.
    Root,
}

impl Collation {
    /// The collation `collate <name>` names, if there is one. The name folds
    /// as an unquoted SQL identifier does: `COLLATE TR` is `tr`.
    pub fn named(name: &str) -> Option<Collation> {
        if name.eq_ignore_ascii_case("tr") {
            Some(Collation::Turkish)
        } else if name.eq_ignore_ascii_case("und") || name.eq_ignore_ascii_case("root") {
            Some(Collation::Root)
        } else {
            None
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Collation::Turkish => "tr",
            Collation::Root => "und",
        }
    }

    /// The byte a schema writes for a field in this collation.
    pub fn code(self) -> u8 {
        match self {
            Collation::Turkish => 1,
            Collation::Root => 2,
        }
    }

    pub fn from_code(c: u8) -> Option<Collation> {
        match c {
            1 => Some(Collation::Turkish),
            2 => Some(Collation::Root),
            _ => None,
        }
    }

    /// What a comparison in this collation looks letters up in, found once:
    /// the tailoring and the Latin letters each behind a lock of their own
    /// were two reads of one on every comparison.
    fn order(self) -> Order {
        static ORDERS: [OnceLock<Order>; 2] = [const { OnceLock::new() }; 2];
        *ORDERS[self.code() as usize - 1].get_or_init(|| Order {
            tail: self.tailoring(),
            latin: self.latin(),
        })
    }

    /// The elements of the code points below [`DIRECT`] in this collation,
    /// made once, [`SLOW`] for one that expands: a Latin letter is then one
    /// read, where the tailoring's table, the root's chunk and unpacking the
    /// entry had taken a comparison from 30 to 41 ns.
    fn latin(self) -> &'static [u64] {
        static LATIN: [OnceLock<Vec<u64>>; 2] = [const { OnceLock::new() }; 2];
        LATIN[self.code() as usize - 1].get_or_init(|| {
            let tail = self.tailoring();
            let root = chunk(0).expect("every build carries the Latin chunk");
            (0..DIRECT)
                .map(|c| {
                    let found = tail.and_then(|t| t.entry(c)).or_else(|| root.entry(c));
                    match found {
                        Some(e) if e & EXPANDS != 0 => SLOW,
                        Some(0) => 0,
                        Some(e) => unpack(e, c),
                        None => fallback(c),
                    }
                })
                .collect()
        })
    }

    /// The tailoring over the root, if the collation is one.
    fn tailoring(self) -> Option<&'static Table> {
        let n = match self {
            Collation::Turkish => 0,
            Collation::Root => return None,
        };
        debug_assert_eq!(TAILORINGS[n], self.name());
        static TAILS: [OnceLock<Table>; TAILORED.len()] = [const { OnceLock::new() }; _];
        Some(carried(&TAILS[n], TAILORED[n], TAIL_MAGIC))
    }

    /// `a` against `b` in this order.
    pub fn compare(self, a: &str, b: &str) -> Ordering {
        // What the two share weighs the same in both, at every level, so the
        // comparison starts where they part: `Ahmet Yılmaz` against `Ahmet
        // Kaya` is `Y` against `K`. Sorting a million names, a comparison
        // went 68 -> 30 ns. It starts at the latest character that cannot be
        // the second or third of characters sorting as one, which the
        // characters before it therefore neither reach into nor look at:
        // started inside such a run (`c` and U+0327, a Thai vowel and its
        // consonant) it would see them apart, and a fixed step back does
        // not do, since Gurung Khema's runs overlap.
        let mut at = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
        if at == a.len() && at == b.len() {
            return Ordering::Equal;
        }
        // The shared bytes are the same characters, so a boundary in one is
        // a boundary in the other.
        while !a.is_char_boundary(at) {
            at -= 1;
        }
        while at > 0 && (continued(a, at) || continued(b, at)) {
            at -= 1;
            while !a.is_char_boundary(at) {
                at -= 1;
            }
        }
        let (a, b) = (&a[at..], &b[at..]);
        let o = self.order();
        // Past the shared part most pairs differ in their first letter, so
        // the first level decides and the other two are never walked.
        level(a, b, 0, o)
            .then_with(|| level(a, b, 1, o))
            .then_with(|| level(a, b, 2, o))
            .then_with(|| a.cmp(b))
    }

    /// Two values as `order ... collate` puts them: text in this order, a
    /// list of text element by element, anything else as
    /// [`Value::cmp_value`] does.
    pub fn compare_values(self, a: &Value, b: &Value) -> Ordering {
        match (a, b) {
            (Value::Text(x), Value::Text(y)) => self.compare(x, y),
            (Value::List(x), Value::List(y)) => {
                for (p, q) in x.iter().zip(y) {
                    let o = self.compare_values(p, q);
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                x.len().cmp(&y.len())
            }
            _ => a.cmp_value(b),
        }
    }
}

/// Whether the character at `at` in `s` can be the second or third of
/// characters that sort as one. None in ASCII is, so its byte answers,
/// inline: the call alone was a twentieth of a comparison.
#[inline(always)]
fn continued(s: &str, at: usize) -> bool {
    s.as_bytes().get(at).is_some_and(|&b| b >= 0x80) && continued_past_ascii(s, at)
}

fn continued_past_ascii(s: &str, at: usize) -> bool {
    s[at..]
        .chars()
        .next()
        .is_some_and(|c| continues_cp(c as u32))
}

/// Whether `c` can be the second or third of characters that sort as one:
/// asked of the character after every one a comparison loads, so the code
/// points below 1024 -- every Latin, Greek and Cyrillic letter -- are a bit
/// each.
#[inline]
fn continues_cp(c: u32) -> bool {
    if c < 1024 {
        return LOW_CONTINUES[(c / 64) as usize] >> (c % 64) & 1 != 0;
    }
    let i = CONTINUES.partition_point(|r| r.1 <= c);
    CONTINUES.get(i).is_some_and(|r| c >= r.0)
}

const LOW_CONTINUES: [u64; 16] = {
    let mut bits = [0u64; 16];
    let mut i = 0;
    while i < CONTINUES.len() {
        let (lo, hi) = CONTINUES[i];
        let mut c = lo;
        while c < hi && c < 1024 {
            bits[(c / 64) as usize] |= 1 << (c % 64);
            c += 1;
        }
        i += 1;
    }
    bits
};

/// Makes a chunk the build does not carry available, from the bytes of its
/// `.bin`: the browser module carries `latin` and is handed the others as a
/// page needs them. Returns the chunk's name, or `None` for bytes that are
/// not one of these tables' chunks.
pub fn add_chunk(bytes: &[u8]) -> Option<&'static str> {
    let (n, table) = Table::parse(bytes, CHUNK_MAGIC)?;
    let n = n as usize;
    let mut table = Some(table);
    fill(CHUNK_TABLES.get(n)?, &mut || {
        table.take().expect("filled once")
    });
    Some(CHUNKS[n])
}

/// The chunks comparisons reached for since the last call and did not
/// have, a bit each by their number in [`CHUNKS`]; clears the note. Never
/// set where the build carries every chunk.
pub fn take_missing() -> u32 {
    MISSING.swap(0, Relaxed)
}

/// Whether the build may lack chunks: the browser module, which is handed
/// them. Everything that checks for them is behind it, so a native build
/// checks nothing.
pub const PARTIAL: bool = cfg!(target_arch = "wasm32");

/// The chunks this build has, a bit each.
pub fn loaded() -> u32 {
    (0..CHUNKS.len())
        .filter(|&n| !EMBEDDED[n].is_empty() || CHUNK_TABLES[n].get().is_some())
        .fold(0, |m, n| m | 1 << n)
}

/// The chunks comparing the text in `v` would reach for and not have, a
/// bit each: what a write checks before it changes anything, since a
/// `@sorted` index kept in a collation that compared without them would be
/// out of order.
pub fn missing_in(v: &Value) -> u32 {
    if !PARTIAL {
        return 0;
    }
    match v {
        Value::Text(s) => {
            let have = loaded();
            let mut mask = 0;
            for c in s.chars() {
                let mut c = c as u32;
                if (0xAC00..0xD7A4).contains(&c) {
                    c = 0x1100;
                }
                if let Some(n) = chunk_of(c) {
                    mask |= 1 << n & !have;
                }
            }
            mask
        }
        Value::List(l) => l.iter().fold(0, |m, v| m | missing_in(v)),
        _ => 0,
    }
}

/// Refuses a statement that would compare without the chunks `mask` names
/// -- noted for [`take_missing`], which is how the browser module tells
/// which to fetch -- or lets it go on when there are none.
pub fn refuse(mask: u32) -> crate::error::Result<()> {
    if mask == 0 {
        return Ok(());
    }
    MISSING.fetch_or(mask, Relaxed);
    let mut names = String::new();
    for name in chunk_names(mask) {
        if !names.is_empty() {
            names.push_str(", ");
        }
        names.push_str(name);
    }
    Err(crate::error::Error::NotFound(format!(
        "the collation data {names}: hand it to the module (`collation`) and run the statement again"
    )))
}

/// The names of the chunks `mask` has a bit for.
pub fn chunk_names(mask: u32) -> impl Iterator<Item = &'static str> {
    CHUNKS
        .iter()
        .enumerate()
        .filter(move |(i, _)| mask & 1 << i != 0)
        .map(|(_, n)| *n)
}

static CHUNK_TABLES: [OnceLock<Table>; CHUNKS.len()] = [const { OnceLock::new() }; _];
static MISSING: AtomicU32 = AtomicU32::new(0);

/// "FNCL" and "FNTL", read little-endian: a chunk and a tailoring.
const CHUNK_MAGIC: u32 = 0x4C43_4E46;
const TAIL_MAGIC: u32 = 0x4C54_4E46;

/// An entry is one element: its primary's rank in bits 0..16, its
/// secondary's in 16..25 and its tertiary's in 25..30 -- or, with this bit,
/// several, counted in bits 24..30 and starting at bits 0..24 of the
/// table's `more`.
const EXPANDS: u32 = 1 << 31;
/// The element's primary is told apart by the code point. ICU gives every
/// ideograph a primary of its own, and most alphabets' letters, 133 534 of
/// them: consecutive code points that alone give consecutive primaries
/// share a rank instead (gen.py), and their code points order them as the
/// primaries did -- 24 931 ranks.
const BY_CODE_POINT: u32 = 1 << 30;
/// A range whose every code point has one entry, in its third word.
const UNIFORM: u32 = 1 << 31;
/// A code point inside a table's ranges that the table has no entry for.
const NONE: u32 = 0x3FFF_FFFF;
/// The code points every table that has an entry below it holds the
/// entries of one each: Latin-1 and Latin Extended-A and -B.
const DIRECT: u32 = 0x250;
/// A chunk of the root's entries, or a tailoring's over them: the words of
/// its `.bin`, read where they lie. A `Vec` a section, each filled through a
/// `collect` of its own, was 2.4 KB of the browser module.
struct Table {
    w: Vec<u32>,
    /// The entries of the code points below [`DIRECT`], one each -- `NONE`
    /// where the table has none -- for a table that has any there: the
    /// letters of every Latin page, looked up without a search. Empty for
    /// the others.
    direct: Vec<u32>,
    /// Where in `w` the ranges start -- `(lo, hi, where their entries
    /// start)`, ascending; with [`UNIFORM`], the entry every code point of
    /// the range has -- and how many there are; then the entries, the
    /// elements those that expand name (`more`), and the contractions --
    /// `(first, second, third or 0, entry)`, ascending -- and their number.
    ranges: usize,
    n_ranges: usize,
    entries: usize,
    more: usize,
    contractions: usize,
    n_contractions: usize,
}

impl Table {
    /// The table in `bytes`, and its number; `None` when they are not a
    /// table of `magic`'s kind and these tables' stamp, or its sections do
    /// not add up to its length. What is read out of them is read with
    /// `get`, so a chunk damaged past that orders wrongly and never takes
    /// the module down; checking every entry here was 0.8 KB of it.
    ///
    /// A section's words are written as differences from the word a record
    /// back, zigzag-folded, in LEB128 (gen.py), and read back into the
    /// words they were: 324 KB of them for every script are 154 KB so, and
    /// under gzip and brotli a third smaller than the words would be.
    fn parse(bytes: &[u8], magic: u32) -> Option<(u32, Table)> {
        let mut w: Vec<u32> = bytes
            .get(..12)?
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_le_bytes(*b))
            .collect();
        if w[0] != magic || w[2] != STAMP {
            return None;
        }
        let mut at = 12;
        let mut next = || {
            let mut n = 0u64;
            for shift in (0..64).step_by(7) {
                let b = *bytes.get(at)?;
                at += 1;
                n |= ((b & 0x7F) as u64) << shift;
                if b < 0x80 {
                    return Some(((n >> 1) as i64) ^ -((n & 1) as i64));
                }
            }
            None
        };
        let mut sections = [(0, 0); 4];
        for (section, stride) in sections.iter_mut().zip([3, 1, 1, 4]) {
            let n = u32::try_from(next()?).ok()? as usize;
            let words = n.checked_mul(stride)?;
            // A word takes a byte at least: a length past what is left is
            // damage, and not an allocation to make.
            if words > bytes.len() {
                return None;
            }
            w.push(n as u32);
            let start = w.len();
            *section = (start, n);
            for i in 0..words {
                let back = if i >= stride {
                    w[start + i - stride] as i64
                } else {
                    0
                };
                w.push(u32::try_from(back + next()?).ok()?);
            }
        }
        if at != bytes.len() {
            return None;
        }
        let [(ranges, n_ranges), (entries, _), (more, _), (contractions, n_contractions)] =
            sections;
        let mut t = Table {
            w,
            direct: Vec::new(),
            ranges,
            n_ranges,
            entries,
            more,
            contractions,
            n_contractions,
        };
        if n_ranges > 0 && t.range(0).0 < DIRECT {
            t.direct = (0..DIRECT).map(|c| t.search(c).unwrap_or(NONE)).collect();
        }
        Some((t.w[1], t))
    }

    fn range(&self, i: usize) -> (u32, u32, u32) {
        let at = self.ranges + 3 * i;
        (self.w[at], self.w[at + 1], self.w[at + 2])
    }

    fn contraction_at(&self, i: usize) -> &[u32] {
        &self.w[self.contractions + 4 * i..][..4]
    }

    /// `c`'s entry, if the table has one.
    #[inline]
    fn entry(&self, c: u32) -> Option<u32> {
        match self.direct.get(c as usize) {
            Some(&e) => (e != NONE).then_some(e),
            None => self.search(c),
        }
    }

    /// `c`'s entry, found in the ranges.
    fn search(&self, c: u32) -> Option<u32> {
        // The first range past `c`'s lower end.
        let (mut i, mut j) = (0, self.n_ranges);
        while i < j {
            let mid = (i + j) / 2;
            if self.range(mid).1 <= c {
                i = mid + 1;
            } else {
                j = mid;
            }
        }
        if i == self.n_ranges {
            return None;
        }
        let (lo, _, at) = self.range(i);
        if c < lo {
            return None;
        }
        let e = if at & UNIFORM != 0 {
            at & !UNIFORM
        } else {
            *self.w.get(self.entries + at as usize + (c - lo) as usize)?
        };
        (e != NONE).then_some(e)
    }

    /// The entry of the longest run of characters from `c`, `m` and `n` that
    /// sorts as one, and how many it takes past `c`.
    fn contraction(&self, c: u32, m: u32, n: u32) -> Option<(u32, usize)> {
        let (mut i, mut j) = (0, self.n_contractions);
        while i < j {
            let mid = (i + j) / 2;
            let k = self.contraction_at(mid);
            if (k[0], k[1]) < (c, m) {
                i = mid + 1;
            } else {
                j = mid;
            }
        }
        let mut found = None;
        while i < self.n_contractions {
            let k = self.contraction_at(i);
            if k[0] != c || k[1] != m {
                break;
            }
            if k[2] == 0 {
                found = Some((k[3], 1));
            } else if k[2] == n {
                return Some((k[3], 2));
            }
            i += 1;
        }
        found
    }

    /// The elements an entry that expands names.
    fn more(&self, e: u32) -> &[u32] {
        let from = self.more + (e & 0xff_ffff) as usize;
        self.w
            .get(from..from + (e >> 24 & 0x3f) as usize)
            .unwrap_or_default()
    }
}

/// The table in a slot, parsed out of the bytes the build carries the first
/// time it is asked for.
fn carried(slot: &'static OnceLock<Table>, bytes: &'static [u8], magic: u32) -> &'static Table {
    fill(slot, &mut || {
        Table::parse(bytes, magic)
            .expect("the tables the build carries parse")
            .1
    })
}

/// A slot's table, made by `make` if it has none. Every slot is filled
/// through here: each `get_or_init` with a closure of its own was its own
/// copy of the once-only machinery.
fn fill<'a>(slot: &'a OnceLock<Table>, make: &mut dyn FnMut() -> Table) -> &'a Table {
    slot.get_or_init(make)
}

/// The chunk `c`'s entry is in when it has one.
#[inline]
fn chunk_of(c: u32) -> Option<usize> {
    if c < BLOCKS[0].1 {
        return Some(BLOCKS[0].2 as usize);
    }
    let i = BLOCKS.partition_point(|b| b.1 <= c);
    match BLOCKS.get(i) {
        Some(&(lo, _, n)) if c >= lo => Some(n as usize),
        _ if c < 0x10000 => Some(OTHER),
        _ if c < 0x20000 => Some(SMP),
        _ => None,
    }
}

/// Chunk `n`, where the build carries it or it has been added; noted as
/// missing otherwise.
#[inline]
fn chunk(n: usize) -> Option<&'static Table> {
    let slot = &CHUNK_TABLES[n];
    if let Some(t) = slot.get() {
        return Some(t);
    }
    if EMBEDDED[n].is_empty() {
        MISSING.fetch_or(1 << n, Relaxed);
        return None;
    }
    Some(carried(slot, EMBEDDED[n], CHUNK_MAGIC))
}

/// One level -- 0 for the letters, 1 the accents, 2 the case -- of two
/// strings compared: their weights at that level in sequence, an element
/// with none there skipped, a prefix before what extends it.
fn level(a: &str, b: &str, level: u32, o: Order) -> Ordering {
    let mut x = Elements::new(a, o);
    let mut y = Elements::new(b, o);
    loop {
        match (x.weight(level), y.weight(level)) {
            (Some(p), Some(q)) if p == q => {}
            (Some(p), Some(q)) => return p.cmp(&q),
            (p, q) => return p.is_some().cmp(&q.is_some()),
        }
    }
}

/// A string's collation elements, a character at a time. An element holds
/// its three weights in one `u64`: the primary above bit 14, the secondary
/// in bits 5..14, the tertiary below. A primary is its rank above bit 21 and,
/// where the rank is shared, the code point below it.
///
/// What a character loads is handed out from where it lies -- one element,
/// or the rest of an expansion in its table -- rather than copied into a
/// queue: the queue a phrase-long ligature needs, zeroed for every string
/// compared, cost a third of a comparison.
struct Elements<'a> {
    rest: Chars<'a>,
    tail: Option<&'static Table>,
    latin: &'static [u64],
    /// The element the character loaded last gave alone; 0 once handed out,
    /// since no element is 0.
    one: u64,
    /// The elements of an expansion still to hand out, for code point `of`.
    more: &'static [u32],
    of: u32,
    /// The jamo of a Hangul syllable still to load, the next in the low
    /// half; 0 when there are none.
    jamo: u64,
}

impl<'a> Elements<'a> {
    fn new(s: &'a str, o: Order) -> Self {
        Elements {
            rest: s.chars(),
            tail: o.tail,
            latin: o.latin,
            one: 0,
            more: &[],
            of: 0,
            jamo: 0,
        }
    }

    /// The next weight at `level`, past the elements that have none there.
    fn weight(&mut self, level: u32) -> Option<u64> {
        loop {
            let e = if self.one != 0 {
                std::mem::take(&mut self.one)
            } else if let Some((&m, rest)) = self.more.split_first() {
                self.more = rest;
                unpack(m, self.of)
            } else if self.jamo != 0 {
                let j = self.jamo as u32;
                self.jamo >>= 32;
                self.single(j);
                continue;
            } else {
                let c = self.rest.next()?;
                self.load(c as u32);
                continue;
            };
            let w = match level {
                0 => e >> 14,
                1 => e >> 5 & 0x1ff,
                _ => e & 0x1f,
            };
            if w != 0 {
                return Some(w);
            }
        }
    }

    /// Loads the elements of `c`, and of the characters after it that sort
    /// as one with it.
    #[inline]
    fn load(&mut self, c: u32) {
        // A Hangul syllable sorts as its jamo, as ICU decomposes it.
        if (0xAC00..0xD7A4).contains(&c) {
            let s = c - 0xAC00;
            let t = if s.is_multiple_of(28) {
                0
            } else {
                0x11A7 + s % 28
            };
            self.jamo = (0x1161 + s % 588 / 28) as u64 | (t as u64) << 32;
            return self.single(0x1100 + s / 588);
        }
        // No ASCII character goes on a contraction, so the next byte rules
        // out most before any is decoded.
        let next = self.rest.as_str().as_bytes().first();
        let mut ahead = self.rest.clone();
        if let Some(m) = next
            .filter(|&&b| b >= 0x80)
            .and_then(|_| ahead.next())
            .map(|m| m as u32)
            .filter(|&m| continues_cp(m))
        {
            let n = ahead.next().map_or(u32::MAX, |n| n as u32);
            let found = match self.tail.and_then(|t| Some((t, t.contraction(c, m, n)?))) {
                Some(found) => Some(found),
                None => chunk_of(c)
                    .and_then(chunk)
                    .and_then(|t| Some((t, t.contraction(c, m, n)?))),
            };
            if let Some((t, (e, took))) = found {
                for _ in 0..took {
                    self.rest.next();
                }
                return self.entry(t, e, c);
            }
        }
        match self.latin.get(c as usize) {
            Some(&e) if e != SLOW => self.one = e,
            _ => self.single(c),
        }
    }

    /// Loads the elements of `c` alone: the tailoring's entry, the root's,
    /// or where ICU puts a code point it has none for.
    #[inline]
    fn single(&mut self, c: u32) {
        if let Some(t) = self.tail {
            if let Some(e) = t.entry(c) {
                return self.entry(t, e, c);
            }
        }
        match chunk_of(c)
            .and_then(chunk)
            .and_then(|t| Some((t, t.entry(c)?)))
        {
            Some((t, e)) => self.entry(t, e, c),
            None => self.one = fallback(c),
        }
    }

    #[inline]
    fn entry(&mut self, t: &'static Table, e: u32, c: u32) {
        if e & EXPANDS != 0 {
            self.more = t.more(e);
            self.of = c;
        } else if e != 0 {
            self.one = unpack(e, c);
        }
    }
}

/// What a comparison in one collation looks letters up in: the tailoring,
/// if the collation is one, and the Latin letters' elements, made once.
#[derive(Clone, Copy)]
struct Order {
    tail: Option<&'static Table>,
    latin: &'static [u64],
}

/// A Latin letter that expands, in [`Collation::latin`]: no element is it.
const SLOW: u64 = u64::MAX;

/// The element of a code point no entry covers: where ICU puts an
/// unassigned or private one, by code point.
fn fallback(c: u32) -> u64 {
    element((IMPLICIT as u64) << 21 | c as u64, COMMON_S, COMMON_T)
}

/// The element an entry packs, for code point `c`.
#[inline]
fn unpack(e: u32, c: u32) -> u64 {
    let rank = (e & 0xffff) as u64;
    let low = if e & BY_CODE_POINT != 0 { c as u64 } else { 0 };
    element(rank << 21 | low, e >> 16 & 0x1ff, e >> 25 & 0x1f)
}

fn element(primary: u64, secondary: u32, tertiary: u32) -> u64 {
    primary << 14 | (secondary as u64) << 5 | tertiary as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(c: Collation, words: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = words.iter().map(|s| s.to_string()).collect();
        v.sort_by(|a, b| c.compare(a, b));
        v
    }

    /// Equal at every level: only the bytes tell them apart.
    fn same(c: Collation, a: &str, b: &str) -> bool {
        (0..3).all(|l| level(a, b, l, c.order()) == Ordering::Equal)
    }

    #[test]
    fn the_turkish_alphabet() {
        let lower = "abcçdefgğhıijklmnoöpqrsştuüvwxyz";
        let upper = "ABCÇDEFGĞHIİJKLMNOÖPQRSŞTUÜVWXYZ";
        for alphabet in [lower, upper] {
            let mut letters: Vec<String> = alphabet.chars().map(String::from).collect();
            letters.reverse();
            letters.sort_by(|a, b| Collation::Turkish.compare(a, b));
            assert_eq!(letters.concat(), alphabet);
        }
    }

    #[test]
    fn letters_then_accents_then_case() {
        assert_eq!(
            sorted(
                Collation::Turkish,
                &["kara", "kâr", "Kar", "kar", "İnce", "ince", "Irmak", "ılık"]
            ),
            ["ılık", "Irmak", "ince", "İnce", "kar", "Kar", "kâr", "kara"]
        );
    }

    #[test]
    fn decomposed_letters_sort_as_the_letters() {
        for c in [Collation::Turkish, Collation::Root] {
            for (nfd, nfc) in [
                ("c\u{327}", "ç"),
                ("C\u{327}", "Ç"),
                ("g\u{306}", "ğ"),
                ("I\u{307}", "İ"),
                ("o\u{308}", "ö"),
                ("s\u{327}", "ş"),
                ("U\u{308}", "Ü"),
                ("e\u{301}", "é"),
                ("и\u{306}", "й"),
                // A vowel sign in two parts, and in three.
                ("\u{9C7}\u{9BE}", "\u{9CB}"),
                ("\u{CC6}\u{CC2}\u{CD5}", "\u{CCB}"),
            ] {
                for (x, y) in [("", ""), ("a", "b"), ("", "z"), ("ş", "")] {
                    let a = format!("{x}{nfd}{y}");
                    let b = format!("{x}{nfc}{y}");
                    assert!(same(c, &a, &b), "{c:?} {a:?} {b:?}");
                }
            }
        }
    }

    #[test]
    fn a_shared_start_does_not_split_a_letter() {
        // `ac` is shared, but its `c` is half of a decomposed `ç`: started
        // after it, the comparison would see a bare mark against `z`.
        let c = Collation::Turkish;
        assert_eq!(c.compare("ac\u{327}", "acz"), Ordering::Greater);
        assert_eq!(c.compare("acz", "ac\u{327}"), Ordering::Less);
        // `I` and U+0307 are `İ`, of the `i`s, which follow the `ı`s.
        assert_eq!(c.compare("I\u{307}a", "Iz"), Ordering::Greater);
        // Past a shared letter and mark, the next mark decides alone.
        assert_eq!(
            c.compare("c\u{327}\u{301}", "c\u{327}\u{300}"),
            level("ç\u{301}", "ç\u{300}", 1, c.order())
        );
        // A mark outside the Latin block joins its letter as well, and the
        // third of three characters that sort as one.
        let r = Collation::Root;
        assert_eq!(r.compare("хи\u{306}", "хиа"), r.compare("хй", "хиа"));
        let (ko, kai) = ("\u{C95}\u{CC6}\u{CC2}\u{CD5}", "\u{C95}\u{CC6}\u{CC2}");
        assert_eq!(
            r.compare(ko, kai),
            r.compare("\u{C95}\u{CCB}", "\u{C95}\u{CCA}")
        );
        // A Thai vowel written first sorts after its consonant.
        assert_eq!(
            r.compare("\u{E40}\u{E01}", "\u{E40}\u{E02}"),
            Ordering::Less
        );
    }

    #[test]
    fn different_strings_never_tie() {
        // U+00AD, a soft hyphen, is ignorable at every level.
        for c in [Collation::Turkish, Collation::Root] {
            assert_eq!(c.compare("ab", "a\u{ad}b"), "ab".cmp("a\u{ad}b"));
            assert_eq!(c.compare("a\u{ad}b", "ab"), "a\u{ad}b".cmp("ab"));
            assert_eq!(c.compare("", ""), Ordering::Equal);
            assert_eq!(c.compare("", "\u{ad}"), Ordering::Less);
        }
    }

    /// Each script in a run of its own -- Latin, Greek, Cyrillic, Hebrew,
    /// Arabic, the Indic scripts, Thai, Hangul, kana, Han -- symbols before
    /// the digits and the digits before the letters; Han in radical and
    /// stroke order, and a Hangul syllable as its jamo.
    #[test]
    fn the_root_orders_every_script() {
        assert_eq!(
            sorted(
                Collation::Root,
                &[
                    "一", "가", "あ", "ก", "क", "ب", "א", "б", "β", "b", "1", "★", "Zeynep"
                ]
            ),
            ["★", "1", "b", "Zeynep", "β", "б", "א", "ب", "क", "ก", "가", "あ", "一"]
        );
        let r = Collation::Root;
        // U+3400, of radical 1, sorts among the ideographs of radical 1 by
        // its strokes (between U+4E1D and U+4E1E), not before them all as
        // its code point would; U+20000, of another plane, between U+4E06
        // and U+4E07.
        assert_eq!(
            sorted(r, &["\u{3400}", "\u{4E1E}", "\u{4E1D}"]),
            ["\u{4E1D}", "\u{3400}", "\u{4E1E}"]
        );
        assert_eq!(
            sorted(r, &["\u{4E07}", "\u{20000}", "\u{4E06}"]),
            ["\u{4E06}", "\u{20000}", "\u{4E07}"]
        );
        assert!(same(r, "한", "\u{1112}\u{1161}\u{11AB}"));
        // Under the root `ç` is a `c` with an accent; under `tr` a letter.
        assert_eq!(sorted(r, &["cb", "ça", "ca"]), ["ca", "ça", "cb"]);
        assert_eq!(
            sorted(Collation::Turkish, &["cb", "ça", "ca"]),
            ["ca", "cb", "ça"]
        );
        // Thai: the vowel written before its consonant sorts after it.
        assert_eq!(sorted(r, &["ข", "เก", "กา"]), ["กา", "เก", "ข"]);
    }

    /// The other planes: a script of its own, a mathematical letter as the
    /// letter in another style, emoji among the symbols.
    #[test]
    fn the_other_planes() {
        let r = Collation::Root;
        // Adlam: a capital and its small letter differ in case alone.
        assert!(!same(r, "\u{1E900}", "\u{1E922}"));
        assert_eq!(
            level("\u{1E900}", "\u{1E922}", 0, r.order()),
            Ordering::Equal
        );
        assert_eq!(
            level("\u{1E900}", "\u{1E922}", 1, r.order()),
            Ordering::Equal
        );
        assert_eq!(
            sorted(r, &["\u{1E901}", "\u{1E922}", "\u{1E900}"]),
            ["\u{1E922}", "\u{1E900}", "\u{1E901}"]
        );
        // 𝐀𝐛𝐜 is Abc at the letters' level.
        assert_eq!(
            level("\u{1D400}\u{1D41B}\u{1D41C}", "Abc", 0, r.order()),
            Ordering::Equal
        );
        assert_eq!(
            sorted(r, &["Abd", "\u{1D400}\u{1D41B}\u{1D41C}"]),
            ["\u{1D400}\u{1D41B}\u{1D41C}", "Abd"]
        );
        // An emoji is a symbol: before the digits and the letters.
        assert_eq!(sorted(r, &["a", "1", "\u{1F600}"]), ["\u{1F600}", "1", "a"]);
    }

    /// An unassigned or private code point: after every assigned one but
    /// U+FFFD and U+FFFF, by code point. U+FFFE comes first of all.
    #[test]
    fn what_no_entry_covers() {
        let r = Collation::Root;
        assert_eq!(
            sorted(
                r,
                &[
                    "\u{FFFF}",
                    "\u{50000}",
                    "\u{E000}",
                    "\u{378}",
                    "\u{FFFD}",
                    "\u{2A6DF}",
                    "a",
                    "\u{FFFE}"
                ]
            ),
            [
                "\u{FFFE}",
                "a",
                "\u{2A6DF}",
                "\u{378}",
                "\u{E000}",
                "\u{50000}",
                "\u{FFFD}",
                "\u{FFFF}"
            ]
        );
        assert_eq!(take_missing(), 0, "a native build carries every chunk");
        assert_eq!(missing_in(&Value::Text("ขอบคุณ 一 😀".into())), 0);
    }

    #[test]
    fn a_chunk_is_its_own_bytes_or_nothing() {
        assert_eq!(add_chunk(b"not a chunk"), None);
        let greek = EMBEDDED[1];
        assert_eq!(add_chunk(greek), Some("greek"));
        assert_eq!(add_chunk(&greek[..greek.len() - 4]), None);
        let mut stale = greek.to_vec();
        stale[8] ^= 1;
        assert_eq!(add_chunk(&stale), None, "another version's chunk");
        assert!(Table::parse(TAILORED[0], CHUNK_MAGIC).is_none());
        let names: Vec<&str> = chunk_names(0b101).collect();
        assert_eq!(names, ["latin", "cyrillic"]);
    }
}
