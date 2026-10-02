//! The C ABI as a binding calls it: pointers in, codes and JSON out.

use fenec_ffi::*;
use std::ffi::{c_char, CStr};
use std::path::PathBuf;

/// A call's code and the text it wrote, freed.
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

fn open(path: &str, flags: u32) -> Result<u64, (i32, String)> {
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
    match taken(code, out) {
        (0, _) => Ok(h),
        failed => Err(failed),
    }
}

fn memory() -> u64 {
    let mut h = 0;
    let code = unsafe { fenec_open_memory(&mut h, std::ptr::null_mut(), std::ptr::null_mut()) };
    assert_eq!(code, FENEC_OK);
    h
}

fn query_with(h: u64, sql: &str, params: &str, vectors: &[u8]) -> (i32, String) {
    let (mut out, mut len) = (std::ptr::null_mut(), 0usize);
    let code = unsafe {
        fenec_query(
            h,
            sql.as_ptr(),
            sql.len(),
            params.as_ptr(),
            params.len(),
            vectors.as_ptr(),
            vectors.len(),
            &mut out,
            &mut len,
        )
    };
    let (code, text) = taken(code, out);
    assert_eq!(len, text.len());
    (code, text)
}

fn query(h: u64, sql: &str, params: &str) -> String {
    let (code, text) = query_with(h, sql, params, &[]);
    assert_eq!(code, FENEC_OK, "{sql}: {text}");
    text
}

fn by_handle(
    f: unsafe extern "C" fn(u64, *mut *mut c_char, *mut usize) -> i32,
    h: u64,
) -> (i32, String) {
    let mut out = std::ptr::null_mut();
    let code = unsafe { f(h, &mut out, std::ptr::null_mut()) };
    taken(code, out)
}

fn changes(h: u64, since: u64) -> String {
    let mut out = std::ptr::null_mut();
    let code = unsafe { fenec_changes(h, since, &mut out, std::ptr::null_mut()) };
    let (code, text) = taken(code, out);
    assert_eq!(code, FENEC_OK, "{text}");
    text
}

fn vector(at: u32, xs: &[f32]) -> Vec<u8> {
    let mut b = [at.to_le_bytes(), (xs.len() as u32).to_le_bytes()].concat();
    xs.iter().for_each(|x| b.extend(x.to_le_bytes()));
    b
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fenec-ffi-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("app.fenec")
}

#[test]
fn a_statement_answers_as_the_browser_module_does() {
    let h = memory();
    let r = query(h, "create collection t (a int, e vector<2> @hnsw(l2))", "");
    assert_eq!(r, r#"{"kind":"ok","message":"collection `t` created"}"#);
    let r = query(h, "put t [{a: 1, e: [1, 0]}, {a: 2, e: [0, 1]}]", "");
    assert_eq!(r, r#"{"kind":"affected","count":2}"#);
    // The vector apart, as f32s, where the JSON holds its null.
    let (code, r) = query_with(
        h,
        "get t select a near e $1 limit 2",
        "[null]",
        &vector(0, &[1.0, 0.0]),
    );
    assert_eq!(code, FENEC_OK, "{r}");
    assert!(
        r.starts_with(r#"{"kind":"rows","result":{"columns":["a"]"#),
        "{r}"
    );
    assert!(
        r.contains(r#"{"a":1,"_score":0},{"a":2,"_score":1.4142135}"#),
        "{r}"
    );
    let r = query(h, "collections", "");
    assert!(r.starts_with(r#"{"kind":"schemas""#), "{r}");
    by_handle(fenec_close, h);
}

#[test]
fn an_error_is_its_code_and_its_json() {
    let h = memory();
    let (code, r) = query_with(h, "get nowhere", "", &[]);
    assert_eq!(code, FENEC_NOT_FOUND, "{r}");
    assert!(r.starts_with(r#"{"kind":"error","message":"#), "{r}");
    let (code, _) = query_with(h, "broken query", "", &[]);
    assert_eq!(code, FENEC_QUERY);
    query(h, "create collection t (a int @unique)", "");
    let (code, _) = query_with(h, "create collection t (a int)", "", &[]);
    assert_eq!(code, FENEC_EXISTS);
    query(h, "put t {a: 1}", "");
    let (code, _) = query_with(h, "put t {a: 1}", "", &[]);
    assert_eq!(code, FENEC_DUPLICATE);
    let (code, _) = query_with(h, "put t {a: \"x\"}", "", &[]);
    assert_eq!(code, FENEC_TYPE);
    let (code, r) = query_with(h, "get t", "[1,", &[]);
    assert_eq!(code, FENEC_QUERY, "{r}");
    let (code, r) = query_with(h, "get t", "[null]", &vector(3, &[1.0]));
    assert_eq!(
        (code, r.contains("malformed vector parameters")),
        (FENEC_QUERY, true)
    );
    // Not UTF-8, and a null with a length.
    let bad = [0xff, 0xfe];
    let mut out = std::ptr::null_mut();
    let code = unsafe {
        fenec_query(
            h,
            bad.as_ptr(),
            2,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            &mut out,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(taken(code, out).0, FENEC_MISUSE);
    let code = unsafe {
        fenec_query(
            h,
            std::ptr::null(),
            4,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            &mut out,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(taken(code, out).0, FENEC_MISUSE);
    // The out pointers may be null: the answer is let go of.
    let code = unsafe {
        let sql = "get t";
        fenec_query(
            h,
            sql.as_ptr(),
            sql.len(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(code, FENEC_OK);
    by_handle(fenec_close, h);
}

#[test]
fn a_closed_handle_is_refused_and_never_reused() {
    let h = memory();
    assert_eq!(by_handle(fenec_close, h).0, FENEC_OK);
    let (code, r) = query_with(h, "collections", "", &[]);
    assert_eq!(code, FENEC_MISUSE, "{r}");
    assert_eq!(by_handle(fenec_close, h).0, FENEC_MISUSE);
    assert_eq!(by_handle(fenec_sync, h).0, FENEC_MISUSE);
    assert_ne!(memory(), h);
    assert_eq!(by_handle(fenec_sync, 0).0, FENEC_MISUSE);
}

#[test]
fn a_json_field_asks_for_its_list_as_json() {
    let h = memory();
    query(h, "create collection t (meta json)", "");
    let (code, r) = query_with(h, "put t {meta: $1}", "[null]", &vector(0, &[0.1, 0.2]));
    assert_eq!(code, FENEC_QUERY);
    assert!(r.ends_with(r#""exact":[0]}"#), "{r}");
    // Sent again as written, its numbers are kept as written.
    query(h, "put t {meta: $1}", "[[0.1, 0.2]]");
    let r = query(h, "get t select meta", "");
    assert!(r.contains(r#"{"meta":[0.1,0.2]}"#), "{r}");
    by_handle(fenec_close, h);
}

#[test]
fn changes_name_the_collections_a_write_landed_in() {
    let h = memory();
    query(
        h,
        "create collection a (n int); create collection b (n int)",
        "",
    );
    let seq = |s: &str| -> u64 {
        let n = &s["{\"seq\":".len()..];
        n[..n.find(',').unwrap()].parse().unwrap()
    };
    let at = seq(&changes(h, u64::MAX));
    query(h, "put a {n: 1}", "");
    assert!(changes(h, at).ends_with(r#""collections":["a"]}"#));
    // A text that fails is put back whole and names nothing.
    let at = seq(&changes(h, u64::MAX));
    let (code, _) = query_with(h, "put a {n: 1}; put b {n: \"x\"}", "", &[]);
    assert_eq!(code, FENEC_TYPE);
    assert!(changes(h, at).ends_with(r#""collections":[]}"#));
    query(h, "drop collection b", "");
    assert!(changes(h, at).ends_with(r#""collections":null}"#));
    by_handle(fenec_close, h);
}

#[test]
fn a_file_is_there_again_after_a_close() {
    let path = scratch("reopen");
    let p = path.to_str().unwrap();
    let h = open(p, 0).unwrap();
    query(
        h,
        "create collection t (title text, e vector<2> @hnsw(cosine))",
        "",
    );
    for i in 0..50 {
        query(
            h,
            "put t {title: $1, e: $2}",
            &format!("[\"n{i}\", [{}, 1]]", i as f32 / 10.0),
        );
    }
    // Open already: here, and so for another process.
    let (code, r) = open(p, 0).unwrap_err();
    assert_eq!(code, FENEC_LOCKED, "{r}");
    assert_eq!(by_handle(fenec_close, h), (FENEC_OK, String::new()));
    let h = open(p, 0).unwrap();
    let r = query(h, "get t count", "");
    assert!(r.contains(r#"{"count":50}"#), "{r}");
    let r = query(h, "get t select title near e [0, 1] limit 1", "");
    assert!(r.contains(r#"{"title":"n0","_score":1}"#), "{r}");
    assert_eq!(by_handle(fenec_checkpoint, h).0, FENEC_OK);
    query(h, "put t {title: \"after\"}", "");
    by_handle(fenec_close, h);
    let h = open(p, 0).unwrap();
    assert!(query(h, "get t count", "").contains(r#"{"count":51}"#));
    by_handle(fenec_close, h);
}

#[test]
fn a_handle_opened_not_to_sync_writes_on_sync_and_flush() {
    let path = scratch("nosync");
    let p = path.to_str().unwrap();
    let h = open(p, FENEC_OPEN_NO_SYNC).unwrap();
    query(h, "create collection t (a int)", "");
    let len = || std::fs::metadata(&path).unwrap().len();
    let before = len();
    query(h, "put t {a: 1}", "");
    // In the buffer still.
    assert_eq!(len(), before);
    assert_eq!(by_handle(fenec_flush, h).0, FENEC_OK);
    assert!(len() > before);
    query(h, "put t {a: 2}", "");
    let before = len();
    assert_eq!(by_handle(fenec_sync, h).0, FENEC_OK);
    assert!(len() > before);
    by_handle(fenec_close, h);
    // Read into memory rather than mapped: the same file.
    let h = open(p, FENEC_OPEN_IN_MEMORY).unwrap();
    assert!(query(h, "get t count", "").contains(r#"{"count":2}"#));
    by_handle(fenec_close, h);
}

#[test]
fn a_handle_is_used_from_several_threads_at_once() {
    let path = scratch("threads");
    let h = open(path.to_str().unwrap(), 0).unwrap();
    query(
        h,
        "create collection t (w int, i int, e vector<4> @hnsw(l2))",
        "",
    );
    let threads: Vec<_> = (0..8)
        .map(|w| {
            std::thread::spawn(move || {
                for i in 0..40 {
                    if w % 2 == 0 {
                        let v = vector(2, &[w as f32, i as f32, 1.0, 0.5]);
                        let (code, r) = query_with(
                            h,
                            "put t {w: $1, i: $2, e: $3}",
                            &format!("[{w}, {i}, null]"),
                            &v,
                        );
                        assert_eq!(code, FENEC_OK, "{r}");
                    } else {
                        let (code, r) = query_with(
                            h,
                            "get t near e $1 limit 3",
                            "[null]",
                            &vector(0, &[0.0, 1.0, 1.0, 0.5]),
                        );
                        assert_eq!(code, FENEC_OK, "{r}");
                    }
                }
            })
        })
        .collect();
    threads.into_iter().for_each(|t| t.join().unwrap());
    assert!(query(h, "get t count", "").contains(r#"{"count":160}"#));
    by_handle(fenec_close, h);
}

#[test]
fn an_index_built_beside_the_database_lands_in_the_file() {
    let path = scratch("maintain");
    let p = path.to_str().unwrap();
    let h = open(p, 0).unwrap();
    query(h, "create collection t (k text, e vector<2>)", "");
    query(
        h,
        "put t [{k: \"a\", e: [1, 0]}, {k: \"b\", e: [0, 1]}]",
        "",
    );
    query(h, "create index on t (e) @hnsw(l2)", "");
    query(h, "create index on t (k) @hash", "");
    query(h, "compact", "");
    by_handle(fenec_close, h);
    let h = open(p, 0).unwrap();
    let r = query(
        h,
        "get t select k where k = \"b\" near e [0, 1] limit 1",
        "",
    );
    assert!(r.contains(r#"{"k":"b","_score":0}"#), "{r}");
    by_handle(fenec_close, h);
}

/// `include/fenec.h` declares every function the library exports, and only
/// those: it is written by hand.
#[test]
fn the_header_declares_what_the_library_exports() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let names = |text: &str, before: &str| -> Vec<String> {
        let mut out: Vec<String> = text
            .match_indices(before)
            .filter_map(|(at, _)| {
                let rest = &text[at + before.len()..];
                let name: String = rest
                    .chars()
                    .skip_while(|c| *c == ' ' || *c == '*')
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                name.starts_with("fenec_").then_some(name)
            })
            .collect();
        out.sort();
        out.dedup();
        out
    };
    let header = std::fs::read_to_string(root.join("include/fenec.h")).unwrap();
    let declared: Vec<String> = header
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim_start().starts_with('*') && !l.starts_with("/*"))
        .flat_map(|l| {
            l.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .filter(|w| w.starts_with("fenec_") && l.contains(&format!("{w}(")))
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let lib = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
    let exported = names(&lib, "extern \"C\" fn");
    assert_eq!(declared, exported);
    // And its codes are the library's.
    for (name, value) in [
        ("FENEC_OK", FENEC_OK),
        ("FENEC_NOT_FOUND", FENEC_NOT_FOUND),
        ("FENEC_QUERY", FENEC_QUERY),
        ("FENEC_IO", FENEC_IO),
        ("FENEC_DENIED", FENEC_DENIED),
        ("FENEC_PANIC", FENEC_PANIC),
        ("FENEC_MISUSE", FENEC_MISUSE),
        ("FENEC_LOCKED", FENEC_LOCKED),
        ("FENEC_OPEN_NO_SYNC", FENEC_OPEN_NO_SYNC as i32),
        ("FENEC_OPEN_IN_MEMORY", FENEC_OPEN_IN_MEMORY as i32),
    ] {
        assert!(
            header.contains(&format!("#define {name} {value}")),
            "{name}"
        );
    }
}

#[test]
fn the_version_is_the_crates() {
    let v = unsafe { CStr::from_ptr(fenec_version()) };
    assert_eq!(v.to_str().unwrap(), env!("CARGO_PKG_VERSION"));
}
