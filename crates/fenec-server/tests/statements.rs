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

/// A page's size and offset as parameters are one statement whatever the
/// page -- one shape here, as the same pages written into the text were
/// already, since a literal is shaped as a place -- and the answers are
/// the pages the numbers written give.
#[test]
fn every_page_size_is_one_shape() {
    let path = tmp("statements", "pages.fenec");
    let s = start(&["--http-token", TOKEN, "--file", path.to_str().unwrap()]);
    let mut c = s.http().with_token(TOKEN);
    c.run("create collection t (n int)");
    c.run("put t [{n: 1}, {n: 2}, {n: 3}, {n: 4}, {n: 5}, {n: 6}]");
    let page = "get t select n where n > $1 order n limit $2 offset $3";
    for (limit, offset) in [(2, 0), (3, 1), (10, 4)] {
        let body = format!(r#"{{"query":"{page}","params":[1,{limit},{offset}]}}"#);
        let a = c.ask("POST", "/query", &body);
        assert_eq!(a.status, 200, "{}", a.body);
        let written = c.run(&format!(
            "get t select n where n > 1 order n limit {limit} offset {offset}"
        ));
        assert_eq!(a.body, written, "limit {limit} offset {offset}");
    }
    let json = stats(&mut c);
    assert_eq!(field(&json, page, "calls"), Some("6"), "{json}");
    assert_eq!(json.matches("order n limit").count(), 1, "{json}");
    // Refused as the number written is, 400 in the literal's words.
    let a = c.ask(
        "POST",
        "/query",
        &format!(r#"{{"query":"{page}","params":[1,-2,0]}}"#),
    );
    assert_eq!(a.status, 400, "{}", a.body);
    assert!(
        a.body
            .contains("`limit` takes a whole number from 0, not -2"),
        "{}",
        a.body
    );
}
