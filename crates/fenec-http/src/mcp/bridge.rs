//! `fenec mcp --url`: the stdio binding in front of a server's Streamable
//! HTTP endpoint, for a client that starts its tools as processes -- Claude
//! Desktop's configuration, an agent framework's stdio transport -- and a
//! database a server holds: a file is a server's alone while it runs, so a
//! second process over it is refused (`--file` takes its lock).
//!
//! Each line is posted as it came, with the headers the 2026-07-28 binding
//! mirrors from the body (`MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name`)
//! and the token from the environment, never from the command line, which
//! `ps` shows; the answer's JSON goes out as a line. The tools run on the
//! server, held to its options and to the token's rules, so the bridge
//! decides nothing. A failure of its own -- the server down, an answer that
//! is not JSON-RPC -- is answered as an error naming the bridge, so it is
//! never taken for the server's. Plain HTTP only: fenecdb speaks no TLS, and
//! a client reaches an https endpoint with its own HTTP transport.

use fenec_core::json;
use fenec_core::prelude::*;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Where the bridge posts: the endpoint's address, its `Host`, its path,
/// and the token sent with every message.
#[derive(Clone, Debug)]
pub struct Upstream {
    pub addr: String,
    pub host: String,
    pub path: String,
    pub token: Option<String>,
}

impl Upstream {
    /// `http://host:port/_mcp`, or `/t/<tenant>/_mcp` on a tenant node or a
    /// router; the path is `/_mcp` when the URL names none.
    pub fn parse(url: &str, token: Option<String>) -> std::result::Result<Upstream, String> {
        let Some(rest) = url.strip_prefix("http://") else {
            return Err(match url.starts_with("https://") {
                true => "fenecdb speaks no TLS, and neither does this bridge: give an https \
                         endpoint to the client's own HTTP transport (\"type\": \"http\", \"url\")"
                    .into(),
                false => format!("`{url}` is not an http:// URL: http://127.0.0.1:8080/_mcp"),
            });
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/_mcp"),
        };
        if authority.is_empty() {
            return Err(format!("`{url}` names no host"));
        }
        let has_port = match authority.rfind(']') {
            Some(close) => authority[close..].contains(':'),
            None => authority.contains(':'),
        };
        let addr = match has_port {
            true => authority.to_string(),
            false => format!("{authority}:80"),
        };
        Ok(Upstream {
            addr,
            host: authority.to_string(),
            path: path.to_string(),
            token,
        })
    }
}

/// What the bridge reads of a message to send it: its id, its method, the
/// tool it calls and the revision it names.
struct Seen {
    id: Option<Value>,
    method: Option<String>,
    name: Option<String>,
    version: Option<String>,
}

fn seen(text: &str) -> Seen {
    let mut s = Seen {
        id: None,
        method: None,
        name: None,
        version: None,
    };
    let Ok(Value::Object(m)) = json::parse_json(text.trim()) else {
        return s;
    };
    let get =
        |m: &[(String, Value)], k: &str| m.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    s.id = get(&m, "id").filter(|v| matches!(v, Value::Int(_) | Value::Text(_)));
    if let Some(Value::Text(t)) = get(&m, "method") {
        s.method = Some(t);
    }
    if let Some(Value::Object(p)) = get(&m, "params") {
        if let Some(Value::Text(n)) = get(&p, "name") {
            s.name = Some(n);
        }
        if let Some(Value::Object(meta)) = get(&p, "_meta") {
            if let Some(Value::Text(v)) = get(&meta, "io.modelcontextprotocol/protocolVersion") {
                s.version = Some(v);
            }
        }
    }
    s
}

/// A header value as the binding sends one that is not plain ASCII, or
/// that could be taken for an encoded one: `=?base64?...?=`.
fn header_value(v: &str) -> String {
    let plain = v.bytes().all(|b| (0x20..0x7f).contains(&b))
        && v.trim() == v
        && !(v.starts_with("=?base64?") && v.ends_with("?="));
    match plain {
        true => v.to_string(),
        false => format!("=?base64?{}?=", crate::crypto::b64_encode(v.as_bytes())),
    }
}

/// An error of the bridge's own, answering the message `id` names.
fn own_error(id: Option<&Value>, why: &str) -> String {
    let mut out = String::from("{\"jsonrpc\":\"2.0\"");
    if let Some(id) = id {
        out.push_str(",\"id\":");
        json::value_into(&mut out, id);
    }
    out.push_str(",\"error\":{\"code\":-32603,\"message\":");
    json::escape_into(&mut out, &format!("fenec mcp: {why}"));
    out.push_str("}}");
    out
}

/// Posts each line of `input` to `up` and writes each answer to `output`,
/// until the input ends.
pub fn bridge(up: &Upstream, mut input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    let mut conn: Option<BufReader<TcpStream>> = None;
    // The revision an `initialize` agreed on, which a handshake client's
    // later requests carry as a header the bridge adds for it.
    let mut negotiated: Option<String> = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let msg = seen(text);
        // A cancel is stdio's alone: over HTTP closing the stream cancels,
        // and the call it names has been answered by the time it is read.
        if msg.method.as_deref() == Some("notifications/cancelled") {
            continue;
        }
        let mut headers: Vec<(&str, String)> = Vec::new();
        if let Some(v) = msg.version.clone().or_else(|| negotiated.clone()) {
            headers.push(("MCP-Protocol-Version", v));
        }
        if let Some(m) = &msg.method {
            headers.push(("Mcp-Method", header_value(m)));
            if m == "tools/call" {
                if let Some(n) = &msg.name {
                    headers.push(("Mcp-Name", header_value(n)));
                }
            }
        }
        let answer = match post(&mut conn, up, &headers, text.as_bytes()) {
            Err(e) => match msg.id.is_some() {
                true => Some(own_error(
                    msg.id.as_ref(),
                    &format!("could not reach {}{}: {e}", up.host, up.path),
                )),
                false => None,
            },
            Ok((202, _)) => None,
            Ok((status, body)) => {
                let body = String::from_utf8_lossy(&body).trim().to_string();
                match json::parse_json(&body) {
                    Ok(Value::Object(m)) if m.iter().any(|(k, _)| k == "jsonrpc") => {
                        if msg.method.as_deref() == Some("initialize") {
                            negotiated = agreed(&m);
                        }
                        Some(body)
                    }
                    // `{"error": ...}`: the server's own refusal, a token's
                    // 401 or a path that is not an endpoint.
                    other => msg.id.as_ref().map(|id| {
                        let why = match other {
                            Ok(Value::Object(m)) => m
                                .iter()
                                .find(|(k, _)| k == "error")
                                .and_then(|(_, v)| match v {
                                    Value::Text(t) => Some(t.clone()),
                                    _ => None,
                                })
                                .unwrap_or(body),
                            _ => body,
                        };
                        let hint = match status {
                            401 => " (the token is FENEC_TOKEN's)",
                            404 => " (is the server started with --mcp?)",
                            _ => "",
                        };
                        own_error(
                            Some(id),
                            &format!("the server answered {status}: {why}{hint}"),
                        )
                    }),
                }
            }
        };
        if let Some(a) = answer {
            output.write_all(a.as_bytes())?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
}

/// The revision an `initialize` result names.
fn agreed(m: &[(String, Value)]) -> Option<String> {
    let result = m.iter().find(|(k, _)| k == "result")?;
    let Value::Object(r) = &result.1 else {
        return None;
    };
    match r.iter().find(|(k, _)| k == "protocolVersion") {
        Some((_, Value::Text(v))) => Some(v.clone()),
        _ => None,
    }
}

fn connect(addr: &str) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{addr} does not resolve"));
    for a in addr.to_socket_addrs()? {
        match TcpStream::connect_timeout(&a, Duration::from_secs(5)) {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Whether a kept connection was closed by the server meanwhile -- its idle
/// timeout -- so the message goes on a new one rather than be sent into a
/// closed one and sent again: a write sent twice is two rows.
fn closed(conn: &BufReader<TcpStream>) -> bool {
    if !conn.buffer().is_empty() {
        return true;
    }
    let s = conn.get_ref();
    if s.set_nonblocking(true).is_err() {
        return true;
    }
    let gone =
        !matches!(s.peek(&mut [0u8; 1]), Err(ref e) if e.kind() == io::ErrorKind::WouldBlock);
    let _ = s.set_nonblocking(false);
    gone
}

fn post(
    conn: &mut Option<BufReader<TcpStream>>,
    up: &Upstream,
    headers: &[(&str, String)],
    body: &[u8],
) -> io::Result<(u16, Vec<u8>)> {
    if conn.as_ref().is_some_and(closed) {
        *conn = None;
    }
    if conn.is_none() {
        *conn = Some(BufReader::new(connect(&up.addr)?));
    }
    let r = conn.as_mut().expect("a connection");
    let mut head = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n",
        up.path,
        up.host,
        body.len()
    );
    if let Some(t) = &up.token {
        head.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let answer = (|| {
        let s = r.get_mut();
        s.write_all(head.as_bytes())?;
        s.write_all(body)?;
        s.flush()?;
        response(r)
    })();
    match answer {
        Ok((status, body, keep)) => {
            if !keep {
                *conn = None;
            }
            Ok((status, body))
        }
        Err(e) => {
            *conn = None;
            Err(e)
        }
    }
}

/// An HTTP answer: its status, its body -- by `Content-Length`, chunked, or
/// to the end -- and whether the connection stays open.
fn response(r: &mut BufReader<TcpStream>) -> io::Result<(u16, Vec<u8>, bool)> {
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::other(format!("not an HTTP answer: {:?}", line.trim())))?;
    let (mut length, mut chunked, mut keep, mut stream) = (None, false, true, false);
    let mut total = 0;
    loop {
        line.clear();
        let n = r.read_line(&mut line)?;
        total += n;
        if n == 0 || total > crate::http::MAX_HEADER_BYTES {
            return Err(io::Error::other("the answer's head did not end"));
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("content-length") {
            length = v.parse::<usize>().ok();
        } else if k.eq_ignore_ascii_case("transfer-encoding") {
            chunked = v.eq_ignore_ascii_case("chunked");
        } else if k.eq_ignore_ascii_case("connection") {
            keep = !v.eq_ignore_ascii_case("close");
        } else if k.eq_ignore_ascii_case("content-type") {
            stream = v.starts_with("text/event-stream");
        }
    }
    if stream {
        return Err(io::Error::other(
            "the server answered with an event stream, which this bridge does not read",
        ));
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            line.clear();
            r.read_line(&mut line)?;
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| io::Error::other("a chunk's size is not a number"))?;
            if size == 0 {
                line.clear();
                r.read_line(&mut line)?;
                break;
            }
            let at = body.len();
            body.resize(at + size, 0);
            r.read_exact(&mut body[at..])?;
            line.clear();
            r.read_line(&mut line)?;
        }
    } else if let Some(n) = length {
        body.resize(n, 0);
        r.read_exact(&mut body)?;
    } else {
        r.read_to_end(&mut body)?;
        keep = false;
    }
    Ok((status, body, keep))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_endpoint_url_is_read_into_where_to_post() {
        let up = Upstream::parse("http://127.0.0.1:8080/t/acme/_mcp", None).unwrap();
        assert_eq!(
            (up.addr.as_str(), up.host.as_str(), up.path.as_str()),
            ("127.0.0.1:8080", "127.0.0.1:8080", "/t/acme/_mcp")
        );
        let up = Upstream::parse("http://localhost", None).unwrap();
        assert_eq!(
            (up.addr.as_str(), up.path.as_str()),
            ("localhost:80", "/_mcp")
        );
        let up = Upstream::parse("http://[::1]:9000/_mcp", None).unwrap();
        assert_eq!(up.addr, "[::1]:9000");
        assert!(Upstream::parse("https://db.example/_mcp", None)
            .unwrap_err()
            .contains("TLS"));
        assert!(Upstream::parse("db.example/_mcp", None).is_err());
    }

    #[test]
    fn a_header_value_outside_plain_ascii_goes_in_base64() {
        assert_eq!(header_value("search"), "search");
        assert_eq!(header_value(" padded "), "=?base64?IHBhZGRlZCA=?=");
        assert_eq!(
            header_value("Hello, 世界"),
            "=?base64?SGVsbG8sIOS4lueVjA==?="
        );
        assert_eq!(
            header_value("=?base64?literal?="),
            "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
        );
    }
}
