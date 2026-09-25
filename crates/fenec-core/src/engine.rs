//! Database engine: where the catalog, execution and persistence meet.

use crate::changes::{ChangeLog, Since, SCHEMA_MARK};
use crate::codec::{get_uvarint, put_uvarint};
use crate::collate::{self, Collation};
use crate::error::{Error, Result};
use crate::history::History;
use crate::plugin::{Plugin, Registry, WriteOp};
use crate::query::*;
use crate::schema::{collatable, IndexKind, Metric, Schema};
use crate::sorted::{Range as SortRange, SortedIndex};
use crate::sparse::SparseIndex;
use crate::store::{Store, OP_DEL, OP_PUT};
use crate::text::{best_first, TextIndex};
use crate::value::{DataType, DocId, Document, Value, VecPrec};
use crate::vector::{distance, dot, norm, normalized, score_from_distance, VectorIndex};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};

mod maintenance;

pub const MAGIC: &[u8; 8] = b"FENECDB\x01";

/// The most rows a `near` query may return (`limit + offset`).
///
/// The ANN candidate list is held in memory: `limit 1_000_000_000` means an
/// allocation of exactly that size. Hence the ceiling; when it is exceeded we
/// raise an error instead of truncating silently, because a truncated
/// similarity result is a wrong answer that looks right. Without a `limit`
/// the ceiling is applied as the default top-k.
pub const MAX_NEAR_ROWS: usize = 10_000;

/// The most rows a `match` query may return (`limit + offset`). Same ceiling
/// and the same reason as [`MAX_NEAR_ROWS`]: the top-k heap is sized from the
/// request, and a silently truncated relevance list is a wrong answer that
/// looks right.
pub const MAX_MATCH_ROWS: usize = 10_000;

/// How many `match` candidates `rerank` rescores when the query does not say.
///
/// Measured on BEIR (`make beir`): on FiQA (57 638 documents) 1 000
/// candidates come within 0.0003 of a full dense scan and 200 reach 98% of
/// it; on SciFact 50 already beat the full scan. 200 sits where the curve
/// has flattened on both, and costs 200 vector reads -- under 0.4% of that
/// corpus.
pub const DEFAULT_RERANK_CANDIDATES: usize = 200;

/// How many candidates each side of `fuse` ranks when the query does not
/// say (and never fewer than the page).
///
/// Measured on BEIR (`make beir`), nDCG@10 at 10 / 20 / 100 a side: SciFact
/// 0.701 / 0.699 / 0.687, FiQA 0.364 / 0.366 / 0.358. Up to 61 a side at
/// `k = 60`, a document both searches found outranks every document only
/// one found -- `2 / (60 + 61)` still beats `1 / 61` -- and deeper, agreement
/// in the middle of both lists starts to outvote the top of one. 20 is at or
/// within 0.003 of the best on both, and on FiQA answers in 0.78 ms against
/// 0.98 ms at 100.
pub const DEFAULT_FUSE_CANDIDATES: usize = 20;

/// The rank offset of `fuse`: 60, the value reciprocal rank fusion was
/// published with and the one most implementations keep. Measured on BEIR at
/// 20 a side, anything from 10 to 120 moved nDCG@10 by at most 0.005 on
/// either dataset, which is noise, so the published value stands.
pub const DEFAULT_FUSE_K: u32 = 60;

const REC_CREATE: u8 = 1;
const REC_DROP: u8 = 2;
const REC_DATA: u8 = 3;
/// Schema change (an index was added). Field names and types do not change,
/// only the index definition; stored documents therefore stay valid.
const REC_ALTER: u8 = 5;
/// The persisted HNSW graph. Derived data: when it cannot be validated it is
/// ignored and the index is rebuilt.
const REC_GRAPH: u8 = 4;
/// Change counter header -- **at the start of the image**, fixed width:
/// `[6][u64 LE seq][u64 LE body length]`.
///
/// Both decisions are deliberate:
///
/// - **At the start**, because at the end a corrupt or half-written tail
///   would break opening. The end of the file is exactly where the graph
///   stops, and the graph is *derived* data: if it breaks it is ignored and
///   rebuilt. A record erroring in that region would undo that tolerance.
/// - **Fixed width**, because the body length is only known once the body
///   has been written; the placeholder is filled in place. A uvarint would
///   change length and the whole image would have to be shifted.
///
/// The body length says "where this image's own records end": every record
/// after it was written *after* the checkpoint and carries the counter
/// forward (see [`Database::load`]). Older files without the record load
/// correctly too; there the counter is counted from zero.
const REC_SEQ: u8 = 6;
/// `[kind][u64][u64]`
const REC_SEQ_LEN: usize = 1 + 8 + 8;
/// The collection's id counter: `[7][collection-id][length][next_id]`.
///
/// The counter is normally derived from the records -- a replay takes one
/// more than the largest `OP_PUT` id it saw. Since `compact` throws away the
/// tombstones, that derivation is misleading there: the highest deleted id
/// disappears from the image entirely and **would be handed out again** on
/// the next open. A returning id silently binds everything holding on to it
/// (links handed out, a row in a subscriber's hands) to the wrong document;
/// a silent wrong answer is the most expensive kind of bug in this file.
///
/// Only `snapshot` writes it. It has no place on the WAL path: there the
/// tombstones are still present and carry the counter themselves.
const REC_NEXTID: u8 = 7;

/// Which history a database's writes belong to: `[8][0][length][following]
/// [count]{[id: u64 LE][from]}`. Not a write -- it moves no counter -- so a
/// replica never receives it as one; see [`History`].
const REC_HISTORY: u8 = crate::history::RECORD;

/// Whether the record at `at` is all there, rather than cut short where the
/// bytes end -- in its header or its body -- as a crash in the middle of an
/// append leaves the last one. Only the kinds written with a length are
/// judged: a kind this version does not know is the loader's to refuse, and
/// the counter header is only ever written by a rewrite, whole or not at all.
fn whole_record(bytes: &[u8], at: usize) -> Result<bool> {
    if !matches!(
        bytes[at],
        REC_CREATE | REC_DROP | REC_DATA | REC_GRAPH | REC_ALTER | REC_NEXTID | REC_HISTORY
    ) {
        return Ok(true);
    }
    let mut pos = at + 1;
    let mut len = 0;
    // The collection's id, then the body's length.
    for _ in 0..2 {
        len = match get_uvarint(bytes, &mut pos) {
            Ok(v) => v,
            Err(_) if pos >= bytes.len() => return Ok(false),
            Err(e) => return Err(e),
        };
    }
    Ok(len <= (bytes.len() - pos) as u64)
}

/// Where the last graph record of each index starts, by collection id and
/// field: the one a load restores. The walk reads record heads alone and
/// stops where the load would -- at a record cut short, or of a kind it
/// refuses -- so the load says what is wrong.
fn last_graphs(bytes: &[u8]) -> Result<Vec<(u32, String, usize)>> {
    let mut last: Vec<(u32, String, usize)> = Vec::new();
    let mut pos = MAGIC.len();
    while pos < bytes.len() && whole_record(bytes, pos)? {
        let at = pos;
        match bytes[pos] {
            REC_SEQ => {
                pos += REC_SEQ_LEN;
                continue;
            }
            REC_CREATE | REC_DROP | REC_DATA | REC_GRAPH | REC_ALTER | REC_NEXTID | REC_HISTORY => {
            }
            _ => break,
        }
        pos += 1;
        let cid = get_uvarint(bytes, &mut pos)? as u32;
        let len = get_uvarint(bytes, &mut pos)? as usize;
        if bytes[at] == REC_GRAPH {
            let mut cp = 0usize;
            let field = crate::codec::decode_str(&bytes[pos..pos + len], &mut cp)?;
            match last.iter_mut().find(|(c, f, _)| *c == cid && *f == field) {
                Some(l) => l.2 = at,
                None => last.push((cid, field, at)),
            }
        }
        pos += len;
    }
    Ok(last)
}

/// Drops from a load's restored graphs those of collection `name` but the
/// fields `keep` names. One function rather than a `retain` at each record
/// kind: each closure was a copy of `retain`, 264 bytes of the browser
/// module apiece.
fn forget(restored: &mut Vec<(String, String, usize)>, name: &str, keep: &dyn Fn(&str) -> bool) {
    restored.retain(|(n, f, _)| n != name || keep(f));
}

/// Whether `ix` is due a record of its own in a file `appended` bytes long,
/// by `(changes, growth)` ([`Database::save_graphs`]).
fn graph_due(ix: &VectorIndex, appended: u64, (changes, growth): (u64, u64)) -> bool {
    if ix.unlinked() > 0 || ix.is_empty() {
        return false;
    }
    let p = ix.persisted();
    let node_bytes = match p.node_bytes.load(Relaxed) {
        0 => GRAPH_NODE_BYTES,
        b => b,
    };
    ix.changes().saturating_sub(p.changes.load(Relaxed)) >= changes
        && appended.saturating_sub(p.at.load(Relaxed)) >= growth * node_bytes * ix.len() as u64
}

/// Takes a data record's frames into a collection's store: the record's
/// offset in the file, its bytes, and what to tell of each document's id.
type Replay<'a> = dyn FnMut(&mut Store, usize, &[u8], &mut dyn FnMut(DocId)) -> Result<usize> + 'a;

/// Where an image is written: the file being rewritten, or a buffer. The
/// counter header's body length is only known once the body is out, so it is
/// patched where it stands rather than the image being written twice.
pub trait ImageOut {
    fn write(&mut self, bytes: &[u8]) -> Result<()>;
    /// Bytes written so far, which is where the next one lands.
    fn at(&self) -> u64;
    /// Overwrites bytes written earlier, in place.
    fn patch(&mut self, at: u64, bytes: &[u8]) -> Result<()>;
}

impl ImageOut for Vec<u8> {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.extend_from_slice(bytes);
        Ok(())
    }
    fn at(&self) -> u64 {
        self.len() as u64
    }
    fn patch(&mut self, at: u64, bytes: &[u8]) -> Result<()> {
        let at = at as usize;
        self[at..at + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }
}

/// The persistence layer. The engine only says "append these bytes"; where
/// they are written (file, IndexedDB, OPFS, S3) is this layer's problem.
pub trait Sink: Send {
    fn append(&mut self, bytes: &[u8]) -> Result<()>;
    /// Appends a write, the `seq`th of the change counter. A sink that
    /// passes writes on -- a primary's feed to its replicas -- numbers them
    /// by it; everything else only appends. Records that are not writes
    /// (the history) come through [`Self::append`] and carry no number.
    fn record(&mut self, seq: u64, bytes: &[u8]) -> Result<()> {
        let _ = seq;
        self.append(bytes)
    }
    fn rewrite(&mut self, bytes: &[u8]) -> Result<()>;
    /// [`Self::rewrite`] with the image written into the file as it is
    /// produced rather than built in memory first: a checkpoint of a 1 GB
    /// database held the whole image beside the data before this. A sink
    /// with nowhere to stream to -- the browser's -- builds it and rewrites.
    fn rewrite_with(
        &mut self,
        image: &mut dyn FnMut(&mut dyn ImageOut) -> Result<()>,
    ) -> Result<()> {
        let mut buf: Vec<u8> = Vec::new();
        image(&mut buf)?;
        self.rewrite(&buf)
    }
    /// The file this sink holds, mapped read-only: what a database whose
    /// records are in a mapping points at after a rewrite, so that the file
    /// it just wrote is the one it reads -- and the old one, unlinked by the
    /// rename, is let go of. `None` for every sink but a file's.
    #[cfg(not(target_arch = "wasm32"))]
    fn remapped(&self) -> Option<crate::store::Base> {
        None
    }
    /// Where a rewrite beside the database writes the file that will take
    /// this one's place ([`Self::adopt`]): a `compact` on a server, written
    /// with no lock held. `None` for every sink but a file's, and the
    /// rewrite then holds the write lock ([`Self::rewrite_with`]).
    #[cfg(not(target_arch = "wasm32"))]
    fn side(&self) -> Option<std::path::PathBuf> {
        None
    }
    /// Puts the file at `side`, an image of every write so far and fsynced,
    /// in place of this one, and appends to it from then on.
    #[cfg(not(target_arch = "wasm32"))]
    fn adopt(&mut self, side: &std::path::Path) -> Result<()> {
        Err(Error::Io(format!(
            "{} has no file to take the place of",
            side.display()
        )))
    }
    /// Pushes to disk what an earlier process wrote and never synced: a
    /// primary calls it before it tells a replica those bytes exist.
    fn sync_existing(&mut self) -> Result<()> {
        Ok(())
    }
    fn sync(&mut self) -> Result<()> {
        Ok(())
    }
    /// `sync` in two halves: hands the buffered writes to the operating
    /// system now, and returns what makes them durable, for the caller to
    /// run once it has let go of the database -- a server's readers then do
    /// not wait on the disk. `None` when this already made them durable.
    fn flush(&mut self) -> Result<Option<Durability>> {
        self.sync()?;
        Ok(None)
    }
}

/// What makes the writes a [`Sink::flush`] handed over durable: an fsync on
/// the file they went to.
pub type Durability = Box<dyn FnOnce() -> Result<()> + Send>;

/// The party that wants to hear that a write happened.
///
/// [`Sink`] receives the raw bytes (where they land is its problem);
/// `Watcher` only receives "the counter reached this point". They differ
/// because their purposes do: a sink persists, a watcher *wakes up*.
///
/// fenec-core therefore never defines a waiting primitive: choosing between
/// `Condvar` and polling is the server's call -- the core also compiles to
/// WASM, where there are no threads. `fenec-http` wires this mark to a Condvar
/// through `Hub`; in a database with no subscribers the cost is zero.
pub trait Watcher: Send + Sync {
    fn notify(&self, seq: u64);
}

/// `[kind][collection][length]`, the head every record but the counter has.
fn record_head(kind: u8, cid: u32, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    out.push(kind);
    put_uvarint(&mut out, cid as u64);
    put_uvarint(&mut out, len as u64);
    out
}

/// A sink that writes nowhere (pure in-memory / browser session).
pub struct NullSink;
impl Sink for NullSink {
    fn append(&mut self, _bytes: &[u8]) -> Result<()> {
        Ok(())
    }
    fn rewrite(&mut self, _bytes: &[u8]) -> Result<()> {
        Ok(())
    }
}

/// A `@hash` index: a value's encoding -> the documents holding it.
///
/// It counts the bytes its keys and buckets hold as they change, so
/// `memory_bytes` -- which `--max-memory` asks before every write that
/// grows the data -- reads a number rather than walking every bucket. A
/// bucket its last document leaves goes with it: kept, the keys of values
/// that come and go (a token, a session id) piled up until the file was
/// opened again.
#[derive(Default)]
pub struct HashIndex {
    map: HashMap<Vec<u8>, Vec<DocId>>,
    heap: usize,
}

// The keys are taken as `&Vec<u8>`, the type the map holds, rather than
// `&[u8]`: looked up as a slice, the map's search and hash were compiled a
// second time, 650 bytes of the browser module.
#[allow(clippy::ptr_arg)]
impl HashIndex {
    /// The documents under `key`, in the order they were added.
    pub fn get(&self, key: &Vec<u8>) -> Option<&Vec<DocId>> {
        self.map.get(key)
    }

    /// The distinct values held.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn add(&mut self, key: Vec<u8>, id: DocId) {
        let key_bytes = key.capacity();
        let bucket = self.map.entry(key).or_default();
        let before = bucket.capacity();
        if before == 0 {
            self.heap += key_bytes;
        }
        bucket.push(id);
        self.heap += (bucket.capacity() - before) * std::mem::size_of::<DocId>();
    }

    fn remove(&mut self, key: &Vec<u8>, id: DocId) {
        let Some(bucket) = self.map.get_mut(key) else {
            return;
        };
        bucket.retain(|d| *d != id);
        if bucket.is_empty() {
            if let Some((k, b)) = self.map.remove_entry(key) {
                self.heap -= k.capacity() + b.capacity() * std::mem::size_of::<DocId>();
            }
        }
    }

    fn clear(&mut self) {
        *self = HashIndex::default();
    }

    /// The table, the keys and the buckets as they sit in memory.
    pub fn memory_bytes(&self) -> usize {
        self.map.capacity() * (std::mem::size_of::<(Vec<u8>, Vec<DocId>)>() + 1) + self.heap
    }
}

pub struct Collection {
    pub id: u32,
    pub schema: Schema,
    pub store: Store,
    /// field name -> HNSW index
    pub vectors: HashMap<String, VectorIndex>,
    /// field name -> hash index
    pub hashes: HashMap<String, HashIndex>,
    /// field name -> inverted index
    pub texts: HashMap<String, TextIndex>,
    /// field name -> ordered index, in schema order. A `Vec` rather than a
    /// map: a collection has a handful of ordered fields, the map's code was
    /// 2.6 KB of the browser module, and a fixed order keeps the choice
    /// between two ranges the same from one run to the next.
    pub sorted: Vec<(String, SortedIndex)>,
    /// field name -> inverted index over a sparse vector, in schema order,
    /// a `Vec` for the reason `sorted` is one.
    pub sparse: Vec<(String, SparseIndex)>,
}

impl Collection {
    #[cfg_attr(
        not(all(
            feature = "vector",
            feature = "text",
            feature = "sparse",
            feature = "sorted"
        )),
        allow(unused_mut)
    )]
    fn new(id: u32, schema: Schema) -> Collection {
        let mut vectors = HashMap::new();
        let mut hashes = HashMap::new();
        let mut texts = HashMap::new();
        let mut sorted = Vec::new();
        let mut sparse = Vec::new();
        for f in &schema.fields {
            match (&f.index, &f.ty) {
                #[cfg(feature = "vector")]
                (IndexKind::Vector(spec), DataType::Vector(dim, prec)) => {
                    vectors.insert(
                        f.name.clone(),
                        VectorIndex::with_precision(*dim, *spec, *prec),
                    );
                }
                (IndexKind::Hash, _) => {
                    hashes.insert(f.name.clone(), HashIndex::default());
                }
                #[cfg(feature = "text")]
                (IndexKind::Text(spec), DataType::Text) => {
                    texts.insert(f.name.clone(), TextIndex::new(*spec));
                }
                #[cfg(feature = "sorted")]
                (IndexKind::Sorted, ty) if SortedIndex::supports(ty) => {
                    sorted.push((f.name.clone(), SortedIndex::new(ty, f.collate)));
                }
                #[cfg(feature = "sparse")]
                (IndexKind::Inverted, DataType::Sparse(_)) => {
                    sparse.push((f.name.clone(), SparseIndex::new()));
                }
                _ => {}
            }
        }
        Collection {
            id,
            schema,
            store: Store::new(),
            vectors,
            hashes,
            texts,
            sorted,
            sparse,
        }
    }

    /// Rebuilds the index structures from the schema's index definitions
    /// (contents empty; filling them is `rebuild_indexes_with`'s job).
    ///
    /// Inlined, as the other two functions [`Database::apply`] shares with the
    /// write path are: the browser module links no `apply`, and with a
    /// second caller they were no longer inlined into their first -- 1.1 KB
    /// of the module for nothing it runs.
    #[inline(always)]
    fn reset_index_structures(&mut self) {
        self.vectors.clear();
        self.hashes.clear();
        self.texts.clear();
        self.sorted.clear();
        self.sparse.clear();
        for f in &self.schema.fields {
            match (&f.index, &f.ty) {
                #[cfg(feature = "vector")]
                (IndexKind::Vector(spec), DataType::Vector(dim, prec)) => {
                    self.vectors.insert(
                        f.name.clone(),
                        VectorIndex::with_precision(*dim, *spec, *prec),
                    );
                }
                (IndexKind::Hash, _) => {
                    self.hashes.insert(f.name.clone(), HashIndex::default());
                }
                #[cfg(feature = "text")]
                (IndexKind::Text(spec), DataType::Text) => {
                    self.texts.insert(f.name.clone(), TextIndex::new(*spec));
                }
                #[cfg(feature = "sorted")]
                (IndexKind::Sorted, ty) if SortedIndex::supports(ty) => {
                    self.sorted
                        .push((f.name.clone(), SortedIndex::new(ty, f.collate)));
                }
                #[cfg(feature = "sparse")]
                (IndexKind::Inverted, DataType::Sparse(_)) => {
                    self.sparse.push((f.name.clone(), SparseIndex::new()));
                }
                _ => {}
            }
        }
    }

    pub fn sparse_index(&self, field: &str) -> Option<&SparseIndex> {
        self.sparse
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, ix)| ix)
    }

    pub fn sorted_index(&self, field: &str) -> Option<&SortedIndex> {
        self.sorted
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, ix)| ix)
    }

    /// Puts `doc` into the indexes, `old` the version it replaces. Inlined
    /// for the reason [`Self::reset_index_structures`] is: a compact beside
    /// the database calls it too.
    #[inline(always)]
    fn index_doc(&mut self, doc: &Document, old: Option<&Document>) {
        for (name, ix) in self.vectors.iter_mut() {
            if let Some(Value::Vector(v)) = doc.get(name) {
                ix.insert(doc.id, v);
            }
        }
        self.index_scalar(doc, old);
    }

    /// Hash, text, ordered and sparse indexes; vectors are left to the batch
    /// path. A field `old` held as it is, `unindex_doc` left in place, and
    /// it is left here too.
    fn index_scalar(&mut self, doc: &Document, old: Option<&Document>) {
        let kept = |name: &str| old.is_some_and(|o| same(o.get(name), doc.get(name)));
        for (name, ix) in self.hashes.iter_mut() {
            if let Some(v) = doc.get(name).filter(|_| !kept(name)) {
                ix.add(hash_key(v), doc.id);
            }
        }
        for (name, ix) in self.texts.iter_mut() {
            if let Some(Value::Text(t)) = doc.get(name).filter(|_| !kept(name)) {
                ix.insert(doc.id, t);
            }
        }
        for (name, ix) in self.sorted.iter_mut() {
            if !kept(name) {
                ix.insert(doc.id, doc.get(name));
            }
        }
        for (name, ix) in self.sparse.iter_mut() {
            if let Some(Value::Sparse(_, e)) = doc.get(name).filter(|_| !kept(name)) {
                ix.insert(doc.id, e);
            }
        }
    }

    /// Bulk-inserts every vector in a batch, field by field.
    ///
    /// Calling `insert` one at a time missed the chance to parallelise the
    /// read-only part of the HNSW build; the batch path takes it.
    #[inline(always)]
    fn index_vectors_batch(&mut self, docs: &[Document]) {
        for (name, ix) in self.vectors.iter_mut() {
            let items: Vec<(DocId, Vec<f32>)> = docs
                .iter()
                .filter_map(|d| match d.get(name) {
                    Some(Value::Vector(v)) => Some((d.id, v.clone())),
                    _ => None,
                })
                .collect();
            if !items.is_empty() {
                ix.insert_batch(&items);
            }
        }
    }

    /// Takes the stored `doc` out of the indexes: out of all of them for a
    /// deletion, and for a rewrite into `new` out of those whose field
    /// changes. An update of one field took the document out of every index
    /// and put it back -- its vector included, 1.89 ms at 20 000 x 768 and a
    /// tombstone each time. A vector stays while `new` has one in its field:
    /// `insert` keeps the node that holds it or retires it for the new one.
    fn unindex_doc(&mut self, doc: &Document, new: Option<&Document>) {
        let kept = |name: &str| new.is_some_and(|n| same(doc.get(name), n.get(name)));
        for (name, ix) in self.vectors.iter_mut() {
            let replaced = new.is_some_and(|n| matches!(n.get(name), Some(Value::Vector(_))));
            if doc.get(name).is_some() && !replaced {
                ix.remove(doc.id);
            }
        }
        for (name, ix) in self.hashes.iter_mut() {
            if let Some(v) = doc.get(name).filter(|_| !kept(name)) {
                ix.remove(&hash_key(v), doc.id);
            }
        }
        // Every caller reads the *stored* document before unindexing, so the
        // terms here are the ones that went in.
        for (name, ix) in self.texts.iter_mut() {
            if let Some(Value::Text(t)) = doc.get(name).filter(|_| !kept(name)) {
                ix.remove(doc.id, t);
            }
        }
        for (name, ix) in self.sorted.iter_mut() {
            if !kept(name) {
                ix.remove(doc.id, doc.get(name));
            }
        }
        for (name, ix) in self.sparse.iter_mut() {
            if let Some(Value::Sparse(_, e)) = doc.get(name).filter(|_| !kept(name)) {
                ix.remove(doc.id, e);
            }
        }
    }

    pub fn stats(&self) -> CollectionStats {
        CollectionStats {
            name: self.schema.name.clone(),
            documents: self.store.len(),
            bytes: self.store.total_bytes(),
            dead_bytes: self.store.dead_bytes(),
            segments: self.store.segment_count(),
            vector_indexes: self
                .vectors
                .iter()
                .map(|(k, v)| VectorIndexStats {
                    field: k.clone(),
                    count: v.len(),
                    dim: v.dim,
                    arena_bytes: v.arena_bytes(),
                    precision: v.precision(),
                })
                .collect(),
            text_indexes: self
                .texts
                .iter()
                .map(|(k, t)| TextIndexStats {
                    field: k.clone(),
                    count: t.len(),
                    terms: t.terms(),
                    postings: t.postings_count(),
                    bytes: t.memory_bytes(),
                })
                .collect(),
        }
    }
}

/// Result of [`Database::changes_since`].
#[derive(Debug, Clone, PartialEq)]
pub enum Changes {
    /// The cursor cannot be caught up incrementally: reseed the subscriber.
    Reseed,
    Batch(ChangeBatch),
}

/// The difference after a cursor, expressed as state.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeBatch {
    /// The end point this answer covers: the subscriber moves its cursor here.
    pub seq: u64,
    /// Rows that exist now and match the shape (new, updated, or newly
    /// *entering* the shape). Applying them is an upsert.
    pub puts: ResultSet,
    /// Ids that no longer exist or have left the shape.
    pub dels: Vec<DocId>,
    /// The collection's schema changed (an index was added, dropped or
    /// recreated): the subscriber must re-read the schema.
    pub schema_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionStats {
    pub name: String,
    pub documents: usize,
    pub bytes: usize,
    pub dead_bytes: usize,
    pub segments: usize,
    pub vector_indexes: Vec<VectorIndexStats>,
    pub text_indexes: Vec<TextIndexStats>,
}

/// State of a single vector index. The arena size scales directly with the
/// precision (`f16` halves it), so it has to be measurable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorIndexStats {
    pub field: String,
    pub count: usize,
    pub dim: usize,
    pub arena_bytes: usize,
    pub precision: VecPrec,
}

/// State of a single full-text index. `postings` is the number that matters
/// for both memory and query cost: a `match` walks the lists of its terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextIndexStats {
    pub field: String,
    pub count: usize,
    pub terms: usize,
    pub postings: usize,
    pub bytes: usize,
}

/// The candidate set for a filter on `id`, or `None` when the index cannot
/// answer it.
///
/// `id` is not a schema field -- `Schema::new` reserves the name -- so it has
/// no hash bucket and had no plan at all: `where id = 42` walked every
/// document in the collection to compare one integer. Measured over 20 000
/// documents, a `set ... where id = N` took 424 us against 22 us for the
/// same write through a `@hash` mirror of the id, which is why the docs used
/// to recommend keeping one. The store's own id index answers it directly.
///
/// A value the id space cannot express (`id = "abc"`) returns `None` and
/// leaves the decision to the eval path, the same rule a hash lookup follows:
/// the presence of an index is never allowed to change an answer. A numeric
/// value out of range simply matches nothing, which is what comparing it to
/// `row.id()` would have concluded anyway.
fn id_candidates(store: &Store, vals: &[&Value]) -> Option<Vec<DocId>> {
    let mut out = Vec::with_capacity(vals.len());
    for v in vals {
        // The same coercion a hash lookup performs: `id = 42.0` has to find
        // document 42, because `cmp_value` says they are equal.
        let Ok(Value::Int(i)) = (*v).clone().coerce(&DataType::Int) else {
            return None;
        };
        if let Ok(id) = DocId::try_from(i) {
            if store.contains(id) {
                out.push(id);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    Some(out)
}

/// A value's bucket key: its encoding, a float's -0.0 filed as 0.0. The scan
/// finds the two equal -- PostgreSQL's float hash hashes -0 as 0 for the
/// same reason -- and under a key of its own a stored -0.0 was missed by
/// `price = 0.0` through the index alone. The document keeps the value as
/// it was written. A -0.0 inside a list or a vector keeps its sign: folding
/// those too was 570 bytes of the browser module, for a hash index on such
/// a field meeting a -0.0.
fn hash_key(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    match v {
        // Adding +0.0 turns -0.0 into 0.0 and leaves every other float alone.
        Value::Float(f) => crate::codec::encode_value(&mut out, &Value::Float(f + 0.0)),
        _ => crate::codec::encode_value(&mut out, v),
    }
    out
}

/// The feature `kind` needs when this build was made without it
/// (`Cargo.toml`): a file declaring such an index opens all the same, the
/// index unbuilt, and what needs it is refused.
fn missing_feature(kind: &IndexKind) -> Option<&'static str> {
    match kind {
        IndexKind::Vector(_) if !cfg!(feature = "vector") => Some("vector"),
        IndexKind::Text(_) if !cfg!(feature = "text") => Some("text"),
        IndexKind::Inverted if !cfg!(feature = "sparse") => Some("sparse"),
        IndexKind::Sorted if !cfg!(feature = "sorted") => Some("sorted"),
        _ => None,
    }
}

fn not_built(what: &str, feature: &str) -> Error {
    Error::Query(format!(
        "{what} needs the `{feature}` feature, which this build was made without"
    ))
}

/// Whether this build has every index, which is where the checks for one
/// it lacks fold away.
const EVERY_INDEX: bool = cfg!(all(
    feature = "vector",
    feature = "text",
    feature = "sparse",
    feature = "sorted"
));

/// The refusal of a `near` or a `match` over `field` when it declares an
/// index this build was made without, ahead of the one for a field that
/// declares none -- whose message stays whole at each call site: put
/// together here from parts passed in, it cost the module with every index
/// 197 bytes brotli. That module asks nothing, since the lookup of the field
/// is a loop the compiler cannot prove ends, and it would stay in.
fn not_built_on(c: &Collection, field: &str, what: &str) -> Option<Error> {
    if EVERY_INDEX {
        return None;
    }
    let feature = missing_feature(&c.schema.field(field)?.index)?;
    Some(not_built(&format!("field `{field}`'s {what}"), feature))
}

/// Whether an index files `a` and `b` as one entry: the same hash key, which
/// every index's own key follows. `==` would call a NaN changed that files
/// where it did, and was 570 bytes of the browser module besides.
fn same(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => hash_key(a) == hash_key(b),
        _ => false,
    }
}

/// The collation data the text of `doc`'s collated fields needs and the
/// browser module has not been handed, a bit a chunk (collate.rs).
fn collation_missing(schema: &Schema, doc: &Document) -> u32 {
    schema
        .fields
        .iter()
        .filter(|f| f.collate.is_some())
        .fold(0, |m, f| {
            m | doc.get(&f.name).map_or(0, collate::missing_in)
        })
}

/// One resolved level of a `lookup` chain: the parts that do not depend on
/// a row, plus the collection the key is read *from*.
///
/// `parent` is the level above -- the driving collection at depth 0, the
/// level's own child collection one step down. That is the only thing a
/// chained level needs that a single one did not: which store the key comes
/// out of.
struct Step<'a> {
    l: &'a Lookup,
    parent: &'a Collection,
    child: &'a Collection,
    probe: Probe<'a>,
    /// Position of the key field in `parent`'s schema; `None` for `id`.
    parent_pos: Option<usize>,
}

impl Step<'_> {
    /// The value a row of the level above probes with.
    fn key(&self, id: DocId) -> Result<Value> {
        Ok(match self.parent_pos {
            None => Value::Int(id as i64),
            Some(p) => self.parent.store.read_field(id, p)?.unwrap_or(Value::Null),
        })
    }

    /// Whether `id` passes this level's own `where`.
    fn passes(&self, id: DocId, ctx: &EvalCtx) -> Result<bool> {
        let Some(f) = &self.l.filter else {
            return Ok(true);
        };
        let mut r = StoreRow {
            store: &self.child.store,
            schema: &self.child.schema,
            id,
            memo: Vec::new(),
        };
        Ok(truthy(&eval(f, &mut r, ctx)?))
    }
}

/// Whether a row at `depth` survives -- it passes that level's `where`, and
/// if the level below is `required`, it has at least one row there that
/// survives in turn.
///
/// `required` is a statement about its own level: it drops rows of the level
/// *above* it. So a review can be lost to an author that is not there, and a
/// product then loses that review -- but only if the product's own level
/// asked for `required` too. Nothing acts at a distance; the recursion is
/// what makes the local rule compose.
///
/// It stops at the first survivor at every level, so a row with a thousand
/// descendants costs what one with a single descendant costs -- the same
/// property the single-level check has, carried down.
fn survives(steps: &[Step], depth: usize, id: DocId, ctx: &EvalCtx) -> Result<bool> {
    if !steps[depth].passes(id, ctx)? {
        return Ok(false);
    }
    let Some(next) = steps.get(depth + 1).filter(|n| n.l.required) else {
        return Ok(true);
    };
    let key = next.key(id)?;
    next.probe
        .any(next.child, &key, |g| survives(steps, depth + 1, g, ctx))
}

/// How `lookup` addresses the child collection.
///
/// Both arms are one lookup per parent. There is deliberately no "scan the
/// child collection" arm: `near` refuses an unindexed field and names
/// `@hnsw`, `match` refuses one and names `@text`, and the same rule keeps
/// `lookup` from being an index probe in one query and a full scan in the
/// next with nothing in the text telling them apart. The gap is not small --
/// the scan was measured at 12.98 ms against 0.092 ms for the indexed
/// equality over the same 200 000 rows.
enum Probe<'a> {
    /// The key is `id`, so the parent's value is already the address. No
    /// index exists or could -- `Schema::new` reserves `id` and refuses it
    /// as a declared field -- and none is needed. This is the foreign key to
    /// primary key case, which is what makes refusing the unindexed one
    /// tenable rather than obstructive.
    Id,
    /// A `@hash` bucket, borrowed for the whole query.
    Hash(&'a HashIndex, &'a DataType),
}

impl<'a> Probe<'a> {
    fn resolve(child: &'a Collection, field: &str, name: &str) -> Result<Probe<'a>> {
        if field == "id" {
            return Ok(Probe::Id);
        }
        let Some(fd) = child.schema.field(field) else {
            return Err(Error::NotFound(format!("field `{name}.{field}`")));
        };
        match child.hashes.get(field) {
            Some(map) => Ok(Probe::Hash(map, &fd.ty)),
            None => Err(Error::Query(format!(
                "`lookup` on `{name}.{field}` needs a hash index (declare it with @hash)"
            ))),
        }
    }

    /// The type the parent's key is compared against.
    fn key_type(&self) -> DataType {
        match self {
            Probe::Id => DataType::Int,
            Probe::Hash(_, ty) => (*ty).clone(),
        }
    }

    /// Whether any live child under `key` satisfies `pred`.
    ///
    /// `ids` below materialises the bucket -- filtered, sorted, deduped --
    /// because the collecting pass needs the children in order. The
    /// existence pass behind `required` does not: it needs one witness, so
    /// it walks the bucket as it lies and stops at the first. Order and
    /// duplicates cannot change the answer to "is there one".
    ///
    /// On a parent holding 56 374 children that is the difference between
    /// sorting all of them and reading a handful.
    fn any(
        &self,
        child: &Collection,
        key: &Value,
        mut pred: impl FnMut(DocId) -> Result<bool>,
    ) -> Result<bool> {
        if key.is_null() {
            return Ok(false);
        }
        match self {
            Probe::Id => {
                if let Ok(Value::Int(i)) = key.clone().coerce(&DataType::Int) {
                    if let Ok(id) = DocId::try_from(i) {
                        if child.store.contains(id) {
                            return pred(id);
                        }
                    }
                }
                Ok(false)
            }
            Probe::Hash(map, ty) => {
                let Ok(k) = key.clone().coerce(ty) else {
                    return Ok(false);
                };
                let Some(ids) = map.get(&hash_key(&k)) else {
                    return Ok(false);
                };
                for &id in ids {
                    if child.store.contains(id) && pred(id)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }

    /// Fills `out` with the child ids matching `key`: ascending, alive and
    /// free of duplicates.
    fn ids(&self, child: &Collection, key: &Value, out: &mut Vec<DocId>) {
        out.clear();
        // A NULL key matches nothing, not even a stored NULL. A field left
        // out of a document is written as NULL and does get a bucket, so
        // without this guard every document missing the key would attach to
        // every other one -- a cross product over exactly the rows that
        // carry no information. It disagrees with `eval`, where `NULL =
        // NULL` is true; `on` is not `where`, and this is the rule SQL
        // settled on for the same reason.
        if key.is_null() {
            return;
        }
        match self {
            Probe::Id => {
                if let Ok(Value::Int(i)) = key.clone().coerce(&DataType::Int) {
                    if i > 0 && child.store.contains(i as DocId) {
                        out.push(i as DocId);
                    }
                }
            }
            Probe::Hash(map, ty) => {
                // The same conversion the write path used to build the
                // bucket key, for the reason spelled out in `matching_ids`:
                // an `int` key has to become `10.0` before it can find
                // `price = 10.0`. Skipping it lets the mere presence of an
                // index change the answer.
                let Ok(k) = key.clone().coerce(ty) else {
                    return;
                };
                let Some(ids) = map.get(&hash_key(&k)) else {
                    return;
                };
                // A bucket can hold ids whose document is gone.
                out.extend(ids.iter().copied().filter(|id| child.store.contains(*id)));
                // Insertion order is not id order -- an upsert removes an id
                // and pushes it back at the end -- so the children would
                // otherwise reshuffle after a rewrite that changed nothing.
                out.sort_unstable();
                // A duplicate here is not one extra row but one extra child
                // *per parent*. The pass is free on a list this short.
                out.dedup();
            }
        }
    }
}

/// The two `lookup` key types must be identical, or both numeric.
///
/// The probe is a bucket lookup keyed by `encode_value(coerce(key, ty))`,
/// while the equality it stands for is `cmp_value` -- two different
/// cross-type rulebooks. They diverge in one reachable place: a `timestamp`
/// parent key against a `text @hash` child key, where `coerce` renders
/// ISO-8601 and so only finds text spelled that exact way, while `cmp_value`
/// parses the text and calls them equal. Refusing the mismatch closes that
/// by construction instead of special-casing the one pair known to differ
/// today. The cost is refusing two pairs that would have agreed.
fn check_key_types(parent: &DataType, child: &DataType, l: &Lookup) -> Result<()> {
    let numeric = |t: &DataType| matches!(t, DataType::Int | DataType::Float | DataType::Timestamp);
    if matches!(parent, DataType::Vector(..)) || matches!(child, DataType::Vector(..)) {
        return Err(Error::Type(
            "a vector cannot be a `lookup` key: float equality is a coincidence, not a match"
                .into(),
        ));
    }
    if parent == child || (numeric(parent) && numeric(child)) {
        return Ok(());
    }
    Err(Error::Type(format!(
        "`lookup` key types do not match: `{}` is {}, `{}.{}` is {}",
        l.parent_field,
        parent.name(),
        l.collection,
        l.child_field,
        child.name()
    )))
}

/// Lazy field access over the store. When the same field is asked for again
/// the decoded value is reused.
struct StoreRow<'a> {
    store: &'a Store,
    schema: &'a Schema,
    id: DocId,
    memo: Vec<(usize, Value)>,
}

impl<'a> RowAccess for StoreRow<'a> {
    fn id(&self) -> DocId {
        self.id
    }
    fn field(&mut self, name: &str) -> Result<Value> {
        let pos = self
            .schema
            .field_pos(name)
            .ok_or_else(|| Error::NotFound(format!("field `{name}`")))?;
        if let Some((_, v)) = self.memo.iter().find(|(p, _)| *p == pos) {
            return Ok(v.clone());
        }
        let v = self.store.read_field(self.id, pos)?.unwrap_or(Value::Null);
        self.memo.push((pos, v.clone()));
        Ok(v)
    }
    fn collation(&self, name: &str) -> Option<Collation> {
        self.schema.field(name).and_then(|f| f.collate)
    }
}

/// Access that uses the document as its source (on the put/update path).
struct DocRow<'a>(&'a Document, &'a Schema);
impl<'a> RowAccess for DocRow<'a> {
    fn id(&self) -> DocId {
        self.0.id
    }
    fn field(&mut self, name: &str) -> Result<Value> {
        Ok(self.0.get(name).cloned().unwrap_or(Value::Null))
    }
    fn collation(&self, name: &str) -> Option<Collation> {
        self.1.field(name).and_then(|f| f.collate)
    }
}

/// For expressions with no field reference (literal/parameter/function).
struct NoRow;
impl RowAccess for NoRow {
    fn id(&self) -> DocId {
        0
    }
    fn field(&mut self, name: &str) -> Result<Value> {
        Err(Error::Query(format!(
            "field `{name}` cannot be accessed in this context"
        )))
    }
}

pub struct Database {
    collections: HashMap<String, Collection>,
    order: Vec<String>,
    next_coll_id: u32,
    registry: Registry,
    /// `Mutex` not for locking but for `Sync`: `Sink` is only required to be
    /// `Send` (a sink carrying a JS handle on the browser side cannot be
    /// `Sync`), but the server expects `Sync` so it can share `Database`
    /// under an `RwLock`. Access always happens through `get_mut` on
    /// `&mut self`: there is no locking cost.
    sink: Mutex<Box<dyn Sink>>,
    /// Whether writes are still not on disk before `sync` is called. The
    /// periodic syncer on the server side skips idle passes with this flag.
    dirty: bool,
    /// The first storage error, once the sink has refused an append, a sync
    /// or a rewrite. From then on every write is refused until the file is
    /// reopened. After a failed `fsync` the kernel may already have dropped
    /// the dirty pages, so a retry that succeeds proves nothing (PostgreSQL
    /// learned this in 2018 and panics instead); and after a failed append the
    /// memory holds a write the file does not. Reads go on: they answer from
    /// memory, which is what every client was told so far.
    failed: Option<String>,
    /// Ring buffer of changed ids. Incremental feeding of replicas goes
    /// through here; see [`crate::changes`].
    changes: ChangeLog,
    /// The party to wake after a write (if any).
    watcher: Option<Arc<dyn Watcher>>,
    /// Which history the writes belong to, and whether they come from a
    /// primary; see [`History`].
    history: History,
    /// Whether the documents are read from a mapped file rather than held in
    /// memory (`fs::open`). It outlives the stores it was set for: a
    /// rewrite, a compaction and an image adopted all end with the stores
    /// pointed at the file again.
    #[cfg(not(target_arch = "wasm32"))]
    mapped: bool,
    /// The writes a `create index` or `compact` running beside the database
    /// has to catch up with once it is built (see `maintenance`). A `Mutex`
    /// for the reason `sink` is one: it is registered under the read lock,
    /// and the write path reaches it through `get_mut`.
    tails: Mutex<maintenance::Tails>,
    /// Whether `tails` holds any: the one thing every write looks at.
    watched: std::sync::atomic::AtomicBool,
    /// Images adopted: each replaces the contents wholesale, possibly at
    /// the change the database already stood at.
    adoptions: u64,
    /// Whether the next load leaves the vectors it would link into a graph
    /// for [`Database::link_pending`] (see [`Database::defer_linking`]).
    defer_links: bool,
    /// The bytes of the file as this database has written it: what it
    /// opened, and every record appended since. An atomic because a graph
    /// record is appended under the read lock ([`Database::save_graphs`]).
    appended: std::sync::atomic::AtomicU64,
    /// When a graph is due a record of its own: [`GRAPH_SAVE_CHANGES`] and
    /// [`GRAPH_SAVE_GROWTH`] unless [`Database::set_graph_saves`] says.
    graph_saves: (u64, u64),
    /// Whether a rewrite beside the database is writing its side file: one
    /// at a time, since the file has one name.
    #[cfg(not(target_arch = "wasm32"))]
    beside: std::sync::atomic::AtomicBool,
}

/// A server appends a graph to its file's tail ([`Database::save_graphs`])
/// once this many of its nodes changed since it last reached the file --
/// added, retired or linked, and every one of them linked again at the open
/// after a crash --
pub const GRAPH_SAVE_CHANGES: u64 = 10_000;

/// -- and the file has grown by this many times the record since: the
/// graph a record holds is the whole of it, so a record every few thousand
/// writes, which a bound on the linking alone would ask of a big graph,
/// would write the graph over and over for a little of it.
pub const GRAPH_SAVE_GROWTH: u64 = 3;

/// What a node takes in a graph record, before one was written: measured
/// at 100 000 x 768, m 16.
const GRAPH_NODE_BYTES: u64 = 72;

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}

impl Database {
    pub fn new() -> Database {
        Database {
            collections: HashMap::new(),
            order: Vec::new(),
            next_coll_id: 1,
            registry: Registry::with_builtins(),
            sink: Mutex::new(Box::new(NullSink)),
            dirty: false,
            failed: None,
            changes: ChangeLog::default(),
            watcher: None,
            history: History::default(),
            #[cfg(not(target_arch = "wasm32"))]
            mapped: false,
            tails: Mutex::default(),
            watched: std::sync::atomic::AtomicBool::new(false),
            adoptions: 0,
            defer_links: false,
            appended: std::sync::atomic::AtomicU64::new(0),
            graph_saves: (GRAPH_SAVE_CHANGES, GRAPH_SAVE_GROWTH),
            #[cfg(not(target_arch = "wasm32"))]
            beside: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn with_sink(sink: Box<dyn Sink>) -> Database {
        let mut db = Database::new();
        db.sink = Mutex::new(sink);
        db
    }

    /// Sends every later write to `sink`. What was written before is not
    /// in it: the caller starts it from a [`Self::snapshot`].
    pub fn set_sink(&mut self, sink: Box<dyn Sink>) {
        *self.sink_mut() = sink;
    }

    /// Exclusive access to the sink. No lock is taken, since it is `&mut self`.
    fn sink_mut(&mut self) -> &mut Box<dyn Sink> {
        self.sink.get_mut().unwrap_or_else(|e| e.into_inner())
    }

    // -------------------------------------------------------------- changes

    /// The current value of the change counter. This is a subscriber's cursor.
    pub fn change_seq(&self) -> u64 {
        self.changes.seq()
    }

    /// The smallest cursor that can still be caught up incrementally. A
    /// subscriber behind this has to be reseeded.
    pub fn change_horizon(&self) -> u64 {
        self.changes.horizon()
    }

    /// Entry count of the ring buffer: it decides how far behind a
    /// subscriber may fall.
    pub fn set_change_capacity(&mut self, n: usize) {
        self.changes.set_capacity(n);
    }

    pub fn change_capacity(&self) -> usize {
        self.changes.capacity()
    }

    /// Sets the party to wake after writes.
    pub fn set_watcher(&mut self, w: Arc<dyn Watcher>) {
        self.watcher = Some(w);
    }

    /// Marks a write on the feed, and for any maintenance running beside
    /// the database on that collection.
    fn note(&mut self, cid: u32, id: DocId) {
        self.changes.record(cid, id);
        if *self.watched.get_mut() {
            self.note_watched(cid, id);
        }
    }

    /// Out of line: a browser never runs a maintenance, and inlined into
    /// every write this was half a kilobyte of its module.
    #[cold]
    #[inline(never)]
    fn note_watched(&mut self, cid: u32, id: DocId) {
        self.tails
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .note(cid, id);
    }

    /// Names of the collections that changed since `since`.
    ///
    /// For live queries in the browser: collection granularity is enough to
    /// pick which query has to be re-run. `None` means the cursor fell
    /// behind the horizon (everything must be re-run).
    pub fn changed_collections_since(&self, since: u64) -> Option<Vec<String>> {
        let cids = self.changes.changed_collections(since)?;
        let mut out = Vec::new();
        for name in &self.order {
            if cids.contains(&self.collections[name].id) {
                out.push(name.clone());
            }
        }
        Some(out)
    }

    /// Returns the changes after a cursor, **as the rows stand right now**.
    ///
    /// `filter` is a *shape* definition: the subscriber follows a subset of
    /// the collection rather than all of it. For every changed id a single
    /// question is asked -- "does it exist now and does it match the
    /// filter?" -- and the answer lands in `puts` or in `dels`. A row that
    /// *leaves* the shape therefore shows up as a deletion by itself; no
    /// separate "left" event and no old image have to be kept.
    ///
    /// There is a deliberate looseness: if a document that never matched the
    /// filter changes, the subscriber sees its id as a deletion. Deleting a
    /// row that is not there locally is harmless, but **the shape is not a
    /// security boundary**: the information that ids outside the shape
    /// *changed* leaks. Hiding rows needs a separate endpoint (or
    /// `--http-read-only` + a token).
    pub fn changes_since(
        &self,
        collection: &str,
        since: u64,
        filter: Option<&Expr>,
        project: Option<&[String]>,
        params: &[Value],
    ) -> Result<Changes> {
        let c = self.collection(collection)?;
        let ids = match self.changes.since(since, c.id) {
            Since::Reseed => return Ok(Changes::Reseed),
            Since::Ids(ids) => ids,
        };

        let columns = projection_columns(&c.schema, &project.map(|p| p.to_vec()));
        for col in &columns {
            if col != "id" && c.schema.field(col).is_none() {
                return Err(Error::NotFound(format!("field `{col}`")));
            }
        }
        let ctx = EvalCtx {
            params,
            registry: &self.registry,
        };

        let mut rows = Vec::new();
        let mut dels = Vec::new();
        let mut schema_changed = false;
        for id in ids {
            if id == SCHEMA_MARK {
                schema_changed = true;
                continue;
            }
            if !c.store.contains(id) {
                dels.push(id);
                continue;
            }
            if let Some(f) = filter {
                let mut row = StoreRow {
                    store: &c.store,
                    schema: &c.schema,
                    id,
                    memo: Vec::new(),
                };
                if !truthy(&eval(f, &mut row, &ctx)?) {
                    // Does not match the shape: for the subscriber it is gone.
                    dels.push(id);
                    continue;
                }
            }
            let mut values = Vec::with_capacity(columns.len());
            for col in &columns {
                if col == "id" {
                    values.push(Value::Int(id as i64));
                } else {
                    let pos = c.schema.field_pos(col).unwrap();
                    values.push(c.store.read_field(id, pos)?.unwrap_or(Value::Null));
                }
            }
            rows.push(Row {
                id,
                values,
                score: None,
            });
        }

        Ok(Changes::Batch(ChangeBatch {
            seq: self.changes.seq(),
            puts: ResultSet {
                columns,
                rows,
                nested: None,
            },
            dels,
            schema_changed,
        }))
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }
    pub fn registry_mut(&mut self) -> &mut Registry {
        &mut self.registry
    }

    pub fn install_plugin(&mut self, p: &dyn Plugin) -> Result<()> {
        self.registry.install(p)
    }

    pub fn collection(&self, name: &str) -> Result<&Collection> {
        self.collections
            .get(name)
            .ok_or_else(|| Error::NotFound(format!("collection `{name}`")))
    }

    pub fn collection_names(&self) -> Vec<String> {
        self.order.clone()
    }

    pub fn stats(&self) -> Vec<CollectionStats> {
        self.order
            .iter()
            .filter_map(|n| self.collections.get(n))
            .map(|c| c.stats())
            .collect()
    }

    /// In-memory data footprint (bytes): segment bytes, offset indexes,
    /// vector arenas and graph links, and the text, sparse, hash and ordered
    /// indexes. Read from counters the indexes keep as they change, so the
    /// cost is proportional to the number of collections -- `--max-memory`
    /// asks before every write that grows the data.
    ///
    /// **This is not RSS.** Left out: the allocator's leftovers, session
    /// buffers, upper-level neighbour allocations (~3% of l0) and temporary
    /// peaks -- opening is ~2x the file, `checkpoint`/`compact` ~3x. It is a
    /// scale, not a ceiling; use it with headroom on top.
    pub fn memory_bytes(&self) -> usize {
        self.collections
            .values()
            .map(|c| {
                c.store.heap_bytes()
                    + c.store.index_bytes()
                    + c.vectors
                        .values()
                        .map(|ix| ix.arena_bytes() + ix.graph_bytes())
                        .sum::<usize>()
                    + c.hashes
                        .values()
                        .map(HashIndex::memory_bytes)
                        .sum::<usize>()
                    + c.texts.values().map(|ix| ix.memory_bytes()).sum::<usize>()
                    + c.sorted
                        .iter()
                        .map(|(_, ix)| ix.memory_bytes())
                        .sum::<usize>()
                    + c.sparse
                        .iter()
                        .map(|(_, ix)| ix.memory_bytes())
                        .sum::<usize>()
            })
            .sum()
    }

    // ---------------------------------------------------------- persistence

    /// Byte image of the whole database. Used to write to IndexedDB/OPFS in
    /// the browser and to a file on native.
    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = Vec::new();
        // A buffer takes every write, so there is nothing to handle -- and
        // `expect` here pulled the error's `Debug` into the browser module,
        // 1.3 KB for a message nobody can reach.
        let _ = self.snapshot_into(&mut out);
        out
    }

    /// [`Self::snapshot`] written into `out` as it is produced: the records
    /// of a collection go straight through, so the image is never a second
    /// copy of the data (`Sink::rewrite_with`).
    pub fn snapshot_into(&self, out: &mut dyn ImageOut) -> Result<()> {
        self.image_into(out, &[], &mut Vec::new())
    }

    /// [`Self::snapshot_into`], leaving out the dead records of the named
    /// collections: what `compact` writes. Where each collection's data
    /// record starts is put in `placed`, which is all [`Self::repoint`]
    /// needs to point the stores at the new file.
    fn image_into(
        &self,
        out: &mut dyn ImageOut,
        compacting: &[String],
        #[cfg_attr(target_arch = "wasm32", allow(unused_variables, clippy::ptr_arg))]
        placed: &mut Vec<(u32, u64)>,
    ) -> Result<()> {
        let mut head = Vec::from(&MAGIC[..]);
        // Counter header: a placeholder now, the body's length once it is
        // written -- fixed width, so it is patched where it stands.
        let head_at = head.len() as u64;
        head.push(REC_SEQ);
        head.extend_from_slice(&self.changes.seq().to_le_bytes());
        head.extend_from_slice(&0u64.to_le_bytes());
        out.write(&head)?;
        let body_at = out.at();

        if self.history.following || !self.history.lineage.is_empty() {
            out.write(&self.history.record())?;
        }

        for name in &self.order {
            let c = &self.collections[name];
            let sc = c.schema.encode();
            out.write(&record_head(REC_CREATE, c.id, sc.len()))?;
            out.write(&sc)?;

            // The counter comes right after the schema: the collection has to
            // exist, and its data can only carry the counter forward.
            let mut counter = Vec::with_capacity(9);
            put_uvarint(&mut counter, c.store.next_id());
            out.write(&record_head(REC_NEXTID, c.id, counter.len()))?;
            out.write(&counter)?;

            let compact = compacting.iter().any(|n| n == name);
            let bytes = match compact {
                true => c.store.live_len(),
                false => c.store.image_len(),
            };
            if bytes > 0 {
                out.write(&record_head(REC_DATA, c.id, bytes))?;
                // The browser maps no file, and its module keeps no list.
                #[cfg(not(target_arch = "wasm32"))]
                placed.push((c.id, out.at()));
                match compact {
                    true => c.store.write_live(out)?,
                    false => c.store.write_image(out)?,
                }
            }

            // The graph comes *after* the data records: to look the vectors
            // up while loading, the documents must already be there.
            for (field, ix) in &c.vectors {
                if ix.is_empty() {
                    continue;
                }
                let mut payload = Vec::new();
                crate::codec::encode_str(&mut payload, field);
                payload.extend_from_slice(&ix.serialize_graph());
                out.write(&record_head(REC_GRAPH, c.id, payload.len()))?;
                out.write(&payload)?;
            }
        }
        let body_len = out.at() - body_at;
        out.patch(head_at + 9, &body_len.to_le_bytes())
    }

    /// Points the stores at the file the sink has just rewritten: the same
    /// documents, in their new places. The hash, ordered, text and vector
    /// indexes stand -- nothing about the documents changed, only where
    /// their bytes are. An image this database wrote says where each
    /// collection's data record went, `placed`, and the collections named
    /// had their live records alone written; the stores then work their new
    /// places out without reading the file. One handed over from elsewhere
    /// is walked, record head by record head.
    #[cfg(not(target_arch = "wasm32"))]
    fn repoint(&mut self, placed: Option<&[(u32, u64)]>, compacted: &[String]) -> Result<()> {
        if !self.mapped {
            return Ok(());
        }
        let Some(base) = self.sink_mut().remapped() else {
            return Ok(());
        };
        if let Some(placed) = placed {
            for (name, c) in self.collections.iter_mut() {
                match placed.iter().find(|(cid, _)| *cid == c.id) {
                    Some(&(_, at)) if compacted.contains(name) => c.store.relocate_live(&base, at),
                    Some(&(_, at)) => c.store.relocate_image(&base, at),
                    None => c.store.let_go(),
                }
            }
            return Ok(());
        }
        let keep = base.clone();
        let bytes = (*keep).as_ref();
        if bytes.len() < MAGIC.len() {
            return Err(Error::Corrupt("the rewritten file is empty".into()));
        }
        let mut by_id: HashMap<u32, String> = HashMap::new();
        let mut fresh: HashMap<String, Store> = HashMap::new();
        let mut pos = MAGIC.len();
        while pos < bytes.len() {
            if !whole_record(bytes, pos)? {
                break;
            }
            let rec = bytes[pos];
            pos += 1;
            if rec == REC_SEQ {
                pos += REC_SEQ_LEN - 1;
                continue;
            }
            let cid = get_uvarint(bytes, &mut pos)? as u32;
            let len = get_uvarint(bytes, &mut pos)? as usize;
            let at = pos;
            pos += len;
            match rec {
                REC_CREATE => {
                    let mut sp = 0usize;
                    let schema = Schema::decode(&bytes[at..at + len], &mut sp)?;
                    by_id.insert(cid, schema.name.clone());
                    fresh.insert(schema.name, Store::new());
                }
                REC_NEXTID => {
                    let mut np = 0usize;
                    let next = get_uvarint(&bytes[at..at + len], &mut np)?;
                    if let Some(store) = by_id.get(&cid).and_then(|n| fresh.get_mut(n)) {
                        store.raise_next_id(next);
                    }
                }
                REC_DATA => {
                    if let Some(store) = by_id.get(&cid).and_then(|n| fresh.get_mut(n)) {
                        store.replay_mapped(&base, at as u64, len as u64, &mut |_| {})?;
                    }
                }
                _ => {}
            }
        }
        for (name, store) in fresh {
            if let Some(c) = self.collections.get_mut(&name) {
                c.store = store;
            }
        }
        Ok(())
    }

    /// Builds the database from a byte image, and returns how much of it
    /// that took: all of it, or the bytes before a last record cut short --
    /// what a crash in the middle of an append leaves. A file is cut back
    /// there before anything is appended to it (`fs::open`): a write
    /// appended after the torn bytes is read back as the rest of them, and
    /// the next open lost it.
    pub fn load(&mut self, bytes: &[u8]) -> Result<usize> {
        self.load_from(bytes, None)
    }

    /// The collation data the text this database holds in collated fields
    /// needs and the browser module has not been handed, a bit a chunk
    /// (collate.rs). A load with some missing is refused (`fenec_load`):
    /// its `@sorted` indexes were built comparing without it. Every write
    /// after keeps it so, checking what it writes before it writes it.
    pub fn collation_missing(&self) -> u32 {
        if !collate::PARTIAL {
            return 0;
        }
        let mut mask = 0;
        for c in self.collections.values() {
            if c.schema.fields.iter().any(|f| f.collate.is_some()) {
                for id in c.store.iter_ids() {
                    if let Ok(Some(doc)) = c.store.read(&c.schema, id) {
                        mask |= collation_missing(&c.schema, &doc);
                    }
                }
            }
        }
        mask
    }

    /// [`Self::load`] over a mapped file (`fs::open_mapped`): the documents
    /// stay in the file, read through the pages the operating system maps
    /// in, and only what is derived from them -- the offset index, the hash,
    /// ordered and text indexes, the graph -- is built in memory.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load_mapped(&mut self, file: crate::store::Base) -> Result<usize> {
        let keep = file.clone();
        self.mapped = true;
        self.load_from((*keep).as_ref(), Some(&file))
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn load_from(&mut self, bytes: &[u8], base: Option<&crate::store::Base>) -> Result<usize> {
        self.load_records(bytes, &mut |store, chunk_at, chunk, note| match base {
            Some(b) => store.replay_mapped(b, chunk_at as u64, chunk.len() as u64, note),
            None => store.replay_noting(chunk, note),
        })
    }

    #[cfg(target_arch = "wasm32")]
    fn load_from(&mut self, bytes: &[u8], _base: Option<&()>) -> Result<usize> {
        self.load_records(bytes, &mut |store, _, chunk, note| {
            store.replay_noting(chunk, note)
        })
    }

    /// Has the next load leave out of the graphs the vectors it would link
    /// into them -- the writes after the checkpoint, and every vector of a
    /// graph it could not restore -- for [`Self::link_pending`] to link.
    /// Until then `near` measures each of them against the query, so an
    /// answer is what the graph finds and the nearest of them together. A
    /// server opens its file this way (`fs::open_serving`) and links them
    /// beside its queries: linked first, they kept its port closed for as
    /// long as they took, 56.6 s at 100 000 x 768 never checkpointed,
    /// which opens in 0.96 s this way.
    pub fn defer_linking(&mut self) {
        self.defer_links = true;
    }

    /// Links up to `max` of the vectors a load left out of the graphs
    /// ([`Self::defer_linking`]) and returns how many are left. The caller
    /// holds the write lock for as long as `max` of them take.
    pub fn link_pending(&mut self, max: usize) -> usize {
        let mut budget = max;
        let mut left = 0;
        for name in &self.order {
            let Some(c) = self.collections.get_mut(name) else {
                continue;
            };
            let Collection {
                schema,
                store,
                vectors,
                ..
            } = c;
            for (field, ix) in vectors.iter_mut() {
                let before = ix.unlinked();
                if before > 0 && budget > 0 {
                    let pos = schema.field_pos(field);
                    let now = ix.link_pending(budget, &mut |doc, out| {
                        pos.is_some_and(|p| store.read_vector_into(doc, p, out).unwrap_or(false))
                    });
                    budget -= (before - now).min(budget);
                }
                left += ix.unlinked();
            }
        }
        left
    }

    /// Whether [`Self::save_graphs`] would append a graph.
    pub fn graphs_due(&self) -> bool {
        let appended = self.appended.load(Relaxed);
        self.collections.values().any(|c| {
            c.vectors
                .values()
                .any(|ix| graph_due(ix, appended, self.graph_saves))
        })
    }

    /// When [`Self::save_graphs`] appends a graph: once `changes` of its
    /// nodes changed since it last reached the file, and the file grew since
    /// by `growth` times its record.
    pub fn set_graph_saves(&mut self, changes: u64, growth: u64) {
        self.graph_saves = (changes, growth);
    }

    /// Appends to the file, as a record of its own, each graph that changed
    /// in [`GRAPH_SAVE_CHANGES`] nodes since it last reached it and has none
    /// waiting to be linked, once the file has grown since by
    /// [`GRAPH_SAVE_GROWTH`] times the record; returns how many. A load
    /// restores a graph where its last record is and links only what was
    /// written after it, where a server that crashed linked every vector
    /// written since its last checkpoint -- which it writes on its way down
    /// alone.
    ///
    /// It takes `&self`, for a caller holding the read lock: that keeps the
    /// writes out while a graph is written -- a record of a graph that
    /// writes changed meanwhile would not match the documents where it lands
    /// -- and lets queries on. The record is not a write: the change counter
    /// does not move and no replica is sent it. What comes back with the
    /// count pushes the records to disk, to run once the lock is let go, as
    /// a write's [`Durability`] is: left in the sink's buffer until the next
    /// write, a record was lost to the crash of a server that had nothing
    /// more to write -- the one that had just linked what the last crash
    /// left. A sink that refuses either is reported back with
    /// [`Self::fail`].
    pub fn save_graphs(&self) -> Result<(usize, Option<Durability>)> {
        if self.failed.is_some() {
            return Ok((0, None));
        }
        let mut n = 0;
        for name in &self.order {
            let c = &self.collections[name];
            for (field, ix) in &c.vectors {
                if !graph_due(ix, self.appended.load(Relaxed), self.graph_saves) {
                    continue;
                }
                let mut payload = Vec::new();
                crate::codec::encode_str(&mut payload, field);
                payload.extend_from_slice(&ix.serialize_graph_kept());
                let mut record = record_head(REC_GRAPH, c.id, payload.len());
                record.extend_from_slice(&payload);
                self.sink
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .append(&record)?;
                let end =
                    self.appended.fetch_add(record.len() as u64, Relaxed) + record.len() as u64;
                let p = ix.persisted();
                p.changes.store(ix.changes(), Relaxed);
                p.at.store(end, Relaxed);
                p.node_bytes
                    .store((payload.len() / ix.len().max(1)) as u64, Relaxed);
                n += 1;
            }
        }
        if n == 0 {
            return Ok((0, None));
        }
        let durable = self
            .sink
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .flush()?;
        Ok((n, durable))
    }

    /// The vectors a load left out of the graphs and [`Self::link_pending`]
    /// has still to link.
    pub fn unlinked(&self) -> usize {
        self.collections
            .values()
            .flat_map(|c| c.vectors.values())
            .map(|ix| ix.unlinked())
            .sum()
    }

    /// The pass over a file's records; `replay` takes a data record's frames
    /// into a collection's store.
    fn load_records(&mut self, bytes: &[u8], replay: &mut Replay<'_>) -> Result<usize> {
        if bytes.len() < MAGIC.len() || bytes[..MAGIC.len()] != MAGIC[..] {
            return Err(Error::Corrupt("invalid fenecdb signature".into()));
        }
        let mut pos = MAGIC.len();
        let mut by_id: HashMap<u32, String> = HashMap::new();
        // Each graph is restored where its last record is, against the
        // documents as they stood when it was written, and the writes after
        // it are applied to it one touched document at a time: a
        // checkpoint's image holds one after each collection's data, and a
        // server appends one to the tail now and then (`save_graphs`).
        // Restored after the whole file instead, a single write in the tail
        // left the node count off and threw the graph away -- a crash cost a
        // full rebuild -- and an earlier record of the same graph is not
        // restored at all: it would read every vector again for nothing. A
        // browser writes one record of a graph, in an image, and restores
        // each it meets rather than carry the walk.
        let last = match cfg!(target_arch = "wasm32") {
            true => None,
            false => Some(last_graphs(bytes)?),
        };
        // The counter is built from two parts: the base written by the
        // checkpoint, and the records appended **after** the image body
        // ended. The boundary is a byte offset (`body_end`), not a record
        // count: a single data record of the image can hold thousands of
        // frames, while the ones in the tail hold one each. Without the
        // header (an old file) the base is zero and every record counts.
        // The graph is derived data and is never counted.
        let mut seq_base = 0u64;
        let mut seq_seen = 0u64;
        let mut body_end = MAGIC.len();
        // The graphs restored, each with how many of its collection's
        // touched documents came before it: it takes the ones after.
        let mut restored: Vec<(String, String, usize)> = Vec::new();
        // Per collection with a restored graph, the documents written after
        // one was. A `Vec`, not a map: a file has a handful of collections,
        // and the map's code was 1.5 KB of the browser module.
        let mut touched: Vec<(String, Vec<DocId>)> = Vec::new();
        let mut whole = bytes.len();
        while pos < bytes.len() {
            if !whole_record(bytes, pos)? {
                // A crash in the middle of an append leaves the last record
                // cut short, and only past the image: an image is written
                // beside the file and renamed over it whole. One cut short
                // inside it is a damaged or truncated file, and cutting the
                // file there -- as a torn tail is cut -- destroyed every
                // record after it, intact ones included.
                if pos < body_end {
                    return Err(Error::Corrupt(
                        "a record of the checkpoint image runs past the end of the file".into(),
                    ));
                }
                whole = pos;
                break;
            }
            let tail = pos >= body_end;
            let at = pos;
            let rec = bytes[pos];
            pos += 1;
            match rec {
                REC_CREATE => {
                    let cid = get_uvarint(bytes, &mut pos)? as u32;
                    let len = get_uvarint(bytes, &mut pos)? as usize;
                    let mut sp = 0usize;
                    let schema = Schema::decode(&bytes[pos..pos + len], &mut sp)?;
                    pos += len;
                    seq_seen += tail as u64;
                    // Made again under its name: no graph restored before
                    // is this collection's.
                    forget(&mut restored, &schema.name, &|_| false);
                    by_id.insert(cid, schema.name.clone());
                    self.order.retain(|n| n != &schema.name);
                    self.order.push(schema.name.clone());
                    self.collections
                        .insert(schema.name.clone(), Collection::new(cid, schema));
                    self.next_coll_id = self.next_coll_id.max(cid + 1);
                }
                REC_DROP => {
                    let cid = get_uvarint(bytes, &mut pos)? as u32;
                    // Its body is empty but the length field is still written
                    // (see [`Database::wal`]); if it is not skipped that `0`
                    // byte is read as the next record kind and the whole file
                    // becomes unopenable with "unknown record kind 0".
                    let len = get_uvarint(bytes, &mut pos)? as usize;
                    pos += len;
                    seq_seen += tail as u64;
                    if let Some(name) = by_id.remove(&cid) {
                        self.collections.remove(&name);
                        self.order.retain(|n| n != &name);
                        forget(&mut restored, &name, &|_| false);
                    }
                }
                REC_DATA => {
                    let cid = get_uvarint(bytes, &mut pos)? as u32;
                    let len = get_uvarint(bytes, &mut pos)? as usize;
                    let name = by_id
                        .get(&cid)
                        .cloned()
                        .ok_or_else(|| Error::Corrupt(format!("unknown collection {cid}")))?;
                    let chunk_at = pos;
                    let chunk = &bytes[pos..pos + len];
                    pos += len;
                    let c = self.collections.get_mut(&name).unwrap();
                    let frames = if restored.iter().any(|(n, _, _)| *n == name) {
                        let at = match touched.iter().position(|(n, _)| *n == name) {
                            Some(at) => at,
                            None => {
                                touched.push((name, Vec::new()));
                                touched.len() - 1
                            }
                        };
                        let ids = &mut touched[at].1;
                        replay(&mut c.store, chunk_at, chunk, &mut |id| ids.push(id))?
                    } else {
                        replay(&mut c.store, chunk_at, chunk, &mut |_| {})?
                    } as u64;
                    if tail {
                        seq_seen += frames;
                    }
                }
                REC_ALTER => {
                    let cid = get_uvarint(bytes, &mut pos)? as u32;
                    let len = get_uvarint(bytes, &mut pos)? as usize;
                    let mut sp = 0usize;
                    let schema = Schema::decode(&bytes[pos..pos + len], &mut sp)?;
                    pos += len;
                    seq_seen += tail as u64;
                    if let Some(name) = by_id.get(&cid) {
                        if let Some(c) = self.collections.get_mut(name) {
                            // The field layout must not have changed: stored
                            // documents are encoded positionally.
                            let same_layout = c.schema.fields.len() == schema.fields.len()
                                && c.schema
                                    .fields
                                    .iter()
                                    .zip(&schema.fields)
                                    .all(|(a, b)| a.name == b.name && a.ty == b.ty);
                            if same_layout {
                                // An index added leaves the graphs restored
                                // before it, as a replica leaves them: the
                                // documents are the same ones. Not in the
                                // browser, whose schemas are declared with
                                // their collections: 767 bytes of its module
                                // for a rebuild it would rarely save.
                                let mut kept = Vec::new();
                                for (n, f, _) in
                                    restored.iter().filter(|_| !cfg!(target_arch = "wasm32"))
                                {
                                    let same = schema.field_pos(f).is_some_and(|p| {
                                        c.schema.fields.get(p).map(|x| &x.index)
                                            == Some(&schema.fields[p].index)
                                    });
                                    if n == name && same {
                                        if let Some(ix) = c.vectors.remove(f) {
                                            kept.push((f.clone(), ix));
                                        }
                                    }
                                }
                                c.schema = schema;
                                c.reset_index_structures();
                                forget(&mut restored, name, &|f| kept.iter().any(|(k, _)| k == f));
                                for (f, ix) in kept {
                                    c.vectors.insert(f, ix);
                                }
                            }
                        }
                    }
                }
                REC_NEXTID => {
                    let cid = get_uvarint(bytes, &mut pos)? as u32;
                    let len = get_uvarint(bytes, &mut pos)? as usize;
                    let mut np = 0usize;
                    let next = get_uvarint(&bytes[pos..pos + len], &mut np)?;
                    pos += len;
                    // Not a write but the counter itself: it does not move `seq`.
                    if let Some(name) = by_id.get(&cid) {
                        if let Some(c) = self.collections.get_mut(name) {
                            c.store.raise_next_id(next);
                        }
                    }
                }
                REC_GRAPH => {
                    let cid = get_uvarint(bytes, &mut pos)? as u32;
                    let len = get_uvarint(bytes, &mut pos)? as usize;
                    let chunk = &bytes[pos..pos + len];
                    pos += len;
                    let mut cp = 0usize;
                    let field = crate::codec::decode_str(chunk, &mut cp)?;
                    let latest = last.as_ref().is_none_or(|last| {
                        last.iter()
                            .any(|(c, f, p)| *c == cid && *f == field && *p == at)
                    });
                    let name = by_id.get(&cid).cloned();
                    if let (true, Some(name)) = (latest, name) {
                        if self.restore_graph(&name, &field, &chunk[cp..])? {
                            let from = match touched.iter().position(|(n, _)| *n == name) {
                                Some(i) => touched[i].1.len(),
                                None => {
                                    touched.push((name.clone(), Vec::new()));
                                    0
                                }
                            };
                            // As saved as its record: the writes after it
                            // are what a crash would take into it again.
                            #[cfg(not(target_arch = "wasm32"))]
                            {
                                let ix = &self.collections[&name].vectors[&field];
                                let p = ix.persisted();
                                p.at.store(pos as u64, Relaxed);
                                p.node_bytes.store((len / ix.len().max(1)) as u64, Relaxed);
                            }
                            forget(&mut restored, &name, &|f| f != field);
                            restored.push((name, field, from));
                        }
                    }
                }
                REC_HISTORY => {
                    let _ = get_uvarint(bytes, &mut pos)?;
                    let len = get_uvarint(bytes, &mut pos)? as usize;
                    // Not a write: it does not move `seq`.
                    self.history = History::decode(&bytes[pos..pos + len])?;
                    pos += len;
                }
                REC_SEQ => {
                    if pos + REC_SEQ_LEN - 1 > bytes.len() {
                        return Err(Error::Corrupt("truncated counter header".into()));
                    }
                    let mut w = [0u8; 8];
                    w.copy_from_slice(&bytes[pos..pos + 8]);
                    seq_base = u64::from_le_bytes(w);
                    w.copy_from_slice(&bytes[pos + 8..pos + 16]);
                    pos += 16;
                    body_end = pos.saturating_add(u64::from_le_bytes(w) as usize);
                    // An image that says it is longer than the file was cut
                    // short between two of its records, which no record's own
                    // length can show.
                    if body_end > bytes.len() {
                        return Err(Error::Corrupt(
                            "the checkpoint image is longer than the file".into(),
                        ));
                    }
                }
                other => return Err(Error::Corrupt(format!("unknown record kind {other}"))),
            }
        }
        // A database loaded from a file has no *history*, only its current
        // state: the ring is emptied and the horizon is set to the counter.
        // A cursor sitting exactly here (a quiet restart) gets an empty
        // answer; everything else is reseeded.
        self.changes.reset(seq_base + seq_seen);
        // Indexes are derived data: a graph restored from the file takes the
        // writes after its record; everything else is rebuilt from the
        // documents.
        self.rebuild_indexes_with(&restored, &touched)?;
        *self.appended.get_mut() = whole as u64;
        self.defer_links = false;
        Ok(whole)
    }

    /// Restores a persisted graph against the documents as they stand where
    /// its record is, keeping it only if it describes them exactly: every
    /// live node's document holds a vector -- `restore_graph` checks that --
    /// and every document holding one has a node. The second used to be a
    /// comparison with the number of documents, so a single document
    /// without a vector threw the graph away on every open.
    fn restore_graph(&mut self, name: &str, field: &str, bytes: &[u8]) -> Result<bool> {
        let Some(c) = self.collections.get_mut(name) else {
            return Ok(false);
        };
        let Some(pos) = c.schema.field_pos(field) else {
            return Ok(false);
        };
        let DataType::Vector(dim, prec) = c.schema.fields[pos].ty else {
            return Ok(false);
        };
        let IndexKind::Vector(spec) = c.schema.fields[pos].index else {
            return Ok(false);
        };
        if !c.vectors.contains_key(field) {
            return Ok(false);
        }
        let store = &c.store;
        let Some(ix) = VectorIndex::restore_graph(bytes, dim, prec, |doc, out| {
            store.read_vector_into(doc, pos, out).unwrap_or(false)
        }) else {
            return Ok(false);
        };
        // Built with other parameters -- another quantization, whose arena
        // holds other codes -- it is not this index's graph.
        if ix.spec != spec {
            return Ok(false);
        }
        let mut with_vector = 0;
        for id in store.iter_ids() {
            with_vector += store.has_vector(id, pos)? as usize;
        }
        if ix.len() != with_vector {
            return Ok(false);
        }
        c.vectors.insert(field.to_string(), ix);
        Ok(true)
    }

    pub fn rebuild_indexes(&mut self) -> Result<()> {
        self.rebuild_indexes_with(&[], &[])
    }

    /// Fills the derived indexes from the documents. A vector index named in
    /// `restored` came back from the file as of its last graph record, and
    /// takes only the documents written after it: those of its collection's
    /// `touched` from the count beside it on.
    fn rebuild_indexes_with(
        &mut self,
        restored: &[(String, String, usize)],
        touched: &[(String, Vec<DocId>)],
    ) -> Result<()> {
        let later = crate::vector::UNLINKED && self.defer_links;
        for name in self.order.clone() {
            let c = self.collections.get_mut(&name).unwrap();
            // `ids()` comes back ascending; no extra sorting needed.
            let ids: Vec<DocId> = c.store.ids();
            let fields: Vec<String> = c.vectors.keys().cloned().collect();
            let from = |field: &str| {
                restored
                    .iter()
                    .find(|(n, f, _)| *n == name && f == field)
                    .map(|(_, _, from)| *from)
            };
            let kept = |field: &str| from(field).is_some();
            let written: &[DocId] = touched
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, ids)| &ids[..])
                .unwrap_or(&[]);

            // 1) The restored graphs take the writes after their records the
            // way the write path took them: a touched document keeps its node
            // while it holds the vector the record had -- `insert` sees to it
            // -- has it retired for the one it holds now otherwise, and
            // leaves the graph without one.
            for field in fields.iter().filter(|f| kept(f)) {
                let Some(pos) = c.schema.field_pos(field) else {
                    continue;
                };
                let mut tail = written[from(field).unwrap_or(0).min(written.len())..].to_vec();
                tail.sort_unstable();
                tail.dedup();
                let mut items: Vec<(DocId, Vec<f32>)> = Vec::new();
                let mut gone = Vec::new();
                let mut buf = Vec::new();
                for &id in &tail {
                    if c.store.read_vector_into(id, pos, &mut buf)? {
                        items.push((id, buf.clone()));
                    } else {
                        gone.push(id);
                    }
                }
                let ix = c
                    .vectors
                    .get_mut(field)
                    .expect("a restored field has its index");
                for id in gone {
                    ix.remove(id);
                }
                if later {
                    ix.defer_batch(&items);
                    continue;
                }
                ix.insert_batch(&items);
                // A graph a server wrote before it had linked everything
                // holds nodes still waiting: linked here, as the tail is.
                if ix.unlinked() > 0 {
                    let store = &c.store;
                    ix.link_pending(usize::MAX, &mut |doc, out| {
                        store.read_vector_into(doc, pos, out).unwrap_or(false)
                    });
                }
            }

            // 2) Rebuild whatever could not be restored, at the field's own
            // precision -- `VectorIndex::new` would have made every rebuilt
            // `vector<N, f16>` index an f32 one.
            for (field, ix) in c.vectors.iter_mut() {
                if kept(field) {
                    continue;
                }
                *ix = VectorIndex::with_precision(ix.dim, ix.spec, ix.precision());
                // The document count is known, so the arena is sized in one go.
                ix.reserve(ids.len());
            }
            for m in c.hashes.values_mut() {
                m.clear();
            }
            for t in c.texts.values_mut() {
                t.clear();
            }
            for (_, ix) in c.sparse.iter_mut() {
                ix.clear();
            }
            if fields.iter().all(|f| kept(f))
                && c.hashes.is_empty()
                && c.texts.is_empty()
                && c.sorted.is_empty()
                && c.sparse.is_empty()
            {
                continue; // everything restored, no need to read the documents
            }
            // The documents are read one at a time, and of each only the
            // fields an index is built from. Decoding every document first
            // held the collection a second time, bodies, field names and all:
            // a 1 GB file of 2.3 million rows with a hash and an ordered
            // index, which want one short field a row each, peaked at 4.2 GB
            // of heap opening; read this way, at 2.3 GB.
            let Collection {
                schema,
                store,
                vectors,
                hashes,
                texts,
                sorted,
                sparse,
                ..
            } = c;
            // Each index beside its field's position, the ordered and vector
            // ones with the rows they are built from afterwards. Pushed in
            // loops: collected, the four lists were 2.5 KB of the browser
            // module.
            let mut hash_ix = Vec::new();
            for (f, m) in hashes.iter_mut() {
                if let Some(p) = schema.field_pos(f) {
                    hash_ix.push((p, m));
                }
            }
            let mut text_ix = Vec::new();
            for (f, t) in texts.iter_mut() {
                if let Some(p) = schema.field_pos(f) {
                    text_ix.push((p, t));
                }
            }
            let mut sorted_ix = Vec::new();
            for (f, ix) in sorted.iter_mut() {
                if let Some(p) = schema.field_pos(f) {
                    sorted_ix.push((p, ix, Vec::new()));
                }
            }
            let mut vector_ix = Vec::new();
            for (f, ix) in vectors.iter_mut() {
                if let Some(p) = schema.field_pos(f).filter(|_| !kept(f)) {
                    vector_ix.push((p, ix, Vec::new()));
                }
            }
            let mut sparse_ix = Vec::new();
            for (f, ix) in sparse.iter_mut() {
                if let Some(p) = schema.field_pos(f) {
                    sparse_ix.push((p, ix));
                }
            }
            // In field order, as `read_fields` wants them; walked rather than
            // sorted, since a sort was 2 KB of the browser module.
            let mut positions = Vec::new();
            for p in 0..schema.fields.len() {
                if hash_ix.iter().any(|(q, _)| *q == p)
                    || text_ix.iter().any(|(q, _)| *q == p)
                    || sorted_ix.iter().any(|(q, ..)| *q == p)
                    || vector_ix.iter().any(|(q, ..)| *q == p)
                    || sparse_ix.iter().any(|(q, _)| *q == p)
                {
                    positions.push(p);
                }
            }
            let slot = |p: usize| positions.iter().position(|&q| q == p).unwrap_or(0);
            // One field has one index, so each value is taken by one of them.
            let mut vals = Vec::with_capacity(positions.len());
            for id in ids {
                if !store.read_fields(id, &positions, &mut vals)? {
                    continue;
                }
                for (p, ix) in hash_ix.iter_mut() {
                    ix.add(hash_key(&vals[slot(*p)]), id);
                }
                for (p, ix) in text_ix.iter_mut() {
                    if let Value::Text(t) = &vals[slot(*p)] {
                        ix.insert(id, t);
                    }
                }
                for (p, _, rows) in sorted_ix.iter_mut() {
                    let v = std::mem::replace(&mut vals[slot(*p)], Value::Null);
                    rows.push((id, Some(v)));
                }
                for (p, _, rows) in vector_ix.iter_mut() {
                    if let Value::Vector(v) = std::mem::replace(&mut vals[slot(*p)], Value::Null) {
                        rows.push((id, v));
                    }
                }
                for (p, ix) in sparse_ix.iter_mut() {
                    if let Value::Sparse(_, e) = &vals[slot(*p)] {
                        ix.insert(id, e);
                    }
                }
            }
            for (_, ix) in text_ix.iter_mut() {
                ix.shrink_to_fit();
            }
            for (_, ix) in sparse_ix.iter_mut() {
                ix.shrink_to_fit();
            }
            // Each ordered index is sorted once from its keys rather than
            // inserted row by row.
            #[cfg(feature = "sorted")]
            for (p, ix, rows) in sorted_ix.iter_mut() {
                **ix = SortedIndex::build(
                    &schema.fields[*p].ty,
                    schema.fields[*p].collate,
                    &mut std::mem::take(rows).into_iter(),
                );
            }
            for (_, ix, rows) in vector_ix.iter_mut() {
                match later {
                    true => ix.defer_batch(rows),
                    false => ix.insert_batch(rows),
                }
            }
        }
        Ok(())
    }

    /// Rewrites the file image (graph included), so the next open does not
    /// have to rebuild the indexes.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.refuse_if_failed()?;
        // The sink is behind a lock, so the image can be written from `self`
        // while the sink takes it: both are shared borrows here.
        let mut placed = Vec::new();
        let mut len = 0;
        let r = {
            let mut sink = self.sink.lock().unwrap_or_else(|e| e.into_inner());
            sink.rewrite_with(&mut |out| {
                self.image_into(out, &[], &mut placed)?;
                len = out.at();
                Ok(())
            })
        };
        self.storage(r)?;
        self.rewrote(len);
        let r = self.sink_mut().sync();
        self.storage(r)?;
        #[cfg(not(target_arch = "wasm32"))]
        self.repoint(Some(&placed), &[])?;
        self.dirty = false;
        Ok(())
    }

    /// The file is an image of the database as it stands, `len` bytes long:
    /// every graph in it is as saved as it can be.
    fn rewrote(&mut self, len: u64) {
        // A browser appends no graph, and its module carries none of this.
        if cfg!(target_arch = "wasm32") {
            return;
        }
        *self.appended.get_mut() = len;
        for c in self.collections.values() {
            for ix in c.vectors.values() {
                let p = ix.persisted();
                p.changes.store(ix.changes(), Relaxed);
                p.at.store(len, Relaxed);
            }
        }
    }

    /// Appends a write's record. Every caller notes the write on the change
    /// counter right after, so the record is numbered as the one after the
    /// counter's current value.
    fn wal(&mut self, rec: u8, cid: u32, payload: &[u8]) -> Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 12);
        frame.push(rec);
        put_uvarint(&mut frame, cid as u64);
        put_uvarint(&mut frame, payload.len() as u64);
        frame.extend_from_slice(payload);
        let seq = self.changes.seq() + 1;
        let r = self.sink_mut().record(seq, &frame);
        self.storage(r)?;
        #[cfg(not(target_arch = "wasm32"))]
        {
            *self.appended.get_mut() += frame.len() as u64;
        }
        self.dirty = true;
        Ok(())
    }

    /// Pushes the writes to disk. After a storage error it does not try
    /// again: it keeps answering with that error (see `failed`).
    pub fn sync(&mut self) -> Result<()> {
        self.refuse_if_failed()?;
        let r = self.sink_mut().sync();
        self.storage(r)?;
        self.dirty = false;
        Ok(())
    }

    /// The first half of [`Self::sync`], for a caller that will not hold the
    /// database while the disk works: the writes go to the operating
    /// system, and what comes back makes them durable. Run it, and if it
    /// fails, report the failure back with [`Self::fail`] -- the second half
    /// runs where the engine cannot see it.
    pub fn flush(&mut self) -> Result<Option<Durability>> {
        self.refuse_if_failed()?;
        let r = self.sink_mut().flush();
        let durable = self.storage(r)?;
        self.dirty = false;
        Ok(durable)
    }

    /// Records a storage failure found outside the engine, a
    /// [`Durability`] that failed: every later write and sync is refused, as
    /// after one the engine saw itself.
    pub fn fail(&mut self, e: &Error) {
        if self.failed.is_none() {
            self.failed = Some(e.to_string());
        }
    }

    /// Whether a write is still waiting to be pushed to disk.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The storage error that stopped writes, if one did.
    pub fn failure(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// Passes a sink result through, remembering the first failure.
    fn storage<T>(&mut self, r: Result<T>) -> Result<T> {
        if let Err(e) = &r {
            if self.failed.is_none() {
                self.failed = Some(e.to_string());
            }
        }
        r
    }

    fn refuse_if_failed(&self) -> Result<()> {
        match &self.failed {
            None => Ok(()),
            Some(m) => Err(Error::Io(format!(
                "writes are refused after a storage error ({m}); \
                 reopen the file to go on with what reached the disk"
            ))),
        }
    }

    // ---------------------------------------------------------- replication

    /// Which history the writes belong to; see [`History`].
    pub fn history(&self) -> &History {
        &self.history
    }

    /// Starts a new history at the current change, under `id`, and takes
    /// writes from here on: a replica being promoted, or a primary about to
    /// have its first replica -- the root history is every database's and
    /// names none. `id` must be one no other database holds; the caller
    /// draws it at random.
    pub fn fork(&mut self, id: u64) -> Result<()> {
        self.set_history(self.history.forked(id, self.changes.seq()))
    }

    /// Takes a primary's history, and from then on no write of its own:
    /// the writes arrive through [`Self::apply`].
    pub fn follow(&mut self, lineage: Vec<(u64, u64)>) -> Result<()> {
        self.set_history(History {
            lineage,
            following: true,
        })
    }

    fn set_history(&mut self, h: History) -> Result<()> {
        self.refuse_if_failed()?;
        if h == self.history {
            return Ok(());
        }
        // Not a write: it has no number, and no replica is sent it.
        let record = h.record();
        let r = self.sink_mut().append(&record);
        self.storage(r)?;
        *self.appended.get_mut() += record.len() as u64;
        self.dirty = true;
        self.history = h;
        Ok(())
    }

    /// Applies writes a primary made. `records` are what its write path
    /// appended to its file, whole and in order, and each is numbered as
    /// the change after this database's counter -- the number the primary
    /// gave it, when the two agree on where they stand
    /// ([`History::continues`]).
    ///
    /// Each record goes through the index upkeep the write path does and on
    /// to this database's own sink, so a replica's file holds the primary's
    /// records and reopens at the same change. The one thing that may come
    /// out different is the HNSW graph: the same vectors go in, in the same
    /// order, but not in the same batches, and the search is approximate
    /// either way.
    pub fn apply(&mut self, records: &[u8]) -> Result<usize> {
        self.refuse_if_failed()?;
        let before = self.changes.seq();
        let mut batch = VectorBatch::default();
        let applied = self.apply_records(records, &mut batch);
        // A record that failed leaves the ones before it applied, and their
        // vectors are indexed all the same.
        self.index_batch(&mut batch);
        let after = self.changes.seq();
        if after != before {
            if let Some(w) = &self.watcher {
                w.notify(after);
            }
        }
        applied
    }

    fn apply_records(&mut self, bytes: &[u8], batch: &mut VectorBatch) -> Result<usize> {
        let cut = || Error::Corrupt("a write record cut short".into());
        let mut n = 0;
        let mut pos = 0;
        while pos < bytes.len() {
            let start = pos;
            let rec = bytes[pos];
            pos += 1;
            let cid = get_uvarint(bytes, &mut pos)? as u32;
            let len = get_uvarint(bytes, &mut pos)? as usize;
            let body = bytes.get(pos..pos + len).ok_or_else(cut)?;
            pos += len;

            let mut marked = SCHEMA_MARK;
            if rec == REC_DATA {
                let mut p = 1;
                marked = get_uvarint(body, &mut p)?;
            }
            // The graph takes a batch as the graph stands before it: a
            // record that needs it as it stands after -- a document the
            // batch holds, another collection, a schema change -- ends it.
            if !batch.docs.is_empty()
                && (rec != REC_DATA || cid != batch.cid || batch.ids.contains(&marked))
            {
                self.index_batch(batch);
            }
            match rec {
                REC_CREATE => {
                    let mut sp = 0;
                    let schema = Schema::decode(body, &mut sp)?;
                    if self.collections.contains_key(&schema.name) || self.named(cid).is_some() {
                        return Err(Error::Corrupt(format!(
                            "collection `{}` is already here",
                            schema.name
                        )));
                    }
                    self.next_coll_id = self.next_coll_id.max(cid + 1);
                    self.order.push(schema.name.clone());
                    self.collections
                        .insert(schema.name.clone(), Collection::new(cid, schema));
                }
                REC_DROP => {
                    let name = self.named(cid).ok_or_else(|| missing(cid))?;
                    self.collections.remove(&name);
                    self.order.retain(|n| *n != name);
                }
                REC_ALTER => {
                    let name = self.named(cid).ok_or_else(|| missing(cid))?;
                    let mut sp = 0;
                    let schema = Schema::decode(body, &mut sp)?;
                    let c = self.collections.get_mut(&name).unwrap();
                    let same_layout = c.schema.fields.len() == schema.fields.len()
                        && c.schema
                            .fields
                            .iter()
                            .zip(&schema.fields)
                            .all(|(a, b)| a.name == b.name && a.ty == b.ty);
                    if !same_layout {
                        return Err(Error::Corrupt(format!(
                            "a schema change moved the fields of `{name}`"
                        )));
                    }
                    // `create index` adds one; anything else rebuilds them all.
                    let changed: Vec<usize> = (0..schema.fields.len())
                        .filter(|&i| c.schema.fields[i].index != schema.fields[i].index)
                        .collect();
                    let added = changed
                        .iter()
                        .all(|&i| c.schema.fields[i].index == IndexKind::None);
                    c.schema = schema;
                    if added {
                        for i in changed {
                            build_index(c, i)?;
                        }
                    } else {
                        c.reset_index_structures();
                        for i in 0..c.schema.fields.len() {
                            build_index(c, i)?;
                        }
                    }
                }
                REC_DATA => {
                    let name = self.named(cid).ok_or_else(|| missing(cid))?;
                    let c = self.collections.get_mut(&name).unwrap();
                    let op = body[0];
                    let mut p = 1;
                    let id = get_uvarint(body, &mut p)?;
                    let plen = get_uvarint(body, &mut p)? as usize;
                    let payload = body.get(p..p + plen).ok_or_else(cut)?;
                    let old = c.store.read(&c.schema, id)?;
                    c.store.append(op, id, payload);
                    let new = match op == OP_PUT {
                        true => Some(c.store.read(&c.schema, id)?.ok_or_else(cut)?),
                        false => None,
                    };
                    if let Some(old) = &old {
                        c.unindex_doc(old, new.as_ref());
                    }
                    if let Some(doc) = new {
                        c.index_scalar(&doc, old.as_ref());
                        if !c.vectors.is_empty() {
                            batch.cid = cid;
                            batch.ids.insert(id);
                            batch.docs.push(doc);
                        }
                    }
                }
                other => {
                    return Err(Error::Corrupt(format!(
                        "record kind {other} is not a write"
                    )))
                }
            }
            let seq = self.changes.seq() + 1;
            let r = self.sink_mut().record(seq, &bytes[start..pos]);
            self.storage(r)?;
            *self.appended.get_mut() += (pos - start) as u64;
            self.dirty = true;
            self.note(cid, marked);
            n += 1;
        }
        Ok(n)
    }

    /// The name of the collection with id `cid`.
    fn named(&self, cid: u32) -> Option<String> {
        self.order
            .iter()
            .find(|n| self.collections[*n].id == cid)
            .cloned()
    }

    fn index_batch(&mut self, batch: &mut VectorBatch) {
        if let Some(name) = self.named(batch.cid) {
            if let Some(c) = self.collections.get_mut(&name) {
                c.index_vectors_batch(&batch.docs);
            }
        }
        batch.docs.clear();
        batch.ids.clear();
    }

    /// Replaces everything with `fresh` -- a database loaded from `image` --
    /// and rewrites the sink with the image: what a replica does when its
    /// primary no longer holds the writes it missed. The sink, the watcher,
    /// the plugins and the change ring's size stay. The ring's marks do not,
    /// so every subscriber reseeds.
    /// How many images [`Self::adopt`] took. What was read from the database
    /// is stale when this moves, whatever the change counter says: an image
    /// can land on the very change the old contents stood at.
    pub fn adoptions(&self) -> u64 {
        self.adoptions
    }

    pub fn adopt(&mut self, fresh: Database, image: &[u8]) -> Result<()> {
        self.refuse_if_failed()?;
        let r = self.sink_mut().rewrite(image);
        self.storage(r)?;
        *self.appended.get_mut() = image.len() as u64;
        let cap = self.changes.capacity();
        self.collections = fresh.collections;
        self.order = fresh.order;
        self.next_coll_id = fresh.next_coll_id;
        self.history = fresh.history;
        self.changes = fresh.changes;
        self.changes.set_capacity(cap);
        self.adoptions += 1;
        // The image is the file now: a mapped database reads the documents
        // from there rather than holding the copy it was handed. Written
        // elsewhere, it is walked to find them.
        #[cfg(not(target_arch = "wasm32"))]
        self.repoint(None, &[])?;
        self.dirty = false;
        if let Some(w) = &self.watcher {
            w.notify(self.changes.seq());
        }
        Ok(())
    }

    // ------------------------------------------------------------ execution

    pub fn execute(&mut self, stmt: &Statement) -> Result<Response> {
        self.execute_with(stmt, &[])
    }

    /// Read-only execution. Because it takes `&self`, several readers can run
    /// at once under an `RwLock`; statements that need to write are rejected
    /// (the caller separates them first with [`Statement::is_read_only`]).
    pub fn query(&self, stmt: &Statement, params: &[Value]) -> Result<Response> {
        match stmt {
            Statement::Select(sel) => Ok(Response::Rows(self.select(sel, params)?)),
            Statement::Explain(sel) => Ok(Response::Rows(self.explain(sel, params)?)),
            Statement::ListCollections => Ok(Response::Schemas(
                self.order
                    .iter()
                    .map(|n| self.collections[n].schema.clone())
                    .collect(),
            )),
            Statement::Describe(name) => Ok(Response::Schemas(vec![self
                .collection(name)?
                .schema
                .clone()])),
            _ => Err(Error::Query("this statement requires write access".into())),
        }
    }

    /// Full execution, writes included. The watcher is woken once *after* the
    /// statement finishes -- not per document: a `put` of 10 000 documents is
    /// a single wake-up, and the subscriber will read one batch anyway.
    pub fn execute_with(&mut self, stmt: &Statement, params: &[Value]) -> Result<Response> {
        if !stmt.is_read_only() {
            self.refuse_if_failed()?;
            // `compact` changes no document, only how the file holds them.
            if self.history.following && !matches!(stmt, Statement::Compact(_)) {
                return Err(Error::ReadOnly(
                    "this database is a replica: its writes come from its primary".into(),
                ));
            }
        }
        let before = self.changes.seq();
        let out = self.execute_inner(stmt, params);
        let after = self.changes.seq();
        if after != before {
            if let Some(w) = &self.watcher {
                w.notify(after);
            }
        }
        // The browser module compares text only in the collation data it
        // has been handed. A read that reached for more is refused rather
        // than answered in the order of what it had, and runs again once
        // the module has it; a write was checked before it changed anything
        // (`put`, `update`, `delete`), and one refused leaves its note for
        // the module to read.
        if collate::PARTIAL && out.is_ok() {
            let missing = collate::take_missing();
            if stmt.is_read_only() {
                collate::refuse(missing)?;
            }
        }
        out
    }

    fn execute_inner(&mut self, stmt: &Statement, params: &[Value]) -> Result<Response> {
        match stmt {
            Statement::CreateCollection {
                schema,
                if_not_exists,
            } => self.create_collection(schema.clone(), *if_not_exists),
            Statement::DropCollection { name, if_exists } => self.drop_collection(name, *if_exists),
            Statement::CreateIndex {
                collection,
                field,
                kind,
                if_not_exists,
            } => self.create_index(collection, field, kind, *if_not_exists),
            Statement::Put { collection, docs } => self.put(collection, docs, params),
            Statement::Select(sel) => Ok(Response::Rows(self.select(sel, params)?)),
            Statement::Explain(sel) => Ok(Response::Rows(self.explain(sel, params)?)),
            Statement::Update {
                collection,
                set,
                filter,
            } => self.update(collection, set, filter, params),
            Statement::Delete { collection, filter } => self.delete(collection, filter, params),
            Statement::ListCollections => Ok(Response::Schemas(
                self.order
                    .iter()
                    .map(|n| self.collections[n].schema.clone())
                    .collect(),
            )),
            Statement::Describe(name) => Ok(Response::Schemas(vec![self
                .collection(name)?
                .schema
                .clone()])),
            Statement::Compact(which) => self.compact(which.as_deref(), true),
        }
    }

    fn create_collection(&mut self, schema: Schema, if_not_exists: bool) -> Result<Response> {
        if self.collections.contains_key(&schema.name) {
            if if_not_exists {
                return Ok(Response::Ok(format!(
                    "collection `{}` already exists",
                    schema.name
                )));
            }
            return Err(Error::Exists(format!("collection `{}`", schema.name)));
        }
        let cid = self.next_coll_id;
        self.next_coll_id += 1;
        let name = schema.name.clone();
        self.wal(REC_CREATE, cid, &schema.encode())?;
        self.note(cid, SCHEMA_MARK);
        self.collections
            .insert(name.clone(), Collection::new(cid, schema));
        self.order.push(name.clone());
        Ok(Response::Ok(format!("collection `{name}` created")))
    }

    fn drop_collection(&mut self, name: &str, if_exists: bool) -> Result<Response> {
        match self.collections.remove(name) {
            Some(c) => {
                self.order.retain(|n| n != name);
                self.wal(REC_DROP, c.id, &[])?;
                self.note(c.id, SCHEMA_MARK);
                Ok(Response::Ok(format!("collection `{name}` dropped")))
            }
            None if if_exists => Ok(Response::Ok(format!("no collection `{name}`"))),
            None => Err(Error::NotFound(format!("collection `{name}`"))),
        }
    }

    /// Builds an index on an existing field and fills it from the current
    /// documents, all under the write lock; a server runs it beside the
    /// database instead ([`Database::maintain`]).
    fn create_index(
        &mut self,
        collection: &str,
        field: &str,
        kind: &IndexKind,
        if_not_exists: bool,
    ) -> Result<Response> {
        if let Some(feature) = missing_feature(kind) {
            return Err(not_built("the index", feature));
        }
        if let Some(done) = self.check_index(collection, field, kind, if_not_exists)? {
            return Ok(done);
        }
        let c = self.collections.get_mut(collection).unwrap();
        let cid = c.id;
        let pos = c.schema.field_pos(field).unwrap();
        c.schema.fields[pos].index = kind.resolved();
        build_index(c, pos)?;

        let encoded = c.schema.encode();
        self.wal(REC_ALTER, cid, &encoded)?;
        self.note(cid, SCHEMA_MARK);
        Ok(Response::Ok(format!(
            "index built on `{collection}.{field}`"
        )))
    }

    fn build_document(
        &self,
        schema: &Schema,
        pairs: &[(String, Expr)],
        params: &[Value],
    ) -> Result<Document> {
        let ctx = EvalCtx {
            params,
            registry: &self.registry,
        };
        let mut doc = Document::default();
        let mut explicit_id: Option<DocId> = None;
        for (k, e) in pairs {
            let v = eval(e, &mut NoRow, &ctx)?;
            if k == "id" {
                explicit_id = match v {
                    Value::Int(i) if i > 0 => Some(i as u64),
                    _ => return Err(Error::Type("`id` must be a positive integer".into())),
                };
                continue;
            }
            let f = schema.field(k).ok_or_else(|| {
                Error::NotFound(format!("field `{k}` in collection `{}`", schema.name))
            })?;
            doc.set(k, v.coerce(&f.ty)?);
        }
        for f in &schema.fields {
            if doc.get(&f.name).is_none() {
                if f.required {
                    return Err(Error::Type(format!("field `{}` is required", f.name)));
                }
                doc.set(&f.name, Value::Null);
            }
        }
        doc.id = explicit_id.unwrap_or(0);
        Ok(doc)
    }

    fn put(
        &mut self,
        collection: &str,
        docs: &[Vec<(String, Expr)>],
        params: &[Value],
    ) -> Result<Response> {
        let schema = self.collection(collection)?.schema.clone();
        let cid = self.collection(collection)?.id;
        let hooks: Vec<_> = self.registry.hooks().to_vec();

        let mut built = Vec::with_capacity(docs.len());
        for pairs in docs {
            built.push(self.build_document(&schema, pairs, params)?);
        }
        if collate::PARTIAL {
            collate::refuse(
                built
                    .iter()
                    .fold(0, |m, d| m | collation_missing(&schema, d)),
            )?;
        }

        let mut n = 0usize;
        let mut written: Vec<Document> = Vec::with_capacity(built.len());
        for mut doc in built {
            let c = self.collections.get_mut(collection).unwrap();
            let op = if doc.id == 0 {
                doc.id = c.store.allocate_id();
                WriteOp::Insert
            } else if c.store.contains(doc.id) {
                WriteOp::Update
            } else {
                WriteOp::Insert
            };
            for h in &hooks {
                h.before_write(&schema, op, &mut doc)?;
            }
            // Drop the old index entries when overwriting.
            let old = match op {
                WriteOp::Update => c.store.read(&schema, doc.id)?,
                _ => None,
            };
            if let Some(old) = &old {
                c.unindex_doc(old, Some(&doc));
            }
            let payload = Store::encode_doc(&schema, &doc);
            let frame = c.store.append(OP_PUT, doc.id, &payload);
            c.index_scalar(&doc, old.as_ref());
            self.wal(REC_DATA, cid, &frame)?;
            self.note(cid, doc.id);
            for h in &hooks {
                h.after_write(collection, op, &doc)?;
            }
            written.push(doc);
            n += 1;
        }
        // Vectors are indexed in a batch: construction can parallelise.
        let c = self.collections.get_mut(collection).unwrap();
        c.index_vectors_batch(&written);
        Ok(Response::Affected(n))
    }

    /// Finds the ids of the documents matching the filter.
    fn matching_ids(
        &self,
        collection: &str,
        filter: &Option<Expr>,
        params: &[Value],
    ) -> Result<Vec<DocId>> {
        self.matching_ids_capped(collection, filter, params, None)
    }

    /// [`Self::matching_ids`], stopping after `cap` matches. The ids come out
    /// ascending either way, so the capped list is exactly the front of the
    /// full one. Before the cap a page of twenty evaluated the filter over
    /// every document: over a million rows `where price >= 0 limit 20` took
    /// 69.6 ms and now 0.002 ms, and `limit 20` with no filter at all took
    /// 2.4 ms, spent listing every id first.
    fn matching_ids_capped(
        &self,
        collection: &str,
        filter: &Option<Expr>,
        params: &[Value],
        cap: Option<usize>,
    ) -> Result<Vec<DocId>> {
        let c = self.collection(collection)?;
        let ctx = EvalCtx {
            params,
            registry: &self.registry,
        };
        let want = cap.unwrap_or(usize::MAX);

        let Some(f) = filter else {
            plan(|| "filter: none, rows in id order".into());
            return Ok(match cap {
                Some(k) => c.store.iter_ids().take(k).collect(),
                None => c.store.ids(),
            });
        };

        let mut out = Vec::new();
        let mut tested = 0usize;
        let scanned = match self.filter_candidates(c, f, params, want)? {
            // The filter is exactly what the index answered: no row needs a
            // second look.
            Some((mut b, true)) => {
                b.truncate(want);
                return Ok(b);
            }
            Some((b, false)) => {
                for id in b {
                    if out.len() >= want {
                        break;
                    }
                    tested += 1;
                    if row_matches(c, f, id, &ctx)? {
                        out.push(id);
                    }
                }
                false
            }
            // No index: the full scan, read lazily so a cap stops it early.
            None => {
                for id in c.store.iter_ids() {
                    if out.len() >= want {
                        break;
                    }
                    tested += 1;
                    if row_matches(c, f, id, &ctx)? {
                        out.push(id);
                    }
                }
                true
            }
        };
        plan(|| {
            let what = if scanned {
                format!("a full scan, {tested} of {} rows tested", c.store.len())
            } else {
                format!("{tested} candidates tested")
            };
            let stop = if out.len() == want {
                ", stopped at the page"
            } else {
                ""
            };
            format!("filter: {what}, {} matched{stop}", out.len())
        });
        Ok(out)
    }

    /// The rows an index narrows a filter to, ascending and live, and
    /// whether they are the answer itself -- `None` when no index narrows
    /// it and every row has to be tested. `want` is how many matches the
    /// caller will take, which bounds how wide a range is worth reading.
    fn filter_candidates(
        &self,
        c: &Collection,
        f: &Expr,
        params: &[Value],
        want: usize,
    ) -> Result<Option<(Vec<DocId>, bool)>> {
        // Hash index pushdown. The candidate set is picked from the smallest
        // of the indexable equalities in the `and` chain.
        let mut eqs = Vec::new();
        f.conjunct_equalities(params, &mut eqs);
        let mut candidates: Option<Vec<DocId>> = None;
        // Which index the candidates came from, for `explain`.
        let mut source: (&str, &str) = ("", "");
        for (field, val) in eqs {
            if field == "id" {
                let Some(hit) = id_candidates(&c.store, &[val]) else {
                    continue;
                };
                if candidates
                    .as_ref()
                    .map(|c| hit.len() < c.len())
                    .unwrap_or(true)
                {
                    candidates = Some(hit);
                    source = ("the id index", "id");
                }
                continue;
            }
            let Some(map) = c.hashes.get(field) else {
                continue;
            };
            // The bucket key is produced on the write path from the value
            // coerced to the field's type (`10` -> `10.0`), so the lookup has
            // to go through the same conversion. Otherwise `price = 10` on
            // `price float @hash` would find an empty bucket and silently
            // return 0 rows -- the mere presence of the index would change the
            // query's answer. A literal that cannot be coerced (`year = "abc"`)
            // skips the index and leaves the decision to the eval path.
            let Some(fd) = c.schema.field(field) else {
                continue;
            };
            let Ok(key) = val.clone().coerce(&fd.ty) else {
                continue;
            };
            let bucket = map.get(&hash_key(&key)).cloned().unwrap_or_default();
            if candidates
                .as_ref()
                .map(|c| bucket.len() < c.len())
                .unwrap_or(true)
            {
                candidates = Some(bucket);
                source = ("the hash index on", field);
            }
        }

        // `in` over a hash field is the union of one bucket per element. It
        // is compared against the equalities above under the same
        // smallest-wins rule, so a query carrying both still picks whichever
        // candidate set is narrower.
        let mut ins = Vec::new();
        f.conjunct_in_sets(params, &mut ins);
        // Whether the chosen candidate set *is* the answer. `in` needs its own
        // flag where a single equality has `is_bare_equality`: without it the
        // union is re-evaluated row by row, which on a 56 374-document bucket
        // costs 3.2 ms against the 90 us the same bucket takes as `= x`.
        let mut bare_in = false;
        for (field, vals) in ins {
            if field == "id" {
                let Some(hit) = id_candidates(&c.store, &vals) else {
                    continue;
                };
                if candidates
                    .as_ref()
                    .map(|c| hit.len() < c.len())
                    .unwrap_or(true)
                {
                    candidates = Some(hit);
                    source = ("the id index, `in`", "id");
                    bare_in = matches!(f, Expr::In(..));
                }
                continue;
            }
            let Some(map) = c.hashes.get(field) else {
                continue;
            };
            let Some(fd) = c.schema.field(field) else {
                continue;
            };
            let mut union: Vec<DocId> = Vec::new();
            let mut whole = true;
            for v in vals {
                // The same coercion the single equality needs, and for the
                // same reason. An element the index cannot express takes the
                // whole list back to the eval path: a union missing one
                // element's rows is a wrong answer, not a slow one.
                let Ok(key) = v.clone().coerce(&fd.ty) else {
                    whole = false;
                    break;
                };
                if let Some(bucket) = map.get(&hash_key(&key)) {
                    union.extend_from_slice(bucket);
                }
            }
            if !whole {
                continue;
            }
            // `sort` rather than `sort_unstable`: the union is k buckets laid
            // end to end, and a bucket is almost always already ascending
            // (ids are pushed in insertion order), so the merge sort joins
            // runs that exist instead of re-sorting them. Over 200 000 rows
            // where one bucket holds 56 374 of them: `in [2]` 901 -> 394 us,
            // `in [100]` 1 260 -> 940 us. Pattern-defeating quicksort does
            // not exploit multiple runs; this is the one place that matters.
            union.sort();
            // Two elements can coerce to the same key (`year in [2024,
            // 2024.0]`), and a document reached twice would be counted twice
            // by `count` and emitted twice by `get`.
            union.dedup();
            if candidates
                .as_ref()
                .map(|c| union.len() < c.len())
                .unwrap_or(true)
            {
                candidates = Some(union);
                source = ("the hash index, `in`, on", field);
                // Exact only when the list is the whole filter. With anything
                // `and`ed on, every candidate still has to be tested -- and a
                // filter holding an equality as well cannot be a bare `in`,
                // so this cannot be set by the wrong branch.
                bare_in = matches!(f, Expr::In(..));
            }
        }

        // Comparisons over an ordered index narrow to a range. It is collected
        // only while it stays smaller than the candidates already in hand and
        // than the scan it would replace: past half the collection the scan
        // is cheaper than gathering and sorting the ids, and a page of twenty
        // over a wide range is found sooner by the scan in id order.
        let mut bare_range = false;
        if !c.sorted.is_empty() {
            let mut ranges = Vec::new();
            f.conjunct_ranges(params, &mut ranges);
            let n = c.store.len();
            for (field, ix) in &c.sorted {
                if !ranges.iter().any(|r| r.0 == field) {
                    continue;
                }
                let Some(fd) = c.schema.field(field) else {
                    continue;
                };
                let Some((range, exact)) = sorted_range(fd, field, f, params) else {
                    continue;
                };
                let mut cap = candidates.as_ref().map_or(n / 2, |b| b.len()).min(n / 2);
                if want != usize::MAX {
                    cap = cap.min(want.saturating_mul(64).max(4096));
                }
                if let Some(ids) = ix.range_ids(&range, cap) {
                    candidates = Some(ids);
                    source = ("the ordered index on", field);
                    bare_range = exact && f.only_ranges_on(field, params);
                    bare_in = false;
                } else {
                    plan(|| {
                        format!(
                            "filter: the ordered index on {field} not used, \
                             its range holds more than {cap} rows"
                        )
                    });
                }
            }
        }

        Ok(candidates.map(|mut b| {
            b.sort_unstable();
            b.retain(|id| c.store.contains(*id));
            // If the filter is exactly that equality, or exactly a list that
            // was pushed down whole, or exactly the range an ordered index
            // expressed, no re-evaluation is needed.
            let exact = f.is_bare_equality(params) || bare_in || bare_range;
            plan(|| {
                let (index, field) = source;
                let named = if field == "id" {
                    index.to_string()
                } else {
                    format!("{index} {field}")
                };
                let then = if exact { ", which is the answer" } else { "" };
                format!("filter: {named}, {} rows{then}", b.len())
            });
            (b, exact)
        }))
    }

    /// A filtered `near`, finding the filter's rows only as far as the plan
    /// needs them.
    ///
    /// The plan follows the size of that set. At most `probe_budget` rows --
    /// about the number of distances the ANN would measure anyway -- are
    /// searched exactly; more go through the ANN, whose candidates are tested
    /// against the filter; and an ANN that comes up short, because the filter
    /// correlates with the vector, falls back to searching the whole set.
    ///
    /// Finding the whole set first was nearly all of the query when no index
    /// narrows the filter: `year >= 2020` over 200 000 x 128 took 16.4 ms,
    /// of which the ANN was 0.14. But the plan only needs to know whether
    /// the set is larger than the budget, so the rows are probed until it is
    /// (1.1 ms), and the rest is read only where the whole set would have
    /// been used -- so the answer is the same one, row for row, and a filter
    /// under the budget costs what it did (17.9 ms against 17.6).
    #[allow(clippy::too_many_arguments)]
    fn filtered_near(
        &self,
        c: &Collection,
        sp: &Space,
        f: &Expr,
        qv: &[f32],
        want: usize,
        near: &Near,
        params: &[Value],
        ctx: &EvalCtx,
    ) -> Result<Vec<(DocId, f32)>> {
        let ix = sp.ix;
        let budget = ix.probe_budget(near.ef);
        let mut probe = match self.filter_candidates(c, f, params, usize::MAX)? {
            Some((rows, true)) => FilterProbe::done(rows),
            Some((rows, false)) => FilterProbe::new(rows),
            None => FilterProbe::new(c.store.ids()),
        };
        let matches = |id: DocId| row_matches(c, f, id, ctx);
        // `exact` is the verification path: it always scans everything.
        let cap = if near.exact { usize::MAX } else { budget + 1 };
        let total = probe.rows.len();
        let whole = probe.run(cap, matches)?;
        // Rows an index answered exactly were not probed; that step said so.
        if total > 0 {
            plan(|| {
                let found = if whole {
                    "the whole set".to_string()
                } else {
                    format!("more than the ANN budget of {budget}")
                };
                format!(
                    "filter: probed {} of {total} rows, {} matched, {found}",
                    probe.tested,
                    probe.matched.len()
                )
            });
        }
        let field = &near.field;
        if whole {
            let ids = probe.into_sorted();
            let accept = |id: DocId| ids.binary_search(&id).is_ok();
            return Ok(if near.exact {
                plan(|| {
                    format!("near: exact scan over every vector in {field}, the set as a test")
                });
                sp.search_exact(qv, want, &accept)?
            } else if ids.len() <= budget {
                // The set is smaller than the number of candidates the ANN
                // walk would measure anyway: the walk buys nothing, and
                // searching the set directly is both cheaper and exact.
                plan(|| {
                    format!(
                        "near: the {} rows searched exactly, under the ANN budget of {budget}",
                        ids.len()
                    )
                });
                sp.search_ids(qv, want, near.ef, &ids)?
            } else {
                let hits = sp.search(qv, want, near.ef, &mut |id| Ok(accept(id)))?;
                plan(|| {
                    format!(
                        "near: ANN over {field}, the set as a test, {} kept",
                        hits.len()
                    )
                });
                if hits.len() < want.min(ids.len()) {
                    plan(|| {
                        format!(
                            "near: the ANN came up short, the {} rows searched exactly",
                            ids.len()
                        )
                    });
                    sp.search_ids(qv, want, near.ef, &ids)?
                } else {
                    hits
                }
            });
        }
        // More rows match than the budget: the ANN, testing each candidate in
        // distance order as `search` would test membership -- over codes
        // only those still able to make the page (`Space::order`).
        let mut tested = 0;
        let hits = sp.search(qv, want, near.ef, &mut |id| {
            tested += 1;
            Ok(c.store.contains(id) && matches(id)?)
        })?;
        plan(|| {
            format!(
                "near: ANN over {field}, {}, {tested} candidates tested, {} kept",
                beam(ix, near, want),
                hits.len()
            )
        });
        // The filter is applied after the candidates are gathered, so a
        // filter correlated with the vector can eliminate all of them. If the
        // result comes up short the rest of the set is found and searched
        // exactly, as it always was.
        if hits.len() < want {
            probe.run(usize::MAX, matches)?;
            let ids = probe.into_sorted();
            if hits.len() < want.min(ids.len()) {
                plan(|| {
                    format!(
                        "near: the ANN came up short, the probe finished, {} rows searched exactly",
                        ids.len()
                    )
                });
                return sp.search_ids(qv, want, near.ef, &ids);
            }
        }
        Ok(hits)
    }

    /// `order <field> limit N` answered by walking an ordered index and
    /// stopping at the page -- `None` when that is not the better plan.
    ///
    /// The walk visits rows in order and tests the filter on each, so it wins
    /// when the page fills early; an equality over a hash index, an `in`, or
    /// a narrow range on another ordered field names a small set that is
    /// cheaper to sort, and those keep the sorting path. Without a `limit`
    /// every row is emitted anyway, and `required` needs every candidate, so
    /// both keep it too. A range on the ordered field itself bounds the walk.
    fn walk_order(
        &self,
        c: &Collection,
        sel: &Select,
        params: &[Value],
        ctx: &EvalCtx,
    ) -> Result<Option<Vec<DocId>>> {
        let [Sort {
            field,
            asc,
            collate,
        }] = sel.order.as_slice()
        else {
            return Ok(None);
        };
        // The index holds its field's order -- a collation's, or the
        // bytes' -- so a key in another one sorts.
        let own = c.schema.field(field).and_then(|f| f.collate);
        if collate.is_some_and(|c| Some(c) != own) {
            return Ok(None);
        }
        let (Some(ix), Some(limit)) = (c.sorted_index(field), sel.limit) else {
            return Ok(None);
        };
        if ix.has_nan() {
            plan(|| format!("order: the ordered index on {field} not walked, it holds a NaN"));
            return Ok(None);
        }
        if sel.lookup.as_ref().is_some_and(|l| l.required) {
            return Ok(None);
        }
        let want = limit.saturating_add(sel.offset);
        let mut range = None;
        let mut bare = true;
        if let Some(f) = &sel.filter {
            let indexed = |name: &str| name == "id" || c.hashes.contains_key(name);
            let mut eqs = Vec::new();
            f.conjunct_equalities(params, &mut eqs);
            let mut ins = Vec::new();
            f.conjunct_in_sets(params, &mut ins);
            if eqs.iter().any(|(n, _)| indexed(n)) || ins.iter().any(|(n, _)| indexed(n)) {
                plan(|| {
                    format!("order: the ordered index on {field} not walked, an equality names fewer rows")
                });
                return Ok(None);
            }
            let mut ranges = Vec::new();
            f.conjunct_ranges(params, &mut ranges);
            for (name, _, _) in &ranges {
                if name == field {
                    continue;
                }
                let (Some(other), Some(fd)) = (c.sorted_index(name), c.schema.field(name)) else {
                    continue;
                };
                if let Some((r, _)) = sorted_range(fd, name, f, params) {
                    if other.range_ids(&r, 4096).is_some() {
                        plan(|| {
                            format!(
                                "order: the ordered index on {field} not walked, \
                                 the range on {name} names fewer rows"
                            )
                        });
                        return Ok(None);
                    }
                }
            }
            let fd = c
                .schema
                .field(field)
                .expect("an ordered index has its field");
            let own = sorted_range(fd, field, f, params);
            bare = matches!(&own, Some((_, true))) && f.only_ranges_on(field, params);
            range = own.map(|(r, _)| r);
        }
        let mut out = Vec::with_capacity(want.min(4096));
        if want == 0 {
            return Ok(Some(out));
        }
        // A filter the index cannot narrow is tested as the walk passes each
        // row, and one that matches almost nothing would have the walk read
        // every row in key order: random reads, which over a million rows
        // took 237 ms against the scan's 121. Past an eighth of the
        // collection the scan is the better bet, and the rows walked so far
        // are what the wrong guess cost -- 1.25x the scan at worst, while a
        // filter matching 1% still fills the page in 0.29 ms against 125.
        let budget = (c.store.len() / 8).max(want);
        let (mut walked, mut gave_up) = (0usize, false);
        ix.walk(!asc, range.as_ref(), |id| {
            if !bare {
                if let Some(f) = &sel.filter {
                    walked += 1;
                    if walked > budget {
                        gave_up = true;
                        return Ok(false);
                    }
                    if !row_matches(c, f, id, ctx)? {
                        return Ok(true);
                    }
                }
            }
            out.push(id);
            Ok(out.len() < want)
        })?;
        plan(|| {
            let dir = if *asc { "" } else { " desc" };
            if gave_up {
                format!(
                    "order: walked the ordered index on {field}{dir}, gave up after {budget} rows \
                     with {} kept, back to the scan",
                    out.len()
                )
            } else if bare {
                format!(
                    "order: walked the ordered index on {field}{dir}, {} rows",
                    out.len()
                )
            } else {
                format!(
                    "order: walked the ordered index on {field}{dir}, {walked} rows tested, {} kept",
                    out.len()
                )
            }
        });
        Ok((!gave_up).then_some(out))
    }

    /// `near`'s first `want` candidates, nearest first -- the filter's rows
    /// only, when there is one.
    fn run_near(
        &self,
        c: &Collection,
        sel: &Select,
        near: &Near,
        want: usize,
        params: &[Value],
        ctx: &EvalCtx,
    ) -> Result<Vec<(DocId, f32)>> {
        if let Some(DataType::Sparse(dim)) = c.schema.field(&near.field).map(|f| &f.ty) {
            return self.run_sparse_near(c, sel, near, *dim, want, params, ctx);
        }
        let ix = c.vectors.get(&near.field).ok_or_else(|| {
            not_built_on(c, &near.field, "vector index").unwrap_or_else(|| {
                Error::Query(format!(
                    "field `{}` has no vector index (declare it with @hnsw)",
                    near.field
                ))
            })
        })?;
        let qv = near_vector(eval(&near.vector, &mut NoRow, ctx)?)?;
        if qv.len() != ix.dim {
            return Err(Error::Type(format!(
                "the query vector must have {} dimensions, got {}",
                ix.dim,
                qv.len()
            )));
        }

        let sp = Space::new(c, ix, &near.field);
        if ix.unlinked() > 0 && !near.exact {
            plan(|| {
                format!(
                    "near: {} vectors of {} not linked into the graph yet, each measured",
                    ix.unlinked(),
                    near.field
                )
            });
        }
        let hits = match &sel.filter {
            // No filter: ANN directly, or a full scan when asked for -- or
            // when tombstones cut the ANN's answer short. One call each to
            // the walk and the scan: a second call site inlined the walk
            // again, 819 bytes of the browser module.
            None => {
                if near.exact {
                    plan(|| format!("near: exact scan over every vector in {}", near.field));
                } else {
                    plan(|| format!("near: ANN over {}, {}", near.field, beam(ix, near, want)));
                }
                let (mut ef, mut exact, mut widened) = (near.ef, near.exact, false);
                loop {
                    if exact {
                        break sp.search_exact(&qv, want, &|_| true)?;
                    }
                    let hits = sp.search(&qv, want, ef, &mut |_| Ok(true))?;
                    match past_tombstones(ix, near, want, hits.len(), widened) {
                        Short::Whole => break hits,
                        Short::Wider(wider) => (ef, widened) = (Some(wider), true),
                        Short::Exact => exact = true,
                    }
                }
            }
            Some(f) => self.filtered_near(c, &sp, f, &qv, want, near, params, ctx)?,
        };
        Ok(hits)
    }

    /// `near` over a `sparse<N>` field: the documents with the largest dot
    /// product with the query, through the field's inverted index -- the
    /// exact top `want`, not an estimate, so there is no beam to widen -- or
    /// with `exact` every document scored, which is what the index is held
    /// to. Only a document sharing a dimension with the query is ranked, on
    /// either path. The filter is tested as the lists are merged, as
    /// `match` tests it.
    #[allow(clippy::too_many_arguments)]
    fn run_sparse_near(
        &self,
        c: &Collection,
        sel: &Select,
        near: &Near,
        dim: usize,
        want: usize,
        params: &[Value],
        ctx: &EvalCtx,
    ) -> Result<Vec<(DocId, f32)>> {
        let field = &near.field;
        let ix = c.sparse_index(field).ok_or_else(|| {
            not_built_on(c, field, "inverted index").unwrap_or_else(|| {
                Error::Query(format!(
                    "field `{field}` has no inverted index (declare it with @inverted)"
                ))
            })
        })?;
        if near.ef.is_some() {
            return Err(Error::Query(format!(
                "`ef` is the HNSW beam; `{field}` is searched exactly, through its inverted index"
            )));
        }
        let (d, q) = match eval(&near.vector, &mut NoRow, ctx)? {
            Value::Sparse(d, e) => crate::sparse::normalise(d, e).map_err(Error::Type)?,
            Value::Text(t) => crate::sparse::parse(&t)?,
            other => {
                return Err(Error::Type(format!(
                    "`near` on `{field}` expects a sparse vector, found {}",
                    other.type_name()
                )))
            }
        };
        if d as usize != dim {
            return Err(Error::Type(format!(
                "the query vector must have dimension {dim}, got {d}"
            )));
        }
        let allowed: Option<Vec<DocId>> = match &sel.filter {
            Some(_) => Some(self.matching_ids(&sel.collection, &sel.filter, params)?),
            None => None,
        };
        let accept = |id: DocId| {
            allowed
                .as_ref()
                .is_none_or(|l| l.binary_search(&id).is_ok())
        };
        if !near.exact {
            let hits = ix.search(&q, want, &accept);
            plan(|| {
                format!(
                    "near: the inverted index on {field}, {} dimensions, {} ranked",
                    q.len(),
                    hits.len()
                )
            });
            return Ok(hits);
        }
        plan(|| format!("near: exact scan over every sparse vector in {field}"));
        let pos = c
            .schema
            .field_pos(field)
            .expect("an indexed field is in the schema");
        let mut hits = Vec::new();
        for id in c.store.iter_ids() {
            if !accept(id) {
                continue;
            }
            if let Some(Value::Sparse(_, e)) = c.store.read_field(id, pos)? {
                if let Some(score) = crate::sparse::dot(&e, &q) {
                    hits.push((id, score as f32));
                }
            }
        }
        hits.sort_by(best_first);
        hits.truncate(want);
        Ok(hits)
    }

    /// `match ... near ... fuse`: each side ranks its own candidates -- the
    /// filter applied to both -- and a document's score is the sum of
    /// `1 / (k + rank)` over the lists it is on. A document one side missed
    /// still scores from the other; ties go to the lower id.
    ///
    /// Built from what the engine already has -- the two searches, the
    /// vector index's id map, the text index's order: written with types of
    /// its own it was 11 KB of the browser module, this way it is 2.
    #[allow(clippy::too_many_arguments)]
    fn run_fuse(
        &self,
        c: &Collection,
        sel: &Select,
        m: &Match,
        near: &Near,
        f: &Fuse,
        params: &[Value],
        ctx: &EvalCtx,
    ) -> Result<Vec<(DocId, f32)>> {
        // Each side ranks its own first `depth`, never fewer than the page:
        // a document neither list reaches cannot be on it.
        let page = ranked_rows(sel, "fuse", MAX_MATCH_ROWS)?;
        let depth = f.candidates.unwrap_or(DEFAULT_FUSE_CANDIDATES).max(page);
        if depth > MAX_MATCH_ROWS {
            return Err(Error::Query(format!(
                "`fuse` ranks at most {MAX_MATCH_ROWS} candidates a side, {depth} were requested"
            )));
        }
        let k = f.k.unwrap_or(DEFAULT_FUSE_K) as f32;
        let text = self.run_match(c, sel, m, depth, params, ctx)?;
        let vectors = self.run_near(c, sel, near, depth, params, ctx)?;
        plan(|| {
            format!(
                "fuse: reciprocal rank, k = {k}, over {} + {} candidates",
                text.len(),
                vectors.len()
            )
        });
        let mut fused: Vec<(DocId, f32)> = Vec::with_capacity(text.len() + vectors.len());
        let mut at: HashMap<DocId, u32> = HashMap::new();
        at.reserve(text.len() + vectors.len());
        let ids = text.iter().map(|h| h.0).chain(vectors.iter().map(|h| h.0));
        for (i, id) in ids.enumerate() {
            // Rank within its own list, from 1.
            let rank = if i < text.len() { i } else { i - text.len() };
            let w = 1.0 / (k + rank as f32 + 1.0);
            match at.get(&id) {
                Some(&slot) => fused[slot as usize].1 += w,
                None => {
                    at.insert(id, fused.len() as u32);
                    fused.push((id, w));
                }
            }
        }
        fused.sort_by(best_first);
        // Two lists can hold twice the ceiling between them; the answer
        // keeps to it, as `match` and `near` do.
        fused.truncate(page);
        Ok(fused)
    }

    /// `match`, and `rerank` on top of it when the query asks for one: the
    /// first `want` rows, best first.
    ///
    /// The two stages answer different questions. `match` is recall: cheap,
    /// lexical, and wrong about meaning. `rerank` is precision: exact vector
    /// distance, but only over what the first stage handed it. Neither needs
    /// an HNSW graph, which is the point -- see [`Rerank`].
    fn run_match(
        &self,
        c: &Collection,
        sel: &Select,
        m: &Match,
        want: usize,
        params: &[Value],
        ctx: &EvalCtx,
    ) -> Result<Vec<(DocId, f32)>> {
        let ix = c.texts.get(&m.field).ok_or_else(|| {
            not_built_on(c, &m.field, "full-text index").unwrap_or_else(|| {
                Error::Query(format!(
                    "field `{}` has no full-text index (declare it with @text)",
                    m.field
                ))
            })
        })?;
        let query = match eval(&m.query, &mut NoRow, ctx)? {
            Value::Text(t) => t,
            other => {
                return Err(Error::Type(format!(
                    "`match` expects text, found {}",
                    other.type_name()
                )))
            }
        };

        let allowed: Option<Vec<DocId>> = match &sel.filter {
            Some(_) => Some(self.matching_ids(&sel.collection, &sel.filter, params)?),
            None => None,
        };
        let accept = |id: DocId| match &allowed {
            Some(list) => list.binary_search(&id).is_ok(),
            None => true,
        };

        let Some(rr) = &sel.rerank else {
            let hits = ix.search(&query, want, accept);
            plan(|| {
                format!(
                    "match: the text index on {}, {} ranked",
                    m.field,
                    hits.len()
                )
            });
            return Ok(hits);
        };

        // The candidate set has to be at least as large as what the caller
        // asked for, or reranking would throw away rows it never scored.
        let candidates = rr.candidates.unwrap_or(DEFAULT_RERANK_CANDIDATES).max(want);
        if candidates > MAX_MATCH_ROWS {
            return Err(Error::Query(format!(
                "`rerank` scores at most {MAX_MATCH_ROWS} candidates, {candidates} were requested"
            )));
        }

        let pos = c
            .schema
            .field_pos(&rr.field)
            .ok_or_else(|| Error::NotFound(format!("field `{}`", rr.field)))?;
        let field = &c.schema.fields[pos];
        let DataType::Vector(dim, _) = field.ty else {
            return Err(Error::Type(format!(
                "`rerank` needs a vector field, `{}` is {}",
                rr.field,
                field.ty.name()
            )));
        };
        // No index is consulted: the vectors are read out of the store. When
        // the field does carry one its metric is reused, so `rerank` and
        // `near` over the same field agree on what "close" means.
        let metric = match &field.index {
            IndexKind::Vector(spec) => spec.metric,
            _ => Metric::Cosine,
        };
        let qv = near_vector(eval(&rr.vector, &mut NoRow, ctx)?)?;
        if qv.len() != dim {
            return Err(Error::Type(format!(
                "the rerank vector must have {dim} dimensions, got {}",
                qv.len()
            )));
        }
        let hits = ix.search(&query, candidates, accept);
        plan(|| {
            format!(
                "match: the text index on {}, {} candidates",
                m.field,
                hits.len()
            )
        });
        plan(|| format!("rerank: {}, exact distance read from the store", rr.field));
        let mut ids = hits.into_iter().map(|h| (h.0, f32::NEG_INFINITY));
        Ok(
            order_exactly(&c.store, pos, metric, &qv, &mut ids, want, &mut |_| {
                Ok(true)
            })?
            .0,
        )
    }

    /// Collects the children of each parent row.
    ///
    /// One bucket probe per parent, which is the same plan an application
    /// would write by hand as a page query plus one indexed query per row --
    /// measured at 0.265 ms for twenty parents through `/batch`. The clause
    /// does not make that cheaper; it removes the round trips and the
    /// regrouping, and it makes `limit` mean "per parent", which is the part
    /// no join can express.
    /// The parts of a `lookup` that do not depend on a row: where the
    /// children live, how they are addressed, and where the parent's key
    /// sits. Resolved once per level by both passes.
    fn lookup_plan<'a>(
        &'a self,
        parent: &Collection,
        l: &Lookup,
    ) -> Result<(&'a Collection, Probe<'a>, Option<usize>)> {
        let child = self.collection(&l.collection)?;
        let probe = Probe::resolve(child, &l.child_field, &l.collection)?;
        // `id` is not a schema field but is the commonest key on both sides.
        let parent_pos = if l.parent_field == "id" {
            None
        } else {
            Some(
                parent
                    .schema
                    .field_pos(&l.parent_field)
                    .ok_or_else(|| Error::NotFound(format!("field `{}`", l.parent_field)))?,
            )
        };
        let parent_ty = match parent_pos {
            None => DataType::Int,
            Some(p) => parent.schema.fields[p].ty.clone(),
        };
        check_key_types(&parent_ty, &probe.key_type(), l)?;
        Ok((child, probe, parent_pos))
    }

    /// The whole chain resolved once, outermost first.
    ///
    /// Every level costs two map lookups and a type check. One resolution
    /// per level is nothing; one per *row* would not be, and both the
    /// collecting pass and the existence check behind `required` walk rows
    /// at every depth. Holding the resolved levels in a slice also turns the
    /// recursion into an index, so nothing below has to re-derive where it is.
    fn lookup_chain<'a>(&'a self, parent: &'a Collection, l: &'a Lookup) -> Result<Vec<Step<'a>>> {
        let mut steps: Vec<Step<'a>> = Vec::new();
        let mut above = parent;
        let mut cur = Some(l);
        while let Some(step) = cur {
            let (child, probe, parent_pos) = self.lookup_plan(above, step)?;
            steps.push(Step {
                l: step,
                parent: above,
                child,
                probe,
                parent_pos,
            });
            above = child;
            cur = step.next.as_deref();
        }
        Ok(steps)
    }

    /// The children a `@hash` equality inside the child filter names
    /// directly, or `None` when the filter holds no equality an index can
    /// answer.
    ///
    /// `Some(&[])` is a real answer and a cheap one: the equality names a
    /// bucket that does not exist, so no child matches and therefore no
    /// parent does.
    ///
    /// An equality the child cannot index is skipped, not fatal -- the same
    /// smallest-wins walk `matching_ids` makes. Returning `None` at the first
    /// one instead made the plan depend on a conjunct that has nothing to do
    /// with it: over 20 000 parents and 200 000 children, `stars = 5 and tag
    /// = "x"` with no `@hash` on `tag` fell back to the parent side at 13.06
    /// ms, while the identical question written `tag ~ "x"` -- never
    /// collected as an equality, so never in the way -- took 8.14 ms. The
    /// candidates are re-filtered in full either way, so the bucket is
    /// always safe to use; it only has to be the smallest one on offer.
    fn child_candidates<'a>(
        child: &'a Collection,
        filter: &Expr,
        params: &[Value],
    ) -> Option<&'a [DocId]> {
        let mut eqs = Vec::new();
        filter.conjunct_equalities(params, &mut eqs);
        let mut best: Option<&[DocId]> = None;
        for (field, val) in eqs {
            let Some(map) = child.hashes.get(field) else {
                continue;
            };
            let Some(fd) = child.schema.field(field) else {
                continue;
            };
            let Ok(key) = val.clone().coerce(&fd.ty) else {
                continue;
            };
            let bucket: &[DocId] = map.get(&hash_key(&key)).map_or(&[], |b| b.as_slice());
            if best.map(|b| bucket.len() < b.len()).unwrap_or(true) {
                best = Some(bucket);
            }
        }
        best
    }

    /// `required` answered from the child side: read the candidate children,
    /// keep the ones the filter passes, and take their parent keys.
    ///
    /// Only for the shape where the parent's key is `id`. That is the
    /// foreign-key-to-primary-key case, and it is the only one where this
    /// plan pays: the child's join field already holds parent ids, so
    /// mapping a key back to its parents is integer work -- sort, dedup,
    /// binary search -- and not one parent is read.
    ///
    /// A named key was written and measured and is not here. Mapping values
    /// back means encoding ~40 000 of them and sorting the byte strings,
    /// which costs more than it saves: over 20 000 parents and 200 000
    /// children it came out 2-4% slower with a `@hash` on the parent's field
    /// and 21-34% slower without one. The win in the `id` case is the cheap
    /// mapping, not the direction of the walk.
    fn retain_via_children(
        &self,
        steps: &[Step],
        ids: Vec<DocId>,
        candidates: &[DocId],
        ctx: &EvalCtx,
    ) -> Result<Vec<DocId>> {
        let l = steps[0].l;
        let child = steps[0].child;
        let child_pos = if l.child_field == "id" {
            None
        } else {
            Some(
                child
                    .schema
                    .field_pos(&l.child_field)
                    .ok_or_else(|| Error::NotFound(format!("field `{}`", l.child_field)))?,
            )
        };

        let mut keys: Vec<DocId> = Vec::new();
        for &cid in candidates {
            // The bucket can name a document that is gone.
            if !child.store.contains(cid) {
                continue;
            }
            // The bucket covers one equality out of the filter; the rest of
            // it still has to be evaluated -- and so does whatever a
            // `required` level below adds, which the bucket knows nothing
            // about. It stays a valid superset either way: a deeper level
            // only ever removes children.
            if !survives(steps, 0, cid, ctx)? {
                continue;
            }
            let k = match child_pos {
                None => Value::Int(cid as i64),
                Some(p) => child.store.read_field(cid, p)?.unwrap_or(Value::Null),
            };
            // The same rule the probe follows from the other side: a NULL
            // key matches nothing, and a value the id space cannot express
            // names no parent.
            let Ok(Value::Int(i)) = k.coerce(&DataType::Int) else {
                continue;
            };
            if let Ok(id) = DocId::try_from(i) {
                keys.push(id);
            }
        }
        keys.sort_unstable();
        keys.dedup();
        Ok(ids
            .into_iter()
            .filter(|id| keys.binary_search(id).is_ok())
            .collect())
    }

    /// Drops the parents that no child matches, for `required`.
    ///
    /// It runs before the ordering and before `limit`, because it decides
    /// who is on the page: filtering afterwards would answer a request for
    /// twenty rows with three. It stops at the first child that passes, so a
    /// parent with a thousand matches costs what one with a single match
    /// costs, and the collecting pass afterwards still only sees the page.
    ///
    /// The cost is one bucket probe per candidate parent -- there is no
    /// index from "a child matching this filter" back to its parent, and
    /// inventing one would be a second index to build, hold and validate. A
    /// field on the parent, maintained on write, remains the cheap way to
    /// ask this often.
    fn retain_with_children(
        &self,
        parent: &Collection,
        l: &Lookup,
        ids: Vec<DocId>,
        ctx: &EvalCtx,
    ) -> Result<Vec<DocId>> {
        let steps = self.lookup_chain(parent, l)?;
        let (child, probe, parent_pos) = {
            let s = &steps[0];
            (s.child, &s.probe, s.parent_pos)
        };

        // Two plans answer this question, and every number that decides
        // between them is exact and already in hand: the candidate parents
        // are a vector whose length is known, a `@hash` bucket's length is an
        // O(1) read, and so is the child collection's size. This is the
        // smallest-wins comparison the planner already makes inside one
        // collection, reaching across the clause.
        //
        // Walking the parents costs one probe each plus however many children
        // it reads before one passes. The bucket's share of the collection is
        // the equality's selectivity `p`, so that is about `1/p` children per
        // parent; reading the bucket instead costs `|bucket|` outright, and
        // `|bucket| = p * n`. Child-driven wins when
        //
        //     |ids| / p  >  |bucket|      i.e.   |ids| * n  >  |bucket|^2
        //
        // which is derived rather than fitted -- the only assumption is that
        // the matching children are spread across parents rather than piled
        // on a few, and where they are piled the comparison errs toward the
        // bucket, which is the bounded side.
        //
        // Measured over 20 000 parents and 200 007 children with a 40 013
        // bucket (threshold 8 004), parent-driven against child-driven:
        // 21 parents 1.10/5.01 ms, 736 1.76/5.87, 4 172 7.67/10.13,
        // 10 932 19.40/17.07, 20 000 36.84/30.68. The rule picks the winner
        // at every one of them.
        if l.parent_field == "id" {
            if let Some(f) = &l.filter {
                if let Some(cands) = Self::child_candidates(child, f, ctx.params) {
                    let n = child.store.len() as u64;
                    let b = cands.len() as u64;
                    if (ids.len() as u64).saturating_mul(n) > b.saturating_mul(b) {
                        let parents = ids.len();
                        let kept = self.retain_via_children(&steps, ids, cands, ctx)?;
                        plan(|| {
                            format!(
                                "required: from the child side, {} children in a hash bucket, \
                                 {} of {parents} parents kept",
                                cands.len(),
                                kept.len()
                            )
                        });
                        return Ok(kept);
                    }
                }
            }
        }

        let mut out = Vec::new();
        let parents = ids.len();
        for id in ids {
            let key = match parent_pos {
                None => Value::Int(id as i64),
                Some(p) => parent.store.read_field(id, p)?.unwrap_or(Value::Null),
            };
            // A child only counts if it survives what is below it too: with
            // a `required` level further down, a child that passes its own
            // `where` may still have nothing hanging off it.
            if probe.any(child, &key, |cid| survives(&steps, 0, cid, ctx))? {
                out.push(id);
            }
        }
        plan(|| {
            format!(
                "required: from the parent side, {} parents probed in {}, {} kept",
                parents,
                l.collection,
                out.len()
            )
        });
        Ok(out)
    }

    fn run_lookup(
        &self,
        parent: &Collection,
        l: &Lookup,
        ids: &[DocId],
        ctx: &EvalCtx,
    ) -> Result<Nested> {
        let steps = self.lookup_chain(parent, l)?;
        self.run_level(&steps, 0, ids, ctx)
    }

    /// One level of the chain, then the level below it.
    ///
    /// The rows it is given are the level above's, and the rows it produces
    /// are what the next call is given -- concatenated in group order, which
    /// is exactly the alignment `Nested` documents. Only the ids travel: a
    /// level reads its keys out of the store, never out of the projected
    /// values, so a chain works no matter what the level above selected.
    fn run_level(
        &self,
        steps: &[Step],
        depth: usize,
        ids: &[DocId],
        ctx: &EvalCtx,
    ) -> Result<Nested> {
        let Step {
            l, child, probe, ..
        } = &steps[depth];
        let (l, child) = (*l, *child);

        let columns = projection_columns(&child.schema, &l.project);
        let mut sources = Vec::with_capacity(columns.len());
        for col in &columns {
            if col == "id" {
                sources.push(None);
                continue;
            }
            sources.push(Some(child.schema.field_pos(col).ok_or_else(|| {
                Error::NotFound(format!("field `{}.{col}`", l.collection))
            })?));
        }

        let mut keys = Vec::with_capacity(l.order.len());
        let owner = format!("{}.", l.collection);
        for s in &l.order {
            keys.push(order_key(&child.schema, s, &owner)?);
        }

        let limit = l.limit.unwrap_or(usize::MAX);
        let mut groups = Vec::with_capacity(ids.len());
        let mut bucket: Vec<DocId> = Vec::new();

        for &pid in ids {
            let key = steps[depth].key(pid)?;
            probe.ids(child, &key, &mut bucket);

            // The child filter is evaluated over the bucket rather than sent
            // through `matching_ids`. The bucket is already one parent's
            // children, so a second index lookup would have to be
            // intersected with it and would almost always cost more than
            // reading the handful of rows it is narrowing.
            //
            // `survives` also drops a child a `required` level below has
            // nothing for, and it does so here -- before the ordering and
            // before `offset` and `limit` -- for the reason `required`
            // already runs there: it decides who is on the page, so cutting
            // afterwards would answer a request for three with one.
            let mut kept: Vec<DocId> = Vec::new();
            for &cid in &bucket {
                if survives(steps, depth, cid, ctx)? {
                    kept.push(cid);
                }
            }

            if !keys.is_empty() {
                // Read the keys up front and sort on them, exactly as the
                // parent path does: comparing through the store would read
                // every row log(n) times.
                let mut keyed: Vec<(Vec<Value>, DocId)> = Vec::with_capacity(kept.len());
                for id in &kept {
                    let mut vals = Vec::with_capacity(keys.len());
                    for (pos, ..) in &keys {
                        vals.push(match pos {
                            None => Value::Int(*id as i64),
                            Some(p) => child.store.read_field(*id, *p)?.unwrap_or(Value::Null),
                        });
                    }
                    keyed.push((vals, *id));
                }
                // The id breaks a tie, ascending whichever way the keys
                // run. A stable sort over an ascending-id input already
                // decided ties that way, so this changes no answer -- it
                // makes the order total, which is what lets the selection
                // below pick the same rows the full sort would.
                let cmp = |a: &(Vec<Value>, DocId), b: &(Vec<Value>, DocId)| {
                    rank(&keys, &a.0, &b.0).then(a.1.cmp(&b.1))
                };
                // `lookup` is the one place a bounded `order` is known up
                // front: the clause carries its own `limit`, so only
                // `offset + limit` children can ever be emitted and the rest
                // never need an order at all. Elsewhere `order` has no such
                // guarantee, which is why the engine sorts in full there.
                let want = l.offset.saturating_add(limit);
                if want < keyed.len() {
                    keyed.select_nth_unstable_by(want, cmp);
                    keyed.truncate(want);
                }
                keyed.sort_by(cmp);
                kept = keyed.into_iter().map(|(_, id)| id).collect();
            }

            let mut group = Vec::new();
            for cid in kept.into_iter().skip(l.offset) {
                if group.len() >= limit {
                    break;
                }
                let mut values = Vec::with_capacity(sources.len());
                for src in &sources {
                    values.push(match src {
                        None => Value::Int(cid as i64),
                        Some(p) => child.store.read_field(cid, *p)?.unwrap_or(Value::Null),
                    });
                }
                group.push(Row {
                    id: cid,
                    values,
                    score: None,
                });
            }
            groups.push(group);
        }

        plan(|| {
            let by = match probe {
                Probe::Id => "the id",
                Probe::Hash(..) => "the hash index",
            };
            format!(
                "lookup: {} on {}, {by} probed for {} parents, {} children",
                l.collection,
                l.child_field,
                ids.len(),
                groups.iter().map(Vec::len).sum::<usize>()
            )
        });
        // The level below is aligned to these rows read left to right, so it
        // is handed exactly that: one flat list of ids, in group order.
        let nested = match steps.get(depth + 1) {
            None => None,
            Some(_) => {
                let below: Vec<DocId> = groups.iter().flatten().map(|r| r.id).collect();
                Some(Box::new(self.run_level(steps, depth + 1, &below, ctx)?))
            }
        };

        Ok(Nested {
            name: l.collection.clone(),
            columns,
            groups,
            nested,
        })
    }

    /// Runs the query and answers with the path it took instead of its
    /// rows: one row a step, in the order the steps ran.
    fn explain(&self, sel: &Select, params: &[Value]) -> Result<ResultSet> {
        PLAN.with(|p| *p.borrow_mut() = Some(Vec::new()));
        let result = self.select(sel, params);
        let mut steps = PLAN.with(|p| p.borrow_mut().take()).unwrap_or_default();
        let rs = result?;
        steps.push(match rs.rows.first().map(|r| &r.values[..]) {
            Some([Value::Int(n)]) if sel.count => format!("count: {n}"),
            _ => format!("rows: {}", rs.rows.len()),
        });
        Ok(ResultSet {
            columns: vec![PLAN_COLUMN.to_string()],
            rows: steps
                .into_iter()
                .map(|s| Row {
                    id: 0,
                    values: vec![Value::Text(s)],
                    score: None,
                })
                .collect(),
            nested: None,
        })
    }

    fn select(&self, sel: &Select, params: &[Value]) -> Result<ResultSet> {
        let c = self.collection(&sel.collection)?;
        let ctx = EvalCtx {
            params,
            registry: &self.registry,
        };
        sel.check()?;
        if !sel.aggregate.is_empty() {
            return self.aggregate(c, sel, params);
        }

        // `count` sends the filter down the same path but never decodes the
        // rows: it returns a single row with a single column.
        if sel.count {
            let mut ids = self.matching_ids(&sel.collection, &sel.filter, params)?;
            // `count` reaches here with a `lookup` only when it is
            // `required`, so the children are a filter and nothing is being
            // attached: the number is how many parents have a match.
            if let Some(l) = &sel.lookup {
                ids = self.retain_with_children(c, l, ids, &ctx)?;
            }
            let n = ids.len();
            return Ok(ResultSet {
                columns: vec![COUNT_COLUMN.to_string()],
                rows: vec![Row {
                    id: 0,
                    values: vec![Value::Int(n as i64)],
                    score: None,
                }],
                nested: None,
            });
        }

        let columns = projection_columns(&c.schema, &sel.project);
        for col in &columns {
            if col != "id" && c.schema.field(col).is_none() {
                return Err(Error::NotFound(format!("field `{col}`")));
            }
        }

        let limit = sel.limit.unwrap_or(usize::MAX);
        let scored: Vec<(DocId, Option<f32>)>;

        if let (Some(m), Some(near), Some(f)) = (&sel.matcher, &sel.near, &sel.fuse) {
            scored = with_scores(self.run_fuse(c, sel, m, near, f, params, &ctx)?);
        } else if let Some(m) = &sel.matcher {
            let want = ranked_rows(sel, "match", MAX_MATCH_ROWS)?;
            scored = with_scores(self.run_match(c, sel, m, want, params, &ctx)?);
        } else if let Some(near) = &sel.near {
            // `near` decides the ordering by similarity; a second ordering is
            // rejected explicitly rather than ignored silently.
            if !sel.order.is_empty() {
                return Err(Error::Query(
                    "`near` cannot be combined with `order`: near orders results by similarity"
                        .into(),
                ));
            }
            let want = ranked_rows(sel, "near", MAX_NEAR_ROWS)?;
            scored = with_scores(self.run_near(c, sel, near, want, params, &ctx)?);
        } else if let Some(ids) = self.walk_order(c, sel, params, &ctx)? {
            scored = ids.into_iter().map(|id| (id, None)).collect();
        } else {
            // With no ordering the page is the first `offset + limit` matches
            // in id order, so the scan can stop there. `required` drops
            // parents after the filter and so still needs every candidate.
            let required = sel.lookup.as_ref().is_some_and(|l| l.required);
            let cap = if sel.order.is_empty() && !required {
                sel.limit.map(|l| l.saturating_add(sel.offset))
            } else {
                None
            };
            let mut ids = self.matching_ids_capped(&sel.collection, &sel.filter, params, cap)?;
            // `required` decides who is on the page, so it runs before the
            // ordering and before `limit`. Dropping rows afterwards would
            // answer a request for twenty with however many happened to
            // survive.
            if let Some(l) = &sel.lookup {
                if l.required {
                    ids = self.retain_with_children(c, l, ids, &ctx)?;
                }
            }
            if !sel.order.is_empty() {
                // `id` is not a schema field but must still be sortable -- the
                // field lookup used to happen first, so `order id` raised an
                // error.
                let mut keys = Vec::with_capacity(sel.order.len());
                for s in &sel.order {
                    keys.push(order_key(&c.schema, s, "")?);
                }
                let k = sel
                    .limit
                    .map(|l| l.saturating_add(sel.offset))
                    .unwrap_or(usize::MAX);
                plan(|| {
                    // Built by hand: `join` brought a 1.2 KB copy of its own
                    // into the browser module for this one line.
                    let mut by = String::new();
                    // The collation in force, named or the field's.
                    for (i, (s, key)) in sel.order.iter().zip(&keys).enumerate() {
                        by.push_str(if i == 0 { "" } else { ", " });
                        by.push_str(&s.field);
                        if let Some(c) = key.2 {
                            by.push_str(" collate ");
                            by.push_str(c.name());
                        }
                        by.push_str(if s.asc { "" } else { " desc" });
                    }
                    let kept = if k < ids.len() {
                        format!("the first {k} put in order")
                    } else {
                        "all put in order".to_string()
                    };
                    format!("order: {by}, every key read, {} matches, {kept}", ids.len())
                });
                ids = order_ids(&c.store, &ids, &keys, k)?;
            }
            scored = ids.into_iter().map(|id| (id, None)).collect();
        }

        let mut rows = Vec::new();
        for (id, score) in scored.into_iter().skip(sel.offset) {
            if rows.len() >= limit {
                break;
            }
            let mut values = Vec::with_capacity(columns.len());
            for col in &columns {
                if col == "id" {
                    values.push(Value::Int(id as i64));
                } else {
                    let pos = c.schema.field_pos(col).unwrap();
                    values.push(c.store.read_field(id, pos)?.unwrap_or(Value::Null));
                }
            }
            rows.push(Row { id, values, score });
        }

        // Children are attached after the parent page is decided, so a
        // `limit 20` probes twenty buckets and not one per matching row.
        // `check` has already refused `count`, `near` and `match` alongside
        // `lookup`, which is what lets this sit at the end of every path
        // rather than forking one.
        let nested = match &sel.lookup {
            None => None,
            Some(l) => {
                let ids: Vec<DocId> = rows.iter().map(|r| r.id).collect();
                Some(self.run_lookup(c, l, &ids, &ctx)?)
            }
        };

        Ok(ResultSet {
            columns,
            rows,
            nested,
        })
    }

    /// An aggregating select: the rows the filter finds -- through the same
    /// indexes any select uses -- folded into one row, or one per value of
    /// the `group` field, each row in the select list's order.
    ///
    /// Written as plain loops over types the engine already has -- the hash
    /// index's map, `order`'s sort: the first version, in iterator chains
    /// over types of its own, was 25 KB of the browser module.
    fn aggregate(&self, c: &Collection, sel: &Select, params: &[Value]) -> Result<ResultSet> {
        let pos_of = |name: &str| {
            c.schema
                .field_pos(name)
                .ok_or_else(|| Error::NotFound(format!("field `{name}`")))
        };
        // Every field the list and the group read, each read once a row, in
        // field order. Kept sorted as it is built: a handful of fields, and
        // `sort` over them was a sort instantiation of its own in the
        // browser module.
        let mut positions: Vec<usize> = Vec::new();
        let names = sel.aggregate.iter().filter_map(Agg::field);
        for f in names.chain(sel.group.as_deref()) {
            let p = pos_of(f)?;
            if let Err(i) = positions.binary_search(&p) {
                positions.insert(i, p);
            }
        }
        let slot_of = |pos: usize| positions.iter().position(|p| *p == pos).unwrap_or(0);
        // Each item's slot in a row read, and its fold as a group starts it.
        let mut slots = Vec::with_capacity(sel.aggregate.len());
        let mut start = Vec::with_capacity(sel.aggregate.len());
        for a in &sel.aggregate {
            match a.field() {
                Some(f) => {
                    let pos = pos_of(f)?;
                    slots.push(Some(slot_of(pos)));
                    let f = &c.schema.fields[pos];
                    start.push(Fold::new(a, &f.ty, f.collate)?);
                }
                None => {
                    slots.push(None);
                    start.push(Fold::new(a, &DataType::Int, None)?);
                }
            }
        }
        let group = match &sel.group {
            Some(g) => Some(slot_of(pos_of(g)?)),
            None => None,
        };

        let ids = self.matching_ids(&sel.collection, &sel.filter, params)?;
        // A group's number under its key's encoding. The hash index's own map
        // type, holding one number: a map of another type was 1 KB of the
        // browser module.
        let mut index: HashMap<Vec<u8>, Vec<DocId>> = HashMap::new();
        let mut keys: Vec<Value> = Vec::new();
        let mut folds: Vec<Vec<Fold>> = Vec::new();
        if group.is_none() {
            keys.push(Value::Null);
            folds.push(start.clone());
        }
        let mut row = Vec::with_capacity(positions.len());
        let mut key = Vec::new();
        for &id in &ids {
            if !c.store.read_fields(id, &positions, &mut row)? {
                continue;
            }
            let at = match group {
                None => 0,
                Some(g) => {
                    key.clear();
                    crate::codec::encode_value(&mut key, &row[g]);
                    // Looked up first and entered only when new: an entry a
                    // row cloned the key every row, 81 -> 100 ms over a
                    // million.
                    match index.get(&key) {
                        Some(n) => n[0] as usize,
                        None => {
                            index
                                .entry(key.clone())
                                .or_default()
                                .push(keys.len() as DocId);
                            keys.push(row[g].clone());
                            folds.push(start.clone());
                            keys.len() - 1
                        }
                    }
                }
            };
            for (i, fold) in folds[at].iter_mut().enumerate() {
                match slots[i] {
                    Some(s) => fold.add(&row[s])?,
                    None => fold.add(&Value::Bool(true))?,
                }
            }
        }
        let n = keys.len();
        plan(|| {
            format!(
                "aggregate: {} rows into {n} {}",
                ids.len(),
                if n == 1 { "group" } else { "groups" }
            )
        });

        let mut columns = Vec::with_capacity(sel.aggregate.len());
        for a in &sel.aggregate {
            columns.push(a.label());
        }
        let width = columns.len();
        let mut values: Vec<Value> = Vec::with_capacity(n * width);
        for (k, group_folds) in keys.iter().zip(folds) {
            for (a, f) in sel.aggregate.iter().zip(group_folds) {
                values.push(match a {
                    Agg::Key(_) => k.clone(),
                    _ => f.value(),
                });
            }
        }
        // Groups come out by their key unless `order` says otherwise, naming
        // the list's columns; the key breaks what the order leaves tied.
        let mut order: Vec<OrderKey> = Vec::with_capacity(sel.order.len() + 1);
        let mut picked: Vec<usize> = Vec::with_capacity(sel.order.len());
        for s in &sel.order {
            let name = &s.field;
            let Some(at) = columns.iter().position(|c| c == name) else {
                return Err(Error::Query(format!(
                    "`order {name}`: not a column of this select"
                )));
            };
            if let Some(coll) = s.collate {
                // Only the key and `min`/`max` carry a field's values;
                // `count`, `sum` and `avg` are numbers whatever they read.
                let text = match &sel.aggregate[at] {
                    Agg::Key(f) | Agg::Min(f) | Agg::Max(f) => {
                        c.schema.field(f).is_some_and(|f| collatable(&f.ty))
                    }
                    _ => false,
                };
                if !text {
                    return Err(Error::Query(format!(
                        "`collate {}` orders text; `{name}` is not",
                        coll.name()
                    )));
                }
            }
            // The group key, `min` and `max` carry a field's values, and
            // order in its collation when the query names none.
            let field = match &sel.aggregate[at] {
                Agg::Key(f) | Agg::Min(f) | Agg::Max(f) => {
                    c.schema.field(f).and_then(|f| f.collate)
                }
                _ => None,
            };
            picked.push(at);
            order.push((None, s.asc, s.collate.or(field)));
        }
        order.push((None, true, None));
        let w = order.len();
        let mut flat: Vec<Value> = Vec::with_capacity(n * w);
        for g in 0..n {
            for &at in &picked {
                flat.push(values[g * width + at].clone());
            }
            flat.push(keys[g].clone());
        }
        let k = sel.limit.map_or(n, |l| l.saturating_add(sel.offset)).min(n);
        let mut rows = Vec::with_capacity(k.saturating_sub(sel.offset));
        for g in order_rows(&flat, w, n, &order, k)
            .into_iter()
            .skip(sel.offset)
        {
            rows.push(Row {
                id: 0,
                values: values[g * width..g * width + width].to_vec(),
                score: None,
            });
        }
        Ok(ResultSet {
            columns,
            rows,
            nested: None,
        })
    }

    fn update(
        &mut self,
        collection: &str,
        set: &[(String, Expr)],
        filter: &Option<Expr>,
        params: &[Value],
    ) -> Result<Response> {
        let ids = self.matching_ids(collection, filter, params)?;
        let schema = self.collection(collection)?.schema.clone();
        let cid = self.collection(collection)?.id;
        let hooks: Vec<_> = self.registry.hooks().to_vec();
        let mut n = 0;
        // What the filter compared without, and what the new values would:
        // worked out in a pass of their own, since a write that stopped
        // half way could not be run again.
        if collate::PARTIAL {
            let mut missing = collate::take_missing();
            if schema.fields.iter().any(|f| f.collate.is_some()) {
                let c = self.collection(collection)?;
                for &id in &ids {
                    if let Some(doc) = c.store.read(&schema, id)? {
                        let doc = self.updated(&schema, doc, set, params)?;
                        missing |= collation_missing(&schema, &doc) | collate::take_missing();
                    }
                }
            }
            collate::refuse(missing)?;
        }

        for id in ids {
            let c = self.collections.get_mut(collection).unwrap();
            let Some(doc) = c.store.read(&schema, id)? else {
                continue;
            };
            let mut doc = self.updated(&schema, doc, set, params)?;
            let c = self.collections.get_mut(collection).unwrap();
            for h in &hooks {
                h.before_write(&schema, WriteOp::Update, &mut doc)?;
            }
            let old = c.store.read(&schema, id)?;
            if let Some(old) = &old {
                c.unindex_doc(old, Some(&doc));
            }
            let payload = Store::encode_doc(&schema, &doc);
            let frame = c.store.append(OP_PUT, id, &payload);
            c.index_doc(&doc, old.as_ref());
            self.wal(REC_DATA, cid, &frame)?;
            self.note(cid, id);
            for h in &hooks {
                h.after_write(collection, WriteOp::Update, &doc)?;
            }
            n += 1;
        }
        Ok(Response::Affected(n))
    }

    /// `doc` with `set` applied, every expression over the document as it
    /// was.
    fn updated(
        &self,
        schema: &Schema,
        mut doc: Document,
        set: &[(String, Expr)],
        params: &[Value],
    ) -> Result<Document> {
        let snapshot = doc.clone();
        let ctx = EvalCtx {
            params,
            registry: &self.registry,
        };
        for (k, e) in set {
            let f = schema
                .field(k)
                .ok_or_else(|| Error::NotFound(format!("field `{k}`")))?;
            let v = eval(e, &mut DocRow(&snapshot, schema), &ctx)?;
            doc.set(k, v.coerce(&f.ty)?);
        }
        Ok(doc)
    }

    fn delete(
        &mut self,
        collection: &str,
        filter: &Option<Expr>,
        params: &[Value],
    ) -> Result<Response> {
        let ids = self.matching_ids(collection, filter, params)?;
        if collate::PARTIAL {
            collate::refuse(collate::take_missing())?;
        }
        let schema = self.collection(collection)?.schema.clone();
        let cid = self.collection(collection)?.id;
        let hooks: Vec<_> = self.registry.hooks().to_vec();
        let mut n = 0;
        for id in ids {
            let c = self.collections.get_mut(collection).unwrap();
            if let Some(doc) = c.store.read(&schema, id)? {
                for h in &hooks {
                    h.after_write(collection, WriteOp::Delete, &doc)?;
                }
                c.unindex_doc(&doc, None);
            }
            let frame = c.store.append(OP_DEL, id, &[]);
            self.wal(REC_DATA, cid, &frame)?;
            self.note(cid, id);
            n += 1;
        }
        Ok(Response::Affected(n))
    }

    /// Drops the dead records of `which` (every collection when `None`) and
    /// rewrites the file. `graphs`: whether a graph holding tombstones is
    /// rebuilt here -- a compact beside the database has rebuilt them
    /// already, without the lock.
    fn compact(&mut self, which: Option<&str>, graphs: bool) -> Result<Response> {
        let targets: Vec<String> = match which {
            Some(n) => {
                self.collection(n)?;
                vec![n.to_string()]
            }
            None => self.order.clone(),
        };
        let reclaimed: usize = targets
            .iter()
            .map(|n| self.collections[n].store.dead_bytes())
            .sum();
        // Over a mapped file the records never come into memory: the live
        // ones are written into the new file, and the stores are pointed at
        // it. The documents are the same ones, so no index is rebuilt.
        #[cfg(not(target_arch = "wasm32"))]
        let mapped = self.mapped;
        #[cfg(target_arch = "wasm32")]
        let mapped = false;
        if !mapped {
            for name in &targets {
                let c = self.collections.get_mut(name).unwrap();
                c.store.compact()?;
            }
        }
        // A compact drops dead records and moves the live ones; the
        // documents are the same, and every index is keyed by id, so none
        // needs rebuilding for that -- a graph without tombstones took 50 s
        // at 100 000 x 768 to rebuild for nothing. A graph with them is
        // rebuilt: nothing else ever takes one out, every rewrite of a
        // document with a vector leaves one, and they crowd the beam `near`
        // walks (a `limit 10` answered 4 rows once enough had gathered).
        if graphs {
            for name in &targets {
                let c = self.collections.get_mut(name).unwrap();
                // By position, not by collecting the names: the list of
                // strings was 440 bytes of the browser module.
                for pos in 0..c.schema.fields.len() {
                    let IndexKind::Vector(spec) = c.schema.fields[pos].index else {
                        continue;
                    };
                    let field = &c.schema.fields[pos].name;
                    if c.vectors.get(field).is_some_and(|ix| ix.dead() > 0) {
                        build_graph(c, pos, spec)?;
                    }
                }
            }
        }
        // After compaction the persisted image is rewritten from scratch.
        let compacting: &[String] = if mapped { &targets } else { &[] };
        let mut placed = Vec::new();
        let mut len = 0;
        let r = {
            let mut sink = self.sink.lock().unwrap_or_else(|e| e.into_inner());
            sink.rewrite_with(&mut |out| {
                self.image_into(out, compacting, &mut placed)?;
                len = out.at();
                Ok(())
            })
        };
        self.storage(r)?;
        self.rewrote(len);
        #[cfg(not(target_arch = "wasm32"))]
        self.repoint(Some(&placed), compacting)?;
        Ok(Response::Ok(format!(
            "compaction done, {reclaimed} bytes reclaimed"
        )))
    }
}

/// The documents [`Database::apply`] has yet to put into the graph: one
/// collection's, none of them twice.
#[derive(Default)]
struct VectorBatch {
    cid: u32,
    ids: std::collections::HashSet<DocId>,
    docs: Vec<Document>,
}

fn missing(cid: u32) -> Error {
    Error::Corrupt(format!("a write to collection {cid}, which is not here"))
}

/// Builds the graph of the vector field at `pos` from the documents' vectors.
/// Not inlined, as `build_index` is: `compact` rebuilds a graph holding
/// tombstones through it too, and a second inlined copy of `build_index`
/// was 3 KB of the browser module.
#[inline(never)]
fn build_graph(c: &mut Collection, pos: usize, spec: crate::schema::VectorIndexSpec) -> Result<()> {
    let field = &c.schema.fields[pos].name;
    let DataType::Vector(dim, prec) = c.schema.fields[pos].ty else {
        return Err(Error::Type(format!("field `{field}` is not vector<N>")));
    };
    let mut ix = VectorIndex::with_precision(dim, spec, prec);
    let ids: Vec<DocId> = c.store.ids();
    ix.reserve(ids.len());
    let mut items: Vec<(DocId, Vec<f32>)> = Vec::with_capacity(ids.len());
    for id in &ids {
        if let Some(Value::Vector(v)) = c.store.read_field(*id, pos)? {
            items.push((*id, v));
        }
    }
    ix.insert_batch(&items);
    c.vectors.insert(field.clone(), ix);
    Ok(())
}

/// Builds the index the schema declares on the field at `pos` and fills it
/// from the collection's documents: what `create index` does, and what a
/// replica does with the primary's. Inlined for the reason
/// [`Collection::reset_index_structures`] is.
#[inline(always)]
fn build_index(c: &mut Collection, pos: usize) -> Result<()> {
    // An index this build lacks is not built: a file's log replays the
    // `create index` that made it, and the file opens all the same. Asked
    // after `EVERY_INDEX`, or the bounds check of the lookup stays in the
    // module that has every index.
    if !EVERY_INDEX && missing_feature(&c.schema.fields[pos].index).is_some() {
        return Ok(());
    }
    let field = c.schema.fields[pos].name.clone();
    match c.schema.fields[pos].index.clone() {
        IndexKind::Vector(spec) => build_graph(c, pos, spec)?,
        IndexKind::Hash => {
            let mut ix = HashIndex::default();
            for id in c.store.ids() {
                if let Some(v) = c.store.read_field(id, pos)? {
                    ix.add(hash_key(&v), id);
                }
            }
            c.hashes.insert(field, ix);
        }
        IndexKind::Text(spec) => {
            let mut ix = TextIndex::new(spec);
            for id in c.store.ids() {
                if let Some(Value::Text(t)) = c.store.read_field(id, pos)? {
                    ix.insert(id, &t);
                }
            }
            ix.shrink_to_fit();
            c.texts.insert(field, ix);
        }
        #[cfg(not(feature = "sorted"))]
        IndexKind::Sorted => return Err(not_built("the index", "sorted")),
        #[cfg(feature = "sorted")]
        IndexKind::Sorted => {
            let ty = c.schema.fields[pos].ty.clone();
            let mut rows = Vec::with_capacity(c.store.len());
            for id in c.store.ids() {
                rows.push((id, c.store.read_field(id, pos)?));
            }
            let coll = c.schema.fields[pos].collate;
            let ix = SortedIndex::build(&ty, coll, &mut rows.into_iter());
            match c.sorted.iter_mut().find(|(n, _)| *n == field) {
                Some(slot) => slot.1 = ix,
                None => c.sorted.push((field, ix)),
            }
        }
        IndexKind::Inverted => {
            let mut ix = SparseIndex::new();
            for id in c.store.ids() {
                if let Some(Value::Sparse(_, e)) = c.store.read_field(id, pos)? {
                    ix.insert(id, &e);
                }
            }
            ix.shrink_to_fit();
            match c.sparse.iter_mut().find(|(n, _)| *n == field) {
                Some(slot) => slot.1 = ix,
                None => c.sparse.push((field, ix)),
            }
        }
        IndexKind::None => {}
    }
    Ok(())
}

/// One aggregate's running value over a group's rows. Nulls are skipped
/// by every one but the row count, as SQL's are.
#[derive(Clone)]
enum Fold {
    Count(i64),
    /// An int field's sum, and how many values went in.
    SumInt(i64, u64),
    SumFloat(f64, u64),
    Avg(f64, u64),
    /// `true` for `max`; the field's collation, which its text orders in.
    Extreme(Option<Value>, bool, Option<Collation>),
}

impl Fold {
    fn new(a: &Agg, ty: &DataType, coll: Option<Collation>) -> Result<Fold> {
        let numeric = |what: &str, f: &str| {
            Error::Type(format!(
                "`{what}({f})` needs an int or float field; `{f}` is {}",
                ty.name()
            ))
        };
        Ok(match a {
            Agg::Count | Agg::Key(_) => Fold::Count(0),
            Agg::Sum(f) => match ty {
                DataType::Int => Fold::SumInt(0, 0),
                DataType::Float => Fold::SumFloat(0.0, 0),
                _ => return Err(numeric("sum", f)),
            },
            Agg::Avg(f) => match ty {
                DataType::Int | DataType::Float => Fold::Avg(0.0, 0),
                _ => return Err(numeric("avg", f)),
            },
            Agg::Min(f) | Agg::Max(f) => match ty {
                DataType::Int
                | DataType::Float
                | DataType::Timestamp
                | DataType::Text
                | DataType::Bool => Fold::Extreme(None, matches!(a, Agg::Max(_)), coll),
                _ => {
                    return Err(Error::Type(format!(
                        "`{}` needs a field with an order; `{f}` is {}",
                        a.label(),
                        ty.name()
                    )))
                }
            },
        })
    }

    fn add(&mut self, v: &Value) -> Result<()> {
        if matches!(v, Value::Null) && !matches!(self, Fold::Count(_)) {
            return Ok(());
        }
        match self {
            Fold::Count(n) => *n += 1,
            Fold::SumInt(sum, n) => {
                let Value::Int(i) = v else { return Ok(()) };
                *sum = sum
                    .checked_add(*i)
                    .ok_or_else(|| Error::Query("the sum does not fit a 64-bit int".into()))?;
                *n += 1;
            }
            Fold::SumFloat(sum, n) | Fold::Avg(sum, n) => {
                let x = match v {
                    Value::Int(i) => *i as f64,
                    Value::Float(f) => *f,
                    _ => return Ok(()),
                };
                *sum += x;
                *n += 1;
            }
            Fold::Extreme(best, max, coll) => {
                let better = match best {
                    None => true,
                    Some(b) => {
                        let o = match coll {
                            Some(c) => c.compare_values(v, b),
                            None => v.cmp_value(b),
                        };
                        if *max {
                            o == Ordering::Greater
                        } else {
                            o == Ordering::Less
                        }
                    }
                };
                if better {
                    *best = Some(v.clone());
                }
            }
        }
        Ok(())
    }

    fn value(self) -> Value {
        match self {
            Fold::Count(n) => Value::Int(n),
            Fold::SumInt(_, 0) | Fold::SumFloat(_, 0) | Fold::Avg(_, 0) => Value::Null,
            Fold::SumInt(s, _) => Value::Int(s),
            Fold::SumFloat(s, _) => Value::Float(s),
            Fold::Avg(s, n) => Value::Float(s / n as f64),
            Fold::Extreme(v, ..) => v.unwrap_or(Value::Null),
        }
    }
}

thread_local! {
    /// The steps an `explain` is recording on this thread, `None` outside
    /// one. A thread-local rather than a parameter: the steps are taken deep
    /// in paths every query shares, and threading a recorder through them
    /// would touch every signature on the way for a statement most queries
    /// never are. Outside an `explain` a step costs one check of this cell.
    static PLAN: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

/// Records a step of the plan while an `explain` runs; `step` is called only
/// then.
///
/// Only the test is inlined into the caller. With the whole of it generic
/// over the closure, each of the two dozen steps carried its own copy of the
/// thread-local access and the push, and `explain` cost the browser module
/// 16 KB.
#[inline(always)]
fn plan(step: impl FnOnce() -> String) {
    if planning() {
        push_step(step());
    }
}

#[inline(never)]
fn planning() -> bool {
    PLAN.with(|p| p.borrow().is_some())
}

#[inline(never)]
fn push_step(line: String) {
    PLAN.with(|p| {
        if let Some(steps) = p.borrow_mut().as_mut() {
            steps.push(line);
        }
    });
}

/// How many rows a ranked clause has to produce: `limit + offset`, or the
/// ceiling when there is no limit. A request past the ceiling is refused
/// before anything is scanned -- erroring beats handing back a truncated
/// ranking.
fn ranked_rows(sel: &Select, clause: &str, ceiling: usize) -> Result<usize> {
    let bound = sel.limit.map(|l| l.saturating_add(sel.offset));
    if bound.unwrap_or(sel.offset) > ceiling {
        return Err(Error::Query(format!(
            "`{clause}` returns at most {ceiling} rows, {} were requested (limit + offset)",
            bound.unwrap_or(sel.offset)
        )));
    }
    Ok(bound.unwrap_or(ceiling).max(1))
}

/// A ranking as the rows carry it. One conversion for `match`, `near` and
/// `fuse`: a closure each was a copy each in the browser module.
fn with_scores(hits: Vec<(DocId, f32)>) -> Vec<(DocId, Option<f32>)> {
    hits.into_iter().map(|(id, s)| (id, Some(s))).collect()
}

/// The `k` of `ids` nearest `q` by the documents' own vectors, read out of
/// the store, that `keep` passes: nearest first, ties to the lower id so the
/// answer is stable. How `match ... rerank` orders the text index's
/// candidates, and `near` the candidates a quantized index found.
///
/// Each id comes with the nearest its vector can lie (`VectorIndex::floor`),
/// and one that cannot come nearer than the `k` held already is neither
/// tested nor read; how many were read is the second half of the answer.
fn order_exactly(
    store: &Store,
    pos: usize,
    metric: Metric,
    q: &[f32],
    ids: &mut dyn Iterator<Item = (DocId, f32)>,
    k: usize,
    keep: &mut dyn FnMut(DocId) -> Result<bool>,
) -> Result<(Vec<(DocId, f32)>, usize)> {
    let q = match metric {
        Metric::Cosine => normalized(q),
        _ => q.to_vec(),
    };
    // Distances held negated, so the engine's one sort puts the nearest
    // first, and cut back to the `k` nearest whenever there are twice as
    // many: `worst` is then what a candidate has to beat, and a scan of
    // every row holds 2k of them rather than all.
    let mut out: Vec<(DocId, f32)> = Vec::new();
    let mut worst = f32::INFINITY;
    let mut read = 0;
    let mut buf: Vec<f32> = Vec::with_capacity(q.len());
    for (id, floor) in ids {
        if floor > worst || !keep(id)? {
            continue;
        }
        read += 1;
        // A document without a vector cannot be ordered.
        if !store.read_vector_into(id, pos, &mut buf)? || buf.len() != q.len() {
            continue;
        }
        // The store holds vectors as they were written; the HNSW arena is
        // what normalises on insert, and it is not in play here. With the
        // query already unit length, cosine only needs the candidate's own
        // norm -- computing it beside the dot product costs one pass and
        // saves a `Vec` per candidate, which at a thousand candidates a
        // query is the difference between an allocation-free scan and a
        // thousand allocations.
        let d = match metric {
            Metric::Cosine => {
                let n = norm(&buf);
                if n == 0.0 {
                    1.0
                } else {
                    1.0 - dot(&q, &buf) / n
                }
            }
            _ => distance(metric, &q, &buf),
        };
        out.push((id, -d));
        if out.len() == k || out.len() == 2 * k {
            out.sort_by(best_first);
            out.truncate(k);
            worst = -out[k - 1].1;
        }
    }
    out.sort_by(best_first);
    out.truncate(k);
    for h in &mut out {
        h.1 = score_from_distance(metric, -h.1);
    }
    Ok((out, read))
}

/// What `near` searches: a vector index, and when the index holds codes
/// rather than vectors (`quant=`) the store the field's vectors are in. The
/// codes find the candidates, the beam's worth of them, and the documents'
/// own vectors put those in order -- so a search over codes answers in exact
/// distances, and an exact search reads every vector it ranks.
struct Space<'a> {
    ix: &'a VectorIndex,
    /// The store and the field's position, over codes.
    exact: Option<(&'a Store, usize)>,
}

impl<'a> Space<'a> {
    fn new(c: &'a Collection, ix: &'a VectorIndex, field: &str) -> Space<'a> {
        let exact = match ix.quantized() {
            true => c.schema.field_pos(field).map(|p| (&c.store, p)),
            false => None,
        };
        Space { ix, exact }
    }

    /// The `k` nearest the walk finds that `keep` passes, over a beam `ef`
    /// wide: the beam's candidates tested in order (`order`). One call to
    /// the index either way: a second one in the other branch doubled its
    /// inlined body in the browser module.
    fn search(
        &self,
        q: &[f32],
        k: usize,
        ef: Option<usize>,
        keep: &mut dyn FnMut(DocId) -> Result<bool>,
    ) -> Result<Vec<(DocId, f32)>> {
        let n = ef.unwrap_or(self.ix.spec.ef_search).max(k);
        let found = self.ix.search(q, n, ef, |_| true);
        self.order(q, found, k, keep)
    }

    /// [`VectorIndex::search_ids`]: every row of `ids` measured, and over
    /// codes the beam's worth of the nearest put in order as a walk's are.
    /// Read whole, a filtered set under the budget of `ef x m0` was up to
    /// 12 800 vectors a query at the beam bit codes take, 39 MB of them at
    /// 768 dimensions; a set of 3 000 rows read 12 over int8 codes and 100
    /// over bit codes at a beam of 100, and answered the same ten.
    fn search_ids(
        &self,
        q: &[f32],
        k: usize,
        ef: Option<usize>,
        ids: &[DocId],
    ) -> Result<Vec<(DocId, f32)>> {
        let n = match self.exact {
            None => k,
            Some(_) => ef.unwrap_or(self.ix.spec.ef_search).max(k),
        };
        let found = self.ix.search_ids(q, n, ids);
        self.order(q, found, k, &mut |_| Ok(true))
    }

    /// The first `k` of `found`, the index's candidates nearest first, that
    /// `keep` passes. Over codes that order is an estimate: the candidates
    /// are put in the order of the documents' own vectors, read nearest
    /// estimate first and only while one can still make the `k` -- an int8
    /// code knows how far off it can be (`VectorIndex::floor`), a bit code
    /// does not and every candidate is read. Reading every one was a third
    /// of an int8 query over a million vectors; at 100 000 x 768 a page of
    /// ten reads 16.8 of a beam of 100, and of 400, with the same answer.
    fn order(
        &self,
        q: &[f32],
        found: Vec<(DocId, f32)>,
        k: usize,
        keep: &mut dyn FnMut(DocId) -> Result<bool>,
    ) -> Result<Vec<(DocId, f32)>> {
        let ix = self.ix;
        let Some((store, pos)) = self.exact else {
            let mut hits = Vec::with_capacity(k);
            for (id, score) in found {
                if hits.len() == k {
                    break;
                }
                if keep(id)? {
                    hits.push((id, score));
                }
            }
            return Ok(hits);
        };
        let n = found.len();
        // The query as the codes measured it: unit length under cosine.
        let reach = match ix.spec.metric {
            Metric::Cosine => 1.0,
            _ => norm(q),
        };
        let (hits, read) = order_exactly(
            store,
            pos,
            ix.spec.metric,
            q,
            &mut found
                .into_iter()
                .map(|(id, score)| (id, ix.floor(id, score, reach))),
            k,
            keep,
        )?;
        plan(|| {
            format!("near: {read} of the {n} candidates of the codes read from the store, in exact order")
        });
        Ok(hits)
    }

    /// [`VectorIndex::search_exact`], exactly over codes as well: every
    /// vector read.
    fn search_exact(
        &self,
        q: &[f32],
        k: usize,
        accept: &dyn Fn(DocId) -> bool,
    ) -> Result<Vec<(DocId, f32)>> {
        match self.exact {
            None => Ok(self.ix.search_exact(q, k, accept)),
            Some((store, pos)) => {
                let mut ids = store
                    .ids()
                    .into_iter()
                    .filter(|id| accept(*id))
                    .map(|id| (id, f32::NEG_INFINITY));
                let metric = self.ix.spec.metric;
                Ok(order_exactly(store, pos, metric, q, &mut ids, k, &mut |_| Ok(true))?.0)
            }
        }
    }
}

/// The ANN's beam as `explain` states it: the `ef` in force, and the page
/// when that is wider, since the walk keeps at least as many candidates as
/// it has to return.
fn beam(ix: &VectorIndex, near: &Near, want: usize) -> String {
    let ef = near.ef.unwrap_or(ix.spec.ef_search);
    if want > ef {
        format!("ef {ef}, widened to the {want} rows asked for")
    } else {
        format!("ef {ef}")
    }
}

/// What an unfiltered ANN answer of `found` rows still needs.
enum Short {
    Whole,
    /// Walked again with this `ef`.
    Wider(usize),
    Exact,
}

/// Every rewrite or deletion of a document with a vector leaves a tombstone
/// in the graph until a compact, and the walk passes through them: a beam
/// of `ef` around many of them held fewer live nodes than the page, and a
/// `limit 10` answered 4 rows. No more than every tombstone can be in the
/// beam, so one wider by their number holds `ef` live nodes -- unless
/// walking that wide costs more than reading every vector: a step measures
/// about `2m` neighbours, so past `live / 2m` steps the exact search is
/// cheaper, and it is also the answer when the wider beam comes up short.
fn past_tombstones(
    ix: &VectorIndex,
    near: &Near,
    want: usize,
    found: usize,
    widened: bool,
) -> Short {
    let live = ix.len();
    if found >= want.min(live) {
        return Short::Whole;
    }
    let dead = ix.dead();
    let ef = near.ef.unwrap_or(ix.spec.ef_search).max(want) + dead;
    if !widened && dead > 0 && ef.saturating_mul(2 * ix.spec.m) < live {
        plan(|| {
            format!(
                "near: the ANN came up short past {dead} tombstones, the beam widened to ef {ef}"
            )
        });
        return Short::Wider(ef);
    }
    plan(|| {
        format!("near: the ANN came up short past {dead} tombstones, every vector searched exactly")
    });
    Short::Exact
}

/// Whether the stored row `id` passes the filter.
fn row_matches(c: &Collection, f: &Expr, id: DocId, ctx: &EvalCtx) -> Result<bool> {
    let mut row = StoreRow {
        store: &c.store,
        schema: &c.schema,
        id,
        memo: Vec::new(),
    };
    Ok(truthy(&eval(f, &mut row, ctx)?))
}

/// A filter evaluated over `rows` in blocks taken a stride apart, so that
/// the first matches it finds come from across the whole collection rather
/// than its oldest rows: a filter for the newest rows, which sit at the end
/// of the id order, is known to be large as soon as one for rows everywhere
/// -- `recent >= 20` over 200 000 rows, the newest 23%, took 1.67 ms against
/// 1.12 for the same share spread out, and 16.7 before the probe. It can
/// stop at a number of matches and later carry on from where it stopped.
struct FilterProbe {
    rows: Vec<DocId>,
    matched: Vec<DocId>,
    tested: usize,
    /// The next row to test, and the pass it belongs to: pass `p` reads
    /// blocks `p`, `p + PROBE_STRIDE`, `p + 2 * PROBE_STRIDE`, ...
    next: usize,
    pass: usize,
}

/// Rows read in sequence. Blocks rather than single rows a stride apart:
/// reading every 64th row took a scan of the whole set from 17.6 ms to 32.1,
/// because a row read alone is a row the prefetcher did not see coming.
const PROBE_BLOCK: usize = 256;
/// Blocks between two read in the same pass.
const PROBE_STRIDE: usize = 16;

impl FilterProbe {
    fn new(rows: Vec<DocId>) -> FilterProbe {
        FilterProbe {
            rows,
            matched: Vec::new(),
            tested: 0,
            next: 0,
            pass: 0,
        }
    }

    /// Rows that are the answer already, with nothing left to test.
    fn done(rows: Vec<DocId>) -> FilterProbe {
        FilterProbe {
            rows: Vec::new(),
            matched: rows,
            tested: 0,
            next: 0,
            pass: 0,
        }
    }

    /// Tests rows until `cap` have matched or none is left; true when none
    /// is left, which makes `matched` the whole set.
    fn run(&mut self, cap: usize, matches: impl Fn(DocId) -> Result<bool>) -> Result<bool> {
        while self.pass < PROBE_STRIDE {
            while self.next < self.rows.len() {
                if self.matched.len() >= cap {
                    return Ok(false);
                }
                let id = self.rows[self.next];
                self.next += 1;
                if self.next.is_multiple_of(PROBE_BLOCK) {
                    self.next += (PROBE_STRIDE - 1) * PROBE_BLOCK;
                }
                self.tested += 1;
                if matches(id)? {
                    self.matched.push(id);
                }
            }
            self.pass += 1;
            self.next = self.pass * PROBE_BLOCK;
        }
        Ok(true)
    }

    fn into_sorted(self) -> Vec<DocId> {
        let mut ids = self.matched;
        ids.sort_unstable();
        ids
    }
}

/// The range a filter's comparisons put on an ordered field, and whether
/// every one of them could be said as a key. A comparison the key space
/// cannot express exactly -- `price < 12.5` on an `int` field -- is left out
/// of the range, which then only narrows, and the filter is evaluated.
fn sorted_range(
    fd: &crate::schema::Field,
    field: &str,
    f: &Expr,
    params: &[Value],
) -> Option<(SortRange, bool)> {
    let ty = &fd.ty;
    let mut ranges = Vec::new();
    f.conjunct_ranges(params, &mut ranges);
    let mut range = SortRange::all(fd.collate);
    let (mut any, mut exact) = (false, true);
    for (name, op, v) in ranges {
        if name != field {
            continue;
        }
        match SortedIndex::bound(ty, v) {
            Some(key) => {
                range.narrow(op, key);
                any = true;
            }
            None => exact = false,
        }
    }
    any.then_some((range, exact))
}

/// Sort keys: a field position (`None` for `id`), whether it ascends, and
/// the collation its text is compared in.
type OrderKey = (Option<usize>, bool, Option<Collation>);

/// The key `s` names in `schema`. `collate` is refused on anything but text:
/// on a number it would claim an order it does not change. `owner` goes in
/// front of the field's name in an error -- `reviews.` for a `lookup`'s.
fn order_key(schema: &Schema, s: &Sort, owner: &str) -> Result<OrderKey> {
    let f = &s.field;
    let pos = if f == "id" {
        None
    } else {
        Some(
            schema
                .field_pos(f)
                .ok_or_else(|| Error::NotFound(format!("field `{owner}{f}`")))?,
        )
    };
    if let Some(c) = s.collate {
        let ty = pos.map_or(&DataType::Int, |p| &schema.fields[p].ty);
        if !collatable(ty) {
            return Err(Error::Query(format!(
                "`collate {}` orders text; `{owner}{f}` is {}",
                c.name(),
                ty.name()
            )));
        }
    }
    // A field in a collation orders in it unless the query names one.
    let field = pos.and_then(|p| schema.fields[p].collate);
    Ok((pos, s.asc, s.collate.or(field)))
}

/// The order `order` asks for between two rows' keys, before any tie-break.
fn rank(keys: &[OrderKey], a: &[Value], b: &[Value]) -> Ordering {
    for (i, (_, asc, collate)) in keys.iter().enumerate() {
        let o = match collate {
            Some(c) => c.compare_values(&a[i], &b[i]),
            None => a[i].cmp_value(&b[i]),
        };
        if o != Ordering::Equal {
            return if *asc { o } else { o.reverse() };
        }
    }
    Ordering::Equal
}

/// `ids` in the order `keys` ask for, the first `k` of them.
///
/// Every key is read whatever `k` is -- the first twenty cannot be known
/// without looking at all of them -- so the cost follows the number of
/// matches, not the limit; only an ordered index would change that. What `k`
/// buys is the sort: the rows past it are partitioned away in linear time.
/// Over a million rows `order created desc limit 20` went 49.4 -> 28.4 ms,
/// and most of that was not the sort but the one small `Vec` each row used to
/// hold its keys, now a single flat buffer.
///
/// A streaming heap of `k` rows would have kept the buffer out of memory,
/// and was measured: 77 ms on the same query. `created` grows with the id,
/// so under `desc` every row beats the ones kept and the heap sifts on every
/// row -- and "the latest twenty" is the ordering asked for most.
fn order_ids(store: &Store, ids: &[DocId], keys: &[OrderKey], k: usize) -> Result<Vec<DocId>> {
    if k == 0 {
        return Ok(Vec::new());
    }
    let n = keys.len();
    let mut flat: Vec<Value> = Vec::with_capacity(ids.len() * n);
    // The read stays inline: behind a closure returning `Result<Value>` the
    // same query measured 39 ms.
    for &id in ids {
        for (pos, ..) in keys {
            flat.push(match pos {
                None => Value::Int(id as i64),
                Some(p) => store.read_field(id, *p)?.unwrap_or(Value::Null),
            });
        }
    }
    let idx = order_rows(&flat, n, ids.len(), keys, k);
    Ok(idx.into_iter().map(|i| ids[i]).collect())
}

/// The first `k` of `count` rows, each `n` keys wide in `flat`, in the
/// order `keys` ask for: `order` over documents, and over the groups of an
/// aggregate -- one function, so the browser module holds one sort for both.
fn order_rows(flat: &[Value], n: usize, count: usize, keys: &[OrderKey], k: usize) -> Vec<usize> {
    let row = |i: usize| &flat[i * n..i * n + n];
    // Ties fall back to the position the row came in, which is what the
    // stable sort this replaced gave them: a selection is not stable, and a
    // page must not change with the plan.
    let cmp = |a: &usize, b: &usize| rank(keys, row(*a), row(*b)).then(a.cmp(b));
    let mut idx: Vec<usize> = (0..count).collect();
    if k == 0 {
        return Vec::new();
    }
    if k < idx.len() {
        idx.select_nth_unstable_by(k - 1, cmp);
        idx.truncate(k);
    }
    idx.sort_unstable_by(cmp);
    idx
}

/// The query vector of `near`. Three forms are accepted, none of them
/// silently mangled.
///
/// The text form (`near embed '[1,2,3]'`) is pgvector's notation and the
/// natural one to type by hand from psql. On the parameter path the same
/// value was already parsed into a list; on the literal path it stayed
/// `text` and raised a type error.
fn near_vector(v: Value) -> Result<Vec<f32>> {
    let items = match v {
        Value::Vector(v) => return Ok(v),
        Value::List(items) => items,
        Value::Text(s) => match crate::json::parse(s.trim()) {
            // The JSON parser already reduces a numeric array to a vector;
            // a mixed array comes back as a list and is checked below.
            Ok(Value::Vector(v)) => return Ok(v),
            Ok(Value::List(items)) => items,
            _ => {
                return Err(Error::Type(format!(
                    "near expects a vector, unparseable text: {s}"
                )))
            }
        },
        other => {
            return Err(Error::Type(format!(
                "near expects a vector, found {}",
                other.type_name()
            )))
        }
    };
    // A non-numeric component used to silently become 0.0: a result that
    // corrupts the distance without erroring, the worst kind of wrong answer.
    items
        .iter()
        .map(|i| {
            i.as_f64().map(|f| f as f32).ok_or_else(|| {
                Error::Type(format!(
                    "non-numeric value in the near vector: {}",
                    i.type_name()
                ))
            })
        })
        .collect()
}
