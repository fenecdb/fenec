use std::fmt;

/// The single error type across fenecdb. Allocation-free variants, no `Box`.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// Data that does not match the schema.
    Type(String),
    /// A collection / field / record that was not found.
    NotFound(String),
    /// A resource that already exists.
    Exists(String),
    /// An `insert` of a document whose id is taken, or a `@unique` value
    /// held: 409 over HTTP, as `Exists` is for a collection.
    Duplicate(String),
    /// A corrupt segment or an unexpected byte sequence.
    Corrupt(String),
    /// Invalid query (semantic errors after parsing).
    Query(String),
    /// I/O error.
    Io(String),
    /// Error originating from a plugin.
    Plugin(String),
    /// A write sent to a database that takes its writes from a primary.
    ReadOnly(String),
    /// Outside what the caller's token may read or write.
    Denied(String),
    /// A write's `require <n>` not met: it wrote another number of rows, and
    /// it and its block are put back. 412 over HTTP.
    Unmet(String),
    /// A block of writes past a quota a server holds the database to --
    /// a tenant's file bytes or rows (`Database::set_limit`) -- put back.
    /// 507 over HTTP.
    Quota(String),
}

impl fmt::Display for Error {
    /// The kind, then the message: one `write!` for every kind, where one
    /// each was 0.6 KB of the browser module.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, m) = match self {
            Error::Type(m) => ("type error", m),
            Error::NotFound(m) => ("not found", m),
            Error::Exists(m) => ("already exists", m),
            Error::Duplicate(m) => ("duplicate", m),
            Error::Corrupt(m) => ("corrupt", m),
            Error::Query(m) => ("query error", m),
            Error::Io(m) => ("io error", m),
            Error::Plugin(m) => ("plugin error", m),
            Error::ReadOnly(m) => ("read only", m),
            Error::Denied(m) => ("denied", m),
            Error::Unmet(m) => ("unmet", m),
            Error::Quota(m) => ("quota", m),
        };
        write!(f, "{kind}: {m}")
    }
}

impl std::error::Error for Error {}

#[cfg(feature = "std-fs")]
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The refusal of what needs a feature this build was made without
/// (`Cargo.toml`): `what` names it, and the message names the feature.
pub(crate) fn not_built(what: &str, feature: &str) -> Error {
    Error::Query(format!(
        "{what} needs the `{feature}` feature, which this build was made without"
    ))
}
