//! Rust with ureq and serde_json. Run by ../run-tests.sh.
use serde_json::{json, Value};

/// One FenecQL statement. A refusal is `ureq::Error::Status` with the
/// server's status, its body `{"error": ...}`.
fn query(q: &str, params: Value) -> Result<Value, ureq::Error> {
    let url = std::env::var("FENEC_URL").unwrap_or("http://127.0.0.1:8080".into());
    let mut req = ureq::post(&format!("{url}/query"));
    if let Ok(token) = std::env::var("FENEC_TOKEN") {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    Ok(req.send_json(json!({"query": q, "params": params}))?.into_json()?)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    query("create collection if not exists docs (title text @text, year int @sorted)", json!([]))?;
    query("put docs {title: $1, year: $2}", json!(["Dunes", 2021]))?;
    query("put docs {title: $1, year: $2}", json!(["Night at the oasis", 2024]))?;

    let rows = query(
        "get docs select title, year where year >= $1 order year desc limit $2",
        json!([2020, 5]),
    )?;
    assert_eq!(
        rows,
        json!([{"title": "Night at the oasis", "year": 2024}, {"title": "Dunes", "year": 2021}])
    );

    let hits = query("get docs select title match title $1 fuzzy", json!(["oasys"]))?;
    let titles: Vec<&str> = hits
        .as_array()
        .ok_or("match answered no rows")?
        .iter()
        .filter_map(|r| r["title"].as_str())
        .collect();
    assert_eq!(titles, ["Night at the oasis"]);
    assert!(hits[0]["_score"].as_f64() > Some(0.0));

    match query("get nowhere", json!([])) {
        Err(ureq::Error::Status(404, _)) => {}
        other => panic!("a missing collection was answered: {other:?}"),
    }
    println!("rust: ok");
    Ok(())
}
