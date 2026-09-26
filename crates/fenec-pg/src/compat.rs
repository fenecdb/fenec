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
    /// Transaction control, which the session carries out: the transaction
    /// is its own.
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

#[derive(Debug, Clone, PartialEq)]
pub enum Tx {
    Begin(Change),
    /// `AND CHAIN` begins the next transaction, in the same mode, as this
    /// one ends.
    Commit {
        chain: bool,
    },
    Rollback {
        chain: bool,
    },
    /// `SET TRANSACTION`: the open transaction's mode.
    Set(Change),
    /// `SET SESSION CHARACTERISTICS AS TRANSACTION`: the mode of every
    /// transaction after it.
    Default(Change),
    /// `SAVEPOINT name`.
    Savepoint(String),
    /// `ROLLBACK TO [SAVEPOINT] name`: the writes after it put back, the
    /// transaction going on.
    RollbackTo(String),
    /// `RELEASE [SAVEPOINT] name`: it and the savepoints after it
    /// forgotten, their writes kept.
    Release(String),
}

/// How a transaction runs.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Mode {
    /// `SERIALIZABLE` or `REPEATABLE READ`: the transaction runs alone from
    /// its first statement rather than from its first write, so every read
    /// in it is of the database as it leaves it.
    pub serial: bool,
    /// `READ ONLY`: a write in it is refused.
    pub read_only: bool,
}

/// What a statement says of a transaction's mode. What it leaves unsaid
/// stays as it was.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Change {
    pub serial: Option<bool>,
    pub read_only: Option<bool>,
}

impl Change {
    fn of(words: &[&str]) -> Change {
        let pair = |a: &str, b: &str| words.windows(2).any(|w| w[0] == a && w[1] == b);
        Change {
            serial: if words.contains(&"serializable") || pair("repeatable", "read") {
                Some(true)
            } else if pair("read", "committed") || pair("read", "uncommitted") {
                Some(false)
            } else {
                None
            },
            read_only: if pair("read", "only") {
                Some(true)
            } else if pair("read", "write") {
                Some(false)
            } else {
                None
            },
        }
    }

    pub fn over(self, m: Mode) -> Mode {
        Mode {
            serial: self.serial.unwrap_or(m.serial),
            read_only: self.read_only.unwrap_or(m.read_only),
        }
    }
}

/// The words of `q` as PostgreSQL reads identifiers: folded to lower case,
/// or as written between double quotes, `""` a quote inside them. `None`
/// for a quote left open.
fn identifiers(q: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut chars = q.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&first) = chars.peek() else {
            return Some(out);
        };
        let mut word = String::new();
        if first == '"' {
            chars.next();
            loop {
                match chars.next()? {
                    '"' if chars.next_if_eq(&'"').is_some() => word.push('"'),
                    '"' => break,
                    c => word.push(c),
                }
            }
        } else {
            while let Some(c) = chars.next_if(|c| !c.is_whitespace() && *c != '"') {
                word.push(c.to_ascii_lowercase());
            }
        }
        out.push(word);
    }
}

/// `SAVEPOINT`, `RELEASE` and `ROLLBACK TO`, with the name they give --
/// psycopg quotes it (`SAVEPOINT "_pg3_1"`), SQLAlchemy does not.
fn savepoint(q: &str) -> Shim {
    let words = identifiers(q).unwrap_or_default();
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let tx = match words.as_slice() {
        ["savepoint", name] => Tx::Savepoint(name.to_string()),
        ["release", "savepoint", name] | ["release", name] => Tx::Release(name.to_string()),
        ["rollback", "to", rest @ ..] | ["rollback", "work" | "transaction", "to", rest @ ..] => {
            match rest {
                ["savepoint", name] | [name] => Tx::RollbackTo(name.to_string()),
                _ => return syntax(q),
            }
        }
        _ => return syntax(q),
    };
    Shim::Tx(tx)
}

fn syntax(q: &str) -> Shim {
    Shim::Refuse {
        code: "42601",
        message: format!("syntax error in `{q}`: a savepoint takes one name"),
    }
}

fn no_two_phase() -> Shim {
    Shim::Refuse {
        code: "0A000",
        message: "two-phase commit is not supported".into(),
    }
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

/// The statements of a query text, split where a `;` stands outside a
/// string, a quoted name and a comment -- `--`, `#` as FenecQL writes one,
/// or `/* */` -- as PostgreSQL splits a simple query's. The empty ones are
/// left out. A backslash escapes the character after it inside quotes, as
/// FenecQL reads a string.
pub fn statements(text: &str) -> Vec<&str> {
    fn end<'a>(text: &'a str, from: usize, to: usize, out: &mut Vec<&'a str>) {
        let s = text[from..to].trim();
        if !s.is_empty() {
            out.push(s);
        }
    }
    let b = text.as_bytes();
    let mut out = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1 + (b[i] == b'\\') as usize;
                }
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 1;
            }
            b';' => {
                end(text, start, i, &mut out);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    end(text, start.min(b.len()), b.len(), &mut out);
    out
}

/// `standby` says whether the database is a replica. It is asked only by the
/// queries that are about it, since it takes the database's read lock.
pub fn handle(sql: &str, cfg: &Config, standby: &dyn Fn() -> bool) -> Option<Shim> {
    let q = sql.trim().trim_end_matches(';').trim();
    let lower = q.to_ascii_lowercase();
    if is_fenecql(&lower) {
        return None;
    }
    // A text of several statements the first of which is answered here --
    // which the simple query protocol splits before it gets here -- is not
    // that statement alone: `BEGIN ; put ...` was taken for its `BEGIN`,
    // and the rest never ran. The extended protocol takes one command at a
    // time, and PostgreSQL refuses it the same way.
    let pieces = statements(q);
    if pieces.len() > 1 && handle(pieces[0], cfg, standby).is_some() {
        return Some(Shim::Refuse {
            code: "42601",
            message: "cannot insert multiple commands into a prepared statement".into(),
        });
    }
    let words: Vec<&str> = lower
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty())
        .collect();
    let first = words.first().copied().unwrap_or("");
    let second = words.get(1).copied().unwrap_or("");

    match first {
        // Transaction control, carried out by the session. A savepoint is
        // read before the `ROLLBACK` it begins with, and two-phase commit is
        // refused rather than read as what it begins with: `ROLLBACK TO s`
        // taken for `ROLLBACK` put back the whole transaction and went on
        // outside one, and `COMMIT PREPARED 'x'` committed the transaction
        // open instead of the one named.
        "rollback"
            if second == "to"
                || (matches!(second, "work" | "transaction") && words.get(2) == Some(&"to")) =>
        {
            return Some(savepoint(q))
        }
        "savepoint" | "release" => return Some(savepoint(q)),
        "commit" | "rollback" if second == "prepared" => return Some(no_two_phase()),
        "prepare" if second == "transaction" => return Some(no_two_phase()),
        "begin" | "start" => return Some(Shim::Tx(Tx::Begin(Change::of(&words)))),
        "commit" | "end" | "rollback" | "abort" => {
            let chain = words.windows(2).any(|w| w == ["and", "chain"]);
            return Some(Shim::Tx(match first {
                "commit" | "end" => Tx::Commit { chain },
                _ => Tx::Rollback { chain },
            }));
        }
        "set" if second == "transaction" => return Some(Shim::Tx(Tx::Set(Change::of(&words)))),
        "set" if words.starts_with(&["set", "session", "characteristics"]) => {
            return Some(Shim::Tx(Tx::Default(Change::of(&words))))
        }
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
        let plain = Change::default();
        assert_eq!(tx("BEGIN"), Some(Tx::Begin(plain)));
        assert_eq!(tx("start transaction"), Some(Tx::Begin(plain)));
        assert_eq!(tx("COMMIT;"), Some(Tx::Commit { chain: false }));
        assert_eq!(tx("end"), Some(Tx::Commit { chain: false }));
        assert_eq!(tx("ROLLBACK"), Some(Tx::Rollback { chain: false }));
        assert_eq!(tx("abort"), Some(Tx::Rollback { chain: false }));
        assert_eq!(tx("commit and chain"), Some(Tx::Commit { chain: true }));
        assert_eq!(tx("COMMIT AND NO CHAIN"), Some(Tx::Commit { chain: false }));

        // The modes a transaction can be begun or set in.
        let serial = Change {
            serial: Some(true),
            read_only: None,
        };
        assert_eq!(
            tx("BEGIN ISOLATION LEVEL SERIALIZABLE"),
            Some(Tx::Begin(serial))
        );
        assert_eq!(
            tx("begin isolation level repeatable read"),
            Some(Tx::Begin(serial))
        );
        assert_eq!(
            tx("START TRANSACTION READ ONLY, ISOLATION LEVEL READ COMMITTED"),
            Some(Tx::Begin(Change {
                serial: Some(false),
                read_only: Some(true),
            }))
        );
        assert_eq!(
            tx("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE"),
            Some(Tx::Set(serial))
        );
        assert_eq!(
            tx("SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE"),
            Some(Tx::Default(Change {
                serial: None,
                read_only: Some(false),
            }))
        );
        assert!(serial.over(Mode::default()).serial);

        // Savepoints, named as PostgreSQL reads an identifier.
        let name = |n: &str| n.to_string();
        assert_eq!(tx("SAVEPOINT s1"), Some(Tx::Savepoint(name("s1"))));
        assert_eq!(tx("savepoint SP_1;"), Some(Tx::Savepoint(name("sp_1"))));
        assert_eq!(
            tx(r#"SAVEPOINT "_pg3_1""#),
            Some(Tx::Savepoint(name("_pg3_1")))
        );
        assert_eq!(
            tx(r#"savepoint "A ""quoted"" One""#),
            Some(Tx::Savepoint(name(r#"A "quoted" One"#)))
        );
        assert_eq!(tx("RELEASE SAVEPOINT s1"), Some(Tx::Release(name("s1"))));
        assert_eq!(tx(r#"RELEASE "_pg3_1""#), Some(Tx::Release(name("_pg3_1"))));
        assert_eq!(
            tx("release savepoint"),
            Some(Tx::Release(name("savepoint")))
        );
        assert_eq!(
            tx("ROLLBACK TO SAVEPOINT sa_savepoint_1"),
            Some(Tx::RollbackTo(name("sa_savepoint_1")))
        );
        assert_eq!(tx("rollback to s1"), Some(Tx::RollbackTo(name("s1"))));
        assert_eq!(
            tx(r#"ROLLBACK TO "_pg3_1""#),
            Some(Tx::RollbackTo(name("_pg3_1")))
        );
        // Read as a plain ROLLBACK, these put back the whole transaction.
        assert_eq!(
            tx("ROLLBACK TRANSACTION TO SAVEPOINT s1"),
            Some(Tx::RollbackTo(name("s1")))
        );
        assert_eq!(tx("rollback work to s1"), Some(Tx::RollbackTo(name("s1"))));
        assert_eq!(tx("ROLLBACK WORK"), Some(Tx::Rollback { chain: false }));
        // As PostgreSQL's grammar reads it: `savepoint` is the name.
        assert_eq!(
            tx("rollback to savepoint"),
            Some(Tx::RollbackTo(name("savepoint")))
        );
        for q in [
            "SAVEPOINT",
            "savepoint a b",
            "ROLLBACK TO",
            "rollback to savepoint a b",
            r#"release "open"#,
        ] {
            assert!(
                matches!(
                    handle(q, &cfg, &|| false),
                    Some(Shim::Refuse { code: "42601", .. })
                ),
                "`{q}` must be refused"
            );
        }

        // What begins like transaction control and is not.
        for q in [
            "COMMIT PREPARED 'x'",
            "ROLLBACK PREPARED 'x'",
            "PREPARE TRANSACTION 'x'",
        ] {
            assert!(
                matches!(
                    handle(q, &cfg, &|| false),
                    Some(Shim::Refuse { code: "0A000", .. })
                ),
                "`{q}` must be refused"
            );
        }

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
    fn a_text_is_split_where_a_semicolon_stands_alone() {
        assert_eq!(
            statements("BEGIN; put t {name: 'a;b'}; COMMIT;"),
            ["BEGIN", "put t {name: 'a;b'}", "COMMIT"]
        );
        assert_eq!(
            statements(
                r#"put t {s: "x\";y"} ; -- a; comment
            get t # another; one
            ; /* and; this */ SAVEPOINT "a;b""#
            ),
            [
                r#"put t {s: "x\";y"}"#,
                "-- a; comment\n            get t # another; one",
                r#"/* and; this */ SAVEPOINT "a;b""#
            ]
        );
        assert_eq!(statements(" ; ;"), Vec::<&str>::new());
        assert_eq!(statements("get t"), ["get t"]);
        // Quotes and comments left open run to the end.
        assert_eq!(statements("put t {s: 'a;"), ["put t {s: 'a;"]);
        assert_eq!(statements("get t /* ;"), ["get t /* ;"]);
    }

    /// The extended protocol takes one command: a text of several the first
    /// of which is answered here is refused, not taken for its first.
    #[test]
    fn several_commands_are_not_taken_for_their_first() {
        let cfg = Config::default();
        for q in [
            "BEGIN ; put t {x: 1}",
            "COMMIT; put t {x: 1}",
            "SET a = 1; get t",
        ] {
            assert!(
                matches!(
                    as_primary(q, &cfg),
                    Some(Shim::Refuse { code: "42601", .. })
                ),
                "`{q}` must be refused"
            );
        }
        assert!(as_primary("put t {x: 1}; put t {x: 2}", &cfg).is_none());
        assert!(matches!(as_primary("BEGIN;", &cfg), Some(Shim::Tx(_))));
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
