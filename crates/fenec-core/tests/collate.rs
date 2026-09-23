//! `order ... collate tr`: Turkish text in the order ICU's `tr` collation
//! gives it, on every path that orders -- the sort, a `@sorted` field, a
//! `lookup`'s children, an aggregate's groups -- and refused where it would
//! mean nothing.

use fenec_core::prelude::*;

fn exec(db: &mut Database, sql: &str, params: &[Value]) {
    db.execute_with(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn query(db: &Database, sql: &str) -> ResultSet {
    db.query(&fenec_ql::parse_one(sql).expect("parse"), &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows()
        .expect("rows")
        .clone()
}

fn texts(rows: &[Row], col: usize) -> Vec<String> {
    rows.iter()
        .map(|r| match &r.values[col] {
            Value::Text(s) => s.clone(),
            other => panic!("not text: {other:?}"),
        })
        .collect()
}

fn error(db: &Database, sql: &str) -> String {
    match fenec_ql::parse_one(sql) {
        Err(e) => e.to_string(),
        Ok(stmt) => match db.query(&stmt, &[]) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("{sql}: no error"),
        },
    }
}

/// `Intl.Collator("tr")`'s order (Node 26.7, ICU 78.3) -- the table was
/// generated from ICU 76.1, so this is a second ICU agreeing with it. Every
/// Turkish letter in both cases, the dotted and dotless `i` apart, the
/// circumflex, and the foreign names a Turkish list holds: an apostrophe,
/// an acute, `ø`, `ł`, `ß` and `æ` against their spellings out.
const ICU: &str = "\
Âdem, Aetna, Ætna, ahmet, Ahmet, Ak, Akın, Aksoy, Akyüz, Ali, Alper, Arda, Aslı, Aydın, \
Aynur, Ayse, Ayşe, Barış, Batuhan, Berk, Burak, Büşra, Can, Cansu, Cem, Cemil, Ceren, \
Cihan, Çağan, çağla, Çağla, Çağrı, Çelik, Çiğdem, D’Angelo, de Souza, Deniz, Derya, \
Dilek, Doğan, Ebru, Ece, Efe, Elif, Émile, Emre, Engin, Erdem, Esra, Éva, Fatma, \
Ferhat, Gamze, Gökhan, Gönül, Görkem, Gül, Gülşen, Hakan, Hande, Hülya, ılgaz, Ilgaz, \
Ilgın, ınce, Ince, Irmak, Işık, İbrahim, İlker, ince, İnce, İnci, İpek, ismail, İsmail, \
Kamil, Kâmil, Kemal, Kılıç, Koç, Kurt, Leyla, Łukasz, Mehmet, Melek, Merve, Mueller, \
Murat, Mustafa, Müller, Nazlı, Nur, Oğuz, Okan, Onur, Orhan, Øystein, ömer, Ömer, \
Özdemir, Özge, Özgür, Öztürk, Pınar, Rüya, Sait, Selin, Serkan, Sibel, Strasse, Straße, \
Şahin, Şebnem, şule, Şule, Tuba, Tuğba, Tülay, Uğur, Umut, Ülkü, ümit, Ümit, Volkan, \
Yağmur, Yıldırım, Yıldız, Yılmaz, Yusuf, Zeynep, Züleyha";

fn icu() -> Vec<&'static str> {
    ICU.split(", ").collect()
}

/// The names in a scrambled order, so the answer cannot be the insertion's.
fn people(extra: &str) -> Database {
    let mut db = Database::new();
    exec(
        &mut db,
        &format!("create collection people (name text{extra}, city text @hash, n int)"),
        &[],
    );
    let icu = icu();
    for i in 0..icu.len() {
        let name = icu[(i * 53) % icu.len()];
        exec(
            &mut db,
            "put people {name: $1, city: $2, n: $3}",
            &[
                Value::Text(name.into()),
                Value::Text(["İzmir", "Çorum", "Iğdır", "Ankara", "Şırnak"][i % 5].into()),
                Value::Int(i as i64),
            ],
        );
    }
    db
}

#[test]
fn a_turkish_name_list_in_icus_order() {
    let db = people("");
    let rs = query(&db, "get people select name order name collate tr");
    assert_eq!(texts(&rs.rows, 0), icu());

    let rs = query(&db, "get people select name order name collate tr desc");
    let mut reversed = icu();
    reversed.reverse();
    assert_eq!(texts(&rs.rows, 0), reversed);

    // Byte order, which is what `order` gives without it, is not that.
    let rs = query(&db, "get people select name order name");
    let mut bytes = icu();
    bytes.sort();
    assert_eq!(texts(&rs.rows, 0), bytes);
    assert_ne!(bytes, icu());
}

#[test]
fn pages_put_together_are_the_whole_order() {
    let db = people("");
    let mut paged = Vec::new();
    for page in 0..14 {
        let rs = query(
            &db,
            &format!(
                "get people select name order name collate tr limit 10 offset {}",
                page * 10
            ),
        );
        paged.extend(texts(&rs.rows, 0));
    }
    assert_eq!(paged, icu());
}

#[test]
fn keys_combine_and_the_direction_reads_either_side() {
    let db = people("");
    let rs = query(
        &db,
        "get people select city, name order city collate tr desc, name collate tr",
    );
    let cities: Vec<String> = texts(&rs.rows, 0);
    let mut runs = cities.clone();
    runs.dedup();
    assert_eq!(runs, ["Şırnak", "İzmir", "Iğdır", "Çorum", "Ankara"]);
    for city in runs {
        let names: Vec<String> = rs
            .rows
            .iter()
            .filter(|r| r.values[0] == Value::Text(city.clone()))
            .map(|r| texts(std::slice::from_ref(r), 1).remove(0))
            .collect();
        let want: Vec<String> = icu()
            .iter()
            .filter(|n| names.contains(&n.to_string()))
            .map(|n| n.to_string())
            .collect();
        assert_eq!(names, want, "{city}");
    }
    let a = query(&db, "get people select name order name collate tr desc");
    let b = query(&db, "get people select name order name desc collate tr");
    assert_eq!(a, b);
}

#[test]
fn a_sorted_field_is_not_walked_in_another_order() {
    // `@sorted` holds byte order; walking it for a collated key would hand
    // back the first page in the wrong order.
    let db = people(" @sorted");
    let rs = query(&db, "get people select name order name collate tr limit 5");
    assert_eq!(texts(&rs.rows, 0), &icu()[..5]);
    let rs = query(&db, "get people select name order name limit 5");
    let mut bytes = icu();
    bytes.sort();
    assert_eq!(texts(&rs.rows, 0), &bytes[..5]);

    let plan = query(
        &db,
        "explain get people select name order name collate tr limit 5",
    );
    let steps = texts(&plan.rows, 0);
    assert!(
        steps
            .iter()
            .any(|s| s.starts_with("order: name collate tr, every key read")),
        "{steps:?}"
    );
}

#[test]
fn a_lookups_children_and_an_aggregates_groups() {
    let mut db = people("");
    exec(&mut db, "create collection cities (name text @hash)", &[]);
    for city in ["Şırnak", "Çorum"] {
        exec(
            &mut db,
            "put cities {name: $1}",
            &[Value::Text(city.into())],
        );
    }
    let rs = query(
        &db,
        "get cities select name order name collate tr \
         lookup people on city = name select name order name collate tr desc limit 3",
    );
    assert_eq!(texts(&rs.rows, 0), ["Çorum", "Şırnak"]);
    let n = rs.nested.as_ref().expect("nested");
    for (i, city) in ["Çorum", "Şırnak"].iter().enumerate() {
        let all = query(
            &db,
            &format!("get people select name where city = \"{city}\" order name collate tr desc"),
        );
        assert_eq!(texts(n.group(i), 0), texts(&all.rows[..3], 0), "{city}");
    }

    let rs = query(
        &db,
        "get people select city, count(*) group city order city collate tr",
    );
    assert_eq!(
        texts(&rs.rows, 0),
        ["Ankara", "Çorum", "Iğdır", "İzmir", "Şırnak"]
    );
    let rs = query(
        &db,
        "get people select city, min(name), count(*) group city order min(name) collate tr",
    );
    let firsts = texts(&rs.rows, 1);
    let mut want = firsts.clone();
    want.sort_by(|a, b| Collation::Turkish.compare(a, b));
    assert_eq!(firsts, want);
}

#[test]
fn a_list_of_text_compares_element_by_element() {
    let mut db = Database::new();
    exec(&mut db, "create collection t (tags [text])", &[]);
    for tags in [
        &["şeker", "a"][..],
        &["su"],
        &["çay", "z"],
        &["çay"],
        &["cam"],
    ] {
        let list = Value::List(tags.iter().map(|t| Value::Text(t.to_string())).collect());
        exec(&mut db, "put t {tags: $1}", &[list]);
    }
    let rs = query(&db, "get t select tags order tags collate tr");
    let got: Vec<Value> = rs.rows.iter().map(|r| r.values[0].clone()).collect();
    let list = |t: &[&str]| Value::List(t.iter().map(|s| Value::Text(s.to_string())).collect());
    assert_eq!(
        got,
        [
            list(&["cam"]),
            list(&["çay"]),
            list(&["çay", "z"]),
            list(&["su"]),
            list(&["şeker", "a"])
        ]
    );
}

#[test]
fn collate_is_refused_where_it_orders_nothing() {
    let mut db = people("");
    exec(&mut db, "create collection cities (name text @hash)", &[]);
    for (sql, says) in [
        (
            "get people order n collate tr",
            "`collate tr` orders text; `n` is int",
        ),
        (
            "get people order id collate tr",
            "`collate tr` orders text; `id` is int",
        ),
        ("get people order name collate de", "unknown collation `de`"),
        ("get people order name collate", "expected a name"),
        (
            "get people select city, count(*) group city order count collate tr",
            "`collate tr` orders text; `count` is not",
        ),
        (
            "get cities lookup people on city = name order n collate tr",
            "`collate tr` orders text; `people.n` is int",
        ),
    ] {
        let err = error(&db, sql);
        assert!(err.contains(says), "{sql}: {err}");
    }
}

fn query_with(db: &Database, sql: &str, params: &[Value]) -> ResultSet {
    db.query(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows()
        .expect("rows")
        .clone()
}

fn text(s: &str) -> Value {
    Value::Text(s.to_string())
}

/// A field in a collation -- `name text collate tr` -- orders in it
/// wherever its text is compared: `order` that names none, `<` and `>` in a
/// `where`, `min` and `max`. So a Turkish list pages by the last name it
/// showed, the keyset way, which byte order made wrong. A `@sorted` index
/// over the field holds the same order and answers as the scan does, row
/// for row; equality stays the bytes'.
#[test]
fn a_collated_field_orders_and_compares_in_its_collation() {
    let icu = icu();
    for extra in [" collate tr", " collate tr @sorted", " @sorted collate tr"] {
        let db = people(extra);
        let names = |sql: &str, params: &[Value]| texts(&query_with(&db, sql, params).rows, 0);
        assert_eq!(
            names("get people select name order name", &[]),
            icu,
            "{extra}"
        );
        let mut reversed = icu.clone();
        reversed.reverse();
        assert_eq!(
            names("get people select name order name desc", &[]),
            reversed,
            "{extra}"
        );
        assert_eq!(
            names("get people select name order name limit 10 offset 40", &[]),
            &icu[40..50],
            "{extra}"
        );

        for (i, p) in icu.iter().enumerate() {
            let cmp = |op: &str| {
                names(
                    &format!("get people select name where name {op} $1 order name"),
                    &[text(p)],
                )
            };
            assert_eq!(cmp(">"), &icu[i + 1..], "{extra}: > {p}");
            assert_eq!(cmp(">="), &icu[i..], "{extra}: >= {p}");
            assert_eq!(cmp("<"), &icu[..i], "{extra}: < {p}");
            assert_eq!(cmp("<="), &icu[..=i], "{extra}: <= {p}");
        }
        let n = icu.len();
        for (a, b) in [(3, 90), (40, 41), (70, n - 1), (0, n - 1)] {
            assert_eq!(
                names(
                    "get people select name where name >= $1 and name < $2 order name",
                    &[text(icu[a]), text(icu[b])]
                ),
                &icu[a..b],
                "{extra}: [{a}, {b})"
            );
        }

        let mut paged: Vec<String> = Vec::new();
        loop {
            let page = match paged.last() {
                None => names("get people select name order name limit 7", &[]),
                Some(last) => names(
                    "get people select name where name > $1 order name limit 7",
                    &[text(last)],
                ),
            };
            if page.is_empty() {
                break;
            }
            paged.extend(page);
        }
        assert_eq!(paged, icu, "{extra}");

        let rs = query(&db, "get people select min(name), max(name)");
        assert_eq!(
            (texts(&rs.rows, 0), texts(&rs.rows, 1)),
            (
                vec![icu[0].to_string()],
                vec![icu[icu.len() - 1].to_string()]
            ),
            "{extra}"
        );
        assert_eq!(
            names("get people select name where name = \"Çağla\"", &[]),
            ["Çağla"],
            "{extra}"
        );
    }
}

/// A `@sorted` index over a collated field is walked for its order -- named
/// or not -- and answers a range; byte order is its bytes' no longer.
#[test]
fn a_collated_sorted_field_is_walked_in_its_order() {
    let db = people(" collate tr @sorted");
    for sql in [
        "explain get people select name order name limit 5",
        "explain get people select name order name collate tr limit 5",
    ] {
        let steps = texts(&query(&db, sql).rows, 0);
        assert!(
            steps
                .iter()
                .any(|s| s.starts_with("order: walked the ordered index on name")),
            "{sql}: {steps:?}"
        );
    }
    let steps = texts(
        &query_with(
            &db,
            "explain get people select name where name > $1",
            &[text("Kemal")],
        )
        .rows,
        0,
    );
    assert!(
        steps
            .iter()
            .any(|s| s.contains("the ordered index on name")),
        "{steps:?}"
    );
    let icu = icu();
    let at = icu.iter().position(|n| *n == "Kemal").unwrap();
    let rs = query_with(
        &db,
        "get people select name where name > $1",
        &[text("Kemal")],
    );
    let mut got = texts(&rs.rows, 0);
    got.sort_by(|a, b| Collation::Turkish.compare(a, b));
    assert_eq!(got, &icu[at + 1..]);
}

/// The collation is the field's, so it travels with the schema: into the
/// file, back out of it, and through `create index`.
#[test]
fn a_fields_collation_is_kept_in_the_file() {
    let mut db = people(" collate tr");
    let image = db.snapshot();
    let mut back = Database::new();
    back.load(&image).expect("load");
    let rs = query(&back, "get people select name order name limit 5");
    assert_eq!(texts(&rs.rows, 0), &icu()[..5]);

    exec(&mut db, "create index on people (name) @sorted", &[]);
    let rs = query(&db, "get people select name order name limit 5");
    assert_eq!(texts(&rs.rows, 0), &icu()[..5]);
    let mut back = Database::new();
    back.load(&db.snapshot()).expect("load");
    let rs = query(&back, "get people select name order name desc limit 3");
    let icu = icu();
    let n = icu.len();
    assert_eq!(texts(&rs.rows, 0), [icu[n - 1], icu[n - 2], icu[n - 3]]);
}

/// Only text orders in a collation, which the schema says before any
/// statement runs.
#[test]
fn a_collation_on_anything_but_text_is_refused() {
    let err = fenec_ql::parse_one("create collection t (n int collate tr)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("`collate tr` orders text; `n` is int"),
        "{err}"
    );
    let err = Schema::new(
        "t",
        vec![Field::new("at", DataType::Timestamp).collated(Collation::Turkish)],
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("`at` is timestamp"), "{err}");
    let mut db = Database::new();
    exec(&mut db, "create collection u (tags [text] collate tr)", &[]);
}
