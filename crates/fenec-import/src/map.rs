//! Derives a fenecdb schema from the source columns.
//!
//! The mapping lives in one place so `--dry-run` and the real load make the
//! same decision: what the plan says is what the load writes.

use crate::{Column, IdSource, Options};
use fenec_core::error::{Error, Result};
use fenec_core::schema::{Field, IndexKind, Schema};
use fenec_core::value::{DataType, VecPrec};

/// When the `id` column is not a positive integer, the field is opened under
/// this name instead. `id` is reserved in fenecdb and cannot be declared in a
/// schema (`Schema::new`).
pub const RENAMED_ID: &str = "source_id";

/// What a source column maps to in the target.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// A schema field; the name it carries is the target name (possibly renamed).
    Field(String),
    /// The document's `id`. Not a schema field.
    Id,
}

/// The derived load plan.
#[derive(Debug, Clone)]
pub struct Plan {
    pub schema: Schema,
    /// Targets in source column order; `targets[i]` corresponds to `columns[i]`.
    pub targets: Vec<Target>,
    /// Indexes to build once the load finishes.
    pub indexes: Vec<(String, IndexKind)>,
    pub warnings: Vec<String>,
}

impl Plan {
    /// Whether the source `id` column is used.
    pub fn uses_source_id(&self) -> bool {
        self.targets.contains(&Target::Id)
    }
}

/// Produces a plan from the column list. It writes nothing.
pub fn plan(columns: &[Column], opts: &Options) -> Result<Plan> {
    if columns.is_empty() {
        return Err(Error::Query("the source table has no columns".into()));
    }
    let mut warnings = Vec::new();
    let id_col = pick_id(columns, opts, &mut warnings)?;

    let mut fields: Vec<Field> = Vec::with_capacity(columns.len());
    let mut targets: Vec<Target> = Vec::with_capacity(columns.len());

    for (i, c) in columns.iter().enumerate() {
        if Some(i) == id_col {
            targets.push(Target::Id);
            continue;
        }
        let ty = resolve_type(c, opts, &mut warnings)?;
        let name = if c.name == "id" {
            warnings.push(format!(
                "the `id` column was not used as the document id, it went into the `{RENAMED_ID}` field"
            ));
            RENAMED_ID.to_string()
        } else {
            c.name.clone()
        };
        fields.push(Field::new(name.clone(), ty));
        targets.push(Target::Field(name));
    }

    if fields.is_empty() {
        return Err(Error::Query(
            "no field is left in the target: every column was consumed as `id`".into(),
        ));
    }

    // The indexes are validated before the schema is built, so `--dry-run`
    // reports the error before writing; `CreateIndex` performs the same check again.
    for (field, kind) in &opts.indexes {
        let f = fields.iter().find(|f| &f.name == field).ok_or_else(|| {
            Error::NotFound(format!("there is no `{field}` field, the index cannot be built"))
        })?;
        if matches!(kind, IndexKind::Vector(_)) && !matches!(f.ty, DataType::Vector(..)) {
            return Err(Error::Type(format!(
                "field `{}` is {}, no vector index can be built -- `--vector {}:<N>` may be needed",
                field,
                f.ty.name(),
                field
            )));
        }
    }

    Ok(Plan {
        schema: Schema::new(opts.into.clone(), fields)?,
        targets,
        indexes: opts.indexes.clone(),
        warnings,
    })
}

/// Which column becomes the document id.
fn pick_id(
    columns: &[Column],
    opts: &Options,
    warnings: &mut Vec<String>,
) -> Result<Option<usize>> {
    match &opts.id {
        IdSource::Generated => Ok(None),
        IdSource::Column(name) => {
            let i = columns
                .iter()
                .position(|c| &c.name == name)
                .ok_or_else(|| Error::NotFound(format!("the source has no `{name}` column")))?;
            if columns[i].ty != Some(DataType::Int) {
                return Err(Error::Type(format!(
                    "the `{name}` column is of type {}; a document id must be a positive integer",
                    columns[i].source_type
                )));
            }
            Ok(Some(i))
        }
        IdSource::Auto => {
            let Some(i) = columns.iter().position(|c| c.name == "id") else {
                return Ok(None);
            };
            if columns[i].ty == Some(DataType::Int) {
                Ok(Some(i))
            } else {
                // Rather than silently falling back to a fenecdb id, say what
                // happened: the source ids are not preserved, and on a rerun
                // this produces new documents instead of an upsert.
                warnings.push(format!(
                    "the `id` column is of type {}, it cannot be used as the document id; \
                     fenecdb will assign its own",
                    columns[i].source_type
                ));
                Ok(None)
            }
        }
    }
}

/// The target type of a column: `--cast` first, then `--vector`, then inference.
fn resolve_type(c: &Column, opts: &Options, warnings: &mut Vec<String>) -> Result<DataType> {
    if let Some((_, ty)) = opts.casts.iter().find(|(n, _)| *n == c.name) {
        return Ok(ty.clone());
    }

    if let Some((_, dim)) = opts.vectors.iter().find(|(n, _)| *n == c.name) {
        match &c.ty {
            // Bytes: an f32 little-endian array. List/vector: already numeric.
            None | Some(DataType::Bytes) | Some(DataType::List(_)) | Some(DataType::Vector(..)) => {}
            Some(other) => {
                return Err(Error::Type(format!(
                    "field `{}` is {}; `--vector` only applies to bytes, list or vector columns",
                    c.name,
                    other.name()
                )))
            }
        }
        return Ok(DataType::Vector(*dim, VecPrec::F32));
    }

    match &c.ty {
        Some(ty) => {
            if let Some(note) = &c.note {
                warnings.push(format!("{}: {note}", c.name));
            }
            Ok(ty.clone())
        }
        None => Err(Error::Type(format!(
            "column `{}` ({}) has no counterpart in fenecdb{}; pick a type with `--cast {}=<type>`",
            c.name,
            c.source_type,
            c.note
                .as_ref()
                .map(|n| format!(" -- {n}"))
                .unwrap_or_default(),
            c.name
        ))),
    }
}

/// Parses a `--cast` value: `int`, `float`, `text`, `bytes`, `bool`,
/// `timestamp`, `vector<N>`, `vector<N, f16>`, `[type]`.
pub fn parse_type(s: &str) -> Option<DataType> {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        return Some(DataType::List(Box::new(parse_type(inner)?)));
    }
    if let Some(args) = s.strip_prefix("vector<").and_then(|r| r.strip_suffix('>')) {
        let mut it = args.split(',');
        let dim: usize = it.next()?.trim().parse().ok()?;
        let prec = match it.next().map(|p| p.trim().to_ascii_lowercase()) {
            None => VecPrec::F32,
            Some(p) if p == "f16" => VecPrec::F16,
            Some(p) if p == "f32" => VecPrec::F32,
            Some(_) => return None,
        };
        if it.next().is_some() || dim == 0 {
            return None;
        }
        return Some(DataType::Vector(dim, prec));
    }
    Some(match s.to_ascii_lowercase().as_str() {
        "bool" => DataType::Bool,
        "int" => DataType::Int,
        "float" => DataType::Float,
        "text" => DataType::Text,
        "bytes" => DataType::Bytes,
        "timestamp" => DataType::Timestamp,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols() -> Vec<Column> {
        vec![
            Column::new("id", DataType::Int, "INTEGER"),
            Column::new("title", DataType::Text, "TEXT"),
            Column::new("embed", DataType::Bytes, "BLOB"),
        ]
    }

    #[test]
    fn integer_id_becomes_document_id() {
        let p = plan(&cols(), &Options::new("articles")).unwrap();
        assert_eq!(p.targets[0], Target::Id);
        assert!(p.uses_source_id());
        // `id` must not be a schema field: Schema::new rejects it.
        assert_eq!(p.schema.fields.len(), 2);
        assert!(p.schema.field("id").is_none());
    }

    /// A text `id` cannot be used as a schema field name (`Schema::new`
    /// rejects it), so it is renamed and the user is warned.
    #[test]
    fn non_integer_id_is_renamed_not_dropped() {
        let mut c = cols();
        c[0] = Column::new("id", DataType::Text, "TEXT");
        let p = plan(&c, &Options::new("m")).unwrap();
        assert_eq!(p.targets[0], Target::Field(RENAMED_ID.into()));
        assert!(p.schema.field(RENAMED_ID).is_some());
        assert_eq!(p.warnings.len(), 2, "{:?}", p.warnings);
    }

    #[test]
    fn generated_id_ignores_source_column() {
        let mut o = Options::new("m");
        o.id = IdSource::Generated;
        let p = plan(&cols(), &o).unwrap();
        assert_eq!(p.targets[0], Target::Field(RENAMED_ID.into()));
        assert!(!p.uses_source_id());
    }

    #[test]
    fn vector_flag_turns_blob_into_vector() {
        let mut o = Options::new("m");
        o.vectors.push(("embed".into(), 8));
        let p = plan(&cols(), &o).unwrap();
        assert_eq!(
            p.schema.field("embed").unwrap().ty,
            DataType::Vector(8, VecPrec::F32)
        );
    }

    #[test]
    fn vector_flag_rejects_scalar_column() {
        let mut o = Options::new("m");
        o.vectors.push(("title".into(), 8));
        let e = plan(&cols(), &o).unwrap_err().to_string();
        assert!(e.contains("--vector"), "{e}");
    }

    /// An unsupported type must not vanish silently: `--cast` is required.
    #[test]
    fn unsupported_column_demands_cast() {
        let c = vec![Column::unsupported(
            "price",
            "numeric",
            "fenecdb has no decimal",
        )];
        let e = plan(&c, &Options::new("m")).unwrap_err().to_string();
        assert!(e.contains("--cast price="), "{e}");

        let mut o = Options::new("m");
        o.casts.push(("price".into(), DataType::Int));
        let p = plan(&c, &o).unwrap();
        assert_eq!(p.schema.field("price").unwrap().ty, DataType::Int);
    }

    #[test]
    fn cast_overrides_inferred_type() {
        let mut o = Options::new("m");
        o.casts.push(("title".into(), DataType::Bytes));
        let p = plan(&cols(), &o).unwrap();
        assert_eq!(p.schema.field("title").unwrap().ty, DataType::Bytes);
    }

    #[test]
    fn index_on_non_vector_field_fails_in_plan() {
        let mut o = Options::new("m");
        o.indexes.push((
            "title".into(),
            IndexKind::Vector(Default::default()),
        ));
        let e = plan(&cols(), &o).unwrap_err().to_string();
        assert!(e.contains("--vector title:<N>"), "{e}");
    }

    #[test]
    fn index_on_missing_field_fails_in_plan() {
        let mut o = Options::new("m");
        o.indexes.push(("nosuch".into(), IndexKind::Hash));
        assert!(plan(&cols(), &o).is_err());
    }

    #[test]
    fn lossy_mapping_surfaces_as_warning() {
        let c = vec![Column::new("t", DataType::Text, "uuid").note("uuid taken as text")];
        let p = plan(&c, &Options::new("m")).unwrap();
        assert_eq!(p.warnings, vec!["t: uuid taken as text"]);
    }

    #[test]
    fn type_parser_covers_every_fenecql_type() {
        assert_eq!(parse_type("int"), Some(DataType::Int));
        assert_eq!(parse_type("TEXT"), Some(DataType::Text));
        assert_eq!(parse_type("timestamp"), Some(DataType::Timestamp));
        assert_eq!(
            parse_type("vector<384>"),
            Some(DataType::Vector(384, VecPrec::F32))
        );
        assert_eq!(
            parse_type("vector<384, f16>"),
            Some(DataType::Vector(384, VecPrec::F16))
        );
        assert_eq!(
            parse_type("[text]"),
            Some(DataType::List(Box::new(DataType::Text)))
        );
        assert_eq!(parse_type("vector<0>"), None);
        assert_eq!(parse_type("vector<8, f8>"), None);
        assert_eq!(parse_type("decimal"), None);
    }
}
