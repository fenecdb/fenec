//! A string's case, as `str::to_lowercase` and `str::to_uppercase` put it,
//! without their code: done a character at a time instead, the browser
//! module was 9.6 KB smaller, and `to_lowercase` slices a string where a
//! slice can panic, which kept the formatting of a panic's `char` --
//! `escape_debug` and Unicode's tables of what prints -- in the module.
//!
//! A character's own case comes from `char::to_lowercase` and
//! `char::to_uppercase`, the same table the string methods read. The one
//! mapping that looks at its neighbours is a capital sigma's: `ς` ending a
//! word, `σ` elsewhere -- Unicode's Final_Sigma, which needs to know which
//! characters are cased and which are case-ignorable. `char` says whether a
//! character is lowercase or uppercase; the 31 titlecase letters and the
//! case-ignorable runs are written down here, and
//! `the_tables_are_the_standard_library_s` derives both from `to_lowercase`
//! itself, character by character -- after a toolchain moves to a new
//! Unicode it fails and prints the table to put here.

/// The case-ignorable code points (Unicode's Case_Ignorable), as runs: the
/// gap from the end of the run before, then the run's length less one, in
/// LEB128.
const CASE_IGNORABLE: [u8; 987] = [
    39, 0, 6, 0, 11, 0, 35, 0, 1, 0, 71, 0, 4, 0, 1, 0, 4, 0, 2, 1, 247, 3, 191, 1, 4, 1, 4, 0, 9,
    1, 1, 0, 251, 1, 6, 207, 1, 0, 5, 0, 49, 44, 1, 0, 1, 1, 1, 1, 1, 0, 44, 0, 11, 5, 10, 10, 1,
    0, 35, 0, 10, 20, 16, 0, 101, 7, 1, 9, 1, 3, 33, 0, 1, 0, 30, 26, 91, 10, 58, 10, 4, 0, 2, 0,
    24, 23, 43, 2, 44, 0, 7, 1, 5, 8, 41, 57, 55, 0, 1, 0, 4, 7, 4, 0, 3, 6, 10, 1, 13, 0, 15, 0,
    58, 0, 4, 3, 8, 0, 20, 1, 26, 0, 2, 1, 57, 0, 4, 1, 4, 1, 2, 2, 3, 0, 30, 1, 3, 0, 11, 1, 57,
    0, 4, 4, 1, 1, 4, 0, 20, 1, 22, 5, 1, 0, 58, 0, 2, 0, 1, 3, 8, 0, 7, 1, 11, 1, 30, 0, 61, 0,
    12, 0, 50, 0, 3, 0, 55, 0, 1, 2, 5, 2, 1, 3, 7, 1, 11, 1, 29, 0, 58, 0, 2, 0, 6, 0, 5, 1, 20,
    1, 28, 1, 57, 1, 4, 3, 8, 0, 20, 1, 29, 0, 72, 0, 7, 2, 1, 0, 90, 0, 2, 6, 11, 8, 98, 0, 2, 8,
    9, 0, 1, 6, 73, 1, 27, 0, 1, 0, 1, 0, 55, 13, 1, 4, 1, 1, 5, 10, 1, 35, 9, 0, 102, 3, 1, 5, 1,
    1, 2, 1, 25, 1, 4, 2, 16, 3, 13, 0, 2, 1, 6, 0, 15, 0, 94, 0, 224, 4, 2, 178, 7, 2, 29, 1, 30,
    1, 30, 1, 64, 1, 1, 6, 8, 0, 2, 10, 3, 0, 5, 0, 45, 4, 51, 0, 65, 1, 34, 0, 118, 2, 4, 1, 9, 0,
    6, 2, 219, 1, 1, 2, 0, 58, 0, 1, 6, 1, 0, 1, 0, 2, 7, 6, 9, 2, 0, 39, 0, 8, 45, 2, 11, 20, 3,
    48, 0, 1, 4, 1, 0, 5, 0, 40, 8, 12, 1, 32, 3, 2, 1, 1, 2, 56, 0, 1, 1, 3, 0, 1, 2, 58, 7, 2, 1,
    64, 5, 82, 2, 1, 12, 1, 6, 4, 0, 6, 0, 3, 1, 50, 62, 13, 0, 34, 100, 189, 3, 0, 1, 2, 11, 2,
    13, 2, 13, 2, 13, 1, 12, 4, 8, 1, 10, 0, 2, 0, 2, 4, 49, 4, 1, 9, 1, 0, 13, 0, 16, 12, 51, 32,
    139, 23, 1, 113, 2, 125, 0, 15, 0, 96, 31, 47, 0, 213, 3, 0, 36, 3, 3, 4, 5, 0, 93, 5, 93, 2,
    150, 222, 1, 0, 226, 9, 5, 142, 2, 0, 98, 3, 1, 9, 1, 0, 28, 3, 80, 1, 14, 33, 78, 0, 23, 2,
    102, 3, 3, 1, 8, 0, 3, 0, 4, 0, 25, 1, 5, 0, 151, 1, 1, 26, 17, 13, 0, 38, 7, 25, 10, 46, 2,
    48, 0, 2, 3, 2, 1, 17, 0, 21, 1, 66, 5, 2, 1, 2, 1, 12, 0, 8, 0, 35, 0, 11, 0, 51, 0, 1, 2, 2,
    1, 5, 1, 1, 0, 27, 0, 14, 1, 5, 1, 1, 0, 100, 4, 9, 2, 121, 0, 2, 0, 4, 0, 176, 158, 1, 0, 147,
    1, 16, 189, 4, 15, 3, 0, 12, 15, 34, 0, 2, 0, 169, 1, 0, 7, 0, 6, 0, 11, 0, 35, 0, 1, 0, 47, 0,
    45, 1, 67, 0, 21, 2, 129, 4, 0, 226, 1, 0, 149, 1, 4, 133, 8, 5, 1, 41, 1, 8, 198, 4, 2, 1, 1,
    5, 3, 40, 2, 4, 0, 165, 1, 1, 189, 4, 3, 38, 0, 26, 4, 1, 0, 187, 2, 1, 24, 0, 52, 5, 70, 10,
    49, 3, 123, 0, 54, 14, 41, 0, 2, 1, 10, 2, 49, 3, 2, 1, 2, 0, 4, 0, 10, 0, 50, 2, 36, 4, 1, 7,
    62, 0, 12, 1, 52, 8, 10, 3, 2, 0, 95, 2, 2, 0, 1, 1, 6, 0, 2, 0, 157, 1, 0, 3, 7, 21, 1, 57, 1,
    3, 0, 37, 6, 3, 4, 70, 5, 13, 0, 1, 0, 1, 0, 14, 1, 85, 7, 2, 2, 1, 0, 23, 0, 84, 5, 1, 0, 4,
    1, 1, 1, 238, 1, 3, 6, 1, 1, 1, 27, 1, 85, 7, 2, 0, 1, 1, 106, 0, 1, 0, 2, 5, 1, 0, 101, 0, 1,
    0, 2, 3, 1, 4, 131, 2, 8, 1, 1, 128, 2, 1, 1, 0, 4, 0, 144, 1, 3, 2, 1, 4, 0, 32, 9, 40, 5, 2,
    3, 8, 0, 9, 5, 2, 2, 46, 12, 1, 1, 198, 1, 0, 1, 2, 1, 0, 201, 1, 6, 1, 5, 1, 0, 82, 21, 2, 6,
    1, 1, 1, 1, 122, 5, 3, 0, 1, 1, 1, 6, 1, 0, 72, 1, 3, 0, 1, 0, 65, 0, 153, 2, 1, 11, 1, 52, 4,
    5, 0, 1, 0, 23, 0, 213, 41, 16, 6, 14, 200, 89, 11, 3, 2, 192, 19, 4, 59, 6, 9, 3, 252, 3, 2,
    40, 1, 226, 3, 0, 63, 16, 64, 1, 1, 1, 13, 1, 252, 127, 3, 1, 6, 1, 1, 158, 25, 1, 1, 3, 220,
    36, 45, 2, 22, 160, 4, 2, 9, 15, 2, 6, 30, 3, 148, 1, 2, 187, 15, 54, 4, 49, 8, 0, 14, 0, 22,
    4, 1, 14, 208, 10, 6, 1, 16, 2, 6, 1, 1, 1, 4, 5, 61, 33, 0, 160, 1, 13, 240, 2, 0, 61, 3, 251,
    3, 4, 254, 1, 1, 243, 1, 0, 2, 0, 7, 1, 5, 0, 9, 0, 208, 3, 6, 109, 7, 175, 21, 4, 129, 152,
    48, 0, 30, 95, 128, 1, 239, 1,
];

/// `s` in lower case, as `str::to_lowercase` puts it.
///
/// ASCII is mapped as ASCII, the whole string at once when it is all ASCII:
/// a character at a time through `char::to_lowercase`, `lower` over 100 000
/// rows of ASCII names took 22.3 ms against the standard library's 18.8.
pub fn lower(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    let mut out = String::with_capacity(s.len());
    for (i, c) in s.char_indices() {
        if c.is_ascii() {
            out.push(c.to_ascii_lowercase());
        } else if c == 'Σ' {
            // Final: a cased letter before it and none after, skipping the
            // case-ignorable characters between. The slices are at the
            // character's own boundaries, so `get` never answers `None`.
            let before = s.get(..i).is_some_and(|b| cased_first(b.chars().rev()));
            let after = s
                .get(i + 'Σ'.len_utf8()..)
                .is_some_and(|a| cased_first(a.chars()));
            out.push(if before && !after { 'ς' } else { 'σ' });
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// `s` in upper case, as `str::to_uppercase` puts it: character by
/// character, since no uppercase mapping looks at the neighbours, and ASCII
/// as ASCII, as in `lower`.
pub fn upper(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_uppercase();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c.to_ascii_uppercase());
        } else {
            out.extend(c.to_uppercase());
        }
    }
    out
}

/// The case fold a `nocase` field's text is compared by (`email text
/// nocase`): what its `=`, its indexes, its order and its groups see in
/// place of the text, which it keeps as written.
///
/// Unicode's full case folding, CaseFolding.txt's C and F mappings, which
/// Python's `str.casefold()` is too: `ß`, `ẞ` and `SS` fold to `ss`, `ς` to
/// `σ`, `ﬁ` to `fi`, `İ` to `i` and a combining dot above. Full rather than
/// simple (C and S, one code point to one): simple folding keeps `ß` apart
/// from `ss`, which ICU's case-insensitive comparison and every German
/// reader find equal, and it would be a table of its own -- where the full
/// fold is the standard library's own lowercase and uppercase mappings,
/// composed (`fold_char`), already in the browser module for `lower`,
/// `upper` and the tokenizer. Nothing is normalised: `é` written as one code
/// point and as `e` and a combining accent stay two texts, as they do
/// everywhere else in fenecdb, and a full-width `Ａ` folds to `ａ`, not `a`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseFold {
    /// Unicode's, for every language.
    Unicode,
    /// Turkish's, a `collate tr` field's: CaseFolding.txt's T mappings over
    /// Unicode's, `I` folding to `ı` and `İ` to `i`, so `ISTANBUL` is
    /// `ıstanbul` and `İSTANBUL` is `istanbul`, as a Turkish reader writes
    /// them -- Unicode's own makes `ISTANBUL` `istanbul` and leaves `ı`
    /// alone.
    Turkish,
}

/// The code points past ASCII whose fold is not their lowercase, as runs,
/// first and last: `µ`, `ß`, `ŉ`, `ſ`, `ǰ`, the combining iota, Greek's
/// final sigma, its symbol letters and its letters with a subscript iota,
/// `և`, Cyrillic's old letter forms, `ẖ`..`ẛ`, `ẞ`, Cherokee, the Latin
/// and Armenian ligatures. Every other folds by one lookup of its
/// lowercase, where the three of [`fold_char`]'s composition, for every
/// character, took a scan for one name over 100 000 rows, a third of them
/// past ASCII, to 8.3 ms against 6.7, an order by it to 20.1 against 16.2
/// (`make nocase-bench`). `the_unlike_runs_are_the_standard_library_s`
/// derives them from the standard library's mappings, and prints them
/// again after a toolchain moves to a new Unicode.
const UNLIKE_LOWER: [(u16, u16); 41] = [
    (0xb5, 0xb5),
    (0xdf, 0xdf),
    (0x149, 0x149),
    (0x17f, 0x17f),
    (0x1f0, 0x1f0),
    (0x345, 0x345),
    (0x390, 0x390),
    (0x3b0, 0x3b0),
    (0x3c2, 0x3c2),
    (0x3d0, 0x3d1),
    (0x3d5, 0x3d6),
    (0x3f0, 0x3f1),
    (0x3f5, 0x3f5),
    (0x587, 0x587),
    (0x13a0, 0x13f5),
    (0x13f8, 0x13fd),
    (0x1c80, 0x1c88),
    (0x1e96, 0x1e9b),
    (0x1e9e, 0x1e9e),
    (0x1f50, 0x1f50),
    (0x1f52, 0x1f52),
    (0x1f54, 0x1f54),
    (0x1f56, 0x1f56),
    (0x1f80, 0x1faf),
    (0x1fb2, 0x1fb4),
    (0x1fb6, 0x1fb7),
    (0x1fbc, 0x1fbc),
    (0x1fbe, 0x1fbe),
    (0x1fc2, 0x1fc4),
    (0x1fc6, 0x1fc7),
    (0x1fcc, 0x1fcc),
    (0x1fd2, 0x1fd3),
    (0x1fd6, 0x1fd7),
    (0x1fe2, 0x1fe4),
    (0x1fe6, 0x1fe7),
    (0x1ff2, 0x1ff4),
    (0x1ff6, 0x1ff7),
    (0x1ffc, 0x1ffc),
    (0xab70, 0xabbf),
    (0xfb00, 0xfb06),
    (0xfb13, 0xfb17),
];

/// Whether `c`'s fold is not its lowercase ([`UNLIKE_LOWER`]).
fn unlike_lower(c: char) -> bool {
    let c = c as u32;
    (0xb5..=0xfb17).contains(&c) && {
        let i = UNLIKE_LOWER.partition_point(|r| (r.1 as u32) < c);
        UNLIKE_LOWER.get(i).is_some_and(|r| c >= r.0 as u32)
    }
}

/// `c`'s fold: one to three characters, in the array's front.
///
/// Mostly its lowercase ([`UNLIKE_LOWER`]). Where it is not, the standard
/// library's lowercase, then uppercase, then lowercase again: a capital's
/// lowercase is mostly its fold already, the uppercase finds the characters
/// lowercase leaves alone that fold to another (`ς`, `ſ`, `ß` through
/// `SS`, `ﬁ` through `FI`), and the first lowercase is what takes `ẞ` to
/// `ß` before the uppercase takes it to `SS`. Two places differ from
/// CaseFolding.txt, and are written down: the dotless `ı`, which no mapping
/// folds but whose uppercase is `I`, and Cherokee, whose capitals were
/// encoded first and are what its letters fold to. Held to
/// `str.casefold()` at every code point (`tests/nocase.rs`).
#[inline]
fn fold_char(c: char, f: CaseFold) -> ([char; 3], usize) {
    let one = |c| ([c, '\0', '\0'], 1);
    if c.is_ascii() {
        return match (c, f) {
            ('I', CaseFold::Turkish) => one('ı'),
            _ => one(c.to_ascii_lowercase()),
        };
    }
    match (c, f) {
        ('ı', _) => return one('ı'),
        ('İ', CaseFold::Turkish) => return one('i'),
        _ => {}
    }
    let (mut out, mut n) = (['\0'; 3], 0);
    if !unlike_lower(c) {
        for x in c.to_lowercase() {
            if n < 3 {
                out[n] = x;
                n += 1;
            }
        }
        return (out, n);
    }
    for l in c.to_lowercase() {
        for u in l.to_uppercase() {
            for x in u.to_lowercase() {
                let x = match x as u32 {
                    // Cherokee's small letters fold to its capitals.
                    0xAB70..=0xABBF => char::from_u32(x as u32 - 0xAB70 + 0x13A0).unwrap_or(x),
                    0x13F8..=0x13FD => char::from_u32(x as u32 - 8).unwrap_or(x),
                    _ => x,
                };
                // Three at most at every code point, as CaseFolding.txt's
                // longest; the test holds every code point to it.
                if n < 3 {
                    out[n] = x;
                    n += 1;
                }
            }
        }
    }
    (out, n)
}

/// `s`'s fold: `s` itself when it is ASCII with no capital, the commonest
/// key of all -- an email, a handle, a code -- in either fold.
pub fn fold(s: &str, f: CaseFold) -> std::borrow::Cow<'_, str> {
    if s.is_ascii() && !s.bytes().any(|b| b.is_ascii_uppercase()) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    fold_into(s, f, &mut out);
    std::borrow::Cow::Owned(out)
}

/// `s`'s fold, appended to `out`.
pub fn fold_into(s: &str, f: CaseFold, out: &mut String) {
    let mut rest = s;
    while !rest.is_empty() {
        // The ASCII up to the next character that is not, lowercased as a
        // run -- `I` with the rest in Turkish.
        let n = ascii_run(rest.as_bytes(), f);
        let (run, after) = rest.split_at_checked(n).unwrap_or((rest, ""));
        out.push_str(run);
        let at = out.len() - run.len();
        if let Some(t) = out.get_mut(at..) {
            t.make_ascii_lowercase();
        }
        let mut chars = after.chars();
        if let Some(c) = chars.next() {
            let (cs, n) = fold_char(c, f);
            out.extend(&cs[..n]);
        }
        rest = chars.as_str();
    }
}

/// How many bytes of `b` from its start are ASCII the fold lowercases as
/// ASCII: all of it but a Turkish `I`.
#[inline]
fn ascii_run(b: &[u8], f: CaseFold) -> usize {
    b.iter()
        .position(|&x| x >= 0x80 || (x == b'I' && f == CaseFold::Turkish))
        .unwrap_or(b.len())
}

/// The characters of a text's fold, one at a time, so two texts are
/// compared by their folds with neither written out.
struct Folds<'a> {
    chars: std::str::Chars<'a>,
    f: CaseFold,
    held: [char; 3],
    at: usize,
    len: usize,
}

impl<'a> Folds<'a> {
    fn new(s: &'a str, f: CaseFold) -> Folds<'a> {
        Folds {
            chars: s.chars(),
            f,
            held: ['\0'; 3],
            at: 0,
            len: 0,
        }
    }
}

impl Iterator for Folds<'_> {
    type Item = char;
    #[inline]
    fn next(&mut self) -> Option<char> {
        if self.at == self.len {
            (self.held, self.len) = fold_char(self.chars.next()?, self.f);
            self.at = 0;
        }
        self.at += 1;
        Some(self.held[self.at - 1])
    }
}

/// `a`'s fold against `folded`, a text that is a fold already -- a filter's
/// value, folded where it was bound -- as their bytes compare, `a` folded
/// only as far as the two agree: a scan's row is mostly told apart at its
/// first character. An ASCII `a` is lowercased byte by byte as it is read,
/// against `folded`'s bytes, which order as its code points do.
pub fn cmp_to_folded(a: &str, folded: &str, f: CaseFold) -> std::cmp::Ordering {
    let (x, y) = (a.as_bytes(), folded.as_bytes());
    let mut i = 0;
    // The ASCII `a` starts with, lowercased byte by byte against `folded`'s
    // bytes, which order as their code points do: an ASCII byte against
    // the first of another character's is below it, as the character is.
    while i < x.len() && i < y.len() && x[i] < 0x80 && (x[i] != b'I' || f == CaseFold::Unicode) {
        let o = x[i].to_ascii_lowercase().cmp(&y[i]);
        if o != std::cmp::Ordering::Equal {
            return o;
        }
        i += 1;
    }
    // From the first character that is not, where both are at a
    // character's start: every byte before it was ASCII on both sides.
    let (a, folded) = (a.get(i..).unwrap_or(""), folded.get(i..).unwrap_or(""));
    Folds::new(a, f).cmp(folded.chars())
}

/// `a`'s fold against `b`'s, as their bytes compare -- code point by code
/// point, which UTF-8 keeps -- with neither written out: the ASCII both
/// start with compared byte by byte, lowercased as it is read.
pub fn cmp_folded(a: &str, b: &str, f: CaseFold) -> std::cmp::Ordering {
    let (x, y) = (a.as_bytes(), b.as_bytes());
    let plain = |c: u8| c < 0x80 && (c != b'I' || f == CaseFold::Unicode);
    let mut i = 0;
    while i < x.len() && i < y.len() && plain(x[i]) && plain(y[i]) {
        let o = x[i].to_ascii_lowercase().cmp(&y[i].to_ascii_lowercase());
        if o != std::cmp::Ordering::Equal {
            return o;
        }
        i += 1;
    }
    let (a, b) = (a.get(i..).unwrap_or(""), b.get(i..).unwrap_or(""));
    Folds::new(a, f).cmp(Folds::new(b, f))
}

/// Whether `a` and `b` fold to one text: [`cmp_folded`]'s answer, whose
/// ASCII path is `eq_ignore_ascii_case`'s -- a walk of its own was 254
/// bytes of the browser module.
pub fn eq_folded(a: &str, b: &str, f: CaseFold) -> bool {
    cmp_folded(a, b, f) == std::cmp::Ordering::Equal
}

/// Whether the first character not case-ignorable is cased.
fn cased_first(mut chars: impl Iterator<Item = char>) -> bool {
    chars.find(|&c| !case_ignorable(c)).is_some_and(cased)
}

/// Unicode's Cased: lowercase, uppercase or titlecase.
fn cased(c: char) -> bool {
    c.is_lowercase()
        || c.is_uppercase()
        || matches!(c as u32, 0x1C5 | 0x1C8 | 0x1CB | 0x1F2 | 0x1F88..=0x1F8F
            | 0x1F98..=0x1F9F | 0x1FA8..=0x1FAF | 0x1FBC | 0x1FCC | 0x1FFC)
}

fn case_ignorable(c: char) -> bool {
    let (c, mut at, mut end) = (c as u32, 0, 0);
    while at < CASE_IGNORABLE.len() {
        let start = end + leb(&mut at);
        if c < start {
            return false;
        }
        end = start + leb(&mut at) + 1;
        if c < end {
            return true;
        }
    }
    false
}

fn leb(at: &mut usize) -> u32 {
    let (mut v, mut shift) = (0, 0);
    loop {
        let b = CASE_IGNORABLE[*at];
        *at += 1;
        v |= ((b & 0x7F) as u32) << shift;
        if b < 0x80 {
            return v;
        }
        shift += 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `lower("AΣ" + c + tail)` names `σ` or `ς` for the sigma: what the
    /// standard library decided about `c`.
    fn sigma_after(c: char, tail: &str) -> char {
        let s: String = ['A', 'Σ', c]
            .iter()
            .chain(tail.chars().collect::<Vec<_>>().iter())
            .collect();
        s.to_lowercase().chars().nth(1).unwrap()
    }

    #[test]
    fn the_tables_are_the_standard_library_s() {
        // "AΣc" ends a word unless `c` is cased and not ignorable; "AΣcA"
        // unless `c` is ignorable or cased. A cased character the char
        // methods do not call lowercase or uppercase is titlecase.
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for c in (0..0x11_0000).filter_map(char::from_u32) {
            let alone = sigma_after(c, "") == 'σ';
            if alone && !c.is_lowercase() && !c.is_uppercase() {
                assert!(
                    cased(c),
                    "{:#x} is titlecase to the standard library",
                    c as u32
                );
            }
            let ignorable = if cased(c) {
                !alone
            } else {
                sigma_after(c, "A") == 'σ'
            };
            if !ignorable {
                assert!(
                    !(cased(c) && !alone),
                    "{:#x} is not cased to the standard library",
                    c as u32
                );
                continue;
            }
            match runs.last_mut() {
                Some(r) if r.1 + 1 == c as u32 => r.1 = c as u32,
                _ => runs.push((c as u32, c as u32)),
            }
        }
        let mut table = Vec::new();
        let mut end = 0;
        for (a, b) in runs {
            for mut x in [a - end, b - a] {
                while x >= 0x80 {
                    table.push((x & 0x7F) as u8 | 0x80);
                    x >>= 7;
                }
                table.push(x as u8);
            }
            end = b + 1;
        }
        assert!(
            table == CASE_IGNORABLE,
            "CASE_IGNORABLE is now ({} bytes): {table:?}",
            table.len()
        );
    }

    #[test]
    fn the_unlike_runs_are_the_standard_library_s() {
        // The code points whose lowercase, uppercase and lowercase again
        // are not their lowercase: `ı` aside, and Cherokee's capitals,
        // whose composition is their lowercase and their fold not.
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for c in (0x80..0x11_0000).filter_map(char::from_u32) {
            let lower: String = c.to_lowercase().collect();
            let composed: String = c
                .to_lowercase()
                .flat_map(char::to_uppercase)
                .flat_map(char::to_lowercase)
                .collect();
            let cherokee = matches!(c as u32, 0x13A0..=0x13F5 | 0x13F8..=0x13FD | 0xAB70..=0xABBF);
            if (composed == lower && !cherokee) || c == 'ı' {
                continue;
            }
            match runs.last_mut() {
                Some(r) if r.1 + 1 == c as u32 => r.1 = c as u32,
                _ => runs.push((c as u32, c as u32)),
            }
        }
        let table: Vec<(u32, u32)> = UNLIKE_LOWER
            .iter()
            .map(|r| (r.0 as u32, r.1 as u32))
            .collect();
        assert!(
            table == runs,
            "UNLIKE_LOWER is now ({} runs): {runs:x?}",
            runs.len()
        );
    }

    #[test]
    fn every_character_maps_as_the_standard_library_maps_it() {
        let mut s = String::new();
        for c in (0..0x11_0000).filter_map(char::from_u32) {
            s.clear();
            s.push(c);
            assert_eq!(lower(&s), s.to_lowercase(), "{:#x}", c as u32);
            assert_eq!(upper(&s), s.to_uppercase(), "{:#x}", c as u32);
        }
    }

    #[test]
    fn a_final_sigma_is_found_as_the_standard_library_finds_it() {
        // Letters, marks and quotes around sigmas, in every arrangement up
        // to four: `ΣΑ'`, `Α̈Σ`, `ǅΣ`, `A:Σ`...
        let parts = [
            "Σ", "Α", "α", "A", "'", "\u{308}", "\u{1C5}", ":", " ", "ς", "σ", "\u{2B0}", "1",
        ];
        let mut stack = vec![String::new()];
        while let Some(s) = stack.pop() {
            assert_eq!(lower(&s), s.to_lowercase(), "{s:?}");
            assert_eq!(upper(&s), s.to_uppercase(), "{s:?}");
            if s.chars().count() < 4 {
                for p in parts {
                    stack.push(format!("{s}{p}"));
                }
            }
        }
        assert_eq!(lower("ΟΔΟΣ ΟΔΟΣ."), "οδος οδος.");
        assert_eq!(lower("ΣΑΣ"), "σας");
    }
}
