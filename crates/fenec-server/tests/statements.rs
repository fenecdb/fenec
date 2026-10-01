//! What each statement cost, by its shape, on a running `fenec-server`:
//! `GET /_stats/statements`. A statement with its values in the text is
//! counted with the same one run with other values; the rows it returned
//! or changed and its failures go with it; the counts need the server's
//! token, and go on `DELETE`.

#[path = "support.rs"]
mod support;

use support::{start, tmp, Http};

const TOKEN: &str = "statements-token";

/// The value after `"key":` in the JSON object for `query`, as written. A
/// shape can hold braces, so the object ends after its last key.
fn field<'a>(json: &'a str, query: &str, key: &str) -> Option<&'a str> {
    let at = json.find(&format!("\"query\":\"{query}\""))?;
    let obj = &json[at..];
    let last = obj.find("\"max_ms\":")?;
    let rest = &obj[..last + obj[last..].find('}')?];
    let v = rest.split(&format!("\"{key}\":")).nth(1)?;
    Some(v.split([',', '}']).next()?.trim_matches('"'))
}

fn stats(c: &mut Http) -> String {
    let a = c.ask("GET", "/_stats/statements", "");
    assert_eq!(a.status, 200, "{}", a.body);
    a.body
}

#[test]
fn statements_are_counted_by_their_shape() {
    let path = tmp("statements", "s.fenec");
    let s = start(&["--http-token", TOKEN, "--file", path.to_str().unwrap()]);
    let mut c = s.http().with_token(TOKEN);
    c.run("create collection t (name text, n int @hash)");
    for i in 0..3 {
        c.run(&format!("put t {{name: \"n{i}\", n: {i}}}"));
    }
    c.run("get t where n = 1");
    c.run("get t where n = 99");
    assert!(c.query("get nosuch where n = 1").is_err());
    c.run("get t count");

    let json = stats(&mut c);
    let put = "put t {name: $1, n: $2}";
    assert_eq!(field(&json, put, "calls"), Some("3"), "{json}");
    assert_eq!(field(&json, put, "rows"), Some("3"), "{json}");
    let get = "get t where n = $1";
    assert_eq!(field(&json, get, "calls"), Some("2"), "{json}");
    assert_eq!(field(&json, get, "rows"), Some("1"), "{json}");
    assert_eq!(
        field(&json, "get nosuch where n = $1", "errors"),
        Some("1"),
        "{json}"
    );
    assert_eq!(field(&json, "get t count", "calls"), Some("1"), "{json}");
    // A REST read is counted by its route, its values as places.
    assert_eq!(c.ask("GET", "/t?n=eq.1", "").status, 200);
    let json = stats(&mut c);
    assert_eq!(field(&json, "GET /t?n=eq.$1", "rows"), Some("1"), "{json}");

    // The token the metrics take; none is no count.
    assert_eq!(s.http().ask("GET", "/_stats/statements", "").status, 401);
    assert_eq!(c.ask("DELETE", "/_stats/statements", "").status, 204);
    let json = stats(&mut c);
    assert!(!json.contains(put), "{json}");
}
