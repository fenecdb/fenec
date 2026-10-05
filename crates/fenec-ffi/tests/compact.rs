//! A file the library compacts on its own, beside the calls: a binary of
//! its own, since the policy it sets for its few megabytes is the
//! process's, and the other tests' files would be compacted under them.

use fenec_core::engine::CompactPolicy;
use fenec_ffi::*;
use std::ffi::{c_char, CStr};
use std::time::Duration;

fn taken(code: i32, out: *mut c_char) -> (i32, String) {
    let text = match out.is_null() {
        true => String::new(),
        false => unsafe {
            let s = CStr::from_ptr(out).to_str().unwrap().to_string();
            fenec_free_string(out);
            s
        },
    };
    (code, text)
}

fn open(path: &str, flags: u32) -> u64 {
    let (mut h, mut out) = (0, std::ptr::null_mut());
    let code = unsafe {
        fenec_open(
            path.as_ptr(),
            path.len(),
            flags,
            &mut h,
            &mut out,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(taken(code, out).0, FENEC_OK);
    h
}

fn query(h: u64, sql: &str) -> String {
    let mut out = std::ptr::null_mut();
    let code = unsafe {
        fenec_query(
            h,
            sql.as_ptr(),
            sql.len(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            &mut out,
            std::ptr::null_mut(),
        )
    };
    let (code, text) = taken(code, out);
    assert_eq!(code, FENEC_OK, "{sql}: {text}");
    text
}

/// A policy that compacts a test's few hundred kilobytes.
fn small() -> CompactPolicy {
    CompactPolicy {
        ratio: 0.5,
        floor: 64 << 10,
    }
}

fn close(h: u64) {
    let mut out = std::ptr::null_mut();
    let code = unsafe { fenec_close(h, &mut out, std::ptr::null_mut()) };
    assert_eq!(taken(code, out).0, FENEC_OK);
}

/// Rows written again and again: the file grows by a version each write,
/// and the library's thread compacts it once half of it is dead -- unless
/// the open said not to.
#[test]
fn a_file_the_app_keeps_updating_stays_near_what_it_holds() {
    set_auto_compact(small(), Duration::from_millis(5));
    let body = "x".repeat(500);
    let mut sizes = Vec::new();
    for flags in [
        FENEC_OPEN_NO_SYNC,
        FENEC_OPEN_NO_SYNC | FENEC_OPEN_NO_AUTO_COMPACT,
    ] {
        let dir =
            std::env::temp_dir().join(format!("fenec-ffi-compact-{}-{flags}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("app.fenec");
        let p = path.to_str().unwrap();
        let h = open(p, flags);
        query(h, "create collection t (n int, body text)");
        for id in 1..=100 {
            query(h, &format!("put t {{id: {id}, n: 0, body: \"{body}\"}}"));
        }
        for round in 1..=60 {
            for id in 1..=100 {
                query(h, &format!("set t {{n: {round}}} where id = {id}"));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        // The thread's last compact: once the writes stop, it compacts
        // the file until the policy no longer asks for one. Waited for, not
        // given 50 ms, which a loaded machine can keep the thread from
        // looking in; bounded only so that a compact that never comes fails.
        if flags & FENEC_OPEN_NO_AUTO_COMPACT == 0 {
            let until = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                let g = garbage(h).unwrap();
                if !small().due(g) {
                    break;
                }
                assert!(std::time::Instant::now() < until, "never compacted: {g:?}");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert!(query(h, "get t where n = 60 count").contains(r#"{"count":100}"#));
        close(h);
        sizes.push(std::fs::metadata(&path).unwrap().len());
        // Opened again, every row is the last version written.
        let h = open(p, FENEC_OPEN_NO_AUTO_COMPACT);
        assert!(query(h, "get t where n = 60 count").contains(r#"{"count":100}"#));
        close(h);
    }
    // 100 rows of half a kilobyte: about 52 KB live, written 61 times.
    assert!(sizes[0] < 300_000, "{sizes:?}");
    assert!(sizes[1] > 3_000_000, "{sizes:?}");
}

/// The library's thread ends with its file: `fenec_close` stops it and
/// joins it before the close takes the lock, so nothing it does outlives
/// the call -- a binding's test that closes, opens the file again at once
/// and removes its directory races no compact. Looked at every 5 ms, a
/// compact due every few rounds, a close lands during one or between two.
#[test]
fn nothing_compacts_a_file_after_its_close() {
    set_auto_compact(small(), Duration::from_millis(5));
    let body = "x".repeat(500);
    let dir = std::env::temp_dir().join(format!("fenec-ffi-compact-close-{}", std::process::id()));
    let listing = || {
        let mut files: Vec<(String, u64)> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    e.metadata().unwrap().len(),
                )
            })
            .collect();
        files.sort();
        files
    };
    for cycle in 0..30 {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("app.fenec");
        let p = path.to_str().unwrap();
        let h = open(p, FENEC_OPEN_NO_SYNC);
        query(h, "create collection t (n int, body text)");
        for id in 1..=100 {
            query(h, &format!("put t {{id: {id}, n: 0, body: \"{body}\"}}"));
        }
        let last = 10 + cycle % 7;
        for round in 1..=last {
            for id in 1..=100 {
                query(h, &format!("set t {{n: {round}}} where id = {id}"));
            }
        }
        close(h);
        let closed = listing();
        assert!(
            !closed.iter().any(|(name, _)| name.contains("beside")),
            "a compact's side file outlived the close: {closed:?}"
        );
        // Several looks' time: a thread still running would have begun a
        // side file or renamed a new file into place.
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(listing(), closed, "the directory changed after the close");
        let h = open(p, FENEC_OPEN_NO_AUTO_COMPACT);
        assert!(query(h, &format!("get t where n = {last} count")).contains(r#"{"count":100}"#));
        close(h);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
