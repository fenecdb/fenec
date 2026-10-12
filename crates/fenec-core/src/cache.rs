//! What memory the system has for a file's pages: the page cache a mapped
//! database reads its documents through ([`room`]).
//!
//! fenecdb keeps no cache of its own: a document is read where it lies in
//! the mapped file, and the kernel decides which of the file's pages stay
//! in memory. Every page holding a live record is one the reads need, and
//! an update leaves the version it replaced dead where it was -- with
//! uniform updates a page of the file stays needed until every record on
//! it has been written again, so the pages the reads need are about the
//! whole file until a compact. Once the file outgrows what the system can
//! hold of it, reads and updates wait for the disk ([`crate::engine`]'s
//! compact policy asks this for when a file is about to).
//!
//! The answer is an estimate of what the page cache could give the file --
//! its own pages already cached included, since the kernel would keep those
//! as readily as any other -- and nothing is held from one call to the
//! next: other processes take and give back memory all the while.

/// The bytes of memory the page cache could hold of a file now: what the
/// machine has, less what the processes hold and the kernel keeps for
/// itself -- on Linux the smaller of the system's `MemAvailable` and what
/// the process's cgroup leaves (a container's `--memory`), on macOS and
/// iOS the physical memory less the apps' memory, the wired pages and the
/// compressor's. `None` where the system does not say.
pub fn room() -> Option<u64> {
    if let Some(assumed) = ASSUMED.with(|a| a.get()) {
        return assumed;
    }
    imp::room()
}

thread_local! {
    static ASSUMED: std::cell::Cell<Option<Option<u64>>> = const { std::cell::Cell::new(None) };
}

/// The major faults a second -- a page the process waited for the disk to
/// read -- past which [`faulting`] says it is waiting for the disk: at
/// 500 a second of 100 to 200 us each, a tenth of a thread's time. A file
/// that fits the cache takes none once its pages are in; one past it took
/// 2 000 to 3 000 a second in a container held to 2 GB (Linux reads 128 KB
/// around each) and 5 000 to 14 000 on this Mac, where macOS's own answer
/// to [`room`] misleads: under the pressure it compresses the apps' memory
/// and the room it reports grows, 2.4 to 2.9 GB while a 1.5 to 2.4 GB file
/// was read from the disk at 5 000 to 7 000 faults a second.
pub const FAULTING_AT: u64 = 500;

/// Whether the process has been waiting for the disk: its major faults a
/// second over the last second or more ([`FAULTING_AT`]), from
/// `getrusage`. Taken over the process, not a file -- the files a process
/// maps share the cache -- and measured over the time since the last
/// measure at least a second old, whoever asked: every caller sees the
/// same answer for the same second. `false` the first time it is asked,
/// and where the system does not count them.
pub fn faulting() -> bool {
    use std::time::Duration;
    let Some(faults) = major_faults() else {
        return false;
    };
    let now = std::time::Instant::now();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    match *last {
        Some((at, _, said)) if now - at < Duration::from_secs(1) => said,
        Some((at, before, _)) => {
            let rate = faults.saturating_sub(before) as f64 / (now - at).as_secs_f64();
            let said = rate >= FAULTING_AT as f64;
            *last = Some((now, faults, said));
            said
        }
        None => {
            *last = Some((now, faults, false));
            false
        }
    }
}

/// When [`faulting`] last measured, the faults then, and what it said.
static LAST: std::sync::Mutex<Option<(std::time::Instant, u64, bool)>> =
    std::sync::Mutex::new(None);

/// Starts the time [`faulting`] measures over afresh, and has it say
/// `false` until a second has passed: what a compact calls once its new
/// file is in place and read in. Its own faults -- the old file's records
/// read, the new file's pages read in after the swap -- say nothing of a
/// file outgrowing the cache, and counted they made the next compact due:
/// on this Mac a compact read its new file in at 30 000 to 56 000 faults
/// a second, the next look found the process faulting, and a 1.6 GB file
/// that took none was compacted every 6 s.
pub fn forget_faults() {
    if let Some(faults) = major_faults() {
        *LAST.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((std::time::Instant::now(), faults, false));
    }
}

/// The process's major faults so far (`getrusage`'s `ru_majflt`): its
/// `rusage` is two `timeval`s, two longs each, and fourteen longs, on Linux
/// and macOS alike.
#[cfg(unix)]
fn major_faults() -> Option<u64> {
    use std::ffi::c_long;
    #[repr(C)]
    struct Rusage {
        times: [c_long; 4],
        counts: [c_long; 14],
    }
    extern "C" {
        fn getrusage(who: i32, usage: *mut Rusage) -> i32;
    }
    const RUSAGE_SELF: i32 = 0;
    let mut r = Rusage {
        times: [0; 4],
        counts: [0; 14],
    };
    (unsafe { getrusage(RUSAGE_SELF, &mut r) } == 0).then_some(r.counts[5] as u64)
}

#[cfg(not(unix))]
fn major_faults() -> Option<u64> {
    None
}

/// Has [`room`] answer `room` on this thread, `None` for the system's
/// answer again: how a test has a compact take the way a machine short of
/// memory has it take, whatever the machine running it holds.
#[doc(hidden)]
pub fn assume(room: Option<Option<u64>>) {
    ASSUMED.with(|a| a.set(room));
}

#[cfg(target_vendor = "apple")]
mod imp {
    /// `vm_statistics64`, `HOST_VM_INFO64`'s answer: counts of pages.
    #[repr(C, align(8))]
    #[derive(Default)]
    struct VmStatistics64 {
        free_count: u32,
        active_count: u32,
        inactive_count: u32,
        wire_count: u32,
        zero_fill_count: u64,
        reactivations: u64,
        pageins: u64,
        pageouts: u64,
        faults: u64,
        cow_faults: u64,
        lookups: u64,
        hits: u64,
        purges: u64,
        purgeable_count: u32,
        speculative_count: u32,
        decompressions: u64,
        compressions: u64,
        swapins: u64,
        swapouts: u64,
        compressor_page_count: u32,
        throttled_count: u32,
        external_page_count: u32,
        internal_page_count: u32,
        total_uncompressed_pages_in_compressor: u64,
    }

    const HOST_VM_INFO64: i32 = 4;

    extern "C" {
        fn mach_host_self() -> u32;
        fn host_statistics64(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
        fn host_page_size(host: u32, size: *mut usize) -> i32;
        fn sysctlbyname(
            name: *const u8,
            old: *mut u8,
            old_len: *mut usize,
            new: *const u8,
            new_len: usize,
        ) -> i32;
    }

    /// The host port, taken once: each `mach_host_self` adds a reference
    /// to the process's right that nothing gives back.
    fn host() -> u32 {
        static HOST: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
        *HOST.get_or_init(|| unsafe { mach_host_self() })
    }

    pub fn room() -> Option<u64> {
        let mut memsize = 0u64;
        let mut len = std::mem::size_of::<u64>();
        let r = unsafe {
            sysctlbyname(
                c"hw.memsize".as_ptr().cast(),
                (&mut memsize as *mut u64).cast(),
                &mut len,
                std::ptr::null(),
                0,
            )
        };
        if r != 0 || memsize == 0 {
            return None;
        }
        let mut page = 0usize;
        if unsafe { host_page_size(host(), &mut page) } != 0 || page == 0 {
            return None;
        }
        let mut s = VmStatistics64::default();
        let mut count = (std::mem::size_of::<VmStatistics64>() / 4) as u32;
        let r = unsafe {
            host_statistics64(
                host(),
                HOST_VM_INFO64,
                (&mut s as *mut VmStatistics64).cast(),
                &mut count,
            )
        };
        if r != 0 {
            return None;
        }
        // What Activity Monitor calls the apps' memory, the wired memory
        // and the compressed: what the file's pages cannot take from.
        let apps = s.internal_page_count.saturating_sub(s.purgeable_count) as u64;
        let held = (apps + s.wire_count as u64 + s.compressor_page_count as u64) * page as u64;
        Some(memsize.saturating_sub(held))
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use std::path::Path;

    pub fn room() -> Option<u64> {
        let system = mem_available();
        let group = cgroup_room();
        match (system, group) {
            (Some(s), Some(g)) => Some(s.min(g)),
            (s, g) => s.or(g),
        }
    }

    /// `MemAvailable` from `/proc/meminfo`: the free memory and what the
    /// kernel can take back of its caches, ours among them. Inside a
    /// container this is the machine's.
    fn mem_available() -> Option<u64> {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        let line = text.lines().find(|l| l.starts_with("MemAvailable:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    }

    /// What the process's cgroup leaves the page cache: its limit less
    /// what it holds that is not file pages, the least of it from the
    /// process's group up to the root -- the page cache is charged to the
    /// group, so a container's `--memory` bounds it. cgroup v2, then v1's
    /// memory controller; `None` with no limit anywhere.
    fn cgroup_room() -> Option<u64> {
        let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        if let Some(path) = own.lines().find_map(|l| l.strip_prefix("0::")) {
            let mut dir = Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
            let mut least: Option<u64> = None;
            loop {
                if let Some(r) = v2_room(&dir) {
                    least = Some(least.map_or(r, |l| l.min(r)));
                }
                if dir == Path::new("/sys/fs/cgroup") || !dir.pop() {
                    break;
                }
            }
            return least;
        }
        let path = own.lines().find_map(|l| {
            let mut parts = l.splitn(3, ':');
            let (_, controllers, path) = (parts.next()?, parts.next()?, parts.next()?);
            controllers
                .split(',')
                .any(|c| c == "memory")
                .then_some(path)
        })?;
        let mut dir = Path::new("/sys/fs/cgroup/memory").join(path.trim_start_matches('/'));
        // Inside a container the group is the mount's root, whatever the
        // path the host gave it.
        if !dir.join("memory.limit_in_bytes").exists() {
            dir = Path::new("/sys/fs/cgroup/memory").to_path_buf();
        }
        v1_room(&dir)
    }

    fn read_u64(path: &Path) -> Option<u64> {
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    fn stat(path: &Path, keys: &[&str]) -> Option<u64> {
        let text = std::fs::read_to_string(path).ok()?;
        let mut sum = None;
        for l in text.lines() {
            let mut it = l.split_whitespace();
            if let (Some(k), Some(v)) = (it.next(), it.next()) {
                if keys.contains(&k) {
                    sum = Some(sum.unwrap_or(0) + v.parse::<u64>().ok()?);
                }
            }
        }
        sum
    }

    fn v2_room(dir: &Path) -> Option<u64> {
        // "max" for none, which does not parse.
        let max = read_u64(&dir.join("memory.max"))?;
        let current = read_u64(&dir.join("memory.current"))?;
        let file = stat(&dir.join("memory.stat"), &["file"]).unwrap_or(0);
        Some(max.saturating_sub(current.saturating_sub(file)))
    }

    fn v1_room(dir: &Path) -> Option<u64> {
        let limit = read_u64(&dir.join("memory.limit_in_bytes"))?;
        // No limit is the largest page-aligned i64.
        if limit >= 1 << 62 {
            return None;
        }
        let usage = read_u64(&dir.join("memory.usage_in_bytes"))?;
        let file = stat(
            &dir.join("memory.stat"),
            &["total_inactive_file", "total_active_file"],
        )
        .unwrap_or(0);
        Some(limit.saturating_sub(usage.saturating_sub(file)))
    }
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
mod imp {
    pub fn room() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    /// Where the system says, it says something between nothing and the
    /// machine.
    #[test]
    fn the_room_is_some_of_the_machine() {
        #[cfg(any(target_vendor = "apple", target_os = "linux"))]
        {
            let r = super::room().expect("the system says");
            assert!(r > 0 && r < 1 << 50, "{r}");
        }
    }
}
