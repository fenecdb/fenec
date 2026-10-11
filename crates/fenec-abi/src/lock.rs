//! `<file>.lock`, held exclusively for as long as a process has a file
//! open to write: two databases over one file corrupt it, whether they are
//! two processes or two opens in one. The native library takes it as an
//! app opens a file, `fenec mcp --file` as an agent's session does, and
//! `fenec-server` as it serves one, so any two of them refuse each other.
//! Not compiled into the browser module, which has no files.

use std::fs::File;
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
pub fn hold(path: &str) -> Result<FileLock, Refused> {
    let name = format!("{path}.lock");
    let mut locked = LOCKED.lock().unwrap_or_else(|e| e.into_inner());
    #[cfg(unix)]
    let key = |m: &std::fs::Metadata| {
        use std::os::unix::fs::MetadataExt;
        (m.dev(), m.ino())
    };
    #[cfg(not(unix))]
    let key = |_: &std::fs::Metadata| (0, 0);
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
    if !record_lock(&lock) {
        return Err(Refused::Held);
    }
    locked.push(key);
    Ok(FileLock {
        file: Some(lock),
        key,
    })
}

/// An exclusive record lock over the whole file, not waited for: whether
/// it was taken. `struct flock` and the commands as each system lays them
/// out -- the crate takes no `libc`.
#[cfg(unix)]
fn record_lock(file: &File) -> bool {
    use std::os::fd::AsRawFd;
    extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    #[cfg(target_vendor = "apple")]
    {
        #[repr(C)]
        struct Flock {
            start: i64,
            len: i64,
            pid: i32,
            kind: i16,
            whence: i16,
        }
        const F_SETLK: i32 = 8;
        const F_WRLCK: i16 = 3;
        let l = Flock {
            start: 0,
            len: 0,
            pid: 0,
            kind: F_WRLCK,
            whence: 0,
        };
        unsafe { fcntl(file.as_raw_fd(), F_SETLK, &l as *const Flock) == 0 }
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // `off_t` is a `long` in the `struct flock` of `F_SETLK`, 32 bits
        // on a 32-bit Android.
        #[repr(C)]
        struct Flock {
            kind: i16,
            whence: i16,
            start: isize,
            len: isize,
            pid: i32,
        }
        const F_SETLK: i32 = 6;
        const F_WRLCK: i16 = 1;
        let l = Flock {
            kind: F_WRLCK,
            whence: 0,
            start: 0,
            len: 0,
            pid: 0,
        };
        unsafe { fcntl(file.as_raw_fd(), F_SETLK, &l as *const Flock) == 0 }
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    {
        extern "C" {
            fn flock(fd: i32, op: i32) -> i32;
        }
        const LOCK_EX: i32 = 2;
        const LOCK_NB: i32 = 4;
        unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) == 0 }
    }
}
