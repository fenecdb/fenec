use serde_json::{json, Value};

fn query(q: &str, params: Value) -> Result<Value, ureq::Error> {
    Ok(ureq::post("http://127.0.0.1:8080/query")
        .send_json(json!({"query": q, "params": params}))?
        .into_json()?)
}

fn titles(rows: &Value) -> String {
    let titles: Vec<&str> = rows.as_array().unwrap().iter().filter_map(|r| r["title"].as_str()).collect();
    titles.join(", ")
}

fn main() -> Result<(), ureq::Error> {
    query("put notes {title: $1, year: $2, kind: $3}", json!(["Sand and stars", 2023, "book"]))?;

    let rows = query("get notes where year >= $1 order year desc", json!([2020]))?;
    println!("from 2020: {}", titles(&rows));

    let hits = query("get notes select title match title $1 fuzzy", json!(["oasys"]))?;
    println!("search: {}", titles(&hits));

    // A refusal is `Error::Status` with the server's status, its body {"error": ...}.
    match query("get nowhere", json!([])) {
        Err(ureq::Error::Status(status, res)) => {
            let body: Value = res.into_json()?;
            println!("refused: {status} {}", body["error"].as_str().unwrap_or_default());
        }
        other => {
            other?;
        }
    }
    Ok(())
}
