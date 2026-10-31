//! # fenec-core
//!
//! A minimal embedded database core that runs in WASM.
//!
//! Design principles
//! 1. **Zero dependencies.** The core uses only `std`; the WASM output stays
//!    small and auditable.
//! 2. **No buffer pool.** Storage is immutable, append-only segments; the
//!    byte format is identical on disk and in memory, so no cache of the
//!    database's own sits in between (the operating system's page cache
//!    keeps the mapped file's pages). See [`store`].
//! 3. **Vectors are first class.** `vector<N>` is a type, `@hnsw` is an
//!    index, `near` is a query clause -- not an add-on.
//! 4. **Extension through plugins.** A registry of functions and write
//!    hooks; a server's access checks and `--follow`'s guard are plugins.
//!
//! ```
//! use fenec_core::prelude::*;
//!
//! let mut db = Database::new();
//! db.execute(&Statement::CreateCollection {
//!     schema: Schema::new("docs", vec![
//!         Field::new("title", DataType::Text),
//!         Field::new("embed", DataType::Vector(3, VecPrec::F32))
//!             .indexed(IndexKind::Vector(VectorIndexSpec::default())),
//!     ]).unwrap(),
//!     if_not_exists: false,
//! }).unwrap();
//! ```

pub mod case;
pub mod changes;
pub mod codec;
pub mod collate;
pub mod composite;
pub mod declared;
pub mod engine;
pub mod error;
pub mod fuzzy;
pub mod geo;
pub mod highlight;
pub mod history;
pub mod hll;
pub mod json;
pub mod maps;
#[cfg(feature = "statements")]
pub mod named;
pub mod num;
mod off;
pub mod plugin;
pub mod query;
pub mod schema;
pub mod sorted;
pub mod sparse;
pub mod store;
pub mod text;
pub mod time;
pub mod tz;
pub mod value;
pub mod vector;

#[cfg(feature = "std-fs")]
pub mod cache;
#[cfg(feature = "std-fs")]
pub mod fs;
#[cfg(all(feature = "std-fs", unix))]
mod synclog;

pub mod prelude {
    pub use crate::changes::{ChangeLog, Since};
    pub use crate::collate::Collation;
    #[cfg(not(target_arch = "wasm32"))]
    pub use crate::engine::Pinned;
    pub use crate::engine::{
        ChangeBatch, Changes, Collection, CollectionStats, Database, Durability, Sink,
        TextIndexStats, VectorIndexStats, Watcher,
    };
    pub use crate::error::{Error, Result};
    pub use crate::history::History;
    pub use crate::plugin::{Hook, Plugin, Registry, ScalarFn, WriteOp};
    pub use crate::query::{
        Alter, CmpOp, Column, Count, Expr, Fuse, Lookup, Match, Near, Nested, Rerank, Response,
        ResultSet, Row, Select, Sort, Statement,
    };
    pub use crate::schema::{
        Field, IndexKind, Metric, Quant, Schema, TextIndexSpec, VectorIndexSpec,
    };
    pub use crate::value::{DataType, DocId, Document, Value, VecPrec};
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Whether this build has every feature a build may go without
/// (`Cargo.toml`'s `full`): where the checks for one it lacks fold away,
/// so that they cost the full browser module nothing.
pub(crate) const FULL: bool = cfg!(all(
    feature = "vector",
    feature = "text",
    feature = "sparse",
    feature = "sorted",
    feature = "composite",
    feature = "geo",
    feature = "nocase",
    feature = "facets",
    feature = "highlight",
    feature = "analytics",
    feature = "collate"
));
