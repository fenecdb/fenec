//! Many databases behind one listener: one `.fenec` file per tenant.
//!
//! **Why a file per tenant and not a tenant field.** A tenant is picked by
//! the request (`/t/<name>/...`), never by the query text, so nothing
//! rewrites a query to add `tenant = ...` -- and two tenants sharing a
//! collection would then read each other's rows. A file each keeps them
//! apart by construction, and settles four things a shared file would have
//! to solve: ids are counted per file, the `/changes` sequence is one number
//! per file, BM25 statistics are the tenant's own, and a `lookup` never
//! crosses a file.
//!
//! **Opened on first use, not at start.** Open cost is proportional to the
//! file (the whole image is read and replayed), so a node with thousands of
//! tenants would spend its start-up on tenants nobody asks for. A tenant is
//! opened by the first request that names it and closed again by
//! [`Tenants::close_idle`] once nothing holds it.
//!
//! **One instance per file, always.** Two `Database` values over one file
//! corrupt it, exactly as two processes would. A tenant is opened and
//! closed under its own slot lock, and closed only when the slot holds the
//! last reference -- so a stream or a request still using it keeps it open,
//! and a request arriving mid-close waits for the close to finish before it
//! opens the file again.

use crate::sse::Hub;
use fenec_core::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

/// Ceiling on a tenant name. It becomes a file name, so it is also kept to
/// a character set no file system treats specially.
pub const MAX_NAME: usize = 64;

/// How long `delete` waits for the requests and streams on a tenant to let
/// go before it gives up with 409.
const RELEASE_WAIT: Duration = Duration::from_secs(5);

type Setup = Box<dyn Fn(&mut Database) -> Result<()> + Send + Sync>;

/// A registry failure, already shaped as an HTTP status and message.
#[derive(Debug)]
pub struct Refused(pub u16, pub String);

pub struct Tenant {
    name: String,
    pub db: Arc<RwLock<Database>>,
    pub hub: Arc<Hub>,
    /// Held shared by every request for the length of its handling, and
    /// exclusively by `freeze`. A write that passed the frozen check but has
    /// not taken the database lock yet would otherwise land *after* the
    /// export that was meant to be final.
    gate: RwLock<()>,
    frozen: AtomicBool,
    /// Milliseconds since the registry's epoch.
    last_used: AtomicU64,
}

impl Tenant {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::SeqCst)
    }

    /// Holds the tenant against `freeze` while a request is being handled.
    pub fn enter(&self) -> RwLockReadGuard<'_, ()> {
        self.gate.read().unwrap_or_else(|e| e.into_inner())
    }

    fn read(&self) -> RwLockReadGuard<'_, Database> {
        self.db.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Database> {
        self.db.write().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Tenant {
    /// The last reference is going: whatever is buffered reaches the disk.
    /// `FileSink` would flush on its own drop, but without an fsync.
    fn drop(&mut self) {
        let mut g = self.write();
        if g.is_dirty() {
            if let Err(e) = g.sync() {
                eprintln!("sync error ({}): {e}", self.name);
            }
        }
    }
}

pub struct Tenants {
    dir: PathBuf,
    /// Only the tenants that are open, or being opened or closed right now.
    /// Held for a lookup and nothing longer: opening a file (seconds for a
    /// large one with no graph in it) happens under the tenant's own slot
    /// lock, so a slow open stalls requests for that tenant and no other.
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    shutting_down: AtomicBool,
    epoch: Instant,
    setup: Option<Setup>,
    change_capacity: usize,
    max_memory: usize,
    checkpoint: bool,
}

/// One tenant's open/closed state. Opening and closing a tenant both happen
/// under this lock, which is what makes "one instance per file" hold: a
/// request arriving mid-close waits and then opens the file again, rather
/// than opening it next to the instance still being closed.
struct Slot {
    held: Mutex<Held>,
}

enum Held {
    Closed,
    Open(Arc<Tenant>),
}

impl Tenants {
    /// A registry over `dir`, created when missing. Nothing is opened yet.
    pub fn new(dir: impl Into<PathBuf>) -> std::io::Result<Tenants> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Tenants {
            dir,
            slots: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            epoch: Instant::now(),
            setup: None,
            change_capacity: fenec_core::changes::DEFAULT_CAPACITY,
            max_memory: 0,
            checkpoint: true,
        })
    }

    /// Runs on every database as it is opened -- `fenec-pg` installs its
    /// plugin here, so a tenant sees the same functions a single file does.
    pub fn with_setup(
        mut self,
        f: impl Fn(&mut Database) -> Result<()> + Send + Sync + 'static,
    ) -> Tenants {
        self.setup = Some(Box::new(f));
        self
    }

    pub fn with_change_capacity(mut self, n: usize) -> Tenants {
        self.change_capacity = n;
        self
    }

    /// Ceiling on the summed footprint of the *open* tenants (0 = off).
    /// Opening one more over it first closes idle tenants, oldest first.
    pub fn with_max_memory(mut self, bytes: usize) -> Tenants {
        self.max_memory = bytes;
        self
    }

    /// Whether closing a tenant (idle, delete, shutdown) writes a checkpoint
    /// when it has a vector index -- the same trade as `--no-checkpoint`.
    pub fn with_checkpoint(mut self, on: bool) -> Tenants {
        self.checkpoint = on;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn registry(&self) -> MutexGuard<'_, HashMap<String, Arc<Slot>>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.fenec"))
    }

    fn now(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Runs `f` on the tenant's slot, locked. The slot is re-checked against
    /// the registry once locked: one that was dropped in the meantime (the
    /// tenant closed and a new slot took its place) is not the one to act
    /// on, and acting on it could open a second instance of the file. A
    /// slot left closed is dropped from the registry on the way out, so the
    /// map holds open tenants only -- a request naming tenants that do not
    /// exist leaves nothing behind.
    ///
    /// Lock order is slot, then registry; nothing takes a slot lock while
    /// holding the registry.
    fn with_slot<R>(&self, name: &str, f: impl FnOnce(&mut Held) -> R) -> R {
        loop {
            let slot = Arc::clone(self.registry().entry(name.to_string()).or_insert_with(|| {
                Arc::new(Slot {
                    held: Mutex::new(Held::Closed),
                })
            }));
            let mut held = slot.held.lock().unwrap_or_else(|e| e.into_inner());
            let current = self
                .registry()
                .get(name)
                .is_some_and(|s| Arc::ptr_eq(s, &slot));
            if !current {
                continue;
            }
            let out = f(&mut held);
            if matches!(*held, Held::Closed) {
                let mut reg = self.registry();
                if reg.get(name).is_some_and(|s| Arc::ptr_eq(s, &slot)) {
                    reg.remove(name);
                }
            }
            return out;
        }
    }

    /// A snapshot of the slots, taken without holding the registry after.
    fn all_slots(&self) -> Vec<Arc<Slot>> {
        self.registry().values().cloned().collect()
    }

    fn refuse_if_shutting_down(&self) -> std::result::Result<(), Refused> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(Refused(503, "the node is shutting down".into()));
        }
        Ok(())
    }

    /// The tenant, opened if it is on disk and not open yet. 404 when there
    /// is no such file: a tenant comes into being through `create`, never
    /// by a request naming it, or a typo would make an empty database.
    pub fn get(&self, name: &str) -> std::result::Result<Arc<Tenant>, Refused> {
        check_name(name)?;
        let t = self.with_slot(name, |held| {
            if let Held::Open(t) = held {
                t.last_used.store(self.now(), Ordering::Relaxed);
                return Ok((Arc::clone(t), false));
            }
            self.refuse_if_shutting_down()?;
            let path = self.path(name);
            if !path.exists() {
                return Err(Refused(404, format!("no tenant `{name}` on this node")));
            }
            let t = self.open_file(name, &path)?;
            *held = Held::Open(Arc::clone(&t));
            Ok((t, true))
        });
        let (t, opened) = t?;
        if opened {
            self.make_room(name);
        }
        Ok(t)
    }

    /// Creates an empty tenant. 409 when it exists.
    pub fn create(&self, name: &str) -> std::result::Result<Arc<Tenant>, Refused> {
        check_name(name)?;
        let t = self.with_slot(name, |held| {
            let path = self.path(name);
            if matches!(held, Held::Open(_)) || path.exists() {
                return Err(Refused(409, format!("tenant `{name}` already exists")));
            }
            self.refuse_if_shutting_down()?;
            let t = self.open_file(name, &path)?;
            *held = Held::Open(Arc::clone(&t));
            Ok(t)
        })?;
        self.make_room(name);
        Ok(t)
    }

    fn open_file(&self, name: &str, path: &Path) -> std::result::Result<Arc<Tenant>, Refused> {
        let mut db = fenec_core::fs::open(path)
            .map_err(|e| Refused(500, format!("could not open tenant `{name}`: {e}")))?;
        if let Some(setup) = &self.setup {
            setup(&mut db).map_err(|e| Refused(500, e.to_string()))?;
        }
        let hub = Hub::new();
        db.set_watcher(Arc::clone(&hub) as Arc<dyn Watcher>);
        db.set_change_capacity(self.change_capacity);
        Ok(Arc::new(Tenant {
            name: name.to_string(),
            db: Arc::new(RwLock::new(db)),
            hub,
            gate: RwLock::new(()),
            frozen: AtomicBool::new(false),
            last_used: AtomicU64::new(self.now()),
        }))
    }

    /// Closes the slot's tenant if nothing but the slot holds it and it is
    /// not frozen: checkpoint when there is a graph to keep, then the last
    /// `Arc` goes and `Drop` does the final sync -- all under the slot lock.
    fn close(&self, held: &mut Held) -> bool {
        let Held::Open(t) = &*held else {
            return false;
        };
        if Arc::strong_count(t) > 1 || t.is_frozen() {
            return false;
        }
        if self.checkpoint {
            let mut g = t.write();
            if g.stats().iter().any(|s| !s.vector_indexes.is_empty()) {
                if let Err(e) = g.checkpoint() {
                    eprintln!("could not write the checkpoint ({}): {e}", t.name);
                }
            }
        }
        *held = Held::Closed;
        true
    }

    /// Closes idle tenants, least recently used first, until the open ones
    /// fit under the ceiling or nothing more can be closed. Over the ceiling
    /// with everything busy, the open goes ahead anyway: refusing a read
    /// because other tenants are busy would turn a memory warning into an
    /// outage. A tenant in the middle of a write is counted as zero rather
    /// than waited for -- the ceiling is an early warning, not a guarantee.
    fn make_room(&self, just_opened: &str) {
        if self.max_memory == 0 {
            return;
        }
        let mut used = 0usize;
        let mut idle: Vec<(u64, String)> = Vec::new();
        for slot in self.all_slots() {
            let Ok(held) = slot.held.try_lock() else {
                continue;
            };
            let Held::Open(t) = &*held else { continue };
            used += t.db.try_read().map(|g| g.memory_bytes()).unwrap_or(0);
            if t.name != just_opened && Arc::strong_count(t) == 1 && !t.is_frozen() {
                idle.push((t.last_used.load(Ordering::Relaxed), t.name.clone()));
            }
        }
        idle.sort();
        for (_, name) in idle {
            if used < self.max_memory {
                break;
            }
            used = used.saturating_sub(self.with_slot(&name, |held| {
                let freed = match &*held {
                    Held::Open(t) => t.db.try_read().map(|g| g.memory_bytes()).unwrap_or(0),
                    Held::Closed => 0,
                };
                if self.close(held) {
                    freed
                } else {
                    0
                }
            }));
        }
    }

    /// The open tenants, cloned out of their slots. A slot that is busy
    /// opening or closing is skipped: it is not open yet, or not any more.
    fn open_tenants(&self) -> Vec<Arc<Tenant>> {
        self.all_slots()
            .iter()
            .filter_map(|s| match &*s.held.try_lock().ok()? {
                Held::Open(t) => Some(Arc::clone(t)),
                Held::Closed => None,
            })
            .collect()
    }

    /// Pushes every dirty open tenant to disk. The periodic syncer calls it.
    pub fn sync_dirty(&self) {
        for t in self.open_tenants() {
            if t.read().is_dirty() {
                if let Err(e) = t.write().sync() {
                    eprintln!("sync error ({}): {e}", t.name);
                }
            }
        }
    }

    /// Closes the tenants untouched for `idle` that nothing holds. A frozen
    /// tenant stays: the freeze lives on the open instance.
    pub fn close_idle(&self, idle: Duration) -> usize {
        let cutoff = self.now().saturating_sub(idle.as_millis() as u64);
        let names: Vec<String> = self
            .open_tenants()
            .iter()
            .filter(|t| t.last_used.load(Ordering::Relaxed) <= cutoff)
            .map(|t| t.name.clone())
            .collect();
        names
            .iter()
            .filter(|name| {
                self.with_slot(name, |held| match held {
                    Held::Open(t) if t.last_used.load(Ordering::Relaxed) <= cutoff => {
                        self.close(held)
                    }
                    _ => false,
                })
            })
            .count()
    }

    /// Stops writes to a tenant and waits for the ones in flight: once it
    /// returns, the database will not change until `thaw`.
    pub fn freeze(&self, name: &str) -> std::result::Result<(), Refused> {
        let t = self.get(name)?;
        t.frozen.store(true, Ordering::SeqCst);
        drop(t.gate.write().unwrap_or_else(|e| e.into_inner()));
        Ok(())
    }

    pub fn thaw(&self, name: &str) -> std::result::Result<(), Refused> {
        let t = self.get(name)?;
        t.frozen.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// The tenant's whole image, graph included -- what a checkpoint would
    /// write, so the receiving node opens it without rebuilding anything.
    pub fn export(&self, name: &str) -> std::result::Result<Vec<u8>, Refused> {
        let t = self.get(name)?;
        let image = t.read().snapshot();
        Ok(image)
    }

    /// Installs an image as a new tenant. It is replayed in memory first, so
    /// a truncated or corrupt transfer is refused before it reaches the
    /// directory; the file then appears by rename, never half-written.
    pub fn import(&self, name: &str, image: &[u8]) -> std::result::Result<(), Refused> {
        check_name(name)?;
        let mut check = Database::new();
        check
            .load(image)
            .map_err(|e| Refused(400, format!("the image does not load: {e}")))?;
        drop(check);

        self.with_slot(name, |held| {
            let path = self.path(name);
            if matches!(held, Held::Open(_)) || path.exists() {
                return Err(Refused(409, format!("tenant `{name}` already exists")));
            }
            let tmp = self.dir.join(format!("{name}.fenec.importing"));
            let written = std::fs::File::create(&tmp).and_then(|mut f| {
                use std::io::Write;
                f.write_all(image)?;
                f.sync_all()
            });
            if let Err(e) = written.and_then(|_| std::fs::rename(&tmp, &path)) {
                let _ = std::fs::remove_file(&tmp);
                return Err(Refused(
                    500,
                    format!("could not write tenant `{name}`: {e}"),
                ));
            }
            Ok(())
        })
    }

    /// Closes the tenant and removes its file. The streams on it are told to
    /// end; a request still running is waited for up to [`RELEASE_WAIT`].
    /// Requests for this tenant wait meanwhile; other tenants do not notice.
    pub fn delete(&self, name: &str) -> std::result::Result<(), Refused> {
        check_name(name)?;
        self.with_slot(name, |held| {
            if let Held::Open(t) = &*held {
                t.hub.close();
                let deadline = Instant::now() + RELEASE_WAIT;
                while Arc::strong_count(t) > 1 {
                    if Instant::now() >= deadline {
                        return Err(Refused(409, format!("tenant `{name}` is still in use")));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                // The last `Arc`: dropping it syncs, and the file is closed
                // before it is removed.
                *held = Held::Closed;
            }
            let path = self.path(name);
            if !path.exists() {
                return Err(Refused(404, format!("no tenant `{name}` on this node")));
            }
            std::fs::remove_file(&path)
                .map_err(|e| Refused(500, format!("could not remove tenant `{name}`: {e}")))?;
            let _ = std::fs::remove_file(path.with_extension("fenec.compacting"));
            Ok(())
        })
    }

    /// Every tenant on disk, sorted.
    pub fn names(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for e in entries.flatten() {
                let file = e.file_name();
                let Some(file) = file.to_str() else { continue };
                if let Some(name) = file.strip_suffix(".fenec") {
                    if check_name(name).is_ok() {
                        out.push(name.to_string());
                    }
                }
            }
        }
        out.sort();
        out
    }

    /// `(name, footprint)` of the open tenants and the bytes on disk of all
    /// of them -- what a router places new tenants by.
    pub fn stats(&self) -> Stats {
        let mut open: Vec<(String, usize, bool)> = self
            .open_tenants()
            .iter()
            .map(|t| (t.name.clone(), t.read().memory_bytes(), t.is_frozen()))
            .collect();
        open.sort();
        let names = self.names();
        let disk = names
            .iter()
            .filter_map(|n| std::fs::metadata(self.path(n)).ok())
            .map(|m| m.len())
            .sum();
        Stats {
            tenants: names.len(),
            disk,
            open,
        }
    }

    /// The shutdown path: no tenant opens from here on, and every open one
    /// is synced, checkpointed when it has a graph, and its slot and write
    /// lock are **kept** -- the caller exits with them held, so no write is
    /// accepted between the last sync and exit. Returns how many were open.
    pub fn shutdown(&self) -> usize {
        // The flag first, the snapshot after: an open that got past the flag
        // inserted its slot before the snapshot, and is waited for below.
        self.shutting_down.store(true, Ordering::SeqCst);
        let mut count = 0;
        for slot in self.all_slots() {
            let held = slot.held.lock().unwrap_or_else(|e| e.into_inner());
            if let Held::Open(t) = &*held {
                count += 1;
                let mut g = t.write();
                if g.is_dirty() {
                    if let Err(e) = g.sync() {
                        eprintln!("sync error ({}): {e}", t.name);
                    }
                }
                if self.checkpoint && g.stats().iter().any(|s| !s.vector_indexes.is_empty()) {
                    if let Err(e) = g.checkpoint() {
                        eprintln!("could not write the checkpoint ({}): {e}", t.name);
                    }
                }
                // Leaked on purpose: the process is about to exit, and a lock
                // released here would let a session write after the final sync.
                std::mem::forget(g);
            }
            std::mem::forget(held);
        }
        count
    }
}

pub struct Stats {
    pub tenants: usize,
    pub disk: u64,
    /// `(name, memory_bytes, frozen)`
    pub open: Vec<(String, usize, bool)>,
}

/// `[a-z0-9_-]{1,64}`: safe as a file name everywhere, no traversal, and
/// the same name on a case-insensitive file system.
pub fn check_name(name: &str) -> std::result::Result<(), Refused> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(Refused(
            400,
            format!("invalid tenant name `{name}`: [a-z0-9_-], 1 to {MAX_NAME} characters"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fenec-registry-{tag}-{}-{:?}",
            std::process::id(),
            Instant::now()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn run(t: &Tenant, sql: &str) {
        let stmt = fenec_ql::parse_one(sql).unwrap();
        t.db.write().unwrap().execute(&stmt).unwrap();
    }

    #[test]
    fn a_missing_tenant_leaves_nothing_in_the_registry() {
        let dir = scratch("missing");
        let reg = Tenants::new(&dir).unwrap();
        for i in 0..100 {
            let Err(Refused(status, _)) = reg.get(&format!("nope{i}")) else {
                panic!("a tenant that is not on disk opened");
            };
            assert_eq!(status, 404);
        }
        assert!(reg.registry().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_slow_open_does_not_stall_other_tenants() {
        let dir = scratch("slow");
        // Every open takes 400 ms, as a large file without its graph would.
        let reg = Arc::new(Tenants::new(&dir).unwrap().with_setup(|_| {
            std::thread::sleep(Duration::from_millis(400));
            Ok(())
        }));
        reg.create("fast").unwrap();
        reg.create("slow").unwrap();
        assert_eq!(reg.close_idle(Duration::ZERO), 1 + 1);
        let fast = reg.get("fast").unwrap();
        drop(fast);

        let opener = Arc::clone(&reg);
        let slow = std::thread::spawn(move || opener.get("slow").map(|_| ()));
        std::thread::sleep(Duration::from_millis(50));
        let t = Instant::now();
        reg.get("fast").unwrap();
        assert!(
            t.elapsed() < Duration::from_millis(100),
            "an open tenant waited {:?} for another one's open",
            t.elapsed()
        );
        slow.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn closing_and_reopening_under_load_loses_nothing() {
        // Writers and a closer racing: a second instance of the file would
        // show up as lost or duplicated documents.
        let dir = scratch("race");
        let reg = Arc::new(Tenants::new(&dir).unwrap());
        run(
            &reg.create("t").unwrap(),
            "create collection n (w int, i int)",
        );

        let stop = Arc::new(AtomicBool::new(false));
        let closer = {
            let (reg, stop) = (Arc::clone(&reg), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut closed = 0;
                while !stop.load(Ordering::SeqCst) {
                    closed += reg.close_idle(Duration::ZERO);
                }
                closed
            })
        };
        let writers: Vec<_> = (0..4)
            .map(|w| {
                let reg = Arc::clone(&reg);
                std::thread::spawn(move || {
                    for i in 0..200 {
                        let t = reg.get("t").unwrap();
                        run(&t, &format!("put n {{w: {w}, i: {i}}}"));
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        stop.store(true, Ordering::SeqCst);
        let closed = closer.join().unwrap();
        assert!(closed > 0, "the closer never got a turn");

        reg.close_idle(Duration::ZERO);
        let db = fenec_core::fs::open(dir.join("t.fenec")).unwrap();
        let stmt = fenec_ql::parse_one("get n count").unwrap();
        let Ok(Response::Rows(rs)) = db.query(&stmt, &[]) else {
            panic!("count")
        };
        assert_eq!(rs.rows[0].values[0], Value::Int(800));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn names_are_file_safe() {
        assert!(check_name("acme").is_ok());
        assert!(check_name("a-b_9").is_ok());
        assert!(check_name("").is_err());
        assert!(check_name("..").is_err());
        assert!(check_name("a/b").is_err());
        assert!(check_name("Acme").is_err());
        assert!(check_name(&"a".repeat(MAX_NAME + 1)).is_err());
    }
}
