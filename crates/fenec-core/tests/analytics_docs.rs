//! Every FenecQL block of site/content/docs/analytics.html, run as
//! `redis_docs.rs` runs its page: statement by statement in order against
//! one database, each `-- → x` the first value of a read's first row.

const PAGE: &str = include_str!("../../../site/content/docs/analytics.html");

#[test]
fn every_recipe_runs_and_answers_what_the_page_says() {
    let (statements, checked) = super::redis_docs::run_page(PAGE);
    assert!(
        statements >= 25 && checked >= 20,
        "{statements} run, {checked} checked"
    );
}
