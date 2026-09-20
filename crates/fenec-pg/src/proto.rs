//! PostgreSQL wire protocol v3 -- message encoding/decoding.
//!
//! Referans: https://www.postgresql.org/docs/current/protocol-message-formats.html
//! Only the subset fenecdb needs; every value is sent in text format (format
//! code 0), which every client supports.

use std::io::{self, Read, Write};

pub const PROTOCOL_V3: i32 = 196_608; // 3.0
pub const SSL_REQUEST: i32 = 80_877_103;
pub const GSSENC_REQUEST: i32 = 80_877_104;
pub const CANCEL_REQUEST: i32 = 80_877_102;

/// The protocol's message ceiling; the same as PostgreSQL's. The server
/// supplies its own (smaller) limit via [`read_message_max`].
pub const MAX_MESSAGE: usize = 1 << 30;

/// Startup packet ceiling -- PostgreSQL uses 10 000 bytes too. The packet
/// only carries the user name, the database and the options; leaving it
/// unbounded meant an allocation of arbitrary size *before* authentication.
pub const MAX_STARTUP: i32 = 10_000;

// PostgreSQL type OIDs
pub const OID_BOOL: i32 = 16;
pub const OID_BYTEA: i32 = 17;
pub const OID_INT8: i32 = 20;
pub const OID_TEXT: i32 = 25;
pub const OID_FLOAT8: i32 = 701;
/// `timestamptz`. fenecdb timestamps are always UTC, so this is reported
/// rather than the zoneless `timestamp` (1114).
pub const OID_TIMESTAMPTZ: i32 = 1184;
/// A parameter whose type was not resolved. Seeing this, the client sends
/// the value as text; on the server side [`decode_param`](crate::server) infers it.
pub const OID_UNSPECIFIED: i32 = 0;

pub struct Writer {
    buf: Vec<u8>,
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

impl Writer {
    pub fn new() -> Writer {
        Writer { buf: Vec::new() }
    }

    /// Writes a tagged message; the length field is filled in automatically.
    pub fn msg(&mut self, tag: u8, body: impl FnOnce(&mut Vec<u8>)) {
        self.buf.push(tag);
        let len_at = self.buf.len();
        self.buf.extend_from_slice(&[0; 4]);
        body(&mut self.buf);
        let len = (self.buf.len() - len_at) as i32;
        self.buf[len_at..len_at + 4].copy_from_slice(&len.to_be_bytes());
    }

    pub fn raw(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    pub fn flush_to(&mut self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(&self.buf)?;
        w.flush()?;
        self.buf.clear();
        Ok(())
    }

    // ---------------------------------------------------------- messages

    pub fn auth_ok(&mut self) {
        self.msg(b'R', |b| b.extend_from_slice(&0i32.to_be_bytes()));
    }

    /// AuthenticationCleartextPassword (3)
    pub fn auth_cleartext(&mut self) {
        self.msg(b'R', |b| b.extend_from_slice(&3i32.to_be_bytes()));
    }

    /// AuthenticationSASL (10) -- the list of supported mechanisms.
    pub fn auth_sasl(&mut self, mechanisms: &[&str]) {
        self.msg(b'R', |b| {
            b.extend_from_slice(&10i32.to_be_bytes());
            for m in mechanisms {
                put_cstr(b, m);
            }
            b.push(0);
        });
    }

    /// AuthenticationSASLContinue (11)
    pub fn auth_sasl_continue(&mut self, data: &str) {
        self.msg(b'R', |b| {
            b.extend_from_slice(&11i32.to_be_bytes());
            b.extend_from_slice(data.as_bytes());
        });
    }

    /// AuthenticationSASLFinal (12)
    pub fn auth_sasl_final(&mut self, data: &str) {
        self.msg(b'R', |b| {
            b.extend_from_slice(&12i32.to_be_bytes());
            b.extend_from_slice(data.as_bytes());
        });
    }

    pub fn parameter_status(&mut self, k: &str, v: &str) {
        self.msg(b'S', |b| {
            put_cstr(b, k);
            put_cstr(b, v);
        });
    }

    pub fn backend_key_data(&mut self, pid: i32, key: i32) {
        self.msg(b'K', |b| {
            b.extend_from_slice(&pid.to_be_bytes());
            b.extend_from_slice(&key.to_be_bytes());
        });
    }

    /// 'I' idle, 'T' in a transaction, 'E' failed transaction
    pub fn ready(&mut self, status: u8) {
        self.msg(b'Z', |b| b.push(status));
    }

    pub fn row_description(&mut self, cols: &[(String, i32)]) {
        self.msg(b'T', |b| {
            b.extend_from_slice(&(cols.len() as i16).to_be_bytes());
            for (name, oid) in cols {
                put_cstr(b, name);
                b.extend_from_slice(&0i32.to_be_bytes()); // table oid
                b.extend_from_slice(&0i16.to_be_bytes()); // column number
                b.extend_from_slice(&oid.to_be_bytes());
                b.extend_from_slice(&(-1i16).to_be_bytes()); // type length
                b.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
                b.extend_from_slice(&0i16.to_be_bytes()); // text format
            }
        });
    }

    pub fn data_row(&mut self, cells: &[Option<String>]) {
        self.msg(b'D', |b| {
            b.extend_from_slice(&(cells.len() as i16).to_be_bytes());
            for c in cells {
                match c {
                    None => b.extend_from_slice(&(-1i32).to_be_bytes()),
                    Some(s) => {
                        b.extend_from_slice(&(s.len() as i32).to_be_bytes());
                        b.extend_from_slice(s.as_bytes());
                    }
                }
            }
        });
    }

    pub fn command_complete(&mut self, tag: &str) {
        self.msg(b'C', |b| put_cstr(b, tag));
    }

    pub fn empty_query(&mut self) {
        self.msg(b'I', |_| {});
    }

    pub fn no_data(&mut self) {
        self.msg(b'n', |_| {});
    }

    pub fn parse_complete(&mut self) {
        self.msg(b'1', |_| {});
    }

    pub fn bind_complete(&mut self) {
        self.msg(b'2', |_| {});
    }

    pub fn close_complete(&mut self) {
        self.msg(b'3', |_| {});
    }

    pub fn parameter_description(&mut self, oids: &[i32]) {
        self.msg(b't', |b| {
            b.extend_from_slice(&(oids.len() as i16).to_be_bytes());
            for o in oids {
                b.extend_from_slice(&o.to_be_bytes());
            }
        });
    }

    /// SQLSTATE codes: 42601 syntax, 42P01 no such table, XX000 internal
    pub fn error(&mut self, code: &str, message: &str) {
        self.diagnostic(b'E', "ERROR", code, message);
    }

    /// NoticeResponse: a warning that lands in the client's log without
    /// breaking the stream.
    pub fn notice(&mut self, code: &str, message: &str) {
        self.diagnostic(b'N', "WARNING", code, message);
    }

    fn diagnostic(&mut self, tag: u8, severity: &str, code: &str, message: &str) {
        self.msg(tag, |b| {
            b.push(b'S');
            put_cstr(b, severity);
            b.push(b'V');
            put_cstr(b, severity);
            b.push(b'C');
            put_cstr(b, code);
            b.push(b'M');
            put_cstr(b, message);
            b.push(0);
        });
    }
}

pub fn put_cstr(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(s.as_bytes());
    b.push(0);
}

/// A tagged message coming from the client.
pub struct Message {
    pub tag: u8,
    pub body: Vec<u8>,
}

pub fn read_i32(r: &mut impl Read) -> io::Result<i32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(i32::from_be_bytes(b))
}

/// Reads a tagged message (every message after the connection is established).
pub fn read_message(r: &mut impl Read) -> io::Result<Message> {
    read_message_max(r, MAX_MESSAGE)
}

/// The caller decides the ceiling. Since the length is read *before* the
/// body, the limit is applied before allocation: the body of an oversized
/// message is never allocated.
pub fn read_message_max(r: &mut impl Read, max: usize) -> io::Result<Message> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)?;
    let len = read_i32(r)?;
    let max = max.min(MAX_MESSAGE);
    if len < 4 || len as usize > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message length {len}, ceiling {max} bytes"),
        ));
    }
    let mut body = vec![0u8; (len - 4) as usize];
    r.read_exact(&mut body)?;
    Ok(Message { tag: tag[0], body })
}

/// Reads a NUL-terminated string from the body.
pub fn take_cstr(body: &[u8], pos: &mut usize) -> String {
    let start = *pos;
    while *pos < body.len() && body[*pos] != 0 {
        *pos += 1;
    }
    let s = String::from_utf8_lossy(&body[start..*pos]).into_owned();
    if *pos < body.len() {
        *pos += 1;
    }
    s
}
