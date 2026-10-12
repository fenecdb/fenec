//! The database's lock, taken whole: a poisoned lock -- a thread that
//! panicked holding it -- is taken as it stands rather than turned into a
//! panic of every request after it. A write leaves the database as a block
//! landed or put back, never half of one, so what it guarded is whole.

use fenec_core::engine::Prepared;
use fenec_core::prelude::*;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// The database to read: whatever a read lock finds has landed, since a
/// block of writes is open only under the write lock of whoever opened it.
///
/// The wait for it is a span of the request this thread serves, when it is
/// traced: a read waits out a write's block, and a write every read.
pub fn read(db: &RwLock<Database>) -> RwLockReadGuard<'_, Database> {
    let waiting = crate::trace::span("lock.wait");
    waiting.attr("fenec.lock", "read");
    db.read().unwrap_or_else(|e| e.into_inner())
}

/// The database to write into.
pub fn write(db: &RwLock<Database>) -> RwLockWriteGuard<'_, Database> {
    let waiting = crate::trace::span("lock.wait");
    waiting.attr("fenec.lock", "write");
    db.write().unwrap_or_else(|e| e.into_inner())
}

/// The documents the `put`s among `stmts` write, built and encoded under
/// the read lock, beside the readers, for the write lock to take
/// ([`Database::prepare`]): a `/batch` of 1 000 single puts held the write
/// lock for all of their building. Nothing, and no lock taken, for a
/// statement of a few documents ([`Prepared::worth`]).
pub fn prepared(db: &RwLock<Database>, stmts: &[(&Statement, &[Value])]) -> Prepared {
    match Prepared::worth(stmts) {
        true => read(db).prepare(stmts),
        false => Prepared::default(),
    }
}
