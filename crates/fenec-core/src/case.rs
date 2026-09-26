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
