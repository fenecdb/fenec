//! The options that say how a table becomes a collection -- `--where`,
//! `--vector`, `--index`, `--cast`, `--id`, `--batch` -- read from their
//! text. `fenec import` takes them as they are, and `fenec-pg --follow` as
//! `--follow-where` and so on: one reading of each, which two copies of had
//! drifted before (`--index`'s).

use crate::{IdSource, Options};
use fenec_core::query::{Expr, Statement};
use fenec_core::schema::IndexKind;
use fenec_core::value::DataType;

/// Applies `flag`, one of the options above, with its `value`: `None` for
/// a flag that is not one of them.
pub fn option(opts: &mut Options, flag: &str, value: &str) -> Option<Result<(), String>> {
    let done = match flag {
        "--where" => parse_where(value).map(|e| opts.filter = Some(e)),
        "--vector" => parse_vector(value).map(|v| opts.vectors.push(v)),
        "--index" => parse_index(value).map(|i| opts.indexes.push(i)),
        "--cast" => parse_cast(value).map(|c| opts.casts.push(c)),
        "--id" => {
            opts.id = match value {
                "none" => IdSource::Generated,
                column => IdSource::Column(column.to_string()),
            };
            Ok(())
        }
        "--batch" => match value.parse() {
            Ok(0) => Err("--batch cannot be zero".into()),
            Ok(n) => {
                opts.batch = n;
                Ok(())
            }
            Err(_) => Err(format!("--batch expects a number, got `{value}`")),
        },
        _ => return None,
    };
    Some(done)
}

/// `field:N`
pub fn parse_vector(s: &str) -> std::result::Result<(String, usize), String> {
    let (name, dim) = s
        .rsplit_once(':')
        .ok_or_else(|| format!("--vector expects `field:N`, got `{s}`"))?;
    let dim: usize = dim
        .trim()
        .parse()
        .map_err(|_| format!("the --vector dimension must be a number, got `{dim}`"))?;
    if name.is_empty() || dim == 0 {
        return Err(format!("invalid --vector: `{s}`"));
    }
    Ok((name.to_string(), dim))
}

/// Turns the `--where` contents into a FenecQL expression.
///
/// There is no separate expression parser; the expression is wrapped in a
/// `get` body and handed to the real parser. The wrapper's other clauses
/// have to stay empty, otherwise extra clauses could be smuggled in via `--where`.
pub fn parse_where(s: &str) -> std::result::Result<Expr, String> {
    let stmt = fenec_ql::parse_one(&format!("get t where {s}"))
        .map_err(|e| format!("--where could not be parsed: {e}"))?;
    let Statement::Select(sel) = stmt else {
        return Err("--where must be an expression".into());
    };
    if sel.project.is_some()
        || sel.near.is_some()
        || !sel.order.is_empty()
        || sel.limit.is_some()
        || sel.offset != 0
        || sel.count
    {
        return Err("--where only takes a condition expression".into());
    }
    sel.filter.ok_or_else(|| "--where is empty".to_string())
}

/// `field=type`
pub fn parse_cast(s: &str) -> std::result::Result<(String, DataType), String> {
    let (name, ty) = s
        .split_once('=')
        .ok_or_else(|| format!("--cast expects `field=type`, got `{s}`"))?;
    let ty = crate::map::parse_type(ty).ok_or_else(|| format!("unknown type: `{ty}`"))?;
    if name.is_empty() {
        return Err(format!("invalid --cast: `{s}`"));
    }
    Ok((name.to_string(), ty))
}

/// `field@hash`, `field@sorted` or `field@hnsw[(metric, m=.., ef_construction=.., ef_search=.., quant=..)]`
pub fn parse_index(s: &str) -> std::result::Result<(String, IndexKind), String> {
    let (name, spec) = s.split_once('@').ok_or_else(|| {
        format!("--index expects `field@hash`, `field@sorted` or `field@hnsw(...)`, got `{s}`")
    })?;
    if name.is_empty() {
        return Err(format!("invalid --index: `{s}`"));
    }
    // The index is read by FenecQL's own parser, as `--where` is: a copy of
    // it here had drifted -- no clamping of `m` and `ef`, no `ef` and `ef_c`,
    // a `quant` spelt one way, no `@text`.
    match fenec_ql::parse_one(&format!("create index on imported ({name}) @{spec}")) {
        Ok(Statement::CreateIndex { field, kind, .. }) => Ok((field, kind)),
        Ok(_) => Err(format!("invalid --index: `{s}`")),
        Err(e) => Err(format!("--index `{s}`: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fenec_core::schema::{Metric, Quant, VectorIndexSpec};
    use fenec_core::value::VecPrec;

    #[test]
    fn vector_flag_parses() {
        assert_eq!(parse_vector("embed:384").unwrap(), ("embed".into(), 384));
        assert!(parse_vector("embed").is_err());
        assert!(parse_vector("embed:0").is_err());
        assert!(parse_vector("embed:x").is_err());
    }

    #[test]
    fn cast_flag_parses() {
        assert_eq!(parse_cast("a=int").unwrap(), ("a".into(), DataType::Int));
        assert_eq!(
            parse_cast("e=vector<8, f16>").unwrap(),
            ("e".into(), DataType::Vector(8, VecPrec::F16))
        );
        assert!(parse_cast("a=decimal").is_err());
        assert!(parse_cast("a").is_err());
    }

    #[test]
    fn index_flag_parses() {
        assert_eq!(parse_index("k@hash").unwrap().1, IndexKind::Hash);

        let (name, IndexKind::Vector(v)) = parse_index("embed@hnsw").unwrap() else {
            panic!()
        };
        assert_eq!(name, "embed");
        assert_eq!(v, VectorIndexSpec::default().resolved());

        let (_, IndexKind::Vector(v)) =
            parse_index("embed@hnsw(l2, m=32, ef_construction=400, ef_search=64)").unwrap()
        else {
            panic!()
        };
        assert_eq!(v.metric, Metric::L2);
        assert_eq!(v.m, 32);
        assert_eq!(v.ef_construction, 400);
        assert_eq!(v.ef_search, 64);

        // Bit codes take a wider beam unless one is named, as in FenecQL.
        let (_, IndexKind::Vector(v)) = parse_index("embed@hnsw(cosine, quant=bit)").unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (v.quant, v.ef_search),
            (Quant::Bit, fenec_core::schema::BIT_EF_SEARCH)
        );
        let (_, IndexKind::Vector(v)) =
            parse_index("embed@hnsw(cosine, quant=int8, ef_search=64)").unwrap()
        else {
            panic!()
        };
        assert_eq!((v.quant, v.ef_search), (Quant::Int8, 64));
        assert!(parse_index("embed@hnsw(cosine, quant=pq)").is_err());
    }

    #[test]
    fn where_flag_parses_fenecql() {
        assert!(parse_where(r#"category = "book" and score >= 10"#).is_ok());
        assert!(parse_where("id > 100").is_ok());
        assert!(parse_where(r#"tags has "rust""#).is_ok());
        assert!(parse_where("title is not null").is_ok());
    }

    /// No extra clause may be smuggled into the wrapper; otherwise `--where`
    /// would silently change the rest of the query too.
    #[test]
    fn where_flag_refuses_extra_clauses() {
        assert!(parse_where("score > 1 limit 5").is_err());
        assert!(parse_where("score > 1 order score desc").is_err());
        assert!(parse_where("score > 1 near embed [1,2]").is_err());
        assert!(parse_where("").is_err());
        assert!(parse_where("this is not an expression )(").is_err());
    }

    #[test]
    fn bad_index_flags_are_refused() {
        assert!(parse_index("embed").is_err(), "@ is missing");
        assert!(parse_index("embed@btree").is_err());
        assert!(parse_index("embed@hnsw(distance)").is_err());
        assert!(parse_index("embed@hnsw(cosine, k=3)").is_err());
        assert!(parse_index("embed@hnsw(cosine").is_err());
        assert!(parse_index("@hash").is_err());
    }
}
