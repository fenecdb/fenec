//! Full-text index: a tokenizer, an inverted index and BM25.
//!
//! **Why a sparse structure instead of reusing the vector index.** Measured on
//! BEIR SciFact (5 183 documents, 300 queries): this index scores nDCG@10
//! 0.662 against 0.645 for a 90 MB transformer, a gap a paired bootstrap
//! cannot separate from zero (p = 0.18). It gets there out of 9.1 MB of
//! postings over 7.8 MB of documents. The same corpus encoded as
//! 4096-dimensional hashed term vectors reaches the same 0.662 and costs
//! 85 MB of arena, or 21 MB quantised to int8 -- and in a database whose
//! whole file is resident that is the whole argument. Lexical retrieval is a
//! sparse problem and wants a sparse structure.
//!
//! Queries are answered with MaxScore pruning, so a common term with a long
//! postings list costs its walk only while it can still change the answer.
//!
//! **What it is not: semantic.** On BEIR FiQA, where questions share little
//! vocabulary with the answers that resolve them, this index scores 0.232
//! against the same transformer's 0.368 -- a gap that *is* significant. No
//! amount of tuning closes it, because no amount of counting words recovers a
//! meaning the words do not carry. That is what `rerank` is for: this index
//! chooses the candidates, stored vectors order them.

// Without the `text` feature the index's code is here but unused: the index
// is a type of no value (`off.rs`), and the compiler drops the rest. The
// tokenizer stays, for what else splits text.
#![cfg_attr(not(feature = "text"), allow(dead_code, unused_imports))]

use crate::schema::TextIndexSpec;
use crate::value::DocId;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

/// Calls `f` with every term in `text`, lowercased and I-folded.
///
/// The ASCII path hands out a borrowed slice and allocates nothing; anything
/// else folds through the Unicode default case mapping into a reused buffer.
/// That is the same trade `query::like_match` makes and for the same reason --
/// dropping the folding path would lose `ÇALIŞMA` matching `çalışma`, which
/// every non-English corpus depends on.
///
/// **The dotted and dotless I are folded together**, and that is not what the
/// default mapping does. Unicode is locale-blind, so `İ` (U+0130) lowercases
/// to `i` followed by a COMBINING DOT ABOVE -- a two-code-point term nobody
/// can type, so `İstanbul` was unreachable from `istanbul`. `I` went to `i`
/// while `ı` stayed, so `IŞIK` missed `ışık` as well.
///
/// `I`, `İ`, `ı` and `i` therefore all index as `i`, and a stray combining dot
/// is dropped. The cost is the `kız`/`kiz` distinction, a real Turkish minimal
/// pair; the gain is that case stops deciding whether a word is findable.
///
/// Measured on the Turkish WebFAQ retrieval set (144 846 documents, 10 000
/// queries), nDCG@10 moves 0.4823 -> 0.4841 when the queries are capitalised
/// the way the corpus is, and 0.4729 -> 0.4841 when they are typed in
/// lowercase the way a person actually types them. Small both times, because
/// in that corpus query and document mostly break the same way and so still
/// meet; the point is the second column, where after the fold the two are the
/// same number. Case no longer decides.
///
/// **A script written without spaces is indexed in runs of characters.** Han,
/// kana and Hangul, and Thai, Lao, Khmer and Myanmar, run their words
/// together or hang particles on them, so a run of them is indexed as its
/// overlapping pairs of characters -- `東京都に` is `東京`, `京都`, `都に` --
/// or triples for the four whose characters are letters rather than
/// syllables, and a query's find the documents that hold them anywhere.
/// Whole, a run was one term no query repeated. nDCG@10 by BM25, before and
/// after: C-MTEB's EcomRetrieval 0.006 -> 0.439 and CovidRetrieval 0.162 ->
/// 0.868, WebFAQ's Chinese 0.146 -> 0.686, JaGovFaqs 0.119 -> 0.582, WebFAQ's
/// Korean 0.537 -> 0.682 and its Thai 0.448 -> 0.547 -- pairs of Thai
/// letters were 0.495 at a query three times as slow, too common to tell
/// documents apart.
pub fn for_each_term(text: &str, mut f: impl FnMut(&str)) {
    terms(text, false, &mut f);
}

/// [`for_each_term`], each character of a run of Han, kana or Hangul handed
/// out as well as each pair when `chars` is set (`TextIndexSpec::chars`).
///
/// `f` is a trait object: generic over it, the tokenizer was compiled once
/// for every caller -- insert, remove and search, with prefixes and without
/// -- six copies and 8 KB of the browser module.
fn terms(text: &str, chars: bool, f: &mut dyn FnMut(&str)) {
    let mut buf = String::new();
    // U+0307 is not alphanumeric, so a plain `is_alphanumeric` split would cut
    // `I\u{307}stanbul` -- the decomposed spelling of `İstanbul`, and what the
    // default mapping leaves behind -- into two terms. Keep it inside the
    // word; it is dropped below. So are a Thai or a Khmer word's marks, some
    // of which are not letters: split at them, `ไม่` lost its tone mark.
    for raw in text.split(|c: char| !c.is_alphanumeric() && c != '\u{0307}' && !marked(c)) {
        if raw.is_empty() || raw.chars().all(|c| c == '\u{0307}') {
            continue;
        }
        // Already-lowercase ASCII is its own answer -- `i` folds to `i`.
        if raw.is_ascii() && !raw.bytes().any(|b| b.is_ascii_uppercase()) {
            f(raw);
            continue;
        }
        // A run of a script written without spaces is its runs of characters
        // (`grams`), what is between them a word as any other.
        let mut rest = raw;
        while let Some(first) = rest.chars().next() {
            let n = gram(first);
            let end = rest
                .char_indices()
                .find(|&(_, c)| gram(c) != n)
                .map_or(rest.len(), |(i, _)| i);
            let (run, after) = rest.split_at(end);
            rest = after;
            if n > 0 {
                grams(run, n, chars && n == 2, f);
                continue;
            }
            if run.chars().all(|c| c == '\u{0307}') {
                continue;
            }
            buf.clear();
            for c in run.chars() {
                match c {
                    'I' | 'İ' | 'ı' => buf.push('i'),
                    // Carries nothing once the I forms are folded, and it is
                    // what the default mapping leaves behind for `İ`.
                    '\u{0307}' => {}
                    // A full-width Latin letter or digit, as Japanese and
                    // Chinese text writes them (`ＩＴ`, `２０２３`), is the
                    // ASCII one.
                    '\u{FF01}'..='\u{FF5E}' => {
                        let a = char::from_u32(c as u32 - 0xFEE0).unwrap_or(c);
                        buf.push(a.to_ascii_lowercase());
                    }
                    _ => buf.extend(c.to_lowercase()),
                }
            }
            f(&buf);
        }
    }
}

/// Every overlapping run of `n` characters in `run` -- the whole of it when it
/// is shorter -- and each character as well with `chars`. The starts of the
/// last `n` characters are held in a ring, so a run allocates nothing.
fn grams(run: &str, n: usize, chars: bool, f: &mut dyn FnMut(&str)) {
    let mut starts = [0usize; 3];
    let mut seen = 0;
    for (i, c) in run.char_indices() {
        let end = i + c.len_utf8();
        if chars {
            f(&run[i..end]);
        }
        starts[seen % n] = i;
        seen += 1;
        if seen >= n {
            // The oldest of the last `n` characters: the one `n` back.
            f(&run[starts[seen % n]..end]);
        }
    }
    if seen < n && !(chars && seen == 1) {
        f(run);
    }
}

/// How a run of `c`'s script is indexed: in overlapping runs of 2
/// characters for Han, kana and Hangul, whose characters are syllables or
/// words, and of 3 for Thai, Lao, Khmer and Myanmar, whose are letters; 0
/// for a script that spaces its words, whose words are the terms.
fn gram(c: char) -> usize {
    match c as u32 {
        0x0E00..=0x0EFF      // Thai, Lao
        | 0x1000..=0x109F    // Myanmar
        | 0x1780..=0x17FF => 3, // Khmer
        0x1100..=0x11FF      // Hangul jamo
        | 0x3005..=0x3007    // 々, 〆, 〇
        | 0x3041..=0x30FF    // Hiragana, Katakana
        | 0x3131..=0x318E    // Hangul compatibility jamo
        | 0x31F0..=0x31FF    // Katakana phonetic extensions
        | 0x3400..=0x4DBF    // Han, extension A
        | 0x4E00..=0x9FFF    // Han
        | 0xA960..=0xA97F    // Hangul jamo extended-A
        | 0xAC00..=0xD7FF    // Hangul syllables, jamo extended-B
        | 0xF900..=0xFAFF    // Han compatibility
        | 0xFF66..=0xFFDC    // half-width katakana and Hangul
        | 0x20000..=0x3FFFF => 2, // Han of the other planes
        _ => 0,
    }
}

/// A mark of a script whose words hold marks that are not letters -- a Thai
/// tone mark, a Khmer or Myanmar sign -- kept inside the word.
fn marked(c: char) -> bool {
    matches!(c as u32, 0x0E31..=0x0E4E | 0x0EB1..=0x0ECD | 0x102B..=0x103E | 0x17B4..=0x17D3)
}

/// `for_each_term` collected. For queries and tests, where the allocation is
/// noise next to the postings walk that follows.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for_each_term(text, |t| out.push(t.to_string()));
    out
}

/// Every term a document contributes under `spec`: the words, and their
/// prefixes when the index asks for them.
///
/// The prefixes are marked with a leading `^` so they cannot collide with a
/// real word of the same length -- a term is alphanumeric by construction, so
/// the marker is free. They count towards the document length as well, which
/// is what keeps BM25's normalisation meaningful: every document grows by
/// roughly the same factor, so `dl / avgdl` is where it was.
pub fn for_each_indexed_term(text: &str, spec: &TextIndexSpec, mut f: impl FnMut(&str)) {
    let Some(prefixes) = spec.prefixes() else {
        terms(text, spec.chars, &mut f);
        return;
    };
    let mut buf = String::new();
    terms(text, spec.chars, &mut |w| {
        f(w);
        // `chars`, not bytes: a Turkish word is not one byte per letter, and
        // slicing it as if it were would panic on a boundary.
        let n = w.chars().count();
        for k in prefixes.clone() {
            if n <= k {
                break; // the whole word is already indexed above
            }
            buf.clear();
            buf.push('^');
            buf.extend(w.chars().take(k));
            f(&buf);
        }
    });
}

/// Postings for one term, held as parallel arrays.
///
/// A `Vec<(DocId, u32)>` is the obvious shape and the wrong one: the tuple
/// pads to 16 bytes, a quarter of it for nothing. Split, the same postings
/// take 12, and the merge -- which reads ids and touches a count only on a
/// hit -- walks a contiguous run of ids instead of striding over the counts.
#[derive(Default)]
struct Postings {
    docs: Vec<DocId>,
    tfs: Vec<u32>,
    /// The largest `tf` in this list, for the MaxScore ceiling in `search`.
    ///
    /// Only ever raised. A removal can leave it too high, which costs a
    /// looser bound and never a wrong answer; a rebuild makes it exact again.
    max_tf: u32,
}

impl Postings {
    fn len(&self) -> usize {
        self.docs.len()
    }
    fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }
    fn find(&self, doc: DocId) -> std::result::Result<usize, usize> {
        self.docs.binary_search(&doc)
    }
    fn set(&mut self, doc: DocId, tf: u32) {
        self.max_tf = self.max_tf.max(tf);
        match self.find(doc) {
            Ok(at) => self.tfs[at] = tf,
            Err(at) => {
                self.docs.insert(at, doc);
                self.tfs.insert(at, tf);
            }
        }
    }
    fn drop_doc(&mut self, doc: DocId) {
        if let Ok(at) = self.find(doc) {
            self.docs.remove(at);
            self.tfs.remove(at);
        }
    }
    fn bytes(&self) -> usize {
        self.docs.capacity() * std::mem::size_of::<DocId>() + self.tfs.capacity() * 4 + 4
    }
    fn shrink(&mut self) {
        self.docs.shrink_to_fit();
        self.tfs.shrink_to_fit();
    }
}

/// Document lengths, dense where the ids are and sparse where they are not.
///
/// Scoring reads this once per candidate document, and on a corpus with
/// common query terms that is most of the collection -- 50 000 of FiQA's
/// 57 638 for the average query. A `HashMap` lookup there is the single
/// largest cost in the merge. The store's `IdIndex` already answers the same
/// shape of question the same way, so this follows it, gap ceiling included.
#[derive(Default)]
struct DocLengths {
    /// `dense[i]` is the length of document `i + 1`; 0 means absent, which is
    /// unambiguous because a document with no terms is never indexed.
    dense: Vec<u32>,
    sparse: HashMap<DocId, u32>,
    count: usize,
}

/// The largest gap still worth extending the dense array for. The store uses
/// the same number for the same reason.
const MAX_DENSE_GAP: u64 = 4096;

impl DocLengths {
    /// The comparison stays on the `u64` side: on wasm32 `usize` is 32 bits
    /// and `id as usize` truncates silently. The store hit exactly that.
    #[inline]
    fn in_dense(&self, id: DocId) -> bool {
        id >= 1 && id <= self.dense.len() as u64
    }

    #[inline]
    fn get(&self, id: DocId) -> u32 {
        if self.in_dense(id) {
            return self.dense[id as usize - 1];
        }
        self.sparse.get(&id).copied().unwrap_or(0)
    }

    /// Returns the length this document had before, if it had one.
    fn insert(&mut self, id: DocId, len: u32) -> Option<u32> {
        let slot = if self.in_dense(id) {
            &mut self.dense[id as usize - 1]
        } else if id >= 1 && id - self.dense.len() as u64 <= MAX_DENSE_GAP {
            self.dense.resize(id as usize, 0);
            &mut self.dense[id as usize - 1]
        } else {
            let old = self.sparse.insert(id, len);
            if old.is_none() {
                self.count += 1;
            }
            return old;
        };
        let old = *slot;
        *slot = len;
        if old == 0 {
            self.count += 1;
            None
        } else {
            Some(old)
        }
    }

    fn remove(&mut self, id: DocId) -> Option<u32> {
        let old = if self.in_dense(id) {
            let slot = &mut self.dense[id as usize - 1];
            let old = *slot;
            *slot = 0;
            if old == 0 {
                None
            } else {
                Some(old)
            }
        } else {
            self.sparse.remove(&id)
        };
        if old.is_some() {
            self.count -= 1;
        }
        old
    }

    fn len(&self) -> usize {
        self.count
    }

    fn clear(&mut self) {
        self.dense.clear();
        self.sparse.clear();
        self.count = 0;
    }

    fn bytes(&self) -> usize {
        self.dense.capacity() * 4 + self.sparse.capacity() * (std::mem::size_of::<DocId>() + 4 + 1)
    }

    fn shrink_to_fit(&mut self) {
        self.dense.shrink_to_fit();
        self.sparse.shrink_to_fit();
    }
}

/// An inverted index with BM25 scoring.
///
/// Like the HNSW graph this is derived data -- it is rebuilt from the
/// documents on open. Unlike the graph it is *not* persisted: rebuilding
/// measures 27 us per document (SciFact, 0.14 s for 5 183) against the
/// graph's ~102 us, and the rebuild pass already reads every document to
/// fill the hash indexes. A second record kind, and the validation path that
/// would have to come with it, was not worth ~3 s per 100 000 documents.
#[cfg(feature = "text")]
pub struct TextIndex {
    pub spec: TextIndexSpec,
    /// term -> postings, kept ascending by document id so `search` can merge
    /// them without sorting.
    postings: HashMap<String, Postings>,
    /// document -> term count, for the length normalisation.
    lengths: DocLengths,
    total_terms: u64,
    /// What the dictionary and the postings hold, as `memory_bytes` counts
    /// it, kept as they change: summed over every term it cost 0.42 ms at
    /// 200 000 documents, and `--max-memory` asks before every write.
    heap: usize,
}

#[cfg(feature = "text")]
impl TextIndex {
    pub fn new(spec: TextIndexSpec) -> TextIndex {
        TextIndex {
            spec,
            postings: HashMap::new(),
            lengths: DocLengths::default(),
            total_terms: 0,
            heap: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.lengths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lengths.len() == 0
    }

    pub fn terms(&self) -> usize {
        self.postings.len()
    }

    pub fn postings_count(&self) -> usize {
        self.postings.values().map(|p| p.len()).sum()
    }

    /// Postings, the term dictionary and the length table, as they sit in
    /// memory. Not RSS -- the same caveat as `Database::memory_bytes`.
    pub fn memory_bytes(&self) -> usize {
        self.heap + self.lengths.bytes()
    }

    /// What one term costs: its dictionary entry and its postings.
    fn term_bytes(term: &str, p: &Postings) -> usize {
        term.len() + 32 + p.bytes()
    }

    pub fn insert(&mut self, doc: DocId, text: &str) {
        let mut tf: HashMap<String, u32> = HashMap::new();
        let mut n = 0u32;
        for_each_indexed_term(text, &self.spec, |t| {
            n += 1;
            match tf.get_mut(t) {
                Some(c) => *c += 1,
                None => {
                    tf.insert(t.to_string(), 1);
                }
            }
        });
        if n == 0 {
            return;
        }
        for (term, count) in tf {
            let len = term.len();
            // Ascending by document id. Ingest hands ids out in order, so the
            // search lands at the end and this is a push; only an update to an
            // older document pays for the shift.
            let p = self.postings.entry(term).or_default();
            // A list is empty only as it is made: the last document out
            // takes it with it.
            let before = if p.is_empty() {
                0
            } else {
                len + 32 + p.bytes()
            };
            p.set(doc, count);
            self.heap += len + 32 + p.bytes() - before;
        }
        if let Some(old) = self.lengths.insert(doc, n) {
            self.total_terms -= old as u64;
        }
        self.total_terms += n as u64;
    }

    /// Removes a document. `text` must be what was indexed -- every caller
    /// reads the stored document first, so the terms are the right ones.
    ///
    /// A list the document leaves empty goes as the document leaves it:
    /// `retain` over the dictionary afterwards walked every term, 0.38 ms a
    /// delete -- or an update of any field -- at 200 000 documents.
    pub fn remove(&mut self, doc: DocId, text: &str) {
        let spec = self.spec;
        for_each_indexed_term(text, &spec, |t| {
            if let Some(list) = self.postings.get_mut(t) {
                list.drop_doc(doc);
                if list.is_empty() {
                    self.heap -= TextIndex::term_bytes(t, list);
                    self.postings.remove(t);
                }
            }
        });
        if let Some(n) = self.lengths.remove(doc) {
            self.total_terms -= n as u64;
        }
    }

    /// Gives back the growth slack of every postings list.
    ///
    /// `Vec` grows by doubling, so a list built one document at a time sits
    /// at ~1.5x the bytes it needs -- 3.8 MB of the 13.0 MB SciFact index.
    /// That slack is the price of cheap appends and it is worth paying while
    /// documents are still arriving; it is not worth paying afterwards.
    /// Called where the index is known to be complete: a rebuild on open, and
    /// `create index`.
    pub fn shrink_to_fit(&mut self) {
        self.heap = 0;
        for (term, list) in self.postings.iter_mut() {
            list.shrink();
            self.heap += TextIndex::term_bytes(term, list);
        }
        self.postings.shrink_to_fit();
        self.lengths.shrink_to_fit();
    }

    pub fn clear(&mut self) {
        self.postings.clear();
        self.lengths.clear();
        self.total_terms = 0;
        self.heap = 0;
    }

    fn avgdl(&self) -> f32 {
        if self.lengths.len() == 0 {
            return 1.0;
        }
        (self.total_terms as f32 / self.lengths.len() as f32).max(1.0)
    }

    /// Robertson/Sparck-Jones idf, the `ln(1 + ...)` form: always positive, so
    /// a term occurring in every document contributes ~0 rather than pushing
    /// the score negative the way the unsmoothed form does.
    fn idf(&self, df: usize) -> f32 {
        let n = self.lengths.len() as f32;
        let df = df as f32;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    /// Top `k` documents for `query`, best first.
    ///
    /// `accept` is the filter membership test; it is applied while the
    /// postings are merged, so a `where` clause never costs a second pass.
    pub fn search<F>(&self, query: &str, k: usize, accept: F) -> Vec<(DocId, f32)>
    where
        F: Fn(DocId) -> bool,
    {
        if k == 0 || self.lengths.len() == 0 {
            return Vec::new();
        }
        // Repeated query terms fold into a weight. Scoring the same postings
        // list twice would give the same answer for twice the walk.
        let mut qtf: HashMap<String, u32> = HashMap::new();
        for_each_indexed_term(query, &self.spec, |t| match qtf.get_mut(t) {
            Some(c) => *c += 1,
            None => {
                qtf.insert(t.to_string(), 1);
            }
        });

        let (k1, b, avgdl) = (self.spec.k1(), self.spec.b(), self.avgdl());
        let mut cursors: Vec<Cursor> = Vec::with_capacity(qtf.len());
        for (term, count) in &qtf {
            if let Some(list) = self.postings.get(term) {
                let weight = *count as f32 * self.idf(list.len());
                // The most this term can ever add to a document: its largest
                // `tf`, against the most generous length normalisation there
                // is (`dl` -> 0). An over-estimate is what makes it safe.
                let tf = list.max_tf as f32;
                let ceiling = weight * (tf * (k1 + 1.0)) / (tf + k1 * (1.0 - b));
                cursors.push(Cursor {
                    at: 0,
                    list,
                    weight,
                    ceiling,
                });
            }
        }
        if cursors.is_empty() {
            return Vec::new();
        }

        // MaxScore (Turtle & Flood). Sorted by ceiling, `reach[i]` is the most
        // the first `i` terms can contribute together. Once the heap is full,
        // any term set whose reach falls short of the worst kept score can no
        // longer *start* a candidate: a document holding only those terms
        // cannot get in. Those terms stay scorable, they just stop driving the
        // frontier -- which is the whole saving, because the terms with the
        // lowest ceilings are the common ones with the longest postings.
        //
        // Worth doing because the exhaustive walk is not selective at all: on
        // BEIR FiQA the average query reaches 86% of the corpus, 50 000 of
        // 57 638 documents, almost all of it through terms that carry no
        // information. Measured, same answers throughout: SciFact 179 -> 47
        // us, FiQA 1918 -> 311 us, Turkish WebFAQ 401 -> 82 us. The rows it
        // returns are exactly the exhaustive ones, which is what
        // `pruning_never_changes_the_answer` holds it to.
        cursors.sort_by(|a, b| a.ceiling.total_cmp(&b.ceiling));
        let mut reach: Vec<f32> = Vec::with_capacity(cursors.len() + 1);
        reach.push(0.0);
        for c in &cursors {
            reach.push(reach[reach.len() - 1] + c.ceiling);
        }

        let mut heap: BinaryHeap<ByScore> = BinaryHeap::with_capacity(k + 1);
        // Nothing is pruned until the heap is full; `-inf` says so without a
        // second flag to keep in step.
        let mut worst_kept = f32::NEG_INFINITY;
        let mut pivot = 0usize;
        loop {
            // Strictly less, never "less or equal": a document that ties the
            // worst kept score still has to be compared against it, or the
            // tie-break on document id would depend on the pruning.
            while pivot < cursors.len() && reach[pivot + 1] < worst_kept {
                pivot += 1;
            }
            if pivot == cursors.len() {
                break; // nothing left that could beat what is already held
            }

            // The lists are ascending, so the frontier is the smallest head
            // among the terms that can still start a candidate. With a
            // handful of them a linear scan beats a heap over the cursors:
            // fewer branches, no allocation.
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
            let dl = self.lengths.get(doc).max(1) as f32;
            let denom_len = k1 * (1.0 - b + b * dl / avgdl);
            let mut score = 0.0f64;
            for c in cursors[pivot..].iter_mut() {
                if c.list.docs.get(c.at) == Some(&doc) {
                    score += c.contribution(c.at, k1, denom_len);
                    c.at += 1;
                }
            }
            // The rest can only add. Walk them from the largest ceiling down
            // and stop as soon as everything still unread cannot carry the
            // score to where it would matter.
            let mut gave_up = false;
            for j in (0..pivot).rev() {
                // What is still unread cannot carry this document to where it
                // would matter. Stop, and do not offer the partial score --
                // it would be popped straight back out, but saying so here is
                // cheaper than relying on that.
                if score + (reach[j + 1] as f64) < (worst_kept as f64) {
                    gave_up = true;
                    break;
                }
                let c = &mut cursors[j];
                // These are only ever asked about increasing document ids, so
                // the search starts where the last one left off.
                c.at += c.list.docs[c.at..].partition_point(|d| *d < doc);
                if c.list.docs.get(c.at) == Some(&doc) {
                    score += c.contribution(c.at, k1, denom_len);
                }
            }
            if !gave_up && score > 0.0 && accept(doc) {
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
        out.sort_by(best_first);
        out
    }
}

/// Highest score first, ties to the lower id. A function rather than a
/// closure so that every ranking sorted this way -- `fuse`'s too -- shares
/// one copy of the sort: a closure inside the generic `search` is a type of
/// its own for every `accept` it is given.
pub(crate) fn best_first(a: &(DocId, f32), b: &(DocId, f32)) -> Ordering {
    b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0))
}

/// One query term's walk through its postings.
struct Cursor<'a> {
    at: usize,
    list: &'a Postings,
    /// idf, times how often the term occurs in the query.
    weight: f32,
    /// The most this term can contribute to any single document.
    ceiling: f32,
}

impl Cursor<'_> {
    /// One term's BM25 contribution, accumulated in `f64`.
    ///
    /// The terms are summed in whatever order the walk reaches them, and that
    /// order is not the same with pruning as without it. In `f32` the
    /// difference lands at ~1e-7, which is enough to reorder two documents
    /// that score exactly the same and so to make the tie-break depend on the
    /// plan rather than on the document id. Summing in `f64` puts the
    /// difference at ~1e-16, which the cast back to `f32` absorbs.
    #[inline]
    fn contribution(&self, at: usize, k1: f32, denom_len: f32) -> f64 {
        let tf = self.list.tfs[at] as f32;
        (self.weight * (tf * (k1 + 1.0)) / (tf + denom_len)) as f64
    }
}

/// Ordered so that the *worst* candidate sits at the top of the max-heap --
/// that is the one a better score evicts. Ties break on the document id so
/// the same query always returns the same rows in the same order. The
/// sparse index keeps its best the same way, through this one type.
#[derive(PartialEq)]
pub(crate) struct ByScore(pub(crate) f32, pub(crate) DocId);

impl Eq for ByScore {}

impl Ord for ByScore {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .0
            .total_cmp(&self.0)
            .then_with(|| self.1.cmp(&other.1))
    }
}

impl PartialOrd for ByScore {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(not(feature = "text"))]
pub use crate::off::TextIndex;

#[cfg(all(test, feature = "text"))]
impl TextIndex {
    /// The same walk with the pruning switched off: every term drives the
    /// frontier, every matching document is scored. Only `search`'s
    /// equivalence test uses it -- it is the thing MaxScore has to agree with.
    fn search_unpruned<F>(&self, query: &str, k: usize, accept: F) -> Vec<(DocId, f32)>
    where
        F: Fn(DocId) -> bool,
    {
        if k == 0 || self.lengths.len() == 0 {
            return Vec::new();
        }
        let mut qtf: HashMap<String, u32> = HashMap::new();
        for_each_indexed_term(query, &self.spec, |t| match qtf.get_mut(t) {
            Some(c) => *c += 1,
            None => {
                qtf.insert(t.to_string(), 1);
            }
        });
        let (k1, b, avgdl) = (self.spec.k1(), self.spec.b(), self.avgdl());
        let mut cursors: Vec<Cursor> = Vec::new();
        for (term, count) in &qtf {
            if let Some(list) = self.postings.get(term) {
                let weight = *count as f32 * self.idf(list.len());
                let tf = list.max_tf as f32;
                cursors.push(Cursor {
                    at: 0,
                    list,
                    weight,
                    ceiling: weight * (tf * (k1 + 1.0)) / (tf + k1 * (1.0 - b)),
                });
            }
        }
        if cursors.is_empty() {
            return Vec::new();
        }
        cursors.sort_by(|a, b| a.ceiling.total_cmp(&b.ceiling));
        let mut heap: BinaryHeap<ByScore> = BinaryHeap::with_capacity(k + 1);
        loop {
            let mut doc = DocId::MAX;
            for c in &cursors {
                if let Some(d) = c.list.docs.get(c.at) {
                    if *d < doc {
                        doc = *d;
                    }
                }
            }
            if doc == DocId::MAX {
                break;
            }
            let dl = self.lengths.get(doc).max(1) as f32;
            let denom_len = k1 * (1.0 - b + b * dl / avgdl);
            let mut score = 0.0f64;
            for c in cursors.iter_mut() {
                if c.list.docs.get(c.at) == Some(&doc) {
                    score += c.contribution(c.at, k1, denom_len);
                    c.at += 1;
                }
            }
            if score > 0.0 && accept(doc) {
                heap.push(ByScore(score as f32, doc));
                if heap.len() > k {
                    heap.pop();
                }
            }
        }
        let mut out: Vec<(DocId, f32)> = heap.into_iter().map(|s| (s.1, s.0)).collect();
        out.sort_by(best_first);
        out
    }
}

#[cfg(all(test, feature = "text"))]
mod tests {
    use super::*;

    fn spec() -> TextIndexSpec {
        TextIndexSpec::default()
    }

    #[test]
    fn tokenizes_and_folds_case() {
        assert_eq!(tokenize("Rust and WASM"), ["rust", "and", "wasm"]);
        assert_eq!(tokenize("a-b_c.d"), ["a", "b", "c", "d"]);
        assert_eq!(tokenize("  "), Vec::<String>::new());
        // Non-ASCII goes through the Unicode mapping, not the ASCII path.
        assert_eq!(tokenize("Grüße"), ["grüße"]);
        // Case must not decide the term, whatever the alphabet. The Turkish
        // pair is spelled out in `the_turkish_i_folds_every_way_it_is_written`.
        for (a, b) in [("ÖDEV", "ödev"), ("ÇALIŞMA", "çalışma"), ("GRÜßE", "grüße")] {
            assert_eq!(tokenize(a), tokenize(b), "{a} vs {b}");
        }
    }

    /// Every way of writing the Turkish I has to land on the same term, or a
    /// capitalised word is unfindable. The default Unicode mapping does not
    /// do this on its own -- see `for_each_term`.
    #[test]
    fn the_turkish_i_folds_every_way_it_is_written() {
        for group in [
            ["İSTANBUL", "İstanbul", "istanbul", "ISTANBUL"],
            ["IŞIK", "ışık", "Işık", "işik"],
            ["KIZ", "kız", "Kız", "kiz"],
        ] {
            let first = tokenize(group[0]);
            for other in &group[1..] {
                assert_eq!(first, tokenize(other), "{:?} vs {other:?}", group[0]);
            }
            // And nothing combining survives into the term.
            assert!(!first[0].contains('\u{0307}'), "{first:?}");
        }
        // The decomposed spelling of `İ` (I + combining dot) folds too.
        assert_eq!(tokenize("I\u{307}stanbul"), tokenize("İstanbul"));
    }

    #[test]
    fn prefixes_are_only_emitted_when_asked_for() {
        // `ı` folds to `i`, so the stored term is `kitaplarin`.
        let mut off = Vec::new();
        for_each_indexed_term("kitapların", &spec(), |t| off.push(t.to_string()));
        assert_eq!(off, ["kitaplarin"]);

        let on = TextIndexSpec {
            prefix_min: 3,
            prefix_max: 6,
            ..TextIndexSpec::default()
        };
        let mut got = Vec::new();
        for_each_indexed_term("kitapların", &on, |t| got.push(t.to_string()));
        assert_eq!(got, ["kitaplarin", "^kit", "^kita", "^kitap", "^kitapl"]);
        // A word no longer than the prefix is already its own term.
        let mut short = Vec::new();
        for_each_indexed_term("ev kar", &on, |t| short.push(t.to_string()));
        assert_eq!(short, ["ev", "kar"]);
    }

    /// A script written without spaces is indexed in overlapping runs of
    /// characters: pairs of Han, kana and Hangul, across the kana a Japanese
    /// word ends in, and triples of Thai letters, a tone mark among them.
    #[test]
    fn unspaced_scripts_are_indexed_in_runs_of_characters() {
        assert_eq!(tokenize("北京天安门"), ["北京", "京天", "天安", "安门"]);
        assert_eq!(
            tokenize("東京都に住む"),
            ["東京", "京都", "都に", "に住", "住む"]
        );
        assert_eq!(tokenize("학교에 간다"), ["학교", "교에", "간다"]);
        assert_eq!(
            tokenize("ไม่มีปัญหา"),
            ["ไม่", "ม่ม", "่มี", "มีป", "ีปั", "ปัญ", "ัญห", "ญหา"]
        );
        // A run shorter than its gram is one term; a script that spaces its
        // words keeps them whole beside one that does not.
        assert_eq!(tokenize("京 ไม่"), ["京", "ไม่"]);
        assert_eq!(
            tokenize("iPhone手机 2023年"),
            ["iphone", "手机", "2023", "年"]
        );
        // Full-width Latin and digits are the ASCII ones.
        assert_eq!(tokenize("ＩＴ企業 ２０２３"), ["it", "企業", "2023"]);
    }

    /// `chars` adds each character of Han, kana and Hangul, once for a run
    /// of one, and nothing to Thai, whose characters are letters.
    #[test]
    fn chars_add_each_character_to_the_pairs() {
        let on = TextIndexSpec {
            chars: true,
            ..TextIndexSpec::default()
        };
        let terms = |text: &str| {
            let mut got = Vec::new();
            for_each_indexed_term(text, &on, |t| got.push(t.to_string()));
            got.sort();
            got
        };
        assert_eq!(terms("北京市"), ["京", "京市", "北", "北京", "市"]);
        assert_eq!(terms("京"), ["京"]);
        assert_eq!(terms("ไม่มี"), ["ม่ม", "ไม่", "่มี"]);
        let mut ix = TextIndex::new(on);
        ix.insert(1, "北京市");
        ix.insert(2, "上海市");
        let ids = |q: &str| {
            ix.search(q, 10, |_| true)
                .iter()
                .map(|h| h.0)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("京"), [1], "one character finds the runs holding it");
        assert_eq!(ids("市").len(), 2);
    }

    /// The whole point of the option: an inflected form and its stem have to
    /// meet somewhere. Byte slicing would also panic here -- these are
    /// two-byte letters.
    #[test]
    fn prefixes_let_an_inflected_form_meet_its_stem() {
        let on = TextIndexSpec {
            prefix_min: 3,
            prefix_max: 6,
            ..TextIndexSpec::default()
        };
        let mut ix = TextIndex::new(on);
        ix.insert(1, "kitapların fiyatı");
        ix.insert(2, "araba kiralama");
        assert_eq!(ix.search("kitap", 10, |_| true)[0].0, 1);
        assert_eq!(ix.search("kitabı", 10, |_| true)[0].0, 1);

        // Without the option the same query finds nothing at all.
        let mut plain = TextIndex::new(spec());
        plain.insert(1, "kitapların fiyatı");
        assert!(plain.search("kitap", 10, |_| true).is_empty());
    }

    /// A small deterministic generator: no dev-dependency, and a failing
    /// seed can be replayed by hand.
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

    /// MaxScore is only allowed to be faster. Whatever it prunes, the rows it
    /// returns -- ids, order and scores -- have to be the ones the exhaustive
    /// walk returns, including how ties break.
    ///
    /// The vocabulary is deliberately skewed: a few terms in almost every
    /// document (which is what makes pruning worth doing) and a long tail of
    /// rare ones (which is what it must not lose).
    #[test]
    fn pruning_never_changes_the_answer() {
        for seed in 1..=250u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let vocab: Vec<String> = (0..60).map(|i| format!("t{i}")).collect();
            let mut ix = TextIndex::new(spec());
            let n_docs = 40 + rng.upto(160);
            for id in 1..=n_docs as u64 {
                let len = 3 + rng.upto(25);
                let mut words = Vec::with_capacity(len);
                for _ in 0..len {
                    // 60% of the time from the first three terms, so they end
                    // up in nearly every document.
                    let w = if rng.upto(10) < 6 {
                        rng.upto(3)
                    } else {
                        rng.upto(vocab.len())
                    };
                    words.push(vocab[w].as_str());
                }
                ix.insert(id, &words.join(" "));
            }
            for _ in 0..25 {
                let qlen = 1 + rng.upto(6);
                let q: Vec<&str> = (0..qlen)
                    .map(|_| {
                        let w = if rng.upto(10) < 5 {
                            rng.upto(3)
                        } else {
                            rng.upto(vocab.len())
                        };
                        vocab[w].as_str()
                    })
                    .collect();
                let query = q.join(" ");
                for k in [1usize, 3, 10, 50] {
                    let odd = rng.upto(4) == 0;
                    let want = ix.search_unpruned(&query, k, |d| !odd || d % 2 == 0);
                    let got = ix.search(&query, k, |d| !odd || d % 2 == 0);
                    assert_eq!(
                        want.iter().map(|x| x.0).collect::<Vec<_>>(),
                        got.iter().map(|x| x.0).collect::<Vec<_>>(),
                        "seed {seed} k {k} query {query:?}"
                    );
                    for (w, g) in want.iter().zip(&got) {
                        assert!(
                            (w.1 - g.1).abs() <= 1e-5 * w.1.abs().max(1.0),
                            "seed {seed} score {} vs {}",
                            w.1,
                            g.1
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn ranks_by_bm25() {
        let mut ix = TextIndex::new(spec());
        ix.insert(1, "the rust programming language");
        ix.insert(2, "rust rust rust and more rust");
        ix.insert(3, "python programming");
        let hits = ix.search("rust", 10, |_| true);
        // Document 2 says it four times in six words; it has to come first.
        assert_eq!(hits[0].0, 2);
        assert_eq!(hits.len(), 2);
        assert!(hits[0].1 > hits[1].1);
    }

    #[test]
    fn a_term_in_every_document_separates_nothing() {
        let mut ix = TextIndex::new(spec());
        for id in 1..=5 {
            ix.insert(id, "common term here");
        }
        // idf is ~0 for a term with df == n, so no document stands out; the
        // unsmoothed form would have gone negative here instead.
        let hits = ix.search("common", 10, |_| true);
        assert_eq!(hits.len(), 5);
        for (_, s) in &hits {
            assert!(*s >= 0.0 && *s < 0.2, "score {s} should be near zero");
        }
    }

    #[test]
    fn removal_restores_the_empty_state() {
        let mut ix = TextIndex::new(spec());
        ix.insert(1, "alpha beta");
        ix.insert(2, "beta gamma");
        ix.remove(1, "alpha beta");
        assert_eq!(ix.len(), 1);
        assert!(ix.search("alpha", 10, |_| true).is_empty());
        assert_eq!(ix.search("beta", 10, |_| true)[0].0, 2);
        ix.remove(2, "beta gamma");
        assert!(ix.is_empty());
        assert_eq!(ix.terms(), 0, "the dictionary must not keep empty lists");
        assert_eq!(ix.total_terms, 0);
    }

    /// The count `memory_bytes` reads is the sum over every term it
    /// replaced, through inserts, updates, removals and a shrink.
    #[test]
    fn the_byte_count_is_the_walk_it_replaced() {
        let walked = |ix: &TextIndex| {
            ix.postings
                .iter()
                .map(|(t, p)| TextIndex::term_bytes(t, p))
                .sum::<usize>()
                + ix.lengths.bytes()
        };
        let mut ix = TextIndex::new(spec());
        let mut seed = 0x51ED_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut docs: Vec<(DocId, String)> = Vec::new();
        for round in 0..2000u64 {
            let id = 1 + next() % 300;
            if let Some(at) = docs.iter().position(|d| d.0 == id) {
                let (_, old) = docs.remove(at);
                ix.remove(id, &old);
            }
            if round % 3 != 0 {
                let words: Vec<String> = (0..1 + next() % 6)
                    .map(|_| format!("w{}", next() % 40))
                    .collect();
                let text = words.join(" ");
                ix.insert(id, &text);
                docs.push((id, text));
            }
            assert_eq!(ix.memory_bytes(), walked(&ix), "round {round}");
        }
        ix.shrink_to_fit();
        assert_eq!(ix.memory_bytes(), walked(&ix));
        for (id, text) in docs {
            ix.remove(id, &text);
        }
        assert_eq!(ix.terms(), 0);
        assert_eq!(ix.memory_bytes(), ix.lengths.bytes());
    }

    #[test]
    fn reindexing_a_document_replaces_its_terms() {
        let mut ix = TextIndex::new(spec());
        ix.insert(1, "alpha beta");
        ix.remove(1, "alpha beta");
        ix.insert(1, "gamma delta");
        assert!(ix.search("alpha", 10, |_| true).is_empty());
        assert_eq!(ix.search("gamma", 10, |_| true)[0].0, 1);
        assert_eq!(ix.len(), 1);
    }

    #[test]
    fn the_filter_is_applied_during_the_merge() {
        let mut ix = TextIndex::new(spec());
        ix.insert(1, "rust");
        ix.insert(2, "rust");
        ix.insert(3, "rust");
        let hits = ix.search("rust", 10, |d| d != 2);
        assert_eq!(hits.iter().map(|h| h.0).collect::<Vec<_>>(), [1, 3]);
    }

    #[test]
    fn top_k_keeps_the_best_and_is_deterministic() {
        let mut ix = TextIndex::new(spec());
        ix.insert(1, "rust rust rust");
        ix.insert(2, "rust rust");
        ix.insert(3, "rust");
        ix.insert(4, "rust");
        let hits = ix.search("rust", 2, |_| true);
        assert_eq!(hits.iter().map(|h| h.0).collect::<Vec<_>>(), [1, 2]);
        // Documents 3 and 4 score identically; ties must resolve on the id.
        let all = ix.search("rust", 4, |_| true);
        assert_eq!(all[2].0, 3);
        assert_eq!(all[3].0, 4);
    }

    #[test]
    fn out_of_order_inserts_keep_the_postings_sorted() {
        let mut ix = TextIndex::new(spec());
        for id in [9u64, 3, 7, 1, 5] {
            ix.insert(id, "term");
        }
        let list = &ix.postings["term"];
        assert!(list.docs.windows(2).all(|w| w[0] < w[1]), "{:?}", list.docs);
    }

    #[test]
    fn repeated_query_terms_do_not_change_the_ranking() {
        let mut ix = TextIndex::new(spec());
        ix.insert(1, "alpha beta beta");
        ix.insert(2, "alpha alpha beta");
        let once = ix.search("alpha beta", 10, |_| true);
        let twice = ix.search("alpha alpha beta beta", 10, |_| true);
        assert_eq!(
            once.iter().map(|h| h.0).collect::<Vec<_>>(),
            twice.iter().map(|h| h.0).collect::<Vec<_>>()
        );
    }
}
