// The sections' vectors, for the search by meaning.
//
//   node site/search-embed.mjs <out.json> < documents.json
//
// `build.py` hands over the documents it cut the pages into; this writes
// one vector per document to <out.json> (null where it has none) and says
// on stderr how many it had, embedded and lacked.
//
// A vector is kept in `search-embeddings.json`, committed beside this file,
// by a hash of the model and the text it embedded: a rebuild embeds only
// the sections that changed, the build is the same on every machine, and
// one with no network or no Cloudflare credentials -- a pull request's CI,
// a laptop -- still ships every vector the cache holds. A section changed
// and not embedded here keeps the vector its last text had (the cache
// names each section's by its URL), close enough until the next build that
// can embed it; a section new and not embedded goes without one, found by
// its words alone. The build says how many of each, and never fails over
// them.
//
// New vectors come from Workers AI, the model the Worker embeds a query
// with (search-vectors.js), over its REST API when CLOUDFLARE_ACCOUNT_ID
// and a token that may run Workers AI are set (CLOUDFLARE_AI_TOKEN, else
// CLOUDFLARE_API_TOKEN), through the AI Gateway CLOUDFLARE_AI_GATEWAY
// names when it is set. The cache is written back with what was embedded
// and without what no section holds any more: commit it.
//
//   node site/search-embed.mjs --fixture
//
// embeds again, the same way, the queries search-fixture.json holds -- the
// tests' questions -- so the fixture and the cache come from one model run
// the same way. Run both after a change of model.

import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { EMBED_MODEL, EMBED_DIM, embedText } from './search-vectors.js';
import { packF16, unpackF16 } from './search-f16.mjs';

const CACHE = fileURLToPath(new URL('./search-embeddings.json', import.meta.url));
const BATCH = 50;

const [out] = process.argv.slice(2);
if (out === '--fixture') {
  const FIXTURE = fileURLToPath(new URL('./search-fixture.json', import.meta.url));
  const fixture = JSON.parse(readFileSync(FIXTURE, 'utf8'));
  const account = process.env.CLOUDFLARE_ACCOUNT_ID;
  const token = process.env.CLOUDFLARE_AI_TOKEN || process.env.CLOUDFLARE_API_TOKEN;
  if (!account || !token) throw new Error('--fixture needs CLOUDFLARE_ACCOUNT_ID and a token that may run Workers AI');
  const queries = Object.keys(fixture.queries).sort();
  const vectors = await workersAi(account, token, queries);
  const lines = queries.map((q, i) => `    ${JSON.stringify(q)}: ${JSON.stringify(packF16(vectors[i]))}`);
  writeFileSync(FIXTURE, `{\n  "model": ${JSON.stringify(EMBED_MODEL)},\n  "made": "Workers AI",\n  "queries": {\n${lines.join(',\n')}\n  }\n}\n`);
  process.exit(0);
}
const docs = JSON.parse(readFileSync(0, 'utf8'));
const key = (text) => createHash('sha256').update(`${EMBED_MODEL}\n${text}`).digest('hex').slice(0, 24);

const cache = existsSync(CACHE) ? JSON.parse(readFileSync(CACHE, 'utf8')) : null;
const held = cache?.model === EMBED_MODEL && cache?.dim === EMBED_DIM ? cache.vectors : {};

const texts = docs.map(embedText);
const keys = texts.map(key);
const missing = [...new Set(keys.filter((k) => !held[k]))];
const textOf = new Map(keys.map((k, i) => [k, texts[i]]));

let embedded = 0;
let why = null;
if (missing.length) {
  const account = process.env.CLOUDFLARE_ACCOUNT_ID;
  const token = process.env.CLOUDFLARE_AI_TOKEN || process.env.CLOUDFLARE_API_TOKEN;
  if (!account || !token) {
    why = 'no CLOUDFLARE_ACCOUNT_ID and token to embed them with';
  } else {
    try {
      for (let i = 0; i < missing.length; i += BATCH) {
        const part = missing.slice(i, i + BATCH);
        const vectors = await workersAi(account, token, part.map((k) => textOf.get(k)));
        part.forEach((k, j) => { held[k] = packF16(vectors[j]); });
        embedded += part.length;
      }
    } catch (e) {
      why = String(e?.message ?? e);
    }
  }
}

// A section whose text changed and could not be embedded keeps the vector
// its URL had.
const before = cache?.sections ?? {};
let stale = 0;
const used = keys.map((k, i) => {
  if (held[k]) return k;
  const old = before[docs[i].url];
  if (old && held[old]) {
    stale++;
    return old;
  }
  return null;
});
const vectors = used.map((k) => (k ? unpackF16(held[k]) : null));
writeFileSync(out, JSON.stringify(vectors.map((v) => (v ? Array.from(v) : null))));

// Written back with this build's sections alone, a line a vector so a
// change to one section is a change to one line.
const kept = [...new Set(used.filter(Boolean))].sort();
const lines = kept.map((k) => `    ${JSON.stringify(k)}: ${JSON.stringify(held[k])}`);
const sections = docs.map((d, i) => [d.url, used[i]]).filter(([, k]) => k).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
const named = sections.map(([u, k]) => `    ${JSON.stringify(u)}: ${JSON.stringify(k)}`);
const body = `{\n  "model": ${JSON.stringify(EMBED_MODEL)},\n  "dim": ${EMBED_DIM},\n  "sections": {\n${named.join(',\n')}\n  },\n  "vectors": {\n${lines.join(',\n')}\n  }\n}\n`;
if (!existsSync(CACHE) || readFileSync(CACHE, 'utf8') !== body) writeFileSync(CACHE, body);

const lacking = vectors.filter((v) => !v).length;
process.stderr.write(JSON.stringify({ sections: docs.length, cached: docs.length - lacking - embedded - stale, embedded, stale, lacking, why }) + '\n');

/* One batch through Workers AI's REST API, or through the AI Gateway in
   front of it: each text's vector, in order. */
async function workersAi(account, token, text) {
  const gateway = process.env.CLOUDFLARE_AI_GATEWAY;
  const url = gateway
    ? `https://gateway.ai.cloudflare.com/v1/${account}/${gateway}/workers-ai/${EMBED_MODEL}`
    : `https://api.cloudflare.com/client/v4/accounts/${account}/ai/run/${EMBED_MODEL}`;
  const r = await fetch(url, {
    method: 'POST',
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: JSON.stringify({ text }),
    signal: AbortSignal.timeout(60_000),
  });
  if (!r.ok) throw new Error(`Workers AI answered ${r.status}: ${(await r.text()).slice(0, 200)}`);
  const got = await r.json();
  const data = got.result?.data ?? got.data;
  if (!Array.isArray(data) || data.length !== text.length || data.some((v) => v.length !== EMBED_DIM)) {
    throw new Error('Workers AI answered no vectors of the width expected');
  }
  return data;
}
