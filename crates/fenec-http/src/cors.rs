//! What a page on another origin is told (`--http-cors`): by a node on
//! every answer, and by `fenec-shard` on every answer it gives, its own and
//! the ones it forwards. One list for both, so a page reads the same
//! headers whichever of the two it reaches.

use crate::http::{Request, Response};

/// Whether `--http-cors` lets `req`'s origin in: `*` lets every one, an
/// origin itself alone. A preflight from another is answered as before,
/// after its token.
pub fn allows(cors: Option<&str>, req: &Request) -> bool {
    match cors {
        None => false,
        Some("*") => true,
        Some(allowed) => req.header("origin") == Some(allowed),
    }
}

/// The headers an answer carries under `--http-cors <origin>`.
fn headers(origin: &str) -> [(&'static str, &str); 6] {
    [
        ("Access-Control-Allow-Origin", origin),
        (
            "Access-Control-Allow-Methods",
            "GET, POST, PATCH, DELETE, OPTIONS",
        ),
        // A page's replica sends each write under an `Idempotency-Key`
        // and reads the `Fenec-Seq` of its answer, which tells it when a
        // stream is past the write: refused and unread across origins
        // before, its retries made a row twice and an insert the shape
        // did not hold kept its temporary row until the next seed. The
        // request id goes out too: the studio's query editor, served by
        // a node and asking a router (`--studio-connect`), shows it
        // beside an answer so a person can find its lines in the logs.
        (
            "Access-Control-Allow-Headers",
            "content-type, authorization, idempotency-key, fenec-after, fenec-wait, if-none-match",
        ),
        (
            "Access-Control-Expose-Headers",
            "fenec-seq, fenec-next, idempotent-replayed, etag, x-request-id, fenec-quota, retry-after",
        ),
        ("Access-Control-Max-Age", "600"),
        ("Vary", "Origin"),
    ]
}

/// `resp` with the headers `cors` names, or as it is without one.
pub fn apply(resp: Response, cors: Option<&str>) -> Response {
    match cors {
        None => resp,
        Some(origin) => headers(origin)
            .into_iter()
            .fold(resp, |resp, (k, v)| resp.header(k, v)),
    }
}

/// The same headers written into a head being built by hand: the router's
/// for an answer it forwards.
pub fn push_into(head: &mut String, origin: &str) {
    for (k, v) in headers(origin) {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
}

/// Whether a header of a node's answer is one [`apply`] writes: a router
/// with `--http-cors` of its own puts its own in their place. Both kept, a
/// node allowing `*` behind a router allowing one origin answered two
/// `Access-Control-Allow-Origin`s, which a browser refuses whole; and the
/// router answers the preflights itself, so the answer has to say what its
/// preflight said. A `Vary` naming anything but `Origin` alone is kept.
pub fn is_cors_header(name: &str, value: &str) -> bool {
    let name = name.as_bytes();
    let prefix = b"access-control-";
    (name.len() > prefix.len() && name[..prefix.len()].eq_ignore_ascii_case(prefix))
        || (name.eq_ignore_ascii_case(b"vary") && value.trim().eq_ignore_ascii_case("origin"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nodes_cors_headers_are_told_from_the_rest() {
        assert!(is_cors_header("Access-Control-Allow-Origin", "*"));
        assert!(is_cors_header("access-control-expose-headers", "x"));
        assert!(is_cors_header("Vary", "Origin"));
        assert!(is_cors_header("vary", " origin "));
        assert!(!is_cors_header("Vary", "Accept-Encoding"));
        assert!(!is_cors_header("Vary", "Origin, Accept-Encoding"));
        assert!(!is_cors_header("Access-Control-", ""));
        assert!(!is_cors_header("Content-Type", "application/json"));
        assert!(!is_cors_header("Fenec-Seq", "3"));
    }

    #[test]
    fn written_by_hand_as_onto_a_response() {
        let resp = apply(Response::empty(204), Some("https://app.example"));
        let mut head = String::new();
        push_into(&mut head, "https://app.example");
        let from_resp: String = resp
            .extra
            .iter()
            .map(|(k, v)| format!("{k}: {v}\r\n"))
            .collect();
        assert_eq!(head, from_resp);
        assert!(apply(Response::empty(204), None).extra.is_empty());
    }
}
