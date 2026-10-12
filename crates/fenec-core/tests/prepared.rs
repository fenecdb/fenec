//! A block's `put`s built and encoded before the write lock
//! (`Database::prepare`), beside the readers, write what the same block
//! built under the lock writes: every answer, every byte of the file,
//! whatever the block holds -- puts over held ids, upserts, `@unique` and
//! its refusals, `returning`, a parameter's documents, sets and deletes
//! between the puts, a block put back -- and a preparation whose schema
//! moved before the lock is built again rather than written. Readers
//! beside prepared blocks landing back to back see each block whole or not
//! at all, and never one put back; and the buffer written behind the lock
//! (`Database::write_behind`) leaves the file what it was.

#![cfg(all(feature = "std-fs", unix, target_pointer_width = "64"))]

use fenec_core::fs::open_mapped;
use fenec_core::plugin::{Hook, Plugin, Registry, WriteOp};
use fenec_core::prelude::*;
use fenec_core::schema::Schema;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

fn stmt(sql: &str) -> Statement {
    fenec_ql::parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn run(db: &mut Database, sql: &str) {
    for s in fenec_ql::parse(sql).expect("parse") {
        db.execute(&s).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

fn answer(db: &Database, sql: &str) -> ResultSet {
    db.query(&stmt(sql), &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows()
        .expect("rows")
        .clone()
}

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fenecdb-prepared-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{tag}.fenec"));
    let _ = std::fs::remove_file(&path);
    path
}

fn same_bytes(a: &PathBuf, b: &PathBuf) {
    let (x, y) = (std::fs::read(a).unwrap(), std::fs::read(b).unwrap());
    assert!(x == y, "{} and {} bytes differ", x.len(), y.len());
}

const QUERIES: &[&str] = &[
    "get docs order id",
    "get docs where kind = \"b\" order id",
    "get docs where n >= 10 and n < 90 order n desc limit 9",
    "get docs where meta.lang = \"tr\" order id",
    "get docs match body \"alpha gamma\" limit 5",
    "get docs near v [0.3, 0.1, 0.9, 0.2] limit 5",
    "get users order id",
    "get users where email = \"USER3@X.COM\"",
    "get ev order id",
    "get ev select k, count(*), sum(n) group k",
];

fn answers(db: &Database) -> Vec<ResultSet> {
    let mut out: Vec<ResultSet> = QUERIES.iter().map(|q| answer(db, q)).collect();
    out.push(answer(db, "get docs count"));
    out
}

fn schema(db: &mut Database) {
    run(
        db,
        "create collection docs (title text, kind text @hash, n int @sorted, body text @text, \
         meta json, v vector<4> @hnsw(cosine, m=8))",
    );
    run(db, "create index on docs (meta.lang) @hash");
    run(
        db,
        "create collection users (email text nocase @unique, name text, score int)",
    );
    run(db, "create collection ev (k text, n int)");
}

/// A block: each statement's text and parameters.
type Block = Vec<(&'static str, Vec<Value>)>;

fn doc(i: i64) -> Vec<Value> {
    let word = ["alpha", "beta", "gamma", "delta"][i as usize % 4];
    vec![
        Value::Text(format!("t{i}")),
        Value::Text(["a", "b", "c"][i as usize % 3].into()),
        Value::Int(i),
        Value::Text(format!("{word} {word} body {i}")),
        Value::Vector(vec![(i % 7) as f32, (i % 5) as f32, (i % 3) as f32, 1.0]),
        Value::Text(["tr", "en"][i as usize % 2].into()),
    ]
}

const PUT_DOC: &str =
    "put docs {title: $1, kind: $2, n: $3, body: $4, v: $5, meta.lang: $6, meta.at: $3}";

fn objects(from: i64, n: i64) -> Value {
    Value::List(
        (from..from + n)
            .map(|i| {
                Value::Object(vec![
                    ("k".into(), Value::Text(format!("k{}", i % 5))),
                    ("n".into(), Value::Int(i)),
                ])
            })
            .collect(),
    )
}

/// Blocks of every shape a `put` takes in one, and blocks that are put
/// back: an insert over a held id, a `@unique` value twice in a case
/// `nocase` folds.
fn workload() -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    blocks.push((0..200).map(|i| (PUT_DOC, doc(i))).collect());
    // Two collections, a parameter's documents, upserts of keys repeated.
    let mut b: Block = Vec::new();
    for i in 0..40i64 {
        b.push((
            "put users {email: $1, name: $2, score: $3} if absent else set {score: score + new.score}",
            vec![
                Value::Text(format!("user{}@x.com", i % 13)),
                Value::Text(format!("name {i}")),
                Value::Int(i),
            ],
        ));
        if i % 10 == 0 {
            b.push(("put ev $1", vec![objects(i * 100, 30)]));
        }
        b.push((PUT_DOC, doc(200 + i)));
    }
    blocks.push(b);
    // Sets and deletes between the puts, puts over held ids, an id named.
    let mut b: Block = Vec::new();
    for i in 0..30i64 {
        b.push((PUT_DOC, doc(300 + i)));
        if i % 7 == 0 {
            b.push((
                "set docs {title: \"changed\", n: n + 1000} where kind = $1 and n < 50",
                vec![Value::Text("b".into())],
            ));
            b.push(("del docs where n = $1", vec![Value::Int(10 + i)]));
        }
        b.push((
            "put docs {id: $1, title: $2, kind: \"c\", n: $3, body: \"over\", meta: {lang: \"tr\"}}",
            vec![
                Value::Int(1 + i * 3),
                Value::Text(format!("over {i}")),
                Value::Int(i),
            ],
        ));
    }
    b.push((
        "put docs {id: 5000, title: \"named\", kind: \"a\", n: 5000}",
        vec![],
    ));
    blocks.push(b);
    // `returning`, and a list of documents in the text.
    let mut b: Block = Vec::new();
    for i in 0..20i64 {
        b.push((
            "insert docs {title: $1, kind: \"b\", n: $2, body: \"ret\"} returning id, title",
            vec![Value::Text(format!("r{i}")), Value::Int(600 + i)],
        ));
    }
    b.push((
        "put ev [{k: \"x\", n: 1}, {k: \"y\", n: 2}, {k: \"x\", n: 3}]",
        vec![],
    ));
    blocks.push(b);
    // Put back: an insert over a held id after fifty puts.
    let mut b: Block = (0..50).map(|i| (PUT_DOC, doc(700 + i))).collect();
    b.push(("insert docs {id: 1, title: \"twice\"}", vec![]));
    blocks.push(b);
    // Put back: a `@unique` value its fold already holds.
    let mut b: Block = vec![("put ev $1", vec![objects(9000, 40)])];
    b.push((
        "insert users {email: \"USER3@X.COM\", name: \"again\"}",
        vec![],
    ));
    blocks.push(b);
    // And one that lands after them, its ids the ones they handed out.
    blocks.push((0..60).map(|i| (PUT_DOC, doc(800 + i))).collect());
    blocks
}

/// What a block answered, or where it stopped and why.
type Outcome = std::result::Result<Vec<Response>, (usize, String)>;

fn plain(db: &mut Database, stmts: &[(&Statement, &[Value])]) -> Outcome {
    db.execute_block(stmts).map_err(|(i, e)| (i, e.to_string()))
}

/// Prepared as a server's `/batch` is, the buffer written behind the lock.
fn prepared(db: &mut Database, stmts: &[(&Statement, &[Value])], spent: &mut usize) -> Outcome {
    let mut p = db.prepare(stmts);
    let out = db
        .execute_prepared(stmts, &mut p)
        .map_err(|(i, e)| (i, e.to_string()));
    *spent += p.spent();
    if let Some(write) = db.write_behind() {
        write().unwrap();
    }
    out
}

fn play(db: &mut Database, blocks: &[Block], prepare: bool) -> (Vec<Outcome>, usize) {
    let mut spent = 0;
    let outs = blocks
        .iter()
        .map(|b| {
            let parsed: Vec<Statement> = b.iter().map(|(s, _)| stmt(s)).collect();
            let stmts: Vec<(&Statement, &[Value])> = parsed
                .iter()
                .zip(b)
                .map(|(s, (_, p))| (s, &p[..]))
                .collect();
            match prepare {
                true => prepared(db, &stmts, &mut spent),
                false => plain(db, &stmts),
            }
        })
        .collect();
    (outs, spent)
}

#[test]
fn a_prepared_block_writes_what_a_block_built_under_the_lock_writes() {
    let (a_path, b_path) = (tmp("same-a"), tmp("same-b"));
    let (mut a, mut b) = (open_mapped(&a_path).unwrap(), open_mapped(&b_path).unwrap());
    schema(&mut a);
    schema(&mut b);
    let blocks = workload();
    let (plain, _) = play(&mut a, &blocks, false);
    let (ready, spent) = play(&mut b, &blocks, true);
    assert_eq!(plain, ready);
    // The blocks put back stopped where they stop built under the lock.
    assert!(matches!(plain[4], Err((50, _))), "{:?}", plain[4]);
    assert!(matches!(plain[5], Err((1, _))), "{:?}", plain[5]);
    // The documents were the preparation's: every put's but those an
    // upsert set rather than wrote, and those of the blocks put back.
    assert!(spent > 400, "{spent}");
    assert_eq!(answers(&a), answers(&b));
    assert_eq!(a.change_seq(), b.change_seq());
    drop((a, b));
    same_bytes(&a_path, &b_path);
    let (a, b) = (open_mapped(&a_path).unwrap(), open_mapped(&b_path).unwrap());
    assert_eq!(answers(&a), answers(&b));
}

/// A block whose statements run one at a time under the lock, as a
/// server's `/batch` runs them (`execute_ready`), writes what the same
/// statements run with `execute_with` write.
#[test]
fn statements_run_one_at_a_time_take_their_documents_as_a_block_does() {
    let (a_path, b_path) = (tmp("each-a"), tmp("each-b"));
    let (mut a, mut b) = (open_mapped(&a_path).unwrap(), open_mapped(&b_path).unwrap());
    schema(&mut a);
    schema(&mut b);
    let mut spent = 0;
    for block in workload() {
        let parsed: Vec<Statement> = block.iter().map(|(s, _)| stmt(s)).collect();
        let stmts: Vec<(&Statement, &[Value])> = parsed
            .iter()
            .zip(&block)
            .map(|(s, (_, p))| (s, &p[..]))
            .collect();
        let mut p = b.prepare(&stmts);
        a.begin().unwrap();
        b.begin().unwrap();
        let mut failed = false;
        for (i, (s, params)) in stmts.iter().enumerate() {
            let x = a.execute_with(s, params);
            let y = b.execute_ready(s, params, &mut p, i);
            assert_eq!(x, y, "{}", block[i].0);
            if x.is_err() {
                failed = true;
                break;
            }
        }
        match failed {
            true => {
                a.rollback();
                b.rollback();
            }
            false => {
                a.commit().unwrap();
                b.commit().unwrap();
            }
        }
        spent += p.spent();
    }
    assert!(spent > 400, "{spent}");
    assert_eq!(answers(&a), answers(&b));
    drop((a, b));
    same_bytes(&a_path, &b_path);
}

/// A preparation made before the schema moved -- a field added, an index
/// made, the collection dropped and made again -- is built again under
/// the lock, not written: built against the schema before, a document
/// would miss the field added, or be encoded for the collection that went.
#[test]
fn a_preparation_the_schema_moved_from_is_built_again() {
    let changes = [
        "alter collection ev add field extra text",
        "create index on ev (k) @hash",
        "drop collection ev; create collection ev (k text, n int, more int)",
    ];
    for (c, change) in changes.iter().enumerate() {
        let (a_path, b_path) = (tmp(&format!("moved-{c}-a")), tmp(&format!("moved-{c}-b")));
        let (mut a, mut b) = (open_mapped(&a_path).unwrap(), open_mapped(&b_path).unwrap());
        schema(&mut a);
        schema(&mut b);
        let put = stmt("put ev $1");
        let params = vec![objects(0, 50)];
        let stmts: Vec<(&Statement, &[Value])> = vec![(&put, &params[..]); 3];
        let mut p = b.prepare(&stmts);
        assert_eq!(p.built(), 3);
        run(&mut a, change);
        run(&mut b, change);
        a.execute_block(&stmts).unwrap();
        b.execute_prepared(&stmts, &mut p).unwrap();
        assert_eq!(p.spent(), 0, "{change}");
        assert_eq!(answer(&a, "get ev order id"), answer(&b, "get ev order id"));
        // Prepared now, it is written.
        let mut p = b.prepare(&stmts);
        a.execute_block(&stmts).unwrap();
        b.execute_prepared(&stmts, &mut p).unwrap();
        assert_eq!(p.spent(), 150, "{change}");
        assert_eq!(answer(&a, "get ev order id"), answer(&b, "get ev order id"));
        drop((a, b));
        same_bytes(&a_path, &b_path);
    }
}

/// Writes `tag` upper-cased into every document, as a hook that makes an
/// embedding writes one.
struct Shout;
impl Hook for Shout {
    fn name(&self) -> &str {
        "shout"
    }
    fn before_write(&self, _: &Schema, _: WriteOp, doc: &mut Document) -> Result<()> {
        if let Some(Value::Text(t)) = doc.get("k").cloned() {
            doc.set("k", Value::Text(t.to_uppercase()));
        }
        Ok(())
    }
}
impl Plugin for Shout {
    fn name(&self) -> &str {
        "shout"
    }
    fn init(&self, reg: &mut Registry) -> Result<()> {
        reg.register_hook(Arc::new(Shout));
        Ok(())
    }
}

/// A hook that may change a document has it encoded as the hook leaves
/// it: the payload built beside the readers is the document before.
#[test]
fn a_hook_that_rewrites_has_its_document_encoded_after_it() {
    let (a_path, b_path) = (tmp("hook-a"), tmp("hook-b"));
    let (mut a, mut b) = (open_mapped(&a_path).unwrap(), open_mapped(&b_path).unwrap());
    for db in [&mut a, &mut b] {
        schema(db);
        db.install_plugin(&Shout).unwrap();
    }
    let put = stmt("put ev $1");
    let params = vec![objects(0, 40)];
    let stmts: Vec<(&Statement, &[Value])> = vec![(&put, &params[..])];
    let mut p = b.prepare(&stmts);
    a.execute_block(&stmts).unwrap();
    b.execute_prepared(&stmts, &mut p).unwrap();
    assert_eq!(p.spent(), 40);
    let rows = answer(&b, "get ev where k = \"K1\" count");
    assert_eq!(rows.rows[0].values[0], Value::Int(8));
    assert_eq!(answer(&a, "get ev order id"), answer(&b, "get ev order id"));
    drop((a, b));
    same_bytes(&a_path, &b_path);
}

/// Four readers beside a writer landing prepared blocks back to back, as
/// a server's `/batch`es do -- built under the read lock, landed under the
/// write lock, the buffer written once it is let go -- every fifth put
/// back at its last statement: each reader finds every block whole or
/// absent, and none put back. Each block writes its number into every row
/// it holds, so a block seen in part is a group of fewer rows.
#[test]
fn readers_beside_prepared_blocks_see_each_whole_or_not_at_all() {
    const ROWS: i64 = 150;
    const BLOCKS: i64 = 300;
    let path = tmp("readers");
    let mut db = open_mapped(&path).unwrap();
    run(
        &mut db,
        "create collection b (block int @hash, i int, note text)",
    );
    // Handed over every few blocks, so the readers meet that too.
    db.set_handover(64 << 10);
    let db = Arc::new(RwLock::new(db));
    let done = AtomicBool::new(false);
    let landed = AtomicU64::new(0);
    std::thread::scope(|s| {
        for r in 0..4u64 {
            let (db, done, landed) = (Arc::clone(&db), &done, &landed);
            s.spawn(move || {
                let groups = stmt("get b select block, count(*) as n group block");
                let one = stmt("get b where id = $1");
                let mut seed = r;
                let mut looks = 0;
                while !done.load(Ordering::Relaxed) || looks < 50 {
                    looks += 1;
                    let floor = landed.load(Ordering::Acquire);
                    let g = db.read().unwrap();
                    let rows = g.query(&groups, &[]).unwrap();
                    let rows = rows.rows().unwrap();
                    let mut whole = 0;
                    for row in &rows.rows {
                        let (Value::Int(k), Value::Int(n)) = (&row.values[0], &row.values[1])
                        else {
                            panic!("{row:?}");
                        };
                        assert_eq!(*n, ROWS, "block {k} seen in part");
                        assert!(k % 5 != 4, "block {k} was put back");
                        whole += 1;
                    }
                    // Never behind what had landed when it looked.
                    assert!(whole >= floor, "{whole} blocks, {floor} landed before");
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let id = (seed >> 33) as i64 % (whole as i64 * ROWS + 1) + 1;
                    let got = g.query(&one, &[Value::Int(id)]).unwrap();
                    drop(g);
                    assert!(got.rows().unwrap().rows.len() <= 1);
                }
            });
        }
        let put = stmt("put b {block: $1, i: $2, note: $3}");
        let dup = stmt("insert b {id: 1, block: -1}");
        let mut good = 0;
        for k in 0..BLOCKS {
            let args: Vec<[Value; 3]> = (0..ROWS)
                .map(|i| {
                    [
                        Value::Int(k),
                        Value::Int(i),
                        Value::Text(format!("n{k}-{i}")),
                    ]
                })
                .collect();
            let mut stmts: Vec<(&Statement, &[Value])> =
                args.iter().map(|a| (&put, &a[..])).collect();
            if k % 5 == 4 {
                stmts.push((&dup, &[]));
            }
            let mut p = db.read().unwrap().prepare(&stmts);
            let behind = {
                let mut g = db.write().unwrap();
                let out = g.execute_prepared(&stmts, &mut p);
                assert_eq!(out.is_err(), k % 5 == 4, "block {k}");
                g.write_behind()
            };
            drop(p);
            if k % 5 != 4 {
                good += 1;
                landed.store(good, Ordering::Release);
            }
            if let Some(write) = behind {
                write().unwrap();
            }
        }
        done.store(true, Ordering::Relaxed);
    });
    let db = Arc::try_unwrap(db).ok().unwrap().into_inner().unwrap();
    let total = answer(&db, "get b count").rows[0].values[0].clone();
    assert_eq!(total, Value::Int(BLOCKS / 5 * 4 * ROWS));
    // The ids the blocks put back handed out were handed out again.
    let last = answer(&db, "get b select id order id desc limit 1").rows[0].values[0].clone();
    assert_eq!(last, Value::Int(BLOCKS / 5 * 4 * ROWS));
    drop(db);
    let db = open_mapped(&path).unwrap();
    assert_eq!(answer(&db, "get b count").rows[0].values[0], total);
}

/// Written behind the lock, the buffer reaches the file as the appends
/// would have written it: a twin that leaves it to its appends holds the
/// same bytes, both hand their documents over, and the file opens to the
/// same answers.
#[test]
fn writes_behind_the_lock_leave_the_file_what_it_was() {
    let (a_path, b_path) = (tmp("behind-a"), tmp("behind-b"));
    let (mut a, mut b) = (open_mapped(&a_path).unwrap(), open_mapped(&b_path).unwrap());
    run(&mut a, "create collection c (n int, s text)");
    run(&mut b, "create collection c (n int, s text)");
    let put = stmt("put c {n: $1, s: $2}");
    let mut wrote = 0;
    for k in 0..80i64 {
        let args: Vec<[Value; 2]> = (0..500)
            .map(|i| [Value::Int(k * 500 + i), Value::Text(format!("row {k} {i}"))])
            .collect();
        let stmts: Vec<(&Statement, &[Value])> = args.iter().map(|a| (&put, &a[..])).collect();
        a.execute_block(&stmts).unwrap();
        b.execute_block(&stmts).unwrap();
        if let Some(write) = b.write_behind() {
            write().unwrap();
            wrote += 1;
        }
    }
    assert!(wrote > 10, "{wrote}");
    // Both handed over: what their stores hold in memory is a few blocks.
    for db in [&a, &b] {
        let held = db.collection("c").unwrap().store.heap_bytes();
        assert!(held < 1 << 20, "{held}");
    }
    assert_eq!(answer(&a, "get c order id"), answer(&b, "get c order id"));
    a.sync().unwrap();
    b.sync().unwrap();
    drop((a, b));
    same_bytes(&a_path, &b_path);
    let b = open_mapped(&b_path).unwrap();
    assert_eq!(
        answer(&b, "get c count").rows[0].values[0],
        Value::Int(40_000)
    );
}

/// Panics at a document whose `k` is `boom`: a thread that unwinds out of
/// a block, as a server's connection thread may, leaves the database to the
/// next request.
struct Boom;
impl Hook for Boom {
    fn name(&self) -> &str {
        "boom"
    }
    fn rewrites(&self) -> bool {
        false
    }
    fn before_write(&self, _: &Schema, _: WriteOp, doc: &mut Document) -> Result<()> {
        if let Some(Value::Text(t)) = doc.get("k") {
            assert_ne!(t, "boom", "a hook that panics");
        }
        Ok(())
    }
}
impl Plugin for Boom {
    fn name(&self) -> &str {
        "boom"
    }
    fn init(&self, reg: &mut Registry) -> Result<()> {
        reg.register_hook(Arc::new(Boom));
        Ok(())
    }
}

/// A preparation a panic left half way is no later block's: its second
/// `put` never ran, and a block run after it with no preparation of its
/// own writes its own documents into the same collection, not those.
#[test]
fn a_preparation_a_panic_left_is_let_go_of() {
    let mut db = Database::new();
    run(&mut db, "create collection ev (k text, n int)");
    db.install_plugin(&Boom).unwrap();
    let put = stmt("put ev $1");
    let first = vec![Value::List(vec![Value::Object(vec![(
        "k".into(),
        Value::Text("boom".into()),
    )])])];
    let second = vec![objects(0, 20)];
    let stmts: Vec<(&Statement, &[Value])> = vec![(&put, &first[..]), (&put, &second[..])];
    let mut p = db.prepare(&stmts);
    assert_eq!(p.built(), 2);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = db.execute_prepared(&stmts, &mut p);
    }));
    assert!(unwound.is_err());
    db.rollback();
    let mine = vec![objects(500, 20)];
    let again: Vec<(&Statement, &[Value])> = vec![(&put, &mine[..]), (&put, &mine[..])];
    db.execute_block(&again).unwrap();
    let rows = answer(&db, "get ev select n order id");
    let ns: Vec<Value> = rows.rows.iter().map(|r| r.values[0].clone()).collect();
    let want: Vec<Value> = (0..2).flat_map(|_| (500..520).map(Value::Int)).collect();
    assert_eq!(ns, want);
}
