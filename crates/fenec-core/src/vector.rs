//! Vector search: metrics + the HNSW index.
//!
//! Design notes
//! - All vectors sit contiguously in a single `Vec<f32>` arena. No separate
//!   allocation per vector; this is both cache friendly and required for
//!   LLVM's auto-vectorisation.
//! - With the cosine metric vectors are normalised *inside the index*, so no
//!   length computation happens at query time (1 - dot).
//! - A delete is a tombstone; skipped while searching, cleaned up on merge.

// Without the `vector` feature the graph's code is here but unused: the
// index is a type of no value (`off.rs`), and the compiler drops the rest.
#![cfg_attr(not(feature = "vector"), allow(dead_code, unused_imports))]

use crate::codec::{get_uvarint, put_uvarint};
use crate::schema::{Metric, Quant, VectorIndexSpec};
use crate::store::DocMap;
use crate::value::{DocId, VecPrec};
use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
#[cfg(not(target_family = "wasm"))]
use std::mem::MaybeUninit;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
#[cfg(not(target_family = "wasm"))]
use std::sync::Mutex;

// -------------------------------------------------------------- metrics

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
#[inline]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    simd::strips(a, b, simd::f32s, simd::f32s, simd::mul, mul, ident, ident)
}

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
#[inline]
pub fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    simd::strips(
        a,
        b,
        simd::f32s,
        simd::f32s,
        simd::diff_sq,
        diff_sq,
        ident,
        ident,
    )
}

/// The browser build is optimised for size (`opt-level = "z"`), and at that
/// level LLVM does not vectorise the strips below: they ran scalar. Written
/// out with the wasm SIMD intrinsics they are vectorised whatever the level.
/// A 20 000 x 384 HNSW build in the browser engine went 28.0 -> 9.95 s for
/// 0.9 KB of the module; building all of `fenec-core` at `opt-level = 3`
/// instead reached 4.35 s for 26 KB.
///
/// Safe Rust throughout: the lanes are built from array elements rather than
/// loaded through a pointer, and LLVM folds the reads into vector loads.
#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
mod simd {
    use core::arch::wasm32::*;

    /// Eight f32 as two four-lane registers.
    #[inline(always)]
    pub(super) fn f32s(x: &[f32; 8]) -> (v128, v128) {
        (f32x4(x[0], x[1], x[2], x[3]), f32x4(x[4], x[5], x[6], x[7]))
    }

    /// Eight f16 widened the way [`super::half`] does it, four lanes at a
    /// time: the same shifts, the same masks, the same multiply, so the same
    /// bits.
    #[inline(always)]
    pub(super) fn halves(x: &[u16; 8]) -> (v128, v128) {
        let v = u16x8(x[0], x[1], x[2], x[3], x[4], x[5], x[6], x[7]);
        let widen = |u: v128| {
            let sign = u32x4_shl(v128_and(u, u32x4_splat(0x8000)), 16);
            let mag = u32x4_shl(v128_and(u, u32x4_splat(0x7fff)), 13);
            f32x4_mul(
                v128_or(sign, mag),
                f32x4_splat(f32::from_bits((254 - 15) << 23)),
            )
        };
        (
            widen(u32x4_extend_low_u16x8(v)),
            widen(u32x4_extend_high_u16x8(v)),
        )
    }

    #[inline(always)]
    pub(super) fn mul(x: v128, y: v128) -> v128 {
        f32x4_mul(x, y)
    }

    #[inline(always)]
    pub(super) fn diff_sq(x: v128, y: v128) -> v128 {
        let d = f32x4_sub(x, y);
        f32x4_mul(d, d)
    }

    /// The scalar loop's eight accumulators as two four-lane registers,
    /// reduced in the same order, and each lane doing the same multiply and
    /// add: the result is bit for bit the scalar one, so a graph built in the
    /// browser is the graph built natively. The tail past the last full
    /// strip runs scalar, through the widening each side needs.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    pub(super) fn strips<A: Copy, B: Copy>(
        a: &[A],
        b: &[B],
        la: impl Fn(&[A; 8]) -> (v128, v128),
        lb: impl Fn(&[B; 8]) -> (v128, v128),
        step: impl Fn(v128, v128) -> v128,
        tail: impl Fn(f32, f32) -> f32,
        widen_a: impl Fn(A) -> f32,
        widen_b: impl Fn(B) -> f32,
    ) -> f32 {
        debug_assert_eq!(a.len(), b.len());
        let (ca, ra) = a.as_chunks::<8>();
        let (cb, rb) = b.as_chunks::<8>();
        let (mut lo, mut hi) = (f32x4_splat(0.0), f32x4_splat(0.0));
        for (x, y) in ca.iter().zip(cb) {
            let ((x0, x1), (y0, y1)) = (la(x), lb(y));
            lo = f32x4_add(lo, step(x0, y0));
            hi = f32x4_add(hi, step(x1, y1));
        }
        let lane = |v: v128| {
            [
                f32x4_extract_lane::<0>(v),
                f32x4_extract_lane::<1>(v),
                f32x4_extract_lane::<2>(v),
                f32x4_extract_lane::<3>(v),
            ]
        };
        let (l, h) = (lane(lo), lane(hi));
        let mut s = (l[0] + l[1]) + (l[2] + l[3]) + ((h[0] + h[1]) + (h[2] + h[3]));
        for (x, y) in ra.iter().zip(rb) {
            s += tail(widen_a(*x), widen_b(*y));
        }
        s
    }
}

#[cfg(not(any(
    all(target_arch = "wasm32", target_feature = "simd128"),
    all(target_arch = "aarch64", target_feature = "neon")
)))]
#[inline]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    // Strips of 8: no bounds check in the loop body, LLVM turns this
    // straight into SIMD. The accumulator array breaks the dependency chain.
    let mut acc = [0.0f32; 8];
    let (ca, ra) = a.as_chunks::<8>();
    let (cb, rb) = b.as_chunks::<8>();
    for (x, y) in ca.iter().zip(cb) {
        for k in 0..8 {
            acc[k] += x[k] * y[k];
        }
    }
    let mut s = (acc[0] + acc[1]) + (acc[2] + acc[3]) + ((acc[4] + acc[5]) + (acc[6] + acc[7]));
    for (x, y) in ra.iter().zip(rb) {
        s += x * y;
    }
    s
}

#[cfg(not(any(
    all(target_arch = "wasm32", target_feature = "simd128"),
    all(target_arch = "aarch64", target_feature = "neon")
)))]
#[inline]
pub fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut acc = [0.0f32; 8];
    let (ca, ra) = a.as_chunks::<8>();
    let (cb, rb) = b.as_chunks::<8>();
    for (x, y) in ca.iter().zip(cb) {
        for k in 0..8 {
            let d = x[k] - y[k];
            acc[k] += d * d;
        }
    }
    let mut s = (acc[0] + acc[1]) + (acc[2] + acc[3]) + ((acc[4] + acc[5]) + (acc[6] + acc[7]));
    for (x, y) in ra.iter().zip(rb) {
        let d = x - y;
        s += d * d;
    }
    s
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    // SAFETY: the build has NEON, which is all `neon::dot` requires.
    unsafe { neon::dot(a, b) }
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline]
pub fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    // SAFETY: as for `dot`.
    unsafe { neon::l2(a, b) }
}

pub fn norm(v: &[f32]) -> f32 {
    dot(v, v).sqrt()
}

pub fn normalized(v: &[f32]) -> Vec<f32> {
    let n = norm(v);
    if n == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|x| x / n).collect()
}

/// *Distance* according to the metric (small = near). dot/cosine are negated
/// so that ordering only ever runs one way.
#[inline]
pub fn distance(metric: Metric, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        // a and b are assumed to be normalised
        Metric::Cosine => 1.0 - dot(a, b),
        Metric::L2 => l2_sq(a, b),
        Metric::Dot => -dot(a, b),
    }
}

/// [`distance`] from `q` to four vectors, the same bits as four calls. Read
/// together, the four wait on memory once rather than in turn, and their
/// adds run side by side: out of an arena of 100 000, a 128-dim distance
/// took 60 ns alone and 47 four at a time on an M1, a 768-dim one 242 and
/// 149. Only aarch64 measures four at once. The browser's module built a
/// 10 000 x 128 graph 4% slower that way and grew 9 KB -- at `opt-level =
/// "z"`, over vectors mostly in cache, there was no wait to hide -- and a
/// walk gathering its neighbours before measuring them one at a time still
/// cost its `near` 7%, so everywhere else the walk is as it was.
#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline]
fn distances4(metric: Metric, q: &[f32], t: [&[f32]; 4]) -> [f32; 4] {
    match metric {
        Metric::Cosine => dots4(q, t).map(|d| 1.0 - d),
        Metric::L2 => l2s4(q, t),
        Metric::Dot => dots4(q, t).map(|d| -d),
    }
}

/// Asks for the cache line at `p` ahead of its use.
#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline(always)]
fn prefetch(p: *const u8) {
    // SAFETY: a prefetch is a hint: it reads nothing the program sees and
    // cannot fault, whatever the address.
    unsafe {
        core::arch::asm!("prfm pldl1keep, [{p}]", p = in(reg) p, options(nostack, readonly, preserves_flags));
    }
}

/// [`distances4`] over vectors in half precision, as `distance_hf` measures
/// each.
#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline]
fn distances4_hf(metric: Metric, q: &[f32], t: [&[u16]; 4]) -> [f32; 4] {
    match metric {
        Metric::Cosine => dots4_hf(q, t).map(|d| 1.0 - d),
        Metric::L2 => l2s4_hf(q, t),
        Metric::Dot => dots4_hf(q, t).map(|d| -d),
    }
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline]
fn dots4(q: &[f32], t: [&[f32]; 4]) -> [f32; 4] {
    // SAFETY: as for `dot`.
    unsafe { neon::dots4(q, t) }
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline]
fn l2s4(q: &[f32], t: [&[f32]; 4]) -> [f32; 4] {
    // SAFETY: as for `dot`.
    unsafe { neon::l2s4(q, t) }
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline]
fn dots4_hf(q: &[f32], t: [&[u16]; 4]) -> [f32; 4] {
    // SAFETY: as for `dot`.
    unsafe { neon::dots4_hf(q, t) }
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline]
fn l2s4_hf(q: &[f32], t: [&[u16]; 4]) -> [f32; 4] {
    // SAFETY: as for `dot`.
    unsafe { neon::l2s4_hf(q, t) }
}

/// Turns a distance into the similarity score shown to the user.
pub fn score_from_distance(metric: Metric, d: f32) -> f32 {
    match metric {
        Metric::Cosine => 1.0 - d,
        Metric::L2 => d.sqrt(),
        Metric::Dot => -d,
    }
}

// ------------------------------------------------- half-precision kernel
//
// In the f16 arena the distance is computed with the widening done inside the
// loop. Widening into an intermediate f32 buffer was possible too; it measured
// slower, because the win comes from memory bandwidth anyway.

// aarch64 writes every strip out (`neon`), and the tests hold them to this.
#[cfg_attr(
    all(target_arch = "aarch64", target_feature = "neon"),
    allow(unused_macros)
)]
macro_rules! strip8 {
    ($a:expr, $b:expr, $get_a:expr, $get_b:expr, $step:expr, $tail:expr) => {{
        let (a, b) = ($a, $b);
        debug_assert_eq!(a.len(), b.len());
        let mut acc = [0.0f32; 8];
        let mut ia = a.chunks_exact(8);
        let mut ib = b.chunks_exact(8);
        for (x, y) in ia.by_ref().zip(ib.by_ref()) {
            for k in 0..8 {
                let (xv, yv): (f32, f32) = ($get_a(x[k]), $get_b(y[k]));
                acc[k] += $step(xv, yv);
            }
        }
        let mut s = (acc[0] + acc[1]) + (acc[2] + acc[3]) + ((acc[4] + acc[5]) + (acc[6] + acc[7]));
        for (x, y) in ia.remainder().iter().zip(ib.remainder()) {
            let (xv, yv): (f32, f32) = ($get_a(*x), $get_b(*y));
            s += $step(xv, yv);
        }
        let _ = $tail;
        s
    }};
}

/// Σ x², summed flat, one element after another: the 8-strip `norm` rounds
/// differently, which would change the normalised vectors and with them
/// the graph. One add waits on the one before, 120 ns at 128 dimensions
/// and 720 at 768 on an M1 -- see [`flat_sqs`] for several at once.
#[inline]
fn flat_sq(v: &[f32]) -> f32 {
    let mut acc = 0.0f32;
    for x in v {
        acc += x * x;
    }
    acc
}

/// [`flat_sq`] of eight vectors of one length, side by side: each sum still
/// flat and in its own order, so the same bits, but eight chains of adds in
/// flight rather than one. A reopen normalises every vector it reads into
/// the arena, and one at a time that was a third of opening a 100 000 x 128
/// graph.
// The eight sums advance together, an element of each at a time: the index
// is the point.
#[allow(clippy::needless_range_loop)]
fn flat_sqs8(v: [&[f32]; 8]) -> [f32; 8] {
    let dim = v[0].len();
    // Cut to one length, so no index below needs checking.
    let v = v.map(|s| &s[..dim]);
    let mut acc = [0.0f32; 8];
    for i in 0..dim {
        for j in 0..8 {
            acc[j] += v[j][i] * v[j][i];
        }
    }
    acc
}

/// [`get_uvarint`] inlined into the loop that reads a graph's links, a link
/// being two or three bytes: called with its `Result` a link at a time, it
/// was a quarter of opening a 100 000 x 128 graph. The same values, a
/// longer one left to `get_uvarint`.
#[inline(always)]
fn graph_varint(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    let p = *pos;
    if let Some(&[a, b, c]) = bytes.get(p..p + 3) {
        let (a7, b7) = ((a & 0x7f) as u64, (b & 0x7f) as u64);
        if a < 0x80 {
            *pos = p + 1;
            return Some(a as u64);
        }
        if b < 0x80 {
            *pos = p + 2;
            return Some(a7 | b7 << 7);
        }
        if c < 0x80 {
            *pos = p + 3;
            return Some(a7 | b7 << 7 | (c as u64) << 14);
        }
    }
    get_uvarint(bytes, pos).ok()
}

/// The nodes whose lists a thread writes at a time into a graph record:
/// small enough that a slow core takes few, the M1's efficiency cores as
/// `spread` says, large enough that a part's bytes are worth a copy.
#[cfg(not(target_family = "wasm"))]
const LIST_PART: usize = 4096;

/// The bytes a link takes in a flat graph record of `nodes` nodes.
fn link_width(nodes: usize) -> usize {
    if nodes <= 1 << 16 {
        2
    } else if nodes <= 1 << 24 {
        3
    } else {
        4
    }
}

/// The bytes a document id takes in a flat graph record: 4 where every one
/// fits.
fn doc_width(docs: &[DocId]) -> usize {
    match docs.iter().all(|&d| d <= u32::MAX as u64) {
        true => 4,
        false => 8,
    }
}

/// Writes a sorted list of links as a flat graph record holds one: the
/// first `lw` bytes whole, then -- past a lone one -- the width of the
/// largest step between neighbours and every step in that many bytes. The
/// steps of a 100 000-node graph's lists take two bytes, as its varint
/// deltas did, and read without a branch a byte.
fn put_links(out: &mut Vec<u8>, sorted: &[u64], lw: usize) {
    let Some((&first, rest)) = sorted.split_first() else {
        return;
    };
    // Room for every link whole, eight bytes, before it is cut to its
    // width (`put_le`).
    out.reserve(9 + 8 * rest.len());
    put_le(out, first, lw);
    if rest.is_empty() {
        return;
    }
    let mut prev = first;
    let mut widest = 0u64;
    for &x in rest {
        widest = widest.max(x.wrapping_sub(prev));
        prev = x;
    }
    let w = (8 - widest.leading_zeros() as usize / 8).max(1);
    out.push(w as u8);
    let mut prev = first;
    for &x in rest {
        put_le(out, x.wrapping_sub(prev), w);
        prev = x;
    }
}

/// `v`'s first `w` bytes, little-endian: all eight written, then cut. A
/// copy of a width known only as the record is written was a call to
/// `memcpy` a link: 110 000 nodes' record took 23.7 ms that way, 20.3 this.
#[inline(always)]
fn put_le(out: &mut Vec<u8>, v: u64, w: usize) {
    let at = out.len();
    out.extend_from_slice(&v.to_le_bytes());
    out.truncate(at + w);
}

/// A node's flag in a graph record: linked and live, a tombstone, or live
/// and waiting to be linked.
const LIVE: u8 = 0;
const DEAD: u8 = 1;
const WAITING: u8 = 2;

/// A graph record read into arrays, whichever layout it was in: what
/// `build_restored` makes the index from.
struct Read<'a> {
    docs: Vec<DocId>,
    /// [`LIVE`], [`DEAD`] or [`WAITING`], a node each.
    flags: Vec<u8>,
    l0_len: Vec<u16>,
    /// The level-0 lists, `m0` a node.
    l0: Vec<u32>,
    upper: Vec<Vec<Vec<u32>>>,
    /// A tombstone's vector as the arena stores it, in node order.
    tombs: Vec<&'a [u8]>,
    /// `(node, doc)`: the documents holding another's vector, in order.
    aliases: Vec<(u32, DocId)>,
    /// The top half of each node's hash in [`Same`], four bytes a node,
    /// where the record carries them ([`HALVES`]).
    #[cfg(not(target_family = "wasm"))]
    halves: Option<&'a [u8]>,
}

impl<'a> Read<'a> {
    fn new(count: usize, m0: usize) -> Option<Read<'a>> {
        Some(Read {
            docs: Vec::with_capacity(count),
            flags: Vec::with_capacity(count),
            l0_len: Vec::with_capacity(count),
            l0: vec![0; count.checked_mul(m0)?],
            upper: Vec::with_capacity(count),
            tombs: Vec::new(),
            aliases: Vec::new(),
            #[cfg(not(target_family = "wasm"))]
            halves: None,
        })
    }

    /// A node's flag as the record gives it, where a build that cannot hold
    /// a node waiting (`waits` false) refuses one.
    fn flag(byte: u8, waits: bool) -> Option<u8> {
        match (byte, waits) {
            (LIVE | DEAD, _) | (WAITING, true) => Some(byte),
            _ => None,
        }
    }

    /// The body of a graph record in the varint layout (versions 3 to 6),
    /// past its head: a node at a time -- its document, flag and levels, a
    /// tombstone's vector, each level's links as varint deltas.
    fn varint(
        bytes: &'a [u8],
        count: usize,
        m0: usize,
        stored: usize,
        waits: bool,
        kept: bool,
    ) -> Option<Read<'a>> {
        let mut r = Read::new(count, m0)?;
        let (mut pos, mut lvl) = (0usize, Vec::new());
        for node in 0..count {
            let doc = get_uvarint(bytes, &mut pos).ok()?;
            let flag = match (*bytes.get(pos)?, waits) {
                (LIVE, _) => LIVE,
                (WAITING, true) => WAITING,
                // Kept with a node waiting, which this build cannot hold.
                (WAITING, false) if kept => return None,
                (DEAD, true) | (_, false) => DEAD,
                _ => return None,
            };
            pos += 1;
            let levels = get_uvarint(bytes, &mut pos).ok()? as usize;
            if levels == 0 {
                return None;
            }
            r.docs.push(doc);
            r.flags.push(flag);
            if flag == DEAD {
                r.tombs.push(bytes.get(pos..pos.checked_add(stored)?)?);
                pos += stored;
            }
            let mut lists = Vec::with_capacity(levels - 1);
            for l in 0..levels {
                let k = get_uvarint(bytes, &mut pos).ok()? as usize;
                if k > count || (l == 0 && k > m0) {
                    return None; // a corrupt length, or past the arena's stride
                }
                lvl.clear();
                let mut prev = 0u64;
                for _ in 0..k {
                    let nb = prev.checked_add(graph_varint(bytes, &mut pos)?)?;
                    if nb as usize >= count {
                        return None; // out-of-range link: the graph is corrupt
                    }
                    prev = nb;
                    lvl.push(nb as u32);
                }
                match l {
                    0 => {
                        r.l0[node * m0..][..k].copy_from_slice(&lvl);
                        r.l0_len.push(k as u16);
                    }
                    _ => lists.push(lvl.clone()),
                }
            }
            r.upper.push(lists);
        }
        Some(r)
    }

    /// The body of a flat graph record ([`GRAPH_VERSION_FLAT`]), past its
    /// head: the nodes' arrays, every level-0 list, the levels above, the
    /// tombstones' vectors, every link checked to name a node.
    fn flat(
        bytes: &'a [u8],
        count: usize,
        m0: usize,
        stored: usize,
        waits: bool,
        aliased: bool,
    ) -> Option<Read<'a>> {
        let mut r = Read::new(count, m0)?;
        let mut at = Cursor { bytes, pos: 0 };
        let widths = at.take(2)?;
        let (lw, dw) = (widths[0] as usize, widths[1] as usize);
        if !(link_width(count)..=4).contains(&lw) || !matches!(dw, 4 | 8) {
            return None;
        }
        let docs = at.take(count.checked_mul(dw)?)?;
        r.docs.extend(docs.chunks_exact(dw).map(|b| {
            let mut w = [0u8; 8];
            w[..dw].copy_from_slice(b);
            u64::from_le_bytes(w)
        }));
        for &b in at.take(count)? {
            r.flags.push(Read::flag(b, waits)?);
        }
        let levels = at.take(count)?;
        for b in at.take(count.checked_mul(2)?)?.as_chunks::<2>().0 {
            let k = u16::from_le_bytes(*b);
            if k as usize > m0 {
                return None; // level-0 degree exceeds the arena stride
            }
            r.l0_len.push(k);
        }
        for (stride, &k) in r.l0.chunks_exact_mut(m0).zip(&r.l0_len) {
            at.links(lw, &mut stride[..k as usize], count)?;
        }
        for &lv in levels {
            let mut lists = Vec::with_capacity(lv as usize);
            for _ in 0..lv {
                let k = at.number(2)? as usize;
                if k > count {
                    return None;
                }
                let mut list = vec![0u32; k];
                at.links(lw, &mut list, count)?;
                lists.push(list);
            }
            r.upper.push(lists);
        }
        for _ in r.flags.iter().filter(|&&f| f == DEAD) {
            r.tombs.push(at.take(stored)?);
        }
        if aliased {
            let n = at.number(8)? as usize;
            r.aliases.reserve(n.min(count.saturating_mul(64)));
            for _ in 0..n {
                let node = at.number(lw)?;
                if node as usize >= count {
                    return None;
                }
                r.aliases.push((node as u32, at.number(8)?));
            }
            // Written in order: anything else is no record of this build.
            if r.aliases.windows(2).any(|w| w[0] >= w[1]) {
                return None;
            }
            // The nodes' hash halves, where they follow, and nothing else:
            // the browser, and a binary from before them, read the record
            // without them.
            #[cfg(not(target_family = "wasm"))]
            match bytes.get(at.pos..)? {
                [] => {}
                [HALVES, halves @ ..] if halves.len() == count.checked_mul(4)? => {
                    r.halves = Some(halves)
                }
                _ => return None,
            }
        }
        Some(r)
    }
}

/// Where a flat graph record is being read.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let part = self.bytes.get(self.pos..self.pos.checked_add(len)?)?;
        self.pos += len;
        Some(part)
    }

    /// A whole number of `w` bytes, little-endian.
    fn number(&mut self, w: usize) -> Option<u64> {
        let mut b = [0u8; 8];
        b[..w].copy_from_slice(self.take(w)?);
        Some(u64::from_le_bytes(b))
    }

    /// A list [`put_links`] wrote into `out`, every link checked to name
    /// one of `count` nodes.
    fn links(&mut self, lw: usize, out: &mut [u32], count: usize) -> Option<()> {
        let Some((first, rest)) = out.split_first_mut() else {
            return Some(());
        };
        let mut prev = self.number(lw)?;
        *first = prev as u32;
        if !rest.is_empty() {
            let w = self.take(1)?[0] as usize;
            if !(1..=4).contains(&w) {
                return None;
            }
            let steps = self.take(rest.len().checked_mul(w)?)?;
            prev = match (cfg!(target_family = "wasm"), w) {
                // The browser, which a restore's links do not hold up, takes
                // the one loop: the four were 0.9 KB of its module.
                (true, _) => steps_any(steps, w, prev, rest),
                (false, 1) => steps_into::<1>(steps, prev, rest),
                (false, 2) => steps_into::<2>(steps, prev, rest),
                (false, 3) => steps_into::<3>(steps, prev, rest),
                _ => steps_into::<4>(steps, prev, rest),
            };
        }
        // Ascending, so the last is the largest.
        ((prev as usize) < count).then_some(())
    }
}

/// [`steps_into`] with the width known only as it runs.
fn steps_any(steps: &[u8], w: usize, mut prev: u64, out: &mut [u32]) -> u64 {
    for (slot, b) in out.iter_mut().zip(steps.chunks_exact(w)) {
        let mut x = [0u8; 8];
        x[..w].copy_from_slice(b);
        prev += u64::from_le_bytes(x);
        *slot = prev as u32;
    }
    prev
}

/// Adds `W`-byte steps up from `prev` into `out`, returning the last: a sum
/// in `u64`, so no list of `u32` steps overflows it.
fn steps_into<const W: usize>(steps: &[u8], mut prev: u64, out: &mut [u32]) -> u64 {
    for (slot, b) in out.iter_mut().zip(steps.as_chunks::<W>().0) {
        let mut w = [0u8; 8];
        w[..W].copy_from_slice(b);
        prev += u64::from_le_bytes(w);
        *slot = prev as u32;
    }
    prev
}

/// Whether a restore sums its vectors' norms eight at a time: natively.
const BATCH_NORMS: bool = cfg!(not(target_family = "wasm"));

/// Pushes the vectors `batch` holds, `dim` apiece, into `arena` in order --
/// made unit ones when `unit`, their norms summed side by side -- and
/// empties it.
fn flush_units(arena: &mut Arena, batch: &mut Vec<f32>, dim: usize, unit: bool) {
    if dim == 0 || batch.is_empty() {
        batch.clear();
        return;
    }
    let mut vs = batch.chunks_exact(dim);
    if batch.len() == 8 * dim && unit {
        let v: [&[f32]; 8] = std::array::from_fn(|_| vs.next().unwrap_or(&[]));
        for (v, sq) in v.iter().zip(flat_sqs8(v)) {
            arena.push_scaled(v, unit_scale(sq));
        }
    } else {
        for v in vs {
            arena.push_scaled(v, if unit { unit_scale(flat_sq(v)) } else { 1.0 });
        }
    }
    batch.clear();
}

/// What a vector is multiplied by to make it a unit one, from its
/// [`flat_sq`]: a zero vector stays as it is.
#[inline]
fn unit_scale(sq: f32) -> f32 {
    let n = sq.sqrt();
    if n > 0.0 {
        1.0 / n
    } else {
        1.0
    }
}

/// A document's vector for a restore, into the buffer handed it: `false`
/// when it holds none. Shared by the threads that fill the arena.
type Lookup<'a> = dyn Fn(DocId, &mut Vec<f32>) -> bool + Sync + 'a;

/// Nodes a share of a restored arena holds, which a thread fills at a time.
#[cfg(not(target_family = "wasm"))]
const FILL_SHARE: usize = 1024;

/// A share of a restored arena's slots, not yet written.
#[cfg(not(target_family = "wasm"))]
enum Part<'a> {
    F32(&'a mut [MaybeUninit<f32>]),
    F16(&'a mut [MaybeUninit<u16>]),
}

#[cfg(not(target_family = "wasm"))]
impl Part<'_> {
    fn len(&self) -> usize {
        match self {
            Part::F32(s) => s.len(),
            Part::F16(s) => s.len(),
        }
    }

    /// Node `k`'s slots from `raw` times `inv`, as [`Arena::push_scaled`]
    /// pushes them.
    fn put_scaled(&mut self, k: usize, dim: usize, raw: &[f32], inv: f32) {
        match self {
            Part::F32(s) => {
                let slots = &mut s[k * dim..(k + 1) * dim];
                match inv != 1.0 {
                    true => slots.iter_mut().zip(raw).for_each(|(o, x)| {
                        o.write(x * inv);
                    }),
                    false => slots.iter_mut().zip(raw).for_each(|(o, x)| {
                        o.write(*x);
                    }),
                }
            }
            Part::F16(s) => s[k * dim..(k + 1) * dim]
                .iter_mut()
                .zip(raw)
                .for_each(|(o, x)| {
                    o.write(crate::codec::f16_from_f32(x * inv));
                }),
        }
    }

    /// Node `k`'s slots from a tombstone's vector as its record holds it,
    /// as [`Arena::push_stored`] reads it: `false` for another length.
    fn put_stored(&mut self, k: usize, dim: usize, bytes: &[u8]) -> bool {
        match self {
            Part::F32(s) => {
                let (words, rest) = bytes.as_chunks::<4>();
                if words.len() != dim || !rest.is_empty() {
                    return false;
                }
                for (o, w) in s[k * dim..(k + 1) * dim].iter_mut().zip(words) {
                    o.write(f32::from_le_bytes(*w));
                }
            }
            Part::F16(s) => {
                let (words, rest) = bytes.as_chunks::<2>();
                if words.len() != dim || !rest.is_empty() {
                    return false;
                }
                for (o, w) in s[k * dim..(k + 1) * dim].iter_mut().zip(words) {
                    o.write(u16::from_le_bytes(*w));
                }
            }
        }
        true
    }

    /// The first `n` of `raws`, read for the nodes `held`, scaled into
    /// their slots: made unit ones for cosine with the norms summed eight
    /// side by side where the target sums them so ([`flat_sqs8`]).
    fn put_units(&mut self, held: &[usize], raws: &[Vec<f32>; 8], dim: usize, unit: bool) {
        let n = held.len();
        let mut inv = [1.0f32; 8];
        if unit && n == 8 && BATCH_NORMS {
            let sq = flat_sqs8(std::array::from_fn(|j| &raws[j][..]));
            inv = sq.map(unit_scale);
        } else if unit {
            for j in 0..n {
                inv[j] = unit_scale(flat_sq(&raws[j]));
            }
        }
        for j in 0..n {
            self.put_scaled(held[j], dim, &raws[j], inv[j]);
        }
    }
}

/// Fills one share of a restored arena, its first node the graph's `from`
/// and its first tombstone the record's `tomb`th: see
/// [`VectorIndex::fill_restored`]. `false` where a document holds no vector
/// of the index's length, or a tombstone's is cut short.
#[cfg(not(target_family = "wasm"))]
fn fill_share(
    part: &mut Part<'_>,
    (from, mut tomb): (usize, usize),
    read: &Read<'_>,
    raws: &mut [Vec<f32>; 8],
    (dim, unit): (usize, bool),
    lookup: &Lookup<'_>,
) -> bool {
    // The share's nodes read and waiting for their norms, a slot of `raws`
    // each.
    let mut held = [0usize; 8];
    let mut n = 0;
    for k in 0..part.len() / dim {
        let node = from + k;
        if read.flags[node] == DEAD {
            let Some(stored) = read.tombs.get(tomb) else {
                return false;
            };
            tomb += 1;
            if !part.put_stored(k, dim, stored) {
                return false;
            }
            continue;
        }
        if !lookup(read.docs[node], &mut raws[n]) || raws[n].len() != dim {
            return false;
        }
        held[n] = k;
        n += 1;
        if n == 8 || !BATCH_NORMS {
            part.put_units(&held[..n], raws, dim, unit);
            n = 0;
        }
    }
    part.put_units(&held[..n], raws, dim, unit);
    true
}

/// f16 -> f32, branchless. In the hot loop the subnormal branch of
/// `codec::f32_from_f16` blocked auto-vectorisation (build time went up 5x);
/// here the same result falls out of a single multiply.
///
/// The bits are shifted by 13 and read as f32, then multiplied by a fixed
/// exponent correction (2^112); the multiply normalises subnormals as well.
/// The sign is added as a bit at the very end. Inf/NaN turn into a large
/// finite number on this path -- they carry no meaning in vector data, and
/// `codec::f32_from_f16` is used whenever an exact conversion is needed.
#[inline(always)]
pub(crate) fn half(x: u16) -> f32 {
    let magic = f32::from_bits((254 - 15) << 23);
    let u = (((x & 0x8000) as u32) << 16) | (((x & 0x7fff) as u32) << 13);
    // The sign is set before the multiply: a negative value scales correctly
    // too, which takes a bit-masking step out of the loop.
    f32::from_bits(u) * magic
}

#[inline]
fn ident(x: f32) -> f32 {
    x
}
#[inline]
fn mul(x: f32, y: f32) -> f32 {
    x * y
}
#[inline]
fn diff_sq(x: f32, y: f32) -> f32 {
    let d = x - y;
    d * d
}

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
#[inline]
fn distance_hf(metric: Metric, a: &[u16], b: &[f32]) -> f32 {
    let (h, f) = (simd::halves, simd::f32s);
    match metric {
        Metric::Cosine => 1.0 - simd::strips(a, b, h, f, simd::mul, mul, half, ident),
        Metric::L2 => simd::strips(a, b, h, f, simd::diff_sq, diff_sq, half, ident),
        Metric::Dot => -simd::strips(a, b, h, f, simd::mul, mul, half, ident),
    }
}

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
#[inline]
fn distance_hh(metric: Metric, a: &[u16], b: &[u16]) -> f32 {
    let h = simd::halves;
    match metric {
        Metric::Cosine => 1.0 - simd::strips(a, b, h, h, simd::mul, mul, half, half),
        Metric::L2 => simd::strips(a, b, h, h, simd::diff_sq, diff_sq, half, half),
        Metric::Dot => -simd::strips(a, b, h, h, simd::mul, mul, half, half),
    }
}

#[cfg(not(any(
    all(target_arch = "wasm32", target_feature = "simd128"),
    all(target_arch = "aarch64", target_feature = "neon")
)))]
#[inline]
fn distance_hf(metric: Metric, a: &[u16], b: &[f32]) -> f32 {
    match metric {
        Metric::Cosine => 1.0 - strip8!(a, b, half, ident, mul, 0),
        Metric::L2 => strip8!(a, b, half, ident, diff_sq, 0),
        Metric::Dot => -strip8!(a, b, half, ident, mul, 0),
    }
}

#[cfg(not(any(
    all(target_arch = "wasm32", target_feature = "simd128"),
    all(target_arch = "aarch64", target_feature = "neon")
)))]
#[inline]
fn distance_hh(metric: Metric, a: &[u16], b: &[u16]) -> f32 {
    match metric {
        Metric::Cosine => 1.0 - strip8!(a, b, half, half, mul, 0),
        Metric::L2 => strip8!(a, b, half, half, diff_sq, 0),
        Metric::Dot => -strip8!(a, b, half, half, mul, 0),
    }
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline]
fn distance_hf(metric: Metric, a: &[u16], b: &[f32]) -> f32 {
    // SAFETY: as for `dot`.
    unsafe {
        match metric {
            Metric::Cosine => 1.0 - neon::dot_hf(a, b),
            Metric::L2 => neon::l2_hf(a, b),
            Metric::Dot => -neon::dot_hf(a, b),
        }
    }
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline]
fn distance_hh(metric: Metric, a: &[u16], b: &[u16]) -> f32 {
    // SAFETY: as for `dot`.
    unsafe {
        match metric {
            Metric::Cosine => 1.0 - neon::dot_hh(a, b),
            Metric::L2 => neon::l2_hh(a, b),
            Metric::Dot => -neon::dot_hh(a, b),
        }
    }
}

// ------------------------------------------------------ quantized kernels
//
// In `strip8!`'s order on every target: LLVM vectorises the strips natively,
// and the browser's scalar loop adds in the same order, so a graph over
// codes is the same graph in both, as over vectors. On aarch64 the int8
// strips are written out (`neon`).

#[cfg(not(all(target_arch = "aarch64", target_feature = "neon")))]
#[inline]
fn widen_i8(x: i8) -> f32 {
    x as f32
}

/// Σ code·q, the int8 arena's dot product with a query before its scale.
#[cfg(not(all(target_arch = "aarch64", target_feature = "neon")))]
#[inline]
fn dot_i8(code: &[i8], q: &[f32]) -> f32 {
    strip8!(code, q, widen_i8, ident, mul, 0)
}

/// Σ (scale·code − q)², the int8 arena's squared distance.
#[cfg(not(all(target_arch = "aarch64", target_feature = "neon")))]
#[inline]
fn l2_i8(code: &[i8], q: &[f32], scale: f32) -> f32 {
    strip8!(code, q, |x: i8| x as f32 * scale, ident, diff_sq, 0)
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline]
fn dot_i8(code: &[i8], q: &[f32]) -> f32 {
    // SAFETY: the build has NEON, which is all `neon::dot_codes` requires.
    unsafe { neon::dot_codes(code, q) }
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline]
fn l2_i8(code: &[i8], q: &[f32], scale: f32) -> f32 {
    // SAFETY: as for `dot_i8`.
    unsafe { neon::l2_codes(code, q, scale) }
}

/// The strips on aarch64, written out. Left to the vectoriser, eight codes
/// were widened as two halves, each zipped with a register it took for
/// spare -- and where that register was an accumulator, every strip waited
/// on the one before: 505 ns a 768-code distance, against 78 written out,
/// and whether a build got that register depended on the code around the
/// loop. A walk over int8 codes ran 2.5x slower than one over bit codes
/// that way, and a build 1.5x slower than now. The f32 and f16 strips it
/// read with four-way de-interleaving loads into registers of two lanes,
/// half of NEON's: a 128-dim dot product took 21 ns where two four-lane
/// registers take 9.4 with the vectors in cache, and 91 against 60 read
/// from an arena of 100 000. Each lane does the scalar loop's multiply and
/// add, and the lanes are summed as `strip8!` sums its accumulators, so the
/// result is the scalar one, bit for bit.
#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
mod neon {
    use core::arch::aarch64::*;

    /// Σ a·b.
    #[target_feature(enable = "neon")]
    pub(super) fn dot(a: &[f32], b: &[f32]) -> f32 {
        strips(
            a,
            b,
            |x| f32s(x),
            |y| f32s(y),
            |x, y| mul(x, y),
            super::mul,
            super::ident,
            super::ident,
        )
    }

    /// Σ (a − b)².
    #[target_feature(enable = "neon")]
    pub(super) fn l2(a: &[f32], b: &[f32]) -> f32 {
        strips(
            a,
            b,
            |x| f32s(x),
            |y| f32s(y),
            |x, y| diff_sq(x, y),
            super::diff_sq,
            super::ident,
            super::ident,
        )
    }

    /// Σ a·b, `a` in half precision.
    #[target_feature(enable = "neon")]
    pub(super) fn dot_hf(a: &[u16], b: &[f32]) -> f32 {
        strips(
            a,
            b,
            |x| halves(x),
            |y| f32s(y),
            |x, y| mul(x, y),
            super::mul,
            super::half,
            super::ident,
        )
    }

    /// Σ (a − b)², `a` in half precision.
    #[target_feature(enable = "neon")]
    pub(super) fn l2_hf(a: &[u16], b: &[f32]) -> f32 {
        strips(
            a,
            b,
            |x| halves(x),
            |y| f32s(y),
            |x, y| diff_sq(x, y),
            super::diff_sq,
            super::half,
            super::ident,
        )
    }

    /// Σ a·b, both in half precision.
    #[target_feature(enable = "neon")]
    pub(super) fn dot_hh(a: &[u16], b: &[u16]) -> f32 {
        strips(
            a,
            b,
            |x| halves(x),
            |y| halves(y),
            |x, y| mul(x, y),
            super::mul,
            super::half,
            super::half,
        )
    }

    /// Σ (a − b)², both in half precision.
    #[target_feature(enable = "neon")]
    pub(super) fn l2_hh(a: &[u16], b: &[u16]) -> f32 {
        strips(
            a,
            b,
            |x| halves(x),
            |y| halves(y),
            |x, y| diff_sq(x, y),
            super::diff_sq,
            super::half,
            super::half,
        )
    }

    /// Σ code·q.
    #[target_feature(enable = "neon")]
    pub(super) fn dot_codes(code: &[i8], q: &[f32]) -> f32 {
        strips(
            code,
            q,
            |c| codes(c),
            |x| f32s(x),
            |c, x| mul(c, x),
            super::mul,
            |c| c as f32,
            super::ident,
        )
    }

    /// Σ (scale·code − q)².
    #[target_feature(enable = "neon")]
    pub(super) fn l2_codes(code: &[i8], q: &[f32], scale: f32) -> f32 {
        strips(
            code,
            q,
            |c| codes(c),
            |x| f32s(x),
            |c, x| diff_sq(vmulq_n_f32(c, scale), x),
            |c, x| super::diff_sq(c * scale, x),
            |c| c as f32,
            super::ident,
        )
    }

    /// Σ q·v for four vectors.
    #[target_feature(enable = "neon")]
    pub(super) fn dots4(q: &[f32], t: [&[f32]; 4]) -> [f32; 4] {
        strips4(
            q,
            t,
            |y| f32s(y),
            |x, y| mul(x, y),
            super::mul,
            super::ident,
        )
    }

    /// Σ (q − v)² for four vectors.
    #[target_feature(enable = "neon")]
    pub(super) fn l2s4(q: &[f32], t: [&[f32]; 4]) -> [f32; 4] {
        strips4(
            q,
            t,
            |y| f32s(y),
            |x, y| diff_sq(x, y),
            super::diff_sq,
            super::ident,
        )
    }

    /// Σ v·q for four vectors in half precision, each product taken as
    /// `dot_hf` takes it, the vector's side first.
    #[target_feature(enable = "neon")]
    pub(super) fn dots4_hf(q: &[f32], t: [&[u16]; 4]) -> [f32; 4] {
        strips4(
            q,
            t,
            |y| halves(y),
            |x, y| mul(y, x),
            |x, y| super::mul(y, x),
            super::half,
        )
    }

    /// Σ (v − q)² for four vectors in half precision.
    #[target_feature(enable = "neon")]
    pub(super) fn l2s4_hf(q: &[f32], t: [&[u16]; 4]) -> [f32; 4] {
        strips4(
            q,
            t,
            |y| halves(y),
            |x, y| diff_sq(y, x),
            |x, y| super::diff_sq(y, x),
            super::half,
        )
    }

    /// [`strips`] from one query to four vectors at once: each vector's
    /// eight accumulators its own two registers, the query's strip read once
    /// for the four, each vector's lanes added and summed as `strips` adds
    /// them alone -- the same bits, with eight chains of adds in flight
    /// rather than two. `step` and `tail` take the query's side first.
    #[target_feature(enable = "neon")]
    #[inline]
    fn strips4<B: Copy>(
        q: &[f32],
        t: [&[B]; 4],
        lb: impl Fn(&[B; 8]) -> (float32x4_t, float32x4_t),
        step: impl Fn(float32x4_t, float32x4_t) -> float32x4_t,
        tail: impl Fn(f32, f32) -> f32,
        wb: impl Fn(B) -> f32,
    ) -> [f32; 4] {
        let (cq, rq) = q.as_chunks::<8>();
        let c = t.map(|v| v.as_chunks::<8>().0);
        debug_assert!(t.iter().all(|v| v.len() == q.len()));
        let z = vdupq_n_f32(0.0);
        let (mut lo, mut hi) = ([z; 4], [z; 4]);
        let strips = cq.iter().zip(c[0]).zip(c[1]).zip(c[2]).zip(c[3]);
        for ((((x, y0), y1), y2), y3) in strips {
            let (x0, x1) = f32s(x);
            for (j, y) in [y0, y1, y2, y3].into_iter().enumerate() {
                let (a, b) = lb(y);
                lo[j] = vaddq_f32(lo[j], step(x0, a));
                hi[j] = vaddq_f32(hi[j], step(x1, b));
            }
        }
        let at = cq.len() * 8;
        let mut out = [0.0f32; 4];
        for j in 0..4 {
            let (l, h) = (lanes(lo[j]), lanes(hi[j]));
            let mut s = (l[0] + l[1]) + (l[2] + l[3]) + ((h[0] + h[1]) + (h[2] + h[3]));
            for (x, y) in rq.iter().zip(&t[j][at..]) {
                s += tail(*x, wb(*y));
            }
            out[j] = s;
        }
        out
    }

    /// `strip8!` four lanes at a time: `la` and `lb` read a strip of eight
    /// of each side as two registers, `step` is each lane's part of the
    /// scalar loop, and the eight accumulators are two registers. Past the
    /// last strip `tail` runs the scalar loop over each side widened by
    /// `wa` and `wb`.
    #[allow(clippy::too_many_arguments)]
    #[target_feature(enable = "neon")]
    #[inline]
    fn strips<A: Copy, B: Copy>(
        a: &[A],
        b: &[B],
        la: impl Fn(&[A; 8]) -> (float32x4_t, float32x4_t),
        lb: impl Fn(&[B; 8]) -> (float32x4_t, float32x4_t),
        step: impl Fn(float32x4_t, float32x4_t) -> float32x4_t,
        tail: impl Fn(f32, f32) -> f32,
        wa: impl Fn(A) -> f32,
        wb: impl Fn(B) -> f32,
    ) -> f32 {
        debug_assert_eq!(a.len(), b.len());
        let (ca, ra) = a.as_chunks::<8>();
        let (cb, rb) = b.as_chunks::<8>();
        let (mut lo, mut hi) = (vdupq_n_f32(0.0), vdupq_n_f32(0.0));
        for (x, y) in ca.iter().zip(cb) {
            let ((x0, x1), (y0, y1)) = (la(x), lb(y));
            lo = vaddq_f32(lo, step(x0, y0));
            hi = vaddq_f32(hi, step(x1, y1));
        }
        let (l, h) = (lanes(lo), lanes(hi));
        let mut s = (l[0] + l[1]) + (l[2] + l[3]) + ((h[0] + h[1]) + (h[2] + h[3]));
        for (x, y) in ra.iter().zip(rb) {
            s += tail(wa(*x), wb(*y));
        }
        s
    }

    /// Each lane's multiply, not fused with the add: the scalar loop rounds
    /// between the two.
    #[target_feature(enable = "neon")]
    #[inline]
    fn mul(x: float32x4_t, y: float32x4_t) -> float32x4_t {
        vmulq_f32(x, y)
    }

    #[target_feature(enable = "neon")]
    #[inline]
    fn diff_sq(x: float32x4_t, y: float32x4_t) -> float32x4_t {
        let d = vsubq_f32(x, y);
        vmulq_f32(d, d)
    }

    /// Eight f32 as two registers.
    #[target_feature(enable = "neon")]
    #[inline]
    fn f32s(x: &[f32; 8]) -> (float32x4_t, float32x4_t) {
        (four(&x[..4]), four(&x[4..]))
    }

    /// Eight codes, sign-extended to sixteen bits and then to 32.
    #[target_feature(enable = "neon")]
    #[inline]
    fn codes(c: &[i8; 8]) -> (float32x4_t, float32x4_t) {
        let w = vmovl_s8(vcreate_s8(u64::from_le_bytes(c.map(|b| b as u8))));
        (
            vcvtq_f32_s32(vmovl_s16(vget_low_s16(w))),
            vcvtq_f32_s32(vmovl_high_s16(w)),
        )
    }

    /// Eight f16 widened as [`super::half`] widens one -- the same masks,
    /// shifts and multiply, so the same bits, where NEON's own conversion
    /// would turn infinities and NaNs into what `half` does not.
    #[target_feature(enable = "neon")]
    #[inline]
    fn halves(x: &[u16; 8]) -> (float32x4_t, float32x4_t) {
        let pack = |h: &[u16]| {
            (h[0] as u64) | (h[1] as u64) << 16 | (h[2] as u64) << 32 | (h[3] as u64) << 48
        };
        let v = vcombine_u16(vcreate_u16(pack(&x[..4])), vcreate_u16(pack(&x[4..])));
        let widen = |u: uint32x4_t| {
            let sign = vshlq_n_u32::<16>(vandq_u32(u, vdupq_n_u32(0x8000)));
            let mag = vshlq_n_u32::<13>(vandq_u32(u, vdupq_n_u32(0x7fff)));
            vmulq_f32(
                vreinterpretq_f32_u32(vorrq_u32(sign, mag)),
                vdupq_n_f32(f32::from_bits((254 - 15) << 23)),
            )
        };
        (widen(vmovl_u16(vget_low_u16(v))), widen(vmovl_high_u16(v)))
    }

    /// Four f32 as a register, built from the elements: LLVM folds the
    /// reads into one load.
    #[target_feature(enable = "neon")]
    #[inline]
    fn four(x: &[f32]) -> float32x4_t {
        let v = vdupq_n_f32(x[0]);
        let v = vsetq_lane_f32::<1>(x[1], v);
        let v = vsetq_lane_f32::<2>(x[2], v);
        vsetq_lane_f32::<3>(x[3], v)
    }

    #[target_feature(enable = "neon")]
    #[inline]
    fn lanes(v: float32x4_t) -> [f32; 4] {
        [
            vgetq_lane_f32::<0>(v),
            vgetq_lane_f32::<1>(v),
            vgetq_lane_f32::<2>(v),
            vgetq_lane_f32::<3>(v),
        ]
    }
}

/// Σ ±q, the sign each bit gives: the bit arena's dot product with a query
/// before its 1/√dim. A clear bit sets the float's sign bit rather than
/// branching, so the strips vectorise as `dot`'s do.
#[inline]
fn dot_bits(bits: &[u64], q: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let (cq, rq) = q.as_chunks::<8>();
    let mut at = 0usize;
    for x in cq {
        let byte = (bits[at / 64] >> (at % 64)) as u32;
        for k in 0..8 {
            let flip = (!byte >> k & 1) << 31;
            acc[k] += f32::from_bits(x[k].to_bits() ^ flip);
        }
        at += 8;
    }
    let mut s = (acc[0] + acc[1]) + (acc[2] + acc[3]) + ((acc[4] + acc[5]) + (acc[6] + acc[7]));
    for x in rq {
        let flip = ((!(bits[at / 64] >> (at % 64)) & 1) as u32) << 31;
        s += f32::from_bits(x.to_bits() ^ flip);
        at += 1;
    }
    s
}

/// The bits a query is cut to a component, a plane each ([`dot_planes`]).
/// RaBitQ cuts to four the query's distance from a centre. Cut whole, as
/// here, a query is mostly its own centre, and four bits were too coarse
/// for what tells a crowded cluster's members apart: the best 40 of 3 000
/// by the codes held 84% of the true ten, five bits 89.5%, and six the
/// 91.5% the query whole holds, which more bits did not add to. Six planes
/// are 72 popcounts over 768 dimensions: at 100 000 x 768 a query at a
/// beam of 200 took 0.292 ms against 0.510 whole, and eight planes 0.303.
const QUERY_BITS: usize = 6;

/// `Σ ±q̄`, the sign each bit gives, over a query cut to [`QUERY_BITS`] bits
/// a component ([`VectorIndex::query_for`]): `planes` holds, a code word at
/// a time, that word of each of the query's bit planes in two's complement,
/// as the bits of two `f32`s each -- they ride in the query's own vector,
/// which every search hands on. `Σ ±q̄` is `2T - Σq̄` with `T` the query
/// summed where a sign is set: the planes' popcounts under the sign words,
/// weighed 1, 2, 4 and on, the top plane's negative. A few dozen integer
/// steps where [`dot_bits`] adds a float a component, and exact, so every
/// target sums it alike.
#[inline]
fn dot_planes(bits: &[u64], planes: &[f32]) -> i32 {
    let mut t = [0u32; QUERY_BITS];
    let (words, _) = planes.as_chunks::<{ 2 * QUERY_BITS }>();
    for (b, p) in bits.iter().zip(words) {
        for (j, t) in t.iter_mut().enumerate() {
            let plane = p[2 * j].to_bits() as u64 | (p[2 * j + 1].to_bits() as u64) << 32;
            *t += (b & plane).count_ones();
        }
    }
    let mut sum = 0i32;
    for (j, t) in t.iter().enumerate() {
        let weight = if j + 1 == QUERY_BITS {
            -(1 << j)
        } else {
            1 << j
        };
        sum += weight * *t as i32;
    }
    sum
}

// ----------------------------------------------------------------- arena

/// Per node of a batch insert: its id, its level, and the neighbours found
/// for it at each level -- what finding them hands linking them.
type Candidates = Vec<(u32, usize, Vec<(usize, Vec<u32>)>)>;

/// Runs `work` for every index of `0..n` on a thread per state, and hands
/// the indexes out one at a time as each thread comes back for another:
/// cut into a share a thread, the M1's four efficiency cores finished
/// their shares last while the other four waited. The results come back
/// in no order.
#[cfg(not(target_family = "wasm"))]
fn spread<S: Send, T: Send>(
    n: usize,
    states: &mut [S],
    work: impl Fn(&mut S, usize) -> T + Sync,
) -> Vec<T> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let (next, work) = (&next, &work);
    let run = move |state: &mut S| {
        let mut out = Vec::new();
        loop {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if i >= n {
                return out;
            }
            out.push(work(state, i));
        }
    };
    let Some((mine, others)) = states.split_first_mut() else {
        return Vec::new();
    };
    std::thread::scope(|scope| {
        let handles: Vec<_> = others
            .iter_mut()
            .take(n.saturating_sub(1))
            .map(|state| scope.spawn(move || run(state)))
            .collect();
        // The calling thread takes a share too, rather than wait.
        let mut out = run(mine);
        for h in handles {
            out.extend(h.join().unwrap_or_default());
        }
        out
    })
}

/// No threads in the browser: every index in turn, on the first state.
#[cfg(target_family = "wasm")]
fn spread<S, T>(n: usize, states: &mut [S], work: impl Fn(&mut S, usize) -> T) -> Vec<T> {
    match states.first_mut() {
        Some(state) => (0..n).map(|i| work(state, i)).collect(),
        None => Vec::new(),
    }
}

/// Vector arena: every vector in one contiguous array, strided by `node * dim`.
///
/// The `F16` variant fits the same array into half the space. The query side
/// is always f32; widening happens inside the distance kernel, so there is no
/// extra allocation on the search path.
///
/// `I8` and `Bit` hold codes instead (`quant=`): a byte a component over a
/// scale a vector -- the largest component over 127 -- or the signs of each
/// vector's distance from the nearest of a few centres ([`Bits`]). Their
/// distances are estimates, and `near` puts the candidates they find in
/// order again by the documents' own vectors.
pub(crate) enum Arena {
    F32(Rows<f32>),
    F16(Rows<u16>),
    I8(Rows<i8>, Vec<f32>),
    Bit(Bits),
}

/// An arena's vectors, a row of `dim` after another. Natively a `Vec`,
/// grown by doubling: the allocator gives a freed block back, and a large
/// one is moved rather than copied on Linux.
#[cfg(not(target_family = "wasm"))]
pub(crate) type Rows<T> = Vec<T>;

/// In the browser module a list of chunks of about [`ROWS_CHUNK`] bytes, a
/// whole number of rows each, the last grown by doubling up to that. A
/// module's memory grows and is never given back, and one `Vec` doubled
/// with old and new side by side: 5 000 768-dim vectors held a 25 MB
/// arena, reached with its 12.6 MB before it still there, and left the
/// module at 100 MB against the 31 its rows and documents are.
#[cfg(target_family = "wasm")]
pub(crate) struct Rows<T> {
    chunks: Vec<Vec<T>>,
    /// Where each chunk's values start, for [`RowsOf::row`] to reach a row
    /// in a load and one bounds check, as a `Vec` does: through `chunks`,
    /// with their checks, a 10 000 x 128 build took 11% longer.
    starts: Vec<*const T>,
    /// Rows a chunk holds, as a power of two, set by the first row.
    shift: u32,
    rows: usize,
}

#[cfg(target_family = "wasm")]
const ROWS_CHUNK: usize = 1 << 20;

/// What the engine asks of [`Rows`] on every target.
pub(crate) trait RowsOf<T: Copy> {
    /// Row `i` of `width`.
    fn row(&self, i: usize, width: usize) -> &[T];
    /// Appends a row of `width` from `values`.
    fn push_row(&mut self, width: usize, values: impl IntoIterator<Item = T>);
    /// The values held, rows times width.
    fn values(&self) -> usize;
}

#[cfg(not(target_family = "wasm"))]
impl<T: Copy> RowsOf<T> for Vec<T> {
    #[inline(always)]
    fn row(&self, i: usize, width: usize) -> &[T] {
        let s = i * width;
        &self[s..s + width]
    }
    #[inline]
    fn push_row(&mut self, _: usize, values: impl IntoIterator<Item = T>) {
        self.extend(values);
    }
    fn values(&self) -> usize {
        self.len()
    }
}

#[cfg(target_family = "wasm")]
impl<T> Rows<T> {
    pub(crate) fn new() -> Rows<T> {
        Rows {
            chunks: Vec::new(),
            starts: Vec::new(),
            shift: 0,
            rows: 0,
        }
    }

    /// Nothing: room past the chunk being filled would be the doubling
    /// the chunks are there to avoid.
    pub(crate) fn reserve(&mut self, _: usize) {}

    /// The chunk the next row of `width` goes into, with room for it.
    /// Apart from [`RowsOf::push_row`], which is compiled for every
    /// iterator it is handed: in it, the module grew 0.6 KB brotli.
    fn room(&mut self, width: usize) -> Option<&mut Vec<T>> {
        if width == 0 {
            return None;
        }
        if self.chunks.is_empty() {
            let rows = (ROWS_CHUNK / (width * size_of::<T>())).max(1);
            self.shift = usize::BITS - 1 - rows.leading_zeros();
        }
        let full = (1 << self.shift) * width;
        if self.chunks.last().is_none_or(|c| c.len() >= full) {
            self.chunks.push(Vec::new());
            self.starts.push(std::ptr::null());
        }
        let last = self.chunks.last_mut()?;
        // Doubled as a `Vec` would be, but never past the chunk.
        if last.capacity() - last.len() < width {
            let to = (last.capacity() * 2).clamp(width, full);
            last.reserve_exact(to - last.len());
        }
        Some(last)
    }

    /// Notes a row pushed into the last chunk, which starts at `start`.
    fn landed(&mut self, whole: bool, start: *const T) {
        // A row cut short would leave the next one straddling two places.
        assert!(whole);
        if let Some(s) = self.starts.last_mut() {
            *s = start;
        }
        self.rows += 1;
    }
}

#[cfg(target_family = "wasm")]
impl<T: Copy> RowsOf<T> for Rows<T> {
    #[inline(always)]
    fn row(&self, i: usize, width: usize) -> &[T] {
        assert!(i < self.rows);
        let at = (i & ((1 << self.shift) - 1)) * width;
        // SAFETY: row `i` was pushed, so its chunk is `starts[i >> shift]`
        // and holds the `width` values from `at`; a chunk is never moved
        // once a row is in it but by `push_row`, which notes where it went.
        unsafe {
            std::slice::from_raw_parts(self.starts.get_unchecked(i >> self.shift).add(at), width)
        }
    }

    fn push_row(&mut self, width: usize, values: impl IntoIterator<Item = T>) {
        let Some(last) = self.room(width) else {
            return;
        };
        let before = last.len();
        last.extend(values);
        let (whole, start) = (last.len() - before == width, last.as_ptr());
        self.landed(whole, start);
    }

    fn values(&self) -> usize {
        self.chunks.iter().map(Vec::len).sum()
    }
}

/// Vectors a `quant=bit` index holds whole, at the field's precision, before
/// it learns its centres from them and codes them all: with fewer, a code
/// has no centres to be taken from. An index smaller than this searches
/// exactly.
pub(crate) const BIT_TRAIN: usize = 2048;

/// Centres per vector learned from, one in sixteen: over 100 000 vectors in
/// 64 clusters, spread in every dimension, 128 centres from the first 2 048
/// vectors let a beam of 100 by the codes hold 91.2% of the true ten, 64
/// held 81.4% -- a centre between two clusters -- and 256 from as many
/// vectors 89.4%, 92.5% from 4 096 (brute force, the codes' best 100 in
/// exact order). The cell is a byte.
const BIT_PER_CENTRE: usize = 16;

/// Lloyd rounds after the seeding: on those clusters two gave what eight
/// did, to the fourth decimal.
const BIT_ROUNDS: usize = 4;

/// What a bit graph record writes where the quantization's code goes: bit
/// codes taken from centres, the centres following. The plain signs before
/// them wrote [`Quant::Bit`]'s own code, which a graph is now built again
/// for, and a binary before them does not know this one, and builds its own.
const BIT_CENTRED: u8 = 3;

/// A bit arena's codes. A unit vector `v` is the nearest centre `C` plus a
/// residual `r`, and the code keeps `r`'s signs `b`. For any `u`, `⟨r, u⟩`
/// is estimated as `σ⟨b, u⟩` with `σ = |r|²/|r|₁` -- exact for `u = r`, as
/// `⟨b, r⟩ = |r|₁` (RaBitQ's estimator, without its rotation) -- and off by
/// as much more as `u` lies away from `r`. So the query is taken from the
/// centre: `⟨q, v⟩ = ⟨q, C⟩ + ⟨C, r⟩ + ⟨q - C, r⟩`, the first once a query a
/// centre, the second exact, and only the third estimated, which comes to
/// `⟨q, C⟩ + κ + σ⟨b, q⟩` with `κ = ⟨C, r⟩ - σ⟨b, C⟩` kept beside `σ`.
///
/// The signs of the vectors themselves were the code before: in a cluster
/// crowded around its centre most signs are the centre's, and they told the
/// members apart so poorly that a beam of 100 by them held 36.9% of the true
/// ten over 100 000 vectors spread in every dimension, against 91.2% now
/// (brute force). Estimated whole, `⟨q, r⟩` put the query's share along the
/// centre through the signs as well: over the same 256 centres, 38.7%
/// against 92.8%.
pub(crate) struct Bits {
    /// A node's residual's signs, `dim.div_ceil(64)` words, and a word of
    /// its `σ` and `κ`, [`Bits::stride`] words a node: in an array of their
    /// own, the factors were a cache miss more a distance.
    words: Vec<u64>,
    /// The centre each node's residual is from: a byte a node, an array
    /// small enough to stay in the cache.
    cells: Vec<u8>,
    stride: usize,
    /// Shared with the arena `retire` codes a vector into to compare it.
    centres: Arc<Centres>,
    /// Whether a vector is coded through half precision, as the field's
    /// first vectors were held: coded from f32 after them, a vector written
    /// again unchanged no longer matched its code, and was retired.
    half: bool,
}

/// The centres a bit arena's residuals are taken from: k-means over the
/// first [`BIT_TRAIN`] vectors, kept in the graph record so that a restored
/// graph codes its vectors as they were coded.
pub(crate) struct Centres {
    /// `dim`-strided.
    at: Vec<f32>,
    /// Half each centre's squared length: the nearest to a vector is the one
    /// whose dot product with it, less this, is largest.
    half: Vec<f32>,
}

impl Centres {
    fn new(at: Vec<f32>, dim: usize) -> Centres {
        let half = at.chunks(dim).map(|c| dot(c, c) / 2.0).collect();
        Centres { at, half }
    }

    #[inline]
    fn centre(&self, c: usize, dim: usize) -> &[f32] {
        &self.at[c * dim..(c + 1) * dim]
    }

    /// The centre nearest `v`, and its score: not finite for a vector that
    /// is not all numbers.
    fn nearest(&self, v: &[f32]) -> (usize, f32) {
        let dim = v.len();
        let (mut best, mut at) = (f32::NEG_INFINITY, 0);
        for (c, half) in self.half.iter().enumerate() {
            let s = dot(v, self.centre(c, dim)) - half;
            if s > best {
                (best, at) = (s, c);
            }
        }
        (at, best)
    }

    /// k-means over `rows`: seeded by k-means++ -- each next centre a row
    /// drawn by its squared distance from the nearest so far -- then
    /// [`BIT_ROUNDS`] rounds of each centre moved to the mean of its rows.
    /// Deterministic, in one order on every target, so the browser learns
    /// the centres a server does. A row holding what is not a number weighs
    /// nothing and moves no centre: one would have made every centre after
    /// it not a number.
    fn learn(rows: &[f32], dim: usize) -> Centres {
        let n = rows.len() / dim;
        let row = |i: usize| &rows[i * dim..(i + 1) * dim];
        let want = (n / BIT_PER_CENTRE).clamp(1, 256);
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let mut far = vec![0.0f32; n];
        let mut at = Vec::with_capacity(want * dim);
        let mut next = (0..n)
            .find(|&i| l2_sq(row(i), row(i)).is_finite())
            .unwrap_or(0);
        loop {
            let c = row(next);
            at.extend_from_slice(c);
            for (i, far) in far.iter_mut().enumerate() {
                let d = l2_sq(row(i), c);
                if at.len() == dim {
                    *far = if d.is_finite() { d } else { 0.0 };
                } else if d < *far {
                    *far = d;
                }
            }
            let total: f64 = far.iter().map(|&d| d as f64).sum();
            // Enough, or every row a centre already.
            if at.len() == want * dim || total <= 0.0 {
                break;
            }
            let mut pick = rng.next_f32() as f64 * total;
            next = n - 1;
            for (i, &d) in far.iter().enumerate() {
                pick -= d as f64;
                if pick < 0.0 {
                    next = i;
                    break;
                }
            }
        }
        let k = at.len() / dim;
        for _ in 0..BIT_ROUNDS {
            let centres = Centres::new(at, dim);
            let (mut sum, mut count) = (vec![0.0f32; k * dim], vec![0u32; k]);
            for i in 0..n {
                let (c, score) = centres.nearest(row(i));
                if score.is_finite() {
                    count[c] += 1;
                    for (s, x) in sum[c * dim..(c + 1) * dim].iter_mut().zip(row(i)) {
                        *s += x;
                    }
                }
            }
            at = centres.at;
            // A centre no row is nearest stays where it was.
            for c in (0..k).filter(|&c| count[c] > 0) {
                for j in c * dim..(c + 1) * dim {
                    at[j] = sum[j] / count[c] as f32;
                }
            }
        }
        Centres::new(at, dim)
    }
}

impl Bits {
    fn new(centres: Arc<Centres>, dim: usize, half: bool) -> Bits {
        Bits {
            words: Vec::new(),
            cells: Vec::new(),
            stride: dim.div_ceil(64) + 1,
            centres,
            half,
        }
    }

    /// A node's signs and its `[σ, κ]`.
    #[inline]
    fn code(&self, node: u32) -> (&[u64], [f32; 2]) {
        let at = node as usize * self.stride;
        let (signs, terms) = self.words[at..at + self.stride].split_at(self.stride - 1);
        let t = terms[0];
        (
            signs,
            [f32::from_bits(t as u32), f32::from_bits((t >> 32) as u32)],
        )
    }

    fn push_terms(&mut self, [sigma, kappa]: [f32; 2]) {
        self.words
            .push(sigma.to_bits() as u64 | (kappa.to_bits() as u64) << 32);
    }

    /// Codes `v`, a unit vector under cosine, at the field's precision,
    /// against the nearest centre: the residual's signs, and its `σ` and
    /// `κ` summed in one pass in the components' order, which is every
    /// target's.
    fn push(&mut self, v: &[f32]) {
        let dim = v.len();
        let cell = self.centres.nearest(v).0;
        let c = self.centres.centre(cell, dim);
        let (mut rr, mut l1, mut cr, mut cb) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        let mut w = 0u64;
        for (i, (x, y)) in v.iter().zip(c).enumerate() {
            let r = x - y;
            rr += r * r;
            l1 += r.abs();
            cr += y * r;
            if r > 0.0 {
                w |= 1 << (i % 64);
                cb += y;
            } else {
                cb -= y;
            }
            if i % 64 == 63 || i + 1 == dim {
                self.words.push(w);
                w = 0;
            }
        }
        let sigma = if l1 > 0.0 { rr / l1 } else { 0.0 };
        self.cells.push(cell as u8);
        self.push_terms([sigma, cr - sigma * cb]);
    }

    /// `⟨q, v⟩` as the code estimates it. `q` is `dim` long -- a code
    /// widened back, which `select_heuristic` measures with, measured whole
    /// -- or a search's query as [`VectorIndex::query_for`] makes it: its
    /// dot product with each centre follows it, then the query cut to
    /// [`QUERY_BITS`] bits a component, its step and its sum, and its bit
    /// planes.
    #[inline]
    fn estimate(&self, q: &[f32], node: u32, dim: usize) -> f32 {
        let cell = self.cells[node as usize] as usize;
        let (signs, [sigma, kappa]) = self.code(node);
        let cut = dim + self.centres.half.len();
        match q.get(cut..cut + 2) {
            Some(&[step, sum]) => {
                // `2T - Σq̄` is a whole number short of 2^24: exact as a float.
                let t = dot_planes(signs, &q[cut + 2..]);
                q[dim + cell] + kappa + sigma * (step * ((2 * t) as f32 - sum))
            }
            _ => {
                let qc = dot(&q[..dim], self.centres.centre(cell, dim));
                qc + kappa + sigma * dot_bits(signs, &q[..dim])
            }
        }
    }
}

impl Arena {
    /// An empty arena for vectors of `prec` under `quant`: a bit index's
    /// holds them whole until it has learned its centres from them.
    fn new(prec: VecPrec, quant: Quant) -> Arena {
        match (quant, prec) {
            (Quant::Int8, _) => Arena::I8(Rows::new(), Vec::new()),
            (_, VecPrec::F32) => Arena::F32(Rows::new()),
            (_, VecPrec::F16) => Arena::F16(Rows::new()),
        }
    }

    /// An empty arena that codes as this one does.
    fn empty_like(&self) -> Arena {
        match self {
            Arena::F32(_) => Arena::F32(Rows::new()),
            Arena::F16(_) => Arena::F16(Rows::new()),
            Arena::I8(..) => Arena::I8(Rows::new(), Vec::new()),
            Arena::Bit(b) => Arena::Bit(Bits {
                words: Vec::new(),
                cells: Vec::new(),
                centres: b.centres.clone(),
                ..*b
            }),
        }
    }

    /// The vectors of this arena, held whole, coded against centres learned
    /// from them.
    fn coded(&self, nodes: usize, dim: usize) -> Arena {
        let mut rows = Vec::with_capacity(nodes * dim);
        let mut v = Vec::with_capacity(dim);
        for node in 0..nodes {
            self.read_into(node as u32, dim, &mut v);
            rows.extend_from_slice(&v);
        }
        let half = matches!(self, Arena::F16(_));
        let mut bits = Bits::new(Arc::new(Centres::learn(&rows, dim)), dim, half);
        bits.words.reserve(nodes * bits.stride);
        for row in rows.chunks(dim) {
            bits.push(row);
        }
        Arena::Bit(bits)
    }

    fn quantized(&self) -> bool {
        matches!(self, Arena::I8(..) | Arena::Bit(_))
    }

    fn reserve(&mut self, nodes: usize, dim: usize) {
        match self {
            Arena::F32(d) => d.reserve(nodes * dim),
            Arena::F16(d) => d.reserve(nodes * dim),
            Arena::I8(c, s) => {
                c.reserve(nodes * dim);
                s.reserve(nodes);
            }
            Arena::Bit(b) => {
                b.words.reserve(nodes * b.stride);
                b.cells.reserve(nodes);
            }
        }
    }

    /// What a node's code adds to every product it estimates: `κ` over bit
    /// codes, which a product of two codes takes from both, and nothing
    /// otherwise.
    #[inline]
    fn offset(&self, node: u32) -> f32 {
        match self {
            Arena::Bit(b) => b.code(node).1[1],
            _ => 0.0,
        }
    }

    /// Appends the vector. When `unit` it is first scaled to unit length
    /// (cosine). On the f32 path normalisation happens in place on the arena,
    /// so there is no intermediate `Vec` allocation; on the f16 path the
    /// scale is applied during the conversion anyway.
    fn push(&mut self, raw: &[f32], unit: bool) {
        let inv = match unit {
            true => unit_scale(flat_sq(raw)),
            false => 1.0,
        };
        self.push_scaled(raw, inv);
    }

    /// Pushes `raw` times `inv`, the scale [`unit_scale`] gave it.
    fn push_scaled(&mut self, raw: &[f32], inv: f32) {
        match self {
            // Scaled as it is copied: a pass over the vector less.
            Arena::F32(d) if inv != 1.0 => d.push_row(raw.len(), raw.iter().map(|x| x * inv)),
            Arena::F32(d) => d.push_row(raw.len(), raw.iter().copied()),
            Arena::F16(d) => {
                d.push_row(
                    raw.len(),
                    raw.iter().map(|x| crate::codec::f16_from_f32(x * inv)),
                );
            }
            Arena::I8(codes, scales) => {
                let top = raw.iter().fold(0.0f32, |m, x| m.max((x * inv).abs()));
                let scale = if top > 0.0 { top / 127.0 } else { 1.0 };
                codes.push_row(
                    raw.len(),
                    raw.iter().map(|x| (x * inv / scale).round() as i8),
                );
                scales.push(scale);
            }
            Arena::Bit(bits) => {
                let v: Vec<f32> = raw
                    .iter()
                    .map(|x| match bits.half {
                        true => half(crate::codec::f16_from_f32(x * inv)),
                        false => x * inv,
                    })
                    .collect();
                bits.push(&v);
            }
        }
    }

    #[inline]
    fn slice_f32(&self, node: u32, dim: usize) -> Option<&[f32]> {
        match self {
            Arena::F32(d) => Some(d.row(node as usize, dim)),
            _ => None,
        }
    }

    #[inline]
    fn dist_to(&self, metric: Metric, q: &[f32], node: u32, dim: usize) -> f32 {
        let n = node as usize;
        match self {
            Arena::F32(d) => distance(metric, q, d.row(n, dim)),
            Arena::F16(d) => distance_hf(metric, d.row(n, dim), q),
            Arena::I8(c, sc) => {
                let (code, scale) = (c.row(n, dim), sc[n]);
                match metric {
                    Metric::Cosine => 1.0 - scale * dot_i8(code, q),
                    Metric::L2 => l2_i8(code, q, scale),
                    Metric::Dot => -scale * dot_i8(code, q),
                }
            }
            Arena::Bit(b) => 1.0 - b.estimate(q, node, dim),
        }
    }

    /// [`Arena::dist_to`] for each of `nodes`, into `out`, bit for bit: four
    /// vectors at a time where the arena holds vectors ([`distances4`]), one
    /// at a time over codes. With `AHEAD` the vectors two groups on are
    /// asked for while a group is measured: an exact search reads rows
    /// scattered over the arena, each a wait on memory, and 10 000 of a
    /// million 128-dim vectors took 0.82 ms against 0.41 asked ahead. The
    /// walk asks for its neighbours' itself, and a pruning's lists are few.
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    fn dists_to<const AHEAD: bool>(
        &self,
        metric: Metric,
        q: &[f32],
        nodes: &[u32],
        dim: usize,
        out: &mut Vec<f32>,
    ) {
        out.clear();
        let (four, rest) = nodes.as_chunks::<4>();
        match self {
            Arena::F32(d) => {
                for (i, c) in four.iter().enumerate() {
                    if let Some(ahead) = four.get(i + 2).filter(|_| AHEAD) {
                        for &n in ahead {
                            // 32 floats, the M series' 128-byte line.
                            for line in d[n as usize * dim..][..dim].chunks(32) {
                                prefetch(line.as_ptr().cast());
                            }
                        }
                    }
                    let t = c.map(|n| &d[n as usize * dim..][..dim]);
                    out.extend(distances4(metric, q, t));
                }
            }
            Arena::F16(d) => {
                for c in four {
                    let t = c.map(|n| &d[n as usize * dim..][..dim]);
                    out.extend(distances4_hf(metric, q, t));
                }
            }
            Arena::I8(..) | Arena::Bit(_) => {
                out.extend(nodes.iter().map(|&n| self.dist_to(metric, q, n, dim)));
                return;
            }
        }
        out.extend(rest.iter().map(|&n| self.dist_to(metric, q, n, dim)));
    }

    #[inline]
    fn dist_nodes(&self, metric: Metric, a: u32, b: u32, dim: usize) -> f32 {
        let (ra, rb) = (a as usize, b as usize);
        match self {
            Arena::F32(d) => distance(metric, d.row(ra, dim), d.row(rb, dim)),
            Arena::F16(d) => distance_hh(metric, d.row(ra, dim), d.row(rb, dim)),
            // Off the hot path: `select_heuristic` widens codes once and
            // keeps them, as it does halves.
            Arena::I8(..) | Arena::Bit(_) => {
                distance(metric, &self.vec_at(a, dim), &self.vec_at(b, dim))
                    - self.offset(a)
                    - self.offset(b)
            }
        }
    }

    /// Returns the node's vector as f32. A borrow in the f32 arena, a widened
    /// copy in f16 -- the caller uses it like a slice either way.
    #[inline]
    fn vec_at(&self, node: u32, dim: usize) -> Cow<'_, [f32]> {
        match self {
            Arena::F32(_) => Cow::Borrowed(self.slice_f32(node, dim).unwrap()),
            _ => {
                let mut v = Vec::with_capacity(dim);
                self.read_into(node, dim, &mut v);
                Cow::Owned(v)
            }
        }
    }

    /// Widens the node's vector into the given buffer. Unlike `vec_at` it does
    /// not allocate a fresh `Vec` on every call on the f16 path -- the same
    /// buffer is reused in the hot build loop.
    #[inline]
    fn read_into(&self, node: u32, dim: usize, out: &mut Vec<f32>) {
        out.clear();
        let n = node as usize;
        match self {
            Arena::F32(d) => out.extend_from_slice(d.row(n, dim)),
            Arena::F16(d) => out.extend(d.row(n, dim).iter().map(|x| half(*x))),
            Arena::I8(c, sc) => {
                let scale = sc[n];
                out.extend(c.row(n, dim).iter().map(|x| *x as f32 * scale));
            }
            // The centre and the residual's signs at its scale: what the
            // code's products with a vector estimate, `κ` apart
            // ([`Arena::offset`]).
            Arena::Bit(b) => {
                let (signs, [sigma, _]) = b.code(node);
                let c = b.centres.centre(b.cells[node as usize] as usize, dim);
                out.extend(c.iter().enumerate().map(|(i, x)| {
                    if signs[i / 64] >> (i % 64) & 1 == 1 {
                        x + sigma
                    } else {
                        x - sigma
                    }
                }));
            }
        }
    }

    /// Appends the node's vector as the arena holds it, little-endian.
    fn write_stored(&self, node: u32, dim: usize, out: &mut Vec<u8>) {
        let n = node as usize;
        match self {
            Arena::F32(d) => d
                .row(n, dim)
                .iter()
                .for_each(|x| out.extend_from_slice(&x.to_le_bytes())),
            Arena::F16(d) => d
                .row(n, dim)
                .iter()
                .for_each(|x| out.extend_from_slice(&x.to_le_bytes())),
            Arena::I8(c, sc) => {
                out.extend(c.row(n, dim).iter().map(|x| *x as u8));
                out.extend_from_slice(&sc[node as usize].to_le_bytes());
            }
            Arena::Bit(b) => {
                let (signs, terms) = b.code(node);
                signs
                    .iter()
                    .for_each(|x| out.extend_from_slice(&x.to_le_bytes()));
                out.push(b.cells[node as usize]);
                terms
                    .iter()
                    .for_each(|x| out.extend_from_slice(&x.to_le_bytes()));
            }
        }
    }

    /// Appends a vector written by `write_stored`, as it was: it was already
    /// normalised, and normalising again would move its last bits.
    fn push_stored(&mut self, bytes: &[u8]) {
        match self {
            Arena::F32(d) => {
                let v = bytes.as_chunks::<4>().0;
                d.push_row(v.len(), v.iter().map(|b| f32::from_le_bytes(*b)));
            }
            Arena::F16(d) => {
                let v = bytes.as_chunks::<2>().0;
                d.push_row(v.len(), v.iter().map(|b| u16::from_le_bytes(*b)));
            }
            Arena::I8(c, sc) => {
                let (code, scale) = bytes.split_at(bytes.len() - 4);
                c.push_row(code.len(), code.iter().map(|x| *x as i8));
                sc.push(f32::from_le_bytes([scale[0], scale[1], scale[2], scale[3]]));
            }
            Arena::Bit(b) => {
                let (words, rest) = bytes.split_at(bytes.len() - 9);
                let f =
                    |i: usize| f32::from_le_bytes([rest[i], rest[i + 1], rest[i + 2], rest[i + 3]]);
                b.words.extend(
                    words
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|x| u64::from_le_bytes(*x)),
                );
                b.cells.push(rest[0]);
                b.push_terms([f(1), f(5)]);
            }
        }
    }

    /// Bytes a vector of `dim` takes in `write_stored`'s form.
    fn stored_len(&self, dim: usize) -> usize {
        match self {
            Arena::F32(_) => dim * 4,
            Arena::F16(_) => dim * 2,
            Arena::I8(..) => dim + 4,
            Arena::Bit(_) => dim.div_ceil(64) * 8 + 9,
        }
    }

    /// Bytes the arena occupies in memory (for statistics).
    pub(crate) fn bytes(&self) -> usize {
        match self {
            Arena::F32(d) => d.values() * 4,
            Arena::F16(d) => d.values() * 2,
            Arena::I8(c, s) => c.values() + s.len() * 4,
            Arena::Bit(b) => {
                b.words.len() * 8 + b.cells.len() + (b.centres.at.len() + b.centres.half.len()) * 4
            }
        }
    }
}

// -------------------------------------------------------------- helpers

#[derive(Copy, Clone, PartialEq)]
struct Cand {
    dist: f32,
    node: u32,
}
impl Eq for Cand {}
impl Ord for Cand {
    fn cmp(&self, other: &Self) -> Ordering {
        // Not `partial_cmp(..).unwrap_or(Equal)`: a distance holding NaN comes
        // out "equal" to everything, produces a non-transitive ordering, and
        // `sort` notices that and panics. `total_cmp` is a real total order;
        // behaviour on finite values is unchanged.
        self.dist.total_cmp(&other.dist)
    }
}
impl PartialOrd for Cand {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The nodes an exact search measures at a time on a thread of its own
/// ([`VectorIndex::nearest_of`]), and the fewest it spreads over the cores.
#[cfg(not(target_family = "wasm"))]
const MEASURE_SHARE: usize = 8192;
#[cfg(not(target_family = "wasm"))]
const MEASURE_APART: usize = 4 * MEASURE_SHARE;

/// Each component of `v` rounded to a half and widened back, as a
/// `vector<N, f16>` field's record holds it.
fn halved(v: &[f32]) -> Vec<f32> {
    v.iter()
        .map(|&x| half(crate::codec::f16_from_f32(x)))
        .collect()
}

/// Up to this many nearest are kept as they are measured, the rest let go
/// at a comparison; more are sorted whole, as a page past it is rare and
/// keeping them would move as many on each insert.
const NEAREST_KEPT: usize = 128;

/// The first `k` of `all` as a stable sort by distance leaves them: ties
/// in the order they came. Sorting every one of 250 000 to keep ten was a
/// quarter of an exact search over them.
fn nearest(all: impl IntoIterator<Item = Cand>, k: usize) -> Vec<Cand> {
    if k == 0 {
        return Vec::new();
    }
    if k > NEAREST_KEPT {
        let mut all: Vec<Cand> = all.into_iter().collect();
        all.sort();
        all.truncate(k);
        return all;
    }
    let mut best: Vec<Cand> = Vec::with_capacity(k + 1);
    for c in all {
        if best.len() == k {
            // No nearer than the last kept: a stable sort leaves it after.
            if c.dist.total_cmp(&best[k - 1].dist) != Ordering::Less {
                continue;
            }
            best.pop();
        }
        let at = best.partition_point(|b| b.dist.total_cmp(&c.dist) != Ordering::Greater);
        best.insert(at, c);
    }
    best
}

/// The beam the upper layers are walked with, by a search and by a node
/// joining the graph, where one node at a time was the greedy descent. Over
/// a million 128-dim vectors in 64 clusters the greedy descent left 20 of
/// 1 000 queries in another cluster than their own, and 12 found none of
/// their ten at any beam below, 97.4% recall from a beam of 200 up to 800:
/// the level-0 walk does not cross between clusters it has no link across.
/// A beam of 4 left none there, and took recall at beams of 40, 100 and 200
/// from 89.9, 96.4 and 97.4% to 92.5, 99.1 and 99.8% -- pgvector's, the
/// same settings over two builds, 93.0 to 93.5, 97.4 to 97.8 and 97.8 to
/// 98.2% -- for a build 3 to 7% longer and a search as fast. Only in the search, it gave 98.2% at 100; only in the
/// build, a graph the greedy search lost 32 queries in.
const UPPER_BEAM: usize = 4;

/// Min-heap that keeps the nearest on top (by hand instead of Reverse).
#[derive(Copy, Clone, PartialEq)]
struct MinCand(Cand);
impl Eq for MinCand {}
impl Ord for MinCand {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.cmp(&self.0)
    }
}
impl PartialOrd for MinCand {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Dependency-free, deterministic PRNG (xorshift64*). For HNSW level choice.
struct Rng(u64);
impl Rng {
    fn next_f32(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        ((v >> 40) as f32) / ((1u32 << 24) as f32)
    }
}

/// Version of the serialised graph format. 3 added the arena's precision
/// and a tombstone's own vector; a record of an older version is rejected
/// and the graph rebuilt once. 4 adds the quantization, and only a quantized
/// index writes it: every other graph stays 3, so a file written before
/// quantization existed is not rebuilt for it. 5 marks the nodes not linked
/// yet ([`VectorIndex::defer_batch`]), and is written only while there are
/// some: a server that shut down before linking them keeps them waiting
/// rather than rebuilding, and a build before it rebuilds once. 6 is a
/// graph a server kept in its file's tail (`Database::save_graphs`), in
/// version 5's layout: a binary before it restored a record there against
/// the documents as the whole file left them, and a vector rewritten after
/// the record kept the links of the one before it. It does not know 6, and
/// rebuilds.
///
/// 7 and 8 are 3 to 6 laid out flat -- the image's and the tail's -- with
/// the quantization and a node's waiting flag always there: arrays of the
/// nodes' documents, flags, levels and level-0 lengths, then every link a
/// `u32`, where the others took a varint delta a link. Decoding them one at
/// a time was a quarter of opening a 100 000 x 128 graph; laid out flat they
/// are copied. A binary before 7 does not know it, and rebuilds.
///
/// 9 and 10 are 7 and 8 with the documents that hold another's vector
/// after the tombstones (`VectorIndex::aliases`): their number, then each
/// one's node and document -- and, written natively, the nodes' hash
/// halves after those ([`HALVES`]).
const GRAPH_VERSION: u8 = 3;
const GRAPH_VERSION_QUANT: u8 = 4;
const GRAPH_VERSION_UNLINKED: u8 = 5;
const GRAPH_VERSION_KEPT: u8 = 6;
const GRAPH_VERSION_FLAT: u8 = 7;
const GRAPH_VERSION_FLAT_KEPT: u8 = 8;
const GRAPH_VERSION_ALIASED: u8 = 9;
const GRAPH_VERSION_ALIASED_KEPT: u8 = 10;

/// What a native build writes after a version 9 or 10 record's aliases:
/// this byte, then the top half of each node's hash in [`Same`], four bytes
/// a node in node order and a tombstone's 0, so that the first vector
/// placed after an open makes the table from them rather than read and
/// hash every vector in the arena again -- 33-169 ms over 1.1 million of
/// 128 dimensions. The reader before them stops at the aliases, so the
/// record needs no version of its own: the browser, whose hash is another,
/// reads it without them, as a binary from before them does. Hashed as the
/// restore fills the arena instead, the vector in cache, the open of
/// 100 000 x 128 took 0.45 ms longer (3.8%) and of 100 000 x 768 2 ms
/// (6.7%), and four vectors' chains side by side hashed only 1.65 times as
/// fast; the 4 bytes a node are 0.68% of a 128-dim file, 0.13% of a 768-dim
/// one.
#[cfg(not(target_family = "wasm"))]
const HALVES: u8 = 1;

/// The live nodes, spread over the arena, whose halves a restore checks
/// against their vectors before it keeps a record's.
#[cfg(not(target_family = "wasm"))]
const HALVES_CHECKED: usize = 64;

/// Whether a graph can hold nodes not linked yet: a server's open leaves
/// them for a thread beside its queries (`fs::open_serving`). The browser
/// has no such thread, and the paths were 1.1 KB brotli of its module: a
/// graph written with nodes waiting is rebuilt there, as any it cannot
/// read is.
pub(crate) const UNLINKED: bool = cfg!(not(target_arch = "wasm32"));

/// Upper bound on a single batch during parallel construction. Nodes inside a
/// batch cannot see each other, so large batches lower recall; this limit is
/// the ceiling of the "1/16 of the graph size" rule.
const MAX_BATCH: usize = 512;

// -------------------------------------------------------------- HNSW

/// Buffers reused during a search.
///
/// Without them every `search_layer` call allocated a `HashSet` and two
/// heaps; 100k inserts x ~3000 visits = hundreds of millions of hash
/// operations. An epoch-stamped array does the same with one `u32` compare.
struct Scratch {
    /// node -> the epoch it was last visited in
    visited: Vec<u32>,
    epoch: u32,
    candidates: BinaryHeap<MinCand>,
    results: BinaryHeap<Cand>,
    /// A node's neighbours not visited yet, and their distances.
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    fresh: Vec<u32>,
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    dists: Vec<f32>,
}

impl Scratch {
    fn new() -> Scratch {
        Scratch {
            visited: Vec::new(),
            epoch: 0,
            candidates: BinaryHeap::new(),
            results: BinaryHeap::new(),
            #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
            fresh: Vec::new(),
            #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
            dists: Vec::new(),
        }
    }

    fn begin(&mut self, nodes: usize) {
        if self.visited.len() < nodes {
            self.visited.resize(nodes, 0);
        }
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            // Overflow: reset the stamps.
            self.visited.iter_mut().for_each(|v| *v = 0);
            self.epoch = 1;
        }
        self.candidates.clear();
        self.results.clear();
    }

    /// A node the walk measured: kept while the beam has room or while it
    /// is nearer than the worst the beam holds.
    #[inline]
    fn offer(&mut self, node: u32, dist: f32, ef: usize) {
        let worst = self.results.peek().map(|c| c.dist).unwrap_or(f32::MAX);
        if self.results.len() < ef || dist < worst {
            let c = Cand { dist, node };
            self.candidates.push(MinCand(c));
            self.results.push(c);
            if self.results.len() > ef {
                self.results.pop();
            }
        }
    }

    #[inline]
    fn see(&mut self, node: u32) -> bool {
        let slot = &mut self.visited[node as usize];
        if *slot == self.epoch {
            false
        } else {
            *slot = self.epoch;
            true
        }
    }
}

/// What a pruning works in, kept from one to the next: the list and the
/// node joining it, their distances, and the candidates they make.
#[derive(Default)]
struct Pruning {
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    nodes: Vec<u32>,
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    dists: Vec<f32>,
    cands: Vec<Cand>,
}

/// Read-only view of the graph.
///
/// Why a separate type: `VectorIndex` carries `RefCell` buffers and is
/// therefore not `Sync`. The read algorithms (descent, beam search, neighbour
/// selection) perform no interior modification; moving them onto a view
/// without interior mutability lets the single-threaded path and the parallel
/// build share the same code. Buffers are handed in from outside as
/// `&mut Scratch` -- every thread carries its own.
#[derive(Clone, Copy)]
pub(crate) struct GraphView<'a> {
    data: &'a Arena,
    l0: &'a [u32],
    l0_len: &'a [u16],
    upper: &'a [Vec<Vec<u32>>],
    deleted: &'a [bool],
    m0: usize,
    dim: usize,
    metric: Metric,
    nodes: usize,
    entry: Option<u32>,
    max_level: usize,
}

impl<'a> GraphView<'a> {
    #[inline]
    fn vec_at(&self, node: u32) -> Cow<'a, [f32]> {
        self.data.vec_at(node, self.dim)
    }

    #[inline]
    fn dist_to(&self, q: &[f32], node: u32) -> f32 {
        self.data.dist_to(self.metric, q, node, self.dim)
    }

    #[inline]
    fn dist_nodes(&self, a: u32, b: u32) -> f32 {
        self.data.dist_nodes(self.metric, a, b, self.dim)
    }

    #[inline]
    fn is_deleted(&self, node: u32) -> bool {
        self.deleted.get(node as usize).copied().unwrap_or(false)
    }

    #[inline]
    fn neighbors(&self, node: u32, level: usize) -> &'a [u32] {
        if level == 0 {
            let base = node as usize * self.m0;
            let n = self.l0_len[node as usize] as usize;
            &self.l0[base..base + n]
        } else {
            match self.upper.get(node as usize).and_then(|u| u.get(level - 1)) {
                Some(v) => v,
                None => &[],
            }
        }
    }

    /// The upper layers walked with a beam of [`UPPER_BEAM`]: the nodes to
    /// start `to_level` from, nearest first.
    fn descend_beam(
        &self,
        sc: &mut Scratch,
        q: &[f32],
        from: u32,
        from_level: usize,
        to_level: usize,
    ) -> Vec<u32> {
        let mut ep = vec![from];
        for l in (to_level + 1..=from_level).rev() {
            ep = self
                .search_layer(sc, q, &ep, UPPER_BEAM, l)
                .into_iter()
                .map(|c| c.node)
                .collect();
        }
        ep
    }

    /// ef-wide beam search on one layer. The result ascends by distance.
    fn search_layer(
        &self,
        sc: &mut Scratch,
        q: &[f32],
        entries: &[u32],
        ef: usize,
        level: usize,
    ) -> Vec<Cand> {
        sc.begin(self.nodes);

        for &e in entries {
            if !sc.see(e) {
                continue;
            }
            let c = Cand {
                dist: self.dist_to(q, e),
                node: e,
            };
            sc.candidates.push(MinCand(c));
            sc.results.push(c);
        }
        while sc.results.len() > ef {
            sc.results.pop();
        }

        while let Some(MinCand(cur)) = sc.candidates.pop() {
            let worst = sc.results.peek().map(|c| c.dist).unwrap_or(f32::MAX);
            if cur.dist > worst && sc.results.len() >= ef {
                break;
            }
            // A walk reads lists and vectors scattered over the graph, a
            // cache miss each, and waits on them one after another: the
            // next candidate's list is asked for while this one's are
            // measured, and the fresh neighbours' vectors all at once
            // before any is. 100 000 x 128 builds in 3.58 to 3.80 s against
            // 3.96 to 4.25, and a `near` answers in 0.08 ms against 0.10.
            #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
            if let (0, Some(MinCand(next))) = (level, sc.candidates.peek()) {
                let n = next.node as usize;
                prefetch(self.l0.as_ptr().wrapping_add(n * self.m0).cast());
                prefetch(self.l0_len.as_ptr().wrapping_add(n).cast());
            }
            #[cfg(not(all(target_arch = "aarch64", target_feature = "neon")))]
            for &nb in self.neighbors(cur.node, level) {
                if sc.see(nb) {
                    let d = self.dist_to(q, nb);
                    sc.offer(nb, d, ef);
                }
            }
            // The neighbours not visited yet are measured together, four
            // vectors at a time, then taken in the order they come: the
            // distances do not depend on the heaps, so the walk is the one
            // a distance at a time made.
            #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
            {
                let mut fresh = std::mem::take(&mut sc.fresh);
                let mut dists = std::mem::take(&mut sc.dists);
                fresh.clear();
                for &nb in self.neighbors(cur.node, level) {
                    if sc.see(nb) {
                        fresh.push(nb);
                    }
                }
                if let Arena::F32(d) = &self.data {
                    for &nb in &fresh {
                        let v = &d[nb as usize * self.dim..][..self.dim];
                        // 32 floats, the M series' 128-byte line.
                        for line in v.chunks(32) {
                            prefetch(line.as_ptr().cast());
                        }
                    }
                }
                self.data
                    .dists_to::<false>(self.metric, q, &fresh, self.dim, &mut dists);
                for (&nb, &d) in fresh.iter().zip(&dists) {
                    sc.offer(nb, d, ef);
                }
                (sc.fresh, sc.dists) = (fresh, dists);
            }
        }

        let mut out: Vec<Cand> = sc.results.drain().collect();
        out.sort();
        out
    }

    /// Whether a node of `out` lies nearer `c` than the one `c` was found
    /// from, over an f32 arena. On aarch64 four are measured at once, each
    /// distance the one `dist_nodes` gives, so the same answer: 100 000 x
    /// 768 builds 5 to 9% sooner, where the distances are most of the work.
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    fn any_nearer(&self, c: &Cand, out: &[u32]) -> bool {
        let Arena::F32(d) = &self.data else {
            return out.iter().any(|&r| self.dist_nodes(c.node, r) < c.dist);
        };
        let dim = self.dim;
        let at = |n: u32| &d[n as usize * dim..][..dim];
        let q = at(c.node);
        let (four, rest) = out.as_chunks::<4>();
        four.iter().any(|n| {
            distances4(self.metric, q, n.map(at))
                .iter()
                .any(|&x| x < c.dist)
        }) || rest.iter().any(|&r| self.dist_nodes(c.node, r) < c.dist)
    }

    #[cfg(not(all(target_arch = "aarch64", target_feature = "neon")))]
    fn any_nearer(&self, c: &Cand, out: &[u32]) -> bool {
        out.iter().any(|&r| self.dist_nodes(c.node, r) < c.dist)
    }

    /// Diversity heuristic (Malkov & Yashunin, Alg. 4).
    ///
    /// Taking only the M nearest candidates traps the graph in local clusters
    /// and lowers recall. A candidate is accepted when it is no closer to any
    /// already-selected neighbour than it is to the query; that spreads the
    /// links in different directions.
    ///
    /// `owner` is the node whose list is being built; no self-link is produced.
    fn select_heuristic(&self, cands: &[Cand], m: usize, owner: u32) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::with_capacity(m);
        // Most of the build time goes into this inner loop: every candidate is
        // compared against all of the selected ones (~ef × m distances). In the
        // f16 arena `dist_nodes` widens two vectors at once; since the selected
        // ones are fixed they are widened once and kept, and the candidate is
        // widened once as well. In the f32 arena a copy would be pointless --
        // the old path is kept exactly as it was.
        let decode = !matches!(self.data, Arena::F32(_));
        let mut picked: Vec<f32> = Vec::new(); // same order as `out`, dim-strided
        let mut offsets: Vec<f32> = Vec::new(); // `Arena::offset`, the same order
        let mut cbuf: Vec<f32> = Vec::new();
        for c in cands {
            if out.len() >= m {
                break;
            }
            if c.node == owner || self.is_deleted(c.node) {
                continue;
            }
            let mut diverse = true;
            if decode {
                self.data.read_into(c.node, self.dim, &mut cbuf);
                let off = self.data.offset(c.node);
                for i in 0..out.len() {
                    let r = &picked[i * self.dim..(i + 1) * self.dim];
                    if distance(self.metric, &cbuf, r) - off - offsets[i] < c.dist {
                        diverse = false;
                        break;
                    }
                }
            } else {
                diverse = !self.any_nearer(c, &out);
            }
            if diverse {
                out.push(c.node);
                if decode {
                    picked.extend_from_slice(&cbuf);
                    offsets.push(self.data.offset(c.node));
                }
            }
        }
        // If the heuristic did not leave enough elements, fill up with the
        // nearest: leaving the graph disconnected is worse than losing recall.
        if out.len() < m {
            for c in cands {
                if out.len() >= m {
                    break;
                }
                if c.node != owner && !self.is_deleted(c.node) && !out.contains(&c.node) {
                    out.push(c.node);
                }
            }
        }
        // Nothing live among the candidates -- every node near this one was
        // deleted, as when a document's chunks are written again, or every
        // node at all -- and a node linked to nothing is one no search
        // reaches: `near` over a collection of two whose rows were deleted
        // and written again answered nothing. A tombstone still routes
        // searches, so the node is linked through the nearest ones, and
        // gets live neighbours as the nodes written after it link back.
        if out.is_empty() {
            for c in cands {
                if out.len() >= m {
                    break;
                }
                if c.node != owner {
                    out.push(c.node);
                }
            }
        }
        out
    }

    /// `nb`'s neighbours at a level once `node` joins `current`, a full
    /// list: chosen again from both by the diversity heuristic. It reads
    /// only `nb`'s list and the vectors, which is what lets a batch prune
    /// the lists of different nodes on different threads.
    fn pruned(
        &self,
        nb: u32,
        current: &[u32],
        node: u32,
        max_deg: usize,
        buf: &mut Pruning,
    ) -> Vec<u32> {
        // `nb` is fixed across all the comparisons: widen it once.
        // Widened, a code leaves out its own offset.
        let (nbv, off) = (self.vec_at(nb), self.data.offset(nb));
        buf.cands.clear();
        #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
        {
            buf.nodes.clear();
            buf.nodes.extend_from_slice(current);
            buf.nodes.push(node);
            self.data
                .dists_to::<false>(self.metric, &nbv, &buf.nodes, self.dim, &mut buf.dists);
            let measured = buf.nodes.iter().zip(&buf.dists);
            buf.cands.extend(measured.map(|(&x, &d)| Cand {
                dist: d - off,
                node: x,
            }));
        }
        #[cfg(not(all(target_arch = "aarch64", target_feature = "neon")))]
        {
            for &x in current {
                buf.cands.push(Cand {
                    dist: self.dist_to(&nbv, x) - off,
                    node: x,
                });
            }
            buf.cands.push(Cand {
                dist: self.dist_to(&nbv, node) - off,
                node,
            });
        }
        drop(nbv);
        buf.cands.sort();
        self.select_heuristic(&buf.cands, max_deg, nb)
    }

    /// Computes a node's neighbour candidates across every level.
    /// It only reads, so it can be run in parallel. `query` is the vector
    /// the node was written with, over an arena of codes (see
    /// [`VectorIndex::build_query`]).
    fn candidates_for(
        &self,
        sc: &mut Scratch,
        node: u32,
        query: Option<&[f32]>,
        level: usize,
        ef_construction: usize,
        m: usize,
    ) -> Vec<(usize, Vec<u32>)> {
        let Some(entry) = self.entry else {
            return Vec::new();
        };
        let v = match query {
            Some(q) => Cow::Borrowed(q),
            None => self.vec_at(node),
        };
        let start = if self.max_level > level {
            self.descend_beam(sc, &v, entry, self.max_level, level)
        } else {
            vec![entry]
        };
        let mut out = Vec::new();
        let mut ep = start.clone();
        for l in (0..=level.min(self.max_level)).rev() {
            let cands = self.search_layer(sc, &v, &ep, ef_construction, l);
            let selected = self.select_heuristic(&cands, m, node);
            ep = if selected.is_empty() {
                start.clone()
            } else {
                selected.clone()
            };
            out.push((l, selected));
        }
        out
    }
}

/// What of a graph has reached the file: the change count it had and the
/// file's length right after its record, and what a node took there. Set by
/// the load that restored it, a rewrite, and `Database::save_graphs` --
/// which writes a graph under the read lock, hence the atomics.
#[derive(Default)]
pub struct Persisted {
    pub changes: AtomicU64,
    pub at: AtomicU64,
    pub node_bytes: AtomicU64,
}

#[cfg(feature = "vector")]
pub struct VectorIndex {
    pub dim: usize,
    pub spec: VectorIndexSpec,
    /// Contiguous vector arena: node i -> data[i*dim .. (i+1)*dim].
    /// Precision comes from the field type (`vector<N, f16>` -> half size),
    /// or codes from `quant`.
    data: Arena,
    /// The field's precision, which a quantized arena does not show.
    prec: VecPrec,
    doc_ids: Vec<DocId>,
    /// Each document's node, one up: 0 is none.
    by_doc: DocMap,

    // --- neighbour lists ------------------------------------------------
    //
    // Level 0 sees ~95% of all the work, so it lives in a single flat array
    // with a fixed stride: `l0[node * m0 .. node * m0 + l0_len[node]]`.
    // In the earlier `Vec<Vec<Vec<u32>>>` layout every neighbour access needed
    // two dependent pointer hops; a flat array needs one index computation.
    /// Level-0 contiguous neighbour arena, stride = `m0`.
    l0: Vec<u32>,
    l0_len: Vec<u16>,
    /// Maximum level-0 degree per node (= 2 * m).
    m0: usize,
    /// Level >= 1 is sparse (~6% of the nodes). `upper[node][level - 1]`.
    /// Nodes that stay on level 0 hold an empty `Vec`, so no allocation.
    upper: Vec<Vec<Vec<u32>>>,

    /// Tombstone flags. Previously a `HashSet<u32>` that got hashed inside the
    /// hot loops; one byte per node is cheaper.
    deleted: Vec<bool>,
    deleted_count: usize,

    entry: Option<u32>,
    max_level: usize,
    rng: Rng,
    level_mult: f32,
    /// Buffer reused while pruning (breaks the allocation cycle).
    prune_buf: Pruning,
    /// Nodes in the arena that no link reaches yet, in the order they came:
    /// a search measures each of them against the query, and
    /// [`VectorIndex::link_pending`] takes them into the graph. A tombstone
    /// among them is dropped when its turn comes.
    pending: Vec<u32>,
    /// Changes to the graph since it was made or restored: nodes added,
    /// retired and linked. What a server weighs against a graph record's
    /// size before it appends one (`Database::save_graphs`): as many as
    /// these would be linked again after a crash.
    changes: u64,
    persisted: Persisted,
    /// Documents holding the vector of another document's node, as
    /// `(node, doc)` in order: a vector written again and again is one node
    /// ([`VectorIndex::place`]). A node each, a vector written hundreds of
    /// times filled its neighbours' lists with copies of itself, which a
    /// walk could not leave: over 20 000 x 128 with 30% of the rows copies
    /// of 20 vectors, 1 812 of 6 019 copies were found by no search, and
    /// recall@10 of other queries fell from 0.82 to 0.67. A sorted `Vec`,
    /// not a map, which would be another copy of hashbrown in the browser.
    aliases: Vec<(u32, DocId)>,
    /// The live nodes by their vector (`Same`).
    same: Same,
    /// The vector being placed, as the arena would store it: kept from one
    /// insert to the next.
    stored_buf: Vec<u8>,
}

/// The nodes by their vector as the arena stores it, for a vector written
/// again to find its node: open addressing over `node + 1` beside the top
/// half of the vector's hash, which is tested before the vectors are: with
/// the node alone, every slot a probe passed read a vector, and a build of
/// 10 000 x 128 in the browser took 5% longer. 8 bytes a slot, 16 to 32 a
/// node -- 3 to 6% of a 128-dim f32 arena, under 1% of a 768-dim one. Made
/// the first time a vector is placed, not at an open, which would hold it
/// for a database that may only be read: natively from the halves of the
/// hashes the graph record carried ([`HALVES`]) where it did, from the
/// arena otherwise.
///
/// In the browser one table, made again from the arena twice the size once
/// it is half full, and a node that became a tombstone stays in it, passed
/// over, until it grows. Natively that reading was the write that found it
/// full: every vector hashed again, a doubling at a time -- 118-132 and
/// 242-281 ms at 2^19 and 2^20 nodes of 128 dimensions, 376 ms at 2^18 of
/// 768 and 1.45 s at 2^19, every write and read waiting for it. So natively
/// a slot's place is its own half of the hash, and a table grows from its
/// slots alone, leaving its tombstones behind; past [`SAME_SPLIT`] slots it
/// is [`SAME_SHARDS`] tables, a slot's picked by the top byte of its half,
/// each growing on its own: no put at a doubling past 10 ms over 1.1
/// million of 128 dimensions or 300 000 of 768. The place taken from the
/// half leaves fewer of its bits to tell two vectors apart -- another
/// vector's slot at the same place passes the test once in 2^(24 - k) in a
/// shard of 2^k slots -- a vector compared for nothing about once in 4 000
/// puts at a million nodes.
#[cfg(target_family = "wasm")]
#[derive(Default)]
struct Same {
    slots: Vec<u64>,
    used: usize,
    built: bool,
}

#[cfg(not(target_family = "wasm"))]
#[derive(Default)]
struct Same {
    /// Every slot, until the table is split; empty after.
    one: SameTable,
    /// Empty until the table is split; then every slot is in one of them.
    shards: Vec<SameTable>,
    built: bool,
    /// The top half of each node's hash as the restored graph record
    /// carried them, until the first vector placed makes the table from
    /// them -- 4 bytes a node, which a database that is only read keeps;
    /// empty after, and where no record carried them.
    halves: Vec<u32>,
}

/// One open-addressed table of [`Same`]: a slot is the top half of a
/// vector's hash over `node + 1`, 0 none, and its place the low bits of
/// that half -- so the table is made again from its slots.
#[cfg(not(target_family = "wasm"))]
#[derive(Default)]
struct SameTable {
    slots: Vec<u64>,
    used: usize,
}

/// Slots past which [`Same`]'s one table is split rather than grown: its
/// 32 768 nodes moved into the shards in 0.72 to 0.80 ms, where the table
/// grew at half the size in 0.3 and a shard at 2 million nodes in 0.15.
/// Below it an index holds one table, as small as before -- a tenant's, a
/// page's.
#[cfg(not(target_family = "wasm"))]
const SAME_SPLIT: usize = 1 << 16;

/// The tables [`Same`] is split into, by the top byte of a slot's half.
#[cfg(not(target_family = "wasm"))]
const SAME_SHARDS: usize = 256;

/// Nodes a thread hashes at a time when [`Same`] is made from the arena.
#[cfg(not(target_family = "wasm"))]
const SAME_SHARE: usize = 4096;

#[cfg(not(target_family = "wasm"))]
impl SameTable {
    /// A table with room for `n` slots, two to four places each.
    fn sized(n: usize) -> SameTable {
        SameTable {
            slots: vec![0; (n.max(4) * 2).next_power_of_two()],
            used: 0,
        }
    }

    fn full(&self) -> bool {
        (self.used + 1) * 2 > self.slots.len()
    }

    /// `slot` into the first free place from its own.
    fn insert(&mut self, slot: u64) {
        let mask = self.slots.len() - 1;
        let mut i = (slot >> 32) as usize & mask;
        while self.slots[i] != 0 {
            i = (i + 1) & mask;
        }
        self.slots[i] = slot;
        self.used += 1;
    }

    /// The table made again from its slots, a tombstone's left out, with
    /// room for twice the rest.
    #[cold]
    fn regrow(&mut self, dead: &[bool]) {
        let live = |s: &&u64| **s != 0 && !dead[(**s as u32 - 1) as usize];
        let mut next = SameTable::sized(self.slots.iter().filter(live).count() + 1);
        for &s in self.slots.iter().filter(live) {
            next.insert(s);
        }
        *self = next;
    }
}

#[cfg(not(target_family = "wasm"))]
impl Same {
    /// The table a hash's slot is in, and where its probe starts.
    #[inline]
    fn home(&self, hash: u64) -> (&[u64], usize) {
        let half = (hash >> 32) as usize;
        let table = match self.shards.is_empty() {
            true => &self.one,
            false => &self.shards[half >> 24],
        };
        (&table.slots, half & (table.slots.len() - 1))
    }

    /// `node`'s slot put in, its table grown -- or the one split -- first
    /// where it is full.
    fn put(&mut self, node: u32, hash: u64, dead: &[bool]) {
        let slot = (hash >> 32) << 32 | (node as u64 + 1);
        if self.shards.is_empty() {
            if !self.one.full() {
                return self.one.insert(slot);
            }
            if self.one.slots.len() < SAME_SPLIT {
                self.one.regrow(dead);
                return self.one.insert(slot);
            }
            let slots = std::mem::take(&mut self.one).slots;
            self.shards = Same::spread(slots.iter().copied().filter(|&s| s != 0), dead);
        }
        let table = &mut self.shards[(slot >> 56) as usize];
        if table.full() {
            table.regrow(dead);
        }
        table.insert(slot);
    }

    /// `slots` in [`SAME_SHARDS`] tables, each with room for twice its
    /// share, a tombstone's left out.
    #[cold]
    fn spread(slots: impl Iterator<Item = u64> + Clone, dead: &[bool]) -> Vec<SameTable> {
        let slots = slots.filter(|&s| !dead[(s as u32 - 1) as usize]);
        let mut counts = vec![0usize; SAME_SHARDS];
        for s in slots.clone() {
            counts[(s >> 56) as usize] += 1;
        }
        // Gathered by shard first, then put into each shard's table in
        // turn, past 2^18 slots a shard to whichever core is free: put in
        // where they fell, every slot was a miss in tables larger than the
        // cache, and 1.1 million took 10.5-10.9 ms against 4.0-4.4, the
        // first put after an open waiting for them.
        let starts: Vec<usize> = counts
            .iter()
            .scan(0, |sum, &n| {
                *sum += n;
                Some(*sum - n)
            })
            .collect();
        let mut at = starts.clone();
        let total: usize = counts.iter().sum();
        let mut gathered = vec![0u64; total];
        for s in slots {
            let shard = (s >> 56) as usize;
            gathered[at[shard]] = s;
            at[shard] += 1;
        }
        let table = |i: usize| {
            let mut t = SameTable::sized(counts[i] + 1);
            for &s in &gathered[starts[i]..starts[i] + counts[i]] {
                t.insert(s);
            }
            t
        };
        let threads = match total > 1 << 18 {
            true => Same::threads(),
            false => 1,
        };
        if threads < 2 {
            return (0..SAME_SHARDS).map(table).collect();
        }
        let mut made = spread(SAME_SHARDS, &mut vec![(); threads], |_, i| (i, table(i)));
        made.sort_unstable_by_key(|&(i, _)| i);
        made.into_iter().map(|(_, t)| t).collect()
    }

    /// The cores to spread over, as `VectorIndex::threads` counts them: a
    /// build without the graph has none of its own.
    fn threads() -> usize {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    }

    /// Made from the top half of every live node's hash, in node order.
    fn made(halves: &[u32], dead: &[bool]) -> Same {
        let slots = halves
            .iter()
            .enumerate()
            .filter(|&(node, _)| !dead[node])
            .map(|(node, &h)| (h as u64) << 32 | (node as u64 + 1));
        let live = dead.iter().filter(|&&d| !d).count();
        let mut same = Same {
            built: true,
            ..Same::default()
        };
        if (live + 1) * 2 <= SAME_SPLIT {
            same.one = SameTable::sized(live + 1);
            slots.for_each(|s| same.one.insert(s));
        } else {
            same.shards = Same::spread(slots, dead);
        }
        same
    }

    /// Each of the first `n` nodes' hash half, read off the slots: 0 for a
    /// node with none. A node has one slot at most, so the shards are read
    /// on every core past [`SAME_SPLIT`] nodes, each half stored where its
    /// node is: in turn, 1.1 million nodes' took 8.3 ms against 2.3, under
    /// the read lock a server keeps its graphs under.
    fn halves_by_node(&self, n: usize) -> Vec<u32> {
        use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
        let halves: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(0)).collect();
        let read = |table: &SameTable| {
            for &s in table.slots.iter().filter(|&&s| s != 0) {
                if let Some(h) = halves.get((s as u32 - 1) as usize) {
                    h.store((s >> 32) as u32, Relaxed);
                }
            }
        };
        read(&self.one);
        match n > SAME_SPLIT {
            true => {
                let threads = Same::threads().min(self.shards.len());
                spread(self.shards.len(), &mut vec![(); threads], |_, i| {
                    read(&self.shards[i])
                });
            }
            false => self.shards.iter().for_each(read),
        }
        halves.into_iter().map(AtomicU32::into_inner).collect()
    }

    fn bytes(&self) -> usize {
        let shards = self
            .shards
            .iter()
            .map(|t| t.slots.capacity() * 8)
            .sum::<usize>();
        self.one.slots.capacity() * 8
            + shards
            + self.shards.capacity() * 32
            + self.halves.capacity() * 4
    }
}

#[cfg(target_family = "wasm")]
impl Same {
    #[inline]
    fn bytes(&self) -> usize {
        self.slots.capacity() * 8
    }
}

/// A hash of a vector as the arena stores it (`Arena::write_stored`), four
/// bytes at a time -- so an f32 arena's is its floats' bits, taken with no
/// bytes written ([`hash_words`]).
#[cfg(target_family = "wasm")]
fn hash_stored(bytes: &[u8]) -> u64 {
    let (words, rest) = bytes.as_chunks::<4>();
    let words = words.iter().map(|w| u32::from_le_bytes(*w));
    hash_words(words.chain(rest.iter().map(|&b| b as u32)))
}

/// A multiply and a rotate a word.
#[cfg(any(target_family = "wasm", not(feature = "vector")))]
fn hash_words(words: impl Iterator<Item = u32>) -> u64 {
    let mut h: u64 = 0x243F_6A88_85A3_08D3;
    for w in words {
        h = (h ^ w as u64)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .rotate_left(29);
    }
    h ^ (h >> 32)
}

/// [`hash_words`] natively: two words a step into each of four chains,
/// folded together and finished, so that every bit of the top half -- a
/// slot's place and its shard -- hears every word. One chain waited on its
/// multiply a word: a 768-dim vector in cache took 1.18 us against 0.17, a
/// 128-dim one 0.16 against 0.02, which leaves [`Same`]'s making from the
/// arena to the memory's speed.
#[cfg(not(target_family = "wasm"))]
#[inline]
fn hash_lanes<T: Copy>(words: &[T], bits: impl Fn(T) -> u32, rest: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let step = |h: u64, w: u64| (h ^ w).wrapping_mul(K).rotate_left(29);
    let mut h: [u64; 4] = [
        0x243F_6A88_85A3_08D3,
        0x1319_8A2E_0370_7344,
        0xA409_3822_299F_31D0,
        0x082E_FA98_EC4E_6C89,
    ];
    let (chunks, tail) = words.as_chunks::<8>();
    for c in chunks {
        for (l, h) in h.iter_mut().enumerate() {
            *h = step(
                *h,
                bits(c[2 * l]) as u64 | (bits(c[2 * l + 1]) as u64) << 32,
            );
        }
    }
    let mut x = (words.len() + rest.len()) as u64;
    for l in h {
        x = step(x, l);
    }
    for &w in tail {
        x = step(x, bits(w) as u64);
    }
    for &b in rest {
        x = step(x, b as u64);
    }
    // MurmurHash3's finish.
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    x = x.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    x ^ (x >> 33)
}

#[cfg(not(target_family = "wasm"))]
#[inline]
fn hash_stored(bytes: &[u8]) -> u64 {
    let (words, rest) = bytes.as_chunks::<4>();
    hash_lanes(words, u32::from_le_bytes, rest)
}

/// A float arena's hash of `raw` as `Arena::push` would store it, each
/// component times `inv` where that is not 1.
#[cfg(not(target_family = "wasm"))]
#[inline]
fn hash_scaled(raw: &[f32], inv: f32) -> u64 {
    match inv != 1.0 {
        true => hash_lanes(raw, |x| (x * inv).to_bits(), &[]),
        false => hash_lanes(raw, f32::to_bits, &[]),
    }
}

thread_local! {
    /// Search buffers are kept per thread.
    ///
    /// It once sat in a `RefCell<Scratch>` field inside `VectorIndex`:
    /// `search` takes `&self` and needed interior mutability to borrow the
    /// buffer. The price was that the type was not `Sync` -- two threads
    /// could not search the same index at once (which pinned every read on
    /// the server side behind a single lock). Moving the buffer into
    /// thread-local storage gives both: an allocation-free hot loop *and*
    /// parallel search.
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::new());
}

#[cfg(feature = "vector")]
impl VectorIndex {
    pub fn new(dim: usize, spec: VectorIndexSpec) -> VectorIndex {
        VectorIndex::with_precision(dim, spec, VecPrec::F32)
    }

    pub fn with_precision(dim: usize, spec: VectorIndexSpec, prec: VecPrec) -> VectorIndex {
        let spec = spec.resolved();
        VectorIndex {
            dim,
            spec,
            data: Arena::new(prec, spec.quant),
            prec,
            doc_ids: Vec::new(),
            by_doc: DocMap::default(),
            l0: Vec::new(),
            l0_len: Vec::new(),
            m0: spec.m * 2,
            upper: Vec::new(),
            deleted: Vec::new(),
            deleted_count: 0,
            entry: None,
            max_level: 0,
            rng: Rng(0x9E37_79B9_7F4A_7C15),
            level_mult: 1.0 / (spec.m.max(2) as f32).ln(),
            prune_buf: Pruning::default(),
            pending: Vec::new(),
            changes: 0,
            persisted: Persisted::default(),
            aliases: Vec::new(),
            same: Same::default(),
            stored_buf: Vec::new(),
        }
    }

    /// Reserves room up front for a known capacity (bulk loading).
    pub fn reserve(&mut self, n: usize) {
        self.data.reserve(n, self.dim);
        self.doc_ids.reserve(n);
        self.l0.reserve(n * self.m0);
        self.l0_len.reserve(n);
        self.upper.reserve(n);
        self.deleted.reserve(n);
        self.by_doc.reserve(n);
    }

    /// Documents in the index: a node each, and the documents holding the
    /// vector of another's node.
    pub fn len(&self) -> usize {
        self.doc_ids.len() - self.deleted_count + self.aliases.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Tombstones: nodes of documents deleted or rewritten, which stay in the
    /// graph -- still routing a search through them -- until it is rebuilt.
    pub fn dead(&self) -> usize {
        self.deleted_count
    }

    #[inline]
    fn is_deleted(&self, node: u32) -> bool {
        self.deleted.get(node as usize).copied().unwrap_or(false)
    }

    #[inline]
    fn vec_at(&self, node: u32) -> Cow<'_, [f32]> {
        self.data.vec_at(node, self.dim)
    }

    /// One node measured: `measure` where it does not measure four at once.
    #[cfg(not(all(target_arch = "aarch64", target_feature = "neon")))]
    #[inline]
    fn dist_to(&self, query: &[f32], node: u32) -> f32 {
        self.data.dist_to(self.spec.metric, query, node, self.dim)
    }

    /// The `k` nearest of `nodes` to `q`, as [`nearest`] keeps them. Where
    /// there are enough, natively a share of [`MEASURE_SHARE`] at a time on
    /// every core, each share's nearest kept and all of those kept again in
    /// the shares' order: the rows a walk in turn keeps, ties in the same
    /// order, since a row among the nearest `k` of all is among its share's.
    /// An exact search is a read of every vector it measures, and on one
    /// core a filter's quarter of 1 000 000 x 128 took 20.8 ms.
    #[cfg(not(target_family = "wasm"))]
    fn nearest_of(&self, q: &[f32], nodes: &[u32], k: usize) -> Vec<Cand> {
        let shares = nodes.len().div_ceil(MEASURE_SHARE);
        let threads = Self::threads().min(shares);
        if threads < 2 || nodes.len() < MEASURE_APART {
            return nearest(self.measure(q, nodes.iter().copied()), k);
        }
        let mut states = vec![(); threads];
        let mut kept = spread(shares, &mut states, |_, s| {
            let end = ((s + 1) * MEASURE_SHARE).min(nodes.len());
            let share = &nodes[s * MEASURE_SHARE..end];
            (s, nearest(self.measure(q, share.iter().copied()), k))
        });
        kept.sort_unstable_by_key(|(s, _)| *s);
        nearest(kept.into_iter().flat_map(|(_, c)| c), k)
    }

    /// The `k` nearest of `nodes` to `q`: no threads in the browser.
    #[cfg(target_family = "wasm")]
    fn nearest_of(&self, q: &[f32], nodes: &[u32], k: usize) -> Vec<Cand> {
        nearest(self.measure(q, nodes.iter().copied()), k)
    }

    /// Each of `nodes` measured against `q`, four at a time on aarch64
    /// ([`distances4`]).
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    fn measure(&self, q: &[f32], nodes: impl Iterator<Item = u32>) -> Vec<Cand> {
        let nodes: Vec<u32> = nodes.collect();
        let mut dists = Vec::with_capacity(nodes.len());
        self.data
            .dists_to::<true>(self.spec.metric, q, &nodes, self.dim, &mut dists);
        let measured = nodes.into_iter().zip(dists);
        measured.map(|(node, dist)| Cand { dist, node }).collect()
    }

    /// Each of `nodes` measured against `q`.
    #[cfg(not(all(target_arch = "aarch64", target_feature = "neon")))]
    fn measure(&self, q: &[f32], nodes: impl Iterator<Item = u32>) -> Vec<Cand> {
        nodes
            .map(|node| Cand {
                dist: self.dist_to(q, node),
                node,
            })
            .collect()
    }

    /// Size of the arena in memory (bytes) -- for measurement and statistics.
    pub fn arena_bytes(&self) -> usize {
        self.data.bytes()
    }

    /// Size of the graph in memory: neighbour arrays, node maps and tombstone
    /// flags. Counted apart from the arena because it scales with `m` and not
    /// with the dimension -- with small vectors this is the dominant item
    /// (dim 4, m0 32: arena 16 bytes/node, graph 128).
    ///
    /// Level >= 1 neighbourhoods are counted only as the header of the outer
    /// array; walking their inner allocations would be O(nodes), and they are
    /// ~3% of l0 (~6% of the nodes reach an upper level).
    pub fn graph_bytes(&self) -> usize {
        use std::mem::size_of;
        let id = size_of::<DocId>();
        self.doc_ids.capacity() * id
            + self.by_doc.bytes()
            + self.l0.capacity() * 4
            + self.l0_len.capacity() * 2
            + self.deleted.capacity()
            + self.upper.capacity() * size_of::<Vec<Vec<u32>>>()
            + self.pending.capacity() * 4
            + self.aliases.capacity() * size_of::<(u32, DocId)>()
            + self.same.bytes()
    }

    /// Nodes that no link reaches yet ([`Self::defer_batch`]), tombstones
    /// among them included until their turn to be linked comes.
    pub fn unlinked(&self) -> usize {
        match UNLINKED {
            true => self.pending.len(),
            false => 0,
        }
    }

    pub fn precision(&self) -> VecPrec {
        self.prec
    }

    /// Whether the arena holds codes rather than vectors (`quant=`), so that
    /// its distances are estimates to be corrected from the documents.
    pub fn quantized(&self) -> bool {
        self.data.quantized()
    }

    /// The nearest `doc`'s own vector can lie, as a distance, given the
    /// `score` its code estimated against a query `reach` long; minus
    /// infinity where the code bounds nothing, and the vector has to be read
    /// to be known.
    ///
    /// An int8 code rounds each component to a step of its scale, so the
    /// estimate is off by the rounding left in every component, weighed by
    /// the query: a sum of terms each within half a step, which spreads as
    /// the step times the query over √12 -- 0.0003 of a cosine distance at
    /// 768 dimensions. A rounding error spread evenly is sub-Gaussian with
    /// its own variance, so the sum passes six of those less often than
    /// 1.5e-8 of the time. In the worst case, every rounding leaning the
    /// query's way, the step times the query's L1 norm is off by 0.0105:
    /// as wide as the tenth nearest's lead over the hundredth, and 98 of a
    /// beam of 100 still had to be read. A bit code keeps no step.
    pub fn floor(&self, doc: DocId, score: f32, reach: f32) -> f32 {
        let Arena::I8(_, scales) = &self.data else {
            return f32::NEG_INFINITY;
        };
        let Some(node) = self.node_of(doc) else {
            return f32::NEG_INFINITY;
        };
        // How far a unit of rounding moves the distance: the query, or
        // under L2 twice the difference, as long as the estimate's root.
        let (d, reach) = match self.spec.metric {
            Metric::Cosine => (1.0 - score, reach),
            Metric::Dot => (-score, reach),
            Metric::L2 => (score * score, 2.0 * score),
        };
        d - 6.0 * scales[node as usize] * reach / 12f32.sqrt()
    }

    /// The vector a node written with `raw` searches the graph for its
    /// neighbours with: `raw` itself, prepared, over an arena of codes --
    /// what the node's code widens back to is the signs alone under `bit` --
    /// and `None`, the arena's own vector, over vectors, whose graphs stay
    /// as they were built.
    fn build_query(&self, raw: &[f32]) -> Option<Vec<f32>> {
        (self.spec.quant != Quant::None).then(|| self.query_for(raw))
    }

    // --- neighbour access ------------------------------------------------

    #[inline]
    fn neighbors(&self, node: u32, level: usize) -> &[u32] {
        if level == 0 {
            let base = node as usize * self.m0;
            let n = self.l0_len[node as usize] as usize;
            &self.l0[base..base + n]
        } else {
            match self.upper.get(node as usize).and_then(|u| u.get(level - 1)) {
                Some(v) => v,
                None => &[],
            }
        }
    }

    #[inline]
    fn node_levels(&self, node: u32) -> usize {
        self.upper.get(node as usize).map(|u| u.len()).unwrap_or(0)
    }

    fn set_neighbors(&mut self, node: u32, level: usize, list: &[u32]) {
        if level == 0 {
            let base = node as usize * self.m0;
            let n = list.len().min(self.m0);
            self.l0[base..base + n].copy_from_slice(&list[..n]);
            self.l0_len[node as usize] = n as u16;
        } else if let Some(u) = self.upper.get_mut(node as usize) {
            if let Some(slot) = u.get_mut(level - 1) {
                slot.clear();
                slot.extend_from_slice(list);
            }
        }
    }

    /// Adds a neighbour. Returns `false` when the list is full (the caller prunes).
    fn push_neighbor(&mut self, node: u32, level: usize, nb: u32, max_deg: usize) -> bool {
        if level == 0 {
            let n = self.l0_len[node as usize] as usize;
            if n >= max_deg.min(self.m0) {
                return false;
            }
            self.l0[node as usize * self.m0 + n] = nb;
            self.l0_len[node as usize] = (n + 1) as u16;
            true
        } else {
            match self
                .upper
                .get_mut(node as usize)
                .and_then(|u| u.get_mut(level - 1))
            {
                Some(slot) => {
                    if slot.len() >= max_deg {
                        return false;
                    }
                    slot.push(nb);
                    true
                }
                None => true, // this node has no such level: skip silently
            }
        }
    }

    /// Read-only view of the graph.
    #[inline]
    pub(crate) fn view(&self) -> GraphView<'_> {
        GraphView {
            data: &self.data,
            l0: &self.l0,
            l0_len: &self.l0_len,
            upper: &self.upper,
            deleted: &self.deleted,
            m0: self.m0,
            dim: self.dim,
            metric: self.spec.metric,
            nodes: self.doc_ids.len(),
            entry: self.entry,
            max_level: self.max_level,
        }
    }

    /// [`GraphView::descend_beam`], with this thread's buffer.
    fn descend_beam(&self, q: &[f32], from: u32, from_level: usize, to_level: usize) -> Vec<u32> {
        let view = self.view();
        SCRATCH.with(|cell| {
            let mut sc = cell.borrow_mut();
            view.descend_beam(&mut sc, q, from, from_level, to_level)
        })
    }

    /// Beam search: borrows the buffer belonging to this thread.
    fn search_layer(&self, q: &[f32], entries: &[u32], ef: usize, level: usize) -> Vec<Cand> {
        let view = self.view();
        SCRATCH.with(|cell| {
            let mut sc = cell.borrow_mut();
            view.search_layer(&mut sc, q, entries, ef, level)
        })
    }

    fn select_heuristic(&self, cands: &[Cand], m: usize, owner: u32) -> Vec<u32> {
        self.view().select_heuristic(cands, m, owner)
    }

    /// Prepares the query vector for the metric (normalise for cosine).
    pub fn prepare_query(&self, q: &[f32]) -> Vec<f32> {
        match self.spec.metric {
            Metric::Cosine => normalized(q),
            _ => q.to_vec(),
        }
    }

    /// The query a search measures with: prepared, and over bit codes
    /// followed by what every code's estimate takes from it, once a query
    /// rather than once a node -- its dot product with each centre, then
    /// itself cut to [`QUERY_BITS`] bits a component for [`dot_planes`]:
    /// the step, the sum of the parts, and the bit planes a code word at a
    /// time. The step is the largest component over the largest part, and
    /// a part the component over the step, rounded.
    fn query_for(&self, q: &[f32]) -> Vec<f32> {
        let mut q = self.prepare_query(q);
        if let (Arena::Bit(b), true) = (&self.data, q.len() == self.dim) {
            let (dim, at) = (self.dim, b.centres.half.len());
            let words = dim.div_ceil(64);
            q.reserve(at + 2 + words * 2 * QUERY_BITS);
            for c in 0..at {
                q.push(dot(&q[..dim], b.centres.centre(c, dim)));
            }
            let top = q[..dim].iter().fold(0.0f32, |m, x| m.max(x.abs()));
            let step = if top > 0.0 {
                top / ((1 << (QUERY_BITS - 1)) - 1) as f32
            } else {
                1.0
            };
            let mut planes = vec![0u64; words * QUERY_BITS];
            let mut sum = 0i32;
            for (i, x) in q[..dim].iter().enumerate() {
                let part = (x / step).round() as i32;
                sum += part;
                for (j, plane) in planes[i / 64 * QUERY_BITS..][..QUERY_BITS]
                    .iter_mut()
                    .enumerate()
                {
                    *plane |= ((part >> j) as u64 & 1) << (i % 64);
                }
            }
            q.push(step);
            q.push(sum as f32);
            for plane in planes {
                q.push(f32::from_bits(plane as u32));
                q.push(f32::from_bits((plane >> 32) as u32));
            }
        }
        q
    }

    fn random_level(&mut self) -> usize {
        let r = self.rng.next_f32().max(f32::MIN_POSITIVE);
        (-r.ln() * self.level_mult) as usize
    }

    /// Opens storage for a new node and writes the vector into the arena.
    /// With cosine the normalisation is done in place on the arena -- there
    /// is no intermediate `Vec` allocation.
    fn alloc_node(&mut self, doc: DocId, raw: &[f32], level: usize) -> u32 {
        self.data.push(raw, self.spec.metric == Metric::Cosine);
        let nodes = self.doc_ids.len() + 1;
        if self.spec.quant == Quant::Bit && nodes >= BIT_TRAIN && !self.data.quantized() {
            self.data = self.data.coded(nodes, self.dim);
        }
        self.alloc_links(doc, level)
    }

    fn alloc_links(&mut self, doc: DocId, level: usize) -> u32 {
        self.changes += 1;
        let node = self.doc_ids.len() as u32;
        self.doc_ids.push(doc);
        self.deleted.push(false);
        let end = self.doc_ids.len() * self.m0;
        if self.l0.len() < end {
            self.l0.resize(end, 0);
        }
        self.l0_len.push(0);
        self.upper.push(if level == 0 {
            Vec::new()
        } else {
            vec![Vec::new(); level]
        });
        node
    }

    pub fn insert(&mut self, doc: DocId, raw: &[f32]) {
        if raw.len() != self.dim {
            return;
        }
        let Some((node, level)) = self.place(doc, raw) else {
            return;
        };
        let query = self.build_query(raw);
        self.link_node(node, level, None, query.as_deref());
    }

    /// Where `doc`'s vector goes: a node of its own, allocated and not yet
    /// linked, and its level -- or `None` where it needs none: the node it
    /// holds already, when the vector is the one it holds, or the node of
    /// another document holding the same vector to the bit, which it joins
    /// ([`VectorIndex::aliases`]). A document written again with another
    /// vector leaves its node first ([`Self::detach`]). The level is drawn
    /// only for a new node, so a graph with no vector twice is the graph it
    /// was.
    #[inline(never)]
    fn place(&mut self, doc: DocId, raw: &[f32]) -> Option<(u32, usize)> {
        let raw = &*self.as_held(raw);
        if let Some(old) = self.node_of(doc) {
            if self.retire(doc, old, raw) {
                return None;
            }
        }
        let (hash, copy) = self.find_copy(raw);
        if let Some(node) = copy {
            self.alias(node, doc);
            return None;
        }
        let level = self.random_level();
        let coded = self.data.quantized();
        let node = self.alloc_node(doc, raw, level);
        self.by_doc.insert(doc, node + 1);
        if coded != self.data.quantized() {
            // A bit index has coded the vectors it held whole: made again,
            // over the codes, the next time a vector is placed.
            self.same.built = false;
        } else if self.same.built {
            self.same_put(node, hash);
        }
        Some((node, level))
    }

    /// `raw` as the arena would store it (`Arena::write_stored`): pushed
    /// into an empty arena coded as this one, made a unit one for cosine as
    /// `insert` makes it. Over codes two vectors may share a code, and
    /// share a node: `near` puts the documents it finds in order by their
    /// own vectors, so each keeps its score.
    fn stored(&self, raw: &[f32]) -> Vec<u8> {
        let mut out = Vec::new();
        self.stored_into(raw, &mut out);
        out
    }

    /// [`Self::stored`] into `out`, through an arena of its own.
    fn stored_into(&self, raw: &[f32], out: &mut Vec<u8>) {
        out.clear();
        let mut probe = self.data.empty_like();
        probe.push(raw, self.spec.metric == Metric::Cosine);
        probe.write_stored(0, self.dim, out);
    }

    /// `raw`'s hash in [`Same`], and a live node holding it to the bit.
    /// Over an f32 arena both come straight from `raw`, scaled as
    /// `Arena::push` scales it, and a node's vector is compared only once
    /// the top half of its hash is `raw`'s: stored into an arena of its own
    /// and written out, and compared at every slot a probe passed, a 10 000
    /// x 128 build in the browser took 5% longer.
    fn find_copy(&mut self, raw: &[f32]) -> (u64, Option<u32>) {
        if !self.same.built {
            self.same_build();
        }
        let scaled = |x: f32, inv: f32| if inv != 1.0 { x * inv } else { x };
        let (hash, inv) = match &self.data {
            Arena::F32(_) => {
                let inv = match self.spec.metric == Metric::Cosine {
                    true => unit_scale(flat_sq(raw)),
                    false => 1.0,
                };
                // The browser's hash takes the closure the comparison
                // below takes: through a function of its own, 21 bytes of
                // its module more.
                #[cfg(target_family = "wasm")]
                let hash = hash_words(raw.iter().map(|&x| scaled(x, inv).to_bits()));
                #[cfg(not(target_family = "wasm"))]
                let hash = hash_scaled(raw, inv);
                (hash, Some(inv))
            }
            _ => {
                let mut stored = std::mem::take(&mut self.stored_buf);
                self.stored_into(raw, &mut stored);
                let hash = hash_stored(&stored);
                self.stored_buf = stored;
                (hash, None)
            }
        };
        // The browser's probe reads its one table as it did: through the
        // slice `home` gives, 21 bytes of its module more.
        #[cfg(target_family = "wasm")]
        let (slots, mask) = (&self.same.slots, self.same.slots.len() - 1);
        #[cfg(target_family = "wasm")]
        let mut i = hash as usize & mask;
        #[cfg(not(target_family = "wasm"))]
        let (slots, mut i) = self.same.home(hash);
        #[cfg(not(target_family = "wasm"))]
        let mask = slots.len() - 1;
        loop {
            let slot = slots[i];
            let Some(node) = (slot as u32).checked_sub(1) else {
                return (hash, None);
            };
            if slot >> 32 == hash >> 32 && !self.is_deleted(node) {
                let same = match (&self.data, inv) {
                    (Arena::F32(d), Some(inv)) => d
                        .row(node as usize, self.dim)
                        .iter()
                        .zip(raw)
                        .all(|(a, &b)| a.to_bits() == scaled(b, inv).to_bits()),
                    _ => self.stored_of(node) == self.stored_buf,
                };
                if same {
                    return (hash, Some(node));
                }
            }
            i = (i + 1) & mask;
        }
    }

    fn stored_of(&self, node: u32) -> Vec<u8> {
        let mut out = Vec::new();
        self.data.write_stored(node, self.dim, &mut out);
        out
    }

    /// Makes [`Same`] from the arena, two to four slots a live node: more
    /// than half full it is made again twice the size.
    #[cfg(target_family = "wasm")]
    fn same_build(&mut self) {
        let live = self.doc_ids.len() - self.deleted_count;
        self.same.slots = vec![0; (live.max(8) * 2).next_power_of_two()];
        self.same.used = 0;
        self.same.built = true;
        for node in 0..self.doc_ids.len() as u32 {
            if !self.is_deleted(node) {
                let h = match &self.data {
                    Arena::F32(d) => {
                        hash_words(d.row(node as usize, self.dim).iter().map(|x| x.to_bits()))
                    }
                    _ => hash_stored(&self.stored_of(node)),
                };
                self.same_put(node, h);
            }
        }
    }

    #[cfg(target_family = "wasm")]
    fn same_put(&mut self, node: u32, hash: u64) {
        if (self.same.used + 1) * 2 > self.same.slots.len() {
            // Made again, `node` among the rest: it is in the arena.
            return self.same_build();
        }
        let mask = self.same.slots.len() - 1;
        let mut i = hash as usize & mask;
        while self.same.slots[i] != 0 {
            i = (i + 1) & mask;
        }
        self.same.slots[i] = (hash >> 32) << 32 | (node as u64 + 1);
        self.same.used += 1;
    }

    /// Makes [`Same`] from the hash halves the restored record carried,
    /// where it did -- no vector read -- and from the arena otherwise, every
    /// node's vector hashed ([`Self::arena_halves`]), the live ones put in
    /// in node order. Read from the arena, a vector at a time down one chain
    /// of multiplies, it took the first put after an open 275-441 ms over
    /// 1.1 million of 128 dimensions; on every core, four chains a vector,
    /// 33-169.
    #[cfg(not(target_family = "wasm"))]
    fn same_build(&mut self) {
        let n = self.doc_ids.len();
        let halves = match self.same.halves.len() == n {
            true => std::mem::take(&mut self.same.halves),
            false => self.arena_halves(n),
        };
        self.same = Same::made(&halves, &self.deleted);
    }

    /// The top half of the first `n` nodes' hashes in [`Same`], from their
    /// vectors in the arena -- a share of [`SAME_SHARE`] nodes at a time by
    /// whichever thread is free, where there are more than one.
    #[cfg(not(target_family = "wasm"))]
    fn arena_halves(&self, n: usize) -> Vec<u32> {
        let dim = self.dim;
        let half = |node: usize, buf: &mut Vec<u8>| self.arena_half(node, buf);
        let threads = match n * dim > SAME_SHARE * 64 {
            true => Self::threads(),
            false => 1,
        };
        let mut halves = vec![0u32; n];
        if threads < 2 {
            let mut buf = Vec::new();
            for (node, h) in halves.iter_mut().enumerate() {
                *h = half(node, &mut buf);
            }
        } else {
            let shares: Vec<Mutex<&mut [u32]>> =
                halves.chunks_mut(SAME_SHARE).map(Mutex::new).collect();
            let mut bufs: Vec<Vec<u8>> = vec![Vec::new(); threads.min(shares.len())];
            spread(shares.len(), &mut bufs, |buf, i| {
                let mut share = shares[i].lock().unwrap_or_else(|e| e.into_inner());
                for (j, h) in share.iter_mut().enumerate() {
                    *h = half(i * SAME_SHARE + j, buf);
                }
            });
        }
        halves
    }

    /// Node `node`'s hash half in [`Same`], from its vector in the arena.
    #[cfg(not(target_family = "wasm"))]
    fn arena_half(&self, node: usize, buf: &mut Vec<u8>) -> u32 {
        let h = match &self.data {
            Arena::F32(d) => hash_lanes(d.row(node, self.dim), f32::to_bits, &[]),
            data => {
                buf.clear();
                data.write_stored(node as u32, self.dim, buf);
                hash_stored(buf)
            }
        };
        (h >> 32) as u32
    }

    /// The hash halves a restored record carries, kept for the first vector
    /// placed to make [`Same`] from, where a few live nodes spread over the
    /// arena hash to theirs: halves of another hash, or of other vectors,
    /// would leave a copy unfound, and the table is made from the arena
    /// then, as for a record without them. Nothing more is checked, as a
    /// link is checked only to name a node: a half gone wrong costs a copy
    /// a node of its own, never a document.
    #[cfg(not(target_family = "wasm"))]
    fn keep_halves(&mut self, bytes: &[u8], flags: &[u8]) {
        let halves: Vec<u32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_le_bytes(*b))
            .collect();
        if halves.len() != flags.len() {
            return;
        }
        let live = flags.iter().filter(|&&f| f != DEAD).count();
        let mut buf = Vec::new();
        let agree = (0..flags.len())
            .filter(|&node| flags[node] != DEAD)
            .step_by((live / HALVES_CHECKED).max(1))
            .all(|node| self.arena_half(node, &mut buf) == halves[node]);
        if agree {
            self.same.halves = halves;
        }
    }

    #[cfg(not(target_family = "wasm"))]
    #[inline]
    fn same_put(&mut self, node: u32, hash: u64) {
        self.same.put(node, hash, &self.deleted);
    }

    /// The documents holding `node`'s vector but its own.
    fn aliases_of(&self, node: u32) -> &[(u32, DocId)] {
        let a = self.aliases.partition_point(|&(n, _)| n < node);
        let b = a + self.aliases[a..].partition_point(|&(n, _)| n == node);
        &self.aliases[a..b]
    }

    /// `doc` joins `node`, whose vector it holds.
    fn alias(&mut self, node: u32, doc: DocId) {
        let at = self.aliases.partition_point(|&x| x < (node, doc));
        self.aliases.insert(at, (node, doc));
        self.by_doc.insert(doc, node + 1);
        self.changes += 1;
    }

    /// Takes `doc` off `node`: one holding its vector besides is left, or
    /// takes the node over where it was the node's own, and the node becomes
    /// a tombstone only when no document holds its vector any more.
    fn detach(&mut self, doc: DocId, node: u32) {
        self.by_doc.remove(doc);
        if let Ok(at) = self.aliases.binary_search(&(node, doc)) {
            self.aliases.remove(at);
            self.changes += 1;
            return;
        }
        let first = self.aliases.partition_point(|&(n, _)| n < node);
        if self.aliases.get(first).is_some_and(|&(n, _)| n == node) {
            let (_, next) = self.aliases.remove(first);
            self.doc_ids[node as usize] = next;
            self.changes += 1;
            return;
        }
        if !self.deleted[node as usize] {
            self.deleted[node as usize] = true;
            self.deleted_count += 1;
            self.changes += 1;
        }
    }

    /// Number of usable threads.
    ///
    /// WASM has a single thread and `std::thread` does not work there;
    /// splitting on the target keeps thread code out of the browser output
    /// entirely (~30 KB difference).
    #[cfg(target_family = "wasm")]
    #[inline]
    fn threads() -> usize {
        1
    }

    #[cfg(not(target_family = "wasm"))]
    #[inline]
    fn threads() -> usize {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    }

    /// Computes the candidates of `pending` against the graph as it stands,
    /// which does not change meanwhile: a node at a time to whichever
    /// thread is free, each with its own `Scratch`. They come back in no
    /// order.
    fn compute_candidates(
        &self,
        pending: &[(u32, usize, Option<Vec<f32>>)],
        scratch: &mut [Scratch],
    ) -> Candidates {
        let (efc, m) = (self.spec.ef_construction, self.spec.m);
        let view = self.view();
        spread(pending.len(), scratch, |sc, i| {
            let (node, level, query) = &pending[i];
            let found = view.candidates_for(sc, *node, query.as_deref(), *level, efc, m);
            (*node, *level, found)
        })
    }

    /// Links a batch whose neighbours were all found against the graph
    /// before it -- so none of them is among another's -- link for link as
    /// linking them one after another in node order would, with the lists
    /// they join pruned in parallel.
    ///
    /// A node's own lists are its candidates. Each neighbour it chose takes
    /// it into a list that, full, is chosen again by the diversity
    /// heuristic: some 33 x 32 / 2 distances a list, up to 16 lists a node.
    /// On one thread that was 70% of a 100 000 x 128 build, the candidates
    /// taking the other 30% on eight. A pruning reads only the list it
    /// prunes and the vectors, so the additions are grouped by the list
    /// they join and each group applied in node order, on whichever thread
    /// is free: a list ends as it would have in turn. The build took 5.3 s
    /// against 10.6, and 23.4 against 48.9 at 768 dimensions.
    fn link_batch(&mut self, mut computed: Candidates, threads: usize) {
        computed.sort_by_key(|(node, _, _)| *node);
        // (the list's node, its level, the node joining it)
        let mut added: Vec<(u32, u32, u32)> = Vec::new();
        for (node, level, per_level) in &computed {
            for (l, selected) in per_level {
                self.set_neighbors(*node, *l, selected);
                for &nb in selected {
                    // Past the neighbour's top level there is no list.
                    if *l == 0 || *l <= self.node_levels(nb) {
                        added.push((nb, *l as u32, *node));
                    }
                }
            }
            if *level > self.max_level {
                self.max_level = *level;
                self.entry = Some(*node);
            }
        }
        // By list, and in node order within one.
        added.sort_unstable();
        let mut groups = Vec::new();
        let mut at = 0;
        while at < added.len() {
            let (nb, l, _) = added[at];
            let len = added[at..]
                .iter()
                .take_while(|&&(n, k, _)| (n, k) == (nb, l))
                .count();
            groups.push(at..at + len);
            at += len;
        }
        // Most groups are a push or one pruning: 16 of them to a thread at
        // a time.
        let blocks: Vec<_> = groups.chunks(16).collect();
        let (view, m, m0) = (self.view(), self.spec.m, self.m0);
        let mut bufs: Vec<Pruning> = (0..threads).map(|_| Pruning::default()).collect();
        let lists = spread(blocks.len(), &mut bufs, |buf, i| {
            let mut out = Vec::with_capacity(blocks[i].len());
            for group in blocks[i] {
                let (nb, l, _) = added[group.start];
                let l = l as usize;
                let max_deg = if l == 0 { m0 } else { m };
                let mut list = view.neighbors(nb, l).to_vec();
                for &(_, _, node) in &added[group.clone()] {
                    if list.len() < max_deg {
                        list.push(node);
                    } else {
                        list = view.pruned(nb, &list, node, max_deg, buf);
                    }
                }
                out.push((nb, l, list));
            }
            out
        });
        for (nb, l, list) in lists.into_iter().flatten() {
            self.set_neighbors(nb, l, &list);
        }
    }

    /// Links a batch one node after another, in node order: what
    /// [`Self::link_batch`] has to end with, link for link.
    #[cfg(test)]
    fn link_serially(&mut self, mut computed: Candidates, _threads: usize) {
        computed.sort_by_key(|(node, _, _)| *node);
        for (node, level, per_level) in computed {
            self.link_node(node, level, Some(per_level), None);
        }
    }

    /// Inserts one batch in parallel.
    ///
    /// Finding a new node's neighbours -- descent, beam search and the
    /// selection among the candidates -- only reads the graph, so a batch
    /// finds all of its nodes' at once; linking them prunes the lists they
    /// join, which [`Self::link_batch`] runs in parallel too, grouped by
    /// list.
    ///
    /// Nodes inside a batch cannot see each other, so the batch size scales
    /// with the graph size (`len/16`, at most 512). In a 100k graph a batch
    /// of 512 means 0.5% invisibility, and the measured recall loss is
    /// negligible.
    ///
    /// The result is **deterministic**: candidates are computed against the
    /// pre-batch graph and links are always applied in node order, so the
    /// thread count does not change the output. (It is not the same graph as
    /// serial insertion -- there every node sees the ones before it.)
    pub fn insert_batch(&mut self, items: &[(DocId, Vec<f32>)]) {
        self.insert_batch_by(items, Self::link_batch)
    }

    /// `raw` as a `vector<N, f16>` field holds it, each component rounded
    /// to a half: what a restore fills the arena with, reading the
    /// document's record. Placed as written, a vector was made a unit one
    /// of other halves under cosine than an open made, and `near ...
    /// exact` scored differently once the database was opened again --
    /// and from the module without the graph, which measures the stored
    /// halves. Only the arena takes it: the walk that links a node may
    /// start from the vector as written.
    fn as_held<'a>(&self, raw: &'a [f32]) -> Cow<'a, [f32]> {
        match self.prec {
            VecPrec::F32 => Cow::Borrowed(raw),
            VecPrec::F16 => Cow::Owned(halved(raw)),
        }
    }

    /// [`Self::insert_batch`], linking each batch with `link`: the tests
    /// hold [`Self::link_batch`] to [`Self::link_serially`] through it.
    fn insert_batch_by(
        &mut self,
        items: &[(DocId, Vec<f32>)],
        link: fn(&mut Self, Candidates, usize),
    ) {
        let threads = Self::threads();
        let mut scratch = Vec::new();

        let mut rest = items;
        while !rest.is_empty() {
            // The parallelism decision is made **per batch**. Made per call,
            // inserting 100k into an empty index in one go would drop the
            // whole batch onto the serial path -- exactly when it should
            // parallelise as the graph grows.
            if threads < 2 || self.doc_ids.len() < 1024 {
                let take = rest
                    .len()
                    .min(1024_usize.saturating_sub(self.doc_ids.len()).max(1));
                for (doc, v) in &rest[..take] {
                    self.insert(*doc, v);
                }
                rest = &rest[take..];
                continue;
            }

            let chunk_len = (self.doc_ids.len() / 16)
                .clamp(64, MAX_BATCH)
                .min(rest.len());
            let (chunk, tail) = rest.split_at(chunk_len);
            rest = tail;

            // 1) Serial: allocate the nodes (arena, id, level).
            let mut pending: Vec<(u32, usize, Option<Vec<f32>>)> = Vec::with_capacity(chunk.len());
            for (doc, v) in chunk {
                if v.len() != self.dim {
                    continue;
                }
                if let Some((node, level)) = self.place(*doc, v) {
                    pending.push((node, level, self.build_query(v)));
                }
            }
            if pending.is_empty() {
                continue;
            }
            if self.entry.is_none() {
                // The first node becomes the entry point; the rest go in serially.
                let (first, level, _) = pending[0];
                self.entry = Some(first);
                self.max_level = level;
                for (node, level, query) in &pending[1..] {
                    self.link_node(*node, *level, None, query.as_deref());
                }
                continue;
            }

            // 2) Parallel: compute the candidates (the graph does not change).
            if scratch.is_empty() {
                scratch = (0..threads).map(|_| Scratch::new()).collect();
            }
            let computed = self.compute_candidates(&pending, &mut scratch);

            // 3) Write the links; the lists they join are pruned in parallel.
            link(self, computed, threads);
        }
    }

    /// Puts `items` in the arena as [`Self::insert_batch`] would, and links
    /// none of them: until [`Self::link_pending`] does, a search measures
    /// each against the query, so an answer is what the graph finds and
    /// the nearest of these together. What a server opens a file with --
    /// the writes after its last checkpoint -- when linking them first
    /// kept its port closed: 56.6 s at 100 000 x 768 never checkpointed.
    /// Returns how many nodes it left waiting.
    pub fn defer_batch(&mut self, items: &[(DocId, Vec<f32>)]) -> usize {
        if !UNLINKED {
            self.insert_batch(items);
            return 0;
        }
        let before = self.pending.len();
        for (doc, v) in items {
            if v.len() != self.dim {
                continue;
            }
            if let Some((node, _)) = self.place(*doc, v) {
                self.pending.push(node);
            }
        }
        self.pending.len() - before
    }

    /// Drops the newest of the waiting nodes, up to `max` of them, for as
    /// long as they are tombstones: what a block that left them waiting
    /// leaves when it is put back, which no link will ever need. Returns
    /// how many it dropped.
    pub fn forget_waiting(&mut self, max: usize) -> usize {
        let mut n = 0;
        while n < max && self.pending.last().is_some_and(|&x| self.is_deleted(x)) {
            self.pending.pop();
            n += 1;
        }
        n
    }

    /// Links up to `max` of the nodes [`Self::defer_batch`] left out of the
    /// graph, the newest first, and returns how many are left. They go in
    /// as a batch does -- one at a time while the graph is small, their
    /// candidates computed in parallel after -- so a caller holding a lock
    /// for it holds it as long as `max` nodes take. Over codes a node looks
    /// for its neighbours with its document's own vector, which `lookup`
    /// reads, as the write path does.
    pub fn link_pending(
        &mut self,
        max: usize,
        lookup: &mut dyn FnMut(DocId, &mut Vec<f32>) -> bool,
    ) -> usize {
        let threads = Self::threads();
        let mut scratch = Vec::new();
        let at = self.pending.len().saturating_sub(max.max(1));
        let taken = self.pending.split_off(at);
        let mut raw = Vec::new();
        let mut todo: Vec<(u32, usize, Option<Vec<f32>>)> = Vec::with_capacity(taken.len());
        for node in taken {
            if self.is_deleted(node) {
                continue;
            }
            let query = match self.quantized() {
                true => {
                    let doc = self.doc_ids[node as usize];
                    (lookup(doc, &mut raw) && raw.len() == self.dim).then(|| self.query_for(&raw))
                }
                false => None,
            };
            todo.push((node, self.node_levels(node), query));
        }
        self.changes += todo.len() as u64;
        // The nodes the graph holds, as `insert_batch` counts them.
        let mut linked = self.doc_ids.len() - self.pending.len() - todo.len();
        let mut rest = &todo[..];
        while !rest.is_empty() {
            if threads < 2 || linked < 1024 || self.entry.is_none() {
                let take = rest.len().min(1024_usize.saturating_sub(linked).max(1));
                for (node, level, query) in &rest[..take] {
                    self.link_node(*node, *level, None, query.as_deref());
                }
                rest = &rest[take..];
                linked += take;
                continue;
            }
            let (chunk, tail) = rest.split_at((linked / 16).clamp(64, MAX_BATCH).min(rest.len()));
            rest = tail;
            linked += chunk.len();
            if scratch.is_empty() {
                scratch = (0..threads).map(|_| Scratch::new()).collect();
            }
            let computed = self.compute_candidates(chunk, &mut scratch);
            self.link_batch(computed, threads);
        }
        self.pending.len()
    }

    /// Links the computed candidates into the graph (computing them itself
    /// when none are supplied, searching for `query` or the node's own
    /// vector).
    fn link_node(
        &mut self,
        node: u32,
        level: usize,
        precomputed: Option<Vec<(usize, Vec<u32>)>>,
        query: Option<&[f32]>,
    ) {
        let Some(entry) = self.entry else {
            self.entry = Some(node);
            self.max_level = level;
            return;
        };

        let per_level: Vec<(usize, Vec<u32>)> = match precomputed {
            Some(p) => p,
            None => {
                let v: Vec<f32> = match query {
                    Some(q) => q.to_vec(),
                    None => self.vec_at(node).into_owned(),
                };
                let start_level = self.max_level;
                let start = if start_level > level {
                    self.descend_beam(&v, entry, start_level, level)
                } else {
                    vec![entry]
                };
                let mut out = Vec::new();
                let mut ep = start.clone();
                for l in (0..=level.min(self.max_level)).rev() {
                    let cands = self.search_layer(&v, &ep, self.spec.ef_construction, l);
                    let selected = self.select_heuristic(&cands, self.spec.m, node);
                    ep = if selected.is_empty() {
                        start.clone()
                    } else {
                        selected.clone()
                    };
                    out.push((l, selected));
                }
                out
            }
        };

        for (l, selected) in per_level {
            let max_deg = if l == 0 { self.m0 } else { self.spec.m };
            self.set_neighbors(node, l, &selected);
            for &nb in &selected {
                if l > 0 && l > self.node_levels(nb) {
                    continue; // the neighbour does not have this level
                }
                if self.push_neighbor(nb, l, node, max_deg) {
                    continue;
                }
                // The list is full: reselect with the diversity heuristic.
                let mut buf = std::mem::take(&mut self.prune_buf);
                let pruned = self
                    .view()
                    .pruned(nb, self.neighbors(nb, l), node, max_deg, &mut buf);
                self.set_neighbors(nb, l, &pruned);
                self.prune_buf = buf;
            }
        }

        if level > self.max_level {
            self.max_level = level;
            self.entry = Some(node);
        }
    }

    /// A document's node met again with `raw`: `true` when it already holds
    /// that vector and stays, and tombstoned otherwise. An update of any
    /// other field wrote the vector again, and took it out of the graph and
    /// back in: 1.89 ms at 20 000 x 768, and a tombstone each time.
    ///
    /// Held is to the bit, in the arena's own form: `raw` is stored into an
    /// empty arena as it would be here and both are written out as the graph
    /// record writes them -- the code already in the browser module, where
    /// a comparison per arena kind was 800 bytes more.
    fn retire(&mut self, doc: DocId, node: u32, raw: &[f32]) -> bool {
        if self.deleted[node as usize] {
            return false;
        }
        if self.stored_of(node) == self.stored(raw) {
            return true;
        }
        self.detach(doc, node);
        false
    }

    pub fn remove(&mut self, doc: DocId) {
        if let Some(node) = self.node_of(doc) {
            self.detach(doc, node);
        }
    }

    /// The node holding `doc`'s vector.
    #[inline]
    fn node_of(&self, doc: DocId) -> Option<u32> {
        self.by_doc.get(doc).checked_sub(1)
    }

    /// Changes to the graph since it was made or restored: nodes added,
    /// retired and linked.
    pub fn changes(&self) -> u64 {
        self.changes
    }

    /// What of this graph has reached the file.
    pub fn persisted(&self) -> &Persisted {
        &self.persisted
    }

    /// k nearest neighbours. The `accept` predicate is applied to the result
    /// set; traversal keeps running over every node, so the filter does not
    /// break the connectivity of the graph.
    pub fn search<F>(
        &self,
        query: &[f32],
        k: usize,
        ef: Option<usize>,
        accept: F,
    ) -> Vec<(DocId, f32)>
    where
        F: Fn(DocId) -> bool,
    {
        if k == 0 || (self.entry.is_none() && self.unlinked() == 0) {
            return Vec::new();
        }
        let q = self.query_for(query);
        let ef = ef.unwrap_or(self.spec.ef_search).max(k);

        let mut found = match self.entry {
            Some(entry) => {
                let start = self.descend_beam(&q, entry, self.max_level, 0);
                self.search_layer(&q, &start, ef, 0)
            }
            None => Vec::new(),
        };
        // No link reaches a node not linked yet, so the walk cannot find
        // one: each is measured, and ranked with what the walk found.
        if self.unlinked() > 0 {
            let live = self
                .pending
                .iter()
                .copied()
                .filter(|&n| !self.is_deleted(n));
            found.extend(self.measure(&q, live));
            found.sort();
        }

        let mut out = Vec::with_capacity(k);
        let mut docs = Vec::new();
        'found: for c in found {
            if self.is_deleted(c.node) {
                continue;
            }
            let score = score_from_distance(self.spec.metric, c.dist);
            if self.aliases.is_empty() {
                let doc = self.doc_ids[c.node as usize];
                if accept(doc) {
                    out.push((doc, score));
                }
            } else {
                // A node is every document holding its vector, at one
                // distance, in the order of their ids.
                self.docs_of(c.node, &mut docs);
                for &doc in &docs {
                    if accept(doc) {
                        out.push((doc, score));
                        if out.len() == k {
                            break 'found;
                        }
                    }
                }
            }
            if out.len() == k {
                break;
            }
        }
        out
    }

    /// `node`'s documents: its own and those holding its vector, in order.
    fn docs_of(&self, node: u32, out: &mut Vec<DocId>) {
        out.clear();
        out.push(self.doc_ids[node as usize]);
        out.extend(self.aliases_of(node).iter().map(|&(_, d)| d));
        if out.len() > 1 {
            out.sort_unstable();
        }
    }

    /// The `k` nearest of `pairs` -- `(node, doc)`, in order -- each node
    /// measured once and handed out as its documents.
    fn nearest_docs(&self, q: &[f32], pairs: &[(u32, DocId)], k: usize) -> Vec<(DocId, f32)> {
        let mut nodes: Vec<u32> = pairs.iter().map(|p| p.0).collect();
        nodes.dedup();
        let mut out = Vec::with_capacity(k);
        for c in self.nearest_of(q, &nodes, k) {
            let score = score_from_distance(self.spec.metric, c.dist);
            let at = pairs.partition_point(|p| p.0 < c.node);
            for &(_, doc) in pairs[at..].iter().take_while(|p| p.0 == c.node) {
                out.push((doc, score));
                if out.len() == k {
                    return out;
                }
            }
        }
        out
    }

    // -------------------------------------------------------- persistence

    /// Serialises the graph -- **not the vectors**.
    ///
    /// The vectors already sit in the document records; writing them a second
    /// time would double the file. The expensive part is building the graph,
    /// while filling the arena is a linear copy. Only the links, the levels
    /// and the entry point are therefore stored.
    pub fn serialize_graph(&self) -> Vec<u8> {
        self.serialize(false)
    }

    fn serialize(&self, kept: bool) -> Vec<u8> {
        let mut out = Vec::new();
        self.serialize_into(kept, &mut out);
        out
    }

    /// [`Self::serialize_graph`] appended to `out`, as the record it lands
    /// in: an image's, or with `kept` one in the file's tail
    /// ([`GRAPH_VERSION_KEPT`]), which a server writes under the read lock.
    pub fn serialize_graph_into(&self, kept: bool, out: &mut Vec<u8>) {
        self.serialize_into(kept, out)
    }

    fn serialize_into(&self, kept: bool, out: &mut Vec<u8>) {
        let n = self.doc_ids.len();
        out.reserve(64 + n * (12 + 4 * self.m0));
        out.push(match kept {
            true => GRAPH_VERSION_ALIASED_KEPT,
            false => GRAPH_VERSION_ALIASED,
        });
        self.write_head(out, true);
        put_uvarint(out, n as u64);
        put_uvarint(out, self.entry.map(|e| e as u64 + 1).unwrap_or(0));
        put_uvarint(out, self.max_level as u64);
        // A list's first link is as many bytes as the node count needs, its
        // steps as many as its widest, a document id 4 where every one fits:
        // links and ids written 4 and 8 whatever, a 100 000 x 128 graph took
        // 12.4 MB against the varint deltas' 6.2, a file 5% larger.
        let (lw, dw) = (link_width(n), doc_width(&self.doc_ids));
        out.extend_from_slice(&[lw as u8, dw as u8]);
        // The nodes still to be linked, flagged as such: written as nodes
        // with no links they would come back unreachable.
        let mut waiting = Vec::new();
        if UNLINKED {
            waiting.resize(n, false);
            for &node in &self.pending {
                waiting[node as usize] = !self.is_deleted(node);
            }
        }
        for &doc in &self.doc_ids {
            put_le(out, doc, dw);
        }
        for node in 0..n {
            out.push(match self.deleted[node] {
                true => 1,
                false => 2 * waiting.get(node).copied().unwrap_or(false) as u8,
            });
        }
        // A level is a byte: `random_level` stops short of 32.
        out.extend(self.upper.iter().map(|u| u.len() as u8));
        for &k in &self.l0_len {
            out.extend_from_slice(&k.to_le_bytes());
        }
        self.put_all_lists(n, lw, out);
        // A tombstone still routes searches, but its document may be gone
        // or hold another vector by now: the vector it was linked with
        // travels with it.
        for node in (0..n as u32).filter(|&v| self.is_deleted(v)) {
            self.data.write_stored(node, self.dim, out);
        }
        put_le(out, self.aliases.len() as u64, 8);
        for &(node, doc) in &self.aliases {
            put_le(out, node as u64, lw);
            put_le(out, doc, 8);
        }
        #[cfg(not(target_family = "wasm"))]
        self.put_halves(n, out);
    }

    /// Each of the `n` nodes' hash halves after a [`HALVES`] byte, a
    /// tombstone's 0: the ones a restore kept, or read off the table's
    /// slots, where a node's half is beside it -- or, with neither at hand,
    /// hashed from the arena, as the table is made where a restore had none
    /// or a bit index coded its vectors. So the record is the same whatever
    /// way the index came to be where it is.
    #[cfg(not(target_family = "wasm"))]
    fn put_halves(&self, n: usize, out: &mut Vec<u8>) {
        if n == 0 {
            return;
        }
        let read;
        let halves = match (self.same.halves.len() == n, self.same.built) {
            (true, _) => &self.same.halves,
            (false, true) => {
                read = self.same.halves_by_node(n);
                &read
            }
            (false, false) => {
                read = self.arena_halves(n);
                &read
            }
        };
        out.reserve(1 + 4 * n);
        out.push(HALVES);
        for (h, &dead) in halves.iter().zip(&self.deleted) {
            let h = if dead { 0 } else { *h };
            out.extend_from_slice(&h.to_le_bytes());
        }
    }

    /// The lists of `nodes`, each as [`put_links`] writes it: level 0's with
    /// `level0`, the ones above otherwise, each behind its length. A list
    /// is a set, written sorted, as the varint layout wrote it: as `u64`s,
    /// whose sort the ids take anyway -- a `u32` sort of its own was 2 KB of
    /// the browser module.
    fn put_lists(
        &self,
        nodes: std::ops::Range<u32>,
        level0: bool,
        lw: usize,
        sorted: &mut Vec<u64>,
        out: &mut Vec<u8>,
    ) {
        let mut links = |out: &mut Vec<u8>, nbs: &[u32]| {
            sorted.clear();
            sorted.extend(nbs.iter().map(|&nb| nb as u64));
            sorted.sort_unstable();
            put_links(out, sorted, lw);
        };
        for node in nodes {
            if level0 {
                links(out, self.neighbors(node, 0));
                continue;
            }
            for l in 1..=self.node_levels(node) {
                let nbs = self.neighbors(node, l);
                out.extend_from_slice(&(nbs.len() as u16).to_le_bytes());
                links(out, nbs);
            }
        }
    }

    /// Every node's level-0 list, then every list above, in node order.
    #[cfg(target_family = "wasm")]
    fn put_all_lists(&self, n: usize, lw: usize, out: &mut Vec<u8>) {
        let mut sorted = Vec::with_capacity(self.m0);
        self.put_lists(0..n as u32, true, lw, &mut sorted, out);
        self.put_lists(0..n as u32, false, lw, &mut sorted, out);
    }

    /// Every node's level-0 list, then every list above, in node order:
    /// natively [`LIST_PART`] nodes' lists at a time on every core, each
    /// part's bytes appended in order. A server writes a graph it keeps
    /// under the read lock, which every write waits out, and the lists are
    /// most of the record: 110 000 nodes' took 20.3 ms in turn, and take
    /// 6.0 on an M1's eight cores.
    #[cfg(not(target_family = "wasm"))]
    fn put_all_lists(&self, n: usize, lw: usize, out: &mut Vec<u8>) {
        let parts = n.div_ceil(LIST_PART);
        let threads = Self::threads().min(parts);
        if threads < 2 {
            let mut sorted = Vec::with_capacity(self.m0);
            self.put_lists(0..n as u32, true, lw, &mut sorted, out);
            self.put_lists(0..n as u32, false, lw, &mut sorted, out);
            return;
        }
        let mut states: Vec<Vec<u64>> = (0..threads).map(|_| Vec::with_capacity(self.m0)).collect();
        let mut done: Vec<(Vec<u8>, Vec<u8>)> = vec![Default::default(); parts];
        let written = spread(parts, &mut states, |sorted, p| {
            let nodes = (p * LIST_PART) as u32..((p + 1) * LIST_PART).min(n) as u32;
            let (mut level0, mut above) = (Vec::new(), Vec::new());
            self.put_lists(nodes.clone(), true, lw, sorted, &mut level0);
            self.put_lists(nodes, false, lw, sorted, &mut above);
            (p, level0, above)
        });
        for (p, level0, above) in written {
            done[p] = (level0, above);
        }
        for (level0, _) in &done {
            out.extend_from_slice(level0);
        }
        for (_, above) in &done {
            out.extend_from_slice(above);
        }
    }

    /// What every layout of a graph record starts with after its version:
    /// the dimension, metric, beams, precision and, where `quant` or the
    /// version asks, the quantization with a bit index's centres.
    fn write_head(&self, out: &mut Vec<u8>, quant_byte: bool) {
        let quant = self.spec.quant;
        put_uvarint(out, self.dim as u64);
        out.push(match self.spec.metric {
            Metric::Cosine => 0,
            Metric::L2 => 1,
            Metric::Dot => 2,
        });
        put_uvarint(out, self.spec.m as u64);
        put_uvarint(out, self.spec.ef_construction as u64);
        put_uvarint(out, self.spec.ef_search as u64);
        out.push(match self.prec {
            VecPrec::F32 => 0,
            VecPrec::F16 => 1,
        });
        if quant_byte {
            match quant {
                Quant::Bit => out.push(BIT_CENTRED),
                _ => out.push(quant.code()),
            }
        }
        // The centres a bit index's codes are taken from: none while it
        // holds its vectors whole.
        if quant == Quant::Bit {
            let at = match &self.data {
                Arena::Bit(b) => &b.centres.at[..],
                _ => &[],
            };
            put_uvarint(out, (at.len() / self.dim.max(1)) as u64);
            at.iter()
                .for_each(|x| out.extend_from_slice(&x.to_le_bytes()));
        }
    }

    #[cfg(test)]
    fn serialize_varint(&self, kept: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.doc_ids.len() * 96);
        let quant = self.spec.quant;
        // The nodes still to be linked, flagged as such: written as nodes
        // with no links they would come back unreachable.
        let mut unlinked = Vec::new();
        if UNLINKED {
            for &node in &self.pending {
                if !self.is_deleted(node) {
                    unlinked.resize(self.doc_ids.len(), false);
                    unlinked[node as usize] = true;
                }
            }
        }
        out.push(if kept {
            GRAPH_VERSION_KEPT
        } else if !unlinked.is_empty() {
            GRAPH_VERSION_UNLINKED
        } else if quant == Quant::None {
            GRAPH_VERSION
        } else {
            GRAPH_VERSION_QUANT
        });
        self.write_head(
            &mut out,
            kept || quant != Quant::None || !unlinked.is_empty(),
        );
        put_uvarint(&mut out, self.doc_ids.len() as u64);
        put_uvarint(&mut out, self.entry.map(|e| e as u64 + 1).unwrap_or(0));
        put_uvarint(&mut out, self.max_level as u64);
        for (node, &doc) in self.doc_ids.iter().enumerate() {
            let node = node as u32;
            put_uvarint(&mut out, doc);
            out.push(match self.is_deleted(node) {
                true => 1,
                false => 2 * unlinked.get(node as usize).copied().unwrap_or(false) as u8,
            });
            let levels = self.node_levels(node) + 1; // level 0 included
            put_uvarint(&mut out, levels as u64);
            // A tombstone still routes searches, but its document may be
            // gone or hold another vector by now: the vector it was linked
            // with travels with it. Before, a restore that could not find a
            // deleted document's vector threw the graph away, so a single
            // `del` rebuilt it on every open until `compact` -- 852 ms
            // against 6.6 at 20 000 x 32.
            if self.is_deleted(node) {
                self.data.write_stored(node, self.dim, &mut out);
            }
            // `u64`s, whose sort the ids take anyway.
            let mut sorted: Vec<u64> = Vec::with_capacity(self.m0);
            for l in 0..levels {
                let nbs = self.neighbors(node, l);
                put_uvarint(&mut out, nbs.len() as u64);
                // The neighbour list is a *set*; its order carries no meaning.
                // Sorting and delta coding shortens the varints: in a 100k
                // node graph a raw id is 3 bytes, a delta about 2.
                sorted.clear();
                sorted.extend(nbs.iter().map(|&nb| nb as u64));
                sorted.sort_unstable();
                let mut prev = 0;
                for &nb in &sorted {
                    put_uvarint(&mut out, nb - prev);
                    prev = nb;
                }
            }
        }
        out
    }

    /// Restores a serialised graph.
    ///
    /// `lookup` must return the vector for every stored document id. If a
    /// single id is missing or validation fails it returns `None` and the
    /// caller rebuilds the index from scratch: the graph is derived data, so
    /// an inconsistency means a rebuild rather than data loss.
    pub fn restore_graph(
        bytes: &[u8],
        expect_dim: usize,
        expect_prec: VecPrec,
        lookup: impl Fn(DocId, &mut Vec<f32>) -> bool + Sync,
    ) -> Option<VectorIndex> {
        let mut pos = 0usize;
        let version = *bytes.first()?;
        let aliased = matches!(version, GRAPH_VERSION_ALIASED | GRAPH_VERSION_ALIASED_KEPT);
        let flat = aliased || matches!(version, GRAPH_VERSION_FLAT | GRAPH_VERSION_FLAT_KEPT);
        // A browser keeps no graph of its own in a tail, and rebuilds one a
        // server kept, as it does one with nodes waiting.
        let kept = UNLINKED
            && matches!(
                version,
                GRAPH_VERSION_KEPT | GRAPH_VERSION_FLAT_KEPT | GRAPH_VERSION_ALIASED_KEPT
            );
        let waits = UNLINKED && (version == GRAPH_VERSION_UNLINKED || kept || flat);
        let plain = matches!(
            version,
            GRAPH_VERSION | GRAPH_VERSION_QUANT | GRAPH_VERSION_FLAT | GRAPH_VERSION_ALIASED
        );
        if !waits && !kept && !plain {
            return None;
        }
        pos += 1;
        let dim = get_uvarint(bytes, &mut pos).ok()? as usize;
        if dim != expect_dim {
            return None;
        }
        let metric = match *bytes.get(pos)? {
            0 => Metric::Cosine,
            1 => Metric::L2,
            2 => Metric::Dot,
            _ => return None,
        };
        pos += 1;
        let mut spec = VectorIndexSpec {
            metric,
            m: get_uvarint(bytes, &mut pos).ok()? as usize,
            ef_construction: get_uvarint(bytes, &mut pos).ok()? as usize,
            ef_search: get_uvarint(bytes, &mut pos).ok()? as usize,
            quant: Quant::None,
        };
        // The field's own precision: restored as f32, a `vector<N, f16>`
        // field held twice the memory after every reopen.
        let prec = match *bytes.get(pos)? {
            0 => VecPrec::F32,
            1 => VecPrec::F16,
            _ => return None,
        };
        pos += 1;
        if prec != expect_prec {
            return None;
        }
        if version != GRAPH_VERSION {
            spec.quant = match *bytes.get(pos)? {
                BIT_CENTRED => Quant::Bit,
                // The plain signs, which this build codes no longer.
                c if c == Quant::Bit.code() => return None,
                c => Quant::from_code(c)?,
            };
            pos += 1;
        }
        let mut ix = VectorIndex::with_precision(dim, spec, prec);
        let mut centred = false;
        if spec.quant == Quant::Bit {
            let k = get_uvarint(bytes, &mut pos).ok()? as usize;
            if k > 256 {
                return None;
            }
            if k > 0 {
                let len = k * dim * 4;
                let at = bytes.get(pos..pos.checked_add(len)?)?;
                pos += len;
                let at = at.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b));
                let centres = Centres::new(at.collect(), dim);
                ix.data = Arena::Bit(Bits::new(Arc::new(centres), dim, prec == VecPrec::F16));
                centred = true;
            }
        }
        let count = get_uvarint(bytes, &mut pos).ok()? as usize;
        let entry_raw = get_uvarint(bytes, &mut pos).ok()?;
        let max_level = get_uvarint(bytes, &mut pos).ok()? as usize;
        // Whole vectors past the count they are coded at: not a record this
        // build wrote.
        if spec.quant == Quant::Bit && !centred && count >= BIT_TRAIN {
            return None;
        }
        let entry = entry_raw.checked_sub(1).map(|e| e as u32);
        let stored = ix.data.stored_len(dim);
        let body = &bytes[pos..];
        let read = match flat {
            true => Read::flat(body, count, ix.m0, stored, waits, aliased)?,
            false => Read::varint(body, count, ix.m0, stored, waits, kept)?,
        };
        ix.build_restored(read, entry, max_level, &lookup)
    }

    /// The index a graph record describes, read into arrays by either
    /// layout: the documents' vectors come in -- read and made unit ones,
    /// eight at a time natively -- in node order, a tombstone's out of the
    /// record, and the links as they were read.
    fn build_restored(
        mut self,
        read: Read<'_>,
        entry: Option<u32>,
        max_level: usize,
        lookup: &Lookup<'_>,
    ) -> Option<VectorIndex> {
        let count = read.docs.len();
        if entry.is_some_and(|e| e as usize >= count) {
            return None;
        }
        self.by_doc.reserve(count);
        let mut dead = 0;
        for (node, (&doc, &flag)) in read.docs.iter().zip(&read.flags).enumerate() {
            if flag == DEAD {
                dead += 1;
                continue;
            }
            if flag == WAITING {
                self.pending.push(node as u32);
            }
            // Two live nodes of one document would count as two of the
            // documents the engine holds this graph to.
            if self.by_doc.insert(doc, node as u32 + 1).is_some() {
                return None;
            }
        }
        if dead > read.tombs.len() || !self.fill_restored(&read, lookup) {
            return None;
        }
        // Each document named as holding a node's vector holds it, to the
        // bit, and is no other node's: anything else is a graph of other
        // documents, built again.
        let mut raw = Vec::new();
        for &(node, doc) in &read.aliases {
            if read.flags[node as usize] == DEAD || self.by_doc.insert(doc, node + 1).is_some() {
                return None;
            }
            if !lookup(doc, &mut raw) || raw.len() != self.dim {
                return None;
            }
            if self.stored(&raw) != self.stored_of(node) {
                return None;
            }
        }
        #[cfg(not(target_family = "wasm"))]
        if let Some(halves) = read.halves {
            self.keep_halves(halves, &read.flags);
        }
        self.deleted = read.flags.iter().map(|&f| f == DEAD).collect();
        self.deleted_count = dead;
        (self.doc_ids, self.l0, self.l0_len, self.upper, self.aliases) =
            (read.docs, read.l0, read.l0_len, read.upper, read.aliases);
        (self.entry, self.max_level) = (entry, max_level);
        // As its record has it: nothing to write again.
        self.changes = 0;
        Some(self)
    }

    /// Fills a restored graph's arena: a tombstone's vector as its record
    /// holds it, a live node's read from its document and made a unit one
    /// for cosine, as `insert` made it -- `false` where a document holds no
    /// vector of the index's length. A float arena is filled a share of
    /// [`FILL_SHARE`] nodes at a time by whichever thread is free, each
    /// writing its nodes' slots where they stand: read and scaled one
    /// vector at a time, copied into a batch and then into the arena, it
    /// was 7.6 of a 22 ms open at 100 000 x 128. A code arena is filled in
    /// turn, as a code can depend on the ones before it.
    #[cfg(target_family = "wasm")]
    fn fill_restored(&mut self, read: &Read<'_>, lookup: &Lookup<'_>) -> bool {
        // The browser has one thread, and the shares would only add to
        // its module: 1.3 KB brotli, for the same 18 ms load.
        let unit = self.spec.metric == Metric::Cosine;
        self.fill_in_turn(read, lookup, unit)
    }

    #[cfg(not(target_family = "wasm"))]
    fn fill_restored(&mut self, read: &Read<'_>, lookup: &Lookup<'_>) -> bool {
        let (count, dim) = (read.docs.len(), self.dim);
        let unit = self.spec.metric == Metric::Cosine;
        let Some(len) = count.checked_mul(dim).filter(|_| dim > 0) else {
            return self.fill_in_turn(read, lookup, unit);
        };
        let share = FILL_SHARE * dim;
        let parts: Vec<Mutex<Part<'_>>> = match &mut self.data {
            Arena::F32(d) => {
                d.reserve_exact(len);
                d.spare_capacity_mut()[..len]
                    .chunks_mut(share)
                    .map(|c| Mutex::new(Part::F32(c)))
                    .collect()
            }
            Arena::F16(d) => {
                d.reserve_exact(len);
                d.spare_capacity_mut()[..len]
                    .chunks_mut(share)
                    .map(|c| Mutex::new(Part::F16(c)))
                    .collect()
            }
            _ => return self.fill_in_turn(read, lookup, unit),
        };
        // Where each share's tombstones start among the record's.
        let mut tombs = Vec::with_capacity(parts.len());
        let mut before = 0;
        for flags in read.flags.chunks(FILL_SHARE) {
            tombs.push(before);
            before += flags.iter().filter(|&&f| f == DEAD).count();
        }
        let mut raws: Vec<[Vec<f32>; 8]> = (0..Self::threads().min(parts.len()))
            .map(|_| Default::default())
            .collect();
        let filled = spread(parts.len(), &mut raws, |raws, i| {
            let mut part = parts[i].lock().unwrap_or_else(|e| e.into_inner());
            let at = (i * FILL_SHARE, tombs[i]);
            fill_share(&mut part, at, read, raws, (dim, unit), lookup)
        });
        drop(parts);
        if filled.len() != tombs.len() || filled.contains(&false) {
            return false;
        }
        // SAFETY: every share wrote each of its nodes' `dim` slots -- a
        // tombstone's from the record, a live node's from its document --
        // or said it had not, which returned above.
        unsafe {
            match &mut self.data {
                Arena::F32(d) => d.set_len(len),
                Arena::F16(d) => d.set_len(len),
                _ => {}
            }
        }
        true
    }

    /// [`Self::fill_restored`] a node after another, on this thread.
    fn fill_in_turn(&mut self, read: &Read<'_>, lookup: &Lookup<'_>, unit: bool) -> bool {
        let dim = self.dim;
        let (mut raw, mut batch) = (Vec::with_capacity(dim), Vec::new());
        let mut tombs = read.tombs.iter();
        self.data.reserve(read.docs.len(), dim);
        for (&doc, &flag) in read.docs.iter().zip(&read.flags) {
            if flag == DEAD {
                if BATCH_NORMS {
                    flush_units(&mut self.data, &mut batch, dim, unit);
                }
                let Some(stored) = tombs.next() else {
                    return false;
                };
                self.data.push_stored(stored);
                continue;
            }
            if !lookup(doc, &mut raw) || raw.len() != dim {
                return false;
            }
            if BATCH_NORMS {
                batch.extend_from_slice(&raw);
                if batch.len() == 8 * dim {
                    flush_units(&mut self.data, &mut batch, dim, unit);
                }
            } else {
                self.data.push(&raw, unit);
            }
        }
        if BATCH_NORMS {
            flush_units(&mut self.data, &mut batch, dim, unit);
        }
        true
    }

    /// Exact (brute-force) search. Used on small collections and when
    /// verifying recall.
    /// Exact scan over only the given document ids.
    ///
    /// The cost is `ids.len()` distance computations -- independent of the
    /// collection size. Required for a filtered `near`: [`VectorIndex::search`]
    /// applies the filter *after* the ANN candidates have been gathered, so a
    /// selective filter may not leave the requested number of results.
    pub fn search_ids(&self, query: &[f32], k: usize, ids: &[DocId]) -> Vec<(DocId, f32)> {
        if k == 0 {
            return Vec::new();
        }
        let q = self.query_for(query);
        if !self.aliases.is_empty() {
            // Put in node order through the `u64` sort the module has: a
            // node and where its document is, one word. A sort of pairs was
            // a copy of its own, most of what the aliases cost the module.
            let mut keyed: Vec<u64> = ids
                .iter()
                .enumerate()
                .filter_map(|(i, &doc)| self.node_of(doc).map(|n| (n as u64) << 32 | i as u64))
                .filter(|&w| !self.is_deleted((w >> 32) as u32))
                .collect();
            keyed.sort_unstable();
            let pairs: Vec<(u32, DocId)> = keyed
                .iter()
                .map(|&w| ((w >> 32) as u32, ids[(w & 0xFFFF_FFFF) as usize]))
                .collect();
            return self.nearest_docs(&q, &pairs, k);
        }
        let nodes: Vec<u32> = ids
            .iter()
            .filter_map(|doc| self.node_of(*doc))
            .filter(|n| !self.is_deleted(*n))
            .collect();
        self.nearest_of(&q, &nodes, k)
            .into_iter()
            .map(|c| {
                (
                    self.doc_ids[c.node as usize],
                    score_from_distance(self.spec.metric, c.dist),
                )
            })
            .collect()
    }

    /// Roughly the number of distance computations an ANN search will perform.
    ///
    /// Beam search keeps `ef` candidates and measures the level-0 neighbours
    /// of each one (at most `m0`). When a filter set is smaller than that,
    /// scanning it directly is both cheaper and exact; the threshold is
    /// therefore derived from the index's own parameters, not hand-picked.
    pub fn probe_budget(&self, ef: Option<usize>) -> usize {
        ef.unwrap_or(self.spec.ef_search).saturating_mul(self.m0)
    }

    pub fn search_exact<F>(&self, query: &[f32], k: usize, accept: F) -> Vec<(DocId, f32)>
    where
        F: Fn(DocId) -> bool,
    {
        let q = self.query_for(query);
        // The test runs here, in turn: `accept` may read the documents
        // through a filter that is not the threads' to share.
        if !self.aliases.is_empty() {
            let mut pairs = Vec::new();
            let mut docs = Vec::new();
            for node in (0..self.doc_ids.len() as u32).filter(|n| !self.is_deleted(*n)) {
                self.docs_of(node, &mut docs);
                pairs.extend(docs.iter().filter(|&&d| accept(d)).map(|&d| (node, d)));
            }
            return self.nearest_docs(&q, &pairs, k);
        }
        let nodes: Vec<u32> = (0..self.doc_ids.len() as u32)
            .filter(|n| !self.is_deleted(*n))
            .filter(|n| accept(self.doc_ids[*n as usize]))
            .collect();
        self.nearest_of(&q, &nodes, k)
            .into_iter()
            .map(|c| {
                (
                    self.doc_ids[c.node as usize],
                    score_from_distance(self.spec.metric, c.dist),
                )
            })
            .collect()
    }
}

#[cfg(not(feature = "vector"))]
pub use crate::off::VectorIndex;

/// A hash of the bits an index would store `raw` as -- unit length under
/// cosine, halved for an `f16` field -- which two vectors the index holds
/// as one node ([`VectorIndex::place`]) share: for a build without the
/// graph to count the nodes a bit index would hold. Hashed, they are told
/// apart through the `u64` sort the module has, where a set of the bits
/// was another copy of the hash table, 2.5 KB of it.
#[cfg(not(feature = "vector"))]
pub(crate) fn stored_hash(metric: Metric, prec: VecPrec, raw: &[f32]) -> u64 {
    let inv = match metric {
        Metric::Cosine => unit_scale(flat_sq(raw)),
        _ => 1.0,
    };
    match prec {
        VecPrec::F32 if inv != 1.0 => hash_words(raw.iter().map(|x| (x * inv).to_bits())),
        VecPrec::F32 => hash_words(raw.iter().map(|x| x.to_bits())),
        VecPrec::F16 => hash_words(
            raw.iter()
                .map(|x| crate::codec::f16_from_f32(x * inv) as u32),
        ),
    }
}

/// [`flat_sq`] of four vectors of one length, side by side, each in its
/// own order: the same bits, four chains of adds in flight rather than
/// one. Zipped rather than indexed, so no read is checked.
#[cfg(not(feature = "vector"))]
fn flat_sqs4(a: &[f32], b: &[f32], c: &[f32], d: &[f32]) -> [f32; 4] {
    let mut s = [0.0f32; 4];
    for (((w, x), y), z) in a.iter().zip(b).zip(c).zip(d) {
        s = [s[0] + w * w, s[1] + x * x, s[2] + y * y, s[3] + z * z];
    }
    s
}

/// `near` in a build without the graph: what [`VectorIndex::search_exact`]
/// answers over an arena holding the vectors `read` gives for `ids`, each
/// as the index would store it -- made a unit one for cosine as
/// `Arena::push` makes it, halved where the field is `f16` -- and measured
/// by the arena's own kernel, the nearest kept in the order [`nearest`]
/// keeps them. So the rows, their scores to the bit and the order of their ties are
/// those of the full build's `near ... exact`, ties in id order where the
/// arena holds the nodes in id order: written in id order and never
/// written again, which a rebuild at an open makes so. Nothing is held
/// but twice the page: an arena of 50 000 x 384 would have been 77 MB of
/// a module's memory, which it never gives back.
///
/// Under cosine a vector's length is a flat sum, each add waiting on the
/// one before ([`flat_sq`]): one at a time it was 40% of a scan of 10 000
/// x 128 in the browser module, so four are read and summed side by side,
/// 4.31 -> 3.29 ms there and 11.8 -> 8.7 at 384 dimensions, for 241 bytes
/// brotli. Eight indexed as a restore sums them natively ([`flat_sqs8`])
/// gained nothing in the module.
#[cfg(not(feature = "vector"))]
pub(crate) fn search_stored(
    metric: Metric,
    prec: VecPrec,
    query: &[f32],
    k: usize,
    ids: &[DocId],
    read: &mut dyn FnMut(DocId, &mut Vec<f32>) -> crate::error::Result<bool>,
) -> crate::error::Result<Vec<(DocId, f32)>> {
    if k == 0 {
        return Ok(Vec::new());
    }
    let q = match metric {
        Metric::Cosine => normalized(query),
        _ => query.to_vec(),
    };
    let dim = q.len();
    let mut halves = Vec::new();
    let mut measure = |raw: &mut [f32], inv: f32| match prec {
        VecPrec::F32 => {
            // Scaled where it was read, as `Arena::push_scaled` scales it
            // as it copies.
            if inv != 1.0 {
                raw.iter_mut().for_each(|x| *x *= inv);
            }
            distance(metric, &q, raw)
        }
        VecPrec::F16 => {
            halves.clear();
            halves.extend(raw.iter().map(|x| crate::codec::f16_from_f32(x * inv)));
            distance_hf(metric, &halves, &q)
        }
    };
    let unit = metric == Metric::Cosine;
    // Up to four vectors read, waiting for their lengths, and where each is
    // among `ids`: one place measures them, the kernels inlined once.
    let (mut four, mut at) = (Vec::with_capacity(4 * dim), [0u32; 4]);
    let mut raw = Vec::new();
    // Distances held negated, so the engine's one sort puts the nearest
    // first and a tie the lower id first -- the order the arena's stable
    // sort leaves nodes written in id order -- cut back to the `k` nearest
    // whenever there are twice as many, as `order_exactly` keeps them: a
    // sort of the arena's candidates of its own was 1.1 KB brotli here.
    let (mut out, mut worst) = (Vec::new(), f32::INFINITY);
    let mut each = ids.iter().enumerate();
    loop {
        let next = each.next();
        if let Some((i, &id)) = next {
            if !read(id, &mut raw)? || raw.len() != dim {
                continue;
            }
            at[four.len() / dim.max(1)] = i as u32;
            four.extend_from_slice(&raw);
            if four.len() < 4 * dim {
                continue;
            }
        }
        let n = four.len() / dim.max(1);
        let sq = match (unit, n) {
            (false, _) => [1.0; 4],
            (true, 4) => {
                let (ab, cd) = four.split_at(2 * dim);
                let ((a, b), (c, d)) = (ab.split_at(dim), cd.split_at(dim));
                flat_sqs4(a, b, c, d)
            }
            (true, _) => std::array::from_fn(|j| match four.get(j * dim..(j + 1) * dim) {
                Some(v) => flat_sq(v),
                None => 1.0,
            }),
        };
        for ((v, sq), i) in four.chunks_mut(dim.max(1)).zip(sq).zip(at) {
            let inv = if unit { unit_scale(sq) } else { 1.0 };
            let dist = measure(v, inv);
            if dist > worst {
                continue;
            }
            out.push((ids[i as usize], -dist));
            if out.len() == k || out.len() == 2 * k {
                out.sort_by(crate::text::best_first);
                out.truncate(k);
                worst = -out[k - 1].1;
            }
        }
        four.clear();
        if next.is_none() {
            break;
        }
    }
    out.sort_by(crate::text::best_first);
    out.truncate(k);
    for hit in &mut out {
        hit.1 = score_from_distance(metric, -hit.1);
    }
    Ok(out)
}

#[cfg(all(test, feature = "vector"))]
mod tests {

    /// The branchless widening in the hot loop must match codec's exact
    /// conversion bit for bit -- except Inf/NaN, absent from vector data.
    #[test]
    fn fast_half_matches_exact() {
        for bits in 0u32..=0xffff {
            let h = bits as u16;
            let exact = crate::codec::f32_from_f16(h);
            if exact.is_nan() || exact.is_infinite() {
                continue;
            }
            let fast = half(h);
            assert_eq!(
                fast.to_bits(),
                exact.to_bits(),
                "0x{h:04x}: fast {fast} != exact {exact}"
            );
        }
    }
    use super::*;
    use std::collections::HashMap;

    fn spec() -> VectorIndexSpec {
        VectorIndexSpec {
            metric: Metric::L2,
            m: 8,
            ef_construction: 64,
            ef_search: 64,
            quant: Quant::None,
        }
    }

    #[test]
    fn finds_nearest() {
        let mut ix = VectorIndex::new(2, spec());
        for i in 0..200u64 {
            ix.insert(i, &[i as f32, 0.0]);
        }
        let r = ix.search(&[10.2, 0.0], 3, None, |_| true);
        assert_eq!(r[0].0, 10);
        assert_eq!(ix.len(), 200);
    }

    /// The int8 kernels -- NEON's own on aarch64 -- give the scalar strips'
    /// result bit for bit, at every length and through the tail, so a graph
    /// over codes built natively is the graph the browser builds.
    #[test]
    fn int8_kernels_match_the_scalar_strips() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for len in (0..70).chain([767, 768, 1536]) {
            for _ in 0..50 {
                let code: Vec<i8> = (0..len).map(|_| next() as i8).collect();
                let q: Vec<f32> = (0..len)
                    .map(|_| (next() % 20_001) as f32 / 10_000.0 - 1.0)
                    .collect();
                let scale = (next() % 1000 + 1) as f32 / 97_000.0;
                let dot = strip8!(&code[..], &q[..], |c: i8| c as f32, ident, mul, 0);
                let l2 = strip8!(
                    &code[..],
                    &q[..],
                    |c: i8| c as f32 * scale,
                    ident,
                    diff_sq,
                    0
                );
                assert_eq!(dot_i8(&code, &q).to_bits(), dot.to_bits(), "dot, {len}");
                assert_eq!(l2_i8(&code, &q, scale).to_bits(), l2.to_bits(), "l2, {len}");
            }
        }
    }

    /// `dot`, `l2_sq` and the half-precision distances add as `strip8!`
    /// adds, bit for bit: aarch64 writes them out, and a graph has to be the
    /// same graph on every target, the browser's included. Signed zeros,
    /// subnormals, values near the ends of the range and every f16 bit
    /// pattern go through them.
    #[test]
    fn f32_and_f16_kernels_match_the_scalar_strips() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let odd = [0.0, -0.0, 1e-40, -1e-40, 3.0e38, -65504.0, 1e-8, 1.0];
        let same = |a: f32, b: f32| a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan());
        for len in (0..70).chain([767, 768, 1536]) {
            for _ in 0..30 {
                let mut f = |_| match next() % 16 {
                    0 => odd[(next() % odd.len() as u64) as usize],
                    _ => (next() % 20_001) as f32 / 10_000.0 - 1.0,
                };
                let a: Vec<f32> = (0..len).map(&mut f).collect();
                let b: Vec<f32> = (0..len).map(&mut f).collect();
                let (h, g): (Vec<u16>, Vec<u16>) =
                    (0..len).map(|_| (next() as u16, next() as u16)).unzip();
                let dot_ab = strip8!(&a[..], &b[..], ident, ident, mul, 0);
                let l2_ab = strip8!(&a[..], &b[..], ident, ident, diff_sq, 0);
                assert!(same(dot(&a, &b), dot_ab), "dot, {len}");
                assert!(same(l2_sq(&a, &b), l2_ab), "l2, {len}");
                let dot_hb = strip8!(&h[..], &b[..], half, ident, mul, 0);
                let l2_hb = strip8!(&h[..], &b[..], half, ident, diff_sq, 0);
                let dot_hg = strip8!(&h[..], &g[..], half, half, mul, 0);
                let l2_hg = strip8!(&h[..], &g[..], half, half, diff_sq, 0);
                for (metric, want_hb, want_hg) in [
                    (Metric::Cosine, 1.0 - dot_hb, 1.0 - dot_hg),
                    (Metric::L2, l2_hb, l2_hg),
                    (Metric::Dot, -dot_hb, -dot_hg),
                ] {
                    assert!(
                        same(distance_hf(metric, &h, &b), want_hb),
                        "{metric:?} hf, {len}"
                    );
                    assert!(
                        same(distance_hh(metric, &h, &g), want_hg),
                        "{metric:?} hh, {len}"
                    );
                }
            }
        }
    }

    /// Four vectors measured together give each the bits it gets alone:
    /// the query's side first or the vector's, as each kernel takes it, and
    /// every length the strips and their tails cover.
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    #[test]
    fn four_at_a_time_is_one_at_a_time() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let odd = [0.0, -0.0, 1e-40, -1e-40, 3.0e38, -65504.0, 1e-8, 1.0];
        let same = |a: f32, b: f32| a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan());
        for len in (0..70).chain([767, 768, 1536]) {
            for _ in 0..10 {
                let mut f = |_| match next() % 16 {
                    0 => odd[(next() % odd.len() as u64) as usize],
                    _ => (next() % 20_001) as f32 / 10_000.0 - 1.0,
                };
                let q: Vec<f32> = (0..len).map(&mut f).collect();
                let v: Vec<Vec<f32>> = (0..4).map(|_| (0..len).map(&mut f).collect()).collect();
                let h: Vec<Vec<u16>> = (0..4)
                    .map(|_| (0..len).map(|_| next() as u16).collect())
                    .collect();
                let t = [&v[0][..], &v[1][..], &v[2][..], &v[3][..]];
                let th = [&h[0][..], &h[1][..], &h[2][..], &h[3][..]];
                for metric in [Metric::Cosine, Metric::L2, Metric::Dot] {
                    let (four, halves) = (distances4(metric, &q, t), distances4_hf(metric, &q, th));
                    for j in 0..4 {
                        let one = distance(metric, &q, t[j]);
                        assert!(same(four[j], one), "{metric:?} f32, {len}");
                        let one = distance_hf(metric, th[j], &q);
                        assert!(same(halves[j], one), "{metric:?} f16, {len}");
                    }
                }
            }
        }
    }

    /// `dists_to` over every kind of arena, and lists of every length past
    /// the last four, is `dist_to` node by node.
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    #[test]
    fn an_arena_measures_a_list_as_it_measures_each() {
        let mut rng = Rng(5);
        for (quant, prec, metric) in [
            (Quant::None, VecPrec::F32, Metric::Cosine),
            (Quant::None, VecPrec::F32, Metric::L2),
            (Quant::None, VecPrec::F16, Metric::Dot),
            (Quant::None, VecPrec::F16, Metric::L2),
            (Quant::Int8, VecPrec::F32, Metric::Cosine),
        ] {
            let dim = 19;
            let spec = VectorIndexSpec {
                metric,
                quant,
                m: 6,
                ef_construction: 32,
                ..VectorIndexSpec::default()
            };
            let mut ix = VectorIndex::with_precision(dim, spec, prec);
            for i in 0..40u64 {
                let v: Vec<f32> = (0..dim).map(|_| rng.next_f32() - 0.5).collect();
                ix.insert(i, &v);
            }
            let raw: Vec<f32> = (0..dim).map(|_| rng.next_f32() - 0.5).collect();
            let q = ix.query_for(&raw);
            let mut out = Vec::new();
            for n in 0..14u32 {
                let nodes: Vec<u32> = (0..n).map(|i| (i * 7 + n) % 40).collect();
                // Asked ahead or not, the same distances.
                let mut ahead = Vec::new();
                ix.data
                    .dists_to::<true>(metric, &q, &nodes, dim, &mut ahead);
                ix.data.dists_to::<false>(metric, &q, &nodes, dim, &mut out);
                assert_eq!(out.len(), nodes.len());
                assert_eq!(ahead, out);
                for (&node, &d) in nodes.iter().zip(&out) {
                    let one = ix.data.dist_to(metric, &q, node, dim);
                    assert_eq!(d.to_bits(), one.to_bits(), "{quant:?} {prec:?} {metric:?}");
                }
            }
        }
    }

    /// The walk measures its neighbours four at a time and the exact search
    /// every vector: a document both find carries the same score from
    /// each, to the bit.
    #[test]
    fn the_walk_scores_as_the_exact_search_does() {
        let mut rng = Rng(17);
        for prec in [VecPrec::F32, VecPrec::F16] {
            let dim = 21;
            let items: Vec<(u64, Vec<f32>)> = (0..3000u64)
                .map(|i| (i, (0..dim).map(|_| rng.next_f32() - 0.5).collect()))
                .collect();
            let mut ix = VectorIndex::with_precision(dim, VectorIndexSpec::default(), prec);
            ix.insert_batch(&items);
            let mut shared = 0;
            for (_, q) in items.iter().step_by(150) {
                let walk = ix.search(q, 10, None, |_| true);
                let exact = ix.search_exact(q, 10, |_| true);
                for (doc, score) in &walk {
                    if let Some((_, e)) = exact.iter().find(|(d, _)| d == doc) {
                        assert_eq!(score.to_bits(), e.to_bits(), "{prec:?}, doc {doc}");
                        shared += 1;
                    }
                }
            }
            assert!(shared > 100, "{prec:?}: only {shared} documents both found");
        }
    }

    /// A restore normalises the documents' vectors eight at a time, a share
    /// of nodes to a thread: the arena it ends with holds the bits the one it
    /// restored held, over tombstones between them and on either side of a
    /// share's edge, and a last batch short of eight.
    #[test]
    fn a_restored_arena_holds_the_vectors_it_had_bit_for_bit() {
        let mut rng = Rng(29);
        for prec in [VecPrec::F32, VecPrec::F16] {
            let dim = 13;
            let spec = VectorIndexSpec {
                m: 6,
                ef_construction: 32,
                ..VectorIndexSpec::default()
            };
            let mut ix = VectorIndex::with_precision(dim, spec, prec);
            let mut held = HashMap::new();
            // Three shares of the fill, the last of 45 nodes.
            let edge = FILL_SHARE as u64;
            let items: Vec<(u64, Vec<f32>)> = (0..2 * edge + 45)
                .map(|i| (i, (0..dim).map(|_| rng.next_f32() * 3.0 - 1.5).collect()))
                .collect();
            ix.insert_batch(&items);
            // What a restore reads is the document's record, which holds
            // an f16 field's halves.
            let stored = |v: Vec<f32>| match prec {
                VecPrec::F32 => v,
                VecPrec::F16 => halved(&v),
            };
            held.extend(items.into_iter().map(|(d, v)| (d, stored(v))));
            // Written again with another vector, and deleted: tombstones
            // carrying their own vectors among the live nodes, on either
            // side of a share's edge too.
            for i in [3u64, 11, 12, 30, edge - 1, edge, 2 * edge + 7] {
                let v: Vec<f32> = (0..dim).map(|_| rng.next_f32() - 0.5).collect();
                ix.insert(i, &v);
                held.insert(i, stored(v));
            }
            for i in [7u64, 20, edge + 1] {
                ix.remove(i);
                held.remove(&i);
            }
            let bytes = ix.serialize_graph();
            let back = VectorIndex::restore_graph(&bytes, dim, prec, |doc, out| {
                out.clear();
                out.extend_from_slice(held.get(&doc).map_or(&[][..], |v| &v[..]));
                held.contains_key(&doc)
            })
            .expect("the graph restores");
            assert_eq!(back.doc_ids.len(), ix.doc_ids.len());
            let (mut a, mut b) = (Vec::new(), Vec::new());
            for node in 0..ix.doc_ids.len() as u32 {
                a.clear();
                b.clear();
                ix.data.write_stored(node, dim, &mut a);
                back.data.write_stored(node, dim, &mut b);
                assert_eq!(a, b, "{prec:?}, node {node}");
            }
        }
    }

    /// A graph written flat restores as the varint layout's record of it
    /// did -- links, levels, tombstones and their vectors, nodes waiting to
    /// be linked, every kind of arena -- and writes back the bytes it was
    /// read from. A link naming no node, or a record cut short, is refused.
    #[test]
    fn a_flat_graph_record_restores_as_the_varint_one_did() {
        let mut rng = Rng(41);
        let cases = [
            (Quant::None, VecPrec::F32, Metric::Cosine, 1500),
            (Quant::None, VecPrec::F16, Metric::L2, 1500),
            (Quant::Int8, VecPrec::F32, Metric::Cosine, 1500),
            (Quant::Bit, VecPrec::F32, Metric::Cosine, 2200),
        ];
        for (quant, prec, metric, n) in cases {
            let dim = 12;
            let spec = VectorIndexSpec {
                metric,
                quant,
                m: 6,
                ef_construction: 24,
                ..VectorIndexSpec::default()
            };
            let mut ix = VectorIndex::with_precision(dim, spec, prec);
            let mut held = HashMap::new();
            let vector =
                |rng: &mut Rng| -> Vec<f32> { (0..dim).map(|_| rng.next_f32() - 0.5).collect() };
            let items: Vec<(u64, Vec<f32>)> =
                (0..n as u64).map(|i| (i, vector(&mut rng))).collect();
            ix.insert_batch(&items);
            held.extend(items);
            // Tombstones: rewritten and deleted documents.
            for i in (0..n as u64).step_by(97) {
                let v = vector(&mut rng);
                ix.insert(i, &v);
                held.insert(i, v);
            }
            for i in (5..n as u64).step_by(131) {
                ix.remove(i);
                held.remove(&i);
            }
            // Nodes waiting to be linked.
            let late: Vec<(u64, Vec<f32>)> = (n as u64..n as u64 + 30)
                .map(|i| (i, vector(&mut rng)))
                .collect();
            ix.defer_batch(&late);
            held.extend(late);
            let lookup = |doc: u64, out: &mut Vec<f32>| {
                out.clear();
                out.extend_from_slice(held.get(&doc).map_or(&[][..], |v| &v[..]));
                held.contains_key(&doc)
            };
            for kept in [false, true] {
                let what = format!("{quant:?} {prec:?} {metric:?} kept {kept}");
                let old = VectorIndex::restore_graph(&ix.serialize_varint(kept), dim, prec, lookup)
                    .expect(&what);
                let flat = ix.serialize(kept);
                let new = VectorIndex::restore_graph(&flat, dim, prec, lookup).expect(&what);
                assert!(
                    new.serialize(kept) == flat,
                    "{what}: written back otherwise"
                );
                assert!(
                    new.serialize(kept) == old.serialize(kept),
                    "{what}: restored otherwise"
                );
                assert_eq!(new.pending, old.pending, "{what}");
                assert_eq!(new.by_doc, old.by_doc, "{what}");
                assert_eq!(new.deleted_count, old.deleted_count, "{what}");
                let (mut a, mut b) = (Vec::new(), Vec::new());
                for node in 0..new.doc_ids.len() as u32 {
                    a.clear();
                    b.clear();
                    new.data.write_stored(node, dim, &mut a);
                    old.data.write_stored(node, dim, &mut b);
                    assert!(a == b, "{what}: node {node}'s vector");
                }
                // The record cut short, and a link naming a node past the
                // last.
                let cut = &flat[..flat.len() - 1];
                assert!(
                    VectorIndex::restore_graph(cut, dim, prec, lookup).is_none(),
                    "{what}: cut short"
                );
                // A document of the second share without its vector: the
                // shares that did fill are let go, none of their slots read.
                let gone = (FILL_SHARE..new.doc_ids.len())
                    .find(|&n| !new.is_deleted(n as u32))
                    .map(|n| new.doc_ids[n])
                    .expect("a live node past the first share");
                let without = |doc: u64, out: &mut Vec<f32>| doc != gone && lookup(doc, out);
                assert!(
                    VectorIndex::restore_graph(&flat, dim, prec, without).is_none(),
                    "{what}: a vector gone"
                );
                // Two live nodes of one document: as many nodes as
                // documents, and one of them left out of the graph.
                let mut twice = VectorIndex::restore_graph(&flat, dim, prec, lookup).expect(&what);
                let live: Vec<usize> = (0..twice.doc_ids.len())
                    .filter(|&n| !twice.is_deleted(n as u32))
                    .take(2)
                    .collect();
                twice.doc_ids[live[1]] = twice.doc_ids[live[0]];
                for bytes in [twice.serialize(kept), twice.serialize_varint(kept)] {
                    assert!(
                        VectorIndex::restore_graph(&bytes, dim, prec, lookup).is_none(),
                        "{what}: a document twice"
                    );
                }
                let mut bad = new;
                bad.l0[0] = bad.doc_ids.len() as u32;
                let bad = bad.serialize(kept);
                assert!(
                    VectorIndex::restore_graph(&bad, dim, prec, lookup).is_none(),
                    "{what}: bad link"
                );
            }
        }
    }

    /// If a vector carrying NaN enters the index the distance becomes NaN too.
    /// When the ordering is not a total order `sort` notices and panics -- as
    /// it once did. The result may be meaningless, but the process must live.
    #[test]
    fn non_finite_distances_do_not_break_the_sort() {
        let mut ix = VectorIndex::new(2, spec());
        for i in 0..200u64 {
            ix.insert(i, &[i as f32, 0.0]);
        }
        ix.insert(200, &[f32::NAN, 0.0]);
        ix.insert(201, &[f32::INFINITY, 0.0]);
        ix.insert(202, &[0.0, f32::NEG_INFINITY]);

        // Both the ANN and the exact path sort; neither may panic.
        let r = ix.search(&[10.2, 0.0], 5, None, |_| true);
        assert!(!r.is_empty());
        let r = ix.search_exact(&[10.2, 0.0], 5, |_| true);
        assert!(!r.is_empty());
        // A finite query must still find its finite neighbour.
        assert_eq!(r[0].0, 10);

        // It must not go down even when the query itself is NaN.
        let _ = ix.search(&[f32::NAN, 0.0], 5, None, |_| true);
        let _ = ix.search_exact(&[f32::NAN, 0.0], 5, |_| true);

        // The batch build path is separate code: it sorts as well.
        let items: Vec<(u64, Vec<f32>)> = (0..2000u64)
            .map(|i| {
                let x = if i % 500 == 0 { f32::NAN } else { i as f32 };
                (i, vec![x, 0.0])
            })
            .collect();
        let mut batch = VectorIndex::new(2, spec());
        batch.insert_batch(&items);
        assert_eq!(batch.len(), 2000);
    }

    #[test]
    fn recall_against_bruteforce() {
        let mut ix = VectorIndex::new(16, VectorIndexSpec::default());
        let mut rng = Rng(42);
        let mut vecs = Vec::new();
        for i in 0..500u64 {
            let v: Vec<f32> = (0..16).map(|_| rng.next_f32() - 0.5).collect();
            ix.insert(i, &v);
            vecs.push(v);
        }
        let q = &vecs[123];
        let approx: Vec<u64> = ix
            .search(q, 10, Some(128), |_| true)
            .into_iter()
            .map(|x| x.0)
            .collect();
        let exact: Vec<u64> = ix
            .search_exact(q, 10, |_| true)
            .into_iter()
            .map(|x| x.0)
            .collect();
        let hit = approx.iter().filter(|d| exact.contains(d)).count();
        assert!(
            hit >= 8,
            "recall too low: {hit}/10 ({approx:?} vs {exact:?})"
        );
    }

    #[test]
    fn batch_insert_matches_sequential_quality() {
        let mut rng = Rng(7);
        let dim = 32;
        let items: Vec<(u64, Vec<f32>)> = (0..5000u64)
            .map(|i| (i, (0..dim).map(|_| rng.next_f32() - 0.5).collect()))
            .collect();

        let mut seq = VectorIndex::new(dim, VectorIndexSpec::default());
        for (d, v) in &items {
            seq.insert(*d, v);
        }
        let mut par = VectorIndex::new(dim, VectorIndexSpec::default());
        par.insert_batch(&items);

        assert_eq!(seq.len(), par.len());

        // The parallel graph may differ from the serial one; what matters is
        // that recall against the exact scan is preserved.
        let mut seq_hits = 0usize;
        let mut par_hits = 0usize;
        let mut total = 0usize;
        for (_, q) in items.iter().step_by(250) {
            let exact: Vec<u64> = seq
                .search_exact(q, 10, |_| true)
                .into_iter()
                .map(|x| x.0)
                .collect();
            let a: Vec<u64> = seq
                .search(q, 10, None, |_| true)
                .into_iter()
                .map(|x| x.0)
                .collect();
            let b: Vec<u64> = par
                .search(q, 10, None, |_| true)
                .into_iter()
                .map(|x| x.0)
                .collect();
            seq_hits += a.iter().filter(|d| exact.contains(d)).count();
            par_hits += b.iter().filter(|d| exact.contains(d)).count();
            total += exact.len();
        }
        let seq_r = seq_hits as f64 / total as f64;
        let par_r = par_hits as f64 / total as f64;
        assert!(
            par_r >= seq_r - 0.05,
            "parallel recall dropped too far: serial {seq_r:.3} vs parallel {par_r:.3}"
        );
    }

    #[test]
    fn batch_insert_is_deterministic() {
        let mut rng = Rng(11);
        let dim = 16;
        let items: Vec<(u64, Vec<f32>)> = (0..3000u64)
            .map(|i| (i, (0..dim).map(|_| rng.next_f32() - 0.5).collect()))
            .collect();
        let build = || {
            let mut ix = VectorIndex::new(dim, VectorIndexSpec::default());
            ix.insert_batch(&items);
            ix
        };
        let a = build();
        let b = build();
        for (_, q) in items.iter().step_by(300) {
            let ra: Vec<u64> = a
                .search(q, 10, None, |_| true)
                .into_iter()
                .map(|x| x.0)
                .collect();
            let rb: Vec<u64> = b
                .search(q, 10, None, |_| true)
                .into_iter()
                .map(|x| x.0)
                .collect();
            assert_eq!(ra, rb, "the parallel build is not deterministic");
        }
    }

    /// The nearest `k` kept as they come are the first `k` a stable sort
    /// leaves -- ties in the order they came -- both below the count kept
    /// that way and above it.
    #[test]
    fn nearest_keeps_what_a_stable_sort_keeps() {
        let mut rng = Rng(31);
        for n in [0usize, 1, 7, 200, 1000] {
            // Few distances, so most of them tie.
            let all: Vec<Cand> = (0..n as u32)
                .map(|node| Cand {
                    dist: (rng.next_f32() * 8.0).floor(),
                    node,
                })
                .collect();
            for k in [0, 1, 5, NEAREST_KEPT, NEAREST_KEPT + 1, 300] {
                let mut sorted = all.clone();
                sorted.sort();
                sorted.truncate(k);
                let kept = nearest(all.clone(), k);
                let pairs = |v: &[Cand]| {
                    v.iter()
                        .map(|c| (c.dist.to_bits(), c.node))
                        .collect::<Vec<_>>()
                };
                assert_eq!(pairs(&kept), pairs(&sorted), "n {n}, k {k}");
            }
        }
    }

    /// An exact search spread over the cores in shares keeps the rows, and
    /// the order of their ties, one walk over the nodes keeps: the vectors
    /// repeat, so equal distances fall on both sides of a share's edge.
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn the_shares_keep_what_one_walk_keeps() {
        let n = MEASURE_APART + 3 * MEASURE_SHARE + 17;
        let mut rng = Rng(37);
        let few: Vec<Vec<f32>> = (0..24)
            .map(|_| (0..4).map(|_| rng.next_f32() - 0.5).collect())
            .collect();
        let items: Vec<(u64, Vec<f32>)> = (0..n as u64)
            .map(|i| (i, few[(i * 7 % 24) as usize].clone()))
            .collect();
        let mut ix = VectorIndex::new(4, spec());
        // Measured, never walked: no graph is needed. A node each, the
        // vectors repeated for the ties, which `place` would make one node.
        for (doc, v) in &items {
            let node = ix.alloc_node(*doc, v, 0);
            ix.by_doc.insert(*doc, node + 1);
            ix.pending.push(node);
        }
        let q = ix.query_for(&[0.1, -0.2, 0.3, 0.05]);
        let every: Vec<u32> = (0..n as u32).collect();
        let odd: Vec<u32> = (0..n as u32).filter(|x| x % 3 != 1).rev().collect();
        for nodes in [&every, &odd] {
            for k in [1, 10, NEAREST_KEPT + 5] {
                let spread = ix.nearest_of(&q, nodes, k);
                let walked = nearest(ix.measure(&q, nodes.iter().copied()), k);
                let pairs = |v: &[Cand]| {
                    v.iter()
                        .map(|c| (c.dist.to_bits(), c.node))
                        .collect::<Vec<_>>()
                };
                assert_eq!(pairs(&spread), pairs(&walked), "k {k}");
            }
        }
    }

    /// A graph record's lists written a part at a time on every core are
    /// the lists written in turn, byte for byte.
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn lists_written_in_parts_are_the_lists_written_in_turn() {
        // Past two parts, the last cut short; the levels above come behind
        // level 0's, each part's in node order.
        let n = 2 * LIST_PART + 1_000;
        let mut rng = Rng(29);
        let items: Vec<(u64, Vec<f32>)> = (0..n as u64)
            .map(|i| (i, (0..4).map(|_| rng.next_f32()).collect()))
            .collect();
        // Narrow lists and a narrow beam: the test runs unoptimised.
        let spec = VectorIndexSpec {
            m: 4,
            ef_construction: 16,
            ..spec()
        };
        let mut ix = VectorIndex::new(4, spec);
        ix.insert_batch(&items);
        let lw = link_width(n);
        let mut parts = Vec::new();
        ix.put_all_lists(n, lw, &mut parts);
        let (mut turn, mut sorted) = (Vec::new(), Vec::new());
        ix.put_lists(0..n as u32, true, lw, &mut sorted, &mut turn);
        ix.put_lists(0..n as u32, false, lw, &mut sorted, &mut turn);
        assert_eq!(parts.len(), turn.len());
        assert!(parts == turn, "the lists written in parts differ");
    }

    /// The lists a batch joins are pruned in parallel, grouped by list, and
    /// must end as linking the batch one node after another left them: the
    /// same graph record, byte for byte, over every kind of arena -- a
    /// code's offset included.
    #[test]
    fn a_batch_links_as_its_nodes_would_one_after_another() {
        // Past the 1 024 nodes a batch needs, and bit codes past the 2 048
        // vectors they learn their centres from.
        let cases = [
            (8, Metric::Cosine, Quant::None, VecPrec::F32, 2000),
            (8, Metric::L2, Quant::None, VecPrec::F32, 2000),
            (8, Metric::Dot, Quant::None, VecPrec::F16, 2000),
            (8, Metric::Cosine, Quant::Int8, VecPrec::F32, 2000),
            (16, Metric::Cosine, Quant::Bit, VecPrec::F32, 2400),
        ];
        for (dim, metric, quant, prec, n) in cases {
            let mut rng = Rng(23);
            let centres: Vec<Vec<f32>> = (0..8)
                .map(|_| (0..dim).map(|_| rng.next_f32() - 0.5).collect())
                .collect();
            let items: Vec<(u64, Vec<f32>)> = (0..n as u64)
                .map(|i| {
                    let c = &centres[i as usize % centres.len()];
                    (
                        i,
                        c.iter().map(|x| x + 0.2 * (rng.next_f32() - 0.5)).collect(),
                    )
                })
                .collect();
            // Narrow lists and a narrow beam: what is held to is how lists
            // end, which fill the sooner, and the test runs unoptimised.
            let spec = VectorIndexSpec {
                metric,
                quant,
                m: 6,
                ef_construction: 32,
                ..VectorIndexSpec::default()
            };
            let build = |link: fn(&mut VectorIndex, Candidates, usize)| {
                let mut ix = VectorIndex::with_precision(dim, spec, prec);
                // Two calls, so the second starts over a graph of its own.
                ix.insert_batch_by(&items[..n / 3], link);
                ix.insert_batch_by(&items[n / 3..], link);
                ix
            };
            let (a, b) = (
                build(VectorIndex::link_batch),
                build(VectorIndex::link_serially),
            );
            assert!(
                a.l0_len.iter().any(|&k| k as usize == a.m0),
                "no list filled up"
            );
            assert!(
                a.serialize_graph() == b.serialize_graph(),
                "{metric:?} {quant:?} {prec:?}: the batch linked otherwise"
            );
        }
    }

    /// 17 vectors written 200 times each among 2 000 others, in batches
    /// and one at a time: every copy is found by a search for its vector,
    /// each is one node's document, and a query of the others finds what
    /// the exact scan finds. A node each, most copies were found by no
    /// search and the other queries lost a third of their recall.
    #[test]
    fn the_graph_reaches_every_copy_of_a_vector_written_many_times() {
        let mut rng = Rng(5);
        let pool: Vec<Vec<f32>> = (0..17)
            .map(|_| (0..16).map(|_| rng.next_f32() - 0.5).collect())
            .collect();
        let mut items: Vec<(u64, Vec<f32>)> = Vec::new();
        for i in 0..5_400u64 {
            let v = match i % 8 < 5 {
                true => pool[(i % 17) as usize].clone(),
                false => (0..16).map(|_| rng.next_f32() - 0.5).collect(),
            };
            items.push((i, v));
        }
        let mut ix = VectorIndex::new(16, spec());
        let (head, tail) = items.split_at(1_000);
        for (d, v) in head {
            ix.insert(*d, v);
        }
        ix.insert_batch(tail);
        assert_eq!(ix.len(), items.len());
        let distinct = items.len() - items.iter().filter(|(i, _)| i % 8 < 5).count() + 17;
        assert_eq!(ix.doc_ids.len(), distinct, "a node a vector");
        for (k, v) in pool.iter().enumerate() {
            let copies: Vec<u64> = items
                .iter()
                .filter(|(i, w)| i % 8 < 5 && w == v)
                .map(|p| p.0)
                .collect();
            let mut found: Vec<u64> = ix
                .search(v, copies.len(), None, |_| true)
                .into_iter()
                .map(|x| x.0)
                .collect();
            found.sort_unstable();
            assert_eq!(found, copies, "vector {k}");
        }
        let queries: Vec<(u64, Vec<f32>)> = (0..50)
            .map(|i| (i, (0..16).map(|_| rng.next_f32() - 0.5).collect()))
            .collect();
        let mut hit = 0;
        for (_, q) in &queries {
            let exact: Vec<f32> = ix
                .search_exact(q, 10, |_| true)
                .iter()
                .map(|x| x.1)
                .collect();
            let walked = ix.search(q, 10, Some(100), |_| true);
            // L2: the score is the distance, the tenth the farthest kept.
            let reach = exact[9] + 1e-6;
            hit += walked.iter().filter(|x| x.1 <= reach).count();
        }
        assert!(hit >= 490, "recall {hit} of 500");
    }

    /// A document holding another's vector joins its node, and leaves it as
    /// it goes: deleted or written with another vector, the others keep the
    /// node, the node's own handing it on, and the node is a tombstone only
    /// once no document holds its vector.
    #[test]
    fn a_copy_joins_its_node_and_the_node_outlives_it() {
        let mut ix = VectorIndex::new(4, spec());
        let v = vec![0.1, 0.2, 0.3, 0.4];
        for d in [1u64, 2, 3] {
            ix.insert(d, &v);
        }
        ix.insert(4, &[0.9, -0.2, 0.1, 0.0]);
        let found = |ix: &VectorIndex| {
            let mut f: Vec<u64> = ix
                .search(&v, 3, None, |_| true)
                .iter()
                .map(|x| x.0)
                .collect();
            f.sort_unstable();
            f
        };
        assert_eq!(
            (ix.doc_ids.len(), ix.len(), found(&ix)),
            (2, 4, vec![1, 2, 3])
        );
        // The node's own document goes: another takes the node over.
        ix.remove(1);
        assert_eq!((ix.dead(), ix.len()), (0, 3));
        assert_eq!(found(&ix)[..2], [2, 3]);
        // Written with another vector, a document leaves for a node of its own.
        ix.insert(2, &[-0.5, 0.5, 0.5, -0.5]);
        assert_eq!((ix.dead(), ix.doc_ids.len()), (0, 3));
        assert_eq!(ix.search(&v, 1, None, |_| true)[0].0, 3);
        // The last goes: a tombstone, and a copy written after is a node again.
        ix.remove(3);
        assert_eq!(ix.dead(), 1);
        ix.insert(9, &v);
        assert_eq!(ix.search(&v, 1, None, |_| true)[0].0, 9);
        // A filter and the exact paths see each document.
        ix.insert(10, &v);
        assert_eq!(ix.search(&v, 5, None, |d| d == 10)[0].0, 10);
        assert_eq!(ix.search_ids(&v, 1, &[10])[0].0, 10);
        let exact: Vec<u64> = ix
            .search_exact(&v, 2, |_| true)
            .iter()
            .map(|x| x.0)
            .collect();
        assert_eq!(exact, [9, 10]);
    }

    /// Natively the table that finds a vector written again grows from its
    /// own slots: past the split a 256th at a time -- no put makes room for
    /// more than a few shares of the nodes, where the one table made room
    /// for all of them at once -- each slot found where it went, and a
    /// tombstone's left behind as its table grows.
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn the_copies_table_grows_a_share_at_a_time() {
        const N: usize = 400_000;
        let hash = |node: usize| {
            let mut x = (node as u64).wrapping_add(0x9E37_79B9_7F4A_7C15);
            x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            x ^ (x >> 31)
        };
        let find = |same: &Same, node: usize| {
            let (slots, mut i) = same.home(hash(node));
            loop {
                match slots[i] {
                    0 => return false,
                    s if s == (hash(node) >> 32) << 32 | (node as u64 + 1) => return true,
                    _ => i = (i + 1) & (slots.len() - 1),
                }
            }
        };
        let mut dead = vec![false; N];
        let mut same = Same::made(&[], &[]);
        let mut most = 0;
        for node in 0..N {
            let before = same.bytes();
            same.put(node as u32, hash(node), &dead);
            if node > SAME_SPLIT {
                most = most.max(same.bytes().saturating_sub(before));
            }
        }
        assert!(!same.shards.is_empty());
        assert!(most < 4 * 4 * 8 * N / SAME_SHARDS, "{most}");
        assert!((0..N).all(|node| find(&same, node)));
        // Nine in ten nodes become tombstones, and as many nodes again come:
        // the tables grow past none of them.
        for d in dead.iter_mut().skip(1).step_by(10) {
            *d = true;
        }
        dead.iter_mut().for_each(|d| *d = !*d);
        dead.resize(2 * N, false);
        for node in N..2 * N {
            same.put(node as u32, hash(node), &dead);
        }
        let live = dead.iter().filter(|&&d| !d).count();
        assert!(same.bytes() < 40 * live, "{} for {live}", same.bytes());
        assert!((0..2 * N)
            .filter(|&n| !dead[n])
            .all(|node| find(&same, node)));
        // Made from the halves, as the first put after an open makes it --
        // on threads, past 2^18 nodes -- and the halves read back off both
        // tables, a tombstone's where one is left.
        let halves: Vec<u32> = (0..2 * N).map(|n| (hash(n) >> 32) as u32).collect();
        let made = Same::made(&halves, &dead);
        assert!((0..2 * N)
            .filter(|&n| !dead[n])
            .all(|node| find(&made, node)));
        assert!((0..2 * N)
            .filter(|&n| dead[n])
            .all(|node| !find(&made, node)));
        let live = |h: Vec<u32>| -> Vec<u32> {
            let h = h.iter().zip(&dead).map(|(&h, &d)| if d { 0 } else { h });
            h.collect()
        };
        assert!(live(made.halves_by_node(2 * N)) == live(halves.clone()));
        assert!(live(same.halves_by_node(2 * N)) == live(halves));
    }

    /// Past the split, a vector written again still joins its node -- over
    /// floats and over codes -- and one whose node became a tombstone makes
    /// a node again, which its copies then join: as one table found them.
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn copies_find_their_node_past_the_split() {
        let quick = VectorIndexSpec {
            m: 4,
            ef_construction: 8,
            ..spec()
        };
        for prec in [VecPrec::F32, VecPrec::F16] {
            let mut rng = Rng(11);
            let n = SAME_SPLIT / 2 + 3_000;
            let items: Vec<(u64, Vec<f32>)> = (0..n as u64)
                .map(|i| (i, (0..3).map(|_| rng.next_f32() - 0.5).collect()))
                .collect();
            let mut ix = VectorIndex::with_precision(3, quick, prec);
            ix.insert_batch(&items);
            assert!(!ix.same.shards.is_empty());
            assert_eq!(ix.doc_ids.len(), n);
            // A copy of every 7th, one at a time and as a batch.
            let copies: Vec<(u64, Vec<f32>)> = items
                .iter()
                .step_by(7)
                .map(|(d, v)| (d + 1_000_000, v.clone()))
                .collect();
            let (one, rest) = copies.split_at(100);
            for (d, v) in one {
                ix.insert(*d, v);
            }
            ix.insert_batch(rest);
            assert_eq!(ix.doc_ids.len(), n, "a node a vector");
            assert_eq!(ix.len(), n + copies.len());
            // Every 11th's documents go: a tombstone each, and the vector
            // written again is a node of its own, which a copy joins.
            let gone: Vec<&(u64, Vec<f32>)> = items.iter().skip(3).step_by(11).collect();
            for (d, _) in &gone {
                ix.remove(*d);
                ix.remove(*d + 1_000_000);
            }
            let dead = ix.dead();
            assert!(dead > 0);
            for (d, v) in &gone {
                ix.insert(d + 2_000_000, v);
                ix.insert(d + 3_000_000, v);
            }
            assert_eq!(ix.doc_ids.len(), n + gone.len());
            for (d, v) in &gone {
                let found: Vec<u64> = ix
                    .search(v, 2, None, |_| true)
                    .iter()
                    .map(|x| x.0)
                    .collect();
                assert_eq!(found, [d + 2_000_000, d + 3_000_000]);
            }
        }
    }

    /// The live slots of a copies table, in order: two tables find the same
    /// nodes by the same halves when these are equal.
    #[cfg(not(target_family = "wasm"))]
    fn live_slots(ix: &VectorIndex) -> Vec<u64> {
        let tables = std::iter::once(&ix.same.one).chain(&ix.same.shards);
        let mut slots: Vec<u64> = tables
            .flat_map(|t| t.slots.iter().copied())
            .filter(|&s| s != 0 && !ix.deleted[(s as u32 - 1) as usize])
            .collect();
        slots.sort_unstable();
        slots
    }

    /// A graph record carries its nodes' hash halves, so the first vector
    /// placed after a restore makes the copies table from them rather than
    /// from the arena -- the same table, over every kind of arena and past
    /// the split. A record without them restores and makes it from the
    /// arena; halves that are not the vectors' are let go of; anything
    /// else after the aliases is a record of no build, and refused.
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn the_copies_table_comes_back_with_its_graph() {
        let mut rng = Rng(53);
        let cases = [
            (Quant::None, VecPrec::F32, 1500),
            (Quant::None, VecPrec::F16, 1500),
            (Quant::Int8, VecPrec::F32, 1500),
            (Quant::Bit, VecPrec::F32, 2200),
            (Quant::None, VecPrec::F32, SAME_SPLIT / 2 + 3000),
        ];
        for (quant, prec, n) in cases {
            let what = format!("{quant:?} {prec:?} {n}");
            let dim = 6;
            let spec = VectorIndexSpec {
                metric: Metric::Cosine,
                quant,
                m: 4,
                ef_construction: 8,
                ..VectorIndexSpec::default()
            };
            let mut ix = VectorIndex::with_precision(dim, spec, prec);
            let vector =
                |rng: &mut Rng| -> Vec<f32> { (0..dim).map(|_| rng.next_f32() - 0.5).collect() };
            let mut held: HashMap<u64, Vec<f32>> = HashMap::new();
            let items: Vec<(u64, Vec<f32>)> =
                (0..n as u64).map(|i| (i, vector(&mut rng))).collect();
            ix.insert_batch(&items);
            held.extend(items.iter().cloned());
            // Copies of every 9th, documents written again and deleted.
            for (d, v) in items.iter().step_by(9) {
                ix.insert(d + 10_000_000, v);
                held.insert(d + 10_000_000, v.clone());
            }
            for i in (1..n as u64).step_by(37) {
                let v = vector(&mut rng);
                ix.insert(i, &v);
                held.insert(i, v);
            }
            for i in (2..n as u64).step_by(41) {
                ix.remove(i);
                held.remove(&i);
            }
            let lookup = |doc: u64, out: &mut Vec<f32>| {
                out.clear();
                let v = held.get(&doc).map(|v| match prec {
                    VecPrec::F32 => v.clone(),
                    VecPrec::F16 => halved(v),
                });
                out.extend_from_slice(v.as_deref().unwrap_or(&[]));
                v.is_some()
            };
            let nodes = ix.doc_ids.len();
            let bytes = ix.serialize_graph();
            let tail = 1 + 4 * nodes;
            assert_eq!(bytes[bytes.len() - tail], HALVES, "{what}");
            let restore = |b: &[u8]| VectorIndex::restore_graph(b, dim, prec, lookup);
            let back = restore(&bytes).expect(&what);
            assert_eq!(back.same.halves.len(), nodes, "{what}: halves kept");
            assert!(!back.same.built, "{what}");
            assert!(
                back.serialize_graph() == bytes,
                "{what}: written back otherwise"
            );
            // Made from the halves, as from the arena, as the index that
            // wrote the record holds it.
            let mut from_halves = restore(&bytes).expect(&what);
            from_halves.same_build();
            let mut from_arena = restore(&bytes).expect(&what);
            from_arena.same.halves.clear();
            from_arena.same_build();
            assert_eq!(live_slots(&from_halves), live_slots(&ix), "{what}");
            assert_eq!(live_slots(&from_arena), live_slots(&ix), "{what}");
            assert!(
                from_halves.serialize_graph() == bytes,
                "{what}: built, written otherwise"
            );
            // A copy written after the restore joins its node.
            let mut after = restore(&bytes).expect(&what);
            for (d, v) in items.iter().skip(4).step_by(13).take(50) {
                if held.contains_key(d) && held[d] == *v {
                    after.insert(d + 20_000_000, v);
                }
            }
            assert_eq!(after.doc_ids.len(), nodes, "{what}: a copy made a node");
            // Without the halves the record restores, and the table is
            // made from the arena; with halves not the vectors', as well.
            let without = restore(&bytes[..bytes.len() - tail]).expect(&what);
            assert!(without.same.halves.is_empty(), "{what}");
            let mut wrong = bytes.clone();
            let at = bytes.len() - tail + 1;
            for h in wrong[at..].as_chunks_mut::<4>().0 {
                *h = u32::from_le_bytes(*h).wrapping_add(1).to_le_bytes();
            }
            let wrong = restore(&wrong).expect(&what);
            assert!(wrong.same.halves.is_empty(), "{what}: wrong halves kept");
            // Cut short, or with a byte past them.
            assert!(restore(&bytes[..bytes.len() - 1]).is_none(), "{what}: cut");
            let mut longer = bytes.clone();
            longer.push(0);
            assert!(restore(&longer).is_none(), "{what}: longer");
        }
    }

    /// The documents holding another's vector travel in the graph record,
    /// and one whose document holds another vector now has the graph built
    /// again rather than restored.
    #[test]
    fn copies_survive_serialization() {
        let mut ix = VectorIndex::new(4, spec());
        let mut rng = Rng(3);
        let mut vecs: Vec<Vec<f32>> = (0..200)
            .map(|_| (0..4).map(|_| rng.next_f32()).collect())
            .collect();
        for i in 200..260 {
            vecs.push(vecs[i % 7].clone());
        }
        for (d, v) in vecs.iter().enumerate() {
            ix.insert(d as u64, v);
        }
        ix.remove(3);
        assert_eq!(ix.len(), 259);
        let bytes = ix.serialize_graph();
        let fetch = |d: u64, out: &mut Vec<f32>| match vecs.get(d as usize) {
            Some(v) => {
                out.clear();
                out.extend_from_slice(v);
                true
            }
            None => false,
        };
        let back = VectorIndex::restore_graph(&bytes, 4, VecPrec::F32, fetch).expect("restored");
        assert_eq!((back.len(), back.aliases.len()), (259, ix.aliases.len()));
        for (k, v) in vecs.iter().enumerate().take(7) {
            let got = |ix: &VectorIndex| ix.search(v, 12, None, |_| true);
            assert_eq!(got(&back), got(&ix), "vector {k}");
        }
        let other = |d: u64, out: &mut Vec<f32>| {
            out.clear();
            match d {
                205 => out.extend_from_slice(&[9.0, 9.0, 9.0, 9.0]),
                _ => out.extend_from_slice(&vecs[d as usize]),
            }
            true
        };
        assert!(VectorIndex::restore_graph(&bytes, 4, VecPrec::F32, other).is_none());
    }

    #[test]
    fn graph_survives_serialization() {
        let mut ix = VectorIndex::new(8, VectorIndexSpec::default());
        let mut rng = Rng(99);
        let mut vecs = Vec::new();
        for i in 0..300u64 {
            let v: Vec<f32> = (0..8).map(|_| rng.next_f32() - 0.5).collect();
            ix.insert(i, &v);
            vecs.push(v);
        }
        let before: Vec<u64> = ix
            .search(&vecs[42], 10, None, |_| true)
            .into_iter()
            .map(|x| x.0)
            .collect();

        let bytes = ix.serialize_graph();
        let fetch = |d: u64, out: &mut Vec<f32>| match vecs.get(d as usize) {
            Some(v) => {
                out.clear();
                out.extend_from_slice(v);
                true
            }
            None => false,
        };
        let restored =
            VectorIndex::restore_graph(&bytes, 8, VecPrec::F32, fetch).expect("restore failed");
        assert_eq!(restored.len(), 300);
        let after: Vec<u64> = restored
            .search(&vecs[42], 10, None, |_| true)
            .into_iter()
            .map(|x| x.0)
            .collect();
        assert_eq!(
            before, after,
            "the restored graph must give the same results"
        );

        // A wrong dimension must be rejected
        assert!(VectorIndex::restore_graph(&bytes, 16, VecPrec::F32, fetch).is_none());
        // A missing document must be rejected
        assert!(VectorIndex::restore_graph(
            &bytes,
            8,
            VecPrec::F32,
            |d: u64, out: &mut Vec<f32>| {
                if d == 7 {
                    false
                } else {
                    fetch(d, out)
                }
            }
        )
        .is_none());
    }

    /// A tombstone's vector travels in the record: its document may be gone
    /// (a `del`) or hold a new vector (an update), and the restored graph has
    /// to route exactly as the one written did. The arena keeps its
    /// precision too.
    #[test]
    fn tombstones_and_precision_survive_serialization() {
        for prec in [VecPrec::F32, VecPrec::F16] {
            let mut ix = VectorIndex::with_precision(8, VectorIndexSpec::default(), prec);
            let mut rng = Rng(7);
            let mut vecs: Vec<Option<Vec<f32>>> = Vec::new();
            // A restore reads the document's record: an f16 field's halves.
            let stored = |v: Vec<f32>| match prec {
                VecPrec::F32 => v,
                VecPrec::F16 => halved(&v),
            };
            for i in 0..300u64 {
                let v: Vec<f32> = (0..8).map(|_| rng.next_f32() - 0.5).collect();
                ix.insert(i, &v);
                vecs.push(Some(stored(v)));
            }
            // Deleted: the document is gone.
            for i in [3u64, 50, 51, 299] {
                ix.remove(i);
                vecs[i as usize] = None;
            }
            // Updated: a new vector under the same id, the old node a tombstone.
            for i in [10u64, 11] {
                let v: Vec<f32> = (0..8).map(|_| rng.next_f32() - 0.5).collect();
                ix.insert(i, &v);
                vecs[i as usize] = Some(stored(v));
            }
            let q: Vec<f32> = (0..8).map(|_| rng.next_f32() - 0.5).collect();
            let before = ix.search(&q, 20, None, |_| true);

            let bytes = ix.serialize_graph();
            let fetch =
                |d: u64, out: &mut Vec<f32>| match vecs.get(d as usize).and_then(|v| v.as_ref()) {
                    Some(v) => {
                        out.clear();
                        out.extend_from_slice(v);
                        true
                    }
                    None => false,
                };
            let restored =
                VectorIndex::restore_graph(&bytes, 8, prec, fetch).expect("restore failed");
            assert_eq!(restored.len(), ix.len());
            // The arena itself, not the field alone: restored as f32, an f16
            // field held twice the memory.
            assert!(matches!(
                (&restored.data, prec),
                (Arena::F32(_), VecPrec::F32) | (Arena::F16(_), VecPrec::F16)
            ));
            assert_eq!(restored.search(&q, 20, None, |_| true), before, "{prec:?}");

            let other = if prec == VecPrec::F32 {
                VecPrec::F16
            } else {
                VecPrec::F32
            };
            assert!(VectorIndex::restore_graph(&bytes, 8, other, fetch).is_none());
        }
    }

    /// Deleting every row and writing it again left `near` answering
    /// nothing over a collection of two: the new nodes' only candidates were
    /// tombstones, the selection skipped them, and a node linked to nothing
    /// is one no search reaches.
    #[test]
    fn a_node_among_tombstones_is_still_reached() {
        let mut ix = VectorIndex::new(2, spec());
        ix.insert(1, &[1.0, 0.0]);
        ix.insert(2, &[0.0, 1.0]);
        ix.remove(1);
        ix.remove(2);
        ix.insert(3, &[1.0, 0.0]);
        ix.insert(4, &[0.0, 1.0]);
        let r = ix.search(&[1.0, 0.0], 2, None, |_| true);
        assert_eq!(r.iter().map(|x| x.0).collect::<Vec<_>>(), [3, 4]);
    }

    /// The same where a whole neighbourhood went, as when a document's
    /// chunks are written again: more of them deleted than
    /// `ef_construction` looks at, and the new ones landing among the old
    /// ones' tombstones while the rest of the collection lives on.
    #[test]
    fn a_rewritten_neighbourhood_is_found() {
        let at = |i: u64, shift: f32| [(i % 20) as f32 * 0.01 + shift, (i / 20) as f32 * 0.01];
        let mut ix = VectorIndex::new(2, spec());
        for i in 0..100u64 {
            ix.insert(i, &[1000.0 + i as f32, 0.0]);
        }
        for i in 0..300u64 {
            ix.insert(1000 + i, &at(i, 0.0));
        }
        for i in 0..300u64 {
            ix.remove(1000 + i);
        }
        for i in 0..300u64 {
            ix.insert(5000 + i, &at(i, 0.001));
        }
        for i in 0..300u64 {
            let r = ix.search(&at(i, 0.001), 1, None, |_| true);
            assert_eq!(r.first().map(|x| x.0), Some(5000 + i), "{i}");
        }
        assert_eq!(ix.search(&[1050.0, 0.0], 1, None, |_| true)[0].0, 50);
    }

    #[test]
    fn delete_and_filter() {
        let mut ix = VectorIndex::new(2, spec());
        for i in 0..50u64 {
            ix.insert(i, &[i as f32, 0.0]);
        }
        ix.remove(10);
        let r = ix.search(&[10.0, 0.0], 1, None, |_| true);
        assert_ne!(r[0].0, 10);
        let r = ix.search(&[10.0, 0.0], 1, None, |d| d % 7 == 0);
        assert_eq!(r[0].0 % 7, 0);
    }

    /// The quantized kernels are `dot`'s strips over widened codes, in the
    /// same order: the same bits, whatever vectorises them.
    #[test]
    fn the_code_kernels_are_dot_over_widened_codes() {
        let mut r = Rng(0x1234_5678);
        for dim in [1usize, 7, 8, 63, 64, 65, 200, 768] {
            let q: Vec<f32> = (0..dim).map(|_| r.next_f32() - 0.5).collect();
            let code: Vec<i8> = (0..dim)
                .map(|_| (r.next_f32() * 254.0 - 127.0) as i8)
                .collect();
            let wide: Vec<f32> = code.iter().map(|c| *c as f32).collect();
            assert_eq!(
                dot_i8(&code, &q).to_bits(),
                dot(&wide, &q).to_bits(),
                "{dim}"
            );
            let scaled: Vec<f32> = wide.iter().map(|c| c * 0.01).collect();
            assert_eq!(
                l2_i8(&code, &q, 0.01).to_bits(),
                l2_sq(&scaled, &q).to_bits()
            );

            let mut bits = vec![0u64; dim.div_ceil(64)];
            let signs: Vec<f32> = (0..dim)
                .map(|i| {
                    let set = r.next_f32() > 0.5;
                    bits[i / 64] |= (set as u64) << (i % 64);
                    if set {
                        1.0
                    } else {
                        -1.0
                    }
                })
                .collect();
            assert_eq!(
                dot_bits(&bits, &q).to_bits(),
                dot(&signs, &q).to_bits(),
                "{dim}"
            );
        }
    }

    /// The planes' popcounts are the sum of the query's parts under the
    /// signs, to the unit, at every length a code word can end on.
    #[test]
    fn planes_sum_the_parts_under_the_signs() {
        let mut r = Rng(7);
        for dim in [1usize, 7, 63, 64, 65, 127, 128, 300, 768] {
            let top = (1 << (QUERY_BITS - 1)) - 1;
            let parts: Vec<i32> = (0..dim)
                .map(|_| (r.next_f32() * (2 * top + 1) as f32) as i32 - top)
                .collect();
            let mut planes = vec![0u64; dim.div_ceil(64) * QUERY_BITS];
            for (i, part) in parts.iter().enumerate() {
                for j in 0..QUERY_BITS {
                    planes[i / 64 * QUERY_BITS + j] |= ((part >> j) as u64 & 1) << (i % 64);
                }
            }
            let packed: Vec<f32> = planes
                .iter()
                .flat_map(|p| [f32::from_bits(*p as u32), f32::from_bits((*p >> 32) as u32)])
                .collect();
            let mut bits = vec![0u64; dim.div_ceil(64)];
            let mut want = 0;
            for (i, part) in parts.iter().enumerate() {
                if r.next_f32() > 0.5 {
                    bits[i / 64] |= 1 << (i % 64);
                    want += part;
                }
            }
            assert_eq!(dot_planes(&bits, &packed), want, "{dim}");
        }
    }

    /// Cut to [`QUERY_BITS`] bits a component, a query estimates as well as
    /// whole: what cutting it adds is lost in what the codes miss anyway.
    #[test]
    fn a_query_in_planes_estimates_as_the_query_whole() {
        let mut r = Rng(11);
        let dim = 384;
        let spec = VectorIndexSpec {
            metric: Metric::Cosine,
            quant: Quant::Bit,
            ..spec()
        };
        let docs: Vec<Vec<f32>> = (0..BIT_TRAIN + 100)
            .map(|_| (0..dim).map(|_| r.next_f32() - 0.5).collect())
            .collect();
        let mut ix = VectorIndex::new(dim, spec);
        for (i, v) in docs.iter().enumerate() {
            ix.insert(i as u64, v);
        }
        let Arena::Bit(b) = &ix.data else {
            panic!("the arena is not coded")
        };
        let (mut whole, mut cut) = (0.0f64, 0.0f64);
        for _ in 0..20 {
            let raw: Vec<f32> = (0..dim).map(|_| r.next_f32() - 0.5).collect();
            let (q, planes) = (ix.prepare_query(&raw), ix.query_for(&raw));
            assert!(planes.len() > dim + b.centres.half.len() + 2);
            for (node, doc) in ix.doc_ids.iter().enumerate() {
                let exact = dot(&q, &normalized(&docs[*doc as usize])) as f64;
                whole += (b.estimate(&q, node as u32, dim) as f64 - exact).powi(2);
                cut += (b.estimate(&planes, node as u32, dim) as f64 - exact).powi(2);
            }
        }
        assert!(cut < whole * 1.02, "{cut} against {whole}");
    }

    /// A graph over codes goes through the file as one over vectors does:
    /// version 4, the quantization with it, a tombstone's code travelling
    /// with it; a graph over vectors stays version 3.
    #[test]
    fn a_graph_over_codes_round_trips() {
        let mut r = Rng(42);
        // Past the vectors a bit index holds whole: its codes, and the
        // centres they were taken from, come back as they were.
        let docs: Vec<Vec<f32>> = (0..BIT_TRAIN + 400)
            .map(|_| (0..16).map(|_| r.next_f32() - 0.5).collect())
            .collect();
        for quant in [Quant::None, Quant::Int8, Quant::Bit] {
            let spec = VectorIndexSpec {
                metric: Metric::Cosine,
                quant,
                ..spec()
            };
            let mut ix = VectorIndex::new(16, spec);
            for (i, v) in docs.iter().enumerate() {
                ix.insert(i as u64, v);
            }
            for i in 0..50 {
                ix.remove(i * 7);
            }
            let bytes = ix.serialize_graph();
            assert_eq!(bytes[0], GRAPH_VERSION_ALIASED);
            // The varint layout wrote a quantized graph as 4, others as 3.
            let old = ix.serialize_varint(false)[0];
            assert_eq!(old, if quant == Quant::None { 3 } else { 4 });
            let fetch = |doc: DocId, out: &mut Vec<f32>| match docs.get(doc as usize) {
                Some(v) => {
                    out.clear();
                    out.extend_from_slice(v);
                    true
                }
                None => false,
            };
            let back = VectorIndex::restore_graph(&bytes, 16, VecPrec::F32, fetch)
                .expect("restore failed");
            assert_eq!(back.spec, spec);
            assert_eq!(back.arena_bytes(), ix.arena_bytes(), "{quant:?}");
            // Restored neighbour lists come back sorted, so equal distances
            // -- common over codes -- may leave in another order.
            let ranked = |ix: &VectorIndex, q: &[f32]| {
                let mut r = ix.search(q, 10, None, |_| true);
                r.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                r
            };
            for q in docs.iter().step_by(37) {
                assert_eq!(ranked(&back, q), ranked(&ix, q), "{quant:?}");
            }
        }
    }

    /// A vector written again as it was keeps its node over bit codes, at
    /// either precision: the ones held whole until the centres were
    /// learned, coded from what they were held as, and the ones after.
    #[test]
    fn a_bit_code_written_again_keeps_its_node() {
        for prec in [VecPrec::F32, VecPrec::F16] {
            let spec = VectorIndexSpec {
                metric: Metric::Cosine,
                quant: Quant::Bit,
                ..spec()
            };
            let mut ix = VectorIndex::with_precision(16, spec, prec);
            let docs = items(3, BIT_TRAIN as u64 + 100, 16);
            for (doc, v) in &docs {
                ix.insert(*doc, v);
            }
            assert!(ix.quantized());
            for (doc, v) in docs.iter().take(100).chain(docs.iter().rev().take(100)) {
                ix.insert(*doc, v);
            }
            assert_eq!(ix.dead(), 0, "{prec:?}");
            let moved: Vec<f32> = docs[5].1.iter().map(|x| x + 0.25).collect();
            ix.insert(docs[5].0, &moved);
            assert_eq!((ix.dead(), ix.len()), (1, docs.len()), "{prec:?}");
        }
    }

    /// A bit code is the signs of a vector's distance from the nearest of
    /// the centres the index learned, not of the vector: in clusters
    /// crowded around centres away from the origin, most of a vector's own
    /// signs are its centre's, and tell the cluster's members apart poorly.
    /// Ranked by the codes, the best 40 of 3 000 hold 91.5% of the true ten;
    /// by the vectors' own signs, 19%.
    #[test]
    fn bit_codes_tell_a_cluster_apart_by_its_residuals() {
        let dim = 64;
        let mut r = Rng(7);
        let centres: Vec<Vec<f32>> = (0..8)
            .map(|_| (0..dim).map(|_| r.next_f32() * 2.0 - 1.0).collect())
            .collect();
        let mut near = |i: usize| -> Vec<f32> {
            centres[i % 8]
                .iter()
                .map(|c| c + 0.3 * (r.next_f32() - 0.5))
                .collect()
        };
        let docs: Vec<Vec<f32>> = (0..3000).map(&mut near).collect();
        let queries: Vec<Vec<f32>> = (0..40).map(&mut near).collect();
        let spec = VectorIndexSpec {
            metric: Metric::Cosine,
            quant: Quant::Bit,
            ..spec()
        };
        let mut ix = VectorIndex::new(dim, spec);
        for (i, v) in docs.iter().enumerate() {
            ix.insert(i as u64, v);
        }
        assert!(ix.quantized());
        let unit: Vec<Vec<f32>> = docs.iter().map(|v| normalized(v)).collect();
        let best = |score: &dyn Fn(usize) -> f32, k: usize| -> Vec<u64> {
            let mut all: Vec<(f32, u64)> = (0..unit.len()).map(|i| (score(i), i as u64)).collect();
            all.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            all[..k].iter().map(|x| x.1).collect()
        };
        let (mut coded, mut signed) = (0, 0);
        for q in &queries {
            let q = normalized(q);
            let truth = best(&|i| dot(&q, &unit[i]), 10);
            let codes: Vec<u64> = ix
                .search_exact(&q, 40, |_| true)
                .iter()
                .map(|x| x.0)
                .collect();
            // The vector's own signs, as bit codes were.
            let signs = best(
                &|i| {
                    unit[i]
                        .iter()
                        .zip(&q)
                        .map(|(x, y)| if *x > 0.0 { *y } else { -*y })
                        .sum()
                },
                40,
            );
            coded += truth.iter().filter(|d| codes.contains(d)).count();
            signed += truth.iter().filter(|d| signs.contains(d)).count();
        }
        let all = queries.len() as f64 * 10.0;
        assert!(coded as f64 / all >= 0.9, "{coded} of {all}");
        assert!(signed < coded, "{signed} against {coded}");
    }

    /// `n` random `(doc, vector)` items from a seed, docs counted from 0.
    fn items(seed: u64, n: u64, dim: usize) -> Vec<(u64, Vec<f32>)> {
        let mut rng = Rng(seed);
        (0..n)
            .map(|i| (i, (0..dim).map(|_| rng.next_f32() - 0.5).collect()))
            .collect()
    }

    /// Recall@10 of the index's walk against its own exact scan.
    fn recall(ix: &VectorIndex, queries: &[(u64, Vec<f32>)]) -> f64 {
        let (mut hit, mut all) = (0, 0);
        for (_, q) in queries {
            let exact: Vec<u64> = ix
                .search_exact(q, 10, |_| true)
                .into_iter()
                .map(|x| x.0)
                .collect();
            let got: Vec<u64> = ix
                .search(q, 10, None, |_| true)
                .into_iter()
                .map(|x| x.0)
                .collect();
            hit += got.iter().filter(|d| exact.contains(d)).count();
            all += exact.len();
        }
        hit as f64 / all as f64
    }

    /// A node left out of the graph is measured by every search until it is
    /// linked: one of the true ten that waits is always found, and an index
    /// of nothing but waiting nodes answers as its exact scan does.
    #[test]
    fn unlinked_nodes_are_measured_until_linked() {
        let dim = 16;
        let all = items(5, 3000, dim);
        let mut ix = VectorIndex::new(dim, VectorIndexSpec::default());
        ix.insert_batch(&all[..2000]);
        ix.defer_batch(&all[2000..]);
        assert_eq!((ix.unlinked(), ix.len()), (1000, 3000));
        for (_, q) in all.iter().step_by(97) {
            let got: Vec<u64> = ix
                .search(q, 10, None, |_| true)
                .into_iter()
                .map(|x| x.0)
                .collect();
            for (doc, _) in ix.search_exact(q, 10, |_| true) {
                assert!(
                    doc < 2000 || got.contains(&doc),
                    "waiting {doc} missed: {got:?}"
                );
            }
        }
        let mut only = VectorIndex::new(dim, VectorIndexSpec::default());
        only.defer_batch(&all[..500]);
        for (_, q) in all.iter().step_by(61) {
            assert_eq!(
                only.search(q, 10, None, |_| true),
                only.search_exact(q, 10, |_| true)
            );
        }
    }

    /// Linked a slice at a time, the waiting nodes make a graph as good as
    /// the batch build's, over a graph linked before them and over none.
    #[test]
    fn linking_in_slices_keeps_the_recall_of_a_batch() {
        let dim = 16;
        let all = items(7, 2400, dim);
        let queries: Vec<_> = all.iter().step_by(120).cloned().collect();
        let mut batch = VectorIndex::new(dim, spec());
        batch.insert_batch(&all);
        let want = recall(&batch, &queries);
        for linked in [0, 1600] {
            let mut ix = VectorIndex::new(dim, spec());
            ix.insert_batch(&all[..linked]);
            ix.defer_batch(&all[linked..]);
            let mut slices = 1;
            while ix.link_pending(37, &mut |_, _| false) > 0 {
                slices += 1;
            }
            assert_eq!(
                (ix.unlinked(), ix.len(), slices),
                (0, 2400, (2400 - linked).div_ceil(37))
            );
            let got = recall(&ix, &queries);
            assert!(
                got >= want - 0.05,
                "{linked} linked first: recall {got:.3} against the batch's {want:.3}"
            );
        }
    }

    /// Written while nodes wait, a graph says which (version 5) and they come
    /// back waiting; linked, it is written as it always was.
    #[test]
    fn unlinked_nodes_survive_serialization() {
        let dim = 8;
        let all = items(9, 800, dim);
        let fetch = |doc: DocId, out: &mut Vec<f32>| {
            out.clear();
            out.extend_from_slice(&all[doc as usize].1);
            true
        };
        let ranked = |ix: &VectorIndex, q: &[f32]| {
            let mut r = ix.search(q, 10, None, |_| true);
            r.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            r
        };
        for quant in [Quant::None, Quant::Int8] {
            let spec = VectorIndexSpec {
                metric: Metric::Cosine,
                quant,
                ..spec()
            };
            let mut ix = VectorIndex::new(dim, spec);
            ix.insert_batch(&all[..500]);
            ix.defer_batch(&all[500..]);
            // Deleted while waiting: a tombstone, no longer waiting.
            ix.remove(600);
            let bytes = ix.serialize_graph();
            assert_eq!(bytes[0], GRAPH_VERSION_ALIASED, "{quant:?}");
            assert_eq!(ix.serialize_varint(false)[0], 5, "{quant:?}");
            let back = VectorIndex::restore_graph(&bytes, dim, VecPrec::F32, fetch)
                .expect("restore failed");
            assert_eq!(back.spec, spec);
            assert_eq!((back.unlinked(), back.dead(), back.len()), (299, 1, 799));
            for (_, q) in all.iter().step_by(41) {
                assert_eq!(ranked(&back, q), ranked(&ix, q), "{quant:?}");
            }
            ix.link_pending(usize::MAX, &mut { fetch });
            // Linked, the varint layout left 5 for 3 or 4.
            let linked = ix.serialize_varint(false);
            assert_eq!(linked[0], if quant == Quant::None { 3 } else { 4 });
        }
    }

    /// A waiting node whose document is written again or deleted is not
    /// linked: the write leaves it a tombstone, as it leaves a linked one --
    /// unless the vector is the one it holds, and it goes on waiting.
    #[test]
    fn a_rewritten_or_deleted_waiting_node_is_not_linked() {
        let dim = 8;
        let all = items(13, 1500, dim);
        let mut ix = VectorIndex::new(dim, VectorIndexSpec::default());
        ix.insert_batch(&all[..1200]);
        ix.defer_batch(&all[1200..]);
        ix.remove(1300);
        let moved: Vec<f32> = all[5].1.iter().map(|x| x + 0.001).collect();
        ix.insert(1400, &moved);
        ix.insert(1450, &all[1450].1.clone());
        assert_eq!((ix.dead(), ix.unlinked()), (2, 300));
        while ix.link_pending(64, &mut |_, _| false) > 0 {}
        assert_eq!(ix.len(), 1499);
        assert_eq!(ix.search(&moved, 1, None, |_| true)[0].0, 1400);
        assert_eq!(ix.search(&all[1450].1, 1, None, |_| true)[0].0, 1450);
        let at_1300 = ix.search(&all[1300].1, 10, None, |_| true);
        assert!(at_1300.iter().all(|h| h.0 != 1300), "{at_1300:?}");
    }

    /// Over codes a waiting node looks for its neighbours with its own
    /// vector, read through `lookup`, as the write path does.
    #[test]
    fn a_waiting_node_over_codes_links_with_its_own_vector() {
        let dim = 16;
        let all = items(17, 3000, dim);
        let spec = VectorIndexSpec {
            metric: Metric::Cosine,
            quant: Quant::Int8,
            ..VectorIndexSpec::default()
        };
        let mut ix = VectorIndex::new(dim, spec);
        ix.insert_batch(&all[..1500]);
        ix.defer_batch(&all[1500..]);
        let mut read = 0;
        while ix.link_pending(100, &mut |doc, out| {
            read += 1;
            out.clear();
            out.extend_from_slice(&all[doc as usize].1);
            true
        }) > 0
        {}
        assert_eq!(read, 1500);
        let mut batch = VectorIndex::new(dim, spec);
        batch.insert_batch(&all);
        let queries: Vec<_> = all.iter().step_by(150).cloned().collect();
        let (got, want) = (recall(&ix, &queries), recall(&batch, &queries));
        assert!(
            got >= want - 0.05,
            "recall {got:.3} against the batch's {want:.3}"
        );
    }
}
