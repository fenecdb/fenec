//! Decimal text to `f64`, without the twelve-kilobyte table -- and floats
//! back to text without the standard library's twenty (`f64_into`, below).
//!
//! `str::parse::<f64>()` is Eisel-Lemire, and Eisel-Lemire is fast because it
//! reads a 751-entry table of 128-bit powers of five. Measured in the wasm
//! build that table is 12 628 bytes -- a twelfth of the whole module, and
//! random enough that neither gzip nor brotli takes anything off it. The
//! algorithm around it is only 469 bytes; the table *is* the cost.
//!
//! So the table goes and the two table-free paths stay:
//!
//! * **Fast.** When the mantissa fits in 53 bits and the decimal exponent is
//!   within ±22, both `m` and `10^e` are exact `f64`s, so one IEEE multiply
//!   or divide is already correctly rounded (Clinger). That is nearly every
//!   number anyone writes, an embedding component included.
//! * **Slow.** Otherwise the digits are shifted as a decimal string until the
//!   value sits in `[1/2, 1)`, and the mantissa bits are read off one at a
//!   time. This is what the standard library falls back to as well, and it
//!   measured 2 854 bytes there -- the part worth keeping.
//!
//! Both paths are correctly rounded, and the tests check exactly that against
//! `str::parse::<f64>()`: on the host the reference costs nothing, and only
//! the wasm build pays for the table it replaces.

/// `10^0` through `10^22` -- every power of ten that is exact in an `f64`.
/// `10^23` is not, which is where the fast path stops.
const POW10: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

/// Digits kept in the slow path. The smallest subnormal needs 751 significant
/// decimal digits, and shifting adds a few; beyond this the tail cannot move
/// a rounding decision, so it is dropped and recorded in `truncated`.
const MAX_DIGITS: usize = 800;

/// Parses a decimal number. `None` for anything that is not one -- the JSON
/// scanner hands over whatever run of `[0-9.eE+-]` it found, so `1.2.3` and
/// `--1` do arrive here and have to be rejected rather than guessed at.
pub fn parse_f64(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0;

    let neg = match b.first() {
        Some(b'-') => {
            i = 1;
            true
        }
        Some(b'+') => {
            i = 1;
            false
        }
        _ => false,
    };

    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int = &b[int_start..i];

    let frac = if i < b.len() && b[i] == b'.' {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        &b[start..i]
    } else {
        &b[..0]
    };

    // A lone `.`, or `-`, is not a number.
    if int.is_empty() && frac.is_empty() {
        return None;
    }

    let mut exp: i32 = 0;
    if i < b.len() && (b[i] | 0x20) == b'e' {
        i += 1;
        let eneg = match b.get(i) {
            Some(b'-') => {
                i += 1;
                true
            }
            Some(b'+') => {
                i += 1;
                false
            }
            _ => false,
        };
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None; // `1e`, `1e+`
        }
        // Saturating: an exponent this far out only decides zero or infinity,
        // and both are settled below without the digits ever being shifted.
        let mut e: i32 = 0;
        for &d in &b[start..i] {
            e = e.saturating_mul(10).saturating_add((d - b'0') as i32);
            if e > 100_000 {
                break;
            }
        }
        exp = if eneg { -e } else { e };
    }

    if i != b.len() {
        return None; // trailing junk: `1.2.3`, `1x`
    }

    Some(convert(neg, int, frac, exp))
}

fn convert(neg: bool, int: &[u8], frac: &[u8], exp: i32) -> f64 {
    // Leading zeros are not significant and would otherwise eat into the
    // 19-digit budget of the fast path: `0.00000001` must still take it.
    let int = {
        let mut k = 0;
        while k < int.len() && int[k] == b'0' {
            k += 1;
        }
        &int[k..]
    };

    // Fast path: accumulate while the mantissa is certainly exact.
    let mut mant: u64 = 0;
    let mut digits = 0;
    let mut truncated = false;
    let mut e10 = exp;
    for &d in int {
        if digits < 19 {
            mant = mant * 10 + (d - b'0') as u64;
            digits += 1;
        } else {
            truncated = true;
            e10 += 1; // the digit still counts, its value no longer fits
        }
    }
    for &d in frac {
        if digits < 19 {
            // Leading zeros of a pure fraction are not significant either.
            if mant == 0 && d == b'0' {
                e10 -= 1;
                continue;
            }
            mant = mant * 10 + (d - b'0') as u64;
            digits += 1;
            e10 -= 1;
        } else {
            truncated = true;
        }
    }

    if mant == 0 {
        return if neg { -0.0 } else { 0.0 };
    }

    if !truncated && mant <= (1u64 << 53) {
        let m = mant as f64;
        if (0..=22).contains(&e10) {
            return sign(neg, m * POW10[e10 as usize]);
        }
        if (-22..0).contains(&e10) {
            return sign(neg, m / POW10[(-e10) as usize]);
        }
        // A little further up: if the surplus above 10^22 still fits in the
        // mantissa, absorbing it there keeps both operands exact and the
        // result to a single rounding. There is no mirror of this below
        // zero -- two divisions would round twice.
        if (23..=22 + 15).contains(&e10) {
            let spare = POW10[(e10 - 22) as usize] as u64;
            if let Some(scaled) = mant.checked_mul(spare) {
                if scaled <= 1u64 << 53 {
                    return sign(neg, (scaled as f64) * POW10[22]);
                }
            }
        }
    }

    sign(neg, Decimal::new(int, frac, exp).into_f64())
}

fn sign(neg: bool, v: f64) -> f64 {
    if neg {
        -v
    } else {
        v
    }
}

/// `floor(i * log2(10))` for `i` in `0..=8`: how many bits a shift may take
/// without overshooting `i` decimal places. Always an under-estimate, so the
/// normalising loops below converge rather than oscillate.
const POWTAB: [u32; 9] = [1, 3, 6, 9, 13, 16, 19, 23, 26];

/// Largest shift the digit arithmetic below can take at once: the carries
/// hold `10 * 2^k`, so this has to leave room in a `u64`, and 57 does with
/// four bits to spare.
const MAX_SHIFT: u32 = 57;

/// Largest step the normalising loops may take, `floor(9 * log2(10))`. It has
/// to stay near the table's reach rather than run up to `MAX_SHIFT`: the two
/// loops run one after the other, so a right shift that overshoots far below
/// zero lets the left shift that corrects it land back above zero, and the
/// value never settles in `[1/2, 1)`.
const MAX_STEP: u32 = 27;

/// `0.d[0]d[1]... * 10^dp`, with no leading or trailing zero digit.
struct Decimal {
    d: [u8; MAX_DIGITS],
    nd: usize,
    dp: i32,
    truncated: bool,
}

impl Decimal {
    fn new(int: &[u8], frac: &[u8], exp: i32) -> Decimal {
        let mut dec = Decimal {
            d: [0; MAX_DIGITS],
            nd: 0,
            dp: 0,
            truncated: false,
        };
        for &c in int {
            dec.push(c - b'0');
        }
        dec.dp = int.len() as i32;
        for &c in frac {
            // A fraction's leading zeros move the point instead of being
            // stored, so `0.0001` keeps its four digits of headroom.
            if dec.nd == 0 && c == b'0' {
                dec.dp -= 1;
                continue;
            }
            dec.push(c - b'0');
        }
        dec.dp += exp;
        dec.trim();
        dec
    }

    fn push(&mut self, digit: u8) {
        if self.nd < MAX_DIGITS {
            self.d[self.nd] = digit;
            self.nd += 1;
        } else if digit != 0 {
            self.truncated = true;
        }
    }

    /// Drops leading zeros (moving the point) and trailing zeros (which carry
    /// nothing). Every operation below assumes this has run.
    fn trim(&mut self) {
        let mut k = 0;
        while k < self.nd && self.d[k] == 0 {
            k += 1;
        }
        if k > 0 {
            self.d.copy_within(k..self.nd, 0);
            self.nd -= k;
            self.dp -= k as i32;
        }
        while self.nd > 0 && self.d[self.nd - 1] == 0 {
            self.nd -= 1;
        }
        if self.nd == 0 {
            self.dp = 0;
        }
    }

    /// value *= 2^k, for `k` up to `MAX_SHIFT`.
    ///
    /// Doubling one bit at a time is a page shorter but costs a pass over
    /// every digit per bit: measured at 0.24 ms for `1e-300`, against 18 ns
    /// for the standard library. Taking 57 bits per pass is what closes that.
    fn shl(&mut self, k: u32) {
        debug_assert!(k <= MAX_SHIFT);
        // 0.302 > log10(2), so this never under-counts the digits gained.
        let extra = (k as usize * 302) / 1000 + 1;
        if self.nd + extra > MAX_DIGITS {
            let keep = MAX_DIGITS - extra;
            if self.d[keep..self.nd].iter().any(|&x| x != 0) {
                self.truncated = true;
            }
            self.nd = keep;
        }
        let mut out = [0u8; MAX_DIGITS];
        let mut carry: u64 = 0;
        for r in (0..self.nd).rev() {
            let acc = ((self.d[r] as u64) << k) + carry;
            out[r + extra] = (acc % 10) as u8;
            carry = acc / 10;
        }
        for i in (0..extra).rev() {
            out[i] = (carry % 10) as u8;
            carry /= 10;
        }
        debug_assert_eq!(carry, 0);
        self.d = out;
        self.nd += extra;
        self.dp += extra as i32;
        self.trim();
    }

    /// value /= 2^k, for `k` up to `MAX_SHIFT`.
    fn shr(&mut self, k: u32) {
        debug_assert!(k <= MAX_SHIFT);
        let mask: u64 = (1u64 << k) - 1;
        let mut r = 0; // read position
        let mut w = 0; // write position
        let mut n: u64 = 0;
        // Pull in digits until the first quotient digit is not a zero, so
        // the write below always emits a significant digit.
        while (n >> k) == 0 {
            if r >= self.nd {
                if n == 0 {
                    self.nd = 0;
                    self.dp = 0;
                    return;
                }
                while (n >> k) == 0 {
                    n *= 10;
                    r += 1;
                }
                break;
            }
            n = n * 10 + self.d[r] as u64;
            r += 1;
        }
        self.dp -= r as i32 - 1;
        let mut out = [0u8; MAX_DIGITS];
        while r < self.nd {
            let c = self.d[r] as u64;
            out[w] = (n >> k) as u8;
            w += 1;
            n = (n & mask) * 10 + c;
            r += 1;
        }
        while n > 0 {
            let dig = (n >> k) as u8;
            if w < MAX_DIGITS {
                out[w] = dig;
                w += 1;
            } else if dig > 0 {
                self.truncated = true;
            }
            n = (n & mask) * 10;
        }
        self.d = out;
        self.nd = w;
        self.trim();
    }

    /// Bits a shift may take to move the point `places` decimal digits.
    fn step(places: i32) -> u32 {
        let p = places.unsigned_abs() as usize;
        if p >= POWTAB.len() {
            MAX_STEP
        } else {
            POWTAB[p]
        }
    }

    /// The integer part, with the fraction rounded in -- half to even, and a
    /// truncated tail counting as more than half.
    fn rounded_integer(&self) -> u64 {
        let mut n: u64 = 0;
        let mut i = 0;
        while i < self.dp {
            let c = if (i as usize) < self.nd {
                self.d[i as usize] as u64
            } else {
                0
            };
            n = n * 10 + c;
            i += 1;
        }
        if self.round_up() {
            n += 1;
        }
        n
    }

    fn round_up(&self) -> bool {
        if self.dp < 0 || self.dp as usize >= self.nd {
            return false; // nothing past the point
        }
        let i = self.dp as usize;
        if self.d[i] == 5 && i + 1 == self.nd {
            // Exactly half, unless digits were dropped off the tail.
            if self.truncated {
                return true;
            }
            return i > 0 && self.d[i - 1] & 1 != 0;
        }
        self.d[i] >= 5
    }

    fn into_f64(mut self) -> f64 {
        if self.nd == 0 {
            return 0.0;
        }
        // Decided by magnitude alone, before any shifting: the largest f64 is
        // ~1.8e308 and the smallest subnormal ~4.9e-324, so anything past
        // these margins is settled and the loops below never have to run.
        if self.dp > 400 {
            return f64::INFINITY;
        }
        if self.dp < -400 {
            return 0.0;
        }

        // Normalise to `m * 2^exp2` with m in [1/2, 1).
        let mut exp2: i32 = 0;
        while self.dp > 0 {
            let k = Decimal::step(self.dp);
            self.shr(k);
            exp2 += k as i32;
        }
        while self.dp < 0 || (self.dp == 0 && self.d[0] < 5) {
            let k = Decimal::step(self.dp);
            self.shl(k);
            exp2 -= k as i32;
        }
        if self.nd == 0 {
            return 0.0;
        }

        // 53 bits of mantissa, fewer once the exponent enters subnormal
        // territory -- there the exponent is pinned at 2^-1074 and precision
        // is what gives way instead. Below that the value is under half the
        // smallest subnormal (m < 1 keeps it off the tie), so it is zero.
        let mut nbits: i32 = 53;
        if exp2 < -1021 {
            nbits = exp2 + 1074;
            if nbits < 0 {
                return 0.0;
            }
        }

        self.shl(nbits as u32);
        assemble(self.rounded_integer(), exp2 - nbits)
    }
}

/// `mant * 2^e2` as an `f64`, built from the bits so no second rounding can
/// creep in on the way.
fn assemble(mant: u64, e2: i32) -> f64 {
    let (mut mant, mut e2) = (mant, e2);
    if mant == 0 {
        return 0.0;
    }
    // Rounding can carry out of 53 bits, and only from an even mantissa.
    if mant >= 1u64 << 53 {
        mant >>= 1;
        e2 += 1;
    }
    if mant >= 1u64 << 52 {
        let field = e2 + 52 + 1023;
        if field >= 0x7ff {
            return f64::INFINITY;
        }
        debug_assert!(field > 0);
        f64::from_bits(((field as u64) << 52) | (mant & ((1u64 << 52) - 1)))
    } else {
        // Subnormal: the exponent field is zero and the value is `mant`
        // scaled by 2^-1074, which is exactly what these bits mean.
        debug_assert_eq!(e2, -1074);
        f64::from_bits(mant)
    }
}

// ------------------------------------------------------------ and back to text

/// `x` as `{}` writes it -- the shortest digits that read back as `x`, in
/// plain decimal, `-0`, `NaN`, `inf` -- without the standard library's
/// formatting: Grisu with Dragon behind it, and Dragon's bignum, were 20.8 KB
/// of the browser module (`make size-report`), 4.6% of its code.
///
/// The digits are Ryu's (Adams, PLDI 2018) with one change. An exact half
/// -- `1125899906842624.25`, whose neighbours `.2` and `.3` are equally near
/// and both read back as it -- Ryu rounds to even and `{}` rounds up, and
/// what a query answered before has to be what it answers now, byte for byte.
pub fn f64_into(out: &mut String, x: f64) {
    float_into(out, x.to_bits(), 52, 11);
}

/// `x` as `{}` writes an `f32`: the shortest digits that read back as the
/// `f32`, not as the `f64` it widens to (`0.1`, not `0.10000000149011612`).
/// It goes through the same code as `f64_into`, with the interval an `f32`
/// stands for: the `f64` multipliers carry more precision than it needs, and
/// the tests hold every one of the 2^32 to `{}`.
pub fn f32_into(out: &mut String, x: f32) {
    float_into(out, x.to_bits() as u64, 23, 8);
}

fn float_into(out: &mut String, bits: u64, mbits: u32, ebits: u32) {
    let mantissa = bits & ((1 << mbits) - 1);
    let exponent = (bits >> mbits) as u32 & ((1 << ebits) - 1);
    let negative = bits >> (mbits + ebits) != 0;
    if exponent == (1 << ebits) - 1 {
        out.push_str(match (mantissa != 0, negative) {
            (true, _) => "NaN",
            (false, true) => "-inf",
            (false, false) => "inf",
        });
        return;
    }
    if negative {
        out.push('-');
    }
    if exponent == 0 && mantissa == 0 {
        out.push('0');
        return;
    }
    let (digits, exp) = shortest(mantissa, exponent, mbits, ebits);
    let mut text = [0u8; 20];
    let mut n = text.len();
    let mut d = digits;
    while d > 0 {
        n -= 1;
        text[n] = b'0' + (d % 10) as u8;
        d /= 10;
    }
    let text = &text[n..];
    // `{}` never writes an exponent: the digits before the point, then the
    // rest, with zeros wherever the exponent reaches past them.
    let point = text.len() as i32 + exp;
    let push = |out: &mut String, digits: &[u8]| digits.iter().for_each(|&b| out.push(b as char));
    if point <= 0 {
        out.push_str("0.");
        (point..0).for_each(|_| out.push('0'));
        push(out, text);
    } else if (point as usize) < text.len() {
        push(out, &text[..point as usize]);
        out.push('.');
        push(out, &text[point as usize..]);
    } else {
        push(out, text);
        (text.len() as i32..point).for_each(|_| out.push('0'));
    }
}

/// The multipliers carry this many bits of `5^i` and of `2^k / 5^i`.
const BITS: i32 = 125;

/// `(digits, e)`: the shortest decimal `digits × 10^e` inside the interval of
/// values that round to this float, the nearest to it when there are several
/// -- Ryu's d2d, for any width of float up to an `f64`.
fn shortest(mantissa: u64, exponent: u32, mbits: u32, ebits: u32) -> (u64, i32) {
    let bias = (1 << (ebits - 1)) - 1;
    // Two more bits, so the interval's bounds are whole numbers too.
    let (e2, m2) = match exponent {
        0 => (1 - bias - mbits as i32 - 2, mantissa),
        _ => (
            exponent as i32 - bias - mbits as i32 - 2,
            (1 << mbits) | mantissa,
        ),
    };
    // An even mantissa rounds to itself from a tie, so its bounds belong to it.
    let accept = m2 & 1 == 0;
    let mv = 4 * m2;
    // The interval is narrower below a power of two, whose lower neighbour
    // is half as far away.
    let mm_shift = (mantissa != 0 || exponent <= 1) as u64;
    let (mm, mp) = (mv - 1 - mm_shift, mv + 2);

    let (mut vr, mut vp, mut vm, e10);
    let mut vm_zeros = false;
    if e2 >= 0 {
        let q = log10_pow2(e2) - (e2 > 3) as i32;
        e10 = q;
        let mul = inv_pow5(q as u32);
        let j = -e2 + q + BITS + pow5_bits(q) - 1;
        (vr, vp, vm) = (
            mul_shift(mv, mul, j),
            mul_shift(mp, mul, j),
            mul_shift(mm, mul, j),
        );
        // Whether the digits cut from the lower bound are all zeros, which
        // decides if that bound is reachable, and an exact upper bound the
        // float does not own comes in by one. One at most of `mm`, `mv` and
        // `mp` is a multiple of 5; Ryu asks only up to q = 21.
        if q <= 21 && !mv.is_multiple_of(5) {
            if accept {
                vm_zeros = pow5_factor(mm) >= q as u32;
            } else {
                vp -= (pow5_factor(mp) >= q as u32) as u64;
            }
        }
    } else {
        let q = log10_pow5(-e2) - (-e2 > 1) as i32;
        e10 = q + e2;
        let i = -e2 - q;
        let mul = pow5(i as u32);
        let j = q - (pow5_bits(i) - BITS);
        (vr, vp, vm) = (
            mul_shift(mv, mul, j),
            mul_shift(mp, mul, j),
            mul_shift(mm, mul, j),
        );
        if q <= 1 {
            if accept {
                vm_zeros = mm_shift == 1;
            } else {
                vp -= 1;
            }
        }
    }

    // Digits go while the bounds still differ above them; the last one cut
    // from `vr` rounds it.
    let (mut removed, mut last) = (0, 0);
    while vp / 10 > vm / 10 {
        vm_zeros &= vm.is_multiple_of(10);
        last = vr % 10;
        (vr, vp, vm) = (vr / 10, vp / 10, vm / 10);
        removed += 1;
    }
    if vm_zeros {
        while vm.is_multiple_of(10) {
            last = vr % 10;
            (vr, vp, vm) = (vr / 10, vp / 10, vm / 10);
            removed += 1;
        }
    }
    // One up when `vr` is a bound the float does not own, or from a half or
    // more: an exact half too, where Ryu would go to the even neighbour.
    let up = (vr == vm && (!accept || !vm_zeros)) || last >= 5;
    (vr + up as u64, e10 + removed)
}

/// `⌊m × mul / 2^j⌋`, `j` at least 64: the product's low 64 bits never
/// reach it.
fn mul_shift(m: u64, mul: u128, j: i32) -> u64 {
    let low = m as u128 * (mul as u64) as u128;
    let high = m as u128 * (mul >> 64);
    (((low >> 64) + high) >> (j - 64)) as u64
}

/// `⌈log2 5^e⌉`, 1 for `e = 0`.
fn pow5_bits(e: i32) -> i32 {
    ((e as u32 * 1_217_359) >> 19) as i32 + 1
}

/// `⌊log10 2^e⌋`.
fn log10_pow2(e: i32) -> i32 {
    ((e as u32 * 78_913) >> 18) as i32
}

/// `⌊log10 5^e⌋`.
fn log10_pow5(e: i32) -> i32 {
    ((e as u32 * 732_923) >> 20) as i32
}

fn pow5_factor(mut v: u64) -> u32 {
    let mut n = 0;
    while v.is_multiple_of(5) {
        v /= 5;
        n += 1;
    }
    n
}

/// Every 26th multiplier is kept whole, and the rest are one of those times
/// `5^1` to `5^25`, plus a correction of 0 to 3 in the last place: two bits a
/// power, sixteen a word. The full tables are 10.4 KB; these are 0.8.
const STEP: u32 = 26;

/// `5^i` for `i` below `STEP`.
const SMALL: [u64; STEP as usize] = {
    let mut t = [1; STEP as usize];
    let mut i = 1;
    while i < t.len() {
        t[i] = t[i - 1] * 5;
        i += 1;
    }
    t
};

/// `5^(26b)`, its top 125 bits: `5^i >> (⌈log2 5^i⌉ - 125)`.
const POW5: [u128; 13] = [
    0x10000000000000000000000000000000,
    0x14adf4b7320334b90000000000000000,
    0x1aba4714957d300d0e549208b31adb10,
    0x1145b7e285bf98f56dc6ad264d8f0866,
    0x1652efdc6018a1fceb1dbd923d8596ca,
    0x1cda62055b2d9d83b4c1b80b22ae923c,
    0x12a5568b9f52f4165bb28b4e8f7e4c30,
    0x1819651531f9e78ff08aed437682d4fb,
    0x1f25c186a6f04c28b4ee134ad99bf150,
    0x1420eb449c8842e616499ecb70c25f03,
    0x1a03fde214caf08585a56ead360865b0,
    0x10cfeb353a97dad8093db1d57999890b,
    0x15baaf44fa52673ecf38bb735e3f36ac,
];

const POW5_FIX: [u32; 21] = [
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x40000000, 0x59695995, 0x55545555, 0x56555515,
    0x41150504, 0x40555410, 0x44555145, 0x44504540, 0x45555550, 0x40004000, 0x96440440, 0x55565565,
    0x54454045, 0x40154151, 0x55559155, 0x51405555, 0x00000105,
];

/// `5^-(26b)` as Ryu multiplies by it: `⌊2^(⌈log2 5^i⌉ - 1 + 125) / 5^i⌋ + 1`.
const POW5_INV: [u128; 13] = [
    0x20000000000000000000000000000001,
    0x18c240c4aecb13bb52a6c95fc0655034,
    0x1327fc58da0f6ff57ca8d50071dfc806,
    0x1da48ce468e7c7026520247d3556476e,
    0x16ef5b40c2fc77796139cdd76802e6e9,
    0x11bebdf578b2f391f951a7ff43de8c79,
    0x1b758d848fac54b07be8bee8d6e957e8,
    0x153eda614071a3b78bd3f9e999a423ea,
    0x10701bd527b4978c0848f973cb3ee3ce,
    0x196fbb9bb44db44d153285ebb9efbfa2,
    0x13ae3591f5b4d936adeee7f86c07b696,
    0x1e74404f3daada914d686a4eaf182222,
    0x17900ea4fda7c25798c0a106e09ebd9f,
];

const POW5_INV_FIX: [u32; 19] = [
    0x54544554, 0x04055545, 0x10041000, 0x00400414, 0x40010000, 0x41155555, 0x00000454, 0x00010044,
    0x40000000, 0x44000041, 0x50454450, 0x55550054, 0x51655554, 0x40004000, 0x01000001, 0x00010500,
    0x51515411, 0x05555554, 0x00000000,
];

fn fix(table: &[u32], i: u32) -> u128 {
    ((table[i as usize / 16] >> (i % 16 * 2)) & 3) as u128
}

/// The multiplier for `5^i`, `i` up to 325 (an `f64`'s smallest subnormal).
fn pow5(i: u32) -> u128 {
    let base = i / STEP;
    let (mul, off) = (POW5[base as usize], i - base * STEP);
    if off == 0 {
        return mul;
    }
    let m = SMALL[off as usize] as u128;
    let shift = pow5_bits(i as i32) - pow5_bits((base * STEP) as i32);
    let (low, high) = (m * (mul as u64) as u128, m * (mul >> 64));
    (low >> shift) + (high << (64 - shift)) + fix(&POW5_FIX, i)
}

/// The multiplier for `5^-i`, `i` up to 290 (an `f64`'s largest).
fn inv_pow5(i: u32) -> u128 {
    let base = i.div_ceil(STEP);
    let (mul, off) = (POW5_INV[base as usize], base * STEP - i);
    if off == 0 {
        return mul;
    }
    let m = SMALL[off as usize] as u128;
    let shift = pow5_bits((base * STEP) as i32) - pow5_bits(i as i32);
    let (low, high) = (m * (mul as u64 - 1) as u128, m * (mul >> 64));
    (low >> shift) + (high << (64 - shift)) + 1 + fix(&POW5_INV_FIX, i)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bit-for-bit, so a mismatched sign on a zero or a one-ulp slip in the
    /// last place both fail rather than comparing equal.
    #[track_caller]
    fn same(text: &str) {
        let want: f64 = text.parse().expect("the reference should parse this");
        let got = parse_f64(text).expect("parse_f64 should parse this");
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "{text}: got {got:e} ({:#x}), want {want:e} ({:#x})",
            got.to_bits(),
            want.to_bits()
        );
    }

    /// Both fuzz tests below run this many rounds, so 200 000 comparisons
    /// against `str::parse` on every `make test`. Raising it is the way to
    /// search harder; it was run at 2 000 000 while this was written, and the
    /// seeds are fixed so a failure is reproducible.
    const SAMPLES: usize = 100_000;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            // xorshift64*, enough to spray the input space and reproducible
            // from a fixed seed when something does fail.
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
    }

    #[test]
    fn the_fast_path_is_exact() {
        for text in [
            "0",
            "-0",
            "1",
            "-1",
            "1.5",
            "0.1",
            "0.2",
            "0.3",
            "3.14159265358979",
            "1e10",
            "1e-10",
            "1e22",
            "1e-22",
            "123456789.123456789",
            "-0.04729",
            "9007199254740992",
            "0.00000001",
            "0.000000000000000000001",
            "2",
            "1024",
            "6.02e23",
        ] {
            same(text);
        }
    }

    #[test]
    fn the_slow_path_is_exact() {
        for text in [
            // Boundaries and the classic torture cases.
            "2.2250738585072011e-308", // the PHP hang
            "2.2250738585072014e-308", // smallest normal
            "4.9406564584124654e-324", // smallest subnormal
            "1e-323",
            "1.7976931348623157e308", // largest finite
            "1.7976931348623159e308", // just over -> infinity
            "1e309",
            "1e-400",
            "9007199254740993", // 2^53 + 1, not representable
            "9007199254740995",
            "1e23", // first power of ten off the fast path
            "1e-23",
            "8.98846567431158e307",
            "0.500000000000000166533453693773481063544750213623046875",
            "3.518437208883201171875e13",
            "1.0000000000000000000000000000000000000000001",
            "123456789012345678901234567890e-15",
        ] {
            same(text);
        }
    }

    #[test]
    fn every_float_survives_a_round_trip() {
        // `{}` and `{:e}` are shortest-round-trip, so re-parsing has to give
        // back the very same bits. Random bit patterns cover subnormals, the
        // exponent extremes and everything between far better than a list.
        let mut rng = Rng(0x5eed_1234_9abc_def0);
        let mut checked = 0;
        for _ in 0..SAMPLES {
            let bits = rng.next();
            let f = f64::from_bits(bits);
            if !f.is_finite() {
                continue;
            }
            same(&format!("{f}"));
            same(&format!("{f:e}"));
            checked += 1;
        }
        assert!(
            checked * 4 > SAMPLES * 3,
            "too few finite samples: {checked}"
        );
    }

    #[test]
    fn random_decimal_text_agrees_with_the_reference() {
        // Built from digits rather than from floats, so the input is not
        // biased towards numbers that happen to be representable.
        let mut rng = Rng(0xfeed_face_0bad_c0de);
        for _ in 0..SAMPLES {
            let ndigits = 1 + (rng.next() % 25) as usize;
            let mut s = String::new();
            if rng.next() & 1 == 0 {
                s.push('-');
            }
            for k in 0..ndigits {
                let d = (rng.next() % 10) as u8;
                // No leading zero, so the digit count means what it says.
                s.push(if k == 0 && d == 0 {
                    '1'
                } else {
                    (b'0' + d) as char
                });
            }
            if rng.next() & 1 == 0 {
                s.push('.');
                for _ in 0..(1 + rng.next() % 25) {
                    s.push((b'0' + (rng.next() % 10) as u8) as char);
                }
            }
            if rng.next() & 1 == 0 {
                let e = (rng.next() % 700) as i64 - 350;
                s.push('e');
                s.push_str(&e.to_string());
            }
            same(&s);
        }
    }

    #[test]
    fn long_digit_strings_are_exact() {
        // Past the 19 digits the fast path can hold, and past the 800 the
        // slow path keeps -- the tail must not move a rounding decision.
        let mut rng = Rng(0x0123_4567_89ab_cdef);
        for _ in 0..2_000 {
            let n = 20 + (rng.next() % 900) as usize;
            let mut s = String::from("0.");
            for _ in 0..n {
                s.push((b'0' + (rng.next() % 10) as u8) as char);
            }
            same(&s);
        }
        let mut nines = String::from("0.");
        nines.push_str(&"9".repeat(1200));
        same(&nines);
    }

    #[test]
    fn malformed_text_is_rejected() {
        // The JSON scanner hands over any run of `[0-9.eE+-]`, so these do
        // arrive here. Rejecting beats guessing.
        for text in [
            "", "-", "+", ".", "-.", "e5", "1e", "1e+", "1e-", "1.2.3", "--1", "1-2", "1e5e5",
            "1..2", "1x", " 1", "1 ", "inf", "nan", "0x10",
        ] {
            assert!(parse_f64(text).is_none(), "{text:?} should be rejected");
        }
    }

    /// A little-endian bignum of 32-bit words, only to hold the multipliers
    /// to the powers of five they are cut from.
    fn big(mut v: u128) -> Vec<u64> {
        let mut out = vec![];
        while v > 0 {
            out.push((v & 0xffff_ffff) as u64);
            v >>= 32;
        }
        out
    }

    fn times(a: &[u64], b: &[u64]) -> Vec<u64> {
        let mut out = vec![0u64; a.len() + b.len() + 1];
        for (i, &x) in a.iter().enumerate() {
            let mut carry = 0;
            for (j, &y) in b.iter().enumerate() {
                let t = out[i + j] + x * y + carry;
                out[i + j] = t & 0xffff_ffff;
                carry = t >> 32;
            }
            let mut k = i + b.len();
            while carry > 0 {
                let t = out[k] + carry;
                out[k] = t & 0xffff_ffff;
                carry = t >> 32;
                k += 1;
            }
        }
        while out.last() == Some(&0) {
            out.pop();
        }
        out
    }

    fn two_to(k: u32) -> Vec<u64> {
        let mut out = vec![0; k as usize / 32];
        out.push(1 << (k % 32));
        out
    }

    fn cmp(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
        a.len()
            .cmp(&b.len())
            .then_with(|| a.iter().rev().cmp(b.iter().rev()))
    }

    fn bits(a: &[u64]) -> i32 {
        (a.len() as i32 - 1) * 32 + 64 - a.last().unwrap().leading_zeros() as i32
    }

    #[test]
    fn the_multipliers_are_the_powers_of_five_they_claim() {
        use std::cmp::Ordering::*;
        let mut p = big(1);
        for i in 0..=325u32 {
            // `pow5(i)` is the top 125 bits of 5^i, cut, not rounded.
            assert_eq!(bits(&p), pow5_bits(i as i32), "5^{i}");
            let v = pow5(i);
            let shift = bits(&p) - BITS;
            if shift >= 0 {
                let unit = two_to(shift as u32);
                assert_ne!(cmp(&times(&big(v), &unit), &p), Greater, "5^{i}");
                assert_eq!(cmp(&times(&big(v + 1), &unit), &p), Greater, "5^{i}");
            } else {
                assert_eq!(
                    cmp(&big(v), &times(&p, &two_to(-shift as u32))),
                    Equal,
                    "5^{i}"
                );
            }
            // `inv_pow5(i)` is one more than 2^k / 5^i, cut.
            if i <= 290 {
                let whole = two_to((pow5_bits(i as i32) - 1 + BITS) as u32);
                let v = inv_pow5(i);
                assert_ne!(cmp(&times(&big(v - 1), &p), &whole), Greater, "5^-{i}");
                assert_eq!(cmp(&times(&big(v), &p), &whole), Greater, "5^-{i}");
            }
            p = times(&p, &[5]);
        }
    }

    #[track_caller]
    fn writes_as_display_f64(x: f64) {
        let mut got = String::new();
        f64_into(&mut got, x);
        assert_eq!(got, format!("{x}"), "{:#x}", x.to_bits());
    }

    #[track_caller]
    fn writes_as_display_f32(x: f32) {
        let mut got = String::new();
        f32_into(&mut got, x);
        assert_eq!(got, format!("{x}"), "{:#x}", x.to_bits());
    }

    #[test]
    fn floats_are_written_as_display_writes_them() {
        for x in [
            0.0,
            -0.0,
            1.0,
            -1.0,
            0.1,
            0.2,
            0.3,
            1.0 / 3.0,
            2.0 / 3.0,
            123456.789,
            1e15,
            1e16,
            1e21,
            1e22,
            1e23,
            1e-5,
            1e-7,
            5e-324,
            2.2250738585072014e-308,
            f64::from_bits(0x000f_ffff_ffff_ffff), // the largest subnormal
            1.7976931348623157e308,
            f64::EPSILON,
            9007199254740991.0,
            9007199254740992.0,
            f64::NAN,
            -f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            writes_as_display_f64(x);
        }
        // Every power of two and of ten, whole numbers, and the smallest and
        // largest of each binade.
        for e in -1074..=1023 {
            writes_as_display_f64(2f64.powi(e));
        }
        for e in -323..=308 {
            writes_as_display_f64(format!("1e{e}").parse().unwrap());
        }
        for n in 0..100_000 {
            writes_as_display_f64(n as f64);
            writes_as_display_f32(n as f32);
        }
        for e in 1..0x7ff_u64 {
            writes_as_display_f64(f64::from_bits(e << 52));
            writes_as_display_f64(f64::from_bits(e << 52 | ((1 << 52) - 1)));
        }
        // Random bits are mostly astronomically large or small; what a
        // document holds is short decimals, so those too.
        let mut rng = Rng(0x0dd_ba11_c0ff_ee00);
        for _ in 0..SAMPLES * 2 {
            writes_as_display_f64(f64::from_bits(rng.next()));
            let digits = rng.next() % 10u64.pow(1 + (rng.next() % 17) as u32);
            let text = format!("{digits}e{}", (rng.next() % 61) as i32 - 30);
            writes_as_display_f64(text.parse().unwrap());
            writes_as_display_f32(text.parse().unwrap());
        }
    }

    #[test]
    fn an_exact_half_rounds_up_as_display_rounds_it() {
        // Where a float's last bit is worth 2^-1 to 2^-8, `x.25`, `x.125`
        // and the like sit exactly between two shortest candidates: Ryu
        // would take the even one, `{}` takes the one above.
        for e in -8..=-1 {
            let start = 2f64.powi(52 + e);
            for j in 0..4096 {
                writes_as_display_f64(start + j as f64 * 2f64.powi(e));
                writes_as_display_f64(2.0 * start - (j + 1) as f64 * 2f64.powi(e));
            }
            let start = 2f32.powi(23 + e);
            for j in 0..4096 {
                writes_as_display_f32(start + j as f32 * 2f32.powi(e));
            }
        }
        let mut got = String::new();
        f64_into(&mut got, f64::from_bits(0x4310_0000_0000_0001)); // 2^50 + 0.25
        assert_eq!(got, "1125899906842624.3");
    }

    #[test]
    fn f32s_are_written_as_display_writes_them() {
        // One in 4 099 of the 2^32, a spread over every exponent; the test
        // below takes all of them.
        for bits in (0..=u32::MAX).step_by(4099) {
            writes_as_display_f32(f32::from_bits(bits));
        }
        for x in [
            0.1f32,
            0.3,
            1.0 / 3.0,
            16777216.0,
            16777217.0,
            f32::MAX,
            f32::MIN_POSITIVE,
            1e-45,
        ] {
            writes_as_display_f32(x);
        }
    }

    /// Every `f32` against `{}`: `cargo test -p fenec-core --release --lib
    /// every_f32 -- --ignored`, 4 294 967 296 of them on every core.
    #[test]
    #[ignore]
    fn every_f32_is_written_as_display_writes_it() {
        let threads = std::thread::available_parallelism().map_or(8, |n| n.get()) as u64;
        std::thread::scope(|scope| {
            for t in 0..threads {
                scope.spawn(move || {
                    let (mut got, mut want) = (String::new(), String::new());
                    let span = (1u64 << 32) / threads;
                    let end = if t == threads - 1 {
                        1 << 32
                    } else {
                        (t + 1) * span
                    };
                    for bits in t * span..end {
                        let x = f32::from_bits(bits as u32);
                        got.clear();
                        want.clear();
                        f32_into(&mut got, x);
                        std::fmt::Write::write_fmt(&mut want, format_args!("{x}")).unwrap();
                        assert_eq!(got, want, "{bits:#x}");
                    }
                });
            }
        });
    }

    /// As many random `f64`s as asked for: `FENEC_FLOATS=1000000000 cargo
    /// test -p fenec-core --release --lib many_f64 -- --ignored`.
    #[test]
    #[ignore]
    fn many_f64s_are_written_as_display_writes_them() {
        let n: u64 = std::env::var("FENEC_FLOATS").map_or(100_000_000, |v| v.parse().unwrap());
        let threads = std::thread::available_parallelism().map_or(8, |n| n.get()) as u64;
        std::thread::scope(|scope| {
            for t in 0..threads {
                scope.spawn(move || {
                    let mut rng =
                        Rng(0x9e37_79b9_7f4a_7c15 ^ (t + 1).wrapping_mul(0xbf58_476d_1ce4_e5b9));
                    let (mut got, mut want) = (String::new(), String::new());
                    for _ in 0..n / threads {
                        let x = f64::from_bits(rng.next());
                        got.clear();
                        want.clear();
                        f64_into(&mut got, x);
                        std::fmt::Write::write_fmt(&mut want, format_args!("{x}")).unwrap();
                        assert_eq!(got, want, "{:#x}", x.to_bits());
                    }
                });
            }
        });
    }

    #[test]
    fn accepted_shapes_match_the_reference_on_signs_and_zeros() {
        for text in [
            "0",
            "-0",
            "0.0",
            "-0.0",
            "0e0",
            "-0e100",
            "0.000e-99",
            "+1.5",
            ".5",
            "5.",
        ] {
            same(text);
        }
    }
}
