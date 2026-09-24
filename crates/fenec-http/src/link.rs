//! Links what a server's open left out of the graphs, beside the queries.
//!
//! A server opens its file with `fs::open_serving`: the vectors written
//! after the last graph that reached the file -- every vector, when there
//! is none to restore -- go into the arena unlinked, and `near` measures
//! each of them until they are linked. Linked at the open instead, they kept
//! the port closed for as long as they took: 56.6 s at 100 000 x 768 never
//! checkpointed, which opens in 0.96 s this way and is linked 61.8 s later.
//!
//! And it keeps its graphs in the file ([`keep`]), since it checkpoints only
//! on its way down: without them a crash after a long run left every vector
//! written since the start to link again.

use fenec_core::prelude::Database;
use std::sync::{Arc, Mutex, Once, RwLock, Weak};
use std::time::{Duration, Instant};

/// How long a slice holds the write lock. Every query and write waits a
/// slice out; at 100 000 x 768 a node took about 0.6 ms of the eight
/// threads' time, so a slice was 16 or so, and the linking as a whole took
/// 61.8 s against the open's 56.6.
const SLICE: Duration = Duration::from_millis(10);

/// Links the database's unlinked vectors on a thread of its own, a slice
/// at a time under the write lock, until none are left; `what` names the
/// database in the log. The thread holds the database only for a slice, so
/// a tenant closed meanwhile is let go of rather than kept open by it.
pub fn beside(what: &str, db: &Arc<RwLock<Database>>) {
    let total = db.read().unwrap_or_else(|e| e.into_inner()).unlinked();
    if total == 0 {
        return;
    }
    let weak = Arc::downgrade(db);
    let name = what.to_string();
    let started = std::thread::Builder::new()
        .name("fenec-link".into())
        .spawn(move || {
            crate::log!("{name}: linking {total} vectors into the graph beside the queries");
            let t0 = Instant::now();
            let mut nodes = 16;
            loop {
                let Some(db) = weak.upgrade() else {
                    return;
                };
                let mut g = db.write().unwrap_or_else(|e| e.into_inner());
                // The pace is the linking's alone, not the wait for the lock.
                let t = Instant::now();
                let left = g.link_pending(nodes);
                let took = t.elapsed();
                drop(g);
                drop(db);
                if left == 0 {
                    crate::log!("{name}: {total} vectors linked in {:.1?}", t0.elapsed());
                    return;
                }
                // As many nodes as fit the slice at the pace just measured,
                // and no more than twice as many as the last: a node costs
                // more as the graph grows, and a pace measured over a small
                // one had the next slice hold the lock for 112 ms.
                let pace = took.as_secs_f64() / nodes as f64;
                let fit = (SLICE.as_secs_f64() / pace.max(1e-6)) as usize;
                nodes = fit.clamp(1, (2 * nodes).min(512));
                std::thread::yield_now();
            }
        });
    if let Err(e) = started {
        crate::log!("could not start linking {what}'s vectors: {e}; near measures them one by one");
    }
}

/// How often the keeper looks at the databases it keeps. A graph is due a
/// record only after thousands of changes, so a look costs a pass over the
/// indexes, nothing more, and a crash between two looks leaves at most
/// what a few seconds wrote beyond the due point to link again.
const KEEP_EVERY: Duration = Duration::from_secs(5);

/// The databases the keeper appends graphs for, each named for the log.
static KEPT: Mutex<Vec<(String, Weak<RwLock<Database>>)>> = Mutex::new(Vec::new());
static KEEPER: Once = Once::new();

/// Keeps the database's graphs in its file: every few seconds one thread,
/// for every database the process serves, appends each graph that changed
/// enough since it last reached the file (`Database::save_graphs`), under
/// the read lock. A crash then links only what was written after it. The
/// thread holds a database only while it looks, so a tenant closed
/// meanwhile is let go of.
pub fn keep(what: &str, db: &Arc<RwLock<Database>>) {
    KEPT.lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((what.to_string(), Arc::downgrade(db)));
    KEEPER.call_once(|| {
        let started = std::thread::Builder::new()
            .name("fenec-graphs".into())
            .spawn(|| loop {
                std::thread::sleep(KEEP_EVERY);
                let kept = {
                    let mut kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
                    kept.retain(|(_, db)| db.strong_count() > 0);
                    kept.clone()
                };
                for (what, db) in kept {
                    if let Some(db) = db.upgrade() {
                        save(&what, &db);
                    }
                }
            });
        if let Err(e) = started {
            crate::log!("could not start keeping the graphs in the files: {e}");
        }
    });
}

/// Appends what of `db`'s graphs is due, and pushes it to disk once the
/// lock is let go.
fn save(what: &str, db: &RwLock<Database>) {
    let g = db.read().unwrap_or_else(|e| e.into_inner());
    if !g.graphs_due() {
        return;
    }
    let t = Instant::now();
    let saved = g.save_graphs();
    let held = t.elapsed();
    drop(g);
    let r = saved.and_then(|(n, durable)| {
        durable.map_or(Ok(()), |d| d())?;
        Ok(n)
    });
    match r {
        Ok(n) => crate::log!(
            "{what}: {n} graph{} written into the file, {held:.1?} under the read lock",
            if n == 1 { "" } else { "s" }
        ),
        Err(e) => {
            crate::log!("{what}: could not write the graphs into the file: {e}");
            db.write().unwrap_or_else(|e| e.into_inner()).fail(&e);
        }
    }
}
