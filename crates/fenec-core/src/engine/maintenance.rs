//! `create index` and `compact` beside the database rather than in its way.
//!
//! Both are builds from the documents: an index from one field of each, a
//! compacted collection from the live ones and every index over them. Run
//! by [`Database::execute`], the whole build holds the write lock, and every
//! reader and writer waits it out -- ~10 s for an HNSW index over 100 000 x
//! 128. [`Database::maintain`] copies what the build reads under the read
//! lock, builds with no lock held, and takes the write lock only to catch up
//! with the writes made meanwhile and put the result in place.
//!
//! Those writes are known exactly, not guessed from the change ring (which
//! a long build would overflow): every write passes through
//! `Database::note`, which hands the id to the [`Tail`] of each maintenance
//! running on that collection. A schema change there -- another index, a
//! drop -- leaves what was built not fitting, and the maintenance says so
//! rather than putting it in place.

use super::*;
use std::sync::RwLock;

/// The writes a maintenance has to catch up with.
pub(super) struct Tail {
    token: u64,
    cid: u32,
    ids: Vec<DocId>,
    /// The collection's schema changed, or it was dropped.
    schema: bool,
}

#[derive(Default)]
pub(super) struct Tails {
    next: u64,
    pub(super) list: Vec<Tail>,
}

impl Tails {
    pub(super) fn note(&mut self, cid: u32, id: DocId) {
        for t in self.list.iter_mut().filter(|t| t.cid == cid) {
            if id == SCHEMA_MARK {
                t.schema = true;
            } else {
                t.ids.push(id);
            }
        }
    }
}

/// An index's field, copied: `(id, value)` in id order.
struct IndexCopy {
    token: u64,
    cid: u32,
    collection: String,
    field: String,
    pos: usize,
    kind: IndexKind,
    ty: DataType,
    /// The field's collation, which a `@sorted` index orders its text in.
    collate: Option<Collation>,
    values: Vec<(DocId, Option<Value>)>,
}

enum Built {
    Vector(VectorIndex),
    Hash(HashIndex),
    Text(TextIndex),
    Sorted(SortedIndex),
    Sparse(SparseIndex),
}

/// The index built from the copy, and the copy: taking a write back out of
/// a hash, text or ordered index needs the value it went in with.
struct BuiltIndex {
    copy: IndexCopy,
    index: Built,
}

/// A collection being compacted: its live documents in a fresh store, and
/// once built, every index over them.
struct Part {
    token: u64,
    name: String,
    collection: Collection,
    reclaimed: usize,
}

/// Takes the maintenances' tails off the list however the run ends: on an
/// error or a panic in the build, the write path would otherwise go on
/// collecting ids for nobody.
struct Watching<'a> {
    db: &'a RwLock<Database>,
    tokens: Vec<u64>,
}

impl Drop for Watching<'_> {
    fn drop(&mut self) {
        if self.tokens.is_empty() {
            return;
        }
        let mut g = self.db.write().unwrap_or_else(|e| e.into_inner());
        for t in &self.tokens {
            g.unwatch(*t);
        }
    }
}

fn read(db: &RwLock<Database>) -> std::sync::RwLockReadGuard<'_, Database> {
    db.read().unwrap_or_else(|e| e.into_inner())
}

fn write(db: &RwLock<Database>) -> std::sync::RwLockWriteGuard<'_, Database> {
    db.write().unwrap_or_else(|e| e.into_inner())
}

impl Database {
    /// Runs a `create index` or a `compact` beside the database: the build
    /// holds no lock, and the write lock is taken only to catch up with the
    /// writes made during it and put the result in place. `None` for any
    /// other statement.
    pub fn maintain(db: &RwLock<Database>, stmt: &Statement) -> Option<Result<Response>> {
        Self::maintain_with(db, stmt, &mut || {})
    }

    /// [`Self::maintain`], calling `during` once the copy is taken and
    /// before the build, with no lock held: how a test makes its writes
    /// land while a build runs, every time rather than when the timing
    /// happens to allow.
    #[doc(hidden)]
    pub fn maintain_with(
        db: &RwLock<Database>,
        stmt: &Statement,
        during: &mut dyn FnMut(),
    ) -> Option<Result<Response>> {
        Some(match stmt {
            Statement::CreateIndex {
                collection,
                field,
                kind,
                if_not_exists,
            } => index_online(db, collection, field, kind, *if_not_exists, during),
            Statement::Compact(which) => compact_online(db, which.as_deref(), during),
            _ => return None,
        })
    }

    fn watch(&self, cid: u32) -> u64 {
        let mut t = self.tails.lock().unwrap_or_else(|e| e.into_inner());
        t.next += 1;
        let token = t.next;
        t.list.push(Tail {
            token,
            cid,
            ids: Vec::new(),
            schema: false,
        });
        // Set under the read lock, read under the write lock: the lock
        // orders the two, so no stronger ordering is needed.
        self.watched
            .store(true, std::sync::atomic::Ordering::Relaxed);
        token
    }

    fn unwatch(&mut self, token: u64) -> Option<Tail> {
        let t = self.tails.get_mut().unwrap_or_else(|e| e.into_inner());
        let at = t.list.iter().position(|x| x.token == token)?;
        let tail = t.list.swap_remove(at);
        *self.watched.get_mut() = !t.list.is_empty();
        Some(tail)
    }

    /// Checks a `create index`: `Some` answers it without building anything.
    pub(super) fn check_index(
        &self,
        collection: &str,
        field: &str,
        kind: &IndexKind,
        if_not_exists: bool,
    ) -> Result<Option<Response>> {
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
                return Ok(Some(Response::Ok(format!("`{field}` is already indexed"))));
            }
            return Err(Error::Exists(format!("an index on field `{field}`")));
        }
        kind.check(field, &f.ty)?;
        Ok(None)
    }

    /// What `execute_with` refuses before a write, for a maintenance that
    /// does not go through it.
    fn may_write(&self, stmt_is_compact: bool) -> Result<()> {
        self.refuse_if_failed()?;
        if self.history.following && !stmt_is_compact {
            return Err(Error::ReadOnly(
                "this database is a replica: its writes come from its primary".into(),
            ));
        }
        Ok(())
    }

    fn begin_index(
        &self,
        collection: &str,
        field: &str,
        kind: &IndexKind,
        if_not_exists: bool,
    ) -> Result<std::result::Result<IndexCopy, Response>> {
        self.may_write(false)?;
        if let Some(done) = self.check_index(collection, field, kind, if_not_exists)? {
            return Ok(Err(done));
        }
        let c = &self.collections[collection];
        let pos = c.schema.field_pos(field).unwrap();
        let mut values = Vec::with_capacity(c.store.len());
        for id in c.store.iter_ids() {
            values.push((id, c.store.read_field(id, pos)?));
        }
        Ok(Ok(IndexCopy {
            token: self.watch(c.id),
            cid: c.id,
            collection: collection.to_string(),
            field: field.to_string(),
            pos,
            kind: kind.resolved(),
            ty: c.schema.fields[pos].ty.clone(),
            collate: c.schema.fields[pos].collate,
            values,
        }))
    }

    fn finish_index(&mut self, b: BuiltIndex) -> Result<Response> {
        let BuiltIndex { copy, index } = b;
        let tail = self.unwatch(copy.token);
        self.refuse_if_failed()?;
        let changed = || {
            Error::Query(format!(
                "`{}` changed while the index was built; run `create index` again",
                copy.collection
            ))
        };
        let tail = tail.ok_or_else(changed)?;
        if tail.schema || self.named(copy.cid).as_deref() != Some(&copy.collection) {
            return Err(changed());
        }
        let c = self.collections.get_mut(&copy.collection).unwrap();
        let pos = copy.pos;
        let mut ids = tail.ids;
        ids.sort_unstable();
        ids.dedup();
        let old = |id: DocId| {
            copy.values
                .binary_search_by_key(&id, |v| v.0)
                .ok()
                .and_then(|k| copy.values[k].1.as_ref())
        };
        // The writes made during the build, applied as the write path would
        // have: out with the value the build saw, in with the one there now.
        match index {
            Built::Vector(mut ix) => {
                // `insert` keeps a node that holds the vector already, and
                // retires it otherwise.
                for id in ids {
                    match c.store.read_field(id, pos)? {
                        Some(Value::Vector(v)) => ix.insert(id, &v),
                        _ => ix.remove(id),
                    }
                }
                c.vectors.insert(copy.field.clone(), ix);
            }
            Built::Hash(mut ix) => {
                for id in ids {
                    if let Some(v) = old(id) {
                        ix.remove(&hash_key(v), id);
                    }
                    if let Some(v) = c.store.read_field(id, pos)? {
                        ix.add(hash_key(&v), id);
                    }
                }
                c.hashes.insert(copy.field.clone(), ix);
            }
            Built::Text(mut ix) => {
                for id in ids {
                    if let Some(Value::Text(t)) = old(id) {
                        ix.remove(id, t);
                    }
                    if let Some(Value::Text(t)) = c.store.read_field(id, pos)? {
                        ix.insert(id, &t);
                    }
                }
                c.texts.insert(copy.field.clone(), ix);
            }
            Built::Sorted(mut ix) => {
                for id in ids {
                    ix.remove(id, old(id));
                    if let Some(v) = c.store.read_field(id, pos)? {
                        ix.insert(id, Some(&v));
                    }
                }
                c.sorted.push((copy.field.clone(), ix));
            }
            Built::Sparse(mut ix) => {
                for id in ids {
                    if let Some(Value::Sparse(_, e)) = old(id) {
                        ix.remove(id, e);
                    }
                    if let Some(Value::Sparse(_, e)) = c.store.read_field(id, pos)? {
                        ix.insert(id, &e);
                    }
                }
                c.sparse.push((copy.field.clone(), ix));
            }
        }
        c.schema.fields[pos].index = copy.kind;
        let (cid, encoded) = (c.id, c.schema.encode());
        self.wal(REC_ALTER, cid, &encoded)?;
        self.note(cid, SCHEMA_MARK);
        if let Some(w) = &self.watcher {
            w.notify(self.changes.seq());
        }
        Ok(Response::Ok(format!(
            "index built on `{}.{}`",
            copy.collection, copy.field
        )))
    }

    fn begin_compact(&self, which: Option<&str>) -> Result<Vec<Part>> {
        self.may_write(true)?;
        let targets: Vec<String> = match which {
            Some(n) => {
                self.collection(n)?;
                vec![n.to_string()]
            }
            None => self.order.clone(),
        };
        let mut parts = Vec::with_capacity(targets.len());
        for name in targets {
            let c = &self.collections[&name];
            let mut fresh = Collection::new(c.id, c.schema.clone());
            fresh.store = c.store.compacted()?;
            parts.push(Part {
                token: 0,
                name,
                collection: fresh,
                reclaimed: c.store.dead_bytes(),
            });
        }
        // Watched only once every copy is taken: a copy that failed leaves
        // no tail behind.
        for p in &mut parts {
            p.token = self.watch(p.collection.id);
        }
        Ok(parts)
    }

    fn finish_compact(&mut self, parts: Vec<Part>) -> Result<Response> {
        let tails: Vec<Option<Tail>> = parts.iter().map(|p| self.unwatch(p.token)).collect();
        self.refuse_if_failed()?;
        for (p, t) in parts.iter().zip(&tails) {
            let moved = t.as_ref().is_none_or(|t| t.schema);
            if moved || self.named(p.collection.id).as_deref() != Some(&p.name) {
                return Err(Error::Query(format!(
                    "`{}` changed while it was compacted; run `compact` again",
                    p.name
                )));
            }
        }
        let mut reclaimed = 0usize;
        for (mut p, t) in parts.into_iter().zip(tails) {
            let live = &self.collections[&p.name];
            let c = &mut p.collection;
            let mut ids = t.unwrap().ids;
            ids.sort_unstable();
            ids.dedup();
            // Each document written during the build takes the state it has
            // now, over the one the copy holds.
            for id in ids {
                let old = c.store.read(&c.schema, id)?;
                let now = live.store.read(&live.schema, id)?;
                if let Some(old) = &old {
                    c.unindex_doc(old, now.as_ref());
                }
                match now {
                    Some(doc) => {
                        c.store
                            .append(OP_PUT, id, &Store::encode_doc(&c.schema, &doc));
                        c.index_doc(&doc, old.as_ref());
                    }
                    None if c.store.contains(id) => {
                        c.store.append(OP_DEL, id, &[]);
                    }
                    None => {}
                }
            }
            // An id handed out and deleted during the build left no record
            // in the copy; it must not come back.
            c.store.raise_next_id(live.store.next_id());
            reclaimed += p.reclaimed;
            self.collections.insert(p.name, p.collection);
        }
        // After compaction the persisted image is rewritten from scratch,
        // streamed into the new file rather than built beside the data.
        let r = {
            let mut sink = self.sink.lock().unwrap_or_else(|e| e.into_inner());
            sink.rewrite_with(&mut |out| self.image_into(out, &[], &mut Vec::new()))
        };
        self.storage(r)?;
        Ok(Response::Ok(format!(
            "compaction done, {reclaimed} bytes reclaimed"
        )))
    }

    /// The graphs a compact of a mapped database rebuilds: those of `which`
    /// holding tombstones, their vectors copied as `create index` copies
    /// them. Nothing else is: the records stay in the file until the
    /// rewrite streams the live ones into the new one.
    #[cfg(not(target_arch = "wasm32"))]
    fn begin_graphs(&self, which: Option<&str>) -> Result<Vec<IndexCopy>> {
        self.may_write(true)?;
        let targets: Vec<String> = match which {
            Some(n) => {
                self.collection(n)?;
                vec![n.to_string()]
            }
            None => self.order.clone(),
        };
        let mut copies = Vec::new();
        for name in targets {
            let c = &self.collections[&name];
            for (field, ix) in &c.vectors {
                if ix.dead() == 0 {
                    continue;
                }
                let pos = c.schema.field_pos(field).unwrap();
                let mut values = Vec::with_capacity(c.store.len());
                for id in c.store.iter_ids() {
                    values.push((id, c.store.read_field(id, pos)?));
                }
                copies.push(IndexCopy {
                    token: 0,
                    cid: c.id,
                    collection: name.clone(),
                    field: field.clone(),
                    pos,
                    kind: c.schema.fields[pos].index.clone(),
                    ty: c.schema.fields[pos].ty.clone(),
                    collate: None,
                    values,
                });
            }
        }
        for copy in &mut copies {
            copy.token = self.watch(copy.cid);
        }
        Ok(copies)
    }

    /// Puts the rebuilt graphs in place, each caught up with the writes
    /// made while it was built, and runs the compact itself: the live
    /// records streamed into the new file and the stores pointed at it.
    #[cfg(not(target_arch = "wasm32"))]
    fn finish_graphs(&mut self, which: Option<&str>, built: Vec<BuiltIndex>) -> Result<Response> {
        let tails: Vec<Option<Tail>> = built.iter().map(|b| self.unwatch(b.copy.token)).collect();
        self.refuse_if_failed()?;
        for (b, t) in built.into_iter().zip(tails) {
            let moved = t.as_ref().is_none_or(|t| t.schema);
            if moved || self.named(b.copy.cid).as_deref() != Some(&b.copy.collection) {
                return Err(Error::Query(format!(
                    "`{}` changed while it was compacted; run `compact` again",
                    b.copy.collection
                )));
            }
            let Built::Vector(mut ix) = b.index else {
                unreachable!("only graphs are rebuilt beside a mapped compact")
            };
            let c = self.collections.get_mut(&b.copy.collection).unwrap();
            let mut ids = t.unwrap().ids;
            ids.sort_unstable();
            ids.dedup();
            for id in ids {
                ix.remove(id);
                if let Some(Value::Vector(v)) = c.store.read_field(id, b.copy.pos)? {
                    ix.insert(id, &v);
                }
            }
            c.vectors.insert(b.copy.field, ix);
        }
        // The writes made meanwhile may have left the new graphs a tombstone
        // or two; those wait for the next compact rather than having the
        // graph rebuilt again under the lock.
        self.compact(which, false)
    }
}

impl IndexCopy {
    fn build(mut self) -> BuiltIndex {
        let index = match &self.kind {
            IndexKind::Vector(spec) => {
                let DataType::Vector(dim, prec) = self.ty else {
                    unreachable!("checked when the copy was taken")
                };
                let mut ix = VectorIndex::with_precision(dim, *spec, prec);
                // A vector comes out of the graph by its id alone, so the
                // copy gives them up rather than holding a second set.
                let items: Vec<(DocId, Vec<f32>)> = self
                    .values
                    .iter_mut()
                    .filter_map(|(id, v)| match v.take() {
                        Some(Value::Vector(v)) => Some((*id, v)),
                        _ => None,
                    })
                    .collect();
                ix.reserve(items.len());
                ix.insert_batch(&items);
                Built::Vector(ix)
            }
            IndexKind::Hash => {
                let mut ix = HashIndex::default();
                for (id, v) in &self.values {
                    if let Some(v) = v {
                        ix.add(hash_key(v), *id);
                    }
                }
                Built::Hash(ix)
            }
            IndexKind::Text(spec) => {
                let mut ix = TextIndex::new(*spec);
                for (id, v) in &self.values {
                    if let Some(Value::Text(t)) = v {
                        ix.insert(*id, t);
                    }
                }
                ix.shrink_to_fit();
                Built::Text(ix)
            }
            IndexKind::Sorted => Built::Sorted(SortedIndex::build(
                &self.ty,
                self.collate,
                &mut self.values.iter().map(|(id, v)| (*id, v.clone())),
            )),
            IndexKind::Inverted => {
                let mut ix = SparseIndex::new();
                for (id, v) in &self.values {
                    if let Some(Value::Sparse(_, e)) = v {
                        ix.insert(*id, e);
                    }
                }
                ix.shrink_to_fit();
                Built::Sparse(ix)
            }
            IndexKind::None => unreachable!("a create index names its kind"),
        };
        BuiltIndex { copy: self, index }
    }
}

fn index_online(
    db: &RwLock<Database>,
    collection: &str,
    field: &str,
    kind: &IndexKind,
    if_not_exists: bool,
    during: &mut dyn FnMut(),
) -> Result<Response> {
    let copy = match read(db).begin_index(collection, field, kind, if_not_exists)? {
        Ok(copy) => copy,
        Err(done) => return Ok(done),
    };
    let mut watching = Watching {
        db,
        tokens: vec![copy.token],
    };
    during();
    let built = copy.build();
    let r = write(db).finish_index(built);
    watching.tokens.clear();
    r
}

fn compact_online(
    db: &RwLock<Database>,
    which: Option<&str>,
    during: &mut dyn FnMut(),
) -> Result<Response> {
    // A mapped database copies no record: the graphs holding tombstones are
    // rebuilt here, beside it, and the rewrite -- the live records streamed
    // from the old file into the new one, 2.1 s for 1 GB -- is left to the
    // write lock. Copying every record into a fresh store, as the rest of
    // this does, took a 427 MB file to +899 MB of heap and left the
    // collection in memory until the process ended.
    #[cfg(not(target_arch = "wasm32"))]
    if read(db).mapped {
        let copies = read(db).begin_graphs(which)?;
        let mut watching = Watching {
            db,
            tokens: copies.iter().map(|c| c.token).collect(),
        };
        during();
        let built: Vec<BuiltIndex> = copies.into_iter().map(IndexCopy::build).collect();
        let r = write(db).finish_graphs(which, built);
        watching.tokens.clear();
        return r;
    }
    let mut parts = read(db).begin_compact(which)?;
    let mut watching = Watching {
        db,
        tokens: parts.iter().map(|p| p.token).collect(),
    };
    during();
    for p in &mut parts {
        let c = &mut p.collection;
        for pos in 0..c.schema.fields.len() {
            build_index(c, pos)?;
        }
    }
    let r = write(db).finish_compact(parts);
    watching.tokens.clear();
    r
}
