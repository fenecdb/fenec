//! `@text`, `match` and `rerank`: the lexical index and the no-graph
//! retrieval path built on top of it.
//!
//! The two stages are tested apart and together, because they fail
//! differently. `match` is wrong when the ranking is wrong; `rerank` is wrong
//! when it reorders by something other than the stored vectors -- and since
//! it deliberately runs without an HNSW graph, nothing else would catch that.

use fenec_core::prelude::*;
use fenec_ql::parse;

fn run(db: &mut Database, sql: &str) -> Response {
    let mut last = Response::Affected(0);
    for s in parse(sql).expect("parse") {
        last = db.execute_with(&s, &[]).expect("execute");
    }
    last
}

fn try_run(db: &mut Database, sql: &str) -> fenec_core::error::Result<Response> {
    let mut last = Response::Affected(0);
    for s in parse(sql)? {
        last = db.execute_with(&s, &[])?;
    }
    Ok(last)
}

fn titles(r: &Response) -> Vec<String> {
    r.rows()
        .expect("rows")
        .rows
        .iter()
        .map(|row| match &row.values[0] {
            Value::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect()
}

fn ids(r: &Response) -> Vec<DocId> {
    r.rows().expect("rows").rows.iter().map(|x| x.id).collect()
}

fn reload(db: &Database) -> Database {
    let mut fresh = Database::new();
    fresh.load(&db.snapshot()).expect("load");
    fresh
}

/// `embed` deliberately carries no `@hnsw`: `rerank` reads the store, and a
/// graph would hide it if it ever stopped doing so.
fn setup() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection docs (title text @text, topic text @hash, embed vector<3>)",
    );
    run(
        &mut db,
        r#"put docs [
             {title: "rust rust rust systems", topic: "lang", embed: [1.0, 0.0, 0.0]},
             {title: "rust programming",       topic: "lang", embed: [0.0, 1.0, 0.0]},
             {title: "rust",                   topic: "misc", embed: [0.0, 0.0, 1.0]},
             {title: "python tutorial",        topic: "lang", embed: [0.0, 0.0, 1.0]}
           ]"#,
    );
    db
}

#[test]
fn match_ranks_by_relevance() {
    let mut db = setup();
    let r = run(&mut db, r#"get docs select title match title "rust""#);
    let got = titles(&r);
    // Three documents contain "rust"; the python one must not be among them.
    assert_eq!(got.len(), 3);
    assert!(!got.contains(&"python tutorial".to_string()));
    // Saying it three times in four words beats saying it once in two only if
    // the length normalisation is actually applied, which is the point of b.
    assert_eq!(got[0], "rust rust rust systems");
    let scores: Vec<f32> = r
        .rows()
        .unwrap()
        .rows
        .iter()
        .map(|x| x.score.expect("match sets a score"))
        .collect();
    assert!(scores.windows(2).all(|w| w[0] >= w[1]), "{scores:?}");
}

#[test]
fn match_is_case_insensitive_and_unicode() {
    let mut db = Database::new();
    run(&mut db, "create collection d (t text @text)");
    run(
        &mut db,
        r#"put d [{t: "ÇALIŞMA raporu"}, {t: "yillik rapor"}]"#,
    );
    // The folding path, not the ASCII one: the stored term is upper case.
    assert_eq!(ids(&run(&mut db, r#"get d match t "çalışma""#)), [1]);
    assert_eq!(ids(&run(&mut db, r#"get d match t "RAPORU""#)), [1]);
}

/// The end-to-end half of `text::tests::the_turkish_i_folds_every_way_it_is_written`:
/// a document capitalised the way Turkish prose capitalises has to be
/// reachable from the query a person types in lowercase.
#[test]
fn turkish_capitalisation_does_not_hide_a_document() {
    let mut db = Database::new();
    run(&mut db, "create collection d (t text @text)");
    run(
        &mut db,
        r#"put d [{t: "İstanbul Ulaşım A.Ş."}, {t: "IŞIK yayınları"}, {t: "ankara"}]"#,
    );
    for (query, want) in [
        ("istanbul", vec![1u64]),
        ("İSTANBUL", vec![1]),
        ("ulaşım", vec![1]),
        ("ışık", vec![2]),
        ("IŞIK", vec![2]),
        ("işik", vec![2]),
    ] {
        let sql = format!(r#"get d match t "{query}""#);
        assert_eq!(ids(&run(&mut db, &sql)), want, "query {query:?}");
    }
}

#[test]
fn match_composes_with_a_filter() {
    let mut db = setup();
    let r = run(
        &mut db,
        r#"get docs select title where topic = "misc" match title "rust""#,
    );
    assert_eq!(titles(&r), ["rust"]);
}

#[test]
fn match_takes_a_parameter() {
    let mut db = setup();
    let stmt = fenec_ql::parse_one("get docs select title match title $1").expect("parse");
    assert_eq!(stmt.max_param(), 1);
    let r = db
        .execute_with(&stmt, &[Value::Text("python".into())])
        .expect("execute");
    assert_eq!(titles(&r), ["python tutorial"]);
}

#[test]
fn rerank_orders_candidates_by_the_stored_vectors() {
    let mut db = setup();
    // `match` alone puts the triple-"rust" document first.
    let lexical = run(&mut db, r#"get docs select title match title "rust""#);
    assert_eq!(titles(&lexical)[0], "rust rust rust systems");

    // Reranking the same candidates against [0,1,0] has to move the second
    // document to the front -- it is the only one that vector points at.
    let reranked = run(
        &mut db,
        r#"get docs select title match title "rust" rerank embed [0.0, 1.0, 0.0]"#,
    );
    assert_eq!(titles(&reranked)[0], "rust programming");
    // The candidate set is still the lexical one: python never enters.
    assert_eq!(reranked.rows().unwrap().rows.len(), 3);
    // Cosine similarity, so the exact hit scores 1.
    let top = reranked.rows().unwrap().rows[0].score.unwrap();
    assert!((top - 1.0).abs() < 1e-6, "score {top}");
}

#[test]
fn rerank_needs_no_vector_index() {
    let mut db = setup();
    // `near` cannot run on this field at all ...
    let e = try_run(
        &mut db,
        "get docs select title near embed [0.0, 1.0, 0.0] limit 1",
    )
    .unwrap_err();
    assert!(
        format!("{e}").contains("no vector index"),
        "unexpected error: {e}"
    );
    // ... while rerank reads the same vectors straight out of the store.
    let r = run(
        &mut db,
        r#"get docs select title match title "rust" rerank embed [0.0, 1.0, 0.0] limit 1"#,
    );
    assert_eq!(titles(&r), ["rust programming"]);
}

#[test]
fn candidates_bound_what_rerank_can_see() {
    let mut db = setup();
    // One candidate: whatever `match` ranked first, reordering cannot rescue
    // the better vector match behind it.
    let r = run(
        &mut db,
        r#"get docs select title match title "rust" rerank embed [0.0, 1.0, 0.0] candidates 1 limit 1"#,
    );
    assert_eq!(titles(&r), ["rust rust rust systems"]);
    // Widen the pool and the right document comes back.
    let r = run(
        &mut db,
        r#"get docs select title match title "rust" rerank embed [0.0, 1.0, 0.0] candidates 10 limit 1"#,
    );
    assert_eq!(titles(&r), ["rust programming"]);
}

#[test]
fn candidates_never_fall_below_the_requested_rows() {
    let mut db = setup();
    // `candidates 1` with `limit 3` would otherwise score one document and
    // return one row, quietly answering a different question.
    let r = run(
        &mut db,
        r#"get docs select title match title "rust" rerank embed [1.0, 0.0, 0.0] candidates 1 limit 3"#,
    );
    assert_eq!(r.rows().unwrap().rows.len(), 3);
}

#[test]
fn writes_keep_the_index_consistent() {
    let mut db = setup();
    run(&mut db, r#"set docs {title: "erlang otp"} where id = 1"#);
    assert!(titles(&run(
        &mut db,
        r#"get docs select title match title "systems""#
    ))
    .is_empty());
    assert_eq!(
        titles(&run(
            &mut db,
            r#"get docs select title match title "erlang""#
        )),
        ["erlang otp"]
    );
    run(&mut db, "del docs where id = 2");
    let left = titles(&run(&mut db, r#"get docs select title match title "rust""#));
    assert_eq!(left, ["rust"]);
}

#[test]
fn the_index_survives_a_reopen() {
    let mut db = setup();
    let before = titles(&run(&mut db, r#"get docs select title match title "rust""#));
    // Nothing about the inverted index is written to the image; this is the
    // test that the rebuild on open actually happens.
    let mut fresh = reload(&db);
    let after = titles(&run(
        &mut fresh,
        r#"get docs select title match title "rust""#,
    ));
    assert_eq!(before, after);
    assert!(!after.is_empty());
}

#[test]
fn compaction_keeps_the_index_correct() {
    let mut db = setup();
    run(&mut db, "del docs where id = 1");
    run(&mut db, "compact docs");
    let got = titles(&run(&mut db, r#"get docs select title match title "rust""#));
    assert_eq!(got, ["rust", "rust programming"]);
}

#[test]
fn create_index_builds_from_the_documents_already_there() {
    let mut db = Database::new();
    run(&mut db, "create collection d (t text)");
    run(
        &mut db,
        r#"put d [{t: "alpha beta"}, {t: "beta gamma"}, {t: "delta"}]"#,
    );
    // Before the index there is nothing to match against.
    let e = try_run(&mut db, r#"get d match t "beta""#).unwrap_err();
    assert!(format!("{e}").contains("no full-text index"), "{e}");

    run(&mut db, "create index on d (t) @text");
    assert_eq!(ids(&run(&mut db, r#"get d match t "beta""#)), [1, 2]);
    assert_eq!(ids(&run(&mut db, r#"get d match t "delta""#)), [3]);
}

/// `@text(prefix=N)` end to end: the option has to survive the schema, reach
/// the tokenizer, and find an inflected form from its stem.
#[test]
fn the_prefix_option_finds_a_stem_inside_an_inflected_word() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection d (t text @text(prefix=6), plain text @text)",
    );
    run(
        &mut db,
        r#"put d [
             {t: "kitapların fiyatları", plain: "kitapların fiyatları"},
             {t: "araba kiralama",       plain: "araba kiralama"}
           ]"#,
    );
    assert_eq!(ids(&run(&mut db, r#"get d match t "kitap""#)), [1]);
    assert_eq!(ids(&run(&mut db, r#"get d match t "kiralamak""#)), [2]);
    // The field without the option cannot do it -- same data, same query.
    assert!(ids(&run(&mut db, r#"get d match plain "kitap""#)).is_empty());

    // And it survives a reopen, which rebuilds the index from the documents.
    let mut fresh = reload(&db);
    assert_eq!(ids(&run(&mut fresh, r#"get d match t "kitap""#)), [1]);
}

/// A word of a script written without spaces is found inside the run it is
/// written in -- Chinese, Japanese, Korean with its particle, Thai -- where
/// the run used to be one term only the whole of it matched.
#[test]
fn a_word_is_found_inside_an_unspaced_run() {
    let mut db = Database::new();
    run(&mut db, "create collection d (t text @text)");
    for (id, t) in [
        (1, "北京天安门广场人很多"),
        (2, "上海外滩的夜景"),
        (3, "東京都に住んでいます"),
        (4, "학교에서 공부합니다"),
        (5, "กรุงเทพมหานครเป็นเมืองหลวง"),
    ] {
        run(&mut db, &format!(r#"put d {{id: {id}, t: "{t}"}}"#));
    }
    for (q, want) in [
        ("天安门", 1),
        ("外滩", 2),
        ("東京", 3),
        ("학교", 4),
        ("เมืองหลวง", 5),
    ] {
        let got = ids(&run(&mut db, &format!(r#"get d match t "{q}""#)));
        assert_eq!(got.first(), Some(&want), "{q}: {got:?}");
    }
    // One character finds a run of more only with `chars`.
    assert!(ids(&run(&mut db, r#"get d match t "滩""#)).is_empty());
    run(&mut db, "create collection c (t text @text(chars))");
    run(&mut db, r#"put c {id: 2, t: "上海外滩的夜景"}"#);
    assert_eq!(ids(&run(&mut db, r#"get c match t "滩""#)), [2]);
}

/// `chars` is written with the schema as an index kind of its own, and
/// every listing of the schema spells the options out.
#[test]
fn the_chars_option_is_carried_through_the_schema() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection d (t text @text(prefix=6, chars), u text @text)",
    );
    let Response::Schemas(s) = run(&mut db, "describe d") else {
        panic!("expected a schema");
    };
    let IndexKind::Text(spec) = &s[0].fields[0].index else {
        panic!("expected a text index, got {:?}", s[0].fields[0].index);
    };
    assert!(spec.chars);
    assert_eq!(spec.args(), "k1=0.9, b=0.4, prefix=6, chars");
    let IndexKind::Text(plain) = &s[0].fields[1].index else {
        panic!("expected a text index");
    };
    assert!(!plain.chars);
    let fresh = reload(&db);
    let c = fresh.collection("d").expect("collection");
    assert_eq!(c.schema.fields[0].index, IndexKind::Text(*spec));
    assert_eq!(c.schema.fields[1].index, IndexKind::Text(*plain));
    // Written as an index kind of its own, 7 where the plain index is 3, so
    // an older binary refuses the file rather than index pairs alone.
    let encoded = |spec: TextIndexSpec| {
        Schema::new(
            "x",
            vec![Field::new("t", DataType::Text).indexed(IndexKind::Text(spec))],
        )
        .unwrap()
        .encode()
    };
    let (with, without) = (
        encoded(*spec),
        encoded(TextIndexSpec {
            chars: false,
            ..*spec
        }),
    );
    let differ: Vec<(u8, u8)> = with
        .iter()
        .zip(&without)
        .filter(|(a, b)| a != b)
        .map(|(a, b)| (*a, *b))
        .collect();
    assert_eq!((with.len(), differ), (without.len(), vec![(7, 3)]));
}

#[test]
fn bm25_parameters_are_carried_through_the_schema() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection d (t text @text(k1=1.2, b=0.75))",
    );
    let Response::Schemas(s) = run(&mut db, "describe d") else {
        panic!("expected a schema");
    };
    let IndexKind::Text(spec) = &s[0].fields[0].index else {
        panic!("expected a text index, got {:?}", s[0].fields[0].index);
    };
    assert_eq!((spec.k1_pct, spec.b_pct), (120, 75));
    assert_eq!(spec.prefix_max, 0, "prefixes are off unless asked for");
    // And across the image, where the schema is encoded and decoded again.
    let fresh = reload(&db);
    let c = fresh.collection("d").expect("collection");
    assert_eq!(c.schema.fields[0].index, IndexKind::Text(*spec));
}

#[test]
fn clauses_that_contradict_each_other_are_refused() {
    let mut db = setup();
    for (sql, want) in [
        (
            r#"get docs match title "rust" near embed [1.0, 0.0, 0.0]"#,
            "cannot be combined",
        ),
        (
            r#"get docs match title "rust" order title"#,
            "cannot be combined with `order`",
        ),
        (
            r#"get docs rerank embed [1.0, 0.0, 0.0]"#,
            "`rerank` needs `match`",
        ),
        (
            r#"get docs match title "rust" count"#,
            "cannot be used together with `match`",
        ),
        (r#"get docs match topic "lang""#, "no full-text index"),
        (
            r#"get docs match title "rust" rerank title [1.0]"#,
            "needs a vector field",
        ),
        (
            r#"get docs match title "rust" rerank embed [1.0, 0.0]"#,
            "must have 3 dimensions",
        ),
    ] {
        let e = try_run(&mut db, sql).expect_err(&format!("should have failed: {sql}"));
        assert!(
            format!("{e}").contains(want),
            "for {sql}\n  wanted {want:?}\n  got    {e}"
        );
    }
}

#[test]
fn the_row_ceiling_errors_instead_of_truncating() {
    let mut db = setup();
    let e = try_run(&mut db, r#"get docs match title "rust" limit 10001"#).unwrap_err();
    assert!(format!("{e}").contains("at most 10000 rows"), "{e}");
    let e = try_run(
        &mut db,
        r#"get docs match title "rust" rerank embed [1.0, 0.0, 0.0] candidates 20000"#,
    )
    .unwrap_err();
    assert!(format!("{e}").contains("at most 10000 candidates"), "{e}");
}

#[test]
fn a_text_index_on_a_non_text_field_is_refused() {
    let mut db = Database::new();
    let e = try_run(&mut db, "create collection d (v vector<2> @text)").unwrap_err();
    assert!(format!("{e}").contains("no full-text index"), "{e}");

    run(&mut db, "create collection e (n int)");
    let e = try_run(&mut db, "create index on e (n) @text").unwrap_err();
    assert!(format!("{e}").contains("no full-text index"), "{e}");
}

#[test]
fn offset_pages_through_the_ranking() {
    let mut db = setup();
    let all = titles(&run(&mut db, r#"get docs select title match title "rust""#));
    assert_eq!(all.len(), 3);
    // The window has to be cut out of the same ranking, not out of a shorter
    // one: the top-k heap is sized from limit + offset.
    for (limit, offset) in [(1usize, 0usize), (1, 1), (1, 2), (2, 1)] {
        let sql =
            format!(r#"get docs select title match title "rust" limit {limit} offset {offset}"#);
        assert_eq!(titles(&run(&mut db, &sql)), all[offset..offset + limit]);
    }
}

#[test]
fn a_query_that_matches_nothing_is_empty_not_an_error() {
    let mut db = setup();
    for sql in [
        r#"get docs select title match title "kotlin""#,
        r#"get docs select title match title """#,
        r#"get docs select title match title "!!! ???""#,
    ] {
        assert!(titles(&run(&mut db, sql)).is_empty(), "{sql}");
    }
    // And the same through rerank, which must not read vectors for nothing.
    assert!(titles(&run(
        &mut db,
        r#"get docs select title match title "kotlin" rerank embed [1.0, 0.0, 0.0]"#
    ))
    .is_empty());
}

#[test]
fn stats_report_the_index() {
    let db = setup();
    let c = db.collection("docs").expect("collection");
    let st = c.stats();
    assert_eq!(st.text_indexes.len(), 1);
    let t = &st.text_indexes[0];
    assert_eq!(t.field, "title");
    assert_eq!(t.count, 4);
    assert!(t.terms > 0 && t.postings >= t.terms && t.bytes > 0);
}
