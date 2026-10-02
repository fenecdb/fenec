//! What fenecdb's C ABIs answer with: the browser module's (`fenec-wasm`)
//! and the native library's (`fenec-ffi`), from one code, so that a page and
//! an app handed the same text get the same bytes back.
//!
//! A statement comes in as text, its parameters as JSON, and the parameters
//! that are vectors apart from them as `f32`s ([`with_vectors`]); the answer
//! goes out as the JSON `fenec_core::json` writes. What differs between the
//! two is around this, not in it: the module holds a database on its one
//! thread and is handed the time, the library holds one behind a lock and
//! takes the read side for a read ([`read_only`], [`query`]) and the write
//! side for the rest ([`execute`]).
//!
//! The functions the module calls once are `#[inline(always)]`: written in
//! the module's own crate they were one function, and out of it the
//! optimizer at `opt-level = "z"` kept each out of line -- the module grew
//! 762 bytes. Inlined it is 161 bytes larger and 216 smaller in brotli,
//! which is what a page downloads.

use fenec_core::collate;
use fenec_core::json;
use fenec_core::prelude::*;

/// A text and its parameters, read and not yet run.
pub struct Prepared {
    pub stmts: Vec<Statement>,
    pub params: Vec<Value>,
}

/// Why a statement was not answered.
pub enum Refused {
    /// The error, and how many of the text's statements ran before it.
    Error(Error, usize),
    /// A json field is handed a list of numbers that came over as `f32`s:
    /// the places of those parameters, written as the answer lists them,
    /// for the caller to send them again as JSON.
    Exact(String),
}

impl From<Error> for Refused {
    #[inline(always)]
    fn from(e: Error) -> Refused {
        Refused::Error(e, 0)
    }
}

/// Reads the text and its parameters, with no database at hand: a caller
/// behind a lock does it before taking one.
#[inline(always)]
pub fn prepare(sql: &str, params_src: &str, vectors: &[u8]) -> Result<Prepared> {
    let params = json::parse_params(params_src).and_then(|p| with_vectors(p, vectors))?;
    let stmts = fenec_ql::parse(sql)?;
    Ok(Prepared { stmts, params })
}

/// The parameters with each vector handed over as `f32`s put in its place:
/// for each, its place among the parameters and its length as two
/// little-endian `u32`s, then its values, where the JSON holds `null`.
/// Written out as text and read back, a page of 200 768-dim vectors spent
/// most of its time on the numbers' digits, both sides of the call.
pub fn with_vectors(mut params: Vec<Value>, mut bytes: &[u8]) -> Result<Vec<Value>> {
    while let Some((&[a0, a1, a2, a3, n0, n1, n2, n3], rest)) = bytes.split_first_chunk() {
        let at = u32::from_le_bytes([a0, a1, a2, a3]) as usize;
        let n = u32::from_le_bytes([n0, n1, n2, n3]) as usize;
        let body = n.checked_mul(4).and_then(|len| rest.split_at_checked(len));
        let slot = params.get_mut(at).filter(|p| p.is_null());
        let (Some((body, rest)), Some(slot)) = (body, slot) else {
            break;
        };
        *slot = Value::Vector(fenec_core::codec::f32s(body));
        bytes = rest;
    }
    match bytes.is_empty() {
        true => Ok(params),
        false => Err(Error::Query(String::from("malformed vector parameters"))),
    }
}

/// Whether the parameter at `at` was handed over as `f32`s ([`with_vectors`]).
#[inline(always)]
pub fn apart(mut bytes: &[u8], at: usize) -> bool {
    while let Some((&[a0, a1, a2, a3, n0, n1, n2, n3], rest)) = bytes.split_first_chunk() {
        if u32::from_le_bytes([a0, a1, a2, a3]) as usize == at {
            return true;
        }
        let n = u32::from_le_bytes([n0, n1, n2, n3]) as usize;
        bytes = rest.get(n.saturating_mul(4)..).unwrap_or_default();
    }
    false
}

/// A list of numbers a json field is handed, or a path compared with, read
/// again as written: from the text, from the parameters' JSON, and from
/// neither where it came over as `f32`s -- the caller is told which to send
/// as JSON ([`Refused::Exact`]), before anything runs. Asks the schemas
/// alone, so a reader holds the read lock for it.
#[inline(always)]
pub fn exact(
    db: &Database,
    p: &mut Prepared,
    sql: &str,
    params_src: &str,
    vectors: &[u8],
) -> std::result::Result<(), Refused> {
    let vectored = p.stmts.iter().any(|s| s.reads_vectors())
        || p.params.iter().any(fenec_core::query::holds_vector);
    if !vectored {
        return Ok(());
    }
    let (mut text, mut exact) = (false, false);
    let mut as_json = String::new();
    for s in &p.stmts {
        let need = db.exactly(s);
        text |= need.text;
        for i in need.params {
            exact = true;
            if apart(vectors, i) {
                if !as_json.is_empty() {
                    as_json.push(',');
                }
                as_json.push_str(&i.to_string());
            }
        }
    }
    if !as_json.is_empty() {
        return Err(Refused::Exact(as_json));
    }
    if text {
        p.stmts = fenec_ql::parse_exact(sql)?;
    }
    if exact {
        p.params = json::parse_params_exact(params_src).and_then(|p| with_vectors(p, vectors))?;
    }
    Ok(())
}

/// Runs the statements as one block: their writes -- a create, a drop or
/// a create index among them, as a page or an app setting itself up sends
/// -- land together or not at all, and one refused for collation data puts
/// back the ones before it, so the page runs the whole text again. A text
/// with a compact runs a statement at a time, each write on its own a
/// block. The answer is the last statement's.
#[inline(always)]
pub fn execute(db: &mut Database, p: &Prepared) -> std::result::Result<Response, Refused> {
    let block = p.stmts.len() > 1 && p.stmts.iter().all(|s| s.fits_block());
    if block {
        db.begin()?;
    }
    let mut last = Response::Ok(String::from("empty"));
    for (ran, s) in p.stmts.iter().enumerate() {
        match db.execute_with(s, &p.params) {
            Ok(r) => last = r,
            Err(e) if block => {
                db.rollback();
                return Err(Refused::Error(e, 0));
            }
            Err(e) => return Err(Refused::Error(e, ran)),
        }
    }
    if block {
        db.commit()?;
    }
    Ok(last)
}

/// Whether every statement only reads, for a caller that can then take a
/// lock others read beside.
pub fn read_only(p: &Prepared) -> bool {
    p.stmts.iter().all(|s| s.is_read_only())
}

/// [`execute`] over statements that only read ([`read_only`]), through
/// `&Database`: the same answers, under a lock shared with other readers.
pub fn query(db: &Database, p: &Prepared) -> std::result::Result<Response, Refused> {
    let mut last = Response::Ok(String::from("empty"));
    for (ran, s) in p.stmts.iter().enumerate() {
        match db.query(s, &p.params) {
            Ok(r) => last = r,
            Err(e) => return Err(Refused::Error(e, ran)),
        }
    }
    Ok(last)
}

/// The answer as JSON: `{"kind":"rows"|"affected"|"ok"|"schemas"|"error", ...}`.
#[inline(always)]
pub fn answer(r: &std::result::Result<Response, Refused>) -> String {
    match r {
        Ok(r) => json::response_to_string(r),
        // The places to send as JSON, for the caller to send them so.
        Err(Refused::Exact(places)) => format!(
            "{{\"kind\":\"error\",\"message\":\"a json field is handed a list of numbers \
             sent over as f32s\",\"exact\":[{places}]}}"
        ),
        Err(Refused::Error(e, ran)) => refused(e, *ran),
    }
}

/// An error as JSON, and when what refused the statement was collation data
/// the module has not been handed, which (`"chunks"`) and how many of the
/// statements before it ran (`"ran"`, left out when none did): the client
/// runs one again by itself only when nothing before it had. A native
/// build carries every chunk, and never names one.
#[inline(always)]
pub fn refused(e: &Error, ran: usize) -> String {
    let mut out = json::error_to_string(e);
    let missing = collate::take_missing();
    if missing != 0 {
        out.pop();
        out.push_str(",\"chunks\":[");
        for (i, name) in collate::chunk_names(missing).enumerate() {
            if i > 0 {
                out.push(',');
            }
            json::escape_into(&mut out, name);
        }
        out.push(']');
        if ran > 0 {
            out.push_str(&format!(",\"ran\":{ran}"));
        }
        out.push('}');
    }
    out
}

/// What has changed since `since`:
/// `{"seq":N,"horizon":M,"collections":["a","b"]}`.
///
/// When `collections` is **null**, the cursor fell behind the ring, or a
/// collection written since was dropped, and it cannot be known which
/// collection changed: the caller must treat everything as stale.
/// Collection granularity is enough for live queries -- re-running a local
/// query is already sub-millisecond, and incremental bookkeeping does not
/// pay for itself on that budget.
///
/// This is how a caller learns what a statement wrote, rather than the
/// answer of a query carrying it: asked once after a burst of writes
/// rather than built into every answer, it costs a write nothing, and a
/// binding with no live query never asks. A block's writes reach the ring
/// as it lands, so one put back names nothing.
#[inline(always)]
pub fn changes(db: &Database, since: u64) -> String {
    let mut s = format!(
        "{{\"seq\":{},\"horizon\":{},\"collections\":",
        db.change_seq(),
        db.change_horizon()
    );
    match db.changed_collections_since(since) {
        None => s.push_str("null"),
        Some(names) => {
            s.push('[');
            for (i, n) in names.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                json::escape_into(&mut s, n);
            }
            s.push(']');
        }
    }
    s.push('}');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(db: &mut Database, sql: &str, params: &str, vectors: &[u8]) -> String {
        let r = prepare(sql, params, vectors)
            .map_err(Refused::from)
            .and_then(|mut p| {
                exact(db, &mut p, sql, params, vectors)?;
                execute(db, &p)
            });
        answer(&r)
    }

    /// A vector handed over as `f32`s lands where the JSON holds its `null`,
    /// and a list that does not fit the parameters is refused whole.
    #[test]
    fn vectors_beside_the_json() {
        let mut db = Database::new();
        run(&mut db, "create collection t (a int, e vector<2>)", "", &[]);
        let vector = |at: u32, n: u32, xs: &[f32]| {
            let mut b = [at.to_le_bytes(), n.to_le_bytes()].concat();
            xs.iter().for_each(|x| b.extend(x.to_le_bytes()));
            b
        };
        let good = vector(1, 2, &[0.5, 0.25]);
        let r = run(&mut db, "put t {a: $1, e: $2}", "[7, null]", &good);
        assert!(r.contains("\"affected\""), "{r}");
        let r = run(&mut db, "get t select a, e", "", &[]);
        assert!(r.contains(r#"{"a":7,"e":[0.5,0.25]}"#), "{r}");
        for bad in [
            vector(0, 2, &[0.5, 0.25]),
            vector(2, 2, &[0.5, 0.25]),
            vector(1, 3, &[0.5, 0.25]),
            vector(u32::MAX, u32::MAX, &[]),
            [good.clone(), vec![1]].concat(),
            good[..6].to_vec(),
        ] {
            let r = run(&mut db, "put t {a: $1, e: $2}", "[7, null]", &bad);
            assert!(r.contains("malformed vector parameters"), "{r}");
        }
    }

    /// A read answered under the shared lock is the answer `execute` gives.
    #[test]
    fn a_read_is_answered_as_by_execute() {
        let mut db = Database::new();
        run(
            &mut db,
            "create collection t (a int, e vector<2> @hnsw(l2)); put t [{a: 1, e: [1, 0]}, {a: 2, e: [0, 1]}]",
            "",
            &[],
        );
        let sql = "get t near e $1 limit 2; get t select a order a desc";
        let p = prepare(sql, "[[1, 0]]", &[]).unwrap();
        assert!(read_only(&p));
        let shared = answer(&query(&db, &p));
        assert_eq!(shared, run(&mut db, sql, "[[1, 0]]", &[]));
        assert!(shared.contains(r#"[{"a":2},{"a":1}]"#), "{shared}");
    }

    #[test]
    fn changes_name_what_landed() {
        let mut db = Database::new();
        run(&mut db, "create collection t (a int)", "", &[]);
        assert!(changes(&db, 0).contains("\"collections\":[\"t\"]"));
        let seq = db.change_seq();
        // A text whose last statement fails is put back whole: nothing to name.
        run(&mut db, "create collection u (a int)", "", &[]);
        let r = run(&mut db, "put t {a: 1}; put u {a: \"x\"}", "", &[]);
        assert!(r.contains("error"), "{r}");
        assert!(changes(&db, seq + 1).contains("\"collections\":[]"));
    }
}
#[cfg(feature = "sync")]
pub mod sync;
