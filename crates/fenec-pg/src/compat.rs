//! Compatibility layer for the queries PostgreSQL clients send by assumption.
//!
//! psql, psycopg and JDBC send queries like `SELECT version()`, `SET ...`
//! and `SHOW ...` the moment the connection is up. These are not FenecQL; they
//! are intercepted here and answered with fixed responses so the clients
//! open cleanly.

use crate::server::Config;

pub enum Shim {
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
        tag: String,
    },
    Tag(String),
    /// Transaction control. The answer depends on what the session did since
    /// `BEGIN`, which this layer cannot see, so the session decides.
    Tx(Tx),
    /// A command whose honest answer is "no": accepting it would tell the
    /// client something happened that did not.
    Refuse {
        code: &'static str,
        message: String,
    },
    /// A query over the catalog: the session runs it over one made from
    /// the database's schemas ([`crate::catalog`]).
    Catalog,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tx {
    Begin,
    Commit,
    Rollback,
}

fn one(col: &str, val: &str) -> Shim {
    Shim::Rows {
        columns: vec![col.to_string()],
        rows: vec![vec![val.to_string()]],
        tag: "SELECT".into(),
    }
}

/// FenecQL commands come before the compatibility layer.
///
/// `set` and `update` exist in both languages: a parameter assignment in
/// PostgreSQL, a document update in FenecQL. What tells them apart is that
/// FenecQL always carries a `{...}` body. Without this check `set products
/// {price: 999}` would be swallowed silently and nothing would be updated.
fn is_fenecql(lower: &str) -> bool {
    let mut words = lower.split_whitespace();
    let first = words.next().unwrap_or("");
    let second = words.next().unwrap_or("");
    match first {
        "get" | "put" | "del" | "collections" | "describe" | "compact" => true,
        "create" | "drop" => second == "collection",
        "set" | "update" | "insert" | "delete" => lower.contains('{'),
        _ => false,
    }
}

/// `standby` says whether the database is a replica. It is asked only by the
/// queries that are about it, since it takes the database's read lock.
pub fn handle(sql: &str, cfg: &Config, standby: &dyn Fn() -> bool) -> Option<Shim> {
    let q = sql.trim().trim_end_matches(';').trim();
    let lower = q.to_ascii_lowercase();
    if is_fenecql(&lower) {
        return None;
    }
    let first = lower.split_whitespace().next().unwrap_or("");

    match first {
        // Transaction control: there are no transactions, every statement is
        // applied as it runs. `BEGIN` and `COMMIT` are still accepted so
        // drivers can open, but `ROLLBACK` is answered by the session: it
        // succeeds only when there is nothing it would have had to undo.
        "begin" | "start" => return Some(Shim::Tx(Tx::Begin)),
        "commit" | "end" => return Some(Shim::Tx(Tx::Commit)),
        "rollback" | "abort" => return Some(Shim::Tx(Tx::Rollback)),
        "set" => return Some(Shim::Tag("SET".into())),
        "discard" => return Some(Shim::Tag("DISCARD ALL".into())),
        // `UNLISTEN` stays a no-op because it is true: nothing is listening.
        // `LISTEN` would leave a client waiting for notifications that never
        // come, and `NOTIFY` would tell the sender it reached someone.
        "unlisten" => return Some(Shim::Tag("UNLISTEN".into())),
        "listen" | "notify" => {
            return Some(Shim::Refuse {
                code: "0A000",
                message: format!(
                    "{} is not supported: fenec-pg sends no notifications; \
                     subscribe to changes over HTTP with GET /<collection>/changes",
                    first.to_uppercase()
                ),
            })
        }
        "show" => {
            let name = lower.strip_prefix("show").unwrap_or("").trim();
            let val = match name {
                "server_version" => cfg.server_version.clone(),
                "transaction_isolation" | "transaction isolation level" => "read committed".into(),
                "standard_conforming_strings" => "on".into(),
                "client_encoding" | "server_encoding" => "UTF8".into(),
                "search_path" => "public".into(),
                // How libpq's `target_session_attrs=read-write` and JDBC's
                // `targetServerType` tell a primary from a standby: a replica
                // saying "off" would be sent the writes it refuses.
                "transaction_read_only"
                | "default_transaction_read_only"
                | "in_hot_standby"
                | "transaction read only" => if standby() { "on" } else { "off" }.into(),
                _ => String::new(),
            };
            return Some(one(name, &val));
        }
        _ => {}
    }

    if lower.starts_with("select version()") || lower == "select version" {
        return Some(one(
            "version",
            &format!(
                "PostgreSQL {} on {}, fenecdb query language: FenecQL",
                cfg.server_version,
                std::env::consts::ARCH
            ),
        ));
    }
    if lower.starts_with("select current_schema") {
        return Some(one("current_schema", "public"));
    }
    if lower.starts_with("select current_database") {
        return Some(one("current_database", "fenec"));
    }
    if lower.starts_with("select current_user") || lower.starts_with("select user") {
        return Some(one("current_user", "fenec"));
    }
    if lower.starts_with("select 1") && !lower.contains("from") {
        return Some(one("?column?", "1"));
    }
    // libpq's `target_session_attrs=primary|standby` asks this, catalog
    // prefix and all, before the catalog rule below would answer it empty.
    if lower.starts_with("select pg_is_in_recovery()")
        || lower.starts_with("select pg_catalog.pg_is_in_recovery()")
    {
        return Some(one("pg_is_in_recovery", if standby() { "t" } else { "f" }));
    }

    // Catalog discovery: run over a catalog made from the schemas. What that
    // cannot read is answered empty, as every catalog query once was, so a
    // client does not stall while opening.
    if crate::catalog::is_catalog(&lower)
        || lower.contains("pg_catalog.")
        || lower.contains("information_schema.")
    {
        return Some(Shim::Catalog);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn as_primary(q: &str, cfg: &Config) -> Option<Shim> {
        handle(q, cfg, &|| false)
    }

    #[test]
    fn a_standby_says_so() {
        let cfg = Config::default();
        let value = |q: &str, standby: bool| match handle(q, &cfg, &move || standby) {
            Some(Shim::Rows { rows, .. }) => rows[0][0].clone(),
            _ => panic!("`{q}` unanswered"),
        };
        for q in ["SHOW transaction_read_only", "show in_hot_standby"] {
            assert_eq!(value(q, true), "on");
            assert_eq!(value(q, false), "off");
        }
        assert_eq!(value("SELECT pg_catalog.pg_is_in_recovery()", true), "t");
        assert_eq!(value("select pg_is_in_recovery()", false), "f");
    }

    #[test]
    fn startup_queries_are_answered() {
        let cfg = Config::default();
        assert!(as_primary("BEGIN", &cfg).is_some());
        assert!(as_primary("SET client_encoding TO 'UTF8'", &cfg).is_some());
        assert!(as_primary("SHOW server_version", &cfg).is_some());
        assert!(as_primary("select version()", &cfg).is_some());
        assert!(as_primary("SELECT c.oid FROM pg_catalog.pg_class c", &cfg).is_some());
    }

    /// Transaction control is handed to the session; a notification command
    /// that could not do what it says is refused rather than acknowledged.
    #[test]
    fn transaction_control_and_notifications() {
        let cfg = Config::default();
        let tx = |q: &str| match handle(q, &cfg, &|| false) {
            Some(Shim::Tx(t)) => Some(t),
            _ => None,
        };
        assert_eq!(tx("BEGIN"), Some(Tx::Begin));
        assert_eq!(tx("start transaction"), Some(Tx::Begin));
        assert_eq!(tx("COMMIT;"), Some(Tx::Commit));
        assert_eq!(tx("end"), Some(Tx::Commit));
        assert_eq!(tx("ROLLBACK"), Some(Tx::Rollback));
        assert_eq!(tx("abort"), Some(Tx::Rollback));

        for q in ["LISTEN jobs", "notify jobs, 'x'"] {
            assert!(
                matches!(
                    handle(q, &cfg, &|| false),
                    Some(Shim::Refuse { code: "0A000", .. })
                ),
                "`{q}` must be refused"
            );
        }
        assert!(matches!(as_primary("UNLISTEN *", &cfg), Some(Shim::Tag(_))));
    }

    #[test]
    fn fenecql_passes_through() {
        let cfg = Config::default();
        assert!(as_primary("get docs limit 1", &cfg).is_none());
        assert!(as_primary("put docs {a: 1}", &cfg).is_none());
        assert!(as_primary("create collection t (a int)", &cfg).is_none());
        assert!(as_primary("drop collection t", &cfg).is_none());
        assert!(as_primary("del docs where a = 1", &cfg).is_none());
        assert!(as_primary("compact", &cfg).is_none());
    }

    #[test]
    fn set_is_disambiguated() {
        let cfg = Config::default();
        // A FenecQL update: it has a body
        assert!(as_primary("set docs {year: 2026} where id = 1", &cfg).is_none());
        assert!(as_primary("update docs {year: 2026}", &cfg).is_none());
        // A PostgreSQL parameter assignment: no body
        assert!(matches!(
            as_primary("SET client_encoding TO 'UTF8'", &cfg),
            Some(Shim::Tag(_))
        ));
        assert!(matches!(
            as_primary("set search_path = public", &cfg),
            Some(Shim::Tag(_))
        ));
    }
}
