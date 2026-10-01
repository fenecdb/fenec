//! Append-only segment store -- *no buffer pool*.
//!
//! In classic databases disk pages are copied into a page cache in user
//! space; consistency, eviction policy and locking cost all come from
//! there. fenecdb skips that entirely:
//!
//! - Segments are *immutable*. A write only appends to the end of the
//!   active segment.
//! - The byte sequence on disk and the byte sequence in memory are in the
//!   same format. There is therefore no "caching" stage; a read decodes
//!   straight off the arena slice.
//! - The only auxiliary structure is the offset index from id to location.
//!
//! The result: no eviction policy, no dirty pages, no checkpoint.

use crate::codec::*;
use crate::error::{Error, Result};
use crate::schema::Schema;
use crate::value::{DocId, Document, Value};
use std::collections::HashMap;

pub const OP_PUT: u8 = 1;
pub const OP_DEL: u8 = 2;

/// Once the active segment passes this size it is sealed and a new one opens.
pub const SEGMENT_MAX: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loc {
    pub seg: u32,
    /// Start of the payload inside the segment (past the frame header).
    pub off: u32,
    pub len: u32,
}

impl Loc {
    const EMPTY: Loc = Loc {
        seg: u32::MAX,
        off: 0,
        len: 0,
    };
    #[inline]
    fn is_empty(&self) -> bool {
        self.seg == u32::MAX
    }

    /// A payload left in the mapped file, at byte `at` of it: the top bit of
    /// `seg` says so, and its other 31 bits are the offset's high word, so a
    /// location stays 12 bytes however large the file.
    fn mapped(at: u64, len: u32) -> Loc {
        Loc {
            seg: MAPPED | (at >> 32) as u32,
            off: at as u32,
            len,
        }
    }
}

/// The bit of [`Loc::seg`] that marks a payload in the mapped file.
const MAPPED: u32 = 1 << 31;

/// Where a location lands in an image of its store's frames as they stand,
/// given where each stretch and segment starts in it
/// ([`Store::image_starts`]).
fn image_place(l: Loc, stretches: &[(u64, u64)], segments: &[u64]) -> u64 {
    match l.seg & MAPPED != 0 {
        true => {
            let from = ((l.seg & !MAPPED) as u64) << 32 | l.off as u64;
            let k = stretches.partition_point(|s| s.0 <= from) - 1;
            stretches[k].1 + (from - stretches[k].0)
        }
        false => segments[l.seg as usize] + l.off as u64,
    }
}

/// Where the payload of a put of `len` bytes to `id` starts, framed right
/// after `end`: past the frame's head, its op and two varints.
#[cfg(not(target_arch = "wasm32"))]
fn after(end: u64, id: DocId, len: u64) -> u64 {
    let varint = |v: u64| (64 - (v | 1).leading_zeros() as u64).div_ceil(7);
    end + 1 + varint(id) + varint(len)
}

/// The first byte of an image's index of a data record, after the id
/// counter in the counter record before it ([`Store::image_index`]). A
/// binary that does not know it reads the counter alone, as every reader
/// of the record did.
pub const FRAMES_MARK: u8 = 1;

/// The bytes a store reads records from without having copied them: a file
/// the operating system maps in on native targets (`fs::open_mapped`), and
/// in the browser the image a load was handed (`fenec_load_owned`), kept
/// rather than copied a document at a time into segments -- 10 000 rows of
/// 768 dimensions restored held 97 MB that way, and hold 66. A concrete
/// `Vec` there: through `dyn AsRef` the module was 126 bytes larger.
#[cfg(not(target_arch = "wasm32"))]
pub type Base = std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>;
#[cfg(target_arch = "wasm32")]
pub type Base = std::sync::Arc<Vec<u8>>;

/// Document id -> location mapping.
///
/// Ids are produced consecutively from 1 by `allocate_id`, so a dense array
/// is enough in the overwhelming majority of cases: no hashing, a single
/// index. Ids supplied from outside that fall far beyond the range land in
/// the sparse map, so `put docs {id: 10_000_000, ...}` does not blow up
/// memory.
///
/// Invariant: every sparse id is above the dense range. `get` looks only at
/// the dense slot for an id in that range, so a sparse entry the dense array
/// grew over would vanish from every lookup while `ids()` still listed it --
/// which is what happened before [`IdIndex::insert`] moved them over.
#[derive(Default, Clone)]
struct IdIndex {
    /// `dense[i]` -> id `i + 1`
    dense: Vec<Loc>,
    /// A `HashMap` rather than an ordered map: a `BTreeMap` here made the
    /// browser module 4.4 KB larger and its HNSW build 15% slower, the whole
    /// id index no longer inlining into the hot paths at `opt-level = "z"`.
    sparse: HashMap<DocId, Loc>,
    /// No sparse id is below this, so the dense array only has to look for
    /// ones to take over once it reaches it. A lower bound, not the minimum:
    /// a removal leaves it where it was, which costs at most one scan that
    /// finds nothing and sets it right.
    sparse_floor: DocId,
    count: usize,
}

/// The largest gap still considered worth extending the dense array for.
const MAX_DENSE_GAP: u64 = 4096;

impl IdIndex {
    /// Allocated bytes. Approximate for `HashMap`: hashbrown also keeps one
    /// control byte per bucket.
    fn bytes(&self) -> usize {
        use std::mem::size_of;
        self.dense.capacity() * size_of::<Loc>()
            + self.sparse.capacity() * (size_of::<DocId>() + size_of::<Loc>() + 1)
    }

    /// Is the id inside the dense array?
    ///
    /// The comparison has to happen on the `u64` side: on WASM32 `usize` is
    /// 32 bits and `id as usize` truncates silently. An id like 2^52 (the
    /// browser client's optimistic rows sit in exactly that range) wrapped
    /// to zero and went to `dense[0 - 1]`.
    #[inline]
    fn in_dense(&self, id: DocId) -> bool {
        id >= 1 && id <= self.dense.len() as u64
    }

    #[inline]
    fn get(&self, id: DocId) -> Option<Loc> {
        if self.in_dense(id) {
            let l = self.dense[id as usize - 1];
            return if l.is_empty() { None } else { Some(l) };
        }
        self.sparse.get(&id).copied()
    }

    fn insert(&mut self, id: DocId, loc: Loc) {
        if self.in_dense(id) {
            let slot = &mut self.dense[id as usize - 1];
            if slot.is_empty() {
                self.count += 1;
            }
            *slot = loc;
            return;
        }
        if id >= 1 && id - self.dense.len() as u64 <= MAX_DENSE_GAP {
            self.dense.resize(id as usize, Loc::EMPTY);
            // Keep the invariant: the sparse ids the array now covers move
            // into it, `id` itself included when this overwrites it. Moving
            // them does not change the count.
            if self.sparse_floor <= id {
                self.take_over_sparse(id);
            }
            let slot = &mut self.dense[id as usize - 1];
            if slot.is_empty() {
                self.count += 1;
            }
            *slot = loc;
            return;
        }
        if self.sparse.is_empty() || id < self.sparse_floor {
            self.sparse_floor = id;
        }
        if self.sparse.insert(id, loc).is_none() {
            self.count += 1;
        }
    }

    /// Moves the sparse entries the dense array, just grown to `upto`, now
    /// covers into it -- and with them every one it can reach under the same
    /// gap rule, growing further to do so. The keys are sorted once, so a run
    /// of sparse ids is taken in one pass: taking only those at or below
    /// `upto` meant one scan of the whole map per insert while the array grew
    /// through the run.
    fn take_over_sparse(&mut self, upto: DocId) {
        let mut keys: Vec<DocId> = self.sparse.keys().copied().collect();
        keys.sort_unstable();
        let mut end = upto;
        let mut taken = 0;
        for &k in &keys {
            if k > end && k - end > MAX_DENSE_GAP {
                break;
            }
            end = end.max(k);
            taken += 1;
        }
        if end > self.dense.len() as u64 {
            self.dense.resize(end as usize, Loc::EMPTY);
        }
        for &k in &keys[..taken] {
            if let Some(l) = self.sparse.remove(&k) {
                self.dense[k as usize - 1] = l;
            }
        }
        self.sparse_floor = keys.get(taken).copied().unwrap_or(DocId::MAX);
    }

    fn remove(&mut self, id: DocId) -> Option<Loc> {
        if self.in_dense(id) {
            let slot = &mut self.dense[id as usize - 1];
            if slot.is_empty() {
                return None;
            }
            let old = *slot;
            *slot = Loc::EMPTY;
            self.count -= 1;
            return Some(old);
        }
        let old = self.sparse.remove(&id);
        if old.is_some() {
            self.count -= 1;
        }
        old
    }

    #[inline]
    fn contains(&self, id: DocId) -> bool {
        self.get(id).is_some()
    }

    fn len(&self) -> usize {
        self.count
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn reserve(&mut self, n: usize) {
        self.dense.reserve(n);
    }

    /// Returns the ids in ascending order.
    fn ids(&self) -> Vec<DocId> {
        let mut out: Vec<DocId> = Vec::with_capacity(self.count);
        out.extend(self.iter());
        out
    }

    /// The ids in ascending order, lazily: the dense part is sorted by
    /// position, and every sparse id is above it (see the invariant). Only
    /// the sparse ids, usually none, are collected and sorted up front.
    fn iter(&self) -> impl Iterator<Item = DocId> + '_ {
        let mut extra: Vec<DocId> = self.sparse.keys().copied().collect();
        extra.sort_unstable();
        self.dense
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.is_empty())
            .map(|(i, _)| i as u64 + 1)
            .chain(extra)
    }

    /// [`Self::iter`] from `from` on: the dense part from its slot, the
    /// sparse ids at or above it. Only a native scan starts from an id
    /// (`Expr::conjunct_id_floor`); `iter` keeps its own walk, which as
    /// this one from 1 was 75 bytes brotli of the browser module.
    fn iter_from(&self, from: DocId) -> impl Iterator<Item = DocId> + '_ {
        let mut extra: Vec<DocId> = self.sparse.keys().copied().filter(|&k| k >= from).collect();
        extra.sort_unstable();
        let skip = (from.max(1) - 1).min(self.dense.len() as u64) as usize;
        self.dense[skip..]
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.is_empty())
            .map(move |(i, _)| (skip + i) as u64 + 1)
            .chain(extra)
    }

    /// Replaces every location with `f`'s, visiting the ids in `iter`'s
    /// order.
    fn relocate(&mut self, mut f: impl FnMut(DocId, Loc) -> Loc) {
        for (i, l) in self.dense.iter_mut().enumerate() {
            if !l.is_empty() {
                *l = f(i as u64 + 1, *l);
            }
        }
        let mut extra: Vec<DocId> = self.sparse.keys().copied().collect();
        extra.sort_unstable();
        for id in extra {
            if let Some(l) = self.sparse.get_mut(&id) {
                *l = f(id, *l);
            }
        }
    }
}

/// Document id -> a number above zero, for the indexes that keep one a
/// document: the text index's lengths and the vector index's nodes, held
/// one up. Dense where the ids are and sparse where they are not, as
/// [`IdIndex`] is, under the same gap: a `HashMap` lookup was the largest
/// cost of a BM25 merge, and filling one with 100 000 nodes 2.1 of a 16.7
/// ms open. A sparse number the dense array grows over stays in the map --
/// read there while its slot is empty, moved into the slot when its id is
/// written again -- rather than be taken over as the id index takes its
/// locations: that was 1 KB of the browser module.
#[derive(Default, Debug, PartialEq)]
#[cfg_attr(not(any(feature = "text", feature = "vector")), allow(dead_code))]
pub(crate) struct DocMap {
    /// `dense[i]` -> id `i + 1`; 0 is none.
    dense: Vec<u32>,
    sparse: HashMap<DocId, u32>,
    count: usize,
}

#[cfg_attr(not(any(feature = "text", feature = "vector")), allow(dead_code))]
impl DocMap {
    /// On the `u64` side, as [`IdIndex::in_dense`].
    #[inline]
    fn in_dense(&self, id: DocId) -> bool {
        id >= 1 && id <= self.dense.len() as u64
    }

    /// The number under `id`, 0 for none.
    #[inline]
    pub fn get(&self, id: DocId) -> u32 {
        if self.in_dense(id) {
            let n = self.dense[id as usize - 1];
            if n != 0 || self.sparse.is_empty() {
                return n;
            }
        }
        self.sparse.get(&id).copied().unwrap_or(0)
    }

    /// Puts `n`, above zero, under `id`, handing back the number it had.
    pub fn insert(&mut self, id: DocId, n: u32) -> Option<u32> {
        if !self.in_dense(id) && id >= 1 && id - self.dense.len() as u64 <= MAX_DENSE_GAP {
            self.dense.resize(id as usize, 0);
        }
        let old = match self.in_dense(id) {
            true => match std::mem::replace(&mut self.dense[id as usize - 1], n) {
                0 => self.take_sparse(id),
                old => old,
            },
            false => self.sparse.insert(id, n).unwrap_or(0),
        };
        if old != 0 {
            return Some(old);
        }
        self.count += 1;
        None
    }

    pub fn remove(&mut self, id: DocId) -> Option<u32> {
        let old = match self.in_dense(id) {
            true => match std::mem::replace(&mut self.dense[id as usize - 1], 0) {
                0 => self.take_sparse(id),
                old => old,
            },
            false => self.take_sparse(id),
        };
        if old == 0 {
            return None;
        }
        self.count -= 1;
        Some(old)
    }

    /// The sparse number under `id`, taken out; 0 for none.
    fn take_sparse(&mut self, id: DocId) -> u32 {
        match self.sparse.is_empty() {
            true => 0,
            false => self.sparse.remove(&id).unwrap_or(0),
        }
    }

    #[cfg_attr(not(feature = "text"), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.count
    }

    #[cfg_attr(not(feature = "text"), allow(dead_code))]
    pub fn clear(&mut self) {
        *self = DocMap::default();
    }

    #[cfg_attr(not(feature = "vector"), allow(dead_code))]
    pub fn reserve(&mut self, n: usize) {
        self.dense.reserve(n);
    }

    /// Allocated bytes, approximate for the `HashMap` as [`IdIndex::bytes`].
    pub fn bytes(&self) -> usize {
        self.dense.capacity() * 4 + self.sparse.capacity() * (std::mem::size_of::<DocId>() + 4 + 1)
    }

    #[cfg_attr(not(feature = "text"), allow(dead_code))]
    pub fn shrink_to_fit(&mut self) {
        self.dense.shrink_to_fit();
        self.sparse.shrink_to_fit();
    }
}

#[derive(Default, Clone)]
pub struct Segment {
    pub data: SegmentBytes,
    pub sealed: bool,
}

/// A segment's bytes: shared on native targets, so that a store cloned for
/// a rewrite beside the database (`compact` on a server) shares its sealed
/// segments rather than copying them -- a gigabyte written since the open
/// was a gigabyte copied under the read lock. The open one is copied the
/// first time it is written to while a clone holds it. In the browser an
/// image being taken holds them the same way ([`crate::engine::Kept`]),
/// rather than a copy of every one.
pub type SegmentBytes = std::sync::Arc<Vec<u8>>;

/// The segment's bytes to append to.
#[inline]
fn grow(b: &mut SegmentBytes) -> &mut Vec<u8> {
    std::sync::Arc::make_mut(b)
}

/// Where a store stood before a block of writes, for [`Store::rewind`] to
/// take it back there.
#[derive(Clone, Copy)]
pub struct Mark {
    segments: usize,
    /// The last segment's length, and whether it was sealed: a block that
    /// filled it sealed it and opened another.
    len: usize,
    sealed: bool,
    /// The stretches of the mapped file the store read from: a block that
    /// spilled into the file took its frames in from there
    /// ([`Store::hand_over`]), and put back, it lets them go.
    stretches: usize,
    next_id: DocId,
    dead_bytes: usize,
    total_bytes: usize,
}

impl Mark {
    /// Where the store stood before a block that has since spilled its
    /// frames into the file, which the store took them in from with the
    /// records before them: the first `stretches` stretches, and nothing in
    /// memory.
    pub fn spilled(self, stretches: usize) -> Mark {
        Mark {
            segments: 1,
            len: 0,
            sealed: false,
            stretches,
            ..self
        }
    }
}

/// Where the frames a store held in memory went when it took them in from
/// the file ([`Store::hand_over`]): a place in the segments it let go of,
/// by where its segment started in them all, moved into the run that held
/// it. What a block that spilled remembers of where a document was before
/// it wrote one, to point it back there.
#[cfg(not(target_arch = "wasm32"))]
pub struct Moved {
    starts: Vec<u64>,
    runs: Vec<(u64, u64)>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Moved {
    /// `l`'s place in the file, where it was in memory; as it is otherwise.
    pub fn loc(&self, l: Loc) -> Loc {
        if l.seg & MAPPED != 0 || l.is_empty() {
            return l;
        }
        let Some(&start) = self.starts.get(l.seg as usize) else {
            return l;
        };
        let at = start + l.off as u64;
        // The run holding it: they tile the segments, in order.
        let mut from = 0;
        for &(len, run_at) in &self.runs {
            if at < from + len {
                return Loc::mapped(run_at + (at - from), l.len);
            }
            from += len;
        }
        l
    }
}

/// Cloned, a store is what it held at that moment: the records in the
/// mapped file shared, the ones in memory copied -- what a rewrite beside
/// the database writes from, with no lock held.
#[derive(Clone)]
pub struct Store {
    segments: Vec<Segment>,
    /// Records that stayed in a mapped file rather than being copied into a
    /// segment: the file as it stood when opened, and the stretches of it
    /// that are this collection's, in order -- the ones it held then, and
    /// those of the records written since that the store took in from it
    /// ([`Self::hand_over`]) -- what `image` writes back.
    base: Option<(Base, Vec<(u64, u64)>)>,
    index: IdIndex,
    next_id: DocId,
    /// Bytes held by deleted/overwritten records (compaction threshold).
    dead_bytes: usize,
    total_bytes: usize,
    /// The collection's dropped places (`Schema::dropped`): a field's
    /// position, as every reader passes it, is a place further in the
    /// payload for each before it. Kept here, set wherever the schema
    /// changes, so that a read of one field takes no schema.
    dropped: Vec<usize>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    pub fn new() -> Store {
        Store {
            segments: vec![Segment::default()],
            base: None,
            index: IdIndex::default(),
            next_id: 1,
            dead_bytes: 0,
            total_bytes: 0,
            dropped: Vec::new(),
        }
    }

    /// The schema's dropped places, which every read by position passes
    /// over ([`Self::dropped`]).
    pub fn set_dropped(&mut self, dropped: &[usize]) {
        self.dropped.clear();
        self.dropped.extend_from_slice(dropped);
    }

    /// Where the field at `pos` is in a payload.
    #[inline]
    fn place(&self, pos: usize) -> usize {
        match self.dropped.is_empty() {
            true => pos,
            false => crate::schema::place(&self.dropped, pos),
        }
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }
    pub fn next_id(&self) -> DocId {
        self.next_id
    }
    pub fn dead_bytes(&self) -> usize {
        self.dead_bytes
    }
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }
    /// The record bytes held in memory: every one, unless some stayed in a
    /// mapped file, whose pages the operating system keeps or lets go of.
    pub fn heap_bytes(&self) -> usize {
        self.segments.iter().map(|s| s.data.len()).sum()
    }
    /// Bytes allocated by the offset index. Small next to the segment bytes,
    /// but a fixed per-document cost: in a collection of small documents it
    /// can reach a third of the total. A mapped store also keeps where each
    /// of its data records sits in the file, 16 bytes a record -- one a
    /// write for the tail since the last checkpoint: 16.7 of the 53.8 MB a
    /// mapped file of a million writes held, and uncounted before.
    pub fn index_bytes(&self) -> usize {
        if let Some((_, stretches)) = &self.base {
            return self.index.bytes() + stretches.capacity() * 16;
        }
        self.index.bytes()
    }
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }
    pub fn contains(&self, id: DocId) -> bool {
        self.index.contains(id)
    }
    /// Document ids, **in ascending order**.
    pub fn ids(&self) -> Vec<DocId> {
        self.index.ids()
    }
    /// The same ids without collecting them: a scan that stops at its
    /// `limit` should not first build a list of every id in the collection.
    pub fn iter_ids(&self) -> impl Iterator<Item = DocId> + '_ {
        self.index.iter()
    }
    /// The ids from `from` on, ascending.
    pub fn iter_ids_from(&self, from: DocId) -> impl Iterator<Item = DocId> + '_ {
        self.index.iter_from(from)
    }
    /// Reserves room up front for a known record count.
    pub fn reserve(&mut self, n: usize) {
        self.index.reserve(n);
    }

    pub fn allocate_id(&mut self) -> DocId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Where the store stands: what [`Self::rewind`] takes it back to.
    pub fn mark(&self) -> Mark {
        let last = self.segments.last();
        Mark {
            segments: self.segments.len(),
            len: last.map_or(0, |s| s.data.len()),
            sealed: last.is_some_and(|s| s.sealed),
            stretches: self.base.as_ref().map_or(0, |(_, s)| s.len()),
            next_id: self.next_id,
            dead_bytes: self.dead_bytes,
            total_bytes: self.total_bytes,
        }
    }

    /// Where `id`'s record is, if it has one: what a block remembers of the
    /// ids it writes, to point them back.
    pub fn loc(&self, id: DocId) -> Option<Loc> {
        self.index.get(id)
    }

    /// Points `id` at `loc`, or at nothing: where it was before a write
    /// that is put back.
    pub fn point(&mut self, id: DocId, loc: Option<Loc>) {
        match loc {
            Some(l) => self.index.insert(id, l),
            None => drop(self.index.remove(id)),
        }
    }

    /// Takes the store back to `mark`, dropping every record appended since
    /// -- a block of writes that did not land, each id it wrote pointed
    /// back first ([`Self::point`]). A block appends to the segments, so
    /// they are cut back where they stood; one that spilled into the file
    /// had the store take those frames in from there, and the stretches are
    /// cut back as well ([`Mark::spilled`]).
    pub fn rewind(&mut self, mark: Mark) {
        if let Some((_, stretches)) = &mut self.base {
            stretches.truncate(mark.stretches);
        }
        self.segments.truncate(mark.segments.max(1));
        if let Some(last) = self.segments.last_mut() {
            if last.data.len() > mark.len {
                grow(&mut last.data).truncate(mark.len);
            }
            last.sealed = mark.sealed;
        }
        self.next_id = mark.next_id;
        self.dead_bytes = mark.dead_bytes;
        self.total_bytes = mark.total_bytes;
    }

    /// Raises the counter to at least `v`; never lowers it.
    ///
    /// The counter is normally derived from the records ([`Store::append`]),
    /// but since `compact` drops tombstones the highest deleted id vanishes
    /// from the image altogether. The persistence layer pushes that level
    /// through this gate. No lowering: handing out an id again silently
    /// binds everything holding it to the wrong row.
    pub fn raise_next_id(&mut self, v: DocId) {
        self.next_id = self.next_id.max(v);
    }

    fn active(&mut self) -> &mut Segment {
        if self.segments.last().map(|s| s.data.len()).unwrap_or(0) >= SEGMENT_MAX {
            if let Some(last) = self.segments.last_mut() {
                last.sealed = true;
                // A segment grows by doubling and is sealed just past 8 MiB,
                // so it held up to twice its records: a 1 GB file's took
                // 1.66 GB of heap. Sealed, it grows no more; one reallocation
                // per 8 MiB gives the rest back -- unless a clone holds it,
                // which a copy to give back the slack would not be worth.
                if let Some(data) = std::sync::Arc::get_mut(&mut last.data) {
                    data.shrink_to_fit();
                }
            }
            self.segments.push(Segment::default());
        }
        self.segments.last_mut().unwrap()
    }

    /// Encodes a document in schema field order, a dropped place a `null`.
    pub fn encode_doc(schema: &Schema, doc: &Document) -> Vec<u8> {
        let mut payload = Vec::with_capacity(64);
        let mut at = 0;
        for (i, f) in schema.fields.iter().enumerate() {
            // The dropped places before this field's: none, most of the
            // time, and the loop not entered.
            while at < schema.place(i) {
                payload.push(crate::codec::TAG_NULL);
                at += 1;
            }
            let v = doc.get(&f.name).cloned().unwrap_or(Value::Null);
            // The field type is passed along: `vector<N, f16>` halves in the record.
            encode_value_as(&mut payload, &v, Some(&f.ty));
            at += 1;
        }
        payload
    }

    /// Produces the frame (header + payload). The same byte sequence goes to
    /// the segment and to the persistent sink.
    pub fn frame(op: u8, id: DocId, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len() + 16);
        out.push(op);
        put_uvarint(&mut out, id);
        put_uvarint(&mut out, payload.len() as u64);
        out.extend_from_slice(payload);
        out
    }

    /// Appends the frame to the active segment and updates the index.
    /// Returns the full frame written to the segment (for the persistence layer).
    pub fn append(&mut self, op: u8, id: DocId, payload: &[u8]) -> Vec<u8> {
        let frame = Store::frame(op, id, payload);
        if let Some(old) = self.index.get(id) {
            self.dead_bytes += old.len as usize;
        }
        let seg_ix = {
            let _ = self.active();
            self.segments.len() - 1
        };
        let seg = &mut self.segments[seg_ix];
        let header_len = frame.len() - payload.len();
        let off = seg.data.len() + header_len;
        let data = grow(&mut seg.data);
        // Doubled as a `Vec` would be, but never past the record that
        // seals it: doubled past 8 MiB, a segment of 768-dim documents took
        // 12.7 MB beside the 6.3 it was copied out of, and the browser
        // module's memory never gives a peak back.
        let need = data.len() + frame.len();
        if data.capacity() < need {
            let to = (data.capacity() * 2)
                .min(SEGMENT_MAX + frame.len())
                .max(need);
            data.reserve_exact(to - data.len());
        }
        data.extend_from_slice(&frame);
        self.total_bytes += frame.len();

        match op {
            OP_PUT => {
                self.index.insert(
                    id,
                    Loc {
                        seg: seg_ix as u32,
                        off: off as u32,
                        len: payload.len() as u32,
                    },
                );
                if id >= self.next_id {
                    self.next_id = id + 1;
                }
            }
            OP_DEL => {
                self.index.remove(id);
                self.dead_bytes += frame.len();
            }
            _ => {}
        }
        frame
    }

    #[inline]
    fn payload(&self, loc: Loc) -> Result<&[u8]> {
        if loc.seg & MAPPED != 0 {
            // A store over a file has one, and in the browser one over the
            // image it was loaded from (`Database::load_mapped`).
            if let Some((b, _)) = &self.base {
                let at = (((loc.seg & !MAPPED) as u64) << 32 | loc.off as u64) as usize;
                let file: &[u8] = (**b).as_ref();
                return file
                    .get(at..at + loc.len as usize)
                    .ok_or_else(|| Error::Corrupt("offset outside the mapped file".into()));
            }
        }
        let seg = self
            .segments
            .get(loc.seg as usize)
            .ok_or_else(|| Error::Corrupt("no such segment".into()))?;
        let s = loc.off as usize;
        let e = s + loc.len as usize;
        seg.data
            .get(s..e)
            .ok_or_else(|| Error::Corrupt("offset outside the segment".into()))
    }

    /// The payload held for `id`, as it was written: what a rewrite copies
    /// without decoding it.
    pub fn raw(&self, id: DocId) -> Result<Option<&[u8]>> {
        match self.index.get(id) {
            Some(loc) => self.payload(loc).map(Some),
            None => Ok(None),
        }
    }

    /// Decodes the whole document: a dropped place passed over, and a
    /// field the payload ends before -- added after it was written -- read
    /// as `null` ([`Schema::read_doc`]).
    pub fn read(&self, schema: &Schema, id: DocId) -> Result<Option<Document>> {
        let Some(loc) = self.index.get(id) else {
            return Ok(None);
        };
        schema.read_doc(id, self.payload(loc)?).map(Some)
    }

    /// Decodes a single field; the fields before it are skipped without
    /// allocating. Filter evaluation and projection use this path. A
    /// payload that ends before it holds `null` there: the field was added
    /// after the document was written, which no document is rewritten for.
    pub fn read_field(&self, id: DocId, field_pos: usize) -> Result<Option<Value>> {
        let Some(loc) = self.index.get(id) else {
            return Ok(None);
        };
        let buf = self.payload(loc)?;
        let mut pos = 0;
        for _ in 0..self.place(field_pos) {
            crate::codec::skip_field(buf, &mut pos)?;
        }
        match pos < buf.len() {
            true => Ok(Some(decode_value(buf, &mut pos)?)),
            false => Ok(Some(Value::Null)),
        }
    }

    /// The value at `keys` (`source.rank`) inside the `json` field at
    /// `field_pos`: `null` where the path leads nowhere or the document
    /// ends before the field, `None` when there is no such document.
    pub fn read_path(&self, id: DocId, field_pos: usize, keys: &str) -> Result<Option<Value>> {
        let mut v = [Value::Null];
        Ok(self
            .read_paths(id, &[(field_pos, keys)], &mut v)?
            .then(|| std::mem::replace(&mut v[0], Value::Null)))
    }

    /// The values at `paths` -- each a json field's position and the keys
    /// past it -- into `out`, from one look-up of the document: the fields
    /// before each skipped once, and two paths into one field reading it
    /// from where the first found it. A text lands in the text its slot
    /// held, as [`Self::read_fields`] has it. `false` when there is no such
    /// document.
    pub fn read_paths(
        &self,
        id: DocId,
        paths: &[(usize, &str)],
        out: &mut [Value],
    ) -> Result<bool> {
        let Some(loc) = self.index.get(id) else {
            return Ok(false);
        };
        let buf = self.payload(loc)?;
        let (mut at, mut pos) = (0usize, 0usize);
        for (slot, &(field, keys)) in out.iter_mut().zip(paths) {
            let want = self.place(field);
            if want < at {
                (at, pos) = (0, 0);
            }
            while at < want {
                crate::codec::skip_field(buf, &mut pos)?;
                at += 1;
            }
            match pos < buf.len() {
                true => crate::codec::decode_path_into(buf, pos, keys, slot)?,
                false => *slot = Value::Null,
            }
        }
        Ok(true)
    }

    /// Decodes the values at `places` -- ascending, each a field's place in
    /// the payload as [`Schema::place`] gives it -- into `out`, in one pass
    /// over the document that skips the others: an aggregate reads two or
    /// three fields of every row, and a `read_field` each would skip the
    /// fields before them once per field. A text lands in the text `out`
    /// held there, if it did. `false` when there is no such document.
    ///
    /// Places rather than positions: worked out once a query by the caller,
    /// not once a row here -- the compare that tells a collection with no
    /// dropped field took a scan of a million rows by one condition 21.2 ->
    /// 21.8 ms.
    pub fn read_fields(&self, id: DocId, places: &[usize], out: &mut Vec<Value>) -> Result<bool> {
        let Some(loc) = self.index.get(id) else {
            return Ok(false);
        };
        let buf = self.payload(loc)?;
        // Slots past the places are the caller's -- a filter keeps its
        // paths' values there -- and are left as they are.
        let mut pos = 0usize;
        let mut at = 0usize;
        for (i, &want) in places.iter().enumerate() {
            // Past the payload's end nothing is skipped: a field added
            // after the document was written is not in it, and reads as
            // `null` below.
            while at < want {
                crate::codec::skip_field(buf, &mut pos)?;
                at += 1;
            }
            // A text into the text the slot held: a scan reads a field of
            // every row into the same slot, and a text decoded anew was a
            // malloc and a free a row. Anything else decoded as it was.
            match (buf.get(pos), out.get_mut(i)) {
                (Some(&crate::codec::TAG_TEXT), Some(Value::Text(s))) => {
                    pos += 1;
                    crate::codec::decode_text_into(buf, &mut pos, s)?;
                }
                (None, Some(slot)) => *slot = Value::Null,
                (None, None) => out.push(Value::Null),
                (_, Some(slot)) => *slot = decode_value(buf, &mut pos)?,
                (_, None) => out.push(decode_value(buf, &mut pos)?),
            }
            at += 1;
        }
        Ok(true)
    }

    /// Decodes a vector field into the given buffer -- with no intermediate
    /// allocation.
    ///
    /// Allocating a `Vec<f32>` per node while restoring the graph was a
    /// noticeable part of the startup time; here a single buffer is reused.
    /// The stored document's bytes from the value at `field_pos` on, or
    /// `None` when there is no such document.
    fn field_at(&self, id: DocId, field_pos: usize) -> Result<Option<&[u8]>> {
        let Some(loc) = self.index.get(id) else {
            return Ok(None);
        };
        let buf = self.payload(loc)?;
        let mut pos = 0usize;
        for _ in 0..self.place(field_pos) {
            crate::codec::skip_field(buf, &mut pos)?;
        }
        Ok(Some(&buf[pos.min(buf.len())..]))
    }

    /// Whether the stored document holds a vector at `field_pos`, read off
    /// the value's tag without decoding it.
    pub fn has_vector(&self, id: DocId, field_pos: usize) -> Result<bool> {
        Ok(matches!(
            self.field_at(id, field_pos)?.and_then(|b| b.first()),
            Some(&(crate::codec::TAG_VECTOR | crate::codec::TAG_VECTOR_F16))
        ))
    }

    pub fn read_vector_into(
        &self,
        id: DocId,
        field_pos: usize,
        out: &mut Vec<f32>,
    ) -> Result<bool> {
        let Some(buf) = self.field_at(id, field_pos)? else {
            return Ok(false);
        };
        let mut pos = 0usize;
        // A `vector<N, f16>` field is stored halved, under its own tag.
        // Reading only the f32 one, every such field looked empty: `rerank`
        // over it returned no rows, and a restore found no vector for any
        // node and rebuilt the graph on every open.
        let half = match buf.get(pos) {
            Some(&crate::codec::TAG_VECTOR) => false,
            Some(&crate::codec::TAG_VECTOR_F16) => true,
            _ => return Ok(false),
        };
        pos += 1;
        let n = crate::codec::get_uvarint(buf, &mut pos)? as usize;
        let end = pos + n * if half { 2 } else { 4 };
        if end > buf.len() {
            return Err(Error::Corrupt("vector outside the segment".into()));
        }
        out.clear();
        out.reserve(n);
        if half {
            // Widened as the arena widens, without a branch: the vectors read
            // here are for measuring, and through `codec::f32_from_f16`'s
            // subnormal branch the loop stayed scalar -- ordering a quantized
            // index's candidates took most of its query that way.
            let (words, _) = buf[pos..end].as_chunks::<2>();
            out.extend(
                words
                    .iter()
                    .map(|w| crate::vector::half(u16::from_le_bytes(*w))),
            );
        } else {
            let (words, _) = buf[pos..end].as_chunks::<4>();
            out.extend(words.iter().map(|w| f32::from_le_bytes(*w)));
        }
        Ok(true)
    }

    /// Replays every record from a file/byte image.
    pub fn replay(&mut self, bytes: &[u8]) -> Result<usize> {
        self.replay_noting(bytes, &mut |_| {})
    }

    /// [`Self::replay`], handing each record's id to `note` -- how an open
    /// learns which documents the writes after a checkpoint touched.
    ///
    /// Each frame is copied as it stands into the segment `append` would
    /// have put it in, with the bookkeeping `append` does: framed afresh
    /// into a `Vec` of its own first, then copied again, a record at a time,
    /// it was 17 of a 25 ms load of 100 000 x 128 read into memory.
    pub fn replay_noting(&mut self, bytes: &[u8], note: &mut dyn FnMut(DocId)) -> Result<usize> {
        let mut pos = 0usize;
        let mut count = 0usize;
        while pos < bytes.len() {
            let start = pos;
            let op = bytes[pos];
            pos += 1;
            let id = get_uvarint(bytes, &mut pos)?;
            let len = get_uvarint(bytes, &mut pos)? as usize;
            if pos + len > bytes.len() {
                // Half-written last record: truncate and stop (crash-safe tail).
                break;
            }
            let frame = &bytes[start..pos + len];
            pos += len;
            if let Some(old) = self.index.get(id) {
                self.dead_bytes += old.len as usize;
            }
            let rest = bytes.len() - start;
            let seg = self.active();
            // A segment the load opens is sized for the records it will
            // hold, rather than grown by doubling past them.
            if seg.data.is_empty() {
                grow(&mut seg.data).reserve(rest.min(SEGMENT_MAX + len));
            }
            let off = seg.data.len() + (frame.len() - len);
            grow(&mut seg.data).extend_from_slice(frame);
            let seg = self.segments.len() as u32 - 1;
            self.total_bytes += frame.len();
            match op {
                OP_PUT => {
                    let loc = Loc {
                        seg,
                        off: off as u32,
                        len: len as u32,
                    };
                    self.index.insert(id, loc);
                    if id >= self.next_id {
                        self.next_id = id + 1;
                    }
                }
                OP_DEL => {
                    self.index.remove(id);
                    self.dead_bytes += frame.len();
                }
                _ => {}
            }
            note(id);
            count += 1;
        }
        Ok(count)
    }

    /// [`Self::replay_noting`] over records that stay where they are: the
    /// `len` bytes at `at` in `base`, the mapped file. The index points into
    /// the file and nothing is copied, so a record is read from pages the
    /// operating system brings in, and can drop again, rather than from
    /// memory the process holds.
    pub fn replay_mapped(
        &mut self,
        base: &Base,
        at: u64,
        len: u64,
        note: &mut dyn FnMut(DocId),
    ) -> Result<usize> {
        let file: &[u8] = (**base).as_ref();
        let bytes = file
            .get(at as usize..(at + len) as usize)
            .ok_or_else(|| Error::Corrupt("data record outside the mapped file".into()))?;
        let mut pos = 0usize;
        let mut count = 0usize;
        while pos < bytes.len() {
            let start = pos;
            let op = bytes[pos];
            pos += 1;
            let id = get_uvarint(bytes, &mut pos)?;
            let plen = get_uvarint(bytes, &mut pos)? as usize;
            if pos + plen > bytes.len() {
                // Half-written last record, as in `replay`.
                pos = start;
                break;
            }
            // The bookkeeping `append` does, with the payload left in place.
            let frame_len = pos + plen - start;
            if let Some(old) = self.index.get(id) {
                self.dead_bytes += old.len as usize;
            }
            self.total_bytes += frame_len;
            match op {
                OP_PUT => {
                    self.index
                        .insert(id, Loc::mapped(at + pos as u64, plen as u32));
                    if id >= self.next_id {
                        self.next_id = id + 1;
                    }
                }
                OP_DEL => {
                    self.index.remove(id);
                    self.dead_bytes += frame_len;
                }
                _ => {}
            }
            pos += plen;
            note(id);
            count += 1;
        }
        let (_, stretches) = self.base.get_or_insert_with(|| (base.clone(), Vec::new()));
        stretches.push((at, pos as u64));
        Ok(count)
    }

    /// Points the store at the file a checkpoint has just written its image
    /// into, `at` where its data record's body starts. The image is the
    /// store's frames as they stand -- the mapped stretches, then the
    /// segments -- so each location moves by where its stretch or segment
    /// landed, and the new file is not read: walking its frames took 1.8 to
    /// 2.3 s of a 1 GB file's checkpoint, all of it under the write lock.
    // In the browser, where `Base` is a type of no value, the body is
    // unreachable, which is the point.
    #[allow(unreachable_code)]
    #[cfg_attr(target_arch = "wasm32", allow(unused_variables))]
    pub fn relocate_image(&mut self, base: &Base, at: u64) {
        let (stretches, segments, len) = self.image_starts();
        self.index
            .relocate(|_, l| Loc::mapped(at + image_place(l, &stretches, &segments), l.len));
        self.base = Some((base.clone(), vec![(at, len)]));
        self.segments = vec![Segment::default()];
    }

    /// Where each stretch of the mapped file -- by where it was in the file
    /// -- and each segment starts in an image of the store's frames as they
    /// stand ([`Self::write_image`]), and the image's length.
    fn image_starts(&self) -> (Vec<(u64, u64)>, Vec<u64>, u64) {
        let mut stretches: Vec<(u64, u64)> = Vec::new();
        let mut to = 0;
        if let Some((_, old)) = &self.base {
            for &(from, len) in old {
                stretches.push((from, to));
                to += len;
            }
        }
        let mut segments = Vec::with_capacity(self.segments.len());
        for s in &self.segments {
            segments.push(to);
            to += s.data.len() as u64;
        }
        (stretches, segments, to)
    }

    /// Appends to `out` an index of the data record an image writes of this
    /// store -- its frames as they stand, or with `live` the live ones
    /// framed afresh in id order ([`Self::write_live`]): the record's
    /// length, the bytes in it that are dead, and for each live document in
    /// id order its id less the one before, its payload's length, and where
    /// the payload starts less where it would if its frame followed the one
    /// before ([`after`]) -- nothing, as a record is written, and a byte a
    /// document for an image of 590-byte records where the offset itself
    /// took two. An open takes the locations from it ([`Self::adopt_index`])
    /// rather than walking the frames' heads, each of which says where the
    /// next one is: a chain of cache misses, 5.5 of a 14.2 ms open at
    /// 100 000 x 128.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn image_index(&self, live: bool, out: &mut Vec<u8>) {
        let (stretches, segments, len) = self.image_starts();
        let (len, dead) = match live {
            true => (self.live_len() as u64, 0),
            false => (len, self.dead_bytes as u64),
        };
        out.push(FRAMES_MARK);
        put_uvarint(out, len);
        put_uvarint(out, dead);
        put_uvarint(out, self.index.len() as u64);
        let mut head = Vec::with_capacity(16);
        let (mut id_was, mut pos_was, mut to) = (0, 0, 0);
        for id in self.index.iter() {
            let Some(l) = self.index.get(id) else {
                continue;
            };
            let pos = match live {
                true => {
                    head.clear();
                    head.push(OP_PUT);
                    put_uvarint(&mut head, id);
                    put_uvarint(&mut head, l.len as u64);
                    to += head.len() as u64;
                    let pos = to;
                    to += l.len as u64;
                    pos
                }
                false => image_place(l, &stretches, &segments),
            };
            put_uvarint(out, id - id_was);
            put_uvarint(out, l.len as u64);
            let expected = after(pos_was, id, l.len as u64);
            put_uvarint(out, crate::codec::zigzag(pos as i64 - expected as i64));
            (id_was, pos_was) = (id, pos + l.len as u64);
        }
    }

    /// Takes a data record's frames from its image's index
    /// ([`Self::image_index`]) rather than walking their heads, the record
    /// `len` bytes at `at` of the mapped file: `false`, the store untouched,
    /// where the index does not describe that record -- a store holding
    /// records already, another length, ids not ascending, a payload past
    /// the record's end, bytes left over -- and the caller walks it.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn adopt_index(&mut self, base: &Base, at: u64, len: u64, index: &[u8]) -> bool {
        if !self.index.is_empty() || self.base.is_some() || self.total_bytes != 0 {
            return false;
        }
        let mut p = 1;
        let mut next = || get_uvarint(index, &mut p).ok();
        let (Some(whole), Some(dead), Some(count)) = (next(), next(), next()) else {
            return false;
        };
        if index.first() != Some(&FRAMES_MARK) || whole != len || dead > len {
            return false;
        }
        let mut fresh = IdIndex::default();
        // A frame is three bytes at the least: a count past that is not
        // this record's, and is not reserved for.
        fresh.reserve(count.min(len / 3) as usize);
        let (mut id, mut end) = (0u64, 0u64);
        for _ in 0..count {
            let (Some(step), Some(n), Some(off)) = (next(), next(), next()) else {
                return false;
            };
            id = match id.checked_add(step) {
                Some(i) if step > 0 && n <= u32::MAX as u64 => i,
                _ => return false,
            };
            let pos = (after(end, id, n) as i64).checked_add(crate::codec::unzigzag(off));
            end = match pos {
                Some(p) if p > 0 && (p as u64).checked_add(n).is_some_and(|e| e <= len) => {
                    p as u64 + n
                }
                _ => return false,
            };
            fresh.insert(id, Loc::mapped(at + end - n, n as u32));
        }
        if p != index.len() {
            return false;
        }
        self.index = fresh;
        self.total_bytes = len as usize;
        self.dead_bytes = dead as usize;
        self.next_id = self.next_id.max(id + 1);
        self.base = Some((base.clone(), vec![(at, len)]));
        true
    }

    /// [`Self::relocate_image`] for the image [`Self::write_live`] wrote: the
    /// live documents alone, in `iter`'s order, each a put -- so each lands
    /// where the frames before it end.
    // In the browser, where `Base` is a type of no value, the body is
    // unreachable, which is the point.
    #[allow(unreachable_code)]
    pub fn relocate_live(&mut self, base: &Base, at: u64) {
        let mut head = Vec::with_capacity(16);
        let mut to = at;
        self.index.relocate(|id, l| {
            head.clear();
            head.push(OP_PUT);
            put_uvarint(&mut head, id);
            put_uvarint(&mut head, l.len as u64);
            to += head.len() as u64;
            let moved = Loc::mapped(to, l.len);
            to += l.len as u64;
            moved
        });
        self.total_bytes = (to - at) as usize;
        self.dead_bytes = 0;
        self.base = Some((base.clone(), vec![(at, to - at)]));
        self.segments = vec![Segment::default()];
    }

    /// Takes the records the segments hold in from the file, and lets the
    /// segments go: the file holds each run of this store's frames a record
    /// appended since it was last read or rewritten, `(length, place)` in
    /// `landed`, in the order they were appended. Held until a restart or a
    /// compact, the documents written since the open were 787 MB of 250 000
    /// 768-dim ones, beside the 768 MB of their vectors the graph holds. The
    /// file holds the same bytes, so a document reads the same from it;
    /// `base` maps it as far as it now goes.
    ///
    /// `landed` has to account for every frame the segments hold, in order,
    /// or nothing moves and `false` comes back: frames no record of the file
    /// holds -- a compact's, built in memory -- stay where they are, and so
    /// do the ones after them, whose place in an image comes after theirs.
    /// Either way the store reads from `base`, the same file mapped further.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn hand_over(&mut self, base: &Base, landed: &[(u64, u64)]) -> bool {
        self.hand_over_keeping(base, landed, 0).is_some()
    }

    /// [`Self::hand_over`], saying where the segments' frames went
    /// ([`Moved`]) and how many stretches the store held once it took the
    /// first `keep` runs in -- a block's spill hands over the records that
    /// landed before the block with its own frames, and a rollback keeps
    /// the ones before it ([`Mark::spilled`]).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn hand_over_keeping(
        &mut self,
        base: &Base,
        landed: &[(u64, u64)],
        keep: usize,
    ) -> Option<(Moved, usize)> {
        let moved = self.landed_places(landed);
        if let Some((b, _)) = &mut self.base {
            *b = base.clone();
        }
        let moved = moved?;
        let mut starts = Vec::with_capacity(self.segments.len());
        let mut to = 0;
        for s in &self.segments {
            starts.push(to);
            to += s.data.len() as u64;
        }
        let kept = self.base.as_ref().map_or(0, |(_, s)| s.len());
        if landed.is_empty() {
            let runs = Vec::new();
            return Some((Moved { starts, runs }, kept));
        }
        for (id, loc) in moved {
            self.index.insert(id, loc);
        }
        let (_, stretches) = self.base.get_or_insert_with(|| (base.clone(), Vec::new()));
        let mut kept = stretches.len();
        for (k, &(len, at)) in landed.iter().enumerate() {
            if len > 0 {
                // Never across `keep`: a rollback cuts the stretches there.
                match stretches.last_mut() {
                    Some(last) if last.0 + last.1 == at && k != keep => last.1 += len,
                    _ => stretches.push((at, len)),
                }
            }
            if k + 1 == keep {
                kept = stretches.len();
            }
        }
        self.segments = vec![Segment::default()];
        let runs = landed.to_vec();
        Some((Moved { starts, runs }, kept))
    }

    /// Whether runs of these lengths account for every frame the segments
    /// hold, in order: what [`Self::hand_over`] asks before anything moves,
    /// asked before the records are written.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn would_hand_over(&self, lens: &[u64]) -> bool {
        let runs: Vec<(u64, u64)> = lens.iter().map(|&l| (l, 0)).collect();
        self.landed_places(&runs).is_some()
    }

    /// Where in the file each document the segments hold its version of
    /// goes, by [`Self::hand_over`]'s `landed`; `None` where the runs do not
    /// account for the segments frame for frame. Nothing moves here: a run
    /// that fails half way leaves the store as it was.
    #[cfg(not(target_arch = "wasm32"))]
    fn landed_places(&self, landed: &[(u64, u64)]) -> Option<Vec<(DocId, Loc)>> {
        let held: u64 = self.segments.iter().map(|s| s.data.len() as u64).sum();
        if landed.iter().map(|l| l.0).sum::<u64>() != held {
            return None;
        }
        let mut moved = Vec::new();
        let (mut s, mut o) = (0usize, 0usize);
        for &(len, at) in landed {
            let (mut left, mut at) = (len as usize, at);
            while left > 0 {
                // A run a seal split goes on at the start of the next
                // segment: a frame is never split, `append` seals first.
                while s < self.segments.len() && o == self.segments[s].data.len() {
                    (s, o) = (s + 1, 0);
                }
                let seg = self.segments.get(s)?;
                let n = left.min(seg.data.len() - o);
                let run = &seg.data[o..o + n];
                let mut p = 0;
                while p < n {
                    let op = run[p];
                    p += 1;
                    let id = get_uvarint(run, &mut p).ok()?;
                    let plen = get_uvarint(run, &mut p).ok()? as usize;
                    if p + plen > n {
                        return None;
                    }
                    // The version the store reads is this one: a later
                    // write of the id points elsewhere, and a delete
                    // nowhere.
                    let was = Loc {
                        seg: s as u32,
                        off: (o + p) as u32,
                        len: plen as u32,
                    };
                    if op == OP_PUT && self.index.get(id) == Some(was) {
                        moved.push((id, Loc::mapped(at + p as u64, plen as u32)));
                    }
                    p += plen;
                }
                (o, left, at) = (o + n, left - n, at + n as u64);
            }
        }
        Some(moved)
    }

    /// A store the rewrite wrote no data record for: nothing live, and no
    /// hold on the file it read from.
    pub fn let_go(&mut self) {
        self.base = None;
        self.segments = vec![Segment::default()];
        self.total_bytes = 0;
        self.dead_bytes = 0;
    }

    /// Moves the live records into fresh segments and drops the tombstones.
    /// The image the file gets is written from the store afterwards; one
    /// built here too was a second copy of the live data, thrown away.
    pub fn compact(&mut self) -> Result<()> {
        *self = self.compacted()?;
        Ok(())
    }

    /// The live records in a fresh store, the dead ones left behind -- and
    /// `self` untouched, so the database goes on using it while a
    /// maintenance builds beside it.
    pub fn compacted(&self) -> Result<Store> {
        let mut fresh = Store::new();
        fresh.next_id = self.next_id;
        fresh.dropped.clone_from(&self.dropped);
        let ids = self.index.ids();
        fresh.reserve(ids.len());
        for id in ids {
            let loc = self.index.get(id).unwrap();
            fresh.append(OP_PUT, id, self.payload(loc)?);
        }
        Ok(fresh)
    }

    /// Bytes [`Self::write_image`] writes: the record bytes this store
    /// holds, mapped and in memory together.
    pub fn image_len(&self) -> usize {
        self.total_bytes
    }

    /// Bytes [`Self::write_live`] writes. Counted rather than taken from
    /// `total_bytes - dead_bytes`: a superseded record leaves its payload's
    /// length behind in `dead_bytes`, not its frame's, and a length that is
    /// off makes the record after the data one unreadable.
    pub fn live_len(&self) -> usize {
        let mut head = Vec::with_capacity(16);
        let mut total = 0usize;
        for id in self.index.iter() {
            let Some(loc) = self.index.get(id) else {
                continue;
            };
            head.clear();
            head.push(OP_PUT);
            put_uvarint(&mut head, id);
            put_uvarint(&mut head, loc.len as u64);
            total += head.len() + loc.len as usize;
        }
        total
    }

    /// The live records, framed afresh, in id order: what `compact` writes
    /// into the new file. Nothing is gathered in memory on the way.
    pub fn write_live(&self, out: &mut dyn crate::engine::ImageOut) -> Result<()> {
        let mut head = Vec::with_capacity(16);
        for id in self.index.iter() {
            let Some(loc) = self.index.get(id) else {
                continue;
            };
            let payload = self.payload(loc)?;
            head.clear();
            head.push(OP_PUT);
            put_uvarint(&mut head, id);
            put_uvarint(&mut head, payload.len() as u64);
            out.write(&head)?;
            out.write(payload)?;
        }
        Ok(())
    }

    /// Whether the records are read from a mapped file rather than held in
    /// memory (`fs::open_mapped`).
    pub fn is_mapped(&self) -> bool {
        self.base.is_some()
    }

    /// The records, written into `out` rather than gathered into a `Vec`:
    /// what a checkpoint of a large collection would otherwise hold beside
    /// the data.
    pub fn write_image(&self, out: &mut dyn crate::engine::ImageOut) -> Result<()> {
        // Held rather than copied in the browser: an image taken a chunk at
        // a time held every chunk of it beside the rows until the first was
        // stored, 30 MB of 50 000 128-dim rows.
        #[cfg(target_arch = "wasm32")]
        {
            use crate::engine::Kept;
            if let Some((base, stretches)) = &self.base {
                for &(at, len) in stretches {
                    let (at, len) = (at as usize, len as usize);
                    out.write_kept(Kept {
                        whole: base.clone(),
                        at,
                        len,
                    })?;
                }
            }
            for s in &self.segments {
                let len = s.data.len();
                out.write_kept(Kept {
                    whole: s.data.clone(),
                    at: 0,
                    len,
                })?;
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some((base, stretches)) = &self.base {
                let file: &[u8] = (**base).as_ref();
                for &(at, len) in stretches {
                    out.write(&file[at as usize..(at + len) as usize])?;
                }
            }
            for s in &self.segments {
                out.write(s.data.as_slice())?;
            }
        }
        Ok(())
    }

    /// Byte image of the whole store (to persist or to move it).
    pub fn image(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.total_bytes);
        // The records left in the mapped file came first; the segments hold
        // what was written after the file was opened.
        if let Some((base, stretches)) = &self.base {
            let file: &[u8] = (**base).as_ref();
            for &(at, len) in stretches {
                out.extend_from_slice(&file[at as usize..(at + len) as usize]);
            }
        }
        for s in &self.segments {
            out.extend_from_slice(s.data.as_slice());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Field;
    use crate::value::DataType;

    fn schema() -> Schema {
        Schema::new(
            "t",
            vec![
                Field::new("a", DataType::Text),
                Field::new("b", DataType::Int),
            ],
        )
        .unwrap()
    }

    /// A field added after a document was written reads `null` where its
    /// payload ends, by every way a field is read; a dropped place is
    /// passed over by all of them, and a payload cut inside a value is
    /// still refused.
    #[test]
    fn a_payload_ends_early_and_a_dropped_place_is_passed_over() {
        let mut st = Store::new();
        let mut doc = Document::default();
        doc.set("a", Value::Text("x".into()));
        doc.set("b", Value::Int(7));
        st.append(OP_PUT, 1, &Store::encode_doc(&schema(), &doc));
        // `b` dropped, then `c` added: `a` at 0, a dropped place at 1, `c`
        // at 2, where the payload has ended.
        let mut later = schema();
        later.fields.remove(1);
        later.dropped = vec![1];
        later.fields.push(Field::new("c", DataType::Int));
        st.set_dropped(&later.dropped);
        let read = st.read(&later, 1).unwrap().unwrap();
        assert_eq!(read.get("a"), Some(&Value::Text("x".into())));
        assert_eq!(read.get("c"), Some(&Value::Null));
        assert_eq!(st.read_field(1, 1).unwrap(), Some(Value::Null));
        assert_eq!(st.read_field(1, 0).unwrap(), Some(Value::Text("x".into())));
        let mut out = Vec::new();
        let places: Vec<usize> = (0..2).map(|p| later.place(p)).collect();
        assert_eq!(places, [0, 2]);
        assert!(st.read_fields(1, &places, &mut out).unwrap());
        assert_eq!(out, [Value::Text("x".into()), Value::Null]);
        // Written now, the dropped place is a null and `c` is where it reads.
        let mut doc = Document::default();
        doc.set("a", Value::Text("y".into()));
        doc.set("c", Value::Int(3));
        let payload = Store::encode_doc(&later, &doc);
        assert_eq!(payload[3], crate::codec::TAG_NULL);
        st.append(OP_PUT, 2, &payload);
        assert_eq!(st.read_field(2, 1).unwrap(), Some(Value::Int(3)));
        assert_eq!(
            later.read_doc(2, &payload).unwrap().get("c"),
            Some(&Value::Int(3))
        );
        // Taken out, the place is gone and the values stand.
        let stripped = later.without_dropped(st.raw(1).unwrap().unwrap()).unwrap();
        let mut compacted = later.clone();
        compacted.dropped.clear();
        assert_eq!(
            compacted.read_doc(1, &stripped).unwrap(),
            later.read_doc(1, st.raw(1).unwrap().unwrap()).unwrap()
        );
        // A payload cut inside a value is no field added after it.
        let cut = &payload[..payload.len() - 1];
        assert!(later.read_doc(2, cut).is_err());
    }

    #[test]
    fn a_schema_writes_its_dropped_places_and_reads_them_back() {
        let mut s = schema();
        s.fields.remove(0);
        s.dropped = vec![0];
        s.fields.push(Field::new("c", DataType::Text));
        let bytes = s.encode();
        assert_eq!(Schema::decode(&bytes, &mut 0).unwrap(), s);
        assert_eq!((s.place(0), s.place(1), s.width()), (1, 2, 3));
    }

    #[test]
    fn put_read_delete_replay() {
        let sc = schema();
        let mut st = Store::new();
        let id = st.allocate_id();
        let doc = Document {
            id,
            fields: vec![
                ("a".into(), Value::Text("x".into())),
                ("b".into(), Value::Int(7)),
            ],
        };
        st.append(OP_PUT, id, &Store::encode_doc(&sc, &doc));
        assert_eq!(
            st.read(&sc, id).unwrap().unwrap().get("b"),
            Some(&Value::Int(7))
        );
        // field-level read
        assert_eq!(st.read_field(id, 1).unwrap(), Some(Value::Int(7)));

        let image = st.image();
        let mut st2 = Store::new();
        st2.replay(&image).unwrap();
        assert_eq!(st2.len(), 1);

        st.append(OP_DEL, id, &[]);
        assert_eq!(st.len(), 0);
    }

    #[test]
    fn id_index_handles_sparse_ids() {
        let sc = schema();
        let mut st = Store::new();
        let mk = |i: i64| Document {
            id: 0,
            fields: vec![
                ("a".into(), Value::Text(format!("v{i}"))),
                ("b".into(), Value::Int(i)),
            ],
        };
        // dense range
        for i in 1..=100u64 {
            st.append(OP_PUT, i, &Store::encode_doc(&sc, &mk(i as i64)));
        }
        // small gap -> still dense
        st.append(OP_PUT, 200, &Store::encode_doc(&sc, &mk(200)));
        // large gap -> falls into the sparse map, memory must not blow up
        st.append(OP_PUT, 9_000_000_000, &Store::encode_doc(&sc, &mk(-1)));

        assert_eq!(st.len(), 102);
        assert!(st.contains(1) && st.contains(100) && st.contains(200));
        assert!(st.contains(9_000_000_000));
        assert!(!st.contains(150));
        assert_eq!(st.read_field(200, 1).unwrap(), Some(Value::Int(200)));
        assert_eq!(
            st.read_field(9_000_000_000, 1).unwrap(),
            Some(Value::Int(-1))
        );
        assert_eq!(st.read_field(150, 1).unwrap(), None);

        // ids must come back in ascending order (dense + sparse merged)
        let ids = st.ids();
        assert_eq!(ids.len(), 102);
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "not sorted");
        assert_eq!(*ids.last().unwrap(), 9_000_000_000);

        // deletion has to work on both paths
        st.append(OP_DEL, 50, &[]);
        st.append(OP_DEL, 9_000_000_000, &[]);
        assert_eq!(st.len(), 100);
        assert!(!st.contains(50) && !st.contains(9_000_000_000));

        // a replay must give the same result
        let mut st2 = Store::new();
        st2.replay(&st.image()).unwrap();
        assert_eq!(st2.len(), 100);
        assert_eq!(st2.ids(), st.ids());
    }

    /// An image's index of a data record gives the store walking the record
    /// would: the same ids at the same places, the same counts. And an index
    /// that does not describe the record leaves the store as it was, for
    /// the walk.
    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn an_image_index_gives_the_store_its_walk_gives() {
        let sc = schema();
        let doc = |i: i64| Document {
            id: 0,
            fields: vec![
                ("a".into(), Value::Text("v".repeat(i as usize % 300))),
                ("b".into(), Value::Int(i)),
            ],
        };
        let mut st = Store::new();
        for id in 1..=400u64 {
            st.append(OP_PUT, id, &Store::encode_doc(&sc, &doc(id as i64)));
        }
        // Rewritten, deleted, and ids past the dense array's gap.
        for id in (1..=400u64).step_by(7) {
            st.append(OP_PUT, id, &Store::encode_doc(&sc, &doc(-(id as i64))));
        }
        for id in (3..=400u64).step_by(11) {
            st.append(OP_DEL, id, &[]);
        }
        st.append(OP_PUT, 1 << 40, &Store::encode_doc(&sc, &doc(5)));
        for live in [false, true] {
            let mut image = Vec::new();
            match live {
                true => st.write_live(&mut image).unwrap(),
                false => st.write_image(&mut image).unwrap(),
            }
            let mut index = Vec::new();
            st.image_index(live, &mut index);
            // Four bytes before the record, as a file has its head.
            let at = 4u64;
            let mut file = vec![0u8; at as usize];
            file.extend_from_slice(&image);
            let base: Base = std::sync::Arc::new(file);
            let len = image.len() as u64;
            let mut walked = Store::new();
            walked.replay_mapped(&base, at, len, &mut |_| {}).unwrap();
            let mut taken = Store::new();
            assert!(taken.adopt_index(&base, at, len, &index), "live {live}");
            assert_eq!(taken.ids(), walked.ids(), "live {live}");
            for id in walked.ids() {
                assert_eq!(
                    taken.read_field(id, 1).unwrap(),
                    walked.read_field(id, 1).unwrap()
                );
            }
            let counts = |s: &Store| (s.len(), s.total_bytes(), s.dead_bytes(), s.next_id());
            assert_eq!(counts(&taken), counts(&walked), "live {live}");
            // Another length, cut short, a byte more, an id twice, a payload
            // past the end: refused, the store left empty. A payload moved
            // within the record is not seen, as a frame's head that says
            // another length is not by the walk.
            let hand = |entries: &[(u64, u64, u64)]| {
                let mut ix = vec![FRAMES_MARK];
                for v in [len, 0, entries.len() as u64] {
                    put_uvarint(&mut ix, v);
                }
                for &(step, n, off) in entries {
                    for v in [step, n, off] {
                        put_uvarint(&mut ix, v);
                    }
                }
                ix
            };
            let (twice, past) = (hand(&[(5, 1, 0), (0, 1, 0)]), hand(&[(1, len, 0)]));
            for (bad, what) in [
                (&index[..], len + 1),
                (&index[..index.len() - 1], len),
                (&[&index[..], &[0]].concat()[..], len),
                (&twice[..], len),
                (&past[..], len),
            ] {
                let mut refused = Store::new();
                assert!(!refused.adopt_index(&base, at, what, bad), "live {live}");
                assert_eq!(counts(&refused), (0, 0, 0, 1), "live {live}");
            }
        }
    }

    /// A `DocMap` keeps a number the dense array grows over, as the id
    /// index keeps a location: the text index's lengths were read from the
    /// dense slot the array had grown over, 0, and a document scored as
    /// empty.
    #[test]
    fn a_doc_map_keeps_what_the_dense_array_grows_over() {
        let mut m = DocMap::default();
        for (id, n) in [(10_000u64, 7u32), (5_000, 5), (4_000, 4), (8_000, 8)] {
            assert_eq!(m.insert(id, n), None);
        }
        assert_eq!(m.len(), 4);
        for (id, n) in [(4_000u64, 4u32), (5_000, 5), (8_000, 8), (10_000, 7)] {
            assert_eq!(m.get(id), n, "{id}");
        }
        assert_eq!(m.insert(10_000, 9), Some(7));
        assert_eq!(m.insert(11_000, 11), None);
        assert_eq!(m.len(), 5);
        assert_eq!(m.remove(5_000), Some(5));
        assert_eq!(m.remove(5_000), None);
        assert_eq!((m.get(5_000), m.get(10_000), m.len()), (0, 9, 4));
        // Past the gap stays sparse, as does the id 0 when the array grows.
        assert_eq!(m.insert(1 << 52, 3), None);
        assert_eq!(m.insert(0, 1), None);
        assert_eq!(m.insert(12_000, 12), None);
        assert_eq!((m.get(1 << 52), m.get(0), m.get(12_000)), (3, 1, 12));
        assert_eq!(m.len(), 7);
    }

    /// A sparse id the dense array later grows over has to move into it.
    /// Before it did, `get t where id = 5000` found nothing, `ids()` listed
    /// 5000 out of order, and `count` still counted it.
    #[test]
    fn sparse_ids_move_into_the_dense_array_as_it_grows() {
        let sc = schema();
        let doc = |i: i64| Document {
            id: 0,
            fields: vec![
                ("a".into(), Value::Text(format!("v{i}"))),
                ("b".into(), Value::Int(i)),
            ],
        };
        let mut st = Store::new();
        // 10 000 and 5 000 are too far from an empty dense array: sparse.
        // 4 000 is within the gap: the array grows to it. 8 000 is within the
        // gap of 4 000: the array grows over 5 000.
        for id in [10_000u64, 5_000, 4_000, 8_000] {
            st.append(OP_PUT, id, &Store::encode_doc(&sc, &doc(id as i64)));
        }
        assert_eq!(st.len(), 4);
        assert!(st.contains(5_000), "the covered sparse id vanished");
        assert_eq!(st.read_field(5_000, 1).unwrap(), Some(Value::Int(5_000)));
        assert_eq!(st.ids(), vec![4_000, 5_000, 8_000, 10_000]);
        assert_eq!(st.iter_ids().collect::<Vec<_>>(), st.ids());

        // Overwriting an id while the array grows over it counts it once.
        st.append(OP_PUT, 10_000, &Store::encode_doc(&sc, &doc(-10)));
        st.append(OP_PUT, 11_000, &Store::encode_doc(&sc, &doc(11_000)));
        assert_eq!(st.len(), 5);
        assert_eq!(st.read_field(10_000, 1).unwrap(), Some(Value::Int(-10)));
        assert_eq!(st.ids(), vec![4_000, 5_000, 8_000, 10_000, 11_000]);
        for from in [
            0,
            1,
            4_000,
            4_001,
            8_000,
            9_999,
            10_000,
            11_000,
            11_001,
            u64::MAX,
        ] {
            let want: Vec<_> = st.ids().into_iter().filter(|&i| i >= from).collect();
            assert_eq!(
                st.iter_ids_from(from).collect::<Vec<_>>(),
                want,
                "from {from}"
            );
        }

        let mut st2 = Store::new();
        st2.replay(&st.image()).unwrap();
        assert_eq!(st2.ids(), st.ids());
        assert!(st2.contains(5_000));

        // A run of sparse ids is taken over in one go when the array reaches
        // it, including the ones past the id that reached it.
        let mut st = Store::new();
        for id in 20_000u64..20_500 {
            st.append(OP_PUT, id, &Store::encode_doc(&sc, &doc(id as i64)));
        }
        for id in (4_000u64..=20_000).step_by(4_000) {
            st.append(OP_PUT, id, &Store::encode_doc(&sc, &doc(id as i64)));
        }
        assert_eq!(st.len(), 504);
        for id in [4_000u64, 16_000, 20_000, 20_001, 20_499] {
            assert!(st.contains(id), "{id} vanished");
        }
        let ids = st.ids();
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "not sorted");
        assert_eq!(ids.len(), 504);
    }

    /// A store takes its frames in from a file that holds them only where
    /// the runs account for every one, in order, and reads the same after:
    /// a run short of a frame, or one that ends inside one, moves nothing.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_handover_moves_nothing_the_runs_do_not_account_for() {
        let sc = schema();
        let doc = |i: i64, pad: usize| Document {
            id: 0,
            fields: vec![
                ("a".into(), Value::Text(format!("v{i}{}", "x".repeat(pad)))),
                ("b".into(), Value::Int(i)),
            ],
        };
        let mut st = Store::new();
        // Past a segment, so a run goes on in the next one.
        let mut frames = Vec::new();
        for i in 1..=2200u64 {
            frames.extend(st.append(OP_PUT, i, &Store::encode_doc(&sc, &doc(i as i64, 4000))));
        }
        frames.extend(st.append(OP_PUT, 7, &Store::encode_doc(&sc, &doc(-7, 3))));
        frames.extend(st.append(OP_DEL, 8, &[]));
        assert!(st.segment_count() > 1);
        let read = |st: &Store| -> Vec<Option<Value>> {
            (1..=2200).map(|id| st.read_field(id, 0).unwrap()).collect()
        };
        let (before, image) = (read(&st), st.image());

        // The file: something else first, then the frames, as records
        // appended after an open hold them.
        let mut file = vec![0u8; 100];
        file.extend_from_slice(&frames);
        let base: Base = std::sync::Arc::new(file);
        let whole = frames.len() as u64;
        let last = Store::frame(OP_DEL, 8, &[]).len() as u64;
        for runs in [
            vec![(whole - last, 100)],
            vec![(whole - 1, 100), (1, 100 + whole - 1)],
            vec![(10, 100), (whole - 10, 110)],
        ] {
            assert!(!st.hand_over(&base, &runs), "{runs:?}");
            assert_eq!(read(&st), before);
            assert!(st.heap_bytes() as u64 >= whole);
        }
        // Two runs, split where a record would end.
        let split = Store::frame(OP_PUT, 1, &Store::encode_doc(&sc, &doc(1, 4000))).len() as u64;
        assert!(st.hand_over(&base, &[(split, 100), (whole - split, 100 + split)]));
        assert_eq!(st.heap_bytes(), 0);
        assert_eq!(read(&st), before);
        assert_eq!(
            st.read_field(7, 0).unwrap(),
            Some(Value::Text("v-7xxx".into()))
        );
        assert!(!st.contains(8));
        // Written back as it stood.
        assert_eq!(st.image(), image);
    }

    #[test]
    fn compaction_drops_tombstones() {
        let sc = schema();
        let mut st = Store::new();
        for i in 0..100 {
            let id = st.allocate_id();
            let doc = Document {
                id,
                fields: vec![
                    ("a".into(), Value::Text(format!("v{i}"))),
                    ("b".into(), Value::Int(i)),
                ],
            };
            st.append(OP_PUT, id, &Store::encode_doc(&sc, &doc));
        }
        for id in 1..=50u64 {
            st.append(OP_DEL, id, &[]);
        }
        assert!(st.dead_bytes() > 0);
        st.compact().unwrap();
        assert_eq!(st.len(), 50);
        assert_eq!(st.dead_bytes(), 0);
        let mut st2 = Store::new();
        st2.replay(&st.image()).unwrap();
        assert_eq!(st2.len(), 50);
    }

    /// An externally supplied large id has to land in the sparse map -- and
    /// **on a 32-bit target too**. Comparing via `id as usize` turned 2^52
    /// into zero on WASM32 and sent it to `dense[0 - 1]`; the browser
    /// client's optimistic rows sit in exactly that range.
    #[test]
    fn ids_beyond_u32_land_in_the_sparse_map() {
        let sc = Schema::new(
            "t",
            vec![crate::schema::Field::new("a", crate::value::DataType::Int)],
        )
        .unwrap();
        let mut st = Store::new();
        let doc = |id: DocId, a: i64| Document {
            id,
            fields: vec![("a".into(), Value::Int(a))],
        };
        st.append(OP_PUT, 1, &Store::encode_doc(&sc, &doc(1, 1)));

        let big: DocId = 1 << 52;
        st.append(OP_PUT, big, &Store::encode_doc(&sc, &doc(big, 2)));
        assert!(st.contains(big));
        assert_eq!(st.len(), 2);
        assert_eq!(
            st.read(&sc, big).unwrap().unwrap().get("a"),
            Some(&Value::Int(2))
        );
        // The dense part must not have grown: an array of 2^52 entries could
        // not be allocated in the first place.
        assert!(st.index_bytes() < 4096);

        st.append(OP_DEL, big, &[]);
        assert!(!st.contains(big));
        assert!(st.contains(1), "the small id must be unaffected");

        // The exact 2^32 boundary: truncation lands on 0 here as well.
        let edge: DocId = 1 << 32;
        st.append(OP_PUT, edge, &Store::encode_doc(&sc, &doc(edge, 3)));
        assert!(st.contains(edge));
        assert_eq!(st.ids(), vec![1, edge]);
    }
}
