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
use crate::schema::{Metric, VectorIndexSpec};
use crate::value::{DocId, VecPrec};
use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

// -------------------------------------------------------------- metrics

#[inline]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    // Strips of 8: no bounds check in the loop body, LLVM turns this
    // straight into SIMD. The accumulator array breaks the dependency chain.
    let mut acc = [0.0f32; 8];
    let mut ia = a.chunks_exact(8);
    let mut ib = b.chunks_exact(8);
    for (x, y) in ia.by_ref().zip(ib.by_ref()) {
        for k in 0..8 {
            acc[k] += x[k] * y[k];
        }
    }
    let mut s = (acc[0] + acc[1]) + (acc[2] + acc[3]) + ((acc[4] + acc[5]) + (acc[6] + acc[7]));
    for (x, y) in ia.remainder().iter().zip(ib.remainder()) {
        s += x * y;
    }
    s
}

#[inline]
pub fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut acc = [0.0f32; 8];
    let mut ia = a.chunks_exact(8);
    let mut ib = b.chunks_exact(8);
    for (x, y) in ia.by_ref().zip(ib.by_ref()) {
        for k in 0..8 {
            let d = x[k] - y[k];
            acc[k] += d * d;
        }
    }
    let mut s = (acc[0] + acc[1]) + (acc[2] + acc[3]) + ((acc[4] + acc[5]) + (acc[6] + acc[7]));
    for (x, y) in ia.remainder().iter().zip(ib.remainder()) {
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
fn half(x: u16) -> f32 {
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

#[inline]
fn distance_hf(metric: Metric, a: &[u16], b: &[f32]) -> f32 {
    match metric {
        Metric::Cosine => 1.0 - strip8!(a, b, half, ident, mul, 0),
        Metric::L2 => strip8!(a, b, half, ident, diff_sq, 0),
        Metric::Dot => -strip8!(a, b, half, ident, mul, 0),
    }
}

#[inline]
fn distance_hh(metric: Metric, a: &[u16], b: &[u16]) -> f32 {
    match metric {
        Metric::Cosine => 1.0 - strip8!(a, b, half, half, mul, 0),
        Metric::L2 => strip8!(a, b, half, half, diff_sq, 0),
        Metric::Dot => -strip8!(a, b, half, half, mul, 0),
    }
}

// ----------------------------------------------------------------- arena

/// Vector arena: every vector in one contiguous array, strided by `node * dim`.
///
/// The `F16` variant fits the same array into half the space. The query side
/// is always f32; widening happens inside the distance kernel, so there is no
/// extra allocation on the search path.
pub(crate) enum Arena {
    F32(Vec<f32>),
    F16(Vec<u16>),
}

impl Arena {
    fn new(prec: VecPrec) -> Arena {
        match prec {
            VecPrec::F32 => Arena::F32(Vec::new()),
            VecPrec::F16 => Arena::F16(Vec::new()),
        }
    }

    pub(crate) fn prec(&self) -> VecPrec {
        match self {
            Arena::F32(_) => VecPrec::F32,
            Arena::F16(_) => VecPrec::F16,
        }
    }

    fn reserve(&mut self, n: usize) {
        match self {
            Arena::F32(d) => d.reserve(n),
            Arena::F16(d) => d.reserve(n),
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
        }
    }

    #[inline]
    fn slice_f32(&self, node: u32, dim: usize) -> Option<&[f32]> {
        match self {
            Arena::F32(d) => {
                let s = node as usize * dim;
                Some(&d[s..s + dim])
            }
            Arena::F16(_) => None,
        }
    }

    #[inline]
    fn dist_to(&self, metric: Metric, q: &[f32], node: u32, dim: usize) -> f32 {
        let s = node as usize * dim;
        match self {
            Arena::F32(d) => distance(metric, q, &d[s..s + dim]),
            Arena::F16(d) => distance_hf(metric, &d[s..s + dim], q),
        }
    }

    #[inline]
    fn dist_nodes(&self, metric: Metric, a: u32, b: u32, dim: usize) -> f32 {
        let (sa, sb) = (a as usize * dim, b as usize * dim);
        match self {
            Arena::F32(d) => distance(metric, &d[sa..sa + dim], &d[sb..sb + dim]),
            Arena::F16(d) => distance_hh(metric, &d[sa..sa + dim], &d[sb..sb + dim]),
        }
    }

    /// Returns the node's vector as f32. A borrow in the f32 arena, a widened
    /// copy in f16 -- the caller uses it like a slice either way.
    #[inline]
    fn vec_at(&self, node: u32, dim: usize) -> Cow<'_, [f32]> {
        match self {
            Arena::F32(_) => Cow::Borrowed(self.slice_f32(node, dim).unwrap()),
            Arena::F16(d) => {
                let s = node as usize * dim;
                Cow::Owned(d[s..s + dim].iter().map(|x| half(*x)).collect())
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
        }
    }

    /// Bytes the arena occupies in memory (for statistics).
    pub(crate) fn bytes(&self) -> usize {
        match self {
            Arena::F32(d) => d.len() * 4,
            Arena::F16(d) => d.len() * 2,
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

/// Version of the serialised graph format.
const GRAPH_VERSION: u8 = 2;

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
        let decode = matches!(self.data, Arena::F16(_));
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
        out
    }

    /// Computes a node's neighbour candidates across every level.
    /// It only reads, so it can be run in parallel.
    fn candidates_for(
        &self,
        sc: &mut Scratch,
        node: u32,
        level: usize,
        ef_construction: usize,
        m: usize,
    ) -> Vec<(usize, Vec<u32>)> {
        let Some(entry) = self.entry else {
            return Vec::new();
        };
        let v = self.vec_at(node);
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
    /// Precision comes from the field type (`vector<N, f16>` -> half size).
    data: Arena,
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
            data: Arena::new(prec),
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
        self.data.reserve(n * self.dim);
        // (the arena allocates in its own element type)
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
        self.data.prec()
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
        let node = self.doc_ids.len() as u32;
        self.data.push(raw, self.spec.metric == Metric::Cosine);
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
        // If the same document was written again, tombstone the old one.
        if let Some(&old) = self.by_doc.get(&doc) {
            if !self.deleted[old as usize] {
                self.deleted[old as usize] = true;
                self.deleted_count += 1;
            }
        }
        let level = self.random_level();
        let node = self.alloc_node(doc, raw, level);
        self.by_doc.insert(doc, node);
        self.link_node(node, level, None);
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
        pending: &[(u32, usize)],
        _threads: usize,
    ) -> Vec<(u32, usize, Vec<(usize, Vec<u32>)>)> {
        let (efc, m) = (self.spec.ef_construction, self.spec.m);
        let view = self.view();
        let mut sc = Scratch::new();
        pending
            .iter()
            .map(|&(node, level)| {
                (
                    node,
                    level,
                    view.candidates_for(&mut sc, node, level, efc, m),
                )
            })
            .collect()
    }

    /// Computes the candidates split across threads. Every thread carries its
    /// own `Scratch` buffer; the graph is read-only at this stage.
    #[cfg(not(target_family = "wasm"))]
    fn compute_candidates(
        &self,
        pending: &[(u32, usize)],
        threads: usize,
    ) -> Vec<(u32, usize, Vec<(usize, Vec<u32>)>)> {
        let (efc, m) = (self.spec.ef_construction, self.spec.m);
        let view = self.view();
        let per = pending.len().div_ceil(threads);
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for part in pending.chunks(per) {
                handles.push(scope.spawn(move || {
                    let mut sc = Scratch::new();
                    part.iter()
                        .map(|&(node, level)| {
                            (
                                node,
                                level,
                                view.candidates_for(&mut sc, node, level, efc, m),
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
            let mut pending: Vec<(u32, usize)> = Vec::with_capacity(chunk.len());
            for (doc, v) in chunk {
                if v.len() != self.dim {
                    continue;
                }
                if let Some(&old) = self.by_doc.get(doc) {
                    if !self.deleted[old as usize] {
                        self.deleted[old as usize] = true;
                        self.deleted_count += 1;
                    }
                }
                let level = self.random_level();
                let node = self.alloc_node(*doc, v, level);
                self.by_doc.insert(*doc, node);
                pending.push((node, level));
            }
            if pending.is_empty() {
                continue;
            }
            if self.entry.is_none() {
                // The first node becomes the entry point; the rest go in serially.
                let (first, level) = pending[0];
                self.entry = Some(first);
                self.max_level = level;
                for &(node, level) in &pending[1..] {
                    self.link_node(node, level, None);
                }
                continue;
            }

            // 2) Parallel: compute the candidates (the graph does not change).
            let mut computed = self.compute_candidates(&pending, threads);

            // 3) Serial: write the links and prune the back-links.
            computed.sort_by_key(|(node, _, _)| *node);
            for (node, level, per_level) in computed {
                self.link_node(node, level, Some(per_level));
            }
        }
    }

    /// Links the computed candidates into the graph (computing them itself
    /// when none are supplied).
    fn link_node(&mut self, node: u32, level: usize, precomputed: Option<Vec<(usize, Vec<u32>)>>) {
        let Some(entry) = self.entry else {
            self.entry = Some(node);
            self.max_level = level;
            return;
        };

        let per_level: Vec<(usize, Vec<u32>)> = match precomputed {
            Some(p) => p,
            None => {
                let v: Vec<f32> = self.vec_at(node).into_owned();
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
        out.push(GRAPH_VERSION);
        put_uvarint(&mut out, self.dim as u64);
        out.push(match self.spec.metric {
            Metric::Cosine => 0,
            Metric::L2 => 1,
            Metric::Dot => 2,
        });
        put_uvarint(&mut out, self.spec.m as u64);
        put_uvarint(&mut out, self.spec.ef_construction as u64);
        put_uvarint(&mut out, self.spec.ef_search as u64);
        put_uvarint(&mut out, self.doc_ids.len() as u64);
        put_uvarint(&mut out, self.entry.map(|e| e as u64 + 1).unwrap_or(0));
        put_uvarint(&mut out, self.max_level as u64);
        for (node, &doc) in self.doc_ids.iter().enumerate() {
            let node = node as u32;
            put_uvarint(&mut out, doc);
            out.push(self.is_deleted(node) as u8);
            let levels = self.node_levels(node) + 1; // level 0 included
            put_uvarint(&mut out, levels as u64);
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
        mut lookup: impl FnMut(DocId, &mut Vec<f32>) -> bool,
    ) -> Option<VectorIndex> {
        let mut pos = 0usize;
        if *bytes.first()? != GRAPH_VERSION {
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
        let spec = VectorIndexSpec {
            metric,
            m: get_uvarint(bytes, &mut pos).ok()? as usize,
            ef_construction: get_uvarint(bytes, &mut pos).ok()? as usize,
            ef_search: get_uvarint(bytes, &mut pos).ok()? as usize,
        };
        let count = get_uvarint(bytes, &mut pos).ok()? as usize;
        let entry_raw = get_uvarint(bytes, &mut pos).ok()?;
        let max_level = get_uvarint(bytes, &mut pos).ok()? as usize;

        let mut ix = VectorIndex::new(dim, spec);
        ix.reserve(count);
        ix.max_level = max_level;
        ix.entry = if entry_raw == 0 {
            None
        } else {
            Some((entry_raw - 1) as u32)
        };

        let mut raw: Vec<f32> = Vec::with_capacity(dim);
        for node in 0..count {
            let doc = get_uvarint(bytes, &mut pos).ok()?;
            let is_deleted = *bytes.get(pos)? != 0;
            pos += 1;

            if !lookup(doc, &mut raw) || raw.len() != dim {
                return None;
            }
            let levels = get_uvarint(bytes, &mut pos).ok()? as usize;
            if levels == 0 {
                return None;
            }
            let n = ix.alloc_node(doc, &raw, levels - 1);
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
        let restored = VectorIndex::restore_graph(&bytes, 8, fetch).expect("restore failed");
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
        assert!(VectorIndex::restore_graph(&bytes, 16, fetch).is_none());
        // A missing document must be rejected
        assert!(
            VectorIndex::restore_graph(&bytes, 8, |d: u64, out: &mut Vec<f32>| {
                if d == 7 {
                    false
                } else {
                    fetch(d, out)
                }
            })
            .is_none()
        );
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
}
