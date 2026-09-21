//! The router's side of the conversation with a node: just enough HTTP/1.1
//! client to forward a request and read the answer.
//!
//! The same scope as the server-side parser: a request with
//! `Content-Length`, a response that is either `Content-Length` delimited
//! or read to the close (an SSE stream). Nodes never send a chunked body,
//! so there is none to decode.
//!
//! Connections are kept per node address and reused. A reused connection
//! may have been closed by the node's idle timeout in the meantime; the
//! first read then sees EOF before a single byte of response, which means
//! the node never saw the request either -- that one case is retried on a
//! fresh connection, and nothing else is.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::Duration;

/// Response header ceiling, same as the server side's.
const MAX_HEAD: usize = fenec_http::http::MAX_HEADER_BYTES;

/// Idle connections kept per node. Beyond it a returned connection is
/// closed: the pool bounds descriptors, the router's thread count bounds
/// concurrency.
const MAX_IDLE: usize = 32;

pub struct Pool {
    idle: Mutex<HashMap<String, Vec<TcpStream>>>,
    timeout: Duration,
}

pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    reader: BufReader<TcpStream>,
    addr: String,
    reusable: bool,
}

impl Pool {
    /// `timeout` bounds connecting and every read of an ordinary answer.
    pub fn new(timeout: Duration) -> Pool {
        Pool {
            idle: Mutex::new(HashMap::new()),
            timeout,
        }
    }

    fn take(&self, addr: &str) -> Option<TcpStream> {
        self.idle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(addr)
            .and_then(|v| v.pop())
    }

    fn put(&self, addr: &str, s: TcpStream) {
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        let list = idle.entry(addr.to_string()).or_default();
        if list.len() < MAX_IDLE {
            list.push(s);
        }
    }

    fn connect(&self, addr: &str) -> io::Result<TcpStream> {
        use std::net::ToSocketAddrs;
        let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{addr} does not resolve"));
        for a in addr.to_socket_addrs()? {
            match TcpStream::connect_timeout(&a, self.timeout) {
                Ok(s) => {
                    let _ = s.set_nodelay(true);
                    return Ok(s);
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Sends a request and reads the response head. `headers` must not
    /// carry `Content-Length` or `Connection`; both are written here.
    pub fn send(
        &self,
        addr: &str,
        method: &str,
        target: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> io::Result<Answer> {
        let mut req = format!("{method} {target} HTTP/1.1\r\nHost: {addr}\r\n");
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str(&format!(
            "Content-Length: {}\r\nConnection: keep-alive\r\n\r\n",
            body.len()
        ));

        if let Some(s) = self.take(addr) {
            match self.exchange(addr, s, req.as_bytes(), body) {
                Ok(a) => return Ok(a),
                // Closed while idle: the request never reached the node.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {}
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
                Err(e) => return Err(e),
            }
        }
        let s = self.connect(addr)?;
        self.exchange(addr, s, req.as_bytes(), body)
    }

    fn exchange(&self, addr: &str, s: TcpStream, head: &[u8], body: &[u8]) -> io::Result<Answer> {
        s.set_read_timeout(Some(self.timeout))?;
        s.set_write_timeout(Some(self.timeout))?;
        let mut w = &s;
        w.write_all(head)?;
        w.write_all(body)?;
        w.flush()?;

        let mut reader = BufReader::new(s);
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let status = line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| bad("malformed status line"))?;
        let mut headers = Vec::new();
        let mut total = line.len();
        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            total += n;
            if n == 0 {
                return Err(bad("truncated response head"));
            }
            if total > MAX_HEAD {
                return Err(bad("response head too large"));
            }
            let l = line.trim_end_matches(['\r', '\n']);
            if l.is_empty() {
                break;
            }
            if let Some((k, v)) = l.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        let reusable = !headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close"));
        Ok(Answer {
            status,
            headers,
            reader,
            addr: addr.to_string(),
            reusable,
        })
    }

    /// A whole request and its `Content-Length` body: the router's own
    /// calls to a node's `/_admin/`.
    pub fn call(
        &self,
        addr: &str,
        method: &str,
        target: &str,
        token: &str,
        body: &[u8],
    ) -> io::Result<(u16, Vec<u8>)> {
        let headers = [("Authorization".to_string(), format!("Bearer {token}"))];
        let a = self.send(addr, method, target, &headers, body)?;
        let status = a.status;
        let body = a.read_body(self, method == "HEAD")?;
        Ok((status, body))
    }
}

impl Answer {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// `None` means the body runs to the close: a stream.
    pub fn content_length(&self) -> Option<usize> {
        self.header("content-length")?.parse().ok()
    }

    /// Reads a `Content-Length` body and hands the connection back to the
    /// pool when the node keeps it open.
    pub fn read_body(mut self, pool: &Pool, head_only: bool) -> io::Result<Vec<u8>> {
        let Some(len) = self.content_length() else {
            let mut body = Vec::new();
            self.reader.read_to_end(&mut body)?;
            return Ok(body);
        };
        let len = if head_only { 0 } else { len };
        let mut body = vec![0u8; len];
        self.reader.read_exact(&mut body)?;
        // Bytes left in the buffer would be the start of an answer nobody
        // asked for; such a connection is not reused.
        if self.reusable && self.reader.buffer().is_empty() {
            pool.put(&self.addr, self.reader.into_inner());
        }
        Ok(body)
    }

    /// The rest of the answer as a byte stream, with no read timeout: an SSE
    /// stream is silent until something changes (the node's keep-alive
    /// line bounds the silence).
    pub fn into_stream(self) -> io::Result<BufReader<TcpStream>> {
        self.reader.get_ref().set_read_timeout(None)?;
        Ok(self.reader)
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}
