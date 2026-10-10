//! Every FenecQL block of site/content/docs/redis.html, run statement by
//! statement in order against one database, and each `-- → x` the answer
//! it checks: a write's count, or the first value of a read's first row. A
//! recipe on the page that stops working fails here, not in a reader's app.

use fenec_core::prelude::*;

const PAGE: &str = include_str!("../../../site/content/docs/redis.html");

/// The page's `<pre data-lang="fenecql">` blocks, unescaped.
fn blocks(page: &str) -> Vec<String> {
    let open = "<pre data-lang=\"fenecql\">";
    let mut out = Vec::new();
    let mut rest = page;
    while let Some(at) = rest.find(open) {
        rest = &rest[at + open.len()..];
        let end = rest.find("</pre>").expect("a block ends");
        out.push(
            rest[..end]
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&amp;", "&"),
        );
        rest = &rest[end..];
    }
    out
}

#[test]
fn every_recipe_runs_and_answers_what_the_page_says() {
    let (statements, checked) = run_page(PAGE);
    // The page has its recipes, and says what most of them answer.
    assert!(
        statements >= 30 && checked >= 20,
        "{statements} run, {checked} checked"
    );
}

/// Runs every statement of a page's blocks, in order, over one database
/// whose clock is 2026-05-03T09:20Z: how many it ran and how many answers
/// it checked. `analytics_docs.rs` and `queues_docs.rs` run their pages
/// through it too. A line of a comment alone is passed over, and one
/// saying `-- 20 s later` moves the clock on: a lease that lapses.
pub(crate) fn run_page(page: &str) -> (usize, usize) {
    let mut db = Database::new();
    let mut now = 1_777_800_000_000;
    db.set_clock(Some(now));
    let (mut statements, mut checked) = (0, 0);
    for block in blocks(page) {
        for line in block.lines().filter(|l| !l.trim().is_empty()) {
            if let Some(comment) = line.trim().strip_prefix("--") {
                if let Some(s) = comment.trim().strip_suffix(" s later") {
                    now += s.parse::<i64>().expect("`-- <n> s later`") * 1000;
                    db.set_clock(Some(now));
                }
                continue;
            }
            let (code, expect) = match line.split_once("-- →") {
                Some((code, rest)) => (code, Some(rest.trim())),
                None => (line, None),
            };
            let st = fenec_ql::parse_one(code)
                .unwrap_or_else(|e| panic!("`{code}` does not parse: {e}"));
            // `-- → refused`: a write whose `require` the page says is not met.
            if expect.is_some_and(|e| e.starts_with("refused")) {
                let e = db.execute(&st).expect_err(code);
                assert!(matches!(e, Error::Unmet(_)), "`{code}`: {e}");
                statements += 1;
                checked += 1;
                continue;
            }
            let answer = db
                .execute(&st)
                .unwrap_or_else(|e| panic!("`{code}` failed: {e}"));
            statements += 1;
            let Some(expect) = expect else { continue };
            // The answer is what comes before a `:` and its explanation.
            // A quoted answer is whole, `:`s and all: a time is one.
            let want = match expect.strip_prefix('"').and_then(|e| e.find('"')) {
                Some(end) => &expect[..end + 2],
                None => expect.split(':').next().unwrap().trim(),
            };
            let got = match &answer {
                Response::Affected(n) => n.to_string(),
                Response::Rows(rs) => match rs.rows.first().and_then(|r| r.values.first()) {
                    Some(Value::Text(t)) => format!("\"{t}\""),
                    Some(v) => fenec_core::json::to_string(v),
                    None => "no row".into(),
                },
                other => format!("{other:?}"),
            };
            assert_eq!(got, want, "`{}` answered {got}", code.trim());
            checked += 1;
        }
    }
    (statements, checked)
}
