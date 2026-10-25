//! When writes reach the disk, and what a shutdown signal does.
//!
//! - Writes are pushed to disk periodically by the background syncer, or
//!   after each write ([`SyncPolicy`]); the fsync runs outside the
//!   exclusive lock, so readers are not held up by the disk.
//! - On `SIGINT`, `SIGTERM` or `SIGHUP` a final `sync` runs, and after it
//!   -- when there is a vector index -- a checkpoint, otherwise every
//!   restart would link the HNSW graph again from the documents.
//! - Under `--exit-on-stdin-close` the end of stdin starts that same
//!   shutdown ([`exit_on_stdin_close`]), and so does a failure of what runs
//!   beside the database ([`stop_with`]): every way a server stops sets
//!   one flag, and the syncer that sees it is the only thing that exits.

use fenec_core::prelude::*;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

/// Whether a shutdown signal arrived. The signal handler writes only this
/// atomic; the syncer thread sees it and performs the final `sync`.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// The code the shutdown exits with: 0, or the first [`stop_with`] gave.
static EXIT_CODE: AtomicI32 = AtomicI32::new(0);

/// How often a syncer looks at the shutdown flag, whatever its interval.
/// Sleeping the whole interval first, `--sync 5000` exited 5 s after its
/// stdin closed, holding `<file>.lock` against the app's next open of the
/// file, and `--sync 60000` would have let docker's SIGKILL, 10 s after
/// its SIGTERM, come before the final sync.
pub const LOOK: Duration = Duration::from_millis(200);

/// Waits out one interval of a syncer, `every`, from now: `true` once it
/// has passed, `false` as soon as `stop` is set, which it looks at every
/// [`LOOK`] meanwhile and once more at the end. The interval is counted
/// from the call, after the last pass's work, as a plain sleep counted it,
/// so the flushes come as often as they did.
pub fn wait_tick(every: Duration, stop: &AtomicBool) -> bool {
    let due = Instant::now() + every;
    loop {
        std::thread::sleep(due.saturating_duration_since(Instant::now()).min(LOOK));
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        if Instant::now() >= due {
            return true;
        }
    }
}

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

/// Starts the shutdown a signal starts, from inside the process. It sets
/// the flag the signal handler sets and nothing else, so the syncer that
/// sees it runs the one final sync, checkpoint and exit: a second way
/// down of its own would be a second place to keep the write lock held
/// to the exit, and a `--dir` node's tenants and `--follow`'s follower
/// would each have to be taught it.
pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// [`request_shutdown`] for a failure: what runs beside the database -- the
/// listener, `--follow`'s mirror -- stopped, and the server goes down the
/// one shutdown and exits with `code`, the first given. Exiting where it
/// failed would cut short a shutdown already under way, its sync among
/// what was cut, and skip the sync of one that was not.
pub fn stop_with(code: i32) {
    let _ = EXIT_CODE.compare_exchange(0, code, Ordering::SeqCst, Ordering::SeqCst);
    request_shutdown();
}

/// The code the shutdown exits with: 0 unless [`stop_with`] gave another.
pub fn exit_code() -> i32 {
    EXIT_CODE.load(Ordering::SeqCst)
}

/// `--exit-on-stdin-close`: a thread that reads stdin until it ends -- the
/// other end of the pipe closed, or a read failed -- and then starts the
/// shutdown SIGTERM starts.
///
/// For a parent that holds a pipe on the server's stdin, an app opening a
/// file in it. A parent that is killed (`kill -9`, a crash) sends no
/// signal, and its server went on running with nobody to answer, holding
/// `<file>.lock`: the file could not be opened again until something
/// reaped it. However a process ends, the kernel closes its descriptors,
/// so the pipe's end comes where the signal does not. Watching the
/// parent's pid instead is a poll, late by its interval and fooled by a
/// pid used again, and macOS has no parent-death signal (Linux's
/// `PR_SET_PDEATHSIG`).
///
/// What arrives on stdin is read into one fixed buffer and dropped: nothing
/// here wants it, and a parent writing to a pipe nobody read would block
/// once the pipe filled. Read from a terminal, a job sent to the background
/// would be stopped whole by `SIGTTIN`, its lock held, so the flag refuses
/// one (`main`). `/dev/null`, or a stdin closed before the start, ends at
/// once, and the server with it. A stdin handed over non-blocking answers
/// `WouldBlock` while the pipe is open and empty, which is no end: it is
/// asked again every [`LOOK`], as a syncer looks at the flag.
pub fn exit_on_stdin_close() -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("fenec-stdin".into())
        .spawn(|| {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 4096];
            let ended = loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break None,
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(LOOK)
                    }
                    Err(e) => break Some(e),
                }
            };
            // The flag first: the line is only what the log says of it.
            request_shutdown();
            match ended {
                None => fenec_http::log!("stdin closed: shutting down"),
                Some(e) => fenec_http::log!("stdin could not be read ({e}): shutting down"),
            }
        })
        .map(|_| ())
}

/// The flag a shutdown signal sets, for a thread of the server's that has
/// to stop with it: `--follow`'s follower stops at it, with every change it
/// applied on disk and confirmed to PostgreSQL.
pub fn shutdown_flag() -> &'static AtomicBool {
    &SHUTDOWN
}

/// How long a shutdown waits for the answers of the requests it ends
/// (`Fenec-Wait`) to be written.
pub const HELD_GRACE: Duration = Duration::from_secs(2);

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
        if !wait_tick(tick, &SHUTDOWN) {
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
    // Claims held until a job comes (`Fenec-Wait`) are answered first, with
    // the nothing they found: left held, each was cut off with the process
    // and its worker learned of the shutdown from a broken connection.
    fenec_http::waits::end_all(HELD_GRACE);
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
    std::process::exit(exit_code());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tick comes no sooner than its interval, as the sleep it replaced
    /// did: the flushes of `--sync <ms>` are as far apart as they were.
    #[test]
    fn a_tick_comes_no_sooner_than_its_interval() {
        let started = Instant::now();
        assert!(wait_tick(
            Duration::from_millis(450),
            &AtomicBool::new(false)
        ));
        assert!(started.elapsed() >= Duration::from_millis(450));
    }

    /// A shutdown asked for in the middle of a long interval is seen long
    /// before its end: the bound is the interval it stands against, half of
    /// it, not the 200 ms it is looked at by.
    #[test]
    fn a_shutdown_is_seen_long_before_a_long_interval_ends() {
        let stop = Arc::new(AtomicBool::new(false));
        let setter = Arc::clone(&stop);
        let started = Instant::now();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            setter.store(true, Ordering::SeqCst);
        });
        assert!(!wait_tick(Duration::from_secs(60), &stop));
        assert!(started.elapsed() < Duration::from_secs(30));
        t.join().unwrap();
    }
}
