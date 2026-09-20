//! Decimal text to `f64`, without the twelve-kilobyte table.
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
            "0", "-0", "1", "-1", "1.5", "0.1", "0.2", "0.3", "3.14159265358979", "1e10",
            "1e-10", "1e22", "1e-22", "123456789.123456789", "-0.04729", "9007199254740992",
            "0.00000001", "0.000000000000000000001", "2", "1024", "6.02e23",
        ] {
            same(text);
        }
    }

    #[test]
    fn the_slow_path_is_exact() {
        for text in [
            // Boundaries and the classic torture cases.
            "2.2250738585072011e-308",  // the PHP hang
            "2.2250738585072014e-308",  // smallest normal
            "4.9406564584124654e-324",  // smallest subnormal
            "1e-323",
            "1.7976931348623157e308",   // largest finite
            "1.7976931348623159e308",   // just over -> infinity
            "1e309",
            "1e-400",
            "9007199254740993",         // 2^53 + 1, not representable
            "9007199254740995",
            "1e23",                     // first power of ten off the fast path
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
        assert!(checked * 4 > SAMPLES * 3, "too few finite samples: {checked}");
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
                s.push(if k == 0 && d == 0 { '1' } else { (b'0' + d) as char });
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

    #[test]
    fn accepted_shapes_match_the_reference_on_signs_and_zeros() {
        for text in ["0", "-0", "0.0", "-0.0", "0e0", "-0e100", "0.000e-99", "+1.5", ".5", "5."] {
            same(text);
        }
    }
}
