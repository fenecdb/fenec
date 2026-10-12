//! What a block of writes is held to as it lands, besides the lease: a
//! server's quota of a tenant's file bytes and rows ([`Database::set_limit`]).
//!
//! **Asked as the block lands, its record built.** A quota asked before a
//! write runs lets that write past it by whatever it turns out to add, and a
//! `set` over every row adds the whole collection again: a tenant at 99% of
//! a 100 MB quota would double its file with one statement. Asked here, the
//! record's length is known to the byte and nothing of it is in the file
//! yet, so a block that would take the file past its quota is put back as a
//! failed one is, and no write that adds takes the file past it. A block too
//! large to hold in memory spills into the file before it lands
//! (`Database::spill`), and each spill is asked the same way first: a spill
//! refused writes nothing, where one let through and its block then refused
//! would leave its bytes dead in the file, past the quota, for every
//! refused block of a tenant sending them.
//!
//! **What only takes away always lands.** A block that writes no document
//! into a collection of the app's -- deletes, a drop, a server's own rows
//! (`_idempotency`, `_consumers`, the `_`-named ones, `declared::own`) --
//! is not refused, or a tenant at its quota could not delete its way back
//! under it: a keyed `del` writes its key into `_idempotency`, and counted
//! as growing it would be refused at a full quota. Such a block does add its
//! tombstones to the file; a compact gives them back.
//!
//! Native only, as the fence is: a page has no server holding it to a
//! quota, and the module carries none of it.

use super::*;

/// What a block would leave the file at, asked of a [`Limit`] before its
/// record -- or one of its spills -- is appended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Landing {
    /// The file's bytes as written, the block's earlier spills included.
    pub file: u64,
    /// The bytes the record about to be appended adds.
    pub adds: u64,
    /// Whether the block writes a document into a collection of the app's
    /// or changes a schema other than by a drop: more than taking away.
    pub grows: bool,
    /// The writes the record holds to the app's collections -- a document
    /// put or deleted, a schema changed -- the server's own rows left out:
    /// what a quota of rows written a second charges.
    pub writes: u64,
}

/// What a block of writes is held to as it lands: asked with the database
/// as the block leaves it -- its documents in the stores, its record not
/// yet in the file -- and the [`Landing`]. An `Err` puts the block back,
/// refused as [`Error::Quota`] with the message given.
pub type Limit = Arc<dyn Fn(&Database, &Landing) -> std::result::Result<(), String> + Send + Sync>;

impl Database {
    /// Holds every block of writes from now on to `limit`, or to none.
    /// Replicated writes (`apply`) and a compact are not blocks, and are
    /// not asked: a replica holds what its primary took, and a compact
    /// only gives bytes back.
    pub fn set_limit(&mut self, limit: Option<Limit>) {
        self.limit = limit;
    }

    /// The documents in the app's own collections -- those not `_`-named
    /// (`declared::own`), which are the database's: idempotency keys,
    /// consumers' places, migrations. A sum over the collections, no
    /// document read: what a rows quota counts.
    pub fn documents(&self) -> u64 {
        self.collections
            .values()
            .filter(|c| !crate::declared::own(&c.schema.name))
            .map(|c| c.store.len() as u64)
            .sum()
    }

    /// The limit's answer to `adds` more bytes holding `writes` writes,
    /// where one is set.
    pub(super) fn limited(&self, grows: bool, adds: u64, writes: u64) -> Result<()> {
        let Some(limit) = &self.limit else {
            return Ok(());
        };
        let landing = Landing {
            file: self.appended.load(Relaxed),
            adds,
            grows,
            writes,
        };
        limit(self, &landing).map_err(Error::Quota)
    }

    /// Whether the writes of `b` from its `from`th on -- those whose frames
    /// it still holds -- write a document into a collection of the app's or
    /// change a schema other than by a drop. Stops at the first that does:
    /// a `put`'s first frame.
    pub(super) fn grows(&self, b: &Block, from: usize) -> bool {
        let mut start = HEAD_ROOM;
        let mut last: Option<(u32, bool)> = None;
        for &(kind, cid, end) in &b.heads[from.min(b.heads.len())..] {
            let op = b.frames.get(start).copied();
            start = end;
            let adds = match kind {
                REC_DATA => op == Some(crate::store::OP_PUT),
                REC_DROP => false,
                _ => true,
            };
            if !adds {
                continue;
            }
            let own = match last {
                Some((c, own)) if c == cid => own,
                _ => {
                    let own = self
                        .collections
                        .values()
                        .find(|c| c.id == cid)
                        .is_some_and(|c| crate::declared::own(&c.schema.name));
                    last = Some((cid, own));
                    own
                }
            };
            if !own {
                return true;
            }
        }
        false
    }
}

impl Database {
    /// The writes of `b` from its `from`th on to a collection of the app's,
    /// not the server's own (`declared::own`): a keyed write's key in
    /// `_idempotency` is not a row the tenant wrote.
    pub(super) fn app_writes(&self, b: &Block, from: usize) -> u64 {
        let mut last: Option<(u32, bool)> = None;
        let mut n = 0;
        for &(_, cid, _) in &b.heads[from.min(b.heads.len())..] {
            let own = match last {
                Some((c, own)) if c == cid => own,
                _ => {
                    let own = self
                        .collections
                        .values()
                        .find(|c| c.id == cid)
                        .is_some_and(|c| crate::declared::own(&c.schema.name));
                    last = Some((cid, own));
                    own
                }
            };
            n += u64::from(!own);
        }
        n
    }
}

impl Block {
    /// The length of the record [`Block::record`] makes, worked out without
    /// making it: a block across collections would be copied once more.
    pub(super) fn record_len(&self) -> usize {
        let (kind, cid, _) = self.heads[0];
        if self.heads.len() == 1 || self.heads.iter().all(|h| h.0 == REC_DATA && h.1 == cid) {
            let len = self.frames.len() - HEAD_ROOM;
            return head(kind, cid, len).1 + len;
        }
        let (mut body, mut start) = (0, HEAD_ROOM);
        for &(kind, cid, end) in &self.heads {
            body += head(kind, cid, end - start).1 + end - start;
            start = end;
        }
        head(REC_BLOCK, 0, body).1 + body
    }
}
