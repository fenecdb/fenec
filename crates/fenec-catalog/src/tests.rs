//! psql 17's own catalog queries, as `psql -E` prints them, run over a
//! database of two collections.

use super::*;
use fenec_core::schema::{Field, VectorIndexSpec};

fn snapshot() -> Snapshot {
    let mut db = Database::new();
    let mut title = Field::new("title", DataType::Text);
    title.index = IndexKind::Hash;
    title.required = true;
    let mut n = Field::new("n", DataType::Int);
    n.index = IndexKind::Sorted;
    let mut embed = Field::new("embed", DataType::Vector(3, VecPrec::F32));
    embed.index = IndexKind::Vector(VectorIndexSpec::default());
    let docs = Schema::new(
        "docs",
        vec![
            title,
            n,
            Field::new("tags", DataType::List(Box::new(DataType::Text))),
            Field::new("at", DataType::Timestamp),
            embed,
        ],
    )
    .unwrap();
    let notes = Schema::new("notes", vec![Field::new("x", DataType::Float)]).unwrap();
    for schema in [docs, notes] {
        db.execute(&Statement::CreateCollection {
            schema,
            if_not_exists: false,
        })
        .unwrap();
    }
    Snapshot::of(&db, "fenec", "16.0")
}

fn run_sql(s: &Snapshot, sql: &str, params: &[Value]) -> Answer {
    answer(sql, params, s).unwrap_or_else(|e| panic!("{e}\n{sql}"))
}

/// The rows as text, nulls as `-`.
fn text(a: &Answer) -> Vec<Vec<String>> {
    a.rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|c| c.clone().unwrap_or_else(|| "-".into()))
                .collect()
        })
        .collect()
}

const LIST: &str = "SELECT n.nspname as \"Schema\",
  c.relname as \"Name\",
  CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' WHEN 'i' THEN 'index' WHEN 'S' THEN 'sequence' WHEN 't' THEN 'TOAST table' WHEN 'f' THEN 'foreign table' WHEN 'p' THEN 'partitioned table' WHEN 'I' THEN 'partitioned index' END as \"Type\",
  pg_catalog.pg_get_userbyid(c.relowner) as \"Owner\"
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam
WHERE c.relkind IN ('r','p','v','m','S','f','')
      AND n.nspname <> 'pg_catalog'
      AND n.nspname !~ '^pg_toast'
      AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1,2;";

#[test]
fn backslash_d_lists_the_collections() {
    let s = snapshot();
    let a = run_sql(&s, LIST, &[]);
    let names: Vec<&str> = a.columns.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["Schema", "Name", "Type", "Owner"]);
    assert_eq!(
        text(&a),
        [
            ["public", "docs", "table", "fenec"],
            ["public", "notes", "table", "fenec"]
        ]
    );
    // \di: the same query over indexes, with the table each belongs to.
    let di = LIST
        .replace("c.relkind IN ('r','p','v','m','S','f','')", "c.relkind IN ('i','I','')")
        .replace(
            "pg_catalog.pg_get_userbyid(c.relowner) as \"Owner\"",
            "pg_catalog.pg_get_userbyid(c.relowner) as \"Owner\",\n  c2.relname as \"Table\"",
        )
        .replace(
            "LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam",
            "LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam\n     LEFT JOIN pg_catalog.pg_index i ON i.indexrelid = c.oid\n     LEFT JOIN pg_catalog.pg_class c2 ON i.indrelid = c2.oid",
        );
    let rows = text(&run_sql(&s, &di, &[]));
    let names: Vec<(&str, &str)> = rows
        .iter()
        .map(|r| (r[1].as_str(), r[4].as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("docs_embed_hnsw", "docs"),
            ("docs_n_sorted", "docs"),
            ("docs_pkey", "docs"),
            ("docs_title_hash", "docs"),
            ("notes_pkey", "notes")
        ]
    );
}

#[test]
fn backslash_d_of_a_collection_describes_it() {
    let s = snapshot();
    // 1. the relation, found by a regular expression
    let found = run_sql(
        &s,
        "SELECT c.oid,
  n.nspname,
  c.relname
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE c.relname OPERATOR(pg_catalog.~) '^(docs)$' COLLATE pg_catalog.default
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 2, 3;",
        &[],
    );
    let rows = text(&found);
    assert_eq!(rows.len(), 1, "{rows:?}");
    let oid = rows[0][0].clone();
    assert_eq!(rows[0][1..], ["public", "docs"]);

    // 2. its kind and flags
    let kind = run_sql(
        &s,
        &format!(
            "SELECT c.relchecks, c.relkind, c.relhasindex, c.relhasrules, c.relhastriggers, c.relrowsecurity, c.relforcerowsecurity, false AS relhasoids, c.relispartition, '', c.reltablespace, CASE WHEN c.reloftype = 0 THEN '' ELSE c.reloftype::pg_catalog.regtype::pg_catalog.text END, c.relpersistence, c.relreplident, am.amname
FROM pg_catalog.pg_class c
 LEFT JOIN pg_catalog.pg_class tc ON (c.reltoastrelid = tc.oid)
LEFT JOIN pg_catalog.pg_am am ON (c.relam = am.oid)
WHERE c.oid = '{oid}';"
        ),
        &[],
    );
    assert_eq!(
        text(&kind),
        [["0", "r", "t", "f", "f", "f", "f", "f", "f", "", "0", "", "p", "d", "fenec"]]
    );

    // 3. its columns: the id first, a vector as vector(3), required fields
    // not null
    let columns = run_sql(
        &s,
        &format!(
            "SELECT a.attname,
  pg_catalog.format_type(a.atttypid, a.atttypmod),
  (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true)
   FROM pg_catalog.pg_attrdef d
   WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef),
  a.attnotnull,
  (SELECT c.collname FROM pg_catalog.pg_collation c, pg_catalog.pg_type t
   WHERE c.oid = a.attcollation AND t.oid = a.atttypid AND a.attcollation <> t.typcollation) AS attcollation,
  a.attidentity,
  a.attgenerated
FROM pg_catalog.pg_attribute a
WHERE a.attrelid = '{oid}' AND a.attnum > 0 AND NOT a.attisdropped
ORDER BY a.attnum;"
        ),
        &[],
    );
    let described: Vec<(String, String, String)> = text(&columns)
        .into_iter()
        .map(|r| (r[0].clone(), r[1].clone(), r[3].clone()))
        .collect();
    let want = [
        ("id", "bigint", "t"),
        ("title", "text", "t"),
        ("n", "bigint", "f"),
        ("tags", "text[]", "f"),
        ("at", "timestamp with time zone", "f"),
        ("embed", "vector(3)", "f"),
    ];
    assert_eq!(
        described,
        want.iter()
            .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
            .collect::<Vec<_>>()
    );

    // 4. its indexes, the primary key first
    let indexes = run_sql(
        &s,
        &format!(
            "SELECT c2.relname, i.indisprimary, i.indisunique, i.indisclustered, i.indisvalid, pg_catalog.pg_get_indexdef(i.indexrelid, 0, true),
  pg_catalog.pg_get_constraintdef(con.oid, true), contype, condeferrable, condeferred, i.indisreplident, c2.reltablespace
FROM pg_catalog.pg_class c, pg_catalog.pg_class c2, pg_catalog.pg_index i
  LEFT JOIN pg_catalog.pg_constraint con ON (conrelid = i.indrelid AND conindid = i.indexrelid AND contype IN ('p','u','x'))
WHERE c.oid = '{oid}' AND c.oid = i.indrelid AND i.indexrelid = c2.oid
ORDER BY i.indisprimary DESC, c2.relname;"
        ),
        &[],
    );
    let rows = text(&indexes);
    assert_eq!(rows[0][0], "docs_pkey");
    assert_eq!(rows[0][1], "t");
    assert_eq!(rows[0][6], "PRIMARY KEY (id)");
    assert_eq!(rows[0][7], "p");
    let hnsw = rows.iter().find(|r| r[0] == "docs_embed_hnsw").unwrap();
    assert_eq!(
        hnsw[5],
        "CREATE INDEX docs_embed_hnsw ON public.docs USING hnsw (embed)"
    );
    assert_eq!(hnsw[6], "-");

    // 5-9. the footers psql asks for: none of them has rows
    for footer in [
        format!("SELECT pol.polname, pol.polpermissive,
  CASE WHEN pol.polroles = '{{0}}' THEN NULL ELSE pg_catalog.array_to_string(array(select rolname from pg_catalog.pg_roles where oid = any (pol.polroles) order by 1),',') END,
  pg_catalog.pg_get_expr(pol.polqual, pol.polrelid),
  pg_catalog.pg_get_expr(pol.polwithcheck, pol.polrelid),
  CASE pol.polcmd
    WHEN 'r' THEN 'SELECT'
    WHEN 'a' THEN 'INSERT'
    WHEN 'w' THEN 'UPDATE'
    WHEN 'd' THEN 'DELETE'
    END AS cmd
FROM pg_catalog.pg_policy pol
WHERE pol.polrelid = '{oid}' ORDER BY 1;"),
        format!("SELECT oid, stxrelid::pg_catalog.regclass, stxnamespace::pg_catalog.regnamespace::pg_catalog.text AS nsp, stxname,
pg_catalog.pg_get_statisticsobjdef_columns(oid) AS columns,
  'd' = any(stxkind) AS ndist_enabled,
  'f' = any(stxkind) AS deps_enabled,
  'm' = any(stxkind) AS mcv_enabled,
stxstattarget
FROM pg_catalog.pg_statistic_ext
WHERE stxrelid = '{oid}'
ORDER BY nsp, stxname;"),
        format!("SELECT pubname
     , NULL
     , NULL
FROM pg_catalog.pg_publication p
     JOIN pg_catalog.pg_publication_namespace pn ON p.oid = pn.pnpubid
     JOIN pg_catalog.pg_class pc ON pc.relnamespace = pn.pnnspid
WHERE pc.oid ='{oid}' and pg_catalog.pg_relation_is_publishable('{oid}')
UNION
SELECT pubname
     , pg_get_expr(pr.prqual, c.oid)
     , (CASE WHEN pr.prattrs IS NOT NULL THEN
         (SELECT string_agg(attname, ', ')
           FROM pg_catalog.generate_series(0, pg_catalog.array_upper(pr.prattrs::pg_catalog.int2[], 1)) s,
                pg_catalog.pg_attribute
          WHERE attrelid = pr.prrelid AND attnum = prattrs[s])
        ELSE NULL END) FROM pg_catalog.pg_publication p
     JOIN pg_catalog.pg_publication_rel pr ON p.oid = pr.prpubid
     JOIN pg_catalog.pg_class c ON c.oid = pr.prrelid
WHERE pr.prrelid = '{oid}'
UNION
SELECT pubname
     , NULL
     , NULL
FROM pg_catalog.pg_publication p
WHERE p.puballtables AND pg_catalog.pg_relation_is_publishable('{oid}')
ORDER BY 1;"),
        format!("SELECT c.oid::pg_catalog.regclass
FROM pg_catalog.pg_class c, pg_catalog.pg_inherits i
WHERE c.oid = i.inhparent AND i.inhrelid = '{oid}'
  AND c.relkind != 'p' AND c.relkind != 'I'
ORDER BY inhseqno;"),
        format!("SELECT c.oid::pg_catalog.regclass, c.relkind, inhdetachpending, pg_catalog.pg_get_expr(c.relpartbound, c.oid)
FROM pg_catalog.pg_class c, pg_catalog.pg_inherits i
WHERE c.oid = i.inhrelid AND i.inhparent = '{oid}'
ORDER BY pg_catalog.pg_get_expr(c.relpartbound, c.oid) = 'DEFAULT', c.oid::pg_catalog.regclass::pg_catalog.text;"),
    ] {
        let a = run_sql(&s, &footer, &[]);
        assert!(a.rows.is_empty(), "{footer}: {:?}", text(&a));
    }
}

#[test]
fn databases_and_schemas() {
    let s = snapshot();
    let l = run_sql(
        &s,
        "SELECT
  d.datname as \"Name\",
  pg_catalog.pg_get_userbyid(d.datdba) as \"Owner\",
  pg_catalog.pg_encoding_to_char(d.encoding) as \"Encoding\",
  CASE d.datlocprovider WHEN 'b' THEN 'builtin' WHEN 'c' THEN 'libc' WHEN 'i' THEN 'icu' END AS \"Locale Provider\",
  d.datcollate as \"Collate\",
  d.datctype as \"Ctype\",
  d.datlocale as \"Locale\",
  d.daticurules as \"ICU Rules\",
  CASE WHEN pg_catalog.array_length(d.datacl, 1) = 0 THEN '(none)' ELSE pg_catalog.array_to_string(d.datacl, E'\\n') END AS \"Access privileges\"
FROM pg_catalog.pg_database d
ORDER BY 1;",
        &[],
    );
    assert_eq!(
        text(&l),
        [["fenec", "fenec", "UTF8", "libc", "C", "C", "-", "-", "-"]]
    );
    let dn = run_sql(
        &s,
        "SELECT n.nspname AS \"Name\",
  pg_catalog.pg_get_userbyid(n.nspowner) AS \"Owner\"
FROM pg_catalog.pg_namespace n
WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
ORDER BY 1;",
        &[],
    );
    assert_eq!(text(&dn), [["public", "fenec"]]);
}

#[test]
fn parameters_casts_and_the_information_schema() {
    let s = snapshot();
    // JDBC binds the schema's oid and the table name as parameters.
    let a = run_sql(
        &s,
        "select c.relname, c.relkind from pg_catalog.pg_class c \
         where c.relnamespace = $1 and c.relname like $2 and c.relkind = 'r' order by 1",
        &[Value::Int(PUBLIC), Value::Text("no%".into())],
    );
    assert_eq!(text(&a), [["notes", "r"]]);
    let a = run_sql(
        &s,
        "select 'docs'::regclass::oid = c.oid, c.oid::regclass from pg_class c where c.relname = 'docs'",
        &[],
    );
    assert_eq!(text(&a), [["t", "docs"]]);
    let a = run_sql(
        &s,
        "select table_name, column_name, data_type, is_nullable from information_schema.columns \
         where table_schema = 'public' and table_name = 'docs' order by ordinal_position",
        &[],
    );
    let rows = text(&a);
    assert_eq!(rows[0], ["docs", "id", "bigint", "NO"]);
    assert_eq!(rows[3], ["docs", "tags", "ARRAY", "YES"]);
    assert_eq!(rows[5], ["docs", "embed", "USER-DEFINED", "YES"]);
    // Aggregates, grouped and whole.
    let a = run_sql(
        &s,
        "select c.relname, count(*) from pg_class c join pg_attribute a on a.attrelid = c.oid \
         where c.relkind = 'r' group by c.relname order by 2 desc",
        &[],
    );
    assert_eq!(text(&a), [["docs", "6"], ["notes", "2"]]);
    let a = run_sql(
        &s,
        "select count(*) from pg_class where relname = 'nothing'",
        &[],
    );
    assert_eq!(text(&a), [["0"]]);
    // A table not built here is empty, and a function not here is null.
    let a = run_sql(
        &s,
        "select x, pg_catalog.pg_nosuch(1) from pg_catalog.pg_trigger",
        &[],
    );
    assert!(a.rows.is_empty());
    let a = run_sql(&s, "select pg_catalog.pg_nosuch(1) as v", &[]);
    assert_eq!(text(&a), [["-"]]);
}

/// JDBC's `getPrimaryKeys`: a set-returning function in the select list,
/// and the outer query filtering on a field of its composite -- a
/// condition on one relation alone, never a join key.
#[test]
fn jdbc_primary_keys() {
    let s = snapshot();
    let a = run_sql(
        &s,
        "SELECT result.TABLE_SCHEM, result.TABLE_NAME, result.COLUMN_NAME, result.KEY_SEQ, result.PK_NAME FROM \
         (SELECT NULL AS TABLE_CAT, n.nspname AS TABLE_SCHEM, ct.relname AS TABLE_NAME, a.attname AS COLUMN_NAME, \
           (information_schema._pg_expandarray(i.indkey)).n AS KEY_SEQ, ci.relname AS PK_NAME, \
           information_schema._pg_expandarray(i.indkey) AS KEYS, a.attnum AS A_ATTNUM \
          FROM pg_catalog.pg_class ct JOIN pg_catalog.pg_attribute a ON (ct.oid = a.attrelid) \
          JOIN pg_catalog.pg_namespace n ON (ct.relnamespace = n.oid) \
          JOIN pg_catalog.pg_index i ON ( a.attrelid = i.indrelid) \
          JOIN pg_catalog.pg_class ci ON (ci.oid = i.indexrelid) \
          WHERE true AND n.nspname = 'public' AND ct.relname = 'docs' AND i.indisprimary ) result \
         where result.A_ATTNUM = (result.KEYS).x ORDER BY result.table_name, result.pk_name, result.key_seq",
        &[],
    );
    assert_eq!(text(&a), [["public", "docs", "id", "1", "docs_pkey"]]);
}

#[test]
fn queries_outside_the_subset_are_refused() {
    let s = snapshot();
    for q in [
        "select * from pg_class c right join pg_namespace n on true",
        // A recursion that never stops is refused rather than run for good.
        "with recursive n(i) as (select 1 union all select i from n) select * from n",
    ] {
        assert!(answer(q, &[], &s).is_err(), "{q}");
    }
    // A `WITH`, and one that reads itself until it adds nothing.
    let a = run_sql(&s, "with x(v) as (select 1) select v from x", &[]);
    assert_eq!(text(&a), [["1"]]);
    let a = run_sql(
        &s,
        "with recursive n(i) as (select 1 union all select i + 1 from n where i < 4)          select i from n order by i desc",
        &[],
    );
    assert_eq!(text(&a), [["4"], ["3"], ["2"], ["1"]]);
    assert!(is_catalog(
        "with x as (select oid from pg_type) select * from x"
    ));
    assert!(is_catalog("select * from pg_catalog.pg_class"));
    assert!(is_catalog("select relname from pg_class"));
    assert!(!is_catalog("select title from docs where n > 1"));
    assert!(!is_catalog("get docs"));
}

/// A `collate tr` field is a column in `tr-x-icu`, the name PostgreSQL gives
/// ICU's Turkish collation, a `collate und` one in `und-x-icu`, its root
/// collation, and `\d` says so where a text column in the default
/// collation says nothing.
#[test]
fn a_collated_column_shows_its_collation() {
    let mut db = Database::new();
    let schema = Schema::new(
        "people",
        vec![
            Field::new("name", DataType::Text).collated(Collation::Turkish),
            Field::new("city", DataType::Text),
            Field::new("title", DataType::Text).collated(Collation::Root),
        ],
    )
    .unwrap();
    db.execute(&Statement::CreateCollection {
        schema,
        if_not_exists: false,
    })
    .unwrap();
    let s = Snapshot::of(&db, "fenec", "16.0");
    let columns = run_sql(
        &s,
        "SELECT a.attname,
  (SELECT c.collname FROM pg_catalog.pg_collation c, pg_catalog.pg_type t
   WHERE c.oid = a.attcollation AND t.oid = a.atttypid AND a.attcollation <> t.typcollation) AS attcollation
FROM pg_catalog.pg_attribute a, pg_catalog.pg_class r
WHERE a.attrelid = r.oid AND r.relname = 'people' AND a.attnum > 0
ORDER BY a.attnum;",
        &[],
    );
    assert_eq!(
        text(&columns),
        [
            ["id", "-"],
            ["name", "tr-x-icu"],
            ["city", "-"],
            ["title", "und-x-icu"]
        ]
    );
}

/// How pgvector's clients and the drivers under them find a type: each
/// library's own query, as it sends it.
#[test]
fn pgvector_clients_find_its_types() {
    let s = snapshot();
    // psycopg's `TypeInfo.fetch`, which pgvector-python's `register_vector`
    // calls: its parameter is text, and the row names the type.
    let psycopg = "SELECT\n    typname AS name, oid, typarray AS array_oid,\n    oid::regtype::text AS regtype, typdelim AS delimiter\nFROM pg_type t\nWHERE t.oid = to_regtype($1)\nORDER BY t.oid\n";
    assert!(is_catalog(&psycopg.to_lowercase()));
    assert_eq!(param_types(psycopg), Some(vec![TEXT]));
    let a = run_sql(&s, psycopg, &[Value::Text("vector".into())]);
    assert_eq!(text(&a), [["vector", "16400", "0", "vector", ","]]);
    assert!(run_sql(&s, psycopg, &[Value::Text("nope".into())])
        .rows
        .is_empty());
    // `register_vector` looks up `bit` as well, which every PostgreSQL has.
    assert_eq!(
        text(&run_sql(&s, psycopg, &[Value::Text("bit".into())])),
        [["bit", "1560", "1561", "bit", ","]]
    );

    // pgvector-go's `RegisterTypes`.
    let pgx = "SELECT to_regtype('vector')::oid, to_regtype('halfvec')::oid, to_regtype('sparsevec')::oid";
    assert!(is_catalog(&pgx.to_lowercase()));
    assert_eq!(text(&run_sql(&s, pgx, &[])), [["16400", "16401", "16402"]]);

    // pgx's `LoadType`, then the type's kind.
    let load = "select $1::text::regtype::oid;";
    assert!(is_catalog(load));
    assert_eq!(param_types(load), Some(vec![TEXT]));
    assert_eq!(
        text(&run_sql(&s, load, &[Value::Text("vector".into())])),
        [["16400"]]
    );
    let kind = "select typtype::text, typbasetype from pg_type where oid=$1";
    assert_eq!(text(&run_sql(&s, kind, &[Value::Int(16400)])), [["b", "0"]]);

    // tokio-postgres's type lookup for an OID it does not know, which
    // pgvector-rust's `Vector` needs named.
    let tokio = "SELECT t.typname, t.typtype, t.typelem, r.rngsubtype, t.typbasetype, n.nspname, t.typrelid \
                 FROM pg_catalog.pg_type t \
                 LEFT OUTER JOIN pg_catalog.pg_range r ON r.rngtypid = t.oid \
                 INNER JOIN pg_catalog.pg_namespace n ON t.typnamespace = n.oid \
                 WHERE t.oid = $1";
    assert_eq!(
        text(&run_sql(&s, tokio, &[Value::Int(16400)])),
        [["vector", "b", "0", "-", "0", "public", "0"]]
    );
    let a = run_sql(&s, tokio, &[Value::Int(16400)]);
    let types: Vec<i32> = a.columns.iter().map(|c| c.1).collect();
    assert_eq!(types, [NAME, CHAR, OID, OID, OID, NAME, OID]);
    // Its `$1` is the oid it is compared with: tokio-postgres binds an
    // `Oid` only to a parameter described as one, and refused the lookup
    // described as text.
    assert_eq!(param_types(tokio), Some(vec![OID]));
    let composite = "SELECT attname, atttypid FROM pg_catalog.pg_attribute                      WHERE attrelid = $1 AND NOT attisdropped AND attnum > 0 ORDER BY attnum";
    assert_eq!(param_types(composite), Some(vec![OID]));
    // Either side, a name, a list; a cast still says; a subquery's
    // columns and an outer query's reach in; what nothing types is text.
    assert_eq!(
        param_types(
            "SELECT c.oid FROM pg_class c WHERE $1 = c.relname AND c.relkind IN ($2, $3) \
             AND c.relnamespace = $4::int8 AND $5 = $6 \
             AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid AND a.attnum = $7)"
        ),
        Some(vec![NAME, CHAR, CHAR, INT8, TEXT, TEXT, INT2])
    );

    // asyncpg's `set_type_codec`, and pgvector-node's `registerTypes`.
    let by_name = "SELECT t.oid, t.typelem AS elemtype, t.typtype AS kind \
                   FROM pg_catalog.pg_type AS t \
                   INNER JOIN pg_catalog.pg_namespace AS ns ON (ns.oid = t.typnamespace) \
                   WHERE t.typname = $1 AND ns.nspname = $2";
    // Each in `public`, where pgvector-python's `register_vector` asks for
    // it: `sparsevec` in `pg_catalog` had no codec registered.
    for (name, oid) in [
        ("vector", "16400"),
        ("halfvec", "16401"),
        ("sparsevec", "16402"),
    ] {
        assert_eq!(
            text(&run_sql(
                &s,
                by_name,
                &[Value::Text(name.into()), Value::Text("public".into())]
            )),
            [[oid, "0", "b"]],
            "{name}"
        );
    }
    let node = "SELECT typname, oid FROM pg_type WHERE typname IN ($1, $2, $3)";
    let names = ["vector", "halfvec", "sparsevec"].map(|n| Value::Text(n.into()));
    assert_eq!(text(&run_sql(&s, node, &names)).len(), 3);

    // An array parameter: `$1::oid[]`, as asyncpg names the types it looks
    // up.
    let many = "SELECT t.typname FROM pg_type t WHERE t.oid = any($1::oid[]) ORDER BY t.typname";
    assert_eq!(param_types(many), Some(vec![OID_ARRAY]));
    let oids = Value::List(vec![Value::Int(16400), Value::Int(16402)]);
    assert_eq!(
        text(&run_sql(&s, many, &[oids])),
        [["sparsevec"], ["vector"]]
    );
}

/// asyncpg's own introspection, as asyncpg 0.31 sends it for a type OID it
/// has no codec for -- a `vector` column's, when the client registered
/// none: a recursive CTE over a derived table, with a recursive CTE of its
/// own in a scalar subquery.
#[test]
fn asyncpg_introspects_a_type_it_does_not_know() {
    let s = snapshot();
    let q = include_str!("asyncpg_introspect.sql");
    assert!(is_catalog(&q.to_lowercase()));
    assert_eq!(param_types(q), Some(vec![OID_ARRAY]));
    let a = run_sql(&s, q, &[Value::List(vec![Value::Int(VECTOR as i64)])]);
    let names: Vec<&str> = a.columns.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "oid",
            "ns",
            "name",
            "kind",
            "basetype",
            "elemtype",
            "elemdelim",
            "range_subtype",
            "attrtypoids",
            "attrnames",
            "depth",
            "basetype_name",
            "elemtype_name",
            "range_subtype_name"
        ]
    );
    assert_eq!(
        text(&a),
        [["16400", "public", "vector", "b", "-", "0", "-", "-", "-", "-", "0", "-", "-", "-"]]
    );
    // An array type brings its element type after it, a level deeper.
    let a = run_sql(&s, q, &[Value::List(vec![Value::Int(TEXT_ARRAY as i64)])]);
    let rows = text(&a);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0][2], "text");
    assert_eq!(rows[1][2], "_text");
}
