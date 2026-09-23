//! # fenec-core
//!
//! A minimal, vector-native embedded database core that runs in WASM.
//!
//! Design principles
//! 1. **Zero dependencies.** The core uses only `std`; the WASM output stays
//!    small and auditable.
//! 2. **No buffer pool.** Storage is immutable, append-only segments; the
//!    byte format is identical on disk and in memory, so no page cache sits
//!    in between. See [`store`].
//! 3. **Vectors are first class.** `vector<N>` is a type, `@hnsw` is an
//!    index, `near` is a query clause -- not an add-on.
//! 4. **Extension through plugins.** A PostgreSQL-style plugin registry;
//!    fenec-pg builds the PostgreSQL wire protocol on top of it.
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

pub mod changes;
pub mod codec;
pub mod collate;
pub mod engine;
pub mod error;
pub mod history;
pub mod json;
pub mod num;
pub mod plugin;
pub mod query;
pub mod schema;
pub mod sorted;
pub mod sparse;
pub mod store;
pub mod text;
pub mod time;
pub mod value;
pub mod vector;

#[cfg(feature = "std-fs")]
pub mod fs;

pub mod prelude {
    pub use crate::changes::{ChangeLog, Since};
    pub use crate::collate::Collation;
    pub use crate::engine::{
        ChangeBatch, Changes, Collection, CollectionStats, Database, Durability, Sink,
        TextIndexStats, VectorIndexStats, Watcher,
    };
    pub use crate::error::{Error, Result};
    pub use crate::history::History;
    pub use crate::plugin::{Hook, Plugin, Registry, ScalarFn, WriteOp};
    pub use crate::query::{
        Agg, CmpOp, Expr, Fuse, Lookup, Match, Near, Nested, Rerank, Response, ResultSet, Row,
        Select, Sort, Statement,
    };
    pub use crate::schema::{
        Field, IndexKind, Metric, Quant, Schema, TextIndexSpec, VectorIndexSpec,
    };
    pub use crate::value::{DataType, DocId, Document, Value, VecPrec};
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
