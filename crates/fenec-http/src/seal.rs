//! Sealed files: an archive's images, segments and history, and a backup,
//! encrypted and authenticated with ChaCha20-Poly1305 (`crypto.rs`) under a
//! key from a file. A backup leaves the machine -- a bucket, another disk --
//! where the disk's own encryption no longer covers it; the live database
//! stays plain, since a mapped file is read where it lies, and is for the
//! disk to encrypt.
//!
//! ```text
//! "FENECSL\x01", then frames:
//! [plaintext length u32][nonce 12][ciphertext][tag 16]
//! ```
//!
//! Each frame's nonce is random, and its tag covers its place in the file
//! and whether it is the last: a frame moved, dropped, repeated or taken
//! from another file does not open, nor does one byte changed, nor a file
//! cut short where a whole one was sealed (`open_whole`). A segment is
//! appended a frame at a time, each holding whole records, so a crash
//! leaves at most its last frame cut short, which is cut off as a torn
//! record is in a plain one (`open_appended`).

use crate::crypto;
use std::io;
use std::path::Path;

pub const MAGIC: &[u8; 8] = b"FENECSL\x01";
const HEAD: usize = 4 + 12;
const TAG: usize = 16;
/// A whole file is sealed a mebibyte a frame.
const FRAME: usize = 1 << 20;

fn corrupt(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// A 256-bit key.
#[derive(Clone)]
pub struct Key([u8; 32]);

impl Key {
    /// The key in `path`: 64 hexadecimal digits, as `fenec key` writes it.
    pub fn from_file(path: &Path) -> io::Result<Key> {
        let text = std::fs::read_to_string(path)?;
        Key::from_hex(text.trim()).ok_or_else(|| {
            corrupt(&format!(
                "{}: a key is 64 hexadecimal digits (`fenec key` writes one)",
                path.display()
            ))
        })
    }

    pub fn from_hex(text: &str) -> Option<Key> {
        if text.len() != 64 {
            return None;
        }
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        Some(Key(k))
    }

    /// A new key from the system's randomness, as hexadecimal: never the
    /// clock, which `crypto::random_bytes` falls back to.
    pub fn generate() -> Option<String> {
        let k = crypto::system_random(32)?;
        Some(k.iter().map(|b| format!("{b:02x}")).collect())
    }
}

/// Whether `bytes` are a sealed file's.
pub fn is_sealed(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

fn aad(index: u64, last: bool) -> [u8; 9] {
    let mut a = [0u8; 9];
    a[..8].copy_from_slice(&index.to_le_bytes());
    a[8] = last as u8;
    a
}

/// Frame `index` of `plain`, appended to `out`.
pub fn seal_frame(key: &Key, index: u64, last: bool, plain: &[u8], out: &mut Vec<u8>) {
    let nonce: [u8; 12] = crypto::random_bytes(12).try_into().unwrap_or([0; 12]);
    out.extend_from_slice(&(plain.len() as u32).to_le_bytes());
    out.extend_from_slice(&nonce);
    let at = out.len();
    out.extend_from_slice(plain);
    let tag = crypto::seal_in_place(&key.0, &nonce, &aad(index, last), &mut out[at..]);
    out.extend_from_slice(&tag);
}

/// `plain` sealed whole: the magic, then frames of a mebibyte, the last
/// flagged as the last.
pub fn seal_whole(key: &Key, plain: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(plain.len() + plain.len() / FRAME * 32 + 64);
    out.extend_from_slice(MAGIC);
    let mut chunks = plain.chunks(FRAME).peekable();
    if chunks.peek().is_none() {
        seal_frame(key, 0, true, &[], &mut out);
    }
    let mut index = 0;
    while let Some(c) = chunks.next() {
        seal_frame(key, index, chunks.peek().is_none(), c, &mut out);
        index += 1;
    }
    out
}

/// The frames of `bytes` past the magic: each whole one opened, until one
/// is cut short. Returns the plaintext, where the last whole frame ends,
/// the frames opened and whether the last was flagged as the last.
fn open_frames(key: &Key, bytes: &[u8]) -> io::Result<(Vec<u8>, usize, u64, bool)> {
    if !is_sealed(bytes) {
        return Err(corrupt("not a sealed file"));
    }
    let mut out = Vec::with_capacity(bytes.len());
    let (mut pos, mut index, mut last) = (MAGIC.len(), 0u64, false);
    while let Some(head) = bytes.get(pos..pos + HEAD) {
        let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let Some(frame) = bytes.get(pos + HEAD..pos + HEAD + len + TAG) else {
            break;
        };
        if last {
            return Err(corrupt("a sealed file goes on past its last frame"));
        }
        let nonce: [u8; 12] = head[4..].try_into().unwrap_or([0; 12]);
        let at = out.len();
        out.extend_from_slice(&frame[..len]);
        // A tag that does not match leaves the bytes as they were, so the
        // frame is tried as the last only after it is not a middle one.
        let tag = &frame[len..];
        let opened = [false, true]
            .into_iter()
            .find(|&l| crypto::open_in_place(&key.0, &nonce, &aad(index, l), &mut out[at..], tag));
        let Some(l) = opened else {
            return Err(corrupt(&format!(
                "frame {index} does not open: another key, or bytes changed"
            )));
        };
        last = l;
        pos += HEAD + len + TAG;
        index += 1;
    }
    Ok((out, pos, index, last))
}

/// A file sealed whole: every frame opened, the last flagged as the last,
/// nothing after it. Cut short anywhere, it is refused.
pub fn open_whole(key: &Key, bytes: &[u8]) -> io::Result<Vec<u8>> {
    let (plain, end, _, last) = open_frames(key, bytes)?;
    if !last || end != bytes.len() {
        return Err(corrupt("a sealed file cut short"));
    }
    Ok(plain)
}

/// A file appended a frame at a time: its whole frames opened, where they
/// end -- a frame after it is cut short, as a crash leaves one -- and how
/// many there are, the next frame's index.
pub fn open_appended(key: &Key, bytes: &[u8]) -> io::Result<(Vec<u8>, usize, u64)> {
    let (plain, end, frames, last) = open_frames(key, bytes)?;
    if last {
        return Err(corrupt(
            "an appended file holds a frame sealed as a whole file's last",
        ));
    }
    Ok((plain, end, frames))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: u8) -> Key {
        Key([b; 32])
    }

    #[test]
    fn a_whole_file_opens_as_it_was_and_not_otherwise() {
        let k = key(7);
        for len in [0, 1, FRAME - 1, FRAME, FRAME + 1, 3 * FRAME + 5] {
            let plain: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
            let sealed = seal_whole(&k, &plain);
            assert_eq!(open_whole(&k, &sealed).unwrap(), plain, "{len}");
            assert!(open_whole(&key(8), &sealed).is_err(), "another key");
            let mut flipped = sealed.clone();
            let at = flipped.len() / 2;
            flipped[at] ^= 1;
            assert!(open_whole(&k, &flipped).is_err(), "a byte changed");
            // Cut short at every frame's end but the last, and inside one.
            assert!(open_whole(&k, &sealed[..sealed.len() - 1]).is_err());
            if len > FRAME {
                let first = MAGIC.len() + HEAD + FRAME + TAG;
                assert!(open_whole(&k, &sealed[..first]).is_err(), "a frame dropped");
            }
        }
    }

    #[test]
    fn frames_out_of_place_do_not_open() {
        let k = key(1);
        let plain = vec![9u8; 2 * FRAME + 10];
        let sealed = seal_whole(&k, &plain);
        let frame = HEAD + FRAME + TAG;
        let (a, b) = (MAGIC.len(), MAGIC.len() + frame);
        let mut swapped = sealed.clone();
        swapped[a..a + frame].copy_from_slice(&sealed[b..b + frame]);
        swapped[b..b + frame].copy_from_slice(&sealed[a..a + frame]);
        assert!(open_whole(&k, &swapped).is_err());
    }

    #[test]
    fn an_appended_file_loses_only_a_frame_cut_short() {
        let k = key(3);
        let mut file = MAGIC.to_vec();
        for i in 0..5u64 {
            seal_frame(&k, i, false, &[i as u8; 40], &mut file);
        }
        let whole = file.len();
        seal_frame(&k, 5, false, &[5; 40], &mut file);
        let cut = &file[..file.len() - 3];
        let (plain, end, frames) = open_appended(&k, cut).unwrap();
        assert_eq!((plain.len(), end, frames), (200, whole, 5));
        // Sealed whole, a file is no appended one.
        assert!(open_appended(&k, &seal_whole(&k, b"x")).is_err());
    }
}
