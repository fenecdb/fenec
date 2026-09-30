//! The smallest crypto set SCRAM-SHA-256 and HS256 JWTs need: SHA-256,
//! HMAC, PBKDF2, and base64 in both its alphabets -- and ChaCha20-Poly1305
//! (RFC 8439), which seals an archive's files (`seal.rs`).
//!
//! No dependencies, like the rest of fenecdb. All three functions are checked
//! against RFC test vectors (see the tests at the end of the module);
//! hand-written crypto is only usable when it can be proven against known
//! vectors.

// ----------------------------------------------------------------- SHA-256

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

pub const SHA256_LEN: usize = 32;
const BLOCK: usize = 64;

pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; BLOCK],
    len: usize,
    total: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Sha256 {
        Sha256 {
            h: H0,
            buf: [0; BLOCK],
            len: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.len > 0 {
            let want = BLOCK - self.len;
            let take = want.min(data.len());
            self.buf[self.len..self.len + take].copy_from_slice(&data[..take]);
            self.len += take;
            data = &data[take..];
            if self.len == BLOCK {
                let block = self.buf;
                self.compress(&block);
                self.len = 0;
            }
        }
        while data.len() >= BLOCK {
            let (block, rest) = data.split_at(BLOCK);
            let mut b = [0u8; BLOCK];
            b.copy_from_slice(block);
            self.compress(&b);
            data = rest;
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.len = data.len();
        }
    }

    pub fn finish(mut self) -> [u8; SHA256_LEN] {
        let bits = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        // Pad with zeros until 8 bytes are left for the length field.
        while self.len != BLOCK - 8 {
            self.update(&[0x00]);
            // `update` grows total; the padding must not count towards the length.
            self.total = self.total.wrapping_sub(1);
        }
        let mut b = self.buf;
        b[BLOCK - 8..].copy_from_slice(&bits.to_be_bytes());
        self.compress(&b);
        let mut out = [0u8; SHA256_LEN];
        for (i, w) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; BLOCK]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = self.h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for (h, v) in self.h.iter_mut().zip(v) {
            *h = h.wrapping_add(v);
        }
    }
}

pub fn sha256(data: &[u8]) -> [u8; SHA256_LEN] {
    let mut h = Sha256::new();
    h.update(data);
    h.finish()
}

// --------------------------------------------------------------- HMAC/PBKDF2

pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; SHA256_LEN] {
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..SHA256_LEN].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(msg);
    let inner = inner.finish();
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(&inner);
    outer.finish()
}

/// PBKDF2-HMAC-SHA256, a single 32-byte block of output (what SCRAM needs).
pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iters: u32) -> [u8; SHA256_LEN] {
    let mut first = Vec::with_capacity(salt.len() + 4);
    first.extend_from_slice(salt);
    first.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha256(password, &first);
    let mut out = u;
    for _ in 1..iters {
        u = hmac_sha256(password, &u);
        for i in 0..SHA256_LEN {
            out[i] ^= u[i];
        }
    }
    out
}

/// Constant-time comparison: verifying the password proof must not exit
/// early.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for i in 0..a.len() {
        d |= a[i] ^ b[i];
    }
    d == 0
}

// -------------------------------------------------------------------- base64

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        s.push(B64[(n >> 18) as usize & 63] as char);
        s.push(B64[(n >> 12) as usize & 63] as char);
        s.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        s.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    s
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// base64url without padding, as a JWT spells its three parts (RFC 7515).
pub fn b64url_encode(data: &[u8]) -> String {
    b64_encode(data)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

/// The inverse of [`b64url_encode`]. The standard alphabet's `+` and `/`
/// are refused: a token that mixes the two is not one this server made.
pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    if s.contains(['+', '/', '=']) {
        return None;
    }
    b64_decode(&s.replace('-', "+").replace('_', "/"))
}

// -------------------------------------------------------- ChaCha20-Poly1305
//
// Chosen over AES-GCM for what hand-written crypto has to get right: ChaCha20
// is additions, rotations and xors, constant-time with no tables, where AES
// in software reads S-boxes by secret indexes. Poly1305 is the 26-bit-limb
// form (poly1305-donna), every step the same whatever the key.

fn quarter(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// One 64-byte block of the ChaCha20 key stream.
fn chacha20_block(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> [u8; 64] {
    let mut init = [0u32; 16];
    init[..4].copy_from_slice(&[0x61707865, 0x3320646e, 0x79622d32, 0x6b206574]);
    for i in 0..8 {
        init[4 + i] = le32(&key[i * 4..]);
    }
    init[12] = counter;
    for i in 0..3 {
        init[13 + i] = le32(&nonce[i * 4..]);
    }
    let mut s = init;
    for _ in 0..10 {
        quarter(&mut s, 0, 4, 8, 12);
        quarter(&mut s, 1, 5, 9, 13);
        quarter(&mut s, 2, 6, 10, 14);
        quarter(&mut s, 3, 7, 11, 15);
        quarter(&mut s, 0, 5, 10, 15);
        quarter(&mut s, 1, 6, 11, 12);
        quarter(&mut s, 2, 7, 8, 13);
        quarter(&mut s, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        out[i * 4..i * 4 + 4].copy_from_slice(&s[i].wrapping_add(init[i]).to_le_bytes());
    }
    out
}

/// `data` xored with the key stream from block `counter` on.
pub fn chacha20_xor(key: &[u8; 32], counter: u32, nonce: &[u8; 12], data: &mut [u8]) {
    for (i, chunk) in data.chunks_mut(64).enumerate() {
        let ks = chacha20_block(key, counter.wrapping_add(i as u32), nonce);
        for (b, k) in chunk.iter_mut().zip(ks) {
            *b ^= k;
        }
    }
}

/// Poly1305's tag of `msg` under a one-time `key`.
pub fn poly1305(key: &[u8; 32], msg: &[u8]) -> [u8; 16] {
    // r, clamped, in five 26-bit limbs; s the key's second half.
    let r0 = le32(&key[0..]) & 0x3ffffff;
    let r1 = (le32(&key[3..]) >> 2) & 0x3ffff03;
    let r2 = (le32(&key[6..]) >> 4) & 0x3ffc0ff;
    let r3 = (le32(&key[9..]) >> 6) & 0x3f03fff;
    let r4 = (le32(&key[12..]) >> 8) & 0x00fffff;
    let (s1, s2, s3, s4) = (r1 * 5, r2 * 5, r3 * 5, r4 * 5);
    let (mut h0, mut h1, mut h2, mut h3, mut h4) = (0u32, 0u32, 0u32, 0u32, 0u32);
    for chunk in msg.chunks(16) {
        let mut b = [0u8; 17];
        b[..chunk.len()].copy_from_slice(chunk);
        b[chunk.len()] = 1;
        let hibit = (b[16] as u32) << 24;
        h0 += le32(&b[0..]) & 0x3ffffff;
        h1 += (le32(&b[3..]) >> 2) & 0x3ffffff;
        h2 += (le32(&b[6..]) >> 4) & 0x3ffffff;
        h3 += (le32(&b[9..]) >> 6) & 0x3ffffff;
        h4 += (le32(&b[12..]) >> 8) | hibit;
        let m = |a: u32, b: u32| a as u64 * b as u64;
        let d0 = m(h0, r0) + m(h1, s4) + m(h2, s3) + m(h3, s2) + m(h4, s1);
        let mut d1 = m(h0, r1) + m(h1, r0) + m(h2, s4) + m(h3, s3) + m(h4, s2);
        let mut d2 = m(h0, r2) + m(h1, r1) + m(h2, r0) + m(h3, s4) + m(h4, s3);
        let mut d3 = m(h0, r3) + m(h1, r2) + m(h2, r1) + m(h3, r0) + m(h4, s4);
        let mut d4 = m(h0, r4) + m(h1, r3) + m(h2, r2) + m(h3, r1) + m(h4, r0);
        let mut c = (d0 >> 26) as u32;
        h0 = d0 as u32 & 0x3ffffff;
        d1 += c as u64;
        c = (d1 >> 26) as u32;
        h1 = d1 as u32 & 0x3ffffff;
        d2 += c as u64;
        c = (d2 >> 26) as u32;
        h2 = d2 as u32 & 0x3ffffff;
        d3 += c as u64;
        c = (d3 >> 26) as u32;
        h3 = d3 as u32 & 0x3ffffff;
        d4 += c as u64;
        c = (d4 >> 26) as u32;
        h4 = d4 as u32 & 0x3ffffff;
        h0 += c * 5;
        c = h0 >> 26;
        h0 &= 0x3ffffff;
        h1 += c;
    }
    // Full carry, then h - p chosen over h without a branch.
    let mut c = h1 >> 26;
    h1 &= 0x3ffffff;
    h2 += c;
    c = h2 >> 26;
    h2 &= 0x3ffffff;
    h3 += c;
    c = h3 >> 26;
    h3 &= 0x3ffffff;
    h4 += c;
    c = h4 >> 26;
    h4 &= 0x3ffffff;
    h0 += c * 5;
    c = h0 >> 26;
    h0 &= 0x3ffffff;
    h1 += c;
    let mut g0 = h0.wrapping_add(5);
    c = g0 >> 26;
    g0 &= 0x3ffffff;
    let mut g1 = h1.wrapping_add(c);
    c = g1 >> 26;
    g1 &= 0x3ffffff;
    let mut g2 = h2.wrapping_add(c);
    c = g2 >> 26;
    g2 &= 0x3ffffff;
    let mut g3 = h3.wrapping_add(c);
    c = g3 >> 26;
    g3 &= 0x3ffffff;
    let g4 = h4.wrapping_add(c).wrapping_sub(1 << 26);
    let mask = (g4 >> 31).wrapping_sub(1);
    let keep = !mask;
    h0 = (h0 & keep) | (g0 & mask);
    h1 = (h1 & keep) | (g1 & mask);
    h2 = (h2 & keep) | (g2 & mask);
    h3 = (h3 & keep) | (g3 & mask);
    h4 = (h4 & keep) | (g4 & mask);
    // h mod 2^128, plus s.
    let w0 = h0 | (h1 << 26);
    let w1 = (h1 >> 6) | (h2 << 20);
    let w2 = (h2 >> 12) | (h3 << 14);
    let w3 = (h3 >> 18) | (h4 << 8);
    let mut f = w0 as u64 + le32(&key[16..]) as u64;
    let t0 = f as u32;
    f = w1 as u64 + le32(&key[20..]) as u64 + (f >> 32);
    let t1 = f as u32;
    f = w2 as u64 + le32(&key[24..]) as u64 + (f >> 32);
    let t2 = f as u32;
    f = w3 as u64 + le32(&key[28..]) as u64 + (f >> 32);
    let t3 = f as u32;
    let mut tag = [0u8; 16];
    for (i, t) in [t0, t1, t2, t3].iter().enumerate() {
        tag[i * 4..i * 4 + 4].copy_from_slice(&t.to_le_bytes());
    }
    tag
}

/// The Poly1305 tag of `aad` and `ct` as RFC 8439's AEAD lays them out.
fn aead_tag(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], ct: &[u8]) -> [u8; 16] {
    let block = chacha20_block(key, 0, nonce);
    let mut otk = [0u8; 32];
    otk.copy_from_slice(&block[..32]);
    let pad = |n: usize| (16 - n % 16) % 16;
    let mut mac = Vec::with_capacity(aad.len() + ct.len() + 48);
    mac.extend_from_slice(aad);
    mac.resize(mac.len() + pad(aad.len()), 0);
    mac.extend_from_slice(ct);
    mac.resize(mac.len() + pad(ct.len()), 0);
    mac.extend_from_slice(&(aad.len() as u64).to_le_bytes());
    mac.extend_from_slice(&(ct.len() as u64).to_le_bytes());
    poly1305(&otk, &mac)
}

/// Encrypts `data` in place and returns its tag: ChaCha20-Poly1305.
pub fn seal_in_place(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], data: &mut [u8]) -> [u8; 16] {
    chacha20_xor(key, 1, nonce, data);
    aead_tag(key, nonce, aad, data)
}

/// Decrypts `data` in place when `tag` is its tag under `key`, `nonce` and
/// `aad`, and leaves it as it was otherwise.
pub fn open_in_place(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    data: &mut [u8],
    tag: &[u8],
) -> bool {
    if !ct_eq(&aead_tag(key, nonce, aad, data), tag) {
        return false;
    }
    chacha20_xor(key, 1, nonce, data);
    true
}

/// Randomness from the system alone: a key must not come from the clock,
/// as [`random_bytes`] falls back to.
pub fn system_random(n: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .ok()?
        .read_exact(&mut buf)
        .ok()?;
    Some(buf)
}

// ------------------------------------------------------------------- random

/// Cryptographic randomness. `/dev/urandom` on Unix; when that cannot be
/// read, the clock, the process id and a stack address are mixed with SHA-256.
pub fn random_bytes(n: usize) -> Vec<u8> {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let mut buf = vec![0u8; n];
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    let mut out = Vec::with_capacity(n);
    let mut counter: u64 = 0;
    while out.len() < n {
        let mut h = Sha256::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        h.update(&now.to_le_bytes());
        h.update(&std::process::id().to_le_bytes());
        h.update(&counter.to_le_bytes());
        let stack = &counter as *const u64 as usize;
        h.update(&stack.to_le_bytes());
        out.extend_from_slice(&h.finish());
        counter += 1;
    }
    out.truncate(n);
    out
}

/// A SCRAM nonce: a string in the base64 alphabet containing no `,` or `=`.
pub fn nonce(len: usize) -> String {
    b64_encode(&random_bytes(len)).replace('=', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    const SUNSCREEN: &[u8] = b"Ladies and Gentlemen of the class of '99: If I could offer you \
        only one tip for the future, sunscreen would be it.";

    /// RFC 8439, 2.4.2: ChaCha20 encryption.
    #[test]
    fn chacha20_rfc8439() {
        let key: [u8; 32] = (0u8..32).collect::<Vec<_>>().try_into().unwrap();
        let nonce: [u8; 12] = unhex("000000000000004a00000000").try_into().unwrap();
        let mut data = SUNSCREEN.to_vec();
        chacha20_xor(&key, 1, &nonce, &mut data);
        assert_eq!(
            data,
            unhex(
                "6e2e359a2568f98041ba0728dd0d6981e97e7aec1d4360c20a27afccfd9fae0b
                 f91b65c5524733ab8f593dabcd62b3571639d624e65152ab8f530c359f0861d8
                 07ca0dbf500d6a6156a38e088a22b65e52bc514d16ccf806818ce91ab7793736
                 5af90bbf74a35be6b40b8eedf2785e42874d"
            )
        );
    }

    /// RFC 8439, 2.5.2: Poly1305.
    #[test]
    fn poly1305_rfc8439() {
        let key: [u8; 32] =
            unhex("85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b")
                .try_into()
                .unwrap();
        assert_eq!(
            poly1305(&key, b"Cryptographic Forum Research Group").to_vec(),
            unhex("a8061dc1305136c6c22b8baf0c0127a9")
        );
    }

    /// RFC 8439, 2.8.2: the AEAD, and a tag that no longer matches.
    #[test]
    fn aead_rfc8439() {
        let key: [u8; 32] = (0x80u8..0xa0).collect::<Vec<_>>().try_into().unwrap();
        let nonce: [u8; 12] = unhex("070000004041424344454647").try_into().unwrap();
        let aad = unhex("50515253c0c1c2c3c4c5c6c7");
        let mut data = SUNSCREEN.to_vec();
        let tag = seal_in_place(&key, &nonce, &aad, &mut data);
        assert_eq!(
            data,
            unhex(
                "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d6
                 3dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b36
                 92ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc
                 3ff4def08e4b7a9de576d26586cec64b6116"
            )
        );
        assert_eq!(tag.to_vec(), unhex("1ae10b594f09e26a7e902ecbd0600691"));
        let mut back = data.clone();
        assert!(open_in_place(&key, &nonce, &aad, &mut back, &tag));
        assert_eq!(back, SUNSCREEN);
        // One bit of the text, of the associated data or of the tag.
        for (d, a, t) in [(0, 0, 0), (1, 0, 0), (0, 1, 0), (0, 0, 1)] {
            let (mut d2, mut a2, mut t2) = (data.clone(), aad.clone(), tag.to_vec());
            d2[5] ^= d;
            a2[3] ^= a;
            t2[15] ^= t;
            let ok = open_in_place(&key, &nonce, &a2, &mut d2, &t2);
            assert_eq!(ok, d + a + t == 0);
        }
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn sha256_vectors() {
        // FIPS 180-4 / NIST examples
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 1,000,000 x 'a' -- exercises the multi-block path and the length field
        let mut h = Sha256::new();
        for _ in 0..1000 {
            h.update(&[b'a'; 1000]);
        }
        assert_eq!(
            hex(&h.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
        // block boundaries
        for n in [55usize, 56, 63, 64, 65, 119, 120, 127, 128] {
            let data = vec![b'x'; n];
            let mut inc = Sha256::new();
            for c in data.chunks(7) {
                inc.update(c);
            }
            assert_eq!(inc.finish(), sha256(&data), "n={n}");
        }
    }

    #[test]
    fn hmac_vectors() {
        // RFC 4231, Test Case 1 and 2
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test Case 3: a key longer than the block size
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn pbkdf2_vector() {
        // RFC 7677 example value: password "pencil", salt "W22ZaJ0SNY7soEsUEjb6gQ==", i=4096
        let salt = b64_decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let sp = pbkdf2_sha256(b"pencil", &salt, 4096);
        assert_eq!(
            b64_encode(&sp),
            "xKSVEDI6tPlSysH6mUQZOeeOp01r6B3fcJbodRPcYV0="
        );
    }

    #[test]
    fn base64_roundtrip() {
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
        for n in 0..40 {
            let data: Vec<u8> = (0..n).map(|i| (i * 7 + 3) as u8).collect();
            assert_eq!(b64_decode(&b64_encode(&data)).unwrap(), data);
        }
        assert!(b64_decode("!!!").is_none());
    }

    #[test]
    fn random_is_not_constant() {
        assert_ne!(random_bytes(16), random_bytes(16));
        let n = nonce(18);
        assert!(!n.contains(',') && !n.contains('='));
    }
}
