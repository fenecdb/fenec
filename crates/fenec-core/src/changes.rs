//! Change feed: "these ids changed", not a transaction log.
//!
//! A replica can be fed in two ways. The classic route is a transaction
//! log: every write becomes an event and the subscriber applies the events
//! in order. fenecdb does *not* do that, because the price comes from two sides:
//!
//! - **Memory.** An event has to carry the document; a ring buffer holding
//!   768-dimensional embeddings reaches hundreds of MB within a few thousand writes.
//! - **Repetition.** Ten writes in a row to the same document are ten events;
//!   none of the intermediate states the subscriber sees is of any use.
//!
//! The feed here is **state based**: the ring holds only `(seq, cid, id)`
//! -- 24 bytes per entry, independent of document size. When the subscriber
//! reads, the row's *current* state is decoded from the store. Three results:
//!
//! 1. N writes to the same document collapse into one event.
//! 2. Re-applying is harmless (idempotent): "the current state of this id
//!    is this" yields the same result when applied twice.
//! 3. The shape filter works correctly for free. If a row is updated out of
//!    the filter, the subscriber gets "not there" -- no separate "left the
//!    set" event has to be produced. With a transaction log that would also
//!    require carrying the old image.
//!
//! The price: a subscriber cannot see intermediate states, and `seq` is a
//! *clock*, not a record. The right trade for a replica; anyone who wants an
//! audit log should not use this.

use crate::value::DocId;
use std::collections::VecDeque;

/// Default entry count of the ring buffer (~96 KB).
///
/// This number directly decides **how far behind a subscriber may fall**:
/// when the buffer overflows the oldest marks are dropped and cursors
/// behind that point can no longer be caught up incrementally -- they get
/// reseeded. At 100 writes per second, 4096 entries is a window of about
/// 40 seconds.
pub const DEFAULT_CAPACITY: usize = 4096;

/// Document id of the schema-change mark. Real ids start at 1, so 0 never
/// collides.
pub const SCHEMA_MARK: DocId = 0;

/// The mark of a single write. It does *not* carry the document (module header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    pub seq: u64,
    pub cid: u32,
    pub id: DocId,
}

/// Result of reading everything after a cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Since {
    /// The cursor fell behind the ring (or is ahead of it): incremental
    /// catch-up is impossible, the subscriber has to be reseeded.
    Reseed,
    /// These ids changed. Sorted and deduplicated; resolving the values from
    /// the store is left to the caller.
    Ids(Vec<DocId>),
}

pub struct ChangeLog {
    ring: VecDeque<Mark>,
    cap: usize,
    seq: u64,
    /// The **smallest** cursor that can still be caught up incrementally. If
    /// the oldest mark in the ring has `seq` H, the cursor that must see H is
    /// H-1, so the horizon is "oldest mark - 1". On an empty ring the horizon
    /// is `seq` itself: only a cursor saying "I saw everything" gets nothing.
    horizon: u64,
}

impl Default for ChangeLog {
    fn default() -> Self {
        ChangeLog::new(DEFAULT_CAPACITY)
    }
}

impl ChangeLog {
    pub fn new(cap: usize) -> ChangeLog {
        ChangeLog {
            ring: VecDeque::new(),
            cap: cap.max(1),
            seq: 0,
            horizon: 0,
        }
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn horizon(&self) -> u64 {
        self.horizon
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Resizes the ring buffer. When it shrinks the oldest entries are
    /// dropped and the horizon rises.
    pub fn set_capacity(&mut self, cap: usize) {
        self.cap = cap.max(1);
        while self.ring.len() > self.cap {
            self.evict();
        }
    }

    /// Sets the counter to the given value and empties the ring.
    ///
    /// Called at startup and after `load`: a database restored from a file
    /// has no history, only its *current* state. The horizon is therefore
    /// set equal to `seq` -- everyone but a cursor sitting exactly here gets
    /// reseeded. Saying "start over" out loud is the right thing to do,
    /// rather than silently sending an incomplete set of events.
    pub fn reset(&mut self, seq: u64) {
        self.ring.clear();
        self.seq = seq;
        self.horizon = seq;
    }

    /// Records a write and returns the new `seq`.
    pub fn record(&mut self, cid: u32, id: DocId) -> u64 {
        self.seq += 1;
        if self.ring.len() == self.cap {
            self.evict();
        }
        self.ring.push_back(Mark {
            seq: self.seq,
            cid,
            id,
        });
        self.seq
    }

    fn evict(&mut self) {
        if let Some(m) = self.ring.pop_front() {
            // The dropped mark is `m.seq`; the cursor that had to see it was
            // `m.seq - 1`. The smallest servable cursor is now `m.seq`.
            self.horizon = self.horizon.max(m.seq);
        }
    }

    /// Ids that changed **in this collection** after `since`.
    ///
    /// Returns [`Since::Reseed`] if `since` is behind the horizon or ahead of
    /// the counter (server restarted, cursor gone stale).
    pub fn since(&self, since: u64, cid: u32) -> Since {
        if since > self.seq || since < self.horizon {
            return Since::Reseed;
        }
        // The ring is sorted ascending by `seq`: the first relevant entry is
        // found by binary search, the whole buffer is never scanned.
        let start = self.ring.partition_point(|m| m.seq <= since);
        let mut out: Vec<DocId> = self
            .ring
            .iter()
            .skip(start)
            .filter(|m| m.cid == cid)
            .map(|m| m.id)
            .collect();
        out.sort_unstable();
        out.dedup();
        Since::Ids(out)
    }

    /// Collection ids that changed after `since` (deduplicated).
    /// For live queries in the browser: this much is enough to pick which
    /// query has to be re-run.
    pub fn changed_collections(&self, since: u64) -> Option<Vec<u32>> {
        if since > self.seq || since < self.horizon {
            return None;
        }
        let start = self.ring.partition_point(|m| m.seq <= since);
        let mut out: Vec<u32> = self.ring.iter().skip(start).map(|m| m.cid).collect();
        out.sort_unstable();
        out.dedup();
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_log_serves_only_its_own_cursor() {
        let log = ChangeLog::new(8);
        assert_eq!(log.since(0, 1), Since::Ids(vec![]));
        assert_eq!(log.since(1, 1), Since::Reseed);
    }

    #[test]
    fn records_and_dedups() {
        let mut log = ChangeLog::new(8);
        log.record(1, 10);
        log.record(1, 11);
        log.record(1, 10);
        log.record(2, 99);
        assert_eq!(log.seq(), 4);
        assert_eq!(log.since(0, 1), Since::Ids(vec![10, 11]));
        assert_eq!(log.since(0, 2), Since::Ids(vec![99]));
        // A cursor that has seen the first two writes only gets the rest.
        assert_eq!(log.since(2, 1), Since::Ids(vec![10]));
        assert_eq!(log.since(4, 1), Since::Ids(vec![]));
    }

    #[test]
    fn overflow_raises_horizon_and_forces_reseed() {
        let mut log = ChangeLog::new(2);
        log.record(1, 1); // seq 1
        log.record(1, 2); // seq 2
        assert_eq!(log.horizon(), 0);
        log.record(1, 3); // seq 3 -> seq 1 is dropped
        assert_eq!(log.horizon(), 1);
        // Cursor 0 needed to see seq 1: no longer possible.
        assert_eq!(log.since(0, 1), Since::Reseed);
        // Cursor 1 can still be served.
        assert_eq!(log.since(1, 1), Since::Ids(vec![2, 3]));
    }

    #[test]
    fn reset_clears_history() {
        let mut log = ChangeLog::new(8);
        log.record(1, 1);
        log.reset(42);
        assert_eq!(log.seq(), 42);
        assert_eq!(log.horizon(), 42);
        assert_eq!(log.since(42, 1), Since::Ids(vec![]));
        assert_eq!(log.since(41, 1), Since::Reseed);
    }

    #[test]
    fn cursor_from_the_future_reseeds() {
        // The server may have restarted and left the counter behind.
        let mut log = ChangeLog::new(8);
        log.record(1, 1);
        assert_eq!(log.since(99, 1), Since::Reseed);
    }

    #[test]
    fn shrinking_capacity_evicts() {
        let mut log = ChangeLog::new(8);
        for i in 1..=8 {
            log.record(1, i);
        }
        log.set_capacity(3);
        assert_eq!(log.horizon(), 5);
        assert_eq!(log.since(5, 1), Since::Ids(vec![6, 7, 8]));
        assert_eq!(log.since(4, 1), Since::Reseed);
    }

    #[test]
    fn changed_collections_is_deduped() {
        let mut log = ChangeLog::new(8);
        log.record(1, 1);
        log.record(2, 1);
        log.record(1, 2);
        assert_eq!(log.changed_collections(0), Some(vec![1, 2]));
        assert_eq!(log.changed_collections(2), Some(vec![1]));
    }
}
