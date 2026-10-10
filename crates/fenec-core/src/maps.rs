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

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

/// What builds an index map's hashers: `Fx` in the browser, std's random
/// keys everywhere else.
#[cfg(target_arch = "wasm32")]
pub type Keys = std::hash::BuildHasherDefault<Fx>;
#[cfg(not(target_arch = "wasm32"))]
pub type Keys = std::collections::hash_map::RandomState;

/// A map a derived index is built into.
pub type Map<K, V> = HashMap<K, V, Keys>;

/// A map whose growth is paid a share at a time. A map that outgrows its
/// table moves every key into one twice its size, in the one insert that
/// found it full and under whatever lock its caller holds: the `@unique`
/// index over a ledger's journal went from 1.8 to 3.7 million keys in one
/// transfer, 150 ms with every request on the server waiting for it, and a
/// `@hash` or `@unique` field of a value a row -- an email, an order's
/// reference, an idempotency key -- does that at every doubling, each
/// twice as long as the last: 7, 19, 49, 108 and 240 ms from 229 376 keys
/// to 3.7 million; a text index's terms 9-10, 23-25, 55-59, 125-132 and
/// 276-324 ms over the same doublings. So natively a map past [`SPLIT_AT`] keys
/// is split into [`SHARDS`] maps, and each grows on its own: a 256th of
/// the keys at a time.
///
/// Natively each key is kept beside its hash ([`Hashed`]), which its table
/// takes as it is ([`Pass`]): a lookup hashes the key once, with the map's
/// own random SipHash keys, and its shard is bits of that hash. Picked by
/// an `Fx` of the key, each shard then hashing it again with keys of its
/// own -- read from the shard, after the pick -- a lookup over a million
/// keys took 94 to 135 ns, over four million 121 to 144, against 68 to 87
/// and 83 to 93 now, under the split 33 to 42 against 24 to 34; the
/// index's build after an open is 15 to 25% quicker, and a table grows and
/// the split moves its keys with no key hashed again (the split's put 6 to
/// 8 ms -> 2.5 to 3.6). What it costs is 8 bytes a key's slot: 48 -> 56 a
/// hash index's, 80 -> 88 a text index's. Kept whole, the table was taken
/// step by step from the old one into a new one instead -- a lookup looking
/// in both until the steps were done, which a map that stops taking keys
/// never finishes -- and lookups were 163 ns and the build a third slower.
#[cfg(not(target_arch = "wasm32"))]
pub struct Sharded<K, V> {
    /// What every key of this map is hashed with, drawn as it is made.
    keys: Keys,
    one: Stored<K, V>,
    /// Empty until the one map is split; then every key is in one of them.
    shards: Vec<Stored<K, V>>,
}

/// The browser's has one thread, a page's data, and the one map it had.
#[cfg(target_arch = "wasm32")]
pub struct Sharded<K, V> {
    one: Map<K, V>,
}

/// A native [`Sharded`]'s tables: each key beside its hash.
#[cfg(not(target_arch = "wasm32"))]
type Stored<K, V> = HashMap<Hashed<K>, V, std::hash::BuildHasherDefault<Pass>>;

/// Keys at which a [`Sharded`] map is split: as many as a table of 2^17
/// buckets holds, moved once into the shards in 2.5 to 3.6 ms, 4.9 to 5.3
/// a text index's. Split at 57 344 keys the longest pause was half, and
/// maps of half the size paid the shard's pick.
#[cfg(not(target_arch = "wasm32"))]
const SPLIT_AT: usize = 114_688;

/// How many maps a split one becomes.
#[cfg(not(target_arch = "wasm32"))]
const SHARDS: usize = 256;

/// The shard of [`SHARDS`] a hash's key is in: the byte below the seven
/// bits a table tags its buckets with -- picked by the top byte, every key
/// of a shard had one tag, and each probe read every key it passed.
#[cfg(not(target_arch = "wasm32"))]
#[inline]
fn shard(hash: u64) -> usize {
    (hash >> 49) as usize & (SHARDS - 1)
}

impl<K, V> Default for Sharded<K, V> {
    fn default() -> Self {
        Sharded {
            #[cfg(not(target_arch = "wasm32"))]
            keys: Keys::default(),
            one: Default::default(),
            #[cfg(not(target_arch = "wasm32"))]
            shards: Vec::new(),
        }
    }
}

/// A key and its hash, as a native [`Sharded`] keeps them.
#[cfg(not(target_arch = "wasm32"))]
pub struct Hashed<K> {
    hash: u64,
    key: K,
}

/// What a key is looked up by: its hash and a form of it -- a `Hashed`
/// key, or the hash beside the form a caller holds (`&str` for a `String`).
#[cfg(not(target_arch = "wasm32"))]
trait Probe<Q: ?Sized> {
    fn hash(&self) -> u64;
    fn key(&self) -> &Q;
}

#[cfg(not(target_arch = "wasm32"))]
impl<K: Borrow<Q>, Q: ?Sized> Probe<Q> for Hashed<K> {
    fn hash(&self) -> u64 {
        self.hash
    }
    fn key(&self) -> &Q {
        self.key.borrow()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<Q: ?Sized> Probe<Q> for (u64, &Q) {
    fn hash(&self) -> u64 {
        self.0
    }
    fn key(&self) -> &Q {
        self.1
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<'a, K: Borrow<Q> + 'a, Q: ?Sized + 'a> Borrow<dyn Probe<Q> + 'a> for Hashed<K> {
    fn borrow(&self) -> &(dyn Probe<Q> + 'a) {
        self
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<Q: ?Sized + Eq> PartialEq for dyn Probe<Q> + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.hash() == other.hash() && self.key() == other.key()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<Q: ?Sized + Eq> Eq for dyn Probe<Q> + '_ {}

#[cfg(not(target_arch = "wasm32"))]
impl<Q: ?Sized> Hash for dyn Probe<Q> + '_ {
    fn hash<H: Hasher>(&self, h: &mut H) {
        h.write_u64(self.hash())
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<K: Eq> PartialEq for Hashed<K> {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash && self.key == other.key
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<K: Eq> Eq for Hashed<K> {}

#[cfg(not(target_arch = "wasm32"))]
impl<K> Hash for Hashed<K> {
    fn hash<H: Hasher>(&self, h: &mut H) {
        h.write_u64(self.hash)
    }
}

/// A hash taken as it was written: a [`Hashed`] key's.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub struct Pass(u64);

#[cfg(not(target_arch = "wasm32"))]
impl Hasher for Pass {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = self.0.rotate_left(8) ^ b as u64;
        }
    }
    fn write_u64(&mut self, i: u64) {
        self.0 = i;
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

// A key is looked up as whatever form of it the caller holds -- the hash
// index's `&Vec<u8>`, the text index's `&str` -- and each form a caller
// passes is a copy of the search: the hash index takes its keys as the
// `Vec<u8>` the map holds for that reason, one copy rather than two.
#[cfg(not(target_arch = "wasm32"))]
impl<K: Hash + Eq, V> Sharded<K, V> {
    #[inline]
    fn hash<Q: Hash + ?Sized>(&self, key: &Q) -> u64 {
        use std::hash::BuildHasher;
        self.keys.hash_one(key)
    }

    /// The map a hash's key is in, or would go in.
    #[inline]
    fn of(&self, hash: u64) -> &Stored<K, V> {
        match self.shards.is_empty() {
            true => &self.one,
            false => &self.shards[shard(hash)],
        }
    }

    #[inline]
    fn of_mut(&mut self, hash: u64) -> &mut Stored<K, V> {
        match self.shards.is_empty() {
            true => &mut self.one,
            false => &mut self.shards[shard(hash)],
        }
    }

    #[inline]
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = self.hash(key);
        self.of(hash).get(&(hash, key) as &dyn Probe<Q>)
    }

    #[inline]
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = self.hash(key);
        self.of_mut(hash).get_mut(&(hash, key) as &dyn Probe<Q>)
    }

    /// `key`'s value, made where it has none; the one map split first once
    /// it holds [`SPLIT_AT`] keys.
    #[inline]
    pub fn or_default(&mut self, key: K) -> &mut V
    where
        V: Default,
    {
        self.room();
        let hash = self.hash(&key);
        self.of_mut(hash).entry(Hashed { hash, key }).or_default()
    }

    #[inline]
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.room();
        let hash = self.hash(&key);
        self.of_mut(hash).insert(Hashed { hash, key }, value)
    }

    #[inline]
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = self.hash(key);
        self.of_mut(hash).remove(&(hash, key) as &dyn Probe<Q>)
    }

    #[inline]
    pub fn remove_entry<Q>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = self.hash(key);
        let (k, v) = self
            .of_mut(hash)
            .remove_entry(&(hash, key) as &dyn Probe<Q>)?;
        Some((k.key, v))
    }

    #[inline]
    fn room(&mut self) {
        if self.shards.is_empty() && self.one.len() >= SPLIT_AT {
            self.split();
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.one.len() + self.shards.iter().map(HashMap::len).sum::<usize>()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The keys the tables have room for.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.one.capacity() + self.shards.iter().map(HashMap::capacity).sum::<usize>()
    }

    /// What the tables hold in memory: a slot each key they have room for,
    /// its hash beside it, and a byte of the table's own.
    pub fn table_bytes(&self) -> usize {
        self.capacity() * (std::mem::size_of::<(Hashed<K>, V)>() + 1)
    }

    /// Every key and its value, in no order.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        (self.one.iter())
            .chain(self.shards.iter().flat_map(|m| m.iter()))
            .map(|(k, v)| (&k.key, v))
    }

    /// Every key and its value, in no order.
    #[inline]
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&K, &mut V)> {
        (self.one.iter_mut())
            .chain(self.shards.iter_mut().flat_map(|m| m.iter_mut()))
            .map(|(k, v)| (&k.key, v))
    }

    /// Every value, in no order.
    #[inline]
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }

    /// Each table no larger than its keys need.
    pub fn shrink_to_fit(&mut self) {
        self.one.shrink_to_fit();
        for m in &mut self.shards {
            m.shrink_to_fit();
        }
    }

    /// Every key gone, and the one map again.
    pub fn clear(&mut self) {
        self.one.clear();
        self.shards = Vec::new();
    }

    /// The one map's keys moved into the shards, each with room for twice
    /// its share, as the one map would have grown -- by the hashes they
    /// hold, none taken again.
    #[cold]
    fn split(&mut self) {
        let room = 2 * self.one.len() / SHARDS;
        let mut shards: Vec<Stored<K, V>> = (0..SHARDS)
            .map(|_| HashMap::with_capacity_and_hasher(room, Default::default()))
            .collect();
        for (k, v) in std::mem::take(&mut self.one) {
            shards[shard(k.hash)].insert(k, v);
        }
        self.shards = shards;
    }
}

#[cfg(target_arch = "wasm32")]
impl<K: Hash + Eq, V> Sharded<K, V> {
    #[inline]
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.one.get(key)
    }

    #[inline]
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.one.get_mut(key)
    }

    #[inline]
    pub fn or_default(&mut self, key: K) -> &mut V
    where
        V: Default,
    {
        self.one.entry(key).or_default()
    }

    #[inline]
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.one.insert(key, value)
    }

    #[inline]
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.one.remove(key)
    }

    #[inline]
    pub fn remove_entry<Q>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.one.remove_entry(key)
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.one.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.one.is_empty()
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.one.capacity()
    }

    pub fn table_bytes(&self) -> usize {
        self.capacity() * (std::mem::size_of::<(K, V)>() + 1)
    }

    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.one.iter()
    }

    #[inline]
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&K, &mut V)> {
        self.one.iter_mut()
    }

    #[inline]
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.one.values()
    }

    #[inline]
    pub fn shrink_to_fit(&mut self) {
        self.one.shrink_to_fit();
    }

    #[inline]
    pub fn clear(&mut self) {
        self.one.clear();
    }
}

/// A word at a time: the running hash turned five bits, the word mixed in,
/// the whole multiplied by an odd constant.
#[derive(Default)]
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

    /// Past the split every key is found where it went, each grows a shard
    /// at a time -- no insert makes room for more than a few shares of the
    /// keys, where one map's made room for all of them at once -- and the
    /// shards take the keys an index holds evenly, encoded values that
    /// differ only in their last bytes among them.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_sharded_map_grows_a_share_at_a_time() {
        const N: u32 = 300_000;
        let key = |i: u32| format!("{i}:dr").into_bytes();
        let mut m: Sharded<Vec<u8>, u32> = Sharded::default();
        let mut one: Map<Vec<u8>, u32> = Map::default();
        let (mut most, mut most_one) = (0, 0);
        for i in 0..N {
            let (before, before_one) = (m.capacity(), one.capacity());
            *m.or_default(key(i)) = i;
            one.insert(key(i), i);
            // The split moves what the one map held, 114 688 keys, once.
            if m.len() > SPLIT_AT + 1 {
                most = most.max(m.capacity() - before);
            }
            most_one = most_one.max(one.capacity() - before_one);
        }
        assert_eq!(m.len(), N as usize);
        assert!(most_one > 200_000, "{most_one}");
        assert!(most < 4 * N as usize / SHARDS, "{most}");
        for i in (0..N).step_by(7) {
            assert_eq!(m.get(&key(i)), Some(&i));
        }
        assert_eq!(m.get(&key(N)), None);
        let mut seen: Vec<u32> = m.iter().map(|(_, &v)| v).collect();
        seen.sort_unstable();
        assert!(seen.iter().copied().eq(0..N));
        let sizes: Vec<usize> = m.shards.iter().map(|s| s.len()).collect();
        let (lo, hi) = (sizes.iter().min().unwrap(), sizes.iter().max().unwrap());
        assert!(*lo > 870 && *hi < 1_480, "{lo} to {hi}");

        // Removals, updates and keys put back answer as a plain map's, and
        // keys that come and go -- idempotency keys past their time -- leave
        // the shards no bigger than the keys they hold need.
        let mut seed = 0x9e37_79b9_u64;
        for i in 0..N {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let j = (seed >> 33) as u32 % N;
            match i % 3 {
                0 => assert_eq!(m.remove_entry(&key(j)), one.remove_entry(&key(j))),
                1 => {
                    if let (Some(a), Some(b)) = (m.get_mut(&key(j)), one.get_mut(&key(j))) {
                        *a += 1;
                        *b += 1;
                    }
                }
                _ => {
                    *m.or_default(key(j)) += 2;
                    *one.entry(key(j)).or_default() += 2;
                }
            }
            assert_eq!(m.get(&key(j)), one.get(&key(j)));
        }
        assert_eq!(m.len(), one.len());
        let mut seen: Vec<(&Vec<u8>, &u32)> = m.iter().collect();
        let mut want: Vec<(&Vec<u8>, &u32)> = one.iter().collect();
        seen.sort_unstable();
        want.sort_unstable();
        assert_eq!(seen, want);
        for i in N..N + N / 2 {
            let gone = m.iter().next().map(|(k, _)| k.clone()).unwrap();
            assert!(m.remove_entry(&gone).is_some());
            *m.or_default(key(i)) = i;
        }
        assert_eq!(m.len(), one.len());
        assert!(m.capacity() < 4 * m.len(), "{} {}", m.capacity(), m.len());
    }
}
