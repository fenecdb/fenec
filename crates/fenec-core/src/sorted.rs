//! The ordered index behind `@sorted`.
//!
//! A hash index answers `=`; everything else used to scan. Over a million
//! rows that made the two most common page shapes the slowest ones: the
//! latest twenty (`order created desc limit 20`, 26.8 ms, every key read and
//! partitioned) and a selective range (`where price >= 99900`, a full scan).
//! An ordered index answers both by walking its keys, and hands a range's ids
//! to the filtered `near` as its allowed set.
//!
//! It is derived data like the hash and text indexes: built from the
//! documents on open and on `create index`, maintained on write, never in the
//! file.
//!
//! Every answer has to be the one the scan gives, row for row. So a key
//! orders exactly as `Value::cmp_value` orders the field's values, a literal
//! the key space cannot express exactly sends the query back to the scan, and
//! ties keep the order the scan keeps them in, ascending id.

use crate::collate::Collation;
use crate::value::{DataType, DocId, Value};
use std::cmp::Ordering;
use std::ops::Bound;

/// A field's index: `int` and `timestamp` keys and `float` keys map onto
/// one ordered `u64` space; `text` keeps its string.
pub enum SortedIndex {
    Num(Ordered<u64>),
    Text(Ordered<Box<str>>),
}

/// The entries, and the rows that have no place among them.
///
/// `null` sorts before every value, so it is held apart rather than given a
/// key. `NaN` is held apart too: `cmp_value` finds it equal to everything,
/// so it matches every inclusive comparison and no strict one, and has no
/// position an order could agree on. Both sets use the key type's default as
/// a dummy key, so they are the same structure as the keys and cost no code
/// of their own.
pub struct Ordered<K> {
    keys: Chunked<(K, DocId)>,
    nulls: Chunked<(K, DocId)>,
    nans: Chunked<(K, DocId)>,
    /// Bytes the keys hold beyond their own size -- the text of a `text`
    /// key -- kept as a running sum: `memory_bytes` is read before every
    /// write under `--max-memory`, and must not walk the entries to answer.
    heap: usize,
}

impl<K: Ord + Clone + Default> Ordered<K>
where
    (K, DocId): Entry,
{
    fn new(coll: Option<Collation>) -> Self {
        Ordered {
            keys: Chunked::new(coll),
            nulls: Chunked::new(None),
            nans: Chunked::new(None),
            heap: 0,
        }
    }
}

/// Upper size of a chunk before it splits in two. Small, because a write to
/// a random key shifts half a chunk: over a million rows the load with two
/// ordered indexes measured 1050 ms at 2048, 900 ms at 512 and no better at
/// 128, and with ranges walked lazily the queries do not tell the sizes
/// apart.
const CHUNK: usize = 512;

/// A sorted sequence in chunks of at most `CHUNK` entries, each chunk a
/// sorted `Vec`, found by binary search over the chunks' first entries.
///
/// Not a `BTreeSet`: with the three it took, this index made the browser
/// module 75 KB larger (15 KB brotli), and nothing else in it pays for B-tree
/// code; on chunks the whole feature is 31 KB (7.6 KB brotli). Chunks cost a
/// binary search and a `Vec` shift of at most one chunk per write, and a range
/// is a run of slices.
struct Chunked<T> {
    chunks: Vec<Vec<T>>,
    len: usize,
    /// The order of a text field's collation (`collate tr`), where the
    /// entries' own is their bytes'.
    coll: Option<Collation>,
}

/// An entry of an index: a key and the id it belongs to, ordered by the key
/// and then the id. A text key orders in its field's collation when it has
/// one -- passed in rather than made a key type of its own, which would have
/// been a third copy of this module's code in the browser module. Public
/// only because [`Ordered`] is.
pub trait Entry: Ord {
    fn order(&self, other: &Self, coll: Option<Collation>) -> Ordering;
}

impl Entry for (u64, DocId) {
    fn order(&self, other: &Self, _: Option<Collation>) -> Ordering {
        self.cmp(other)
    }
}

impl Entry for (Box<str>, DocId) {
    fn order(&self, other: &Self, coll: Option<Collation>) -> Ordering {
        match coll {
            Some(c) => c.compare(&self.0, &other.0).then(self.1.cmp(&other.1)),
            None => self.cmp(other),
        }
    }
}

impl<T: Entry> Chunked<T> {
    fn new(coll: Option<Collation>) -> Self {
        Chunked {
            chunks: Vec::new(),
            len: 0,
            coll,
        }
    }

    fn cmp(&self, a: &T, b: &T) -> Ordering {
        a.order(b, self.coll)
    }

    /// From entries already sorted and unique.
    fn from_sorted(v: Vec<T>, coll: Option<Collation>) -> Self {
        let len = v.len();
        let mut chunks = Vec::with_capacity(len / CHUNK + 1);
        let mut it = v.into_iter();
        loop {
            let chunk: Vec<T> = it.by_ref().take(CHUNK / 2).collect();
            if chunk.is_empty() {
                break;
            }
            chunks.push(chunk);
        }
        Chunked { chunks, len, coll }
    }

    /// The chunk an entry belongs in: the last whose first entry is not
    /// above it.
    fn chunk_for(&self, x: &T) -> usize {
        self.chunks
            .partition_point(|c| self.cmp(&c[0], x) != Ordering::Greater)
            .saturating_sub(1)
    }

    fn insert(&mut self, x: T) -> bool {
        // A key that grows with time lands past the last entry: no search,
        // and the full chunk it passes stays full instead of being split in
        // half and never written again. A million creation times went in
        // in 7 ms instead of 90, in half the memory.
        if self.chunks.last().is_none_or(|c| {
            self.cmp(c.last().expect("chunks are never empty"), &x) == Ordering::Less
        }) {
            match self.chunks.last_mut() {
                Some(c) if c.len() < CHUNK => c.push(x),
                _ => self.chunks.push(vec![x]),
            }
            self.len += 1;
            return true;
        }
        let ci = self.chunk_for(&x);
        let Err(pos) = self.chunks[ci].binary_search_by(|e| self.cmp(e, &x)) else {
            return false;
        };
        // A full chunk splits before the insert, not after: past `CHUNK`
        // its `Vec` would double, and the half left behind would keep the
        // doubled buffer.
        let (ci, pos) = if self.chunks[ci].len() < CHUNK {
            (ci, pos)
        } else {
            let tail = self.chunks[ci].split_off(CHUNK / 2);
            self.chunks.insert(ci + 1, tail);
            if pos <= CHUNK / 2 {
                (ci, pos)
            } else {
                (ci + 1, pos - CHUNK / 2)
            }
        };
        self.chunks[ci].insert(pos, x);
        self.len += 1;
        true
    }

    fn remove(&mut self, x: &T) -> bool {
        if self.chunks.is_empty() {
            return false;
        }
        let ci = self.chunk_for(x);
        let coll = self.coll;
        let chunk = &mut self.chunks[ci];
        let Ok(pos) = chunk.binary_search_by(|e| e.order(x, coll)) else {
            return false;
        };
        chunk.remove(pos);
        if chunk.is_empty() {
            self.chunks.remove(ci);
        }
        self.len -= 1;
        true
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The position of the first entry above `x` when `past` is set, and of
    /// the first not below it otherwise. A flag rather than a closure: each
    /// closure was a search of its own in the browser module.
    fn seek(&self, x: &T, past: bool) -> (usize, usize) {
        let below = |e: &T| match self.cmp(e, x) {
            Ordering::Less => true,
            Ordering::Equal => past,
            Ordering::Greater => false,
        };
        let ci = self
            .chunks
            .partition_point(|c| below(c.last().expect("chunks are never empty")));
        if ci == self.chunks.len() {
            return (ci, 0);
        }
        (ci, self.chunks[ci].partition_point(below))
    }

    /// The entries within the bounds, in order, both ways.
    fn range(&self, lo: &Bound<T>, hi: &Bound<T>) -> impl DoubleEndedIterator<Item = &T> {
        let start = match lo {
            Bound::Included(x) => self.seek(x, false),
            Bound::Excluded(x) => self.seek(x, true),
            Bound::Unbounded => (0, 0),
        };
        let end = match hi {
            Bound::Included(x) => self.seek(x, true),
            Bound::Excluded(x) => self.seek(x, false),
            Bound::Unbounded => (self.chunks.len(), 0),
        };
        // Lazily, chunk by chunk: a walk that stops at a page of twenty must
        // not pay for every chunk the range spans. Collecting the slices first
        // measured 0.021 ms for `limit 20` at 256-entry chunks, against 0.001.
        let last = if end.1 == 0 { end.0 } else { end.0 + 1 };
        (start.0..last.max(start.0)).flat_map(move |c| {
            let chunk = &self.chunks[c];
            let from = if c == start.0 { start.1 } else { 0 };
            let to = if c == end.0 { end.1 } else { chunk.len() };
            &chunk[from.min(to)..to]
        })
    }

    fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.chunks.iter().flatten()
    }
}

/// A key a value sorts under, or where it goes instead.
enum Slot<K> {
    Key(K),
    Null,
    Nan,
}

/// A bound on the key space, from `where` comparisons.
#[derive(Clone, Debug, PartialEq)]
pub enum Key {
    Num(u64),
    Text(Box<str>),
}

/// The ids a range covers. `strict` says whether any of the comparisons that
/// made it was `<` or `>`, which decides whether the `NaN` rows match.
pub struct Range {
    pub lo: Bound<Key>,
    pub hi: Bound<Key>,
    pub strict: bool,
    /// The collation text bounds compare in: the field's.
    pub coll: Option<Collation>,
}

/// `int`/`timestamp` onto `u64`, order kept: flipping the sign bit puts the
/// negatives below the positives.
fn int_key(i: i64) -> u64 {
    (i as u64) ^ (1 << 63)
}

/// `float` onto `u64`, order kept: a negative has every bit flipped, a
/// positive only its sign. `-0.0` and `0.0` compare equal, so they share a
/// key. The caller keeps `NaN` out.
fn float_key(f: f64) -> u64 {
    let f = if f == 0.0 { 0.0 } else { f };
    let b = f.to_bits();
    if b >> 63 == 1 {
        !b
    } else {
        b | (1 << 63)
    }
}

impl SortedIndex {
    /// The types an ordered index is built for.
    pub fn supports(ty: &DataType) -> bool {
        matches!(
            ty,
            DataType::Int | DataType::Float | DataType::Timestamp | DataType::Text
        )
    }

    /// An empty index over a field of type `ty`, its text in `coll` when
    /// the field names one.
    pub fn new(ty: &DataType, coll: Option<Collation>) -> SortedIndex {
        match ty {
            DataType::Text => SortedIndex::Text(Ordered::new(coll)),
            _ => SortedIndex::Num(Ordered::new(None)),
        }
    }

    /// Built in one pass from `(id, value)` pairs: sorted once and cut into
    /// chunks, rather than inserted one at a time. The rows come through
    /// `dyn` so that the two callers share one copy of this.
    pub fn build(
        ty: &DataType,
        coll: Option<Collation>,
        rows: &mut dyn Iterator<Item = (DocId, Option<Value>)>,
    ) -> Self {
        match ty {
            DataType::Text => {
                let (mut keys, mut nulls) = (Vec::new(), Vec::new());
                let mut heap = 0;
                for (id, v) in rows {
                    match text_slot(v.as_ref()) {
                        Slot::Key(k) => {
                            heap += k.len();
                            keys.push((k, id));
                        }
                        _ => nulls.push((Box::default(), id)),
                    }
                }
                // One closure for both: each closure is a sort of its own,
                // and a sort of these pairs is 4.5 KB of the browser module.
                let by = |a: &(Box<str>, DocId), b: &(Box<str>, DocId)| a.order(b, coll);
                keys.sort_unstable_by(by);
                nulls.sort_unstable_by(by);
                SortedIndex::Text(Ordered::from_sorted(keys, nulls, Vec::new(), heap, coll))
            }
            _ => {
                let (mut keys, mut nulls, mut nans) = (Vec::new(), Vec::new(), Vec::new());
                for (id, v) in rows {
                    match num_slot(v.as_ref()) {
                        Slot::Key(k) => keys.push((k, id)),
                        Slot::Null => nulls.push((0, id)),
                        Slot::Nan => nans.push((0, id)),
                    }
                }
                for v in [&mut keys, &mut nulls, &mut nans] {
                    radix_sort(v);
                }
                SortedIndex::Num(Ordered::from_sorted(keys, nulls, nans, 0, None))
            }
        }
    }

    pub fn insert(&mut self, id: DocId, v: Option<&Value>) {
        match self {
            SortedIndex::Num(o) => match num_slot(v) {
                Slot::Key(k) => {
                    o.keys.insert((k, id));
                }
                Slot::Null => {
                    o.nulls.insert((0, id));
                }
                Slot::Nan => {
                    o.nans.insert((0, id));
                }
            },
            SortedIndex::Text(o) => match text_slot(v) {
                Slot::Key(k) => {
                    let len = k.len();
                    if o.keys.insert((k, id)) {
                        o.heap += len;
                    }
                }
                _ => {
                    o.nulls.insert((Box::default(), id));
                }
            },
        }
    }

    /// Takes out the entry `insert` put in for the same value -- every
    /// caller reads the stored document first, as the other indexes do.
    pub fn remove(&mut self, id: DocId, v: Option<&Value>) {
        match self {
            SortedIndex::Num(o) => match num_slot(v) {
                Slot::Key(k) => {
                    o.keys.remove(&(k, id));
                }
                Slot::Null => {
                    o.nulls.remove(&(0, id));
                }
                Slot::Nan => {
                    o.nans.remove(&(0, id));
                }
            },
            SortedIndex::Text(o) => match text_slot(v) {
                Slot::Key(k) => {
                    let len = k.len();
                    if o.keys.remove(&(k, id)) {
                        o.heap -= len;
                    }
                }
                _ => {
                    o.nulls.remove(&(Box::default(), id));
                }
            },
        }
    }

    /// Whether any row holds `NaN`, which has no place in an order.
    pub fn has_nan(&self) -> bool {
        match self {
            SortedIndex::Num(o) => !o.nans.is_empty(),
            SortedIndex::Text(_) => false,
        }
    }

    /// Allocated bytes, estimated: chunks split when full, so they run
    /// between half full and full, counted here as a quarter again the
    /// entries' own size.
    pub fn memory_bytes(&self) -> usize {
        use std::mem::size_of;
        match self {
            SortedIndex::Num(o) => o.entries() * size_of::<(u64, DocId)>() * 5 / 4,
            SortedIndex::Text(o) => o.entries() * size_of::<(Box<str>, DocId)>() * 5 / 4 + o.heap,
        }
    }

    /// The key a literal compares as against this field, when the key space
    /// can say it exactly -- the same answer `cmp_value` gives. `None` means
    /// the comparison has to be evaluated row by row.
    pub fn bound(ty: &DataType, v: &Value) -> Option<Key> {
        match (ty, v) {
            (DataType::Int, Value::Int(i)) => Some(Key::Num(int_key(*i))),
            (DataType::Timestamp, Value::Timestamp(ms)) => Some(Key::Num(int_key(*ms))),
            // `cmp_value` meets a timestamp and an int as two f64s, exact
            // only up to 2^53.
            (DataType::Timestamp, Value::Int(ms)) if ms.unsigned_abs() <= 1 << 53 => {
                Some(Key::Num(int_key(*ms)))
            }
            (DataType::Timestamp, Value::Text(t)) => {
                crate::time::parse(t).ok().map(|ms| Key::Num(int_key(ms)))
            }
            // `cmp_value` compares an int against a float as two f64s, and so
            // does this key.
            (DataType::Float, Value::Float(f)) if !f.is_nan() => Some(Key::Num(float_key(*f))),
            (DataType::Float, Value::Int(i)) => Some(Key::Num(float_key(*i as f64))),
            (DataType::Text, Value::Text(t)) => Some(Key::Text(t.as_str().into())),
            _ => None,
        }
    }

    /// Every id in the range, in key order, and the `NaN` rows when every
    /// comparison was inclusive. `None` once more than `cap` ids turn up: a
    /// range that wide is cheaper to scan than to collect and sort.
    pub fn range_ids(&self, r: &Range, cap: usize) -> Option<Vec<DocId>> {
        let mut out = Vec::new();
        let mut take = |ids: &mut dyn Iterator<Item = DocId>| {
            for id in ids {
                if out.len() >= cap {
                    return false;
                }
                out.push(id);
            }
            true
        };
        let whole = match self {
            SortedIndex::Num(o) => {
                let nans = if r.strict { None } else { Some(&o.nans) };
                let (lo, hi) = (num_bound(&r.lo, true), num_bound(&r.hi, false));
                nans.is_none_or(|n| take(&mut n.iter().map(|e| e.1)))
                    && take(&mut o.keys.range(&lo, &hi).map(|e| e.1))
            }
            SortedIndex::Text(o) => {
                let (lo, hi) = (text_bound(&r.lo, true), text_bound(&r.hi, false));
                take(&mut o.keys.range(&lo, &hi).map(|e| e.1))
            }
        };
        whole.then_some(out)
    }

    /// Walks the ids in order -- `null` first ascending, last descending, as
    /// `cmp_value` puts it below every value -- within `range` when one is
    /// given, calling `emit` until it returns `false`.
    ///
    /// Ties come out in ascending id both ways, because that is the order the
    /// scan leaves them in. Walking the entries backwards gives equal keys in
    /// descending id, so a descending walk gathers each run of equal keys and
    /// hands it over reversed.
    ///
    /// The caller checks `has_nan` first: this walk has no place for them.
    pub fn walk(
        &self,
        desc: bool,
        range: Option<&Range>,
        mut emit: impl FnMut(DocId) -> crate::error::Result<bool>,
    ) -> crate::error::Result<()> {
        match self {
            SortedIndex::Num(o) => {
                let bounds = range.map(|r| (num_bound(&r.lo, true), num_bound(&r.hi, false)));
                walk_ordered(o, bounds, desc, &mut emit)
            }
            SortedIndex::Text(o) => {
                let bounds = range.map(|r| (text_bound(&r.lo, true), text_bound(&r.hi, false)));
                walk_ordered(o, bounds, desc, &mut emit)
            }
        }
    }
}

impl<K: Ord + Clone + Default> Ordered<K>
where
    (K, DocId): Entry,
{
    fn from_sorted(
        keys: Vec<(K, DocId)>,
        nulls: Vec<(K, DocId)>,
        nans: Vec<(K, DocId)>,
        heap: usize,
        coll: Option<Collation>,
    ) -> Self {
        Ordered {
            keys: Chunked::from_sorted(keys, coll),
            nulls: Chunked::from_sorted(nulls, None),
            nans: Chunked::from_sorted(nans, None),
            heap,
        }
    }

    fn entries(&self) -> usize {
        self.keys.len + self.nulls.len + self.nans.len
    }
}

/// The two ends of a range in the space the entries are ordered by.
type Ends<K> = (Bound<(K, DocId)>, Bound<(K, DocId)>);

fn walk_ordered<K: Ord + Clone + Default>(
    o: &Ordered<K>,
    bounds: Option<Ends<K>>,
    desc: bool,
    emit: &mut impl FnMut(DocId) -> crate::error::Result<bool>,
) -> crate::error::Result<()>
where
    (K, DocId): Entry,
{
    // A range never matches `null`, so only a whole walk visits those rows.
    let with_nulls = bounds.is_none();
    let (lo, hi) = bounds.unwrap_or((Bound::Unbounded, Bound::Unbounded));
    if !desc {
        if with_nulls {
            for (_, id) in o.nulls.iter() {
                if !emit(*id)? {
                    return Ok(());
                }
            }
        }
        for (_, id) in o.keys.range(&lo, &hi) {
            if !emit(*id)? {
                return Ok(());
            }
        }
        return Ok(());
    }
    let mut run: Vec<DocId> = Vec::new();
    let mut run_key: Option<&K> = None;
    for (k, id) in o.keys.range(&lo, &hi).rev() {
        if run_key != Some(k) {
            for id in run.drain(..).rev() {
                if !emit(id)? {
                    return Ok(());
                }
            }
            run_key = Some(k);
        }
        run.push(*id);
    }
    for id in run.drain(..).rev() {
        if !emit(id)? {
            return Ok(());
        }
    }
    if with_nulls {
        for (_, id) in o.nulls.iter() {
            if !emit(*id)? {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Sorts `(key, id)` pairs with an LSD radix over their sixteen bytes,
/// skipping every byte all the entries share -- the high bytes of the ids,
/// and of keys spread over a narrow span.
///
/// Rows mostly arrive in id order, and that is worth two shortcuts: a key
/// that grows with the id, a creation time, arrives sorted already, and
/// otherwise the passes over the key bytes alone keep ties in id order,
/// since each pass is stable. Without the first, a million creation times
/// took 50 ms longer to reopen than under `sort_unstable`, which notices a
/// sorted run.
///
/// Not `sort_unstable` itself: its copy for this pair type was 4.5 KB of
/// the browser module, and this is the only place that sorts it.
fn radix_sort(v: &mut Vec<(u64, DocId)>) {
    if v.is_sorted() {
        return;
    }
    let by_id = v.is_sorted_by_key(|e| e.1);
    let n = v.len();
    let mut counts = vec![[0usize; 256]; 16];
    for &(k, id) in v.iter() {
        for b in 0..8 {
            counts[b][(id >> (8 * b)) as u8 as usize] += 1;
            counts[8 + b][(k >> (8 * b)) as u8 as usize] += 1;
        }
    }
    let mut spare = vec![(0, 0); n];
    for (pass, count) in counts.iter().enumerate() {
        if count.contains(&n) || (by_id && pass < 8) {
            continue;
        }
        let mut at = [0usize; 256];
        let mut sum = 0;
        for (a, c) in at.iter_mut().zip(count) {
            *a = sum;
            sum += c;
        }
        let shift = 8 * (pass % 8);
        for &e in v.iter() {
            let b = ((if pass < 8 { e.1 } else { e.0 }) >> shift) as u8 as usize;
            spare[at[b]] = e;
            at[b] += 1;
        }
        std::mem::swap(v, &mut spare);
    }
}

fn num_slot(v: Option<&Value>) -> Slot<u64> {
    match v {
        Some(Value::Int(i) | Value::Timestamp(i)) => Slot::Key(int_key(*i)),
        Some(Value::Float(f)) if f.is_nan() => Slot::Nan,
        Some(Value::Float(f)) => Slot::Key(float_key(*f)),
        _ => Slot::Null,
    }
}

fn text_slot(v: Option<&Value>) -> Slot<Box<str>> {
    match v {
        Some(Value::Text(t)) => Slot::Key(t.as_str().into()),
        _ => Slot::Null,
    }
}

/// A bound in the tuple space the tree is ordered by. An id of 0 is below
/// every row with that key and `DocId::MAX` above it, so an inclusive bound
/// takes every row of its key and an exclusive one none.
fn num_bound(b: &Bound<Key>, lower: bool) -> Bound<(u64, DocId)> {
    match b {
        Bound::Included(Key::Num(k)) => Bound::Included((*k, if lower { 0 } else { DocId::MAX })),
        Bound::Excluded(Key::Num(k)) => Bound::Excluded((*k, if lower { DocId::MAX } else { 0 })),
        _ => Bound::Unbounded,
    }
}

fn text_bound(b: &Bound<Key>, lower: bool) -> Bound<(Box<str>, DocId)> {
    match b {
        Bound::Included(Key::Text(k)) => {
            Bound::Included((k.clone(), if lower { 0 } else { DocId::MAX }))
        }
        Bound::Excluded(Key::Text(k)) => {
            Bound::Excluded((k.clone(), if lower { DocId::MAX } else { 0 }))
        }
        _ => Bound::Unbounded,
    }
}

impl Range {
    /// No bounds yet: every value, and no `NaN` excluded, over a field
    /// whose text compares in `coll`.
    pub fn all(coll: Option<Collation>) -> Range {
        Range {
            lo: Bound::Unbounded,
            hi: Bound::Unbounded,
            strict: false,
            coll,
        }
    }

    /// Narrows the range by one comparison `field <op> key`.
    pub fn narrow(&mut self, op: crate::query::CmpOp, key: Key) {
        use crate::query::CmpOp::*;
        match op {
            Eq => {
                self.tighten_lo(Bound::Included(key.clone()));
                self.tighten_hi(Bound::Included(key));
            }
            Ge => self.tighten_lo(Bound::Included(key)),
            Gt => {
                self.strict = true;
                self.tighten_lo(Bound::Excluded(key));
            }
            Le => self.tighten_hi(Bound::Included(key)),
            Lt => {
                self.strict = true;
                self.tighten_hi(Bound::Excluded(key));
            }
            Ne => {}
        }
    }

    fn tighten_lo(&mut self, b: Bound<Key>) {
        if bound_cmp(&b, &self.lo, true, self.coll) == Ordering::Greater {
            self.lo = b;
        }
    }

    fn tighten_hi(&mut self, b: Bound<Key>) {
        if bound_cmp(&b, &self.hi, false, self.coll) == Ordering::Less {
            self.hi = b;
        }
    }
}

/// Orders two bounds of the same side by how much they admit: for a lower
/// bound the greater one admits less, and at an equal key an exclusive one
/// is the greater; for an upper bound the other way round.
fn bound_cmp(a: &Bound<Key>, b: &Bound<Key>, lower: bool, coll: Option<Collation>) -> Ordering {
    use std::cmp::Ordering::*;
    let key = |k: &Key, k2: &Key| match (k, k2) {
        (Key::Num(x), Key::Num(y)) => x.cmp(y),
        (Key::Text(x), Key::Text(y)) => coll.map_or_else(|| x.cmp(y), |c| c.compare(x, y)),
        // One field has one key type; mixed bounds never meet.
        _ => Equal,
    };
    let unbounded = if lower { Less } else { Greater };
    match (a, b) {
        (Bound::Unbounded, Bound::Unbounded) => Equal,
        (Bound::Unbounded, _) => unbounded,
        (_, Bound::Unbounded) => unbounded.reverse(),
        (Bound::Included(x), Bound::Included(y)) | (Bound::Excluded(x), Bound::Excluded(y)) => {
            key(x, y)
        }
        (Bound::Excluded(x), Bound::Included(y)) => {
            key(x, y).then(if lower { Greater } else { Less })
        }
        (Bound::Included(x), Bound::Excluded(y)) => {
            key(x, y).then(if lower { Less } else { Greater })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_keep_the_order_values_have() {
        let ints = [i64::MIN, -5, -1, 0, 1, 7, i64::MAX];
        for w in ints.windows(2) {
            assert!(int_key(w[0]) < int_key(w[1]), "{:?}", w);
        }
        let floats = [
            f64::NEG_INFINITY,
            -1e300,
            -2.5,
            -0.0,
            1e-300,
            2.5,
            f64::INFINITY,
        ];
        for w in floats.windows(2) {
            assert!(float_key(w[0]) < float_key(w[1]), "{:?}", w);
        }
        assert_eq!(float_key(-0.0), float_key(0.0));
    }

    /// The chunks against the B-tree they replaced: every insert and remove
    /// answered alike, and every range, both ways, over enough entries to
    /// split, empty and cross many chunks.
    #[test]
    fn chunks_agree_with_a_btreeset() {
        use std::collections::BTreeSet;
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let within = |lo: &Bound<(u64, u64)>, hi: &Bound<(u64, u64)>, e: &(u64, u64)| {
            (match lo {
                Bound::Included(a) => e >= a,
                Bound::Excluded(a) => e > a,
                Bound::Unbounded => true,
            }) && (match hi {
                Bound::Included(b) => e <= b,
                Bound::Excluded(b) => e < b,
                Bound::Unbounded => true,
            })
        };
        let mut c: Chunked<(u64, u64)> = Chunked::new(None);
        let mut r: BTreeSet<(u64, u64)> = BTreeSet::new();
        for step in 0..60_000 {
            let e = (next() % 500, next() % 30_000);
            if next() % 4 == 0 {
                assert_eq!(c.remove(&e), r.remove(&e), "remove {e:?}");
            } else {
                assert_eq!(c.insert(e), r.insert(e), "insert {e:?}");
            }
            if step % 10_000 == 0 {
                assert!(c.iter().eq(r.iter()), "after step {step}");
            }
        }
        // Keys that only grow, ties among them, appended past the end; then
        // random writes into the chunks the appends filled.
        for k in 0..3_000u64 {
            let e = (600 + k / 3, k);
            assert_eq!(c.insert(e), r.insert(e), "append {e:?}");
        }
        for _ in 0..6_000 {
            let e = (next() % 1_700, next() % 30_000);
            if next() % 3 == 0 {
                assert_eq!(c.remove(&e), r.remove(&e), "remove {e:?}");
            } else {
                assert_eq!(c.insert(e), r.insert(e), "insert {e:?}");
            }
        }
        assert!(c.iter().eq(r.iter()), "after the appends");
        assert_eq!(c.len, r.len());
        assert!(c.chunks.len() > 4, "only {} chunks", c.chunks.len());
        assert!(c
            .chunks
            .iter()
            .all(|ch| !ch.is_empty() && ch.capacity() <= CHUNK));

        // Built in one go, and then written to, it agrees as well.
        let mut built = Chunked::from_sorted(r.iter().copied().collect(), None);
        for _ in 0..5_000 {
            let e = (next() % 500, next() % 30_000);
            assert_eq!(built.insert(e), c.insert(e));
            r.insert(e);
        }
        assert!(built.iter().eq(r.iter()));

        for _ in 0..80 {
            let a = (next() % 1_720, next() % 30_000);
            let b = (next() % 1_720, next() % 30_000);
            let sides = |v: (u64, u64)| [Bound::Included(v), Bound::Excluded(v), Bound::Unbounded];
            for lo in sides(a) {
                for hi in sides(b) {
                    let want: Vec<_> = r.iter().filter(|e| within(&lo, &hi, e)).copied().collect();
                    let got: Vec<_> = c.range(&lo, &hi).copied().collect();
                    assert_eq!(got, want, "{lo:?}..{hi:?}");
                    let back: Vec<_> = c.range(&lo, &hi).rev().copied().collect();
                    assert!(
                        back.iter().rev().eq(want.iter()),
                        "{lo:?}..{hi:?} backwards"
                    );
                }
            }
        }
    }

    /// The radix sort against the comparison sort it replaced: keys that
    /// share their high bytes, keys that share none, keys all one value,
    /// ids small and ids anywhere.
    #[test]
    fn radix_sort_agrees_with_sort_unstable() {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for n in [0, 1, 2, 3, 1000, 20_000] {
            for key_span in [1, 7, 1 << 20, u64::MAX] {
                for id_span in [3000, u64::MAX] {
                    let mut v: Vec<(u64, DocId)> = (0..n)
                        .map(|_| (next() % key_span, next() % id_span))
                        .collect();
                    let mut want = v.clone();
                    want.sort_unstable();
                    radix_sort(&mut v);
                    assert_eq!(
                        v, want,
                        "{n} entries, keys under {key_span}, ids under {id_span}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_descending_walk_keeps_ties_in_id_order() {
        let rows = vec![
            (1, Some(Value::Int(5))),
            (2, Some(Value::Int(7))),
            (3, Some(Value::Int(5))),
            (4, None),
            (5, Some(Value::Int(7))),
        ];
        let ix = SortedIndex::build(&DataType::Int, None, &mut rows.into_iter());
        let walk = |desc| {
            let mut out = Vec::new();
            ix.walk(desc, None, |id| {
                out.push(id);
                Ok(true)
            })
            .unwrap();
            out
        };
        assert_eq!(walk(false), vec![4, 1, 3, 2, 5]);
        assert_eq!(walk(true), vec![2, 5, 1, 3, 4]);
    }
}
