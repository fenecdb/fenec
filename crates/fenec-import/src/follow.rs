//! `fenec import --follow`: once the copy is written, the table's changes as
//! they commit, through a logical replication slot and `pgoutput`.
//!
//! The stream goes through the same mapping as the copy: a row that arrives
//! as a change becomes the document the copy would have made of it, so the
//! two can never disagree about a type.
//!
//! **Nothing is confirmed before it is on disk.** The slot's confirmed
//! position is what the server may forget, so it only ever moves to a
//! transaction whose changes an fsync has covered. A stream that breaks, or
//! a follower that is killed, starts again from there; what it had applied
//! past that point comes again, and applying it again changes nothing --
//! every change is written by id, as a put of the whole row or a delete.
//!
//! **The copy and the slot need not agree on a moment.** The slot is made
//! first and the copy read after it, so the stream begins with changes the
//! copy may already hold; replaying them converges on the same rows, for
//! the same reason.
//!
//! **A copy is only resumed once it is known to be whole.** The target file
//! keeps a small `_follow` collection saying which collection follows which
//! slot, and whether its copy finished. A copy cut short -- the process
//! killed halfway -- is made again rather than streamed on top of, which
//! would leave the rows it never reached missing for good.

use crate::load;
use crate::map::{self, Plan, Target};
use crate::pg::{self, Kind, Query, Reader, Url, VectorOids};
use crate::{Column, Options, Summary};
use fenec_core::error::{Error, Result};
use fenec_core::plugin::Registry;
use fenec_core::prelude::*;
use fenec_core::query::EvalCtx;
use fenec_core::schema::Field;
use fenec_pg::client::{self, Client, FieldDesc, Wal, WalStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{PoisonError, RwLock, RwLockWriteGuard};
use std::time::{Duration, Instant};

/// A log position as PostgreSQL prints it, for reporting one.
pub use fenec_pg::client::lsn_text;

/// The collection in the target file that remembers what follows what.
pub const MARKER: &str = "_follow";

/// How long applied changes may wait for their fsync while the stream keeps
/// the socket busy. An idle socket syncs at once; this only bounds the wait
/// under a steady stream, where one fsync then covers many transactions.
const SYNC_EVERY: Duration = Duration::from_millis(100);

/// How long one wait for the stream lasts: it is also how quickly a stop is
/// noticed.
const IDLE_WAIT: Duration = Duration::from_millis(200);

/// The first pause before opening a broken stream again; it doubles up to
/// [`MAX_BACKOFF`] and starts over once a stream opens.
const FIRST_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// The slot and publication a collection follows through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Follow {
    pub slot: String,
    pub publication: String,
}

impl Follow {
    /// `fenec_<collection>`, both of them: the collection's name made a plain
    /// PostgreSQL identifier.
    pub fn named_after(collection: &str) -> Follow {
        let mut name = String::from("fenec_");
        for c in collection.chars() {
            name.push(if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            });
        }
        name.truncate(63);
        Follow {
            slot: name.clone(),
            publication: name,
        }
    }
}

/// What the follower reports as it goes; printing it is the caller's job.
#[derive(Debug, Clone)]
pub enum Event {
    /// The publication and the slot are in place; `true` where one had to be
    /// made.
    Prepared {
        publication_created: bool,
        slot_created: bool,
    },
    /// The copy is being made, from the start; the reason says why.
    Copying(&'static str),
    /// Rows of the copy written so far.
    CopyProgress(u64),
    /// The copy is written, indexed and on disk.
    Copied(Summary),
    /// The stream is open, from the slot's confirmed position.
    Streaming,
    /// Transactions applied, on disk and confirmed to the server, since the
    /// stream opened.
    Confirmed {
        lsn: u64,
        transactions: u64,
        changes: u64,
    },
    /// A column of the source that has no field in the collection -- added
    /// after the copy; its values are not kept.
    Ignored(String),
    /// The stream broke, and opens again after `wait`.
    Reconnecting { error: String, wait: Duration },
}

/// Follows `table` into `opts.into` until `stop` is set: the copy first when
/// there is no whole one, then the changes. Returns once stopped, with every
/// applied change on disk and confirmed; an error it cannot wait out ends it.
pub fn run(
    url: &Url,
    table: &str,
    db: &RwLock<Database>,
    opts: &Options,
    follow: &Follow,
    stop: &AtomicBool,
    report: &mut dyn FnMut(Event),
) -> Result<()> {
    let source = Source::inspect(url, table, opts, follow)?;
    report(Event::Prepared {
        publication_created: source.publication_created,
        slot_created: source.slot_created,
    });
    ensure_copy(&source, db, opts, follow, report)?;

    let mut backoff = FIRST_BACKOFF;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        match stream(&source, db, opts, follow, stop, &mut backoff, report) {
            Ok(()) => return Ok(()),
            Err(Failure::Stream(e)) if retryable(&e) => {
                report(Event::Reconnecting {
                    error: e.to_string(),
                    wait: backoff,
                });
                pause(backoff, stop);
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            Err(Failure::Stream(e) | Failure::Apply(e)) => return Err(e),
        }
    }
}

// ------------------------------------------------------------------ catalog

/// What the catalog says about the followed table, and the connection
/// details a stream needs.
struct Source {
    url: Url,
    /// As it was given: `docs`, `public.docs`.
    table: String,
    /// Quoted for SQL: `"public"."docs"`.
    sql: String,
    namespace: String,
    name: String,
    columns: Vec<Column>,
    kinds: Vec<Kind>,
    oids: VectorOids,
    plan: Plan,
    publication_created: bool,
    slot_created: bool,
}

impl Source {
    /// Checks what a stream needs and makes what is missing: the table, the
    /// server's `wal_level`, the publication, the slot -- the slot before
    /// any copy is read, so that nothing committed after the copy can be
    /// missed.
    fn inspect(url: &Url, table: &str, opts: &Options, follow: &Follow) -> Result<Source> {
        for name in [&follow.slot, &follow.publication] {
            if !client::plain_name(name) {
                return Err(Error::Query(format!(
                    "`{name}` cannot name a slot or a publication: lowercase letters, \
                     digits and `_`, 63 at most -- pick one with --slot / --publication"
                )));
            }
        }
        let mut c = Client::connect(url)?;
        let level = first(&c.query("show wal_level")?).unwrap_or_default();
        if level != "logical" {
            return Err(Error::Query(format!(
                "the server's wal_level is `{level}`; following needs `logical` \
                 (alter system set wal_level = logical, then a restart)"
            )));
        }

        let sql = pg::quote_ident(table)?;
        let r = c.query(&format!(
            "select n.nspname, c.relname from pg_class c \
             join pg_namespace n on n.oid = c.relnamespace \
             where c.oid = {}::regclass",
            literal(&sql)
        ))?;
        let (namespace, name) = match r.rows.first().map(|r| r.as_slice()) {
            Some([Some(ns), Some(n)]) => (ns.clone(), n.clone()),
            _ => return Err(Error::NotFound(format!("table `{table}`"))),
        };

        let oids = pg::vector_oids(&mut c)?;
        let desc = c.query(&format!("select * from {sql} limit 0"))?;
        let (columns, kinds): (Vec<Column>, Vec<Kind>) =
            desc.columns.iter().map(|f| pg::map_oid(f, &oids)).unzip();
        let plan = map::plan(&columns, opts)?;
        if !plan.uses_source_id() {
            return Err(Error::Query(
                "following needs the rows' key as the document id, to find a row \
                 again when it changes: an integer `id` column, or --id <column>"
                    .into(),
            ));
        }

        let publication_created = publication(&mut c, follow, &sql, &namespace, &name)?;
        let slot_created = slot(&mut c, follow, url)?;
        Ok(Source {
            url: url.clone(),
            table: table.to_string(),
            sql,
            namespace,
            name,
            columns,
            kinds,
            oids,
            plan,
            publication_created,
            slot_created,
        })
    }

    /// The source column the document id comes from.
    fn id_column(&self) -> usize {
        self.plan
            .targets
            .iter()
            .position(|t| *t == Target::Id)
            .unwrap_or(0)
    }
}

/// Makes the publication when it is missing. One that exists has to carry
/// every kind of change for this table: a publication that leaves out
/// deletes would leave deleted rows in the collection, silently.
fn publication(
    c: &mut Client,
    follow: &Follow,
    sql: &str,
    namespace: &str,
    name: &str,
) -> Result<bool> {
    let p = &follow.publication;
    let r = c.query(&format!(
        "select pubinsert, pubupdate, pubdelete, pubtruncate \
         from pg_publication where pubname = {}",
        literal(p)
    ))?;
    let Some(row) = r.rows.first() else {
        c.query(&format!("create publication {p} for table {sql}"))?;
        return Ok(true);
    };
    if row.iter().any(|v| v.as_deref() != Some("t")) {
        return Err(Error::Query(format!(
            "publication `{p}` leaves out some kinds of change; a follower needs \
             inserts, updates, deletes and truncates: \
             alter publication {p} set (publish = 'insert, update, delete, truncate')"
        )));
    }
    let r = c.query(&format!(
        "select 1 from pg_publication_tables where pubname = {} \
         and schemaname = {} and tablename = {}",
        literal(p),
        literal(namespace),
        literal(name)
    ))?;
    if r.rows.is_empty() {
        return Err(Error::Query(format!(
            "publication `{p}` does not include {sql}: alter publication {p} add table {sql}"
        )));
    }
    Ok(false)
}

/// Makes the slot when it is missing; one that exists has to be a logical
/// `pgoutput` slot of this database.
fn slot(c: &mut Client, follow: &Follow, url: &Url) -> Result<bool> {
    let s = &follow.slot;
    let r = c.query(&format!(
        "select plugin, slot_type, database from pg_replication_slots where slot_name = {}",
        literal(s)
    ))?;
    let Some(row) = r.rows.first() else {
        c.query(&format!(
            "select lsn from pg_create_logical_replication_slot({}, 'pgoutput')",
            literal(s)
        ))?;
        return Ok(true);
    };
    let got = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
    if got(1) != "logical" || got(0) != "pgoutput" || got(2) != url.database {
        return Err(Error::Query(format!(
            "slot `{s}` exists but is not a logical pgoutput slot of `{}` \
             ({} {} in `{}`): pick another with --slot",
            url.database,
            got(1),
            got(0),
            got(2)
        )));
    }
    Ok(false)
}

/// A SQL string literal.
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The first cell of a result.
fn first(r: &client::QueryResult) -> Option<String> {
    r.rows.first()?.first()?.clone()
}

// --------------------------------------------------------------------- copy

/// What `_follow` says about one collection.
struct Marker {
    id: DocId,
    slot: String,
    copied: bool,
}

fn marker_schema() -> Result<Schema> {
    Schema::new(
        MARKER,
        vec![
            Field::new("collection", DataType::Text),
            Field::new("slot", DataType::Text),
            Field::new("publication", DataType::Text),
            Field::new("copied", DataType::Bool),
        ],
    )
}

fn read_marker(db: &Database, collection: &str) -> Result<Option<Marker>> {
    let Ok(c) = db.collection(MARKER) else {
        return Ok(None);
    };
    for id in c.store.ids() {
        let Some(doc) = c.store.read(&c.schema, id)? else {
            continue;
        };
        if doc.get("collection") != Some(&Value::Text(collection.to_string())) {
            continue;
        }
        let slot = match doc.get("slot") {
            Some(Value::Text(s)) => s.clone(),
            _ => String::new(),
        };
        let copied = doc.get("copied") == Some(&Value::Bool(true));
        return Ok(Some(Marker { id, slot, copied }));
    }
    Ok(None)
}

fn write_marker(
    db: &mut Database,
    id: Option<DocId>,
    collection: &str,
    follow: &Follow,
    copied: bool,
) -> Result<DocId> {
    db.execute(&Statement::CreateCollection {
        schema: marker_schema()?,
        if_not_exists: true,
    })?;
    let text = |s: &str| Expr::Lit(Value::Text(s.to_string()));
    let mut doc = vec![
        ("collection".to_string(), text(collection)),
        ("slot".to_string(), text(&follow.slot)),
        ("publication".to_string(), text(&follow.publication)),
        ("copied".to_string(), Expr::Lit(Value::Bool(copied))),
    ];
    if let Some(id) = id {
        doc.push(("id".to_string(), Expr::Lit(Value::Int(id as i64))));
    }
    db.execute(&Statement::Put {
        collection: MARKER.to_string(),
        docs: vec![doc],
    })?;
    db.sync()?;
    let marker = read_marker(db, collection)?;
    Ok(marker.map_or(0, |m| m.id))
}

/// Makes the copy unless a whole one of this slot is already in the file.
///
/// A collection with no `_follow` entry is not the follower's to replace; one
/// whose copy never finished, or whose slot is new -- the changes made while
/// nothing held a slot are lost to it -- is made again.
fn ensure_copy(
    source: &Source,
    db: &RwLock<Database>,
    opts: &Options,
    follow: &Follow,
    report: &mut dyn FnMut(Event),
) -> Result<()> {
    let mut g = write(db);
    let exists = g.collection(&opts.into).is_ok();
    let marker = read_marker(&g, &opts.into)?;
    if let Some(m) = &marker {
        if m.slot != follow.slot {
            return Err(Error::Query(format!(
                "`{}` follows slot `{}`; this run names `{}`",
                opts.into, m.slot, follow.slot
            )));
        }
    }
    let why = match (&marker, exists) {
        (None, true) => {
            return Err(Error::Exists(format!(
                "`{}` is already in the file and was not copied by a follower: \
                 import into another collection or another file",
                opts.into
            )))
        }
        (Some(m), true) if m.copied && !source.slot_created => {
            return resume(source, &mut g, opts);
        }
        (Some(m), true) if m.copied => "the slot is new, and what changed without one is unknown",
        (Some(_), true) => "the last copy did not finish",
        (_, false) => "there is none yet",
    };
    report(Event::Copying(why));
    if exists {
        g.execute(&Statement::DropCollection {
            name: opts.into.clone(),
            if_exists: true,
        })?;
    }
    let id = write_marker(&mut g, marker.map(|m| m.id), &opts.into, follow, false)?;

    let mut reader = Reader::open(&source.url, &Query::table(source.table.clone()))?;
    let copied = load::run_with_progress(&mut reader, &mut g, opts, &mut |n| {
        report(Event::CopyProgress(n))
    });
    let summary = match copied {
        Ok(s) => s,
        Err(e) => {
            // Half a collection is worth nothing to a follower: the next run
            // copies again, and a rerun of this one should not find it.
            let _ = g.execute(&Statement::DropCollection {
                name: opts.into.clone(),
                if_exists: true,
            });
            let _ = g.sync();
            return Err(e);
        }
    };
    write_marker(&mut g, Some(id), &opts.into, follow, true)?;
    report(Event::Copied(summary));
    Ok(())
}

/// A whole copy is in the file: it must have the fields this run's plan
/// would make, or the stream would write rows of another shape into it.
fn resume(source: &Source, db: &mut Database, opts: &Options) -> Result<()> {
    let have = &db.collection(&opts.into)?.schema.fields;
    let want = &source.plan.schema.fields;
    let same = have.len() == want.len()
        && have
            .iter()
            .zip(want)
            .all(|(a, b)| a.name == b.name && a.ty == b.ty);
    if !same {
        return Err(Error::Query(format!(
            "`{}` was copied with another mapping of the table: give the options \
             of the first run (--vector, --cast, --id), or copy afresh into a new file",
            opts.into
        )));
    }
    // An index named now and not at the copy is built now.
    for (field, kind) in &opts.indexes {
        db.execute(&Statement::CreateIndex {
            collection: opts.into.clone(),
            field: field.clone(),
            kind: kind.clone(),
            if_not_exists: true,
        })?;
    }
    Ok(())
}

// ------------------------------------------------------------------- stream

/// Why a stream ended: the connection, which is waited out, or the data,
/// which is not.
enum Failure {
    Stream(Error),
    Apply(Error),
}

/// Whether waiting and opening the stream again can help.
fn retryable(e: &Error) -> bool {
    match e {
        // The network, from the client.
        Error::Io(_) => true,
        // The slot still held by a connection the server has not noticed is
        // gone; the server shutting down or starting up; a broken connection.
        Error::Query(m) => ["55006", "57P01", "57P02", "57P03", "08"]
            .iter()
            .any(|code| m.starts_with(&format!("postgres {code}"))),
        _ => false,
    }
}

fn pause(wait: Duration, stop: &AtomicBool) {
    let until = Instant::now() + wait;
    while Instant::now() < until && !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50).min(until - Instant::now()));
    }
}

fn write(db: &RwLock<Database>) -> RwLockWriteGuard<'_, Database> {
    db.write().unwrap_or_else(PoisonError::into_inner)
}

/// Puts what was applied on disk, the fsync outside the lock -- as the HTTP
/// endpoint does under `--sync always` -- so readers do not wait on it.
fn durable(db: &RwLock<Database>) -> Result<()> {
    let pending = write(db).flush()?;
    if let Some(sync) = pending {
        sync().inspect_err(|e| write(db).fail(e))?;
    }
    Ok(())
}

/// Where a stream stands.
#[derive(Default)]
struct Progress {
    /// The end of the last transaction applied.
    applied: u64,
    /// What the server was last told is safe.
    confirmed: u64,
    /// Since when applied changes have been waiting for disk.
    unsynced: Option<Instant>,
    transactions: u64,
    changes: u64,
}

/// Puts the applied changes on disk and tells the server they are safe.
fn settle(
    db: &RwLock<Database>,
    wal: &mut WalStream,
    p: &mut Progress,
    report: &mut dyn FnMut(Event),
) -> std::result::Result<(), Failure> {
    durable(db).map_err(Failure::Apply)?;
    wal.confirm(p.applied).map_err(Failure::Stream)?;
    p.confirmed = p.applied;
    p.unsynced = None;
    report(Event::Confirmed {
        lsn: p.applied,
        transactions: p.transactions,
        changes: p.changes,
    });
    Ok(())
}

/// One stream, from opening it until it breaks or `stop` is set.
fn stream(
    source: &Source,
    db: &RwLock<Database>,
    opts: &Options,
    follow: &Follow,
    stop: &AtomicBool,
    backoff: &mut Duration,
    report: &mut dyn FnMut(Event),
) -> std::result::Result<(), Failure> {
    let client = Client::connect_replication(&source.url).map_err(Failure::Stream)?;
    let mut wal = client
        .start_replication(&follow.slot, &follow.publication)
        .map_err(Failure::Stream)?;
    *backoff = FIRST_BACKOFF;
    report(Event::Streaming);

    let mut mirror = Mirror::new(source, opts);
    let mut p = Progress::default();
    let mut in_transaction = false;
    loop {
        if stop.load(Ordering::SeqCst) && !in_transaction {
            if p.unsynced.is_some() {
                settle(db, &mut wal, &mut p, report)?;
            }
            return Ok(());
        }
        // With changes waiting for disk the wait is a glance: an idle
        // socket means the burst is over and the fsync can run now.
        let wait = if p.unsynced.is_some() {
            Duration::from_millis(1)
        } else {
            IDLE_WAIT
        };
        match wal.next(wait).map_err(Failure::Stream)? {
            None => {
                if p.unsynced.is_some() && !in_transaction {
                    settle(db, &mut wal, &mut p, report)?;
                }
            }
            Some(Wal::Keepalive { end, reply }) => {
                // Nothing of ours is in flight: every change up to the
                // server's end has been sent and applied. Confirming it lets
                // the slot move on while the table is quiet, or it would hold
                // the server's log for every other table's writes.
                if !in_transaction && p.unsynced.is_none() && end > p.confirmed {
                    wal.confirm(end).map_err(Failure::Stream)?;
                    p.confirmed = end;
                } else if reply {
                    wal.confirm(p.confirmed).map_err(Failure::Stream)?;
                }
            }
            Some(Wal::Data { body, .. }) => match decode(&body).map_err(Failure::Apply)? {
                Message::Begin => in_transaction = true,
                Message::Commit { end } => {
                    mirror.flush(db).map_err(Failure::Apply)?;
                    in_transaction = false;
                    p.applied = end;
                    p.transactions += 1;
                    p.unsynced.get_or_insert_with(Instant::now);
                }
                Message::Relation(r) => mirror.relation(r, report).map_err(Failure::Apply)?,
                Message::Skip => {}
                change => p.changes += mirror.change(change, db).map_err(Failure::Apply)?,
            },
        }
        // Under a steady stream the socket is never idle: bound how long
        // applied changes wait for their fsync.
        if p.unsynced.is_some_and(|t| t.elapsed() >= SYNC_EVERY) && !in_transaction {
            settle(db, &mut wal, &mut p, report)?;
        }
    }
}

// ----------------------------------------------------------------- pgoutput

/// A column of a `Relation` message.
#[derive(Debug, Clone, PartialEq)]
struct RelColumn {
    /// Part of the replica identity: sent with deletes and key changes.
    key: bool,
    name: String,
    oid: i32,
    typmod: i32,
}

#[derive(Debug, Clone, PartialEq)]
struct Relation {
    id: u32,
    namespace: String,
    name: String,
    /// `d` default (the primary key), `n` nothing, `f` full, `i` an index.
    identity: u8,
    columns: Vec<RelColumn>,
}

/// One column of a row as `pgoutput` sends it.
#[derive(Debug, Clone, PartialEq)]
enum Cell {
    Null,
    /// A TOASTed value the change did not touch: not sent at all. A vector
    /// of 768 floats is 3 KB, past the TOAST threshold -- so an update of
    /// any other column of such a row arrives without its vector.
    Unchanged,
    Text(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq)]
enum Message {
    Begin,
    Commit {
        end: u64,
    },
    Relation(Relation),
    Insert {
        rel: u32,
        new: Vec<Cell>,
    },
    /// `old` is the key when it changed, or the whole old row under
    /// `REPLICA IDENTITY FULL`.
    Update {
        rel: u32,
        old: Option<Vec<Cell>>,
        new: Vec<Cell>,
    },
    Delete {
        rel: u32,
        old: Vec<Cell>,
    },
    Truncate {
        rels: Vec<u32>,
    },
    /// Origin, type and message records: nothing for a follower.
    Skip,
}

struct Cursor<'a> {
    b: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let s = self
            .b
            .get(self.at..self.at + n)
            .ok_or_else(|| Error::Corrupt("pgoutput: a message ended early".into()))?;
        self.at += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }
    fn cstr(&mut self) -> Result<String> {
        let rest = self.b.get(self.at..).unwrap_or_default();
        let n = rest
            .iter()
            .position(|&c| c == 0)
            .ok_or_else(|| Error::Corrupt("pgoutput: an unterminated string".into()))?;
        let s = String::from_utf8_lossy(&rest[..n]).into_owned();
        self.at += n + 1;
        Ok(s)
    }
    fn tuple(&mut self) -> Result<Vec<Cell>> {
        let n = self.u16()? as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(match self.u8()? {
                b'n' => Cell::Null,
                b'u' => Cell::Unchanged,
                b't' => {
                    let len = self.u32()? as usize;
                    Cell::Text(self.take(len)?.to_vec())
                }
                other => {
                    return Err(Error::Corrupt(format!(
                        "pgoutput: a column sent as `{}`, protocol 1 has only text",
                        other as char
                    )))
                }
            });
        }
        Ok(out)
    }
}

/// Decodes one `pgoutput` message, protocol 1.
fn decode(body: &[u8]) -> Result<Message> {
    let mut c = Cursor { b: body, at: 0 };
    Ok(match c.u8()? {
        b'B' => Message::Begin,
        b'C' => {
            c.u8()?; // flags
            c.u64()?; // the commit's LSN
            let end = c.u64()?;
            Message::Commit { end }
        }
        b'R' => {
            let id = c.u32()?;
            let namespace = c.cstr()?;
            let name = c.cstr()?;
            let identity = c.u8()?;
            let n = c.u16()? as usize;
            let mut columns = Vec::with_capacity(n);
            for _ in 0..n {
                let key = c.u8()? & 1 == 1;
                let name = c.cstr()?;
                let oid = c.u32()? as i32;
                let typmod = c.u32()? as i32;
                columns.push(RelColumn {
                    key,
                    name,
                    oid,
                    typmod,
                });
            }
            Message::Relation(Relation {
                id,
                namespace,
                name,
                identity,
                columns,
            })
        }
        b'I' => {
            let rel = c.u32()?;
            expect(&mut c, b'N')?;
            Message::Insert {
                rel,
                new: c.tuple()?,
            }
        }
        b'U' => {
            let rel = c.u32()?;
            let mut old = None;
            let mut tag = c.u8()?;
            if tag == b'K' || tag == b'O' {
                old = Some(c.tuple()?);
                tag = c.u8()?;
            }
            if tag != b'N' {
                return Err(Error::Corrupt(format!(
                    "pgoutput: an update without its new row (`{}`)",
                    tag as char
                )));
            }
            Message::Update {
                rel,
                old,
                new: c.tuple()?,
            }
        }
        b'D' => {
            let rel = c.u32()?;
            let tag = c.u8()?;
            if tag != b'K' && tag != b'O' {
                return Err(Error::Corrupt(format!(
                    "pgoutput: a delete without its key (`{}`)",
                    tag as char
                )));
            }
            Message::Delete {
                rel,
                old: c.tuple()?,
            }
        }
        b'T' => {
            let n = c.u32()? as usize;
            c.u8()?; // cascade / restart identity
            let mut rels = Vec::with_capacity(n);
            for _ in 0..n {
                rels.push(c.u32()?);
            }
            Message::Truncate { rels }
        }
        b'O' | b'Y' | b'M' => Message::Skip,
        other => {
            return Err(Error::Corrupt(format!(
                "pgoutput: unknown message `{}`",
                other as char
            )))
        }
    })
}

fn expect(c: &mut Cursor, tag: u8) -> Result<()> {
    let got = c.u8()?;
    if got != tag {
        return Err(Error::Corrupt(format!(
            "pgoutput: expected `{}`, found `{}`",
            tag as char, got as char
        )));
    }
    Ok(())
}

// ------------------------------------------------------------------- mirror

/// A write to the collection, in the order the source made it.
#[derive(Debug, Clone, PartialEq)]
enum Op {
    Put(Vec<(String, Expr)>),
    Delete(DocId),
    Truncate,
}

/// Where each column of the source sits in this session's rows.
struct Layout {
    rel: u32,
    /// Source column -> position in the relation's tuples.
    at: Vec<usize>,
    kinds: Vec<Kind>,
}

/// Turns the stream's changes into writes to the collection.
struct Mirror<'a> {
    source: &'a Source,
    opts: &'a Options,
    types: Vec<Option<DataType>>,
    id_column: usize,
    layout: Option<Layout>,
    /// Columns already reported as ignored, so each is reported once.
    ignored: Vec<String>,
    ops: Vec<Op>,
    registry: Registry,
    /// A connection for reading back a row whose unchanged values the
    /// collection does not hold; opened on first need.
    reader: Option<Client>,
    rows: u64,
}

impl<'a> Mirror<'a> {
    fn new(source: &'a Source, opts: &'a Options) -> Mirror<'a> {
        let types = source
            .plan
            .targets
            .iter()
            .map(|t| match t {
                Target::Id => None,
                Target::Field(name) => source.plan.schema.field(name).map(|f| f.ty.clone()),
            })
            .collect();
        Mirror {
            source,
            opts,
            types,
            id_column: source.id_column(),
            layout: None,
            ignored: Vec::new(),
            ops: Vec::new(),
            registry: Registry::with_builtins(),
            reader: None,
            rows: 0,
        }
    }

    /// A table's shape, sent before its first change in each session and
    /// again when it changes. Columns are matched by name: one the collection
    /// has no field for is reported and left out, and one the collection
    /// needs that the table no longer has stops the follower.
    fn relation(&mut self, r: Relation, report: &mut dyn FnMut(Event)) -> Result<()> {
        if r.namespace != self.source.namespace || r.name != self.source.name {
            return Ok(()); // another table of the same publication
        }
        let mut at = Vec::with_capacity(self.source.columns.len());
        let mut kinds = Vec::with_capacity(self.source.columns.len());
        for col in &self.source.columns {
            let j = r
                .columns
                .iter()
                .position(|c| c.name == col.name)
                .ok_or_else(|| {
                    Error::Query(format!(
                        "column `{}` is gone from {}; the collection has a field for it -- \
                         copy afresh into a new file",
                        col.name, self.source.sql
                    ))
                })?;
            let desc = FieldDesc {
                name: col.name.clone(),
                oid: r.columns[j].oid,
                typmod: r.columns[j].typmod,
            };
            at.push(j);
            kinds.push(pg::map_oid(&desc, &self.source.oids).1);
        }
        let key = &r.columns[at[self.id_column]];
        if r.identity == b'n' || (r.identity != b'f' && !key.key) {
            return Err(Error::Query(format!(
                "a delete from {} would not carry `{}`, so the document could not be \
                 found: make it the primary key, or alter table ... replica identity full",
                self.source.sql, key.name
            )));
        }
        for c in &r.columns {
            let known = self.source.columns.iter().any(|s| s.name == c.name);
            if !known && !self.ignored.contains(&c.name) {
                self.ignored.push(c.name.clone());
                report(Event::Ignored(c.name.clone()));
            }
        }
        self.layout = Some(Layout {
            rel: r.id,
            at,
            kinds,
        });
        Ok(())
    }

    fn layout(&self, rel: u32) -> Option<&Layout> {
        self.layout.as_ref().filter(|l| l.rel == rel)
    }

    /// One insert, update, delete or truncate; `1` when it was this table's.
    fn change(&mut self, m: Message, db: &RwLock<Database>) -> Result<u64> {
        match m {
            Message::Insert { rel, new } if self.layout(rel).is_some() => {
                self.upsert(&new, db)?;
            }
            Message::Update { rel, old, new } if self.layout(rel).is_some() => {
                if let Some(old) = old {
                    let (before, after) = (self.id_of(&old)?, self.id_of(&new)?);
                    if before != after {
                        self.ops.push(Op::Delete(before));
                    }
                }
                self.upsert(&new, db)?;
            }
            Message::Delete { rel, old } if self.layout(rel).is_some() => {
                let id = self.id_of(&old)?;
                self.ops.push(Op::Delete(id));
            }
            Message::Truncate { rels }
                if self.layout.as_ref().is_some_and(|l| rels.contains(&l.rel)) =>
            {
                self.ops.push(Op::Truncate);
            }
            _ => return Ok(0),
        }
        if self.ops.len() >= self.opts.batch {
            self.flush(db)?;
        }
        Ok(1)
    }

    /// The document id a row carries.
    fn id_of(&self, cells: &[Cell]) -> Result<DocId> {
        let layout = self.layout.as_ref().ok_or_else(no_layout)?;
        let name = &self.source.columns[self.id_column].name;
        match cells.get(layout.at[self.id_column]) {
            Some(Cell::Text(raw)) => {
                match pg::parse_cell(raw, &layout.kinds[self.id_column], name)? {
                    Value::Int(n) if n > 0 => Ok(n as DocId),
                    other => Err(Error::Type(format!(
                        "`{name}` is {other:?}; a document id must be a positive integer"
                    ))),
                }
            }
            _ => Err(Error::Corrupt(format!(
                "a change to {} came without `{name}`",
                self.source.sql
            ))),
        }
    }

    /// A row as it now stands: written whole if it passes `--where`, deleted
    /// if it does not (an earlier version may have passed).
    fn upsert(&mut self, cells: &[Cell], db: &RwLock<Database>) -> Result<()> {
        let id = self.id_of(cells)?;
        let Some(values) = self.values(id, cells, db)? else {
            return Ok(()); // gone from the source too; its delete follows
        };
        self.rows += 1;
        let doc = load::document(&self.source.plan, &self.types, values, self.rows)?;
        let keep = match &self.opts.filter {
            None => true,
            Some(f) => {
                let ctx = EvalCtx {
                    params: &[],
                    registry: &self.registry,
                };
                load::keep(f, &doc, &ctx, self.rows)?
            }
        };
        self.ops
            .push(if keep { Op::Put(doc) } else { Op::Delete(id) });
        Ok(())
    }

    /// The row's values in source column order. A value the change left
    /// out -- TOASTed and untouched -- is the one the collection holds; when
    /// it holds none, the row is read back from the source.
    fn values(
        &mut self,
        id: DocId,
        cells: &[Cell],
        db: &RwLock<Database>,
    ) -> Result<Option<Vec<Value>>> {
        let layout = self.layout.as_ref().ok_or_else(no_layout)?;
        let unchanged = layout
            .at
            .iter()
            .any(|&j| matches!(cells.get(j), Some(Cell::Unchanged)));
        let current = if unchanged {
            self.current(id, db)?
        } else {
            None
        };
        let layout = self.layout.as_ref().ok_or_else(no_layout)?;
        let mut out = Vec::with_capacity(layout.at.len());
        for (i, &j) in layout.at.iter().enumerate() {
            let name = &self.source.columns[i].name;
            out.push(match cells.get(j) {
                Some(Cell::Text(raw)) => pg::parse_cell(raw, &layout.kinds[i], name)?,
                Some(Cell::Null) | None => Value::Null,
                Some(Cell::Unchanged) => {
                    let held = match &self.source.plan.targets[i] {
                        Target::Field(field) => current
                            .as_ref()
                            .and_then(|d| d.iter().find(|(k, _)| k == field)),
                        Target::Id => None,
                    };
                    match held {
                        Some((_, v)) => v.clone(),
                        None => return self.read_back(id),
                    }
                }
            });
        }
        Ok(Some(out))
    }

    /// The fields the collection holds for `id`, counting the writes not yet
    /// applied: the newest of those touching it decides, and only with none
    /// is the collection read. Applying them first cost the batching: in a
    /// table of 768-dimension vectors every row of an update arrives without
    /// its vector, each read then made its row a statement of its own, and a
    /// 10 000-row update drained at 5 900 rows/s -- this way at 17 100.
    fn current(&self, id: DocId, db: &RwLock<Database>) -> Result<Option<Vec<(String, Value)>>> {
        let is_id = |(k, e): &(String, Expr)| {
            k == "id" && matches!(e, Expr::Lit(Value::Int(n)) if *n as DocId == id)
        };
        for op in self.ops.iter().rev() {
            match op {
                Op::Put(doc) if doc.iter().any(is_id) => {
                    let fields = doc
                        .iter()
                        .filter_map(|(k, e)| match e {
                            Expr::Lit(v) => Some((k.clone(), v.clone())),
                            _ => None,
                        })
                        .collect();
                    return Ok(Some(fields));
                }
                Op::Delete(d) if *d == id => return Ok(None),
                Op::Truncate => return Ok(None),
                _ => {}
            }
        }
        let g = db.read().unwrap_or_else(PoisonError::into_inner);
        let c = g.collection(&self.opts.into)?;
        Ok(c.store.read(&c.schema, id)?.map(|d| d.fields))
    }

    /// The row read from the source table itself, for the rare change whose
    /// untouched values the collection does not hold -- a row `--where` kept
    /// out until now. `None` when the source no longer has it.
    fn read_back(&mut self, id: DocId) -> Result<Option<Vec<Value>>> {
        if self.reader.is_none() {
            self.reader = Some(Client::connect(&self.source.url)?);
        }
        let reader = self.reader.as_mut().ok_or_else(no_layout)?;
        let key = pg::quote_ident(&self.source.columns[self.id_column].name)?;
        let r = reader.query(&format!(
            "select * from {} where {key} = {id}",
            self.source.sql
        ))?;
        let Some(row) = r.rows.first() else {
            return Ok(None);
        };
        let mut out = Vec::with_capacity(self.source.columns.len());
        for (col, kind) in self.source.columns.iter().zip(&self.source.kinds) {
            let j = r
                .columns
                .iter()
                .position(|f| f.name == col.name)
                .ok_or_else(|| Error::NotFound(format!("column `{}`", col.name)))?;
            out.push(match &row[j] {
                Some(s) => pg::parse_cell(s.as_bytes(), kind, &col.name)?,
                None => Value::Null,
            });
        }
        Ok(Some(out))
    }

    /// Applies the pending writes in their order: runs of puts as one `put`,
    /// runs of deletes as one `del ... where id in [...]`.
    fn flush(&mut self, db: &RwLock<Database>) -> Result<()> {
        if self.ops.is_empty() {
            return Ok(());
        }
        let ops = std::mem::take(&mut self.ops);
        let collection = &self.opts.into;
        let mut g = write(db);
        let mut i = 0;
        while i < ops.len() {
            let stmt = match &ops[i] {
                Op::Put(_) => {
                    let mut docs = Vec::new();
                    while let Some(Op::Put(doc)) = ops.get(i) {
                        docs.push(doc.clone());
                        i += 1;
                    }
                    Statement::Put {
                        collection: collection.clone(),
                        docs,
                    }
                }
                Op::Delete(_) => {
                    let mut ids = Vec::new();
                    while let Some(Op::Delete(id)) = ops.get(i) {
                        ids.push(Expr::Lit(Value::Int(*id as i64)));
                        i += 1;
                    }
                    Statement::Delete {
                        collection: collection.clone(),
                        filter: Some(Expr::In(Box::new(Expr::Field("id".into())), ids)),
                    }
                }
                Op::Truncate => {
                    i += 1;
                    Statement::Delete {
                        collection: collection.clone(),
                        filter: None,
                    }
                }
            };
            g.execute(&stmt)?;
        }
        Ok(())
    }
}

fn no_layout() -> Error {
    Error::Corrupt("pgoutput: a change came before its table's description".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fenec_core::value::VecPrec;

    // ------------------------------------------------ building messages

    fn cstr(b: &mut Vec<u8>, s: &str) {
        b.extend_from_slice(s.as_bytes());
        b.push(0);
    }

    fn tuple(b: &mut Vec<u8>, cells: &[Option<&str>]) {
        b.extend_from_slice(&(cells.len() as u16).to_be_bytes());
        for c in cells {
            match c {
                None => b.push(b'n'),
                Some("\u{0}unchanged") => b.push(b'u'),
                Some(s) => {
                    b.push(b't');
                    b.extend_from_slice(&(s.len() as u32).to_be_bytes());
                    b.extend_from_slice(s.as_bytes());
                }
            }
        }
    }

    const U: Option<&str> = Some("\u{0}unchanged");

    fn relation(identity: u8, cols: &[(&str, i32, bool)]) -> Vec<u8> {
        let mut b = vec![b'R'];
        b.extend_from_slice(&16_384u32.to_be_bytes());
        cstr(&mut b, "public");
        cstr(&mut b, "docs");
        b.push(identity);
        b.extend_from_slice(&(cols.len() as u16).to_be_bytes());
        for (name, oid, key) in cols {
            b.push(*key as u8);
            cstr(&mut b, name);
            b.extend_from_slice(&(*oid as u32).to_be_bytes());
            b.extend_from_slice(&(-1i32 as u32).to_be_bytes());
        }
        b
    }

    fn insert(cells: &[Option<&str>]) -> Vec<u8> {
        let mut b = vec![b'I'];
        b.extend_from_slice(&16_384u32.to_be_bytes());
        b.push(b'N');
        tuple(&mut b, cells);
        b
    }

    fn update(old: Option<&[Option<&str>]>, new: &[Option<&str>]) -> Vec<u8> {
        let mut b = vec![b'U'];
        b.extend_from_slice(&16_384u32.to_be_bytes());
        if let Some(old) = old {
            b.push(b'K');
            tuple(&mut b, old);
        }
        b.push(b'N');
        tuple(&mut b, new);
        b
    }

    fn delete(old: &[Option<&str>]) -> Vec<u8> {
        let mut b = vec![b'D'];
        b.extend_from_slice(&16_384u32.to_be_bytes());
        b.push(b'K');
        tuple(&mut b, old);
        b
    }

    #[test]
    fn messages_decode() {
        let r = decode(&relation(b'd', &[("id", 20, true), ("title", 25, false)])).unwrap();
        let Message::Relation(r) = r else {
            panic!("{r:?}")
        };
        assert_eq!(
            (r.id, r.namespace.as_str(), r.name.as_str()),
            (16_384, "public", "docs")
        );
        assert_eq!(r.identity, b'd');
        assert!(r.columns[0].key);
        assert_eq!(r.columns[1].oid, 25);

        assert_eq!(
            decode(&insert(&[Some("7"), None])).unwrap(),
            Message::Insert {
                rel: 16_384,
                new: vec![Cell::Text(b"7".to_vec()), Cell::Null]
            }
        );
        assert_eq!(
            decode(&update(Some(&[Some("7"), None]), &[Some("8"), U])).unwrap(),
            Message::Update {
                rel: 16_384,
                old: Some(vec![Cell::Text(b"7".to_vec()), Cell::Null]),
                new: vec![Cell::Text(b"8".to_vec()), Cell::Unchanged]
            }
        );
        let mut commit = vec![b'C', 0];
        commit.extend_from_slice(&5u64.to_be_bytes());
        commit.extend_from_slice(&9u64.to_be_bytes());
        commit.extend_from_slice(&0i64.to_be_bytes());
        assert_eq!(decode(&commit).unwrap(), Message::Commit { end: 9 });
        let mut t = vec![b'T'];
        t.extend_from_slice(&2u32.to_be_bytes());
        t.push(0);
        t.extend_from_slice(&1u32.to_be_bytes());
        t.extend_from_slice(&16_384u32.to_be_bytes());
        assert_eq!(
            decode(&t).unwrap(),
            Message::Truncate {
                rels: vec![1, 16_384]
            }
        );
        // Cut anywhere, a message is an error and never a panic.
        let whole = update(Some(&[Some("7"), None]), &[Some("8"), Some("x")]);
        for n in 0..whole.len() {
            assert!(decode(&whole[..n]).is_err(), "{n}");
        }
    }

    // ------------------------------------------------ applying them

    /// A mirror of `docs (id int8, title text, embed vector(2), tags text[])`
    /// into an in-memory database, as a copy would have left it.
    fn fixture(filter: Option<&str>) -> (Source, Options, RwLock<Database>) {
        let oids = VectorOids {
            vector: Some(90_000),
            halfvec: None,
        };
        let desc = |name: &str, oid: i32, typmod: i32| FieldDesc {
            name: name.into(),
            oid,
            typmod,
        };
        let (columns, kinds): (Vec<Column>, Vec<Kind>) = [
            desc("id", 20, -1),
            desc("title", 25, -1),
            desc("embed", 90_000, 2),
            desc("tags", 1009, -1),
        ]
        .iter()
        .map(|f| pg::map_oid(f, &oids))
        .unzip();
        let mut opts = Options::new("docs");
        opts.filter = filter.map(fenec_ql_free_filter);
        let plan = map::plan(&columns, &opts).unwrap();
        let mut db = Database::new();
        db.execute(&Statement::CreateCollection {
            schema: plan.schema.clone(),
            if_not_exists: false,
        })
        .unwrap();
        let source = Source {
            url: Url::parse("postgres://u@127.0.0.1:1/d").unwrap(),
            table: "docs".into(),
            sql: "\"docs\"".into(),
            namespace: "public".into(),
            name: "docs".into(),
            columns,
            kinds,
            oids,
            plan,
            publication_created: false,
            slot_created: false,
        };
        (source, opts, RwLock::new(db))
    }

    /// `title ~ "keep"`, built by hand: this crate does not depend on the
    /// parser.
    fn fenec_ql_free_filter(word: &str) -> Expr {
        Expr::Like(
            Box::new(Expr::Field("title".into())),
            Box::new(Expr::Lit(Value::Text(word.into()))),
        )
    }

    const COLS: [(&str, i32, bool); 4] = [
        ("id", 20, true),
        ("title", 25, false),
        ("embed", 90_000, false),
        ("tags", 1009, false),
    ];

    fn apply(mirror: &mut Mirror, db: &RwLock<Database>, messages: &[Vec<u8>]) {
        let mut report = |_| {};
        for m in messages {
            match decode(m).unwrap() {
                Message::Relation(r) => mirror.relation(r, &mut report).unwrap(),
                other => {
                    mirror.change(other, db).unwrap();
                }
            }
        }
        mirror.flush(db).unwrap();
    }

    fn doc(db: &RwLock<Database>, id: DocId) -> Option<Document> {
        let g = db.read().unwrap();
        let c = g.collection("docs").unwrap();
        c.store.read(&c.schema, id).unwrap()
    }

    fn count(db: &RwLock<Database>) -> usize {
        db.read().unwrap().collection("docs").unwrap().store.len()
    }

    #[test]
    fn changes_become_writes_by_id() {
        let (source, opts, db) = fixture(None);
        let mut m = Mirror::new(&source, &opts);
        apply(
            &mut m,
            &db,
            &[
                relation(b'd', &COLS),
                insert(&[Some("1"), Some("one"), Some("[1,0]"), Some("{a,b}")]),
                insert(&[Some("2"), Some("two"), Some("[0,1]"), None]),
                insert(&[Some("3"), Some("three"), None, Some("{}")]),
                // The title changes; the vector is TOASTed and untouched.
                update(None, &[Some("1"), Some("uno"), U, Some("{a}")]),
                // The key changes: the old document goes, the new one comes.
                update(
                    Some(&[Some("2"), None, None, None]),
                    &[Some("20"), Some("two"), Some("[0,1]"), None],
                ),
                delete(&[Some("3"), None, None, None]),
            ],
        );
        let one = doc(&db, 1).unwrap();
        assert_eq!(one.get("title"), Some(&Value::Text("uno".into())));
        assert_eq!(one.get("embed"), Some(&Value::Vector(vec![1.0, 0.0])));
        assert_eq!(
            one.get("tags"),
            Some(&Value::List(vec![Value::Text("a".into())]))
        );
        assert!(doc(&db, 2).is_none() && doc(&db, 3).is_none());
        assert_eq!(
            doc(&db, 20).unwrap().get("title"),
            Some(&Value::Text("two".into()))
        );

        // Everything again, as after a reconnect: the same rows.
        let before: Vec<_> = [1, 20].iter().map(|&id| doc(&db, id)).collect();
        let mut m = Mirror::new(&source, &opts);
        apply(
            &mut m,
            &db,
            &[
                relation(b'd', &COLS),
                insert(&[Some("1"), Some("one"), Some("[1,0]"), Some("{a,b}")]),
                insert(&[Some("2"), Some("two"), Some("[0,1]"), None]),
                insert(&[Some("3"), Some("three"), None, Some("{}")]),
                update(None, &[Some("1"), Some("uno"), U, Some("{a}")]),
                update(
                    Some(&[Some("2"), None, None, None]),
                    &[Some("20"), Some("two"), Some("[0,1]"), None],
                ),
                delete(&[Some("3"), None, None, None]),
            ],
        );
        let after: Vec<_> = [1, 20].iter().map(|&id| doc(&db, id)).collect();
        assert_eq!(before, after);
        assert_eq!(count(&db), 2);

        // A truncate empties the collection.
        let mut t = vec![b'T'];
        t.extend_from_slice(&1u32.to_be_bytes());
        t.push(0);
        t.extend_from_slice(&16_384u32.to_be_bytes());
        apply(&mut m, &db, &[t]);
        assert_eq!(count(&db), 0);
    }

    #[test]
    fn a_row_leaving_the_filter_is_deleted() {
        let (source, opts, db) = fixture(Some("keep"));
        let mut m = Mirror::new(&source, &opts);
        apply(
            &mut m,
            &db,
            &[
                relation(b'd', &COLS),
                insert(&[Some("1"), Some("keep me"), None, None]),
                insert(&[Some("2"), Some("drop me"), None, None]),
            ],
        );
        assert!(doc(&db, 1).is_some() && doc(&db, 2).is_none());
        apply(
            &mut m,
            &db,
            &[update(None, &[Some("1"), Some("now dropped"), None, None])],
        );
        assert!(doc(&db, 1).is_none());
    }

    #[test]
    fn a_table_that_cannot_be_followed_says_why() {
        let (source, opts, _db) = fixture(None);
        let mut report = |_| {};
        // No replica identity: deletes would carry no key.
        let mut m = Mirror::new(&source, &opts);
        let Message::Relation(r) = decode(&relation(b'n', &COLS)).unwrap() else {
            unreachable!()
        };
        let e = m.relation(r, &mut report).unwrap_err().to_string();
        assert!(e.contains("replica identity"), "{e}");
        // A column the collection needs is gone.
        let Message::Relation(r) = decode(&relation(b'd', &COLS[..3])).unwrap() else {
            unreachable!()
        };
        let e = m.relation(r, &mut report).unwrap_err().to_string();
        assert!(e.contains("tags"), "{e}");
        // A column added after the copy is reported, once, and left out.
        let mut seen = Vec::new();
        let mut cols = COLS.to_vec();
        cols.push(("extra", 25, false));
        for _ in 0..2 {
            let Message::Relation(r) = decode(&relation(b'd', &cols)).unwrap() else {
                unreachable!()
            };
            m.relation(r, &mut |e| seen.push(format!("{e:?}"))).unwrap();
        }
        assert_eq!(seen.len(), 1, "{seen:?}");
        let _ = VecPrec::F32;
    }

    #[test]
    fn slots_are_named_after_the_collection() {
        assert_eq!(Follow::named_after("articles").slot, "fenec_articles");
        assert_eq!(Follow::named_after("Ürün-2").slot, "fenec__r_n_2");
        assert!(client::plain_name(
            &Follow::named_after(&"x".repeat(80)).slot
        ));
    }
}
