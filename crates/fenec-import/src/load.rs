//! The bulk load pipeline: rows first, indexes second.
//!
//! The order matters. If a vector field is opened with an index, every `put`
//! updates the HNSW graph; when the index is built afterwards the write path
//! is a pure append and the graph is built in one go, in parallel. The same
//! workflow is the recommended order for SQLite and PostgreSQL too (see the
//! README, "Comparison with SQLite and PostgreSQL").

use crate::map::{self, Plan, Target};
use crate::{Options, Source, Summary};
use std::time::Instant;
use fenec_core::error::{Error, Result};
use fenec_core::plugin::Registry;
use fenec_core::prelude::*;
use fenec_core::query::{eval, truthy, EvalCtx, RowAccess};
use fenec_core::value::DataType;

/// Reads the source, derives the schema and loads it.
pub fn run(src: &mut dyn Source, db: &mut Database, opts: &Options) -> Result<Summary> {
    run_with_progress(src, db, opts, &mut |_| {})
}

/// [`run`], but reports the rows written so far after every batch. Progress
/// output is the caller's job; this module prints nothing.
pub fn run_with_progress(
    src: &mut dyn Source,
    db: &mut Database,
    opts: &Options,
    progress: &mut dyn FnMut(u64),
) -> Result<Summary> {
    let columns = src.columns()?;
    let plan = map::plan(&columns, opts)?;
    let t0 = Instant::now();

    db.execute(&Statement::CreateCollection {
        schema: plan.schema.clone(),
        if_not_exists: false,
    })?;

    // Target types are resolved before the row loop: the conversion of a
    // BLOB column marked with `--vector` is decided here.
    let types: Vec<Option<DataType>> = plan
        .targets
        .iter()
        .map(|t| match t {
            Target::Id => None,
            Target::Field(name) => plan.schema.field(name).map(|f| f.ty.clone()),
        })
        .collect();

    // The filter is evaluated with its own registry: borrowing the target
    // database would prevent writing in the same loop. The price is that
    // only builtins, not plugin functions, can be used inside `--where`.
    let registry = Registry::with_builtins();
    let ctx = EvalCtx {
        params: &[],
        registry: &registry,
    };

    let mut rows: u64 = 0;
    let mut skipped: u64 = 0;
    let mut seen: u64 = 0;
    let mut batch: Vec<Vec<(String, Expr)>> = Vec::with_capacity(opts.batch);
    while let Some(values) = src.next_row()? {
        if opts.limit.is_some_and(|n| rows >= n) {
            break;
        }
        seen += 1;
        let doc = document(&plan, &types, values, seen)?;
        if let Some(f) = &opts.filter {
            if !keep(f, &doc, &ctx, seen)? {
                skipped += 1;
                continue;
            }
        }
        batch.push(doc);
        rows += 1;
        if batch.len() >= opts.batch {
            flush(db, &opts.into, &mut batch, rows)?;
            progress(rows);
        }
    }
    if !batch.is_empty() {
        flush(db, &opts.into, &mut batch, rows)?;
        progress(rows);
    }

    for (field, kind) in &plan.indexes {
        db.execute(&Statement::CreateIndex {
            collection: opts.into.clone(),
            field: field.clone(),
            kind: kind.clone(),
            if_not_exists: false,
        })?;
    }

    // The checkpoint writes the HNSW graph into the file too: the next open
    // does not rebuild the index.
    db.checkpoint()?;

    Ok(Summary {
        rows,
        skipped,
        elapsed: t0.elapsed(),
        warnings: plan.warnings,
    })
}

/// Turns a row into a `put` body.
fn document(
    plan: &Plan,
    types: &[Option<DataType>],
    values: Vec<Value>,
    row_no: u64,
) -> Result<Vec<(String, Expr)>> {
    if values.len() != plan.targets.len() {
        return Err(Error::Corrupt(format!(
            "row {row_no} has {} values, {} columns were expected",
            values.len(),
            plan.targets.len()
        )));
    }
    let mut doc = Vec::with_capacity(values.len());
    for ((target, ty), value) in plan.targets.iter().zip(types).zip(values) {
        match target {
            // A NULL id: let fenecdb assign its own id to this document.
            Target::Id if value.is_null() => {}
            Target::Id => doc.push(("id".to_string(), Expr::Lit(value))),
            Target::Field(name) => {
                let v = match ty {
                    // The position is exact here: known before entering the batch.
                    Some(ty) => adapt(value, ty).map_err(|e| match e {
                        Error::Type(m) => {
                            Error::Type(format!("row {row_no}, field `{name}`: {m}"))
                        }
                        other => other,
                    })?,
                    None => value,
                };
                doc.push((name.clone(), Expr::Lit(v)));
            }
        }
    }
    Ok(doc)
}

/// Applies the filter to one row.
fn keep(
    filter: &Expr,
    doc: &[(String, Expr)],
    ctx: &EvalCtx,
    row_no: u64,
) -> Result<bool> {
    let mut row = ImportedRow { doc };
    let v = eval(filter, &mut row, ctx).map_err(|e| match e {
        Error::NotFound(m) => Error::NotFound(format!(
            "{m} -- `--where` uses the field names of the target schema"
        )),
        Error::Type(m) => Error::Type(format!("filter on row {row_no}: {m}")),
        other => other,
    })?;
    Ok(truthy(&v))
}

/// Field access over a document that has not been written yet.
struct ImportedRow<'a> {
    doc: &'a [(String, Expr)],
}

impl RowAccess for ImportedRow<'_> {
    fn id(&self) -> DocId {
        // The id from the source when there is one; otherwise fenecdb has not assigned it yet.
        self.doc
            .iter()
            .find(|(n, _)| n == "id")
            .and_then(|(_, e)| match e {
                Expr::Lit(Value::Int(n)) if *n > 0 => Some(*n as DocId),
                _ => None,
            })
            .unwrap_or(0)
    }

    fn field(&mut self, name: &str) -> Result<Value> {
        self.doc
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, e)| match e {
                Expr::Lit(v) => v.clone(),
                _ => Value::Null,
            })
            .ok_or_else(|| Error::NotFound(format!("field `{name}`")))
    }
}

/// Fits a source value to the target type.
///
/// `Value::coerce` is strict in order to protect the schema: it does not go
/// from text to number or from integer to bool. In an import those
/// conversions are unavoidable -- SQLite columns can change type from row to
/// row, and everything is text in PostgreSQL COPY output. This is the only
/// place that binds a loosely typed source to the schema; everything else is
/// left to `coerce`.
fn adapt(v: Value, ty: &DataType) -> Result<Value> {
    let mismatch = |want: &str, got: &Value| {
        Error::Type(format!("expected {want}, could not parse `{}`", show(got)))
    };
    Ok(match (ty, v) {
        // Raw bytes marked with `--vector`: an f32 little-endian array.
        (DataType::Vector(n, _), Value::Bytes(b)) => {
            if b.len() != n * 4 {
                return Err(Error::Type(format!(
                    "vector<{n}> expects {} bytes, found {}",
                    n * 4,
                    b.len()
                )));
            }
            finite(
                b.chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
            )?
        }
        // List and vector values are checked before they enter the index too.
        (DataType::Vector(..), Value::Vector(v)) => finite(v)?,
        // BOOLEAN is a 0/1 integer in SQLite; `t`/`f` in PostgreSQL text.
        (DataType::Bool, Value::Int(n)) => Value::Bool(n != 0),
        (DataType::Bool, Value::Text(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "t" | "true" | "1" | "yes" | "on" => Value::Bool(true),
            "f" | "false" | "0" | "no" | "off" => Value::Bool(false),
            _ => return Err(mismatch("bool", &Value::Text(s))),
        },
        (DataType::Int, Value::Text(s)) => match s.trim().parse::<i64>() {
            Ok(n) => Value::Int(n),
            Err(_) => return Err(mismatch("int", &Value::Text(s))),
        },
        (DataType::Float, Value::Text(s)) => match s.trim().parse::<f64>() {
            Ok(f) => Value::Float(f),
            Err(_) => return Err(mismatch("float", &Value::Text(s))),
        },
        // A numeric value landing in a text column: in SQLite the type
        // belongs to the value rather than the column, so this is ordinary.
        (DataType::Text, Value::Int(n)) => Value::Text(n.to_string()),
        (DataType::Text, Value::Float(f)) => Value::Text(f.to_string()),
        (DataType::Bytes, Value::Int(n)) => Value::Bytes(n.to_string().into_bytes()),
        (DataType::Bytes, Value::Float(f)) => Value::Bytes(f.to_string().into_bytes()),
        (_, v) => v,
    })
}

/// Rejects a vector carrying a non-finite component.
///
/// A NaN makes a distance unsortable: while sorting the HNSW candidate list
/// the comparison does not produce a total order and `sort` panics. Rather
/// than feeding corrupt data into the index, it is better to stop and say
/// which row it was.
fn finite(v: Vec<f32>) -> Result<Value> {
    if let Some(i) = v.iter().position(|f| !f.is_finite()) {
        return Err(Error::Type(format!(
            "component {} of the vector is not finite ({}); is the source column really an f32 array?",
            i, v[i]
        )));
    }
    Ok(Value::Vector(v))
}

/// Shows the value briefly in error text; long strings and byte blobs are truncated.
fn show(v: &Value) -> String {
    let s = match v {
        Value::Text(s) => s.clone(),
        Value::Bytes(b) => format!("{} bytes", b.len()),
        other => format!("{other:?}"),
    };
    if s.chars().count() > 40 {
        format!("{}...", s.chars().take(40).collect::<String>())
    } else {
        s
    }
}

fn flush(
    db: &mut Database,
    collection: &str,
    batch: &mut Vec<Vec<(String, Expr)>>,
    rows_done: u64,
) -> Result<()> {
    let n = batch.len() as u64;
    let docs = std::mem::take(batch);
    db.execute(&Statement::Put {
        collection: collection.to_string(),
        docs,
    })
    .map_err(|e| locate(e, rows_done - n + 1, rows_done))
    .map(|_| ())
}

/// The engine does not say which document it tripped on. Since the sources
/// are loosely typed (a SQLite column can change type from row to row), at
/// least the batch has to be reported.
fn locate(e: Error, first: u64, last: u64) -> Error {
    let at = if first == last {
        format!(" (row {first})")
    } else {
        format!(" (rows {first}-{last})")
    };
    match e {
        Error::Type(m) => Error::Type(m + &at),
        Error::Query(m) => Error::Query(m + &at),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Column, IdSource, Rows};
    use fenec_core::schema::VectorIndexSpec;
    use fenec_core::value::VecPrec;

    /// Document count in a collection. `Collection` gives stats, not a counter.
    fn count(db: &Database, name: &str) -> usize {
        db.stats().iter().find(|s| s.name == name).unwrap().documents
    }

    fn source(rows: Vec<Vec<Value>>) -> Rows {
        Rows::new(
            vec![
                Column::new("id", DataType::Int, "INTEGER"),
                Column::new("title", DataType::Text, "TEXT"),
                Column::new("embed", DataType::Vector(2, VecPrec::F32), "BLOB"),
            ],
            rows,
        )
    }

    fn rows(n: i64) -> Vec<Vec<Value>> {
        (1..=n)
            .map(|i| {
                vec![
                    Value::Int(i),
                    Value::Text(format!("document {i}")),
                    Value::Vector(vec![i as f32, 0.0]),
                ]
            })
            .collect()
    }

    #[test]
    fn loads_rows_and_preserves_source_ids() {
        let mut src = source(rows(5));
        let mut db = Database::new();
        let s = run(&mut src, &mut db, &Options::new("m")).unwrap();
        assert_eq!(s.rows, 5);

        let out = db
            .execute(&Statement::Select(Select {
                collection: "m".into(),
                project: Some(vec!["id".into(), "title".into()]),
                ..Default::default()
            }))
            .unwrap();
        let Response::Rows(rs) = out else { panic!() };
        assert_eq!(rs.rows.len(), 5);
        let mut ids: Vec<i64> = rs
            .rows
            .iter()
            .map(|r| match &r.values[0] {
                Value::Int(n) => *n,
                other => panic!("{other:?}"),
            })
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 3, 4, 5], "the source ids must be preserved");
    }

    /// No row may be lost at a batch boundary: the last partial batch is written too.
    #[test]
    fn partial_final_batch_is_written() {
        for n in [1, 99, 100, 101, 250] {
            let mut src = source(rows(n));
            let mut db = Database::new();
            let mut o = Options::new("m");
            o.batch = 100;
            let s = run(&mut src, &mut db, &o).unwrap();
            assert_eq!(s.rows, n as u64, "n={n}");
            assert_eq!(count(&db, "m"), n as usize, "n={n}");
        }
    }

    fn where_expr(src: &str) -> Expr {
        let Statement::Select(sel) = fenec_ql::parse_one(&format!("get t where {src}")).unwrap()
        else {
            panic!()
        };
        sel.filter.unwrap()
    }

    #[test]
    fn filter_skips_rows_before_writing() {
        let mut src = source(rows(10));
        let mut db = Database::new();
        let mut o = Options::new("m");
        o.filter = Some(where_expr("title = \"document 3\""));
        let s = run(&mut src, &mut db, &o).unwrap();
        assert_eq!(s.rows, 1);
        assert_eq!(s.skipped, 9);
        assert_eq!(count(&db, "m"), 1);
    }

    /// The filter must be able to see the document id as well.
    #[test]
    fn filter_can_use_the_document_id() {
        let mut src = source(rows(10));
        let mut db = Database::new();
        let mut o = Options::new("m");
        o.filter = Some(where_expr("id > 7"));
        let s = run(&mut src, &mut db, &o).unwrap();
        assert_eq!(s.rows, 3, "8, 9, 10");
        assert_eq!(s.skipped, 7);
    }

    /// `--limit` counts *after* the filter: the user wants that many documents.
    #[test]
    fn limit_counts_rows_that_survive_the_filter() {
        let mut src = source(rows(100));
        let mut db = Database::new();
        let mut o = Options::new("m");
        o.filter = Some(where_expr("id > 50"));
        o.limit = Some(5);
        let s = run(&mut src, &mut db, &o).unwrap();
        assert_eq!(s.rows, 5);
        assert_eq!(count(&db, "m"), 5);
    }

    /// Asking about a field that does not exist must not silently drop every row.
    #[test]
    fn filter_on_unknown_field_is_an_error() {
        let mut src = source(rows(3));
        let mut db = Database::new();
        let mut o = Options::new("m");
        o.filter = Some(where_expr("no_such = 1"));
        let e = run(&mut src, &mut db, &o).unwrap_err().to_string();
        assert!(e.contains("no_such"), "{e}");
        assert!(e.contains("field names of the target schema"), "{e}");
    }

    /// Builtin functions must be usable in a filter.
    #[test]
    fn builtin_functions_work_in_filters() {
        let mut src = source(rows(3));
        let mut db = Database::new();
        let mut o = Options::new("m");
        o.filter = Some(where_expr("upper(title) = \"DOCUMENT 2\""));
        let s = run(&mut src, &mut db, &o).unwrap();
        assert_eq!(s.rows, 1);
        assert_eq!(s.skipped, 2);
    }

    #[test]
    fn limit_stops_early() {
        let mut src = source(rows(50));
        let mut db = Database::new();
        let mut o = Options::new("m");
        o.limit = Some(7);
        assert_eq!(run(&mut src, &mut db, &o).unwrap().rows, 7);
    }

    #[test]
    fn index_is_built_after_load() {
        let mut src = source(rows(20));
        let mut db = Database::new();
        let mut o = Options::new("m");
        o.indexes.push((
            "embed".into(),
            IndexKind::Vector(VectorIndexSpec::default()),
        ));
        run(&mut src, &mut db, &o).unwrap();

        let stats = db.stats();
        let vi = &stats[0].vector_indexes;
        assert_eq!(vi.len(), 1, "the vector index was not built");
        assert_eq!(vi[0].field, "embed");
        assert_eq!(vi[0].count, 20);
    }

    /// An unparseable value has to say which row it was on; in a 2000-row
    /// batch an error without a position is useless.
    #[test]
    fn unparseable_value_reports_row_window() {
        let cols = vec![
            Column::new("score", DataType::Int, "INTEGER"),
        ];
        let mut src = Rows::new(
            cols,
            vec![
                vec![Value::Int(1)],
                vec![Value::Int(2)],
                vec![Value::Text("abc".into())],
            ],
        );
        let mut db = Database::new();
        let mut o = Options::new("m");
        o.batch = 10;
        let e = run(&mut src, &mut db, &o).unwrap_err().to_string();
        assert!(e.contains("expected int"), "{e}");
        assert!(e.contains("row 3, field `score`"), "{e}");
    }

    /// In SQLite the type belongs to the value rather than the column: a
    /// TEXT column can hold a number and an INTEGER column the text of one.
    /// These are not errors but ordinary cases that have to be converted.
    #[test]
    fn loose_source_types_are_reconciled() {
        let cols = vec![
            Column::new("score", DataType::Int, "INTEGER"),
            Column::new("name", DataType::Text, "TEXT"),
            Column::new("active", DataType::Bool, "BOOLEAN"),
            Column::new("ratio", DataType::Float, "REAL"),
        ];
        let mut src = Rows::new(
            cols,
            vec![vec![
                Value::Text(" 42 ".into()),
                Value::Int(7),
                Value::Int(1),
                Value::Text("2.5".into()),
            ]],
        );
        let mut db = Database::new();
        run(&mut src, &mut db, &Options::new("m")).unwrap();

        let Response::Rows(rs) = db
            .execute(&Statement::Select(Select {
                collection: "m".into(),
                project: Some(vec![
                    "score".into(),
                    "name".into(),
                    "active".into(),
                    "ratio".into(),
                ]),
                ..Default::default()
            }))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            rs.rows[0].values,
            vec![
                Value::Int(42),
                Value::Text("7".into()),
                Value::Bool(true),
                Value::Float(2.5),
            ]
        );
    }

    /// A BLOB marked with `--vector` is parsed as f32 little-endian -- the
    /// same encoding fenec-bench writes into SQLite.
    /// fenec-bench'in SQLite'a yazdigi kodlamanin aynisi.
    #[test]
    fn blob_becomes_vector_under_vector_flag() {
        let v: Vec<f32> = vec![1.5, -2.0, 0.25];
        let bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
        let mut src = Rows::new(
            vec![Column::new("embed", DataType::Bytes, "BLOB")],
            vec![vec![Value::Bytes(bytes)]],
        );
        let mut db = Database::new();
        let mut o = Options::new("m");
        o.vectors.push(("embed".into(), 3));
        run(&mut src, &mut db, &o).unwrap();

        let Response::Rows(rs) = db
            .execute(&Statement::Select(Select {
                collection: "m".into(),
                project: Some(vec!["embed".into()]),
                ..Default::default()
            }))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(rs.rows[0].values[0], Value::Vector(v));
    }

    /// Random bytes can produce a NaN when read as f32. If a NaN enters a
    /// vector index, HNSW's sorting panics; the import has to stop first.
    #[test]
    fn non_finite_vector_is_refused() {
        // 0x7FC00000 = NaN
        let bytes = vec![0x00, 0x00, 0xC0, 0x7F, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut src = Rows::new(
            vec![Column::new("embed", DataType::Bytes, "BLOB")],
            vec![vec![Value::Bytes(bytes)]],
        );
        let mut db = Database::new();
        let mut o = Options::new("m");
        o.vectors.push(("embed".into(), 3));
        o.indexes.push((
            "embed".into(),
            IndexKind::Vector(VectorIndexSpec::default()),
        ));
        let e = run(&mut src, &mut db, &o).unwrap_err().to_string();
        assert!(e.contains("not finite"), "{e}");
        assert!(e.contains("row 1, field `embed`"), "{e}");
    }

    /// A wrongly sized BLOB must not be truncated silently.
    #[test]
    fn wrong_sized_blob_is_rejected() {
        let mut src = Rows::new(
            vec![Column::new("embed", DataType::Bytes, "BLOB")],
            vec![vec![Value::Bytes(vec![0; 10])]],
        );
        let mut db = Database::new();
        let mut o = Options::new("m");
        o.vectors.push(("embed".into(), 3));
        let e = run(&mut src, &mut db, &o).unwrap_err().to_string();
        assert!(e.contains("expects 12 bytes"), "{e}");
        assert!(e.contains("row 1, field `embed`"), "{e}");
    }

    #[test]
    fn generated_ids_when_source_id_suppressed() {
        let mut src = source(rows(3));
        let mut db = Database::new();
        let mut o = Options::new("m");
        o.id = IdSource::Generated;
        run(&mut src, &mut db, &o).unwrap();
        // id now stands as an ordinary field.
        assert!(db.collection("m").unwrap().schema.field("source_id").is_some());
    }

    #[test]
    fn null_id_falls_back_to_generated() {
        let mut r = rows(2);
        r[0][0] = Value::Null;
        let mut src = source(r);
        let mut db = Database::new();
        run(&mut src, &mut db, &Options::new("m")).unwrap();
        assert_eq!(count(&db, "m"), 2);
    }

    #[test]
    fn empty_source_creates_empty_collection() {
        let mut src = source(vec![]);
        let mut db = Database::new();
        let s = run(&mut src, &mut db, &Options::new("m")).unwrap();
        assert_eq!(s.rows, 0);
        assert_eq!(count(&db, "m"), 0);
    }
}
