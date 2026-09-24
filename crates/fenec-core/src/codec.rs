//! Our own binary encoder.
//!
//! Why not serde: (1) WASM bundle size, (2) so that segment bytes are
//! *the same on disk as in memory*. The same byte sequence lives both in
//! the file and in the arena; that is why no separate page cache (buffer
//! pool) is needed -- a read decodes straight off the arena slice.

use crate::error::{Error, Result};
use crate::value::{DataType, Value, VecPrec};

pub const TAG_NULL: u8 = 0;
pub const TAG_BOOL: u8 = 1;
pub const TAG_INT: u8 = 2;
pub const TAG_FLOAT: u8 = 3;
pub const TAG_TEXT: u8 = 4;
pub const TAG_BYTES: u8 = 5;
pub const TAG_VECTOR: u8 = 6;
pub const TAG_LIST: u8 = 7;
/// Half-precision vector. A separate tag, because older files were written
/// with `TAG_VECTOR`: adding a new tag does not break backward compatibility,
/// an old build cannot open a new file (forward incompatibility is inevitable).
pub const TAG_VECTOR_F16: u8 = 8;
/// UTC epoch milliseconds; carries a zigzag varint just like `TAG_INT`.
pub const TAG_TIMESTAMP: u8 = 9;
/// Sparse vector: `[dimension][count]` then the indices, each the distance
/// from the one before -- a SPLADE vector's are close together, and small
/// gaps take a byte each -- then the weights as `f32`.
pub const TAG_SPARSE: u8 = 10;
/// Never a value's: a schema writes it, and the collation's code, before the
/// type of a field whose text orders in that collation (`name text collate
/// tr`). A version that knows no collation meets an unknown type tag and
/// refuses the file, rather than read the field in byte order.
pub const TAG_COLLATED: u8 = 11;

// ------------------------------------------------------- half precision
//
// IEEE 754 binary16. Our own conversion: `f16` is still unstable (nightly)
// and pulling in a dependency would break the core's `std`-only rule.

/// f32 -> binary16. Rounds to nearest (ties-to-even), saturates to infinity
/// on overflow, preserves the subnormal range.
pub fn f16_from_f32(v: f32) -> u16 {
    let x = v.to_bits();
    let sign = ((x >> 16) & 0x8000) as u16;
    let raw_exp = ((x >> 23) & 0xff) as i32;
    let mant = x & 0x007f_ffff;

    if raw_exp == 0xff {
        // Inf or NaN. Keep NaN's quiet bit, otherwise it becomes infinity.
        return sign | 0x7c00 | if mant != 0 { 0x0200 } else { 0 };
    }
    let exp = raw_exp - 127 + 15;
    if exp >= 0x1f {
        return sign | 0x7c00;
    }
    if exp <= 0 {
        if exp < -10 {
            return sign; // below even the f16 subnormal range: +-0
        }
        let m = mant | 0x0080_0000; // implicit 1
        let shift = (14 - exp) as u32; // 14..=24
        let mut h = (m >> shift) as u16;
        let rem = m & ((1u32 << shift) - 1);
        let half = 1u32 << (shift - 1);
        if rem > half || (rem == half && h & 1 == 1) {
            h += 1;
        }
        return sign | h;
    }
    let mut h = ((exp as u16) << 10) | ((mant >> 13) as u16);
    let rem = mant & 0x1fff;
    // If rounding overflows the mantissa it carries into the exponent; correct.
    if rem > 0x1000 || (rem == 0x1000 && h & 1 == 1) {
        h += 1;
    }
    sign | h
}

/// binary16 -> f32. Lossless: the whole f16 range is representable in f32.
pub fn f32_from_f16(h: u16) -> f32 {
    let sign = ((h & 0x8000) as u32) << 16;
    let exp = ((h >> 10) & 0x1f) as u32;
    let mant = (h & 0x03ff) as u32;
    let bits = if exp == 0 {
        if mant == 0 {
            sign
        } else {
            // Subnormal: the value is mant * 2^-24, written as a normal in f32.
            let msb = 31 - mant.leading_zeros();
            let e = msb as i32 - 24;
            sign | (((e + 127) as u32) << 23) | ((mant << (23 - msb)) & 0x007f_ffff)
        }
    } else if exp == 0x1f {
        sign | 0x7f80_0000 | (mant << 13)
    } else {
        sign | ((exp + 127 - 15) << 23) | (mant << 13)
    };
    f32::from_bits(bits)
}

// ---------------------------------------------------------------- varint

pub fn put_uvarint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn get_uvarint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0;
    loop {
        let b = *buf
            .get(*pos)
            .ok_or_else(|| Error::Corrupt("varint ended early".into()))?;
        *pos += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift > 63 {
            return Err(Error::Corrupt("varint overflow".into()));
        }
    }
}

#[inline]
fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

// ---------------------------------------------------------------- values

/// Schema-aware encoding. `vector<N, f16>` fields are written halved;
/// for everything else this is identical to `encode_value`.
pub fn encode_value_as(out: &mut Vec<u8>, v: &Value, ty: Option<&DataType>) {
    if let (Value::Vector(vals), Some(DataType::Vector(_, VecPrec::F16))) = (v, ty) {
        out.push(TAG_VECTOR_F16);
        put_uvarint(out, vals.len() as u64);
        for f in vals {
            out.extend_from_slice(&f16_from_f32(*f).to_le_bytes());
        }
        return;
    }
    encode_value(out, v);
}

pub fn encode_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(TAG_NULL),
        Value::Bool(b) => {
            out.push(TAG_BOOL);
            out.push(*b as u8);
        }
        Value::Int(i) => {
            out.push(TAG_INT);
            put_uvarint(out, zigzag(*i));
        }
        Value::Timestamp(ms) => {
            out.push(TAG_TIMESTAMP);
            put_uvarint(out, zigzag(*ms));
        }
        Value::Float(f) => {
            out.push(TAG_FLOAT);
            out.extend_from_slice(&f.to_le_bytes());
        }
        Value::Text(s) => {
            out.push(TAG_TEXT);
            put_uvarint(out, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b) => {
            out.push(TAG_BYTES);
            put_uvarint(out, b.len() as u64);
            out.extend_from_slice(b);
        }
        Value::Vector(v) => {
            out.push(TAG_VECTOR);
            put_uvarint(out, v.len() as u64);
            // f32 LE array: allows zero-copy when read aligned.
            for f in v {
                out.extend_from_slice(&f.to_le_bytes());
            }
        }
        Value::List(items) => {
            out.push(TAG_LIST);
            put_uvarint(out, items.len() as u64);
            for it in items {
                encode_value(out, it);
            }
        }
        Value::Sparse(dim, entries) => {
            out.push(TAG_SPARSE);
            put_uvarint(out, *dim as u64);
            put_uvarint(out, entries.len() as u64);
            // Wrapping, as the reader adds: entries out of order -- a value
            // built by hand, never one a write coerced -- still round-trip.
            let mut last = 0u32;
            for (i, _) in entries {
                put_uvarint(out, i.wrapping_sub(last) as u64);
                last = *i;
            }
            for (_, w) in entries {
                out.extend_from_slice(&w.to_le_bytes());
            }
        }
    }
}

pub fn decode_value(buf: &[u8], pos: &mut usize) -> Result<Value> {
    let tag = *buf
        .get(*pos)
        .ok_or_else(|| Error::Corrupt("missing value tag".into()))?;
    *pos += 1;
    match tag {
        TAG_NULL => Ok(Value::Null),
        TAG_BOOL => {
            let b = *buf
                .get(*pos)
                .ok_or_else(|| Error::Corrupt("missing bool".into()))?;
            *pos += 1;
            Ok(Value::Bool(b != 0))
        }
        TAG_INT => Ok(Value::Int(unzigzag(get_uvarint(buf, pos)?))),
        TAG_TIMESTAMP => Ok(Value::Timestamp(unzigzag(get_uvarint(buf, pos)?))),
        TAG_FLOAT => {
            let raw = take(buf, pos, 8)?;
            Ok(Value::Float(f64::from_le_bytes(raw.try_into().unwrap())))
        }
        TAG_TEXT => {
            let n = get_uvarint(buf, pos)? as usize;
            let raw = take(buf, pos, n)?;
            Ok(Value::Text(
                std::str::from_utf8(raw)
                    .map_err(|_| Error::Corrupt("invalid utf8".into()))?
                    .to_string(),
            ))
        }
        TAG_BYTES => {
            let n = get_uvarint(buf, pos)? as usize;
            Ok(Value::Bytes(take(buf, pos, n)?.to_vec()))
        }
        TAG_VECTOR => {
            let n = get_uvarint(buf, pos)? as usize;
            let raw = take(buf, pos, n * 4)?;
            let mut v = Vec::with_capacity(n);
            for chunk in raw.as_chunks::<4>().0 {
                v.push(f32::from_le_bytes(*chunk));
            }
            Ok(Value::Vector(v))
        }
        TAG_VECTOR_F16 => {
            let n = get_uvarint(buf, pos)? as usize;
            let raw = take(buf, pos, n * 2)?;
            let mut v = Vec::with_capacity(n);
            for chunk in raw.as_chunks::<2>().0 {
                v.push(f32_from_f16(u16::from_le_bytes(*chunk)));
            }
            Ok(Value::Vector(v))
        }
        TAG_LIST => {
            let n = get_uvarint(buf, pos)? as usize;
            let mut items = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                items.push(decode_value(buf, pos)?);
            }
            Ok(Value::List(items))
        }
        TAG_SPARSE => {
            let dim = get_uvarint(buf, pos)? as u32;
            let n = get_uvarint(buf, pos)? as usize;
            let mut entries = Vec::with_capacity(n.min(1 << 16));
            let mut at = 0u32;
            for _ in 0..n {
                at = at.wrapping_add(get_uvarint(buf, pos)? as u32);
                entries.push((at, 0.0));
            }
            let raw = take(buf, pos, n * 4)?;
            for (e, w) in entries.iter_mut().zip(raw.as_chunks::<4>().0) {
                e.1 = f32::from_le_bytes(*w);
            }
            Ok(Value::Sparse(dim, entries))
        }
        other => Err(Error::Corrupt(format!("unknown value tag {other}"))),
    }
}

/// Skips over a value without decoding it. Lets projection move past the
/// fields it does not read without allocating.
pub fn skip_value(buf: &[u8], pos: &mut usize) -> Result<()> {
    let tag = *buf
        .get(*pos)
        .ok_or_else(|| Error::Corrupt("missing value tag".into()))?;
    *pos += 1;
    match tag {
        TAG_NULL => Ok(()),
        TAG_BOOL => {
            *pos += 1;
            Ok(())
        }
        TAG_INT | TAG_TIMESTAMP => {
            get_uvarint(buf, pos)?;
            Ok(())
        }
        TAG_FLOAT => {
            take(buf, pos, 8)?;
            Ok(())
        }
        TAG_TEXT | TAG_BYTES => {
            let n = get_uvarint(buf, pos)? as usize;
            take(buf, pos, n)?;
            Ok(())
        }
        TAG_VECTOR => {
            let n = get_uvarint(buf, pos)? as usize;
            take(buf, pos, n * 4)?;
            Ok(())
        }
        TAG_VECTOR_F16 => {
            let n = get_uvarint(buf, pos)? as usize;
            take(buf, pos, n * 2)?;
            Ok(())
        }
        TAG_LIST => {
            let n = get_uvarint(buf, pos)?;
            for _ in 0..n {
                skip_value(buf, pos)?;
            }
            Ok(())
        }
        TAG_SPARSE => {
            get_uvarint(buf, pos)?;
            let n = get_uvarint(buf, pos)? as usize;
            for _ in 0..n {
                get_uvarint(buf, pos)?;
            }
            take(buf, pos, n * 4)?;
            Ok(())
        }
        other => Err(Error::Corrupt(format!("unknown value tag {other}"))),
    }
}

#[inline]
fn take<'a>(buf: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = pos
        .checked_add(n)
        .ok_or_else(|| Error::Corrupt("length overflow".into()))?;
    if end > buf.len() {
        return Err(Error::Corrupt(format!(
            "{n} bytes requested, {} left",
            buf.len().saturating_sub(*pos)
        )));
    }
    let out = &buf[*pos..end];
    *pos = end;
    Ok(out)
}

// ---------------------------------------------------------------- types

pub fn encode_type(out: &mut Vec<u8>, ty: &DataType) {
    match ty {
        DataType::Bool => out.push(TAG_BOOL),
        DataType::Int => out.push(TAG_INT),
        DataType::Float => out.push(TAG_FLOAT),
        DataType::Text => out.push(TAG_TEXT),
        DataType::Bytes => out.push(TAG_BYTES),
        DataType::Timestamp => out.push(TAG_TIMESTAMP),
        DataType::Vector(d, prec) => {
            out.push(match prec {
                VecPrec::F32 => TAG_VECTOR,
                VecPrec::F16 => TAG_VECTOR_F16,
            });
            put_uvarint(out, *d as u64);
        }
        DataType::List(inner) => {
            out.push(TAG_LIST);
            encode_type(out, inner);
        }
        DataType::Sparse(d) => {
            out.push(TAG_SPARSE);
            put_uvarint(out, *d as u64);
        }
    }
}

pub fn decode_type(buf: &[u8], pos: &mut usize) -> Result<DataType> {
    let tag = *buf
        .get(*pos)
        .ok_or_else(|| Error::Corrupt("missing type tag".into()))?;
    *pos += 1;
    Ok(match tag {
        TAG_BOOL => DataType::Bool,
        TAG_INT => DataType::Int,
        TAG_FLOAT => DataType::Float,
        TAG_TEXT => DataType::Text,
        TAG_BYTES => DataType::Bytes,
        TAG_TIMESTAMP => DataType::Timestamp,
        TAG_VECTOR => DataType::Vector(get_uvarint(buf, pos)? as usize, VecPrec::F32),
        TAG_VECTOR_F16 => DataType::Vector(get_uvarint(buf, pos)? as usize, VecPrec::F16),
        TAG_LIST => DataType::List(Box::new(decode_type(buf, pos)?)),
        TAG_SPARSE => DataType::Sparse(get_uvarint(buf, pos)? as usize),
        other => return Err(Error::Corrupt(format!("unknown type tag {other}"))),
    })
}

pub fn encode_str(out: &mut Vec<u8>, s: &str) {
    put_uvarint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

pub fn decode_str(buf: &[u8], pos: &mut usize) -> Result<String> {
    let n = get_uvarint(buf, pos)? as usize;
    let raw = take(buf, pos, n)?;
    Ok(std::str::from_utf8(raw)
        .map_err(|_| Error::Corrupt("invalid utf8".into()))?
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_values() {
        let vals = vec![
            Value::Null,
            Value::Bool(true),
            Value::Int(-9_000_000_000),
            Value::Float(3.5),
            Value::Text("hello world".into()),
            Value::Bytes(vec![1, 2, 3]),
            Value::Vector(vec![0.1, -0.2, 0.3]),
            Value::List(vec![Value::Int(1), Value::Text("a".into())]),
            Value::Sparse(30_522, vec![(0, 0.5), (7, -1.25), (30_521, 3.0)]),
            Value::Sparse(4, vec![]),
        ];
        let mut buf = Vec::new();
        for v in &vals {
            encode_value(&mut buf, v);
        }
        let mut pos = 0;
        for v in &vals {
            assert_eq!(&decode_value(&buf, &mut pos).unwrap(), v);
        }
        assert_eq!(pos, buf.len());

        // skip_value must cover the same distance
        let mut skip_pos = 0;
        for _ in &vals {
            skip_value(&buf, &mut skip_pos).unwrap();
        }
        assert_eq!(skip_pos, buf.len());
    }

    #[test]
    fn half_precision_roundtrip() {
        // Exactly representable values must round-trip losslessly.
        for v in [
            0.0f32, -0.0, 1.0, -1.0, 0.5, 2.0, 0.25, 1024.0, 65504.0, -65504.0,
        ] {
            let back = f32_from_f16(f16_from_f32(v));
            assert_eq!(back, v, "{v} did not round-trip, got {back}");
        }
        // Special values.
        assert!(f32_from_f16(f16_from_f32(f32::INFINITY)).is_infinite());
        assert!(f32_from_f16(f16_from_f32(f32::NAN)).is_nan());
        // Overflow saturates to infinity, a very small value to zero.
        assert_eq!(f32_from_f16(f16_from_f32(1e30)), f32::INFINITY);
        assert_eq!(f32_from_f16(f16_from_f32(1e-30)), 0.0);
        // Subnormal range: the smallest f16 subnormal is 2^-24.
        assert_eq!(f32_from_f16(1), 2f32.powi(-24));
        assert_eq!(f16_from_f32(2f32.powi(-24)), 1);
        // Relative error must stay under 2^-11 (10-bit mantissa + implicit bit).
        let mut seed = 12345u32;
        for _ in 0..20_000 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let v = (seed as f32 / u32::MAX as f32) * 4.0 - 2.0;
            let back = f32_from_f16(f16_from_f32(v));
            assert!(
                (back - v).abs() <= v.abs() * 2f32.powi(-11) + 1e-7,
                "{v} -> {back}"
            );
        }
    }

    #[test]
    fn varint_edges() {
        for v in [0u64, 1, 127, 128, u32::MAX as u64, u64::MAX] {
            let mut b = Vec::new();
            put_uvarint(&mut b, v);
            let mut p = 0;
            assert_eq!(get_uvarint(&b, &mut p).unwrap(), v);
        }
    }
}
