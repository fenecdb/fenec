//! Which history a database's writes belong to.
//!
//! A replica takes a primary's writes by their place in the change counter
//! (see [`crate::engine::Database::apply`]), so two databases that agree on
//! a number have to agree on the write under it as well. They stop agreeing
//! when a history forks: a replica promoted after its primary died writes
//! its own change 1001, while the old primary -- or a second replica that
//! had got further along -- may hold a different 1001 of its own. Numbers
//! alone would stream the promoted replica's 1002 onto that other 1001, and
//! nothing downstream could tell.
//!
//! So every fork gets a fresh id and remembers the change where it left its
//! parent, the way PostgreSQL numbers timelines, and a replica is continued
//! only from a position the primary's own history passed through
//! ([`History::continues`]). Anything else gets a whole image instead.
//!
//! Crashes need none of this: a primary sends only writes that are on its
//! disk, so a primary that comes back from one holds everything any replica
//! was sent, and continues the history it was on.

use crate::codec::{get_uvarint, put_uvarint};
use crate::error::{Error, Result};

/// The record kind the history is kept under: `[8][0][length][history]`.
pub const RECORD: u8 = 8;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    /// `(id, from)`, oldest first: the writes after change `from` are
    /// history `id`'s. Before the first entry is the root, id 0 -- the
    /// history of a database that never had a replica, which names nothing:
    /// every such database has it.
    pub lineage: Vec<(u64, u64)>,
    /// Whether the writes come from a primary. A following database takes
    /// no write of its own: it would fork its history without a new id.
    pub following: bool,
}

impl History {
    /// The id of the history the next write belongs to.
    pub fn current(&self) -> u64 {
        self.lineage.last().map_or(0, |e| e.0)
    }

    /// Whether a database on history `id` and `seq` changes in holds only
    /// changes this one holds too -- this one being `now` changes in.
    ///
    /// The root's id is shared by every database that never had a replica,
    /// so it proves nothing: under it only an empty database is a prefix.
    /// A database at a change past the point where its history's successor
    /// forked off holds a write this one does not.
    pub fn continues(&self, id: u64, seq: u64, now: u64) -> bool {
        if seq > now {
            return false;
        }
        if id == 0 {
            return seq == 0;
        }
        match self.lineage.iter().position(|e| e.0 == id) {
            None => false,
            Some(k) => seq <= self.lineage.get(k + 1).map_or(now, |e| e.1),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![self.following as u8];
        put_uvarint(&mut out, self.lineage.len() as u64);
        for &(id, from) in &self.lineage {
            out.extend_from_slice(&id.to_le_bytes());
            put_uvarint(&mut out, from);
        }
        out
    }

    /// The whole record, as a file holds it: appended to an image, it
    /// replaces the image's history without moving its change counter.
    pub fn record(&self) -> Vec<u8> {
        let body = self.encode();
        let mut out = Vec::with_capacity(body.len() + 4);
        out.push(RECORD);
        put_uvarint(&mut out, 0);
        put_uvarint(&mut out, body.len() as u64);
        out.extend_from_slice(&body);
        out
    }

    /// This history with a fork at change `at` under `id`, taking writes.
    pub fn forked(&self, id: u64, at: u64) -> History {
        let mut lineage = self.lineage.clone();
        lineage.push((id, at));
        History {
            lineage,
            following: false,
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<History> {
        Self::parse(bytes).ok_or_else(|| Error::Corrupt("history record".into()))
    }

    fn parse(bytes: &[u8]) -> Option<History> {
        let (&following, _) = bytes.split_first()?;
        let mut pos = 1;
        let n = get_uvarint(bytes, &mut pos).ok()?;
        let mut lineage = Vec::new();
        for _ in 0..n {
            let id = u64::from_le_bytes(bytes.get(pos..pos + 8)?.try_into().ok()?);
            pos += 8;
            lineage.push((id, get_uvarint(bytes, &mut pos).ok()?));
        }
        Some(History {
            lineage,
            following: following != 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_history_continues_up_to_where_it_was_left() {
        // The root until change 10, then 7 until 20, then 9.
        let h = History {
            lineage: vec![(7, 10), (9, 20)],
            following: false,
        };
        assert_eq!(h.current(), 9);
        // Only an empty database is on the root as far as anyone can tell.
        assert!(h.continues(0, 0, 30));
        assert!(!h.continues(0, 5, 30));
        // 7 was left at 20: a database past it wrote something 9 did not.
        assert!(h.continues(7, 15, 30));
        assert!(h.continues(7, 20, 30));
        assert!(!h.continues(7, 21, 30));
        // The current history runs to now, and nobody is ahead of it.
        assert!(h.continues(9, 30, 30));
        assert!(!h.continues(9, 31, 30));
        // A history this one never had.
        assert!(!h.continues(8, 1, 30));
    }

    #[test]
    fn a_history_round_trips() {
        let h = History {
            lineage: vec![(u64::MAX, 0), (1, 1 << 40)],
            following: true,
        };
        assert_eq!(History::decode(&h.encode()).unwrap(), h);
        assert_eq!(
            History::decode(&History::default().encode()).unwrap(),
            History::default()
        );
        assert!(History::decode(&[0, 1, 2]).is_err());
    }
}
