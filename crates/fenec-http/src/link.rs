//! Links what a server's open left out of the graphs, beside the queries.
//!
//! A server opens its file with `fs::open_serving`: the vectors written
//! after the last checkpoint -- every vector, when there is no graph to
//! restore -- go into the arena unlinked, and `near` measures each of them
//! until they are linked. Linked at the open instead, they kept the port
//! closed for as long as they took: 56.6 s at 100 000 x 768 never
//! checkpointed, which opens in 0.96 s this way and is linked 61.8 s later.

use fenec_core::prelude::Database;
use std::sync::{Arc, RwLock};
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
