// Vectors kept as f16 in base64, for the embeddings committed beside the
// site (search-embeddings.json, search-fixture.json).

/* A vector as f16, the width the index keeps, in base64: a third of its
   text as JSON numbers. Rounded to the nearest, ties to even, as the engine
   rounds an f32 into an f16 field -- so the image holds what it would hold
   handed the f32s. */
export function packF16(v) {
  const out = new Uint16Array(v.length);
  const f = new Float32Array(1);
  const u = new Uint32Array(f.buffer);
  for (let i = 0; i < v.length; i++) {
    f[0] = v[i];
    const x = u[0];
    const sign = (x >>> 16) & 0x8000;
    const exp = (x >>> 23) & 0xff;
    let mant = x & 0x7fffff;
    let h;
    if (exp === 0xff) h = sign | 0x7c00 | (mant ? 0x200 : 0);
    else {
      const e = exp - 127 + 15;
      if (e >= 0x1f) h = sign | 0x7c00;
      else if (e <= 0) {
        if (e < -10) h = sign;
        else {
          mant |= 0x800000;
          const shift = 14 - e;
          let m = mant >>> shift;
          const rest = mant & ((1 << shift) - 1);
          const half = 1 << (shift - 1);
          if (rest > half || (rest === half && (m & 1))) m++;
          h = sign | m;
        }
      } else {
        let m = mant >>> 13;
        const rest = mant & 0x1fff;
        h = sign | (e << 10) | m;
        if (rest > 0x1000 || (rest === 0x1000 && (m & 1))) h++;
      }
    }
    out[i] = h;
  }
  return Buffer.from(out.buffer).toString('base64');
}

export function unpackF16(b64) {
  const bytes = Buffer.from(b64, 'base64');
  const h = new Uint16Array(bytes.buffer, bytes.byteOffset, bytes.length / 2);
  const v = new Float32Array(h.length);
  for (let i = 0; i < h.length; i++) {
    const s = h[i] & 0x8000 ? -1 : 1;
    const e = (h[i] >>> 10) & 0x1f;
    const m = h[i] & 0x3ff;
    v[i] = e === 0 ? s * m * 2 ** -24 : e === 0x1f ? (m ? NaN : s * Infinity) : s * (1 + m / 1024) * 2 ** (e - 15);
  }
  return v;
}
