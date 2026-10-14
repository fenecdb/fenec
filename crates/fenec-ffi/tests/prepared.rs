//! A write of many documents through the library has them built and
//! encoded under the read lock, beside the app's reads
//! (`Database::prepare`), and writes what the same text run under the write
//! lock alone writes (`fenec_abi::execute`, as it ran before): every
//! answer, every byte of the file -- documents written out in the text, a
//! parameter's list of them, json fields whose numbers are read again as
//! written, vectors handed over as `f32`s, a text of several statements as
//! one block, upserts, `returning`, a held id and a `@unique` value put
//! back. Writers at once, whose writes land between another's preparation
//! and its write lock, each land whole with every number as written.

use fenec_ffi::*;
use std::ffi::{c_char, CStr};
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fenec-ffi-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("app.fenec")
}

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

fn open(path: &str) -> u64 {
    let (mut h, mut out) = (0, std::ptr::null_mut());
    let flags = FENEC_OPEN_NO_SYNC | FENEC_OPEN_NO_AUTO_COMPACT;
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

fn query(h: u64, sql: &str, params: &str, vectors: &[u8]) -> (i32, String) {
    let mut out = std::ptr::null_mut();
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
            std::ptr::null_mut(),
        )
    };
    taken(code, out)
}

fn close(h: u64) {
    let mut out = std::ptr::null_mut();
    let code = unsafe { fenec_close(h, &mut out, std::ptr::null_mut()) };
    assert_eq!(taken(code, out).0, FENEC_OK);
}

fn vector(at: u32, xs: &[f32]) -> Vec<u8> {
    let mut b = [at.to_le_bytes(), (xs.len() as u32).to_le_bytes()].concat();
    xs.iter().for_each(|x| b.extend(x.to_le_bytes()));
    b
}

/// The text as it ran before: read, its lists read again as written, run
/// under the write lock alone.
fn under_the_lock(
    db: &mut fenec_core::prelude::Database,
    sql: &str,
    params: &str,
    vectors: &[u8],
) -> String {
    let r = fenec_abi::prepare(sql, params, vectors)
        .map_err(fenec_abi::Refused::from)
        .and_then(|mut p| {
            fenec_abi::exact(db, &mut p, sql, params, vectors)?;
            fenec_abi::execute(db, &p)
        });
    fenec_abi::answer(&r)
}

fn doc(i: usize) -> String {
    format!(
        r#"{{title: "t{i}", kind: "{}", n: {i}, code: "c{i}", meta: {{lang: "tr", s: [0.1, 0.30000000000000004, {i}.5]}}, v: [{}.25, 0.5, -0.125, 1]}}"#,
        ["a", "b", "c"][i % 3],
        i % 7
    )
}

fn object(i: usize) -> String {
    format!(
        r#"{{"title":"p{i}","n":{i},"code":"p{i}","meta":{{"s":[0.30000000000000004,{i}.1]}}}}"#
    )
}

/// Each text of the workload, its parameters and its vectors apart.
fn workload() -> Vec<(String, String, Vec<u8>)> {
    let docs = |from: usize, n: usize| (from..from + n).map(doc).collect::<Vec<_>>().join(", ");
    let mut out = vec![(
        format!("put t [{}]", docs(0, 40)),
        String::new(),
        Vec::new(),
    )];
    // A parameter's documents, whose json numbers are read as written.
    let list: Vec<String> = (100..130).map(object).collect();
    out.push((
        "put t $1".into(),
        format!("[[{}]]", list.join(",")),
        Vec::new(),
    ));
    // Vectors handed over as `f32`s.
    let (mut text, mut bytes) = (String::from("put t ["), Vec::new());
    for i in 0..20 {
        if i > 0 {
            text.push_str(", ");
        }
        text.push_str(&format!("{{n: {}, v: ${}}}", 200 + i, i + 1));
        bytes.extend(vector(i as u32, &[i as f32 * 0.1, 0.2, 0.3, 0.4]));
    }
    text.push(']');
    out.push((text, format!("[{}]", vec!["null"; 20].join(",")), bytes));
    // Several statements, one block.
    let several: Vec<String> = (300..320).map(|i| format!("put t {}", doc(i))).collect();
    out.push((several.join("; "), String::new(), Vec::new()));
    // Upserts, and `returning`.
    let ups: Vec<String> = (0..20)
        .map(|i| format!(r#"{{code: "C{}", n: {i}}}"#, i % 7))
        .collect();
    out.push((
        format!(
            "put t [{}] if absent else set {{n: n + new.n}}",
            ups.join(", ")
        ),
        String::new(),
        Vec::new(),
    ));
    out.push((
        format!("insert t [{}] returning id, title, meta", docs(400, 20)),
        String::new(),
        Vec::new(),
    ));
    // A held id after 29 documents; a `@unique` value twice, as `nocase`
    // folds it: each put back whole.
    out.push((
        format!(
            "put t [{}, {{id: 1, title: \"over\"}}]; insert t {{id: 1}}",
            docs(500, 29)
        ),
        String::new(),
        Vec::new(),
    ));
    out.push((
        format!(
            "put t [{}, {{code: \"dup\"}}, {{code: \"DUP\"}}]",
            docs(600, 20)
        ),
        String::new(),
        Vec::new(),
    ));
    // A json field handed a vector as `f32`s: asked for again as JSON.
    let list: Vec<String> = (0..20)
        .map(|i| format!("{{n: {}, meta: ${}}}", 700 + i, i + 1))
        .collect();
    let bytes: Vec<u8> = (0..20).flat_map(|i| vector(i, &[0.1, 0.2])).collect();
    out.push((
        format!("put t [{}]", list.join(", ")),
        format!("[{}]", vec!["null"; 20].join(",")),
        bytes,
    ));
    // A compact among them: each statement on its own.
    out.push((
        format!(
            "put t [{}]; compact t; put t [{}]",
            docs(800, 20),
            docs(900, 20)
        ),
        String::new(),
        Vec::new(),
    ));
    out
}

const SCHEMA: &str = "create collection t (title text, kind text @hash, n int @sorted, \
                      code text nocase @unique, meta json, v vector<4>)";

#[test]
fn a_write_of_many_documents_writes_what_one_built_under_the_lock_writes() {
    let (a_path, b_path) = (scratch("prepared-a"), scratch("prepared-b"));
    let mut a = fenec_core::fs::open(&a_path).unwrap();
    let b = open(b_path.to_str().unwrap());
    assert_eq!(
        under_the_lock(&mut a, SCHEMA, "", &[]),
        query(b, SCHEMA, "", &[]).1
    );
    let mut codes = Vec::new();
    for (sql, params, vectors) in workload() {
        let x = under_the_lock(&mut a, &sql, &params, &vectors);
        let (code, y) = query(b, &sql, &params, &vectors);
        assert_eq!(x, y, "{sql}");
        codes.push(code);
    }
    assert_eq!(
        codes,
        [
            0,
            0,
            0,
            0,
            0,
            0,
            FENEC_DUPLICATE,
            FENEC_DUPLICATE,
            FENEC_QUERY,
            0
        ]
    );
    for q in [
        "get t order id",
        "get t where meta.s has 0.30000000000000004 count",
    ] {
        assert_eq!(
            under_the_lock(&mut a, q, "", &[]),
            query(b, q, "", &[]).1,
            "{q}"
        );
    }
    a.sync().unwrap();
    drop(a);
    close(b);
    let (x, y) = (
        std::fs::read(&a_path).unwrap(),
        std::fs::read(&b_path).unwrap(),
    );
    assert!(x == y, "{} and {} bytes differ", x.len(), y.len());
}

/// Four writers at once, each a text of 20 documents at a time whose json
/// numbers are read as written: another's write lands between one's
/// preparation and its write lock, which reads them again then. Every
/// text lands whole, every number as it was written.
#[test]
fn writers_at_once_each_land_whole_with_their_numbers_as_written() {
    let path = scratch("prepared-writers");
    let h = open(path.to_str().unwrap());
    assert_eq!(
        query(h, "create collection w (k int, at int, meta json)", "", &[]).0,
        FENEC_OK
    );
    std::thread::scope(|s| {
        for k in 0..4 {
            s.spawn(move || {
                for at in 0..25 {
                    let list: Vec<String> = (0..20)
                        .map(|i| {
                            format!(
                                r#"{{"k":{k},"at":{at},"meta":{{"s":[0.30000000000000004,{i}.1]}}}}"#
                            )
                        })
                        .collect();
                    let params = format!("[[{}]]", list.join(","));
                    let (code, text) = query(h, "put w $1", &params, &[]);
                    assert_eq!(code, FENEC_OK, "{text}");
                }
            });
        }
        s.spawn(move || {
            for _ in 0..200 {
                let (code, text) = query(h, "get w select k, at, count(*) group k, at", "", &[]);
                assert_eq!(code, FENEC_OK, "{text}");
                // A text seen in part is a group of fewer than its 20.
                for group in text.split(r#""count":"#).skip(1) {
                    let n: String = group.chars().take_while(char::is_ascii_digit).collect();
                    assert_eq!(n, "20", "{text}");
                }
            }
        });
    });
    let (_, all) = query(h, "get w count", "", &[]);
    assert!(all.contains(r#"{"count":2000}"#), "{all}");
    let (_, exact) = query(
        h,
        "get w where meta.s has 0.30000000000000004 count",
        "",
        &[],
    );
    assert!(exact.contains(r#"{"count":2000}"#), "{exact}");
    close(h);
}
