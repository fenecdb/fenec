//! The fenecdb type a SQL column's declared type is: PostgreSQL's types by
//! name, and any other declaration by SQLite's affinity rules. One table
//! for both readers of a SQL schema -- `fenec import`, which maps a
//! PostgreSQL column by its type's name and a SQLite column by its
//! declaration, and FenecQL's reading of SQL's `create table` -- so the
//! two cannot part.

use fenec_core::value::{DataType, VecPrec};

/// What a declared type is in fenecdb.
#[derive(Debug, Clone, PartialEq)]
pub enum Mapped {
    /// A field's type, and whether its text compares by its case fold, as
    /// `citext`'s does (a `nocase` field).
    Type(DataType, bool),
    /// No fenecdb type keeps the column's values, for this reason.
    Not(Why),
}

/// Why a declared type has no fenecdb type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// `numeric`, `decimal`, `money`: fenecdb has no decimal.
    Decimal,
    /// `vector`, `halfvec` or `sparsevec` with no dimension.
    NoDimension,
    /// No type, or one no rule reads: SQLite's `NUMERIC` affinity, which
    /// takes any value.
    Untyped,
}

/// PostgreSQL's type `name` -- lowercased, its words one space apart, its
/// modifiers apart in `args`: `varchar(80)` is `varchar` and 80, `vector(384)`
/// `vector` and 384, `timestamp(3) with time zone` `timestamp with time
/// zone` and 3 -- or `None` for a name that is none of PostgreSQL's. A
/// length is not held, as SQLite holds none, and a smaller integer or
/// float is the one fenecdb has.
pub fn postgres(name: &str, args: &[i64]) -> Option<Mapped> {
    use DataType::*;
    let dim = args.first().copied().filter(|&d| d > 0).map(|d| d as usize);
    let ty = |t: DataType| Some(Mapped::Type(t, false));
    match name {
        "bool" | "boolean" => ty(Bool),
        "smallint" | "int2" | "integer" | "int" | "int4" | "bigint" | "int8" => ty(Int),
        "real" | "float4" | "double precision" | "float8" | "float" => ty(Float),
        "text" | "varchar" | "character varying" | "char" | "character" | "bpchar" | "name"
        | "uuid" => ty(Text),
        "citext" => Some(Mapped::Type(Text, true)),
        "bytea" => ty(Bytes),
        "date"
        | "timestamp"
        | "timestamptz"
        | "timestamp with time zone"
        | "timestamp without time zone" => ty(Timestamp),
        "json" | "jsonb" => ty(Json),
        "point" => ty(Geo),
        "vector" | "halfvec" | "sparsevec" => Some(match (name, dim) {
            ("vector", Some(d)) => Mapped::Type(Vector(d, VecPrec::F32), false),
            ("halfvec", Some(d)) => Mapped::Type(Vector(d, VecPrec::F16), false),
            (_, Some(d)) => Mapped::Type(Sparse(d), false),
            (_, None) => Mapped::Not(Why::NoDimension),
        }),
        "numeric" | "decimal" | "money" => Some(Mapped::Not(Why::Decimal)),
        // PostgreSQL's own, which SQLite's rules would read otherwise --
        // `interval` holds `INT` -- and fenecdb has none of.
        "interval"
        | "time"
        | "timetz"
        | "time with time zone"
        | "time without time zone"
        | "inet"
        | "cidr"
        | "macaddr"
        | "xml"
        | "tsvector"
        | "tsquery"
        | "bit"
        | "bit varying"
        | "varbit"
        | "box"
        | "line"
        | "lseg"
        | "path"
        | "polygon"
        | "circle"
        | "int4range"
        | "int8range"
        | "numrange"
        | "tsrange"
        | "tstzrange"
        | "daterange"
        | "geometry"
        | "geography"
        | "oid" => Some(Mapped::Not(Why::Untyped)),
        _ => None,
    }
}

/// A declared type as SQLite reads it -- its affinity, by the words in it
/// -- with the types fenecdb has besides: a boolean, and a time.
pub fn sqlite(decl: &str) -> Mapped {
    let d = decl.trim_ascii().to_ascii_uppercase();
    // A window of bytes compared: `str::contains` was the standard
    // library's two-way searcher and a slice's panic, 5.5 KB of the
    // browser module.
    let has = |k: &str| d.as_bytes().windows(k.len()).any(|w| w == k.as_bytes());
    let ty = |t: DataType| Mapped::Type(t, false);
    if has("BOOL") {
        return ty(DataType::Bool);
    }
    if has("DATETIME") || has("TIMESTAMP") || d == "DATE" {
        return ty(DataType::Timestamp);
    }
    if has("INT") {
        return ty(DataType::Int);
    }
    if has("CHAR") || has("CLOB") || has("TEXT") {
        return ty(DataType::Text);
    }
    if has("BLOB") {
        return ty(DataType::Bytes);
    }
    if has("REAL") || has("FLOA") || has("DOUB") {
        return ty(DataType::Float);
    }
    if has("DEC") || has("NUMERIC") || has("MONEY") {
        return Mapped::Not(Why::Decimal);
    }
    Mapped::Not(Why::Untyped)
}
