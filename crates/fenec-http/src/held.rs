//! A database a pg session holds a transaction open on, between the
//! transaction's statements.
//!
//! The session takes the write lock for each statement alone and leaves
//! the block open between them ([`Database::leave_block`]): readers read
//! meanwhile, from what has landed, and writers wait for the transaction to
//! end, as they did when the session held the write lock throughout. Every
//! path in or beside the servers takes the database through one of these,
//! or through a lock of its own that neither reads documents nor writes:
//! the engine refuses a statement run into another session's block, and a
//! read of one that is not parked.

use fenec_core::prelude::*;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

/// The database to read what has landed: a block left open between a
/// transaction's statements is parked for the read ([`Database::park`]),
/// its writes put back while it waits -- readers go on rather than wait for
/// the transaction to end -- and a block that cannot be parked, one that
/// changed a graph or the schema, is waited for, as every block was.
pub fn read_landed(db: &RwLock<Database>) -> RwLockReadGuard<'_, Database> {
    let mut waited = 0;
    loop {
        let g = db.read().unwrap_or_else(|e| e.into_inner());
        if g.reads_landed() {
            return g;
        }
        drop(g);
        if db.write().unwrap_or_else(|e| e.into_inner()).park() {
            continue;
        }
        pause(&mut waited);
    }
}

/// The database to write into, for a write that is not an open block's
/// own: a block left open between a transaction's statements is waited for
/// to end.
pub fn write_unheld(db: &RwLock<Database>) -> RwLockWriteGuard<'_, Database> {
    let mut waited = 0;
    loop {
        let g = db.write().unwrap_or_else(|e| e.into_inner());
        if !g.block_left() {
            return g;
        }
        drop(g);
        pause(&mut waited);
    }
}

/// The database once no block is open on it at all: what reads it whole
/// and hands it on -- a tenant's export for a move -- waits for an open
/// transaction to end, so that its commit lands before the export rather
/// than into a file the move leaves behind.
pub fn read_quiet(db: &RwLock<Database>) -> RwLockReadGuard<'_, Database> {
    let mut waited = 0;
    loop {
        let g = db.read().unwrap_or_else(|e| e.into_inner());
        if !g.in_block() {
            return g;
        }
        drop(g);
        pause(&mut waited);
    }
}

/// A wait for a transaction's next statement or its end: the yield a lock
/// wait makes, then a sleep, as `fenec-pg` waits for a lock.
pub fn pause(waited: &mut u32) {
    *waited += 1;
    match *waited < 64 {
        true => std::thread::yield_now(),
        false => std::thread::sleep(Duration::from_micros(200)),
    }
}
