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
    /// The error, how many of the text's statements ran before it and
    /// stayed, and -- for a text of several -- which one it stopped at,
    /// from 0, as a `/batch` answers `at`.
    Error(Error, usize, Option<usize>),
    /// A json field is handed a list of numbers that came over as `f32`s:
    /// the places of those parameters, written as the answer lists them,
    /// for the caller to send them again as JSON.
    Exact(String),
}

impl From<Error> for Refused {
    #[inline(always)]
    fn from(e: Error) -> Refused {
        Refused::Error(e, 0, None)
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
/// block. The answer is the last statement's; a refusal of a text of
/// several names the statement that stopped it, as a `/batch`'s does: a
/// ledger's debit and credit are both `set accounts`, and the error alone
/// could not tell them apart -- the page ran the text's prefixes again,
/// each ended by a statement that always fails, to find which.
#[inline(always)]
pub fn execute(db: &mut Database, p: &Prepared) -> std::result::Result<Response, Refused> {
    let block = p.stmts.len() > 1 && p.stmts.iter().all(|s| s.fits_block());
    if block {
        db.begin()?;
    }
    let several = p.stmts.len() > 1;
    let mut last = Response::Ok(String::from("empty"));
    for (ran, s) in p.stmts.iter().enumerate() {
        let at = several.then_some(ran);
        match db.execute_with(s, &p.params) {
            Ok(r) => last = r,
            Err(e) if block => {
                db.rollback();
                return Err(Refused::Error(e, 0, at));
            }
            Err(e) => return Err(Refused::Error(e, ran, at)),
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
    let several = p.stmts.len() > 1;
    let mut last = Response::Ok(String::from("empty"));
    for (ran, s) in p.stmts.iter().enumerate() {
        match db.query(s, &p.params) {
            Ok(r) => last = r,
            Err(e) => return Err(Refused::Error(e, ran, several.then_some(ran))),
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
        Err(Refused::Error(e, ran, at)) => refused(e, *ran, *at),
    }
}

/// An error as JSON, and when what refused the statement was collation data
/// the module has not been handed, which (`"chunks"`) and how many of the
/// statements before it ran (`"ran"`, left out when none did): the client
/// runs one again by itself only when nothing before it had. A native
/// build carries every chunk, and never names one. `at` is the statement
/// of a text of several that stopped it (`"at"`, from 0).
#[inline(always)]
pub fn refused(e: &Error, ran: usize, at: Option<usize>) -> String {
    let mut out = json::error_to_string(e);
    if let Some(at) = at {
        out.pop();
        out.push_str(",\"at\":");
        out.push_str(&at.to_string());
        out.push('}');
    }
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

// ------------------------------------------------------------------ schema

/// What [`schema`] found and did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    pub plan: declared::Plan,
    /// The migrations recorded (or, planned, to be), by number from 1.
    pub migrations: Vec<usize>,
    /// Whether those migrations' statements ran (or would run): a database
    /// made from the description holds what they lead to already, and
    /// records them without running them.
    pub ran: bool,
    /// Whether anything was written.
    pub applied: bool,
}

impl Outcome {
    /// `{"kind":"schema","applied":..,"ran":..,"migrations":[..],
    /// "statements":[..],"refusals":[..]}`
    pub fn json(&self) -> String {
        let yes = |b: bool| if b { "true" } else { "false" };
        let mut out = String::from("{\"kind\":\"schema\",\"applied\":");
        out.push_str(yes(self.applied));
        out.push_str(",\"ran\":");
        out.push_str(yes(self.ran));
        out.push_str(",\"migrations\":[");
        for (i, n) in self.migrations.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&n.to_string());
        }
        out.push_str("],");
        self.plan.json_into(&mut out);
        out.push('}');
        out
    }
}

use fenec_core::declared;

/// A description read, its collections out of their FenecQL text where
/// it holds them so (`"fenecql"`).
pub fn read(request: &str) -> Result<declared::Declared> {
    let mut d = declared::parse(request)?;
    if let Some(text) = d.text.take() {
        d.collections = fenec_ql::schema_text(&text)?;
    }
    Ok(d)
}

/// Every collection the database holds, its own among them: borrowed,
/// where cloned they were drop glue and a clone of the browser module's own.
fn schemas(db: &Database) -> Vec<&Schema> {
    let mut out = Vec::new();
    for n in &db.collection_names() {
        if let Ok(c) = db.collection(n) {
            out.push(&c.schema);
        }
    }
    out
}

/// `(n, text)` of each migration `_migrations` records, ascending.
/// Read through the write path's `execute_with`, the one a schema change
/// takes anyway: through `query`, the browser module grew 0.8 KB for it.
fn recorded(db: &mut Database) -> Result<Vec<(i64, String)>> {
    if db.collection(declared::MIGRATIONS).is_err() {
        return Ok(Vec::new());
    }
    let stmt = fenec_ql::parse_one("get _migrations select n, text order n")?;
    let mut out = Vec::new();
    if let Response::Rows(rs) = db.execute_with(&stmt, &[])? {
        for r in rs.rows {
            if let [Value::Int(n), Value::Text(t)] = r.values.as_slice() {
                out.push((*n, t.clone()));
            }
        }
    }
    Ok(out)
}

/// Runs FenecQL inside the block [`schema`] holds open: a compact, which
/// cannot be put back, is refused.
fn exec(db: &mut Database, text: &str, params: &[Value]) -> Result<()> {
    for s in fenec_ql::parse(text)? {
        if !s.fits_block() {
            return Err(Error::Query(
                "a migration cannot compact: a compact is not put back with the rest".into(),
            ));
        }
        db.execute_with(&s, params)?;
    }
    Ok(())
}

/// A migration's error, saying which migration it was: a query error
/// whatever it was, the migration's being the statement that failed.
fn in_migration(n: usize, e: Error) -> Error {
    Error::Query(format!("migration {n}: {e}"))
}

/// The database compared with a schema declared in code -- a description
/// (`fenec_core::declared`), with its migrations -- and brought to it as
/// `mode` says. Under [`true`] the migrations not yet recorded
/// run in order, each recorded in `_migrations` with `now`, and then what
/// only adds is applied, all of it one block; anything refused puts the
/// block back, so a database is either brought to the code or left as it
/// was. A database holding none of its own collections yet is made from
/// the description, and its migrations recorded without running: they lead
/// from schemas it never had. Not `apply`, it says what an apply would do
/// and writes nothing: the migrations still to run run in a block put back,
/// so the plan is the one they lead to.
///
/// A database another owns -- a server a replica follows -- is compared by
/// [`follow`], apart: the browser module, which applies, carries none of it.
pub fn schema(db: &mut Database, request: &str, apply: bool, now: Option<i64>) -> Result<Outcome> {
    let d = read(request)?;
    let texts = declared::migration_texts(&d)?;
    let done = declared::applied(&recorded(db)?, &texts)?;
    let pending = done..texts.len();
    let fresh = done == 0 && db.collection_names().iter().all(|n| declared::own(n));
    if pending.is_empty() {
        let plan = declared::plan(&schemas(db), &d.collections, declared::Mode::Apply);
        if !apply || !plan.refusals.is_empty() || plan.statements.is_empty() {
            return Ok(Outcome {
                plan,
                ..Outcome::default()
            });
        }
    }
    db.begin()?;
    let mut run = || -> Result<declared::Plan> {
        if !pending.is_empty() {
            exec(db, declared::MIGRATIONS_DDL, &[])?;
        }
        for i in pending.clone() {
            if !fresh {
                exec(db, &texts[i], &[]).map_err(|e| in_migration(i + 1, e))?;
            }
            let at = now.map_or(Value::Null, Value::Timestamp);
            let row = [Value::Int(i as i64 + 1), Value::Text(texts[i].clone()), at];
            exec(db, "put _migrations {n: $1, text: $2, at: $3}", &row)?;
        }
        let plan = declared::plan(&schemas(db), &d.collections, declared::Mode::Apply);
        if apply && plan.refusals.is_empty() {
            for s in &plan.statements {
                exec(db, s, &[])?;
            }
        }
        Ok(plan)
    };
    let plan = match run() {
        Ok(plan) => plan,
        Err(e) => {
            db.rollback();
            return Err(e);
        }
    };
    let applied = apply && plan.refusals.is_empty();
    match applied {
        true => db.commit()?,
        false => db.rollback(),
    }
    let mut migrations = Vec::new();
    for i in pending {
        migrations.push(i + 1);
    }
    Ok(Outcome {
        plan,
        migrations,
        ran: !fresh,
        applied,
    })
}

/// The database compared with a description as another's: everything the
/// code declares must be there as declared, what is there beside it is its
/// owner's, nothing runs -- and the migrations are not read, since only the
/// owner runs them. Through `&Database`: a reader beside the others.
pub fn follow(db: &Database, request: &str) -> Result<Outcome> {
    let d = read(request)?;
    Ok(Outcome {
        plan: declared::plan(&schemas(db), &d.collections, declared::Mode::Follow),
        ..Outcome::default()
    })
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

    fn described(collections: &str, migrations: &str) -> String {
        format!(r#"{{"format":1,"collections":{collections},"migrations":{migrations}}}"#)
    }

    const TODOS: &str = r#"[{"name":"todos","fields":[{"name":"title","type":"text","required":true},{"name":"done","type":"bool","index":{"kind":"hash"}}]}]"#;

    /// A database made from the description; opened again, nothing to do.
    #[test]
    fn a_description_makes_a_database_and_then_nothing() {
        let mut db = Database::new();
        let req = described(
            TODOS,
            r#"["alter collection todos rename field name to title"]"#,
        );
        let o = schema(&mut db, &req, true, Some(5)).unwrap();
        assert!(o.applied && !o.ran, "{o:?}");
        assert_eq!(o.migrations, [1]);
        assert_eq!(
            o.plan.statements,
            ["create collection todos (title text required, done bool @hash)"]
        );
        // Recorded, not run: the field it renames never was.
        assert_eq!(
            recorded(&mut db).unwrap(),
            [(
                1,
                "alter collection todos rename field name to title".into()
            )]
        );
        let seq = db.change_seq();
        let o = schema(&mut db, &req, true, Some(6)).unwrap();
        assert_eq!(o, Outcome::default());
        assert_eq!(db.change_seq(), seq);
    }

    /// A field added in the code is added; a field dropped is refused,
    /// with nothing written, until a migration says what it means.
    #[test]
    fn additions_apply_and_a_drop_waits_for_its_migration() {
        let mut db = Database::new();
        schema(&mut db, &described(TODOS, "[]"), true, None).unwrap();
        run(&mut db, "put todos {title: \"a\", done: false}", "", &[]);
        let more = r#"[{"name":"todos","fields":[{"name":"name","type":"text","required":true},{"name":"done","type":"bool","index":{"kind":"hash"}},{"name":"at","type":"timestamp","index":{"kind":"sorted"}}]}]"#;
        let seq = db.change_seq();
        let o = schema(&mut db, &described(more, "[]"), true, None).unwrap();
        assert!(!o.applied);
        let kinds: Vec<_> = o.plan.refusals.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, ["required_added", "field_not_declared"]);
        assert_eq!(db.change_seq(), seq);
        // The rename, as a migration: run once, recorded, and the rest applied.
        let req = described(
            more,
            r#"["alter collection todos rename field title to name"]"#,
        );
        let plan = schema(&mut db, &req, false, None).unwrap();
        assert!(
            !plan.applied && plan.ran && plan.plan.refusals.is_empty(),
            "{plan:?}"
        );
        assert_eq!(
            plan.plan.statements,
            ["alter collection todos add field at timestamp @sorted"]
        );
        assert_eq!(db.change_seq(), seq, "a plan writes nothing");
        let o = schema(&mut db, &req, true, Some(7)).unwrap();
        assert!(o.applied && o.ran, "{o:?}");
        let r = run(&mut db, "get todos select name, at", "", &[]);
        assert!(r.contains(r#"{"name":"a","at":null}"#), "{r}");
        assert_eq!(
            schema(&mut db, &req, true, Some(8)).unwrap(),
            Outcome::default()
        );
        // A migration changed after it ran is refused.
        let changed = described(
            more,
            r#"["alter collection todos rename field title to nom"]"#,
        );
        let e = schema(&mut db, &changed, true, None).unwrap_err();
        assert!(
            e.to_string().contains("not the one the database applied"),
            "{e}"
        );
    }

    /// A rebuild makes a field again as declared, its values kept.
    #[test]
    fn a_rebuild_changes_an_index() {
        let mut db = Database::new();
        let tags =
            r#"[{"name":"tags","fields":[{"name":"name","type":"text","index":{"kind":"hash"}}]}]"#;
        schema(&mut db, &described(tags, "[]"), true, None).unwrap();
        run(&mut db, "put tags [{name: \"b\"}, {name: \"a\"}]", "", &[]);
        let sorted = tags.replace(r#""kind":"hash""#, r#""kind":"sorted""#);
        let o = schema(&mut db, &described(&sorted, "[]"), true, None).unwrap();
        assert_eq!(o.plan.refusals[0].kind, "index_changed");
        let req = described(
            &sorted,
            r#"[{"rebuild":{"collection":"tags","field":"name"}}]"#,
        );
        let o = schema(&mut db, &req, true, None).unwrap();
        assert!(o.applied, "{o:?}");
        let r = run(&mut db, "explain get tags where name > \"a\"", "", &[]);
        assert!(r.contains("ordered index on name"), "{r}");
        let r = run(&mut db, "get tags select name order name", "", &[]);
        assert!(r.contains(r#"[{"name":"a"},{"name":"b"}]"#), "{r}");
        let again = schema(&mut db, &req, true, None).unwrap();
        assert_eq!(again, Outcome::default());
    }

    /// Followed, nothing is written and only what the code lacks is told.
    #[test]
    fn a_followed_database_is_compared_only() {
        let mut db = Database::new();
        run(
            &mut db,
            "create collection todos (title text required, done bool @hash, extra int)",
            "",
            &[],
        );
        let o = follow(&db, &described(TODOS, "[]")).unwrap();
        assert_eq!(o, Outcome::default());
        let more = TODOS.replace(
            r#"{"name":"done""#,
            r#"{"name":"due","type":"timestamp"},{"name":"done""#,
        );
        let o = follow(&db, &described(&more, "[]")).unwrap();
        assert_eq!(o.plan.refusals[0].kind, "field_missing");
        assert!(o.plan.statements.is_empty() && !o.applied);
    }

    /// integrations/schema-golden.json, which every SDK that declares a
    /// schema is held to (`web/schema-golden.mjs` writes it): each
    /// declaration's description reads, and each plan is the one the
    /// engine makes here, natively, of the same database and description.
    #[test]
    fn the_schema_golden_file_is_the_engines() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../integrations/schema-golden.json"
        );
        let golden = json::parse_json(&std::fs::read_to_string(path).unwrap()).unwrap();
        let list = |key: &str| match golden.member(key) {
            Some(Value::List(l)) => l.clone(),
            _ => panic!("no {key}"),
        };
        let text = |v: &Value, key: &str| {
            v.member(key)
                .and_then(Value::as_text)
                .unwrap_or("")
                .to_string()
        };
        let (mut read, mut planned) = (0, 0);
        for case in list("declarations") {
            let Some(c) = case.member("collection") else {
                continue;
            };
            let one = format!(r#"{{"format":1,"collections":[{}]}}"#, json::to_string(c));
            let d =
                declared::parse(&one).unwrap_or_else(|e| panic!("{}: {e}", text(&case, "name")));
            // Described again, it reads as the same collection; written as
            // FenecQL, the declaration's text and the engine's both read as it.
            assert_eq!(
                declared::parse(&declared::describe(&d.collections))
                    .unwrap()
                    .collections,
                d.collections
            );
            let written = fenec_ql::schema_text(&text(&case, "fenecql")).unwrap();
            assert_eq!(written, d.collections, "{}", text(&case, "name"));
            assert_eq!(
                fenec_ql::schema_text(&declared::fenecql(&d.collections)).unwrap(),
                d.collections
            );
            read += 1;
        }
        // A schema written as FenecQL: what an empty database is made with, or the refusal.
        let mut texts = 0;
        for case in list("texts") {
            let name = text(&case, "name");
            let request = format!(
                r#"{{"format":1,"fenecql":{}}}"#,
                json::to_string(case.member("fenecql").unwrap())
            );
            match (
                schema(&mut Database::new(), &request, false, None),
                case.member("error"),
            ) {
                (Ok(o), None) => {
                    let made = json::parse_json(&o.json()).unwrap();
                    assert_eq!(
                        made.member("statements"),
                        case.member("statements"),
                        "{name}"
                    );
                }
                (Err(e), Some(want)) => {
                    assert_eq!(e.to_string(), want.as_text().unwrap(), "{name}")
                }
                (got, _) => panic!("{name}: {got:?}"),
            }
            texts += 1;
        }
        for case in list("plans") {
            let name = text(&case, "name");
            let mut db = Database::new();
            if let Some(Value::List(statements)) = case.member("db") {
                for s in statements {
                    let r = run(&mut db, s.as_text().unwrap(), "", &[]);
                    assert!(!r.contains("\"error\""), "{name}: {r}");
                }
            }
            let seq = db.change_seq();
            let description = json::to_string(case.member("description").unwrap());
            let o = schema(&mut db, &description, false, None);
            // The same schema as FenecQL text plans the same.
            let mut as_text = format!(
                r#"{{"format":1,"fenecql":{}"#,
                json::to_string(case.member("fenecql").unwrap())
            );
            if let Some(m) = case
                .member("description")
                .and_then(|d| d.member("migrations"))
            {
                as_text.push_str(&format!(r#","migrations":{}"#, json::to_string(m)));
            }
            as_text.push('}');
            let t = schema(&mut db, &as_text, false, None);
            assert_eq!(db.change_seq(), seq, "{name}: a plan writes nothing");
            match (o, t, case.member("error")) {
                (Err(e), Err(f), Some(want)) => {
                    assert_eq!(e.to_string(), want.as_text().unwrap(), "{name}");
                    assert_eq!(f.to_string(), e.to_string(), "{name}");
                }
                (Ok(o), Ok(t), None) => {
                    assert_eq!(t, o, "{name}");
                    let made = json::parse_json(&o.json()).unwrap();
                    let mut want = case.member("plan").unwrap().clone();
                    if let Value::Object(m) = &mut want {
                        m.push(("applied".into(), Value::Bool(false)));
                        m.push(("kind".into(), Value::Text("schema".into())));
                        m.sort_by(|a, b| a.0.cmp(&b.0));
                    }
                    assert_eq!(made, want, "{name}");
                }
                (o, t, _) => panic!("{name}: {o:?} / {t:?}"),
            }
            planned += 1;
        }
        assert!(texts > 10, "{texts} texts");
        assert!(
            read > 10 && planned > 10,
            "{read} declarations, {planned} plans"
        );
    }
}
#[cfg(feature = "sync")]
pub mod sync;
