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

use crate::error::{Error, Result};
#[cfg(feature = "text")]
use crate::fuzzy::{typos, Dictionary};
use crate::maps::Map;
use crate::schema::TextIndexSpec;
#[cfg(feature = "text")]
use crate::store::DocMap;
use crate::value::DocId;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
#[cfg(feature = "text")]
use terms::{List, Postings, Terms};

#[cfg(feature = "text")]
mod terms;

/// How loosely a `match` reads its query's words: `match body $1 fuzzy`,
/// `fuzzy 1`, `prefix`. Plain -- no edits, no prefix -- is a `match` as it
/// always was, down the same code.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Loose {
    /// The most edits a word may take, 0 to 2, each word held besides to
    /// what its length allows ([`crate::fuzzy::typos`]): `fuzzy` alone is
    /// 2, so the length decides.
    pub fuzzy: u8,
    /// The query's last word also matches the words it begins: what a
    /// search box sends as it is typed into.
    pub prefix: bool,
}

impl Loose {
    pub fn is_plain(&self) -> bool {
        self.fuzzy == 0 && !self.prefix
    }
}

/// The most words of an index one word of a query may reach through
/// `fuzzy` or `prefix`, itself among them. Past it the query is refused,
/// never cut: a word read as the first 10 000 of its 30 000 completions is
/// a wrong answer believed right. 10 000 as `near` and `match` answer at
/// most 10 000 rows: FiQA's 80 426 words hold no prefix of one letter
/// reaching more (`s` reaches 7 020, `co` 2 245, `con` 794), so a search
/// box is refused only at a first letter over a larger vocabulary. Each
/// word reached is a list merged into its word's: over 100 000 documents of
/// 60 000 made-up words a word's first two letters took 3.0 ms, its first
/// three 0.44 (`make fuzzy-bench`).
pub const MAX_EXPANSIONS: usize = 10_000;

/// What a word found near a query's word counts for, by its distance: one
/// it begins (`prefix`), one edit away, two. Of the weight the word's
/// variants of that distance carry together, as one term (`Reach`).
/// Measured on BEIR with a typo in each query (`make beir`, nDCG@10), a
/// word one edit away is the word meant -- at the whole weight SciFact's
/// typed queries went 0.636 -> 0.663, at half 0.657 -- and two away is as
/// often another word: at the whole weight FiQA's went 0.2125 -> 0.2261, at
/// a quarter 0.2292.
#[cfg(feature = "text")]
const NEAR_WEIGHT: [f32; 3] = [1.0, 1.0, 0.25];

/// What a derived index of the text index's is held in: a lock natively,
/// a second reader waiting for the first one's build, a cell in the
/// browser, which has one thread -- as `engine::Derived` holds an index.
#[cfg(all(feature = "text", not(target_arch = "wasm32")))]
type Lazy<T> = std::sync::OnceLock<T>;
#[cfg(all(feature = "text", target_arch = "wasm32"))]
type Lazy<T> = std::cell::OnceCell<T>;

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
    terms(text, false, &mut |t, _, _| f(t));
}

/// [`for_each_term`], each character of a run of Han, kana or Hangul handed
/// out as well as each pair when `chars` is set (`TextIndexSpec::chars`).
///
/// `f` is a trait object: generic over it, the tokenizer was compiled once
/// for every caller -- insert, remove and search, with prefixes and without
/// -- six copies and 8 KB of the browser module.
///
/// Each term comes with where it stands in `text`, its first byte and the
/// one past its last: what highlighting marks (`highlight.rs`), so that a
/// marked span is exactly the text a term the index matched was read from.
/// The index passes them over.
pub(crate) fn terms(text: &str, chars: bool, f: &mut dyn FnMut(&str, usize, usize)) {
    let mut buf = String::new();
    // U+0307 is not alphanumeric, so a plain `is_alphanumeric` split would cut
    // `I\u{307}stanbul` -- the decomposed spelling of `İstanbul`, and what the
    // default mapping leaves behind -- into two terms. Keep it inside the
    // word; it is dropped below. So are a Thai or a Khmer word's marks, some
    // of which are not letters: split at them, `ไม่` lost its tone mark.
    for raw in text.split(splits_words) {
        if raw.is_empty() || raw.chars().all(|c| c == '\u{0307}') {
            continue;
        }
        // `split` hands out slices of `text`, so where one starts is how far
        // its pointer is past the text's.
        let base = raw.as_ptr() as usize - text.as_ptr() as usize;
        // Already-lowercase ASCII is its own answer -- `i` folds to `i`.
        if raw.is_ascii() && !raw.bytes().any(|b| b.is_ascii_uppercase()) {
            f(raw, base, base + raw.len());
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
            // Checked, though `end` is a boundary: `split_at`'s panic formats
            // the character it would cut into.
            let (run, after) = rest.split_at_checked(end).unwrap_or((rest, ""));
            let at = base + (raw.len() - rest.len());
            rest = after;
            if n > 0 {
                grams(run, at, n, chars && n == 2, f);
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
            f(&buf, at, at + run.len());
        }
    }
}

/// Every overlapping run of `n` characters in `run` -- the whole of it when it
/// is shorter -- and each character as well with `chars`. The starts of the
/// last `n` characters are held in a ring, so a run allocates nothing.
/// `at` is where the run starts in the text the spans are of.
fn grams(run: &str, at: usize, n: usize, chars: bool, f: &mut dyn FnMut(&str, usize, usize)) {
    let mut starts = [0usize; 3];
    let mut seen = 0;
    for (i, c) in run.char_indices() {
        let end = i + c.len_utf8();
        // `get`: the ends are the characters' own, and an index would keep
        // its panic's formatting of a `char` in the browser module.
        if chars {
            f(run.get(i..end).unwrap_or(""), at + i, at + end);
        }
        starts[seen % n] = i;
        seen += 1;
        if seen >= n {
            // The oldest of the last `n` characters: the one `n` back.
            let from = starts[seen % n];
            f(run.get(from..end).unwrap_or(""), at + from, at + end);
        }
    }
    if seen < n && !(chars && seen == 1) {
        f(run, at, at + run.len());
    }
}

/// Where text is cut into words: at what is neither a letter nor a digit,
/// a stray dot above or a mark a word of a script holds.
pub(crate) fn splits_words(c: char) -> bool {
    !c.is_alphanumeric() && c != '\u{0307}' && !marked(c)
}

/// How a run of `c`'s script is indexed: in overlapping runs of 2
/// characters for Han, kana and Hangul, whose characters are syllables or
/// words, and of 3 for Thai, Lao, Khmer and Myanmar, whose are letters; 0
/// for a script that spaces its words, whose words are the terms.
pub(crate) fn gram(c: char) -> usize {
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
    indexed_terms(text, spec, &mut |t, _, _| f(t));
}

/// [`for_each_indexed_term`], each term with the span of `text` it was read
/// from: a prefix the span of its whole word, which is what it matched.
pub(crate) fn indexed_terms(
    text: &str,
    spec: &TextIndexSpec,
    f: &mut dyn FnMut(&str, usize, usize),
) {
    let Some(prefixes) = spec.prefixes() else {
        terms(text, spec.chars, f);
        return;
    };
    let mut buf = String::new();
    terms(text, spec.chars, &mut |w, from, to| {
        f(w, from, to);
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
            f(&buf, from, to);
        }
    });
}

/// An inverted index with BM25 scoring.
///
/// Like the HNSW graph this is derived data -- it is built from the
/// documents by the first statement that reads it after an open. Unlike the
/// graph it is *not* persisted: building it measures 16 us per document
/// (SciFact, 85 ms for 5 183) against the graph's ~44 us. A second record
/// kind, and the validation path that would have to come with it, was not
/// worth ~2 s per 100 000 documents.
#[cfg(feature = "text")]
pub struct TextIndex {
    pub spec: TextIndexSpec,
    /// term -> postings, kept ascending by document id so `search` can merge
    /// them without sorting ([`terms`]). Its table grows a shard at a time:
    /// a field whose rows bring terms of their own -- an id, a code, a name
    /// -- took its doublings whole under the write lock, 125-132 ms at 1.8
    /// million terms and 276-324 at 3.7 million.
    postings: Terms,
    /// document -> term count, for the length normalisation.
    /// Each document's number of terms: none is indexed without one, so
    /// 0 is none.
    lengths: DocMap,
    total_terms: u64,
    /// The words in order, for `fuzzy` and `prefix`: sorted by the first
    /// `match` that asks either, kept up by the writes from then on, as a
    /// derived index is ([`crate::fuzzy`]). An index no `match` reads
    /// loosely never makes it.
    words: Lazy<Dictionary>,
    /// The most terms a document has held: the longest a document can be,
    /// below which a word's own term outscores its variants (`Reach`).
    /// Only ever raised, as `Postings::max_tf` is.
    longest: u32,
}

#[cfg(feature = "text")]
impl TextIndex {
    pub fn new(spec: TextIndexSpec) -> TextIndex {
        TextIndex {
            spec,
            postings: Terms::default(),
            lengths: DocMap::default(),
            total_terms: 0,
            words: Lazy::new(),
            longest: 0,
        }
    }

    /// The words in order, sorted the first time they are asked for.
    fn dictionary(&self) -> &Dictionary {
        self.words
            .get_or_init(|| Dictionary::of_bytes(self.postings.words()))
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
        self.postings.postings()
    }

    /// Postings, the term dictionary and the length table, as they sit in
    /// memory -- each list's arrays with what an allocator takes past
    /// them -- and the sorted words once a `match` has asked for them. Not
    /// RSS, the same caveat as `Database::memory_bytes`, but within a tenth
    /// of what musl's allocator holds for them (`make text-memory`), where
    /// the count of a map, its table and its allocations left out, was a
    /// third of it for a tenant's notes and four fifths for FiQA.
    pub fn memory_bytes(&self) -> usize {
        self.postings.bytes() + self.lengths.bytes() + self.words.get().map_or(0, Dictionary::bytes)
    }

    /// Each of the document's terms goes straight into its list, a word at
    /// a time. Counted first in a map of the document's own, with a `String`
    /// for each term, SciFact's 5 183 documents took 160 ms to index against
    /// 85, and 20 000 short texts 21.9 ms in the browser module against 8.0.
    pub fn insert(&mut self, doc: DocId, text: &str) {
        let spec = self.spec;
        let mut n = 0u32;
        for_each_indexed_term(text, &spec, |t| {
            n += 1;
            if self.postings.bump(t, doc) {
                if let Some(words) = self.words.get_mut() {
                    words.add(t);
                }
            }
        });
        if n == 0 {
            return;
        }
        self.longest = self.longest.max(n);
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
            if self.postings.drop_doc(t, doc) {
                if let Some(words) = self.words.get_mut() {
                    words.remove(t);
                }
            }
        });
        if let Some(n) = self.lengths.remove(doc) {
            self.total_terms -= n as u64;
        }
    }

    /// Gives back the growth slack of every postings list, of the table
    /// and of the arrays the terms are in.
    ///
    /// `Vec` grows by doubling, so a list built one document at a time sits
    /// at ~1.5x the bytes it needs -- 3.8 MB of the 13.0 MB SciFact index.
    /// That slack is the price of cheap appends and it is worth paying while
    /// documents are still arriving; it is not worth paying afterwards.
    /// Called where the index is known to be complete: its build from the
    /// documents, and `create index`.
    pub fn shrink_to_fit(&mut self) {
        self.postings.shrink_to_fit();
        self.lengths.shrink_to_fit();
    }

    pub fn clear(&mut self) {
        self.postings = Terms::default();
        self.lengths.clear();
        self.total_terms = 0;
        self.words = Lazy::new();
        self.longest = 0;
    }

    fn avgdl(&self) -> f32 {
        if self.lengths.len() == 0 {
            return 1.0;
        }
        (self.total_terms as f32 / self.lengths.len() as f32).max(1.0)
    }

    /// How many documents BM25 counts and their mean length: the index's,
    /// or those of `within` (ascending), each length summed as the index
    /// sums its own, so a set scores as an index holding it alone would.
    fn counted(&self, within: Option<&[DocId]>) -> (usize, f32) {
        let Some(w) = within else {
            return (self.lengths.len(), self.avgdl());
        };
        let total: u64 = w.iter().map(|&d| self.lengths.get(d) as u64).sum();
        let mean = match w.len() {
            0 => 1.0,
            n => (total as f32 / n as f32).max(1.0),
        };
        (w.len(), mean)
    }

    /// Every document holding a term of `query`, ascending: the set a
    /// `match` ranks, which `facet` counts over. The lists are ascending
    /// already, so one alone is copied as it is; more are put together
    /// through the engine's one sort of ids.
    pub fn matching(&self, query: &str) -> Vec<DocId> {
        self.matching_loose(query, Loose::default())
            .unwrap_or_default()
    }

    /// [`Self::matching`] with the query's words read loosely: every
    /// document holding a term of the query or a variant of one of its
    /// words, what `facet` counts beside a loose `match`.
    pub fn matching_loose(&self, query: &str, loose: Loose) -> Result<Vec<DocId>> {
        let mut lists: Vec<Postings> = Vec::new();
        for_each_indexed_term(query, &self.spec, |t| {
            if let Some(list) = self.postings.get(t) {
                if !lists.iter().any(|l| l.docs.as_ptr() == list.docs.as_ptr()) {
                    lists.push(list);
                }
            }
        });
        if !loose.is_plain() {
            // A variant is no term of the query's own, so no list twice.
            for r in self.reach(query, loose, &self.query_terms(query))? {
                lists.extend(r.found.iter().map(|f| f.2));
            }
        }
        let mut out: Vec<DocId> = Vec::new();
        for l in &lists {
            out.extend_from_slice(l.docs);
        }
        if lists.len() > 1 {
            out.sort_unstable();
            out.dedup();
        }
        Ok(out)
    }

    /// Top `k` documents for `query`, best first.
    ///
    /// `accept` is the filter membership test; it is applied while the
    /// postings are merged, so a `where` clause never costs a second pass.
    pub fn search<F>(&self, query: &str, k: usize, accept: F) -> Vec<(DocId, f32)>
    where
        F: Fn(DocId) -> bool,
    {
        self.search_within(query, k, accept, None)
    }

    /// [`Self::search`], BM25's statistics taken over `within` (ascending)
    /// when it is given: its documents counted, their mean length, and each
    /// term's documents among them. A scoped token's `match` is scored so
    /// ([`crate::query::Match::within`]): over the collection a score said
    /// how many rows the token could not read held a term. A term none of
    /// them holds is passed over, its list unwalked.
    pub fn search_within<F>(
        &self,
        query: &str,
        k: usize,
        accept: F,
        within: Option<&[DocId]>,
    ) -> Vec<(DocId, f32)>
    where
        F: Fn(DocId) -> bool,
    {
        if k == 0 || self.lengths.len() == 0 {
            return Vec::new();
        }
        // The browser holds no scoped token to ask for it: left in, the
        // counting was 151 bytes brotli of its module.
        let within = within.filter(|_| cfg!(not(target_arch = "wasm32")));
        let qtf = self.query_terms(query);
        let (n, avgdl) = self.counted(within);
        let mut cursors: Vec<Cursor> = Vec::with_capacity(qtf.len());
        for (term, count) in &qtf {
            if let Some(list) = self.postings.get(term) {
                cursors.extend(self.cursor(list, *count as f32, n, within));
            }
        }
        self.ranked(cursors, k, accept, avgdl)
    }

    /// The terms of `query` as the index reads them, each with how often
    /// the query holds it. Repeated query terms fold into a weight: scoring
    /// the same postings list twice would give the same answer for twice
    /// the walk.
    fn query_terms(&self, query: &str) -> Map<String, u32> {
        let mut qtf: Map<String, u32> = Map::default();
        for_each_indexed_term(query, &self.spec, |t| match qtf.get_mut(t) {
            Some(c) => *c += 1,
            None => {
                qtf.insert(t.to_string(), 1);
            }
        });
        qtf
    }

    /// A walk through `list` weighted `times` its idf, or none where
    /// `within` holds none of its documents and the term is passed over,
    /// its list unwalked.
    fn cursor<'a>(
        &self,
        list: Postings<'a>,
        times: f32,
        n: usize,
        within: Option<&[DocId]>,
    ) -> Option<Cursor<'a>> {
        let df = match within {
            None => list.len(),
            Some(w) => held_by(list.docs, w),
        };
        if df == 0 {
            return None;
        }
        let (k1, b) = (self.spec.k1(), self.spec.b());
        let weight = times * idf(n, df);
        // The most this term can ever add to a document: its largest `tf`,
        // against the most generous length normalisation there is (`dl` ->
        // 0). An over-estimate is what makes it safe.
        let tf = list.max_tf as f32;
        Some(Cursor {
            at: 0,
            list,
            weight,
            ceiling: weight * (tf * (k1 + 1.0)) / (tf + k1 * (1.0 - b)),
        })
    }

    /// [`Self::search_within`] with the query's words read loosely
    /// ([`Loose`]): each word's variants of one distance -- the words
    /// `fuzzy` finds within its edits, the words `prefix` finds the last
    /// word begins -- ranked as one more term, a list of the documents any
    /// of them holds, each document's counts summed ([`union`]). A
    /// document holding three spellings of a word scores as one holding
    /// the word three times, BM25's saturation and all, not three words
    /// over; and the term's idf is the union's, so a rare misspelling the
    /// corpus holds twice ranks no higher than the word it misspells.
    ///
    /// **A word's own term outranks its variants.** Their weights are
    /// scaled, where they would reach it, below the least the word itself
    /// adds to a document -- one occurrence in the longest document there
    /// is -- so of two documents, one holding a word of the query as typed
    /// and one only a variant of it, the first scores more from that word:
    /// a one-word query ranks every document holding it above every
    /// document holding only what is near it. A word no document holds
    /// as typed -- the typo, the half-typed word -- ranks by its variants
    /// alone: a completion of a prefix and a word one edit away at the
    /// whole weight, two edits away at a quarter (`NEAR_WEIGHT`).
    ///
    /// The variants are cursors as any term's, their ceilings their own,
    /// so MaxScore is as safe over them as over a plain query, which
    /// `pruning_never_changes_a_loose_answer` holds it to.
    pub fn search_loose<F>(
        &self,
        query: &str,
        k: usize,
        accept: F,
        within: Option<&[DocId]>,
        loose: Loose,
    ) -> Result<Vec<(DocId, f32)>>
    where
        F: Fn(DocId) -> bool,
    {
        if loose.is_plain() {
            return Ok(self.search_within(query, k, accept, within));
        }
        if k == 0 || self.lengths.len() == 0 {
            return Ok(Vec::new());
        }
        let within = within.filter(|_| cfg!(not(target_arch = "wasm32")));
        let qtf = self.query_terms(query);
        let reach = self.reach(query, loose, &qtf)?;
        let (n, avgdl) = self.counted(within);
        let unions = unions(&reach);
        let cursors = self.loose_cursors(&qtf, &reach, &unions, n, avgdl, within);
        Ok(self.ranked(cursors, k, accept, avgdl))
    }

    /// What each word of `query` reaches read loosely, the words the query
    /// holds as terms of its own left out; a word whose variants pass
    /// [`MAX_EXPANSIONS`] refuses the query.
    fn reach<'a>(
        &'a self,
        query: &str,
        loose: Loose,
        qtf: &Map<String, u32>,
    ) -> Result<Vec<Reach<'a>>> {
        let mut words: Vec<(String, u32)> = Vec::new();
        let mut last = 0;
        terms(query, self.spec.chars, &mut |t, _, _| {
            last = match words.iter().position(|w| w.0 == t) {
                Some(i) => {
                    words[i].1 += 1;
                    i
                }
                None => {
                    words.push((t.to_string(), 1));
                    words.len() - 1
                }
            };
        });
        let mut out = Vec::new();
        let mut near: Vec<(&str, u8)> = Vec::new();
        for (i, (w, count)) in words.iter().enumerate() {
            let k = typos(w.chars().count()).min(loose.fuzzy);
            let prefix = loose.prefix && i == last;
            if k == 0 && !prefix {
                continue;
            }
            near.clear();
            if !self
                .dictionary()
                .near(w, k, prefix, MAX_EXPANSIONS, &mut near)
            {
                let how = if prefix { "prefix" } else { "fuzzy" };
                return Err(Error::Query(format!(
                    "`match ... {how}` reads `{w}` as more than {MAX_EXPANSIONS} words of the \
                     index, and a word reaches {MAX_EXPANSIONS} at most: refused rather than cut"
                )));
            }
            let mut found = Vec::new();
            for &(t, d) in &near {
                if t == w || qtf.contains_key(t) {
                    continue;
                }
                if let Some(list) = self.postings.get(t) {
                    found.push((t, d, list));
                }
            }
            out.push(Reach {
                count: *count,
                own: self.postings.get(w),
                found,
            });
        }
        Ok(out)
    }

    /// The query's own terms' cursors, then each word's variants', a
    /// distance a cursor, scaled below the word's own (`search_loose`).
    #[allow(clippy::too_many_arguments)]
    fn loose_cursors<'a>(
        &'a self,
        qtf: &Map<String, u32>,
        reach: &[Reach<'a>],
        unions: &'a [List],
        n: usize,
        avgdl: f32,
        within: Option<&[DocId]>,
    ) -> Vec<Cursor<'a>> {
        let mut cursors: Vec<Cursor> = Vec::with_capacity(qtf.len() + 3 * reach.len());
        for (term, count) in qtf {
            if let Some(list) = self.postings.get(term) {
                cursors.extend(self.cursor(list, *count as f32, n, within));
            }
        }
        let (k1, b) = (self.spec.k1(), self.spec.b());
        let mut next = unions.iter().map(List::view);
        for r in reach {
            let mark = cursors.len();
            for (d, scale) in NEAR_WEIGHT.iter().enumerate() {
                let lists = r.of(d as u8);
                let list = match lists.len() {
                    0 => continue,
                    1 => lists[0],
                    _ => match next.next() {
                        Some(u) => u,
                        None => continue,
                    },
                };
                cursors.extend(self.cursor(list, r.count as f32 * scale, n, within));
            }
            let Some(own) = r
                .own
                .and_then(|l| self.cursor(l, r.count as f32, n, within))
            else {
                continue;
            };
            // The least the word's own term adds to a document holding it:
            // once, in the longest document there is. A hundredth below it
            // survives the scores' rounding to `f32`.
            let longest = self.longest.max(1) as f32;
            let floor =
                0.99 * own.weight * (k1 + 1.0) / (1.0 + k1 * (1.0 - b + b * longest / avgdl));
            let most: f32 = cursors[mark..].iter().map(|c| c.ceiling).sum();
            if most > floor {
                for c in &mut cursors[mark..] {
                    c.weight *= floor / most;
                    c.ceiling *= floor / most;
                }
            }
        }
        cursors
    }

    /// The words `query` reaches read loosely besides its own terms: what
    /// `highlight()` marks beside them.
    pub fn loose_words(&self, query: &str, loose: Loose) -> Result<Vec<&str>> {
        if loose.is_plain() {
            return Ok(Vec::new());
        }
        let qtf = self.query_terms(query);
        let reach = self.reach(query, loose, &qtf)?;
        let mut out = Vec::new();
        for r in &reach {
            for f in &r.found {
                out.push(f.0);
            }
        }
        Ok(out)
    }

    /// The first `k` documents `cursors` reach, best first. Natively in
    /// line with each caller, as the walk was before a loose `match` shared
    /// it: called, FiQA's `match` took 0.261 -> 0.269 ms p50; in line, 0.257.
    /// The browser module's `match` moved by noise alone either way, so it
    /// keeps one copy.
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    fn ranked<F>(&self, cursors: Vec<Cursor>, k: usize, accept: F, avgdl: f32) -> Vec<(DocId, f32)>
    where
        F: Fn(DocId) -> bool,
    {
        if cursors.is_empty() {
            return Vec::new();
        }
        let (k1, b) = (self.spec.k1(), self.spec.b());

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
        let mut cursors = by_ceiling(&cursors);
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

/// One word of a loose `match`'s query and what it reaches.
#[cfg(feature = "text")]
struct Reach<'a> {
    /// How often the query holds the word.
    count: u32,
    /// The word's own list, where the index holds it.
    own: Option<Postings<'a>>,
    /// Each variant, its distance and its list.
    found: Vec<(&'a str, u8, Postings<'a>)>,
}

#[cfg(feature = "text")]
impl<'a> Reach<'a> {
    /// The lists of its variants `d` edits away.
    fn of(&self, d: u8) -> Vec<Postings<'a>> {
        self.found
            .iter()
            .filter(|f| f.1 == d)
            .map(|f| f.2)
            .collect()
    }
}

/// Each word's variants of one distance as one list where there are
/// several, in the order [`TextIndex::loose_cursors`] takes them.
#[cfg(feature = "text")]
fn unions(reach: &[Reach]) -> Vec<List> {
    let mut out = Vec::new();
    for r in reach {
        for d in 0..NEAR_WEIGHT.len() as u8 {
            let lists = r.of(d);
            if lists.len() > 1 {
                out.push(union(&lists));
            }
        }
    }
    out
}

/// The documents any of `lists` holds, ascending, each with its counts
/// there summed: merged two at a time, a level at a time, so a word
/// reaching `n` lists costs the postings `log n` times over.
#[cfg(feature = "text")]
fn union(lists: &[Postings]) -> List {
    let none = List::default();
    let mut level: Vec<List> = Vec::with_capacity(lists.len().div_ceil(2));
    for p in lists.chunks(2) {
        level.push(merged(p[0], p.get(1).copied().unwrap_or(none.view())));
    }
    while level.len() > 1 {
        let mut up = Vec::with_capacity(level.len().div_ceil(2));
        for p in level.chunks(2) {
            up.push(merged(p[0].view(), p.get(1).unwrap_or(&none).view()));
        }
        level = up;
    }
    level.pop().unwrap_or_default()
}

#[cfg(feature = "text")]
fn merged(a: Postings, b: Postings) -> List {
    let mut out = List::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    loop {
        let (doc, tf) = match (a.docs.get(i), b.docs.get(j)) {
            (Some(&x), Some(&y)) if x == y => {
                i += 1;
                j += 1;
                (x, a.tfs[i - 1] + b.tfs[j - 1])
            }
            (Some(&x), Some(&y)) if x < y => {
                i += 1;
                (x, a.tfs[i - 1])
            }
            (Some(&x), None) => {
                i += 1;
                (x, a.tfs[i - 1])
            }
            (_, Some(&y)) => {
                j += 1;
                (y, b.tfs[j - 1])
            }
            (None, None) => break,
        };
        out.docs.push(doc);
        out.tfs.push(tf);
        out.max_tf = out.max_tf.max(tf);
    }
    out
}

/// Robertson/Sparck-Jones idf of a term `df` of `n` documents hold, the
/// `ln(1 + ...)` form: always positive, so a term occurring in every
/// document contributes ~0 rather than pushing the score negative the way
/// the unsmoothed form does.
fn idf(n: usize, df: usize) -> f32 {
    let (n, df) = (n as f32, df as f32);
    (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
}

/// How many of `docs` are in `within`, both ascending: the shorter walked,
/// each looked for in the longer from where the last was found -- `|short|
/// log |long|`, which for a user's rows against a common term's list is a
/// few hundred steps.
fn held_by(docs: &[DocId], within: &[DocId]) -> usize {
    let (short, long) = if docs.len() <= within.len() {
        (docs, within)
    } else {
        (within, docs)
    };
    let mut at = 0;
    let mut n = 0;
    for d in short {
        at += long[at..].partition_point(|x| x < d);
        if long.get(at) == Some(d) {
            n += 1;
            at += 1;
        }
    }
    n
}

/// Highest score first, ties to the lower id. A function rather than a
/// closure so that every ranking sorted this way -- `fuse`'s too -- shares
/// one copy of the sort: a closure inside the generic `search` is a type of
/// its own for every `accept` it is given.
pub(crate) fn best_first(a: &(DocId, f32), b: &(DocId, f32)) -> Ordering {
    b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0))
}

/// `cursors` by ceiling, lowest first, ties in the order they came -- put in
/// that order by `best_first` as `(position, -ceiling)`, since the browser
/// module holds that sort anyway: a sort of `Cursor`s of its own was 4.2 KB
/// of it.
///
/// The vector index's candidates keep theirs: this way a build of 30 000 x
/// 128 took 2.5% longer, for 0.4 KB brotli.
#[cfg(feature = "text")]
fn by_ceiling<'a>(cursors: &[Cursor<'a>]) -> Vec<Cursor<'a>> {
    let mut order: Vec<(DocId, f32)> = cursors
        .iter()
        .enumerate()
        .map(|(i, c)| (i as DocId, -c.ceiling))
        .collect();
    order.sort_by(best_first);
    order.iter().map(|&(i, _)| cursors[i as usize]).collect()
}

/// One query term's walk through its postings.
#[cfg(feature = "text")]
#[derive(Clone, Copy)]
struct Cursor<'a> {
    at: usize,
    list: Postings<'a>,
    /// idf, times how often the term occurs in the query.
    weight: f32,
    /// The most this term can contribute to any single document.
    ceiling: f32,
}

#[cfg(feature = "text")]
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
        let mut qtf: Map<String, u32> = Map::default();
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
                let weight = *count as f32 * idf(self.lengths.len(), list.len());
                let tf = list.max_tf as f32;
                cursors.push(Cursor {
                    at: 0,
                    list,
                    weight,
                    ceiling: weight * (tf * (k1 + 1.0)) / (tf + k1 * (1.0 - b)),
                });
            }
        }
        self.ranked_unpruned(cursors, k, accept, avgdl)
    }

    /// [`Self::search_loose`] with the pruning switched off: the same
    /// cursors, variants and all, every one driving the frontier.
    fn search_loose_unpruned<F>(
        &self,
        query: &str,
        k: usize,
        accept: F,
        loose: Loose,
    ) -> Vec<(DocId, f32)>
    where
        F: Fn(DocId) -> bool,
    {
        let qtf = self.query_terms(query);
        let reach = self.reach(query, loose, &qtf).unwrap();
        let (n, avgdl) = self.counted(None);
        let unions = unions(&reach);
        let cursors = self.loose_cursors(&qtf, &reach, &unions, n, avgdl, None);
        self.ranked_unpruned(cursors, k, accept, avgdl)
    }

    fn ranked_unpruned<F>(
        &self,
        cursors: Vec<Cursor>,
        k: usize,
        accept: F,
        avgdl: f32,
    ) -> Vec<(DocId, f32)>
    where
        F: Fn(DocId) -> bool,
    {
        if cursors.is_empty() {
            return Vec::new();
        }
        let (k1, b) = (self.spec.k1(), self.spec.b());
        let mut cursors = by_ceiling(&cursors);
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

    /// A dictionary past the split -- its terms in shards -- answers as the
    /// one map did: each term's documents, a term gone with its last one,
    /// the same ranking through a shrink, and empty again once cleared.
    #[test]
    fn a_dictionary_in_shards_answers_as_one_did() {
        let text = |d: u64| format!("t{d}a t{d}b t{}c shared", d / 2);
        let mut ix = TextIndex::new(spec());
        let n = 60_000u64;
        for d in 0..n {
            ix.insert(d, &text(d));
        }
        assert_eq!(ix.terms(), 2 * n as usize + n as usize / 2 + 1);
        for d in (0..n).step_by(97) {
            assert_eq!(ix.matching(&format!("t{d}a")), [d]);
            assert_eq!(ix.matching(&format!("t{}c", d / 2)), [d & !1, d | 1]);
        }
        for d in (0..n).step_by(3) {
            ix.remove(d, &text(d));
        }
        assert_eq!(ix.matching("t3a t4a t6b"), [4]);
        let pair = ix.search("t5c t9b", 10, |_| true);
        ix.shrink_to_fit();
        assert_eq!(ix.search("t5c t9b", 10, |_| true), pair);
        assert_eq!(ix.postings_count(), 4 * ix.len());
        ix.clear();
        assert_eq!((ix.terms(), ix.len()), (0, 0));
        ix.insert(1, "alpha");
        assert_eq!(ix.matching("alpha"), [1]);
    }

    /// The count `memory_bytes` reads is what the terms hold -- each
    /// shard's count of its lists their walk (`Terms::check`) -- through
    /// inserts, updates, removals and a shrink, and the index answers as
    /// one built afresh from the documents left.
    #[test]
    fn the_byte_count_is_the_walk_it_replaced() {
        let walked = |ix: &TextIndex| {
            ix.postings.check();
            ix.postings.bytes() + ix.lengths.bytes()
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
        let mut fresh = TextIndex::new(spec());
        for (id, text) in &docs {
            fresh.insert(*id, text);
        }
        for q in ["w1", "w2 w3", "w7 w7 w30", "w39 w0 w12 w5"] {
            assert_eq!(ix.search(q, 50, |_| true), fresh.search(q, 50, |_| true));
            assert_eq!(ix.matching(q), fresh.matching(q));
        }
        assert_eq!(ix.postings_count(), fresh.postings_count());
        for (id, text) in docs {
            ix.remove(id, &text);
        }
        assert_eq!(ix.terms(), 0);
        ix.shrink_to_fit();
        assert_eq!(ix.memory_bytes(), walked(&ix));
        assert_eq!(ix.postings.bytes(), 0);
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
        let list = ix.postings.get("term").unwrap();
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

    fn fuzzy() -> Loose {
        Loose {
            fuzzy: 2,
            prefix: false,
        }
    }

    fn ids(hits: &[(DocId, f32)]) -> Vec<DocId> {
        hits.iter().map(|h| h.0).collect()
    }

    /// Words over five letters, five to eleven of them, each a third of the
    /// time a typo of one before it and a third that one with letters
    /// after: many are an edit or two from another, and many begin another.
    fn near_words(rng: &mut Rng, n: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(n);
        while out.len() < n {
            let w = match (out.is_empty(), rng.upto(3)) {
                (false, 0) => {
                    let w = out[rng.upto(out.len())].clone();
                    misspelt(rng, &w)
                }
                (false, 1) => {
                    let mut w = out[rng.upto(out.len())].clone();
                    w.extend((0..1 + rng.upto(3)).map(|_| b"abcde"[rng.upto(5)] as char));
                    w
                }
                _ => {
                    let len = 5 + rng.upto(7);
                    (0..len).map(|_| b"abcde"[rng.upto(5)] as char).collect()
                }
            };
            if !out.contains(&w) {
                out.push(w);
            }
        }
        out
    }

    /// A word with a typo at one place past the first: a letter dropped,
    /// doubled, changed, or swapped with the next.
    fn misspelt(rng: &mut Rng, w: &str) -> String {
        let mut c: Vec<char> = w.chars().collect();
        let at = 1 + rng.upto(c.len() - 1);
        match rng.upto(4) {
            0 => {
                c.remove(at);
            }
            1 => c.insert(at, c[at]),
            2 => c[at] = b"abcde"[rng.upto(5)] as char,
            _ if at + 1 < c.len() => c.swap(at, at + 1),
            _ => {}
        }
        c.into_iter().collect()
    }

    /// A word misspelt finds the documents holding it; a word begun finds
    /// the documents holding what it begins; plain, neither does.
    #[test]
    fn a_typo_and_a_word_begun_find_the_word() {
        let mut ix = TextIndex::new(spec());
        ix.insert(1, "a database of documents");
        ix.insert(2, "databases and their indexes");
        ix.insert(3, "nothing to see");
        let begun = Loose {
            fuzzy: 0,
            prefix: true,
        };
        let loose = |q: &str, l: Loose| ids(&ix.search_loose(q, 10, |_| true, None, l).unwrap());
        assert!(ix.search("databse", 10, |_| true).is_empty());
        // Seven letters take one edit: `databases` is two away.
        assert_eq!(loose("databse", fuzzy()), [1]);
        assert_eq!(loose("databasse", fuzzy()), [1, 2], "nine letters take two");
        assert_eq!(loose("datab", begun), [1, 2]);
        assert_eq!(loose("indexes datab", begun), [2, 1]);
        // Only the last word is a prefix.
        assert_eq!(loose("datab nothing", begun), [3]);
        // A word under five letters takes no edit; a typo in the first
        // letter counts two.
        assert!(loose("seee", fuzzy()).is_empty());
        let one = Loose {
            fuzzy: 1,
            prefix: false,
        };
        assert!(loose("tatabases", one).is_empty());
        assert_eq!(
            loose("tatabases", fuzzy()),
            [2],
            "two edits from nine letters"
        );
        // A misspelt word begun.
        let both = Loose {
            fuzzy: 2,
            prefix: true,
        };
        assert_eq!(loose("datbas", both), [1, 2]);
        assert_eq!(ix.matching_loose("databasse", fuzzy()).unwrap(), [1, 2]);
        let mut words = ix.loose_words("databasse indexes", fuzzy()).unwrap();
        words.sort_unstable();
        assert_eq!(words, ["database", "databases"]);
    }

    /// For a one-word query, every document holding the word as typed
    /// ranks above every document holding only a variant of it, however
    /// often and in however short a document -- over generated corpora,
    /// with `fuzzy`, `prefix` and both.
    #[test]
    fn a_word_as_typed_outranks_its_variants() {
        let mut checked = 0;
        for seed in 1..=60u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let n = 40 + rng.upto(80);
            let vocab = near_words(&mut rng, n);
            let mut ix = TextIndex::new(spec());
            let mut docs: Vec<Vec<&str>> = Vec::new();
            for id in 0..(30 + rng.upto(200)) as u64 {
                let len = 1 + rng.upto(40);
                let words: Vec<&str> = (0..len)
                    .map(|_| vocab[rng.upto(vocab.len())].as_str())
                    .collect();
                ix.insert(id, &words.join(" "));
                docs.push(words);
            }
            for _ in 0..20 {
                let w = &vocab[rng.upto(vocab.len())];
                let loose = Loose {
                    fuzzy: [0, 1, 2][rng.upto(3)],
                    prefix: rng.upto(2) == 0,
                };
                // Sometimes the word itself, sometimes a typo no document
                // may hold, sometimes a word begun.
                let q = match rng.upto(3) {
                    0 => w.clone(),
                    1 => misspelt(&mut rng, w),
                    _ => w.chars().take(2 + rng.upto(w.len() - 1)).collect(),
                };
                let hits = ix.search_loose(&q, 1_000, |_| true, None, loose).unwrap();
                let holds = |d: DocId| docs[d as usize].contains(&q.as_str());
                let first_variant = hits.iter().position(|h| !holds(h.0)).unwrap_or(hits.len());
                assert!(
                    hits[first_variant..].iter().all(|h| !holds(h.0)),
                    "seed {seed} {q:?} {loose:?}: {hits:?}"
                );
                if first_variant > 0 && first_variant < hits.len() {
                    assert!(hits[first_variant - 1].1 > hits[first_variant].1);
                    checked += 1;
                }
                // And every document holding it is found.
                let n = (0..docs.len() as DocId).filter(|&d| holds(d)).count();
                assert_eq!(first_variant, n, "seed {seed} {q:?}");
            }
        }
        assert!(checked > 100, "{checked}");
    }

    /// MaxScore over a loose query's cursors -- the variants' unions among
    /// them -- returns what the exhaustive walk over the same cursors does.
    #[test]
    fn pruning_never_changes_a_loose_answer() {
        let mut queries = 0;
        for seed in 1..=120u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let n = 30 + rng.upto(120);
            let vocab = near_words(&mut rng, n);
            let mut ix = TextIndex::new(spec());
            for id in 1..=(40 + rng.upto(160)) as u64 {
                let len = 3 + rng.upto(25);
                let words: Vec<&str> = (0..len)
                    .map(|_| match rng.upto(10) < 5 {
                        true => vocab[rng.upto(3)].as_str(),
                        false => vocab[rng.upto(vocab.len())].as_str(),
                    })
                    .collect();
                ix.insert(id, &words.join(" "));
            }
            for _ in 0..20 {
                let qlen = 1 + rng.upto(4);
                let q: Vec<String> = (0..qlen)
                    .map(|_| {
                        let w = &vocab[rng.upto(vocab.len())];
                        match rng.upto(3) {
                            0 => w.clone(),
                            1 => misspelt(&mut rng, w),
                            _ => w.chars().take(3).collect(),
                        }
                    })
                    .collect();
                let query = q.join(" ");
                let loose = Loose {
                    fuzzy: [0, 1, 2][rng.upto(3)],
                    prefix: rng.upto(2) == 0,
                };
                for k in [1usize, 3, 10, 50] {
                    let odd = rng.upto(4) == 0;
                    let accept = |d: DocId| !odd || d.is_multiple_of(2);
                    let want = ix.search_loose_unpruned(&query, k, accept, loose);
                    let got = ix.search_loose(&query, k, accept, None, loose).unwrap();
                    assert_eq!(
                        ids(&want),
                        ids(&got),
                        "seed {seed} k {k} {query:?} {loose:?}"
                    );
                    for (w, g) in want.iter().zip(&got) {
                        assert!((w.1 - g.1).abs() <= 1e-5 * w.1.abs().max(1.0));
                    }
                    queries += !want.is_empty() as usize;
                }
            }
        }
        assert!(queries > 5_000, "{queries}");
    }

    /// Past the bound a word is refused, never read as some of what it
    /// reaches.
    #[test]
    fn a_word_reaching_too_many_is_refused() {
        let mut ix = TextIndex::new(spec());
        for i in 0..MAX_EXPANSIONS as u64 {
            ix.insert(i, &format!("pre{i}"));
        }
        let prefix = Loose {
            fuzzy: 0,
            prefix: true,
        };
        let hits = ix.search_loose("pre", 5, |_| true, None, prefix).unwrap();
        assert_eq!(hits.len(), 5);
        ix.insert(MAX_EXPANSIONS as u64, "prez");
        let e = ix
            .search_loose("pre", 5, |_| true, None, prefix)
            .unwrap_err();
        assert!(e.to_string().contains("refused rather than cut"), "{e}");
        assert!(ix.matching_loose("pre", prefix).is_err());
        assert!(ix.search_loose("pre1", 5, |_| true, None, prefix).is_ok());
    }

    /// The sorted words, once a loose `match` has made them, are kept up
    /// by the writes: an index written to after it answers as one built
    /// from the documents as they stand.
    #[test]
    fn the_words_are_kept_up_by_the_writes() {
        for seed in 1..=30u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let vocab = near_words(&mut rng, 200);
            let text = |rng: &mut Rng| {
                (0..1 + rng.upto(6))
                    .map(|_| vocab[rng.upto(vocab.len())].as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let mut ix = TextIndex::new(spec());
            let mut docs: Vec<(DocId, String)> = Vec::new();
            for id in 0..50 {
                let t = text(&mut rng);
                ix.insert(id, &t);
                docs.push((id, t));
            }
            ix.search_loose("abcde", 10, |_| true, None, fuzzy())
                .unwrap();
            for round in 0..300u64 {
                let at = rng.upto(docs.len());
                if rng.upto(3) == 0 {
                    let (id, old) = docs.remove(at);
                    ix.remove(id, &old);
                } else {
                    let t = text(&mut rng);
                    ix.insert(1_000 + round, &t);
                    docs.push((1_000 + round, t));
                }
            }
            let mut fresh = TextIndex::new(spec());
            for (id, t) in &docs {
                fresh.insert(*id, t);
            }
            assert_eq!(
                ix.dictionary().len(),
                fresh.dictionary().len(),
                "seed {seed}"
            );
            let both = Loose {
                fuzzy: 1,
                prefix: true,
            };
            for _ in 0..20 {
                let w = &vocab[rng.upto(vocab.len())];
                let q = misspelt(&mut rng, w);
                for loose in [fuzzy(), both] {
                    let mut a = ix.loose_words(&q, loose).unwrap();
                    let mut b = fresh.loose_words(&q, loose).unwrap();
                    a.sort_unstable();
                    b.sort_unstable();
                    assert_eq!(a, b, "seed {seed} {q:?}");
                }
            }
        }
    }
}
