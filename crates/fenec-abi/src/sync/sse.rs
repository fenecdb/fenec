//! Server-Sent Events read off the bytes as they come, in whatever pieces
//! the platform's client hands them over: a binding feeds the stream's
//! body and never parses it, so the reading is written once, here.
//!
//! What `web/http.js`'s `sseEvents` reads: an event is the lines up to a
//! blank one, `event:` naming it and each `data:` line's text appended,
//! `\r\n` read as `\n`; a comment (`: keepalive`) and an event with no name
//! are no event.

/// A stream's bytes not yet read into an event.
#[derive(Default)]
pub struct Frames {
    buf: Vec<u8>,
    /// How far `buf` is known to hold no `\n`: a seed is one line of
    /// megabytes, arriving in pieces of kilobytes, and searched from the
    /// start for each piece it was read over hundreds of times.
    scanned: usize,
    name: String,
    data: String,
}

/// One event: its name and its data.
pub struct Event {
    pub name: String,
    pub data: String,
}

impl Frames {
    /// The events `bytes` completes.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut start = 0;
        let mut from = self.scanned;
        while let Some(at) = self.buf[from..].iter().position(|&b| b == b'\n') {
            let end = from + at;
            let mut line = &self.buf[start..end];
            if let [rest @ .., b'\r'] = line {
                line = rest;
            }
            if line.is_empty() {
                let name = std::mem::take(&mut self.name);
                let data = std::mem::take(&mut self.data);
                if !name.is_empty() {
                    out.push(Event { name, data });
                }
            } else if let Some(v) = line.strip_prefix(b"event:") {
                self.name = String::from_utf8_lossy(v).trim().to_string();
            } else if let Some(v) = line.strip_prefix(b"data:") {
                self.data.push_str(String::from_utf8_lossy(v).trim());
            }
            start = end + 1;
            from = start;
        }
        self.buf.drain(..start);
        self.scanned = self.buf.len();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pieces cut anywhere -- inside a name, between `\r` and `\n` -- read
    /// as the whole does.
    #[test]
    fn events_read_the_same_however_the_bytes_are_cut() {
        let text = b"event: seed\r\ndata: {\"seq\":1,\r\n\r\n: keepalive\n\nevent: change\ndata: {\"seq\":2}\n\ndata: lost\n\n";
        for cut in 1..text.len() {
            let mut f = Frames::default();
            let mut got = Vec::new();
            for piece in text.chunks(cut) {
                got.extend(f.push(piece).into_iter().map(|e| (e.name, e.data)));
            }
            assert_eq!(
                got,
                [
                    ("seed".to_string(), "{\"seq\":1,".to_string()),
                    ("change".to_string(), "{\"seq\":2}".to_string())
                ],
                "cut every {cut}"
            );
        }
    }
}
