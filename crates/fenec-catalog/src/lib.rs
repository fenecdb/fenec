//! The catalog a PostgreSQL client expects: `pg_class`, `pg_attribute`,
//! `pg_type`, `information_schema.tables` and the rest, made from the
//! database's own schemas, and the `SELECT`s tools run against them.
//!
//! psql's `\d`, JDBC's `DatabaseMetaData` and DBeaver's navigator do not ask
//! "which collections are there" -- they send SQL over the catalog, with
//! joins, `CASE`, casts to `regclass`, correlated subqueries and `UNION`.
//! Matching those texts one by one breaks with the next client version, so
//! they are *run*: [`sql`] reads the part of `SELECT` they use, and this
//! module evaluates it over tables built fresh from the schemas each time.
//!
//! What a collection looks like from here: a table in `public`, its `id` the
//! first column and the primary key, each field a column of the type the
//! wire already reports for it -- `vector(384)` for a vector -- and each
//! fenecdb index an index with its own access method (`hash`, `btree` for
//! `@sorted`, `hnsw`, `bm25` for `@text`).
//!
//! A query outside what [`sql`] reads, or a catalog table not built here, is
//! not an error to the client: the one is answered empty, as every catalog
//! query was before, and the other is an empty table.

pub mod regex;
pub mod sql;

use fenec_core::prelude::*;
use sql::{Expr, Item, JoinKind, Query, Select, Source, TypeName, Unsupported};
use std::cmp::Ordering;
use std::collections::HashMap;

type Out<T> = std::result::Result<T, Unsupported>;

// ------------------------------------------------------------------- oids

const BOOL: i32 = 16;
const BYTEA: i32 = 17;
const CHAR: i32 = 18;
const NAME: i32 = 19;
const INT8: i32 = 20;
const INT2: i32 = 21;
const INT2VECTOR: i32 = 22;
const INT4: i32 = 23;
const REGPROC: i32 = 24;
const TEXT: i32 = 25;
const OID: i32 = 26;
const OIDVECTOR: i32 = 30;
const NODE_TREE: i32 = 194;
const FLOAT4: i32 = 700;
const FLOAT8: i32 = 701;
const BOOL_ARRAY: i32 = 1000;
const BYTEA_ARRAY: i32 = 1001;
const NAME_ARRAY: i32 = 1003;
const INT2_ARRAY: i32 = 1005;
const INT4_ARRAY: i32 = 1007;
const TEXT_ARRAY: i32 = 1009;
const INT8_ARRAY: i32 = 1016;
const FLOAT8_ARRAY: i32 = 1022;
const OID_ARRAY: i32 = 1028;
const ACL_ARRAY: i32 = 1034;
const VARCHAR: i32 = 1043;
const TIMESTAMPTZ: i32 = 1184;
const TIMESTAMPTZ_ARRAY: i32 = 1185;
const REGCLASS: i32 = 2205;
const REGTYPE: i32 = 2206;
const REGNAMESPACE: i32 = 4089;
/// Types of fenecdb's own, where pgvector would have put its.
const VECTOR: i32 = 16_400;
const HALFVEC: i32 = 16_401;
const SPARSEVEC: i32 = 16_402;

const PG_CATALOG: i64 = 11;
const PUBLIC: i64 = 2200;
const INFORMATION_SCHEMA: i64 = 13_000;
/// The role every session is, as PostgreSQL's bootstrap superuser is 10.
const ROLE: i64 = 10;
const DATABASE: i64 = 16_384;
/// `tr-x-icu` and `und-x-icu`, the names PostgreSQL gives ICU's Turkish
/// collation and its root one: what a `collate tr` and a `collate und`
/// field's text orders in, and what `\d` shows for it.
const COLL_TR: i64 = 12_800;
const COLL_UND: i64 = 12_801;

const AM_HEAP: i64 = 2;
const AM_BTREE: i64 = 403;
const AM_HASH: i64 = 405;
const AM_HNSW: i64 = 16_410;
const AM_BM25: i64 = 16_411;
/// A sparse vector's index: a word index's shape, scored by dot product.
const AM_INVERTED: i64 = 16_413;
const AM_FENEC: i64 = 16_412;

/// A collection's oid, and room after it for its indexes and its key:
/// `+ 1` the primary key's index, `+ 1 + attnum` a field's, `+ 512` the
/// primary key constraint. The collection id is kept in the file, so the
/// oid a client caches stays the collection's across restarts.
fn table_oid(collection_id: u32) -> i64 {
    20_000 + collection_id as i64 * 1024
}

// ----------------------------------------------------------------- values

#[derive(Debug, Clone, Copy, PartialEq)]
enum Reg {
    Class,
    Type,
    Namespace,
}

/// A value in a catalog query.
#[derive(Debug, Clone, PartialEq)]
enum V {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Array(Vec<V>),
    /// `int2vector` / `oidvector`: printed space-separated, subscripted from 0.
    Vector(Vec<i64>),
    /// An oid that prints as the name of what it points to.
    Reg(Reg, i64),
    /// A composite: `information_schema._pg_expandarray` returns `(x, n)`.
    Record(Vec<(&'static str, V)>),
}

fn t(s: impl Into<String>) -> V {
    V::Text(s.into())
}

fn nul() -> V {
    V::Null
}

// --------------------------------------------------------------- snapshot

struct Field2 {
    name: String,
    attnum: i64,
    ty: DataType,
    index: IndexKind,
    required: bool,
    collate: Option<Collation>,
}

struct Table {
    oid: i64,
    name: String,
    rows: u64,
    bytes: u64,
    fields: Vec<Field2>,
}

impl Table {
    /// The indexes: the primary key's first, then one per indexed field.
    fn indexes(&self) -> Vec<Index> {
        let mut out = vec![Index {
            oid: self.oid + 1,
            name: format!("{}_pkey", self.name),
            am: AM_BTREE,
            attnum: 1,
            column: "id".into(),
            ty: DataType::Int,
            primary: true,
        }];
        for f in &self.fields {
            let (am, kind) = match &f.index {
                IndexKind::None => continue,
                IndexKind::Hash => (AM_HASH, "hash"),
                IndexKind::Sorted => (AM_BTREE, "sorted"),
                IndexKind::Text(_) => (AM_BM25, "text"),
                IndexKind::Vector(_) => (AM_HNSW, "hnsw"),
                IndexKind::Inverted => (AM_INVERTED, "inverted"),
            };
            out.push(Index {
                oid: self.oid + 1 + f.attnum,
                name: format!("{}_{}_{kind}", self.name, f.name),
                am,
                attnum: f.attnum,
                column: f.name.clone(),
                ty: f.ty.clone(),
                primary: false,
            });
        }
        out
    }
}

struct Index {
    oid: i64,
    name: String,
    am: i64,
    attnum: i64,
    column: String,
    ty: DataType,
    primary: bool,
}

/// The schemas the catalog is made from, taken under the read lock and
/// then held without it while the query runs.
pub struct Snapshot {
    database: String,
    version: String,
    tables: Vec<Table>,
    /// Every relation's name by oid and oid by name: a `regclass` is
    /// printed or resolved once per row, over hundreds of relations.
    names: HashMap<i64, String>,
    oids: HashMap<String, i64>,
}

impl Snapshot {
    pub fn of(db: &Database, database: &str, version: &str) -> Snapshot {
        let mut tables = Vec::new();
        for name in db.collection_names() {
            let Ok(c) = db.collection(&name) else {
                continue;
            };
            let fields = c
                .schema
                .fields
                .iter()
                .enumerate()
                .map(|(i, f)| Field2 {
                    name: f.name.clone(),
                    attnum: i as i64 + 2,
                    ty: f.ty.clone(),
                    index: f.index.clone(),
                    required: f.required,
                    collate: f.collate,
                })
                .collect();
            tables.push(Table {
                oid: table_oid(c.id),
                name,
                rows: c.store.len() as u64,
                bytes: c.store.total_bytes() as u64,
                fields,
            });
        }
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        let mut names = HashMap::new();
        for (oid, name) in CATALOG_TABLES {
            names.insert(oid, name.to_string());
        }
        for t in &tables {
            names.insert(t.oid, t.name.clone());
            for i in t.indexes() {
                names.insert(i.oid, i.name);
            }
        }
        let oids = names.iter().map(|(o, n)| (n.clone(), *o)).collect();
        Snapshot {
            database: database.to_string(),
            version: version.to_string(),
            tables,
            names,
            oids,
        }
    }

    fn relation_name(&self, oid: i64) -> Option<String> {
        self.names.get(&oid).cloned()
    }

    fn relation_oid(&self, name: &str) -> Option<i64> {
        let name = name.rsplit('.').next().unwrap_or(name).trim_matches('"');
        self.oids.get(name).copied()
    }
}

/// The catalog tables a `'pg_class'::regclass` may name, with the oids
/// PostgreSQL gives them.
const CATALOG_TABLES: [(i64, &str); 8] = [
    (1259, "pg_class"),
    (1249, "pg_attribute"),
    (1247, "pg_type"),
    (2615, "pg_namespace"),
    (1255, "pg_proc"),
    (2606, "pg_constraint"),
    (2610, "pg_index"),
    (2604, "pg_attrdef"),
];

// ------------------------------------------------------------------ types

/// The PostgreSQL type a field is shown as: its oid, its modifier (a
/// vector's dimension) and whether it is an array.
fn pg_type(ty: &DataType) -> (i32, i64, i64) {
    match ty {
        DataType::Bool => (BOOL, -1, 0),
        DataType::Int => (INT8, -1, 0),
        DataType::Float => (FLOAT8, -1, 0),
        DataType::Text => (TEXT, -1, 0),
        DataType::Bytes => (BYTEA, -1, 0),
        DataType::Timestamp => (TIMESTAMPTZ, -1, 0),
        DataType::Vector(n, VecPrec::F32) => (VECTOR, *n as i64, 0),
        DataType::Vector(n, VecPrec::F16) => (HALFVEC, *n as i64, 0),
        DataType::Sparse(n) => (SPARSEVEC, *n as i64, 0),
        DataType::List(inner) => {
            let oid = match **inner {
                DataType::Bool => BOOL_ARRAY,
                DataType::Int => INT8_ARRAY,
                DataType::Float => FLOAT8_ARRAY,
                DataType::Bytes => BYTEA_ARRAY,
                DataType::Timestamp => TIMESTAMPTZ_ARRAY,
                _ => TEXT_ARRAY,
            };
            (oid, -1, 1)
        }
    }
}

/// `pg_type`'s rows: (oid, name, length, category, element, array, collation).
const TYPES: [(i32, &str, i64, &str, i32, i32, i64); 31] = [
    (BOOL, "bool", 1, "B", 0, BOOL_ARRAY, 0),
    (BYTEA, "bytea", -1, "U", 0, BYTEA_ARRAY, 0),
    (CHAR, "char", 1, "Z", 0, 1002, 0),
    (NAME, "name", 64, "S", CHAR, NAME_ARRAY, 950),
    (INT8, "int8", 8, "N", 0, INT8_ARRAY, 0),
    (INT2, "int2", 2, "N", 0, INT2_ARRAY, 0),
    (INT2VECTOR, "int2vector", -1, "A", INT2, 1006, 0),
    (INT4, "int4", 4, "N", 0, INT4_ARRAY, 0),
    (REGPROC, "regproc", 4, "N", 0, 1008, 0),
    (TEXT, "text", -1, "S", 0, TEXT_ARRAY, 100),
    (OID, "oid", 4, "N", 0, OID_ARRAY, 0),
    (OIDVECTOR, "oidvector", -1, "A", OID, 1013, 0),
    (NODE_TREE, "pg_node_tree", -1, "Z", 0, 0, 100),
    (FLOAT4, "float4", 4, "N", 0, 1021, 0),
    (FLOAT8, "float8", 8, "N", 0, FLOAT8_ARRAY, 0),
    (BOOL_ARRAY, "_bool", -1, "A", BOOL, 0, 0),
    (BYTEA_ARRAY, "_bytea", -1, "A", BYTEA, 0, 0),
    (NAME_ARRAY, "_name", -1, "A", NAME, 0, 950),
    (INT2_ARRAY, "_int2", -1, "A", INT2, 0, 0),
    (INT4_ARRAY, "_int4", -1, "A", INT4, 0, 0),
    (TEXT_ARRAY, "_text", -1, "A", TEXT, 0, 100),
    (INT8_ARRAY, "_int8", -1, "A", INT8, 0, 0),
    (FLOAT8_ARRAY, "_float8", -1, "A", FLOAT8, 0, 0),
    (OID_ARRAY, "_oid", -1, "A", OID, 0, 0),
    (VARCHAR, "varchar", -1, "S", 0, 1015, 100),
    (TIMESTAMPTZ, "timestamptz", 8, "D", 0, TIMESTAMPTZ_ARRAY, 0),
    (
        TIMESTAMPTZ_ARRAY,
        "_timestamptz",
        -1,
        "A",
        TIMESTAMPTZ,
        0,
        0,
    ),
    (REGCLASS, "regclass", 4, "N", 0, 2210, 0),
    (VECTOR, "vector", -1, "U", 0, 0, 0),
    (HALFVEC, "halfvec", -1, "U", 0, 0, 0),
    (SPARSEVEC, "sparsevec", -1, "U", 0, 0, 0),
];

/// `format_type`: the name `\d` prints.
fn format_type(oid: i64, typmod: i64) -> Option<String> {
    let base = |oid: i64| -> Option<&'static str> {
        Some(match oid as i32 {
            BOOL => "boolean",
            BYTEA => "bytea",
            CHAR => "\"char\"",
            NAME => "name",
            INT8 => "bigint",
            INT2 => "smallint",
            INT4 => "integer",
            TEXT => "text",
            OID => "oid",
            FLOAT4 => "real",
            FLOAT8 => "double precision",
            VARCHAR => "character varying",
            TIMESTAMPTZ => "timestamp with time zone",
            REGCLASS => "regclass",
            REGTYPE => "regtype",
            INT2VECTOR => "int2vector",
            OIDVECTOR => "oidvector",
            _ => return None,
        })
    };
    match oid as i32 {
        VECTOR | HALFVEC | SPARSEVEC => {
            let name = match oid as i32 {
                VECTOR => "vector",
                HALFVEC => "halfvec",
                _ => "sparsevec",
            };
            Some(if typmod > 0 {
                format!("{name}({typmod})")
            } else {
                name.to_string()
            })
        }
        _ => {
            if let Some(b) = base(oid) {
                return Some(b.to_string());
            }
            let (_, _, _, _, elem, _, _) = TYPES.iter().find(|t| t.0 as i64 == oid)?;
            base(*elem as i64).map(|b| format!("{b}[]"))
        }
    }
}

fn type_by_name(name: &str) -> Option<i64> {
    let name = name.rsplit('.').next().unwrap_or(name).trim_matches('"');
    let alias = match name {
        "boolean" => "bool",
        "bigint" => "int8",
        "integer" | "int" => "int4",
        "smallint" => "int2",
        "real" => "float4",
        "double precision" => "float8",
        other => other,
    };
    TYPES.iter().find(|t| t.1 == alias).map(|t| t.0 as i64)
}

// ------------------------------------------------------------------ tables

/// A relation a query reads: the catalog table, a subquery's result or a
/// function's rows.
struct Rel {
    alias: String,
    columns: Vec<(String, i32)>,
    rows: Vec<Vec<V>>,
}

fn rel(alias: &str, cols: &[(&str, i32)], rows: Vec<Vec<V>>) -> Rel {
    Rel {
        alias: alias.to_string(),
        columns: cols.iter().map(|(n, t)| (n.to_string(), *t)).collect(),
        rows,
    }
}

/// A catalog table by name. One not built here is an empty table: a join
/// against it finds nothing, as it would against an empty catalog.
fn catalog_table(schema: Option<&str>, name: &str, alias: &str, s: &Snapshot) -> Rel {
    if schema == Some("information_schema") {
        return information_schema(name, alias, s);
    }
    match name {
        "pg_namespace" => rel(
            alias,
            &[
                ("oid", OID),
                ("nspname", NAME),
                ("nspowner", OID),
                ("nspacl", ACL_ARRAY),
            ],
            vec![
                vec![V::Int(PG_CATALOG), t("pg_catalog"), V::Int(ROLE), nul()],
                vec![V::Int(PUBLIC), t("public"), V::Int(ROLE), nul()],
                vec![
                    V::Int(INFORMATION_SCHEMA),
                    t("information_schema"),
                    V::Int(ROLE),
                    nul(),
                ],
            ],
        ),
        "pg_class" => pg_class(alias, s),
        "pg_attribute" => pg_attribute(alias, s),
        "pg_type" => pg_type_table(alias),
        "pg_index" => pg_index(alias, s),
        "pg_constraint" => pg_constraint(alias, s),
        "pg_am" => rel(
            alias,
            &[
                ("oid", OID),
                ("amname", NAME),
                ("amhandler", REGPROC),
                ("amtype", CHAR),
            ],
            [
                (AM_HEAP, "heap", "t"),
                (AM_BTREE, "btree", "i"),
                (AM_HASH, "hash", "i"),
                (AM_HNSW, "hnsw", "i"),
                (AM_BM25, "bm25", "i"),
                (AM_INVERTED, "inverted", "i"),
                (AM_FENEC, "fenec", "t"),
            ]
            .iter()
            .map(|(oid, name, kind)| vec![V::Int(*oid), t(*name), t("-"), t(*kind)])
            .collect(),
        ),
        "pg_database" => rel(
            alias,
            &[
                ("oid", OID),
                ("datname", NAME),
                ("datdba", OID),
                ("encoding", INT4),
                ("datlocprovider", CHAR),
                ("datistemplate", BOOL),
                ("datallowconn", BOOL),
                ("dathasloginevt", BOOL),
                ("datconnlimit", INT4),
                ("datfrozenxid", OID),
                ("datminmxid", OID),
                ("dattablespace", OID),
                ("datcollate", TEXT),
                ("datctype", TEXT),
                ("datlocale", TEXT),
                ("daticurules", TEXT),
                ("datcollversion", TEXT),
                ("datacl", ACL_ARRAY),
            ],
            vec![vec![
                V::Int(DATABASE),
                t(&s.database),
                V::Int(ROLE),
                V::Int(6),
                t("c"),
                V::Bool(false),
                V::Bool(true),
                V::Bool(false),
                V::Int(-1),
                V::Int(0),
                V::Int(0),
                V::Int(1663),
                t("C"),
                t("C"),
                nul(),
                nul(),
                nul(),
                nul(),
            ]],
        ),
        "pg_roles" | "pg_authid" => rel(
            alias,
            &[
                ("oid", OID),
                ("rolname", NAME),
                ("rolsuper", BOOL),
                ("rolinherit", BOOL),
                ("rolcreaterole", BOOL),
                ("rolcreatedb", BOOL),
                ("rolcanlogin", BOOL),
                ("rolreplication", BOOL),
                ("rolconnlimit", INT4),
                ("rolpassword", TEXT),
                ("rolvaliduntil", TIMESTAMPTZ),
                ("rolbypassrls", BOOL),
                ("rolconfig", TEXT_ARRAY),
            ],
            vec![vec![
                V::Int(ROLE),
                t("fenec"),
                V::Bool(true),
                V::Bool(true),
                V::Bool(true),
                V::Bool(true),
                V::Bool(true),
                V::Bool(true),
                V::Int(-1),
                t("********"),
                nul(),
                V::Bool(true),
                nul(),
            ]],
        ),
        "pg_user" | "pg_shadow" => rel(
            alias,
            &[
                ("usename", NAME),
                ("usesysid", OID),
                ("usecreatedb", BOOL),
                ("usesuper", BOOL),
                ("userepl", BOOL),
                ("usebypassrls", BOOL),
                ("passwd", TEXT),
                ("valuntil", TIMESTAMPTZ),
                ("useconfig", TEXT_ARRAY),
            ],
            vec![vec![
                t("fenec"),
                V::Int(ROLE),
                V::Bool(true),
                V::Bool(true),
                V::Bool(true),
                V::Bool(true),
                t("********"),
                nul(),
                nul(),
            ]],
        ),
        "pg_tablespace" => rel(
            alias,
            &[
                ("oid", OID),
                ("spcname", NAME),
                ("spcowner", OID),
                ("spcacl", ACL_ARRAY),
                ("spcoptions", TEXT_ARRAY),
            ],
            vec![
                vec![V::Int(1663), t("pg_default"), V::Int(ROLE), nul(), nul()],
                vec![V::Int(1664), t("pg_global"), V::Int(ROLE), nul(), nul()],
            ],
        ),
        "pg_collation" => rel(
            alias,
            &[
                ("oid", OID),
                ("collname", NAME),
                ("collnamespace", OID),
                ("collowner", OID),
                ("collprovider", CHAR),
                ("collisdeterministic", BOOL),
                ("collencoding", INT4),
                ("collcollate", TEXT),
                ("collctype", TEXT),
            ],
            vec![
                vec![
                    V::Int(100),
                    t("default"),
                    V::Int(PG_CATALOG),
                    V::Int(ROLE),
                    t("d"),
                    V::Bool(true),
                    V::Int(-1),
                    nul(),
                    nul(),
                ],
                vec![
                    V::Int(950),
                    t("C"),
                    V::Int(PG_CATALOG),
                    V::Int(ROLE),
                    t("c"),
                    V::Bool(true),
                    V::Int(-1),
                    t("C"),
                    t("C"),
                ],
                vec![
                    V::Int(951),
                    t("POSIX"),
                    V::Int(PG_CATALOG),
                    V::Int(ROLE),
                    t("c"),
                    V::Bool(true),
                    V::Int(-1),
                    t("POSIX"),
                    t("POSIX"),
                ],
                vec![
                    V::Int(COLL_TR),
                    t("tr-x-icu"),
                    V::Int(PG_CATALOG),
                    V::Int(ROLE),
                    t("i"),
                    V::Bool(true),
                    V::Int(-1),
                    t("tr"),
                    t("tr"),
                ],
                vec![
                    V::Int(COLL_UND),
                    t("und-x-icu"),
                    V::Int(PG_CATALOG),
                    V::Int(ROLE),
                    t("i"),
                    V::Bool(true),
                    V::Int(-1),
                    t("und"),
                    t("und"),
                ],
            ],
        ),
        "pg_settings" => rel(
            alias,
            &[
                ("name", TEXT),
                ("setting", TEXT),
                ("unit", TEXT),
                ("category", TEXT),
                ("short_desc", TEXT),
                ("context", TEXT),
                ("vartype", TEXT),
                ("source", TEXT),
            ],
            settings(s)
                .into_iter()
                .map(|(k, v)| {
                    vec![
                        t(k),
                        t(v),
                        nul(),
                        t("fenecdb"),
                        nul(),
                        t("internal"),
                        t("string"),
                        t("default"),
                    ]
                })
                .collect(),
        ),
        "pg_tables" => rel(
            alias,
            &[
                ("schemaname", NAME),
                ("tablename", NAME),
                ("tableowner", NAME),
                ("tablespace", NAME),
                ("hasindexes", BOOL),
                ("hasrules", BOOL),
                ("hastriggers", BOOL),
                ("rowsecurity", BOOL),
            ],
            s.tables
                .iter()
                .map(|tb| {
                    vec![
                        t("public"),
                        t(&tb.name),
                        t("fenec"),
                        nul(),
                        V::Bool(true),
                        V::Bool(false),
                        V::Bool(false),
                        V::Bool(false),
                    ]
                })
                .collect(),
        ),
        "pg_indexes" => rel(
            alias,
            &[
                ("schemaname", NAME),
                ("tablename", NAME),
                ("indexname", NAME),
                ("tablespace", NAME),
                ("indexdef", TEXT),
            ],
            s.tables
                .iter()
                .flat_map(|tb| {
                    tb.indexes().into_iter().map(move |i| {
                        vec![
                            t("public"),
                            t(&tb.name),
                            t(&i.name),
                            nul(),
                            t(index_def(tb, &i)),
                        ]
                    })
                })
                .collect(),
        ),
        _ => Rel {
            alias: alias.to_string(),
            columns: Vec::new(),
            rows: Vec::new(),
        },
    }
}

fn settings(s: &Snapshot) -> Vec<(&'static str, String)> {
    let num = s
        .version
        .split('.')
        .map(|p| p.parse::<u32>().unwrap_or(0))
        .collect::<Vec<_>>();
    let version_num = num.first().copied().unwrap_or(0) * 10_000 + num.get(1).copied().unwrap_or(0);
    vec![
        ("server_version", s.version.clone()),
        ("server_version_num", version_num.to_string()),
        ("server_encoding", "UTF8".into()),
        ("client_encoding", "UTF8".into()),
        ("standard_conforming_strings", "on".into()),
        ("search_path", "public".into()),
        ("DateStyle", "ISO, MDY".into()),
        ("TimeZone", "UTC".into()),
        ("integer_datetimes", "on".into()),
        ("max_identifier_length", "63".into()),
        ("max_index_keys", "32".into()),
        ("transaction_isolation", "read committed".into()),
    ]
}

fn pg_class(alias: &str, s: &Snapshot) -> Rel {
    let cols = [
        ("oid", OID),
        ("relname", NAME),
        ("relnamespace", OID),
        ("reltype", OID),
        ("reloftype", OID),
        ("relowner", OID),
        ("relam", OID),
        ("relfilenode", OID),
        ("reltablespace", OID),
        ("relpages", INT4),
        ("reltuples", FLOAT4),
        ("relallvisible", INT4),
        ("reltoastrelid", OID),
        ("relhasindex", BOOL),
        ("relisshared", BOOL),
        ("relpersistence", CHAR),
        ("relkind", CHAR),
        ("relnatts", INT2),
        ("relchecks", INT2),
        ("relhasrules", BOOL),
        ("relhastriggers", BOOL),
        ("relhassubclass", BOOL),
        ("relrowsecurity", BOOL),
        ("relforcerowsecurity", BOOL),
        ("relispopulated", BOOL),
        ("relreplident", CHAR),
        ("relispartition", BOOL),
        ("relrewrite", OID),
        ("relfrozenxid", OID),
        ("relminmxid", OID),
        ("relacl", ACL_ARRAY),
        ("reloptions", TEXT_ARRAY),
        ("relpartbound", NODE_TREE),
    ];
    let row = |oid: i64,
               name: &str,
               am: i64,
               pages: i64,
               tuples: f64,
               index: bool,
               kind: &str,
               natts: i64| {
        vec![
            V::Int(oid),
            t(name),
            V::Int(PUBLIC),
            V::Int(0),
            V::Int(0),
            V::Int(ROLE),
            V::Int(am),
            V::Int(oid),
            V::Int(0),
            V::Int(pages),
            V::Float(tuples),
            V::Int(0),
            V::Int(0),
            V::Bool(index),
            V::Bool(false),
            t("p"),
            t(kind),
            V::Int(natts),
            V::Int(0),
            V::Bool(false),
            V::Bool(false),
            V::Bool(false),
            V::Bool(false),
            V::Bool(false),
            V::Bool(true),
            t(if kind == "r" { "d" } else { "n" }),
            V::Bool(false),
            V::Int(0),
            V::Int(0),
            V::Int(0),
            nul(),
            nul(),
            nul(),
        ]
    };
    let mut rows = Vec::new();
    for tb in &s.tables {
        let pages = (tb.bytes / 8192) as i64;
        rows.push(row(
            tb.oid,
            &tb.name,
            AM_FENEC,
            pages,
            tb.rows as f64,
            true,
            "r",
            tb.fields.len() as i64 + 1,
        ));
        for i in tb.indexes() {
            rows.push(row(i.oid, &i.name, i.am, 1, tb.rows as f64, false, "i", 1));
        }
    }
    rel(alias, &cols, rows)
}

fn pg_attribute(alias: &str, s: &Snapshot) -> Rel {
    let cols = [
        ("attrelid", OID),
        ("attname", NAME),
        ("atttypid", OID),
        ("attlen", INT2),
        ("attnum", INT2),
        ("atttypmod", INT4),
        ("attndims", INT2),
        ("attbyval", BOOL),
        ("attalign", CHAR),
        ("attstorage", CHAR),
        ("attcompression", CHAR),
        ("attnotnull", BOOL),
        ("atthasdef", BOOL),
        ("atthasmissing", BOOL),
        ("attidentity", CHAR),
        ("attgenerated", CHAR),
        ("attisdropped", BOOL),
        ("attislocal", BOOL),
        ("attinhcount", INT2),
        ("attcollation", OID),
        ("attstattarget", INT2),
        ("attacl", ACL_ARRAY),
        ("attoptions", TEXT_ARRAY),
        ("attfdwoptions", TEXT_ARRAY),
    ];
    let row =
        |rel: i64, name: &str, ty: &DataType, num: i64, notnull: bool, coll: Option<Collation>| {
            let (oid, typmod, ndims) = pg_type(ty);
            let collation = match coll {
                Some(Collation::Turkish) => COLL_TR,
                Some(Collation::Root) => COLL_UND,
                None => TYPES.iter().find(|x| x.0 == oid).map_or(0, |x| x.6),
            };
            let len = TYPES.iter().find(|x| x.0 == oid).map_or(-1, |x| x.2);
            vec![
                V::Int(rel),
                t(name),
                V::Int(oid as i64),
                V::Int(len),
                V::Int(num),
                V::Int(typmod),
                V::Int(ndims),
                V::Bool(len > 0 && len <= 8),
                t("d"),
                t(if len > 0 { "p" } else { "x" }),
                t(""),
                V::Bool(notnull),
                V::Bool(false),
                V::Bool(false),
                t(""),
                t(""),
                V::Bool(false),
                V::Bool(true),
                V::Int(0),
                V::Int(collation),
                V::Int(-1),
                nul(),
                nul(),
                nul(),
            ]
        };
    let mut rows = Vec::new();
    for tb in &s.tables {
        rows.push(row(tb.oid, "id", &DataType::Int, 1, true, None));
        for f in &tb.fields {
            rows.push(row(tb.oid, &f.name, &f.ty, f.attnum, f.required, f.collate));
        }
        for i in tb.indexes() {
            rows.push(row(i.oid, &i.column, &i.ty, 1, i.primary, None));
        }
    }
    rel(alias, &cols, rows)
}

fn pg_type_table(alias: &str) -> Rel {
    let cols = [
        ("oid", OID),
        ("typname", NAME),
        ("typnamespace", OID),
        ("typowner", OID),
        ("typlen", INT2),
        ("typbyval", BOOL),
        ("typtype", CHAR),
        ("typcategory", CHAR),
        ("typispreferred", BOOL),
        ("typisdefined", BOOL),
        ("typdelim", CHAR),
        ("typrelid", OID),
        ("typelem", OID),
        ("typarray", OID),
        ("typinput", REGPROC),
        ("typoutput", REGPROC),
        ("typreceive", REGPROC),
        ("typsend", REGPROC),
        ("typalign", CHAR),
        ("typstorage", CHAR),
        ("typnotnull", BOOL),
        ("typbasetype", OID),
        ("typtypmod", INT4),
        ("typndims", INT4),
        ("typcollation", OID),
        ("typdefault", TEXT),
    ];
    let rows = TYPES
        .iter()
        .map(|(oid, name, len, cat, elem, array, coll)| {
            let ours = *oid == VECTOR || *oid == HALFVEC;
            vec![
                V::Int(*oid as i64),
                t(*name),
                V::Int(if ours { PUBLIC } else { PG_CATALOG }),
                V::Int(ROLE),
                V::Int(*len),
                V::Bool(*len > 0 && *len <= 8),
                t("b"),
                t(*cat),
                V::Bool(false),
                V::Bool(true),
                t(","),
                V::Int(0),
                V::Int(*elem as i64),
                V::Int(*array as i64),
                // JDBC tells an array type by `typinput = array_in`; the
                // vector types are category A without being arrays.
                t(if name.starts_with('_') {
                    "array_in".to_string()
                } else {
                    format!("{name}in")
                }),
                t(format!("{name}out")),
                t(format!("{name}recv")),
                t(format!("{name}send")),
                t("i"),
                t(if *len > 0 { "p" } else { "x" }),
                V::Bool(false),
                V::Int(0),
                V::Int(-1),
                V::Int(0),
                V::Int(*coll),
                nul(),
            ]
        })
        .collect();
    rel(alias, &cols, rows)
}

fn pg_index(alias: &str, s: &Snapshot) -> Rel {
    let cols = [
        ("indexrelid", OID),
        ("indrelid", OID),
        ("indnatts", INT2),
        ("indnkeyatts", INT2),
        ("indisunique", BOOL),
        ("indnullsnotdistinct", BOOL),
        ("indisprimary", BOOL),
        ("indisexclusion", BOOL),
        ("indimmediate", BOOL),
        ("indisclustered", BOOL),
        ("indisvalid", BOOL),
        ("indcheckxmin", BOOL),
        ("indisready", BOOL),
        ("indislive", BOOL),
        ("indisreplident", BOOL),
        ("indkey", INT2VECTOR),
        ("indcollation", OIDVECTOR),
        ("indclass", OIDVECTOR),
        ("indoption", INT2VECTOR),
        ("indexprs", NODE_TREE),
        ("indpred", NODE_TREE),
    ];
    let mut rows = Vec::new();
    for tb in &s.tables {
        for i in tb.indexes() {
            rows.push(vec![
                V::Int(i.oid),
                V::Int(tb.oid),
                V::Int(1),
                V::Int(1),
                V::Bool(i.primary),
                V::Bool(false),
                V::Bool(i.primary),
                V::Bool(false),
                V::Bool(true),
                V::Bool(false),
                V::Bool(true),
                V::Bool(false),
                V::Bool(true),
                V::Bool(true),
                // The default replica identity is the primary key without
                // the index being marked as one.
                V::Bool(false),
                V::Vector(vec![i.attnum]),
                V::Vector(vec![0]),
                V::Vector(vec![0]),
                V::Vector(vec![0]),
                nul(),
                nul(),
            ]);
        }
    }
    rel(alias, &cols, rows)
}

fn pg_constraint(alias: &str, s: &Snapshot) -> Rel {
    let cols = [
        ("oid", OID),
        ("conname", NAME),
        ("connamespace", OID),
        ("contype", CHAR),
        ("condeferrable", BOOL),
        ("condeferred", BOOL),
        ("convalidated", BOOL),
        ("conrelid", OID),
        ("contypid", OID),
        ("conindid", OID),
        ("conparentid", OID),
        ("confrelid", OID),
        ("confupdtype", CHAR),
        ("confdeltype", CHAR),
        ("confmatchtype", CHAR),
        ("conislocal", BOOL),
        ("coninhcount", INT2),
        ("connoinherit", BOOL),
        ("conkey", INT2_ARRAY),
        ("confkey", INT2_ARRAY),
    ];
    let rows = s
        .tables
        .iter()
        .map(|tb| {
            vec![
                V::Int(tb.oid + 512),
                t(format!("{}_pkey", tb.name)),
                V::Int(PUBLIC),
                t("p"),
                V::Bool(false),
                V::Bool(false),
                V::Bool(true),
                V::Int(tb.oid),
                V::Int(0),
                V::Int(tb.oid + 1),
                V::Int(0),
                V::Int(0),
                t(" "),
                t(" "),
                t(" "),
                V::Bool(true),
                V::Int(0),
                V::Bool(true),
                V::Array(vec![V::Int(1)]),
                nul(),
            ]
        })
        .collect();
    rel(alias, &cols, rows)
}

fn information_schema(name: &str, alias: &str, s: &Snapshot) -> Rel {
    let db = || t(&s.database);
    match name {
        "schemata" => rel(
            alias,
            &[
                ("catalog_name", NAME),
                ("schema_name", NAME),
                ("schema_owner", NAME),
            ],
            ["public", "pg_catalog", "information_schema"]
                .iter()
                .map(|n| vec![db(), t(*n), t("fenec")])
                .collect(),
        ),
        "tables" => rel(
            alias,
            &[
                ("table_catalog", NAME),
                ("table_schema", NAME),
                ("table_name", NAME),
                ("table_type", TEXT),
                ("is_insertable_into", TEXT),
                ("is_typed", TEXT),
            ],
            s.tables
                .iter()
                .map(|tb| {
                    vec![
                        db(),
                        t("public"),
                        t(&tb.name),
                        t("BASE TABLE"),
                        t("YES"),
                        t("NO"),
                    ]
                })
                .collect(),
        ),
        "columns" => {
            let cols = [
                ("table_catalog", NAME),
                ("table_schema", NAME),
                ("table_name", NAME),
                ("column_name", NAME),
                ("ordinal_position", INT4),
                ("column_default", TEXT),
                ("is_nullable", TEXT),
                ("data_type", TEXT),
                ("udt_catalog", NAME),
                ("udt_schema", NAME),
                ("udt_name", NAME),
                ("is_identity", TEXT),
                ("is_generated", TEXT),
                ("is_updatable", TEXT),
            ];
            let mut rows = Vec::new();
            for tb in &s.tables {
                let id = (String::from("id"), DataType::Int, 1i64, true);
                let fields = tb
                    .fields
                    .iter()
                    .map(|f| (f.name.clone(), f.ty.clone(), f.attnum, f.required));
                for (name, ty, num, required) in std::iter::once(id).chain(fields) {
                    let (oid, typmod, _) = pg_type(&ty);
                    let data_type = match oid {
                        VECTOR | HALFVEC | SPARSEVEC => "USER-DEFINED".to_string(),
                        _ if matches!(ty, DataType::List(_)) => "ARRAY".to_string(),
                        _ => format_type(oid as i64, typmod).unwrap_or_default(),
                    };
                    let udt = TYPES.iter().find(|x| x.0 == oid).map_or("text", |x| x.1);
                    rows.push(vec![
                        db(),
                        t("public"),
                        t(&tb.name),
                        t(name),
                        V::Int(num),
                        nul(),
                        t(if required { "NO" } else { "YES" }),
                        t(data_type),
                        db(),
                        t(if oid == VECTOR || oid == HALFVEC {
                            "public"
                        } else {
                            "pg_catalog"
                        }),
                        t(udt),
                        t("NO"),
                        t("NEVER"),
                        t("YES"),
                    ]);
                }
            }
            rel(alias, &cols, rows)
        }
        "table_constraints" => rel(
            alias,
            &[
                ("constraint_catalog", NAME),
                ("constraint_schema", NAME),
                ("constraint_name", NAME),
                ("table_catalog", NAME),
                ("table_schema", NAME),
                ("table_name", NAME),
                ("constraint_type", TEXT),
                ("is_deferrable", TEXT),
                ("initially_deferred", TEXT),
                ("enforced", TEXT),
            ],
            s.tables
                .iter()
                .map(|tb| {
                    vec![
                        db(),
                        t("public"),
                        t(format!("{}_pkey", tb.name)),
                        db(),
                        t("public"),
                        t(&tb.name),
                        t("PRIMARY KEY"),
                        t("NO"),
                        t("NO"),
                        t("YES"),
                    ]
                })
                .collect(),
        ),
        "key_column_usage" => rel(
            alias,
            &[
                ("constraint_catalog", NAME),
                ("constraint_schema", NAME),
                ("constraint_name", NAME),
                ("table_catalog", NAME),
                ("table_schema", NAME),
                ("table_name", NAME),
                ("column_name", NAME),
                ("ordinal_position", INT4),
            ],
            s.tables
                .iter()
                .map(|tb| {
                    vec![
                        db(),
                        t("public"),
                        t(format!("{}_pkey", tb.name)),
                        db(),
                        t("public"),
                        t(&tb.name),
                        t("id"),
                        V::Int(1),
                    ]
                })
                .collect(),
        ),
        _ => Rel {
            alias: alias.to_string(),
            columns: Vec::new(),
            rows: Vec::new(),
        },
    }
}

/// `pg_get_indexdef`: the statement that would make the index.
fn index_def(tb: &Table, i: &Index) -> String {
    let am = match i.am {
        AM_HASH => "hash",
        AM_HNSW => "hnsw",
        AM_BM25 => "bm25",
        AM_INVERTED => "inverted",
        _ => "btree",
    };
    let unique = if i.primary { "UNIQUE " } else { "" };
    format!(
        "CREATE {unique}INDEX {} ON public.{} USING {am} ({})",
        i.name, tb.name, i.column
    )
}

// --------------------------------------------------------------- evaluation

struct Ctx<'a> {
    snap: &'a Snapshot,
    params: &'a [V],
}

/// One joined row: which row of each relation, `None` where a left join
/// found none.
type Bound = Vec<Option<usize>>;

/// The rows bound at one level of a query, and the level around it for a
/// correlated subquery.
struct Scope<'a> {
    rels: &'a [Rel],
    /// Which row of each relation; `None` where a left join found none.
    row: &'a [Option<usize>],
    /// The rows of the group being aggregated, when there is one.
    group: Option<&'a [Vec<Option<usize>>]>,
    /// This row's window and set-returning values, by slot.
    slots: &'a [V],
    outer: Option<&'a Scope<'a>>,
}

const EMPTY: Scope<'static> = Scope {
    rels: &[],
    row: &[],
    group: None,
    slots: &[],
    outer: None,
};

fn column(scope: &Scope, qual: Option<&str>, name: &str) -> V {
    let mut level = Some(scope);
    while let Some(s) = level {
        for (k, r) in s.rels.iter().enumerate() {
            if qual.is_some_and(|q| q != r.alias) {
                continue;
            }
            match r.columns.iter().position(|(c, _)| c == name) {
                Some(ci) => {
                    return match s.row.get(k).copied().flatten() {
                        Some(ri) => r.rows[ri][ci].clone(),
                        None => V::Null,
                    }
                }
                // A qualified name of a table that does not have the column:
                // an empty or partial catalog table; the value is null.
                None if qual.is_some() => return V::Null,
                None => {}
            }
        }
        level = s.outer;
    }
    // A system column (`ctid`, `xmin`) or one no table here declares.
    V::Null
}

/// The type a column is declared with, for the row description.
fn column_type(rels: &[Rel], qual: Option<&str>, name: &str) -> i32 {
    rels.iter()
        .filter(|r| qual.is_none_or(|q| q == r.alias))
        .find_map(|r| r.columns.iter().find(|(c, _)| c == name).map(|(_, t)| *t))
        .unwrap_or(TEXT)
}

fn truth(v: &V) -> Option<bool> {
    match v {
        V::Null => None,
        V::Bool(b) => Some(*b),
        V::Int(i) => Some(*i != 0),
        V::Text(s) => match s.as_str() {
            "t" | "true" | "on" | "yes" | "1" => Some(true),
            "f" | "false" | "off" | "no" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn as_int(v: &V) -> Option<i64> {
    match v {
        V::Int(i) | V::Reg(_, i) => Some(*i),
        V::Bool(b) => Some(*b as i64),
        V::Float(f) => Some(*f as i64),
        V::Text(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// How two values compare, as PostgreSQL would once it had cast the literal
/// side to the other's type; `None` when either is null.
fn compare(a: &V, b: &V, s: &Snapshot) -> Option<Ordering> {
    match (a, b) {
        (V::Null, _) | (_, V::Null) => None,
        (V::Int(_) | V::Reg(..), V::Int(_) | V::Reg(..)) => Some(as_int(a)?.cmp(&as_int(b)?)),
        (V::Reg(k, _), V::Text(t)) | (V::Text(t), V::Reg(k, _)) => {
            let other = match t.trim().parse::<i64>() {
                Ok(n) => n,
                Err(_) => match k {
                    Reg::Class => s.relation_oid(t)?,
                    Reg::Type => type_by_name(t)?,
                    Reg::Namespace => namespace_oid(t)?,
                },
            };
            let mine = as_int(if matches!(a, V::Reg(..)) { a } else { b })?;
            let ord = mine.cmp(&other);
            Some(if matches!(a, V::Reg(..)) {
                ord
            } else {
                ord.reverse()
            })
        }
        (V::Int(_), V::Text(_)) | (V::Text(_), V::Int(_)) => match (as_int(a), as_int(b)) {
            (Some(x), Some(y)) => Some(x.cmp(&y)),
            _ => Some(render(a)?.cmp(&render(b)?)),
        },
        (V::Float(_), _) | (_, V::Float(_)) => {
            let f = |v: &V| match v {
                V::Float(f) => Some(*f),
                other => as_int(other).map(|i| i as f64),
            };
            f(a)?.partial_cmp(&f(b)?)
        }
        (V::Bool(_), _) | (_, V::Bool(_)) => Some(truth(a)?.cmp(&truth(b)?)),
        _ => Some(render(a)?.cmp(&render(b)?)),
    }
}

fn namespace_oid(name: &str) -> Option<i64> {
    match name.trim_matches('"') {
        "pg_catalog" => Some(PG_CATALOG),
        "public" => Some(PUBLIC),
        "information_schema" => Some(INFORMATION_SCHEMA),
        _ => None,
    }
}

fn namespace_name(oid: i64) -> Option<&'static str> {
    match oid {
        PG_CATALOG => Some("pg_catalog"),
        PUBLIC => Some("public"),
        INFORMATION_SCHEMA => Some("information_schema"),
        _ => None,
    }
}

/// A value as PostgreSQL's text output writes it; `None` is null.
fn render(v: &V) -> Option<String> {
    Some(match v {
        V::Null => return None,
        V::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
        V::Int(i) => i.to_string(),
        V::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                format!("{f}")
            }
        }
        V::Text(s) => s.clone(),
        V::Vector(items) => items
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" "),
        V::Array(items) => {
            let mut out = String::from("{");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                match render(item) {
                    None => out.push_str("NULL"),
                    Some(s) => {
                        let plain = !s.is_empty()
                            && !s.eq_ignore_ascii_case("null")
                            && !s.contains(|c: char| {
                                c == ','
                                    || c == '{'
                                    || c == '}'
                                    || c == '"'
                                    || c == '\\'
                                    || c.is_whitespace()
                            });
                        if plain {
                            out.push_str(&s);
                        } else {
                            out.push('"');
                            out.push_str(&s.replace('\\', "\\\\").replace('"', "\\\""));
                            out.push('"');
                        }
                    }
                }
            }
            out.push('}');
            out
        }
        V::Reg(_, oid) => oid.to_string(),
        V::Record(fields) => {
            let parts: Vec<String> = fields
                .iter()
                .map(|(_, v)| render(v).unwrap_or_default())
                .collect();
            format!("({})", parts.join(","))
        }
    })
}

/// [`render`], with a `regclass` or `regtype` printed as the name it points
/// to -- what PostgreSQL writes for `c.oid::regclass`.
fn render_named(v: &V, s: &Snapshot) -> Option<String> {
    match v {
        V::Reg(Reg::Class, oid) => Some(s.relation_name(*oid).unwrap_or_else(|| oid.to_string())),
        V::Reg(Reg::Type, oid) => Some(format_type(*oid, -1).unwrap_or_else(|| oid.to_string())),
        V::Reg(Reg::Namespace, oid) => {
            Some(namespace_name(*oid).map_or_else(|| oid.to_string(), str::to_string))
        }
        V::Array(items) if items.iter().any(|i| matches!(i, V::Reg(..))) => render(&V::Array(
            items
                .iter()
                .map(|i| render_named(i, s).map_or(V::Null, V::Text))
                .collect(),
        )),
        other => render(other),
    }
}

/// `{a,b,"c d"}` as items, for a cast of text to an array.
fn parse_array(s: &str) -> Option<Vec<V>> {
    let inner = s.trim().strip_prefix('{')?.strip_suffix('}')?;
    if inner.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut was_quoted) = (false, false);
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' if quoted => cur.push(chars.next()?),
            '"' => {
                quoted = !quoted;
                was_quoted = true;
            }
            ',' if !quoted => {
                out.push(if !was_quoted && cur.eq_ignore_ascii_case("null") {
                    V::Null
                } else {
                    V::Text(std::mem::take(&mut cur))
                });
                cur.clear();
                was_quoted = false;
            }
            c => cur.push(c),
        }
    }
    out.push(if !was_quoted && cur.eq_ignore_ascii_case("null") {
        V::Null
    } else {
        V::Text(cur)
    });
    Some(out)
}

fn cast(v: V, ty: &TypeName, s: &Snapshot) -> V {
    if ty.array {
        let items = match v {
            V::Null => return V::Null,
            V::Array(items) => items,
            V::Vector(items) => items.into_iter().map(V::Int).collect(),
            V::Text(text) => match parse_array(&text) {
                Some(items) => items,
                None => text.split_whitespace().map(t).collect(),
            },
            other => vec![other],
        };
        let base = TypeName {
            name: ty.name.clone(),
            array: false,
        };
        return V::Array(items.into_iter().map(|i| cast(i, &base, s)).collect());
    }
    match ty.name.as_str() {
        "text" | "name" | "varchar" | "char" | "bpchar" | "character" => {
            match render_named(&v, s) {
                Some(text) => V::Text(text),
                None => V::Null,
            }
        }
        "int" | "int2" | "int4" | "int8" | "integer" | "bigint" | "smallint" | "oid" | "xid" => {
            match v {
                V::Null => V::Null,
                other => as_int(&other).map_or(V::Null, V::Int),
            }
        }
        "bool" | "boolean" => truth(&v).map_or(V::Null, V::Bool),
        "float4" | "float8" | "real" | "numeric" | "decimal" => match v {
            V::Float(f) => V::Float(f),
            V::Text(text) => text.trim().parse().map_or(V::Null, V::Float),
            other => as_int(&other).map_or(V::Null, |i| V::Float(i as f64)),
        },
        "regclass" => match v {
            V::Text(name) => match name.trim().parse::<i64>() {
                Ok(n) => V::Reg(Reg::Class, n),
                Err(_) => s
                    .relation_oid(&name)
                    .map_or(V::Null, |o| V::Reg(Reg::Class, o)),
            },
            V::Null => V::Null,
            other => as_int(&other).map_or(V::Null, |o| V::Reg(Reg::Class, o)),
        },
        "regtype" => match v {
            V::Text(name) => type_by_name(&name).map_or(V::Null, |o| V::Reg(Reg::Type, o)),
            V::Null => V::Null,
            other => as_int(&other).map_or(V::Null, |o| V::Reg(Reg::Type, o)),
        },
        "regnamespace" => match v {
            V::Text(name) => namespace_oid(&name).map_or(V::Null, |o| V::Reg(Reg::Namespace, o)),
            V::Null => V::Null,
            other => as_int(&other).map_or(V::Null, |o| V::Reg(Reg::Namespace, o)),
        },
        // A function by name; only compared with another by name here.
        "regproc" | "regprocedure" => match render(&v) {
            Some(name) => t(name.trim_start_matches("pg_catalog.")),
            None => V::Null,
        },
        // Dates and the rest pass through unchanged.
        _ => v,
    }
}

/// The oid of the type a cast names, for the row description.
fn cast_type(ty: &TypeName) -> i32 {
    let base = match ty.name.as_str() {
        "bool" | "boolean" => BOOL,
        "int2" | "smallint" => INT2,
        "int4" | "int" | "integer" => INT4,
        "int8" | "bigint" => INT8,
        "oid" => OID,
        "name" => NAME,
        "char" => CHAR,
        "varchar" => VARCHAR,
        "float4" | "real" => FLOAT4,
        "float8" => FLOAT8,
        "regclass" => REGCLASS,
        "regtype" => REGTYPE,
        "regnamespace" => REGNAMESPACE,
        "timestamptz" | "timestamp" => TIMESTAMPTZ,
        "bytea" => BYTEA,
        _ => TEXT,
    };
    if !ty.array {
        return base;
    }
    match base {
        BOOL => BOOL_ARRAY,
        INT2 => INT2_ARRAY,
        INT4 => INT4_ARRAY,
        INT8 => INT8_ARRAY,
        OID => OID_ARRAY,
        NAME => NAME_ARRAY,
        _ => TEXT_ARRAY,
    }
}

fn like(text: &str, pattern: &str, insensitive: bool) -> bool {
    let fold = |s: &str| -> Vec<char> {
        if insensitive {
            s.chars().flat_map(char::to_lowercase).collect()
        } else {
            s.chars().collect()
        }
    };
    fn go(t: &[char], p: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some(('%', rest)) => (0..=t.len()).any(|i| go(&t[i..], rest)),
            Some(('_', rest)) => !t.is_empty() && go(&t[1..], rest),
            Some(('\\', rest)) if !rest.is_empty() => {
                !t.is_empty() && t[0] == rest[0] && go(&t[1..], &rest[1..])
            }
            Some((c, rest)) => !t.is_empty() && t[0] == *c && go(&t[1..], rest),
        }
    }
    go(&fold(text), &fold(pattern))
}

const AGGREGATES: [&str; 9] = [
    "count",
    "sum",
    "max",
    "min",
    "string_agg",
    "array_agg",
    "bool_or",
    "bool_and",
    "every",
];

fn has_aggregate(e: &Expr) -> bool {
    match e {
        Expr::Call { name, args, .. } => {
            AGGREGATES.contains(&name.as_str()) || args.iter().any(has_aggregate)
        }
        Expr::Case {
            operand,
            arms,
            otherwise,
        } => {
            operand.as_deref().is_some_and(has_aggregate)
                || arms
                    .iter()
                    .any(|(a, b)| has_aggregate(a) || has_aggregate(b))
                || otherwise.as_deref().is_some_and(has_aggregate)
        }
        Expr::Cast(a, _) | Expr::Unary(_, a) | Expr::Not(a) | Expr::IsNull(a, _) => {
            has_aggregate(a)
        }
        Expr::Binary(_, a, b)
        | Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Like(a, b, ..)
        | Expr::Index(a, b)
        | Expr::Quantified(_, a, b, _) => has_aggregate(a) || has_aggregate(b),
        _ => false,
    }
}

fn eval(e: &Expr, scope: &Scope, ctx: &Ctx) -> Out<V> {
    Ok(match e {
        Expr::Null => V::Null,
        Expr::Bool(b) => V::Bool(*b),
        Expr::Int(i) => V::Int(*i),
        Expr::Str(s) => V::Text(s.clone()),
        Expr::Param(n) => ctx.params.get(n - 1).cloned().unwrap_or(V::Null),
        Expr::Column(q, name) => column(scope, q.as_deref(), name),
        Expr::Call {
            name,
            args,
            star,
            distinct,
        } if AGGREGATES.contains(&name.as_str()) => {
            aggregate(name, args, *star, *distinct, scope, ctx)?
        }
        Expr::Call { name, args, .. } => {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval(a, scope, ctx)?);
            }
            call(name, vals, ctx)?
        }
        Expr::Case {
            operand,
            arms,
            otherwise,
        } => {
            let subject = match operand {
                Some(o) => Some(eval(o, scope, ctx)?),
                None => None,
            };
            for (when, then) in arms {
                let w = eval(when, scope, ctx)?;
                let hit = match &subject {
                    Some(sv) => compare(sv, &w, ctx.snap) == Some(Ordering::Equal),
                    None => truth(&w) == Some(true),
                };
                if hit {
                    return eval(then, scope, ctx);
                }
            }
            match otherwise {
                Some(o) => eval(o, scope, ctx)?,
                None => V::Null,
            }
        }
        Expr::Cast(inner, ty) => cast(eval(inner, scope, ctx)?, ty, ctx.snap),
        Expr::Unary(_, inner) => match eval(inner, scope, ctx)? {
            V::Int(i) => V::Int(-i),
            V::Float(f) => V::Float(-f),
            _ => V::Null,
        },
        Expr::Not(inner) => truth(&eval(inner, scope, ctx)?).map_or(V::Null, |b| V::Bool(!b)),
        Expr::And(a, b) => {
            let x = truth(&eval(a, scope, ctx)?);
            if x == Some(false) {
                return Ok(V::Bool(false));
            }
            match (x, truth(&eval(b, scope, ctx)?)) {
                (_, Some(false)) => V::Bool(false),
                (Some(true), Some(true)) => V::Bool(true),
                _ => V::Null,
            }
        }
        Expr::Or(a, b) => {
            let x = truth(&eval(a, scope, ctx)?);
            if x == Some(true) {
                return Ok(V::Bool(true));
            }
            match (x, truth(&eval(b, scope, ctx)?)) {
                (_, Some(true)) => V::Bool(true),
                (Some(false), Some(false)) => V::Bool(false),
                _ => V::Null,
            }
        }
        Expr::IsNull(inner, negated) => V::Bool((eval(inner, scope, ctx)? == V::Null) != *negated),
        Expr::Binary(op, a, b) => {
            let (x, y) = (eval(a, scope, ctx)?, eval(b, scope, ctx)?);
            binary(op, &x, &y, ctx.snap)
        }
        Expr::InList(subject, list, negated) => {
            let x = eval(subject, scope, ctx)?;
            let mut values = Vec::with_capacity(list.len());
            for item in list {
                values.push(eval(item, scope, ctx)?);
            }
            member(&x, &values, *negated, ctx.snap)
        }
        Expr::InQuery(subject, q, negated) => {
            let x = eval(subject, scope, ctx)?;
            let out = run(q, Some(scope), ctx)?;
            let values: Vec<V> = out
                .rows
                .into_iter()
                .filter_map(|r| r.into_iter().next())
                .collect();
            member(&x, &values, *negated, ctx.snap)
        }
        Expr::Quantified(op, a, b, all) => {
            let x = eval(a, scope, ctx)?;
            let items = match eval(b, scope, ctx)? {
                V::Array(items) => items,
                V::Vector(items) => items.into_iter().map(V::Int).collect(),
                V::Text(text) => parse_array(&text).unwrap_or_default(),
                V::Null => return Ok(V::Null),
                other => vec![other],
            };
            let mut seen_null = false;
            for item in &items {
                match truth(&binary(op, &x, item, ctx.snap)) {
                    Some(true) if !*all => return Ok(V::Bool(true)),
                    Some(false) if *all => return Ok(V::Bool(false)),
                    None => seen_null = true,
                    _ => {}
                }
            }
            if seen_null {
                V::Null
            } else {
                V::Bool(*all)
            }
        }
        Expr::Like(a, b, insensitive, negated) => {
            match (eval(a, scope, ctx)?, eval(b, scope, ctx)?) {
                (V::Null, _) | (_, V::Null) => V::Null,
                (x, y) => {
                    let hit = like(
                        &render_named(&x, ctx.snap).unwrap_or_default(),
                        &render(&y).unwrap_or_default(),
                        *insensitive,
                    );
                    V::Bool(hit != *negated)
                }
            }
        }
        Expr::Between(x, lo, hi, negated) => {
            let v = eval(x, scope, ctx)?;
            let (l, h) = (eval(lo, scope, ctx)?, eval(hi, scope, ctx)?);
            match (compare(&v, &l, ctx.snap), compare(&v, &h, ctx.snap)) {
                (Some(a), Some(b)) => V::Bool((a.is_ge() && b.is_le()) != *negated),
                _ => V::Null,
            }
        }
        Expr::Subquery(q) => {
            let out = run(q, Some(scope), ctx)?;
            out.rows
                .into_iter()
                .next()
                .and_then(|r| r.into_iter().next())
                .unwrap_or(V::Null)
        }
        Expr::Exists(q) => V::Bool(!run(q, Some(scope), ctx)?.rows.is_empty()),
        Expr::ArrayQuery(q) => V::Array(
            run(q, Some(scope), ctx)?
                .rows
                .into_iter()
                .filter_map(|r| r.into_iter().next())
                .collect(),
        ),
        Expr::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for i in items {
                out.push(eval(i, scope, ctx)?);
            }
            V::Array(out)
        }
        Expr::Field(inner, name) => match eval(inner, scope, ctx)? {
            V::Record(fields) => fields
                .into_iter()
                .find(|(f, _)| f == name)
                .map_or(V::Null, |(_, v)| v),
            _ => V::Null,
        },
        Expr::Slot(k) => scope.slots.get(*k).cloned().unwrap_or(V::Null),
        Expr::Window { .. } => {
            return Err(Unsupported(
                "a window function outside the select list".into(),
            ))
        }
        Expr::Index(arr, idx) => {
            let i = as_int(&eval(idx, scope, ctx)?);
            match (eval(arr, scope, ctx)?, i) {
                // Arrays count from 1, the vector types from 0.
                (V::Array(items), Some(i)) if i >= 1 => {
                    items.get(i as usize - 1).cloned().unwrap_or(V::Null)
                }
                (V::Vector(items), Some(i)) if i >= 0 => {
                    items.get(i as usize).map_or(V::Null, |n| V::Int(*n))
                }
                _ => V::Null,
            }
        }
    })
}

fn member(x: &V, values: &[V], negated: bool, s: &Snapshot) -> V {
    if *x == V::Null {
        return V::Null;
    }
    let mut seen_null = false;
    for v in values {
        match compare(x, v, s) {
            Some(Ordering::Equal) => return V::Bool(!negated),
            None => seen_null = true,
            _ => {}
        }
    }
    if seen_null {
        V::Null
    } else {
        V::Bool(negated)
    }
}

fn binary(op: &str, x: &V, y: &V, s: &Snapshot) -> V {
    let ord = || compare(x, y, s);
    let b = |o: Option<bool>| o.map_or(V::Null, V::Bool);
    match op {
        "=" => b(ord().map(|o| o == Ordering::Equal)),
        "<>" => b(ord().map(|o| o != Ordering::Equal)),
        "<" => b(ord().map(|o| o == Ordering::Less)),
        "<=" => b(ord().map(|o| o != Ordering::Greater)),
        ">" => b(ord().map(|o| o == Ordering::Greater)),
        ">=" => b(ord().map(|o| o != Ordering::Less)),
        "~" | "!~" | "~*" | "!~*" => {
            let (Some(text), Some(pattern)) = (render_named(x, s), render(y)) else {
                return V::Null;
            };
            let insensitive = op.ends_with('*');
            match regex::Regex::new(&pattern, insensitive) {
                Some(re) => V::Bool(re.is_match(&text) != op.starts_with('!')),
                None => V::Null,
            }
        }
        "||" => match (render_named(x, s), render_named(y, s)) {
            (Some(a), Some(c)) => V::Text(a + &c),
            _ => V::Null,
        },
        "+" | "-" | "*" | "/" | "%" | "&" | "|" | "<<" | ">>" => match (as_int(x), as_int(y)) {
            (Some(a), Some(c)) => match op {
                "+" => V::Int(a.wrapping_add(c)),
                "-" => V::Int(a.wrapping_sub(c)),
                "*" => V::Int(a.wrapping_mul(c)),
                "&" => V::Int(a & c),
                "|" => V::Int(a | c),
                "<<" => V::Int(a.wrapping_shl(c as u32)),
                ">>" => V::Int(a.wrapping_shr(c as u32)),
                "/" if c != 0 => V::Int(a / c),
                "%" if c != 0 => V::Int(a % c),
                _ => V::Null,
            },
            _ => V::Null,
        },
        _ => V::Null,
    }
}

fn aggregate(
    name: &str,
    args: &[Expr],
    star: bool,
    distinct: bool,
    scope: &Scope,
    ctx: &Ctx,
) -> Out<V> {
    let single = [scope.row.to_vec()];
    let rows: &[Vec<Option<usize>>] = scope.group.unwrap_or(&single);
    let mut values = Vec::with_capacity(rows.len());
    for row in rows {
        let s = Scope {
            rels: scope.rels,
            row,
            group: None,
            slots: scope.slots,
            outer: scope.outer,
        };
        values.push(match args.first() {
            Some(a) if !star => eval(a, &s, ctx)?,
            _ => V::Int(1),
        });
    }
    if name != "count" || !star {
        values.retain(|v| *v != V::Null);
    }
    if distinct {
        let mut seen: Vec<V> = Vec::new();
        values.retain(|v| {
            if seen.contains(v) {
                false
            } else {
                seen.push(v.clone());
                true
            }
        });
    }
    Ok(match name {
        "count" => V::Int(values.len() as i64),
        "sum" => {
            if values.is_empty() {
                V::Null
            } else {
                V::Int(values.iter().filter_map(as_int).sum())
            }
        }
        "max" | "min" => {
            let mut best: Option<V> = None;
            for v in values {
                let better = match &best {
                    None => true,
                    Some(b) => {
                        let o = compare(&v, b, ctx.snap);
                        if name == "max" {
                            o == Some(Ordering::Greater)
                        } else {
                            o == Some(Ordering::Less)
                        }
                    }
                };
                if better {
                    best = Some(v);
                }
            }
            best.unwrap_or(V::Null)
        }
        "string_agg" => {
            if values.is_empty() {
                V::Null
            } else {
                let sep = match args.get(1) {
                    Some(e) => render(&eval(e, scope, ctx)?).unwrap_or_default(),
                    None => String::new(),
                };
                V::Text(
                    values
                        .iter()
                        .filter_map(|v| render_named(v, ctx.snap))
                        .collect::<Vec<_>>()
                        .join(&sep),
                )
            }
        }
        "array_agg" => {
            if values.is_empty() {
                V::Null
            } else {
                V::Array(values)
            }
        }
        _ => {
            // bool_or, bool_and, every
            if values.is_empty() {
                V::Null
            } else {
                let or = name == "bool_or";
                V::Bool(if or {
                    values.iter().any(|v| truth(v) == Some(true))
                } else {
                    values.iter().all(|v| truth(v) == Some(true))
                })
            }
        }
    })
}

/// The scalar functions tools call, answered from the snapshot. One not
/// here is null rather than an error: an introspection query then still
/// returns its rows, with that one value missing.
fn call(name: &str, args: Vec<V>, ctx: &Ctx) -> Out<V> {
    let s = ctx.snap;
    let arg = |i: usize| args.get(i).cloned().unwrap_or(V::Null);
    let int = |i: usize| as_int(&arg(i));
    Ok(match name {
        "pg_get_userbyid" => match int(0) {
            Some(ROLE) => t("fenec"),
            Some(n) => t(format!("unknown (OID={n})")),
            None => V::Null,
        },
        "pg_table_is_visible"
        | "pg_type_is_visible"
        | "pg_function_is_visible"
        | "has_table_privilege"
        | "has_schema_privilege"
        | "has_database_privilege"
        | "has_column_privilege"
        | "has_any_column_privilege"
        | "has_function_privilege"
        | "has_sequence_privilege"
        | "pg_has_role" => V::Bool(true),
        "pg_relation_is_publishable" => V::Bool(false),
        "format_type" => match int(0) {
            Some(oid) => format_type(oid, int(1).unwrap_or(-1)).map_or_else(|| t("???"), V::Text),
            None => V::Null,
        },
        "pg_get_indexdef" => {
            let oid = int(0);
            let column = int(1).unwrap_or(0);
            let mut out = V::Null;
            for tb in &s.tables {
                if let Some(i) = tb.indexes().into_iter().find(|i| Some(i.oid) == oid) {
                    out = if column > 0 {
                        t(i.column.clone())
                    } else {
                        t(index_def(tb, &i))
                    };
                }
            }
            out
        }
        "pg_get_constraintdef" => {
            if s.tables.iter().any(|tb| Some(tb.oid + 512) == int(0)) {
                t("PRIMARY KEY (id)")
            } else {
                V::Null
            }
        }
        "pg_encoding_to_char" => t("UTF8"),
        "pg_char_to_encoding" => V::Int(6),
        "current_schema" => t("public"),
        "current_schemas" => V::Array(vec![t("public")]),
        "current_database" | "current_catalog" => t(&s.database),
        "current_user" | "session_user" | "user" => t("fenec"),
        "version" => t(format!(
            "PostgreSQL {} on {}, fenecdb query language: FenecQL",
            s.version,
            std::env::consts::ARCH
        )),
        "current_setting" => {
            let want = render(&arg(0)).unwrap_or_default();
            settings(s)
                .into_iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(&want))
                .map_or(V::Null, |(_, v)| t(v))
        }
        "set_config" => arg(1),
        "array_to_string" => match arg(0) {
            V::Array(items) => {
                let sep = render(&arg(1)).unwrap_or_default();
                let null = render(&arg(2));
                V::Text(
                    items
                        .iter()
                        .filter_map(|v| render_named(v, s).or_else(|| null.clone()))
                        .collect::<Vec<_>>()
                        .join(&sep),
                )
            }
            V::Null => V::Null,
            other => V::Text(render_named(&other, s).unwrap_or_default()),
        },
        "array_length" | "array_upper" | "cardinality" => match arg(0) {
            V::Array(items) if !items.is_empty() => V::Int(items.len() as i64),
            V::Vector(items) if !items.is_empty() => {
                // An int2vector is subscripted from 0.
                V::Int(items.len() as i64 - (name == "array_upper") as i64)
            }
            _ => V::Null,
        },
        "array_lower" => match arg(0) {
            V::Array(items) if !items.is_empty() => V::Int(1),
            V::Vector(items) if !items.is_empty() => V::Int(0),
            _ => V::Null,
        },
        "to_regclass" => match render(&arg(0)) {
            Some(n) => s
                .relation_oid(&n)
                .map_or(V::Null, |o| V::Reg(Reg::Class, o)),
            None => V::Null,
        },
        "pg_relation_size" | "pg_table_size" | "pg_total_relation_size" => {
            let oid = int(0);
            s.tables
                .iter()
                .find(|tb| Some(tb.oid) == oid)
                .map_or(V::Int(0), |tb| V::Int(tb.bytes as i64))
        }
        "pg_indexes_size" => V::Int(0),
        "pg_size_pretty" => match int(0) {
            Some(n) => {
                let mut v = n as f64;
                let mut unit = "bytes";
                for u in ["kB", "MB", "GB", "TB"] {
                    if v.abs() < 10.0 * 1024.0 {
                        break;
                    }
                    v /= 1024.0;
                    unit = u;
                }
                t(format!("{} {unit}", v.round() as i64))
            }
            None => V::Null,
        },
        "pg_backend_pid" => V::Int(std::process::id() as i64),
        "quote_ident" => match render(&arg(0)) {
            Some(n)
                if n.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') =>
            {
                t(n)
            }
            Some(n) => t(format!("\"{}\"", n.replace('"', "\"\""))),
            None => V::Null,
        },
        "lower" => render(&arg(0)).map_or(V::Null, |x| t(x.to_lowercase())),
        "upper" => render(&arg(0)).map_or(V::Null, |x| t(x.to_uppercase())),
        "length" | "char_length" => {
            render(&arg(0)).map_or(V::Null, |x| V::Int(x.chars().count() as i64))
        }
        "coalesce" => args.into_iter().find(|v| *v != V::Null).unwrap_or(V::Null),
        "nullif" => {
            if compare(&arg(0), &arg(1), s) == Some(Ordering::Equal) {
                V::Null
            } else {
                arg(0)
            }
        }
        "concat" => t(args
            .iter()
            .filter_map(|v| render_named(v, s))
            .collect::<String>()),
        "btrim" | "ltrim" | "rtrim" => match render_named(&arg(0), s) {
            Some(text) => {
                let chars: Vec<char> = render(&arg(1))
                    .unwrap_or_else(|| " ".into())
                    .chars()
                    .collect();
                let cut = |c: char| chars.contains(&c);
                t(match name {
                    "ltrim" => text.trim_start_matches(cut),
                    "rtrim" => text.trim_end_matches(cut),
                    _ => text.trim_matches(cut),
                })
            }
            None => V::Null,
        },
        // A set-returning function used where one value is wanted.
        "_pg_expandarray" | "unnest" | "generate_series" => {
            expand(name, &args).into_iter().next().unwrap_or(V::Null)
        }
        "replace" => match (render(&arg(0)), render(&arg(1)), render(&arg(2))) {
            (Some(a), Some(b), Some(c)) => t(a.replace(&b, &c)),
            _ => V::Null,
        },
        _ => V::Null,
    })
}

// ------------------------------------------------------------------ queries

/// A query's result: columns with their types, and rows.
struct Output {
    columns: Vec<(String, i32)>,
    rows: Vec<Vec<V>>,
}

/// The rows a set-returning function gives: `_pg_expandarray`'s `(x, n)`
/// pairs, `unnest`'s items, `generate_series`'s numbers.
fn expand(name: &str, args: &[V]) -> Vec<V> {
    let items = |v: Option<&V>| -> Vec<V> {
        match v {
            Some(V::Array(items)) => items.clone(),
            Some(V::Vector(items)) => items.iter().map(|i| V::Int(*i)).collect(),
            Some(V::Text(text)) => parse_array(text).unwrap_or_default(),
            _ => Vec::new(),
        }
    };
    match name {
        "_pg_expandarray" => items(args.first())
            .into_iter()
            .enumerate()
            .map(|(i, x)| V::Record(vec![("x", x), ("n", V::Int(i as i64 + 1))]))
            .collect(),
        "unnest" => items(args.first()),
        "generate_series" => {
            let (Some(lo), Some(hi)) =
                (args.first().and_then(as_int), args.get(1).and_then(as_int))
            else {
                return Vec::new();
            };
            let step = args.get(2).and_then(as_int).unwrap_or(1).max(1);
            // A catalog never asks for more; a runaway series would.
            (lo..=hi.min(lo.saturating_add(100_000)))
                .step_by(step as usize)
                .map(V::Int)
                .collect()
        }
        _ => Vec::new(),
    }
}

const SET_RETURNING: [&str; 3] = ["_pg_expandarray", "unnest", "generate_series"];

fn source(src: &Source, outer: Option<&Scope>, ctx: &Ctx) -> Out<Rel> {
    Ok(match src {
        Source::Table {
            schema,
            name,
            alias,
        } => catalog_table(
            schema.as_deref(),
            name,
            alias.as_deref().unwrap_or(name),
            ctx.snap,
        ),
        Source::Query { query, alias } => {
            let out = run(query, outer, ctx)?;
            Rel {
                alias: alias.clone(),
                columns: out.columns,
                rows: out.rows,
            }
        }
        Source::Function {
            name,
            args,
            alias,
            columns,
        } => {
            let alias = alias.clone().unwrap_or_else(|| name.clone());
            let mut vals = Vec::new();
            for a in args {
                vals.push(eval(a, outer.unwrap_or(&EMPTY), ctx)?);
            }
            let rows = expand(name, &vals);
            // `_pg_expandarray` gives two columns; the others one, named
            // after the alias unless a column list names it.
            if name == "_pg_expandarray" {
                let names = [
                    columns.first().map_or("x", String::as_str),
                    columns.get(1).map_or("n", String::as_str),
                ];
                Rel {
                    columns: vec![(names[0].to_string(), INT4), (names[1].to_string(), INT4)],
                    alias,
                    rows: rows
                        .into_iter()
                        .map(|r| match r {
                            V::Record(f) => f.into_iter().map(|(_, v)| v).collect(),
                            other => vec![other],
                        })
                        .collect(),
                }
            } else {
                let column = columns.first().cloned().unwrap_or_else(|| alias.clone());
                Rel {
                    columns: vec![(
                        column,
                        if name == "generate_series" {
                            INT4
                        } else {
                            TEXT
                        },
                    )],
                    alias,
                    rows: rows.into_iter().map(|v| vec![v]).collect(),
                }
            }
        }
    })
}

/// Collects the relations a condition reads at this level into `out`;
/// `false` when it cannot tell -- a subquery, a window -- and the condition
/// waits for every relation. Every kind of expression is named here: one
/// passed over as reading nothing would be taken for a constant, and a join
/// key made of it matched nothing.
fn aliases(e: &Expr, rels: &[Rel], out: &mut Vec<usize>) -> bool {
    match e {
        Expr::Column(q, name) => {
            let found = rels.iter().position(|r| match q {
                Some(q) => r.alias == *q,
                None => r.columns.iter().any(|(c, _)| c == name),
            });
            if let Some(i) = found {
                if !out.contains(&i) {
                    out.push(i);
                }
            }
            true
        }
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Str(_)
        | Expr::Param(_)
        | Expr::Slot(_) => true,
        Expr::Subquery(_)
        | Expr::Exists(_)
        | Expr::ArrayQuery(_)
        | Expr::InQuery(..)
        | Expr::Window { .. } => false,
        Expr::Call { args, .. } => args.iter().all(|a| aliases(a, rels, out)),
        Expr::Array(items) => items.iter().all(|a| aliases(a, rels, out)),
        Expr::Case {
            operand,
            arms,
            otherwise,
        } => {
            operand.as_deref().is_none_or(|o| aliases(o, rels, out))
                && arms
                    .iter()
                    .all(|(a, b)| aliases(a, rels, out) && aliases(b, rels, out))
                && otherwise.as_deref().is_none_or(|o| aliases(o, rels, out))
        }
        Expr::Cast(a, _)
        | Expr::Unary(_, a)
        | Expr::Not(a)
        | Expr::IsNull(a, _)
        | Expr::Field(a, _) => aliases(a, rels, out),
        Expr::Binary(_, a, b)
        | Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Like(a, b, ..)
        | Expr::Index(a, b)
        | Expr::Quantified(_, a, b, _) => aliases(a, rels, out) && aliases(b, rels, out),
        Expr::InList(a, list, _) => {
            aliases(a, rels, out) && list.iter().all(|x| aliases(x, rels, out))
        }
        Expr::Between(a, b, c, _) => {
            aliases(a, rels, out) && aliases(b, rels, out) && aliases(c, rels, out)
        }
    }
}

/// A value as a join key: equal keys for the values `compare` finds equal
/// -- an oid and the text of its number, say -- and none for a null.
fn hash_key(v: &V) -> Option<String> {
    match v {
        V::Null => None,
        V::Int(i) | V::Reg(_, i) => Some(i.to_string()),
        V::Text(t) => Some(match t.trim().parse::<i64>() {
            Ok(i) => i.to_string(),
            Err(_) => t.clone(),
        }),
        V::Float(f) if f.fract() == 0.0 => Some((*f as i64).to_string()),
        other => render(other),
    }
}

fn conjuncts(e: Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::And(a, b) => {
            conjuncts(*a, out);
            conjuncts(*b, out);
        }
        other => out.push(other),
    }
}

fn holds(
    e: &Expr,
    rels: &[Rel],
    row: &[Option<usize>],
    outer: Option<&Scope>,
    ctx: &Ctx,
) -> Out<bool> {
    let scope = Scope {
        rels,
        row,
        group: None,
        slots: &[],
        outer,
    };
    Ok(truth(&eval(e, &scope, ctx)?) == Some(true))
}

/// The joined rows of a select's `FROM`, its `WHERE` applied as early as
/// each condition's relations are bound: a join of three catalog tables
/// would otherwise build their whole cross product first.
fn joined(sel: &Select, outer: Option<&Scope>, ctx: &Ctx) -> Out<(Vec<Rel>, Vec<Bound>)> {
    enum Step {
        Base,
        Join(JoinKind, Option<Expr>),
    }
    let mut rels: Vec<Rel> = Vec::new();
    let mut steps = Vec::new();
    for item in &sel.from {
        rels.push(source(&item.first, outer, ctx)?);
        steps.push(Step::Base);
        for j in &item.joins {
            let right = source(&j.source, outer, ctx)?;
            // `USING (c)`: the column of the first relation on the left that
            // has one, equal to the right's.
            let mut on = j.on.clone();
            for c in &j.using {
                let left = rels
                    .iter()
                    .find(|r| r.columns.iter().any(|(n, _)| n == c))
                    .map(|r| r.alias.clone());
                let cond = match left {
                    Some(l) => Expr::Binary(
                        "=",
                        Box::new(Expr::Column(Some(l), c.clone())),
                        Box::new(Expr::Column(Some(right.alias.clone()), c.clone())),
                    ),
                    None => Expr::Bool(false),
                };
                on = Some(match on {
                    Some(prev) => Expr::And(Box::new(prev), Box::new(cond)),
                    None => cond,
                });
            }
            rels.push(right);
            steps.push(Step::Join(j.kind, on));
        }
    }
    let mut pending: Vec<(Expr, Option<Vec<usize>>)> = Vec::new();
    if let Some(f) = sel.filter.clone() {
        let mut parts = Vec::new();
        conjuncts(f, &mut parts);
        for p in parts {
            let mut used = Vec::new();
            let known = aliases(&p, &rels, &mut used);
            pending.push((p, known.then_some(used)));
        }
    }

    let mut rows: Vec<Vec<Option<usize>>> = vec![Vec::new()];
    for (k, step) in steps.iter().enumerate() {
        let n = rels[k].rows.len();
        // Equalities between this relation and those already bound -- from
        // the join's own condition, or from the WHERE for a comma join --
        // find its matching rows by key instead of trying every one. Over a
        // thousand collections, trying every pair took psql's index query
        // for `\d` 3.9 s and JDBC's column query 18.1 s; by key, 18 ms and
        // 122 ms.
        let mut conds: Vec<Expr> = Vec::new();
        match step {
            Step::Join(_, Some(on)) => conjuncts(on.clone(), &mut conds),
            Step::Base => conds.extend(
                pending
                    .iter()
                    .filter(|(_, used)| {
                        used.as_ref()
                            .is_some_and(|u| u.contains(&k) && u.iter().all(|x| *x <= k))
                    })
                    .map(|(e, _)| e.clone()),
            ),
            Step::Join(_, None) => {}
        }
        let mut keys: Vec<(Expr, Expr)> = Vec::new();
        for c in &conds {
            let Expr::Binary("=", a, b) = c else {
                continue;
            };
            let side = |e: &Expr| -> Option<bool> {
                let mut used = Vec::new();
                if !aliases(e, &rels, &mut used) {
                    return None;
                }
                if used == [k] {
                    Some(true)
                } else if used.iter().all(|u| *u < k) {
                    Some(false)
                } else {
                    None
                }
            };
            match (side(a), side(b)) {
                (Some(false), Some(true)) => keys.push(((**a).clone(), (**b).clone())),
                (Some(true), Some(false)) => keys.push(((**b).clone(), (**a).clone())),
                _ => {}
            }
        }
        let index: Option<HashMap<Vec<String>, Vec<usize>>> = if keys.is_empty() {
            None
        } else {
            let mut ix: HashMap<Vec<String>, Vec<usize>> = HashMap::new();
            let mut alone = vec![None; k];
            alone.push(None);
            'rows: for ri in 0..n {
                alone[k] = Some(ri);
                let scope = Scope {
                    rels: &rels,
                    row: &alone,
                    group: None,
                    slots: &[],
                    outer,
                };
                let mut key = Vec::with_capacity(keys.len());
                for (_, mine) in &keys {
                    match hash_key(&eval(mine, &scope, ctx)?) {
                        Some(part) => key.push(part),
                        // A null equals nothing.
                        None => continue 'rows,
                    }
                }
                ix.entry(key).or_default().push(ri);
            }
            Some(ix)
        };
        let all: Vec<usize> = (0..n).collect();
        let mut next = Vec::new();
        for row in &rows {
            let candidates: &[usize] = match &index {
                None => &all,
                Some(ix) => {
                    let scope = Scope {
                        rels: &rels,
                        row,
                        group: None,
                        slots: &[],
                        outer,
                    };
                    let mut key = Vec::with_capacity(keys.len());
                    let mut null = false;
                    for (theirs, _) in &keys {
                        match hash_key(&eval(theirs, &scope, ctx)?) {
                            Some(part) => key.push(part),
                            None => null = true,
                        }
                    }
                    if null {
                        &[]
                    } else {
                        ix.get(&key).map_or(&[][..], Vec::as_slice)
                    }
                }
            };
            let mut matched = false;
            for &ri in candidates {
                let mut candidate = row.clone();
                candidate.push(Some(ri));
                let keep = match step {
                    Step::Join(_, Some(on)) => holds(on, &rels, &candidate, outer, ctx)?,
                    _ => true,
                };
                if keep {
                    matched = true;
                    next.push(candidate);
                }
            }
            if !matched && matches!(step, Step::Join(JoinKind::Left, _)) {
                let mut padded = row.clone();
                padded.push(None);
                next.push(padded);
            }
        }
        rows = next;
        // Every condition whose relations are now all bound.
        let bound = k + 1;
        let mut i = 0;
        while i < pending.len() {
            let ready = match &pending[i].1 {
                Some(used) => used.iter().all(|u| *u < bound),
                None => bound == rels.len(),
            };
            if ready {
                let (cond, _) = pending.remove(i);
                let mut kept = Vec::with_capacity(rows.len());
                for row in rows {
                    if holds(&cond, &rels, &row, outer, ctx)? {
                        kept.push(row);
                    }
                }
                rows = kept;
            } else {
                i += 1;
            }
        }
    }
    // A select with no FROM has one empty row; its conditions run here.
    for (cond, _) in pending {
        let mut kept = Vec::new();
        for row in rows {
            if holds(&cond, &rels, &row, outer, ctx)? {
                kept.push(row);
            }
        }
        rows = kept;
    }
    Ok((rels, rows))
}

/// The name a result column takes when the query gives none.
fn column_name(e: &Expr) -> String {
    match e {
        Expr::Column(_, n) => n.clone(),
        Expr::Call { name, .. } => name.clone(),
        Expr::Cast(inner, ty) => match **inner {
            Expr::Column(..) | Expr::Call { .. } => column_name(inner),
            _ => ty.name.clone(),
        },
        Expr::Case { .. } => "case".into(),
        Expr::Subquery(q) => match q.first.items.first() {
            Some(Item::Expr(e, alias)) => alias.clone().unwrap_or_else(|| column_name(e)),
            _ => "?column?".into(),
        },
        Expr::Exists(_) => "exists".into(),
        Expr::ArrayQuery(_) | Expr::Array(_) => "array".into(),
        _ => "?column?".into(),
    }
}

fn expr_type(e: &Expr, rels: &[Rel]) -> i32 {
    match e {
        Expr::Bool(_) => BOOL,
        Expr::Int(_) => INT4,
        Expr::Column(q, n) => column_type(rels, q.as_deref(), n),
        Expr::Cast(_, ty) => cast_type(ty),
        Expr::Case {
            arms, otherwise, ..
        } => arms
            .first()
            .map(|(_, v)| v)
            .or(otherwise.as_deref())
            .map_or(TEXT, |v| expr_type(v, rels)),
        Expr::Call { name, args, .. } => match name.as_str() {
            "count"
            | "sum"
            | "pg_relation_size"
            | "pg_table_size"
            | "pg_total_relation_size"
            | "pg_indexes_size" => INT8,
            "max" | "min" | "coalesce" | "nullif" => {
                args.first().map_or(TEXT, |a| expr_type(a, rels))
            }
            "array_length" | "array_upper" | "array_lower" | "cardinality" | "length"
            | "char_length" | "pg_backend_pid" => INT4,
            "pg_table_is_visible"
            | "pg_type_is_visible"
            | "pg_function_is_visible"
            | "pg_relation_is_publishable"
            | "bool_or"
            | "bool_and"
            | "every" => BOOL,
            n if n.starts_with("has_") || n == "pg_has_role" => BOOL,
            "pg_get_userbyid"
            | "current_schema"
            | "current_database"
            | "current_catalog"
            | "current_user"
            | "session_user"
            | "user"
            | "pg_encoding_to_char" => NAME,
            "to_regclass" => REGCLASS,
            "current_schemas" => NAME_ARRAY,
            _ => TEXT,
        },
        Expr::And(..)
        | Expr::Or(..)
        | Expr::Not(_)
        | Expr::IsNull(..)
        | Expr::InList(..)
        | Expr::InQuery(..)
        | Expr::Quantified(..)
        | Expr::Like(..)
        | Expr::Between(..)
        | Expr::Exists(_) => BOOL,
        Expr::Binary(op, a, _) => match *op {
            "+" | "-" | "*" | "/" | "%" => expr_type(a, rels),
            "||" => TEXT,
            _ => BOOL,
        },
        Expr::Unary(_, a) => expr_type(a, rels),
        Expr::Window { .. } => INT8,
        _ => TEXT,
    }
}

/// A value a select-list expression needs before its row is evaluated.
enum Lifted {
    /// `name() OVER (PARTITION BY .. ORDER BY ..)`, computed over all rows.
    Window(String, Vec<Expr>, Vec<sql::Order>),
    /// A set-returning call: each of its values makes a row of its own.
    Set(String, Vec<Expr>),
}

/// Takes window functions and set-returning calls out of an expression,
/// each replaced by the slot its value will be in.
fn lift(e: &Expr, out: &mut Vec<Lifted>) -> Expr {
    let boxed = |x: &Expr, out: &mut Vec<Lifted>| Box::new(lift(x, out));
    match e {
        Expr::Window {
            name,
            partition,
            order,
        } => {
            out.push(Lifted::Window(
                name.clone(),
                partition.clone(),
                order.clone(),
            ));
            Expr::Slot(out.len() - 1)
        }
        Expr::Call { name, args, .. } if SET_RETURNING.contains(&name.as_str()) => {
            out.push(Lifted::Set(name.clone(), args.clone()));
            Expr::Slot(out.len() - 1)
        }
        Expr::Call {
            name,
            args,
            star,
            distinct,
        } => Expr::Call {
            name: name.clone(),
            args: args.iter().map(|a| lift(a, out)).collect(),
            star: *star,
            distinct: *distinct,
        },
        Expr::Field(inner, f) => Expr::Field(boxed(inner, out), f.clone()),
        Expr::Cast(inner, ty) => Expr::Cast(boxed(inner, out), ty.clone()),
        Expr::Unary(op, a) => Expr::Unary(op, boxed(a, out)),
        Expr::Not(a) => Expr::Not(boxed(a, out)),
        Expr::IsNull(a, n) => Expr::IsNull(boxed(a, out), *n),
        Expr::Binary(op, a, b) => {
            let a = boxed(a, out);
            Expr::Binary(op, a, boxed(b, out))
        }
        Expr::And(a, b) => {
            let a = boxed(a, out);
            Expr::And(a, boxed(b, out))
        }
        Expr::Or(a, b) => {
            let a = boxed(a, out);
            Expr::Or(a, boxed(b, out))
        }
        Expr::Index(a, b) => {
            let a = boxed(a, out);
            Expr::Index(a, boxed(b, out))
        }
        Expr::Case {
            operand,
            arms,
            otherwise,
        } => Expr::Case {
            operand: operand.as_deref().map(|o| Box::new(lift(o, out))),
            arms: arms
                .iter()
                .map(|(a, b)| (lift(a, out), lift(b, out)))
                .collect(),
            otherwise: otherwise.as_deref().map(|o| Box::new(lift(o, out))),
        },
        other => other.clone(),
    }
}

/// A window function's value for every row: `row_number`, `rank`,
/// `dense_rank` within each partition, in the window's order.
fn window(
    name: &str,
    partition: &[Expr],
    order: &[sql::Order],
    rels: &[Rel],
    rows: &[Vec<Option<usize>>],
    outer: Option<&Scope>,
    ctx: &Ctx,
) -> Out<Vec<V>> {
    let scope_of = |row: &[Option<usize>]| -> Out<(Vec<V>, Vec<V>)> {
        let scope = Scope {
            rels,
            row,
            group: None,
            slots: &[],
            outer,
        };
        let mut p = Vec::with_capacity(partition.len());
        for e in partition {
            p.push(eval(e, &scope, ctx)?);
        }
        let mut o = Vec::with_capacity(order.len());
        for x in order {
            o.push(eval(&x.expr, &scope, ctx)?);
        }
        Ok((p, o))
    };
    // Each partition's key, and its rows' order keys with their positions.
    type Partition = (Vec<V>, Vec<(Vec<V>, usize)>);
    let mut parts: Vec<Partition> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let (p, o) = scope_of(row)?;
        match parts.iter_mut().find(|(k, _)| *k == p) {
            Some((_, members)) => members.push((o, i)),
            None => parts.push((p, vec![(o, i)])),
        }
    }
    let mut out = vec![V::Null; rows.len()];
    for (_, mut members) in parts {
        let mut keyed: Vec<(Vec<V>, Vec<V>)> = members
            .iter()
            .map(|(o, i)| (vec![V::Int(*i as i64)], o.clone()))
            .collect();
        sort(&mut keyed, order, ctx.snap);
        members = keyed
            .into_iter()
            .map(|(i, o)| (o, as_int(&i[0]).unwrap_or(0) as usize))
            .collect();
        let total = members.len();
        let (mut rank, mut dense) = (0usize, 0usize);
        let mut prev: Option<&Vec<V>> = None;
        for (pos, (key, i)) in members.iter().enumerate() {
            if prev != Some(key) {
                rank = pos + 1;
                dense += 1;
            }
            out[*i] = match name {
                "row_number" => V::Int(pos as i64 + 1),
                "rank" => V::Int(rank as i64),
                "dense_rank" => V::Int(dense as i64),
                "count" => V::Int(total as i64),
                _ => V::Null,
            };
            prev = Some(key);
        }
    }
    Ok(out)
}

/// One select's rows, each with the keys `order` sorts it by.
fn select(
    sel: &Select,
    order: &[sql::Order],
    outer: Option<&Scope>,
    ctx: &Ctx,
) -> Out<(Output, Vec<Vec<V>>)> {
    let (rels, rows) = joined(sel, outer, ctx)?;

    // The result's columns, `*` spread out, windows and sets lifted out.
    let mut lifted = Vec::new();
    let mut exprs: Vec<(Expr, String, i32)> = Vec::new();
    for item in &sel.items {
        match item {
            Item::Star(q) => {
                for r in rels
                    .iter()
                    .filter(|r| q.as_ref().is_none_or(|q| *q == r.alias))
                {
                    for (c, ty) in &r.columns {
                        exprs.push((
                            Expr::Column(Some(r.alias.clone()), c.clone()),
                            c.clone(),
                            *ty,
                        ));
                    }
                }
            }
            Item::Expr(e, alias) => {
                let name = alias.clone().unwrap_or_else(|| column_name(e));
                exprs.push((lift(e, &mut lifted), name, expr_type(e, &rels)));
            }
        }
    }

    let grouped = !sel.group.is_empty()
        || sel.having.is_some()
        || sel
            .items
            .iter()
            .any(|i| matches!(i, Item::Expr(e, _) if has_aggregate(e)));
    if grouped && !lifted.is_empty() {
        return Err(Unsupported(
            "a window or set-returning function beside an aggregate".into(),
        ));
    }
    let mut windows: Vec<Vec<V>> = Vec::with_capacity(lifted.len());
    for l in &lifted {
        windows.push(match l {
            Lifted::Window(name, partition, order) => {
                window(name, partition, order, &rels, &rows, outer, ctx)?
            }
            Lifted::Set(..) => Vec::new(),
        });
    }

    // Each output row, and the rows it stands for.
    let groups: Vec<Vec<Vec<Option<usize>>>> = if grouped {
        let mut keys: Vec<Vec<V>> = Vec::new();
        let mut groups: Vec<Vec<Vec<Option<usize>>>> = Vec::new();
        for row in rows {
            let scope = Scope {
                rels: &rels,
                row: &row,
                group: None,
                slots: &[],
                outer,
            };
            let mut key = Vec::with_capacity(sel.group.len());
            for g in &sel.group {
                key.push(eval(g, &scope, ctx)?);
            }
            match keys.iter().position(|k| *k == key) {
                Some(i) => groups[i].push(row),
                None => {
                    keys.push(key);
                    groups.push(vec![row]);
                }
            }
        }
        // An aggregate over no rows is still one row: `count(*)` is 0.
        if groups.is_empty() && sel.group.is_empty() {
            groups.push(Vec::new());
        }
        groups
    } else {
        rows.into_iter().map(|r| vec![r]).collect()
    };

    let nulls = vec![None; rels.len()];
    let mut out_rows = Vec::with_capacity(groups.len());
    let mut keys = Vec::with_capacity(groups.len());
    for (ri, group) in groups.iter().enumerate() {
        let first = group.first().unwrap_or(&nulls);
        let base: Vec<V> = windows
            .iter()
            .map(|w| w.get(ri).cloned().unwrap_or(V::Null))
            .collect();
        // Set-returning calls run once per row; their values advance
        // together, and the longest decides how many rows this one makes.
        let mut sets: Vec<Option<Vec<V>>> = Vec::with_capacity(lifted.len());
        {
            let scope = Scope {
                rels: &rels,
                row: first,
                group: None,
                slots: &base,
                outer,
            };
            for l in &lifted {
                sets.push(match l {
                    Lifted::Set(name, args) => {
                        let mut vals = Vec::with_capacity(args.len());
                        for a in args {
                            vals.push(eval(a, &scope, ctx)?);
                        }
                        Some(expand(name, &vals))
                    }
                    Lifted::Window(..) => None,
                });
            }
        }
        let count = if sets.iter().all(Option::is_none) {
            1
        } else {
            sets.iter().flatten().map(Vec::len).max().unwrap_or(0)
        };
        for n in 0..count {
            let mut slots = base.clone();
            for (k, set) in sets.iter().enumerate() {
                if let Some(values) = set {
                    slots[k] = values.get(n).cloned().unwrap_or(V::Null);
                }
            }
            let scope = Scope {
                rels: &rels,
                row: first,
                group: grouped.then_some(group.as_slice()),
                slots: &slots,
                outer,
            };
            if let Some(h) = &sel.having {
                if truth(&eval(h, &scope, ctx)?) != Some(true) {
                    continue;
                }
            }
            let mut values = Vec::with_capacity(exprs.len());
            for (e, _, _) in &exprs {
                values.push(eval(e, &scope, ctx)?);
            }
            let mut key = Vec::with_capacity(order.len());
            for o in order {
                key.push(match &o.expr {
                    Expr::Int(n) if *n >= 1 => {
                        values.get(*n as usize - 1).cloned().unwrap_or(V::Null)
                    }
                    // An output column's name sorts by that column.
                    Expr::Column(None, name) if exprs.iter().any(|(_, n, _)| n == name) => {
                        let i = exprs.iter().position(|(_, n, _)| n == name).unwrap_or(0);
                        values[i].clone()
                    }
                    e => eval(e, &scope, ctx)?,
                });
            }
            out_rows.push(values);
            keys.push(key);
        }
    }
    let columns = exprs.into_iter().map(|(_, n, ty)| (n, ty)).collect();
    Ok((
        Output {
            columns,
            rows: out_rows,
        },
        keys,
    ))
}

fn sort(rows: &mut [(Vec<V>, Vec<V>)], order: &[sql::Order], s: &Snapshot) {
    rows.sort_by(|(_, a), (_, b)| {
        for (i, o) in order.iter().enumerate() {
            let ord = match (&a[i], &b[i]) {
                (V::Null, V::Null) => Ordering::Equal,
                // Nulls sort last going up, first going down, as in PostgreSQL.
                (V::Null, _) => Ordering::Greater,
                (_, V::Null) => Ordering::Less,
                (x, y) => compare(x, y, s).unwrap_or(Ordering::Equal),
            };
            let ord = if o.desc { ord.reverse() } else { ord };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    });
}

fn run(q: &Query, outer: Option<&Scope>, ctx: &Ctx) -> Out<Output> {
    let mut rows: Vec<(Vec<V>, Vec<V>)>;
    let columns;
    if q.unions.is_empty() {
        let (out, keys) = select(&q.first, &q.order, outer, ctx)?;
        columns = out.columns;
        rows = out.rows.into_iter().zip(keys).collect();
        if q.first.distinct {
            let mut seen: Vec<Vec<V>> = Vec::new();
            rows.retain(|(r, _)| {
                if seen.contains(r) {
                    false
                } else {
                    seen.push(r.clone());
                    true
                }
            });
        }
    } else {
        let (first, _) = select(&q.first, &[], outer, ctx)?;
        columns = first.columns;
        let mut all = first.rows;
        let mut dedupe = false;
        for (keep_all, sel) in &q.unions {
            let (out, _) = select(sel, &[], outer, ctx)?;
            all.extend(out.rows);
            dedupe |= !keep_all;
        }
        if dedupe {
            let mut seen: Vec<Vec<V>> = Vec::new();
            all.retain(|r| {
                if seen.contains(r) {
                    false
                } else {
                    seen.push(r.clone());
                    true
                }
            });
        }
        // After a union, `ORDER BY` names the result's columns.
        rows = Vec::with_capacity(all.len());
        for r in all {
            let mut key = Vec::with_capacity(q.order.len());
            for o in &q.order {
                key.push(match &o.expr {
                    Expr::Int(n) if *n >= 1 => r.get(*n as usize - 1).cloned().unwrap_or(V::Null),
                    Expr::Column(_, name) => columns
                        .iter()
                        .position(|(c, _)| c == name)
                        .and_then(|i| r.get(i).cloned())
                        .unwrap_or(V::Null),
                    _ => V::Null,
                });
            }
            rows.push((r, key));
        }
    }
    if !q.order.is_empty() {
        sort(&mut rows, &q.order, ctx.snap);
    }
    let offset = match &q.offset {
        Some(e) => as_int(&eval(e, outer.unwrap_or(&EMPTY), ctx)?)
            .unwrap_or(0)
            .max(0) as usize,
        None => 0,
    };
    let limit = match &q.limit {
        Some(e) => as_int(&eval(e, outer.unwrap_or(&EMPTY), ctx)?).map(|n| n.max(0) as usize),
        None => None,
    };
    let rows = rows
        .into_iter()
        .skip(offset)
        .take(limit.unwrap_or(usize::MAX))
        .map(|(r, _)| r)
        .collect();
    Ok(Output { columns, rows })
}

// -------------------------------------------------------------------- entry

/// A catalog query's answer, as the wire sends it: columns with their type
/// oids, and rows in text format.
pub struct Answer {
    pub columns: Vec<(String, i32)>,
    pub rows: Vec<Vec<Option<String>>>,
}

/// Whether a query reads the catalog rather than a collection: a `pg_`
/// table, `information_schema`, or a catalog function.
pub fn is_catalog(lower: &str) -> bool {
    let lower = lower.trim_start_matches('(').trim_start();
    if !lower.starts_with("select") {
        return false;
    }
    [
        "pg_catalog.",
        "information_schema.",
        "from pg_",
        "join pg_",
        ", pg_",
        "current_setting(",
        "set_config(",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

/// How many parameters a catalog query takes; `None` when it is not one
/// this module reads.
pub fn params(sql: &str) -> Option<usize> {
    sql::parse(sql).ok().map(|(_, n)| n)
}

/// Runs a catalog query over `snap`. `Err` means the query is outside what
/// this module reads; the caller answers it the old way, empty.
pub fn answer(sql: &str, params: &[Value], snap: &Snapshot) -> Out<Answer> {
    let (query, _) = sql::parse(sql)?;
    let params: Vec<V> = params
        .iter()
        .map(|p| match p {
            Value::Null => V::Null,
            Value::Bool(b) => V::Bool(*b),
            Value::Int(i) | Value::Timestamp(i) => V::Int(*i),
            Value::Float(f) => V::Float(*f),
            Value::Text(s) => V::Text(s.clone()),
            other => V::Text(format!("{other:?}")),
        })
        .collect();
    let ctx = Ctx {
        snap,
        params: &params,
    };
    let out = run(&query, None, &ctx)?;
    Ok(Answer {
        columns: out.columns,
        rows: out
            .rows
            .iter()
            .map(|r| r.iter().map(|v| render_named(v, snap)).collect())
            .collect(),
    })
}

#[cfg(test)]
mod tests;
