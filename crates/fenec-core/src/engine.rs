//! Database engine: where the catalog, execution and persistence meet.

use crate::changes::{ChangeLog, Since, SCHEMA_MARK};
use crate::codec::{get_uvarint, put_uvarint};
use crate::error::{Error, Result};
use crate::plugin::{Plugin, Registry, WriteOp};
use crate::query::*;
use crate::schema::{IndexKind, Metric, Schema};
use crate::sorted::{Range as SortRange, SortedIndex};
use crate::store::{Store, OP_DEL, OP_PUT};
use crate::text::TextIndex;
use crate::value::{DataType, DocId, Document, Value, VecPrec};
use crate::vector::{distance, dot, norm, normalized, score_from_distance, VectorIndex};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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
/// Measured on BEIR: on FiQA (57 638 documents) 1 000 candidates reproduce a
/// full dense scan exactly and 250 reach 97% of it; on SciFact 50 already
/// beat the full scan. 200 sits where the curve has flattened on both, and
/// costs 200 vector reads -- under 0.4% of that corpus.
pub const DEFAULT_RERANK_CANDIDATES: usize = 200;

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

/// The persistence layer. The engine only says "append these bytes"; where
/// they are written (file, IndexedDB, OPFS, S3) is this layer's problem.
pub trait Sink: Send {
    fn append(&mut self, bytes: &[u8]) -> Result<()>;
    fn rewrite(&mut self, bytes: &[u8]) -> Result<()>;
    fn sync(&mut self) -> Result<()> {
        Ok(())
    }
}

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

pub struct Collection {
    pub id: u32,
    pub schema: Schema,
    pub store: Store,
    /// field name -> HNSW index
    pub vectors: HashMap<String, VectorIndex>,
    /// field name -> (encoded value -> document ids)
    pub hashes: HashMap<String, HashMap<Vec<u8>, Vec<DocId>>>,
    /// field name -> inverted index
    pub texts: HashMap<String, TextIndex>,
    /// field name -> ordered index, in schema order. A `Vec` rather than a
    /// map: a collection has a handful of ordered fields, the map's code was
    /// 2.6 KB of the browser module, and a fixed order keeps the choice
    /// between two ranges the same from one run to the next.
    pub sorted: Vec<(String, SortedIndex)>,
}

impl Collection {
    fn new(id: u32, schema: Schema) -> Collection {
        let mut vectors = HashMap::new();
        let mut hashes = HashMap::new();
        let mut texts = HashMap::new();
        let mut sorted = Vec::new();
        for f in &schema.fields {
            match (&f.index, &f.ty) {
                (IndexKind::Vector(spec), DataType::Vector(dim, prec)) => {
                    vectors.insert(
                        f.name.clone(),
                        VectorIndex::with_precision(*dim, *spec, *prec),
                    );
                }
                (IndexKind::Hash, _) => {
                    hashes.insert(f.name.clone(), HashMap::new());
                }
                (IndexKind::Text(spec), DataType::Text) => {
                    texts.insert(f.name.clone(), TextIndex::new(*spec));
                }
                (IndexKind::Sorted, ty) if SortedIndex::supports(ty) => {
                    sorted.push((f.name.clone(), SortedIndex::new(ty)));
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
        }
    }

    /// Rebuilds the index structures from the schema's index definitions
    /// (contents empty; filling them is `rebuild_indexes_with`'s job).
    fn reset_index_structures(&mut self) {
        self.vectors.clear();
        self.hashes.clear();
        self.texts.clear();
        self.sorted.clear();
        for f in &self.schema.fields {
            match (&f.index, &f.ty) {
                (IndexKind::Vector(spec), DataType::Vector(dim, prec)) => {
                    self.vectors.insert(
                        f.name.clone(),
                        VectorIndex::with_precision(*dim, *spec, *prec),
                    );
                }
                (IndexKind::Hash, _) => {
                    self.hashes.insert(f.name.clone(), HashMap::new());
                }
                (IndexKind::Text(spec), DataType::Text) => {
                    self.texts.insert(f.name.clone(), TextIndex::new(*spec));
                }
                (IndexKind::Sorted, ty) if SortedIndex::supports(ty) => {
                    self.sorted.push((f.name.clone(), SortedIndex::new(ty)));
                }
                _ => {}
            }
        }
    }

    pub fn sorted_index(&self, field: &str) -> Option<&SortedIndex> {
        self.sorted
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, ix)| ix)
    }

    fn index_doc(&mut self, doc: &Document) {
        for (name, ix) in self.vectors.iter_mut() {
            if let Some(Value::Vector(v)) = doc.get(name) {
                ix.insert(doc.id, v);
            }
        }
        self.index_scalar(doc);
    }

    /// Hash and full-text indexes. Vectors are left to the batch path.
    fn index_scalar(&mut self, doc: &Document) {
        for (name, map) in self.hashes.iter_mut() {
            if let Some(v) = doc.get(name) {
                let key = hash_key(v);
                map.entry(key).or_default().push(doc.id);
            }
        }
        for (name, ix) in self.texts.iter_mut() {
            if let Some(Value::Text(t)) = doc.get(name) {
                ix.insert(doc.id, t);
            }
        }
        for (name, ix) in self.sorted.iter_mut() {
            ix.insert(doc.id, doc.get(name));
        }
    }

    /// Bulk-inserts every vector in a batch, field by field.
    ///
    /// Calling `insert` one at a time missed the chance to parallelise the
    /// read-only part of the HNSW build; the batch path takes it.
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

    fn unindex_doc(&mut self, doc: &Document) {
        for (name, ix) in self.vectors.iter_mut() {
            if doc.get(name).is_some() {
                ix.remove(doc.id);
            }
        }
        for (name, map) in self.hashes.iter_mut() {
            if let Some(v) = doc.get(name) {
                if let Some(bucket) = map.get_mut(&hash_key(v)) {
                    bucket.retain(|d| *d != doc.id);
                }
            }
        }
        // Every caller reads the *stored* document before unindexing, so the
        // terms here are the ones that went in.
        for (name, ix) in self.texts.iter_mut() {
            if let Some(Value::Text(t)) = doc.get(name) {
                ix.remove(doc.id, t);
            }
        }
        for (name, ix) in self.sorted.iter_mut() {
            ix.remove(doc.id, doc.get(name));
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

fn hash_key(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    crate::codec::encode_value(&mut out, v);
    out
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
    Hash(&'a HashMap<Vec<u8>, Vec<DocId>>, &'a DataType),
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
}

/// Access that uses the document as its source (on the put/update path).
struct DocRow<'a>(&'a Document);
impl<'a> RowAccess for DocRow<'a> {
    fn id(&self) -> DocId {
        self.0.id
    }
    fn field(&mut self, name: &str) -> Result<Value> {
        Ok(self.0.get(name).cloned().unwrap_or(Value::Null))
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
}

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
        }
    }

    pub fn with_sink(sink: Box<dyn Sink>) -> Database {
        let mut db = Database::new();
        db.sink = Mutex::new(sink);
        db
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

    /// Sets the party to wake after writes.
    pub fn set_watcher(&mut self, w: Arc<dyn Watcher>) {
        self.watcher = Some(w);
    }

    /// Marks a write on the feed.
    fn note(&mut self, cid: u32, id: DocId) {
        self.changes.record(cid, id);
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
    /// vector arenas and graph links. Read from counters, so the cost is
    /// proportional to the number of collections.
    ///
    /// **This is not RSS.** Left out: the allocator's leftovers, session
    /// buffers, upper-level neighbour allocations (~3% of l0) and temporary
    /// peaks -- opening is ~2x the file, `checkpoint`/`compact` ~3x. It is a
    /// scale, not a ceiling; use it with headroom on top.
    pub fn memory_bytes(&self) -> usize {
        self.collections
            .values()
            .map(|c| {
                c.store.total_bytes()
                    + c.store.index_bytes()
                    + c.vectors
                        .values()
                        .map(|ix| ix.arena_bytes() + ix.graph_bytes())
                        .sum::<usize>()
                    + c.texts.values().map(|ix| ix.memory_bytes()).sum::<usize>()
                    + c.sorted
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
        let mut out = Vec::from(&MAGIC[..]);
        // Counter header: a placeholder now, the values after the body.
        let head_at = out.len();
        out.push(REC_SEQ);
        out.extend_from_slice(&self.changes.seq().to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        let body_at = out.len();

        for name in &self.order {
            let c = &self.collections[name];
            let sc = c.schema.encode();
            out.push(REC_CREATE);
            put_uvarint(&mut out, c.id as u64);
            put_uvarint(&mut out, sc.len() as u64);
            out.extend_from_slice(&sc);

            // The counter comes right after the schema: the collection has to
            // exist, and its data can only carry the counter forward.
            let mut counter = Vec::with_capacity(9);
            put_uvarint(&mut counter, c.store.next_id());
            out.push(REC_NEXTID);
            put_uvarint(&mut out, c.id as u64);
            put_uvarint(&mut out, counter.len() as u64);
            out.extend_from_slice(&counter);

            let image = c.store.image();
            if !image.is_empty() {
                out.push(REC_DATA);
                put_uvarint(&mut out, c.id as u64);
                put_uvarint(&mut out, image.len() as u64);
                out.extend_from_slice(&image);
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
                out.push(REC_GRAPH);
                put_uvarint(&mut out, c.id as u64);
                put_uvarint(&mut out, payload.len() as u64);
                out.extend_from_slice(&payload);
            }
        }
        let body_len = (out.len() - body_at) as u64;
        out[head_at + 9..head_at + REC_SEQ_LEN].copy_from_slice(&body_len.to_le_bytes());
        out
    }

    /// Builds the database from a byte image.
    pub fn load(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != &MAGIC[..] {
            return Err(Error::Corrupt("invalid fenecdb signature".into()));
        }
        let mut pos = MAGIC.len();
        let mut by_id: HashMap<u32, String> = HashMap::new();
        let mut graphs: HashMap<(String, String), Vec<u8>> = HashMap::new();
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
        while pos < bytes.len() {
            let tail = pos >= body_end;
            let rec = bytes[pos];
            pos += 1;
            match rec {
                REC_CREATE => {
                    let cid = get_uvarint(bytes, &mut pos)? as u32;
                    let len = get_uvarint(bytes, &mut pos)? as usize;
                    if pos + len > bytes.len() {
                        break;
                    }
                    let mut sp = 0usize;
                    let schema = Schema::decode(&bytes[pos..pos + len], &mut sp)?;
                    pos += len;
                    seq_seen += tail as u64;
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
                    if pos + len > bytes.len() {
                        break; // half-written tail
                    }
                    pos += len;
                    seq_seen += tail as u64;
                    if let Some(name) = by_id.remove(&cid) {
                        self.collections.remove(&name);
                        self.order.retain(|n| n != &name);
                    }
                }
                REC_DATA => {
                    let cid = get_uvarint(bytes, &mut pos)? as u32;
                    let len = get_uvarint(bytes, &mut pos)? as usize;
                    if pos + len > bytes.len() {
                        break; // half-written tail
                    }
                    let name = by_id
                        .get(&cid)
                        .cloned()
                        .ok_or_else(|| Error::Corrupt(format!("unknown collection {cid}")))?;
                    let chunk = &bytes[pos..pos + len];
                    pos += len;
                    let c = self.collections.get_mut(&name).unwrap();
                    let frames = c.store.replay(chunk)? as u64;
                    if tail {
                        seq_seen += frames;
                    }
                }
                REC_ALTER => {
                    let cid = get_uvarint(bytes, &mut pos)? as u32;
                    let len = get_uvarint(bytes, &mut pos)? as usize;
                    if pos + len > bytes.len() {
                        break;
                    }
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
                                c.schema = schema;
                                c.reset_index_structures();
                            }
                        }
                    }
                }
                REC_NEXTID => {
                    let cid = get_uvarint(bytes, &mut pos)? as u32;
                    let len = get_uvarint(bytes, &mut pos)? as usize;
                    if pos + len > bytes.len() {
                        break;
                    }
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
                    if pos + len > bytes.len() {
                        break;
                    }
                    let chunk = &bytes[pos..pos + len];
                    pos += len;
                    let mut cp = 0usize;
                    let field = crate::codec::decode_str(chunk, &mut cp)?;
                    if let Some(name) = by_id.get(&cid) {
                        graphs.insert((name.clone(), field), chunk[cp..].to_vec());
                    }
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
                    body_end = pos + u64::from_le_bytes(w) as usize;
                }
                other => return Err(Error::Corrupt(format!("unknown record kind {other}"))),
            }
        }
        // A database loaded from a file has no *history*, only its current
        // state: the ring is emptied and the horizon is set to the counter.
        // A cursor sitting exactly here (a quiet restart) gets an empty
        // answer; everything else is reseeded.
        self.changes.reset(seq_base + seq_seen);
        // Indexes are derived data: restored from the persisted graph when
        // there is one, rebuilt otherwise (or when validation fails).
        self.rebuild_indexes_with(&graphs)?;
        Ok(())
    }

    pub fn rebuild_indexes(&mut self) -> Result<()> {
        self.rebuild_indexes_with(&HashMap::new())
    }

    fn rebuild_indexes_with(&mut self, graphs: &HashMap<(String, String), Vec<u8>>) -> Result<()> {
        for name in self.order.clone() {
            let c = self.collections.get_mut(&name).unwrap();
            // `ids()` comes back ascending; no extra sorting needed.
            let ids: Vec<DocId> = c.store.ids();

            // 1) Try to restore from the persisted graph.
            let fields: Vec<String> = c.vectors.keys().cloned().collect();
            let mut restored: Vec<String> = Vec::new();
            for field in &fields {
                let Some(bytes) = graphs.get(&(name.clone(), field.clone())) else {
                    continue;
                };
                let (dim, pos) = {
                    let ix = &c.vectors[field];
                    (ix.dim, c.schema.field_pos(field))
                };
                let Some(pos) = pos else { continue };
                let store = &c.store;
                let candidate = VectorIndex::restore_graph(bytes, dim, |doc, out| {
                    store.read_vector_into(doc, pos, out).unwrap_or(false)
                });
                if let Some(ix) = candidate {
                    // If the live document count does not match, the graph is stale.
                    if ix.len() == ids.len() {
                        c.vectors.insert(field.clone(), ix);
                        restored.push(field.clone());
                    }
                }
            }

            // 2) Rebuild whatever could not be restored.
            for (field, ix) in c.vectors.iter_mut() {
                if restored.contains(field) {
                    continue;
                }
                *ix = VectorIndex::new(ix.dim, ix.spec);
                // The document count is known, so the arena is sized in one go.
                ix.reserve(ids.len());
            }
            for (_, m) in c.hashes.iter_mut() {
                m.clear();
            }
            for t in c.texts.values_mut() {
                t.clear();
            }
            if restored.len() == fields.len()
                && c.hashes.is_empty()
                && c.texts.is_empty()
                && c.sorted.is_empty()
            {
                continue; // everything restored, no need to read the documents
            }
            // The rebuild goes through the batch path as well.
            let mut docs: Vec<Document> = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(doc) = c.store.read(&c.schema, id)? {
                    for (field, map) in c.hashes.iter_mut() {
                        if let Some(v) = doc.get(field) {
                            map.entry(hash_key(v)).or_default().push(doc.id);
                        }
                    }
                    for (field, ix) in c.texts.iter_mut() {
                        if let Some(Value::Text(t)) = doc.get(field) {
                            ix.insert(doc.id, t);
                        }
                    }
                    docs.push(doc);
                }
            }
            for ix in c.texts.values_mut() {
                ix.shrink_to_fit();
            }
            // Built from the documents in one pass each, sorted once rather
            // than inserted row by row.
            for (field, ix) in c.sorted.iter_mut() {
                let Some(fd) = c.schema.field(field) else {
                    continue;
                };
                *ix = SortedIndex::build(
                    &fd.ty,
                    &mut docs.iter().map(|d| (d.id, d.get(field).cloned())),
                );
            }
            for (field, ix) in c.vectors.iter_mut() {
                if restored.contains(field) {
                    continue;
                }
                let items: Vec<(DocId, Vec<f32>)> = docs
                    .iter()
                    .filter_map(|d| match d.get(field) {
                        Some(Value::Vector(v)) => Some((d.id, v.clone())),
                        _ => None,
                    })
                    .collect();
                ix.insert_batch(&items);
            }
        }
        Ok(())
    }

    /// Rewrites the file image (graph included), so the next open does not
    /// have to rebuild the indexes.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.refuse_if_failed()?;
        let image = self.snapshot();
        let r = self.sink_mut().rewrite(&image);
        self.storage(r)?;
        let r = self.sink_mut().sync();
        self.storage(r)?;
        self.dirty = false;
        Ok(())
    }

    fn wal(&mut self, rec: u8, cid: u32, payload: &[u8]) -> Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 12);
        frame.push(rec);
        put_uvarint(&mut frame, cid as u64);
        put_uvarint(&mut frame, payload.len() as u64);
        frame.extend_from_slice(payload);
        let r = self.sink_mut().append(&frame);
        self.storage(r)?;
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
        }
        let before = self.changes.seq();
        let out = self.execute_inner(stmt, params);
        let after = self.changes.seq();
        if after != before {
            if let Some(w) = &self.watcher {
                w.notify(after);
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
            Statement::Compact(which) => self.compact(which.as_deref()),
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

    /// Builds an index on an existing field and fills it from the current documents.
    fn create_index(
        &mut self,
        collection: &str,
        field: &str,
        kind: &IndexKind,
        if_not_exists: bool,
    ) -> Result<Response> {
        let c = self
            .collections
            .get(collection)
            .ok_or_else(|| Error::NotFound(format!("collection `{collection}`")))?;
        let f = c
            .schema
            .field(field)
            .ok_or_else(|| Error::NotFound(format!("field `{field}` in `{collection}`")))?;
        if f.index != IndexKind::None {
            if if_not_exists {
                return Ok(Response::Ok(format!("`{field}` is already indexed")));
            }
            return Err(Error::Exists(format!("an index on field `{field}`")));
        }
        if let IndexKind::Vector(_) = kind {
            if !matches!(f.ty, DataType::Vector(..)) {
                return Err(Error::Type(format!(
                    "field `{field}` is not vector<N>, no vector index can be built"
                )));
            }
        }
        if let IndexKind::Text(_) = kind {
            if !matches!(f.ty, DataType::Text) {
                return Err(Error::Type(format!(
                    "field `{field}` is not text, no full-text index can be built"
                )));
            }
        }
        if *kind == IndexKind::Sorted && !SortedIndex::supports(&f.ty) {
            return Err(Error::Type(format!(
                "field `{field}` is not int, float, timestamp or text, no ordered index can be built"
            )));
        }

        let cid = c.id;
        let c = self.collections.get_mut(collection).unwrap();
        let pos = c.schema.field_pos(field).unwrap();
        c.schema.fields[pos].index = kind.clone();

        // Build the index structure and fill it from the current documents.
        match kind {
            IndexKind::Vector(spec) => {
                let DataType::Vector(dim, prec) = c.schema.fields[pos].ty else {
                    unreachable!()
                };
                let mut ix = VectorIndex::with_precision(dim, *spec, prec);
                let ids: Vec<DocId> = c.store.ids();
                ix.reserve(ids.len());
                let mut items: Vec<(DocId, Vec<f32>)> = Vec::with_capacity(ids.len());
                for id in &ids {
                    if let Some(Value::Vector(v)) = c.store.read_field(*id, pos)? {
                        items.push((*id, v));
                    }
                }
                ix.insert_batch(&items);
                c.vectors.insert(field.to_string(), ix);
            }
            IndexKind::Hash => {
                let mut map: HashMap<Vec<u8>, Vec<DocId>> = HashMap::new();
                for id in c.store.ids() {
                    if let Some(v) = c.store.read_field(id, pos)? {
                        map.entry(hash_key(&v)).or_default().push(id);
                    }
                }
                c.hashes.insert(field.to_string(), map);
            }
            IndexKind::Text(spec) => {
                let mut ix = TextIndex::new(*spec);
                for id in c.store.ids() {
                    if let Some(Value::Text(t)) = c.store.read_field(id, pos)? {
                        ix.insert(id, &t);
                    }
                }
                ix.shrink_to_fit();
                c.texts.insert(field.to_string(), ix);
            }
            IndexKind::Sorted => {
                let ty = c.schema.fields[pos].ty.clone();
                let mut rows = Vec::with_capacity(c.store.len());
                for id in c.store.ids() {
                    rows.push((id, c.store.read_field(id, pos)?));
                }
                c.sorted.push((
                    field.to_string(),
                    SortedIndex::build(&ty, &mut rows.into_iter()),
                ));
            }
            IndexKind::None => {}
        }

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
                h.before_write(collection, op, &mut doc)?;
            }
            // Drop the old index entries when overwriting.
            if op == WriteOp::Update {
                if let Some(old) = c.store.read(&schema, doc.id)? {
                    c.unindex_doc(&old);
                }
            }
            let payload = Store::encode_doc(&schema, &doc);
            let frame = c.store.append(OP_PUT, doc.id, &payload);
            c.index_scalar(&doc);
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
            return Ok(match cap {
                Some(k) => c.store.iter_ids().take(k).collect(),
                None => c.store.ids(),
            });
        };

        // Hash index pushdown. The candidate set is picked from the smallest
        // of the indexable equalities in the `and` chain.
        let mut eqs = Vec::new();
        f.conjunct_equalities(params, &mut eqs);
        let mut candidates: Option<Vec<DocId>> = None;
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
                let Some((range, exact)) = sorted_range(&fd.ty, field, f, params) else {
                    continue;
                };
                let mut cap = candidates.as_ref().map_or(n / 2, |b| b.len()).min(n / 2);
                if want != usize::MAX {
                    cap = cap.min(want.saturating_mul(64).max(4096));
                }
                if let Some(ids) = ix.range_ids(&range, cap) {
                    candidates = Some(ids);
                    bare_range = exact && f.only_ranges_on(field, params);
                    bare_in = false;
                }
            }
        }

        let matches = |id: DocId| -> Result<bool> {
            let mut row = StoreRow {
                store: &c.store,
                schema: &c.schema,
                id,
                memo: Vec::new(),
            };
            Ok(truthy(&eval(f, &mut row, &ctx)?))
        };

        let mut out = Vec::new();
        match candidates {
            Some(mut b) => {
                b.sort_unstable();
                b.retain(|id| c.store.contains(*id));
                // If the filter is exactly that equality, or exactly a list
                // that was pushed down whole, or exactly the range an ordered
                // index expressed, no re-evaluation is needed.
                if f.is_bare_equality(params) || bare_in || bare_range {
                    b.truncate(want);
                    return Ok(b);
                }
                for id in b {
                    if out.len() >= want {
                        break;
                    }
                    if matches(id)? {
                        out.push(id);
                    }
                }
            }
            // No index: the full scan, read lazily so a cap stops it early.
            None => {
                for id in c.store.iter_ids() {
                    if out.len() >= want {
                        break;
                    }
                    if matches(id)? {
                        out.push(id);
                    }
                }
            }
        }
        Ok(out)
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
        let [(field, asc)] = sel.order.as_slice() else {
            return Ok(None);
        };
        let (Some(ix), Some(limit)) = (c.sorted_index(field), sel.limit) else {
            return Ok(None);
        };
        if ix.has_nan() || sel.lookup.as_ref().is_some_and(|l| l.required) {
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
                if let Some((r, _)) = sorted_range(&fd.ty, name, f, params) {
                    if other.range_ids(&r, 4096).is_some() {
                        return Ok(None);
                    }
                }
            }
            let fd = c
                .schema
                .field(field)
                .expect("an ordered index has its field");
            let own = sorted_range(&fd.ty, field, f, params);
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
                    let mut row = StoreRow {
                        store: &c.store,
                        schema: &c.schema,
                        id,
                        memo: Vec::new(),
                    };
                    if !truthy(&eval(f, &mut row, ctx)?) {
                        return Ok(true);
                    }
                }
            }
            out.push(id);
            Ok(out.len() < want)
        })?;
        Ok((!gave_up).then_some(out))
    }

    /// `match`, and `rerank` on top of it when the query asks for one.
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
        params: &[Value],
        ctx: &EvalCtx,
    ) -> Result<Vec<(DocId, Option<f32>)>> {
        let ix = c.texts.get(&m.field).ok_or_else(|| {
            Error::Query(format!(
                "field `{}` has no full-text index (declare it with @text)",
                m.field
            ))
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

        // As with `near`: the ceiling is checked before any scanning, because
        // erroring beats handing back a truncated relevance list.
        let bound = sel.limit.map(|l| l.saturating_add(sel.offset));
        if bound.unwrap_or(sel.offset) > MAX_MATCH_ROWS {
            return Err(Error::Query(format!(
                "`match` returns at most {MAX_MATCH_ROWS} rows, {} were requested (limit + offset)",
                bound.unwrap_or(sel.offset)
            )));
        }

        let allowed: Option<Vec<DocId>> = match &sel.filter {
            Some(_) => Some(self.matching_ids(&sel.collection, &sel.filter, params)?),
            None => None,
        };
        let accept = |id: DocId| match &allowed {
            Some(list) => list.binary_search(&id).is_ok(),
            None => true,
        };

        let want = bound.unwrap_or(MAX_MATCH_ROWS).max(1);
        let Some(rr) = &sel.rerank else {
            let hits = ix.search(&query, want, accept);
            return Ok(hits.into_iter().map(|(id, s)| (id, Some(s))).collect());
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
        let qv = match metric {
            Metric::Cosine => normalized(&qv),
            _ => qv,
        };

        let hits = ix.search(&query, candidates, accept);
        let mut out: Vec<(DocId, f32)> = Vec::with_capacity(hits.len());
        let mut buf: Vec<f32> = Vec::with_capacity(dim);
        for (id, _) in hits {
            if !c.store.read_vector_into(id, pos, &mut buf)? {
                continue; // no vector on this document: it cannot be ordered
            }
            if buf.len() != dim {
                continue;
            }
            // The store holds vectors as they were written; the HNSW arena is
            // what normalises on insert, and it is not in play here. With the
            // query already unit length, cosine only needs the candidate's
            // own norm -- computing it beside the dot product costs one pass
            // and saves a `Vec` per candidate, which at a thousand candidates
            // a query is the difference between an allocation-free scan and a
            // thousand allocations.
            let d = match metric {
                Metric::Cosine => {
                    let n = norm(&buf);
                    if n == 0.0 {
                        1.0
                    } else {
                        1.0 - dot(&qv, &buf) / n
                    }
                }
                _ => distance(metric, &qv, &buf),
            };
            out.push((id, d));
        }
        // Ascending distance, ties on the id so the answer is stable.
        out.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        out.truncate(want);
        Ok(out
            .into_iter()
            .map(|(id, d)| (id, Some(score_from_distance(metric, d))))
            .collect())
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
                        return self.retain_via_children(&steps, ids, cands, ctx);
                    }
                }
            }
        }

        let mut out = Vec::new();
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
        for (field, asc) in &l.order {
            let pos =
                if field == "id" {
                    None
                } else {
                    Some(child.schema.field_pos(field).ok_or_else(|| {
                        Error::NotFound(format!("field `{}.{field}`", l.collection))
                    })?)
                };
            keys.push((pos, *asc));
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
                    for (pos, _) in &keys {
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
                    for (i, (_, asc)) in keys.iter().enumerate() {
                        let o = a.0[i].cmp_value(&b.0[i]);
                        if o != Ordering::Equal {
                            return if *asc { o } else { o.reverse() };
                        }
                    }
                    a.1.cmp(&b.1)
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

    fn select(&self, sel: &Select, params: &[Value]) -> Result<ResultSet> {
        let c = self.collection(&sel.collection)?;
        let ctx = EvalCtx {
            params,
            registry: &self.registry,
        };
        sel.check()?;

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

        if let Some(m) = &sel.matcher {
            scored = self.run_match(c, sel, m, params, &ctx)?;
        } else if let Some(near) = &sel.near {
            // `near` decides the ordering by similarity; a second ordering is
            // rejected explicitly rather than ignored silently.
            if !sel.order.is_empty() {
                return Err(Error::Query(
                    "`near` cannot be combined with `order`: near orders results by similarity"
                        .into(),
                ));
            }
            let ix = c.vectors.get(&near.field).ok_or_else(|| {
                Error::Query(format!(
                    "field `{}` has no vector index (declare it with @hnsw)",
                    near.field
                ))
            })?;
            let qv = near_vector(eval(&near.vector, &mut NoRow, &ctx)?)?;
            if qv.len() != ix.dim {
                return Err(Error::Type(format!(
                    "the query vector must have {} dimensions, got {}",
                    ix.dim,
                    qv.len()
                )));
            }

            // If the requested row count exceeds the ceiling we stop before
            // scanning the filter: erroring beats returning a truncated result.
            let bound = sel.limit.map(|l| l.saturating_add(sel.offset));
            if bound.unwrap_or(sel.offset) > MAX_NEAR_ROWS {
                return Err(Error::Query(format!(
                    "`near` returns at most {MAX_NEAR_ROWS} rows, {} were requested (limit + offset)",
                    bound.unwrap_or(sel.offset)
                )));
            }

            // When there is a filter, work out the matching id set first and
            // then use it as a membership test during the ANN walk.
            let allowed: Option<Vec<DocId>> = match &sel.filter {
                Some(_) => Some(self.matching_ids(&sel.collection, &sel.filter, params)?),
                None => None,
            };
            let accept = |id: DocId| match &allowed {
                Some(list) => list.binary_search(&id).is_ok(),
                None => true,
            };

            let want = bound.unwrap_or(MAX_NEAR_ROWS).max(1);
            let hits = match &allowed {
                // No filter: ANN directly, or a full scan when asked for.
                None if near.exact => ix.search_exact(&qv, want, accept),
                None => ix.search(&qv, want, near.ef, accept),
                // `exact` is the verification path: it always scans everything.
                Some(_) if near.exact => ix.search_exact(&qv, want, accept),
                Some(ids) if ids.len() <= ix.probe_budget(near.ef) => {
                    // The filter set is smaller than the number of candidates
                    // the ANN walk would measure anyway: the walk buys nothing,
                    // and scanning directly is both cheaper and exact.
                    ix.search_ids(&qv, want, ids)
                }
                Some(ids) => {
                    let hits = ix.search(&qv, want, near.ef, &accept);
                    // `search` applies the filter *after* the candidates are
                    // gathered. A selective filter may leave fewer results than
                    // the limit, or none at all when the filter field
                    // correlates with the vector (all `ef` nearest neighbours
                    // are eliminated). If it comes up short we scan the whole
                    // filter set and give the right answer.
                    if hits.len() < want.min(ids.len()) {
                        ix.search_ids(&qv, want, ids)
                    } else {
                        hits
                    }
                }
            };
            scored = hits.into_iter().map(|(id, s)| (id, Some(s))).collect();
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
                for (field, asc) in &sel.order {
                    let pos = if field == "id" {
                        None
                    } else {
                        Some(
                            c.schema
                                .field_pos(field)
                                .ok_or_else(|| Error::NotFound(format!("field `{field}`")))?,
                        )
                    };
                    keys.push((pos, *asc));
                }
                let k = sel
                    .limit
                    .map(|l| l.saturating_add(sel.offset))
                    .unwrap_or(usize::MAX);
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

        for id in ids {
            let c = self.collections.get_mut(collection).unwrap();
            let Some(mut doc) = c.store.read(&schema, id)? else {
                continue;
            };
            {
                let snapshot = doc.clone();
                let ctx = EvalCtx {
                    params,
                    registry: &self.registry,
                };
                for (k, e) in set {
                    let f = schema
                        .field(k)
                        .ok_or_else(|| Error::NotFound(format!("field `{k}`")))?;
                    let v = eval(e, &mut DocRow(&snapshot), &ctx)?;
                    doc.set(k, v.coerce(&f.ty)?);
                }
            }
            let c = self.collections.get_mut(collection).unwrap();
            for h in &hooks {
                h.before_write(collection, WriteOp::Update, &mut doc)?;
            }
            if let Some(old) = c.store.read(&schema, id)? {
                c.unindex_doc(&old);
            }
            let payload = Store::encode_doc(&schema, &doc);
            let frame = c.store.append(OP_PUT, id, &payload);
            c.index_doc(&doc);
            self.wal(REC_DATA, cid, &frame)?;
            self.note(cid, id);
            for h in &hooks {
                h.after_write(collection, WriteOp::Update, &doc)?;
            }
            n += 1;
        }
        Ok(Response::Affected(n))
    }

    fn delete(
        &mut self,
        collection: &str,
        filter: &Option<Expr>,
        params: &[Value],
    ) -> Result<Response> {
        let ids = self.matching_ids(collection, filter, params)?;
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
                c.unindex_doc(&doc);
            }
            let frame = c.store.append(OP_DEL, id, &[]);
            self.wal(REC_DATA, cid, &frame)?;
            self.note(cid, id);
            n += 1;
        }
        Ok(Response::Affected(n))
    }

    fn compact(&mut self, which: Option<&str>) -> Result<Response> {
        let targets: Vec<String> = match which {
            Some(n) => {
                self.collection(n)?;
                vec![n.to_string()]
            }
            None => self.order.clone(),
        };
        let mut reclaimed = 0usize;
        for name in targets {
            let c = self.collections.get_mut(&name).unwrap();
            reclaimed += c.store.dead_bytes();
            c.store.compact()?;
        }
        self.rebuild_indexes()?;
        // After compaction the persisted image is rewritten from scratch.
        let image = self.snapshot();
        let r = self.sink_mut().rewrite(&image);
        self.storage(r)?;
        Ok(Response::Ok(format!(
            "compaction done, {reclaimed} bytes reclaimed"
        )))
    }
}

/// The range a filter's comparisons put on an ordered field, and whether
/// every one of them could be said as a key. A comparison the key space
/// cannot express exactly -- `price < 12.5` on an `int` field -- is left out
/// of the range, which then only narrows, and the filter is evaluated.
fn sorted_range(
    ty: &DataType,
    field: &str,
    f: &Expr,
    params: &[Value],
) -> Option<(SortRange, bool)> {
    let mut ranges = Vec::new();
    f.conjunct_ranges(params, &mut ranges);
    let mut range = SortRange::all();
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

/// Sort keys: a field position (`None` for `id`) and whether it ascends.
type OrderKey = (Option<usize>, bool);

/// The order `order` asks for between two rows' keys, before any tie-break.
fn rank(keys: &[OrderKey], a: &[Value], b: &[Value]) -> Ordering {
    for (i, (_, asc)) in keys.iter().enumerate() {
        let o = a[i].cmp_value(&b[i]);
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
        for (pos, _) in keys {
            flat.push(match pos {
                None => Value::Int(id as i64),
                Some(p) => store.read_field(id, *p)?.unwrap_or(Value::Null),
            });
        }
    }
    let row = |i: usize| &flat[i * n..i * n + n];
    // Ties fall back to the position the row came in, which is what the
    // stable sort this replaced gave them: a selection is not stable, and a
    // page must not change with the plan.
    let cmp = |a: &usize, b: &usize| rank(keys, row(*a), row(*b)).then(a.cmp(b));
    let mut idx: Vec<usize> = (0..ids.len()).collect();
    if k < idx.len() {
        idx.select_nth_unstable_by(k - 1, cmp);
        idx.truncate(k);
    }
    idx.sort_unstable_by(cmp);
    Ok(idx.into_iter().map(|i| ids[i]).collect())
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
