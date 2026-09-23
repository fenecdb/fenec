//! Sparse vectors, and the inverted index `near` searches them through.
//!
//! A `sparse<N>` value keeps only its non-zero entries, `(index, weight)`
//! ascending by index. That is the shape a learned sparse model gives a
//! text: SPLADE weighs every entry of a 30 522-word vocabulary and all but a
//! few dozen come out zero -- 65 a document on SciFact, 56 a query. Held as a
//! `vector<30522>` the same text would be 119 KB, nearly all of it zeros.
//!
//! The text form is pgvector's `sparsevec`, indices counted from 1 --
//! `{1:0.5,3:0.25}/30522` -- so a value a pgvector client writes, or one
//! `fenec import` copies out of PostgreSQL, reads as the same vector. Held
//! in memory and on disk the indices count from 0, as pgvector's do.
//!
//! The index is the text index's shape with weights where the term counts
//! were: dimension -> the documents with a weight there, ascending by id.
//! `near` scores by dot product and walks the lists with MaxScore, the
//! pruning `match` uses, so what it returns is the exact top k, not an
//! estimate of it; `pruning_never_changes_the_answer` holds it to an
//! exhaustive walk. A document sharing no dimension with the query is on
//! none of the lists and is not ranked -- as `match` ranks only documents
//! holding a word of its query.

use crate::error::{Error, Result};
use crate::text::ByScore;
use crate::value::DocId;
use std::collections::{BinaryHeap, HashMap};

/// The largest dimension a `sparse<N>` may declare, pgvector's.
pub const MAX_DIM: usize = 1_000_000_000;

/// Reads pgvector's text form, `{1:0.5,3:0.25}/30522`: the dimension, and
/// the entries with their indices from 0, ascending. A zero weight is
/// dropped -- it would put a document on a list it adds nothing to -- and
/// an index given twice is refused rather than one of the two picked.
///
/// One pass over the bytes, space allowed around every part: written with
/// `split` and `trim` it was 1.5 KB of the browser module.
pub fn parse(s: &str) -> Result<(u32, Vec<(u32, f32)>)> {
    let b = s.as_bytes();
    let mut at = 0;
    let space = |at: &mut usize| {
        while *at < b.len() && b[*at].is_ascii_whitespace() {
            *at += 1;
        }
    };
    // A whole number, or a decimal one when `num` is set, and where it ends.
    let token = |at: &mut usize, num: bool| {
        // Past the end once a separator was missing: an empty token, which
        // no number reads.
        *at = (*at).min(b.len());
        space(at);
        let from = *at;
        while *at < b.len()
            && (b[*at].is_ascii_digit()
                || num && matches!(b[*at], b'.' | b'-' | b'+' | b'e' | b'E'))
        {
            *at += 1;
        }
        let t = &s[from..*at];
        space(at);
        t
    };
    let mut entries = Vec::new();
    let mut ok = {
        space(&mut at);
        b.get(at) == Some(&b'{')
    };
    at += 1;
    space(&mut at);
    if ok && b.get(at) != Some(&b'}') {
        loop {
            let i = uint(token(&mut at, false));
            ok &= b.get(at) == Some(&b':');
            at += 1;
            let v = crate::num::parse_f64(token(&mut at, true));
            match (i, v) {
                (Some(i), Some(v)) if ok && i >= 1 && i <= u32::MAX as u64 => {
                    entries.push((i as u32 - 1, v as f32));
                }
                _ => ok = false,
            }
            if !ok || b.get(at) != Some(&b',') {
                break;
            }
            at += 1;
        }
    }
    ok &= b.get(at) == Some(&b'}');
    at += 1;
    space(&mut at);
    ok &= b.get(at) == Some(&b'/');
    at += 1;
    let dim = uint(token(&mut at, false)).filter(|d| ok && at == b.len() && *d >= 1);
    let bad = |why: &str| Error::Type(format!("sparse vector `{s}`: {why}"));
    let Some(dim) = dim else {
        return Err(bad("expected {index:value,...}/dimension, indices from 1"));
    };
    if dim > MAX_DIM as u64 {
        return Err(bad("the dimension is past 1000000000"));
    }
    normalise(dim as u32, entries).map_err(|e| bad(&e))
}

/// Puts entries in index order, refuses one out of range or given twice,
/// and drops the zeros: every way a sparse vector enters goes through here,
/// so the index only ever sees what it can rely on.
pub fn normalise(
    dim: u32,
    mut entries: Vec<(u32, f32)>,
) -> std::result::Result<(u32, Vec<(u32, f32)>), String> {
    // Most arrive in order -- a model writes them so, and so does
    // `format_into` -- and the check costs a pass where a sort would move
    // nothing.
    if entries.windows(2).any(|w| w[0].0 >= w[1].0) {
        entries = by_index(&entries);
        if entries.windows(2).any(|w| w[0].0 == w[1].0) {
            return Err("an index is given twice".into());
        }
    }
    if entries.last().is_some_and(|e| e.0 >= dim) {
        return Err("an index is outside the dimension".into());
    }
    if entries.iter().any(|e| !e.1.is_finite()) {
        return Err("a value is not finite".into());
    }
    entries.retain(|e| e.1 != 0.0);
    Ok((dim, entries))
}

/// `entries` in index order, through the sort every ranking in the engine
/// already goes through -- equal scores go by id, and the index is the id
/// here -- rather than a sort of their own: one for the pairs and one for
/// `search`'s cursors were 8.9 KB of the browser module.
fn by_index(entries: &[(u32, f32)]) -> Vec<(u32, f32)> {
    // The id is the index above the entry's own position, so each finds its
    // weight again in one step.
    let mut keyed: Vec<(DocId, f32)> = Vec::with_capacity(entries.len());
    for (at, e) in entries.iter().enumerate() {
        keyed.push(((e.0 as DocId) << 32 | at as DocId, 0.0));
    }
    keyed.sort_by(crate::text::best_first);
    let mut out = Vec::with_capacity(entries.len());
    for (key, _) in &keyed {
        out.push(((key >> 32) as u32, entries[(key & 0xffff_ffff) as usize].1));
    }
    out
}

/// A whole number of digits alone, surrounding space allowed.
fn uint(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() || s.len() > 19 {
        return None;
    }
    let mut n = 0u64;
    for b in s.bytes() {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n * 10 + (b - b'0') as u64;
    }
    Some(n)
}

/// pgvector's text form of a sparse vector, indices from 1. Weights print
/// as the shortest decimal that reads back as the same `f32`, as pgvector
/// prints them.
pub fn format_into(out: &mut String, dim: u32, entries: &[(u32, f32)]) {
    out.push('{');
    for (k, (i, v)) in entries.iter().enumerate() {
        if k > 0 {
            out.push(',');
        }
        out.push_str(&(i + 1).to_string());
        out.push(':');
        out.push_str(&v.to_string());
    }
    out.push_str("}/");
    out.push_str(&dim.to_string());
}

/// The dot product of two sparse vectors over the dimensions they share,
/// `None` when they share none. Summed in `f64`, as the index sums.
pub fn dot(a: &[(u32, f32)], b: &[(u32, f32)]) -> Option<f64> {
    let (mut i, mut j) = (0, 0);
    let mut sum = 0.0f64;
    let mut shared = false;
    while i < a.len() && j < b.len() {
        match a[i].0.cmp(&b[j].0) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                sum += a[i].1 as f64 * b[j].1 as f64;
                shared = true;
                i += 1;
                j += 1;
            }
        }
    }
    shared.then_some(sum)
}

/// One dimension's documents, ascending by id, their weights beside them.
///
/// Parallel arrays for the reason the text index's are: the merge reads
/// ids and touches a weight only on a hit.
#[derive(Default)]
struct List {
    docs: Vec<DocId>,
    weights: Vec<f32>,
    /// The largest and smallest weight the list has held, for the MaxScore
    /// ceiling. Only ever widened: a removal can leave the bound loose,
    /// never wrong, and a rebuild makes it exact again. Both start at 0,
    /// which the ceiling is clamped to anyway.
    max: f32,
    min: f32,
}

impl List {
    fn set(&mut self, doc: DocId, w: f32) {
        self.max = self.max.max(w);
        self.min = self.min.min(w);
        match self.docs.binary_search(&doc) {
            Ok(at) => self.weights[at] = w,
            Err(at) => {
                self.docs.insert(at, doc);
                self.weights.insert(at, w);
            }
        }
    }

    fn drop_doc(&mut self, doc: DocId) {
        if let Ok(at) = self.docs.binary_search(&doc) {
            self.docs.remove(at);
            self.weights.remove(at);
        }
    }

    fn bytes(&self) -> usize {
        self.docs.capacity() * std::mem::size_of::<DocId>() + self.weights.capacity() * 4 + 8
    }
}

/// The inverted index over one `sparse<N>` field.
///
/// Derived data, like the text index, and for the same reason not written
/// to the file: it is rebuilt from the documents on open, in the pass that
/// already reads every one of them for the other indexes.
#[derive(Default)]
pub struct SparseIndex {
    /// Dimension -> where its list is in `lists`: the map the vector index
    /// keeps from a document to its node, reused. A map of its own for this
    /// key was 3.1 KB of the browser module.
    at: HashMap<DocId, u32>,
    /// A dimension's list stays when its last document goes, its memory
    /// given back; the dimension is likely to come again.
    lists: Vec<List>,
    docs: usize,
    /// What the lists hold, kept as they change rather than summed over
    /// every dimension when `--max-memory` asks, before every write.
    heap: usize,
}

impl SparseIndex {
    pub fn new() -> SparseIndex {
        SparseIndex::default()
    }

    /// Documents indexed: those with at least one non-zero entry.
    pub fn len(&self) -> usize {
        self.docs
    }

    pub fn is_empty(&self) -> bool {
        self.docs == 0
    }

    /// Dimensions some document has a weight in.
    pub fn dimensions(&self) -> usize {
        self.lists.iter().filter(|l| !l.docs.is_empty()).count()
    }

    pub fn postings_count(&self) -> usize {
        self.lists.iter().map(|l| l.docs.len()).sum()
    }

    /// The lists and the map to them as they sit in memory. Not RSS -- the
    /// same caveat as `Database::memory_bytes`.
    pub fn memory_bytes(&self) -> usize {
        self.heap
            + self.lists.capacity() * std::mem::size_of::<List>()
            + self.at.capacity() * (std::mem::size_of::<DocId>() + 4 + 1)
    }

    fn list(&self, i: u32) -> Option<&List> {
        self.at.get(&(i as DocId)).map(|k| &self.lists[*k as usize])
    }

    pub fn insert(&mut self, doc: DocId, entries: &[(u32, f32)]) {
        if entries.is_empty() {
            return;
        }
        for &(i, w) in entries {
            let k = match self.at.get(&(i as DocId)) {
                Some(k) => *k as usize,
                None => {
                    self.at.insert(i as DocId, self.lists.len() as u32);
                    self.lists.push(List::default());
                    self.heap += List::default().bytes();
                    self.lists.len() - 1
                }
            };
            let list = &mut self.lists[k];
            let before = list.bytes();
            list.set(doc, w);
            self.heap += list.bytes() - before;
        }
        self.docs += 1;
    }

    /// Takes a document out. `entries` must be what was indexed -- every
    /// caller reads the stored document first, as the text index's do.
    pub fn remove(&mut self, doc: DocId, entries: &[(u32, f32)]) {
        if entries.is_empty() {
            return;
        }
        for (i, _) in entries {
            if let Some(k) = self.at.get(&(*i as DocId)) {
                let list = &mut self.lists[*k as usize];
                list.drop_doc(doc);
                if list.docs.is_empty() {
                    self.heap -= list.bytes() - List::default().bytes();
                    *list = List::default();
                }
            }
        }
        self.docs = self.docs.saturating_sub(1);
    }

    /// Gives back the growth slack of every list, where the index is known
    /// complete: a rebuild on open, and `create index`.
    pub fn shrink_to_fit(&mut self) {
        self.heap = 0;
        for list in self.lists.iter_mut() {
            list.docs.shrink_to_fit();
            list.weights.shrink_to_fit();
            self.heap += list.bytes();
        }
        self.lists.shrink_to_fit();
    }

    pub fn clear(&mut self) {
        *self = SparseIndex::default();
    }

    /// The `k` documents with the largest dot product with `query`, best
    /// first, ties to the lower id. `accept` is the filter, tested as the
    /// lists are merged, as `match` tests it.
    ///
    /// MaxScore as the text index walks it: each query dimension's ceiling
    /// is the most it can add to any document, and once the kept `k` are
    /// all better than what the dimensions with the smallest ceilings could
    /// add up to together, those stop proposing documents and are only read
    /// for the documents the rest propose. A dimension whose weights have a
    /// sign against the query's adds at most nothing, so its ceiling is 0.
    ///
    /// The bounds are compared once rounded to `f32`, the precision scores
    /// are kept in: a document skipped for falling short of the worst kept
    /// score cannot tie it, so the tie-break on the id never depends on
    /// what was pruned. They carry a relative slack of 1e-9 against the
    /// rounding of summing them in `f64`, far past what a sum of fewer than
    /// a million terms can drift.
    pub fn search(
        &self,
        query: &[(u32, f32)],
        k: usize,
        accept: &dyn Fn(DocId) -> bool,
    ) -> Vec<(DocId, f32)> {
        if k == 0 {
            return Vec::new();
        }
        let mut found: Vec<Cursor> = Vec::with_capacity(query.len());
        for &(i, q) in query {
            if let Some(list) = self.list(i) {
                let (q64, max, min) = (q as f64, list.max as f64, list.min as f64);
                let ceiling = (q64 * max).max(q64 * min).max(0.0) * (1.0 + 1e-9);
                found.push(Cursor {
                    at: 0,
                    list,
                    weight: q64,
                    ceiling,
                });
            }
        }
        if found.is_empty() {
            return Vec::new();
        }
        // Smallest ceiling first, through the engine's one sort of scored
        // ids: the negated ceiling is the score, the cursor's place the id.
        // The order is the pruning's efficiency, never its answer -- `reach`
        // sums the ceilings in whatever order they stand -- so their
        // rounding to `f32` here costs nothing.
        let mut keyed: Vec<(DocId, f32)> = Vec::with_capacity(found.len());
        for (at, c) in found.iter().enumerate() {
            keyed.push((at as DocId, -(c.ceiling as f32)));
        }
        keyed.sort_by(crate::text::best_first);
        let mut cursors: Vec<Cursor> = Vec::with_capacity(found.len());
        for (at, _) in &keyed {
            cursors.push(found[*at as usize]);
        }
        let mut reach: Vec<f64> = Vec::with_capacity(cursors.len() + 1);
        reach.push(0.0);
        for c in &cursors {
            reach.push(reach[reach.len() - 1] + c.ceiling);
        }

        let mut heap: BinaryHeap<ByScore> = BinaryHeap::with_capacity(k + 1);
        let mut worst_kept = f32::NEG_INFINITY;
        let mut pivot = 0usize;
        loop {
            while pivot < cursors.len() && (reach[pivot + 1] as f32) < worst_kept {
                pivot += 1;
            }
            if pivot == cursors.len() {
                break;
            }
            let mut doc = DocId::MAX;
            for c in &cursors[pivot..] {
                if let Some(d) = c.list.docs.get(c.at) {
                    if *d < doc {
                        doc = *d;
                    }
                }
            }
            if doc == DocId::MAX {
                break;
            }
            let mut score = 0.0f64;
            for c in cursors[pivot..].iter_mut() {
                if c.list.docs.get(c.at) == Some(&doc) {
                    score += c.weight * c.list.weights[c.at] as f64;
                    c.at += 1;
                }
            }
            let mut gave_up = false;
            for j in (0..pivot).rev() {
                if ((score + reach[j + 1]) as f32) < worst_kept {
                    gave_up = true;
                    break;
                }
                let c = &mut cursors[j];
                c.at += c.list.docs[c.at..].partition_point(|d| *d < doc);
                if c.list.docs.get(c.at) == Some(&doc) {
                    score += c.weight * c.list.weights[c.at] as f64;
                }
            }
            if !gave_up && accept(doc) {
                heap.push(ByScore(score as f32, doc));
                if heap.len() > k {
                    heap.pop();
                }
                if heap.len() == k {
                    worst_kept = heap.peek().map(|s| s.0).unwrap_or(f32::NEG_INFINITY);
                }
            }
        }
        let mut out: Vec<(DocId, f32)> = heap.into_iter().map(|s| (s.1, s.0)).collect();
        out.sort_by(crate::text::best_first);
        out
    }
}

/// One query dimension's walk through its list.
#[derive(Clone, Copy)]
struct Cursor<'a> {
    at: usize,
    list: &'a List,
    /// The query's weight in this dimension.
    weight: f64,
    /// The most this dimension can add to any one document.
    ceiling: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<(u32, f32)> {
        parse(s).unwrap().1
    }

    #[test]
    fn reads_and_writes_pgvectors_text_form() {
        let (dim, e) = parse("{1:0.5,3:0.25}/5").unwrap();
        assert_eq!((dim, e.clone()), (5, vec![(0, 0.5), (2, 0.25)]));
        let mut s = String::new();
        format_into(&mut s, dim, &e);
        assert_eq!(s, "{1:0.5,3:0.25}/5");
        // Space where pgvector allows it, an empty vector, and order and
        // zeros settled on the way in.
        assert_eq!(parse(" { 3 : 0.25 , 1 : 0.5, 2: 0 } / 5 ").unwrap().1, e);
        assert_eq!(parse("{}/7").unwrap(), (7, vec![]));
        assert_eq!(parse(" { } / 7 ").unwrap(), (7, vec![]));
        assert_eq!(
            parse("{1:1e-3,2:+2}/2").unwrap().1,
            vec![(0, 0.001), (1, 2.0)]
        );
        let mut s = String::new();
        format_into(&mut s, 3, &[(0, 0.1), (2, -2.5e-7)]);
        assert_eq!(s, "{1:0.1,3:-0.00000025}/3");
        assert_eq!(parse(&s).unwrap().1, vec![(0, 0.1), (2, -2.5e-7)]);
    }

    #[test]
    fn refuses_what_is_not_a_sparse_vector() {
        for bad in [
            "{1:0.5}",
            "[1,2,3]",
            "{1:0.5}/0",
            "{1:0.5}/1000000001",
            "{0:0.5}/3",
            "{4:0.5}/3",
            "{1:0.5,1:0.25}/3",
            "{1:x}/3",
            "{1:NaN}/3",
            "{1:1e39}/3",
            "{-1:0.5}/3",
            "{1.5:0.5}/3",
            "{1:0.5,}/3",
            "{1:0.5}/3x",
            "",
            "{",
            "{}",
            "{}/",
            "{1:}/3",
            "{:1}/3",
            "{1:1}/3 x",
            "{1 1}/3",
            "1:1/3",
        ] {
            assert!(parse(bad).is_err(), "{bad} was read");
        }
    }

    #[test]
    fn ranks_by_dot_product_over_shared_dimensions() {
        let mut ix = SparseIndex::new();
        ix.insert(1, &v("{1:1,2:1}/4"));
        ix.insert(2, &v("{2:3}/4"));
        ix.insert(3, &v("{4:9}/4"));
        ix.insert(4, &v("{1:2}/4"));
        let q = v("{1:1,2:1}/4");
        let hits = ix.search(&q, 10, &|_| true);
        // 2 scores 3, 1 and 4 tie at 2 and go by id; 3 shares nothing.
        assert_eq!(hits, vec![(2, 3.0), (1, 2.0), (4, 2.0)]);
        assert_eq!(ix.search(&q, 2, &|_| true), vec![(2, 3.0), (1, 2.0)]);
        assert_eq!(ix.search(&q, 10, &|d| d != 2), vec![(1, 2.0), (4, 2.0)]);
        assert!(ix.search(&v("{3:1}/4"), 10, &|_| true).is_empty());
        assert!(ix.search(&q, 0, &|_| true).is_empty());
    }

    #[test]
    fn a_weight_against_the_query_lowers_the_score() {
        let mut ix = SparseIndex::new();
        ix.insert(1, &v("{1:1,2:-4}/2"));
        ix.insert(2, &v("{1:1}/2"));
        ix.insert(3, &v("{2:-1}/2"));
        let hits = ix.search(&v("{1:1,2:1}/2"), 10, &|_| true);
        assert_eq!(hits, vec![(2, 1.0), (3, -1.0), (1, -3.0)]);
    }

    #[test]
    fn removal_and_update_leave_what_a_fresh_build_would() {
        let mut ix = SparseIndex::new();
        ix.insert(1, &v("{1:1,2:1}/3"));
        ix.insert(2, &v("{2:2,3:1}/3"));
        let before = ix.memory_bytes();
        ix.remove(2, &v("{2:2,3:1}/3"));
        ix.insert(2, &v("{1:5}/3"));
        assert_eq!(ix.len(), 2);
        assert_eq!(ix.dimensions(), 2);
        assert_eq!(
            ix.search(&v("{1:1,2:1,3:1}/3"), 10, &|_| true),
            vec![(2, 5.0), (1, 2.0)]
        );
        ix.remove(1, &v("{1:1,2:1}/3"));
        ix.remove(2, &v("{1:5}/3"));
        assert!(ix.is_empty());
        assert_eq!(ix.dimensions(), 0);
        assert!(ix.memory_bytes() < before);
    }

    /// The exhaustive answer: every document scored against the query, the
    /// best `k` kept, ties to the lower id.
    fn exhaustive(
        docs: &[(DocId, Vec<(u32, f32)>)],
        q: &[(u32, f32)],
        k: usize,
        accept: &dyn Fn(DocId) -> bool,
    ) -> Vec<(DocId, f32)> {
        let mut all: Vec<(DocId, f32)> = docs
            .iter()
            .filter(|(id, _)| accept(*id))
            .filter_map(|(id, e)| dot(e, q).map(|s| (*id, s as f32)))
            .collect();
        all.sort_by(crate::text::best_first);
        all.truncate(k);
        all
    }

    #[test]
    fn pruning_never_changes_the_answer() {
        let mut seed = 0x5eed_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..400 {
            let dims = 8 + (next() % 200) as u32;
            let n = 20 + (next() % 300) as usize;
            let signed = round % 4 == 0;
            let entries = |len: u64, next: &mut dyn FnMut() -> u64| {
                let mut e: Vec<(u32, f32)> = Vec::new();
                for _ in 0..1 + next() % len {
                    let i = (next() % dims as u64) as u32;
                    // Skewed, as a model's weights are, and coarse so ties
                    // happen.
                    let mut w = ((next() % 64) as f32 / 8.0).powi(2) / 8.0 + 0.125;
                    if signed && next().is_multiple_of(3) {
                        w = -w;
                    }
                    e.push((i, w));
                }
                e.sort_by_key(|x| x.0);
                e.dedup_by_key(|x| x.0);
                e
            };
            let mut ix = SparseIndex::new();
            let mut docs = Vec::new();
            for id in 1..=n as DocId {
                let e = entries(24, &mut next);
                ix.insert(id, &e);
                docs.push((id, e));
            }
            // Some go again, the way updates and deletions leave an index.
            for _ in 0..n / 5 {
                let at = (next() % docs.len() as u64) as usize;
                let (id, old) = docs.remove(at);
                ix.remove(id, &old);
                if next() % 2 == 0 {
                    let e = entries(24, &mut next);
                    ix.insert(id, &e);
                    docs.push((id, e));
                }
            }
            docs.sort_by_key(|d| d.0);
            // The count `memory_bytes` reads is the sum it replaced.
            let walked = |ix: &SparseIndex| ix.lists.iter().map(List::bytes).sum::<usize>();
            assert_eq!(ix.heap, walked(&ix), "round {round}");
            if round % 50 == 0 {
                ix.shrink_to_fit();
                assert_eq!(ix.heap, walked(&ix), "round {round}, shrunk");
            }
            for _ in 0..8 {
                let q = entries(12, &mut next);
                let k = [1, 3, 10, 50][(next() % 4) as usize];
                let odd = next() % 2 == 0;
                let accept = |id: DocId| !odd || id % 2 == 1;
                assert_eq!(
                    ix.search(&q, k, &accept),
                    exhaustive(&docs, &q, k, &accept),
                    "round {round}, k {k}, query {q:?}"
                );
            }
        }
    }
}
