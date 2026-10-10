//! The dead bytes a file holds, and when a compact is due to give them back.
//!
//! Every write appends its record at the file's end, and the version it
//! replaced -- or the document it deleted -- stays where it was, dead, until
//! a `compact` writes the live records into a new file. Nothing compacted on
//! its own: under YCSB's updates, 1 000 000 records of about 1 KB left a
//! buffered file at 4.7 to 6.5 GB on an 8 GB machine, a live record a few to
//! a page among dead ones, and one read in five waited for the disk -- 30.1k
//! reads a second at one thread, where the same file freshly loaded read
//! 443k. So a host looks every few seconds ([`compact_when_due`]) and runs
//! the compact beside the database once [`CompactPolicy::due`] says so: one
//! rule, here, for the server, the native library and an embedding app.
//!
//! What is dead is counted, not estimated, and nothing is added to the write
//! path for it: every store already counts the bytes of the records it
//! holds and of the versions it no longer reads (`Store::total_bytes`,
//! `dead_bytes`, kept as a write lands and carried in an image's index), and
//! the database the bytes of its file as written (`appended`). The file less
//! the live records is what a compact gives back -- the dead versions, a
//! record's head a write, the graph records a server appended in the tail
//! and the spills of a block put back -- less what the last compact left
//! that is no document: the schemas, the counters, the graphs.

use super::*;
use std::sync::RwLock;

/// What a database's file holds against what it reads: [`Database::garbage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Garbage {
    /// The file's bytes as this database has written them.
    pub file: u64,
    /// The live records' bytes: what a compact would write of the documents.
    pub live: u64,
    /// What the last compact left besides the documents -- schemas,
    /// counters, the graphs -- which the next one would write again.
    pub kept: u64,
}

impl Garbage {
    /// The bytes a compact would give back now.
    pub fn dead(&self) -> u64 {
        self.file.saturating_sub(self.live + self.kept)
    }

    /// The share of the file that is dead, 0 to 1.
    pub fn ratio(&self) -> f64 {
        match self.file {
            0 => 0.0,
            f => self.dead() as f64 / f as f64,
        }
    }
}

/// When a compact is due: once the dead bytes are at least `ratio` of the
/// file and at least `floor` bytes. At the default half, the file stays
/// under twice what the last compact left it at -- each compact writing the
/// live records once for as many bytes of updates -- and the floor keeps a
/// small database from being rewritten for a few megabytes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactPolicy {
    pub ratio: f64,
    pub floor: u64,
}

/// The share of the file [`CompactPolicy::default`] lets be dead.
pub const AUTO_COMPACT_RATIO: f64 = 0.5;

/// The dead bytes under which [`CompactPolicy::default`] compacts nothing.
pub const AUTO_COMPACT_FLOOR: u64 = 64 << 20;

/// How often a host looks: the server's keeper and the native library's
/// thread. A look is a sum over the collections, and a file grows by at
/// most a few seconds' writes past the point it was due.
pub const AUTO_COMPACT_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

impl Default for CompactPolicy {
    fn default() -> Self {
        CompactPolicy {
            ratio: AUTO_COMPACT_RATIO,
            floor: AUTO_COMPACT_FLOOR,
        }
    }
}

impl CompactPolicy {
    /// A policy at `ratio`, the default floor: what `--auto-compact <ratio>`
    /// sets. A ratio outside (0, 1) is refused, since none would ever be
    /// due or every look would be.
    pub fn at(ratio: f64) -> Result<CompactPolicy> {
        if !(ratio > 0.0 && ratio < 1.0) {
            return Err(Error::Query(format!(
                "an auto-compact ratio is a share of the file between 0 and 1, not {ratio}"
            )));
        }
        Ok(CompactPolicy {
            ratio,
            ..CompactPolicy::default()
        })
    }

    /// Whether a compact is due for a file holding `g`.
    pub fn due(&self, g: Garbage) -> bool {
        let dead = g.dead();
        dead >= self.floor && dead as f64 >= self.ratio * g.file as f64
    }
}

impl Database {
    /// What the file holds against what the database reads: a sum over the
    /// collections, no document read.
    pub fn garbage(&self) -> Garbage {
        let live = self
            .collections
            .values()
            .map(|c| c.store.total_bytes().saturating_sub(c.store.dead_bytes()) as u64)
            .sum();
        Garbage {
            file: self.appended.load(Relaxed),
            live,
            kept: self.kept,
        }
    }

    /// Notes what a compact left besides the documents, once its file is in
    /// place: the next one is due only once as many dead bytes as the policy
    /// asks for have come on top.
    ///
    /// What the stores hold, dead or alive, is no part of it: the versions
    /// a compact beside the writes copied and then saw written over, and a
    /// collection it did not compact, are dead in the new file and counted
    /// so. Taken as kept, they raised the bar for the next compact by as
    /// much -- a compact that ran beside a burst of updates of 100 rows left
    /// 101 KB of them, and the file at 307 KB, six times its 53 KB of rows,
    /// was not due again.
    pub(super) fn compacted_to(&mut self, len: u64) {
        let held: u64 = self
            .collections
            .values()
            .map(|c| c.store.total_bytes() as u64)
            .sum();
        self.kept = len.saturating_sub(held);
        self.compactions += 1;
    }

    /// The compacts that rewrote this database's file since it was opened.
    pub fn compactions(&self) -> u64 {
        self.compactions
    }

    /// How long the last compact held the write lock: beside the database,
    /// the writes made meanwhile added and the file put in place; under
    /// the lock, the whole of it.
    pub fn last_compact_held(&self) -> std::time::Duration {
        self.swap_held
    }

    /// Whether a `create index` or a `compact` is running beside the
    /// database: a host's compact waits for the next look.
    pub fn maintaining(&self) -> bool {
        #[cfg(all(feature = "std-fs", unix, target_pointer_width = "64"))]
        if self.beside.load(Relaxed) {
            return true;
        }
        self.watched.load(Relaxed)
    }

    /// Whether `policy` asks for a compact of this database now: dead bytes
    /// enough, a file to write, no maintenance running and no storage error.
    pub fn compact_due(&self, policy: &CompactPolicy) -> bool {
        self.failed.is_none()
            && !self.maintaining()
            && self
                .sink
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .side()
                .is_some()
            && policy.due(self.garbage())
    }
}

/// Runs a compact of `db` beside it if `policy` says one is due: looked at
/// under the read lock and run as `compact` runs on a server, the live
/// records written into a side file with no lock held and the write lock
/// taken only to add the writes made meanwhile and put the file in place
/// ([`Database::maintain`]). `None` when nothing was due.
///
/// The look waits for a write under way. Passed over while the lock was
/// taken (`try_read`), a due compact waited for the next look, 5 s later:
/// under one writer updating a million 1 KB records, 10 looks in 12 runs
/// of `make compact-bench` found the lock taken, each putting a compact
/// off while the file grew 100 to 160 MB a second -- the runs that had
/// them began compacts at up to 3.0 GB, the others at up to 2.7.
///
/// A host that embeds the engine calls this from a thread of its own every
/// few seconds, or starts a [`Compactor`]; the browser module has neither
/// threads nor a file to write beside, and compacts when its page says so.
pub fn compact_when_due(db: &RwLock<Database>, policy: &CompactPolicy) -> Option<Result<Response>> {
    let due = db
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .compact_due(policy);
    if !due {
        return None;
    }
    Database::maintain(db, &Statement::Compact(None))
}

/// A thread that compacts one database when it is due, looking every
/// [`AUTO_COMPACT_EVERY`]: what the native library starts for each file it
/// opens, and what an app embedding the engine may start for its own. It
/// holds the database only while it looks, and stops when dropped or once
/// the database is.
pub struct Compactor {
    stop: Arc<(Mutex<bool>, std::sync::Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Compactor {
    pub fn start(db: &Arc<RwLock<Database>>, policy: CompactPolicy) -> std::io::Result<Compactor> {
        Self::start_every(db, policy, AUTO_COMPACT_EVERY)
    }

    /// [`Self::start`], looking every `every`.
    pub fn start_every(
        db: &Arc<RwLock<Database>>,
        policy: CompactPolicy,
        every: std::time::Duration,
    ) -> std::io::Result<Compactor> {
        let stop = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let (s, db) = (Arc::clone(&stop), Arc::downgrade(db));
        let thread = std::thread::Builder::new()
            .name("fenec-compact".into())
            .spawn(move || loop {
                {
                    let (lock, cv) = &*s;
                    let held = lock.lock().unwrap_or_else(|e| e.into_inner());
                    let (held, _) = cv
                        .wait_timeout_while(held, every, |stop| !*stop)
                        .unwrap_or_else(|e| e.into_inner());
                    if *held {
                        return;
                    }
                }
                let Some(db) = db.upgrade() else {
                    return;
                };
                // An error leaves the database as it was -- a compact that
                // fails is put back -- and the next look tries again; a
                // storage error stops the looks, since `compact_due` asks.
                let _ = compact_when_due(&db, &policy);
            })?;
        Ok(Compactor {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Compactor {
    fn drop(&mut self) {
        let (lock, cv) = &*self.stop;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
        if let Some(t) = self.thread.take() {
            // Dropped on the thread itself -- its last strong reference to
            // the database let go there -- it cannot join itself.
            if t.thread().id() != std::thread::current().id() {
                let _ = t.join();
            }
        }
    }
}
