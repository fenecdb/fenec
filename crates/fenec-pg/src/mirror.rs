//! `fenec-pg --follow`: a PostgreSQL table mirrored into the file this
//! server serves, over the pg wire, HTTP and its subscriptions at once.
//!
//! `fenec import --follow` is the single writer of the file it keeps, so
//! serving its mirror took a second process over a copy of it: two
//! processes over one file corrupt it. Here the importer's follower
//! (`fenec_import::follow`) runs on a thread of the server's, over the
//! database the server serves -- the copy when there is no whole one, then
//! the changes as they commit -- and its writes reach a subscriber as any
//! write does, a primary's replicas too.
//!
//! The mirrored collection is the follower's to write: a write from
//! anyone else is refused ([`Guard`]), since the next change from
//! PostgreSQL would overwrite it, or a copy made again forget it. What the
//! follower confirms to the server is on disk first, as ever, and it stops
//! with the server, before the checkpoint.

use fenec_core::error::{Error, Result};
use fenec_core::plugin::{Hook, Plugin, Registry, WriteOp};
use fenec_core::prelude::*;
use fenec_import::follow::{self, lsn_text, Event, Follow};
use fenec_import::pg::Url;
use fenec_import::Options;
use std::cell::Cell;
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

thread_local! {
    /// Whether this thread is the follower's, the one that may write the
    /// mirrored collection. The follower does all of its writing on the
    /// thread [`start`] gives it.
    static FOLLOWER: Cell<bool> = const { Cell::new(false) };
}

/// How long the shutdown waits for the follower to put its last changes on
/// disk and confirm them: past it, a copy still being made is left for the
/// next start to make again, as a copy cut short always is.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// What `--follow` was given.
pub struct Mirror {
    pub url: Url,
    pub table: String,
    pub opts: Options,
    pub follow: Follow,
}

/// Refuses a write to the mirrored collection from anything but the
/// follower's thread.
struct Guard {
    collection: String,
}

impl Guard {
    fn check(&self, collection: &str) -> Result<()> {
        if collection == self.collection && !FOLLOWER.with(Cell::get) {
            return Err(Error::ReadOnly(format!(
                "`{collection}` mirrors a PostgreSQL table: its writes come from there"
            )));
        }
        Ok(())
    }
}

impl Hook for Guard {
    fn name(&self) -> &str {
        "mirror"
    }
    fn before_write(&self, schema: &Schema, _op: WriteOp, _doc: &mut Document) -> Result<()> {
        self.check(&schema.name)
    }
    /// A delete reaches the hooks only here, before it is made.
    fn after_write(&self, collection: &str, op: WriteOp, _doc: &Document) -> Result<()> {
        match op {
            WriteOp::Delete => self.check(collection),
            _ => Ok(()),
        }
    }
}

/// Installs [`Guard`] over `collection`.
pub struct GuardPlugin(pub String);

impl Plugin for GuardPlugin {
    fn name(&self) -> &str {
        "mirror"
    }
    fn init(&self, reg: &mut Registry) -> Result<()> {
        reg.register_hook(Arc::new(Guard {
            collection: self.0.clone(),
        }));
        Ok(())
    }
}

/// Starts the follower on a thread of its own over `db`, stopping when the
/// server does. An error it cannot wait out ends the process: a server
/// that went on answering from a mirror that no longer moves would say
/// nothing of it.
pub fn start(m: Mirror, db: Arc<RwLock<Database>>) -> std::io::Result<()> {
    let (done, stopped) = mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("fenec-follow".into())
        .spawn(move || {
            FOLLOWER.with(|f| f.set(true));
            let stop = crate::server::shutdown_flag();
            let mut report = reporter(&m);
            match follow::run(&m.url, &m.table, &db, &m.opts, &m.follow, stop, &mut report) {
                Ok(()) => {
                    let _ = done.send(());
                }
                Err(e) => {
                    fenec_http::log!("follow: the mirror of {} stopped: {e}", m.table);
                    std::process::exit(1);
                }
            }
        })?;
    crate::server::before_shutdown(move || {
        let _ = stopped.recv_timeout(STOP_WAIT);
    });
    Ok(())
}

/// What the follower reports, as log lines: its progress now and then
/// rather than a line a transaction.
fn reporter(m: &Mirror) -> impl FnMut(Event) {
    let (table, slot, publication) = (
        m.table.clone(),
        m.follow.slot.clone(),
        m.follow.publication.clone(),
    );
    let mut last = Instant::now() - Duration::from_secs(60);
    move |e: Event| {
        match e {
        Event::Prepared {
            publication_created,
            slot_created,
        } => {
            let made = |yes: bool| if yes { " (created)" } else { "" };
            fenec_http::log!(
                "follow: {table}: slot {slot}{}, publication {publication}{}",
                made(slot_created),
                made(publication_created)
            );
            if slot_created {
                fenec_http::log!(
                    "follow: the slot keeps the server's WAL until this server confirms it; \
                     to stop following for good: select pg_drop_replication_slot('{slot}')"
                );
            }
        }
        Event::Copying(why) => fenec_http::log!("follow: copying {table}: {why}"),
        Event::CopyProgress(n) => {
            if last.elapsed() >= Duration::from_secs(10) {
                fenec_http::log!("follow: {n} rows of {table} copied");
                last = Instant::now();
            }
        }
        Event::Copied(s) => {
            fenec_http::log!(
                "follow: {table} copied, {} rows in {:.2?}",
                s.rows,
                s.elapsed
            );
            for w in &s.warnings {
                fenec_http::log!("follow: {w}");
            }
        }
        Event::Streaming => fenec_http::log!("follow: {table}: streaming its changes"),
        Event::Confirmed {
            lsn,
            transactions,
            changes,
        } => {
            if last.elapsed() >= Duration::from_secs(60) {
                fenec_http::log!(
                    "follow: {table}: {transactions} transactions, {changes} changes, at {}",
                    lsn_text(lsn)
                );
                last = Instant::now();
            }
        }
        Event::Ignored(c) => fenec_http::log!(
            "follow: column `{c}` of {table} has no field in the collection; its values are not kept"
        ),
        Event::Reconnecting { error, wait } => {
            fenec_http::log!("follow: {table}: the stream broke ({error}); again in {wait:.1?}")
        }
    }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(db: &mut Database, sql: &str) -> Result<Response> {
        db.execute(&fenec_ql::parse_one(sql).expect("parse"))
    }

    /// The mirrored collection takes the follower's writes and deletes and
    /// refuses anyone else's, as a replica refuses one (25006); the rest of
    /// the file is written as ever.
    #[test]
    fn the_mirror_takes_the_followers_writes_alone() {
        let mut db = Database::new();
        db.install_plugin(&GuardPlugin("docs".into())).unwrap();
        run(&mut db, "create collection docs (n int)").unwrap();
        run(&mut db, "create collection notes (n int)").unwrap();
        let db = RwLock::new(db);

        let refused = |r: Result<Response>| match r {
            Err(Error::ReadOnly(m)) => assert!(m.contains("mirrors a PostgreSQL table"), "{m}"),
            other => panic!("not refused: {other:?}"),
        };
        refused(run(&mut db.write().unwrap(), "put docs {n: 1}"));

        std::thread::scope(|s| {
            s.spawn(|| {
                FOLLOWER.with(|f| f.set(true));
                let mut g = db.write().unwrap();
                run(&mut g, "put docs {n: 1}").unwrap();
                run(&mut g, "put docs {n: 2}").unwrap();
                run(&mut g, "set docs {n: 3} where n = 2").unwrap();
            })
            .join()
            .unwrap();
        });
        let mut g = db.write().unwrap();
        refused(run(&mut g, "set docs {n: 9} where n = 1"));
        refused(run(&mut g, "del docs where n = 1"));
        run(&mut g, "put notes {n: 1}").unwrap();
        run(&mut g, "del notes where n = 1").unwrap();
        let r = g
            .query(&fenec_ql::parse_one("get docs count").unwrap(), &[])
            .unwrap();
        assert_eq!(r.rows().unwrap().rows[0].values[0], Value::Int(2));
    }
}
