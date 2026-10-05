//! HyperLogLog: a distinct count in fixed memory, and sketches that merge.
//!
//! `count(distinct ...)` keeps every value it has seen and refuses past a
//! million of them -- a month of an analytics site's visitors was 2.4
//! million -- and a day's count cannot be added to the next day's.
//! `approx_count_distinct(e)` keeps a sketch instead: 2^14 registers of the
//! longest run of zeros a hashed value began with, a standard error of
//! 1.04 / 2^7, about 0.81%, in 16 KB a group however many values come.
//! `hll_accumulate(e)` answers the sketch itself as bytes, `hll_combine(s)`
//! merges sketches -- a register each, the larger -- and `hll_estimate(s)`
//! reads a count off one, so days kept as sketches count a month.
//!
//! A sketch starts sparse, the registers it has set as `(index, rank)` pairs
//! -- a group of a few values costs a few words, not 16 KB -- and turns
//! dense once those would take a quarter of the dense registers. The count
//! is Ertl's improved raw estimate ("New cardinality estimation algorithms
//! for HyperLogLog sketches", 2017), which needs neither the bias tables of
//! HLL++ nor a switch to linear counting at small counts. The hash is this
//! module's own, the same on every target, so a sketch the browser module
//! made merges with a server's.

use crate::error::{Error, Result};

/// The registers' index bits: 2^14 of them.
pub const P: u32 = 14;
const M: usize = 1 << P;
/// The rank past every hash bit after the index, the most a register holds.
const Q: u32 = 64 - P;
/// A sketch's bytes start with this, then `P`: dense, then the registers;
/// sparse, then `(index: u16 le, rank: u8)` triples, ascending.
const DENSE: u8 = 1;
const SPARSE: u8 = 2;
/// The most the sketches of one query may hold, past which it is refused
/// rather than answered short: 4 096 dense ones.
pub const MAX_BYTES: usize = 64 << 20;
/// Pairs held sparse before the registers are made dense: a quarter of
/// their bytes.
const SPARSE_MOST: usize = M / 16;

/// A HyperLogLog sketch of `P` index bits.
#[derive(Clone, Debug, PartialEq)]
pub enum Sketch {
    /// `index << 8 | rank`, ascending by index, one a register set.
    Sparse(Vec<u32>),
    Dense(Box<[u8]>),
}

impl Default for Sketch {
    fn default() -> Self {
        Sketch::Sparse(Vec::new())
    }
}

/// A value's hash: 64 bits, each word mixed in and the whole avalanched,
/// so the register index and the run of zeros after it are uniform. Its
/// own rather than the maps', which is keyed at random natively and a
/// word at a time in the browser: a sketch has to mean the same thing
/// wherever it was made.
pub fn hash(bytes: &[u8]) -> u64 {
    let mut h = 0x9e37_79b9_7f4a_7c15u64 ^ (bytes.len() as u64).wrapping_mul(0xff51_afd7_ed55_8ccd);
    let mut rest = bytes;
    while let Some((w, r)) = rest.split_first_chunk::<8>() {
        h = mix(h ^ u64::from_le_bytes(*w));
        rest = r;
    }
    if !rest.is_empty() {
        let mut w = [0u8; 8];
        w[..rest.len()].copy_from_slice(rest);
        h = mix(h ^ u64::from_le_bytes(w));
    }
    mix(h)
}

/// MurmurHash3's finalizer: every bit of `x` moves every bit of the result.
fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^ (x >> 33)
}

impl Sketch {
    /// The bytes it holds, about: what a group's sketch costs.
    pub fn bytes(&self) -> usize {
        match self {
            Sketch::Sparse(v) => v.capacity() * 4,
            Sketch::Dense(_) => M,
        }
    }

    /// Adds a value by its hash.
    pub fn add(&mut self, h: u64) {
        let index = (h >> Q) as u32;
        // The run of zeros after the index bits, plus one; a hash whose
        // every such bit is zero ranks past them all.
        let rank = ((h << P) | (1 << (P - 1))).leading_zeros().min(Q) + 1;
        self.set(index, rank as u8);
    }

    fn set(&mut self, index: u32, rank: u8) {
        match self {
            Sketch::Dense(r) => {
                let slot = &mut r[index as usize];
                *slot = (*slot).max(rank);
            }
            Sketch::Sparse(v) => {
                match v.binary_search_by_key(&index, |e| e >> 8) {
                    Ok(i) => v[i] = v[i].max(index << 8 | rank as u32),
                    Err(i) => v.insert(i, index << 8 | rank as u32),
                }
                if v.len() > SPARSE_MOST {
                    self.densify();
                }
            }
        }
    }

    fn densify(&mut self) {
        if let Sketch::Sparse(v) = self {
            let mut r = vec![0u8; M].into_boxed_slice();
            for e in v.iter() {
                r[(e >> 8) as usize] = *e as u8;
            }
            *self = Sketch::Dense(r);
        }
    }

    /// Takes `other` in: each register the larger of the two.
    pub fn merge(&mut self, other: &Sketch) {
        match other {
            Sketch::Sparse(v) => {
                for e in v {
                    self.set(e >> 8, *e as u8);
                }
            }
            Sketch::Dense(o) => {
                self.densify();
                if let Sketch::Dense(r) = self {
                    for (a, b) in r.iter_mut().zip(o.iter()) {
                        *a = (*a).max(*b);
                    }
                }
            }
        }
    }

    /// How many distinct values went in, estimated.
    pub fn estimate(&self) -> u64 {
        // How many registers hold each rank, 0 to Q + 1.
        let mut c = [0u32; Q as usize + 2];
        match self {
            Sketch::Sparse(v) => {
                c[0] = (M - v.len()) as u32;
                for e in v {
                    c[(*e as u8) as usize] += 1;
                }
            }
            Sketch::Dense(r) => {
                for &x in r.iter() {
                    c[x as usize] += 1;
                }
            }
        }
        let m = M as f64;
        let mut z = m * tau(1.0 - c[Q as usize + 1] as f64 / m);
        for k in (1..=Q as usize).rev() {
            z = 0.5 * (z + c[k] as f64);
        }
        z += m * sigma(c[0] as f64 / m);
        // 1 / (2 ln 2): the estimator's constant as the registers go to
        // infinity, which the corrections above make right at every count.
        let alpha = 0.721_347_520_444_481_7;
        (alpha * m * m / z).round() as u64
    }

    /// The sketch as bytes, sparse while that is shorter.
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Sketch::Sparse(v) => {
                let mut out = Vec::with_capacity(2 + 3 * v.len());
                out.extend_from_slice(&[SPARSE, P as u8]);
                for e in v {
                    out.extend_from_slice(&((e >> 8) as u16).to_le_bytes());
                    out.push(*e as u8);
                }
                out
            }
            Sketch::Dense(r) => {
                let mut out = Vec::with_capacity(2 + M);
                out.extend_from_slice(&[DENSE, P as u8]);
                out.extend_from_slice(r);
                out
            }
        }
    }

    /// A sketch `to_bytes` wrote, or the refusal of anything else.
    pub fn from_bytes(b: &[u8]) -> Result<Sketch> {
        let bad = || {
            Error::Type(
                "not a sketch: hll_combine and hll_estimate take what hll_accumulate makes".into(),
            )
        };
        match b {
            [DENSE, p, r @ ..] if *p as u32 == P && r.len() == M => {
                if r.iter().any(|&x| x as u32 > Q + 1) {
                    return Err(bad());
                }
                Ok(Sketch::Dense(r.into()))
            }
            [SPARSE, p, r @ ..] if *p as u32 == P && r.len() % 3 == 0 => {
                let mut v = Vec::with_capacity(r.len() / 3);
                for t in r.as_chunks::<3>().0 {
                    let (index, rank) = (u16::from_le_bytes([t[0], t[1]]) as u32, t[2]);
                    if index as usize >= M
                        || rank == 0
                        || rank as u32 > Q + 1
                        || v.last().is_some_and(|l: &u32| l >> 8 >= index)
                    {
                        return Err(bad());
                    }
                    v.push(index << 8 | rank as u32);
                }
                Ok(Sketch::Sparse(v))
            }
            _ => Err(bad()),
        }
    }
}

/// Ertl's `sigma`: the registers still zero, a share `x` of them.
fn sigma(mut x: f64) -> f64 {
    if x == 1.0 {
        return f64::INFINITY;
    }
    let (mut y, mut z) = (1.0, x);
    loop {
        x *= x;
        let before = z;
        z += x * y;
        y += y;
        if z == before {
            return z;
        }
    }
}

/// Ertl's `tau`: the registers at the most rank, a share `1 - x` of them.
fn tau(mut x: f64) -> f64 {
    if x == 0.0 || x == 1.0 {
        return 0.0;
    }
    let (mut y, mut z) = (1.0, 1.0 - x);
    loop {
        x = x.sqrt();
        let before = z;
        y *= 0.5;
        z -= (1.0 - x) * (1.0 - x) * y;
        if z == before {
            return z / 3.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The estimate against the exact count of random distinct values, from
    /// one to two million: within 3% at every count -- about four standard
    /// errors -- and within 1% averaged over the counts past a thousand.
    /// The values and the hash are fixed, so the answers are the same every
    /// run.
    #[test]
    fn the_estimate_is_within_its_error_of_the_exact_count() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let (mut s, mut n, mut sum, mut counted) = (Sketch::default(), 0u64, 0.0, 0);
        let mut seen = std::collections::HashSet::new();
        let marks = [
            1, 2, 5, 10, 50, 100, 500, 1_000, 3_000, 10_000, 30_000, 100_000, 300_000, 1_000_000,
            2_000_000,
        ];
        for &mark in &marks {
            while n < mark {
                // Values as a column holds them: short texts, encoded.
                let v = format!("user-{}", next() % 50_000_000);
                if seen.insert(v.clone()) {
                    s.add(hash(v.as_bytes()));
                    n += 1;
                }
            }
            let e = s.estimate() as f64;
            let err = (e - n as f64).abs() / n as f64;
            assert!(
                err < 0.03,
                "{n} values estimated {e}, {:.2}% off",
                err * 100.0
            );
            if n >= 1_000 {
                sum += err;
                counted += 1;
            }
            // Its bytes read back to the same sketch.
            assert_eq!(Sketch::from_bytes(&s.to_bytes()).unwrap(), s);
        }
        assert!(
            sum / (counted as f64) < 0.01,
            "{:.3}%",
            100.0 * sum / counted as f64
        );
        assert!(matches!(s, Sketch::Dense(_)));
        assert_eq!(Sketch::default().estimate(), 0);
    }

    /// Thirty sketches of 200 000 values each: the root mean square of their
    /// errors is the standard error, 1.04 / 2^7 = 0.81% (0.77% here), and
    /// they are not biased.
    #[test]
    fn the_error_over_many_sketches_is_the_standard_error() {
        let mut errs = Vec::new();
        for run in 0..30u64 {
            let mut s = Sketch::default();
            for i in 0..200_000u64 {
                s.add(hash(format!("r{run}-user-{i}").as_bytes()));
            }
            errs.push((s.estimate() as f64 - 200_000.0) / 200_000.0 * 100.0);
        }
        let mean = errs.iter().map(|e| e.abs()).sum::<f64>() / errs.len() as f64;
        let rms = (errs.iter().map(|e| e * e).sum::<f64>() / errs.len() as f64).sqrt();
        let bias = errs.iter().sum::<f64>() / errs.len() as f64;
        assert!(
            mean < 0.8 && rms < 1.0 && bias.abs() < 0.3,
            "{mean} {rms} {bias}"
        );
    }

    /// Two sketches merged estimate their union, whichever is sparse.
    #[test]
    fn merged_sketches_count_the_union() {
        let (mut a, mut b, mut all) = (Sketch::default(), Sketch::default(), Sketch::default());
        for i in 0..60_000u64 {
            let h = hash(&i.to_le_bytes());
            all.add(h);
            match i % 3 {
                0 => a.add(h),
                1 => b.add(h),
                _ => {
                    a.add(h);
                    b.add(h)
                }
            }
        }
        let mut few = Sketch::default();
        for i in 0..50u64 {
            few.add(hash(&i.to_le_bytes()));
        }
        let mut u = a.clone();
        u.merge(&b);
        assert_eq!(u, all);
        let mut v = few.clone();
        v.merge(&all);
        assert_eq!(v, all);
        let mut w = all.clone();
        w.merge(&few);
        assert_eq!(w, all);
        assert!(Sketch::from_bytes(&[3, 14]).is_err());
        assert!(Sketch::from_bytes(&[SPARSE, 14, 5, 0]).is_err());
        assert!(Sketch::from_bytes(b"hello").is_err());
    }
}
