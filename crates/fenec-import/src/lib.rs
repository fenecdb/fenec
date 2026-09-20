//! # fenec-import
//!
//! Builds a fenecdb collection from an existing database: from a SQLite file
//! or from a live PostgreSQL server.
//!
//! Both readers feed through a single interface ([`Source`]), so the load
//! pipeline ([`load`]) and the plan output ([`map`]) are source independent.
//!
//! The load order is the one fenecdb recommends: bulk write first, index build
//! second. Updating the HNSW graph on every insert does the same work far
//! more expensively (see `fenec_core::query::Statement::CreateIndex`).
//!
//! ```no_run
//! use fenec_import::{load, Options};
//! let mut src = fenec_import::sqlite::Reader::open("data.sqlite", "docs")?;
//! let mut db = fenec_core::fs::open("data.fenec")?;
//! let summary = load::run(&mut src, &mut db, &Options::new("articles"))?;
//! println!("{} rows", summary.rows);
//! # Ok::<(), fenec_core::error::Error>(())
//! ```

pub mod load;
pub mod map;
pub mod pg;
pub mod sqlite;

use std::time::Duration;
use fenec_core::error::Result;
use fenec_core::query::Expr;
use fenec_core::schema::IndexKind;
use fenec_core::value::{DataType, Value};

/// A single column of the source table and its fenecdb counterpart.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// The column name in the source.
    pub name: String,
    /// The inferred fenecdb type. `None` means there is no safe counterpart
    /// (`numeric`, `json`) and `--cast` becomes mandatory.
    pub ty: Option<DataType>,
    /// The source type, verbatim: `INTEGER`, `numeric`, `vector(384)`.
    /// Only for the plan output and error text.
    pub source_type: String,
    /// A lossy-mapping warning, or the reason there is no `ty`.
    pub note: Option<String>,
}

impl Column {
    pub fn new(name: impl Into<String>, ty: DataType, source_type: impl Into<String>) -> Column {
        Column {
            name: name.into(),
            ty: Some(ty),
            source_type: source_type.into(),
            note: None,
        }
    }

    /// A column with no safe counterpart in fenecdb. Without `--cast` it
    /// errors at the plan stage -- stopping beats silently losing a type.
    pub fn unsupported(
        name: impl Into<String>,
        source_type: impl Into<String>,
        reason: impl Into<String>,
    ) -> Column {
        Column {
            name: name.into(),
            ty: None,
            source_type: source_type.into(),
            note: Some(reason.into()),
        }
    }

    pub fn note(mut self, note: impl Into<String>) -> Column {
        self.note = Some(note.into());
        self
    }
}

/// A source that streams rows.
///
/// Every call to `next_row` returns one row in source column order; `None`
/// signals the end of the stream. Value types must be produced to match
/// [`Column::ty`]: `Value::coerce` does not go from text to number or from
/// integer to bool, and those conversions are the reader's job.
pub trait Source {
    /// The column list. Called before row reading starts.
    fn columns(&mut self) -> Result<Vec<Column>>;

    /// The next row, or `None` when the stream is done.
    fn next_row(&mut self) -> Result<Option<Vec<Value>>>;

    /// The known row count; `None` when unknown. Used only for progress
    /// output, so its accuracy does not affect the load.
    fn row_count(&self) -> Option<u64> {
        None
    }
}

/// Where the document's `id` comes from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum IdSource {
    /// Use an `id` column when it is a positive integer, otherwise let fenecdb assign one.
    #[default]
    Auto,
    /// Use this column as the `id`.
    Column(String),
    /// Whatever the source `id` is, let fenecdb assign its own.
    Generated,
}

/// Import options. A one-to-one match for the CLI flags.
#[derive(Debug, Clone)]
pub struct Options {
    /// Target collection name.
    pub into: String,
    /// `--vector <field>:<N>` -- take a BLOB/array column as `vector<N>`.
    pub vectors: Vec<(String, usize)>,
    /// `--cast <field>=<type>` -- override the inferred type.
    pub casts: Vec<(String, DataType)>,
    /// `--index <field>@hnsw(..)` -- built once the load finishes.
    pub indexes: Vec<(String, IndexKind)>,
    /// `--id <column>|none`
    pub id: IdSource,
    /// `--where <expr>` -- a FenecQL filter applied before the row is written.
    /// Field names are the names in the *target* schema (after renaming).
    pub filter: Option<Expr>,
    /// `--limit <N>` -- read at most this many rows.
    pub limit: Option<u64>,
    /// `--batch <N>` -- documents per `put` statement.
    pub batch: usize,
}

/// The `put` batch size. The value fenec-bench uses in its own measurements
/// (`crates/fenec-bench/src/main.rs`): a larger batch does not lower the
/// per-document cost, while peak memory grows linearly.
pub const DEFAULT_BATCH: usize = 2_000;

impl Options {
    pub fn new(into: impl Into<String>) -> Options {
        Options {
            into: into.into(),
            vectors: Vec::new(),
            casts: Vec::new(),
            indexes: Vec::new(),
            id: IdSource::Auto,
            filter: None,
            limit: None,
            batch: DEFAULT_BATCH,
        }
    }
}

/// The result of a load.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    /// Number of documents written.
    pub rows: u64,
    /// Number of rows dropped by `--where`.
    pub skipped: u64,
    pub elapsed: Duration,
    /// Warnings produced at the plan stage (lossy mapping, renaming).
    pub warnings: Vec<String>,
}

/// A source that reads rows from memory. For tests and for experiments
/// outside `--dry-run`; the real readers live in the [`sqlite`] and [`pg`]
/// modules.
pub struct Rows {
    columns: Vec<Column>,
    rows: std::vec::IntoIter<Vec<Value>>,
    total: u64,
}

impl Rows {
    pub fn new(columns: Vec<Column>, rows: Vec<Vec<Value>>) -> Rows {
        let total = rows.len() as u64;
        Rows {
            columns,
            rows: rows.into_iter(),
            total,
        }
    }
}

impl Source for Rows {
    fn columns(&mut self) -> Result<Vec<Column>> {
        Ok(self.columns.clone())
    }
    fn next_row(&mut self) -> Result<Option<Vec<Value>>> {
        Ok(self.rows.next())
    }
    fn row_count(&self) -> Option<u64> {
        Some(self.total)
    }
}
