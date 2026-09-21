//! The subset of HTTP/1.1 that fenecdb needs.
//!
//! Why our own parser: the only requirement is "request line + headers +
//! `Content-Length` body", and that costs ~250 lines. An HTTP library would
//! bring a dependency tree, compile time and binary size -- the rest of the
//! project writes its own JSON and its own PostgreSQL wire for the same
//! reason.
//!
//! Unsupported and rejected **explicitly, not silently**:
//! `Transfer-Encoding: chunked` (411), body over the ceiling (413), header
//! block over the ceiling (431).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

/// Ceiling of the header block. A large ceiling would let a single
/// connection grow memory without bound.
pub const MAX_HEADER_BYTES: usize = 64 << 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Patch,
    Put,
    Delete,
    Options,
    Head,
}

impl Method {
    fn parse(s: &str) -> Option<Method> {
        Some(match s {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "PATCH" => Method::Patch,
            "PUT" => Method::Put,
            "DELETE" => Method::Delete,
            "OPTIONS" => Method::Options,
            "HEAD" => Method::Head,
            _ => return None,
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Patch => "PATCH",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Options => "OPTIONS",
            Method::Head => "HEAD",
        }
    }
}

pub struct Request {
    pub method: Method,
    /// The request target exactly as sent, still percent-encoded: a proxy
    /// forwards this, since re-encoding `path` and `query` would not
    /// reproduce every byte of the original.
    pub target: String,
    /// Percent-decoded path: `/articles/near`
    pub path: String,
    /// Query string pairs, **in order** and with repeats: two conditions on
    /// the same field (`?year=gte.2020&year=lt.2030`) would be lost in a map.
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub keep_alive: bool,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The non-empty `/`-separated segments of the path.
    pub fn segments(&self) -> Vec<&str> {
        self.path.split('/').filter(|s| !s.is_empty()).collect()
    }
}

/// A parse error: maps directly onto an HTTP status.
pub struct BadRequest(pub u16, pub String);

/// Reads a request. `Ok(None)` means the connection closed cleanly.
pub fn read_request(
    reader: &mut BufReader<TcpStream>,
    max_body: usize,
) -> Result<Option<Request>, BadRequest> {
    let mut head = Vec::new();
    loop {
        let mut line = Vec::new();
        let n = read_line(reader, &mut line, MAX_HEADER_BYTES - head.len())?;
        if n == 0 {
            return if head.is_empty() {
                Ok(None) // the connection closed
            } else {
                Err(BadRequest(400, "truncated request header".into()))
            };
        }
        let done = line == b"\r\n" || line == b"\n";
        head.extend_from_slice(&line);
        if done {
            break;
        }
        if head.len() >= MAX_HEADER_BYTES {
            return Err(BadRequest(431, "header block too large".into()));
        }
    }

    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n").flat_map(|l| l.split('\n'));
    let start = lines.next().unwrap_or_default();
    let mut parts = start.split_whitespace();
    let (method, target, version) = match (parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(t), Some(v)) => (m, t, v),
        _ => return Err(BadRequest(400, "malformed request line".into())),
    };
    let method = Method::parse(method)
        .ok_or_else(|| BadRequest(501, format!("`{method}` is not supported")))?;

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            return Err(BadRequest(400, "malformed header line".into()));
        };
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }

    let get = |name: &str| -> Option<&str> {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };

    if get("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        return Err(BadRequest(
            411,
            "a chunked body is not supported: Content-Length is required".into(),
        ));
    }

    let len: usize = match get("content-length") {
        Some(v) => v
            .trim()
            .parse()
            .map_err(|_| BadRequest(400, "invalid Content-Length".into()))?,
        None => 0,
    };
    if len > max_body {
        return Err(BadRequest(
            413,
            format!("the body is {len} bytes, the ceiling is {max_body}"),
        ));
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        reader
            .read_exact(&mut body)
            .map_err(|e| BadRequest(400, format!("could not read the body: {e}")))?;
    }

    // HTTP/1.1 defaults to a persistent connection; 1.0 is the other way round.
    let keep_alive = match get("connection") {
        Some(v) if v.eq_ignore_ascii_case("close") => false,
        Some(v) if v.eq_ignore_ascii_case("keep-alive") => true,
        _ => version.eq_ignore_ascii_case("HTTP/1.1"),
    };

    let (raw_path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    Ok(Some(Request {
        method,
        target: target.to_string(),
        path: percent_decode(raw_path),
        query: parse_query(raw_query),
        headers,
        body,
        keep_alive,
    }))
}

fn read_line(
    reader: &mut BufReader<TcpStream>,
    out: &mut Vec<u8>,
    budget: usize,
) -> Result<usize, BadRequest> {
    let mut taken = 0;
    loop {
        let available = reader
            .fill_buf()
            .map_err(|e| BadRequest(400, format!("read error: {e}")))?;
        if available.is_empty() {
            return Ok(taken);
        }
        match available.iter().position(|b| *b == b'\n') {
            Some(i) => {
                out.extend_from_slice(&available[..=i]);
                reader.consume(i + 1);
                return Ok(taken + i + 1);
            }
            None => {
                let n = available.len();
                out.extend_from_slice(available);
                reader.consume(n);
                taken += n;
                if taken > budget {
                    return Err(BadRequest(431, "header line too long".into()));
                }
            }
        }
    }
}

/// `a=1&b=2` -> `[("a","1"), ("b","2")]`. A valueless key becomes an empty
/// string (`?count` and `?count=` mean the same thing).
pub fn parse_query(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(p), String::new()),
        })
        .collect()
}

/// Decodes percent encoding and `+`. An invalid sequence is left as is.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => match hex(b[i + 1]).zip(hex(b[i + 2])) {
                Some((h, l)) => {
                    out.push(h << 4 | l);
                    i += 3;
                }
                None => {
                    out.push(b[i]);
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// ----------------------------------------------------------------- response

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    pub content_type: &'static str,
    pub extra: Vec<(String, String)>,
}

impl Response {
    pub fn json(status: u16, body: impl Into<Vec<u8>>) -> Response {
        Response {
            status,
            body: body.into(),
            content_type: "application/json; charset=utf-8",
            extra: Vec::new(),
        }
    }

    pub fn error(status: u16, message: &str) -> Response {
        let mut body = String::from("{\"error\":");
        fenec_core::json::escape_into(&mut body, message);
        body.push('}');
        Response::json(status, body)
    }

    pub fn empty(status: u16) -> Response {
        Response {
            status,
            body: Vec::new(),
            content_type: "application/json; charset=utf-8",
            extra: Vec::new(),
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Response {
        self.extra.push((name.to_string(), value.to_string()));
        self
    }

    pub fn write(
        &self,
        out: &mut impl Write,
        keep_alive: bool,
        head_only: bool,
    ) -> std::io::Result<()> {
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n",
            self.status,
            reason(self.status),
            self.content_type,
            self.body.len(),
            if keep_alive { "keep-alive" } else { "close" },
        );
        for (k, v) in &self.extra {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        out.write_all(head.as_bytes())?;
        if !head_only {
            out.write_all(&self.body)?;
        }
        out.flush()
    }
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_percent_and_plus() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("%C3%BCst"), "\u{fc}st");
        // An invalid sequence stays as is.
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn query_keeps_repeats_and_order() {
        let q = parse_query("year=gte.2020&year=lt.2030&count");
        assert_eq!(
            q,
            vec![
                ("year".to_string(), "gte.2020".to_string()),
                ("year".to_string(), "lt.2030".to_string()),
                ("count".to_string(), String::new()),
            ]
        );
    }
}
