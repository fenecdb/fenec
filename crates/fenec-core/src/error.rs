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
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Type(m) => write!(f, "type error: {m}"),
            Error::NotFound(m) => write!(f, "not found: {m}"),
            Error::Exists(m) => write!(f, "already exists: {m}"),
            Error::Corrupt(m) => write!(f, "corrupt: {m}"),
            Error::Query(m) => write!(f, "query error: {m}"),
            Error::Io(m) => write!(f, "io error: {m}"),
            Error::Plugin(m) => write!(f, "plugin error: {m}"),
            Error::ReadOnly(m) => write!(f, "read only: {m}"),
        }
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
