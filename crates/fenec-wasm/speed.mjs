// How fast the browser module is, in Node: `make wasm-speed`, or
// `node crates/fenec-wasm/speed.mjs a.wasm b.wasm ...` to hold builds of it
// side by side -- a compiler option is taken only if it makes the module
// smaller without making it slower, and this is the slower half of that.
//
// Each module runs the same work in turn, and each figure is the best of
// its rounds, since what is compared is the code, not the machine's noise:
//   build   an HNSW graph of 10 000 x 128 clustered vectors, once
//   near    a query against it, `limit 10`
//   filter  a scan of 20 000 rows with a comparison and an order
//   match   BM25 over 20 000 short texts
//   json    a page of 200 768-dim vectors out of the module as JSON
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
  // Clustered, as embeddings are: 32 centres and a spread around each.
  const centres = Array.from({ length: 32 }, () => Array.from({ length: dim }, () => next() - 0.5));
  return Array.from({ length: n }, () => {
    const c = centres[Math.floor(next() * 32)];
    return Float32Array.from(c, (v) => v + (next() - 0.5) * 0.3);
  });
}

function best(rounds, f) {
  let b = Infinity;
  for (let r = 0; r < rounds; r++) {
    const t = performance.now();
    f();
    b = Math.min(b, performance.now() - t);
  }
  return b;
}

async function measure(path) {
  const bytes = await readFile(path);
  const out = {};

  let db = await Fenec.open(bytes);
  const next = rng(0x2545f491);
  const vs = vectors(10000, 128, next);
  db.run('create collection docs (n int, embed vector<128> @hnsw(cosine))');
  let t = performance.now();
  for (let i = 0; i < vs.length; i += 500) {
    await db.from('docs').insert(vs.slice(i, i + 500).map((embed, j) => ({ n: i + j, embed })));
  }
  out.build = performance.now() - t;
  const qs = vectors(200, 128, rng(7));
  out.near = best(5, () => {
    for (const q of qs) db.run('get docs near embed $1 limit 10', [Array.from(q)]);
  }) / qs.length;

  db = await Fenec.open(bytes);
  db.run('create collection rows (n int, score float, title text @text)');
  const words = ['vector', 'index', 'search', 'browser', 'module', 'query', 'text', 'graph', 'order', 'match'];
  const tn = rng(11);
  for (let i = 0; i < 20000; i += 1000) {
    await db.from('rows').insert(Array.from({ length: 1000 }, (_, j) => ({
      n: i + j,
      score: tn(),
      title: Array.from({ length: 6 }, () => words[Math.floor(tn() * 10)]).join(' '),
    })));
  }
  out.filter = best(10, () => db.run('get rows where n > 5000 and score < 0.5 order score desc limit 20'));
  out.match = best(10, () => db.run('get rows match title "vector graph order" limit 10'));

  db = await Fenec.open(bytes);
  db.run('create collection page (embed vector<768>)');
  const pn = rng(13);
  await db.from('page').insert(Array.from({ length: 200 }, () => ({ embed: Float32Array.from({ length: 768 }, () => pn() - 0.5) })));
  out.json = best(10, () => db.run('get page limit 200'));

  // A put of 1 000 128-dim rows as text, into a collection with a json
  // field beside the vector: what a list of numbers is read as is asked of
  // the schema there, and nowhere else.
  const pv = rng(17);
  const text = 'put t [' + Array.from({ length: 1000 }, (_, i) =>
    `{n: ${i}, embed: [${Array.from({ length: 128 }, () => pv() - 0.5).join(', ')}]}`).join(', ') + ']';
  for (const [key, schema] of [['put', ''], ['putJson', ', meta json']]) {
    out[key] = best(10, () => {
      const d = db;
      d.run('drop collection if exists t');
      d.run(`create collection t (n int, embed vector<128>${schema})`);
      d.run(text);
    });
  }
  return out;
}

const paths = process.argv.slice(2);
if (paths.length === 0) paths.push(new URL('../../web/fenec.wasm', import.meta.url).pathname);
console.log('module'.padEnd(40), 'build ms'.padStart(9), 'near ms'.padStart(9), 'filter ms'.padStart(10), 'match ms'.padStart(9), 'json ms'.padStart(9), 'put ms'.padStart(8), 'put+json'.padStart(9));
for (const p of paths) {
  const m = await measure(p);
  console.log(
    p.split('/').slice(-2).join('/').padEnd(40),
    m.build.toFixed(0).padStart(9),
    m.near.toFixed(3).padStart(9),
    m.filter.toFixed(2).padStart(10),
    m.match.toFixed(3).padStart(9),
    m.json.toFixed(2).padStart(9),
    m.put.toFixed(2).padStart(8),
    m.putJson.toFixed(2).padStart(9),
  );
}
