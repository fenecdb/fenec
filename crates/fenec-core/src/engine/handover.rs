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

use super::*;

/// A run of a collection's frames in a record appended to the file: how
/// long it is, and where it starts.
#[derive(Clone, Copy)]
pub(super) struct Landed {
    cid: u32,
    len: u64,
    at: u64,
}

/// A handover also runs once this many runs are noted: a run is 24 bytes,
/// and 16 MB of 100-byte documents written one at a time are 168 000 of
/// them, 4 MB of notes.
const HANDOVER_RUNS: usize = 1 << 16;

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
    match r.kind {
        REC_DATA => note(landed, bytes, r.cid, r.body.len(), at + r.body_at as u64),
        REC_BLOCK => {
            let mut p = 0;
            while p < r.body.len() {
                let Ok(inner) = record_at(r.body, &mut p) else {
                    return;
                };
                if inner.kind == REC_DATA {
                    let body_at = (r.body_at + inner.body_at) as u64;
                    note(landed, bytes, inner.cid, inner.body.len(), at + body_at);
                }
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
        if self.landed_bytes >= self.handover_at || self.landed.len() >= HANDOVER_RUNS {
            let _ = self.hand_over();
        }
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
    }

    /// The bytes the notes take.
    pub(super) fn landed_bytes_held(&self) -> usize {
        self.landed.capacity() * std::mem::size_of::<Landed>()
    }
}
