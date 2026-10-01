//! The database's lock, taken whole: a poisoned lock -- a thread that
//! panicked holding it -- is taken as it stands rather than turned into a
//! panic of every request after it. A write leaves the database as a block
//! landed or put back, never half of one, so what it guarded is whole.

use fenec_core::prelude::*;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// The database to read: whatever a read lock finds has landed, since a
/// block of writes is open only under the write lock of whoever opened it.
pub fn read(db: &RwLock<Database>) -> RwLockReadGuard<'_, Database> {
    db.read().unwrap_or_else(|e| e.into_inner())
}

/// The database to write into.
pub fn write(db: &RwLock<Database>) -> RwLockWriteGuard<'_, Database> {
    db.write().unwrap_or_else(|e| e.into_inner())
}
