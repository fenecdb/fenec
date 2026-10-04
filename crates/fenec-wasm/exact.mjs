// `near` in a module without the graph, in Node: `make wasm-exact-speed`.
//
// A module made without `vector` -- the test build `make wasm-lite` --
// answers `near` by measuring every vector, as `near ... exact` does in the
// full one. This is what that costs as a collection grows: 1 000, 10 000 and
// 50 000 clustered vectors of 128 and 384 dimensions, a `limit 10` each,
// against the full module's walk of its graph and its own exact scan over
// the same image. Each figure is the median query of the best of three
// rounds of 50.
//
//   node crates/fenec-wasm/exact.mjs [full.wasm] [without-graph.wasm ...]
//
// Several modules without the graph are measured side by side, in turns
// over the same images.
import { readFile } from 'node:fs/promises';

const { Fenec } = await import(new URL('../../web/fenec.js', import.meta.url));

function rng(seed) {
  let x = BigInt(seed);
  return () => {
    x ^= x << 13n; x &= 0xffffffffn;
    x ^= x >> 17n;
    x ^= x << 5n; x &= 0xffffffffn;
    return Number(x) / 4294967296;
  };
}

function vectors(n, dim, next) {
  const centres = Array.from({ length: 32 }, () => Array.from({ length: dim }, () => next() - 0.5));
  return Array.from({ length: n }, () => {
    const c = centres[Math.floor(next() * 32)];
    return Float32Array.from(c, (v) => v + (next() - 0.5) * 0.3);
  });
}

function median(db, sql, qs) {
  let best = Infinity;
  for (let round = 0; round < 3; round++) {
    const times = qs.map((q) => {
      const t = performance.now();
      db.run(sql, [q]);
      return performance.now() - t;
    });
    times.sort((a, b) => a - b);
    best = Math.min(best, times[times.length >> 1]);
  }
  return best;
}

const here = (p) => new URL(p, import.meta.url).pathname;
const [fullPath = here('../../web/fenec.wasm'), ...barePaths] = process.argv.slice(2);
if (barePaths.length === 0) barePaths.push(here('../../web/fenec-lite.wasm'));
const fullBytes = await readFile(fullPath);
const bares = await Promise.all(barePaths.map((p) => readFile(p)));
const sizes = (process.env.ROWS ?? '1000,10000,50000').split(',').map(Number);

console.log(
  'rows x dim'.padEnd(14),
  'graph ms'.padStart(9),
  'exact ms'.padStart(9),
  ...barePaths.map((p) => `${p.split('/').pop()} ms`.padStart(24)),
);
for (const dim of [128, 384]) {
  for (const n of sizes) {
    const next = rng(0x2545f491 + dim);
    const full = await Fenec.open(fullBytes);
    full.run(`create collection docs (n int, embed vector<${dim}> @hnsw(cosine))`);
    const vs = vectors(n, dim, next);
    for (let i = 0; i < n; i += 1000) {
      await full.from('docs').insert(vs.slice(i, i + 1000).map((embed, j) => ({ n: i + j, embed })));
    }
    const image = full.snapshot();
    const without = [];
    for (const bytes of bares) {
      const bare = await Fenec.open(bytes);
      bare.load(image);
      without.push(bare);
    }
    const qs = vectors(50, dim, rng(7 + dim)).map((q) => Array.from(q));
    const graph = median(full, 'get docs near embed $1 limit 10', qs);
    const exact = median(full, 'get docs near embed $1 exact limit 10', qs);
    const times = without.map((bare) => median(bare, 'get docs near embed $1 limit 10', qs));
    console.log(
      `${n} x ${dim}`.padEnd(14),
      graph.toFixed(3).padStart(9),
      exact.toFixed(3).padStart(9),
      ...times.map((t) => t.toFixed(3).padStart(24)),
    );
    full.close();
    for (const bare of without) bare.close();
  }
}
