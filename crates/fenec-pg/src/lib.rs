//! # fenec-pg
//!
//! fenecdb's PostgreSQL plugin.
//!
//! It provides two things:
//!
//! 1. **[`PgPlugin`]** -- adds PostgreSQL-side functions to the core plugin
//!    registry (`pg_version`, `pg_typeof`, `to_pgvector`).
//! 2. **[`Server`]** -- a PostgreSQL v3 wire protocol server. Any PostgreSQL
//!    client (psql, psycopg, node-postgres, JDBC, pgbouncer) can connect to
//!    fenecdb and run FenecQL.
//!
//! What is compatible is the *transport layer*, not the language: fenecdb does
//! not speak SQL, but it works with the PostgreSQL ecosystem's tooling.
//!
//! ```no_run
//! use std::sync::{Arc, RwLock};
//! use fenec_core::prelude::*;
//! use fenec_pg::{Config, PgPlugin, Server};
//!
//! let mut db = Database::new();
//! db.install_plugin(&PgPlugin).unwrap();
//! // RwLock: read-only statements run in parallel under a shared lock.
//! let db = Arc::new(RwLock::new(db));
//! Server::new(db, Config::default()).serve().unwrap();
//! ```

pub mod client;
pub mod compat;
/// SHA-256, HMAC, PBKDF2 and base64 now live in `fenec-http`, which checks
/// JWTs with them; SCRAM uses them from there.
pub use fenec_http::crypto;
pub mod proto;
pub mod scram;
pub mod server;

pub use server::{to_pg_text, Config, Server};

use fenec_core::error::{Error, Result};
use fenec_core::plugin::{Plugin, Registry, ScalarFn};
use fenec_core::value::Value;
use std::sync::Arc;

/// PostgreSQL compatibility plugin.
pub struct PgPlugin;

struct PgVersion;
impl ScalarFn for PgVersion {
    fn call(&self, _args: &[Value]) -> Result<Value> {
        Ok(Value::Text(format!(
            "PostgreSQL 16.0 (fenecdb {})",
            fenec_core::VERSION
        )))
    }
    fn arity(&self) -> (usize, Option<usize>) {
        (0, Some(0))
    }
    fn doc(&self) -> &str {
        "PostgreSQL-compatible version string"
    }
}

struct PgTypeof;
impl ScalarFn for PgTypeof {
    fn call(&self, args: &[Value]) -> Result<Value> {
        Ok(Value::Text(
            match &args[0] {
                Value::Null => "unknown",
                Value::Bool(_) => "boolean",
                Value::Int(_) => "bigint",
                Value::Timestamp(_) => "timestamp with time zone",
                Value::Float(_) => "double precision",
                Value::Text(_) => "text",
                Value::Bytes(_) => "bytea",
                Value::Vector(_) => "vector",
                Value::List(_) => "array",
            }
            .to_string(),
        ))
    }
    fn arity(&self) -> (usize, Option<usize>) {
        (1, Some(1))
    }
    fn doc(&self) -> &str {
        "the PostgreSQL type name of a value"
    }
}

struct ToPgVector;
impl ScalarFn for ToPgVector {
    fn call(&self, args: &[Value]) -> Result<Value> {
        match &args[0] {
            Value::Vector(_) | Value::List(_) => Ok(Value::Text(
                to_pg_text(&args[0]).unwrap_or_else(|| "[]".into()),
            )),
            other => Err(Error::Type(format!(
                "expected a vector, found {}",
                other.type_name()
            ))),
        }
    }
    fn arity(&self) -> (usize, Option<usize>) {
        (1, Some(1))
    }
    fn doc(&self) -> &str {
        "converts a vector into pgvector's text form: [1,2,3]"
    }
}

impl Plugin for PgPlugin {
    fn name(&self) -> &str {
        "postgres"
    }
    fn version(&self) -> &str {
        "0.1.0"
    }
    fn init(&self, reg: &mut Registry) -> Result<()> {
        reg.register_fn("pg_version", Arc::new(PgVersion))?;
        reg.register_fn("pg_typeof", Arc::new(PgTypeof))?;
        reg.register_fn("to_pgvector", Arc::new(ToPgVector))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fenec_core::prelude::*;

    #[test]
    fn plugin_registers_functions() {
        let mut db = Database::new();
        db.install_plugin(&PgPlugin).unwrap();
        let names = db.registry().function_names();
        assert!(names.contains(&"pg_version".to_string()));
        assert!(names.contains(&"to_pgvector".to_string()));
        assert_eq!(db.registry().plugins()[0].0, "postgres");
        // the same plugin cannot be installed twice
        assert!(db.install_plugin(&PgPlugin).is_err());
    }

    #[test]
    fn pgvector_text_format() {
        assert_eq!(
            to_pg_text(&Value::Vector(vec![1.0, 2.5])).unwrap(),
            "[1,2.5]"
        );
        assert_eq!(
            to_pg_text(&Value::List(vec![
                Value::Text("a".into()),
                Value::Text("b".into())
            ]))
            .unwrap(),
            "{\"a\",\"b\"}"
        );
    }
}
