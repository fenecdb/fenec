//! # FenecQL
//!
//! fenecdb's query language. Not SQL: vector search (`near`) and document
//! writes (`put`) sit at the core of the language, not bolted on afterwards.
//!
//! ```
//! use fenec_ql::parse_one;
//! let stmt = parse_one(r#"get docs where year >= 2020 near embed [0.1, 0.2] limit 5"#).unwrap();
//! ```

mod join;
pub mod lexer;
pub mod parser;
pub mod sqltypes;

pub use parser::{
    parse, parse_exact, parse_facet_list, parse_for, parse_group_list, parse_one, parse_one_for,
    parse_select_list, reading, spans, Reading, Refusal, Rest, SelectList, MAX_EXPR_DEPTH,
};

/// The grammar's epoch: raised by every change that could read a text
/// stored before it as another tree. A named statement is stored as its
/// text, with the epoch it was parsed under and its tree's digest
/// (`fenec_core::named::Definition::digest`); an open parses it again and
/// marks it broken where the digest is another, rather than run a tree its
/// author never wrote. The corpus rule keeps that from happening
/// (`docs/design/fenecql.md`): no text that parsed reads otherwise, so the
/// epoch moves only where a change means to, and says so.
pub const GRAMMAR_EPOCH: u32 = 1;

use fenec_core::error::Result;
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
    // The description's own refusals and words (`declared::bad`): one place
    // for both forms, and fewer texts in the browser module.
    use fenec_core::declared::{bad, cat};
    let mut out: Vec<Schema> = Vec::new();
    for s in parse(src)? {
        match s {
            Statement::CreateCollection { schema, .. } => {
                if out.iter().any(|o| o.name == schema.name) {
                    return Err(bad(&schema.name, "is declared twice"));
                }
                out.push(schema);
            }
            Statement::CreateIndex {
                collection,
                field,
                kind,
                ..
            } => {
                let at = cat(&[&collection, ".", &field]);
                let Some(s) = out.iter_mut().find(|s| s.name == collection) else {
                    return Err(bad(&at, "is indexed before its collection is declared"));
                };
                let path = s.path_of(&field)?.is_some();
                let f = match path {
                    true => s.path(&field),
                    false => s.field(&field),
                };
                match f {
                    None if !path => return Err(bad(&at, "is indexed and not declared")),
                    Some(f) if path || f.index != IndexKind::None => {
                        return Err(bad(&at, "has two indexes: a field takes one"))
                    }
                    _ => {}
                }
                match path {
                    true => {
                        kind.check(&field, &DataType::Json)?;
                        s.add_path(Field::new(field, DataType::Json).indexed(kind));
                    }
                    false => {
                        let pos = s.field_pos(&field).unwrap_or_default();
                        kind.check(&field, &s.fields[pos].ty)?;
                        s.fields[pos].index = kind;
                    }
                }
            }
            // An index over several fields, made after its collection.
            Statement::CreateComposite {
                collection, index, ..
            } => {
                let Some(s) = out.iter_mut().find(|s| s.name == collection) else {
                    return Err(bad(&collection, "is indexed before it is declared"));
                };
                s.add_composite(index)?;
            }
            _ => {
                return Err(bad(
                    "a schema",
                    "is `create collection` and `create index` statements",
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

    /// An index over several fields, as a line of `create collection` and
    /// as `create index`, written out as FenecQL and as a description and
    /// read back as the same schema; and the plan that brings a database to
    /// a declaration of one.
    #[test]
    fn an_index_over_several_fields_reads_back_and_plans() {
        use fenec_core::declared::{self, Mode};
        use fenec_core::schema::Composite;
        let stmt = parse_one(
            "create collection posts (owner text, at timestamp, slug text, meta json, \
             @sorted(owner, at desc), @unique(owner, slug))",
        )
        .unwrap();
        let Statement::CreateCollection { schema, .. } = stmt else {
            panic!()
        };
        assert_eq!(
            schema.composites,
            vec![
                Composite {
                    keys: vec![("owner".into(), false), ("at".into(), true)],
                    unique: false
                },
                Composite {
                    keys: vec![("owner".into(), false), ("slug".into(), false)],
                    unique: true
                },
            ]
        );
        let text = declared::fenecql(&[&schema]);
        assert!(
            text.contains("@sorted(owner, at desc), @unique(owner, slug))"),
            "{text}"
        );
        assert_eq!(schema_text(&text).unwrap(), vec![schema.clone()]);
        let described = declared::describe(&[&schema]);
        assert!(
            described.contains(
                r#""indexes":[{"kind":"sorted","keys":[{"field":"owner"},{"field":"at","desc":true}]}"#
            ),
            "{described}"
        );
        assert_eq!(
            declared::parse(&described).unwrap().collections,
            vec![schema.clone()]
        );
        // In the binary schema too.
        let encoded = schema.encode();
        assert_eq!(Schema::decode(&encoded, &mut 0).unwrap(), schema);
        // Made by `create index` after its collection, in a schema text.
        let later = schema_text(
            "create collection posts (owner text, at timestamp, slug text, meta json)\n\
             create index on posts (owner, at desc) @sorted\n\
             create index on posts (owner, slug) @unique",
        )
        .unwrap();
        assert_eq!(later, vec![schema.clone()]);

        // Against a database with none: made. With other directions, and
        // with one the code lacks: refused, the fix a migration dropping it.
        let plain =
            parse_one("create collection posts (owner text, at timestamp, slug text, meta json)")
                .unwrap();
        let Statement::CreateCollection { schema: bare, .. } = plain else {
            panic!()
        };
        let p = declared::plan(&[&bare], std::slice::from_ref(&schema), Mode::Apply);
        assert_eq!(
            p.statements,
            vec![
                "create index on posts (owner, at desc) @sorted",
                "create index on posts (owner, slug) @unique"
            ]
        );
        assert!(p.refusals.is_empty());
        let mut other = schema.clone();
        other.composites[0].keys[1].1 = false;
        other.composites.push(Composite {
            keys: vec![("slug".into(), false), ("at".into(), false)],
            unique: false,
        });
        let p = declared::plan(&[&other], std::slice::from_ref(&schema), Mode::Apply);
        let kinds: Vec<_> = p.refusals.iter().map(|r| (r.kind, r.fix.clone())).collect();
        assert_eq!(
            kinds,
            vec![
                (
                    "index_changed",
                    "the migration `drop index on posts (owner, at)`, after which the plan makes \
                     the one declared"
                        .to_string()
                ),
                (
                    "index_removed",
                    "the migration `drop index on posts (slug, at)`".to_string()
                ),
            ]
        );
        // Following, the database's own extra one is its owner's; a missing
        // one is refused.
        let p = declared::plan(&[&other], std::slice::from_ref(&schema), Mode::Follow);
        assert_eq!(p.refusals.len(), 1, "{:?}", p.refusals);
        let p = declared::plan(&[&bare], std::slice::from_ref(&schema), Mode::Follow);
        assert_eq!(p.refusals[0].kind, "index_missing");
        assert!(p.statements.is_empty());

        let Statement::DropComposite { keys, .. } =
            parse_one("drop index if exists on posts (owner, at)").unwrap()
        else {
            panic!()
        };
        assert_eq!(keys, ["owner", "at"]);
    }

    /// A point field and its index, written out and read back as the same
    /// schema.
    #[test]
    fn a_point_field_takes_geo() {
        let stmt = parse_one("create collection p (name text, loc geo @geo)").unwrap();
        let Statement::CreateCollection { schema, .. } = stmt else {
            panic!()
        };
        assert_eq!(schema.fields[1].ty, DataType::Geo);
        assert_eq!(schema.fields[1].index, IndexKind::Geo);
        let text = fenec_core::declared::fenecql(&[&schema]);
        assert!(text.contains("loc geo @geo"), "{text}");
        assert_eq!(schema_text(&text).unwrap(), vec![schema]);
        let Statement::CreateIndex { kind, .. } =
            parse_one("create index on p (loc) @geo").unwrap()
        else {
            panic!()
        };
        assert_eq!(kind, IndexKind::Geo);
        assert!(parse_one("create collection p (loc geo @hash)").is_err());
        assert!(parse_one("create collection p (n int @geo)").is_err());
        // The functions are calls, and `near` takes a point as it takes a
        // vector.
        let Statement::Select(s) =
            parse_one("get p where distance(loc, $1) <= 500 near loc $1 limit 10").unwrap()
        else {
            panic!()
        };
        assert_eq!(s.near.unwrap().field, "loc");
        assert!(matches!(s.filter, Some(Expr::Cmp(CmpOp::Le, ..))));
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
        assert_eq!(sel.limit, Some(Count::N(10)));
        assert_eq!(sel.offset, Count::N(5));
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
        // A count's parameter is one of the statement's, wherever it is.
        assert_eq!(n("get t where a = $1 limit $2 offset $3"), 3);
        assert_eq!(n("get t limit $2"), 2);
        assert_eq!(n("get t facet b top $4"), 4);
        assert_eq!(n("get t lookup c on k limit $1 offset $5"), 5);
        assert_eq!(n("get t lookup c on k lookup d on j limit $6"), 6);
        assert_eq!(n("set t {a: 1} order a limit $3"), 3);
        assert_eq!(n("del t where a = $1 order a limit $2"), 2);
        assert_eq!(n("get t where a in (get u select b limit $7)"), 7);
    }

    /// `limit`, `offset` and a facet's `top` take a parameter wherever they
    /// take a number, and the number written is held to what a parameter's
    /// value is held to, in the same words.
    #[test]
    fn counts_take_a_parameter() {
        let sel = |q: &str| match parse_one(q).unwrap() {
            Statement::Select(s) => s,
            other => panic!("{other:?}"),
        };
        let s = sel("get t where a = $1 limit $2 offset $3 facet b top $4");
        assert_eq!(s.limit, Some(Count::Param(1)));
        assert_eq!(s.offset, Count::Param(2));
        assert_eq!(s.facets[0].top, Some(Count::Param(3)));
        let s = sel("get t near e $1 limit $2");
        assert_eq!(s.limit, Some(Count::Param(1)));
        let s = sel("get t lookup c on k limit $1 offset $2 lookup d on j limit $3");
        let l = s.lookup.unwrap();
        assert_eq!(
            (l.limit, l.offset),
            (Some(Count::Param(0)), Count::Param(1))
        );
        assert_eq!(l.next.unwrap().limit, Some(Count::Param(2)));
        let Statement::Update { pick, .. } = parse_one("set t {a: 1} order a limit $1").unwrap()
        else {
            panic!()
        };
        assert_eq!(pick.unwrap().limit, Some(Count::Param(0)));
        let Statement::Delete { pick, .. } = parse_one("del t limit $2").unwrap() else {
            panic!()
        };
        assert_eq!(pick.unwrap().limit, Some(Count::Param(1)));
        // A float with no fraction is the number it is, as a parameter's.
        assert_eq!(sel("get t limit 20.0").limit, Some(Count::N(20)));
        // `offset $1` is an offset whatever it is handed, so `count` refuses
        // it, as it refuses `offset 5`; an aggregate without `group` picks
        // the rows it folds with it, as with `limit 5`.
        assert!(parse_one("get t offset $1 count").is_err());
        let s = sel("get t select count(*) limit $1 offset $2");
        assert_eq!(
            (s.limit, s.offset),
            (Some(Count::Param(0)), Count::Param(1))
        );
        let refused = |q: &str| parse_one(q).unwrap_err().to_string();
        for (q, says) in [
            (
                "get t limit -1",
                "`limit` takes a whole number from 0, not -1",
            ),
            (
                "get t offset 2.5",
                "`offset` takes a whole number from 0, not 2.5",
            ),
            (
                "get t limit \"ten\"",
                "`limit` takes a whole number from 0, not \"ten\"",
            ),
            (
                "get t limit x",
                "`limit` takes a whole number from 0, or a parameter",
            ),
            (
                "get t facet b top -2",
                "`top` takes a whole number from 0, not -2",
            ),
            (
                "get t lookup c on k offset -1",
                "`offset` takes a whole number from 0, not -1",
            ),
            (
                "del t order a limit 1.5",
                "`limit` takes a whole number from 0, not 1.5",
            ),
        ] {
            let e = refused(q);
            assert!(e.contains(says), "{q}: {e}");
        }
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

        // Arithmetic is depth too: a chain of `+`, a run of `-`, and
        // parentheses around sums, which take two levels each.
        let sum = vec!["1"; MAX_EXPR_DEPTH * 2].join(" + ");
        let e = parse_one(&format!("get t where n = {sum}"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("too deep"), "{e}");
        let minus = "- ".repeat(MAX_EXPR_DEPTH * 2);
        let e = parse_one(&format!("get t where n = {minus}1"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("too deep"), "{e}");
        let half = MAX_EXPR_DEPTH / 2 - 2;
        let nested = format!("set t {{n: {}n{}}}", "(1 + ".repeat(half), ")".repeat(half));
        let st = parse_one(&nested).unwrap();
        let mut db = fenec_core::engine::Database::new();
        db.execute(&parse_one("create collection t (n int)").unwrap())
            .unwrap();
        db.execute(&parse_one("put t {n: 0}").unwrap()).unwrap();
        db.execute(&st).unwrap();
        let Ok(Response::Rows(rs)) = db.query(&parse_one("get t select n").unwrap(), &[]) else {
            panic!("rows")
        };
        assert_eq!(rs.rows[0].values[0], Value::Int(half as i64));

        // An expression below the limit parses normally.
        let ok = format!(
            "get t where {}n = 1{}",
            "(".repeat(MAX_EXPR_DEPTH - 1),
            ")".repeat(MAX_EXPR_DEPTH - 1)
        );
        assert!(parse_one(&ok).is_ok());

        // In a named statement's body and its `check`, the same bound: a
        // definition's frames above the body's statement are a few: 511
        // parentheses in a body took 1 011 KiB in release, as at the top,
        // and 7 282 KiB in a debug build against 7 266.
        #[cfg(feature = "statements")]
        for depth in [MAX_EXPR_DEPTH - 1, MAX_EXPR_DEPTH] {
            let (open, close) = ("(".repeat(depth), ")".repeat(depth));
            for text in [
                format!("create statement s() as get t select a where {open}n = 1{close}"),
                format!(
                    "create statement s() as begin get t select a where n = 1; get t select a \
                     where {open}n = 1{close} end"
                ),
                format!("create statement s($n int) check {open}$n = 1{close} as get t select a"),
            ] {
                match parse_one(&text) {
                    Ok(_) => assert!(depth < MAX_EXPR_DEPTH),
                    Err(e) => {
                        let deep = e.to_string().contains("too deep");
                        assert!(depth == MAX_EXPR_DEPTH && deep, "{e}");
                    }
                }
            }
        }
    }
}
