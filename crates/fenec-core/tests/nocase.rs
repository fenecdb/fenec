//! `nocase`: a text field compared by its case fold.
//!
//! The fold is held to Python's `str.casefold()` at every code point
//! (`casefold.txt`, written by `tools/casefold.py`), and every filter,
//! order, group, facet and unique index over a `nocase` field to a twin
//! collection holding each value's fold -- worked out by that table, not by
//! the fold under test -- in a plain text field, row for row.

use fenec_core::case::{cmp_folded, cmp_to_folded, eq_folded, fold, CaseFold};

/// Python's fold of every code point it changes.
fn python() -> std::collections::HashMap<u32, String> {
    let mut out = std::collections::HashMap::new();
    for line in include_str!("casefold.txt").lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let mut cps = line
            .split(' ')
            .map(|h| u32::from_str_radix(h, 16).expect("hex"));
        let cp = cps.next().expect("a code point");
        let folded: String = cps.map(|c| char::from_u32(c).expect("a char")).collect();
        out.insert(cp, folded);
    }
    out
}

/// What Python's table folds `s` to, a code point at a time -- Unicode's
/// full folding, as `str.casefold()` does -- and with Turkish's two
/// mappings first.
fn reference(table: &std::collections::HashMap<u32, String>, s: &str, f: CaseFold) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match (c, f) {
            ('I', CaseFold::Turkish) => out.push('ı'),
            ('İ', CaseFold::Turkish) => out.push('i'),
            _ => match table.get(&(c as u32)) {
                Some(t) => out.push_str(t),
                None => out.push(c),
            },
        }
    }
    out
}

/// The capitals the toolchain's Unicode (17.0) assigned after the table's
/// (16.0), their small letters with them, which the table leaves alone as
/// unassigned: three Latin letters and Beria Erfe's. A toolchain on a
/// Unicode past 17 fails here, naming the code point to look up.
const NEWER: [(u32, u32); 4] = [
    (0xA7CE, 0xA7CE),
    (0xA7D2, 0xA7D2),
    (0xA7D4, 0xA7D4),
    (0x16EA0, 0x16EB8),
];

#[test]
fn the_fold_is_python_s_casefold_at_every_code_point() {
    let table = python();
    let mut s = String::new();
    for c in (0..0x11_0000).filter_map(char::from_u32) {
        if NEWER.iter().any(|r| (r.0..=r.1).contains(&(c as u32))) {
            continue;
        }
        s.clear();
        s.push(c);
        let want = reference(&table, &s, CaseFold::Unicode);
        let got = fold(&s, CaseFold::Unicode);
        assert_eq!(got, want, "{:#x}", c as u32);
        // A fold is its own fold, and compares as its bytes do.
        assert_eq!(fold(&got, CaseFold::Unicode), got, "{:#x}", c as u32);
        let t = fold(&s, CaseFold::Turkish);
        assert_eq!(
            t,
            reference(&table, &s, CaseFold::Turkish),
            "{:#x}",
            c as u32
        );
        assert_eq!(fold(&t, CaseFold::Turkish), t, "{:#x}", c as u32);
    }
}

#[test]
fn folds_compare_as_their_texts_would() {
    let words = [
        "",
        "a",
        "A",
        "ab",
        "aB",
        "Ab",
        "B",
        "b",
        "straße",
        "STRASSE",
        "strasse",
        "STRAẞE",
        "Straße",
        "ὈΔΥΣΣΕΎΣ",
        "ὀδυσσεύς",
        "ὀδυσσεύσ",
        "ΟΔΟΣ",
        "οδος",
        "İstanbul",
        "istanbul",
        "ISTANBUL",
        "ıstanbul",
        "I\u{307}stanbul",
        "Ꭰ",
        "ꭰ",
        "Ａ",
        "ａ",
        "A",
        "ﬁx",
        "FIX",
        "ǅ",
        "ǆ",
        "Ǆ",
        "z",
        "ζ",
        "Z",
        "kelvin",
        "\u{212A}elvin",
        "zz",
        "\u{0}",
        "a\u{0}",
        "Ilık",
        "ılık",
        "abİ",
        "abI",
        "abı",
        "aSS",
        "aß",
        "a\u{7f}",
        "a\u{80}",
    ];
    for f in [CaseFold::Unicode, CaseFold::Turkish] {
        for a in words {
            for b in words {
                let (x, y) = (fold(a, f), fold(b, f));
                assert_eq!(
                    cmp_folded(a, b, f),
                    x.as_bytes().cmp(y.as_bytes()),
                    "{a:?} {b:?}"
                );
                assert_eq!(
                    cmp_to_folded(a, &y, f),
                    x.as_bytes().cmp(y.as_bytes()),
                    "{a:?} {b:?}"
                );
                assert_eq!(eq_folded(a, b, f), x == y, "{a:?} {b:?}");
            }
        }
    }
}

#[test]
fn what_is_equal_to_what() {
    let eq = |a: &str, b: &str| eq_folded(a, b, CaseFold::Unicode);
    let tr = |a: &str, b: &str| eq_folded(a, b, CaseFold::Turkish);
    assert!(eq("Alice@Example.com", "alice@example.COM"));
    // German: ß, its capital and SS are one.
    assert!(eq("straße", "STRASSE") && eq("STRAẞE", "strasse") && eq("ß", "ẞ"));
    // Greek: a final sigma is a sigma.
    assert!(eq("ΟΔΟΣ", "οδος") && eq("οδος", "οδοσ"));
    // Cherokee, whose capitals came first.
    assert!(eq("ᏣᎳᎩ", "ꮳꮃꭹ"));
    // Width is not case: a full-width letter is its own lowercase's.
    assert!(eq("ＡＢＣ", "ａｂｃ") && !eq("ＡＢＣ", "abc"));
    // Accents are not case, nor is a text written in other code points.
    assert!(!eq("résumé", "resume") && !eq("é", "e\u{301}"));
    // `İ` is `i` and a combining dot, as its decomposed spelling is; `ı`
    // is a letter of its own.
    assert!(eq("İstanbul", "I\u{307}stanbul") && !eq("İstanbul", "istanbul"));
    assert!(eq("ISTANBUL", "istanbul") && !eq("ıstanbul", "istanbul"));
    // Turkish: I is the capital of ı, İ of i.
    assert!(tr("İstanbul", "istanbul") && tr("ISPARTA", "ısparta") && !tr("ISPARTA", "isparta"));
    assert!(tr("IŞIK", "ışık") && !tr("KIZ", "kiz"));
    // Ligatures and titlecase digraphs.
    assert!(eq("ﬁle", "FILE") && eq("ǅemal", "ǆemal") && eq("ǅ", "Ǆ"));
}

// ------------------------------------------------------------ the twin

use fenec_core::prelude::*;
use fenec_core::query::FacetValues;

fn exec(db: &mut Database, sql: &str, params: &[Value]) -> Result<Response> {
    db.execute_with(&fenec_ql::parse_one(sql).expect(sql), params)
}

fn answer(db: &Database, sql: &str, params: &[Value]) -> ResultSet {
    db.query(&fenec_ql::parse_one(sql).expect(sql), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows()
        .expect("rows")
        .clone()
}

fn ids(rs: &ResultSet) -> Vec<DocId> {
    rs.rows.iter().map(|r| r.id).collect()
}

fn text(v: &Value) -> &str {
    match v {
        Value::Text(t) => t,
        other => panic!("not text: {other:?}"),
    }
}

fn values(rs: &ResultSet) -> Vec<Vec<Value>> {
    rs.rows.iter().map(|r| r.values.clone()).collect()
}

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// What words are made of: ASCII, Turkish's four I's, German's ß and its
/// capital, Greek with its final sigma, Cherokee in both cases, full-width
/// letters, a ligature, the Kelvin sign, titlecase digraphs, an accent
/// precomposed and combining -- what a fold has to take apart and put
/// together.
const PIECES: [&str; 41] = [
    "a", "b", "z", "Al", "ice", "İ", "ı", "I", "i", "stanbul", "ş", "Ş", "ğ", "straße", "STRASSE",
    "ẞ", "ss", "Σ", "σ", "ς", "οδο", "ΟΔΟ", "Ꭰ", "ꭰ", "Ꮳ", "ꮳ", "Ａ", "ａ", "ﬁ", "FI", "K",
    "\u{212A}", "é", "e\u{301}", "ǅ", "ǆ", "Ǆ", "0", "9", " ", "@x.com",
];

/// A word spelled with each character's case thrown again: `ß` may come
/// out `SS`, `ǅ` as `ǆ` or `Ǆ`, `ς` as `Σ`.
fn respell(rng: &mut Rng, w: &str) -> String {
    let mut out = String::new();
    for c in w.chars() {
        match rng.below(3) {
            0 => out.push(c),
            1 => out.extend(c.to_uppercase()),
            _ => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// A word of `vocab`, respelled.
fn pick(rng: &mut Rng, vocab: &[String]) -> String {
    let i = rng.below(vocab.len());
    respell(rng, &vocab[i])
}

/// A vocabulary of words, each written many ways.
fn vocabulary(rng: &mut Rng, n: usize) -> Vec<String> {
    (0..n)
        .map(|_| {
            let k = 1 + rng.below(3);
            (0..k).map(|_| PIECES[rng.below(PIECES.len())]).collect()
        })
        .collect()
}

/// `nc` holds each text as written in `nocase` fields -- hashed, ordered,
/// neither, and a part of an index over two fields -- and `tw` its fold,
/// by Python's table, in plain ones indexed the same way: the same rows
/// under the same ids. The texts written, in id order, come back with it
/// (empty for a row holding none).
fn twins(rows: usize, seed: u64) -> (Database, Vec<String>, Vec<String>) {
    let mut db = Database::new();
    let table = python();
    exec(
        &mut db,
        "create collection nc (h text nocase @hash, s text nocase @sorted, p text nocase, \
         g int, c text nocase, @sorted(g, c))",
        &[],
    )
    .unwrap();
    exec(
        &mut db,
        "create collection tw (h text @hash, s text @sorted, p text, g int, c text, \
         @sorted(g, c))",
        &[],
    )
    .unwrap();
    let mut rng = Rng(seed);
    let vocab = vocabulary(&mut rng, 60);
    let mut written = Vec::new();
    for i in 0..rows {
        // A row now and then holds no text, which every index files apart.
        let w = match rng.below(40) {
            0 => None,
            _ => Some(pick(&mut rng, &vocab)),
        };
        let g = Value::Int((i % 7) as i64);
        let (a, b) = match &w {
            Some(w) => (
                Value::Text(w.clone()),
                Value::Text(reference(&table, w, CaseFold::Unicode)),
            ),
            None => (Value::Null, Value::Null),
        };
        written.push(w.unwrap_or_default());
        for (coll, v) in [("nc", &a), ("tw", &b)] {
            exec(
                &mut db,
                &format!("put {coll} {{h: $1, s: $1, p: $1, g: $2, c: $1}}"),
                &[v.clone(), g.clone()],
            )
            .unwrap();
        }
    }
    (db, vocab, written)
}

/// Each probe asked of `nc` as written and of `tw` as its fold.
fn probes(rng: &mut Rng, vocab: &[String], n: usize) -> Vec<String> {
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(pick(rng, vocab));
    }
    // And a few no row holds.
    out.extend(["nobody".to_string(), "ẞẞẞ".to_string(), String::new()]);
    out
}

/// `sql` over `nc` with its parameters as written, and over `tw` with
/// their folds: the same ids, in the same order.
fn same_ids(db: &Database, sql: &str, params: &[Value]) {
    let table = python();
    let folded: Vec<Value> = params
        .iter()
        .map(|v| match v {
            Value::Text(t) => Value::Text(reference(&table, t, CaseFold::Unicode)),
            v => v.clone(),
        })
        .collect();
    let a = answer(db, &sql.replace("COLL", "nc"), params);
    let b = answer(db, &sql.replace("COLL", "tw"), &folded);
    assert_eq!(ids(&a), ids(&b), "{sql} {params:?}");
}

#[test]
fn every_filter_and_order_over_a_nocase_field_is_its_fold_s() {
    let (db, vocab, _) = twins(1500, 0x9E37_79B9_7F4A_7C15);
    let mut rng = Rng(42);
    let probes = probes(&mut rng, &vocab, 40);
    let t = |s: &str| Value::Text(s.to_string());
    for f in ["h", "s", "p", "c"] {
        for q in &probes {
            same_ids(&db, &format!("get COLL where {f} = $1"), &[t(q)]);
            same_ids(&db, &format!("get COLL where {f} != $1"), &[t(q)]);
            same_ids(&db, &format!("get COLL where {f} >= $1"), &[t(q)]);
            same_ids(&db, &format!("get COLL where {f} < $1 and g = 3"), &[t(q)]);
            same_ids(&db, &format!("get COLL where g = 3 and {f} = $1"), &[t(q)]);
        }
        for w in probes.windows(2).step_by(5) {
            same_ids(
                &db,
                &format!("get COLL where {f} > $1 and {f} <= $2"),
                &[t(&w[0]), t(&w[1])],
            );
            same_ids(
                &db,
                &format!("get COLL where {f} in [$1, $2]"),
                &[t(&w[0]), t(&w[1])],
            );
        }
        // A long list, which a row is looked up in.
        let long: Vec<Value> = probes.iter().take(12).map(|q| t(q)).collect();
        let list: Vec<String> = (1..=long.len()).map(|i| format!("${i}")).collect();
        same_ids(
            &db,
            &format!("get COLL where {f} in [{}]", list.join(", ")),
            &long,
        );
        // `~`: the fold holds the needle's fold.
        for needle in ["SS", "ß", "İ", "ΣΣ", "a", "Ꭰ"] {
            same_ids(&db, &format!("get COLL where {f} ~ $1"), &[t(needle)]);
        }
        // The order: the folds', ties by id, both ways, paged, and through
        // the ordered index's and the composite's walks.
        same_ids(&db, &format!("get COLL order {f}"), &[]);
        same_ids(&db, &format!("get COLL order {f} desc"), &[]);
        same_ids(&db, &format!("get COLL order {f} limit 20"), &[]);
        same_ids(
            &db,
            &format!("get COLL order {f} desc limit 20 offset 7"),
            &[],
        );
        same_ids(
            &db,
            &format!("get COLL where g = 3 order {f} limit 15"),
            &[],
        );
        same_ids(
            &db,
            &format!("get COLL where g = 3 order {f} desc limit 15"),
            &[],
        );
        same_ids(&db, &format!("get COLL order {f}, g desc"), &[]);
        same_ids(
            &db,
            &format!("get COLL where g in [1, 4] order {f} limit 30"),
            &[],
        );
    }
    // An inner `get`'s column, in a list the outer field folds.
    same_ids(
        &db,
        "get COLL where h in (get COLL select p where g = 2 limit 5)",
        &[],
    );
    // The probes meet rows written more than one way...
    let spellings = probes
        .iter()
        .map(|q| {
            let hit = answer(&db, "get nc select h where h = $1", &[t(q)]);
            let mut seen: Vec<&str> = hit.rows.iter().map(|r| text(&r.values[0])).collect();
            seen.sort();
            seen.dedup();
            seen.len()
        })
        .max();
    assert!(spellings > Some(3), "{spellings:?}");
    // ...through the indexes, which file every spelling under one key.
    let plan = |sql: &str| -> String {
        let stmt = fenec_ql::parse_one(&format!("explain {sql}")).expect(sql);
        let rs = db.query(&stmt, &[t(&probes[0])]).expect(sql);
        format!("{:?}", rs.rows().expect("rows").rows)
    };
    assert!(plan("get nc where h = $1").contains("the hash index on h, "));
    assert!(plan("get nc where s >= $1").contains("the ordered index on s"));
    assert!(plan("get nc order s limit 5").contains("walked the ordered index on s"));
    assert!(plan("get nc where g = 1 and c = $1").contains("the index on (g, c)"));
}

/// `rs`'s rows with the texts at `keys` made their folds, by Python's
/// table: what `tw` answers.
fn folded_rows(rs: &ResultSet, keys: &[usize]) -> Vec<Vec<Value>> {
    let table = python();
    rs.rows
        .iter()
        .map(|r| {
            r.values
                .iter()
                .enumerate()
                .map(|(i, v)| match (keys.contains(&i), v) {
                    (true, Value::Text(t)) => Value::Text(reference(&table, t, CaseFold::Unicode)),
                    _ => v.clone(),
                })
                .collect()
        })
        .collect()
}

/// The text of the first row, in id order, that holds `fold`.
fn first_holding(written: &[String], fold: &str) -> String {
    let table = python();
    written
        .iter()
        .find(|w| !w.is_empty() && reference(&table, w, CaseFold::Unicode) == fold)
        .cloned()
        .unwrap_or_default()
}

#[test]
fn groups_counts_and_facets_are_the_folds_shown_as_first_written() {
    let (db, _, written) = twins(1200, 0xD1B5_4A32_D192_ED03);
    let table = python();
    for f in ["h", "s", "p", "c"] {
        for (sql, keys) in [
            (format!("get COLL select {f}, count() group {f}"), vec![0]),
            (
                format!("get COLL select {f}, count() as n group {f} order n desc, {f}"),
                vec![0],
            ),
            (
                format!("get COLL select g, {f}, count() where g < 3 group g, {f}"),
                vec![1],
            ),
            (
                format!("get COLL select {f}, count() group {f} having count() > 3"),
                vec![0],
            ),
            (
                format!("get COLL select {f}, min(p), max(s) group {f}"),
                vec![0, 1, 2],
            ),
        ] {
            let a = answer(&db, &sql.replace("COLL", "nc"), &[]);
            let b = answer(&db, &sql.replace("COLL", "tw"), &[]);
            assert_eq!(folded_rows(&a, &keys), values(&b), "{sql}");
            // A group is shown as its first row wrote it.
            if keys == [0] {
                for r in &a.rows {
                    if let Value::Text(k) = &r.values[0] {
                        let fold = reference(&table, k, CaseFold::Unicode);
                        assert_eq!(*k, first_holding(&written, &fold), "{sql}");
                    }
                }
            }
        }
        for sql in [
            format!("get COLL select count(distinct {f}), approx_count_distinct({f})"),
            format!("get COLL select min({f}), max({f})"),
            format!("get COLL select g, count(distinct {f}) group g"),
            format!("get COLL select count() where {f} >= 'k' group {f} count"),
        ] {
            let a = answer(&db, &sql.replace("COLL", "nc"), &[]);
            let b = answer(&db, &sql.replace("COLL", "tw"), &[]);
            assert_eq!(folded_rows(&a, &[0, 1]), values(&b), "{sql}");
        }
        // A facet, through the hash index's buckets and by the scan, over
        // every row and over a filter.
        for sql in [
            format!("get COLL facet {f}"),
            format!("get COLL where g = 2 facet {f}"),
            format!("get COLL where g = 2 facet {f} top 5"),
        ] {
            let a = answer(&db, &sql.replace("COLL", "nc"), &[]);
            let b = answer(&db, &sql.replace("COLL", "tw"), &[]);
            let shown = |fv: &FacetValues| -> Vec<(Value, u64)> {
                fv.values
                    .iter()
                    .map(|(v, n)| match v {
                        Value::Text(t) => {
                            (Value::Text(reference(&table, t, CaseFold::Unicode)), *n)
                        }
                        v => (v.clone(), *n),
                    })
                    .collect()
            };
            assert_eq!(shown(&a.facets[0]), b.facets[0].values, "{sql}");
            if !sql.contains("where") {
                for (v, _) in &a.facets[0].values {
                    if let Value::Text(t) = v {
                        let fold = reference(&table, t, CaseFold::Unicode);
                        assert_eq!(*t, first_holding(&written, &fold), "{sql}");
                    }
                }
            }
        }
    }
}

#[test]
fn a_unique_nocase_field_refuses_another_spelling() {
    let table = python();
    let mut rng = Rng(7);
    let vocab = vocabulary(&mut rng, 40);
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection users (email text nocase @unique, org text nocase, slug text \
         nocase, n int, @unique(org, slug))",
        &[],
    )
    .unwrap();
    let mut held = std::collections::HashSet::new();
    let mut pairs = std::collections::HashSet::new();
    for i in 0..600 {
        let w = pick(&mut rng, &vocab);
        let org = respell(&mut rng, ["Acme", "İzmir", "STRASSE"][i % 3]);
        let slug = pick(&mut rng, &vocab[..8]);
        let fold = reference(&table, &w, CaseFold::Unicode);
        let pair = (
            reference(&table, &org, CaseFold::Unicode),
            reference(&table, &slug, CaseFold::Unicode),
        );
        let r = exec(
            &mut db,
            "insert users {email: $1, org: $2, slug: $3, n: $4}",
            &[
                Value::Text(w.clone()),
                Value::Text(org),
                Value::Text(slug),
                Value::Int(i as i64),
            ],
        );
        let fresh = !held.contains(&fold) && !pairs.contains(&pair);
        match r {
            Ok(_) => {
                assert!(fresh, "{w:?} was taken");
                held.insert(fold);
                pairs.insert(pair);
            }
            Err(Error::Duplicate(m)) => assert!(!fresh, "{w:?} was refused: {m}"),
            Err(e) => panic!("{e}"),
        }
    }
    // The clash names the other row, and a look-up finds it by any
    // spelling.
    let first = answer(&db, "get users select email order id limit 1", &[]);
    let id = first.rows[0].id;
    let email = text(&first.rows[0].values[0]).to_string();
    let shouted: String = email.chars().flat_map(char::to_uppercase).collect();
    let e = exec(
        &mut db,
        "put users {email: $1, n: 0}",
        &[Value::Text(shouted.clone())],
    )
    .unwrap_err();
    assert!(
        e.to_string().contains(&format!("document {id} holds")),
        "{e}"
    );
    let found = answer(
        &db,
        "get users where email = $1",
        &[Value::Text(shouted.clone())],
    );
    assert_eq!(ids(&found), vec![id]);
    // `if absent` passes over it, `else set` sets the row holding it.
    let n = exec(
        &mut db,
        "put users {email: $1, n: 0} if absent",
        &[Value::Text(shouted.clone())],
    )
    .unwrap();
    assert_eq!(n, Response::Affected(0));
    exec(
        &mut db,
        "put users {email: $1, n: 0} if absent else set {n: n + 1000}",
        &[Value::Text(shouted)],
    )
    .unwrap();
    let after = answer(
        &db,
        "get users select email, n where id = $1",
        &[Value::Int(id as i64)],
    );
    assert_eq!(
        after.rows[0].values[0],
        Value::Text(email),
        "kept as written"
    );
    assert_eq!(after.rows[0].values[1], Value::Int(1000));
    // An index made over values two rows hold under one fold is refused,
    // naming the value as the first wrote it.
    exec(&mut db, "create collection two (e text nocase)", &[]).unwrap();
    for e in ["Straße@x.com", "STRASSE@X.COM"] {
        exec(&mut db, "put two {e: $1}", &[Value::Text(e.into())]).unwrap();
    }
    let e = exec(&mut db, "create index on two (e) @unique", &[]).unwrap_err();
    assert!(e.to_string().contains("Straße@x.com"), "{e}");
}

#[test]
fn a_lookup_finds_a_nocase_child_by_its_fold() {
    let mut db = Database::new();
    let table = python();
    exec(&mut db, "create collection pa (email text, k int)", &[]).unwrap();
    exec(
        &mut db,
        "create collection ch (email text nocase @hash, n int)",
        &[],
    )
    .unwrap();
    exec(&mut db, "create collection pat (email text, k int)", &[]).unwrap();
    exec(
        &mut db,
        "create collection cht (email text @hash, n int)",
        &[],
    )
    .unwrap();
    let mut rng = Rng(99);
    let vocab = vocabulary(&mut rng, 30);
    for i in 0..300 {
        let w = pick(&mut rng, &vocab);
        let f = Value::Text(reference(&table, &w, CaseFold::Unicode));
        let n = Value::Int(i);
        // The parents hold their keys as written, folded in the twin.
        let (put, twin, key) = match i % 3 {
            0 => (
                "put pa {email: $1, k: $2}",
                "put pat {email: $1, k: $2}",
                "k",
            ),
            _ => (
                "put ch {email: $1, n: $2}",
                "put cht {email: $1, n: $2}",
                "n",
            ),
        };
        let _ = key;
        exec(&mut db, put, &[Value::Text(w), n.clone()]).unwrap();
        exec(&mut db, twin, &[f, n]).unwrap();
    }
    for (a, b) in [
        (
            "get pa lookup ch on email = email",
            "get pat lookup cht on email = email",
        ),
        (
            "get pa lookup ch on email = email required",
            "get pat lookup cht on email = email required",
        ),
        (
            "get pa lookup ch on email = email where n > 100 limit 2",
            "get pat lookup cht on email = email where n > 100 limit 2",
        ),
    ] {
        let x = answer(&db, a, &[]);
        let y = answer(&db, b, &[]);
        let kids = |rs: &ResultSet| -> Vec<Vec<DocId>> {
            rs.nested
                .as_ref()
                .expect("children")
                .groups
                .iter()
                .map(|g| g.iter().map(|r| r.id).collect())
                .collect()
        };
        assert_eq!(ids(&x), ids(&y), "{a}");
        assert_eq!(kids(&x), kids(&y), "{a}");
    }
}

#[test]
fn a_turkish_nocase_field_folds_its_is_as_turkish_does() {
    let table = python();
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection tr (t text collate tr nocase @sorted, h text collate tr nocase @hash)",
        &[],
    )
    .unwrap();
    exec(
        &mut db,
        "create collection trt (t text collate tr @sorted, h text collate tr @hash)",
        &[],
    )
    .unwrap();
    let names = [
        "İstanbul",
        "istanbul",
        "ISTANBUL",
        "ıstanbul",
        "Istanbul",
        "IŞIK",
        "ışık",
        "işik",
        "İnce",
        "ince",
        "INCE",
        "ılık",
        "ILIK",
        "Çağla",
        "ÇAĞLA",
        "çağla",
        "Ömer",
        "ÖMER",
        "Şule",
        "şULE",
        "kız",
        "KIZ",
        "kiz",
        "KİZ",
        "Ilgaz",
        "ılgaz",
        "ILGAZ",
    ];
    let mut rng = Rng(5);
    for i in 0..400 {
        let w = names[(i * 7 + rng.below(3)) % names.len()];
        let f = reference(&table, w, CaseFold::Turkish);
        exec(&mut db, "put tr {t: $1, h: $1}", &[Value::Text(w.into())]).unwrap();
        exec(&mut db, "put trt {t: $1, h: $1}", &[Value::Text(f)]).unwrap();
    }
    let ask = |sql: &str, q: &str| {
        let a = answer(&db, &sql.replace("COLL", "tr"), &[Value::Text(q.into())]);
        let b = answer(
            &db,
            &sql.replace("COLL", "trt"),
            &[Value::Text(reference(&table, q, CaseFold::Turkish))],
        );
        assert_eq!(ids(&a), ids(&b), "{sql} {q}");
        a
    };
    for q in names {
        for f in ["t", "h"] {
            ask(&format!("get COLL where {f} = $1"), q);
            ask(
                &format!("get COLL where {f} >= $1 order {f}, id limit 50"),
                q,
            );
            ask(&format!("get COLL where {f} < $1 order {f} desc"), q);
        }
    }
    // `ISTANBUL` is `ıstanbul` to a Turkish reader, `İstanbul` is
    // `istanbul`, and the two are apart.
    let a = ask("get COLL where t = $1", "ISTANBUL");
    let b = ask("get COLL where h = $1", "istanbul");
    assert!(!a.rows.is_empty() && !b.rows.is_empty());
    assert!(ids(&a).iter().all(|i| !ids(&b).contains(i)));
    // In `tr`'s order, over the folds: `ılık` before `ince`.
    let all = answer(&db, "get tr select t order t", &[]);
    let order: Vec<String> = all
        .rows
        .iter()
        .map(|r| reference(&table, text(&r.values[0]), CaseFold::Turkish))
        .collect();
    let mut sorted = order.clone();
    sorted.sort_by(|x, y| Collation::Turkish.compare(x, y));
    assert_eq!(order, sorted);
}

#[test]
fn the_fold_survives_an_image_and_a_reopen() {
    let (db, vocab, _) = twins(400, 11);
    let mut back = Database::new();
    back.load(&db.snapshot()).unwrap();
    let mut rng = Rng(3);
    for q in probes(&mut rng, &vocab, 10) {
        for f in ["h", "s", "c"] {
            same_ids(
                &back,
                &format!("get COLL where {f} = $1"),
                &[Value::Text(q.clone())],
            );
        }
    }
    let schema = back.collection("nc").unwrap().schema.clone();
    assert!(schema.fields.iter().filter(|f| f.nocase).count() == 4);
}

/// The records a primary appends, as its feed would pass them on.
#[derive(Clone, Default)]
struct Tap(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Sink for Tap {
    fn append(&mut self, _: &[u8]) -> Result<()> {
        Ok(())
    }
    fn record(&mut self, _: u64, bytes: &[u8]) -> Result<()> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(())
    }
    fn rewrite(&mut self, _: &[u8]) -> Result<()> {
        Ok(())
    }
}

#[test]
fn a_field_added_nocase_folds_on_the_primary_and_its_replica() {
    let tap = Tap::default();
    let mut primary = Database::with_sink(Box::new(tap.clone()));
    exec(&mut primary, "create collection users (n int)", &[]).unwrap();
    exec(&mut primary, "put users [{n: 1}, {n: 2}]", &[]).unwrap();
    // A field added with its index: the rows before it hold no value.
    exec(
        &mut primary,
        "alter collection users add field email text nocase @unique",
        &[],
    )
    .unwrap();
    let t = |s: &str| Value::Text(s.into());
    exec(
        &mut primary,
        "put users {n: 3, email: $1}",
        &[t("Straße@Example.com")],
    )
    .unwrap();
    let e = exec(
        &mut primary,
        "put users {n: 4, email: $1}",
        &[t("STRASSE@example.COM")],
    )
    .unwrap_err();
    assert!(matches!(e, Error::Duplicate(_)), "{e}");
    exec(
        &mut primary,
        "put users {n: 4, email: $1}",
        &[t("ömer@x.com")],
    )
    .unwrap();
    // Compacted, the documents are written again and the fold holds.
    exec(&mut primary, "compact", &[]).unwrap();

    let mut replica = Database::new();
    replica.follow(vec![(1, 0)]).unwrap();
    let records = tap.0.lock().unwrap().clone();
    replica.apply(&records).unwrap();
    for db in [&primary, &replica] {
        for (q, want) in [
            ("strasse@example.com", vec![3]),
            ("ÖMER@X.COM", vec![4]),
            ("nobody", vec![]),
        ] {
            let rs = answer(db, "get users select n where email = $1", &[t(q)]);
            let got: Vec<i64> = rs
                .rows
                .iter()
                .map(|r| match r.values[0] {
                    Value::Int(n) => n,
                    _ => -1,
                })
                .collect();
            assert_eq!(got, want, "{q}");
        }
    }
    assert!(replica.collection("users").unwrap().schema.fields[1].nocase);
}
