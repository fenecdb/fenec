//! What `highlight()` and `snippet()` mark: the spans of a field's text
//! that the terms of a query's `match` were read from.
//!
//! **The text is read again, by the index's own tokenizer.** Nothing about
//! where a term stood is kept in the index -- its postings hold counts, and
//! positions would have been a file format and a third of the postings
//! again -- so a row's text is split at query time with the same splitting,
//! the same folding (`İstanbul`, `ISTANBUL` and `istanbul` are one term)
//! and the same prefixes and runs of characters the index was built with
//! (`text::indexed_terms`), and a span is marked where a term it yields is
//! one of the query's. A mark is therefore exactly a term the index
//! matched: a word, the whole word a prefix (`prefix=N`) was cut from, and
//! the characters of a pair (or a triple) of Han, kana, Hangul or Thai.
//! The same query over the same text marks the same spans in every build.
//!
//! Spans are bytes here; what goes out is UTF-16 code units (`utf16`), the
//! index a JavaScript, Java, Kotlin, C#, Dart or Swift (`NSString`) string
//! is read by. A span never ends inside a character, and is widened over
//! the marks that belong to the characters at its ends (`extends`): a
//! combining accent after a word, a Thai tone mark after a run, the
//! variation selector after a digit -- marked text can never split a
//! character from its accent.

use crate::maps::Map;
use crate::schema::TextIndexSpec;
use crate::text::{indexed_terms, splits_words};

/// A query's terms, as the index of the field `match` ran over reads them,
/// each once. In the map type the index's own search counts them in: a
/// sorted `Vec<String>` was a sort of strings of its own, 3.5 KB of the
/// browser module.
pub struct Terms {
    spec: TextIndexSpec,
    terms: Map<String, u32>,
}

impl Terms {
    pub fn new(query: &str, spec: TextIndexSpec) -> Terms {
        let mut terms: Map<String, u32> = Map::default();
        indexed_terms(query, &spec, &mut |t, _, _| {
            if !terms.contains_key(t) {
                terms.insert(t.to_string(), 1);
            }
        });
        Terms { spec, terms }
    }

    /// The spans of `text` a term of the query was read from, in order, an
    /// overlap or two that touch made one (a run of Han read in pairs is one
    /// span, not a pair each), each widened to whole characters with their
    /// marks.
    pub fn spans(&self, text: &str) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = Vec::new();
        if self.terms.is_empty() {
            return out;
        }
        indexed_terms(text, &self.spec, &mut |t, from, to| {
            if !self.terms.contains_key(t) {
                return;
            }
            let (from, to) = whole(text, from, to);
            // The terms come in the order of their spans' starts, a word's
            // prefixes after it on the same span, so only the last can meet
            // this one.
            match out.last_mut() {
                Some(last) if from <= last.1 => last.1 = last.1.max(to),
                _ => out.push((from, to)),
            }
        });
        out
    }
}

/// `from..to` widened to whole characters with their marks: back to the
/// character a mark at its start belongs to, on past the marks after its
/// end.
fn whole(text: &str, mut from: usize, mut to: usize) -> (usize, usize) {
    while let Some(c) = text.get(..from).and_then(|s| s.chars().next_back()) {
        if !text
            .get(from..)
            .and_then(|s| s.chars().next())
            .is_some_and(extends)
        {
            break;
        }
        from -= c.len_utf8();
    }
    while let Some(c) = text.get(to..).and_then(|s| s.chars().next()) {
        if !extends(c) {
            break;
        }
        to += c.len_utf8();
    }
    (from, to)
}

/// Whether `c` belongs to the character before it: a combining mark, a
/// joiner, a variation selector, a skin tone, a Hangul vowel or final
/// consonant jamo. Not Unicode's whole table, which no part of the engine
/// carries: the blocks a text puts such marks in, each of which the
/// tokenizer would otherwise leave outside a word.
fn extends(c: char) -> bool {
    let u = c as u32;
    // A letter of an Indic script's block is a word's own, which a span
    // holds whole already; only the signs between them are asked.
    if (0x0900..=0x0DFF).contains(&u) {
        return !c.is_alphanumeric() && !matches!(u, 0x0964..=0x0965);
    }
    matches!(
        u,
        0x0300..=0x036F         // combining diacritical marks
            | 0x0483..=0x0489   // Cyrillic
            | 0x0591..=0x05BD | 0x05BF | 0x05C1..=0x05C2 | 0x05C4..=0x05C5 | 0x05C7 // Hebrew
            | 0x0610..=0x061A | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06DC | 0x06DF..=0x06E4
            | 0x06E7..=0x06E8 | 0x06EA..=0x06ED // Arabic
            | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E // Thai
            | 0x0EB1 | 0x0EB4..=0x0EBC | 0x0EC8..=0x0ECD // Lao
            | 0x0F18..=0x0F19 | 0x0F35 | 0x0F37 | 0x0F39 | 0x0F71..=0x0F84 // Tibetan
            | 0x102B..=0x103E | 0x17B4..=0x17D3 // Myanmar, Khmer
            | 0x1160..=0x11FF   // Hangul vowel and final jamo
            | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF // more combining marks
            | 0x200C..=0x200D   // zero-width (non-)joiner
            | 0x20D0..=0x20FF   // combining marks for symbols, the keycap
            | 0x3099..=0x309A   // kana voicing marks
            | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F // variation selectors, half marks
            | 0x1F3FB..=0x1F3FF // skin tones
            | 0xE0100..=0xE01EF // variation selectors supplement
    )
}

/// The spans of `text` a snippet is counted in: its words, and each
/// character of a run the index reads in pairs or triples.
fn units(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for raw in text.split(splits_words) {
        if raw.is_empty() || raw.chars().all(|c| c == '\u{0307}') {
            continue;
        }
        let base = raw.as_ptr() as usize - text.as_ptr() as usize;
        let mut word: Option<usize> = None;
        for (i, c) in raw.char_indices() {
            if crate::text::gram(c) > 0 {
                if let Some(w) = word.take() {
                    out.push((base + w, base + i));
                }
                out.push((base + i, base + i + c.len_utf8()));
            } else if word.is_none() {
                word = Some(i);
            }
        }
        if let Some(w) = word {
            out.push((base + w, base + raw.len()));
        }
    }
    out
}

/// The window of `tokens` words a snippet shows: the one holding the most
/// that a span marks, centred on the first and the last of them, and the
/// first `tokens` words where none is marked. Bytes of `text`, from the
/// start of the text when the window holds its first word and to its end
/// when the window holds its last.
pub fn window(text: &str, spans: &[(usize, usize)], tokens: usize) -> (usize, usize) {
    let units = units(text);
    let n = units.len();
    if n <= tokens || tokens == 0 {
        return (0, text.len());
    }
    // Whether each word is marked: the spans and the words both run in
    // order, so one walk tells.
    let mut hit = vec![false; n];
    let mut s = 0;
    for (i, u) in units.iter().enumerate() {
        while s < spans.len() && spans[s].1 <= u.0 {
            s += 1;
        }
        hit[i] = s < spans.len() && spans[s].0 < u.1;
    }
    // The densest window, the earliest of the densest.
    let mut count = hit[..tokens].iter().filter(|h| **h).count();
    let (mut best, mut at) = (count, 0);
    for start in 1..=n - tokens {
        count += hit[start + tokens - 1] as usize;
        count -= hit[start - 1] as usize;
        if count > best {
            best = count;
            at = start;
        }
    }
    if best > 0 {
        // Its marked words all fit, so centred on them it still holds them.
        let first = (at..at + tokens).find(|&i| hit[i]).unwrap_or(at);
        let last = (at..at + tokens).rev().find(|&i| hit[i]).unwrap_or(at);
        let mid = (first + last) / 2;
        at = mid.saturating_sub(tokens / 2).min(n - tokens);
    }
    let from = if at == 0 {
        0
    } else {
        whole(text, units[at].0, units[at].1).0
    };
    let to = if at + tokens == n {
        text.len()
    } else {
        whole(text, units[at + tokens - 1].0, units[at + tokens - 1].1).1
    };
    (from, to)
}

/// `bytes` -- offsets into `text`, ascending -- as UTF-16 code units: the
/// units a JavaScript string's index counts. One walk over the text.
pub fn utf16(text: &str, bytes: &mut [usize]) {
    let (mut b, mut u) = (0usize, 0usize);
    let mut chars = text.chars();
    for x in bytes.iter_mut() {
        while b < *x {
            let Some(c) = chars.next() else { break };
            b += c.len_utf8();
            u += c.len_utf16();
        }
        *x = u;
    }
}

/// How many UTF-16 code units `s` is.
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `text` with each span wrapped in `pre` and `post`. The text is not
/// escaped: the tags are the caller's, and so is what it renders them in.
pub fn marked(text: &str, spans: &[(usize, usize)], pre: &str, post: &str) -> String {
    let mut out = String::with_capacity(text.len() + spans.len() * (pre.len() + post.len()));
    let mut at = 0;
    for &(from, to) in spans {
        out.push_str(text.get(at..from).unwrap_or(""));
        out.push_str(pre);
        out.push_str(text.get(from..to).unwrap_or(""));
        out.push_str(post);
        at = to;
    }
    out.push_str(text.get(at..).unwrap_or(""));
    out
}
