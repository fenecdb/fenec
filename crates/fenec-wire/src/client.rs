//! PostgreSQL v3 wire protocol -- client side.
//!
//! The `server` module makes fenecdb look like PostgreSQL; this module does
//! the opposite and connects to a real PostgreSQL server. The subset needed
//! for import: connect, authenticate, simple query and `COPY ... TO STDOUT`
//! -- and for `--follow`, a logical replication stream ([`WalStream`]).
//!
//! Framing comes from [`crate::proto`], crypto from [`crate::crypto`]. The
//! server half of SCRAM is in fenec-pg's `scram`; this is the client half.
//!
//! No TLS is spoken. The password is protected by SCRAM but the data flows
//! in plain text; on an open network it has to go behind a tunnel.

use crate::crypto::{b64_decode, b64_encode, hmac_sha256, nonce, pbkdf2_sha256, sha256};
use crate::proto::{put_cstr, read_message, Message, Writer, PROTOCOL_V3};
use fenec_core::error::{Error, Result};
use std::io::{BufReader, ErrorKind, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The timeout used while connecting and while waiting for a message.
const TIMEOUT: Duration = Duration::from_secs(30);

fn io(e: impl std::fmt::Display) -> Error {
    Error::Io(format!("postgres: {e}"))
}

// -------------------------------------------------------------------- url

/// `postgres://user[:password]@host[:port]/database`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub user: String,
    pub password: Option<String>,
    pub host: String,
    pub port: u16,
    pub database: String,
}

impl Url {
    /// Parses the connection string. Without a password `PGPASSWORD` is
    /// used, and without a user `PGUSER` or `USER`.
    pub fn parse(s: &str) -> Result<Url> {
        let rest = s
            .strip_prefix("postgresql://")
            .or_else(|| s.strip_prefix("postgres://"))
            .ok_or_else(|| {
                Error::Query(format!(
                    "`{s}` is not a PostgreSQL connection string \
                     (expected postgres://user@host:port/database)"
                ))
            })?;
        // Query parameters are ignored; TLS and similar options are not
        // supported and must not be swallowed silently.
        let (rest, params) = match rest.split_once('?') {
            Some((r, p)) => (r, Some(p)),
            None => (rest, None),
        };
        if let Some(p) = params {
            if !p.is_empty() {
                return Err(Error::Query(format!(
                    "connection parameters are not supported: `{p}`"
                )));
            }
        }
        // The user info runs up to the last `@`; the host contains no `@`.
        let (userinfo, authority) = match rest.rsplit_once('@') {
            Some((u, a)) => (Some(u), a),
            None => (None, rest),
        };
        let (hostport, database) = match authority.split_once('/') {
            Some((h, d)) => (h, d),
            None => (authority, ""),
        };
        if database.is_empty() {
            return Err(Error::Query(
                "the connection string has no database name (`.../database`)".into(),
            ));
        }
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (
                h,
                p.parse::<u16>()
                    .map_err(|_| Error::Query(format!("invalid port `{p}`")))?,
            ),
            None => (hostport, 5432),
        };

        let (user, password) = match userinfo {
            Some(u) => match u.split_once(':') {
                Some((u, p)) => (decode(u), Some(decode(p))),
                None => (decode(u), None),
            },
            None => (String::new(), None),
        };
        let user = if user.is_empty() {
            std::env::var("PGUSER")
                .or_else(|_| std::env::var("USER"))
                .map_err(|_| Error::Query("the connection string has no user name".into()))?
        } else {
            user
        };
        Ok(Url {
            user,
            password: password.or_else(|| std::env::var("PGPASSWORD").ok()),
            host: if host.is_empty() { "127.0.0.1" } else { host }.to_string(),
            port,
            database: decode(database),
        })
    }
}

/// Decodes percent escapes; this is how `@` and `:` travel in passwords.
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ----------------------------------------------------------------- messages

/// A single column inside `RowDescription`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDesc {
    pub name: String,
    /// The type OID. Extension types (pgvector) are not fixed and are
    /// learned from `pg_type`.
    pub oid: i32,
    /// The type modifier. In pgvector it is the vector dimension; -1 when unknown.
    pub typmod: i32,
}

/// A simple query result; the values always arrive in text format.
#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    pub columns: Vec<FieldDesc>,
    pub rows: Vec<Vec<Option<String>>>,
}

/// One field from an `ErrorResponse` body.
fn field(m: &Message, key: u8) -> Option<String> {
    let mut pos = 0;
    while pos < m.body.len() && m.body[pos] != 0 {
        let k = m.body[pos];
        pos += 1;
        let start = pos;
        while pos < m.body.len() && m.body[pos] != 0 {
            pos += 1;
        }
        if k == key {
            return Some(String::from_utf8_lossy(&m.body[start..pos]).into_owned());
        }
        pos += 1;
    }
    None
}

fn server_error(m: &Message) -> Error {
    let code = field(m, b'C').unwrap_or_default();
    let msg = field(m, b'M').unwrap_or_default();
    Error::Query(format!("postgres {code}: {msg}"))
}

fn be_i16(b: &[u8], at: usize) -> i16 {
    i16::from_be_bytes([b[at], b[at + 1]])
}

fn be_i32(b: &[u8], at: usize) -> i32 {
    i32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// `RowDescription` -> columns.
///
/// For each column: name, table oid (4), column number (2), type oid (4),
/// type length (2), type modifier (4), format (2) -- 18 bytes after the name.
fn row_description(m: &Message) -> Result<Vec<FieldDesc>> {
    let short = || Error::Corrupt("postgres: RowDescription too short".into());
    if m.body.len() < 2 {
        return Err(short());
    }
    let n = be_i16(&m.body, 0) as usize;
    let mut pos = 2;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let start = pos;
        while pos < m.body.len() && m.body[pos] != 0 {
            pos += 1;
        }
        let name = String::from_utf8_lossy(&m.body[start..pos]).into_owned();
        pos += 1;
        if pos + 18 > m.body.len() {
            return Err(short());
        }
        out.push(FieldDesc {
            name,
            oid: be_i32(&m.body, pos + 6),
            typmod: be_i32(&m.body, pos + 12),
        });
        pos += 18;
    }
    Ok(out)
}

/// `DataRow` -> cells.
fn data_row(m: &Message) -> Result<Vec<Option<String>>> {
    let short = || Error::Corrupt("postgres: DataRow too short".into());
    if m.body.len() < 2 {
        return Err(short());
    }
    let n = be_i16(&m.body, 0) as usize;
    let mut pos = 2;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        if pos + 4 > m.body.len() {
            return Err(short());
        }
        let len = be_i32(&m.body, pos);
        pos += 4;
        if len < 0 {
            out.push(None);
            continue;
        }
        let end = pos + len as usize;
        let cell = m.body.get(pos..end).ok_or_else(short)?;
        out.push(Some(String::from_utf8_lossy(cell).into_owned()));
        pos = end;
    }
    Ok(out)
}

// ------------------------------------------------------------------ client

/// A connection to a live PostgreSQL server.
#[derive(Debug)]
pub struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    /// The version the server reports; useful in error text.
    pub server_version: String,
}

impl Client {
    /// Connects and completes authentication.
    pub fn connect(url: &Url) -> Result<Client> {
        Client::open(url, false)
    }

    /// A connection in logical replication mode (`replication=database`):
    /// it takes [`Client::start_replication`] besides ordinary SQL.
    pub fn connect_replication(url: &Url) -> Result<Client> {
        Client::open(url, true)
    }

    fn open(url: &Url, replication: bool) -> Result<Client> {
        let stream = TcpStream::connect((url.host.as_str(), url.port))
            .map_err(|e| io(format!("{}:{} -- {e}", url.host, url.port)))?;
        stream.set_read_timeout(Some(TIMEOUT)).ok();
        stream.set_nodelay(true).ok();
        let reader = BufReader::new(stream.try_clone().map_err(io)?);
        let mut c = Client {
            reader,
            writer: stream,
            server_version: String::new(),
        };

        // A StartupMessage has no tag: length + version + key/value pairs.
        let mut body = PROTOCOL_V3.to_be_bytes().to_vec();
        put_cstr(&mut body, "user");
        put_cstr(&mut body, &url.user);
        put_cstr(&mut body, "database");
        put_cstr(&mut body, &url.database);
        put_cstr(&mut body, "application_name");
        put_cstr(
            &mut body,
            if replication {
                "fenec-follow"
            } else {
                "fenec-import"
            },
        );
        if replication {
            put_cstr(&mut body, "replication");
            put_cstr(&mut body, "database");
        }
        body.push(0);
        let mut packet = ((body.len() + 4) as i32).to_be_bytes().to_vec();
        packet.extend_from_slice(&body);
        c.writer.write_all(&packet).map_err(io)?;
        c.writer.flush().map_err(io)?;

        loop {
            let m = c.read()?;
            match m.tag {
                b'R' => {
                    if m.body.len() < 4 {
                        return Err(Error::Corrupt("postgres: short auth message".into()));
                    }
                    match be_i32(&m.body, 0) {
                        // AuthenticationOk / SASLContinue / SASLFinal:
                        // the second and third are consumed inside scram().
                        0 | 11 | 12 => {}
                        3 => {
                            let pw = url.password.clone().unwrap_or_default();
                            let mut w = Writer::new();
                            w.msg(b'p', |b| put_cstr(b, &pw));
                            w.flush_to(&mut c.writer).map_err(io)?;
                        }
                        10 => {
                            let pw = url.password.as_deref().ok_or_else(|| {
                                Error::Query("the server wants a password; add it to the connection string or set PGPASSWORD".into())
                            })?;
                            c.scram(pw)?;
                        }
                        5 => {
                            return Err(Error::Query(
                                "the server wants md5 authentication; that is not supported. \
                                 Switch the server to scram-sha-256 or temporarily use \
                                 cleartext/trust"
                                    .into(),
                            ))
                        }
                        other => {
                            return Err(Error::Query(format!(
                                "unsupported authentication method (code {other})"
                            )))
                        }
                    }
                }
                b'S' => {
                    let mut pos = 0;
                    let k = crate::proto::take_cstr(&m.body, &mut pos);
                    let v = crate::proto::take_cstr(&m.body, &mut pos);
                    if k == "server_version" {
                        c.server_version = v;
                    }
                }
                b'K' => {}
                b'E' => return Err(server_error(&m)),
                b'Z' => return Ok(c),
                other => {
                    return Err(Error::Corrupt(format!(
                        "postgres: unexpected message `{}` while connecting",
                        other as char
                    )))
                }
            }
        }
    }

    /// The SCRAM-SHA-256 client half (RFC 5802 + RFC 7677).
    ///
    /// The password is never sent over the wire; only nonces and HMAC proofs
    /// travel. The server signature is verified too, which proves the other
    /// side really knows the password.
    fn scram(&mut self, password: &str) -> Result<()> {
        let cnonce = nonce(24);
        let client_first_bare = format!("n=,r={cnonce}");
        let initial = format!("n,,{client_first_bare}");
        let mut w = Writer::new();
        w.msg(b'p', |b| {
            put_cstr(b, "SCRAM-SHA-256");
            b.extend_from_slice(&(initial.len() as i32).to_be_bytes());
            b.extend_from_slice(initial.as_bytes());
        });
        w.flush_to(&mut self.writer).map_err(io)?;

        let m = self.read()?;
        if m.tag == b'E' {
            return Err(server_error(&m));
        }
        let bad = || Error::Corrupt("postgres: the SCRAM response could not be parsed".into());
        let server_first = String::from_utf8_lossy(m.body.get(4..).ok_or_else(bad)?).into_owned();
        let (mut snonce, mut salt, mut iters) = (String::new(), Vec::new(), 0u32);
        for kv in server_first.split(',') {
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            match k {
                "r" => snonce = v.to_string(),
                "s" => salt = b64_decode(v).ok_or_else(bad)?,
                "i" => iters = v.parse().map_err(|_| bad())?,
                _ => {}
            }
        }
        // The server has to carry our nonce as a prefix; otherwise the
        // response belongs to another session.
        if !snonce.starts_with(&cnonce) || salt.is_empty() || iters == 0 {
            return Err(Error::Query(
                "postgres: invalid SCRAM server response".into(),
            ));
        }

        // `c=biws`, base64("n,,") -- no channel binding in use.
        let without_proof = format!("c=biws,r={snonce}");
        let auth = format!("{client_first_bare},{server_first},{without_proof}");
        let salted = pbkdf2_sha256(password.as_bytes(), &salt, iters);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let sig = hmac_sha256(&sha256(&client_key), auth.as_bytes());
        let proof: Vec<u8> = client_key.iter().zip(sig).map(|(a, b)| a ^ b).collect();
        let final_msg = format!("{without_proof},p={}", b64_encode(&proof));
        let mut w = Writer::new();
        w.msg(b'p', |b| b.extend_from_slice(final_msg.as_bytes()));
        w.flush_to(&mut self.writer).map_err(io)?;

        let m = self.read()?;
        if m.tag == b'E' {
            return Err(server_error(&m));
        }
        let got = String::from_utf8_lossy(m.body.get(4..).ok_or_else(bad)?).into_owned();
        let server_key = hmac_sha256(&salted, b"Server Key");
        let want = format!(
            "v={}",
            b64_encode(&hmac_sha256(&server_key, auth.as_bytes()))
        );
        if got != want {
            return Err(Error::Query(
                "postgres: the server signature could not be verified".into(),
            ));
        }
        Ok(())
    }

    fn read(&mut self) -> Result<Message> {
        read_message(&mut self.reader).map_err(io)
    }

    fn send_query(&mut self, sql: &str) -> Result<()> {
        let mut w = Writer::new();
        w.msg(b'Q', |b| put_cstr(b, sql));
        w.flush_to(&mut self.writer).map_err(io)
    }

    /// A simple query; the whole result is buffered. For catalog queries and
    /// reading `RowDescription` -- large tables stream through `copy_out`.
    pub fn query(&mut self, sql: &str) -> Result<QueryResult> {
        self.send_query(sql)?;
        let mut out = QueryResult::default();
        let mut failed = None;
        loop {
            let m = self.read()?;
            match m.tag {
                b'T' => out.columns = row_description(&m)?,
                b'D' => out.rows.push(data_row(&m)?),
                b'E' => failed = Some(server_error(&m)),
                // ReadyForQuery is awaited even after an error, otherwise the
                // connection becomes unusable for the next query.
                b'Z' => break,
                _ => {}
            }
        }
        match failed {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }

    /// Starts `COPY ... TO STDOUT`. Rows are read as they stream.
    ///
    /// It takes the connection over: no other query can run on the same
    /// connection while the stream is open, so catalog queries have to come
    /// first.
    pub fn copy_out(mut self, sql: &str) -> Result<CopyOut> {
        self.send_query(sql)?;
        // Read up to CopyOutResponse; on an error the stream never starts.
        loop {
            let m = self.read()?;
            match m.tag {
                b'H' => break,
                b'E' => {
                    let e = server_error(&m);
                    // Read up to Z so the connection stays usable.
                    while self.read()?.tag != b'Z' {}
                    return Err(e);
                }
                b'Z' => {
                    return Err(Error::Query(
                        "postgres: the COPY stream did not start (the query may return no rows)"
                            .into(),
                    ))
                }
                _ => {}
            }
        }
        Ok(CopyOut {
            client: self,
            buf: Vec::new(),
            pos: 0,
            done: false,
        })
    }
}

impl Client {
    /// `START_REPLICATION` of a logical slot through `pgoutput`, protocol 1:
    /// the connection turns into a stream of the publication's changes, a
    /// transaction at a time as each commits, from where the slot was last
    /// confirmed. Needs a [`Client::connect_replication`] connection.
    ///
    /// The names go into the command as they are, so they have to be plain
    /// lowercase identifiers; anything else is refused rather than quoted.
    pub fn start_replication(mut self, slot: &str, publication: &str) -> Result<WalStream> {
        for name in [slot, publication] {
            if !plain_name(name) {
                return Err(Error::Query(format!(
                    "`{name}` is not a plain name: lowercase letters, digits and `_`, \
                     63 at most, not starting with a digit"
                )));
            }
        }
        self.send_query(&format!(
            "START_REPLICATION SLOT {slot} LOGICAL 0/0 \
             (proto_version '1', publication_names '{publication}')"
        ))?;
        loop {
            let m = self.read()?;
            match m.tag {
                // CopyBothResponse: from here on both sides send CopyData.
                b'W' => return Ok(WalStream { client: self }),
                b'E' => return Err(server_error(&m)),
                _ => {}
            }
        }
    }
}

/// Whether `name` can go into a replication command unquoted.
pub fn plain_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && !b[0].is_ascii_digit()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// A write-ahead log position as PostgreSQL prints it: `16/B374D848`.
pub fn lsn_text(lsn: u64) -> String {
    format!("{:X}/{:X}", lsn >> 32, lsn as u32)
}

/// The other way round; `None` when it is not one.
pub fn parse_lsn(s: &str) -> Option<u64> {
    let (hi, lo) = s.trim().split_once('/')?;
    let hi = u32::from_str_radix(hi, 16).ok()?;
    let lo = u32::from_str_radix(lo, 16).ok()?;
    Some((hi as u64) << 32 | lo as u64)
}

/// One message of a replication stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wal {
    /// `XLogData`: one `pgoutput` message, and how far the server's log
    /// reaches.
    Data { end: u64, body: Vec<u8> },
    /// A keepalive: how far the server's log reaches, and whether it wants
    /// a status update now -- it drops a receiver that stays silent past
    /// `wal_sender_timeout`.
    Keepalive { end: u64, reply: bool },
}

/// A logical replication stream, from [`Client::start_replication`].
#[derive(Debug)]
pub struct WalStream {
    client: Client,
}

impl WalStream {
    /// The next message, or `None` when none arrived within `wait`.
    ///
    /// The wait is a `peek`, never a read: a read that timed out halfway
    /// through a message would leave the rest of it to be taken for the
    /// start of the next one.
    pub fn next(&mut self, wait: Duration) -> Result<Option<Wal>> {
        if self.client.reader.buffer().is_empty() {
            let sock = self.client.reader.get_ref();
            // A zero timeout is refused by the socket, and means "forever"
            // to the kernel.
            sock.set_read_timeout(Some(wait.max(Duration::from_millis(1))))
                .map_err(io)?;
            let peeked = sock.peek(&mut [0u8; 1]);
            sock.set_read_timeout(Some(TIMEOUT)).map_err(io)?;
            match peeked {
                Ok(0) => return Err(io("the server closed the replication stream")),
                Ok(_) => {}
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(None)
                }
                Err(e) => return Err(io(e)),
            }
        }
        let m = self.client.read()?;
        match m.tag {
            b'd' => wal_message(&m.body).map(Some),
            b'E' => Err(server_error(&m)),
            b'c' => Err(io("the server ended the replication stream")),
            // A notice, or anything else outside the copy: nothing to act on.
            _ => Ok(None),
        }
    }

    /// Standby status update: everything up to `lsn` is safe with the
    /// receiver, and the slot may let the server forget it. The server takes
    /// the flushed position as the slot's confirmed one, so it must never run
    /// ahead of what is on disk.
    pub fn confirm(&mut self, lsn: u64) -> Result<()> {
        // Microseconds since 2000-01-01, PostgreSQL's epoch; only for the
        // server's lag statistics.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_micros() as i64)
            - 946_684_800_000_000;
        let mut w = Writer::new();
        w.msg(b'd', |b| {
            b.push(b'r');
            for _ in 0..3 {
                // written, flushed, applied
                b.extend_from_slice(&lsn.to_be_bytes());
            }
            b.extend_from_slice(&now.to_be_bytes());
            b.push(0);
        });
        w.flush_to(&mut self.client.writer).map_err(io)
    }
}

/// The body of a `CopyData` on a replication stream.
fn wal_message(b: &[u8]) -> Result<Wal> {
    let short = || Error::Corrupt("postgres: short replication message".into());
    let u64_at = |at: usize| -> Result<u64> {
        let bytes = b.get(at..at + 8).ok_or_else(short)?;
        Ok(u64::from_be_bytes(bytes.try_into().map_err(|_| short())?))
    };
    match b.first() {
        // 'w', start (8), end (8), send time (8), data
        Some(b'w') => Ok(Wal::Data {
            end: u64_at(9)?,
            body: b.get(25..).ok_or_else(short)?.to_vec(),
        }),
        // 'k', end (8), send time (8), reply requested (1)
        Some(b'k') => Ok(Wal::Keepalive {
            end: u64_at(1)?,
            reply: *b.get(17).ok_or_else(short)? == 1,
        }),
        Some(other) => Err(Error::Corrupt(format!(
            "postgres: unknown replication message `{}`",
            *other as char
        ))),
        None => Err(short()),
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Terminate: let the server close the connection cleanly.
        let mut w = Writer::new();
        w.msg(b'X', |_| {});
        let _ = w.flush_to(&mut self.writer);
    }
}

/// A `COPY ... TO STDOUT` stream.
#[derive(Debug)]
pub struct CopyOut {
    client: Client,
    buf: Vec<u8>,
    pos: usize,
    done: bool,
}

impl CopyOut {
    /// The raw bytes of the next line (without the line ending), or `None`
    /// when the stream is done. `CopyData` frames do not align with line
    /// boundaries: a frame can end mid-line or carry several lines.
    pub fn next_line(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if let Some(i) = self.buf[self.pos..].iter().position(|&b| b == b'\n') {
                let line = self.buf[self.pos..self.pos + i].to_vec();
                self.pos += i + 1;
                return Ok(Some(line));
            }
            if self.done {
                if self.pos < self.buf.len() {
                    let line = self.buf[self.pos..].to_vec();
                    self.pos = self.buf.len();
                    return Ok(Some(line));
                }
                return Ok(None);
            }
            self.buf.drain(..self.pos);
            self.pos = 0;
            let m = self.client.read()?;
            match m.tag {
                b'd' => self.buf.extend_from_slice(&m.body),
                b'c' => {
                    self.done = true;
                    // CommandComplete + ReadyForQuery: keep the connection usable.
                    while self.client.read()?.tag != b'Z' {}
                }
                b'E' => {
                    let e = server_error(&m);
                    while self.client.read()?.tag != b'Z' {}
                    return Err(e);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_parsed() {
        let u = Url::parse("postgres://alice:secret@db.local:6432/production").unwrap();
        assert_eq!(u.user, "alice");
        assert_eq!(u.password.as_deref(), Some("secret"));
        assert_eq!(u.host, "db.local");
        assert_eq!(u.port, 6432);
        assert_eq!(u.database, "production");

        // 5432 when no port is given.
        let u = Url::parse("postgresql://alice@localhost/test").unwrap();
        assert_eq!(u.port, 5432);
        assert_eq!(u.password, None);
    }

    /// `@` and `:` travel percent-escaped in a password; splitting on the
    /// last `@` is essential, otherwise the password is mistaken for the host.
    #[test]
    fn password_escapes_survive() {
        let u = Url::parse("postgres://alice:a%40b%3Ac@localhost/t").unwrap();
        assert_eq!(u.password.as_deref(), Some("a@b:c"));
        assert_eq!(u.host, "localhost");
    }

    #[test]
    fn bad_urls_are_refused() {
        assert!(Url::parse("mysql://x@y/z").is_err());
        assert!(
            Url::parse("postgres://alice@localhost").is_err(),
            "no database"
        );
        assert!(Url::parse("postgres://alice@localhost:abc/t").is_err());
        // It must not be swallowed silently.
        assert!(Url::parse("postgres://alice@localhost/t?sslmode=require").is_err());
    }

    #[test]
    fn row_description_carries_typmod() {
        // One column: name "embed", type oid 16385, typmod 384.
        let mut body = 1i16.to_be_bytes().to_vec();
        put_cstr(&mut body, "embed");
        body.extend_from_slice(&0i32.to_be_bytes()); // table oid
        body.extend_from_slice(&0i16.to_be_bytes()); // column number
        body.extend_from_slice(&16_385i32.to_be_bytes()); // type oid
        body.extend_from_slice(&(-1i16).to_be_bytes()); // type length
        body.extend_from_slice(&384i32.to_be_bytes()); // type modifier
        body.extend_from_slice(&0i16.to_be_bytes()); // format
        let cols = row_description(&Message { tag: b'T', body }).unwrap();
        assert_eq!(
            cols,
            vec![FieldDesc {
                name: "embed".into(),
                oid: 16_385,
                typmod: 384,
            }]
        );
    }

    #[test]
    fn data_row_handles_nulls() {
        let mut body = 3i16.to_be_bytes().to_vec();
        body.extend_from_slice(&2i32.to_be_bytes());
        body.extend_from_slice(b"hi");
        body.extend_from_slice(&(-1i32).to_be_bytes()); // NULL
        body.extend_from_slice(&0i32.to_be_bytes()); // empty string
        let cells = data_row(&Message { tag: b'D', body }).unwrap();
        assert_eq!(cells, vec![Some("hi".into()), None, Some(String::new())]);
    }

    #[test]
    fn replication_messages_are_parsed() {
        let mut w = vec![b'w'];
        w.extend_from_slice(&1u64.to_be_bytes());
        w.extend_from_slice(&0x16_B374_D848u64.to_be_bytes());
        w.extend_from_slice(&0i64.to_be_bytes());
        w.extend_from_slice(b"BEGIN");
        assert_eq!(
            wal_message(&w).unwrap(),
            Wal::Data {
                end: 0x16_B374_D848,
                body: b"BEGIN".to_vec()
            }
        );
        let mut k = vec![b'k'];
        k.extend_from_slice(&7u64.to_be_bytes());
        k.extend_from_slice(&0i64.to_be_bytes());
        k.push(1);
        assert_eq!(
            wal_message(&k).unwrap(),
            Wal::Keepalive {
                end: 7,
                reply: true
            }
        );
        assert!(wal_message(&k[..10]).is_err());
        assert_eq!(lsn_text(0x16_B374_D848), "16/B374D848");
        assert_eq!(parse_lsn("16/B374D848"), Some(0x16_B374_D848));
        assert_eq!(parse_lsn("0/0"), Some(0));
        assert_eq!(parse_lsn("nope"), None);
        assert!(plain_name("fenec_articles2"));
        for bad in ["", "Fenec", "2a", "a-b", "a b", "a'b"] {
            assert!(!plain_name(bad), "{bad}");
        }
    }

    #[test]
    fn error_fields_are_extracted() {
        let mut body = Vec::new();
        body.push(b'S');
        put_cstr(&mut body, "ERROR");
        body.push(b'C');
        put_cstr(&mut body, "42P01");
        body.push(b'M');
        put_cstr(&mut body, "relation \"nosuch\" does not exist");
        body.push(0);
        let e = server_error(&Message { tag: b'E', body }).to_string();
        assert!(e.contains("42P01"), "{e}");
        assert!(e.contains("does not exist"), "{e}");
    }
}
