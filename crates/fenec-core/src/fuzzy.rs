//! What `match ... fuzzy` and `match ... prefix` find near a query's words:
//! the words of a text index within a few edits of one, and the words one
//! begins.
//!
//! **A sorted dictionary, walked as the trie it is.** The text index keeps
//! its terms in a hash map, which answers "this word" and nothing near it.
//! So the first `fuzzy` or `prefix` over an index sorts its words into a
//! [`Dictionary`] -- derived data as the index is, never in the file -- and
//! the writes keep it up from then on. A word's prefix is a range of it,
//! two searches; the words within `k` edits of one are found by walking it
//! depth first, a row of the edit distance's table a character, each node's
//! children found by binary search, a branch given up once its row holds
//! nothing within `k`. Where a character the query does not hold would end
//! the row, only the characters it holds near that depth are looked for.
//!
//! Measured over FiQA's and SciFact's 99 558 words, 3 000 query words each
//! with a typo: this walk 39 us a word at one edit and 396 at two, 1.2 MB;
//! SymSpell's deletions 9.4 us, but 303.6 MB and 2.0 s to build; an index of
//! the words' trigrams 8.4 MB and 316 us, and no prefixes; every word read
//! with the bounded table, 4.3 ms. The same walk over a `Box<str>` a word
//! was 2.4 MB, and 25% slower for the pointer each comparison followed.
//!
//! **The distance is Damerau's, restricted** -- optimal string alignment:
//! two letters swapped are one edit, as a third or more of typing errors
//! are (Damerau 1964), where plain Levenshtein counts them two and a
//! five-letter word with a swap would be out of reach. It is what Lucene's
//! `FuzzyQuery`, Elasticsearch's `fuzzy_transpositions` and Meilisearch
//! default to. **An edit that touches a word's first character counts two**,
//! as Meilisearch counts it: a first letter is mistyped least often (7% of
//! misspellings in Pollock and Zamora's 50 000), and allowing it at one edit
//! makes every first letter a branch to walk -- the walk went 186 -> 790 us
//! a word, finding 9% more, mostly words no one meant.

#![cfg_attr(not(feature = "text"), allow(dead_code))]

use std::cmp::Ordering;

/// Words a chunk holds at most before it splits in two: a write shifts at
/// most one chunk's bytes, as an ordered index's chunks do.
const CHUNK: usize = 256;

/// Edits a word of `n` characters may take: none under 5, one up to 8, two
/// from 9 -- Meilisearch's defaults. A short word has too many neighbours
/// one edit away (`cart` is a letter from `card`, `care`, `cars`, `cat`,
/// `art`), and two edits away a word of five is half rewritten.
pub fn typos(n: usize) -> u8 {
    match n {
        0..=4 => 0,
        5..=8 => 1,
        _ => 2,
    }
}

/// Words in byte order, end to end: a chunk of the dictionary.
#[derive(Default)]
struct Chunk {
    bytes: Vec<u8>,
    /// Where each word ends in `bytes`; it starts where the one before ends.
    ends: Vec<u32>,
}

impl Chunk {
    fn len(&self) -> usize {
        self.ends.len()
    }

    fn start(&self, i: usize) -> usize {
        match i {
            0 => 0,
            _ => self.ends[i - 1] as usize,
        }
    }

    #[inline]
    fn at(&self, i: usize) -> &[u8] {
        self.bytes
            .get(self.start(i)..self.ends[i] as usize)
            .unwrap_or(&[])
    }

    fn push(&mut self, w: &[u8]) {
        self.bytes.extend_from_slice(w);
        self.ends.push(self.bytes.len() as u32);
    }

    fn insert(&mut self, i: usize, w: &[u8]) {
        let from = self.start(i);
        self.bytes.extend_from_slice(w);
        self.bytes[from..].rotate_right(w.len());
        self.ends.insert(i, from as u32);
        for e in &mut self.ends[i..] {
            *e += w.len() as u32;
        }
    }

    fn remove(&mut self, i: usize) {
        let (from, to) = (self.start(i), self.ends[i] as usize);
        self.bytes.copy_within(to.., from);
        self.bytes.truncate(self.bytes.len() - (to - from));
        self.ends.remove(i);
        for e in &mut self.ends[i..] {
            *e -= (to - from) as u32;
        }
    }

    /// The words from `at` on, taken out into a chunk of their own.
    fn split(&mut self, at: usize) -> Chunk {
        let from = self.start(at);
        let mut tail = Chunk {
            bytes: self.bytes.split_off(from),
            ends: self.ends.split_off(at),
        };
        for e in &mut tail.ends {
            *e -= from as u32;
        }
        tail
    }

    /// Where `w` is in this chunk, or would go.
    fn find(&self, w: &[u8]) -> Result<usize, usize> {
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.at(mid).cmp(w) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return Ok(mid),
            }
        }
        Err(lo)
    }
}

/// A place in a [`Dictionary`]: a chunk, and a word of it. The end of a
/// chunk is written as the start of the next.
type Pos = (usize, usize);

/// The words of a text index in byte order -- a term's characters' order,
/// UTF-8 being made so -- in chunks of at most [`CHUNK`]. A word is its
/// bytes and a 4-byte end: 1.2 MB for 99 558 words, where a `Box<str>` each
/// was 2.4 MB. Not the words an index of prefixes (`prefix=N`) marks `^`:
/// they are not words a query types.
#[derive(Default)]
pub struct Dictionary {
    chunks: Vec<Chunk>,
}

impl Dictionary {
    /// The dictionary of `words`, in any order, each once.
    pub fn of<'a>(words: impl Iterator<Item = &'a str>) -> Dictionary {
        let mut all: Vec<&[u8]> = words
            .filter(|w| !w.starts_with('^'))
            .map(str::as_bytes)
            .collect();
        all.sort_unstable();
        let mut d = Dictionary::default();
        // Half full, as an ordered index's chunks start: the first write to
        // each splits none.
        for part in all.chunks(CHUNK / 2) {
            let mut c = Chunk::default();
            for w in part {
                c.push(w);
            }
            d.chunks.push(c);
        }
        d
    }

    /// How many words it holds.
    pub fn len(&self) -> usize {
        self.chunks.iter().map(Chunk::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// What it holds in memory.
    pub fn bytes(&self) -> usize {
        self.chunks.capacity() * std::mem::size_of::<Chunk>()
            + (self.chunks.iter())
                .map(|c| c.bytes.capacity() + 4 * c.ends.capacity())
                .sum::<usize>()
    }

    /// The chunk `w` is in or would go in: the last whose first word is
    /// not above it.
    fn chunk_for(&self, w: &[u8]) -> usize {
        self.chunks
            .partition_point(|c| c.at(0) <= w)
            .saturating_sub(1)
    }

    /// A word the index has just made its first posting for.
    pub fn add(&mut self, w: &str) {
        if w.starts_with('^') {
            return;
        }
        let w = w.as_bytes();
        if self.chunks.is_empty() {
            self.chunks.push(Chunk::default());
        }
        let c = self.chunk_for(w);
        let Err(i) = self.chunks[c].find(w) else {
            return;
        };
        // A full chunk splits before the insert, so it never holds more.
        let (c, i) = match self.chunks[c].len() < CHUNK {
            true => (c, i),
            false => {
                let tail = self.chunks[c].split(CHUNK / 2);
                self.chunks.insert(c + 1, tail);
                match i <= CHUNK / 2 {
                    true => (c, i),
                    false => (c + 1, i - CHUNK / 2),
                }
            }
        };
        self.chunks[c].insert(i, w);
    }

    /// A word whose last posting the index has just let go of.
    pub fn remove(&mut self, w: &str) {
        let w = w.as_bytes();
        if self.chunks.is_empty() {
            return;
        }
        let c = self.chunk_for(w);
        if let Ok(i) = self.chunks[c].find(w) {
            self.chunks[c].remove(i);
            if self.chunks[c].len() == 0 {
                self.chunks.remove(c);
            }
        }
    }

    fn end(&self) -> Pos {
        (self.chunks.len(), 0)
    }

    /// The word at `p`, which is not the end.
    #[inline]
    fn at(&self, p: Pos) -> &[u8] {
        self.chunks[p.0].at(p.1)
    }

    /// The word at `p` as the text it was.
    fn word(&self, p: Pos) -> &str {
        std::str::from_utf8(self.at(p)).unwrap_or("")
    }

    fn next(&self, p: Pos) -> Pos {
        match p.1 + 1 < self.chunks[p.0].len() {
            true => (p.0, p.1 + 1),
            false => (p.0 + 1, 0),
        }
    }

    /// The first place in `lo..hi` whose word, from byte `off` on, is not
    /// below `key` -- or, `through`, neither below `key` nor starting with
    /// it. The words of the range share their first `off` bytes, so each
    /// question is true of a run from `lo` and false after.
    fn part(&self, lo: Pos, hi: Pos, off: usize, key: &[u8], through: bool) -> Pos {
        if lo == hi {
            return lo;
        }
        // A byte at a time: the keys are a character or the few left of a
        // word, where a call to `memcmp` a probe cost more than the bytes.
        let holds = |w: &[u8]| {
            let rest = w.get(off..).unwrap_or(&[]);
            for (i, &k) in key.iter().enumerate() {
                match rest.get(i) {
                    None => return true,
                    Some(&b) if b != k => return b < k,
                    _ => {}
                }
            }
            through
        };
        // The chunks the range reaches past its first: the last of them
        // whose first word still holds is where the run ends.
        let last = if hi.1 == 0 { hi.0 } else { hi.0 + 1 };
        let (mut a, mut b) = (lo.0 + 1, last.max(lo.0 + 1));
        while a < b {
            let mid = a + (b - a) / 2;
            match holds(self.chunks[mid].at(0)) {
                true => a = mid + 1,
                false => b = mid,
            }
        }
        let c = a - 1;
        let chunk = &self.chunks[c];
        let mut i = if c == lo.0 { lo.1 } else { 0 };
        let mut j = if c == hi.0 { hi.1 } else { chunk.len() };
        // A few words are read in turn: the run that holds is mostly short
        // there, and each probe of a search is a branch mispredicted.
        while j - i > 8 {
            let mid = i + (j - i) / 2;
            match holds(chunk.at(mid)) {
                true => i = mid + 1,
                false => j = mid,
            }
        }
        while i < j && holds(chunk.at(i)) {
            i += 1;
        }
        match i < chunk.len() {
            true => (c, i),
            false => (c + 1, 0),
        }
    }

    /// The words `w` begins, itself among them.
    pub fn prefixed<'a>(&'a self, w: &str, cap: usize, out: &mut Vec<(&'a str, u8)>) -> bool {
        if self.is_empty() {
            return true;
        }
        let key = w.as_bytes();
        let lo = self.part((0, 0), self.end(), 0, key, false);
        let hi = self.part(lo, self.end(), 0, key, true);
        let mut p = lo;
        while p != hi {
            if out.len() >= cap {
                return false;
            }
            out.push((self.word(p), 0));
            p = self.next(p);
        }
        true
    }

    /// The words within `k` edits of `w` -- with `prefix`, the words one
    /// of whose prefixes is -- each with its distance, `w` itself among
    /// them; `false` once they pass `cap`.
    pub fn near<'a>(
        &'a self,
        w: &str,
        k: u8,
        prefix: bool,
        cap: usize,
        out: &mut Vec<(&'a str, u8)>,
    ) -> bool {
        if k == 0 && prefix {
            return self.prefixed(w, cap, out);
        }
        let q: Vec<char> = w.chars().collect();
        // Where each of the query's characters starts in `w`, and its end.
        let mut at: Vec<usize> = w.char_indices().map(|(i, _)| i).collect();
        at.push(w.len());
        // The row of the empty prefix: each of the query's characters
        // deleted, the first at two.
        let rows: Vec<u8> = (0..=q.len()).map(|j| sat(j + (j > 0) as usize)).collect();
        let (least, last) = (0, rows[q.len()]);
        let mut walk = Walk {
            dict: self,
            w: w.as_bytes(),
            at: &at,
            q: &q,
            k,
            prefix,
            rows,
            path: Vec::new(),
            out,
            cap,
            over: false,
        };
        if !self.is_empty() {
            walk.node((0, 0), self.end(), 0, least, last, u8::MAX);
        }
        !walk.over
    }
}

/// `n` as a row's cell: past 255 nothing is within reach anyway.
fn sat(n: usize) -> u8 {
    n.min(255) as u8
}

/// A character no query holds: no term holds what is not a letter or a
/// digit, and U+10FFFF is neither.
const OTHER: char = '\u{10FFFF}';

/// One walk of a dictionary for one word.
struct Walk<'a, 'o> {
    dict: &'a Dictionary,
    /// The query word, its bytes, and where each of its characters starts.
    w: &'o [u8],
    at: &'o [usize],
    q: &'o [char],
    k: u8,
    prefix: bool,
    /// The table's rows down the path walked, `q.len() + 1` cells each.
    /// Only the cells within `k` of the diagonal are worked out: a cell
    /// further holds more than `k`, and is left at `k + 1`, which every
    /// question asked of it answers the same.
    rows: Vec<u8>,
    path: Vec<char>,
    out: &'o mut Vec<(&'a str, u8)>,
    cap: usize,
    over: bool,
}

impl<'a> Walk<'a, '_> {
    fn found(&mut self, p: Pos, d: u8) {
        if self.out.len() >= self.cap {
            self.over = true;
            return;
        }
        self.out.push((self.dict.word(p), d));
    }

    /// Every word of `lo..hi`, at distance `d`.
    fn all(&mut self, lo: Pos, hi: Pos, d: u8) {
        let mut p = lo;
        while p != hi && !self.over {
            self.found(p, d);
            p = self.dict.next(p);
        }
    }

    /// The row of `c` after the path pushed, with its least cell and its
    /// last: optimal string alignment, an edit touching either word's
    /// first character at two.
    fn push(&mut self, c: char) -> (u8, u8) {
        let w = self.q.len() + 1;
        let d = self.path.len() + 1;
        let k = self.k as usize;
        let base = self.rows.len();
        self.rows.resize(base + w, sat(k + 1));
        self.rows[base] = sat(d + 1);
        let mut least = self.rows[base];
        for j in d.saturating_sub(k).max(1)..(d + k + 1).min(w) {
            let first = (d == 1 || j == 1) as u8;
            let up = self.rows[base - w + j];
            let diag = self.rows[base - w + j - 1];
            let left = self.rows[base + j - 1];
            let swap = match self.q.get(j - 1).zip(self.q.get(j.wrapping_sub(2))) {
                Some((&qj, &qi)) if d > 1 && c == qi && self.path.get(d - 2) == Some(&qj) => {
                    self.rows[base - 2 * w + j - 2].saturating_add(1 + (d == 2 || j == 2) as u8)
                }
                _ => u8::MAX,
            };
            let same = self.q.get(j - 1) == Some(&c);
            let v = up
                .saturating_add(1 + (d == 1) as u8)
                .min(left.saturating_add(1 + (j == 1) as u8))
                .min(diag.saturating_add(if same { 0 } else { 1 + first }))
                .min(swap);
            self.rows[base + j] = v;
            least = least.min(v);
        }
        (least, self.rows[base + w - 1])
    }

    fn pop(&mut self) {
        self.rows.truncate(self.rows.len() - (self.q.len() + 1));
    }

    /// The words of `lo..hi`, which share the path's `off` bytes and whose
    /// row is the last pushed, its least cell `least` and its last `last`;
    /// `best` is the least last cell of the rows above it: the distance of
    /// the nearest prefix of the path from the query, where that counts.
    #[allow(clippy::too_many_arguments)]
    fn node(&mut self, lo: Pos, hi: Pos, off: usize, least: u8, last: u8, best: u8) {
        let k = self.k;
        let best = if self.prefix { best.min(last) } else { u8::MAX };
        // No row below holds a cell under this one's least.
        if best <= k && least >= k {
            // A prefix of the path is near enough, and nothing below it is
            // nearer: every word of the range, at that distance.
            return self.all(lo, hi, best);
        }
        let w = self.q.len() + 1;
        let base = self.rows.len() - w;
        // Spent: no cell under `k`, and no swap of the path's last
        // character with the next to come -- a swap is counted from the row
        // above this one, which may still pay for it.
        let d = self.path.len() + 1;
        let spent = least == k
            && !(base >= w
                && (2..w).any(|j| {
                    self.path.last() == self.q.get(j - 1)
                        && (self.rows[base - w + j - 2])
                            .saturating_add(1 + (d == 2 || j == 2) as u8)
                            <= k
                }));
        if spent {
            // No edit is left: a word below is the path, then the rest of
            // the query from a cell at `k`, exactly -- looked up, rather
            // than walked a character at a time. Shortest rest first: a
            // range found with `prefix` holds those of the longer rests
            // that begin with it.
            let mut seen: [(Pos, Pos); 5] = [((0, 0), (0, 0)); 5];
            let mut n = 0;
            for j in (0..w).rev() {
                if self.rows[base + j] != k || self.over {
                    continue;
                }
                let rest = self.w.get(self.at[j]..).unwrap_or(&[]);
                let a = self.dict.part(lo, hi, off, rest, false);
                let b = match self.prefix {
                    true => self.dict.part(a, hi, off, rest, true),
                    false
                        if a != hi
                            && (self.dict.at(a).get(off..)).is_some_and(|r| {
                                r.len() == rest.len() && r.iter().zip(rest).all(|(x, y)| x == y)
                            }) =>
                    {
                        self.dict.next(a)
                    }
                    false => a,
                };
                if a == b || seen[..n].iter().any(|s| s.0 <= a && b <= s.1) {
                    continue;
                }
                self.all(a, b, k);
                // A row within `k` of the diagonal has 2k + 1 cells at most.
                if n < seen.len() {
                    seen[n] = (a, b);
                    n += 1;
                }
            }
            return;
        }
        let mut a = lo;
        if self.dict.at(a).len() == off {
            let d = last.min(best);
            if d <= k {
                self.found(a, d);
            }
            a = self.dict.next(a);
        }
        if a == hi || self.over {
            return;
        }
        let (o_least, o_last) = self.push(OTHER);
        self.pop();
        if o_least <= k || best.min(o_last) <= k {
            // Any character keeps the row within reach: every child.
            let mut g0 = a;
            while g0 != hi && !self.over {
                let rest = self.dict.at(g0).get(off..).unwrap_or(&[]);
                let c = std::str::from_utf8(rest)
                    .ok()
                    .and_then(|s| s.chars().next())
                    .unwrap_or(OTHER);
                let mut enc = [0u8; 4];
                let key = c.encode_utf8(&mut enc).as_bytes();
                let g1 = self.dict.part(g0, hi, off, key, true);
                self.child(c, g0, g1, off, best);
                g0 = g1;
            }
            return;
        }
        // Only a character the query holds near this depth can: each found
        // by binary search, in order, each from where the last ended.
        let depth = self.path.len();
        let from = depth.saturating_sub(k as usize + 1);
        let to = (depth + k as usize + 1).min(self.q.len());
        // At most 2k + 2 of them, in order, each once: sorted in place.
        let mut cands = [OTHER; 6];
        let mut n = 0;
        for &c in self.q.get(from..to).unwrap_or(&[]) {
            let at = cands[..n].partition_point(|&x| x < c);
            if cands.get(at) != Some(&c) && n < cands.len() {
                cands.copy_within(at..n, at + 1);
                cands[at] = c;
                n += 1;
            }
        }
        let mut start = a;
        for &c in &cands[..n] {
            if self.over {
                return;
            }
            let mut enc = [0u8; 4];
            let key = c.encode_utf8(&mut enc).as_bytes();
            let g0 = self.dict.part(start, hi, off, key, false);
            let g1 = self.dict.part(g0, hi, off, key, true);
            start = g1;
            if g0 != g1 {
                self.child(c, g0, g1, off, best);
            }
        }
    }

    fn child(&mut self, c: char, lo: Pos, hi: Pos, off: usize, best: u8) {
        let (least, last) = self.push(c);
        if least <= self.k || best.min(last) <= self.k {
            self.path.push(c);
            self.node(lo, hi, off + c.len_utf8(), least, last, best);
            self.path.pop();
        }
        self.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The distance as defined, by the whole table -- optimal string
    /// alignment, an edit touching either word's first character at two --
    /// of the query `b` from the word `a`, and from the nearest of `a`'s
    /// prefixes, the empty one among them.
    fn distance(a: &[char], b: &[char]) -> (usize, usize) {
        let (n, m) = (a.len(), b.len());
        let w = m + 1;
        let mut d = vec![0usize; (n + 1) * w];
        for j in 1..=m {
            d[j] = j + 1;
        }
        let mut prefix = d[m];
        for i in 1..=n {
            d[i * w] = i + 1;
            for j in 1..=m {
                let first = (i == 1 || j == 1) as usize;
                let sub = if a[i - 1] == b[j - 1] { 0 } else { 1 + first };
                let mut v = (d[(i - 1) * w + j] + 1 + (i == 1) as usize)
                    .min(d[i * w + j - 1] + 1 + (j == 1) as usize)
                    .min(d[(i - 1) * w + j - 1] + sub);
                if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                    v = v.min(d[(i - 2) * w + j - 2] + 1 + (i == 2 || j == 2) as usize);
                }
                d[i * w + j] = v;
            }
            prefix = prefix.min(d[i * w + m]);
        }
        (d[n * w + m], prefix)
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn upto(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Words over a few letters, so that many are near each other, some
    /// outside ASCII; a word of a query a word of the dictionary changed.
    fn word(rng: &mut Rng, letters: &[char]) -> String {
        let n = 1 + rng.upto(11);
        (0..n).map(|_| letters[rng.upto(letters.len())]).collect()
    }

    fn typo(rng: &mut Rng, w: &str, letters: &[char]) -> String {
        let mut c: Vec<char> = w.chars().collect();
        for _ in 0..rng.upto(3) {
            let at = rng.upto(c.len() + 1);
            match rng.upto(4) {
                0 if at < c.len() => {
                    c.remove(at);
                }
                1 => c.insert(at, letters[rng.upto(letters.len())]),
                2 if at < c.len() => c[at] = letters[rng.upto(letters.len())],
                _ if at + 1 < c.len() => c.swap(at, at + 1),
                _ => {}
            }
        }
        c.into_iter().collect()
    }

    /// Every word the walk finds is within `k` at the distance it says,
    /// and every word within `k` is found -- held to the whole table over
    /// every word of the dictionary, with prefixes and without, through
    /// words added and taken away.
    #[test]
    fn the_walk_finds_what_the_table_finds() {
        let letters: Vec<char> = "abcdeé字".chars().collect();
        let mut found = [0usize; 3];
        for seed in 1..=40u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut words: Vec<String> = (0..300 + rng.upto(700))
                .map(|_| word(&mut rng, &letters))
                .collect();
            words.sort();
            words.dedup();
            let mut dict = Dictionary::of(words.iter().map(String::as_str));
            // Writes after the build: some words in, some out.
            for _ in 0..rng.upto(400) {
                let w = word(&mut rng, &letters);
                match words.binary_search(&w) {
                    Ok(at) if rng.upto(2) == 0 => {
                        words.remove(at);
                        dict.remove(&w);
                    }
                    Err(at) => {
                        words.insert(at, w.clone());
                        dict.add(&w);
                    }
                    _ => {}
                }
            }
            assert_eq!(dict.len(), words.len(), "seed {seed}");
            for _ in 0..40 {
                let base = &words[rng.upto(words.len())];
                let q = typo(&mut rng, base, &letters);
                if q.is_empty() {
                    continue;
                }
                let qc: Vec<char> = q.chars().collect();
                let all: Vec<(usize, usize)> = (words.iter())
                    .map(|t| distance(&t.chars().collect::<Vec<char>>(), &qc))
                    .collect();
                for k in 0..=2u8 {
                    for prefix in [false, true] {
                        let mut want: Vec<(String, u8)> = Vec::new();
                        for (t, &(whole, part)) in words.iter().zip(&all) {
                            let d = if prefix { part } else { whole };
                            if d <= k as usize {
                                want.push((t.clone(), d as u8));
                            }
                        }
                        let mut got = Vec::new();
                        assert!(dict.near(&q, k, prefix, usize::MAX, &mut got));
                        let mut got: Vec<(String, u8)> =
                            got.into_iter().map(|(w, d)| (w.to_string(), d)).collect();
                        got.sort();
                        assert_eq!(got, want, "seed {seed} {q:?} k {k} prefix {prefix}");
                        found[k as usize] += want.len();
                        // Past the cap, refused rather than cut.
                        if !want.is_empty() {
                            let mut cut = Vec::new();
                            assert!(!dict.near(&q, k, prefix, want.len() - 1, &mut cut));
                            let mut all = Vec::new();
                            assert!(dict.near(&q, k, prefix, want.len(), &mut all));
                        }
                    }
                }
            }
        }
        // Not a test of nothing: each bound found thousands.
        assert!(found.iter().all(|&n| n > 2_000), "{found:?}");
    }

    /// A prefix is a range of the dictionary: every word holding it, and
    /// nothing else, across chunks.
    #[test]
    fn a_prefix_is_a_range() {
        let words: Vec<String> = (0..5_000).map(|i| format!("w{}", i * 7 % 5_003)).collect();
        let dict = Dictionary::of(words.iter().map(String::as_str));
        for p in ["w1", "w12", "w4999", "w5", "x", "w", "v"] {
            let mut got = Vec::new();
            assert!(dict.prefixed(p, usize::MAX, &mut got));
            let mut got: Vec<&str> = got.into_iter().map(|(w, _)| w).collect();
            got.sort_unstable();
            let mut want: Vec<&str> = words
                .iter()
                .map(String::as_str)
                .filter(|w| w.starts_with(p))
                .collect();
            want.sort_unstable();
            assert_eq!(got, want, "{p}");
        }
    }

    #[test]
    fn words_of_an_index_of_prefixes_are_left_out() {
        let mut dict = Dictionary::of(["kitap", "^kit", "^kita"].into_iter());
        dict.add("^kitap");
        dict.add("kitaplar");
        let mut got = Vec::new();
        assert!(dict.prefixed("", usize::MAX, &mut got));
        assert_eq!(got, [("kitap", 0), ("kitaplar", 0)]);
    }

    #[test]
    fn the_bound_grows_with_the_word() {
        let got: Vec<u8> = (0..12).map(typos).collect();
        assert_eq!(got, [0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2]);
    }
}
