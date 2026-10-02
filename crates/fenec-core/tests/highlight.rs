//! `highlight()` and `snippet()`: the spans of a text the terms of a
//! `match` were read from, as UTF-16 offsets or the text with tags around
//! them -- held to offsets worked out here from the text itself, in every
//! script the tokenizer reads, and to the tokenizer over generated texts.

use fenec_core::prelude::*;

fn exec(db: &mut Database, sql: &str, params: &[Value]) {
    db.execute_with(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn answer(db: &Database, sql: &str, params: &[Value]) -> ResultSet {
    let r = db
        .query(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows().unwrap().clone()
}

fn error(db: &Database, sql: &str) -> String {
    match fenec_ql::parse_one(sql) {
        Err(e) => e.to_string(),
        Ok(stmt) => db.query(&stmt, &[]).expect_err(sql).to_string(),
    }
}

/// A database of one collection, `body` under `@text(<spec>)`.
fn one(spec: &str, bodies: &[&str]) -> Database {
    let mut db = Database::new();
    exec(
        &mut db,
        &format!("create collection d (body text @text{spec}, n int)"),
        &[],
    );
    for (i, b) in bodies.iter().enumerate() {
        exec(
            &mut db,
            "put d {body: $1, n: $2}",
            &[Value::Text(b.to_string()), Value::Int(i as i64)],
        );
    }
    db
}

/// The marks of `highlight(body)` for the row whose `n` is `n`.
fn marks(db: &Database, query: &str, n: i64) -> Vec<(usize, usize)> {
    let rs = answer(
        db,
        "get d select n, highlight(body) match body $1 limit 100",
        &[Value::Text(query.into())],
    );
    let row = rs
        .rows
        .iter()
        .find(|r| r.values[0] == Value::Int(n))
        .unwrap_or_else(|| panic!("row {n} not found for {query:?}"));
    pairs(&row.values[1])
}

fn pairs(v: &Value) -> Vec<(usize, usize)> {
    let Value::List(items) = v else {
        panic!("not a list: {v:?}")
    };
    items
        .iter()
        .map(|p| match p {
            Value::List(p) => match (&p[0], &p[1]) {
                (Value::Int(a), Value::Int(b)) => (*a as usize, *b as usize),
                _ => panic!("{p:?}"),
            },
            _ => panic!("{p:?}"),
        })
        .collect()
}

/// Where `part` stands in `text` -- its `nth` occurrence -- in UTF-16 code
/// units, as JavaScript would find it: worked out here from the text, not
/// by the engine's own conversion.
fn at(text: &str, part: &str, nth: usize) -> (usize, usize) {
    let b = text
        .match_indices(part)
        .nth(nth)
        .unwrap_or_else(|| panic!("{part:?} #{nth} not in {text:?}"))
        .0;
    let s = text[..b].encode_utf16().count();
    (s, s + part.encode_utf16().count())
}

#[test]
fn ascii_marks_every_occurrence_whatever_its_case() {
    let t = "Rust and WASM, rust! RUSTy is not rust.";
    let db = one("", &[t]);
    assert_eq!(
        marks(&db, "rust", 0),
        [at(t, "Rust", 0), at(t, "rust", 0), at(t, "rust", 1)]
    );
    // Two terms, each marked where it stands, in order.
    assert_eq!(
        marks(&db, "wasm RUST", 0),
        [
            at(t, "Rust", 0),
            at(t, "WASM", 0),
            at(t, "rust", 0),
            at(t, "rust", 1)
        ]
    );
    let rs = answer(
        &db,
        "get d select highlight(body, \"<b>\", \"</b>\") match body \"rust\"",
        &[],
    );
    assert_eq!(
        rs.rows[0].values[0],
        Value::Text("<b>Rust</b> and WASM, <b>rust</b>! RUSTy is not <b>rust</b>.".into())
    );
    assert_eq!(rs.columns, ["highlight(body)"]);
}

#[test]
fn turkish_folds_the_dotted_and_dotless_i_as_the_index_does() {
    let t = "İstanbul'da ISTANBUL, istanbul ve ılık IŞIK ışık";
    let db = one("", &[t]);
    assert_eq!(
        marks(&db, "istanbul", 0),
        [
            at(t, "İstanbul", 0),
            at(t, "ISTANBUL", 0),
            at(t, "istanbul", 0)
        ]
    );
    // `ışık` and `IŞIK` are one term to the index, and so marked both.
    assert_eq!(marks(&db, "ışık", 0), [at(t, "IŞIK", 0), at(t, "ışık", 0)]);
    let rs = answer(
        &db,
        "get d select highlight(body, $1, $2) match body \"İSTANBUL\"",
        &[Value::Text("[".into()), Value::Text("]".into())],
    );
    assert_eq!(
        rs.rows[0].values[0],
        Value::Text("[İstanbul]'da [ISTANBUL], [istanbul] ve ılık IŞIK ışık".into())
    );
}

#[test]
fn han_kana_and_hangul_mark_the_characters_their_pairs_matched() {
    let t = "東京都に住む。서울에서 왔다";
    let db = one("", &[t]);
    // `京都` is one pair of the run.
    assert_eq!(marks(&db, "京都", 0), [at(t, "京都", 0)]);
    // `東京都`'s pairs overlap, and so are one mark.
    assert_eq!(marks(&db, "東京都", 0), [at(t, "東京都", 0)]);
    assert_eq!(marks(&db, "서울", 0), [at(t, "서울", 0)]);
    // A character alone finds a run only under `chars`.
    let db = one("(chars)", &[t]);
    assert_eq!(marks(&db, "都", 0), [at(t, "都", 0)]);
}

#[test]
fn emoji_and_astral_characters_count_as_javascript_counts_them() {
    // 🦀 is two UTF-16 units, ❤️ a heart and a variation selector, 𠮷 a Han
    // character past the first plane.
    let t = "I ❤️ Rust 🦀 and rust 𠮷野家 🦀 rust";
    let db = one("", &[t]);
    assert_eq!(
        marks(&db, "rust", 0),
        [at(t, "Rust", 0), at(t, "rust", 0), at(t, "rust", 1)]
    );
    assert_eq!(marks(&db, "𠮷野", 0), [at(t, "𠮷野", 0)]);
    // A keycap's digit takes its variation selector and its enclosing
    // mark: no mark ends between a character and what belongs to it.
    let t = "press 1\u{FE0F}\u{20E3} now";
    let db = one("", &[t]);
    assert_eq!(marks(&db, "1", 0), [at(t, "1\u{FE0F}\u{20E3}", 0)]);
}

#[test]
fn a_combining_mark_stays_with_its_letter() {
    // Decomposed, `é` is an `e` and a combining acute, which the tokenizer
    // cuts at: the `e` is its own term, and its mark keeps its accent.
    let t = "e\u{301}cole and ecole";
    let db = one("", &[t]);
    assert_eq!(
        marks(&db, "e", 0),
        [at(t, "e\u{301}", 0)],
        "the accent belongs to the mark"
    );
    // A Thai run's triples: the mark covers the matched characters and the
    // tone mark after them.
    let t = "ไม่ใช่";
    let db = one("", &[t]);
    let m = marks(&db, "ไม่", 0);
    assert_eq!(m, [at(t, "ไม่", 0)]);
}

#[test]
fn a_prefix_marks_the_whole_word_it_was_cut_from() {
    let t = "kitapların kitabı kalem kitap";
    let db = one("(prefix=5)", &[t]);
    // `kitap` and its prefixes: every word sharing three letters or more
    // with it is a match the index made, and is marked whole.
    assert_eq!(
        marks(&db, "kitaplar", 0),
        [
            at(t, "kitapların", 0),
            at(t, "kitabı", 0),
            at(t, "kitap", 1)
        ]
    );
    // Without prefixes only the word itself.
    let db = one("", &[t]);
    assert_eq!(marks(&db, "kitap", 0), [at(t, "kitap", 1)]);
}

#[test]
fn a_snippet_is_the_window_around_the_densest_marks() {
    let words: Vec<String> = (0..100).map(|i| format!("w{i}")).collect();
    let mut t = words.join(" ");
    t = t.replace("w50 ", "needle ").replace("w52 ", "needle ");
    let db = one("", &[&t]);
    let rs = answer(
        &db,
        "get d select snippet(body, 9, \"…\") match body \"needle\"",
        &[],
    );
    let Value::Object(o) = &rs.rows[0].values[0] else {
        panic!("{:?}", rs.rows[0].values[0])
    };
    let text = match &o[1] {
        (k, Value::Text(s)) if k == "text" => s.clone(),
        other => panic!("{other:?}"),
    };
    // Nine words, centred on the two marks, an ellipsis either side.
    assert_eq!(text, "…w47 w48 w49 needle w51 needle w53 w54 w55…");
    assert_eq!(
        pairs(&o[0].1),
        [at(&text, "needle", 0), at(&text, "needle", 1)]
    );
    // With tags, the marked text.
    let rs = answer(
        &db,
        "get d select snippet(body, 3, \"...\", \"<b>\", \"</b>\") match body \"needle\"",
        &[],
    );
    assert_eq!(
        rs.rows[0].values[0],
        Value::Text("...<b>needle</b> w51 <b>needle</b>...".into())
    );
    // At the start of the text: no ellipsis before it.
    let t2 = "needle in a haystack of many more words than fit";
    let db = one("", &[t2]);
    let rs = answer(
        &db,
        "get d select snippet(body, 4, \"…\", \"[\", \"]\") match body \"needle\"",
        &[],
    );
    assert_eq!(
        rs.rows[0].values[0],
        Value::Text("[needle] in a haystack…".into())
    );
    // Shorter than the window: the whole text, no ellipsis at all.
    let rs = answer(
        &db,
        "get d select snippet(body, 40, \"…\", \"[\", \"]\") match body \"haystack\"",
        &[],
    );
    assert_eq!(
        rs.rows[0].values[0],
        Value::Text("needle in a [haystack] of many more words than fit".into())
    );
    // A run of Han counts a character a word.
    let t3 = "前文前文前文前文東京都に住む後文後文後文後文";
    let db = one("", &[t3]);
    let rs = answer(
        &db,
        "get d select snippet(body, 5, \"…\", \"[\", \"]\") match body \"東京\"",
        &[],
    );
    assert_eq!(rs.rows[0].values[0], Value::Text("…前文[東京]都…".into()));
}

#[test]
fn marks_follow_match_through_fuse_and_rerank() {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection d (body text @text, n int, v vector<2> @hnsw(cosine))",
        &[],
    );
    let bodies = [
        "rust engine",
        "wasm engine",
        "nothing here",
        "rust and wasm",
    ];
    for (i, b) in bodies.iter().enumerate() {
        exec(
            &mut db,
            "put d {body: $1, n: $2, v: $3}",
            &[
                Value::Text(b.to_string()),
                Value::Int(i as i64),
                Value::Vector(vec![1.0, i as f32]),
            ],
        );
    }
    // `fuse` takes rows `near` alone found: their text holds no term, and
    // has no mark.
    let rs = answer(
        &db,
        "get d select n, highlight(body) match body \"rust\" near v [1, 2] fuse limit 4",
        &[],
    );
    assert_eq!(rs.rows.len(), 4);
    for r in &rs.rows {
        let Value::Int(n) = r.values[0] else { panic!() };
        let t = bodies[n as usize];
        let want: Vec<(usize, usize)> = t.match_indices("rust").map(|(b, _)| (b, b + 4)).collect();
        assert_eq!(pairs(&r.values[1]), want, "{t}");
    }
    let rs = answer(
        &db,
        "get d select n, highlight(body, \"*\", \"*\") match body \"wasm\" rerank v [1, 0]",
        &[],
    );
    let got: Vec<&Value> = rs.rows.iter().map(|r| &r.values[1]).collect();
    assert_eq!(
        got,
        [
            &Value::Text("*wasm* engine".into()),
            &Value::Text("rust and *wasm*".into())
        ]
    );
}

#[test]
fn star_puts_the_marks_after_every_field_and_a_null_text_marks_nothing() {
    let mut db = one("", &["rust"]);
    exec(&mut db, "put d {n: 9}", &[]);
    let rs = answer(
        &db,
        "get d select *, highlight(body), snippet(body, 2) match body \"rust\"",
        &[],
    );
    assert_eq!(
        rs.columns,
        ["id", "body", "n", "highlight(body)", "snippet(body)"]
    );
    // The SQL order reads the same list.
    let rs2 = answer(
        &db,
        "select n, highlight(body) from d match body \"rust\"",
        &[],
    );
    assert_eq!(rs2.columns, ["n", "highlight(body)"]);
    assert_eq!(pairs(&rs2.rows[0].values[1]), [(0, 4)]);
}

#[test]
fn what_has_no_terms_to_mark_is_refused() {
    let mut db = one("", &["rust"]);
    exec(
        &mut db,
        "create collection v (body text, e vector<2> @hnsw(cosine))",
        &[],
    );
    exec(&mut db, "put v {body: \"rust\", e: [1, 0]}", &[]);
    for (sql, want) in [
        ("get d select highlight(body)", "needs `match`"),
        (
            "get v select highlight(body) near e [1, 0]",
            "needs `match`",
        ),
        (
            "get d select snippet(body, 0) match body \"x\"",
            "at least one word",
        ),
        ("get d select snippet(body) match body \"x\"", "expected"),
        ("get d select highlight(n) match body \"x\"", "marks text"),
        (
            "get d select highlight(body, \"<b>\") match body \"x\"",
            "marks with",
        ),
        (
            "get d select highlight(body), count(*) match body \"x\"",
            "aggregates",
        ),
        (
            "get d select highlight(body, 1, 2) match body \"rust\"",
            "a tag is text",
        ),
        (
            "get d select title, *, highlight(body) match body \"x\"",
            "",
        ),
        ("get d select *, n match body \"x\"", "after `*`"),
        (
            "get d select highlight(body), highlight(body, \"[\", \"]\") match body \"x\"",
            "asked twice",
        ),
    ] {
        let e = error(&db, sql);
        assert!(e.contains(want), "{sql}: {e}");
    }
}

/// A tiny xorshift: the generated texts are the same on every run.
struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// Over texts made of every kind of piece the tokenizer reads, each mark
/// holds a term of the query when read again by the same tokenizer, starts
/// and ends on whole characters with no mark of a character left outside
/// it, and the marks are in order and apart; and the tagged text, its tags
/// taken out, is the text.
#[test]
fn marks_over_generated_texts_hold_the_terms_and_whole_characters() {
    const PIECES: [&str; 16] = [
        "rust",
        "Rust",
        "İstanbul",
        "ışık",
        "東京都",
        "京都",
        "🦀",
        "❤\u{FE0F}",
        "e\u{301}",
        "café",
        "ไม่",
        "서울",
        " ",
        ", ",
        "-",
        "1\u{FE0F}\u{20E3}",
    ];
    const QUERIES: [&str; 6] = ["rust", "istanbul ışık", "京都", "e", "서울 ไม่", "café 1"];
    let mut rng = Rng(0x9e3779b97f4a7c15);
    let mut texts = Vec::new();
    for _ in 0..300 {
        let n = 1 + rng.below(14);
        let t: String = (0..n).map(|_| PIECES[rng.below(16) as usize]).collect();
        texts.push(t);
    }
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let db = one("", &refs);
    for q in QUERIES {
        let want: Vec<String> = fenec_core::text::tokenize(q);
        let rs = answer(
            &db,
            "get d select n, highlight(body), snippet(body, 1000, \"…\", \"\u{1}\", \"\u{2}\") match body $1 limit 300",
            &[Value::Text(q.into())],
        );
        for r in &rs.rows {
            let Value::Int(n) = r.values[0] else { panic!() };
            let t = &texts[n as usize];
            let units: Vec<u16> = t.encode_utf16().collect();
            let mut end = 0;
            let ms = pairs(&r.values[1]);
            assert!(!ms.is_empty(), "{q:?} matched {t:?} and marked nothing");
            for (s, e) in ms {
                assert!(s >= end && s < e && e <= units.len(), "{t:?}: {s}..{e}");
                end = e;
                let part = String::from_utf16(&units[s..e])
                    .unwrap_or_else(|_| panic!("{t:?}: {s}..{e} splits a character"));
                let terms = fenec_core::text::tokenize(&part);
                assert!(
                    terms.iter().any(|x| want.contains(x)),
                    "{t:?}: {part:?} holds no term of {q:?}"
                );
                // Nothing that belongs to the character before is left
                // right after the mark.
                let after = String::from_utf16_lossy(&units[e..]);
                let next = after.chars().next();
                assert!(
                    !matches!(next, Some('\u{301}' | '\u{FE0F}' | '\u{20E3}' | '\u{E48}')),
                    "{t:?}: the mark {part:?} leaves {next:?} out"
                );
            }
            let Value::Text(tagged) = &r.values[2] else {
                panic!()
            };
            assert_eq!(&tagged.replace(['\u{1}', '\u{2}'], ""), t);
        }
    }
}
