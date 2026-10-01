//! PostgreSQL wire protocol v3 -- the framing the client speaks.
//!
//! Reference: https://www.postgresql.org/docs/current/protocol-message-formats.html
//! Only what `fenec import` and `--follow` send and read: the startup, the
//! password exchange, simple queries, COPY out and the replication stream.

use std::io::{self, Read, Write};

pub const PROTOCOL_V3: i32 = 196_608; // 3.0

/// The protocol's message ceiling; the same as PostgreSQL's.
pub const MAX_MESSAGE: usize = 1 << 30;

/// The messages a client sends, written into one buffer and flushed at
/// once.
#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
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

    pub fn flush_to(&mut self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(&self.buf)?;
        w.flush()?;
        self.buf.clear();
        Ok(())
    }
}

pub fn put_cstr(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(s.as_bytes());
    b.push(0);
}

/// A tagged message coming from the server.
pub struct Message {
    pub tag: u8,
    pub body: Vec<u8>,
}

pub fn read_i32(r: &mut impl Read) -> io::Result<i32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(i32::from_be_bytes(b))
}

/// Reads a tagged message. Since the length is read *before* the body, the
/// ceiling is applied before allocation: the body of an oversized message
/// is never allocated.
pub fn read_message(r: &mut impl Read) -> io::Result<Message> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)?;
    let len = read_i32(r)?;
    if len < 4 || len as usize > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message length {len}, ceiling {MAX_MESSAGE} bytes"),
        ));
    }
    let mut body = vec![0u8; (len - 4) as usize];
    r.read_exact(&mut body)?;
    Ok(Message { tag: tag[0], body })
}

/// Reads a NUL-terminated string from the body. The terminator is found a
/// word at a time (`CStr`): a byte at a time, a `put` of 1 000 128-dim rows
/// spent 0.45 ms of its 12.6 finding the end of its text when the server
/// side read it.
pub fn take_cstr(body: &[u8], pos: &mut usize) -> String {
    let rest = body.get(*pos..).unwrap_or_default();
    let len = std::ffi::CStr::from_bytes_until_nul(rest).map_or(rest.len(), |c| c.to_bytes().len());
    // Checked whole first: `from_utf8_lossy` walks a valid text in chunks a
    // byte at a time, where `from_utf8` takes ASCII a word at a time -- 335
    // of a simple `put` of 1 000 128-dim rows' samples went to the walk.
    let s = match std::str::from_utf8(&rest[..len]) {
        Ok(s) => s.to_owned(),
        Err(_) => String::from_utf8_lossy(&rest[..len]).into_owned(),
    };
    *pos += len + (len < rest.len()) as usize;
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A string ends at its NUL, or at the body's end without one; the
    /// position is past the NUL, and never past the body.
    #[test]
    fn a_cstr_ends_at_its_nul_or_at_the_body() {
        let body = b"one\0\0tw\xffo";
        let mut pos = 0;
        assert_eq!(take_cstr(body, &mut pos), "one");
        assert_eq!(pos, 4);
        assert_eq!(take_cstr(body, &mut pos), "");
        assert_eq!(pos, 5);
        assert_eq!(take_cstr(body, &mut pos), "tw\u{fffd}o");
        assert_eq!(pos, body.len());
        assert_eq!(take_cstr(body, &mut pos), "");
        assert_eq!(pos, body.len());
    }
}
