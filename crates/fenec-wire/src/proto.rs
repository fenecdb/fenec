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
/// is never allocated. Under it the body's buffer is the length the server
/// says, allocated so that its failure is an error ([`zeroed`]).
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
    let mut body = zeroed((len - 4) as usize)?;
    r.read_exact(&mut body)?;
    Ok(Message { tag: tag[0], body })
}

/// `len` zero bytes for a message the server says is that long: the
/// allocation `vec![0u8; len]` makes -- `alloc_zeroed`, the system's zero
/// pages, each taken up as its bytes arrive -- with its failure an error
/// where `vec!`'s aborts. Under the ceiling a length is up to a gigabyte,
/// sent by the server, buggy or not, or by anyone on the path (fenecdb
/// speaks no TLS), and where the system refuses that much -- a host that
/// does not overcommit, Linux's heuristic refusing more than its memory and
/// swap on a small machine -- the refusal aborted `fenec import`, and
/// `fenec-server --follow` with every request it served; now it is an
/// error, which the follower takes for a broken stream and connects again
/// after. A buffer grown as the bytes came would cost a lying length only
/// what was sent, but would copy what had come at each growth of every
/// message past its first size -- a row of a wide vector or a long text is
/// one COPY message -- where believing the length costs the pages its bytes
/// reach, and on a system that lends it nothing more. fenec-shard reads a
/// node's answer the same way, for the same reasons (its `zeroed`).
fn zeroed(len: usize) -> io::Result<Vec<u8>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let Ok(layout) = std::alloc::Layout::array::<u8>(len) else {
        return Err(refused(len));
    };
    // SAFETY: a layout of a non-zero size.
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    if ptr.is_null() {
        return Err(refused(len));
    }
    // SAFETY: allocated by the global allocator for exactly `len` bytes of
    // `u8`'s alignment, every one of them initialised to zero -- the
    // allocation a `Vec<u8>` of that capacity owns, as `vec!` makes it.
    Ok(unsafe { Vec::from_raw_parts(ptr, len, len) })
}

/// The error of a length no allocation holds, out of line: formatted where
/// it is refused, the length was stored to the stack for its address on
/// every message read, the one instruction `read_message`'s loop gained.
#[cold]
#[inline(never)]
fn refused(len: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::OutOfMemory,
        format!("the server's message says it is {len} bytes, which cannot be held"),
    )
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

    /// A message's tag and its length, as a server sends them.
    fn header(tag: u8, len: i32) -> Vec<u8> {
        let mut m = vec![tag];
        m.extend_from_slice(&len.to_be_bytes());
        m
    }

    /// A length is held to the protocol before anything is allocated or
    /// read past it: under 4 (its own four bytes) or past PostgreSQL's
    /// gigabyte, the message is refused with the rest of the stream
    /// unread. Under the ceiling a length is believed as far as the system
    /// lends it: a gigabyte said and three bytes sent is an error -- the
    /// allocation refused, or lent and the message cut short -- never an
    /// abort; past what an allocation can be, refused before it is asked.
    /// And a message of 20 MiB comes whole, its neighbour after it.
    #[test]
    fn a_message_s_length_is_held_to_the_protocol_and_never_aborts() {
        for len in [i32::MIN, -1, 0, 3, (MAX_MESSAGE + 1) as i32, i32::MAX] {
            let mut m = header(b'd', len);
            m.extend_from_slice(b"rest");
            let mut r = &m[..];
            let e = read_message(&mut r).err().unwrap();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{len}: {e}");
            assert_eq!(r, b"rest", "{len}: read past its header");
        }
        let mut m = header(b'd', MAX_MESSAGE as i32);
        m.extend_from_slice(b"abc");
        let e = read_message(&mut &m[..]).err().unwrap();
        let kind = e.kind();
        assert!(
            matches!(
                kind,
                io::ErrorKind::OutOfMemory | io::ErrorKind::UnexpectedEof
            ),
            "{e}"
        );
        let past = zeroed(isize::MAX as usize + 1).unwrap_err();
        assert_eq!(past.kind(), io::ErrorKind::OutOfMemory, "{past}");
        assert!(zeroed(0).unwrap().is_empty());
        let whole: Vec<u8> = (0..(20 << 20) + 7).map(|i| (i % 251) as u8).collect();
        let mut m = header(b'd', whole.len() as i32 + 4);
        m.extend_from_slice(&whole);
        m.extend_from_slice(&header(b'Z', 5));
        m.push(b'I');
        let mut r = &m[..];
        let d = read_message(&mut r).unwrap();
        assert!(d.tag == b'd' && d.body == whole);
        let z = read_message(&mut r).unwrap();
        assert_eq!((z.tag, z.body), (b'Z', b"I".to_vec()));
        assert!(r.is_empty());
    }
}
