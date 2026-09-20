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

pub use parser::{parse, parse_one, MAX_EXPR_DEPTH};

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
        assert_eq!(schema.field("year").unwrap().index, IndexKind::Hash);
        let IndexKind::Vector(spec) = schema.field("embed").unwrap().index else {
            panic!()
        };
        assert_eq!(spec.metric, Metric::Cosine);
        assert_eq!(spec.m, 32);
        assert_eq!(spec.ef_search, 100);
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
    /// in `fenec-pg` a single query would take the whole server down. The limit
    /// is applied at parse time.
    #[test]
    fn expression_depth_is_bounded() {
        // libtest runs tests on 2 MiB threads and frames are ~10x larger in a
        // debug build: this test would `abort` on that stack -- not a
        // catchable panic but a stack overflow that kills the process. The
        // budget matches the stack `fenec-pg` gives its sessions (see
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
