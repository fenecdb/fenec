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
    set_auto_compact(
        CompactPolicy {
            ratio: 0.5,
            floor: 64 << 10,
        },
        Duration::from_millis(5),
    );
    let body = "x".repeat(500);
    let mut sizes = Vec::new();
    for flags in [FENEC_OPEN_NO_SYNC, FENEC_OPEN_NO_SYNC | FENEC_OPEN_NO_AUTO_COMPACT] {
        let dir = std::env::temp_dir().join(format!("fenec-ffi-compact-{}-{flags}", std::process::id()));
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
        // The last look has had its chance.
        std::thread::sleep(Duration::from_millis(50));
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
