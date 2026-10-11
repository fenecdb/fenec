//! Every FenecQL block of site/content/docs/mcp.html -- the memory recipe
//! an agent's `remember` and `recall` run -- run as `redis_docs.rs` runs
//! its page: statement by statement in order against one database, each
//! `-- → x` the answer it checks, `-- 20 s later` the clock moved on so a
//! memory expires.

const PAGE: &str = include_str!("../../../site/content/docs/mcp.html");

#[test]
fn every_recipe_runs_and_answers_what_the_page_says() {
    let (statements, checked) = super::redis_docs::run_page(PAGE);
    assert!(
        statements >= 10 && checked >= 8,
        "{statements} run, {checked} checked"
    );
}
