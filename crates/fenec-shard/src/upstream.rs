//! The router's side of the conversation with a node: just enough HTTP/1.1
//! client to forward a request and read the answer.
//!
//! The same scope as the server-side parser: a request with
//! `Content-Length`, a response that is either `Content-Length` delimited
//! or read to the close (an SSE stream). Nodes never send a chunked body,
//! so there is none to decode.
//!
//! Connections are kept per node address and reused, and a node closes one
//! silent past its `--idle-timeout`. So a pooled connection is looked at
//! before it is used ([`open`]: a read that does not wait) and let go of
//! when the node has closed or reset it. Before the look, macOS answered a
//! request after a minute of quiet 502: some 60 s after the node closed the
//! connection (`net.inet.tcp.fin_timeout`) it is reset, and `setsockopt`
//! there refuses a socket that can neither send nor receive with EINVAL,
//! which `set_read_timeout` returned before a byte was written -- an error
//! the retry, which knew EOF, a broken pipe and a reset, did not know.
//!
//! The look cannot see a close on its way: the node's idle timeout firing
//! as the request goes out. What may be sent again on a fresh connection
//! then turns on how far the exchange got ([`Stage`]), never on the error's
//! kind, which differs by system. Failing before the request's last byte
//! was handed to the system, the node never read a whole request and ran
//! nothing: it is sent again, whatever it is. Failing after it, the
//! connection ending before the first byte of an answer, the node may have
//! closed it as the request came in or run the request and gone down: it is
//! sent again only where running it twice is harmless -- a `GET`, `HEAD` or
//! `OPTIONS`, which a node never writes for, or a write under an
//! `Idempotency-Key`, which it makes once. Anything else is the client's
//! 502: retried here, a `put` that had landed was made twice.

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
    idle: Mutex<HashMap<String, Vec<Idle>>>,
    timeout: Duration,
}

/// A pooled connection and the read timeout it was left with, so that a
/// request waiting as long sets none: the two `setsockopt`s every exchange
/// made took 0.67 us, more than the look ([`open`]) made in their place.
struct Idle {
    stream: TcpStream,
    read_timeout: Duration,
}

pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    reader: BufReader<TcpStream>,
    addr: String,
    reusable: bool,
    read_timeout: Duration,
}

/// How far an exchange got before it failed, which is what decides whether
/// the request may be sent again (the module's comment).
enum Stage {
    /// Before the request's last byte was handed to the system: the node
    /// cannot have read a whole request, so it ran nothing.
    Unsent,
    /// The request went out whole and the connection ended before the
    /// first byte of an answer: the node may have run it.
    Unanswered,
    /// A timeout, or an answer cut short or malformed.
    Other,
}

struct Failure(Stage, io::Error);

impl Pool {
    /// `timeout` bounds connecting and every read of an ordinary answer.
    pub fn new(timeout: Duration) -> Pool {
        Pool {
            idle: Mutex::new(HashMap::new()),
            timeout,
        }
    }

    fn take(&self, addr: &str) -> Option<Idle> {
        self.idle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(addr)
            .and_then(|v| v.pop())
    }

    fn put(&self, addr: &str, s: Idle) {
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
                    // Once a connection: it is the same for every request.
                    s.set_write_timeout(Some(self.timeout))?;
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
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> io::Result<Answer> {
        self.send_held(addr, method, target, headers, body, Duration::ZERO)
    }

    /// [`Pool::send`] of a request the node may hold for `held` before it
    /// answers (`Fenec-Wait`): the answer is waited for that much longer.
    /// Cut at the timeout alone, a claim held longer was answered 502 here
    /// while the node went on to take a job for it, leased to no one.
    pub fn send_held(
        &self,
        addr: &str,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        held: Duration,
    ) -> io::Result<Answer> {
        let mut req = String::with_capacity(256);
        for part in [method, " ", target, " HTTP/1.1\r\nHost: ", addr, "\r\n"] {
            req.push_str(part);
        }
        for (k, v) in headers {
            for part in [k, ": ", v, "\r\n"] {
                req.push_str(part);
            }
        }
        // The id of the request this thread serves goes on to the node, which
        // takes it as its own: one request through the router, one id in
        // both logs. A forwarded request's own header is left out by
        // `forward`, so it is never sent twice.
        fenec_http::request_id::with(|id| {
            if !id.is_empty() {
                for part in [fenec_http::request_id::HEADER, ": ", id, "\r\n"] {
                    req.push_str(part);
                }
            }
        });
        // The trace this thread's request is in goes on as the id does,
        // its parent the span open now -- `forward`'s -- so the node's spans
        // hang under the router's. A forwarded request's own `traceparent`
        // is left out by `forward` while the router traces, and passed on
        // as it came while it does not.
        fenec_http::trace::outgoing(|parent, state| {
            for part in [fenec_http::trace::TRACEPARENT, ": ", parent, "\r\n"] {
                req.push_str(part);
            }
            if let Some(state) = state {
                for part in [fenec_http::trace::TRACESTATE, ": ", state, "\r\n"] {
                    req.push_str(part);
                }
            }
        });
        req.push_str(&format!(
            "Content-Length: {}\r\nConnection: keep-alive\r\n\r\n",
            body.len()
        ));
        let head = req.as_bytes();

        // The node closes its idle connections together, so a closed one
        // found here is let go of and the next one looked at.
        while let Some(idle) = self.take(addr) {
            if !open(&idle.stream) {
                continue;
            }
            match self.exchange(addr, idle.stream, Some(idle.read_timeout), head, body, held) {
                Ok(a) => return Ok(a),
                Err(Failure(Stage::Unsent, _)) => break,
                Err(Failure(Stage::Unanswered, _)) if replayable(method, headers) => break,
                Err(Failure(_, e)) => return Err(e),
            }
        }
        let s = self.connect(addr)?;
        self.exchange(addr, s, None, head, body, held)
            .map_err(|Failure(_, e)| e)
    }

    /// One request and the head of its answer over `s`, whose read timeout
    /// is `set` where it is known.
    fn exchange(
        &self,
        addr: &str,
        s: TcpStream,
        set: Option<Duration>,
        head: &[u8],
        body: &[u8],
        held: Duration,
    ) -> Result<Answer, Failure> {
        let read_timeout = self.timeout + held;
        if set != Some(read_timeout) {
            s.set_read_timeout(Some(read_timeout))
                .map_err(|e| Failure(Stage::Unsent, e))?;
        }
        let mut w = &s;
        // A `write` that fails took none of its bytes, so the request's
        // last byte never went out: an empty body writes nothing, and the
        // head's last byte is then the last.
        w.write_all(head)
            .and_then(|_| w.write_all(body))
            .map_err(|e| {
                // Timed out, the node is there and not reading: sent again,
                // it would wait as long.
                let stage = if timed_out(&e) {
                    Stage::Other
                } else {
                    Stage::Unsent
                };
                Failure(stage, e)
            })?;

        let mut reader = BufReader::new(s);
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                return Err(Failure(
                    Stage::Unanswered,
                    io::ErrorKind::UnexpectedEof.into(),
                ))
            }
            Ok(_) => {}
            Err(e) if line.is_empty() && !timed_out(&e) => {
                return Err(Failure(Stage::Unanswered, e))
            }
            Err(e) => return Err(Failure(Stage::Other, e)),
        }
        let other = |e| Failure(Stage::Other, e);
        let status = line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| bad("malformed status line"))
            .map_err(other)?;
        let mut headers = Vec::new();
        let mut total = line.len();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).map_err(other)?;
            total += n;
            if n == 0 {
                return Err(other(bad("truncated response head")));
            }
            if total > MAX_HEAD {
                return Err(other(bad("response head too large")));
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
            read_timeout,
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
        let bearer = format!("Bearer {token}");
        let headers = [("Authorization", bearer.as_str())];
        let a = self.send(addr, method, target, &headers, body)?;
        let status = a.status;
        let body = a.read_body(self, method == "HEAD")?;
        Ok((status, body))
    }
}

/// Whether a request whose connection ended before its answer may be sent
/// again: one a node never writes for, or a write the node makes once
/// under its key, answering a second request with the first one's answer.
fn replayable(method: &str, headers: &[(&str, &str)]) -> bool {
    matches!(method, "GET" | "HEAD" | "OPTIONS")
        || headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("idempotency-key"))
}

fn timed_out(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Whether a pooled connection is still open: a read that does not wait
/// finds nothing to read. One the node closed reads its end, one it reset
/// an error, and one holding bytes no request asked for is no use either.
fn open(s: &TcpStream) -> bool {
    matches!(peek_now(s), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
}

/// A `recv` of `MSG_PEEK | MSG_DONTWAIT`: one system call, where
/// `set_nonblocking`, `peek` and `set_nonblocking` again are three (0.31
/// us against 0.74 on an M1). The crate takes no `libc`.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn peek_now(s: &TcpStream) -> io::Result<usize> {
    use std::os::fd::AsRawFd;
    extern "C" {
        fn recv(fd: i32, buf: *mut u8, len: usize, flags: i32) -> isize;
    }
    const MSG_PEEK: i32 = 2;
    #[cfg(target_vendor = "apple")]
    const MSG_DONTWAIT: i32 = 0x80;
    #[cfg(not(target_vendor = "apple"))]
    const MSG_DONTWAIT: i32 = 0x40;
    let mut byte = 0u8;
    // SAFETY: one byte, into a byte that outlives the call.
    let n = unsafe { recv(s.as_raw_fd(), &mut byte, 1, MSG_PEEK | MSG_DONTWAIT) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn peek_now(s: &TcpStream) -> io::Result<usize> {
    s.set_nonblocking(true)?;
    let seen = s.peek(&mut [0u8]);
    s.set_nonblocking(false)?;
    seen
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
            let idle = Idle {
                stream: self.reader.into_inner(),
                read_timeout: self.read_timeout,
            };
            pool.put(&self.addr, idle);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A connection, and the node's end of it.
    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let ours = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (theirs, _) = l.accept().unwrap();
        (ours, theirs)
    }

    /// Waits until a read that does not wait on `s` stops finding nothing:
    /// what the node's end did has arrived.
    fn arrived(s: &TcpStream) -> io::Result<usize> {
        let until = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match peek_now(s) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < until, "nothing arrived");
                    std::thread::yield_now();
                }
                seen => return seen,
            }
        }
    }

    #[test]
    fn the_look_tells_an_open_connection_from_a_closed_or_reset_one() {
        let (ours, theirs) = pair();
        assert!(open(&ours));
        drop(theirs);
        assert_eq!(arrived(&ours).unwrap(), 0);
        assert!(!open(&ours));

        // Closed with a byte it never read, the node's end resets.
        let (mut ours, theirs) = pair();
        ours.write_all(b"x").unwrap();
        // The byte is there before the close, or the close would be a FIN.
        theirs
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        assert_eq!(theirs.peek(&mut [0u8]).unwrap(), 1);
        drop(theirs);
        let reset = arrived(&ours).unwrap_err();
        assert_eq!(reset.kind(), io::ErrorKind::ConnectionReset, "{reset}");
        assert!(!open(&ours));

        // Bytes no request asked for.
        let (ours, mut theirs) = pair();
        theirs.write_all(b"HTTP/1.1 200 OK\r\n").unwrap();
        assert_eq!(arrived(&ours).unwrap(), 1);
        assert!(!open(&ours));
    }
}
