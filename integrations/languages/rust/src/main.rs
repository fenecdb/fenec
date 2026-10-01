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
    query(
        "create collection if not exists docs (title text, embed vector<3> @hnsw(cosine))",
        json!([]),
    )?;
    query("put docs {title: $1, embed: $2}", json!(["Night at the oasis", [0.1, 0.2, 0.3]]))?;
    query("put docs {title: $1, embed: $2}", json!(["Dunes", [0.9, 0.1, 0.0]]))?;

    let rows = query("get docs select title near embed $1 limit 5", json!([[0.1, 0.2, 0.3]]))?;
    let titles: Vec<&str> = rows
        .as_array()
        .ok_or("near answered no rows")?
        .iter()
        .filter_map(|r| r["title"].as_str())
        .collect();
    assert_eq!(titles, ["Night at the oasis", "Dunes"]);

    match query("get nowhere", json!([])) {
        Err(ureq::Error::Status(404, _)) => {}
        other => panic!("a missing collection was answered: {other:?}"),
    }
    println!("rust: ok");
    Ok(())
}
