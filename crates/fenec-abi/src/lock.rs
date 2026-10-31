//! `<file>.lock`, held exclusively for as long as a process has a file
//! open to write: two databases over one file corrupt it, whether they are
//! two processes or two opens in one. The native library takes it as an
//! app opens a file, `fenec mcp --file` as an agent's session does,
//! `fenec-server` as it serves a file and a `--dir` node as it opens a
//! tenant's, `fenec-shard` its directory, and the shell (`fenec <file>`,
//! `-c`, a script, `fenec import`, a backup or a restore into a file)
//! before it opens or writes one, so any two of them refuse each other.
//! Not compiled into the browser module, which has no files.
//!
//! Elsewhere than on a Unix the lock is not taken: [`hold`] makes the file
//! and refuses nothing, in this process or another. No release is built
//! for Windows yet. Nor does a record lock reach across a file system that
//! does not pass it on: two processes on either side of a Docker Desktop
//! bind mount, or of a network file system that keeps no locks, each take
//! the lock of the same file.

use fenec_core::engine::{Durability, ImageOut, Sink};
use fenec_core::error::Result as CoreResult;
use std::fs::File;
use std::io::{Read, Write};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// `<path>.lock`, held exclusively ([`hold`]), and noted among this
/// process's ([`LOCKED`]) until dropped.
pub struct FileLock {
    file: Option<File>,
    /// The lock file's device and inode, under which [`LOCKED`] notes it.
    key: (u64, u64),
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let mut locked = LOCKED.lock().unwrap_or_else(|e| e.into_inner());
        // Closed before the note goes: an open that finds the file free
        // here finds the lock let go of too.
        drop(self.file.take());
        locked.retain(|k| *k != self.key);
        LET_GO.notify_all();
    }
}

/// A database's sink holding `<file>.lock` ([`holding`]) for as long as
/// the database holds the sink, which is its whole life: a sink is set
/// once, at the open. What a file whose database has several owners is
/// opened with -- a tenant's, which the registry, its follower and a
/// linking thread hold -- so the lock goes with the last of them, never
/// before: held by the tenant, a follower writing on after the registry
/// let the tenant go would have written a file the lock no longer kept.
/// Every method is passed on, the defaults too: one left to the trait's
/// default would skip the file's own (a rewrite built whole in memory, a
/// write behind the lock never run).
pub struct Holding {
    /// Dropped first, as fields drop in order: the file is closed before
    /// its lock is let go.
    sink: Box<dyn Sink>,
    _lock: FileLock,
}

/// What wraps a file's sink as it is opened (`fenec_core::fs::Wrap`) to
/// hold `lock` with it ([`Holding`]).
pub fn holding(lock: FileLock) -> impl FnOnce(Box<dyn Sink>) -> CoreResult<Box<dyn Sink>> {
    move |sink| Ok(Box::new(Holding { sink, _lock: lock }) as Box<dyn Sink>)
}

impl Sink for Holding {
    fn append(&mut self, bytes: &[u8]) -> CoreResult<()> {
        self.sink.append(bytes)
    }
    fn record(&mut self, seq: u64, bytes: &[u8]) -> CoreResult<()> {
        self.sink.record(seq, bytes)
    }
    fn append_deferred(&mut self, bytes: &[u8]) -> CoreResult<()> {
        self.sink.append_deferred(bytes)
    }
    fn rewrite(&mut self, bytes: &[u8]) -> CoreResult<()> {
        self.sink.rewrite(bytes)
    }
    fn rewrite_with(
        &mut self,
        image: &mut dyn FnMut(&mut dyn ImageOut) -> CoreResult<()>,
    ) -> CoreResult<()> {
        self.sink.rewrite_with(image)
    }
    fn remapped(&mut self) -> Option<fenec_core::store::Base> {
        self.sink.remapped()
    }
    fn written_through(&mut self) -> CoreResult<Option<fenec_core::store::Base>> {
        self.sink.written_through()
    }
    fn syncing(&self) -> bool {
        self.sink.syncing()
    }
    fn land(&mut self, seq: u64, spilled: &[&[u8]], record: &[u8]) -> CoreResult<()> {
        self.sink.land(seq, spilled, record)
    }
    fn side(&self) -> Option<std::path::PathBuf> {
        self.sink.side()
    }
    fn adopt(&mut self, side: &std::path::Path) -> CoreResult<()> {
        self.sink.adopt(side)
    }
    fn sync_existing(&mut self) -> CoreResult<()> {
        self.sink.sync_existing()
    }
    fn write_out(&mut self) -> CoreResult<()> {
        self.sink.write_out()
    }
    fn write_behind(&mut self, all: bool) -> Option<Durability> {
        self.sink.write_behind(all)
    }
    fn written_all(&self) -> bool {
        self.sink.written_all()
    }
    fn sync(&mut self) -> CoreResult<()> {
        self.sink.sync()
    }
    fn flush(&mut self) -> CoreResult<Option<Durability>> {
        self.sink.flush()
    }
}

/// Why [`hold`] did not take the lock.
#[derive(Debug)]
pub enum Refused {
    /// The file is open already, in this process or another.
    Held,
    /// The lock file could not be made or read.
    Io(std::io::Error),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Held => f.write_str(
                "it is open already, in this process or another: two databases over one file corrupt it",
            ),
            Refused::Io(e) => write!(f, "its lock file: {e}"),
        }
    }
}

/// The lock files this process holds, by device and inode. A record lock
/// is the process's, so it refuses only another process's open; a second
/// open here is refused by this list -- before the lock file is opened
/// again, since closing any descriptor of a file lets go of the process's
/// record locks on it.
static LOCKED: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

/// Told each time this process lets a lock go, for [`hold_within`]'s wait.
static LET_GO: Condvar = Condvar::new();

/// A lock file's device and inode, which [`LOCKED`] notes it by.
#[cfg(unix)]
fn key(m: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (m.dev(), m.ino())
}

#[cfg(not(unix))]
fn key(_: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

/// The lock file of the database at `path`: `<file>.lock` beside the file
/// the path names, a symbolic link in its place followed to what it links
/// to, links to links too -- named as written, a shell through a link to
/// a served file took a lock of its own and wrote beside the server. A
/// directory reached through a link needs nothing: the lock file's name
/// goes through the same link to the same directory. So the path is
/// resolved no further than its last name, one `lstat` where it is no link:
/// resolved whole (`canonicalize`), it was 20 us of a 50 us hold, a node
/// opening a tenant paying it each time. Hard links are two names for one
/// file that no name tells apart: each has a lock of its own.
pub fn lock_path(path: impl AsRef<std::path::Path>) -> std::path::PathBuf {
    let mut real = path.as_ref().to_path_buf();
    // As many links as the system follows in one path before it gives up.
    for _ in 0..40 {
        match std::fs::symlink_metadata(&real) {
            Ok(m) if m.file_type().is_symlink() => match std::fs::read_link(&real) {
                Ok(to) if to.is_absolute() => real = to,
                Ok(to) => {
                    let dir = real.parent().map(std::path::Path::to_path_buf);
                    real = dir.unwrap_or_default().join(to);
                }
                Err(_) => break,
            },
            _ => break,
        }
    }
    // Joined as the system's bytes, not as text: a lossy name would be
    // another file.
    let mut name = real.into_os_string();
    name.push(".lock");
    name.into()
}

/// Holds `<path>.lock` exclusively for as long as the file is open. The
/// file itself cannot carry the lock: a checkpoint or a compact renames a
/// new file over it, and a lock on the old one would let a second opener
/// in. It is a record lock (`fcntl`'s `F_SETLK`), which belongs to the
/// process, not an `flock`, which belongs to the open file: a child
/// spawned while the lock file was open -- a test starting a server, an app
/// starting a helper -- shares that open file until its exec closes it, and
/// a close and an open again in that window found the `flock` still held
/// (37 of 20 000 beside a thread spawning `/usr/bin/true`, none with the
/// record lock). The system lets either go when the process ends, however.
///
/// Once it is taken, the process's id and program are written into the
/// file, through the descriptor that holds the lock, for an opener it
/// refuses to name ([`holder`]). The lock is the record lock alone: what
/// the file holds decides nothing.
pub fn hold(path: impl AsRef<std::path::Path>) -> Result<FileLock, Refused> {
    hold_within(path, Duration::ZERO)
}

/// [`hold`], waiting up to `wait` for a holder in this process to let go.
/// A database this process let go of may be held a moment longer by what
/// ran beside it -- a tenant's follower ending its thread, a linking
/// thread's slice, the graph keeper's append -- and the open that follows
/// a close, a delete or a follow would be refused for nothing, a 500 to a
/// request racing it. Woken as each lock goes ([`LET_GO`]), never polled.
/// Another process's hold is refused at once, as [`hold`] refuses it: it
/// goes when that process chooses.
pub fn hold_within(path: impl AsRef<std::path::Path>, wait: Duration) -> Result<FileLock, Refused> {
    let name = lock_path(path);
    let deadline = Instant::now() + wait;
    let mut locked = LOCKED.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let here = |locked: &Vec<(u64, u64)>| {
            cfg!(unix) && std::fs::metadata(&name).is_ok_and(|m| locked.contains(&key(&m)))
        };
        if !here(&locked) {
            return take(name, locked);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Refused::Held);
        }
        locked = LET_GO
            .wait_timeout(locked, left)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

/// Takes the lock file `name` no lock of this process holds, under
/// [`LOCKED`]'s guard.
fn take(
    name: std::path::PathBuf,
    mut locked: MutexGuard<'_, Vec<(u64, u64)>>,
) -> Result<FileLock, Refused> {
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&name)
        .map_err(Refused::Io)?;
    let meta = lock.metadata().map_err(Refused::Io)?;
    let key = key(&meta);
    #[cfg(unix)]
    if !record::take(&lock) {
        return Err(Refused::Held);
    }
    locked.push(key);
    // Written only now, under the lock: before it, the line was the
    // holder's to keep. Over the old one in place, and cut only where that
    // was of another length: with a truncate each time and the program's
    // name looked up each time, a hold was 50-58 us on an M1, where a
    // tenant's open is 100; now 32-36, 20 of them the lock file's open. A
    // failure costs only the name a refusal gives.
    let line = own_line();
    let _ =
        (&lock)
            .write_all(line.as_bytes())
            .and_then(|()| match meta.len() == line.len() as u64 {
                true => Ok(()),
                false => lock.set_len(line.len() as u64),
            });
    Ok(FileLock {
        file: Some(lock),
        key,
    })
}

/// This process's line ([`Holder::line`]), worked out once: its program
/// does not change while it runs.
fn own_line() -> &'static str {
    static LINE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    LINE.get_or_init(|| Holder::this_process().line())
}

/// The process holding a lock file, as far as can be told ([`holder`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// Its id: the system's (`F_GETLK`), or the one it wrote into the file
    /// where the system names none.
    pub pid: Option<u32>,
    /// Its program's file name, as it wrote it when it took the lock --
    /// `fenec-server`, `fenec`, an app's: taken only where the id it wrote
    /// is the one the system names, or a line an earlier holder left could
    /// name the wrong program.
    pub program: Option<String>,
}

impl Holder {
    fn this_process() -> Holder {
        let exe = std::env::current_exe()
            .ok()
            .or_else(|| std::env::args_os().next().map(Into::into));
        let program = exe
            .as_deref()
            .and_then(std::path::Path::file_name)
            .map(|n| n.to_string_lossy().replace(['\n', '\r'], " "))
            .filter(|n| !n.trim().is_empty());
        Holder {
            pid: Some(std::process::id()),
            program,
        }
    }

    /// What [`hold`] writes into the file: `<pid> <program>`, a line.
    fn line(&self) -> String {
        format!(
            "{} {}\n",
            self.pid.unwrap_or(0),
            self.program.as_deref().unwrap_or("")
        )
    }

    /// A line [`line`](Self::line) wrote, read back: the id and the program.
    fn read(text: &str) -> Option<(u32, Option<String>)> {
        let line = text.lines().next()?;
        let (pid, program) = line.split_once(' ').unwrap_or((line, ""));
        let pid = pid.parse().ok().filter(|p| *p != 0)?;
        let program = Some(program.trim().to_string()).filter(|p| !p.is_empty());
        Some((pid, program))
    }

    /// Whether anything is known of it.
    pub fn is_known(&self) -> bool {
        self.pid.is_some() || self.program.is_some()
    }
}

/// `fenec-server, pid 4242`, as much of it as is known.
impl std::fmt::Display for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.program, self.pid) {
            (Some(p), Some(pid)) => write!(f, "{p}, pid {pid}"),
            (Some(p), None) => f.write_str(p),
            (None, Some(pid)) => write!(f, "pid {pid}"),
            (None, None) => f.write_str("a process the system does not name"),
        }
    }
}

/// Who holds `<path>.lock`, for a refusal to name: this process, where it
/// is among [`LOCKED`]; else the process the system says holds the record
/// lock (`F_GETLK`), with the program it wrote into the file as it took it
/// ([`hold`]). `None` where nothing holds it now, or the file cannot be
/// read. The lock file is opened only once it is known not to be this
/// process's own: closing a descriptor of it would let this process's lock
/// go. A holder that wrote nothing, an older release, is named by its id
/// alone.
pub fn holder(path: impl AsRef<std::path::Path>) -> Option<Holder> {
    let name = lock_path(path);
    // Held across the open and the close, so no `hold` in this process
    // takes the lock meanwhile and has it let go by the close below.
    let locked = LOCKED.lock().unwrap_or_else(|e| e.into_inner());
    let m = std::fs::metadata(&name).ok()?;
    if cfg!(unix) && locked.contains(&key(&m)) {
        return Some(Holder::this_process());
    }
    let file = File::open(&name).ok()?;
    let mut text = String::new();
    // A line, and no more: the file could be anything a user named so.
    let _ = (&file).take(512).read_to_string(&mut text);
    let written = Holder::read(&text);
    #[cfg(unix)]
    let asked = record::holder(&file);
    #[cfg(not(unix))]
    let asked: Option<Option<i32>> = None;
    drop(file);
    drop(locked);
    match asked {
        // Free now: the holder let go since the refusal.
        Some(None) => None,
        // An id of 0 or less names no process here: a holder in another
        // pid namespace, a container's, whose own id the line holds and
        // this one cannot check, or a lock kept by an open file rather than
        // a process (-1, Linux's OFD locks). Held, and named by nothing.
        Some(Some(pid)) if pid <= 0 => Some(Holder {
            pid: None,
            program: None,
        }),
        Some(Some(pid)) => {
            let pid = pid as u32;
            Some(Holder {
                pid: Some(pid),
                program: written.and_then(|(p, program)| program.filter(|_| p == pid)),
            })
        }
        // The system cannot be asked: the line alone.
        None => written.map(|(pid, program)| Holder {
            pid: Some(pid),
            program,
        }),
    }
}

/// An exclusive record lock over the whole file, not waited for, and the
/// process holding one: `struct flock` and the commands as each system lays
/// them out -- the crate takes no `libc`.
#[cfg(unix)]
mod record {
    use std::fs::File;
    use std::os::fd::AsRawFd;

    extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }

    #[cfg(target_vendor = "apple")]
    #[repr(C)]
    struct Flock {
        start: i64,
        len: i64,
        pid: i32,
        kind: i16,
        whence: i16,
    }
    #[cfg(target_vendor = "apple")]
    const F_GETLK: i32 = 7;
    #[cfg(target_vendor = "apple")]
    const F_SETLK: i32 = 8;
    #[cfg(target_vendor = "apple")]
    const F_WRLCK: i16 = 3;

    // `off_t` is a `long` in the `struct flock` of `F_SETLK`, 32 bits on a
    // 32-bit Android.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[repr(C)]
    struct Flock {
        kind: i16,
        whence: i16,
        start: isize,
        len: isize,
        pid: i32,
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const F_GETLK: i32 = 5;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const F_SETLK: i32 = 6;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const F_WRLCK: i16 = 1;

    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
    const F_UNLCK: i16 = 2;

    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
    fn whole() -> Flock {
        Flock {
            kind: F_WRLCK,
            whence: 0,
            start: 0,
            len: 0,
            pid: 0,
        }
    }

    /// Whether the lock was taken.
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
    pub fn take(file: &File) -> bool {
        let l = whole();
        unsafe { fcntl(file.as_raw_fd(), F_SETLK, &l as *const Flock) == 0 }
    }

    /// The id of the process whose lock would refuse this one's, inside
    /// `None` where none would; `None` where the system could not be asked.
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
    pub fn holder(file: &File) -> Option<Option<i32>> {
        let mut l = whole();
        if unsafe { fcntl(file.as_raw_fd(), F_GETLK, &mut l as *mut Flock) } != 0 {
            return None;
        }
        Some(if l.kind == F_UNLCK { None } else { Some(l.pid) })
    }

    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    pub fn take(file: &File) -> bool {
        extern "C" {
            fn flock(fd: i32, op: i32) -> i32;
        }
        const LOCK_EX: i32 = 2;
        const LOCK_NB: i32 = 4;
        unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) == 0 }
    }

    /// An `flock` names no holder.
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    pub fn holder(_: &File) -> Option<Option<i32>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("fenec-abi-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(format!("{}.lock", path.display()));
        path.to_string_lossy().into_owned()
    }

    /// The line a holder writes reads back as what it wrote; one that is
    /// not such a line names no one.
    #[test]
    fn a_holders_line_reads_back() {
        let me = Holder::this_process();
        let (pid, program) = Holder::read(&me.line()).unwrap();
        assert_eq!(Some(pid), me.pid);
        assert_eq!(program, me.program);
        assert_eq!(
            Holder::read("4242 fenec-server\n"),
            Some((4242, Some("fenec-server".into())))
        );
        assert_eq!(Holder::read("4242\n"), Some((4242, None)));
        for not in ["", "\n", "0 fenec", "fenec-server 4242", "-1 x"] {
            assert_eq!(Holder::read(not), None, "{not:?}");
        }
    }

    /// Held here, the holder is this process, named without the lock file
    /// opened again -- which would have let the lock go: a second hold is
    /// still refused after the asking. Let go, nothing holds it, whatever
    /// line the file still holds.
    #[cfg(unix)]
    #[test]
    fn this_processs_own_lock_is_named_and_kept() {
        let p = scratch("own.fenec");
        let held = hold(&p).unwrap();
        let who = holder(&p).expect("a holder");
        assert_eq!(who.pid, Some(std::process::id()));
        assert!(matches!(hold(&p), Err(Refused::Held)));
        drop(held);
        // Read once it is let go: a read of it while held is a descriptor
        // closed, and the lock with it.
        let line = std::fs::read_to_string(format!("{p}.lock")).unwrap();
        assert!(
            line.starts_with(&format!("{} ", std::process::id())),
            "{line:?}"
        );
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
        assert_eq!(holder(&p), None);
        drop(hold(&p).unwrap());
    }

    #[test]
    fn a_holder_reads_as_a_refusal_names_it() {
        let h = |program: Option<&str>, pid| Holder {
            pid,
            program: program.map(Into::into),
        };
        assert_eq!(
            h(Some("fenec-server"), Some(42)).to_string(),
            "fenec-server, pid 42"
        );
        assert_eq!(h(None, Some(42)).to_string(), "pid 42");
        assert_eq!(h(Some("fenec"), None).to_string(), "fenec");
        assert!(!h(None, None).is_known());
    }

    /// A link to a database -- absolute, relative, a link to a link, one
    /// whose file is not made yet -- and a directory reached through a
    /// link take the target's lock: named as written, a shell through a
    /// link wrote beside a server.
    #[cfg(unix)]
    #[test]
    fn a_file_through_a_link_takes_its_targets_lock() {
        use std::os::unix::fs::symlink;
        let p = scratch("target.fenec");
        std::fs::write(&p, b"").unwrap();
        let dir = std::path::Path::new(&p).parent().unwrap().to_path_buf();
        let at = |n: &str| dir.join(n);
        let links = ["link.fenec", "relative.fenec", "chain.fenec"];
        for n in links.iter().chain(&["linked-dir", "dangling.fenec"]) {
            let _ = std::fs::remove_file(at(n));
        }
        symlink(&p, at("link.fenec")).unwrap();
        symlink("target.fenec", at("relative.fenec")).unwrap();
        symlink("link.fenec", at("chain.fenec")).unwrap();
        symlink(&dir, at("linked-dir")).unwrap();
        symlink("not-yet.fenec", at("dangling.fenec")).unwrap();
        for n in links {
            assert_eq!(lock_path(at(n)), lock_path(&p), "{n}");
        }
        assert_eq!(lock_path(at("dangling.fenec")), at("not-yet.fenec.lock"));
        let held = hold(&p).unwrap();
        let through_dir = at("linked-dir").join("target.fenec");
        for other in links.map(at).into_iter().chain([through_dir]) {
            let refused = matches!(hold(&other), Err(Refused::Held));
            assert!(refused, "{}", other.display());
        }
        drop(held);
        drop(hold(at("chain.fenec")).unwrap());
    }

    /// A hold that may wait takes the lock once this process's holder lets
    /// it go, woken by the drop, and gives up after its wait; one that may
    /// not is refused at once.
    #[test]
    fn a_hold_waits_for_this_process_to_let_go() {
        let p = scratch("waits.fenec");
        let first = hold(&p).unwrap();
        assert!(matches!(hold(&p), Err(Refused::Held)));
        let wait = Duration::from_millis(50);
        let t = Instant::now();
        assert!(matches!(hold_within(&p, wait), Err(Refused::Held)));
        assert!(t.elapsed() >= wait);
        let (started, began) = std::sync::mpsc::channel();
        let q = p.clone();
        let waiter = std::thread::spawn(move || {
            started.send(()).unwrap();
            hold_within(&q, Duration::from_secs(600)).map(drop)
        });
        began.recv().unwrap();
        drop(first);
        assert!(waiter.join().unwrap().is_ok());
        drop(hold(&p).unwrap());
    }

    /// Every method of a sink holding a lock reaches the sink it holds: one
    /// left to the trait's default would run the default's body instead --
    /// `record` an `append`, `flush` a `sync`, `side` none.
    #[test]
    fn a_sink_holding_its_lock_passes_every_call_on() {
        use std::sync::{Arc, Mutex};
        struct Rec(Arc<Mutex<Vec<&'static str>>>);
        impl Rec {
            fn note(&self, what: &'static str) {
                self.0.lock().unwrap().push(what);
            }
            fn ok(&self, what: &'static str) -> CoreResult<()> {
                self.note(what);
                Ok(())
            }
        }
        impl Sink for Rec {
            fn append(&mut self, _: &[u8]) -> CoreResult<()> {
                self.ok("append")
            }
            fn record(&mut self, _: u64, _: &[u8]) -> CoreResult<()> {
                self.ok("record")
            }
            fn append_deferred(&mut self, _: &[u8]) -> CoreResult<()> {
                self.ok("append_deferred")
            }
            fn rewrite(&mut self, _: &[u8]) -> CoreResult<()> {
                self.ok("rewrite")
            }
            fn rewrite_with(
                &mut self,
                _: &mut dyn FnMut(&mut dyn ImageOut) -> CoreResult<()>,
            ) -> CoreResult<()> {
                self.ok("rewrite_with")
            }
            fn remapped(&mut self) -> Option<fenec_core::store::Base> {
                self.note("remapped");
                None
            }
            fn written_through(&mut self) -> CoreResult<Option<fenec_core::store::Base>> {
                self.note("written_through");
                Ok(None)
            }
            fn syncing(&self) -> bool {
                self.note("syncing");
                true
            }
            fn land(&mut self, _: u64, _: &[&[u8]], _: &[u8]) -> CoreResult<()> {
                self.ok("land")
            }
            fn side(&self) -> Option<std::path::PathBuf> {
                self.note("side");
                None
            }
            fn adopt(&mut self, _: &std::path::Path) -> CoreResult<()> {
                self.ok("adopt")
            }
            fn sync_existing(&mut self) -> CoreResult<()> {
                self.ok("sync_existing")
            }
            fn write_out(&mut self) -> CoreResult<()> {
                self.ok("write_out")
            }
            fn write_behind(&mut self, _: bool) -> Option<Durability> {
                self.note("write_behind");
                None
            }
            fn written_all(&self) -> bool {
                self.note("written_all");
                false
            }
            fn sync(&mut self) -> CoreResult<()> {
                self.ok("sync")
            }
            fn flush(&mut self) -> CoreResult<Option<Durability>> {
                self.note("flush");
                Ok(None)
            }
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let p = scratch("holding.fenec");
        let mut s = holding(hold(&p).unwrap())(Box::new(Rec(Arc::clone(&seen)))).unwrap();
        s.append(b"").unwrap();
        s.record(1, b"").unwrap();
        s.append_deferred(b"").unwrap();
        s.rewrite(b"").unwrap();
        s.rewrite_with(&mut |_| Ok(())).unwrap();
        s.remapped();
        s.written_through().unwrap();
        assert!(s.syncing());
        s.land(1, &[], b"").unwrap();
        s.side();
        s.adopt(std::path::Path::new("x")).unwrap();
        s.sync_existing().unwrap();
        s.write_out().unwrap();
        s.write_behind(true);
        assert!(!s.written_all());
        s.sync().unwrap();
        s.flush().unwrap();
        let every = [
            "append",
            "record",
            "append_deferred",
            "rewrite",
            "rewrite_with",
            "remapped",
            "written_through",
            "syncing",
            "land",
            "side",
            "adopt",
            "sync_existing",
            "write_out",
            "write_behind",
            "written_all",
            "sync",
            "flush",
        ];
        assert_eq!(*seen.lock().unwrap(), every);
        // Held while the sink is, and let go with it.
        assert!(matches!(hold(&p), Err(Refused::Held)));
        drop(s);
        drop(hold(&p).unwrap());
    }
}
