//! `collate`: text in the order a language expects rather than the order of
//! its bytes.
//!
//! Byte order is what `order` gives text, and for Turkish it is wrong twice
//! over: every letter past ASCII -- `ç ğ ı İ ö ş ü` -- lands after `z`, so
//! `Çağla` follows `Zeynep`, and every capitalised word comes before every
//! lowercase one. `order name collate tr` gives ICU's Turkish collation
//! instead, the one PostgreSQL calls `tr-x-icu`: `ç` after `c`, `ğ` after
//! `g`, `ı` before `i` with `I` as its capital and `İ` as the capital of
//! `i`, `ö` after `o`, `ş` after `s`, `ü` after `u`.
//!
//! It compares as ICU does, one level at a time over the whole string: the
//! letters, then their accents (`kar` < `kâr` < `kara`), then their case
//! (`ince` < `İnce`), so an accent or a capital only decides between strings
//! whose letters are the same. What all three leave equal is ordered by its
//! bytes, as under a deterministic PostgreSQL collation: the order is
//! total, and two different strings never tie.
//!
//! The weights are ICU's own, written into `collate/table.rs` by
//! `tools/collate/gen.py`, for Latin-1, Latin Extended-A and -B, the
//! combining marks, general punctuation and the currency signs. Over those
//! the order is ICU's exactly -- decomposed text too, `c` and U+0327 sorting
//! as `ç` -- which `web/fenec.test.js` checks against `Intl.Collator("tr")`.
//! Past them it keeps ICU's broad shape without its tables: another script's
//! letter goes after the Latin ones, by the code point of its lowercase;
//! any other symbol before the digits, by code point.

use crate::value::Value;
use std::cmp::Ordering;
use std::str::Chars;

mod table;
use table::{COMMON_S, COMMON_T, CONTRACTIONS, LETTERS, MORE, RANGES, SYMBOLS, TABLE, UPPER_T};

/// A text order other than the bytes'.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collation {
    /// `tr`: Turkish, as ICU orders it.
    Turkish,
}

impl Collation {
    /// The collation `collate <name>` names, if there is one. The name folds
    /// as an unquoted SQL identifier does: `COLLATE TR` is `tr`.
    pub fn named(name: &str) -> Option<Collation> {
        name.eq_ignore_ascii_case("tr")
            .then_some(Collation::Turkish)
    }

    pub fn name(self) -> &'static str {
        match self {
            Collation::Turkish => "tr",
        }
    }

    /// The byte a schema writes for a field in this collation.
    pub fn code(self) -> u8 {
        match self {
            Collation::Turkish => 1,
        }
    }

    pub fn from_code(c: u8) -> Option<Collation> {
        (c == 1).then_some(Collation::Turkish)
    }

    /// `a` against `b` in this order.
    pub fn compare(self, a: &str, b: &str) -> Ordering {
        // What the two share weighs the same in both, at every level, so the
        // comparison starts where they part: `Ahmet Yılmaz` against `Ahmet
        // Kaya` is `Y` against `K`. A character earlier when that is a mark,
        // which may make one letter with the one before it (`c` and U+0327).
        // Sorting a million names, a comparison went 68 -> 30 ns.
        let mut at = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
        if at == a.len() && at == b.len() {
            return Ordering::Equal;
        }
        // The shared bytes are the same characters, so a boundary in one is
        // a boundary in the other.
        while !a.is_char_boundary(at) {
            at -= 1;
        }
        let mark = |s: &str| matches!(s.as_bytes().get(at), Some(0xcc | 0xcd));
        if at > 0 && (mark(a) || mark(b)) {
            at -= 1;
            while !a.is_char_boundary(at) {
                at -= 1;
            }
        }
        let (a, b) = (&a[at..], &b[at..]);
        // Past the shared part most pairs differ in their first letter, so
        // the first level decides and the other two are never walked.
        level(a, b, 0)
            .then_with(|| level(a, b, 1))
            .then_with(|| level(a, b, 2))
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

/// One level -- 0 for the letters, 1 the accents, 2 the case -- of two
/// strings compared: their weights at that level in sequence, an element
/// with none there skipped, a prefix before what extends it.
fn level(a: &str, b: &str, level: u32) -> Ordering {
    let mut x = Elements::new(a);
    let mut y = Elements::new(b);
    loop {
        match (x.weight(level), y.weight(level)) {
            (Some(p), Some(q)) if p == q => {}
            (Some(p), Some(q)) => return p.cmp(&q),
            (p, q) => return p.is_some().cmp(&q.is_some()),
        }
    }
}

// A table entry, and an element in `MORE`, packs the primary rank in bits
// 0..11, the secondary in 11..18 and the tertiary in 18..23. An entry may
// add a second element, a mark's secondary in 23..30 -- most of the range is
// a letter and one accent -- or, marked `EXPANDS`, point at 2 to 4 elements
// in `MORE` instead: `½`, `æ`, `ß`, `…`.
const MARK_SHIFT: u32 = 23;
const EXPANDS: u32 = 1 << 30;

/// A string's collation elements, a character at a time. An element holds
/// its three weights in one `u64`: the primary above bit 16, the secondary
/// in bits 8..16, the tertiary below.
struct Elements<'a> {
    rest: Chars<'a>,
    queue: [u64; 4],
    at: usize,
    len: usize,
}

impl<'a> Elements<'a> {
    fn new(s: &'a str) -> Self {
        Elements {
            rest: s.chars(),
            queue: [0; 4],
            at: 0,
            len: 0,
        }
    }

    /// The next weight at `level`, past the elements that have none there.
    fn weight(&mut self, level: u32) -> Option<u32> {
        loop {
            while self.at == self.len {
                let c = self.rest.next()?;
                self.load(c);
            }
            let e = self.queue[self.at];
            self.at += 1;
            let w = match level {
                0 => (e >> 16) as u32,
                1 => (e >> 8) as u32 & 0xff,
                _ => e as u32 & 0xff,
            };
            if w != 0 {
                return Some(w);
            }
        }
    }

    /// Queues the elements of `c`.
    fn load(&mut self, mut c: char) {
        // A decomposed Turkish letter is the letter: `c` and U+0327 sort as
        // `ç`. Every contraction starts with an ASCII character and goes on
        // with a mark from U+0300..U+0370, whose UTF-8 starts 0xCC or 0xCD.
        if c.is_ascii() && matches!(self.rest.as_str().as_bytes().first(), Some(0xcc | 0xcd)) {
            let mut ahead = self.rest.clone();
            let mark = ahead.next();
            if let Some(&(_, _, to)) = CONTRACTIONS
                .iter()
                .find(|&&(base, m, _)| base == c && Some(m) == mark)
            {
                self.rest = ahead;
                c = to;
            }
        }
        self.at = 0;
        self.len = 0;
        let Some(e) = entry(c) else {
            let lower = c.to_lowercase().next().unwrap_or(c);
            let (band, tertiary) = if c.is_alphanumeric() {
                (LETTERS, if lower == c { COMMON_T } else { UPPER_T })
            } else {
                (SYMBOLS, COMMON_T)
            };
            self.push(element(band << 21 | lower as u32, COMMON_S, tertiary));
            return;
        };
        if e & EXPANDS != 0 {
            let from = (e & 0xffff) as usize;
            let n = (e >> 16 & 0xf) as usize;
            for &m in &MORE[from..from + n] {
                self.push(unpack(m));
            }
        } else if e != 0 {
            self.push(unpack(e));
            let mark = e >> MARK_SHIFT & 0x7f;
            if mark != 0 {
                self.push(element(0, mark, COMMON_T));
            }
        }
    }

    fn push(&mut self, e: u64) {
        self.queue[self.len] = e;
        self.len += 1;
    }
}

/// The table's entry for `c`, if the table covers it.
fn entry(c: char) -> Option<u32> {
    let cp = c as u32;
    let mut at = 0;
    for (lo, hi) in RANGES {
        if cp < hi {
            return (cp >= lo).then(|| TABLE[(at + cp - lo) as usize]);
        }
        at += hi - lo;
    }
    None
}

/// The first element a table entry packs. A primary rank goes above bit 21
/// so that the fallback's code points fit below it, in the band of the rank
/// they follow.
fn unpack(e: u32) -> u64 {
    let rank = e & 0x7ff;
    element(
        if rank == 0 { 0 } else { rank << 21 },
        e >> 11 & 0x7f,
        e >> 18 & 0x1f,
    )
}

fn element(primary: u32, secondary: u32, tertiary: u32) -> u64 {
    (primary as u64) << 16 | (secondary as u64) << 8 | tertiary as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(words: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = words.iter().map(|s| s.to_string()).collect();
        v.sort_by(|a, b| Collation::Turkish.compare(a, b));
        v
    }

    #[test]
    fn the_alphabet() {
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
            sorted(&["kara", "kâr", "Kar", "kar", "İnce", "ince", "Irmak", "ılık"]),
            ["ılık", "Irmak", "ince", "İnce", "kar", "Kar", "kâr", "kara"]
        );
    }

    #[test]
    fn decomposed_letters_sort_as_the_letters() {
        for (nfd, nfc) in [
            ("c\u{327}", "ç"),
            ("C\u{327}", "Ç"),
            ("g\u{306}", "ğ"),
            ("I\u{307}", "İ"),
            ("o\u{308}", "ö"),
            ("s\u{327}", "ş"),
            ("U\u{308}", "Ü"),
            ("e\u{301}", "é"),
        ] {
            for (x, y) in [("", ""), ("a", "b"), ("", "z"), ("ş", "")] {
                let a = format!("{x}{nfd}{y}");
                let b = format!("{x}{nfc}{y}");
                // Equal at every level; only the bytes tell them apart.
                for l in 0..3 {
                    assert_eq!(level(&a, &b, l), Ordering::Equal, "{a:?} {b:?} {l}");
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
            level("ç\u{301}", "ç\u{300}", 1)
        );
    }

    #[test]
    fn different_strings_never_tie() {
        // U+00AD, a soft hyphen, is ignorable at every level.
        let c = Collation::Turkish;
        assert_eq!(c.compare("ab", "a\u{ad}b"), "ab".cmp("a\u{ad}b"));
        assert_eq!(c.compare("a\u{ad}b", "ab"), "a\u{ad}b".cmp("ab"));
        assert_eq!(c.compare("", ""), Ordering::Equal);
        assert_eq!(c.compare("", "\u{ad}"), Ordering::Less);
    }

    #[test]
    fn outside_the_table() {
        // Another script's letters after every Latin one, case folded;
        // other symbols before the digits.
        assert_eq!(
            sorted(&["б", "Zeynep", "А", "0", "★", "a", "а"]),
            ["★", "0", "a", "Zeynep", "а", "А", "б"]
        );
    }
}
