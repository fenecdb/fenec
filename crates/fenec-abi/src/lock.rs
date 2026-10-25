//! `<file>.lock`, held exclusively for as long as a process has a file
//! open to write: two databases over one file corrupt it, whether they are
//! two processes or two opens in one. The native library takes it as an
//! app opens a file, `fenec mcp --file` as an agent's session does,
//! `fenec-server` as it serves one, and the shell (`fenec <file>`, `-c`, a
//! script, `fenec import`, a backup or a restore into a file) before it
//! opens or writes one, so any two of them refuse each other. Not compiled
//! into the browser module, which has no files.
//!
//! Elsewhere than on a Unix the lock is not taken: [`hold`] makes the file
//! and refuses nothing, in this process or another. No release is built
//! for Windows yet.

use std::fs::File;
use std::io::{Read, Write};
use std::sync::Mutex;

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
pub fn hold(path: &str) -> Result<FileLock, Refused> {
    let name = format!("{path}.lock");
    let mut locked = LOCKED.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(m) = std::fs::metadata(&name) {
        if cfg!(unix) && locked.contains(&key(&m)) {
            return Err(Refused::Held);
        }
    }
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&name)
        .map_err(Refused::Io)?;
    let key = key(&lock.metadata().map_err(Refused::Io)?);
    #[cfg(unix)]
    if !record::take(&lock) {
        return Err(Refused::Held);
    }
    locked.push(key);
    // Cut only now, under the lock: before it, the line was the holder's
    // to keep. A failure to write it costs only the name a refusal gives.
    let _ = lock
        .set_len(0)
        .and_then(|()| (&lock).write_all(Holder::this_process().line().as_bytes()));
    Ok(FileLock {
        file: Some(lock),
        key,
    })
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
pub fn holder(path: &str) -> Option<Holder> {
    let name = format!("{path}.lock");
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
    let asked: Option<Option<u32>> = None;
    drop(file);
    drop(locked);
    match asked {
        // Free now: the holder let go since the refusal.
        Some(None) => None,
        // An id of 0: a holder in another pid namespace, a container's,
        // whose own id the line holds and this one cannot check.
        Some(Some(0)) => Some(Holder {
            pid: None,
            program: None,
        }),
        Some(Some(pid)) => Some(Holder {
            pid: Some(pid),
            program: written.and_then(|(p, program)| program.filter(|_| p == pid)),
        }),
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
    pub fn holder(file: &File) -> Option<Option<u32>> {
        let mut l = whole();
        if unsafe { fcntl(file.as_raw_fd(), F_GETLK, &mut l as *mut Flock) } != 0 {
            return None;
        }
        Some(if l.kind == F_UNLCK {
            None
        } else {
            Some(l.pid as u32)
        })
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
    pub fn holder(_: &File) -> Option<Option<u32>> {
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
}
