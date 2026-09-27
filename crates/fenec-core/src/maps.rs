//! The hash maps the derived indexes are built into.
//!
//! In the browser, std's SipHash takes its keys from where a value on the
//! stack and one on the heap lie -- wasm32-unknown-unknown has no source of
//! randomness -- and those are the same in every page, so it keeps out
//! nothing there that a fixed hash lets in. The browser's maps hash a word
//! at a time with a rotate and a multiply instead (`Fx`, as rustc hashes):
//! the first `match` over 20 000 short texts builds its index in 7.5 ms
//! against 8.0, a hash index 3.12 against 3.45, for 69 bytes brotli. A
//! server keeps SipHash, keyed at random, against keys written to collide.

use std::collections::HashMap;
use std::hash::Hasher;

/// What builds an index map's hashers: `Fx` in the browser, std's random
/// keys everywhere else.
#[cfg(target_arch = "wasm32")]
pub type Keys = std::hash::BuildHasherDefault<Fx>;
#[cfg(not(target_arch = "wasm32"))]
pub type Keys = std::collections::hash_map::RandomState;

/// A map a derived index is built into.
pub type Map<K, V> = HashMap<K, V, Keys>;

/// A word at a time: the running hash turned five bits, the word mixed in,
/// the whole multiplied by an odd constant.
#[derive(Default)]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub struct Fx(u64);

impl Fx {
    #[inline]
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

impl Hasher for Fx {
    fn write(&mut self, mut bytes: &[u8]) {
        while let Some((word, rest)) = bytes.split_first_chunk::<8>() {
            self.add(u64::from_le_bytes(*word));
            bytes = rest;
        }
        if !bytes.is_empty() {
            let mut word = [0u8; 8];
            word[..bytes.len()].copy_from_slice(bytes);
            self.add(u64::from_le_bytes(word));
        }
    }
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    /// Turned half way round. On wasm32 the map takes a key's bucket and
    /// its tag both from the low 32 bits, and a product's low bits hear only
    /// the low bits of what was multiplied: encoded values that differ past
    /// their fourth byte all fell in one bucket, and the hash index was
    /// built slower than under SipHash. The top half hears every bit.
    fn finish(&self) -> u64 {
        self.0.rotate_left(32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::BuildHasherDefault;

    /// The browser's hash, as a map uses it: every key found again, and
    /// the keys an index holds -- terms, encoded values, ids -- spread over
    /// the tags the map sorts them by.
    #[test]
    fn fx_holds_what_a_map_holds() {
        let mut terms: HashMap<String, u32, BuildHasherDefault<Fx>> = HashMap::default();
        let mut ids: HashMap<u64, u32, BuildHasherDefault<Fx>> = HashMap::default();
        for i in 0..100_000u32 {
            terms.insert(format!("word{i}"), i);
            ids.insert(i as u64 * 7, i);
        }
        for i in 0..100_000u32 {
            assert_eq!(terms.get(&format!("word{i}")), Some(&i));
            assert_eq!(ids.get(&(i as u64 * 7)), Some(&i));
        }
        assert_eq!(terms.get("word100000"), None);
        // On wasm32 a bucket and its tag are the low 32 bits: keys that
        // differ only past their fourth byte, as encoded values do, spread
        // over every tag and every bucket of a small table.
        let (mut tags, mut buckets) = ([0u32; 128], [0u32; 64]);
        for i in 0..10_000u32 {
            let mut h = Fx::default();
            std::hash::Hash::hash(&format!("k{:04}", 1000 + i % 9000).into_bytes(), &mut h);
            let low = h.finish() as u32;
            tags[(low >> 25) as usize] += 1;
            buckets[(low & 63) as usize] += 1;
        }
        assert!(tags.iter().all(|&n| n > 30), "{tags:?}");
        assert!(buckets.iter().all(|&n| n > 80), "{buckets:?}");
    }
}
