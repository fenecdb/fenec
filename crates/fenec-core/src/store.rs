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
}

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
#[derive(Default)]
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
}

#[derive(Default)]
pub struct Segment {
    pub data: Vec<u8>,
    pub sealed: bool,
}

pub struct Store {
    segments: Vec<Segment>,
    index: IdIndex,
    next_id: DocId,
    /// Bytes held by deleted/overwritten records (compaction threshold).
    dead_bytes: usize,
    total_bytes: usize,
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
            index: IdIndex::default(),
            next_id: 1,
            dead_bytes: 0,
            total_bytes: 0,
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
    /// Bytes allocated by the offset index. Small next to the segment bytes,
    /// but a fixed per-document cost: in a collection of small documents it
    /// can reach a third of the total.
    pub fn index_bytes(&self) -> usize {
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
    /// Reserves room up front for a known record count.
    pub fn reserve(&mut self, n: usize) {
        self.index.reserve(n);
    }

    pub fn allocate_id(&mut self) -> DocId {
        let id = self.next_id;
        self.next_id += 1;
        id
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
                // per 8 MiB gives the rest back.
                last.data.shrink_to_fit();
            }
            self.segments.push(Segment::default());
        }
        self.segments.last_mut().unwrap()
    }

    /// Encodes a document in schema field order.
    pub fn encode_doc(schema: &Schema, doc: &Document) -> Vec<u8> {
        let mut payload = Vec::with_capacity(64);
        for f in &schema.fields {
            let v = doc.get(&f.name).cloned().unwrap_or(Value::Null);
            // The field type is passed along: `vector<N, f16>` halves in the record.
            encode_value_as(&mut payload, &v, Some(&f.ty));
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
        seg.data.extend_from_slice(&frame);
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

    /// Decodes the whole document.
    pub fn read(&self, schema: &Schema, id: DocId) -> Result<Option<Document>> {
        let Some(loc) = self.index.get(id) else {
            return Ok(None);
        };
        let buf = self.payload(loc)?;
        let mut pos = 0;
        let mut fields = Vec::with_capacity(schema.fields.len());
        for f in &schema.fields {
            fields.push((f.name.clone(), decode_value(buf, &mut pos)?));
        }
        Ok(Some(Document { id, fields }))
    }

    /// Decodes a single field; the fields before it are skipped without
    /// allocating. Filter evaluation and projection use this path.
    pub fn read_field(&self, id: DocId, field_pos: usize) -> Result<Option<Value>> {
        let Some(loc) = self.index.get(id) else {
            return Ok(None);
        };
        let buf = self.payload(loc)?;
        let mut pos = 0;
        for _ in 0..field_pos {
            skip_value(buf, &mut pos)?;
        }
        Ok(Some(decode_value(buf, &mut pos)?))
    }

    /// Decodes the fields at `positions` -- ascending -- into `out`, in one
    /// pass over the document that skips the others: an aggregate reads two
    /// or three fields of every row, and a `read_field` each would skip the
    /// fields before them once per field. `false` when there is no such
    /// document.
    pub fn read_fields(
        &self,
        id: DocId,
        positions: &[usize],
        out: &mut Vec<Value>,
    ) -> Result<bool> {
        let Some(loc) = self.index.get(id) else {
            return Ok(false);
        };
        let buf = self.payload(loc)?;
        out.clear();
        let mut pos = 0usize;
        let mut at = 0usize;
        for &want in positions {
            while at < want {
                skip_value(buf, &mut pos)?;
                at += 1;
            }
            out.push(decode_value(buf, &mut pos)?);
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
        for _ in 0..field_pos {
            crate::codec::skip_value(buf, &mut pos)?;
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
            let (words, _) = buf[pos..end].as_chunks::<2>();
            out.extend(
                words
                    .iter()
                    .map(|w| crate::codec::f32_from_f16(u16::from_le_bytes(*w))),
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
    pub fn replay_noting(&mut self, bytes: &[u8], note: &mut dyn FnMut(DocId)) -> Result<usize> {
        let mut pos = 0usize;
        let mut count = 0usize;
        while pos < bytes.len() {
            let op = bytes[pos];
            pos += 1;
            let id = get_uvarint(bytes, &mut pos)?;
            let len = get_uvarint(bytes, &mut pos)? as usize;
            if pos + len > bytes.len() {
                // Half-written last record: truncate and stop (crash-safe tail).
                break;
            }
            let payload = bytes[pos..pos + len].to_vec();
            pos += len;
            self.append(op, id, &payload);
            note(id);
            count += 1;
        }
        Ok(count)
    }

    /// Moves the live records into fresh segments and drops the tombstones.
    /// Returns the new full byte image (to be written over the file).
    pub fn compact(&mut self) -> Result<Vec<u8>> {
        let mut fresh = Store::new();
        fresh.next_id = self.next_id;
        let mut image = Vec::with_capacity(self.total_bytes - self.dead_bytes);
        let ids = self.index.ids();
        fresh.reserve(ids.len());
        for id in ids {
            let loc = self.index.get(id).unwrap();
            let payload = self.payload(loc)?.to_vec();
            image.extend_from_slice(&fresh.append(OP_PUT, id, &payload));
        }
        *self = fresh;
        Ok(image)
    }

    /// The live records in a fresh store, the dead ones left behind -- and
    /// `self` untouched, so the database goes on using it while a
    /// maintenance builds beside it.
    pub fn compacted(&self) -> Result<Store> {
        let mut fresh = Store::new();
        fresh.next_id = self.next_id;
        let ids = self.index.ids();
        fresh.reserve(ids.len());
        for id in ids {
            let loc = self.index.get(id).unwrap();
            fresh.append(OP_PUT, id, self.payload(loc)?);
        }
        Ok(fresh)
    }

    /// Byte image of the whole store (to persist or to move it).
    pub fn image(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.total_bytes);
        for s in &self.segments {
            out.extend_from_slice(&s.data);
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
        let image = st.compact().unwrap();
        assert_eq!(st.len(), 50);
        assert_eq!(st.dead_bytes(), 0);
        let mut st2 = Store::new();
        st2.replay(&image).unwrap();
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
