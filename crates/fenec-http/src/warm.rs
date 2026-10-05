//! `fenec-server --warm`: the hash, text, ordered and sparse indexes built
//! after the open, beside the queries, rather than by the first read of
//! each.
//!
//! An open builds none of them (`Derived`): the first statement that reads
//! one fills it from the documents, and pays for it. A shop's first search
//! page after a start took 2.68 s to its largest paint against 1.40 s
//! warm, the `@text` index of its products built inside that request. The
//! port still opens in the time the documents take to read; then one
//! thread builds each index the server was told to warm, one at a time,
//! each under the read lock, which reads take beside it -- a reader that
//! needs the index being built waits for that build rather than starting
//! its own, and a writer waits out one index at most.

use fenec_core::prelude::Database;
use std::sync::{Arc, RwLock};
use std::time::Instant;

/// What to warm: every derived index (`--warm`), or those named
/// (`--warm products,orders.status`).
#[derive(Clone, Debug, Default)]
pub struct Warm {
    /// Collections and `collection.field`s; empty: every one.
    pub only: Vec<String>,
}

impl Warm {
    /// `--warm`'s value: `all`, or a list of collections and
    /// `collection.field`s separated by commas.
    pub fn parse(v: &str) -> Result<Warm, String> {
        if v == "all" {
            return Ok(Warm::default());
        }
        let only: Vec<String> = v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if only.is_empty() {
            return Err(
                "--warm takes `all` or collections and collection.fields, by commas".into(),
            );
        }
        Ok(Warm { only })
    }
}

/// Builds what `warm` names of `db` on a thread of its own, logging what
/// it built and how long it took. The thread holds the database only while
/// it builds an index, so a tenant closed meanwhile is let go of.
pub fn start(what: &str, db: &Arc<RwLock<Database>>, warm: &Warm) {
    let (what, db, only) = (what.to_string(), Arc::downgrade(db), warm.only.clone());
    let started = std::thread::Builder::new()
        .name("fenec-warm".into())
        .spawn(move || {
            let t = Instant::now();
            let todo = match db.upgrade() {
                Some(db) => crate::held::read(&db).unbuilt_indexes(&only),
                None => return,
            };
            let mut built = 0;
            for (c, f) in &todo {
                let Some(db) = db.upgrade() else { return };
                // One index under each hold of the lock, so a writer waits
                // out one build and not all of them.
                let r = crate::held::read(&db).warm_index(c, f);
                if let Err(e) = r {
                    crate::log!("warm ({what}): {c}.{f}: {e}");
                } else {
                    built += 1;
                }
            }
            if !todo.is_empty() {
                crate::log!(
                    "warm ({what}): {built} index(es) built in {} ms",
                    t.elapsed().as_millis()
                );
            }
        });
    if let Err(e) = started {
        crate::log!("could not start warming the indexes: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_names_collections_and_fields() {
        assert!(Warm::parse("all").unwrap().only.is_empty());
        assert_eq!(Warm::parse("a, b.c").unwrap().only, ["a", "b.c"]);
        assert!(Warm::parse(",").is_err());
    }

    /// The thread builds every index while the database is shared.
    #[test]
    fn the_thread_builds_what_it_was_told() {
        let mut db = Database::new();
        for s in fenec_ql::parse(
            r#"create collection t (a text @hash, b text @text);
               put t [{a: "x", b: "one"}, {a: "y", b: "two"}]"#,
        )
        .unwrap()
        {
            db.execute(&s).unwrap();
        }
        let mut back = Database::new();
        back.load(&db.snapshot()).unwrap();
        let shared = Arc::new(RwLock::new(back));
        assert_eq!(crate::held::read(&shared).unbuilt_indexes(&[]).len(), 2);
        start("test", &shared, &Warm::default());
        let t = Instant::now();
        while !crate::held::read(&shared).unbuilt_indexes(&[]).is_empty() {
            assert!(t.elapsed().as_secs() < 10, "not warmed");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}
