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
    /// A statement's `require <n>` not met: a write wrote another number
    /// of rows, or a `get` answered one, and it and its block are put back.
    /// 412 over HTTP. Where its `else '<reason>'` gave a reason, the
    /// message is that reason, then the count's words in parentheses --
    /// "insufficient funds (`set accounts` wrote 0 rows, and requires 1)"
    /// -- which [`Error::reason`] reads it back from: held in the message,
    /// never beside it. A `String` more beside it took the error, and every
    /// `Result` of the crate no larger, from 32 bytes to 48; a `u16` of its
    /// length kept 32, but rustc laid the variant out by its `String`'s
    /// niche, at another offset than the other eleven's, and the error's
    /// drop, `Clone` and `Display` each branched on it: 1.3 KB more of the
    /// browser module, 0.5 KB brotli.
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

impl Error {
    /// The reason a statement's `require <n> else '<reason>'` gave, where
    /// that refused it: the statement's own words, which a caller tells one
    /// refusal from another by. The engine writes the count's words after
    /// it in parentheses -- a verb and a collection in backticks, then
    /// numbers -- and a name holds no space, parenthesis or backtick, so
    /// the last `` (` `` opens them, whatever the reason holds. Found by
    /// its bytes: `rfind` of a pattern of several took the string searcher
    /// into the browser module, 7.8 KB.
    pub fn reason(&self) -> Option<&str> {
        let Error::Unmet(m) = self else {
            return None;
        };
        if !m.ends_with(')') {
            return None;
        }
        match m.as_bytes().windows(3).rposition(|w| w == b" (`") {
            Some(0) | None => None,
            Some(at) => m.get(..at),
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::Error;

    /// The reason takes no byte of the error: it is in the message
    /// ([`Error::Unmet`]).
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn a_reason_takes_no_byte_more() {
        assert_eq!(std::mem::size_of::<Error>(), 32);
    }

    #[test]
    fn the_reason_is_the_message_before_the_count() {
        let count = "(`get a` answered 0 rows, and requires 1)";
        let said = |r: &str| Error::Unmet(format!("{r} {count}"));
        assert_eq!(said("no such account").reason(), Some("no such account"));
        // Whatever the reason holds, parentheses and backticks among it.
        for r in [
            "sold out (for now)",
            "see (`docs`)",
            "a ) (` b",
            "ödül (`x`) yok",
        ] {
            assert_eq!(said(r).reason(), Some(r), "{r}");
        }
        // A count with no reason before it, and every other error, say none.
        assert_eq!(
            Error::Unmet(count[1..count.len() - 1].into()).reason(),
            None
        );
        assert_eq!(Error::Unmet(count.into()).reason(), None);
        assert_eq!(Error::Query(format!("x {count}")).reason(), None);
    }
}
