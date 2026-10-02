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

#[cfg(not(target_arch = "wasm32"))]
mod handover;
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

/// A block of writes to more than one collection, or holding a schema
/// change among others, which lands whole or not at all
/// ([`Database::execute_block`]): `[9][0][length]{record}`, each a data
/// record or a schema change's. A block's writes to one collection go into
/// one data record of as many frames instead -- the form an image's records
/// take, which a version before blocks reads -- and a lone schema change is
/// the record it always was, so only a block across collections, or a
/// schema change with other writes, needs this kind. A version before
/// blocks refuses it, and one before 10.4 a block holding a schema change,
/// as corrupt. Either way the block is one record, appended whole or cut
/// off whole: a crash leaves none of it.
const REC_BLOCK: u8 = 9;

/// A part of a block not yet landed, which spilled into the file as the
/// block outgrew [`SPILL_AT`] (`Database::spill`): `[10][0][length]{record}`,
/// its body a block record's. It moves no counter, and a load takes it only
/// where a land names it: a crash, a rollback or a block put back leaves it
/// dead in the file, and a load cuts off the ones after the last write. A
/// version before it refuses it as corrupt.
const REC_SPILL: u8 = 10;

/// A block that spilled, landing: `[11][0][length][count]{[place][length]}
/// {record}` -- the spills it takes, by where their bodies are in the file,
/// then the rest of the block's records. A load applies the spills' records
/// in order and then its own, and the counter moves by all their writes. It
/// never travels: a primary's feed is sent the block as the one block record
/// it would have been ([`Sink::land`], [`landed_block`]).
const REC_LAND: u8 = 11;

/// A collection's fields changed (`alter collection`): `[12][collection]
/// [length][change][field][new name][schema]` -- which change, the field it
/// is of, the name a rename gives it (empty otherwise), and the schema
/// after it. A record of its own rather than an `REC_ALTER` with a change
/// byte: a version before it reads every kind-5 record as an index added,
/// and would have taken a field moved for one; this one it refuses. Undone
/// in a block as an index built is ([`Undo::Altered`]).
const REC_FIELDS: u8 = 12;
const FIELD_ADD: u8 = 1;
const FIELD_DROP: u8 = 2;
const FIELD_RENAME: u8 = 3;
/// A field's expiry set or taken off (`alter field f @ttl(..)`): the
/// schema after it, and nothing of the field's ordered index changes.
const FIELD_TTL: u8 = 4;

/// The expiry a [`FIELD_TTL`] change leaves its field with.
fn ttl_after(ch: &FieldChange) -> Option<u64> {
    ch.schema.field(&ch.field).and_then(|f| f.index.ttl())
}

/// A [`REC_FIELDS`] record's body, read.
struct FieldChange {
    op: u8,
    field: String,
    to: String,
    schema: Schema,
}

impl FieldChange {
    fn encode(&self) -> Vec<u8> {
        let mut out = vec![self.op];
        crate::codec::encode_str(&mut out, &self.field);
        crate::codec::encode_str(&mut out, &self.to);
        out.extend_from_slice(&self.schema.encode());
        out
    }

    fn decode(body: &[u8]) -> Result<FieldChange> {
        let op = *body
            .first()
            .ok_or_else(|| Error::Corrupt("an alter record cut short".into()))?;
        if !matches!(op, FIELD_ADD | FIELD_DROP | FIELD_RENAME | FIELD_TTL) {
            return Err(Error::Corrupt(format!("unknown field change {op}")));
        }
        let mut p = 1;
        let field = crate::codec::decode_str(body, &mut p)?;
        let to = crate::codec::decode_str(body, &mut p)?;
        let schema = Schema::decode(body, &mut p)?;
        Ok(FieldChange {
            op,
            field,
            to,
            schema,
        })
    }
}

/// The schema a schema change's record holds: a create's or an index's
/// whole, an alter's after its change.
#[cfg(not(target_arch = "wasm32"))]
fn schema_in(kind: u8, body: &[u8]) -> Result<Schema> {
    match kind {
        REC_FIELDS => FieldChange::decode(body).map(|c| c.schema),
        _ => Schema::decode(body, &mut 0),
    }
}

/// The spills a land names, by where their bodies are in the file and how
/// long they are, and the records after them.
type LandParts<'a> = (Vec<(u64, u64)>, &'a [u8]);

/// A land's body, in its parts.
fn land_parts(body: &[u8]) -> Result<LandParts<'_>> {
    let mut p = 0;
    let n = get_uvarint(body, &mut p)? as usize;
    let mut named = Vec::with_capacity(n.min(body.len()));
    for _ in 0..n {
        let at = get_uvarint(body, &mut p)?;
        named.push((at, get_uvarint(body, &mut p)?));
    }
    let rest = body
        .get(p..)
        .ok_or_else(|| Error::Corrupt("a land cut short".into()))?;
    Ok((named, rest))
}

/// The block record a block that spilled would have been, from the bodies
/// of its spills and its land: what a primary's feed sends its replicas,
/// which apply a block whole.
#[cfg(not(target_arch = "wasm32"))]
pub fn landed_block(spilled: &[&[u8]], land: &[u8]) -> Result<Vec<u8>> {
    let r = record_at(land, &mut 0)?;
    if r.kind != REC_LAND {
        return Err(Error::Corrupt("not a land".into()));
    }
    let (_, rest) = land_parts(r.body)?;
    let mut body = Vec::with_capacity(spilled.iter().map(|s| s.len()).sum::<usize>() + rest.len());
    for s in spilled {
        body.extend_from_slice(s);
    }
    body.extend_from_slice(rest);
    Ok(framed(REC_BLOCK, 0, &body))
}

/// The writes a record holds, which is how far it moves the change counter:
/// a data record's frames, a block's records' writes, one for a schema
/// change, none for what is not a write. A feed counting records by their
/// numbers -- a replica's, an archive's -- counts them with this.
pub fn writes_in(record: &[u8]) -> Result<u64> {
    if record.is_empty() {
        return Ok(0);
    }
    let r = record_at(record, &mut 0)?;
    let body = r.body;
    match record.first() {
        Some(&REC_DATA) => frames_in(body),
        Some(&REC_BLOCK) => {
            let mut n = 0;
            each_inner(body, &mut |kind, _, inner| {
                n += match kind {
                    REC_DATA => frames_in(inner)?,
                    _ => 1,
                };
                Ok(())
            })?;
            Ok(n)
        }
        Some(&(REC_CREATE | REC_DROP | REC_ALTER | REC_FIELDS)) => Ok(1),
        // The land's own records; the spills' are counted where it is
        // loaded, since it never travels.
        Some(&REC_LAND) => {
            let (_, rest) = land_parts(body)?;
            let mut n = 0;
            each_inner(rest, &mut |kind, _, inner| {
                n += match kind {
                    REC_DATA => frames_in(inner)?,
                    _ => 1,
                };
                Ok(())
            })?;
            Ok(n)
        }
        _ => Ok(0),
    }
}

/// The frames in a data record's body.
fn frames_in(body: &[u8]) -> Result<u64> {
    let mut n = 0;
    let mut pos = 0;
    while pos < body.len() {
        pos += 1;
        get_uvarint(body, &mut pos)?;
        pos += get_uvarint(body, &mut pos)? as usize;
        n += 1;
    }
    if pos > body.len() {
        return Err(Error::Corrupt("a frame runs past its record".into()));
    }
    Ok(n)
}

/// A write, as [`Database::changes_in`] reads it out of a record.
#[cfg(not(target_arch = "wasm32"))]
pub struct Change {
    /// The change counter's number for it.
    pub seq: u64,
    /// Its collection's name; `None` for one this database no longer
    /// knows.
    pub collection: Option<String>,
    pub kind: ChangeKind,
}

#[cfg(not(target_arch = "wasm32"))]
pub enum ChangeKind {
    /// A document written, `None` where its collection is not known.
    Put(DocId, Option<Document>),
    Del(DocId),
    Create(Schema),
    /// An index made, or a field added, dropped or renamed: the schema
    /// after it, which the documents after it are read by.
    Alter(Schema),
    Drop,
}

/// [`Database::changes_in`]'s walk: the schemas by collection id, as the
/// records change them, and the number of the last write handed over.
#[cfg(not(target_arch = "wasm32"))]
struct ChangeWalk<'a> {
    known: HashMap<u32, Schema>,
    seq: u64,
    go: bool,
    each: &'a mut dyn FnMut(Change) -> bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl ChangeWalk<'_> {
    fn record(&mut self, kind: u8, cid: u32, body: &[u8]) -> Result<()> {
        if kind != REC_DATA {
            let what = match kind {
                REC_DROP => ChangeKind::Drop,
                _ => {
                    let schema = schema_in(kind, body)?;
                    self.known.insert(cid, schema.clone());
                    match kind {
                        REC_CREATE => ChangeKind::Create(schema),
                        _ => ChangeKind::Alter(schema),
                    }
                }
            };
            let collection = match kind {
                REC_DROP => self.known.remove(&cid).map(|s| s.name),
                _ => self.known.get(&cid).map(|s| s.name.clone()),
            };
            self.hand(collection, what);
            return Ok(());
        }
        let cut = || Error::Corrupt("a write record cut short".into());
        let mut p = 0;
        while p < body.len() && self.go {
            let op = body[p];
            p += 1;
            let id = get_uvarint(body, &mut p)?;
            let plen = get_uvarint(body, &mut p)? as usize;
            let payload = body.get(p..p + plen).ok_or_else(cut)?;
            p += plen;
            let schema = self.known.get(&cid);
            let what = match (op, schema) {
                (OP_PUT, Some(schema)) => ChangeKind::Put(id, Some(schema.read_doc(id, payload)?)),
                (OP_PUT, None) => ChangeKind::Put(id, None),
                _ => ChangeKind::Del(id),
            };
            let collection = schema.map(|s| s.name.clone());
            self.hand(collection, what);
        }
        Ok(())
    }

    fn hand(&mut self, collection: Option<String>, kind: ChangeKind) {
        self.seq += 1;
        let seq = self.seq;
        self.go = (self.each)(Change {
            seq,
            collection,
            kind,
        });
    }
}

/// What [`each_inner`] hands a block's records to: each one's kind,
/// collection id and body.
type Inner<'a> = dyn FnMut(u8, u32, &[u8]) -> Result<()> + 'a;

/// Walks a block's records, each a data record or a schema change's -- a
/// block holds nothing else -- handing `f` its kind, collection id and
/// body.
fn each_inner(body: &[u8], f: &mut Inner<'_>) -> Result<()> {
    let mut pos = 0;
    while pos < body.len() {
        let r = record_at(body, &mut pos)?;
        if !matches!(
            r.kind,
            REC_DATA | REC_CREATE | REC_DROP | REC_ALTER | REC_FIELDS
        ) {
            return Err(Error::Corrupt(
                "a block holds a record that is no write".into(),
            ));
        }
        f(r.kind, r.cid, r.body)?;
    }
    Ok(())
}

/// `[kind][collection id][length][body]`.
fn framed(kind: u8, cid: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + HEAD_ROOM);
    frame_into(&mut out, kind, cid, body);
    out
}

fn frame_into(out: &mut Vec<u8>, kind: u8, cid: u32, body: &[u8]) {
    let (h, n) = head(kind, cid, body.len());
    out.extend_from_slice(&h[..n]);
    out.extend_from_slice(body);
}

/// A record, as a walk over a file or a feed of them finds it.
struct Rec<'a> {
    kind: u8,
    cid: u32,
    /// Where it starts, and where its body does, in the bytes walked.
    at: usize,
    body_at: usize,
    body: &'a [u8],
}

/// The record at `*pos` -- `[kind][collection][length][body]`, or the
/// counter's fixed-width head -- and `*pos` moved past it: the one place a
/// head is read.
fn record_at<'a>(bytes: &'a [u8], pos: &mut usize) -> Result<Rec<'a>> {
    let at = *pos;
    let kind = bytes[at];
    if kind == REC_SEQ {
        let body = bytes
            .get(at + 1..at + REC_SEQ_LEN)
            .ok_or_else(|| Error::Corrupt("truncated counter header".into()))?;
        *pos = at + REC_SEQ_LEN;
        return Ok(Rec {
            kind,
            cid: 0,
            at,
            body_at: at + 1,
            body,
        });
    }
    let mut p = at + 1;
    let cid = get_uvarint(bytes, &mut p)? as u32;
    let len = get_uvarint(bytes, &mut p)? as usize;
    let body = bytes
        .get(p..p.saturating_add(len))
        .ok_or_else(|| Error::Corrupt("a record cut short".into()))?;
    *pos = p + len;
    Ok(Rec {
        kind,
        cid,
        at,
        body_at: p,
        body,
    })
}

/// The whole records of a file, from `pos`: what a load, a repoint and the
/// search for the last graphs each walk. It stops at the end or at a record
/// cut short -- `torn`, `pos` where it starts -- which is the walker's to
/// judge: a crash's torn tail, or damage inside an image.
struct Walk<'a> {
    bytes: &'a [u8],
    pos: usize,
    torn: bool,
}

impl<'a> Walk<'a> {
    fn new(bytes: &'a [u8], pos: usize) -> Walk<'a> {
        Walk {
            bytes,
            pos,
            torn: false,
        }
    }

    fn next(&mut self) -> Result<Option<Rec<'a>>> {
        if self.pos >= self.bytes.len() {
            return Ok(None);
        }
        if !whole_record(self.bytes, self.pos)? {
            self.torn = true;
            return Ok(None);
        }
        record_at(self.bytes, &mut self.pos).map(Some)
    }
}

/// A record's head, `[kind][collection][length]`, and how many of the bytes
/// it took: the one place a head is written, the counter's fixed-width one
/// aside ([`image_head`]). Into an array, so that a block's can go into the
/// room it left before its frames.
fn head(kind: u8, cid: u32, len: usize) -> ([u8; HEAD_ROOM], usize) {
    let mut h = [0u8; HEAD_ROOM];
    h[0] = kind;
    let mut n = 1;
    for mut v in [cid as u64, len as u64] {
        while v >= 0x80 {
            h[n] = (v as u8) | 0x80;
            v >>= 7;
            n += 1;
        }
        h[n] = v as u8;
        n += 1;
    }
    (h, n)
}

/// The room a block leaves before its frames for the header of the data
/// record they land as -- a kind, then a collection id and a length, at
/// most 5 and 10 bytes as uvarints -- so the frames are not copied again.
const HEAD_ROOM: usize = 16;

/// The writes of a block not yet landed ([`Database::execute_block`]).
#[derive(Default)]
struct Block {
    /// `HEAD_ROOM` bytes, then each write's frame, as the store holds it --
    /// or, for a schema change, its record's body.
    frames: Vec<u8>,
    /// Per write: its record's kind, its collection, and where its frame
    /// ends in `frames`.
    heads: Vec<(u8, u32, usize)>,
    /// The writes to note on the change feed once the block lands.
    notes: Vec<(u32, DocId)>,
    /// Per collection written: where its store stood before the first.
    marks: Vec<(u32, crate::store::Mark)>,
    /// Per write, what puts it back.
    was: Vec<Undo>,
    /// Whether [`Database::begin`] opened it -- a `/batch`, the browser
    /// module's `run` of several -- rather than a lone statement: its statements are a batch
    /// together, so a `put` in it leaves its vectors waiting, and they are
    /// linked [`LINK_AT`] at a time on every core and at its end.
    defers: bool,
    /// Per collection and vector field, the nodes the block's `put`s left
    /// waiting since they were last linked: the newest of the field's
    /// waiting nodes, since nothing else leaves one while a block is open.
    waiting: Vec<(u32, String, usize)>,
    /// Where the block's spills' bodies are in the file (`Database::spill`),
    /// and how many of its writes they hold -- the first of `heads`, whose
    /// frames `frames` no longer holds.
    #[cfg(not(target_arch = "wasm32"))]
    spilled: Vec<(u64, u64)>,
    #[cfg(not(target_arch = "wasm32"))]
    spilled_writes: usize,
    /// The file as mapped when the block last spilled: its land reads the
    /// spills' bodies from there for a feed.
    #[cfg(not(target_arch = "wasm32"))]
    spill_base: Option<crate::store::Base>,
    /// A spill could not be made: the block spills no more, and lands
    /// holding its frames as a block that never spilled does.
    #[cfg(not(target_arch = "wasm32"))]
    unspillable: bool,
}

/// The nodes a block's `put`s leave waiting before they are linked, a
/// field at a time: a batch as wide as the widest the graph links at once
/// (`MAX_BATCH`), whose candidates are found on every core. Linked as each
/// statement wrote them, a row at a time as a driver's `executemany` and a
/// `/batch` of single puts send them, 100 000 128-dim rows went in at
/// 5.4k rows/s with the graph kept over the pg wire as it was and 5.3k over
/// HTTP; linked together, at 16.9k and 16.0k (`make load-bench`).
const LINK_AT: usize = 512;

/// Whether a block's `put`s leave their vectors waiting: natively. The
/// browser links a vector as it is written, on its one thread, and the
/// waiting was 843 bytes brotli of its module for nothing.
const DEFERS: bool = !cfg!(target_arch = "wasm32");

/// What puts one of a block's writes back ([`Database::rollback`]).
enum Undo {
    /// A document written: its collection, its id, and where its record was
    /// before.
    Doc(u32, DocId, Option<crate::store::Loc>),
    /// A collection made: it goes, and its id is handed out again.
    Created(u32),
    /// A collection dropped, with where its name stood among the others: it
    /// comes back as it was.
    Dropped(Box<Collection>, usize),
    /// An index built over a collection's field: it goes.
    Indexed(u32, usize),
    /// An index built on a path into a json field, by the path: it goes.
    PathIndexed(u32, String),
    /// A collection's fields changed: the schema before, and the indexes
    /// a dropped field took with it, to put back.
    Altered(u32, Box<Altered>),
}

/// What [`Undo::Altered`] puts back.
struct Altered {
    before: Schema,
    op: u8,
    field: String,
    to: String,
    taken: Taken,
}

/// The indexes of one field, taken off a collection by a drop or a rename
/// and put back under the name they go by after it.
#[derive(Default)]
struct Taken {
    vector: Option<VectorIndex>,
    hash: Option<Derived<HashIndex>>,
    text: Option<Derived<TextIndex>>,
    sorted: Option<Derived<SortedIndex>>,
    sparse: Option<Derived<SparseIndex>>,
}

/// Puts `ix` among a collection's ordered or sparse indexes where `field`
/// stands in the schema -- a field among the fields, a path after them
/// among the paths: the order they are kept in, which keeps the choice
/// between two ranges the same as after an open.
fn in_schema_order<T>(list: &mut Vec<(String, T)>, schema: &Schema, field: &str, ix: T) {
    let rank = |n: &str| match schema.field_pos(n) {
        Some(p) => Some(p),
        None => schema
            .paths
            .iter()
            .position(|p| p.name == n)
            .map(|i| schema.fields.len() + i),
    };
    let pos = rank(field);
    let at = list
        .iter()
        .position(|(n, _)| rank(n) > pos)
        .unwrap_or(list.len());
    list.insert(at, (field.to_string(), ix));
}

impl Block {
    /// Emptied for the next block, what it allocated kept: every write is a
    /// block, and allocated anew each time -- five buffers, and the record
    /// a copy of the frames -- they took a lone `put` from 832 ns to 985.
    /// Kept, a put takes 841 against the 829 it took before blocks, a `del`
    /// 648 against 634. A large block's are not kept ([`Database::spare`]).
    fn cleared(mut self) -> Block {
        self.frames.clear();
        self.frames.extend_from_slice(&[0; HEAD_ROOM]);
        self.heads.clear();
        self.notes.clear();
        self.marks.clear();
        self.was.clear();
        self.defers = false;
        self.waiting.clear();
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.spilled.clear();
            self.spilled_writes = 0;
            self.spill_base = None;
            self.unspillable = false;
        }
        self
    }

    /// The records of the writes from `from` on, as a block record's body
    /// holds them: a data record for each run of one collection's frames --
    /// its frames one stretch where a store takes them in from the file --
    /// and each schema change's record. What a spill writes, and a land
    /// after the spills.
    #[cfg(not(target_arch = "wasm32"))]
    fn body_from(&self, from: usize) -> Vec<u8> {
        let mut body = Vec::with_capacity(self.frames.len());
        let (mut k, mut start) = (from, HEAD_ROOM);
        while k < self.heads.len() {
            let (kind, cid, mut end) = self.heads[k];
            let mut next = k + 1;
            while kind == REC_DATA
                && next < self.heads.len()
                && self.heads[next].0 == REC_DATA
                && self.heads[next].1 == cid
            {
                end = self.heads[next].2;
                next += 1;
            }
            frame_into(&mut body, kind, cid, &self.frames[start..end]);
            (start, k) = (end, next);
        }
        body
    }

    /// What an open block's buffers hold: the frames above all, a second
    /// copy of every document the block wrote until it lands -- uncounted, a
    /// COPY could write twice `--max-memory` before the ceiling saw it. The
    /// spare's are not data, and a large block's are let go of as it lands
    /// ([`Database::spare`]): counted, a small ceiling stayed shut after a
    /// `del` and a `compact`, by the spare's buffers alone.
    fn bytes(&self) -> usize {
        use std::mem::size_of;
        self.frames.capacity()
            + self.heads.capacity() * size_of::<(u8, u32, usize)>()
            + self.notes.capacity() * size_of::<(u32, DocId)>()
            + self.was.capacity() * size_of::<Undo>()
    }

    /// The block as the one record it lands as: one data record of every
    /// frame when they are all one collection's writes -- a lone write's
    /// record as it always was -- the record of a lone schema change, and a
    /// block record around each write's record otherwise.
    fn record(&mut self) -> std::borrow::Cow<'_, [u8]> {
        let (kind, cid, _) = self.heads[0];
        if self.heads.len() == 1 || self.heads.iter().all(|h| h.0 == REC_DATA && h.1 == cid) {
            // The header goes into the room before the frames.
            let (h, n) = head(kind, cid, self.frames.len() - HEAD_ROOM);
            self.frames[HEAD_ROOM - n..HEAD_ROOM].copy_from_slice(&h[..n]);
            return std::borrow::Cow::Borrowed(&self.frames[HEAD_ROOM - n..]);
        }
        let mut body = Vec::with_capacity(self.frames.len() + HEAD_ROOM * self.heads.len());
        let mut start = HEAD_ROOM;
        for &(kind, cid, end) in &self.heads {
            frame_into(&mut body, kind, cid, &self.frames[start..end]);
            start = end;
        }
        std::borrow::Cow::Owned(framed(REC_BLOCK, 0, &body))
    }
}

/// Whether the record at `at` is all there, rather than cut short where the
/// bytes end -- in its header or its body -- as a crash in the middle of an
/// append leaves the last one. Only the kinds written with a length are
/// judged: a kind this version does not know is the loader's to refuse, and
/// the counter header is only ever written by a rewrite, whole or not at all.
fn whole_record(bytes: &[u8], at: usize) -> Result<bool> {
    if !matches!(
        bytes[at],
        REC_CREATE
            | REC_DROP
            | REC_DATA
            | REC_GRAPH
            | REC_ALTER
            | REC_FIELDS
            | REC_NEXTID
            | REC_HISTORY
            | REC_BLOCK
            | REC_SPILL
            | REC_LAND
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
    let mut walk = Walk::new(bytes, MAGIC.len());
    while walk.pos < bytes.len() {
        if !matches!(
            bytes[walk.pos],
            REC_SEQ
                | REC_CREATE
                | REC_DROP
                | REC_DATA
                | REC_GRAPH
                | REC_ALTER
                | REC_FIELDS
                | REC_NEXTID
                | REC_HISTORY
                | REC_BLOCK
                | REC_SPILL
                | REC_LAND
        ) {
            break;
        }
        let Some(r) = walk.next()? else { break };
        if r.kind == REC_GRAPH {
            let field = crate::codec::decode_str(r.body, &mut 0)?;
            match last.iter_mut().find(|(c, f, _)| *c == r.cid && *f == field) {
                Some(l) => l.2 = r.at,
                None => last.push((r.cid, field, r.at)),
            }
        }
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
/// offset in the file, its bytes, the index of its frames the image wrote
/// before it ([`Store::image_index`]), and what to tell of each document's
/// id.
type Replay<'a> =
    dyn FnMut(&mut Store, usize, &[u8], Option<&[u8]>, &mut dyn FnMut(DocId)) -> Result<usize> + 'a;

/// Where an image is written: the file being rewritten, or a buffer. The
/// counter header's body length is only known once the body is out, so it is
/// patched where it stands rather than the image being written twice.
pub trait ImageOut {
    fn write(&mut self, bytes: &[u8]) -> Result<()>;
    /// Bytes of a store that stay as they are while `kept` holds them,
    /// which a writer may hold rather than copy: in the browser an image
    /// is taken a chunk at a time as the page stores each ([`Kept`]).
    fn write_kept(&mut self, kept: Kept) -> Result<()> {
        self.write(kept.bytes())
    }
    /// Bytes written so far, which is where the next one lands.
    fn at(&self) -> u64;
    /// Overwrites bytes written earlier, in place.
    fn patch(&mut self, at: u64, bytes: &[u8]) -> Result<()>;
}

/// A stretch of a store's bytes, held: the image a load kept, which never
/// changes, or a segment, which a write copies first while it is held
/// (`Arc::make_mut`). So an image taken a chunk at a time is the database
/// as it stood when it was begun, whatever is written meanwhile.
pub struct Kept {
    pub whole: std::sync::Arc<Vec<u8>>,
    pub at: usize,
    pub len: usize,
}

impl Kept {
    pub fn bytes(&self) -> &[u8] {
        &self.whole[self.at..self.at + self.len]
    }
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
    /// [`Self::append`] for a record whose durability ([`Self::flush`]) its
    /// caller runs as soon as it lets its lock go -- a graph a server keeps
    /// ([`Database::save_graphs`]): a file's sink leaves it to that
    /// durability rather than writing what outgrew its buffer there and
    /// then, which put megabytes into the file under the read lock.
    fn append_deferred(&mut self, bytes: &[u8]) -> Result<()> {
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
    fn remapped(&mut self) -> Option<crate::store::Base> {
        None
    }
    /// Writes every record appended so far into the file and hands back the
    /// file mapped as far as it goes: where a mapped database's stores take
    /// the records written since the open in from, rather than hold them
    /// ([`Database::hand_over`]). `Ok(None)` for every sink but a file's.
    #[cfg(not(target_arch = "wasm32"))]
    fn written_through(&mut self) -> Result<Option<crate::store::Base>> {
        Ok(None)
    }
    /// Appends a block that spilled as it lands ([`Database::spill`]):
    /// `record` is its land, naming the spills the file holds already, and
    /// `spilled` their bodies, for a sink that passes writes on -- a
    /// primary's feed -- to send the block as the one block record it
    /// would have been ([`landed_block`]), which a replica applies whole.
    #[cfg(not(target_arch = "wasm32"))]
    fn land(&mut self, seq: u64, spilled: &[&[u8]], record: &[u8]) -> Result<()> {
        let _ = spilled;
        self.record(seq, record)
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

/// Whether a database may take a write now, and why not: a node's lease
/// from its router, which names the tenants the node may write and lapses
/// unless renewed ([`Database::set_fence`]). Native only: a page has no
/// router.
#[cfg(not(target_arch = "wasm32"))]
pub type Fence = Arc<dyn Fn() -> std::result::Result<(), String> + Send + Sync>;

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
pub(crate) fn record_head(kind: u8, cid: u32, len: usize) -> Vec<u8> {
    let (h, n) = head(kind, cid, len);
    h[..n].to_vec()
}

/// The front of an image: the signature, and the change counter's head,
/// fixed width -- `seq`, and a zero for the body's length, which is
/// patched in place once the body is written (`head_at + 9`).
fn image_head(seq: u64) -> Vec<u8> {
    let mut out = Vec::from(&MAGIC[..]);
    out.push(REC_SEQ);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
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
    map: crate::maps::Map<Vec<u8>, Vec<DocId>>,
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

    /// The table, the keys and the buckets as they sit in memory.
    pub fn memory_bytes(&self) -> usize {
        self.map.capacity() * (std::mem::size_of::<(Vec<u8>, Vec<DocId>)>() + 1) + self.heap
    }

    /// A value two documents or more hold, `null` aside, and two of them:
    /// what `create index ... @unique` over the documents there is refused
    /// for, naming it.
    fn shared(&self) -> Option<(Value, DocId, DocId)> {
        let null = hash_key(&Value::Null);
        let (key, ids) = self
            .map
            .iter()
            .find(|(k, ids)| ids.len() > 1 && **k != null)?;
        let v = crate::codec::decode_value(key, &mut 0).ok()?;
        Some((v, ids[0], ids[1]))
    }
}

/// Field name -> index, in the order the fields were indexed: a `Vec`
/// searched by name rather than a map, as `sorted` and `sparse` are. A
/// collection indexes a handful of fields, and each map of them was a copy
/// of hashbrown's code in the browser module.
pub struct Fields<T>(Vec<(String, T)>);

impl<T> Default for Fields<T> {
    fn default() -> Self {
        Fields(Vec::new())
    }
}

impl<T> Fields<T> {
    pub fn get(&self, name: &str) -> Option<&T> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut T> {
        self.0.iter_mut().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Puts `v` under `name`, handing back what was there.
    pub fn insert(&mut self, name: String, v: T) -> Option<T> {
        match self.get_mut(&name) {
            Some(slot) => Some(std::mem::replace(slot, v)),
            None => {
                self.0.push((name, v));
                None
            }
        }
    }

    pub fn remove(&mut self, name: &str) -> Option<T> {
        let at = self.0.iter().position(|(n, _)| n == name)?;
        Some(self.0.remove(at).1)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &T)> {
        self.into_iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&String, &mut T)> {
        self.0.iter_mut().map(|(k, v)| (&*k, v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.0.iter().map(|(_, v)| v)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.0.iter_mut().map(|(_, v)| v)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clear(&mut self) {
        self.0.clear()
    }
}

fn pair<K, V>(e: &(K, V)) -> (&K, &V) {
    (&e.0, &e.1)
}

impl<'a, T> IntoIterator for &'a Fields<T> {
    type Item = (&'a String, &'a T);
    type IntoIter =
        std::iter::Map<std::slice::Iter<'a, (String, T)>, fn(&'a (String, T)) -> Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter().map(pair)
    }
}

impl<T> std::ops::Index<&str> for Fields<T> {
    type Output = T;

    fn index(&self, name: &str) -> &T {
        self.get(name).expect("no index on that field")
    }
}

/// A hash, text, ordered or sparse index as a collection holds it: built,
/// or left for the documents to fill the first time a statement reads it.
/// An open leaves them unbuilt -- building them was 9 of a 31 ms open at
/// 100 000 x 128 for one `@hash` field, and a `@text` field costs 16 us a
/// document -- and a process that never reads one never pays for it. A
/// write skips an unbuilt index, since its build reads the documents as
/// they stand then; a build that fails fails every read after it the same
/// way, as the documents it could not read are still there.
pub struct Derived<T>(std::sync::OnceLock<Result<T>>);

impl<T> Derived<T> {
    /// An index as it stands: a new collection's, or one just built.
    fn new(ix: T) -> Self {
        Derived(std::sync::OnceLock::from(Ok(ix)))
    }

    fn unbuilt() -> Self {
        Derived(std::sync::OnceLock::new())
    }

    /// The index, if something has built it.
    pub fn built(&self) -> Option<&T> {
        self.0.get().and_then(|r| r.as_ref().ok())
    }

    /// The index, built by `build` if nothing has read it yet: under a
    /// read lock too, a second reader waiting for the first one's build.
    fn or_build(&self, build: impl FnOnce() -> Result<T>) -> Result<&T> {
        match self.0.get_or_init(build) {
            Ok(ix) => Ok(ix),
            Err(e) => Err(e.clone()),
        }
    }

    /// The index for a write to keep up to date, none while it is unbuilt.
    fn get_mut(&mut self) -> Option<&mut T> {
        self.0.get_mut().and_then(|r| r.as_mut().ok())
    }
}

/// The documents' vectors in the field `name`, with their ids.
fn vectors_of(docs: &[Document], name: &str) -> Vec<(DocId, Vec<f32>)> {
    docs.iter()
        .filter_map(|d| match d.get(name) {
            Some(Value::Vector(v)) => Some((d.id, v.clone())),
            _ => None,
        })
        .collect()
}

pub struct Collection {
    pub id: u32,
    pub schema: Schema,
    pub store: Store,
    /// field name -> HNSW index
    pub vectors: Fields<VectorIndex>,
    /// field name -> hash index
    pub hashes: Fields<Derived<HashIndex>>,
    /// field name -> inverted index
    pub texts: Fields<Derived<TextIndex>>,
    /// field name -> ordered index, in schema order. A `Vec` rather than a
    /// map: a collection has a handful of ordered fields, the map's code was
    /// 2.6 KB of the browser module, and a fixed order keeps the choice
    /// between two ranges the same from one run to the next.
    pub sorted: Vec<(String, Derived<SortedIndex>)>,
    /// field name -> inverted index over a sparse vector, in schema order,
    /// a `Vec` for the reason `sorted` is one.
    pub sparse: Vec<(String, Derived<SparseIndex>)>,
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
        let mut vectors = Fields::default();
        let mut hashes = Fields::default();
        let mut texts = Fields::default();
        let mut sorted = Vec::new();
        let mut sparse = Vec::new();
        for f in schema.fields.iter().chain(&schema.paths) {
            match (&f.index, &f.ty) {
                #[cfg(feature = "vector")]
                (IndexKind::Vector(spec), DataType::Vector(dim, prec)) => {
                    vectors.insert(
                        f.name.clone(),
                        VectorIndex::with_precision(*dim, *spec, *prec),
                    );
                }
                (IndexKind::Hash { .. }, _) => {
                    hashes.insert(f.name.clone(), Derived::new(HashIndex::default()));
                }
                #[cfg(feature = "text")]
                (IndexKind::Text(spec), DataType::Text) => {
                    texts.insert(f.name.clone(), Derived::new(TextIndex::new(*spec)));
                }
                #[cfg(feature = "sorted")]
                (IndexKind::Sorted { .. }, ty) if SortedIndex::supports(ty) => {
                    let ix = SortedIndex::new(ty, f.collate);
                    sorted.push((f.name.clone(), Derived::new(ix)));
                }
                #[cfg(feature = "sparse")]
                (IndexKind::Inverted, DataType::Sparse(_)) => {
                    sparse.push((f.name.clone(), Derived::new(SparseIndex::new())));
                }
                _ => {}
            }
        }
        let mut store = Store::new();
        store.set_dropped(&schema.dropped);
        Collection {
            id,
            schema,
            store,
            vectors,
            hashes,
            texts,
            sorted,
            sparse,
        }
    }

    /// Rebuilds the index structures from the schema's index definitions:
    /// the graphs empty, for `rebuild_indexes_with` to fill, and the others
    /// unbuilt, for the documents to fill when something reads them.
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
        for f in self.schema.fields.iter().chain(&self.schema.paths) {
            match (&f.index, &f.ty) {
                #[cfg(feature = "vector")]
                (IndexKind::Vector(spec), DataType::Vector(dim, prec)) => {
                    self.vectors.insert(
                        f.name.clone(),
                        VectorIndex::with_precision(*dim, *spec, *prec),
                    );
                }
                (IndexKind::Hash { .. }, _) => {
                    self.hashes.insert(f.name.clone(), Derived::unbuilt());
                }
                #[cfg(feature = "text")]
                (IndexKind::Text(_), DataType::Text) => {
                    self.texts.insert(f.name.clone(), Derived::unbuilt());
                }
                #[cfg(feature = "sorted")]
                (IndexKind::Sorted { .. }, ty) if SortedIndex::supports(ty) => {
                    self.sorted.push((f.name.clone(), Derived::unbuilt()));
                }
                #[cfg(feature = "sparse")]
                (IndexKind::Inverted, DataType::Sparse(_)) => {
                    self.sparse.push((f.name.clone(), Derived::unbuilt()));
                }
                _ => {}
            }
        }
    }

    /// The hash index on `field`, built from the documents if nothing has
    /// read it since the open.
    pub fn hash(&self, field: &str) -> Result<Option<&HashIndex>> {
        let (Some(d), Some((pos, keys))) = (self.hashes.get(field), source(&self.schema, field))
        else {
            return Ok(None);
        };
        d.or_build(|| hash_of(&self.store, pos, keys)).map(Some)
    }

    /// Takes `field`'s indexes off the collection, whichever it has.
    fn take_indexes(&mut self, field: &str) -> Taken {
        let sorted = self.sorted.iter().position(|(n, _)| n == field);
        let sparse = self.sparse.iter().position(|(n, _)| n == field);
        Taken {
            vector: self.vectors.remove(field),
            hash: self.hashes.remove(field),
            text: self.texts.remove(field),
            sorted: sorted.map(|i| self.sorted.remove(i).1),
            sparse: sparse.map(|i| self.sparse.remove(i).1),
        }
    }

    /// Puts indexes [`Self::take_indexes`] took under `field`, the schema
    /// already naming it.
    fn put_indexes(&mut self, field: &str, t: Taken) {
        if let Some(ix) = t.vector {
            self.vectors.insert(field.to_string(), ix);
        }
        if let Some(ix) = t.hash {
            self.hashes.insert(field.to_string(), ix);
        }
        if let Some(ix) = t.text {
            self.texts.insert(field.to_string(), ix);
        }
        if let Some(ix) = t.sorted {
            in_schema_order(&mut self.sorted, &self.schema, field, ix);
        }
        if let Some(ix) = t.sparse {
            in_schema_order(&mut self.sparse, &self.schema, field, ix);
        }
    }

    /// The indexes on paths made to match the schema's paths: one it names
    /// and the collection lacks made unbuilt, for the first read to build
    /// from the documents, as an open leaves it; one it names no more let
    /// go. What an alter of a json field leaves, and a block put back --
    /// moved by name as a field's are, the moves were 1.4 KB of the browser
    /// module for what a read builds again.
    fn fit_paths(&mut self) {
        let schema = &self.schema;
        let gone: Vec<String> = (self.hashes.keys())
            .chain(self.sorted.iter().map(|(n, _)| n))
            .filter(|n| n.contains('.') && schema.path(n).is_none())
            .cloned()
            .collect();
        for n in gone {
            drop(self.take_indexes(&n));
        }
        for i in 0..self.schema.paths.len() {
            let p = &self.schema.paths[i];
            match p.index {
                IndexKind::Hash { .. } if !self.hashes.contains_key(&p.name) => {
                    self.hashes.insert(p.name.clone(), Derived::unbuilt());
                }
                #[cfg(feature = "sorted")]
                IndexKind::Sorted { .. } if !self.sorted.iter().any(|(n, _)| *n == p.name) => {
                    let name = p.name.clone();
                    in_schema_order(&mut self.sorted, &self.schema, &name, Derived::unbuilt());
                }
                _ => {}
            }
        }
    }

    /// The fields changed as `ch` says -- the schema after it in place, and
    /// the store reading by its places -- and the indexes of a field
    /// dropped taken off, those of one renamed moved to its name: what an
    /// `alter collection` does, and a replica's apply of one. A field added
    /// has its index built by the caller, after this, which cannot fail.
    fn alter_fields(&mut self, ch: &FieldChange) -> Taken {
        let taken = match ch.op {
            FIELD_ADD | FIELD_TTL => Taken::default(),
            _ => self.take_indexes(&ch.field),
        };
        self.schema = ch.schema.clone();
        self.store.set_dropped(&self.schema.dropped);
        // A json field's indexes on paths go with it, by the paths the
        // schema after names.
        self.fit_paths();
        if ch.op == FIELD_RENAME {
            self.put_indexes(&ch.to, taken);
            return Taken::default();
        }
        taken
    }

    /// The documents written again without the places of dropped fields,
    /// which `compact` takes out: the bytes of every value kept copied as
    /// they stand, into a fresh store -- in memory until the rewrite that
    /// follows points it at the new file.
    fn strip_dropped(&mut self) -> Result<()> {
        let mut fresh = Store::new();
        fresh.raise_next_id(self.store.next_id());
        fresh.reserve(self.store.len());
        for id in self.store.iter_ids() {
            let payload = self.store.raw(id)?.unwrap_or_default();
            fresh.append(OP_PUT, id, &self.schema.without_dropped(payload)?);
        }
        self.store = fresh;
        self.schema.dropped.clear();
        Ok(())
    }

    /// The refusal of `doc` where an `@unique` field of it holds a value
    /// another document holds: asked by `put`, `insert` and `set` before
    /// anything is written, against the bucket the value would be filed
    /// under -- the block's own earlier writes are in it, as a write keeps
    /// a built index up, and the first ask after an open builds it from
    /// the documents, so the answer is exact. `null` is no value, and a
    /// document keeping its own value is not a second one. A field without
    /// `@unique` costs the look at its index kind.
    fn unique_clash(&self, doc: &Document) -> Result<()> {
        for f in self.schema.fields.iter().chain(&self.schema.paths) {
            if !f.index.is_unique() {
                continue;
            }
            let Some(v) = doc.at(&f.name).filter(|v| !matches!(v, Value::Null)) else {
                continue;
            };
            let Some(ix) = self.hash(&f.name)? else {
                continue;
            };
            let bucket = ix.get(&hash_key(v));
            if let Some(other) = bucket.and_then(|b| b.iter().find(|&&d| d != doc.id)) {
                return Err(Error::Duplicate(format!(
                    "`{}.{}` is unique, and document {other} holds {} already",
                    self.schema.name,
                    f.name,
                    crate::json::to_string(v)
                )));
            }
        }
        Ok(())
    }

    /// The full-text index on `field`, built as [`Self::hash`] is.
    pub fn text(&self, field: &str) -> Result<Option<&TextIndex>> {
        let (Some(d), Some(pos)) = (self.texts.get(field), self.schema.field_pos(field)) else {
            return Ok(None);
        };
        let IndexKind::Text(spec) = self.schema.fields[pos].index else {
            return Ok(None);
        };
        d.or_build(|| text_of(&self.store, pos, spec)).map(Some)
    }

    /// The inverted index over the sparse vectors of `field`, built as
    /// [`Self::hash`] is.
    pub fn sparse_index(&self, field: &str) -> Result<Option<&SparseIndex>> {
        let Some((_, d)) = self.sparse.iter().find(|(name, _)| name == field) else {
            return Ok(None);
        };
        let Some(pos) = self.schema.field_pos(field) else {
            return Ok(None);
        };
        d.or_build(|| sparse_of(&self.store, pos)).map(Some)
    }

    /// The ordered index on `field`, built as [`Self::hash`] is.
    pub fn sorted_index(&self, field: &str) -> Result<Option<&SortedIndex>> {
        let Some((_, d)) = self.sorted.iter().find(|(name, _)| name == field) else {
            return Ok(None);
        };
        let (Some((pos, keys)), Some(fd)) =
            (source(&self.schema, field), self.schema.indexed(field))
        else {
            return Ok(None);
        };
        d.or_build(|| sorted_of(&self.store, fd, pos, keys))
            .map(Some)
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
        let kept = |name: &str| old.is_some_and(|o| same(o.at(name), doc.at(name)));
        for (name, ix) in self.hashes.iter_mut() {
            let Some(ix) = ix.get_mut() else { continue };
            if let Some(v) = doc.at(name).filter(|_| !kept(name)) {
                ix.add(hash_key(v), doc.id);
            }
        }
        for (name, ix) in self.texts.iter_mut() {
            let Some(ix) = ix.get_mut() else { continue };
            if let Some(Value::Text(t)) = doc.get(name).filter(|_| !kept(name)) {
                ix.insert(doc.id, t);
            }
        }
        for (name, ix) in self.sorted.iter_mut() {
            let Some(ix) = ix.get_mut() else { continue };
            if !kept(name) {
                ix.insert(doc.id, doc.at(name));
            }
        }
        for (name, ix) in self.sparse.iter_mut() {
            let Some(ix) = ix.get_mut() else { continue };
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
            let items = vectors_of(docs, name);
            if !items.is_empty() {
                ix.insert_batch(&items);
            }
        }
    }

    /// [`Self::index_vectors_batch`] for a `put` in a block that defers:
    /// every vector left waiting, and counted into `waiting` for
    /// [`Database::link_waiting`] to link.
    fn defer_vectors_batch(&mut self, docs: &[Document], waiting: &mut Vec<(u32, String, usize)>) {
        for (name, ix) in self.vectors.iter_mut() {
            let items = vectors_of(docs, name);
            let n = match items.is_empty() {
                true => 0,
                false => ix.defer_batch(&items),
            };
            if n == 0 {
                continue;
            }
            match waiting
                .iter_mut()
                .find(|(c, f, _)| *c == self.id && f == name)
            {
                Some(w) => w.2 += n,
                None => waiting.push((self.id, name.clone(), n)),
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
        let kept = |name: &str| new.is_some_and(|n| same(doc.at(name), n.at(name)));
        for (name, ix) in self.vectors.iter_mut() {
            let replaced = new.is_some_and(|n| matches!(n.get(name), Some(Value::Vector(_))));
            if doc.get(name).is_some() && !replaced {
                ix.remove(doc.id);
            }
        }
        for (name, ix) in self.hashes.iter_mut() {
            let Some(ix) = ix.get_mut() else { continue };
            if let Some(v) = doc.at(name).filter(|_| !kept(name)) {
                ix.remove(&hash_key(v), doc.id);
            }
        }
        // Every caller reads the *stored* document before unindexing, so the
        // terms here are the ones that went in.
        for (name, ix) in self.texts.iter_mut() {
            let Some(ix) = ix.get_mut() else { continue };
            if let Some(Value::Text(t)) = doc.get(name).filter(|_| !kept(name)) {
                ix.remove(doc.id, t);
            }
        }
        for (name, ix) in self.sorted.iter_mut() {
            let Some(ix) = ix.get_mut() else { continue };
            if !kept(name) {
                ix.remove(doc.id, doc.at(name));
            }
        }
        for (name, ix) in self.sparse.iter_mut() {
            let Some(ix) = ix.get_mut() else { continue };
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
            // The ones built: counted by building, every metrics scrape
            // would build them all.
            text_indexes: self
                .texts
                .iter()
                .filter_map(|(k, t)| {
                    let t = t.built()?;
                    Some(TextIndexStats {
                        field: k.clone(),
                        count: t.len(),
                        terms: t.terms(),
                        postings: t.postings_count(),
                        bytes: t.memory_bytes(),
                    })
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
///
/// A whole float an `f64` holds exactly as an int, up to 2^53, is filed
/// under the int: a json path holds `3` in one document and `3.0` in
/// another, which the scan finds equal and a bucket each would have split.
/// A typed field never meets the two, its values and its literals coerced
/// to its one type first, so it files and finds as it did.
fn hash_key(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    match v {
        Value::Float(f) if f.fract() == 0.0 && f.abs() <= EXACT_INT => {
            crate::codec::encode_value(&mut out, &Value::Int(*f as i64))
        }
        // Adding +0.0 turns -0.0 into 0.0 and leaves every other float alone.
        Value::Float(f) => crate::codec::encode_value(&mut out, &Value::Float(f + 0.0)),
        _ => crate::codec::encode_value(&mut out, v),
    }
    out
}

/// 2^53: the ints an `f64` holds every one of.
const EXACT_INT: f64 = 9_007_199_254_740_992.0;

/// The bucket key `v` finds in the hash index on `field`: the value coerced
/// to the field's type, as the write path filed it, or -- over a `json`
/// field or a path, whose values have no type -- the value itself where
/// the bucket is exactly what `=` finds: null, a boolean, text, and a
/// number an `f64` holds exactly. `None` sends the comparison to the scan:
/// an int past 2^53 equals floats near it that no bucket of its own holds,
/// and a timestamp equals the text it parses from.
fn lookup_key(schema: &Schema, field: &str, v: &Value) -> Option<Vec<u8>> {
    match &schema.indexed(field)?.ty {
        DataType::Json => match v {
            Value::Null | Value::Bool(_) | Value::Text(_) => Some(hash_key(v)),
            Value::Int(i) if i.unsigned_abs() <= 1 << 53 => Some(hash_key(v)),
            Value::Float(f) if f.abs() <= EXACT_INT => Some(hash_key(v)),
            _ => None,
        },
        ty => v.clone().coerce(ty).ok().map(|k| hash_key(&k)),
    }
}

/// Where a query reads `name`: the position of its field, and for a path
/// the keys past it. `None` for a name the schema has no field for, or a
/// path it cannot read ([`Schema::path_of`] says why).
fn source<'a>(schema: &Schema, name: &'a str) -> Option<(usize, Option<&'a str>)> {
    match schema.path_of(name) {
        Ok(Some((pos, keys))) => Some((pos, Some(keys))),
        Ok(None) => schema.field_pos(name).map(|p| (p, None)),
        Err(_) => None,
    }
}

/// [`source`], refused naming what is wrong: a field not there, or a path
/// into one that is not `json`. `owner` goes before the name, as
/// `reviews.` does for a `lookup`'s.
fn source_or_err<'a>(
    schema: &Schema,
    name: &'a str,
    owner: &str,
) -> Result<(usize, Option<&'a str>)> {
    match schema.path_of(name)? {
        Some((pos, keys)) => Ok((pos, Some(keys))),
        None => schema
            .field_pos(name)
            .map(|p| (p, None))
            .ok_or_else(|| Error::NotFound(format!("field `{owner}{name}`"))),
    }
}

/// The value of document `id` at a [`source`], `null` where it has none.
fn read_source(store: &Store, id: DocId, (pos, keys): (usize, Option<&str>)) -> Result<Value> {
    Ok(match keys {
        None => store.read_field(id, pos)?,
        Some(keys) => store.read_path(id, pos, keys)?,
    }
    .unwrap_or(Value::Null))
}

/// The feature `kind` needs when this build was made without it
/// (`Cargo.toml`): a file declaring such an index opens all the same, the
/// index unbuilt, and what needs it is refused.
fn missing_feature(kind: &IndexKind) -> Option<&'static str> {
    match kind {
        IndexKind::Vector(_) if !cfg!(feature = "vector") => Some("vector"),
        IndexKind::Text(_) if !cfg!(feature = "text") => Some("text"),
        IndexKind::Inverted if !cfg!(feature = "sparse") => Some("sparse"),
        IndexKind::Sorted { .. } if !cfg!(feature = "sorted") => Some("sorted"),
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
    /// The level's `where`, bound the first time a row is tested.
    test: std::cell::OnceCell<Filter<'a>>,
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
        let test = self.test.get_or_init(|| Filter::new(self.child, f, ctx));
        test.matches(id, ctx)
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
        match child.hash(field)? {
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

/// Lazy field access over the store: a field is read when the filter asks
/// for it, and read again when asked again. Each value read was once kept
/// in a list the row allocated, and handed out as a copy -- a string twice
/// allocated -- which cost more than reading a field twice: a scan of
/// 20 000 rows with a comparison and an order took 2.39 ms natively and
/// 3.51 in the browser module, and takes 1.93 and 2.86 without.
struct StoreRow<'a> {
    store: &'a Store,
    schema: &'a Schema,
    id: DocId,
}

impl<'a> RowAccess for StoreRow<'a> {
    fn id(&self) -> DocId {
        self.id
    }
    fn field(&mut self, name: &str) -> Result<Value> {
        let at = source_or_err(self.schema, name, "")?;
        read_source(self.store, self.id, at)
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
        self.1.path_of(name)?;
        Ok(self.0.at(name).cloned().unwrap_or(Value::Null))
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
    /// On a node a router's lease lets write, whether the lease still does
    /// ([`Self::set_fence`]): asked as a block lands, and as a schema change
    /// or a maintenance starts, so no write lands after the lease lapsed --
    /// the router promotes the tenant elsewhere once it knows the lease has.
    #[cfg(not(target_arch = "wasm32"))]
    fence: Option<Fence>,
    /// The block of writes running, if one is: its writes are held back
    /// from the sink and the feed until it lands, and put back if it does
    /// not ([`Self::execute_block`]).
    block: Option<Block>,
    /// The last block's buffers, for the next one ([`Block::cleared`]).
    spare: Block,
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
    /// Where the records appended since the file was read or rewritten hold
    /// each collection's frames, and how many bytes they are: what
    /// [`Database::hand_over`] points the stores at, once they amount to
    /// `handover_at`.
    #[cfg(not(target_arch = "wasm32"))]
    landed: Vec<handover::Landed>,
    #[cfg(not(target_arch = "wasm32"))]
    landed_bytes: u64,
    /// The writes the noted records hold, for [`handover::HANDOVER_DOCS`].
    #[cfg(not(target_arch = "wasm32"))]
    landed_writes: u64,
    #[cfg(not(target_arch = "wasm32"))]
    handover_at: u64,
    /// When an open block spills ([`SPILL_AT`] unless set).
    #[cfg(not(target_arch = "wasm32"))]
    spill_at: u64,
    /// Whether a rewrite beside the database is writing its side file: one
    /// at a time, since the file has one name. Only where a file is mapped,
    /// the one place a rewrite writes beside it.
    #[cfg(all(feature = "std-fs", unix, target_pointer_width = "64"))]
    beside: std::sync::atomic::AtomicBool,
    /// The time a read of a collection whose rows expire is answered at
    /// ([`Self::set_clock`]); the system's clock when `None`. The browser
    /// module has no clock and sets it before each statement, as it is
    /// handed every time; a test pins it.
    clock: Option<i64>,
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

/// A mapped database hands the documents written since its file was opened
/// over to the file once they amount to this many bytes
/// ([`Database::hand_over`]): what its segments hold at the most, beside
/// the block being written. A handover holds the write lock for as long as
/// its documents take, 1 ms for 16 MB of 768-dim ones and 2 ms of 128-dim
/// ones, where 64 MB held it 3.9 and 8.5 ms: the same work in shorter
/// pauses, and a quarter of the memory.
pub const HANDOVER_AT: u64 = 16 << 20;

/// A mapped database's open block spills the frames it holds into the file
/// once they amount to this many bytes (`Database::spill`): a block held
/// every document it wrote twice until it landed, once in its record and
/// once in the stores, and 250 000 768-dim rows in one COPY peaked 1 569 MB
/// over what they left.
pub const SPILL_AT: u64 = 16 << 20;

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
            #[cfg(not(target_arch = "wasm32"))]
            fence: None,
            block: None,
            spare: Block::default(),
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
            landed: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            landed_bytes: 0,
            #[cfg(not(target_arch = "wasm32"))]
            landed_writes: 0,
            #[cfg(not(target_arch = "wasm32"))]
            handover_at: HANDOVER_AT,
            #[cfg(not(target_arch = "wasm32"))]
            spill_at: SPILL_AT,
            #[cfg(all(feature = "std-fs", unix, target_pointer_width = "64"))]
            beside: std::sync::atomic::AtomicBool::new(false),
            clock: None,
        }
    }

    /// Answers a read of a collection whose rows expire (`@ttl`) as at
    /// `now`, in milliseconds since the epoch; `None` goes back to the
    /// system's clock. `wasm32-unknown-unknown` has none, so the browser
    /// module sets it before each statement, from `Date.now()`.
    pub fn set_clock(&mut self, now: Option<i64>) {
        self.clock = now;
    }

    /// The time a read is answered at.
    fn now(&self) -> Result<i64> {
        match self.clock {
            Some(t) => Ok(t),
            None => crate::time::now_ms(),
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
        // In a block, once it lands: a subscriber or a maintenance sees the
        // block whole, and one that does not land moves nothing.
        if let Some(b) = &mut self.block {
            b.notes.push((cid, id));
            return;
        }
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
    /// behind the horizon, or a collection written since has been dropped:
    /// everything must be re-run.
    ///
    /// Exact otherwise, blocks included: a block's writes reach the ring as
    /// it lands (`note`), so one put back -- a rollback, a failed statement
    /// in a text of several -- names nothing, and a read inside an open
    /// block sees none of its writes here yet.
    pub fn changed_collections_since(&self, since: u64) -> Option<Vec<String>> {
        let cids = self.changes.changed_collections(since)?;
        let mut out = Vec::new();
        for name in &self.order {
            if cids.contains(&self.collections[name].id) {
                out.push(name.clone());
            }
        }
        // A dropped collection has no name left to give, and was left out
        // above: a query of it has to run again and be told it is gone. So
        // a drop answers "everything", re-running queries that read other
        // collections for nothing -- harmless, and drops are rare -- where a
        // name kept for every collection ever dropped would be kept for good.
        (out.len() == cids.len()).then_some(out)
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
            if col != "id" {
                source_or_err(&c.schema, col, "")?;
            }
        }
        let ctx = EvalCtx {
            params,
            registry: &self.registry,
        };
        // A row past its time is gone for the subscriber as for any read:
        // one that changed and expired is a deletion, and the sweep's
        // delete of it later is one it was already told of.
        let alive = self.alive(collection)?;
        let shape;
        let filter = match (filter, alive) {
            (f, None) => f,
            (Some(f), Some(a)) => {
                shape = Expr::And(Box::new(f.clone()), Box::new(a));
                Some(&shape)
            }
            (None, Some(a)) => {
                shape = a;
                Some(&shape)
            }
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
                    let at = source_or_err(&c.schema, col, "")?;
                    values.push(read_source(&c.store, id, at)?);
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
                        .filter_map(Derived::built)
                        .map(HashIndex::memory_bytes)
                        .sum::<usize>()
                    + c.texts
                        .values()
                        .filter_map(Derived::built)
                        .map(|ix| ix.memory_bytes())
                        .sum::<usize>()
                    + c.sorted
                        .iter()
                        .filter_map(|(_, ix)| ix.built())
                        .map(|ix| ix.memory_bytes())
                        .sum::<usize>()
                    + c.sparse
                        .iter()
                        .filter_map(|(_, ix)| ix.built())
                        .map(|ix| ix.memory_bytes())
                        .sum::<usize>()
            })
            .sum::<usize>()
            + self.noted_bytes()
            + self.block.as_ref().map_or(0, Block::bytes)
    }

    /// What the notes of where the records since the open went take
    /// ([`Self::hand_over`]); none in the browser, which keeps none.
    fn noted_bytes(&self) -> usize {
        #[cfg(not(target_arch = "wasm32"))]
        return self.landed_bytes_held();
        #[cfg(target_arch = "wasm32")]
        0
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
        // Counter header: a placeholder now, the body's length once it is
        // written -- fixed width, so it is patched where it stands.
        let head_at = MAGIC.len() as u64;
        out.write(&image_head(self.changes.seq()))?;
        let body_at = out.at();

        if self.history.following || !self.history.lineage.is_empty() {
            out.write(&self.history.record())?;
        }

        for name in &self.order {
            let c = &self.collections[name];
            let sc = c.schema.encode();
            out.write(&record_head(REC_CREATE, c.id, sc.len()))?;
            out.write(&sc)?;

            let compact = compacting.iter().any(|n| n == name);
            let bytes = match compact {
                true => c.store.live_len(),
                false => c.store.image_len(),
            };
            // The counter comes right after the schema: the collection has to
            // exist, and its data can only carry the counter forward. Behind
            // it, the index of the data record's frames an open takes rather
            // than walking them; the browser's module keeps no writer of it.
            let mut counter = Vec::with_capacity(9);
            put_uvarint(&mut counter, c.store.next_id());
            #[cfg(not(target_arch = "wasm32"))]
            if bytes > 0 {
                c.store.image_index(compact, &mut counter);
            }
            out.write(&record_head(REC_NEXTID, c.id, counter.len()))?;
            out.write(&counter)?;
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
                ix.serialize_graph_into(false, &mut payload);
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
        let mut walk = Walk::new(bytes, MAGIC.len());
        while let Some(r) = walk.next()? {
            match r.kind {
                REC_CREATE => {
                    let schema = Schema::decode(r.body, &mut 0)?;
                    by_id.insert(r.cid, schema.name.clone());
                    fresh.insert(schema.name, Store::new());
                }
                REC_NEXTID => {
                    let next = get_uvarint(r.body, &mut 0)?;
                    if let Some(store) = by_id.get(&r.cid).and_then(|n| fresh.get_mut(n)) {
                        store.raise_next_id(next);
                    }
                }
                REC_DATA => {
                    if let Some(store) = by_id.get(&r.cid).and_then(|n| fresh.get_mut(n)) {
                        let (at, len) = (r.body_at as u64, r.body.len() as u64);
                        store.replay_mapped(&base, at, len, &mut |_| {})?;
                    }
                }
                _ => {}
            }
        }
        for (name, mut store) in fresh {
            if let Some(c) = self.collections.get_mut(&name) {
                store.set_dropped(&c.schema.dropped);
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
    /// ordered and text indexes, the graph -- is built in memory. In the
    /// browser the image a load was handed, kept rather than copied.
    pub fn load_mapped(&mut self, file: crate::store::Base) -> Result<usize> {
        let keep = file.clone();
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.mapped = true;
        }
        self.load_from((*keep).as_ref(), Some(&file))
    }

    /// With `base` the documents are read where they lie in it, and
    /// without it copied into segments.
    fn load_from(&mut self, bytes: &[u8], base: Option<&crate::store::Base>) -> Result<usize> {
        #[cfg_attr(target_arch = "wasm32", allow(unused_variables))]
        self.load_records(bytes, &mut |store, chunk_at, chunk, index, note| {
            let (at, len) = (chunk_at as u64, chunk.len() as u64);
            match base {
                #[cfg(not(target_arch = "wasm32"))]
                Some(b) if index.is_some_and(|ix| store.adopt_index(b, at, len, ix)) => Ok(0),
                Some(b) => store.replay_mapped(b, at, len, note),
                None => store.replay_noting(chunk, note),
            }
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

    /// Links the vectors the open block's `put`s left waiting, in each field
    /// where they number `at_least` or more: [`LINK_AT`] after a `put`, and
    /// all of them as the block lands.
    fn link_waiting(&mut self, at_least: usize) {
        let Some(b) = self.block.as_mut() else {
            return;
        };
        for (cid, field, n) in b.waiting.iter_mut() {
            if *n == 0 || *n < at_least {
                continue;
            }
            let waiting = std::mem::take(n);
            let Some(c) = self.collections.values_mut().find(|c| c.id == *cid) else {
                continue;
            };
            let Collection {
                schema,
                store,
                vectors,
                ..
            } = c;
            if let Some(ix) = vectors.get_mut(field) {
                let pos = schema.field_pos(field);
                ix.link_pending(waiting, &mut |doc, out| {
                    pos.is_some_and(|p| store.read_vector_into(doc, p, out).unwrap_or(false))
                });
            }
        }
    }

    /// Drops the nodes a block's `put`s left waiting that its undo made
    /// tombstones, from the newest back, as far as `waiting` counts them.
    fn forget_waiting(&mut self, waiting: &mut [(u32, String, usize)]) {
        for (cid, field, n) in waiting.iter_mut() {
            if *n == 0 {
                continue;
            }
            let ix = self
                .collections
                .values_mut()
                .find(|c| c.id == *cid)
                .and_then(|c| c.vectors.get_mut(field));
            *n -= ix.map_or(*n, |ix| ix.forget_waiting(*n));
        }
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
                // Written where it goes out, its head into room left before
                // it as a block's is: the graph copied into a payload and the
                // payload into a record were two more copies of it under the
                // read lock.
                let mut buf = vec![0; HEAD_ROOM];
                crate::codec::encode_str(&mut buf, field);
                ix.serialize_graph_into(true, &mut buf);
                let body = buf.len() - HEAD_ROOM;
                let (h, hn) = head(REC_GRAPH, c.id, body);
                buf[HEAD_ROOM - hn..HEAD_ROOM].copy_from_slice(&h[..hn]);
                let record = &buf[HEAD_ROOM - hn..];
                self.sink
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .append_deferred(record)?;
                let end =
                    self.appended.fetch_add(record.len() as u64, Relaxed) + record.len() as u64;
                let p = ix.persisted();
                p.changes.store(ix.changes(), Relaxed);
                p.at.store(end, Relaxed);
                p.node_bytes.store((body / ix.len().max(1)) as u64, Relaxed);
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
        let pos = MAGIC.len();
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
        let mut walk = Walk::new(bytes, pos);
        // The spills since the last write, by where each record and its
        // body start and the body's length: a land takes the ones it names.
        let mut spills: Vec<(usize, usize, usize)> = Vec::new();
        // Where the spills the file ends with start: nothing lands them, and
        // the file is cut there as after a torn record. One followed by any
        // other record -- a graph, the history -- stays, dead.
        let mut dead_tail: Option<usize> = None;
        // The index of a data record's frames an image writes after the
        // collection's id counter, for the record right after it alone.
        let mut frames: Option<(u32, &[u8])> = None;
        while let Some(r) = walk.next()? {
            let tail = r.at >= body_end;
            if matches!(
                r.kind,
                REC_CREATE | REC_DROP | REC_ALTER | REC_FIELDS | REC_DATA | REC_BLOCK
            ) {
                spills.clear();
            }
            dead_tail = match r.kind {
                REC_SPILL => dead_tail.or(Some(r.at)),
                _ => None,
            };
            let index = frames
                .take()
                .filter(|(cid, _)| r.kind == REC_DATA && *cid == r.cid && !tail)
                .map(|(_, ix)| ix);
            match r.kind {
                REC_CREATE | REC_DROP | REC_ALTER | REC_FIELDS => {
                    seq_seen += tail as u64;
                    self.load_schema(r.kind, r.cid, r.body, &mut by_id, &mut restored)?;
                }
                REC_DATA => {
                    let written = self.load_data(
                        r.cid,
                        bytes,
                        r.body_at,
                        r.body.len(),
                        &by_id,
                        &restored,
                        &mut touched,
                        index,
                        replay,
                    )?;
                    if tail {
                        seq_seen += written;
                    }
                }
                REC_BLOCK => {
                    let frames = self.load_block(
                        bytes,
                        r.body,
                        &mut by_id,
                        &mut restored,
                        &mut touched,
                        replay,
                    )?;
                    if tail {
                        seq_seen += frames;
                    }
                }
                // Held until a land names it; a land never comes for one a
                // rollback or a crash left.
                REC_SPILL => spills.push((r.at, r.body_at, r.body.len())),
                REC_LAND => {
                    let (named, rest) = land_parts(r.body)?;
                    let mut frames = 0;
                    for (at, len) in named {
                        let Some(&(_, body_at, _)) = spills
                            .iter()
                            .find(|s| s.1 as u64 == at && s.2 as u64 == len)
                        else {
                            return Err(Error::Corrupt(
                                "a land names a spill the file does not hold".into(),
                            ));
                        };
                        frames += self.load_block(
                            bytes,
                            &bytes[body_at..body_at + len as usize],
                            &mut by_id,
                            &mut restored,
                            &mut touched,
                            replay,
                        )?;
                    }
                    frames += self.load_block(
                        bytes,
                        rest,
                        &mut by_id,
                        &mut restored,
                        &mut touched,
                        replay,
                    )?;
                    if tail {
                        seq_seen += frames;
                    }
                    spills.clear();
                }
                REC_NEXTID => {
                    // Not a write but the counter itself: it does not move `seq`.
                    let mut p = 0;
                    let next = get_uvarint(r.body, &mut p)?;
                    if let Some(name) = by_id.get(&r.cid) {
                        if let Some(c) = self.collections.get_mut(name) {
                            c.store.raise_next_id(next);
                        }
                    }
                    if let Some(index) = r.body.get(p..).filter(|ix| !ix.is_empty()) {
                        frames = Some((r.cid, index));
                    }
                }
                REC_GRAPH => {
                    let (cid, at, chunk) = (r.cid, r.at, r.body);
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
                                p.at.store(walk.pos as u64, Relaxed);
                                p.node_bytes
                                    .store((chunk.len() / ix.len().max(1)) as u64, Relaxed);
                            }
                            forget(&mut restored, &name, &|f| f != field);
                            restored.push((name, field, from));
                        }
                    }
                }
                // Not a write: it does not move `seq`.
                REC_HISTORY => self.history = History::decode(r.body)?,
                REC_SEQ => {
                    let mut w = [0u8; 8];
                    w.copy_from_slice(&r.body[..8]);
                    seq_base = u64::from_le_bytes(w);
                    w.copy_from_slice(&r.body[8..16]);
                    body_end = walk.pos.saturating_add(u64::from_le_bytes(w) as usize);
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
        if walk.torn {
            // A crash in the middle of an append leaves the last record cut
            // short, and only past the image: an image is written beside the
            // file and renamed over it whole. One cut short inside it is a
            // damaged or truncated file, and cutting the file there -- as a
            // torn tail is cut -- destroyed every record after it, intact
            // ones included.
            if walk.pos < body_end {
                return Err(Error::Corrupt(
                    "a record of the checkpoint image runs past the end of the file".into(),
                ));
            }
            whole = walk.pos;
        }
        if let Some(at) = dead_tail {
            whole = whole.min(at);
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
        #[cfg(not(target_arch = "wasm32"))]
        self.forget_landed();
        self.defer_links = false;
        Ok(whole)
    }

    /// A schema change of the file -- a record of its own, or one of a
    /// block's -- made again: a collection made or dropped, or an index
    /// added, which is built with the rest once the file is read.
    fn load_schema(
        &mut self,
        kind: u8,
        cid: u32,
        body: &[u8],
        by_id: &mut HashMap<u32, String>,
        restored: &mut Vec<(String, String, usize)>,
    ) -> Result<()> {
        if kind == REC_DROP {
            if let Some(name) = by_id.remove(&cid) {
                self.collections.remove(&name);
                self.order.retain(|n| n != &name);
                forget(restored, &name, &|_| false);
            }
            return Ok(());
        }
        if kind == REC_FIELDS {
            let ch = FieldChange::decode(body)?;
            let Some(name) = by_id.get(&cid) else {
                return Ok(());
            };
            let Some(c) = self.collections.get_mut(name) else {
                return Ok(());
            };
            // The graphs restored before it stay, under the name their
            // field has after it -- the documents are the same ones -- and
            // a dropped field's goes with it.
            let mut kept = Vec::new();
            for (n, f, _) in restored.iter_mut() {
                if n != name || (ch.op == FIELD_DROP && *f == ch.field) {
                    continue;
                }
                let after = match ch.op == FIELD_RENAME && *f == ch.field {
                    true => ch.to.clone(),
                    false => f.clone(),
                };
                if let Some(ix) = c.vectors.remove(f.as_str()) {
                    kept.push((after.clone(), ix));
                }
                *f = after;
            }
            c.schema = ch.schema;
            c.store.set_dropped(&c.schema.dropped);
            c.reset_index_structures();
            forget(restored, name, &|f| kept.iter().any(|(k, _)| k == f));
            for (f, ix) in kept {
                c.vectors.insert(f, ix);
            }
            return Ok(());
        }
        let schema = Schema::decode(body, &mut 0)?;
        if kind == REC_CREATE {
            // Made again under its name: no graph restored before is this
            // collection's.
            forget(restored, &schema.name, &|_| false);
            by_id.insert(cid, schema.name.clone());
            self.order.retain(|n| n != &schema.name);
            self.order.push(schema.name.clone());
            self.collections
                .insert(schema.name.clone(), Collection::new(cid, schema));
            self.next_coll_id = self.next_coll_id.max(cid + 1);
            return Ok(());
        }
        let Some(name) = by_id.get(&cid) else {
            return Ok(());
        };
        let Some(c) = self.collections.get_mut(name) else {
            return Ok(());
        };
        // The field layout must not have changed: stored documents are
        // encoded positionally.
        let same_layout = c.schema.fields.len() == schema.fields.len()
            && c.schema.dropped == schema.dropped
            && c.schema
                .fields
                .iter()
                .zip(&schema.fields)
                .all(|(a, b)| a.name == b.name && a.ty == b.ty);
        if same_layout {
            // An index added leaves the graphs restored before it, as a
            // replica leaves them: the documents are the same ones. Not in
            // the browser, whose schemas are declared with their
            // collections: 767 bytes of its module for a rebuild it would
            // rarely save.
            let mut kept = Vec::new();
            for (n, f, _) in restored.iter().filter(|_| !cfg!(target_arch = "wasm32")) {
                let same = schema.field_pos(f).is_some_and(|p| {
                    c.schema.fields.get(p).map(|x| &x.index) == Some(&schema.fields[p].index)
                });
                if n == name && same {
                    if let Some(ix) = c.vectors.remove(f) {
                        kept.push((f.clone(), ix));
                    }
                }
            }
            c.schema = schema;
            c.reset_index_structures();
            forget(restored, name, &|f| kept.iter().any(|(k, _)| k == f));
            for (f, ix) in kept {
                c.vectors.insert(f, ix);
            }
        }
        Ok(())
    }

    /// A block record's body loaded -- its data records and its schema
    /// changes, in order -- as a block, a spill or a land holds them;
    /// returns its writes.
    #[allow(clippy::too_many_arguments)]
    fn load_block(
        &mut self,
        bytes: &[u8],
        body: &[u8],
        by_id: &mut HashMap<u32, String>,
        restored: &mut Vec<(String, String, usize)>,
        touched: &mut Vec<(String, Vec<DocId>)>,
        replay: &mut Replay<'_>,
    ) -> Result<u64> {
        let mut frames = 0;
        each_inner(body, &mut |kind, cid, inner| {
            if kind != REC_DATA {
                frames += 1;
                return self.load_schema(kind, cid, inner, by_id, restored);
            }
            // Where the record's frames are in the file, which a mapped
            // store reads them from.
            let at = inner.as_ptr() as usize - bytes.as_ptr() as usize;
            frames += self.load_data(
                cid,
                bytes,
                at,
                inner.len(),
                by_id,
                restored,
                touched,
                None,
                replay,
            )?;
            Ok(())
        })?;
        Ok(frames)
    }

    /// A data record's frames, `len` bytes at `at` in the file, into their
    /// collection's store -- noting each id for a graph restored before it,
    /// which takes the writes after it. How many frames it held.
    #[allow(clippy::too_many_arguments)]
    fn load_data(
        &mut self,
        cid: u32,
        bytes: &[u8],
        at: usize,
        len: usize,
        by_id: &HashMap<u32, String>,
        restored: &[(String, String, usize)],
        touched: &mut Vec<(String, Vec<DocId>)>,
        index: Option<&[u8]>,
        replay: &mut Replay<'_>,
    ) -> Result<u64> {
        let name = by_id
            .get(&cid)
            .cloned()
            .ok_or_else(|| Error::Corrupt(format!("unknown collection {cid}")))?;
        let chunk = &bytes[at..at + len];
        let c = self.collections.get_mut(&name).unwrap();
        let frames = if restored.iter().any(|(n, _, _)| *n == name) {
            let i = match touched.iter().position(|(n, _)| *n == name) {
                Some(i) => i,
                None => {
                    touched.push((name, Vec::new()));
                    touched.len() - 1
                }
            };
            let ids = &mut touched[i].1;
            replay(&mut c.store, at, chunk, None, &mut |id| ids.push(id))?
        } else {
            replay(&mut c.store, at, chunk, index, &mut |_| {})?
        };
        Ok(frames as u64)
    }

    /// Restores a persisted graph against the documents as they stand where
    /// its record is, keeping it only if it describes them exactly: every
    /// live node's document holds a vector, and no two nodes share one --
    /// `restore_graph` checks both -- and every document holding one has a
    /// node. The last used to be a comparison with the number of documents,
    /// so a single document without a vector threw the graph away on every
    /// open; it is still the answer when the counts agree, since the nodes
    /// are then every document, and only a collection holding documents
    /// without a vector reads each one's field to count them: 5.4 ms of a
    /// 36 ms open at 100 000 x 128.
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
        if ix.len() != store.len() {
            let mut with_vector = 0;
            for id in store.iter_ids() {
                with_vector += store.has_vector(id, pos)? as usize;
            }
            if ix.len() != with_vector {
                return Ok(false);
            }
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
            // The hash, text, ordered and sparse indexes are left for the
            // documents to fill when a statement first reads each
            // (`Derived`), but an ordered index over a collated field: in the
            // browser a comparison meeting a script whose chunk it has not
            // been handed notes it, and the load is refused and run again
            // with the chunk (`collate::refuse`). Built by a read, the index
            // would keep the order it had without the chunk.
            for d in c.hashes.values_mut() {
                *d = Derived::unbuilt();
            }
            for d in c.texts.values_mut() {
                *d = Derived::unbuilt();
            }
            for (_, d) in c.sparse.iter_mut() {
                *d = Derived::unbuilt();
            }
            let collated = |f: &str| c.schema.field(f).is_some_and(|f| f.collate.is_some());
            for (f, d) in c.sorted.iter_mut() {
                if !collated(f) {
                    *d = Derived::unbuilt();
                }
            }
            if fields.iter().all(|f| kept(f)) && !c.sorted.iter().any(|(f, _)| collated(f)) {
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
                sorted,
                ..
            } = c;
            // Each index beside its field's position and the rows it is
            // built from. Pushed in loops: collected, the lists were 2.5 KB
            // of the browser module.
            let mut sorted_ix = Vec::new();
            for (f, ix) in sorted.iter_mut() {
                if let Some(p) = schema
                    .field_pos(f)
                    .filter(|&p| schema.fields[p].collate.is_some())
                {
                    sorted_ix.push((p, ix, Vec::new()));
                }
            }
            let mut vector_ix = Vec::new();
            for (f, ix) in vectors.iter_mut() {
                if let Some(p) = schema.field_pos(f).filter(|_| !kept(f)) {
                    vector_ix.push((p, ix, Vec::new()));
                }
            }
            // In field order, as `read_fields` wants them; walked rather than
            // sorted, since a sort was 2 KB of the browser module.
            let mut positions = Vec::new();
            for p in 0..schema.fields.len() {
                if sorted_ix.iter().any(|(q, ..)| *q == p)
                    || vector_ix.iter().any(|(q, ..)| *q == p)
                {
                    positions.push(p);
                }
            }
            let slot = |p: usize| positions.iter().position(|&q| q == p).unwrap_or(0);
            // Pushed in a loop: collected, a `map` over the positions was
            // 0.3 KB of the browser module.
            let mut places = Vec::with_capacity(positions.len());
            for &p in &positions {
                places.push(schema.place(p));
            }
            // One field has one index, so each value is taken by one of them.
            let mut vals = Vec::with_capacity(positions.len());
            for id in ids {
                if !store.read_fields(id, &places, &mut vals)? {
                    continue;
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
            }
            // Each ordered index is sorted once from its keys rather than
            // inserted row by row.
            #[cfg(feature = "sorted")]
            for (p, ix, rows) in sorted_ix.iter_mut() {
                **ix = Derived::new(SortedIndex::build(
                    &schema.fields[*p].ty,
                    schema.fields[*p].collate,
                    &mut std::mem::take(rows).into_iter(),
                ));
            }
            for (_, ix, rows) in vector_ix.iter_mut() {
                match later {
                    true => {
                        ix.defer_batch(rows);
                    }
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
        // Its image would hold them: a block's writes land as its record,
        // or not at all.
        if self.in_block() {
            return Err(Error::Query(
                "a block of writes is open: a checkpoint waits for it to end".into(),
            ));
        }
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
        #[cfg(not(target_arch = "wasm32"))]
        self.forget_landed();
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
        // In a block, held back: the block lands as one record, or none.
        if let Some(b) = &mut self.block {
            b.frames.extend_from_slice(payload);
            b.heads.push((rec, cid, b.frames.len()));
            return Ok(());
        }
        let frame = framed(rec, cid, payload);
        let seq = self.changes.seq() + 1;
        let r = self.sink_mut().record(seq, &frame);
        self.storage(r)?;
        *self.appended.get_mut() += frame.len() as u64;
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
        #[cfg(not(target_arch = "wasm32"))]
        self.hand_over_when_due();
        let after = self.changes.seq();
        if after != before {
            if let Some(w) = &self.watcher {
                w.notify(after);
            }
        }
        applied
    }

    /// The writes `records` hold -- what a primary's feed carries, the
    /// first of them numbered `first` -- one at a time to `each`, numbered
    /// as the change counter numbered them, until `each` says to stop.
    /// Returns the number of the last write handed over, `first - 1` for
    /// none. A document is read by the schema of its collection as this
    /// database knows it, or as a create among the records made it; a
    /// collection known to neither -- made before them and dropped since --
    /// has its writes handed over without their documents. What a record
    /// holds that is no write (a graph, the history) is no change: the feed
    /// sends none of it.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn changes_in(
        &self,
        records: &[u8],
        first: u64,
        each: &mut dyn FnMut(Change) -> bool,
    ) -> Result<u64> {
        let mut d = ChangeWalk {
            known: self
                .collections
                .values()
                .map(|c| (c.id, c.schema.clone()))
                .collect(),
            seq: first.saturating_sub(1),
            go: true,
            each,
        };
        let mut pos = 0;
        while pos < records.len() && d.go {
            let r = record_at(records, &mut pos)?;
            match r.kind {
                REC_CREATE | REC_DROP | REC_ALTER | REC_FIELDS | REC_DATA => {
                    d.record(r.kind, r.cid, r.body)?
                }
                REC_BLOCK => each_inner(r.body, &mut |kind, cid, inner| match d.go {
                    true => d.record(kind, cid, inner),
                    false => Ok(()),
                })?,
                _ => {}
            }
        }
        Ok(d.seq)
    }

    fn apply_records(&mut self, bytes: &[u8], batch: &mut VectorBatch) -> Result<usize> {
        let mut n = 0;
        let mut pos = 0;
        let mut notes: Vec<(u32, DocId)> = Vec::new();
        while pos < bytes.len() {
            let Rec {
                kind: rec,
                cid,
                at: start,
                body,
                ..
            } = record_at(bytes, &mut pos)?;

            // The graph takes a batch as the graph stands before it: a
            // schema change needs it as it stands after, and ends it.
            if rec != REC_DATA && rec != REC_BLOCK && !batch.docs.is_empty() {
                self.index_batch(batch);
            }
            match rec {
                REC_CREATE | REC_DROP | REC_ALTER | REC_FIELDS => {
                    self.apply_schema(rec, cid, body, &mut notes)?
                }
                REC_DATA => self.apply_frames(cid, body, batch, &mut notes)?,
                // A block's records, whole: they came as one, and reach this
                // database's file as one.
                REC_BLOCK => each_inner(body, &mut |kind, cid, inner| {
                    if kind == REC_DATA {
                        return self.apply_frames(cid, inner, batch, &mut notes);
                    }
                    if !batch.docs.is_empty() {
                        self.index_batch(batch);
                    }
                    self.apply_schema(kind, cid, inner, &mut notes)
                })?,
                other => {
                    return Err(Error::Corrupt(format!(
                        "record kind {other} is not a write"
                    )))
                }
            }
            // Numbered as its last write, as the primary numbered it.
            let seq = self.changes.seq() + notes.len() as u64;
            let r = self.sink_mut().record(seq, &bytes[start..pos]);
            self.storage(r)?;
            // The frames went into the stores as the primary wrote them,
            // and the record into the file as it came.
            #[cfg(not(target_arch = "wasm32"))]
            if self.mapped {
                let at = *self.appended.get_mut();
                handover::note_record(
                    &mut self.landed,
                    &mut self.landed_bytes,
                    at,
                    &bytes[start..pos],
                );
                self.landed_writes += notes.len() as u64;
            }
            *self.appended.get_mut() += (pos - start) as u64;
            self.dirty = true;
            for (cid, id) in notes.drain(..) {
                self.note(cid, id);
            }
            n += 1;
        }
        Ok(n)
    }

    /// A schema change from a primary -- a record of its own, or one of a
    /// block's -- made here as it was there, and noted in `notes`.
    fn apply_schema(
        &mut self,
        kind: u8,
        cid: u32,
        body: &[u8],
        notes: &mut Vec<(u32, DocId)>,
    ) -> Result<()> {
        notes.push((cid, SCHEMA_MARK));
        if kind == REC_DROP {
            let name = self.named(cid).ok_or_else(|| missing(cid))?;
            self.collections.remove(&name);
            self.order.retain(|n| *n != name);
            return Ok(());
        }
        // A field added, dropped or renamed, as the primary did it: the
        // primary checked the change, and the replica takes it.
        if kind == REC_FIELDS {
            let ch = FieldChange::decode(body)?;
            let name = self.named(cid).ok_or_else(|| missing(cid))?;
            let c = self.collections.get_mut(&name).unwrap();
            c.alter_fields(&ch);
            if let (FIELD_ADD, Some(pos)) = (ch.op, c.schema.field_pos(&ch.field)) {
                build_index(c, pos)?;
            }
            return Ok(());
        }
        let schema = Schema::decode(body, &mut 0)?;
        if kind == REC_CREATE {
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
            return Ok(());
        }
        let name = self.named(cid).ok_or_else(|| missing(cid))?;
        let c = self.collections.get_mut(&name).unwrap();
        let same_layout = c.schema.fields.len() == schema.fields.len()
            && c.schema.dropped == schema.dropped
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
        // `create index` adds one -- on a field, or on a path -- and
        // anything else rebuilds them all.
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
            // A path's, built by the first read.
            c.fit_paths();
        } else {
            c.reset_index_structures();
            for i in 0..c.schema.fields.len() {
                build_index(c, i)?;
            }
        }
        Ok(())
    }

    /// A data record's frames into collection `cid`, each through the index
    /// upkeep the write path does -- vectors into `batch`, which each frame
    /// that needs the graph as it stands after the ones before ends -- and
    /// each noted in `notes`.
    fn apply_frames(
        &mut self,
        cid: u32,
        body: &[u8],
        batch: &mut VectorBatch,
        notes: &mut Vec<(u32, DocId)>,
    ) -> Result<()> {
        let cut = || Error::Corrupt("a write record cut short".into());
        let name = self.named(cid).ok_or_else(|| missing(cid))?;
        let mut p = 0;
        while p < body.len() {
            let op = body[p];
            p += 1;
            let id = get_uvarint(body, &mut p)?;
            let plen = get_uvarint(body, &mut p)? as usize;
            let payload = body.get(p..p + plen).ok_or_else(cut)?;
            p += plen;
            // Another collection's, or a document the batch holds: the
            // graph has to take the batch first.
            if !batch.docs.is_empty() && (cid != batch.cid || batch.ids.contains(&id)) {
                self.index_batch(batch);
            }
            let c = self.collections.get_mut(&name).unwrap();
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
            notes.push((cid, id));
        }
        Ok(())
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
        #[cfg(not(target_arch = "wasm32"))]
        self.forget_landed();
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

    // ---------------------------------------------------------------- blocks

    /// Remembers, for a block that does not land, where `id`'s record was
    /// before this write -- and where its collection's store stood before
    /// the block's first write to it.
    fn remember(
        &mut self,
        cid: u32,
        id: DocId,
        was: Option<crate::store::Loc>,
        mark: crate::store::Mark,
    ) {
        if let Some(b) = &mut self.block {
            if !b.marks.iter().any(|(c, _)| *c == cid) {
                b.marks.push((cid, mark));
            }
            b.was.push(Undo::Doc(cid, id, was));
        }
    }

    /// What decides whether a write may land: a router's lease the node
    /// holds, on a node that takes one. `None` takes every write.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn set_fence(&mut self, fence: Option<Fence>) {
        self.fence = fence;
    }

    /// The fence's refusal, when it has one ([`Self::set_fence`]).
    pub(crate) fn fenced(&self) -> Result<()> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(f) = &self.fence {
            return f().map_err(Error::ReadOnly);
        }
        Ok(())
    }

    /// Whether a block of writes is open ([`Self::begin`]).
    pub fn in_block(&self) -> bool {
        self.block.is_some()
    }

    /// Opens a block: the writes from here to [`Self::commit`] land whole or
    /// not at all, and [`Self::rollback`] puts them back. The database is
    /// the block's alone meanwhile, as it is a statement's -- whoever opens
    /// one holds the write lock until it ends. Reads in it see its writes.
    pub fn begin(&mut self) -> Result<()> {
        if self.block.is_some() {
            return Err(Error::Query("a block is open already".into()));
        }
        self.open_block();
        if let Some(b) = &mut self.block {
            b.defers = true;
        }
        Ok(())
    }

    fn open_block(&mut self) {
        self.block = Some(std::mem::take(&mut self.spare).cleared());
    }

    /// Lands the open block as one record -- written whole or cut off whole
    /// by a crash -- numbered as its last write, and notes its writes on the
    /// change feed, where a subscriber sees them all at once. When the sink
    /// refuses it, the block is put back, and the database takes no more
    /// writes (see `failed`).
    pub fn commit(&mut self) -> Result<()> {
        // Linked before it lands, so that a block put back after all -- a
        // lapsed lease, a refused append -- is put back as any other.
        if DEFERS {
            self.link_waiting(1);
        }
        let Some(mut b) = self.block.take() else {
            return Ok(());
        };
        if b.heads.is_empty() {
            self.spare(b);
            return Ok(());
        }
        // The lease may have lapsed since the block's first write was let
        // through: a block that lands after it would be a write the tenant's
        // next primary never sees.
        if let Err(e) = self.fenced() {
            self.undo(b);
            return Err(e);
        }
        let seq = self.changes.seq() + b.heads.len() as u64;
        #[cfg(not(target_arch = "wasm32"))]
        let at = *self.appended.get_mut();
        // A block that spilled lands as the spills it names and the rest.
        #[cfg(not(target_arch = "wasm32"))]
        let landed = match b.spilled.is_empty() {
            true => None,
            false => Some(self.land_spilled(&mut b, seq, at)),
        };
        #[cfg(target_arch = "wasm32")]
        let landed = None;
        let (r, len) = if let Some(landed) = landed {
            landed
        } else {
            #[cfg(not(target_arch = "wasm32"))]
            let writes = b.notes.len() as u64;
            let record = b.record();
            let r = self.sink_mut().record(seq, &record);
            // Where its frames are in the file, for the handover: the
            // stores hold them as the record does.
            #[cfg(not(target_arch = "wasm32"))]
            if r.is_ok() && self.mapped {
                handover::note_record(&mut self.landed, &mut self.landed_bytes, at, &record);
                self.landed_writes += writes;
            }
            (r, record.len())
        };
        if let Err(e) = self.storage(r) {
            self.undo(b);
            return Err(e);
        }
        *self.appended.get_mut() += len as u64;
        self.dirty = true;
        for &(cid, id) in &b.notes {
            self.note(cid, id);
        }
        // Now, not at the next block's start: a collection the block dropped
        // is held here until then.
        b.was.clear();
        self.spare(b);
        if let Some(w) = &self.watcher {
            w.notify(self.changes.seq());
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.hand_over_when_due();
        Ok(())
    }

    /// Puts the open block's writes back: every store as it stood before
    /// the block's first write to it, and every document it wrote out of
    /// the indexes, the version before it back in. Nothing reached the sink
    /// or the feed, so there is nothing else to undo. Ids a block handed out
    /// are handed out again.
    pub fn rollback(&mut self) {
        if let Some(b) = self.block.take() {
            self.undo(b);
        }
    }

    fn undo(&mut self, mut b: Block) {
        let marks = std::mem::take(&mut b.marks);
        self.rewind(&marks, &mut b.was);
        if DEFERS {
            let mut waiting = std::mem::take(&mut b.waiting);
            self.forget_waiting(&mut waiting);
        }
        self.spare(b);
    }

    /// Keeps a block's buffers for the next one, a large block's let go of
    /// now: a node holds one of these for every database it has open, and
    /// let go of only as the next block began, a COPY of 50 000 768-dim rows
    /// held its 154 MB of frames for as long as nothing else was written.
    fn spare(&mut self, b: Block) {
        self.spare = match b.frames.capacity() > 1 << 16 || b.was.capacity() > 1 << 10 {
            true => Block::default(),
            false => b,
        };
    }

    /// Puts back the writes `undo` holds, the last first, and takes each
    /// store `marks` names back to its mark: what a block wrote. Taken off the end one at a time: a
    /// drain of the log was 0.5 KB of the browser module.
    fn rewind(&mut self, marks: &[(u32, crate::store::Mark)], undo: &mut Vec<Undo>) {
        // The collection the last document was of: a block's writes come in
        // runs of one collection.
        let mut of: Option<(u32, String)> = None;
        while let Some(u) = undo.pop() {
            match u {
                // Each write on its own, the last first: its document out of
                // the indexes, and the one it replaced pointed at and put
                // back in -- 3.0 ms for a block of 50 000 writes. Put back an
                // id at a time, the first of each id's writes had to be
                // found: searching the ids so far for each, those writes took
                // 402 ms under the write lock, and sorted, the sort was 4.6 KB
                // of the browser module. An id written again in the block is
                // put back again, its vector re-linked each time.
                Undo::Doc(cid, id, loc) => {
                    if of.as_ref().is_none_or(|(c, _)| *c != cid) {
                        of = self.named(cid).map(|n| (cid, n));
                    }
                    let Some((_, name)) = &of else {
                        continue;
                    };
                    let c = self.collections.get_mut(name).unwrap();
                    let now = c.store.read(&c.schema, id).ok().flatten();
                    c.store.point(id, loc);
                    let before = c.store.read(&c.schema, id).ok().flatten();
                    if let Some(now) = &now {
                        c.unindex_doc(now, before.as_ref());
                    }
                    if let Some(before) = &before {
                        c.index_doc(before, now.as_ref());
                    }
                }
                // Everything after it was put back before it, so its name is
                // the last, where it went.
                Undo::Created(cid) => {
                    if let Some(name) = self.order.pop() {
                        debug_assert_eq!(self.collections[&name].id, cid);
                        self.collections.remove(&name);
                    }
                    self.next_coll_id = cid;
                    of = None;
                }
                Undo::Dropped(c, at) => {
                    let name = c.schema.name.clone();
                    self.order.insert(at.min(self.order.len()), name.clone());
                    self.collections.insert(name, *c);
                    of = None;
                }
                Undo::Indexed(cid, pos) => {
                    if let Some(name) = self.named(cid) {
                        let c = self.collections.get_mut(&name).unwrap();
                        let f = c.schema.fields[pos].name.clone();
                        drop(c.take_indexes(&f));
                        c.schema.fields[pos].index = IndexKind::None;
                    }
                }
                Undo::PathIndexed(cid, path) => {
                    if let Some(name) = self.named(cid) {
                        let c = self.collections.get_mut(&name).unwrap();
                        if let Some(at) = c.schema.paths.iter().position(|p| p.name == path) {
                            c.schema.paths.remove(at);
                        }
                        c.fit_paths();
                    }
                }
                // The schema as it was, then the indexes: a field added has
                // its index taken off, a dropped one's put back, and a
                // renamed one's moved back to the name it had.
                Undo::Altered(cid, a) => {
                    if let Some(name) = self.named(cid) {
                        let c = self.collections.get_mut(&name).unwrap();
                        let Altered {
                            before,
                            op,
                            field,
                            to,
                            taken,
                        } = *a;
                        c.store.set_dropped(&before.dropped);
                        c.schema = before;
                        match op {
                            FIELD_ADD => drop(c.take_indexes(&field)),
                            FIELD_DROP => c.put_indexes(&field, taken),
                            FIELD_TTL => {}
                            _ => {
                                let t = c.take_indexes(&to);
                                c.put_indexes(&field, t);
                            }
                        }
                        c.fit_paths();
                    }
                }
            }
        }
        for &(cid, mark) in marks {
            if let Some(name) = self.named(cid) {
                self.collections.get_mut(&name).unwrap().store.rewind(mark);
            }
        }
    }

    /// Runs `stmts` as one block: every write in it lands, as one record,
    /// or none does, and a read in it sees the writes before it. What each
    /// answered; or where it stopped and why, nothing of it applied. A
    /// schema change or a compact is refused in a block of more than one --
    /// it runs on its own ([`Statement::fits_block`]). A `put` of many
    /// documents is a block of its own ([`Self::execute_with`]).
    pub fn execute_block(
        &mut self,
        stmts: &[(&Statement, &[Value])],
    ) -> std::result::Result<Vec<Response>, (usize, Error)> {
        if let Some(i) = stmts.iter().position(|(s, _)| !s.fits_block()) {
            return Err((
                i,
                Error::Query(
                    "create, drop, create index and compact run on their own, not in a block"
                        .into(),
                ),
            ));
        }
        if let Some(i) = stmts.iter().position(|(s, _)| !s.is_read_only()) {
            self.may_start().map_err(|e| (i, e))?;
        }
        let outer = self.block.is_some();
        if !outer {
            self.open_block();
        }
        let mut out = Vec::with_capacity(stmts.len());
        for (i, (stmt, params)) in stmts.iter().enumerate() {
            match self.run_one(stmt, params) {
                Ok(r) => out.push(r),
                Err(e) => {
                    if !outer {
                        self.rollback();
                    }
                    return Err((i, e));
                }
            }
        }
        if !outer {
            self.commit()
                .map_err(|e| (stmts.len().saturating_sub(1), e))?;
        }
        Ok(out)
    }

    /// One statement as a block runs it: its reads refused for collation
    /// data the module has not been handed.
    fn run_one(&mut self, stmt: &Statement, params: &[Value]) -> Result<Response> {
        let out = self.execute_inner(stmt, params);
        // A block that outgrew its bound spills what it holds into the file,
        // a statement's writes at a time: a /batch's statements, a `put` of
        // many rows.
        #[cfg(not(target_arch = "wasm32"))]
        if out.is_ok() && !stmt.is_read_only() {
            self.spill_when_due()?;
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

    // ------------------------------------------------------------ execution

    pub fn execute(&mut self, stmt: &Statement) -> Result<Response> {
        self.execute_with(stmt, &[])
    }

    /// Read-only execution. Because it takes `&self`, several readers can run
    /// at once under an `RwLock`; statements that need to write are rejected
    /// (the caller separates them first with [`Statement::is_read_only`]).
    pub fn query(&self, stmt: &Statement, params: &[Value]) -> Result<Response> {
        self.refuse_inexact(stmt, params)?;
        let stmt = self.answered(stmt, params)?;
        match &*stmt {
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
        // A write is a block of one: a `put` of many documents stopped half
        // way -- a hook's refusal, a crash -- left the ones before applied.
        // Its writes land as one record, which for a lone document is the
        // record it always was.
        if !stmt.is_read_only() && stmt.fits_block() {
            self.may_start()?;
            if self.block.is_some() {
                return self.run_one(stmt, params);
            }
            self.open_block();
            return match self.run_one(stmt, params) {
                Ok(r) => self.commit().map(|_| r),
                Err(e) => {
                    self.rollback();
                    Err(e)
                }
            };
        }
        if !stmt.is_read_only() {
            if self.block.is_some() {
                return Err(Error::Query(
                    "compact runs on its own, not in a block".into(),
                ));
            }
            // `compact` changes no document, only how the file holds them.
            self.may_write(matches!(stmt, Statement::Compact(_)))?;
        }
        let before = self.changes.seq();
        let out = self.run_one(stmt, params);
        let after = self.changes.seq();
        if after != before {
            if let Some(w) = &self.watcher {
                w.notify(after);
            }
        }
        out
    }

    fn execute_inner(&mut self, stmt: &Statement, params: &[Value]) -> Result<Response> {
        self.refuse_inexact(stmt, params)?;
        let answered = self.answered(stmt, params)?;
        match &*answered {
            Statement::CreateCollection {
                schema,
                if_not_exists,
            } => self.create_collection(schema.clone(), *if_not_exists),
            Statement::DropCollection { name, if_exists } => self.drop_collection(name, *if_exists),
            Statement::AlterCollection { collection, change } => {
                self.alter_collection(collection, change)
            }
            Statement::CreateIndex {
                collection,
                field,
                kind,
                if_not_exists,
            } => self.create_index(collection, field, kind, *if_not_exists),
            Statement::Put {
                collection,
                docs,
                insert,
            } => self.put(collection, docs, *insert, params),
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

    /// `stmt` as it runs: each `in (get ...)` in a filter answered as the
    /// list it is, and each filter over a collection whose rows expire
    /// (`@ttl`) given the test that leaves out those past their time. The
    /// statement itself where it holds neither, which costs a look at its
    /// filters and at each collection's fields -- a handful, against a
    /// query's rows. `explain` answers its own ([`Self::explain`]), so its
    /// plan holds the inner `get`s.
    fn answered<'s>(
        &self,
        stmt: &'s Statement,
        params: &[Value],
    ) -> Result<std::borrow::Cow<'s, Statement>> {
        use std::borrow::Cow;
        let (collection, filter) = match stmt {
            Statement::Select(sel) => {
                return Ok(match self.answered_select(sel, params, 0)? {
                    Cow::Borrowed(_) => Cow::Borrowed(stmt),
                    Cow::Owned(s) => Cow::Owned(Statement::Select(s)),
                })
            }
            Statement::Update {
                collection, filter, ..
            }
            | Statement::Delete { collection, filter } => (collection, filter),
            _ => return Ok(Cow::Borrowed(stmt)),
        };
        if !filter.as_ref().is_some_and(Expr::has_subquery) && self.ttl_of(collection).is_none() {
            return Ok(Cow::Borrowed(stmt));
        }
        let mut filter = filter.clone();
        self.answer_filter(collection, &mut filter, params, 0)?;
        let collection = collection.clone();
        // Made anew rather than the statement cloned whole: `Statement`'s
        // clone was its every variant's, a schema's among them.
        Ok(Cow::Owned(match stmt {
            Statement::Update { set, .. } => Statement::Update {
                collection,
                set: set.clone(),
                filter,
            },
            _ => Statement::Delete { collection, filter },
        }))
    }

    /// [`Self::answered`] for a `get`, which an inner one is as well,
    /// `depth` levels down.
    fn answered_select<'s>(
        &self,
        sel: &'s Select,
        params: &[Value],
        depth: usize,
    ) -> Result<std::borrow::Cow<'s, Select>> {
        let expiring = self.ttl_of(&sel.collection).is_some()
            || sel
                .lookup
                .as_ref()
                .is_some_and(|l| l.chain().any(|s| self.ttl_of(&s.collection).is_some()));
        if !expiring && !sel.has_subquery() {
            return Ok(std::borrow::Cow::Borrowed(sel));
        }
        let mut sel = sel.clone();
        sel.each_filter_mut(&mut |collection, f| self.answer_filter(collection, f, params, depth))?;
        Ok(std::borrow::Cow::Owned(sel))
    }

    /// A filter over `collection` as it runs ([`Self::answered`]).
    fn answer_filter(
        &self,
        collection: &str,
        filter: &mut Option<Expr>,
        params: &[Value],
        depth: usize,
    ) -> Result<()> {
        if let Some(f) = filter {
            f.each_subquery_mut(&mut |e| self.answer_subquery(e, params, depth))?;
        }
        if let Some(alive) = self.alive(collection)? {
            *filter = Some(match filter.take() {
                Some(f) => Expr::And(Box::new(f), Box::new(alive)),
                None => alive,
            });
        }
        Ok(())
    }

    /// `e`, an `in (get ...)`, made the `in [..]` it answers: the inner
    /// `get` run once, its one column the list, a null left out -- it
    /// equals no value a row could be found by, and in the list it would
    /// find the rows whose field is null. Past [`MAX_SUBQUERY_VALUES`] it
    /// is refused, never cut short.
    fn answer_subquery(&self, e: &mut Expr, params: &[Value], depth: usize) -> Result<()> {
        let Expr::InSelect(lhs, inner) = std::mem::replace(e, Expr::Lit(Value::Null)) else {
            unreachable!("each_subquery_mut hands an `in (get ...)`")
        };
        if depth >= MAX_SUBQUERY_DEPTH {
            return Err(Error::Query(format!(
                "`in (get ...)` nested too deep: at most {MAX_SUBQUERY_DEPTH} levels"
            )));
        }
        inner.check_subquery()?;
        let mut inner = self
            .answered_select(&inner, params, depth + 1)?
            .into_owned();
        // One past the bound is all it takes to know the set is past it:
        // a scan stops there rather than gather a million values to refuse
        // them. `near` and `match` are bounded by their own pages, and an
        // aggregate without `group` answers one row.
        let ranked = inner.near.is_some() || inner.matcher.is_some();
        if !ranked && (inner.aggregate.is_empty() || inner.group.is_some()) {
            let bound = MAX_SUBQUERY_VALUES + 1;
            inner.limit = Some(inner.limit.map_or(bound, |l| l.min(bound)));
        }
        let rows = self.select(&inner, params)?.rows;
        if rows.len() > MAX_SUBQUERY_VALUES {
            return Err(Error::Query(format!(
                "`in (get {} ...)` found more than {MAX_SUBQUERY_VALUES} values: a set is not cut \
                 short, so narrow the inner `get` or ask with `lookup ... required`",
                inner.collection
            )));
        }
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            if let Some(v) = row.values.into_iter().next().filter(|v| !v.is_null()) {
                items.push(Expr::Lit(v));
            }
        }
        plan(|| {
            format!(
                "subquery: {} values from {}, an `in` list",
                items.len(),
                inner.collection
            )
        });
        *e = Expr::In(lhs, items);
        Ok(())
    }

    /// The field whose value a row of `collection` expires by, and how
    /// long after it (`@ttl`), in milliseconds.
    fn ttl_of(&self, collection: &str) -> Option<(&str, u64)> {
        let c = self.collections.get(collection)?;
        c.schema
            .fields
            .iter()
            .find_map(|f| f.index.ttl().map(|ttl| (f.name.as_str(), ttl)))
    }

    /// The test a row of `collection` passes while it lives, at the time a
    /// read is answered (`@ttl`): `not (field <= now - ttl)`, so a row whose
    /// field is null -- which has no time to expire from -- lives, and none
    /// is swept. `None` for a collection whose rows do not expire. A `not`
    /// rather than `field > now - ttl`, so that the planner never takes it
    /// for a range to narrow by: it is a test of each row the rest found.
    fn alive(&self, collection: &str) -> Result<Option<Expr>> {
        let Some((field, ttl)) = self.ttl_of(collection) else {
            return Ok(None);
        };
        let cutoff = self.now()?.saturating_sub(ttl.min(i64::MAX as u64) as i64);
        Ok(Some(Expr::Not(Box::new(Expr::Cmp(
            CmpOp::Le,
            Box::new(Expr::Field(field.to_string())),
            Box::new(Expr::Lit(Value::Timestamp(cutoff))),
        )))))
    }

    /// The collections whose rows expire, by name: what a server's sweeper
    /// looks at.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn expiring(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|n| self.ttl_of(n).is_some())
            .cloned()
            .collect()
    }

    /// Up to `max` of `collection`'s rows past their time at `now`,
    /// ascending: a range of the field's ordered index where it is narrow,
    /// and the scan in id order, stopped at `max`, where most rows are past
    /// their time. Under the read lock; [`Self::sweep`] deletes them.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn expired(&self, collection: &str, now: i64, max: usize) -> Result<Vec<DocId>> {
        let Some((field, ttl)) = self.ttl_of(collection) else {
            return Ok(Vec::new());
        };
        let cutoff = now.saturating_sub(ttl.min(i64::MAX as u64) as i64);
        let past = Expr::Cmp(
            CmpOp::Le,
            Box::new(Expr::Field(field.to_string())),
            Box::new(Expr::Lit(Value::Timestamp(cutoff))),
        );
        self.matching_ids_capped(collection, &Some(past), &[], Some(max))
    }

    /// Deletes those of `ids` still past their time at `now` -- a write
    /// since [`Self::expired`] may have moved one's time on -- as one block
    /// of ordinary deletes, so a replica, `/_changes`, an archive and a
    /// subscriber see each as any delete. Not through a statement: a
    /// `del` leaves out the rows past their time, as every read of the
    /// collection does, and would find none of them.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn sweep(&mut self, collection: &str, now: i64, ids: &[DocId]) -> Result<usize> {
        let Some((field, ttl)) = self.ttl_of(collection) else {
            return Ok(0);
        };
        if ids.is_empty() {
            return Ok(0);
        }
        let cutoff = now.saturating_sub(ttl.min(i64::MAX as u64) as i64);
        let filter = Some(Expr::And(
            Box::new(Expr::In(
                Box::new(Expr::Field("id".into())),
                ids.iter()
                    .map(|&id| Expr::Lit(Value::Int(id as i64)))
                    .collect(),
            )),
            Box::new(Expr::Cmp(
                CmpOp::Le,
                Box::new(Expr::Field(field.to_string())),
                Box::new(Expr::Lit(Value::Timestamp(cutoff))),
            )),
        ));
        self.may_start()?;
        if self.block.is_some() {
            return Err(Error::Query(
                "a block is open: the sweep waits for it".into(),
            ));
        }
        let collection = collection.to_string();
        self.open_block();
        match self.delete(&collection, &filter, &[]) {
            Ok(Response::Affected(n)) => self.commit().map(|_| n),
            Ok(_) => self.commit().map(|_| 0),
            Err(e) => {
                self.rollback();
                Err(e)
            }
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
        if let Some(b) = &mut self.block {
            b.was.push(Undo::Created(cid));
        }
        Ok(Response::Ok(format!("collection `{name}` created")))
    }

    fn drop_collection(&mut self, name: &str, if_exists: bool) -> Result<Response> {
        match self.collections.remove(name) {
            Some(c) => {
                let at = self.order.iter().position(|n| n == name);
                if let Some(at) = at {
                    self.order.remove(at);
                }
                self.wal(REC_DROP, c.id, &[])?;
                self.note(c.id, SCHEMA_MARK);
                // Kept until the block lands, for it to come back if the
                // block does not.
                if let Some(b) = &mut self.block {
                    let at = at.unwrap_or(self.order.len());
                    b.was.push(Undo::Dropped(Box::new(c), at));
                }
                Ok(Response::Ok(format!("collection `{name}` dropped")))
            }
            None if if_exists => Ok(Response::Ok(format!("no collection `{name}`"))),
            None => Err(Error::NotFound(format!("collection `{name}`"))),
        }
    }

    /// Adds, drops or renames a field, rewriting no document: a field added
    /// goes last, where a document written before it ends, and reads as
    /// `null` there; a field dropped leaves its place, skipped on read and
    /// written as `null`, for `compact` to take out, and its index goes with
    /// it; a rename is the schema alone. Undone in a block as an index built
    /// is ([`Undo::Altered`]).
    fn alter_collection(&mut self, collection: &str, change: &Alter) -> Result<Response> {
        // The vectors the block's `put`s left waiting are linked first:
        // they wait under their field's name, which a rename or a drop
        // takes away.
        if DEFERS {
            self.link_waiting(1);
        }
        let c = self.collection(collection)?;
        let mut schema = c.schema.clone();
        let no_field = |f: &str| Error::NotFound(format!("field `{f}` in `{collection}`"));
        let (op, field, to) = match change {
            Alter::AddField(f) => {
                if let Some(feature) = missing_feature(&f.index) {
                    return Err(not_built("the index", feature));
                }
                if f.required {
                    return Err(Error::Query(format!(
                        "`{}` cannot be required: the documents `{collection}` holds have no \
                         value for it",
                        f.name
                    )));
                }
                // The checks a `create collection` makes of its fields: a
                // name taken or reserved, a collation or an index the type
                // cannot have.
                let mut fields = schema.fields.clone();
                fields.push(f.clone());
                // A `String`, as the parser hands it one: a `&str` was a
                // second copy of `Schema::new`, 0.6 KB of the browser module.
                schema.fields = Schema::new(collection.to_string(), fields)?.fields;
                (FIELD_ADD, f.name.clone(), String::new())
            }
            Alter::DropField(name) => {
                let pos = schema.field_pos(name).ok_or_else(|| no_field(name))?;
                if schema.fields.len() == 1 {
                    return Err(Error::Query(format!(
                        "`{name}` is the only field of `{collection}`, which keeps one at least"
                    )));
                }
                let place = schema.place(pos);
                schema.fields.remove(pos);
                let at = schema.dropped.partition_point(|&d| d < place);
                schema.dropped.insert(at, place);
                // The indexes on paths into it go with it.
                let prefix = format!("{name}.");
                schema.paths.retain(|p| !p.name.starts_with(&prefix));
                (FIELD_DROP, name.clone(), String::new())
            }
            Alter::RenameField(from, to) => {
                let pos = schema.field_pos(from).ok_or_else(|| no_field(from))?;
                // A name taken or reserved, as `create collection` refuses.
                let mut fields = schema.fields.clone();
                fields[pos].name = to.clone();
                schema.fields = Schema::new(collection.to_string(), fields)?.fields;
                // The paths into it are under its new name.
                let prefix = format!("{from}.");
                for p in schema.paths.iter_mut() {
                    if let Some(keys) = p.name.strip_prefix(&prefix) {
                        p.name = format!("{to}.{keys}");
                    }
                }
                (FIELD_RENAME, from.clone(), to.clone())
            }
            Alter::Ttl(name, ttl) => {
                let pos = schema.field_pos(name).ok_or_else(|| no_field(name))?;
                let f = &mut schema.fields[pos];
                if !matches!(f.index, IndexKind::Sorted { .. }) {
                    return Err(Error::Query(format!(
                        "`{name}` has no ordered index to expire rows by: \
                         `create index on {collection} ({name}) @ttl(..)` makes one"
                    )));
                }
                let kind = IndexKind::Sorted { ttl: *ttl };
                kind.check(name, &f.ty)?;
                f.index = kind;
                crate::schema::one_expiry(collection, schema.fields.iter())?;
                (FIELD_TTL, name.clone(), String::new())
            }
        };
        let ch = FieldChange {
            op,
            field,
            to,
            schema,
        };
        let c = self.collections.get_mut(collection).unwrap();
        let (cid, before) = (c.id, c.schema.clone());
        let taken = c.alter_fields(&ch);
        // Before the index of a field added is built, which a block that
        // does not land -- or a build that fails -- puts back with it.
        if let Some(b) = &mut self.block {
            b.was.push(Undo::Altered(
                cid,
                Box::new(Altered {
                    before,
                    op,
                    field: ch.field.clone(),
                    to: ch.to.clone(),
                    taken,
                }),
            ));
        }
        let c = self.collections.get_mut(collection).unwrap();
        if let (FIELD_ADD, Some(pos)) = (op, c.schema.field_pos(&ch.field)) {
            build_index(c, pos)?;
        }
        self.wal(REC_FIELDS, cid, &ch.encode())?;
        self.note(cid, SCHEMA_MARK);
        Ok(Response::Ok(match op {
            FIELD_ADD => format!("field `{}` added to `{collection}`", ch.field),
            FIELD_DROP => format!("field `{}` dropped from `{collection}`", ch.field),
            FIELD_TTL => match ttl_after(&ch) {
                Some(ms) => format!(
                    "rows of `{collection}` expire {} after `{}`",
                    crate::schema::ttl_text(ms),
                    ch.field
                ),
                None => format!("rows of `{collection}` no longer expire"),
            },
            _ => format!("field `{}` renamed to `{}`", ch.field, ch.to),
        }))
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
        // A path's index is a field of its own beside the fields, and is
        // built here under the write lock: a hash or an ordered index reads
        // a value a row -- 11 ms over 100 000 documents for `@hash` on
        // `meta.lang`, 21 for `@sorted` on `meta.source.rank` -- not worth a
        // copy beside the database, as an HNSW build's seconds are.
        if crate::schema::split_path(field).is_some() {
            let f = crate::schema::Field::new(field, DataType::Json).indexed(kind.resolved());
            c.schema.add_path(f);
            if let Some(b) = &mut self.block {
                b.was.push(Undo::PathIndexed(cid, field.to_string()));
            }
            let c = self.collections.get_mut(collection).unwrap();
            build_path_index(c, field)?;
        } else {
            let pos = c.schema.field_pos(field).unwrap();
            c.schema.fields[pos].index = kind.resolved();
            // Before the build, which a block that does not land -- or a
            // build that fails -- puts back with it.
            if let Some(b) = &mut self.block {
                b.was.push(Undo::Indexed(cid, pos));
            }
            let c = self.collections.get_mut(collection).unwrap();
            build_index(c, pos)?;
        }
        let c = self.collections.get_mut(collection).unwrap();
        // Over documents that hold a value twice, a unique index is refused
        // -- the statement put back, the index with it -- and names one.
        if kind.is_unique() {
            refuse_shared(c, field)?;
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
            set_field(schema, &mut doc, k, v)?;
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
        insert: bool,
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
            let mark = c.store.mark();
            let op = if doc.id == 0 {
                doc.id = c.store.allocate_id();
                WriteOp::Insert
            } else if c.store.contains(doc.id) {
                // The statement is a block: what it wrote before this one
                // is put back with it.
                if insert {
                    return Err(Error::Duplicate(format!(
                        "`{collection}` holds a document {} already: insert makes new \
                         ones, put writes over",
                        doc.id
                    )));
                }
                WriteOp::Update
            } else {
                WriteOp::Insert
            };
            // Before anything is written, as a taken id is refused above --
            // and the id handed out above handed out again, as a block put
            // back hands its ids out again: nothing of this document is in
            // the store for the block's mark to take back.
            let checked = hooks
                .iter()
                .try_for_each(|h| h.before_write(&schema, op, &mut doc))
                .and_then(|_| c.unique_clash(&doc));
            if let Err(e) = checked {
                c.store.rewind(mark);
                return Err(e);
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
            let was = c.store.loc(doc.id);
            let frame = c.store.append(OP_PUT, doc.id, &payload);
            c.index_scalar(&doc, old.as_ref());
            self.remember(cid, doc.id, was, mark);
            self.wal(REC_DATA, cid, &frame)?;
            self.note(cid, doc.id);
            for h in &hooks {
                h.after_write(collection, op, &doc)?;
            }
            written.push(doc);
            n += 1;
        }
        // Vectors are indexed in a batch: construction can parallelise. In
        // a block that defers, the block's statements are one batch.
        let c = self.collections.get_mut(collection).unwrap();
        match self.block.as_mut().filter(|b| DEFERS && b.defers) {
            Some(b) => {
                c.defer_vectors_batch(&written, &mut b.waiting);
                self.link_waiting(LINK_AT);
            }
            None => c.index_vectors_batch(&written),
        }
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
        let test = Filter::new(c, f, &ctx);
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
                    if test.matches(id, &ctx)? {
                        out.push(id);
                    }
                }
                false
            }
            // No index: the full scan, read lazily so a cap stops it early,
            // from the lowest id the filter lets through.
            None => {
                // The browser leaves it out: 0.4 KB brotli of its module,
                // for collections it holds in memory.
                #[cfg(not(target_arch = "wasm32"))]
                let ids = c
                    .store
                    .iter_ids_from(f.conjunct_id_floor(params).unwrap_or(0));
                #[cfg(target_arch = "wasm32")]
                let ids = c.store.iter_ids();
                for id in ids {
                    if out.len() >= want {
                        break;
                    }
                    tested += 1;
                    if test.matches(id, &ctx)? {
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
            let Some(map) = c.hash(field)? else {
                continue;
            };
            // The bucket key is produced on the write path from the value
            // coerced to the field's type (`10` -> `10.0`), so the lookup has
            // to go through the same conversion. Otherwise `price = 10` on
            // `price float @hash` would find an empty bucket and silently
            // return 0 rows -- the mere presence of the index would change the
            // query's answer. A literal that cannot be coerced (`year = "abc"`)
            // skips the index and leaves the decision to the eval path.
            let Some(key) = lookup_key(&c.schema, field, val) else {
                continue;
            };
            let bucket = map.get(&key).cloned().unwrap_or_default();
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
            let Some(map) = c.hash(field)? else {
                continue;
            };
            let mut union: Vec<DocId> = Vec::new();
            let mut whole = true;
            for v in vals {
                // The same coercion the single equality needs, and for the
                // same reason. An element the index cannot express takes the
                // whole list back to the eval path: a union missing one
                // element's rows is a wrong answer, not a slow one.
                let Some(key) = lookup_key(&c.schema, field, v) else {
                    whole = false;
                    break;
                };
                if let Some(bucket) = map.get(&key) {
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
            for (field, _) in &c.sorted {
                if !ranges.iter().any(|r| r.0 == field) {
                    continue;
                }
                let Some(fd) = c.schema.indexed(field) else {
                    continue;
                };
                let Some((range, exact)) = sorted_range(fd, field, f, params) else {
                    continue;
                };
                let Some(ix) = c.sorted_index(field)? else {
                    continue;
                };
                if !ix.answers() {
                    plan(|| {
                        format!(
                            "filter: the ordered index on {field} not used, it holds a value \
                             it cannot order"
                        )
                    });
                    continue;
                }
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
        let test = Filter::new(c, f, ctx);
        let matches = |id: DocId| test.matches(id, ctx);
        // A set an index names and sizes past the budget -- a hash bucket, a
        // union of them -- decides the plan without being gathered: 250 000
        // ids of a quarter's bucket, copied, sorted and looked up each, were
        // most of the 0.77 ms a filter keeping a quarter of a million rows
        // took, against 0.18 unfiltered. The walk's beam is widened until
        // about twice the page passes the filter: at the page exactly, 43%
        // of the walks at a beam of 40 came up short and searched the
        // quarter's 250 000 rows exactly. Where that beam would measure as
        // many vectors as the set holds, the set is searched exactly at
        // once, as the walk would have ended doing.
        if !near.exact {
            if let Some(size) = self.indexed_size(c, f, params)? {
                if size > budget {
                    let beam = near.ef.unwrap_or(ix.spec.ef_search).max(want);
                    let rows = c.store.len().max(1);
                    let wide = want
                        .saturating_mul(2)
                        .saturating_mul(rows)
                        .div_ceil(size)
                        .max(beam);
                    if ix.probe_budget(Some(wide)) < size {
                        plan(|| {
                            format!(
                                "filter: an index names {size} rows, more than the ANN budget \
                                 of {budget}; walked with a beam of {wide}"
                            )
                        });
                        let field = &near.field;
                        return self.filtered_walk(
                            c,
                            sp,
                            f,
                            qv,
                            want,
                            field,
                            Some(wide),
                            params,
                            &matches,
                            None,
                        );
                    }
                    plan(|| {
                        format!(
                            "filter: an index names {size} rows of {rows}, fewer than a beam of \
                             {wide} would measure to pass the page"
                        )
                    });
                    let mut probe = match self.filter_candidates(c, f, params, usize::MAX)? {
                        Some((rows, true)) => FilterProbe::done(rows),
                        Some((rows, false)) => FilterProbe::new(rows),
                        None => FilterProbe::new(c.store.ids()),
                    };
                    probe.run(usize::MAX, matches)?;
                    let ids = probe.into_sorted();
                    plan(|| format!("near: the {} rows searched exactly", ids.len()));
                    return sp.search_ids(qv, want, near.ef, &ids);
                }
            }
        }
        let mut probe = match self.filter_candidates(c, f, params, usize::MAX)? {
            Some((rows, true)) => FilterProbe::done(rows),
            Some((rows, false)) => FilterProbe::new(rows),
            None => FilterProbe::new(c.store.ids()),
        };
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
        self.filtered_walk(
            c,
            sp,
            f,
            qv,
            want,
            &near.field,
            near.ef,
            params,
            &matches,
            Some(probe),
        )
    }

    /// More rows match than the budget: the ANN, testing each candidate in
    /// distance order as `search` would test membership -- over codes only
    /// those still able to make the page (`Space::order`). `probe` is what
    /// has been found of the set, `None` for none of it yet.
    #[allow(clippy::too_many_arguments)]
    fn filtered_walk(
        &self,
        c: &Collection,
        sp: &Space,
        f: &Expr,
        qv: &[f32],
        want: usize,
        field: &str,
        ef: Option<usize>,
        params: &[Value],
        matches: &dyn Fn(DocId) -> Result<bool>,
        probe: Option<FilterProbe>,
    ) -> Result<Vec<(DocId, f32)>> {
        let mut tested = 0;
        let hits = sp.search(qv, want, ef, &mut |id| {
            tested += 1;
            Ok(c.store.contains(id) && matches(id)?)
        })?;
        plan(|| {
            format!(
                "near: ANN over {field}, {}, {tested} candidates tested, {} kept",
                beam(sp.ix, ef, want),
                hits.len()
            )
        });
        // The filter is applied after the candidates are gathered, so a
        // filter correlated with the vector can eliminate all of them. If the
        // result comes up short the rest of the set is found and searched
        // exactly, as it always was.
        if hits.len() < want {
            let mut probe = match probe {
                Some(p) => p,
                None => match self.filter_candidates(c, f, params, usize::MAX)? {
                    Some((rows, true)) => FilterProbe::done(rows),
                    Some((rows, false)) => FilterProbe::new(rows),
                    None => FilterProbe::new(c.store.ids()),
                },
            };
            probe.run(usize::MAX, matches)?;
            let ids = probe.into_sorted();
            if hits.len() < want.min(ids.len()) {
                plan(|| {
                    format!(
                        "near: the ANN came up short, the probe finished, {} rows searched exactly",
                        ids.len()
                    )
                });
                return sp.search_ids(qv, want, ef, &ids);
            }
        }
        Ok(hits)
    }

    /// How many rows the narrowest set an index names for `f` holds -- a
    /// hash bucket for an equality, the buckets of an `in`, the ids named
    /// -- counted without gathering it, as [`Self::filter_candidates`]
    /// would choose it; `None` where no index names one. A bucket may still
    /// hold a row deleted in a block not landed, so it is a bound.
    fn indexed_size(&self, c: &Collection, f: &Expr, params: &[Value]) -> Result<Option<usize>> {
        let mut best: Option<usize> = None;
        let mut take = |n: usize| best = Some(best.map_or(n, |b| b.min(n)));
        let mut eqs = Vec::new();
        f.conjunct_equalities(params, &mut eqs);
        for (field, val) in eqs {
            if field == "id" {
                take(1);
                continue;
            }
            let Some(map) = c.hash(field)? else {
                continue;
            };
            if let Some(key) = lookup_key(&c.schema, field, val) {
                take(map.get(&key).map_or(0, |b| b.len()));
            }
        }
        let mut ins = Vec::new();
        f.conjunct_in_sets(params, &mut ins);
        for (field, vals) in ins {
            if field == "id" {
                take(vals.len());
                continue;
            }
            let Some(map) = c.hash(field)? else {
                continue;
            };
            let mut sum = 0;
            let mut whole = true;
            for v in vals {
                match lookup_key(&c.schema, field, v) {
                    Some(key) => sum += map.get(&key).map_or(0, |b| b.len()),
                    None => {
                        whole = false;
                        break;
                    }
                }
            }
            if whole {
                take(sum);
            }
        }
        Ok(best)
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
        let (Some(ix), Some(limit)) = (c.sorted_index(field)?, sel.limit) else {
            return Ok(None);
        };
        if ix.has_nan() {
            let what = match ix.answers() {
                true => "a NaN",
                false => "a value it cannot order",
            };
            plan(|| format!("order: the ordered index on {field} not walked, it holds {what}"));
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
                let (Some(other), Some(fd)) = (c.sorted_index(name)?, c.schema.indexed(name))
                else {
                    continue;
                };
                if !other.answers() {
                    continue;
                }
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
                .indexed(field)
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
        let test = sel.filter.as_ref().map(|f| Filter::new(c, f, ctx));
        ix.walk(!asc, range.as_ref(), |id| {
            if !bare {
                if let Some(test) = &test {
                    walked += 1;
                    if walked > budget {
                        gave_up = true;
                        return Ok(false);
                    }
                    if !test.matches(id, ctx)? {
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
                    plan(|| format!("near: ANN over {}, {}", near.field, beam(ix, near.ef, want)));
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
        let ix = c.sparse_index(field)?.ok_or_else(|| {
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
        let whole_k = f.k.unwrap_or(DEFAULT_FUSE_K);
        let k = whole_k as f32;
        let text = self.run_match(c, sel, m, depth, params, ctx)?;
        let vectors = self.run_near(c, sel, near, depth, params, ctx)?;
        plan(|| {
            format!(
                "fuse: reciprocal rank, k = {whole_k}, over {} + {} candidates",
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
        let ix = c.text(&m.field)?.ok_or_else(|| {
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
                test: std::cell::OnceCell::new(),
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
            let Ok(Some(map)) = child.hash(field) else {
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
            let owner = format!("{}.", l.collection);
            sources.push(Some(source_or_err(&child.schema, col, &owner)?));
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
                // Put in order as the parent path is, through `order_ids`,
                // which reads the keys up front -- comparing through the
                // store would read every row log(n) times. Its ties go by
                // position, so the children go in ascending id first: a
                // hash bucket holds them that way almost always, not always,
                // and a tie has gone to the lower id. A sort of its own, of
                // `(Vec<Value>, DocId)` rows, was 8.1 KB of the browser
                // module.
                //
                // `lookup` is the one place a bounded `order` is known up
                // front: the clause carries its own `limit`, so only
                // `offset + limit` children can ever be emitted and the rest
                // never need an order at all. Elsewhere `order` has no such
                // guarantee, which is why the engine sorts in full there.
                kept.sort_unstable();
                kept = order_ids(&child.store, &kept, &keys, l.offset.saturating_add(limit))?;
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
                        Some(at) => read_source(&child.store, cid, *at)?,
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
        // The inner `get`s run inside the plan, which names what each found.
        let result = self
            .answered_select(sel, params, 0)
            .and_then(|sel| self.select(&sel, params));
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
        // Each column's field, or the path into one, found once a query.
        let mut sources = Vec::with_capacity(columns.len());
        for col in &columns {
            sources.push(match col == "id" {
                true => None,
                false => Some(source_or_err(&c.schema, col, "")?),
            });
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
            for src in &sources {
                values.push(match src {
                    None => Value::Int(id as i64),
                    Some(at) => read_source(&c.store, id, *at)?,
                });
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
        let pos_of = |name: &str| match name.contains('.') {
            // Folded by its field's type, which a path has none of.
            true => Err(Error::Query(format!(
                "`{name}` is a path: an aggregate and `group` read a field"
            ))),
            false => c
                .schema
                .field_pos(name)
                .ok_or_else(|| Error::NotFound(format!("field `{name}`"))),
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
        let mut index: crate::maps::Map<Vec<u8>, Vec<DocId>> = Default::default();
        let mut keys: Vec<Value> = Vec::new();
        let mut folds: Vec<Vec<Fold>> = Vec::new();
        if group.is_none() {
            keys.push(Value::Null);
            folds.push(start.clone());
        }
        let mut row = Vec::with_capacity(positions.len());
        let mut key = Vec::new();
        let mut places = Vec::with_capacity(positions.len());
        for &p in &positions {
            places.push(c.schema.place(p));
        }
        for &id in &ids {
            if !c.store.read_fields(id, &places, &mut row)? {
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
            order.push((None, s.asc, s.collate.or(field), None));
        }
        order.push((None, true, None, None));
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
            // An update makes a duplicate as a put does: `set email = "a"`
            // over two documents is refused at the second.
            c.unique_clash(&doc)?;
            let old = c.store.read(&schema, id)?;
            if let Some(old) = &old {
                c.unindex_doc(old, Some(&doc));
            }
            let payload = Store::encode_doc(&schema, &doc);
            let (mark, was) = (c.store.mark(), c.store.loc(id));
            let frame = c.store.append(OP_PUT, id, &payload);
            c.index_doc(&doc, old.as_ref());
            self.remember(cid, id, was, mark);
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
            // Checked before the value is worked out, over the document as
            // it was, as every other is.
            if schema.field(k).is_none() && schema.path_of(k)?.is_none() {
                return Err(Error::NotFound(format!("field `{k}`")));
            }
            let v = eval(e, &mut DocRow(&snapshot, schema), &ctx)?;
            set_field(schema, &mut doc, k, v)?;
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
            let (mark, was) = (c.store.mark(), c.store.loc(id));
            let frame = c.store.append(OP_DEL, id, &[]);
            self.remember(cid, id, was, mark);
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
        // A dropped field's places are taken out of every document, which
        // compacts the store besides; over a mapped file the documents come
        // into memory for it, until the rewrite below points them at the new
        // file.
        for name in &targets {
            let c = self.collections.get_mut(name).unwrap();
            match c.schema.dropped.is_empty() {
                false => c.strip_dropped()?,
                true if !mapped => c.store.compact()?,
                true => {}
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

/// The hash index of the field at `pos`, from the documents: what `create
/// index` builds, and what the first read after an open does.
fn hash_of(store: &Store, pos: usize, keys: Option<&str>) -> Result<HashIndex> {
    let mut ix = HashIndex::default();
    for id in store.ids() {
        let v = match keys {
            None => store.read_field(id, pos)?,
            Some(keys) => store.read_path(id, pos, keys)?,
        };
        if let Some(v) = v {
            ix.add(hash_key(&v), id);
        }
    }
    Ok(ix)
}

/// The refusal of a unique index over the field at `pos` whose documents
/// hold a value twice.
fn refuse_shared(c: &Collection, field: &str) -> Result<()> {
    let shared = c.hash(field)?.and_then(HashIndex::shared);
    match shared {
        Some((v, a, b)) => Err(Error::Duplicate(format!(
            "`{}.{field}` cannot be unique: documents {a} and {b} both hold {}",
            c.schema.name,
            crate::json::to_string(&v)
        ))),
        None => Ok(()),
    }
}

/// The full-text index of the field at `pos`, as [`hash_of`].
fn text_of(store: &Store, pos: usize, spec: crate::schema::TextIndexSpec) -> Result<TextIndex> {
    let mut ix = TextIndex::new(spec);
    for id in store.ids() {
        if let Some(Value::Text(t)) = store.read_field(id, pos)? {
            ix.insert(id, &t);
        }
    }
    ix.shrink_to_fit();
    Ok(ix)
}

/// The ordered index of the field at `pos`, as [`hash_of`]: sorted once
/// from its keys rather than inserted row by row.
#[cfg(feature = "sorted")]
fn sorted_of(
    store: &Store,
    field: &crate::schema::Field,
    pos: usize,
    keys: Option<&str>,
) -> Result<SortedIndex> {
    let mut rows = Vec::with_capacity(store.len());
    for id in store.ids() {
        rows.push((
            id,
            match keys {
                None => store.read_field(id, pos)?,
                Some(keys) => store.read_path(id, pos, keys)?,
            },
        ));
    }
    Ok(SortedIndex::build(
        &field.ty,
        field.collate,
        &mut rows.into_iter(),
    ))
}

/// A build without ordered indexes holds none to build.
#[cfg(not(feature = "sorted"))]
fn sorted_of(
    _: &Store,
    _: &crate::schema::Field,
    _: usize,
    _: Option<&str>,
) -> Result<SortedIndex> {
    Err(not_built("the index", "sorted"))
}

/// The inverted index over the sparse vectors of the field at `pos`, as
/// [`hash_of`].
fn sparse_of(store: &Store, pos: usize) -> Result<SparseIndex> {
    let mut ix = SparseIndex::new();
    for id in store.ids() {
        if let Some(Value::Sparse(_, e)) = store.read_field(id, pos)? {
            ix.insert(id, &e);
        }
    }
    ix.shrink_to_fit();
    Ok(ix)
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
        IndexKind::Hash { .. } => {
            let ix = Derived::new(hash_of(&c.store, pos, None)?);
            c.hashes.insert(field, ix);
        }
        IndexKind::Text(spec) => {
            let ix = Derived::new(text_of(&c.store, pos, spec)?);
            c.texts.insert(field, ix);
        }
        IndexKind::Sorted { .. } => {
            let ix = Derived::new(sorted_of(&c.store, &c.schema.fields[pos], pos, None)?);
            match c.sorted.iter_mut().find(|(n, _)| *n == field) {
                Some(slot) => slot.1 = ix,
                None => c.sorted.push((field, ix)),
            }
        }
        IndexKind::Inverted => {
            let ix = Derived::new(sparse_of(&c.store, pos)?);
            match c.sparse.iter_mut().find(|(n, _)| *n == field) {
                Some(slot) => slot.1 = ix,
                None => c.sparse.push((field, ix)),
            }
        }
        IndexKind::None => {}
    }
    Ok(())
}

/// Builds the index the schema declares on the path `path` into a json
/// field and fills it from the documents, as [`build_index`] does a
/// field's: a hash or an ordered index, the two a path takes.
fn build_path_index(c: &mut Collection, path: &str) -> Result<()> {
    let Some(f) = c.schema.path(path) else {
        return Ok(());
    };
    if !EVERY_INDEX && missing_feature(&f.index).is_some() {
        return Ok(());
    }
    let Some((pos, keys)) = source(&c.schema, path) else {
        return Ok(());
    };
    match f.index {
        IndexKind::Hash { .. } => {
            let ix = Derived::new(hash_of(&c.store, pos, keys)?);
            c.hashes.insert(path.to_string(), ix);
        }
        IndexKind::Sorted { .. } => {
            let ix = Derived::new(sorted_of(&c.store, f, pos, keys)?);
            match c.sorted.iter_mut().find(|(n, _)| *n == path) {
                Some(slot) => slot.1 = ix,
                None => in_schema_order(&mut c.sorted, &c.schema, path, ix),
            }
        }
        _ => {}
    }
    Ok(())
}

/// What of a statement has to be read as written for a json field: its
/// text, where a literal list of numbers is put into one or compared with a
/// path into one, and the parameters given there by their number (`$1` is
/// 0). A list of numbers alone is read into a vector's `f32`s by a reader
/// that has no schema -- FenecQL's lexer, the JSON reader of a query's
/// parameters, the browser module's vectors handed over apart -- which is
/// the quick way for the vector fields it nearly always is, and a json
/// field refuses (`Value::coerce`). A caller holding the text reads it again
/// as written (`fenec_ql::parse_exact`, `json::parse_params_exact`) where
/// this names it, and only then: a statement with no list of numbers costs
/// the walk of its literals, and one into a collection with no json field a
/// look at its fields.
#[derive(Debug, Default, PartialEq)]
pub struct Exactly {
    pub text: bool,
    pub params: Vec<usize>,
}

impl Exactly {
    pub fn is_needed(&self) -> bool {
        self.text || !self.params.is_empty()
    }
}

impl Database {
    /// The refusal of a statement that would hand a json field, or compare
    /// a path with, a vector where a list of numbers was written: read in
    /// the quick way it holds `f32`s, and a write would keep other numbers
    /// than those given and a comparison find none of the documents holding
    /// them -- a wrong answer believed right. A statement with no list of
    /// numbers, the one nearly every statement is, costs the walk of its
    /// literals.
    fn refuse_inexact(&self, stmt: &Statement, params: &[Value]) -> Result<()> {
        if !stmt.reads_vectors() && !params.iter().any(crate::query::holds_vector) {
            return Ok(());
        }
        let need = self.exactly(stmt);
        let param = need
            .params
            .iter()
            .any(|&i| params.get(i).is_some_and(crate::query::holds_vector));
        match need.text || param {
            false => Ok(()),
            true => Err(Error::Query(
                "a json field keeps a list of numbers as written, and this one was read into \
                 a vector's f32s: read the text with fenec_ql::parse_for, a parameter as JSON"
                    .into(),
            )),
        }
    }

    /// What of `stmt` a json field needs read as written ([`Exactly`]).
    pub fn exactly(&self, stmt: &Statement) -> Exactly {
        let mut out = match self.collection(stmt_collection(stmt)) {
            Ok(c) => exactly_for(&c.schema, stmt),
            Err(_) => Exactly::default(),
        };
        // Each `lookup` level's filter, by its own collection.
        if let Statement::Select(s) | Statement::Explain(s) = stmt {
            let mut level = s.lookup.as_ref();
            while let Some(l) = level {
                if let (Some(f), Ok(c)) = (&l.filter, self.collection(&l.collection)) {
                    filter_needs(&c.schema, f, &mut out);
                }
                level = l.next.as_deref();
            }
        }
        out
    }
}

/// The collection a statement [`exactly_for`] looks at is of, or `""`.
fn stmt_collection(stmt: &Statement) -> &str {
    match stmt {
        Statement::Put { collection, .. }
        | Statement::Update { collection, .. }
        | Statement::Delete { collection, .. } => collection,
        Statement::Select(s) | Statement::Explain(s) => &s.collection,
        _ => "",
    }
}

/// What of `stmt`, a statement over a collection of `schema`, a json field
/// needs read as written ([`Exactly`]).
pub fn exactly_for(schema: &Schema, stmt: &Statement) -> Exactly {
    let mut out = Exactly::default();
    if !schema.fields.iter().any(|f| f.ty == DataType::Json) {
        return out;
    }
    // The value first: a key is looked up only for a list of numbers or a
    // parameter, so a `put` of 1 000 rows looks up the vector field's name
    // and none of the others.
    let mut pair = |(k, e): &(String, Expr)| {
        if (matches!(e, Expr::Param(_)) || e.reads_vectors()) && names_json(schema, k) {
            given(e, &mut out);
        }
    };
    let filter = match stmt {
        Statement::Put { docs, .. } => {
            docs.iter().flatten().for_each(&mut pair);
            None
        }
        Statement::Update { set, filter, .. } => {
            set.iter().for_each(&mut pair);
            filter.as_ref()
        }
        Statement::Delete { filter, .. } => filter.as_ref(),
        Statement::Select(s) | Statement::Explain(s) => s.filter.as_ref(),
        _ => None,
    };
    if let Some(f) = filter {
        filter_needs(schema, f, &mut out);
    }
    out
}

/// Whether `name` is a json field of `schema` or a path into one.
fn names_json(schema: &Schema, name: &str) -> bool {
    // A path's field is a json one, or the path is refused where it is read.
    let field = name.split_once('.').map_or(name, |(f, _)| f);
    schema.field(field).is_some_and(|f| f.ty == DataType::Json)
}

/// What a value given a json field, or compared with one, needs.
fn given(e: &Expr, out: &mut Exactly) {
    match e {
        Expr::Param(i) => out.params.push(*i),
        e => out.text |= e.reads_vectors(),
    }
}

/// What a filter over a collection of `schema` needs ([`Exactly`]): each
/// value compared with a json field or a path into one.
fn filter_needs(schema: &Schema, filter: &Expr, out: &mut Exactly) {
    if !schema.fields.iter().any(|f| f.ty == DataType::Json) {
        return;
    }
    let json = |name: &str| names_json(schema, name);
    fn walk(e: &Expr, json: &dyn Fn(&str) -> bool, given: &mut dyn FnMut(&Expr)) {
        let field = |e: &Expr| matches!(e, Expr::Field(n) if json(n));
        match e {
            Expr::And(a, b) | Expr::Or(a, b) => {
                walk(a, json, given);
                walk(b, json, given);
            }
            Expr::Not(a) => walk(a, json, given),
            Expr::Cmp(_, a, b) | Expr::Has(a, b) => {
                if field(a) {
                    given(b);
                }
                if field(b) {
                    given(a);
                }
            }
            Expr::In(a, items) if field(a) => items.iter().for_each(given),
            _ => {}
        }
    }
    walk(filter, &json, &mut |e| given(e, out));
}

/// Sets `k` of `doc` to `v`: a field, coerced to its type, or a path into
/// a json field (`meta.lang: "en"`), the one key set inside it and the rest
/// kept, an object made where the path finds none.
fn set_field(schema: &Schema, doc: &mut Document, k: &str, v: Value) -> Result<()> {
    if let Some((pos, keys)) = schema.path_of(k)? {
        let f = &schema.fields[pos];
        let mut whole = doc.get(&f.name).cloned().unwrap_or(Value::Null);
        whole.set_path(&f.name, keys, v)?;
        doc.set(&f.name, whole.coerce(&f.ty)?);
        return Ok(());
    }
    let f = schema
        .field(k)
        .ok_or_else(|| Error::NotFound(format!("field `{k}` in collection `{}`", schema.name)))?;
    doc.set(k, v.coerce(&f.ty)?);
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
fn beam(ix: &VectorIndex, ef: Option<usize>, want: usize) -> String {
    let ef = ef.unwrap_or(ix.spec.ef_search);
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

/// A filter bound to the collection it tests rows of, once a query: each
/// field it compares with a value by position, and the value worked out.
/// A row is then decoded once for every field the filter reads
/// (`Store::read_fields`), and tested without looking the field up by
/// name or cloning the value, row after row. What it cannot bind -- a field
/// it does not know, a parameter not given, a call, two fields compared --
/// it evaluates as `eval` does, so every error comes where and as it came
/// before: at the first row tested, and never over no rows.
struct Filter<'q> {
    c: &'q Collection,
    root: Test<'q>,
    /// The places in a document of the fields the filter reads, ascending
    /// (`Schema::place`), and for each field position where its value lands
    /// among them.
    positions: Vec<usize>,
    index_of: Vec<usize>,
    /// The paths into json fields the filter reads, each its field's
    /// position and the keys past it: read a row after the fields, each
    /// its value off the bytes, into `vals` past theirs.
    paths: Vec<(usize, &'q str)>,
    /// A row's values: its fields', then its paths'.
    vals: std::cell::RefCell<Vec<Value>>,
    /// A row's value encoded, for a [`Test::InSet`] to look up.
    key: std::cell::RefCell<Vec<u8>>,
}

/// How long an `in` list is before a row is looked up in it rather than
/// compared with each value. An `in (get ...)` hands one of up to 100 000:
/// the 2 000 codes one handed a scan of 200 000 rows by an unindexed field
/// took 2 302 ms compared a value at a time, and take 16 looked up. Over
/// those rows 4 ints took 9.2 ms compared and 13.2 looked up, 8 took 11.6
/// and 11.1, 64 took 56.4 and 11.5; text pays sooner, 4 of it 20.2 against
/// 14.9.
const IN_SET_AT: usize = 8;

/// The values of an `in` over `slot` as the keys a hash index files them
/// under, when the list is long and a key's bytes say what `cmp_value`
/// says: every value of the type the field holds -- an int, a text, a
/// timestamp or a boolean, which a typed field's every row holds or
/// `null` -- so equal keys are equal values and no others are. A float
/// (`NaN` equals everything), a timestamp written as text, a json field or
/// a path is compared a value at a time, as before.
fn in_set(c: &Collection, slot: Slot, values: &[Value]) -> Option<HashIndex> {
    let ty = match slot {
        Slot::Id => DataType::Int,
        Slot::At(p) => c.schema.fields[p].ty.clone(),
        Slot::Path(_) => return None,
    };
    let mut set = HashIndex::default();
    for v in values {
        match (&ty, v) {
            (DataType::Int, Value::Int(_))
            | (DataType::Text, Value::Text(_))
            | (DataType::Timestamp, Value::Timestamp(_))
            | (DataType::Bool, Value::Bool(_)) => {}
            _ => return None,
        }
        let mut key = Vec::new();
        crate::codec::encode_value(&mut key, v);
        set.add(key, 0);
    }
    Some(set)
}

/// Where a bound test reads its field: the document's id, a field by its
/// position in the schema, or the `n`th of the filter's paths.
#[derive(Clone, Copy)]
enum Slot {
    Id,
    At(usize),
    Path(usize),
}

enum Test<'q> {
    And(Box<Test<'q>>, Box<Test<'q>>),
    Or(Box<Test<'q>>, Box<Test<'q>>),
    Not(Box<Test<'q>>),
    IsNull(Slot),
    /// The field against the value, the field on the left when `first`,
    /// and ordered in the field's collation when both are text.
    Cmp {
        op: CmpOp,
        slot: Slot,
        value: Value,
        first: bool,
        coll: Option<Collation>,
    },
    In(Slot, Vec<Value>),
    /// [`Test::In`] over a long list, its values made the keys a hash index
    /// files them under ([`in_set`]) when the first row is tested: made
    /// where the filter is bound, it was built for an `in` an index answers
    /// whole, and 2 000 ids took that query from 0.93 to 2.48 ms.
    InSet(Slot, Vec<Value>, std::cell::OnceCell<Option<HashIndex>>),
    Like(Slot, Value),
    Has(Slot, Value),
    Eval(&'q Expr),
}

impl<'q> Filter<'q> {
    fn new(c: &'q Collection, f: &'q Expr, ctx: &EvalCtx) -> Filter<'q> {
        let mut positions = Vec::new();
        let mut paths = Vec::new();
        let root = Filter::bind(c, f, ctx, &mut positions, &mut paths);
        let mut index_of = vec![usize::MAX; c.schema.fields.len()];
        for (i, p) in positions.iter_mut().enumerate() {
            index_of[*p] = i;
            // Its place in the payload, past a dropped field's, once a
            // query rather than once a row.
            *p = c.schema.place(*p);
        }
        Filter {
            c,
            root,
            positions,
            index_of,
            paths,
            vals: std::cell::RefCell::new(Vec::new()),
            key: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn bind(
        c: &Collection,
        e: &'q Expr,
        ctx: &EvalCtx,
        used: &mut Vec<usize>,
        paths: &mut Vec<(usize, &'q str)>,
    ) -> Test<'q> {
        let mut slot = |e: &'q Expr| match e {
            Expr::Field(name) if name == "id" => Some(Slot::Id),
            // A path is bound as a field is: its field's position and its
            // keys once a query, the keys walked a row. One the schema
            // cannot read is left to `eval`, which says why at the first
            // row.
            Expr::Field(name) if name.contains('.') => match c.schema.path_of(name) {
                Ok(Some(p)) => Some(Slot::Path(match paths.iter().position(|q| *q == p) {
                    Some(n) => n,
                    None => {
                        paths.push(p);
                        paths.len() - 1
                    }
                })),
                _ => None,
            },
            // Kept ascending as they come, a few at most: sorted after,
            // `usize` was a sort of its own, 3 KB of the browser module.
            Expr::Field(name) => c.schema.field_pos(name).map(|p| {
                let at = used.iter().position(|&q| q >= p).unwrap_or(used.len());
                if used.get(at) != Some(&p) {
                    used.insert(at, p);
                }
                Slot::At(p)
            }),
            _ => None,
        };
        let value = |e: &Expr| match e {
            Expr::Lit(v) => Some(v.clone()),
            Expr::Param(i) => ctx.params.get(*i).cloned(),
            _ => None,
        };
        let coll = |s: Slot| match s {
            Slot::At(p) => c.schema.fields[p].collate,
            Slot::Id | Slot::Path(_) => None,
        };
        match e {
            Expr::And(a, b) => Test::And(
                Box::new(Filter::bind(c, a, ctx, used, paths)),
                Box::new(Filter::bind(c, b, ctx, used, paths)),
            ),
            Expr::Or(a, b) => Test::Or(
                Box::new(Filter::bind(c, a, ctx, used, paths)),
                Box::new(Filter::bind(c, b, ctx, used, paths)),
            ),
            Expr::Not(a) => Test::Not(Box::new(Filter::bind(c, a, ctx, used, paths))),
            Expr::IsNull(a) => match slot(a) {
                Some(s) => Test::IsNull(s),
                None => Test::Eval(e),
            },
            Expr::Cmp(op, a, b) => match (slot(a), value(b), value(a), slot(b)) {
                (Some(s), Some(value), ..) => Test::Cmp {
                    op: *op,
                    slot: s,
                    value,
                    first: true,
                    coll: coll(s),
                },
                (_, _, Some(value), Some(s)) => Test::Cmp {
                    op: *op,
                    slot: s,
                    value,
                    first: false,
                    coll: coll(s),
                },
                _ => Test::Eval(e),
            },
            Expr::In(a, items) => {
                let mut values = Vec::with_capacity(items.len());
                for it in items {
                    match value(it) {
                        Some(v) => values.push(v),
                        None => return Test::Eval(e),
                    }
                }
                match slot(a) {
                    Some(s) if values.len() >= IN_SET_AT => {
                        Test::InSet(s, values, Default::default())
                    }
                    Some(s) => Test::In(s, values),
                    None => Test::Eval(e),
                }
            }
            Expr::Like(a, b) => match (slot(a), value(b)) {
                (Some(s), Some(v)) => Test::Like(s, v),
                _ => Test::Eval(e),
            },
            Expr::Has(a, b) => match (slot(a), value(b)) {
                (Some(s), Some(v)) => Test::Has(s, v),
                _ => Test::Eval(e),
            },
            _ => Test::Eval(e),
        }
    }

    /// Whether the stored row `id` passes the filter; `ctx` is what the
    /// filter was bound with, for what it evaluates as `eval` does.
    ///
    /// Inlined into its scans natively: left to the compiler, the paths'
    /// branch beside a field's read had it out of line, and a scan of
    /// 100 000 rows by a text field took 3.05 -> 3.17 ms; inlined, 3.07,
    /// and one by two numbers 3.47 -> 3.36.
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    fn matches(&self, id: DocId, ctx: &EvalCtx) -> Result<bool> {
        let mut vals = self.vals.borrow_mut();
        if !self.positions.is_empty()
            && !self.c.store.read_fields(id, &self.positions, &mut vals)?
        {
            vals.clear();
            vals.resize(self.positions.len(), Value::Null);
        }
        // A filter over no path reads none, and asks no more than this,
        // the reading out of line so that what is inlined into every scan
        // stays the size it was. Its paths' values go after its fields',
        // which `read_fields` leaves where they are.
        if !self.paths.is_empty() {
            self.read_paths(id, &mut vals)?;
        }
        self.test(&self.root, id, &vals, &Value::Int(id as i64), ctx)
    }

    #[inline(never)]
    fn read_paths(&self, id: DocId, vals: &mut Vec<Value>) -> Result<()> {
        let n = self.positions.len();
        vals.resize(n + self.paths.len(), Value::Null);
        if !self.c.store.read_paths(id, &self.paths, &mut vals[n..])? {
            vals[n..].fill(Value::Null);
        }
        Ok(())
    }

    fn test(
        &self,
        t: &Test,
        id: DocId,
        vals: &[Value],
        idv: &Value,
        ctx: &EvalCtx,
    ) -> Result<bool> {
        let get = |s: Slot| match s {
            Slot::Id => idv,
            Slot::At(p) => &vals[self.index_of[p]],
            Slot::Path(n) => &vals[self.positions.len() + n],
        };
        Ok(match t {
            Test::And(a, b) => {
                self.test(a, id, vals, idv, ctx)? && self.test(b, id, vals, idv, ctx)?
            }
            Test::Or(a, b) => {
                self.test(a, id, vals, idv, ctx)? || self.test(b, id, vals, idv, ctx)?
            }
            Test::Not(a) => !self.test(a, id, vals, idv, ctx)?,
            Test::IsNull(s) => get(*s).is_null(),
            Test::Cmp {
                op,
                slot,
                value,
                first,
                coll,
            } => {
                let f = get(*slot);
                let (l, r) = if *first { (f, value) } else { (value, f) };
                compare(*op, l, r, &|| *coll)
            }
            Test::In(s, values) => {
                let f = get(*s);
                values.iter().any(|v| v.cmp_value(f) == Ordering::Equal)
            }
            Test::InSet(s, values, set) => {
                let f = get(*s);
                match set.get_or_init(|| in_set(self.c, *s, values)) {
                    Some(set) => {
                        let mut key = self.key.borrow_mut();
                        key.clear();
                        crate::codec::encode_value(&mut key, f);
                        !f.is_null() && set.get(&key).is_some()
                    }
                    None => values.iter().any(|v| v.cmp_value(f) == Ordering::Equal),
                }
            }
            Test::Like(s, v) => match (get(*s).as_text(), v.as_text()) {
                (Some(hay), Some(needle)) => like_match(hay, needle),
                _ => false,
            },
            Test::Has(s, v) => match get(*s) {
                Value::List(items) => items.iter().any(|i| i.cmp_value(v) == Ordering::Equal),
                _ => false,
            },
            Test::Eval(e) => {
                let mut row = StoreRow {
                    store: &self.c.store,
                    schema: &self.c.schema,
                    id,
                };
                truthy(&eval(e, &mut row, ctx)?)
            }
        })
    }
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

/// Sort keys: a field position (`None` for `id`), whether it ascends, the
/// collation its text is compared in, and the keys past the field of a
/// path into a json one.
type OrderKey = (Option<usize>, bool, Option<Collation>, Option<Box<str>>);

/// The key `s` names in `schema`. `collate` is refused on anything but text:
/// on a number it would claim an order it does not change. `owner` goes in
/// front of the field's name in an error -- `reviews.` for a `lookup`'s.
fn order_key(schema: &Schema, s: &Sort, owner: &str) -> Result<OrderKey> {
    let f = &s.field;
    let (pos, keys) = match f == "id" {
        true => (None, None),
        false => {
            let (p, keys) = source_or_err(schema, f, owner)?;
            (Some(p), keys.map(Box::from))
        }
    };
    if let Some(c) = s.collate {
        let ty = match (pos, &keys) {
            // A path's values have no type: a text among them orders in
            // the collation, and the rest as they always do.
            (_, Some(_)) => &DataType::Text,
            (Some(p), None) => &schema.fields[p].ty,
            (None, _) => &DataType::Int,
        };
        if !collatable(ty) {
            return Err(Error::Query(format!(
                "`collate {}` orders text; `{owner}{f}` is {}",
                c.name(),
                ty.name()
            )));
        }
    }
    // A field in a collation orders in it unless the query names one.
    let field = pos
        .filter(|_| keys.is_none())
        .and_then(|p| schema.fields[p].collate);
    Ok((pos, s.asc, s.collate.or(field), keys))
}

/// The order `order` asks for between two rows' keys, before any tie-break.
fn rank(keys: &[OrderKey], a: &[Value], b: &[Value]) -> Ordering {
    for (i, (_, asc, collate, _)) in keys.iter().enumerate() {
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
        for (pos, _, _, path) in keys {
            flat.push(match (pos, path) {
                (None, _) => Value::Int(id as i64),
                (Some(p), None) => store.read_field(id, *p)?.unwrap_or(Value::Null),
                (Some(p), Some(k)) => store.read_path(id, *p, k)?.unwrap_or(Value::Null),
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
/// The text form (`near embed '[1,2,3]'`) is pgvector's notation, and a
/// string is what most clients send. On the parameter path the same
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
