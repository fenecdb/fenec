//! Deletes the rows past their time (`@ttl`) on a primary.
//!
//! A read leaves an expired row out at once (`Database::alive`), so the
//! sweep is not what makes expiry exact; it is what gives the space back.
//! Once a minute one thread looks at every database the process serves and
//! deletes, a batch at a time, the rows of each collection whose rows
//! expire: found under the read lock (`Database::expired`, a range of the
//! field's ordered index), deleted under the write lock as one block of
//! ordinary deletes (`Database::sweep`), so a replica, `/_changes`, an
//! archive and a subscriber see each as any delete. A replica never
//! sweeps -- its writes come from its primary, whose sweep it applies --
//! and neither does a following tenant, nor the browser, which has no
//! server to sweep for it.

use fenec_core::prelude::Database;
use std::sync::{Mutex, Once, RwLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often the sweeper looks. A row past its time is out of every read
/// already; this is how long its bytes stay before they are deleted.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// How many rows a batch deletes under the write lock: every read and write
/// waits a batch out. 100 000 expired rows of a session's size, out of a
/// file of 200 000, went in batches of 1 000 holding the lock 1.06 ms at
/// the median and 2.3 at most, 0.56 s in all with an fsync a batch; in
/// batches of 10 000, 13.9 ms at the median, and of 100, 0.16 ms but 6.0 s
/// in all, each batch finding its rows again (`make ttl-bench`).
pub const BATCH: usize = 1_000;

/// The databases the sweeper looks at, each named for the log.
static SWEPT: Mutex<Vec<(String, Weak<RwLock<Database>>)>> = Mutex::new(Vec::new());
static SWEEPER: Once = Once::new();

/// Sweeps `db` with every other database the process serves, once a
/// minute. The thread holds a database only while it sweeps it, so a
/// tenant closed meanwhile is let go of.
pub fn watch(what: &str, db: &std::sync::Arc<RwLock<Database>>) {
    SWEPT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((what.to_string(), std::sync::Arc::downgrade(db)));
    SWEEPER.call_once(|| {
        let started = std::thread::Builder::new()
            .name("fenec-sweep".into())
            .spawn(|| loop {
                std::thread::sleep(SWEEP_EVERY);
                let swept = {
                    let mut swept = SWEPT.lock().unwrap_or_else(|e| e.into_inner());
                    swept.retain(|(_, db)| db.strong_count() > 0);
                    swept.clone()
                };
                for (what, db) in swept {
                    if let Some(db) = db.upgrade() {
                        pass(&what, &db);
                    }
                }
            });
        if let Err(e) = started {
            crate::log!("could not start sweeping expired rows: {e}");
        }
    });
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// One sweep of `db`, now: every collection whose rows expire, a batch at
/// a time until none past its time is left. How many rows it deleted.
pub fn pass(what: &str, db: &RwLock<Database>) -> usize {
    let collections = {
        let g = crate::held::read(db);
        // Its writes come from its primary, which sweeps.
        if g.history().following {
            return 0;
        }
        g.expiring()
    };
    let mut total = 0;
    for c in collections {
        loop {
            let now = now_ms();
            let ids = match crate::held::read(db).expired(&c, now, BATCH) {
                Ok(ids) => ids,
                Err(e) => {
                    crate::log!("{what}: could not find `{c}`'s expired rows: {e}");
                    break;
                }
            };
            if ids.is_empty() {
                break;
            }
            let t = Instant::now();
            let swept = {
                let mut g = crate::held::write(db);
                g.sweep(&c, now, &ids)
                    .and_then(|n| g.flush().map(|durable| (n, durable)))
            };
            let held = t.elapsed();
            match swept {
                Ok((n, durable)) => {
                    // The deletes reach a replica once an fsync covers them,
                    // as any write does: run once the lock is let go.
                    if let Some(d) = durable {
                        if let Err(e) = d() {
                            crate::log!("{what}: sync error after a sweep: {e}");
                            crate::held::write(db).fail(&e);
                            return total;
                        }
                    }
                    total += n;
                    if n > 0 && held > Duration::from_millis(50) {
                        crate::log!("{what}: {n} expired rows of `{c}` swept, {held:.1?} under the write lock");
                    }
                }
                // A lapsed lease, a storage error, a replica promoted away:
                // the next pass finds the rows again.
                Err(e) => {
                    crate::log!("{what}: could not sweep `{c}`: {e}");
                    break;
                }
            }
            if ids.len() < BATCH {
                break;
            }
        }
    }
    total
}
