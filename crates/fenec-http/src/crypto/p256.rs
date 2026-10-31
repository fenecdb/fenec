//! ES256: ECDSA over NIST P-256 with SHA-256 (FIPS 186-5 §6.4.2, SEC 1
//! §4.1.4), the signature as RFC 7518 §3.4 writes it -- `R || S`, 32
//! bytes each, never DER. Verification alone: an identity provider signs
//! with its private key, a server checks with the public one, so every
//! value here is public and nothing needs to run in constant time.
//!
//! Written to be read rather than to be fast: field elements (mod p) and
//! scalars (mod n) are four 64-bit limbs in Montgomery form under one
//! [`Modulus`], points are Jacobian, and `u1·G + u2·Q` is Shamir's trick
//! over an addition that meets doubling and the point at infinity, the
//! cases a hand-written verifier gets wrong. A verification takes some
//! 5 000 field multiplications, 145 us on an Apple M1 where RS256 takes 37
//! (`make jwt-bench`); a server keeps the tokens it verified
//! (`Access::verified`), so a client's token pays it once a server.
//!
//! Held to every case of Project Wycheproof's ECDSA P-256 SHA-256 vectors
//! in the P1363 encoding JWS uses, to RFC 7515's A.3 example and to tokens
//! node:crypto signed (`tests/es256.rs`), and the curve's own identities
//! below: hand-written crypto is only usable when it can be proven against
//! known vectors.

use super::sha256;

/// 256 bits, least significant limb first.
type Limbs = [u64; 4];

const ZERO: Limbs = [0; 4];

/// The field's prime, `p = 2^256 - 2^224 + 2^192 + 2^96 - 1`.
const P: Modulus = Modulus::new([
    0xffff_ffff_ffff_ffff,
    0x0000_0000_ffff_ffff,
    0x0000_0000_0000_0000,
    0xffff_ffff_0000_0001,
]);

/// The group's order: `n·G` is the point at infinity. Prime, and the
/// curve's cofactor is 1, so every point of the curve but infinity
/// generates the whole group -- a public key needs no `n·Q` check.
const N: Modulus = Modulus::new([
    0xf3b9_cac2_fc63_2551,
    0xbce6_faad_a717_9e84,
    0xffff_ffff_ffff_ffff,
    0xffff_ffff_0000_0000,
]);

/// The curve is `y² = x³ - 3x + b`.
const B: Limbs = [
    0x3bce_3c3e_27d2_604b,
    0x651d_06b0_cc53_b0f6,
    0xb3eb_bd55_7698_86bc,
    0x5ac6_35d8_aa3a_93e7,
];

/// The base point G.
const GX: Limbs = [
    0xf4a1_3945_d898_c296,
    0x7703_7d81_2deb_33a0,
    0xf8bc_e6e5_63a4_40f2,
    0x6b17_d1f2_e12c_4247,
];
const GY: Limbs = [
    0xcbb6_4068_37bf_51f5,
    0x2bce_3357_6b31_5ece,
    0x8ee7_eb4a_7c0f_9e16,
    0x4fe3_42e2_fe1a_7f9b,
];

/// 32 big-endian bytes as limbs; anything longer or shorter is `None`.
fn limbs(be: &[u8]) -> Option<Limbs> {
    let be: &[u8; 32] = be.try_into().ok()?;
    let mut out = ZERO;
    for (i, limb) in out.iter_mut().enumerate() {
        let at = 32 - 8 * (i + 1);
        *limb = u64::from_be_bytes([
            be[at],
            be[at + 1],
            be[at + 2],
            be[at + 3],
            be[at + 4],
            be[at + 5],
            be[at + 6],
            be[at + 7],
        ]);
    }
    Some(out)
}

fn bit(a: &Limbs, i: usize) -> bool {
    a[i / 64] >> (i % 64) & 1 == 1
}

/// `a >= b`.
const fn geq(a: &Limbs, b: &Limbs) -> bool {
    let mut i = 4;
    while i > 0 {
        i -= 1;
        if a[i] != b[i] {
            return a[i] > b[i];
        }
    }
    true
}

/// `a + b`, and whether it carried out of 256 bits.
const fn add_carry(a: &Limbs, b: &Limbs) -> (Limbs, bool) {
    let mut out = ZERO;
    let mut carry = false;
    let mut i = 0;
    while i < 4 {
        let (s, o1) = a[i].overflowing_add(b[i]);
        let (s, o2) = s.overflowing_add(carry as u64);
        out[i] = s;
        carry = o1 || o2;
        i += 1;
    }
    (out, carry)
}

/// `a - b` modulo 2^256, and whether it borrowed.
const fn sub_borrow(a: &Limbs, b: &Limbs) -> (Limbs, bool) {
    let mut out = ZERO;
    let mut borrow = false;
    let mut i = 0;
    while i < 4 {
        let (d, o1) = a[i].overflowing_sub(b[i]);
        let (d, o2) = d.overflowing_sub(borrow as u64);
        out[i] = d;
        borrow = o1 || o2;
        i += 1;
    }
    (out, borrow)
}

/// An odd modulus of 256 bits and what Montgomery multiplication by it
/// needs. A number `a` is held as `a·R mod m`, `R = 2^256`, so a product
/// needs no division by `m`, only by `R`, which is a shift. Worked out at
/// compile time from `m` alone rather than written as constants: nothing
/// here a reader has to take on trust.
struct Modulus {
    m: Limbs,
    /// `-m^-1 mod 2^64`.
    inv: u64,
    /// `R mod m`: 1 in Montgomery form.
    one: Limbs,
    /// `R² mod m`: a Montgomery product with it takes `a` to `a·R`.
    r2: Limbs,
}

impl Modulus {
    const fn new(m: Limbs) -> Modulus {
        // Newton's iteration for m^-1 mod 2^64: each step doubles the
        // bits that are right, and 1 is right in the lowest of an odd m.
        let mut inv = 1u64;
        let mut i = 0;
        while i < 6 {
            inv = inv.wrapping_mul(2u64.wrapping_sub(m[0].wrapping_mul(inv)));
            i += 1;
        }
        // 2^256 and 2^512 mod m, doubling 1 that many times. A doubling
        // that carries out of 256 bits is past m, and subtracting m modulo
        // 2^256 gives the right value, which is below m.
        let mut x: Limbs = [1, 0, 0, 0];
        let mut one = ZERO;
        let mut i = 0;
        while i < 512 {
            let (d, carry) = add_carry(&x, &x);
            x = if carry || geq(&d, &m) {
                sub_borrow(&d, &m).0
            } else {
                d
            };
            i += 1;
            if i == 256 {
                one = x;
            }
        }
        Modulus {
            m,
            inv: inv.wrapping_neg(),
            one,
            r2: x,
        }
    }

    /// `a·b/R mod m`, both below `m` (CIOS, as `RsaKey::mont`). Written
    /// again at a fixed four limbs rather than shared over slices: the
    /// compiler unrolls it, and a verification is thousands of them.
    fn mul(&self, a: &Limbs, b: &Limbs) -> Limbs {
        let m = &self.m;
        let mut t = [0u64; 6];
        for &bi in b {
            let mut c = 0u128;
            for j in 0..4 {
                let x = t[j] as u128 + a[j] as u128 * bi as u128 + c;
                t[j] = x as u64;
                c = x >> 64;
            }
            let x = t[4] as u128 + c;
            t[4] = x as u64;
            t[5] = (x >> 64) as u64;
            // The multiple of m that clears the lowest limb, then a shift
            // down by it.
            let q = t[0].wrapping_mul(self.inv) as u128;
            let mut c = (t[0] as u128 + q * m[0] as u128) >> 64;
            for j in 1..4 {
                let x = t[j] as u128 + q * m[j] as u128 + c;
                t[j - 1] = x as u64;
                c = x >> 64;
            }
            let x = t[4] as u128 + c;
            t[3] = x as u64;
            t[4] = t[5] + (x >> 64) as u64;
        }
        // Below 2m with a and b below m: one subtraction at most.
        let out = [t[0], t[1], t[2], t[3]];
        if t[4] != 0 || geq(&out, m) {
            sub_borrow(&out, m).0
        } else {
            out
        }
    }

    /// `a + b mod m`, both below `m`.
    fn add(&self, a: &Limbs, b: &Limbs) -> Limbs {
        let (s, carry) = add_carry(a, b);
        if carry || geq(&s, &self.m) {
            sub_borrow(&s, &self.m).0
        } else {
            s
        }
    }

    /// `a - b mod m`, both below `m`.
    fn sub(&self, a: &Limbs, b: &Limbs) -> Limbs {
        let (d, borrow) = sub_borrow(a, b);
        if borrow {
            add_carry(&d, &self.m).0
        } else {
            d
        }
    }

    /// `a`, below `m`, into Montgomery form.
    fn to_mont(&self, a: &Limbs) -> Limbs {
        self.mul(a, &self.r2)
    }

    /// Out of Montgomery form.
    fn to_plain(&self, a: &Limbs) -> Limbs {
        self.mul(a, &[1, 0, 0, 0])
    }

    /// `a^-1`, in Montgomery form as `a` is: `a^(m-2)` (Fermat), since m is
    /// prime. Square and multiply, a bit of the exponent at a time; `a`
    /// must not be 0, which has no inverse (it would give 0).
    fn invert(&self, a: &Limbs) -> Limbs {
        let e = sub_borrow(&self.m, &[2, 0, 0, 0]).0;
        let mut x = self.one;
        for i in (0..256).rev() {
            x = self.mul(&x, &x);
            if bit(&e, i) {
                x = self.mul(&x, a);
            }
        }
        x
    }
}

/// A point of the curve in Jacobian coordinates, each in Montgomery form
/// mod p: `(X, Y, Z)` is the affine `(X/Z², Y/Z³)`, and `Z = 0` the point
/// at infinity. Jacobian so that an addition or a doubling divides by
/// nothing; one inversion at the end takes the result back to affine.
#[derive(Clone, Copy)]
struct Point {
    x: Limbs,
    y: Limbs,
    z: Limbs,
}

impl Point {
    const INFINITY: Point = Point {
        x: ZERO,
        y: ZERO,
        z: ZERO,
    };

    /// An affine point, its coordinates plain numbers below p.
    fn affine(x: &Limbs, y: &Limbs) -> Point {
        Point {
            x: P.to_mont(x),
            y: P.to_mont(y),
            z: P.one,
        }
    }

    fn generator() -> Point {
        Point::affine(&GX, &GY)
    }

    fn is_infinity(&self) -> bool {
        self.z == ZERO
    }

    /// `2·self`, the textbook Jacobian doubling for a curve whose `a` is -3:
    /// M = 3(X - Z²)(X + Z²) = 3X² - 3Z⁴, S = 4XY², X' = M² - 2S,
    /// Y' = M(S - X') - 8Y⁴, Z' = 2YZ. A point with Y = 0 doubles to Z' = 0,
    /// infinity, as it should; P-256 has none, its order being odd.
    fn double(&self) -> Point {
        if self.is_infinity() {
            return *self;
        }
        let f = &P;
        let zz = f.mul(&self.z, &self.z);
        let m = f.mul(&f.sub(&self.x, &zz), &f.add(&self.x, &zz));
        let m = f.add(&f.add(&m, &m), &m);
        let yy = f.mul(&self.y, &self.y);
        let s = f.mul(&self.x, &yy);
        let s = f.add(&s, &s);
        let s = f.add(&s, &s);
        let x = f.sub(&f.sub(&f.mul(&m, &m), &s), &s);
        let y4 = f.mul(&yy, &yy);
        let y8 = f.add(&y4, &y4);
        let y8 = f.add(&y8, &y8);
        let y8 = f.add(&y8, &y8);
        let y = f.sub(&f.mul(&m, &f.sub(&s, &x)), &y8);
        let z = f.mul(&self.y, &self.z);
        let z = f.add(&z, &z);
        Point { x, y, z }
    }

    /// `self + o`, whatever the two are (`add-1998-cmo-2`, its terms
    /// spelled out): U1 = X1·Z2², U2 = X2·Z1², S1 = Y1·Z2³, S2 = Y2·Z1³,
    /// H = U2 - U1, R = S2 - S1, X' = R² - H³ - 2·U1·H²,
    /// Y' = R(U1·H² - X') - S1·H³, Z' = Z1·Z2·H.
    ///
    /// The formulas divide by nothing but are wrong where H = 0, the two
    /// points sharing an x: the same point, which is a doubling, or each
    /// other's negatives, whose sum is infinity. Shamir's trick meets both
    /// -- Wycheproof's `EdgeCaseShamirMultiplication` and
    /// `PointDuplication` cases are made to -- so both are asked first.
    /// Comparing U1 with U2 compares the x's: every value here is reduced
    /// below p, and Montgomery form maps each number to one value.
    fn add(&self, o: &Point) -> Point {
        if self.is_infinity() {
            return *o;
        }
        if o.is_infinity() {
            return *self;
        }
        let f = &P;
        let z1z1 = f.mul(&self.z, &self.z);
        let z2z2 = f.mul(&o.z, &o.z);
        let u1 = f.mul(&self.x, &z2z2);
        let u2 = f.mul(&o.x, &z1z1);
        let s1 = f.mul(&self.y, &f.mul(&o.z, &z2z2));
        let s2 = f.mul(&o.y, &f.mul(&self.z, &z1z1));
        if u1 == u2 {
            return if s1 == s2 {
                self.double()
            } else {
                Point::INFINITY
            };
        }
        let h = f.sub(&u2, &u1);
        let r = f.sub(&s2, &s1);
        let hh = f.mul(&h, &h);
        let hhh = f.mul(&h, &hh);
        let v = f.mul(&u1, &hh);
        let x = f.sub(&f.sub(&f.sub(&f.mul(&r, &r), &hhh), &v), &v);
        let y = f.sub(&f.mul(&r, &f.sub(&v, &x)), &f.mul(&s1, &hhh));
        let z = f.mul(&f.mul(&self.z, &o.z), &h);
        Point { x, y, z }
    }

    /// The affine x, a plain number below p; `None` for infinity.
    fn affine_x(&self) -> Option<Limbs> {
        if self.is_infinity() {
            return None;
        }
        let zi = P.invert(&self.z);
        Some(P.to_plain(&P.mul(&self.x, &P.mul(&zi, &zi))))
    }
}

/// `u1·G + u2·Q` at once (Shamir's trick, Straus's method a bit at a
/// time): from the top bit down, double, then add G, Q or `G + Q` as the
/// two scalars' bits say. 256 doublings and some 192 additions, where the
/// two products apart take 512 and 256. `gq` is `G + Q`, which may be
/// infinity (Q = -G): adding it then adds nothing, which is right.
fn shamir(u1: &Limbs, u2: &Limbs, g: &Point, q: &Point, gq: &Point) -> Point {
    let mut acc = Point::INFINITY;
    for i in (0..256).rev() {
        acc = acc.double();
        match (bit(u1, i), bit(u2, i)) {
            (true, true) => acc = acc.add(gq),
            (true, false) => acc = acc.add(g),
            (false, true) => acc = acc.add(q),
            (false, false) => {}
        }
    }
    acc
}

/// A P-256 public key, for ES256 tokens an identity provider signs: the
/// point Q, and `G + Q`, which every verification adds, worked out once a
/// key.
pub struct P256Key {
    q: Point,
    gq: Point,
}

impl P256Key {
    /// A key from its coordinates, big-endian as a JWK's `x` and `y` hold
    /// them: 32 bytes each, as RFC 7518 §6.2.1.2 has them, each below p,
    /// and a point of the curve (SEC 1 §3.2.2.1). A coordinate at or past
    /// p would be a second spelling of a smaller one, and a point off the
    /// curve lies on another curve, where the arithmetic above proves
    /// nothing; either is a file that is corrupt or was made to deceive.
    /// The point at infinity has no affine coordinates -- `(0, 0)`, which
    /// some encodings spell it as, is not on the curve, b not being 0.
    pub fn new(x: &[u8], y: &[u8]) -> Option<P256Key> {
        let (x, y) = (limbs(x)?, limbs(y)?);
        if geq(&x, &P.m) || geq(&y, &P.m) {
            return None;
        }
        let q = Point::affine(&x, &y);
        let f = &P;
        let xx = f.mul(&q.x, &q.x);
        let xxx = f.mul(&xx, &q.x);
        let three_x = f.add(&f.add(&q.x, &q.x), &q.x);
        let rhs = f.add(&f.sub(&xxx, &three_x), &f.to_mont(&B));
        if f.mul(&q.y, &q.y) != rhs {
            return None;
        }
        Some(P256Key {
            q,
            gq: Point::generator().add(&q),
        })
    }

    /// Whether `sig`, `R || S`, is this key's ECDSA signature of `msg`
    /// with SHA-256: ES256 (SEC 1 §4.1.4).
    pub fn verify_sha256(&self, msg: &[u8], sig: &[u8]) -> bool {
        // 64 bytes or none: a DER signature, or R and S of other lengths,
        // is not JWS's encoding, and taking DER too would let one signature
        // be spelled two ways.
        let (Some(r), Some(s)) = (sig.get(..32).and_then(limbs), sig.get(32..).and_then(limbs))
        else {
            return false;
        };
        // 1 <= r, s < n. Java 15 to 18 took r = s = 0 for any message
        // under any key (CVE-2022-21449, "psychic signatures"); 0 has no
        // inverse mod n, and a value at or past n is a second spelling of
        // a smaller one.
        if r == ZERO || s == ZERO || geq(&r, &N.m) || geq(&s, &N.m) {
            return false;
        }
        // e: the hash as a number. n has 256 bits as SHA-256 does, so all
        // of it, below 2^256 < 2n: reduced by one subtraction at most.
        let mut e = limbs(&sha256(msg)).unwrap_or(ZERO);
        if geq(&e, &N.m) {
            e = sub_borrow(&e, &N.m).0;
        }
        // w = s^-1, u1 = e·w and u2 = r·w mod n, out of Montgomery form
        // again for their bits.
        let w = N.invert(&N.to_mont(&s));
        let u1 = N.to_plain(&N.mul(&N.to_mont(&e), &w));
        let u2 = N.to_plain(&N.mul(&N.to_mont(&r), &w));
        let point = shamir(&u1, &u2, &Point::generator(), &self.q, &self.gq);
        // Infinity is no signature. R's x is below p, which is below 2n:
        // mod n is one subtraction at most, and Wycheproof's cases with an
        // x past n are there for the verifier that skips it.
        let Some(x) = point.affine_x() else {
            return false;
        };
        let x = if geq(&x, &N.m) {
            sub_borrow(&x, &N.m).0
        } else {
            x
        };
        x == r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn times_g(k: &Limbs) -> Point {
        let g = Point::generator();
        shamir(k, &ZERO, &g, &g, &g)
    }

    fn affine(p: &Point) -> (Limbs, Limbs) {
        let zi = P.invert(&p.z);
        let zi2 = P.mul(&zi, &zi);
        let x = P.to_plain(&P.mul(&p.x, &zi2));
        let y = P.to_plain(&P.mul(&p.y, &P.mul(&zi2, &zi)));
        (x, y)
    }

    fn hex(s: &str) -> Limbs {
        let b: Vec<u8> = (0..64)
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect();
        limbs(&b).unwrap()
    }

    #[test]
    fn montgomery_constants() {
        for m in [&P, &N] {
            assert_eq!(m.m[0].wrapping_mul(m.inv), u64::MAX, "-m^-1 mod 2^64");
            assert_eq!(m.to_plain(&m.one), [1, 0, 0, 0]);
            assert_eq!(m.to_mont(&[1, 0, 0, 0]), m.one);
            let seven = m.to_mont(&[7, 0, 0, 0]);
            let back = m.mul(&seven, &m.invert(&seven));
            assert_eq!(back, m.one, "7 · 7^-1 = 1");
            // m - 1 = -1, its square 1.
            let minus = m.to_mont(&sub_borrow(&m.m, &[1, 0, 0, 0]).0);
            assert_eq!(m.mul(&minus, &minus), m.one);
            assert_eq!(m.add(&minus, &m.one), ZERO);
            assert_eq!(m.sub(&ZERO, &m.one), minus);
        }
    }

    /// The generator is on the curve, `n·G` is infinity, `(n-1)·G` is -G,
    /// and `2·G` is the value every P-256 implementation agrees on.
    #[test]
    fn the_curves_identities() {
        let mut gx = [0u8; 32];
        let mut gy = [0u8; 32];
        for i in 0..4 {
            gx[24 - 8 * i..32 - 8 * i].copy_from_slice(&GX[i].to_be_bytes());
            gy[24 - 8 * i..32 - 8 * i].copy_from_slice(&GY[i].to_be_bytes());
        }
        assert!(P256Key::new(&gx, &gy).is_some());
        assert!(times_g(&N.m).is_infinity());
        let minus = times_g(&sub_borrow(&N.m, &[1, 0, 0, 0]).0);
        assert_eq!(affine(&minus), (GX, sub_borrow(&P.m, &GY).0));
        let two = (
            hex("7cf27b188d034f7e8a52380304b51ac3c08969e277f21b35a60b48fc47669978"),
            hex("07775510db8ed040293d9ac69f7430dbba7dade63ce982299e04b79d227873d1"),
        );
        assert_eq!(affine(&times_g(&[2, 0, 0, 0])), two);
        assert_eq!(affine(&Point::generator().double()), two);
        let g = Point::generator();
        assert_eq!(affine(&g.add(&g)), two, "an addition of a point to itself");
        assert!(g.add(&minus).is_infinity(), "G + (-G)");
        // 3G both ways round.
        assert_eq!(affine(&g.double().add(&g)), affine(&times_g(&[3, 0, 0, 0])));
    }

    #[test]
    fn a_key_off_the_curve_is_refused() {
        let gx: Vec<u8> = (0..4).rev().flat_map(|i| GX[i].to_be_bytes()).collect();
        let gy: Vec<u8> = (0..4).rev().flat_map(|i| GY[i].to_be_bytes()).collect();
        assert!(P256Key::new(&gx, &gy).is_some());
        let mut off = gy.clone();
        off[31] ^= 1;
        assert!(P256Key::new(&gx, &off).is_none(), "off the curve");
        assert!(P256Key::new(&[0; 32], &[0; 32]).is_none(), "(0, 0)");
        assert!(P256Key::new(&gx[1..], &gy).is_none(), "31 bytes");
        let mut long = vec![0u8];
        long.extend_from_slice(&gx);
        assert!(P256Key::new(&long, &gy).is_none(), "33 bytes");
        // p is 0 again modulo p, and no coordinate's spelling.
        let p: Vec<u8> = (0..4).rev().flat_map(|i| P.m[i].to_be_bytes()).collect();
        assert!(P256Key::new(&p, &gy).is_none(), "x = p");
        assert!(P256Key::new(&[0xff; 32], &gy).is_none(), "x past p");
        // A coordinate's second spelling, itself plus p, which fits in 256
        // bits where the coordinate is below 2^224 - 2^192 - 2^96 + 1: the
        // arithmetic would take it for the coordinate, so only the range
        // check refuses it. (5, y) and (x, 5) are points of the curve.
        let small_x = (
            hex("0000000000000000000000000000000000000000000000000000000000000005"),
            hex("459243b9aa581806fe913bce99817ade11ca503c64d9a3c533415c083248fbcc"),
        );
        let small_y = (
            hex("d7325d7646cd60d80a92738ceb345f844cffaf35841022cab176f692de8de1d7"),
            hex("0000000000000000000000000000000000000000000000000000000000000005"),
        );
        let plus_p = |a: &Limbs| {
            let (sum, carry) = add_carry(a, &P.m);
            assert!(!carry);
            be(&sum)
        };
        for (x, y) in [small_x, small_y] {
            assert!(P256Key::new(&be(&x), &be(&y)).is_some(), "on the curve");
        }
        let (x, y) = small_x;
        assert!(P256Key::new(&plus_p(&x), &be(&y)).is_none(), "x + p");
        let (x, y) = small_y;
        assert!(P256Key::new(&be(&x), &plus_p(&y)).is_none(), "y + p");
    }

    fn be(a: &Limbs) -> Vec<u8> {
        (0..4).rev().flat_map(|i| a[i].to_be_bytes()).collect()
    }

    fn mod_n(a: Limbs) -> Limbs {
        if geq(&a, &N.m) {
            sub_borrow(&a, &N.m).0
        } else {
            a
        }
    }

    /// The key `d·G` and its signature of `msg` with the nonce `k` (SEC 1
    /// §4.1.3): `r = x(k·G) mod n`, `s = k^-1 (e + r·d) mod n`.
    fn signed(d: &Limbs, k: &Limbs, msg: &[u8]) -> (P256Key, Vec<u8>) {
        let (qx, qy) = affine(&times_g(d));
        let key = P256Key::new(&be(&qx), &be(&qy)).unwrap();
        let r = mod_n(times_g(k).affine_x().unwrap());
        let e = mod_n(limbs(&sha256(msg)).unwrap());
        let m = |a: &Limbs| N.to_mont(a);
        let s = N.mul(&N.invert(&m(k)), &N.add(&m(&e), &N.mul(&m(&r), &m(d))));
        let mut sig = be(&r);
        sig.extend(be(&N.to_plain(&s)));
        (key, sig)
    }

    /// Keys no vector file holds, where `G + Q` -- the point every
    /// verification adds where both scalars' bits are set -- is a doubling
    /// (Q = G, d = 1) or infinity (Q = -G, d = n - 1): the two branches of
    /// `add` a key's own `gq` takes.
    #[test]
    fn keys_whose_sum_with_g_is_a_doubling_or_infinity() {
        let minus_one = sub_borrow(&N.m, &[1, 0, 0, 0]).0;
        let k = hex("a6e3c57dd01abe90086538398355dd4c3b17aa873382b0f24d6129493d8aad60");
        for d in [[1, 0, 0, 0], minus_one, [2, 0, 0, 0], k] {
            for k in [[3, 0, 0, 0], k, sub_borrow(&N.m, &[5, 0, 0, 0]).0] {
                let (key, sig) = signed(&d, &k, b"a message");
                assert!(key.verify_sha256(b"a message", &sig), "d {d:x?} k {k:x?}");
                assert!(!key.verify_sha256(b"a messagf", &sig));
                // (r, n - s) verifies too: ECDSA's own malleability, which
                // JWS does not forbid.
                let s = limbs(&sig[32..]).unwrap();
                let mut other = sig[..32].to_vec();
                other.extend(be(&sub_borrow(&N.m, &s).0));
                assert!(key.verify_sha256(b"a message", &other));
            }
        }
    }
}
