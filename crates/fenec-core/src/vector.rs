//! Vector search: metrics + the HNSW index.
//!
//! Design notes
//! - All vectors sit contiguously in a single `Vec<f32>` arena. No separate
//!   allocation per vector; this is both cache friendly and required for
//!   LLVM's auto-vectorisation.
//! - With the cosine metric vectors are normalised *inside the index*, so no
//!   length computation happens at query time (1 - dot).
//! - A delete is a tombstone; skipped while searching, cleaned up on merge.

use crate::codec::{get_uvarint, put_uvarint};
use crate::schema::{Metric, Quant, VectorIndexSpec};
use crate::value::{DocId, VecPrec};
use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

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

#[cfg(not(all(target_arch = "wasm32", target_feature = "simd128")))]
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

#[cfg(not(all(target_arch = "wasm32", target_feature = "simd128")))]
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

#[cfg(not(all(target_arch = "wasm32", target_feature = "simd128")))]
#[inline]
fn distance_hf(metric: Metric, a: &[u16], b: &[f32]) -> f32 {
    match metric {
        Metric::Cosine => 1.0 - strip8!(a, b, half, ident, mul, 0),
        Metric::L2 => strip8!(a, b, half, ident, diff_sq, 0),
        Metric::Dot => -strip8!(a, b, half, ident, mul, 0),
    }
}

#[cfg(not(all(target_arch = "wasm32", target_feature = "simd128")))]
#[inline]
fn distance_hh(metric: Metric, a: &[u16], b: &[u16]) -> f32 {
    match metric {
        Metric::Cosine => 1.0 - strip8!(a, b, half, half, mul, 0),
        Metric::L2 => strip8!(a, b, half, half, diff_sq, 0),
        Metric::Dot => -strip8!(a, b, half, half, mul, 0),
    }
}

// ------------------------------------------------------ quantized kernels
//
// Scalar on every target, in `strip8!`'s order: LLVM vectorises the strips
// natively, and the browser's scalar loop adds in the same order, so a graph
// over codes is the same graph in both, as over vectors.

#[inline]
fn widen_i8(x: i8) -> f32 {
    x as f32
}

/// Σ code·q, the int8 arena's dot product with a query before its scale.
#[inline]
fn dot_i8(code: &[i8], q: &[f32]) -> f32 {
    strip8!(code, q, widen_i8, ident, mul, 0)
}

/// Σ (scale·code − q)², the int8 arena's squared distance.
#[inline]
fn l2_i8(code: &[i8], q: &[f32], scale: f32) -> f32 {
    strip8!(code, q, |x: i8| x as f32 * scale, ident, diff_sq, 0)
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

// ----------------------------------------------------------------- arena

/// Per node of a batch insert: its id, its level, and the neighbours found
/// for it at each level -- what the parallel half hands the serial one.
type Candidates = Vec<(u32, usize, Vec<(usize, Vec<u32>)>)>;

/// Vector arena: every vector in one contiguous array, strided by `node * dim`.
///
/// The `F16` variant fits the same array into half the space. The query side
/// is always f32; widening happens inside the distance kernel, so there is no
/// extra allocation on the search path.
///
/// `I8` and `Bit` hold codes instead (`quant=`): a byte a component over a
/// scale a vector -- the largest component over 127 -- or the sign of each
/// component, 64 to a word. Their distances are estimates, and `near` puts
/// the candidates they find in order again by the documents' own vectors.
pub(crate) enum Arena {
    F32(Vec<f32>),
    F16(Vec<u16>),
    I8(Vec<i8>, Vec<f32>),
    Bit(Vec<u64>),
}

impl Arena {
    fn new(prec: VecPrec, quant: Quant) -> Arena {
        match (quant, prec) {
            (Quant::Int8, _) => Arena::I8(Vec::new(), Vec::new()),
            (Quant::Bit, _) => Arena::Bit(Vec::new()),
            (Quant::None, VecPrec::F32) => Arena::F32(Vec::new()),
            (Quant::None, VecPrec::F16) => Arena::F16(Vec::new()),
        }
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
            Arena::Bit(b) => b.reserve(nodes * dim.div_ceil(64)),
        }
    }

    /// Appends the vector. When `unit` it is first scaled to unit length
    /// (cosine). On the f32 path normalisation happens in place on the arena,
    /// so there is no intermediate `Vec` allocation; on the f16 path the
    /// scale is applied during the conversion anyway.
    fn push(&mut self, raw: &[f32], unit: bool) {
        let inv = if unit {
            // The summation order is deliberately flat: the 8-strip `norm`
            // rounds differently, which would change the normalised vectors
            // and with them the graph. We keep the old order for measurement.
            let n = {
                let mut acc = 0.0f32;
                for x in raw {
                    acc += x * x;
                }
                acc.sqrt()
            };
            if n > 0.0 {
                1.0 / n
            } else {
                1.0
            }
        } else {
            1.0
        };
        match self {
            Arena::F32(d) => {
                let base = d.len();
                d.extend_from_slice(raw);
                if inv != 1.0 {
                    for x in &mut d[base..] {
                        *x *= inv;
                    }
                }
            }
            Arena::F16(d) => {
                d.extend(raw.iter().map(|x| crate::codec::f16_from_f32(x * inv)));
            }
            Arena::I8(codes, scales) => {
                let top = raw.iter().fold(0.0f32, |m, x| m.max((x * inv).abs()));
                let scale = if top > 0.0 { top / 127.0 } else { 1.0 };
                codes.extend(raw.iter().map(|x| (x * inv / scale).round() as i8));
                scales.push(scale);
            }
            // A sign needs no normalising.
            Arena::Bit(words) => {
                for part in raw.chunks(64) {
                    let mut w = 0u64;
                    for (i, x) in part.iter().enumerate() {
                        w |= ((*x > 0.0) as u64) << i;
                    }
                    words.push(w);
                }
            }
        }
    }

    #[inline]
    fn slice_f32(&self, node: u32, dim: usize) -> Option<&[f32]> {
        match self {
            Arena::F32(d) => {
                let s = node as usize * dim;
                Some(&d[s..s + dim])
            }
            _ => None,
        }
    }

    #[inline]
    fn dist_to(&self, metric: Metric, q: &[f32], node: u32, dim: usize) -> f32 {
        let s = node as usize * dim;
        match self {
            Arena::F32(d) => distance(metric, q, &d[s..s + dim]),
            Arena::F16(d) => distance_hf(metric, &d[s..s + dim], q),
            Arena::I8(c, sc) => {
                let (code, scale) = (&c[s..s + dim], sc[node as usize]);
                match metric {
                    Metric::Cosine => 1.0 - scale * dot_i8(code, q),
                    Metric::L2 => l2_i8(code, q, scale),
                    Metric::Dot => -scale * dot_i8(code, q),
                }
            }
            // A code stands for the unit vector of its signs, ±1/√dim.
            Arena::Bit(b) => {
                let w = dim.div_ceil(64);
                let at = node as usize * w;
                1.0 - dot_bits(&b[at..at + w], q) / (dim as f32).sqrt()
            }
        }
    }

    #[inline]
    fn dist_nodes(&self, metric: Metric, a: u32, b: u32, dim: usize) -> f32 {
        let (sa, sb) = (a as usize * dim, b as usize * dim);
        match self {
            Arena::F32(d) => distance(metric, &d[sa..sa + dim], &d[sb..sb + dim]),
            Arena::F16(d) => distance_hh(metric, &d[sa..sa + dim], &d[sb..sb + dim]),
            // Off the hot path: `select_heuristic` widens codes once and
            // keeps them, as it does halves.
            Arena::I8(..) | Arena::Bit(_) => {
                distance(metric, &self.vec_at(a, dim), &self.vec_at(b, dim))
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
        let s = node as usize * dim;
        match self {
            Arena::F32(d) => out.extend_from_slice(&d[s..s + dim]),
            Arena::F16(d) => out.extend(d[s..s + dim].iter().map(|x| half(*x))),
            Arena::I8(c, sc) => {
                let scale = sc[node as usize];
                out.extend(c[s..s + dim].iter().map(|x| *x as f32 * scale));
            }
            Arena::Bit(b) => {
                let at = node as usize * dim.div_ceil(64);
                let unit = 1.0 / (dim as f32).sqrt();
                out.extend((0..dim).map(|i| {
                    if b[at + i / 64] >> (i % 64) & 1 == 1 {
                        unit
                    } else {
                        -unit
                    }
                }));
            }
        }
    }

    /// Appends the node's vector as the arena holds it, little-endian.
    fn write_stored(&self, node: u32, dim: usize, out: &mut Vec<u8>) {
        let s = node as usize * dim;
        match self {
            Arena::F32(d) => d[s..s + dim]
                .iter()
                .for_each(|x| out.extend_from_slice(&x.to_le_bytes())),
            Arena::F16(d) => d[s..s + dim]
                .iter()
                .for_each(|x| out.extend_from_slice(&x.to_le_bytes())),
            Arena::I8(c, sc) => {
                out.extend(c[s..s + dim].iter().map(|x| *x as u8));
                out.extend_from_slice(&sc[node as usize].to_le_bytes());
            }
            Arena::Bit(b) => {
                let w = dim.div_ceil(64);
                b[node as usize * w..(node as usize + 1) * w]
                    .iter()
                    .for_each(|x| out.extend_from_slice(&x.to_le_bytes()));
            }
        }
    }

    /// Appends a vector written by `write_stored`, as it was: it was already
    /// normalised, and normalising again would move its last bits.
    fn push_stored(&mut self, bytes: &[u8]) {
        match self {
            Arena::F32(d) => d.extend(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b)),
            ),
            Arena::F16(d) => d.extend(
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| u16::from_le_bytes(*b)),
            ),
            Arena::I8(c, sc) => {
                let (code, scale) = bytes.split_at(bytes.len() - 4);
                c.extend(code.iter().map(|x| *x as i8));
                sc.push(f32::from_le_bytes([scale[0], scale[1], scale[2], scale[3]]));
            }
            Arena::Bit(b) => b.extend(
                bytes
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|x| u64::from_le_bytes(*x)),
            ),
        }
    }

    /// Bytes a vector of `dim` takes in `write_stored`'s form.
    fn stored_len(&self, dim: usize) -> usize {
        match self {
            Arena::F32(_) => dim * 4,
            Arena::F16(_) => dim * 2,
            Arena::I8(..) => dim + 4,
            Arena::Bit(_) => dim.div_ceil(64) * 8,
        }
    }

    /// Bytes the arena occupies in memory (for statistics).
    pub(crate) fn bytes(&self) -> usize {
        match self {
            Arena::F32(d) => d.len() * 4,
            Arena::F16(d) => d.len() * 2,
            Arena::I8(c, s) => c.len() + s.len() * 4,
            Arena::Bit(b) => b.len() * 8,
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
/// quantization existed is not rebuilt for it.
const GRAPH_VERSION: u8 = 3;
const GRAPH_VERSION_QUANT: u8 = 4;

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
}

impl Scratch {
    fn new() -> Scratch {
        Scratch {
            visited: Vec::new(),
            epoch: 0,
            candidates: BinaryHeap::new(),
            results: BinaryHeap::new(),
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

    /// Greedy descent through the upper layers.
    fn descend(&self, q: &[f32], from: u32, from_level: usize, to_level: usize) -> u32 {
        let mut cur = from;
        let mut cur_dist = self.dist_to(q, cur);
        let mut lc = from_level;
        while lc > to_level {
            let mut improved = true;
            while improved {
                improved = false;
                for &nb in self.neighbors(cur, lc) {
                    let d = self.dist_to(q, nb);
                    if d < cur_dist {
                        cur_dist = d;
                        cur = nb;
                        improved = true;
                    }
                }
            }
            lc -= 1;
        }
        cur
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
            for &nb in self.neighbors(cur.node, level) {
                if !sc.see(nb) {
                    continue;
                }
                let d = self.dist_to(q, nb);
                let worst = sc.results.peek().map(|c| c.dist).unwrap_or(f32::MAX);
                if sc.results.len() < ef || d < worst {
                    let c = Cand { dist: d, node: nb };
                    sc.candidates.push(MinCand(c));
                    sc.results.push(c);
                    if sc.results.len() > ef {
                        sc.results.pop();
                    }
                }
            }
        }

        let mut out: Vec<Cand> = sc.results.drain().collect();
        out.sort();
        out
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
                for i in 0..out.len() {
                    let r = &picked[i * self.dim..(i + 1) * self.dim];
                    if distance(self.metric, &cbuf, r) < c.dist {
                        diverse = false;
                        break;
                    }
                }
            } else {
                for &r in &out {
                    if self.dist_nodes(c.node, r) < c.dist {
                        diverse = false;
                        break;
                    }
                }
            }
            if diverse {
                out.push(c.node);
                if decode {
                    picked.extend_from_slice(&cbuf);
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
        let cur = if self.max_level > level {
            self.descend(&v, entry, self.max_level, level)
        } else {
            entry
        };
        let mut out = Vec::new();
        let mut ep = vec![cur];
        for l in (0..=level.min(self.max_level)).rev() {
            let cands = self.search_layer(sc, &v, &ep, ef_construction, l);
            let selected = self.select_heuristic(&cands, m, node);
            ep = if selected.is_empty() {
                vec![cur]
            } else {
                selected.clone()
            };
            out.push((l, selected));
        }
        out
    }
}

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
    by_doc: HashMap<DocId, u32>,

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
    prune_buf: Vec<Cand>,
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

impl VectorIndex {
    pub fn new(dim: usize, spec: VectorIndexSpec) -> VectorIndex {
        VectorIndex::with_precision(dim, spec, VecPrec::F32)
    }

    pub fn with_precision(dim: usize, spec: VectorIndexSpec, prec: VecPrec) -> VectorIndex {
        VectorIndex {
            dim,
            spec,
            data: Arena::new(prec, spec.quant),
            prec,
            doc_ids: Vec::new(),
            by_doc: HashMap::new(),
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
            prune_buf: Vec::new(),
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

    pub fn len(&self) -> usize {
        self.doc_ids.len() - self.deleted_count
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

    #[inline]
    fn dist_to(&self, query: &[f32], node: u32) -> f32 {
        self.data.dist_to(self.spec.metric, query, node, self.dim)
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
            + self.by_doc.capacity() * (id + 4 + 1)
            + self.l0.capacity() * 4
            + self.l0_len.capacity() * 2
            + self.deleted.capacity()
            + self.upper.capacity() * size_of::<Vec<Vec<u32>>>()
    }

    pub fn precision(&self) -> VecPrec {
        self.prec
    }

    /// Whether the arena holds codes rather than vectors (`quant=`), so that
    /// its distances are estimates to be corrected from the documents.
    pub fn quantized(&self) -> bool {
        self.data.quantized()
    }

    /// The vector a node written with `raw` searches the graph for its
    /// neighbours with: `raw` itself, prepared, over an arena of codes --
    /// what the node's code widens back to is the signs alone under `bit` --
    /// and `None`, the arena's own vector, over vectors, whose graphs stay
    /// as they were built.
    fn build_query(&self, raw: &[f32]) -> Option<Vec<f32>> {
        self.quantized().then(|| self.prepare_query(raw))
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

    /// Greedy descent through the upper layers. The algorithm is on [`GraphView`].
    fn descend(&self, q: &[f32], from: u32, from_level: usize, to_level: usize) -> u32 {
        self.view().descend(q, from, from_level, to_level)
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

    fn random_level(&mut self) -> usize {
        let r = self.rng.next_f32().max(f32::MIN_POSITIVE);
        (-r.ln() * self.level_mult) as usize
    }

    /// Opens storage for a new node and writes the vector into the arena.
    /// With cosine the normalisation is done in place on the arena -- there
    /// is no intermediate `Vec` allocation.
    fn alloc_node(&mut self, doc: DocId, raw: &[f32], level: usize) -> u32 {
        self.data.push(raw, self.spec.metric == Metric::Cosine);
        self.alloc_links(doc, level)
    }

    /// `alloc_node` for a vector in the arena's stored form.
    fn alloc_node_stored(&mut self, doc: DocId, stored: &[u8], level: usize) -> u32 {
        self.data.push_stored(stored);
        self.alloc_links(doc, level)
    }

    fn alloc_links(&mut self, doc: DocId, level: usize) -> u32 {
        let node = self.doc_ids.len() as u32;
        self.doc_ids.push(doc);
        self.deleted.push(false);
        self.l0.resize(self.l0.len() + self.m0, 0);
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
        // The same document written again keeps its node when the vector is
        // the one it holds, and tombstones it otherwise.
        if let Some(&old) = self.by_doc.get(&doc) {
            if self.retire(old, raw) {
                return;
            }
        }
        let level = self.random_level();
        let node = self.alloc_node(doc, raw, level);
        self.by_doc.insert(doc, node);
        let query = self.build_query(raw);
        self.link_node(node, level, None, query.as_deref());
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

    #[cfg(target_family = "wasm")]
    fn compute_candidates(
        &self,
        pending: &[(u32, usize, Option<Vec<f32>>)],
        _threads: usize,
    ) -> Candidates {
        let (efc, m) = (self.spec.ef_construction, self.spec.m);
        let view = self.view();
        let mut sc = Scratch::new();
        pending
            .iter()
            .map(|(node, level, query)| {
                (
                    *node,
                    *level,
                    view.candidates_for(&mut sc, *node, query.as_deref(), *level, efc, m),
                )
            })
            .collect()
    }

    /// Computes the candidates split across threads. Every thread carries its
    /// own `Scratch` buffer; the graph is read-only at this stage.
    #[cfg(not(target_family = "wasm"))]
    fn compute_candidates(
        &self,
        pending: &[(u32, usize, Option<Vec<f32>>)],
        threads: usize,
    ) -> Candidates {
        let (efc, m) = (self.spec.ef_construction, self.spec.m);
        let view = self.view();
        let per = pending.len().div_ceil(threads);
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for part in pending.chunks(per) {
                handles.push(scope.spawn(move || {
                    let mut sc = Scratch::new();
                    part.iter()
                        .map(|(node, level, query)| {
                            (
                                *node,
                                *level,
                                view.candidates_for(
                                    &mut sc,
                                    *node,
                                    query.as_deref(),
                                    *level,
                                    efc,
                                    m,
                                ),
                            )
                        })
                        .collect::<Vec<_>>()
                }));
            }
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap_or_default())
                .collect()
        })
    }

    /// Inserts one batch in parallel.
    ///
    /// ~75% of the build time is read-only work: descent + beam search +
    /// neighbour selection in the graph for every new node. None of that
    /// modifies the graph, so it can be run in parallel one batch at a time.
    /// Only the final step -- writing the links and pruning the back-links --
    /// is serial.
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
        let threads = Self::threads();

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
                if let Some(&old) = self.by_doc.get(doc) {
                    if self.retire(old, v) {
                        continue;
                    }
                }
                let level = self.random_level();
                let node = self.alloc_node(*doc, v, level);
                self.by_doc.insert(*doc, node);
                pending.push((node, level, self.build_query(v)));
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
            let mut computed = self.compute_candidates(&pending, threads);

            // 3) Serial: write the links and prune the back-links.
            computed.sort_by_key(|(node, _, _)| *node);
            for (node, level, per_level) in computed {
                self.link_node(node, level, Some(per_level), None);
            }
        }
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
                let cur = if start_level > level {
                    self.descend(&v, entry, start_level, level)
                } else {
                    entry
                };
                let mut out = Vec::new();
                let mut ep = vec![cur];
                for l in (0..=level.min(self.max_level)).rev() {
                    let cands = self.search_layer(&v, &ep, self.spec.ef_construction, l);
                    let selected = self.select_heuristic(&cands, self.spec.m, node);
                    ep = if selected.is_empty() {
                        vec![cur]
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
                buf.clear();
                // `nb` is fixed across all the comparisons: widen it once.
                let nbv = self.vec_at(nb);
                for &x in self.neighbors(nb, l) {
                    buf.push(Cand {
                        dist: self.dist_to(&nbv, x),
                        node: x,
                    });
                }
                buf.push(Cand {
                    dist: self.dist_to(&nbv, node),
                    node,
                });
                drop(nbv);
                buf.sort();
                let pruned = self.select_heuristic(&buf, max_deg, nb);
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
    fn retire(&mut self, node: u32, raw: &[f32]) -> bool {
        if self.deleted[node as usize] {
            return false;
        }
        let mut probe = Arena::new(self.prec, self.spec.quant);
        probe.push(raw, self.spec.metric == Metric::Cosine);
        let (mut held, mut new) = (Vec::new(), Vec::new());
        self.data.write_stored(node, self.dim, &mut held);
        probe.write_stored(0, self.dim, &mut new);
        if held == new {
            return true;
        }
        self.deleted[node as usize] = true;
        self.deleted_count += 1;
        false
    }

    pub fn remove(&mut self, doc: DocId) {
        if let Some(&node) = self.by_doc.get(&doc) {
            if !self.deleted[node as usize] {
                self.deleted[node as usize] = true;
                self.deleted_count += 1;
            }
            self.by_doc.remove(&doc);
        }
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
        let Some(entry) = self.entry else {
            return Vec::new();
        };
        if k == 0 {
            return Vec::new();
        }
        let q = self.prepare_query(query);
        let ef = ef.unwrap_or(self.spec.ef_search).max(k);

        let cur = self.descend(&q, entry, self.max_level, 0);
        let found = self.search_layer(&q, &[cur], ef, 0);

        let mut out = Vec::with_capacity(k);
        for c in found {
            if self.is_deleted(c.node) {
                continue;
            }
            let doc = self.doc_ids[c.node as usize];
            if !accept(doc) {
                continue;
            }
            out.push((doc, score_from_distance(self.spec.metric, c.dist)));
            if out.len() == k {
                break;
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
        let mut out = Vec::with_capacity(self.doc_ids.len() * 96);
        let quant = self.spec.quant;
        out.push(if quant == Quant::None {
            GRAPH_VERSION
        } else {
            GRAPH_VERSION_QUANT
        });
        put_uvarint(&mut out, self.dim as u64);
        out.push(match self.spec.metric {
            Metric::Cosine => 0,
            Metric::L2 => 1,
            Metric::Dot => 2,
        });
        put_uvarint(&mut out, self.spec.m as u64);
        put_uvarint(&mut out, self.spec.ef_construction as u64);
        put_uvarint(&mut out, self.spec.ef_search as u64);
        out.push(match self.prec {
            VecPrec::F32 => 0,
            VecPrec::F16 => 1,
        });
        if quant != Quant::None {
            out.push(quant.code());
        }
        put_uvarint(&mut out, self.doc_ids.len() as u64);
        put_uvarint(&mut out, self.entry.map(|e| e as u64 + 1).unwrap_or(0));
        put_uvarint(&mut out, self.max_level as u64);
        for (node, &doc) in self.doc_ids.iter().enumerate() {
            let node = node as u32;
            put_uvarint(&mut out, doc);
            out.push(self.is_deleted(node) as u8);
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
            let mut sorted: Vec<u32> = Vec::with_capacity(self.m0);
            for l in 0..levels {
                let nbs = self.neighbors(node, l);
                put_uvarint(&mut out, nbs.len() as u64);
                // The neighbour list is a *set*; its order carries no meaning.
                // Sorting and delta coding shortens the varints: in a 100k
                // node graph a raw id is 3 bytes, a delta about 2.
                sorted.clear();
                sorted.extend_from_slice(nbs);
                sorted.sort_unstable();
                let mut prev = 0u32;
                for &nb in &sorted {
                    put_uvarint(&mut out, (nb - prev) as u64);
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
        mut lookup: impl FnMut(DocId, &mut Vec<f32>) -> bool,
    ) -> Option<VectorIndex> {
        let mut pos = 0usize;
        let version = *bytes.first()?;
        if version != GRAPH_VERSION && version != GRAPH_VERSION_QUANT {
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
        if version == GRAPH_VERSION_QUANT {
            spec.quant = Quant::from_code(*bytes.get(pos)?)?;
            pos += 1;
        }
        let count = get_uvarint(bytes, &mut pos).ok()? as usize;
        let entry_raw = get_uvarint(bytes, &mut pos).ok()?;
        let max_level = get_uvarint(bytes, &mut pos).ok()? as usize;

        let mut ix = VectorIndex::with_precision(dim, spec, prec);
        ix.reserve(count);
        ix.max_level = max_level;
        ix.entry = if entry_raw == 0 {
            None
        } else {
            Some((entry_raw - 1) as u32)
        };

        let mut raw: Vec<f32> = Vec::with_capacity(dim);
        let stored = ix.data.stored_len(dim);
        for node in 0..count {
            let doc = get_uvarint(bytes, &mut pos).ok()?;
            let is_deleted = *bytes.get(pos)? != 0;
            pos += 1;
            let levels = get_uvarint(bytes, &mut pos).ok()? as usize;
            if levels == 0 {
                return None;
            }
            let n = if is_deleted {
                let v = bytes.get(pos..pos + stored)?;
                pos += stored;
                ix.alloc_node_stored(doc, v, levels - 1)
            } else {
                if !lookup(doc, &mut raw) || raw.len() != dim {
                    return None;
                }
                ix.alloc_node(doc, &raw, levels - 1)
            };
            debug_assert_eq!(n as usize, node);
            if is_deleted {
                ix.deleted[node] = true;
                ix.deleted_count += 1;
            } else {
                ix.by_doc.insert(doc, node as u32);
            }

            for l in 0..levels {
                let k = get_uvarint(bytes, &mut pos).ok()? as usize;
                if k > count {
                    return None; // corrupt length
                }
                let mut lvl = Vec::with_capacity(k);
                let mut prev = 0u64;
                for _ in 0..k {
                    let delta = get_uvarint(bytes, &mut pos).ok()?;
                    let nb = prev.checked_add(delta)?;
                    if nb as usize >= count {
                        return None; // out-of-range link: the graph is corrupt
                    }
                    prev = nb;
                    lvl.push(nb as u32);
                }
                if l == 0 && k > ix.m0 {
                    return None; // level-0 degree exceeds the arena stride
                }
                ix.set_neighbors(node as u32, l, &lvl);
            }
        }

        if ix.entry.map(|e| e as usize >= count).unwrap_or(false) {
            return None;
        }
        Some(ix)
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
        let q = self.prepare_query(query);
        let mut all: Vec<Cand> = ids
            .iter()
            .filter_map(|doc| self.by_doc.get(doc).copied())
            .filter(|n| !self.is_deleted(*n))
            .map(|n| Cand {
                dist: self.dist_to(&q, n),
                node: n,
            })
            .collect();
        all.sort();
        all.truncate(k);
        all.into_iter()
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
        let q = self.prepare_query(query);
        let mut all: Vec<Cand> = (0..self.doc_ids.len() as u32)
            .filter(|n| !self.is_deleted(*n))
            .filter(|n| accept(self.doc_ids[*n as usize]))
            .map(|n| Cand {
                dist: self.dist_to(&q, n),
                node: n,
            })
            .collect();
        all.sort();
        all.truncate(k);
        all.into_iter()
            .map(|c| {
                (
                    self.doc_ids[c.node as usize],
                    score_from_distance(self.spec.metric, c.dist),
                )
            })
            .collect()
    }
}

#[cfg(test)]
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
            for i in 0..300u64 {
                let v: Vec<f32> = (0..8).map(|_| rng.next_f32() - 0.5).collect();
                ix.insert(i, &v);
                vecs.push(Some(v));
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
                vecs[i as usize] = Some(v);
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

    /// A graph over codes goes through the file as one over vectors does:
    /// version 4, the quantization with it, a tombstone's code travelling
    /// with it; a graph over vectors stays version 3.
    #[test]
    fn a_graph_over_codes_round_trips() {
        let mut r = Rng(42);
        let docs: Vec<Vec<f32>> = (0..600)
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
            assert_eq!(bytes[0], if quant == Quant::None { 3 } else { 4 });
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
}
