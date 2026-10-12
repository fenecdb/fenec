//! A block's documents built before the write lock, beside the readers.
//!
//! A block holds the write lock from its first write to its record, and a
//! reader waits it out: four threads reading by id beside blocks of 1 000
//! puts landing back to back read 138-213k rows a second, the longest wait
//! 4.8 ms, where SQLite's WAL reads the last commit meanwhile (`make
//! concurrency-bench`). Of a put's time in such a block, what the database
//! as the write lock holds it has no part in -- the document built from
//! the statement and its parameters, each value made its field's type, the
//! payload encoded, and the document let go of once written -- was about
//! half. [`Database::prepare`] does that much under the read lock, beside
//! the readers, and the block runs with it ([`Database::execute_prepared`],
//! [`Database::execute_ready`]): a `put` takes the documents and their
//! payloads where its collection still holds the schema they were built
//! against, and builds its own otherwise, as it always did. Everything the
//! database decides -- the id, an insert over a held id, `@unique`, the
//! hooks, the indexes, the record -- is decided under the write lock as
//! before, so the block writes what it wrote, byte for byte.
//!
//! Native only: the browser module has one thread, and no reader to let
//! in.

use super::*;

/// A block's statements as [`Database::prepare`] left them: each `put`'s
/// documents, built and encoded, in one buffer for the whole block -- a
/// statement's own buffers were three allocations a statement, and a block
/// of 1 000 single puts spent a quarter of what it saved on them -- and
/// once the block has run, the documents it wrote, for the caller to let
/// go of after the lock.
#[derive(Default)]
pub struct Prepared {
    /// Per statement, the documents of a `put` whose were built.
    stmts: Vec<Option<Built>>,
    /// The schemas they were built against, a collection's once, held: a
    /// change to the collection's copies it, so a `put` that finds its
    /// collection's the same knows nothing changed ([`SchemaCell`]).
    /// Held a statement each, its count's two atomic steps were 2% of a
    /// block of single puts.
    schemas: Vec<(u32, SchemaCell)>,
    docs: Vec<Document>,
    /// Each document's payload as [`Store::encode_doc`] writes it, end to
    /// end, and where each ends.
    payloads: Vec<u8>,
    ends: Vec<usize>,
    /// What the block wrote, let go of with the preparation.
    spent: Vec<Document>,
}

/// One `put`'s documents in a [`Prepared`]: which of its schemas they
/// were built against, and where they are among its documents.
struct Built {
    schema: usize,
    first: usize,
    count: usize,
}

/// The documents a block's `put`s write at least for them to be built
/// before the write lock ([`Prepared::worth`]): a preparation is a read
/// lock taken and let go and its buffers, which a statement of a document
/// or two would spend more on than it takes off the lock.
pub const PREPARE_AT: usize = 16;

impl Prepared {
    /// Whether `stmts` write enough documents to be worth preparing:
    /// [`PREPARE_AT`] at least, among their `put`s' pairs and the objects
    /// their parameters hold.
    pub fn worth(stmts: &[(&Statement, &[Value])]) -> bool {
        let mut n = 0;
        for (s, params) in stmts {
            if let Statement::Put {
                docs, docs_param, ..
            } = s
            {
                n += docs.len();
                n += match docs_param.and_then(|i| params.get(i)) {
                    Some(Value::List(l)) => l.len(),
                    Some(_) => 1,
                    None => 0,
                };
                if n >= PREPARE_AT {
                    return true;
                }
            }
        }
        false
    }

    /// How many statements had their documents built.
    pub fn built(&self) -> usize {
        self.stmts.iter().filter(|b| b.is_some()).count()
    }

    /// How many of the documents built a block took and wrote.
    pub fn spent(&self) -> usize {
        self.spent.len()
    }

    /// The `i`th document's payload, of the whole block.
    pub(super) fn payload(&self, i: usize) -> Option<&[u8]> {
        let end = *self.ends.get(i)?;
        let start = match i {
            0 => 0,
            _ => self.ends[i - 1],
        };
        self.payloads.get(start..end)
    }

    /// Keeps the documents a `put` wrote, to be let go of once the lock
    /// is: 10% of a put's time in a block of 1 000 was freeing its
    /// document's names and values.
    pub(super) fn spend(&mut self, written: &mut Vec<Document>) {
        self.spent.append(written);
    }
}

impl Database {
    /// Builds and encodes the documents of each `put` among `stmts`, as it
    /// would under the write lock, for the block to take
    /// ([`Self::execute_prepared`]): under the read lock, beside the
    /// readers. A statement that is not a `put`, names a collection that is
    /// not here, or whose documents fail to build, is left to build as it
    /// runs -- where it fails as it always did.
    pub fn prepare(&self, stmts: &[(&Statement, &[Value])]) -> Prepared {
        // A document a statement, as a `/batch` of single puts has, and
        // its payload a few dozen bytes: grown from nothing, the buffers'
        // copies were 1% of a block of 1 000.
        let mut p = Prepared {
            stmts: Vec::with_capacity(stmts.len()),
            docs: Vec::with_capacity(stmts.len()),
            payloads: Vec::with_capacity(stmts.len() * 32),
            ends: Vec::with_capacity(stmts.len()),
            ..Prepared::default()
        };
        for (s, params) in stmts {
            let b = self.build(s, params, &mut p);
            p.stmts.push(b);
        }
        p
    }

    fn build(&self, stmt: &Statement, params: &[Value], p: &mut Prepared) -> Option<Built> {
        let Statement::Put {
            collection,
            docs,
            docs_param,
            ..
        } = stmt
        else {
            return None;
        };
        let c = self.collections.get(collection)?;
        // A statement a json field's list of numbers refuses
        // (`refuse_inexact`) is refused as it runs, before its documents
        // are taken: asked here as well, the walk was 1% of a block.
        let ctx = EvalCtx {
            params,
            registry: &self.registry,
            clock: self.clock,
        };
        let first = p.docs.len();
        if build_docs(&c.schema, collection, docs, *docs_param, &ctx, &mut p.docs).is_err() {
            p.docs.truncate(first);
            return None;
        }
        for d in &p.docs[first..] {
            Store::encode_doc_into(&c.schema, d, &mut p.payloads);
            p.ends.push(p.payloads.len());
        }
        let schema = match p.schemas.iter().position(|(cid, _)| *cid == c.id) {
            Some(at) => at,
            None => {
                p.schemas.push((c.id, c.schema.clone()));
                p.schemas.len() - 1
            }
        };
        Some(Built {
            schema,
            first,
            count: p.docs.len() - first,
        })
    }

    /// [`Self::execute_block`] with each `put`'s documents as
    /// [`Self::prepare`] built them. `prepared` holds, after, what the
    /// block wrote, to be let go of once the lock is.
    pub fn execute_prepared(
        &mut self,
        stmts: &[(&Statement, &[Value])],
        prepared: &mut Prepared,
    ) -> std::result::Result<Vec<Response>, (usize, Error)> {
        self.preparation = std::mem::take(prepared);
        self.armed = true;
        let out = self.execute_block(stmts);
        *prepared = std::mem::take(&mut self.preparation);
        out
    }

    /// [`Self::execute_with`] for statement `i` of those `prepared` was
    /// made for, with its documents as [`Self::prepare`] built them: a
    /// `/batch` whose statements run one at a time under the lock.
    /// `prepared` holds, after, what it wrote, to be let go of once the
    /// lock is.
    pub fn execute_ready(
        &mut self,
        stmt: &Statement,
        params: &[Value],
        prepared: &mut Prepared,
        i: usize,
    ) -> Result<Response> {
        self.preparation = std::mem::take(prepared);
        self.offer(i);
        self.armed = true;
        let out = self.execute_with(stmt, params);
        self.ready_at = None;
        *prepared = std::mem::take(&mut self.preparation);
        out
    }

    /// As [`Self::execute_block`] and [`Self::execute_with`] start: the
    /// preparation is theirs only where one of the two above handed it over
    /// just now. Any other is one a panic left half way, and let go of, so
    /// that no statement takes the documents built for another.
    pub(super) fn disarm(&mut self) {
        if !std::mem::take(&mut self.armed)
            && (self.ready_at.is_some() || !self.preparation.stmts.is_empty())
        {
            self.ready_at = None;
            self.preparation = Prepared::default();
        }
    }

    /// Statement `i` of a block run with a preparation is the one running.
    pub(super) fn offer(&mut self, i: usize) {
        self.ready_at = match self.preparation.stmts.is_empty() {
            true => None,
            false => Some(i),
        };
    }

    /// The documents prepared for the `put` running, where they were built
    /// against `cid`'s schema as it stands: where they start among the
    /// block's, and how many. Taken, so a second `put` of one statement
    /// builds its own.
    pub(super) fn take_ready(&mut self, cid: u32, schema: &SchemaCell) -> Option<(usize, usize)> {
        let i = self.ready_at.take()?;
        let p = &self.preparation;
        let b = p.stmts.get(i)?.as_ref()?;
        let (held, built) = p.schemas.get(b.schema)?;
        match *held == cid && Arc::ptr_eq(built, schema) {
            true => Some((b.first, b.count)),
            false => None,
        }
    }

    /// The prepared documents from `first`, `count` of them, taken out of
    /// the preparation for the `put` to write.
    pub(super) fn take_built(&mut self, first: usize, count: usize) -> Vec<Document> {
        self.preparation.docs[first..first + count]
            .iter_mut()
            .map(std::mem::take)
            .collect()
    }
}
