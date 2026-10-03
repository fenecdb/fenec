//! The documents written since a file was opened, handed over to it.
//!
//! A mapped database reads what its file held when it was opened from the
//! file's pages, and held every document written since in its stores'
//! segments until a restart or a compact: 250 000 768-dim documents loaded
//! into a server were 787 MB of heap beside the 768 MB of vectors the graph
//! holds -- the engine counted 1 592 MB, and the same file started again
//! 818 (`make scale-bench`). The file holds the same bytes from the moment a
//! record is written, so once the records written since amount to
//! [`HANDOVER_AT`] bytes each store takes its documents in from there and
//! lets its segments go ([`Database::hand_over`]): 820 MB after the load.
//!
//! Where each record went is noted as it is appended, a [`Landed`] run of a
//! collection's frames, so a handover reads nothing of the file: what a
//! store's segments hold is its runs, in the order they landed, frame for
//! frame -- which the store checks before anything moves.
//!
//! A block that has not landed is in no record, and held every document it
//! wrote twice until it did, in its record and in the stores: 250 000
//! 768-dim rows in one COPY peaked 1 569 MB over what they left. Once its
//! frames amount to [`SPILL_AT`] bytes it spills them into the file
//! ([`Database::spill`]), and the stores take them in from there with the
//! records that landed before it; its land names the spills
//! ([`REC_LAND`]), and a load applies them then or never.

use super::*;

/// A run of a collection's frames in a record appended to the file: how
/// long it is, and where it starts.
#[derive(Clone, Copy)]
pub(super) struct Landed {
    cid: u32,
    len: u64,
    at: u64,
}

impl Landed {
    /// The collection, the run's length and where it starts.
    pub(super) fn run(&self) -> (u32, u64, u64) {
        (self.cid, self.len, self.at)
    }
}

/// A handover also runs once this many runs are noted: a run is 24 bytes,
/// and 16 MB of 100-byte documents written one at a time are 168 000 of
/// them, 4 MB of notes.
const HANDOVER_RUNS: usize = 1 << 16;

/// A handover also runs once the noted records hold this many writes. Its
/// pause is a document's work each -- the frame read and the id pointed at
/// its place, about 17 ns -- so 16 MB of small documents held the write
/// lock far past the bytes' measure: 930 000 rows of a text and an int
/// written 1 000 a block, 16 to 17.5 ms each time, which was the longest
/// wait of four readers beside them. The same work in pauses of 1 ms.
pub(super) const HANDOVER_DOCS: u64 = 1 << 16;

/// Notes a run of `len` bytes of collection `cid`'s frames at `at` in the
/// file.
pub(super) fn note(landed: &mut Vec<Landed>, bytes: &mut u64, cid: u32, len: usize, at: u64) {
    if len > 0 {
        landed.push(Landed {
            cid,
            len: len as u64,
            at,
        });
        *bytes += len as u64;
    }
}

/// Notes where `record`, appended to the file at `at`, holds a collection's
/// frames: a data record's body, or each of a block's data records'. A
/// record that does not read as one notes what came before it, and the
/// stores whose frames it held are left out of the handovers until the next
/// rewrite: their segments hold more than their runs account for.
pub(super) fn note_record(landed: &mut Vec<Landed>, bytes: &mut u64, at: u64, record: &[u8]) {
    let mut pos = 0;
    let Ok(r) = record_at(record, &mut pos) else {
        return;
    };
    // The records a block's body holds, from `from` in the record's.
    let mut inner = |from: usize| {
        let body = &r.body[from - r.body_at..];
        let mut p = 0;
        while p < body.len() {
            let Ok(inner) = record_at(body, &mut p) else {
                return;
            };
            if inner.kind == REC_DATA {
                let body_at = (from + inner.body_at) as u64;
                note(landed, bytes, inner.cid, inner.body.len(), at + body_at);
            }
        }
    };
    match r.kind {
        REC_DATA => note(landed, bytes, r.cid, r.body.len(), at + r.body_at as u64),
        REC_BLOCK => inner(r.body_at),
        // Its spills' frames the stores took in as they spilled.
        REC_LAND => {
            if let Ok((_, rest)) = land_parts(r.body) {
                inner(r.body_at + r.body.len() - rest.len());
            }
        }
        _ => {}
    }
}

impl Database {
    /// Hands the documents written since the file was opened or rewritten
    /// over to it: every record appended so far is written into the file,
    /// and each store points the documents it holds at their places there
    /// and lets its segments go -- what a restart did, with nothing read or
    /// written again. It runs as a block lands or a replica applies its
    /// primary's records, once they amount to [`HANDOVER_AT`] bytes, under
    /// the write lock. Returns the bytes handed over: none for a database
    /// read into memory, or with a block open, whose writes the segments
    /// hold alone until it lands.
    pub fn hand_over(&mut self) -> Result<u64> {
        if !self.mapped || self.block.is_some() || self.landed.is_empty() {
            return Ok(0);
        }
        self.refuse_if_failed()?;
        let r = self.sink_mut().written_through();
        let Some(base) = self.storage(r)? else {
            return Ok(0);
        };
        let mut landed = std::mem::take(&mut self.landed);
        self.landed_bytes = 0;
        self.landed_writes = 0;
        // A collection's runs together, in the order they landed.
        landed.sort_by_key(|l| l.cid);
        let mut runs: Vec<(u64, u64)> = Vec::new();
        let mut handed = 0;
        for c in self.collections.values_mut() {
            let from = landed.partition_point(|l| l.cid < c.id);
            let to = from + landed[from..].partition_point(|l| l.cid == c.id);
            runs.clear();
            runs.extend(landed[from..to].iter().map(|l| (l.len, l.at)));
            if c.store.hand_over(&base, &runs) {
                handed += runs.iter().map(|r| r.0).sum::<u64>();
            }
        }
        // Kept for the next runs, as a block's buffers are -- but for a
        // block of more writes than a handover waits for.
        landed.clear();
        if landed.capacity() <= HANDOVER_RUNS {
            self.landed = landed;
        }
        Ok(handed)
    }

    /// [`Self::hand_over`], once what is noted amounts to `handover_at`
    /// bytes or [`HANDOVER_RUNS`] runs. A failure is the sink's, and the
    /// database's from then on: the next write, and the durability of the
    /// ones before, report it.
    pub(super) fn hand_over_when_due(&mut self) {
        if self.landed_bytes >= self.handover_at
            || self.landed.len() >= HANDOVER_RUNS
            || (self.landed_writes >= HANDOVER_DOCS && self.handover_at != u64::MAX)
        {
            let _ = self.hand_over();
        }
    }

    /// [`Self::spill`], once the open block's frames amount to `spill_at`
    /// bytes and it may spill: not once a spill could not be made.
    pub(super) fn spill_when_due(&mut self) -> Result<()> {
        let due = self.block.as_ref().is_some_and(|b| {
            b.frames.len() - HEAD_ROOM >= self.spill_at as usize && !b.unspillable
        });
        if due && self.mapped && self.failed.is_none() {
            self.spill()?;
        }
        Ok(())
    }

    /// Spills the frames the open block holds into the file as a spill
    /// record ([`REC_SPILL`]), which the stores take them in from with the
    /// records that landed before the block, and lets the block's buffer go.
    /// The block lands naming its spills ([`Self::land_spilled`]), or never:
    /// a rollback, a crash or a lapsed lease leaves them dead in the file. A
    /// rollback puts each store back to where it stood before the block,
    /// the records before it taken in (`Mark::spilled`), and each document
    /// to where it was, in the file now ([`crate::store::Moved`]).
    pub fn spill(&mut self) -> Result<()> {
        let Some(mut b) = self.block.take() else {
            return Ok(());
        };
        let r = self.spill_block(&mut b);
        self.block = Some(b);
        r
    }

    fn spill_block(&mut self, b: &mut Block) -> Result<()> {
        let body = b.body_from(b.spilled_writes);
        // Each collection's runs, and where in the body their frames start.
        let mut runs: Vec<(u32, u64, usize)> = Vec::new();
        let mut p = 0;
        while p < body.len() {
            let r = record_at(&body, &mut p)?;
            if r.kind == REC_DATA {
                runs.push((r.cid, r.body.len() as u64, r.body_at));
            }
        }
        let mut cids: Vec<u32> = runs.iter().map(|r| r.0).collect();
        cids.sort_unstable();
        cids.dedup();
        // Every store takes its frames in, or the block spills nothing: asked
        // before the record is written. A collection dropped in the block
        // has no store here, and its frames go with it.
        let lens = |cid: u32, landed: &[Landed]| -> Vec<u64> {
            let before = landed.iter().filter(|l| l.cid == cid).map(|l| l.len);
            before
                .chain(runs.iter().filter(|r| r.0 == cid).map(|r| r.1))
                .collect()
        };
        for &cid in &cids {
            if let Some(c) = self.collections.values().find(|c| c.id == cid) {
                if !c.store.would_hand_over(&lens(cid, &self.landed)) {
                    b.unspillable = true;
                    return Ok(());
                }
            }
        }
        let record = framed(REC_SPILL, 0, &body);
        let head = (record.len() - body.len()) as u64;
        let at = *self.appended.get_mut();
        let r = self.sink_mut().append(&record);
        self.storage(r)?;
        *self.appended.get_mut() += record.len() as u64;
        self.dirty = true;
        let r = self.sink_mut().written_through();
        let Some(base) = self.storage(r)? else {
            // Nothing to take them in from: the spill is dead, and the block
            // holds its frames as it did.
            b.unspillable = true;
            return Ok(());
        };
        // A collection the block dropped keeps the notes of what landed
        // before it: put back, the block brings it back with them.
        let mut handed = Vec::with_capacity(cids.len());
        for &cid in &cids {
            let Some(c) = self.collections.values_mut().find(|c| c.id == cid) else {
                continue;
            };
            handed.push(cid);
            let mut all: Vec<(u64, u64)> = self
                .landed
                .iter()
                .filter(|l| l.cid == cid)
                .map(|l| (l.len, l.at))
                .collect();
            let keep = all.len();
            all.extend(
                runs.iter()
                    .filter(|r| r.0 == cid)
                    .map(|r| (r.1, at + head + r.2 as u64)),
            );
            let Some((moved, kept)) = c.store.hand_over_keeping(&base, &all, keep) else {
                return Err(Error::Corrupt(
                    "a store could not take its spilled frames in".into(),
                ));
            };
            if let Some(m) = b.marks.iter_mut().find(|(c, _)| *c == cid) {
                m.1 = m.1.spilled(kept);
            }
            for u in b.was.iter_mut() {
                if let Undo::Doc(c, _, Some(loc)) = u {
                    if *c == cid {
                        *loc = moved.loc(*loc);
                    }
                }
            }
        }
        self.landed.retain(|l| !handed.contains(&l.cid));
        self.landed_bytes = self.landed.iter().map(|l| l.len).sum();
        b.spilled.push((at + head, body.len() as u64));
        b.spill_base = Some(base);
        b.frames.truncate(HEAD_ROOM);
        b.spilled_writes = b.heads.len();
        Ok(())
    }

    /// Lands a block that spilled, as a land record ([`REC_LAND`]) naming
    /// its spills, then the rest of its records; returns what the sink said
    /// and the record's length. The sink is handed the spills' bodies too,
    /// for a feed to send the block as the one block record it would have
    /// been ([`Sink::land`]).
    pub(super) fn land_spilled(&mut self, b: &mut Block, seq: u64, at: u64) -> (Result<()>, usize) {
        let mut body = Vec::new();
        put_uvarint(&mut body, b.spilled.len() as u64);
        for &(s, n) in &b.spilled {
            put_uvarint(&mut body, s);
            put_uvarint(&mut body, n);
        }
        body.extend_from_slice(&b.body_from(b.spilled_writes));
        let record = framed(REC_LAND, 0, &body);
        let base = b.spill_base.clone();
        let file: &[u8] = base.as_deref().map_or(&[], |m| m.as_ref());
        let spilled: Vec<&[u8]> = b
            .spilled
            .iter()
            .map(|&(s, n)| file.get(s as usize..(s + n) as usize).unwrap_or(&[]))
            .collect();
        let r = self.sink_mut().land(seq, &spilled, &record);
        if r.is_ok() {
            note_record(&mut self.landed, &mut self.landed_bytes, at, &record);
        }
        (r, record.len())
    }

    /// When an open block spills: once its frames amount to `bytes`
    /// ([`SPILL_AT`] unless set), `u64::MAX` never.
    pub fn set_spill(&mut self, bytes: u64) {
        self.spill_at = bytes;
    }

    /// When a handover runs: once the documents written since the last one
    /// amount to `bytes` ([`HANDOVER_AT`] unless set), 0 after every block,
    /// `u64::MAX` never.
    pub fn set_handover(&mut self, bytes: u64) {
        self.handover_at = bytes;
    }

    /// The runs noted refer to a file that is no more, or to records the
    /// stores no longer hold in memory: a rewrite pointed them at the new
    /// file, a load read them there.
    pub(super) fn forget_landed(&mut self) {
        self.landed.clear();
        self.landed_bytes = 0;
        self.landed_writes = 0;
    }

    /// The bytes the notes take.
    pub(super) fn landed_bytes_held(&self) -> usize {
        self.landed.capacity() * std::mem::size_of::<Landed>()
    }
}
