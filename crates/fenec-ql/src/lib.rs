//! # FenecQL
//!
//! fenecdb's query language. Not SQL: vector search (`near`) and document
//! writes (`put`) sit at the core of the language, not bolted on afterwards.
//!
//! ```
//! use fenec_ql::parse_one;
//! let stmt = parse_one(r#"get docs where year >= 2020 near embed [0.1, 0.2] limit 5"#).unwrap();
//! ```

pub mod lexer;
pub mod parser;

pub use parser::{
    parse, parse_exact, parse_for, parse_one, parse_one_for, parse_select_list, spans,
    MAX_EXPR_DEPTH,
};

use fenec_core::error::{Error, Result};
use fenec_core::query::Statement;
use fenec_core::schema::{Field, IndexKind, Schema};
use fenec_core::value::DataType;

/// A schema written as FenecQL -- a `schema.fenecql` file: `create
/// collection` statements, and `create index` over their fields and the
/// paths into their json fields -- as the collections it declares, what a
/// description's `fenecql` holds (`fenec_core::declared`). The language
/// every SDK already speaks, so each manages its schema with this text and
/// no builder of its own; and what the browser module reads a schema in,
/// since it carries the parser and not the reader of a description's JSON
/// collections.
pub fn schema_text(src: &str) -> Result<Vec<Schema>> {
    let mut out: Vec<Schema> = Vec::new();
    let refuse = |why: String| Error::Query(format!("schema text: {why}"));
    for s in parse(src)? {
        match s {
            Statement::CreateCollection { schema, .. } => {
                if out.iter().any(|o| o.name == schema.name) {
                    return Err(refuse(format!("`{}` is declared twice", schema.name)));
                }
                out.push(schema);
            }
            Statement::CreateIndex {
                collection,
                field,
                kind,
                ..
            } => {
                let s = out
                    .iter_mut()
                    .find(|s| s.name == collection)
                    .ok_or_else(|| {
                        refuse(format!("an index on `{collection}`, declared after none"))
                    })?;
                if s.path_of(&field)?.is_some() {
                    kind.check(&field, &DataType::Json)?;
                    if s.path(&field).is_some() {
                        return Err(refuse(format!("`{collection}.{field}` has two indexes")));
                    }
                    s.add_path(Field::new(field, DataType::Json).indexed(kind));
                    continue;
                }
                let f = s
                    .fields
                    .iter_mut()
                    .find(|f| f.name == field)
                    .ok_or_else(|| {
                        refuse(format!(
                            "an index on `{collection}.{field}`, no field of it"
                        ))
                    })?;
                if f.index != IndexKind::None {
                    return Err(refuse(format!(
                        "`{collection}.{field}` has two indexes: a field takes one"
                    )));
                }
                kind.check(&field, &f.ty)?;
                f.index = kind;
            }
            _ => {
                return Err(refuse(
                    "a schema is `create collection` and `create index` statements".into(),
                ))
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fenec_core::query::*;
    use fenec_core::schema::{IndexKind, Metric};
    use fenec_core::value::{DataType, Value};

    #[test]
    fn create_with_vector_index() {
        let s = parse_one(
            "create collection docs (
               title text,
               tags [text],
               year int @hash,
               embed vector<384> @hnsw(cosine, m=32, ef_search=100)
             )",
        )
        .unwrap();
        let Statement::CreateCollection { schema, .. } = s else {
            panic!()
        };
        assert_eq!(schema.fields.len(), 4);
        assert_eq!(
            schema.field("embed").unwrap().ty,
            DataType::Vector(384, fenec_core::value::VecPrec::F32)
        );
        assert_eq!(
            schema.field("tags").unwrap().ty,
            DataType::List(Box::new(DataType::Text))
        );
        assert_eq!(schema.field("year").unwrap().index, IndexKind::HASH);
        let IndexKind::Vector(spec) = schema.field("embed").unwrap().index else {
            panic!()
        };
        assert_eq!(spec.metric, Metric::Cosine);
        assert_eq!(spec.m, 32);
        assert_eq!(spec.ef_search, 100);
    }

    #[test]
    fn unique_is_a_field_index_and_a_create_index() {
        let s =
            parse_one("create collection u (email text @unique, n int required @hash)").unwrap();
        let Statement::CreateCollection { schema, .. } = s else {
            panic!()
        };
        assert_eq!(schema.field("email").unwrap().index, IndexKind::UNIQUE);
        assert_eq!(schema.field("n").unwrap().index, IndexKind::HASH);
        let Statement::CreateIndex { kind, .. } =
            parse_one("create index on u (email) @UNIQUE").unwrap()
        else {
            panic!()
        };
        assert!(kind.is_unique());
    }

    #[test]
    fn alter_adds_drops_and_renames() {
        use fenec_core::query::Alter;
        let alter = |q: &str| match parse_one(q).unwrap() {
            Statement::AlterCollection { collection, change } => (collection, change),
            s => panic!("{s:?}"),
        };
        let (c, change) = alter("alter collection orders add field note text @hash");
        assert_eq!(c, "orders");
        let Alter::AddField(f) = change else { panic!() };
        assert_eq!((f.name.as_str(), f.index), ("note", IndexKind::HASH));
        assert_eq!(
            alter("ALTER COLLECTION orders ADD n int").1,
            Alter::AddField(fenec_core::schema::Field::new("n", DataType::Int))
        );
        assert_eq!(
            alter("alter collection orders drop field note").1,
            Alter::DropField("note".into())
        );
        assert_eq!(
            alter("alter collection orders rename field total to amount").1,
            Alter::RenameField("total".into(), "amount".into())
        );
        assert!(parse_one("alter collection orders rename total amount").is_err());
        assert!(parse_one("alter table orders add field x int").is_err());
    }

    #[test]
    fn put_batch() {
        let s = parse_one(r#"put docs [ {title: "a"}, {title: "b", embed: [0.1, 0.2]} ]"#).unwrap();
        let Statement::Put { docs, .. } = s else {
            panic!()
        };
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[1][1].1, Expr::Lit(Value::Vector(vec![0.1, 0.2])));
    }

    #[test]
    fn select_full() {
        let s = parse_one(
            r#"get docs select id, title
                 where year >= 2020 and (tags has "ai" or title ~ "rust")
                 near embed $1 ef 128
                 limit 10 offset 5"#,
        )
        .unwrap();
        let Statement::Select(sel) = s else { panic!() };
        assert_eq!(sel.project, Some(vec!["id".into(), "title".into()]));
        assert_eq!(sel.limit, Some(10));
        assert_eq!(sel.offset, 5);
        let near = sel.near.unwrap();
        assert_eq!(near.field, "embed");
        assert_eq!(near.ef, Some(128));
        assert_eq!(near.vector, Expr::Param(0));
        assert!(sel.filter.is_some());
    }

    /// The parameter count in the `Describe` response comes from here; since
    /// `$n` is stored zero-based, it was once reported one too low.
    #[test]
    fn param_count_is_highest_dollar_number() {
        let n = |q: &str| parse_one(q).unwrap().max_param();
        assert_eq!(n("get t select a"), 0);
        assert_eq!(n("get t where y >= $1"), 1);
        assert_eq!(n("get t where y >= $1 and a ~ $2"), 2);
        // Out-of-order use: the largest one decides.
        assert_eq!(n("get t where a = $3 and b = $1"), 3);
        assert_eq!(n("get t near e $1 limit 3"), 1);
        assert_eq!(n("get t where y >= $2 near e $1"), 2);
        assert_eq!(n("put t {a: $1, b: $2}"), 2);
        assert_eq!(n("set t {a: $1} where b = $2"), 2);
        assert_eq!(n("del t where a = $1"), 1);
        assert_eq!(n("get t where a in [$1, $2, $3]"), 3);
        // A `lookup`'s own `where` is part of the same statement, so its
        // parameters have to be counted too: `Describe` answers with this
        // number before the query runs, and a client that is told there are
        // none will not send any.
        assert_eq!(n("get t lookup c on k where s >= $1"), 1);
        assert_eq!(n("get t where y >= $1 lookup c on k where s >= $2"), 2);
        assert_eq!(n("get t where cosine(e, $1) > 0.5"), 1);
        assert_eq!(n("collections"), 0);
    }

    #[test]
    fn precedence_and_over_or() {
        let s = parse_one("get t where a = 1 or b = 2 and c = 3").unwrap();
        let Statement::Select(sel) = s else { panic!() };
        // must be or(a=1, and(b=2, c=3))
        match sel.filter.unwrap() {
            Expr::Or(_, right) => assert!(matches!(*right, Expr::And(_, _))),
            other => panic!("expected Or, found {other:?}"),
        }
    }

    #[test]
    fn functions_and_is_null() {
        let s = parse_one("get t where lower(title) = \"x\" and body is not null").unwrap();
        let Statement::Select(sel) = s else { panic!() };
        let Expr::And(l, r) = sel.filter.unwrap() else {
            panic!()
        };
        assert!(matches!(*l, Expr::Cmp(CmpOp::Eq, _, _)));
        assert!(matches!(*r, Expr::Not(_)));
    }

    #[test]
    fn errors_are_positional() {
        let e = parse_one("get docs where").unwrap_err().to_string();
        assert!(e.contains("position"), "{e}");
    }

    /// Recursive descent used to overflow the stack on a deep expression. A
    /// stack overflow is not a catchable panic but an `abort` of the process:
    /// in `fenec-server` a single query would take the whole server down. The limit
    /// is applied at parse time.
    #[test]
    fn expression_depth_is_bounded() {
        // libtest runs tests on 2 MiB threads and frames are ~10x larger in a
        // debug build: this test would `abort` on that stack -- not a
        // catchable panic but a stack overflow that kills the process. The
        // budget matches the stack `fenec-server` gives its sessions (see
        // MAX_EXPR_DEPTH).
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(deep_expression_cases)
            .unwrap()
            .join()
            .unwrap();
    }

    fn deep_expression_cases() {
        let deep = format!(
            "get t where {}n = 1{}",
            "(".repeat(MAX_EXPR_DEPTH),
            ")".repeat(MAX_EXPR_DEPTH)
        );
        let e = parse_one(&deep).unwrap_err().to_string();
        assert!(e.contains("too deep"), "{e}");

        // A chain is depth too: `a or b or c` is a left-leaning tree, and
        // even though parsing is a loop, evaluation recurses to that depth.
        let chain: Vec<String> = (0..MAX_EXPR_DEPTH * 2)
            .map(|i| format!("n = {i}"))
            .collect();
        let e = parse_one(&format!("get t where {}", chain.join(" or ")))
            .unwrap_err()
            .to_string();
        assert!(e.contains("too deep"), "{e}");

        // An expression below the limit parses normally.
        let ok = format!(
            "get t where {}n = 1{}",
            "(".repeat(MAX_EXPR_DEPTH - 1),
            ")".repeat(MAX_EXPR_DEPTH - 1)
        );
        assert!(parse_one(&ok).is_ok());
    }
}
