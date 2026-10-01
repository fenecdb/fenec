//! When writes reach the disk, and what a shutdown signal does.
//!
//! - Writes are pushed to disk periodically by the background syncer, or
//!   after each write ([`SyncPolicy`]); the fsync runs outside the
//!   exclusive lock, so readers are not held up by the disk.
//! - On `SIGINT`, `SIGTERM` or `SIGHUP` a final `sync` runs, and after it
//!   -- when there is a vector index -- a checkpoint, otherwise every
//!   restart would link the HNSW graph again from the documents.

use fenec_core::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

/// Whether a shutdown signal arrived. The signal handler writes only this
/// atomic; the syncer thread sees it and performs the final `sync`.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// When writes are pushed to disk.
///
/// fenecdb does not `fsync` on every write; writes accumulate in a 1 MB
/// buffer. On the server side that has to be tied to a *policy*, otherwise
/// nothing reaches the disk until the buffer fills.
#[derive(Clone, Copy, PartialEq)]
pub enum SyncPolicy {
    /// Only on shutdown. The fastest, the least durable.
    Off,
    /// `fsync` after every write statement. The most durable, the slowest.
    Always,
    /// Periodic: at most one interval's worth of writes is at risk.
    Interval(Duration),
}

impl SyncPolicy {
    /// `off` | `always` | `<ms>`
    pub fn parse(s: &str) -> std::result::Result<SyncPolicy, String> {
        match s {
            "off" | "none" => Ok(SyncPolicy::Off),
            "always" | "on" => Ok(SyncPolicy::Always),
            ms => ms
                .parse::<u64>()
                .map(|n| SyncPolicy::Interval(Duration::from_millis(n)))
                .map_err(|_| format!("--sync expects off | always | <ms>, got `{ms}`")),
        }
    }

    /// How the startup line names it.
    pub fn describe(&self) -> String {
        match self {
            SyncPolicy::Off => "on shutdown".to_string(),
            SyncPolicy::Always => "every write".to_string(),
            SyncPolicy::Interval(d) => format!("{} ms", d.as_millis()),
        }
    }

    /// How often the syncer looks: the interval, or often enough to notice
    /// a shutdown signal when there is none.
    pub fn tick(&self) -> Duration {
        match self {
            SyncPolicy::Interval(d) if !d.is_zero() => *d,
            _ => Duration::from_millis(200),
        }
    }
}

/// Catches `SIGINT`/`SIGTERM`/`SIGHUP`. The handler only writes an atomic
/// (signal-safe); the real `sync` happens in the syncer thread.
///
/// Installed before the line saying the server listens: a supervisor that
/// sends SIGTERM as soon as it reads that line would otherwise stop it by
/// the default action, with no final sync.
pub fn install_signal_handlers() {
    // libc's `signal` function, declared directly so as not to add a
    // dependency. SIGINT=2, SIGTERM=15, SIGHUP=1.
    extern "C" {
        fn signal(sig: i32, handler: usize) -> usize;
    }
    extern "C" fn on_signal(_sig: i32) {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }
    unsafe {
        for sig in [1, 2, 15] {
            signal(sig, on_signal as *const () as usize);
        }
    }
}

/// Whether a shutdown signal has arrived.
pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

/// The flag a shutdown signal sets, for a thread of the server's that has
/// to stop with it: `--follow`'s follower stops at it, with every change it
/// applied on disk and confirmed to PostgreSQL.
pub fn shutdown_flag() -> &'static AtomicBool {
    &SHUTDOWN
}

/// What runs before the shutdown's sync and checkpoint.
type Before = Box<dyn FnOnce() + Send>;
static BEFORE_SHUTDOWN: std::sync::Mutex<Vec<Before>> = std::sync::Mutex::new(Vec::new());

/// Runs `f` once a shutdown signal has arrived, before the final sync and
/// checkpoint: the follower's thread is waited for there, so that the
/// checkpoint holds what it applied last.
pub fn before_shutdown(f: impl FnOnce() + Send + 'static) {
    BEFORE_SHUTDOWN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Box::new(f));
}

fn read_lock(db: &RwLock<Database>) -> RwLockReadGuard<'_, Database> {
    db.read().unwrap_or_else(|e| e.into_inner())
}

fn write_lock(db: &RwLock<Database>) -> RwLockWriteGuard<'_, Database> {
    db.write().unwrap_or_else(|e| e.into_inner())
}

/// The periodic syncer and the shutdown hook of a server over one file, on
/// the calling thread: it returns only through the shutdown's
/// `process::exit`. A `--dir` node runs its own loop over its tenants.
pub fn run_syncer(db: Arc<RwLock<Database>>, policy: SyncPolicy, checkpoint: bool) -> ! {
    let tick = policy.tick();
    loop {
        std::thread::sleep(tick);
        if shutdown_requested() {
            shutdown(&db, checkpoint);
        }
        if !matches!(policy, SyncPolicy::Interval(_)) {
            continue;
        }
        // No need to take the lock when nothing is dirty: idle passes must
        // not block the readers. After a storage error nothing is ever clean
        // again, and every pass would log the same error; the writes that
        // follow are refused by the engine and say it themselves.
        let pending = {
            let g = read_lock(&db);
            g.is_dirty() && g.failure().is_none()
        };
        // The flush takes the exclusive lock, the fsync does not: a reader
        // arriving while the disk works is not held up by it.
        if pending {
            let flushed = write_lock(&db).flush();
            match flushed {
                Ok(Some(durability)) => {
                    if let Err(e) = durability() {
                        fenec_http::log!("sync error: {e}");
                        write_lock(&db).fail(&e);
                    }
                }
                Ok(None) => {}
                Err(e) => fenec_http::log!("sync error: {e}"),
            }
        }
    }
}

/// The final `sync`, an optional checkpoint, exit.
///
/// The exclusive lock is *held until exit*. Releasing it before exiting
/// meant a write accepted between `sync` and `exit` could look successful to
/// the client and never reach the disk; the window was small but silent.
fn shutdown(db: &RwLock<Database>, checkpoint: bool) -> ! {
    let before = std::mem::take(&mut *BEFORE_SHUTDOWN.lock().unwrap_or_else(|e| e.into_inner()));
    for f in before {
        f();
    }
    let mut g = write_lock(db);
    let dirty = g.is_dirty();
    if dirty {
        if let Err(e) = g.sync() {
            fenec_http::log!("sync error: {e}");
        }
    }
    fenec_http::log!(
        "\nshutting down: {}",
        if dirty {
            "writes were pushed to disk"
        } else {
            "no pending writes"
        }
    );

    // The checkpoint comes *after* the `sync` and only buys anything when
    // there is a vector index. Since the whole image is built in memory,
    // peak memory is ~3x the file and shutdown takes longer on a large
    // database; if we are killed meanwhile (docker stop timeout, cgroup
    // OOM) the data is already on disk and only the graph is lost, to be
    // rebuilt on open. Because `rewrite` writes to a side file and renames,
    // a half-written checkpoint cannot corrupt the file.
    if checkpoint && g.stats().iter().any(|s| !s.vector_indexes.is_empty()) {
        match g.checkpoint() {
            Ok(()) => fenec_http::log!("checkpoint written: the HNSW graph is persisted"),
            Err(e) => fenec_http::log!("could not write the checkpoint: {e}"),
        }
    }
    std::process::exit(0);
}
