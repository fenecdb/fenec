//! The words of a text index and the documents holding each, in as few
//! bytes as a word's documents need.
//!
//! **Each word was a slot of 88 bytes and three allocations.** A map of
//! `String` to two `Vec`s, its hash beside it: a tenant's 200 notes of 30
//! made-up words, 5 983 words each in one note, took 997 KB in musl's slots
//! (818 KB asked for) -- the table's 8 192 slots 729 KB of it, every word's
//! bytes, document and count three allocations of 16 bytes -- for 40 KB of
//! text, where its hash and ordered indexes took 4 KB together. Now a word
//! is 9 bytes of a table -- a control byte, and a slot naming where its
//! term is under half its hash, compared before the bytes and what places
//! it again as the table grows, no word hashed again -- its bytes in an
//! arena after their length, and a term. A word one document holds --
//! three in four of a tenant's notes over a Zipf vocabulary, all but a few
//! of made-up words -- is a [`One`], 16 bytes holding that document and its
//! count, with no allocation of its own; a word of more is a [`List`], which
//! the slot names directly, as the map's slot held one. The tenant of
//! made-up words takes 230 KB in musl's slots, one over a Zipf vocabulary
//! 174 against 504; in a `fenec-server --dir` node in Docker, 500 tenants'
//! indexes held 225 and 192 KB of anonymous memory each, against 1 135 and
//! 565.
//!
//! **A word that goes takes nothing with it but its slot.** Its place in
//! its array is left to the next word (`free`, `spare`), and its bytes are
//! dead in the arena, which is written again once half of it is --
//! amortised over the half that died, and a shard's at a time. A slot that
//! goes moves the ones after it back rather than leave a tombstone, so a
//! table words come and go through stays as short to walk as one they only
//! came to. Where the index is known complete -- its build, `create index`
//! -- the places are closed up and every array shrunk to its words.
//!
//! **Past [`SPLIT_AT`] words natively, 256 shards each grow on their own**,
//! as `maps::Sharded` does a hash index's keys: a table, an array or an
//! arena outgrowing itself moves a 256th of the words, not all of them,
//! under the write lock. A shard is the top byte of a word's hash, which
//! its slot keeps, so the split hashes nothing again either. The browser
//! keeps one, a page's data.

use crate::maps::Keys;
use crate::value::DocId;
use std::hash::BuildHasher;

/// Words at which the one shard natively is split: as many as a table of
/// 2^17 slots holds at three quarters.
#[cfg(not(target_arch = "wasm32"))]
const SPLIT_AT: usize = 98_304;

/// Bytes of words at which the one shard is split whatever its words'
/// number: a word's place in its arena is 32 bits, and 256 shards of 4 GiB
/// each are a terabyte of words.
#[cfg(not(target_arch = "wasm32"))]
const SPLIT_BYTES: usize = 1 << 31;

/// How many shards a split one becomes: a hash's top byte.
#[cfg(not(target_arch = "wasm32"))]
const SHARDS: usize = 256;

/// A term's documents as a walk reads them: ids ascending, each one's count
/// at its place beside it, and the largest count, the MaxScore ceiling's.
#[derive(Clone, Copy)]
pub(super) struct Postings<'a> {
    pub docs: &'a [DocId],
    pub tfs: &'a [u32],
    /// Only ever raised while more than one document holds the term. A
    /// removal can leave it too high, which costs a looser bound and never
    /// a wrong answer; a rebuild makes it exact again.
    pub max_tf: u32,
}

impl Postings<'_> {
    pub fn len(&self) -> usize {
        self.docs.len()
    }
}

/// A term one document holds: where its word is in its shard's arena, the
/// document and how often it holds the word -- 0 for a place free.
#[derive(Clone, Copy)]
struct One {
    at: u32,
    tf: u32,
    doc: DocId,
}

/// The documents of a term more than one holds, as parallel arrays, and
/// where its word is in its shard's arena (0 for a list a loose `match`
/// made of others').
///
/// A `Vec<(DocId, u32)>` is the obvious shape and the wrong one: the tuple
/// pads to 16 bytes, a quarter of it for nothing. Split, the same postings
/// take 12, and the merge -- which reads ids and touches a count only on a
/// hit -- walks a contiguous run of ids instead of striding over the counts.
#[derive(Default)]
pub(super) struct List {
    pub docs: Vec<DocId>,
    pub tfs: Vec<u32>,
    pub max_tf: u32,
    at: u32,
}

impl List {
    /// An empty list with room for `n` documents.
    pub fn with_capacity(n: usize) -> List {
        List {
            docs: Vec::with_capacity(n),
            tfs: Vec::with_capacity(n),
            ..List::default()
        }
    }

    pub fn view(&self) -> Postings<'_> {
        Postings {
            docs: &self.docs,
            tfs: &self.tfs,
            max_tf: self.max_tf,
        }
    }

    /// One more of the term in `doc`: its entry counted up, or a new one of
    /// 1 where the order puts it. A document's terms come one at a time, so
    /// its entry is the last, but an older document's, updated, is not.
    #[inline]
    fn bump(&mut self, doc: DocId) {
        match self.docs.last() {
            Some(&last) if last < doc => {
                self.docs.push(doc);
                self.tfs.push(1);
                self.max_tf = self.max_tf.max(1);
            }
            Some(&last) if last == doc => {
                if let Some(tf) = self.tfs.last_mut() {
                    *tf += 1;
                    self.max_tf = self.max_tf.max(*tf);
                }
            }
            _ => self.bump_held(doc),
        }
    }

    /// [`List::bump`] where `doc` is before the last document: an older
    /// document's, written again.
    fn bump_held(&mut self, doc: DocId) {
        let at = match self.docs.binary_search(&doc) {
            Ok(at) => at,
            Err(at) => {
                self.docs.insert(at, doc);
                self.tfs.insert(at, 0);
                at
            }
        };
        if let Some(tf) = self.tfs.get_mut(at) {
            *tf += 1;
            self.max_tf = self.max_tf.max(*tf);
        }
    }

    fn drop_doc(&mut self, doc: DocId) {
        if let Ok(at) = self.docs.binary_search(&doc) {
            self.docs.remove(at);
            self.tfs.remove(at);
        }
    }

    /// What its arrays hold, and what each allocation costs past what it
    /// asks for (`ALLOC`).
    fn bytes(&self) -> usize {
        bytes_of(self.docs.capacity(), self.tfs.capacity())
    }

    fn shrink(&mut self) {
        self.docs.shrink_to_fit();
        self.tfs.shrink_to_fit();
    }
}

/// The most documents a spare list keeps room for.
const SPARE_ROOM: usize = 16;

/// What an allocation of its own costs past the bytes it asks for, as
/// [`Terms::bytes`] counts a list's arrays: a small one is rounded up to 16
/// bytes and carries a header -- 4 bytes in musl's mallocng, 8 in glibc's --
/// so a list of two documents asks for 24 bytes and takes 48.
const ALLOC: usize = 12;

/// What a list's arrays of room for `docs` ids and `tfs` counts hold.
#[inline]
fn bytes_of(docs: usize, tfs: usize) -> usize {
    let held = |n: usize| if n == 0 { 0 } else { n + ALLOC };
    held(docs * std::mem::size_of::<DocId>()) + held(tfs * 4)
}

/// A share of the words: the table that finds them, their terms, their
/// bytes.
#[derive(Default)]
struct Shard {
    /// Probed a group at a time from a word's hash: a slot is its term's
    /// place, in `ones` or in `lists` ([`slot`]), under its tag; 0 is none.
    slots: Vec<u64>,
    /// A byte a slot: 7 bits of its word's hash ([`Hash::h2`]), or [`EMPTY`].
    ctrl: Vec<u8>,
    ones: Vec<One>,
    /// Places of `ones` free for the next word: a word that leaves, or
    /// passes to a list, leaves its place to the next. The last moved into
    /// it, its slot found by its word's hash again, was a hash and a probe
    /// more a removal: the browser's update of a note of 12 made-up words
    /// took 12.7-13.2 us against 11.5-11.7.
    free: Vec<u32>,
    lists: Vec<List>,
    /// Places of `lists` free, each list empty with its arrays kept for the
    /// next word passing to two documents: an update takes a word of two
    /// documents to one and back, and a list let go of and made again was
    /// two allocations, and the last list moved into its place a hash: the
    /// browser's update of a note took 11.5-11.7 us against 11.0-11.4.
    spare: Vec<u32>,
    /// Each term's word after its length, a varint.
    bytes: Vec<u8>,
    /// Bytes of `bytes` no term holds any more.
    dead: usize,
    /// What the lists' arrays hold, as [`List::bytes`] counts it, kept as
    /// they change: summed over every list it would walk every term, and
    /// `--max-memory` asks before every write.
    heap: usize,
}

/// What a word's hash is read as.
#[derive(Clone, Copy)]
struct Hash {
    /// Half of it, which its slot keeps: where its probe starts, its
    /// shard natively (the top byte), and what a probe compares first.
    tag: u32,
    /// Seven bits of the other half, its control byte.
    h2: u8,
}

impl Hash {
    /// Natively SipHash, every bit mixed: the top half the tag, the low
    /// seven bits the control byte. The browser's `Fx` turns its mixed
    /// half low as it finishes, so that half is the tag there, and its
    /// control byte the tag's top seven bits, which no table of a page
    /// reaches.
    #[inline]
    fn of(h: u64) -> Hash {
        match cfg!(target_arch = "wasm32") {
            true => Hash {
                tag: h as u32,
                h2: (h >> 25) as u8 & 0x7f,
            },
            false => Hash {
                tag: (h >> 32) as u32,
                h2: h as u8 & 0x7f,
            },
        }
    }
}

/// `w`'s hash under `keys`, hashed as the text it is: as bytes, the stop
/// a `str` ends with, or a slice's length, went through the hasher's
/// general path, a call more a word.
#[inline]
fn hash_of(keys: &Keys, w: &str) -> Hash {
    Hash::of(keys.hash_one(w))
}

/// The hash of a word held, as [`hash_of`] took it: its bytes came from a
/// `str`.
#[cfg(test)]
fn hash_held(keys: &Keys, w: &[u8]) -> Hash {
    hash_of(keys, std::str::from_utf8(w).unwrap_or(""))
}

/// A slot's control byte while it is empty; a slot held has a byte below
/// it.
const EMPTY: u8 = 0x80;

/// Slots a probe reads at once, as the map's table did: their control
/// bytes as one word, each byte holding `h2` found without a branch, so
/// where in its group a word lies decides nothing. Probed a slot at a time,
/// a word past its own slot cost a loop's exit mispredicted: the build of
/// `make search-bench`'s 100 000 documents over 5 000 words took 149 ms
/// against the map's 142, and takes 142 by groups -- though FiQA, whose
/// commonest words sit in their own slots, built in 505 ms a slot at a time
/// against 536 by groups and the map's 551. A group's tags compared one by
/// one rather than its control bytes took 171.
const GROUP: usize = 8;

/// Each byte of a word.
const LO: u64 = 0x0101_0101_0101_0101;
/// Each byte's top bit.
const HI: u64 = 0x8080_8080_8080_8080;

/// Where a probe for the word slot `v` names starts: the first slot of
/// the group its tag falls in.
#[inline]
fn home(v: u64, mask: usize) -> usize {
    (v >> 32) as usize & mask & !(GROUP - 1)
}

/// Where a term is, as its slot names it: its place in `ones` or, `list`,
/// in `lists`.
#[derive(Clone, Copy, PartialEq)]
struct Place {
    i: usize,
    list: bool,
}

/// The slot of the term at `p` under `tag`: the place twice and whether it
/// is a list's, and one, so that 0 is none.
#[inline]
fn slot(tag: u32, p: Place) -> u64 {
    ((tag as u64) << 32) | (((p.i as u64) << 1 | p.list as u64) + 1)
}

/// The place slot `v` names.
#[inline]
fn place_of(v: u64) -> Place {
    let r = v as u32 - 1;
    Place {
        i: (r >> 1) as usize,
        list: r & 1 == 1,
    }
}

/// Whether `n` words fit `slots` slots: three quarters at most.
#[inline]
fn fits(n: usize, slots: usize) -> bool {
    n * 4 <= slots * 3
}

/// The fewest slots that fit `n` words: a group at least.
fn slots_for(n: usize) -> usize {
    match n {
        0 => 0,
        _ => {
            let mut s = GROUP;
            while !fits(n, s) {
                s *= 2;
            }
            s
        }
    }
}

/// The word whose length starts at `at`: its bytes, and where they end.
#[inline]
fn word_at(bytes: &[u8], at: usize) -> (&[u8], usize) {
    let (mut n, mut shift, mut i) = (0usize, 0, at);
    while let Some(&b) = bytes.get(i) {
        i += 1;
        n |= ((b & 0x7f) as usize) << shift;
        if b < 0x80 {
            break;
        }
        shift += 7;
    }
    (bytes.get(i..i + n).unwrap_or(&[]), i + n)
}

/// A word looked for, as a probe compares it: its length and first seven
/// bytes as the arena holds them, one word to compare against eight
/// bytes read from where a term's word starts, masked to the word's.
/// Compared a byte at a time -- or through `memcmp` -- a lookup took a
/// branch on the length at every candidate, mispredicted at every word.
struct Key<'a> {
    w: &'a [u8],
    head: u64,
    mask: u64,
}

impl<'a> Key<'a> {
    #[inline(always)]
    fn of(w: &'a [u8]) -> Key<'a> {
        let mut head = w.len() as u64 & 0x7f;
        for (i, &b) in w.iter().take(7).enumerate() {
            head |= (b as u64) << (8 * (i + 1));
        }
        let mask = match w.len() {
            7.. => !0,
            n => (1 << (8 * (n + 1))) - 1,
        };
        Key { w, head, mask }
    }

    /// Whether the word whose length starts at `at` is this one.
    #[inline(always)]
    fn is_at(&self, bytes: &[u8], at: usize) -> bool {
        if self.w.len() < 0x80 {
            if let Some(&c) = bytes.get(at..).and_then(<[u8]>::first_chunk::<8>) {
                return (u64::from_le_bytes(c) ^ self.head) & self.mask == 0
                    && (self.w.len() <= 7
                        || bytes.get(at + 8..at + 1 + self.w.len()) == self.w.get(7..));
            }
        }
        word_at(bytes, at).0 == self.w
    }
}

impl Shard {
    #[cfg(not(target_arch = "wasm32"))]
    fn with_room(words: usize, bytes: usize) -> Shard {
        Shard {
            slots: vec![0; slots_for(words)],
            ctrl: vec![EMPTY; slots_for(words)],
            ones: Vec::with_capacity(words),
            bytes: Vec::with_capacity(bytes),
            ..Shard::default()
        }
    }

    fn len(&self) -> usize {
        self.ones.len() - self.free.len() + self.lists.len() - self.spare.len()
    }

    /// Where the word of the term at `p` starts.
    #[inline]
    fn at(&self, p: Place) -> usize {
        match p.list {
            true => self.lists[p.i].at as usize,
            false => self.ones[p.i].at as usize,
        }
    }

    #[cfg(test)]
    fn word(&self, p: Place) -> &[u8] {
        word_at(&self.bytes, self.at(p)).0
    }

    /// The slot holding `w`, whose hash is `h`, or the first empty one its
    /// probe met, where `w` would go. The table has a group of slots at
    /// least, and an empty one. In line natively, where it is the write's
    /// and the lookup's whole cost; the browser module keeps one copy.
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    fn find(&self, h: Hash, w: &[u8]) -> Result<usize, usize> {
        let mask = self.slots.len() - 1;
        let want = LO * h.h2 as u64;
        let key = Key::of(w);
        let mut g = home((h.tag as u64) << 32, mask);
        loop {
            let Some(&c) = self.ctrl.get(g..).and_then(<[u8]>::first_chunk::<GROUP>) else {
                return Err(g);
            };
            let c = u64::from_le_bytes(c);
            // A byte of `x` is 0 where the slot holds `h2`; a byte past one
            // that is may read as 0 too, which the tag then turns away.
            let x = c ^ want;
            let mut hit = x.wrapping_sub(LO) & !x & HI;
            while hit != 0 {
                let s = g + (hit.trailing_zeros() / 8) as usize;
                let v = self.slots[s];
                if (v >> 32) as u32 == h.tag && key.is_at(&self.bytes, self.at(place_of(v))) {
                    return Ok(s);
                }
                hit &= hit - 1;
            }
            let empty = c & HI;
            if empty != 0 {
                return Err(g + (empty.trailing_zeros() / 8) as usize);
            }
            g = (g + GROUP) & mask;
        }
    }

    /// The documents of the term in slot `s`.
    #[inline]
    fn postings(&self, s: usize) -> Postings<'_> {
        let p = place_of(self.slots[s]);
        match p.list {
            true => self.lists[p.i].view(),
            false => {
                let t = &self.ones[p.i];
                Postings {
                    docs: std::slice::from_ref(&t.doc),
                    tfs: std::slice::from_ref(&t.tf),
                    max_tf: t.tf,
                }
            }
        }
    }

    /// The table made `n` slots, each word placed again by its tag.
    fn resize(&mut self, n: usize) {
        let old = std::mem::replace(&mut self.slots, vec![0; n]);
        let ctrl = std::mem::replace(&mut self.ctrl, vec![EMPTY; n]);
        for (v, c) in old.into_iter().zip(ctrl) {
            if v != 0 {
                self.place(v, c);
            }
        }
    }

    /// Slot `v` and its control byte `c` put in the first empty slot from
    /// its group's first: every slot between taken, so a probe reaches it.
    fn place(&mut self, v: u64, c: u8) {
        let mask = self.slots.len() - 1;
        let mut s = home(v, mask);
        while self.slots[s] != 0 {
            s = (s + 1) & mask;
        }
        self.slots[s] = v;
        self.ctrl[s] = c;
    }

    /// `w`'s bytes appended, and where they start.
    fn put_word(&mut self, w: &[u8]) -> u32 {
        let at = self.bytes.len();
        // A shard natively splits before its arena passes 2 GiB, and 256 of
        // 4 GiB each are a terabyte of words; the browser's memory is 4 GiB.
        assert!(
            at + w.len() + 5 <= u32::MAX as usize,
            "a shard of words past 4 GiB"
        );
        let mut n = w.len();
        while n >= 0x80 {
            self.bytes.push(n as u8 | 0x80);
            n >>= 7;
        }
        self.bytes.push(n as u8);
        self.bytes.extend_from_slice(w);
        at as u32
    }

    /// One more of the term in slot `s` in `doc`.
    #[inline(always)]
    fn bump(&mut self, s: usize, doc: DocId) {
        let p = place_of(self.slots[s]);
        if p.list {
            let l = &mut self.lists[p.i];
            // Counted again only where an array grew.
            let held = (l.docs.capacity(), l.tfs.capacity());
            l.bump(doc);
            if (l.docs.capacity(), l.tfs.capacity()) != held {
                self.heap = self.heap + l.bytes() - bytes_of(held.0, held.1);
            }
            return;
        }
        let t = &mut self.ones[p.i];
        match t.doc == doc {
            true => t.tf += 1,
            false => self.second(s, doc),
        }
    }

    /// `w`, new, as `doc`'s once, into the empty slot `s` its probe ended
    /// at -- or, where it would fill the table past three quarters, into
    /// one twice the size.
    #[inline(never)]
    fn add(&mut self, h: Hash, w: &[u8], doc: DocId, mut s: usize) {
        if !fits(self.len() + 1, self.slots.len()) {
            self.resize((self.slots.len() * 2).max(GROUP));
            s = self.find(h, w).unwrap_or_else(|s| s);
        }
        let at = self.put_word(w);
        let p = self.hold(One { at, tf: 1, doc });
        self.slots[s] = slot(h.tag, p);
        self.ctrl[s] = h.h2;
    }

    /// A place of `ones` for `t`, a free one first.
    fn hold(&mut self, t: One) -> Place {
        let i = match self.free.pop() {
            Some(i) => {
                self.ones[i as usize] = t;
                i as usize
            }
            None => {
                self.ones.push(t);
                self.ones.len() - 1
            }
        };
        Place { i, list: false }
    }

    /// The place of `ones` at `i`, free.
    fn release(&mut self, i: usize) {
        self.ones[i].tf = 0;
        self.free.push(i as u32);
    }

    /// A second document for the term in slot `s`: the two into a list of
    /// their own, a spare one first.
    #[inline(never)]
    fn second(&mut self, s: usize, doc: DocId) {
        let v = self.slots[s];
        let p = place_of(v);
        let t = self.ones[p.i];
        let i = match self.spare.pop() {
            Some(i) => i as usize,
            None => {
                let l = List::with_capacity(2);
                self.heap += l.bytes();
                self.lists.push(l);
                self.lists.len() - 1
            }
        };
        let l = &mut self.lists[i];
        let held = l.bytes();
        (l.at, l.max_tf) = (t.at, t.tf);
        l.docs.push(t.doc);
        l.tfs.push(t.tf);
        l.bump(doc);
        self.heap = self.heap + l.bytes() - held;
        self.slots[s] = slot((v >> 32) as u32, Place { i, list: true });
        self.release(p.i);
    }

    /// The term in slot `s` no longer in `doc`; whether that left it in
    /// none, and so out of the shard.
    fn drop_doc(&mut self, s: usize, doc: DocId) -> bool {
        let p = place_of(self.slots[s]);
        if !p.list {
            if self.ones[p.i].doc != doc {
                return false;
            }
            self.close(s);
            let (_, end) = word_at(&self.bytes, self.ones[p.i].at as usize);
            self.dead += end - self.ones[p.i].at as usize;
            self.release(p.i);
            if self.dead * 2 > self.bytes.len() {
                self.compact();
            }
            return true;
        }
        // A removal never shrinks an array, so the count stands.
        let l = &mut self.lists[p.i];
        l.drop_doc(doc);
        // Down to one document, the term is a `One` again, and its list
        // spare.
        if l.docs.len() == 1 {
            let one = One {
                at: l.at,
                tf: l.tfs[0],
                doc: l.docs[0],
            };
            self.spare_list(p.i);
            let to = self.hold(one);
            self.slots[s] = slot((self.slots[s] >> 32) as u32, to);
        }
        false
    }

    /// The list at `i` emptied and its place free, its arrays kept while
    /// they are small: one of a word thousands of documents held went
    /// down to, kept, would hold its room for a word of two.
    fn spare_list(&mut self, i: usize) {
        let l = &mut self.lists[i];
        l.docs.clear();
        l.tfs.clear();
        l.max_tf = 0;
        if l.docs.capacity() > SPARE_ROOM {
            self.heap -= l.bytes();
            *l = List::default();
        }
        self.spare.push(i as u32);
    }

    /// Slot `hole` emptied, the slots after it moved back while their
    /// probes reach it -- no tombstone, so a probe stops where it always
    /// would. A word is found while every slot from its group's first to
    /// its own is taken: a probe meets no empty slot before its group.
    fn close(&mut self, mut hole: usize) {
        let mask = self.slots.len() - 1;
        let mut j = hole;
        loop {
            j = (j + 1) & mask;
            let v = self.slots[j];
            if v == 0 {
                break;
            }
            if j.wrapping_sub(home(v, mask)) & mask >= j.wrapping_sub(hole) & mask {
                self.slots[hole] = v;
                self.ctrl[hole] = self.ctrl[j];
                hole = j;
            }
        }
        self.slots[hole] = 0;
        self.ctrl[hole] = EMPTY;
    }

    /// The arena written again with the living words alone.
    fn compact(&mut self) {
        let mut bytes = Vec::with_capacity(self.bytes.len() - self.dead);
        let old = &self.bytes;
        let mut copy = |at: &mut u32| {
            let (_, end) = word_at(old, *at as usize);
            let from = std::mem::replace(at, bytes.len() as u32) as usize;
            bytes.extend_from_slice(old.get(from..end).unwrap_or(&[]));
        };
        for t in self.ones.iter_mut().filter(|t| t.tf != 0) {
            copy(&mut t.at);
        }
        for l in self.lists.iter_mut().filter(|l| !l.docs.is_empty()) {
            copy(&mut l.at);
        }
        self.bytes = bytes;
        self.dead = 0;
    }

    /// Every array no larger than its words need, and no place free.
    fn shrink(&mut self) {
        if !self.free.is_empty() || !self.spare.is_empty() {
            self.close_up();
        }
        if self.dead > 0 {
            self.compact();
        }
        self.bytes.shrink_to_fit();
        self.ones.shrink_to_fit();
        self.lists.shrink_to_fit();
        let n = slots_for(self.len());
        if n < self.slots.len() {
            self.resize(n);
        }
        self.heap = 0;
        for l in self.lists.iter_mut() {
            l.shrink();
            self.heap += l.bytes();
        }
    }

    /// `ones` and `lists` without their free places, each slot naming its
    /// term's new one.
    fn close_up(&mut self) {
        let mut one = vec![0u32; self.ones.len()];
        let mut ones = Vec::with_capacity(self.ones.len() - self.free.len());
        for (i, t) in self.ones.iter().enumerate().filter(|(_, t)| t.tf != 0) {
            one[i] = ones.len() as u32;
            ones.push(*t);
        }
        let mut list = vec![0u32; self.lists.len()];
        let mut lists = Vec::with_capacity(self.lists.len() - self.spare.len());
        for (i, l) in self.lists.iter_mut().enumerate() {
            if !l.docs.is_empty() {
                list[i] = lists.len() as u32;
                lists.push(std::mem::take(l));
            }
        }
        for v in self.slots.iter_mut().filter(|v| **v != 0) {
            let p = place_of(*v);
            let i = match p.list {
                true => list[p.i],
                false => one[p.i],
            } as usize;
            *v = slot((*v >> 32) as u32, Place { i, list: p.list });
        }
        (self.ones, self.lists) = (ones, lists);
        (self.free, self.spare) = (Vec::new(), Vec::new());
    }

    /// What it holds in memory.
    fn bytes(&self) -> usize {
        self.slots.capacity() * 9
            + (self.free.capacity() + self.spare.capacity()) * 4
            + self.ones.capacity() * std::mem::size_of::<One>()
            + self.lists.capacity() * std::mem::size_of::<List>()
            + self.bytes.capacity()
            + self.heap
    }
}

/// The words of a text index and each one's documents.
#[derive(Default)]
pub(super) struct Terms {
    /// What every word is hashed with: the browser's `Fx`, std's random
    /// keys natively, against words written to collide.
    keys: Keys,
    one: Shard,
    /// Empty until the one shard is split; then every word is in one of
    /// them, by its tag's top byte.
    #[cfg(not(target_arch = "wasm32"))]
    shards: Vec<Shard>,
}

impl Terms {
    #[inline]
    fn of(&self, tag: u32) -> &Shard {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if !self.shards.is_empty() {
                return &self.shards[(tag >> 24) as usize];
            }
        }
        let _ = tag;
        &self.one
    }

    #[inline]
    fn of_mut(&mut self, tag: u32) -> &mut Shard {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if !self.shards.is_empty() {
                return &mut self.shards[(tag >> 24) as usize];
            }
        }
        let _ = tag;
        &mut self.one
    }

    /// Every shard: the one, or the 256 it was split into.
    fn shards(&self) -> &[Shard] {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if !self.shards.is_empty() {
                return &self.shards;
            }
        }
        std::slice::from_ref(&self.one)
    }

    fn shards_mut(&mut self) -> &mut [Shard] {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if !self.shards.is_empty() {
                return &mut self.shards;
            }
        }
        std::slice::from_mut(&mut self.one)
    }

    /// The documents holding `w`.
    #[inline]
    pub fn get(&self, w: &str) -> Option<Postings<'_>> {
        let h = hash_of(&self.keys, w);
        let w = w.as_bytes();
        let shard = self.of(h.tag);
        if shard.slots.is_empty() {
            return None;
        }
        let s = shard.find(h, w).ok()?;
        Some(shard.postings(s))
    }

    /// One more of `w` in `doc`; whether `w` is new to the index.
    #[inline]
    pub fn bump(&mut self, w: &str, doc: DocId) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        self.room();
        let h = hash_of(&self.keys, w);
        let w = w.as_bytes();
        let shard = self.of_mut(h.tag);
        let found = match shard.slots.is_empty() {
            true => Err(0),
            false => shard.find(h, w),
        };
        match found {
            Ok(s) => {
                shard.bump(s, doc);
                false
            }
            Err(s) => {
                shard.add(h, w, doc, s);
                true
            }
        }
    }

    /// `w` no longer in `doc`; whether that left it in none, and so out of
    /// the index.
    #[inline]
    pub fn drop_doc(&mut self, w: &str, doc: DocId) -> bool {
        let h = hash_of(&self.keys, w);
        let w = w.as_bytes();
        let shard = self.of_mut(h.tag);
        if shard.slots.is_empty() {
            return false;
        }
        match shard.find(h, w) {
            Ok(s) => shard.drop_doc(s, doc),
            Err(_) => false,
        }
    }

    /// How many words it holds.
    pub fn len(&self) -> usize {
        let mut n = 0;
        for s in self.shards() {
            n += s.len();
        }
        n
    }

    /// How many (word, document) pairs it holds.
    pub fn postings(&self) -> usize {
        let mut n = 0;
        for s in self.shards() {
            n += s.ones.len() - s.free.len();
            for l in &s.lists {
                n += l.docs.len();
            }
        }
        n
    }

    /// Every word's bytes, in no order, but the prefixes an index of them
    /// marks `^`: the words a [`crate::fuzzy::Dictionary`] is made of.
    pub fn words(&self) -> Vec<&[u8]> {
        let mut out = Vec::with_capacity(self.len());
        for s in self.shards() {
            let ones = s.ones.iter().filter(|t| t.tf != 0).map(|t| t.at);
            let ats = ones.chain(s.lists.iter().filter(|l| !l.docs.is_empty()).map(|l| l.at));
            for at in ats {
                let w = word_at(&s.bytes, at as usize).0;
                if w.first() != Some(&b'^') {
                    out.push(w);
                }
            }
        }
        out
    }

    /// What it holds in memory: each shard's table and arrays, its words'
    /// bytes, its lists and what their allocations cost.
    pub fn bytes(&self) -> usize {
        let mut n = 0;
        for s in self.shards() {
            n += s.bytes();
        }
        #[cfg(not(target_arch = "wasm32"))]
        let n = n + self.shards.capacity() * std::mem::size_of::<Shard>();
        n
    }

    /// Every array no larger than its words need: where the index is known
    /// complete, its build and `create index`.
    pub fn shrink_to_fit(&mut self) {
        for s in self.shards_mut() {
            s.shrink();
        }
    }

    /// The one shard split once it holds [`SPLIT_AT`] words or
    /// [`SPLIT_BYTES`] of them.
    #[cfg(not(target_arch = "wasm32"))]
    #[inline]
    fn room(&mut self) {
        if self.shards.is_empty()
            && (self.one.len() >= SPLIT_AT || self.one.bytes.len() >= SPLIT_BYTES)
        {
            self.split();
        }
    }

    /// The one shard's words moved into [`SHARDS`], each with room for
    /// twice its share, by the tags their slots keep.
    #[cfg(not(target_arch = "wasm32"))]
    #[cold]
    fn split(&mut self) {
        let mut one = std::mem::take(&mut self.one);
        let share = one.len() / SHARDS;
        let bytes = (one.bytes.len() - one.dead) / SHARDS;
        let mut shards: Vec<Shard> = (0..SHARDS)
            .map(|_| Shard::with_room(2 * share, 2 * bytes))
            .collect();
        for (&v, &c) in one.slots.iter().zip(&one.ctrl) {
            if v == 0 {
                continue;
            }
            let tag = (v >> 32) as u32;
            let p = place_of(v);
            let to = &mut shards[(tag >> 24) as usize];
            if !fits(to.len() + 1, to.slots.len()) {
                to.resize((to.slots.len() * 2).max(GROUP));
            }
            let at = to.put_word(word_at(&one.bytes, one.at(p)).0);
            let i = match p.list {
                true => {
                    let mut l = std::mem::take(&mut one.lists[p.i]);
                    l.at = at;
                    to.heap += l.bytes();
                    to.lists.push(l);
                    to.lists.len() - 1
                }
                false => {
                    to.ones.push(One {
                        at,
                        ..one.ones[p.i]
                    });
                    to.ones.len() - 1
                }
            };
            to.place(slot(tag, Place { i, list: p.list }), c);
        }
        self.shards = shards;
    }
}

#[cfg(test)]
impl Terms {
    /// Holds every shard to what its parts promise: each term found by its
    /// word through its tag and named by one slot, no list of fewer than
    /// two documents, the dead bytes what no term holds, the lists' count
    /// their walk.
    pub fn check(&self) {
        for s in self.shards() {
            let held = s.slots.iter().filter(|&&v| v != 0).count();
            assert_eq!(s.ctrl.len(), s.slots.len());
            for (&v, &c) in s.slots.iter().zip(&s.ctrl) {
                assert_eq!(v == 0, c == EMPTY);
            }
            assert_eq!(held, s.len());
            assert!(s.len() == 0 || fits(s.len(), s.slots.len()));
            let mut live = 0;
            let places = (0..s.ones.len())
                .filter(|&i| s.ones[i].tf != 0)
                .map(|i| Place { i, list: false })
                .chain(
                    (0..s.lists.len())
                        .filter(|&i| !s.lists[i].docs.is_empty())
                        .map(|i| Place { i, list: true }),
                );
            for p in places {
                let w = s.word(p);
                let h = hash_held(&self.keys, w);
                let found = s.find(h, w).expect("a term its word finds");
                assert!(place_of(s.slots[found]) == p);
                assert_eq!(s.ctrl[found], h.h2);
                let (_, end) = word_at(&s.bytes, s.at(p));
                live += end - s.at(p);
                if p.list {
                    let l = &s.lists[p.i];
                    assert!(l.docs.len() >= 2, "a list of one document");
                    assert!(l.docs.windows(2).all(|d| d[0] < d[1]));
                    assert_eq!(l.docs.len(), l.tfs.len());
                    assert!(l.tfs.iter().all(|&tf| tf >= 1 && tf <= l.max_tf));
                } else {
                    assert!(s.ones[p.i].tf >= 1);
                }
            }
            assert_eq!(live + s.dead, s.bytes.len());
            for &f in &s.free {
                assert_eq!(s.ones[f as usize].tf, 0);
            }
            let unused = s.ones.iter().filter(|t| t.tf == 0).count();
            assert_eq!(unused, s.free.len());
            for &f in &s.spare {
                assert!(s.lists[f as usize].docs.is_empty());
                assert!(s.lists[f as usize].docs.capacity() <= SPARE_ROOM);
            }
            let spare = s.lists.iter().filter(|l| l.docs.is_empty()).count();
            assert_eq!(spare, s.spare.len());
            assert_eq!(s.heap, s.lists.iter().map(List::bytes).sum::<usize>());
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn split_into(&self) -> usize {
        self.shards.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};

    /// Words coming and going at random answer as a map of each word to
    /// its documents does -- through tables growing, slots closing up, the
    /// arena written again, terms passing between one document and more,
    /// and a shrink -- and every shard holds to what it promises after each.
    #[test]
    fn the_terms_answer_as_a_map_of_their_documents() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut terms = Terms::default();
        let mut want: HashMap<String, BTreeMap<DocId, u32>> = HashMap::new();
        // Short words, ones whose length takes two bytes, and ones that
        // are not ASCII.
        let word = |i: u64| match i % 3 {
            0 => format!("w{i}"),
            1 => format!("long{i}{}", "x".repeat(100 + (i % 150) as usize)),
            _ => format!("ü{i}"),
        };
        for round in 0..60_000u64 {
            let w = word(next(if round < 30_000 { 3_000 } else { 400 }));
            let doc = next(40);
            if next(3) == 0 {
                let gone = match want.get_mut(&w) {
                    Some(m) => m.remove(&doc).is_some() && m.is_empty(),
                    None => false,
                };
                if gone {
                    want.remove(&w);
                }
                assert_eq!(terms.drop_doc(&w, doc), gone, "round {round} {w}");
            } else {
                let new = !want.contains_key(&w);
                *want.entry(w.clone()).or_default().entry(doc).or_default() += 1;
                assert_eq!(terms.bump(&w, doc), new, "round {round} {w}");
            }
            if round % 5_000 == 0 {
                terms.check();
            }
            if round == 45_000 {
                terms.shrink_to_fit();
                terms.check();
            }
        }
        terms.check();
        assert_eq!(terms.len(), want.len());
        for (w, docs) in &want {
            let p = terms.get(w).expect("a word held");
            assert_eq!(p.docs, docs.keys().copied().collect::<Vec<_>>().as_slice());
            assert_eq!(p.tfs, docs.values().copied().collect::<Vec<_>>().as_slice());
            assert!(p.max_tf >= *docs.values().max().unwrap());
        }
        assert!(terms.get("never").is_none());
        let mut words: Vec<&str> = (terms.words().into_iter())
            .map(|w| std::str::from_utf8(w).unwrap())
            .collect();
        words.sort_unstable();
        let mut held: Vec<&str> = want.keys().map(String::as_str).collect();
        held.sort_unstable();
        assert_eq!(words, held);
        assert_eq!(
            terms.postings(),
            want.values().map(BTreeMap::len).sum::<usize>()
        );
    }

    /// Past the split every word is where it went, each shard grows on its
    /// own, and the words leaving take their bytes with them.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_split_finds_every_word_and_grows_a_share_at_a_time() {
        let mut terms = Terms::default();
        let n = SPLIT_AT as u64 + 50_000;
        let mut most = 0;
        for i in 0..n {
            let before = terms.bytes();
            terms.bump(&format!("k{i}x"), i);
            terms.bump(&format!("k{i}x"), i + 1);
            terms.bump(&format!("k{i}y"), i);
            if i > SPLIT_AT as u64 {
                most = most.max(terms.bytes().saturating_sub(before));
            }
        }
        assert_eq!(terms.split_into(), SHARDS);
        terms.check();
        // A shard grows by its own share: a table, an array or an arena of
        // a few hundred words, never the whole.
        assert!(most < 64 * 1024, "{most}");
        for i in (0..n).step_by(13) {
            assert_eq!(terms.get(&format!("k{i}x")).unwrap().docs, [i, i + 1]);
            assert_eq!(terms.get(&format!("k{i}y")).unwrap().docs, [i]);
        }
        let full = terms.bytes();
        for i in 0..n {
            assert!(!terms.drop_doc(&format!("k{i}x"), i));
            assert!(terms.drop_doc(&format!("k{i}x"), i + 1));
            assert!(terms.drop_doc(&format!("k{i}y"), i));
        }
        terms.check();
        assert_eq!(terms.len(), 0);
        terms.shrink_to_fit();
        assert!(terms.bytes() < full / 20, "{} of {full}", terms.bytes());
    }
}
