/* The site's one endpoint: a reader's query in, its embedding out.

     POST /api/embed  {"q": "how do I stop my file from growing"}
       -> 200 {"vector": [0.0123, ...]}   (1024 numbers, bge-m3)

   Everything else on fenecdb.com is a static file the asset router answers
   without running this (`run_worker_first` names /api/* alone). The search
   runs in the page; this only turns words into the vector the page's
   `near` needs, and the page searches by the words alone whenever it says
   no, is slow or is not there -- so every refusal here costs a reader
   nothing but the meaning.

   What it will not do: take anything but {q}, give anything but a vector,
   run any model but the one the index was built with, answer another
   site's page (no CORS: a browser will not hand another origin the
   answer, and Origin and Referer must be this site's), embed more than
   MAX_QUERY characters, or spend past the day's budget. In front of the
   model: this Worker's cache, a per-address rate limit, the day's budget,
   then the AI Gateway with its own cache and rate limit.

   Off until SEMANTIC is "on" (wrangler.jsonc): until the owner has made
   the gateway, it answers 404 and the page stays with the words. */

import { EMBED_MODEL, EMBED_DIM, MAX_QUERY, normalQuery } from './search-vectors.js';

const DAY = 86_400;
// A query's vector does not change while the model does not: a month.
const CACHE_TTL = 30 * DAY;
const MAX_BODY = 1024;
const AI_TIMEOUT_MS = 3000;

export default {
  fetch(request, env, ctx) {
    return handle(request, env, ctx);
  },
};

/** The endpoint, with what it uses handed in, for its tests to fake. */
export async function handle(request, env, ctx, cache = globalThis.caches?.default) {
  const url = new URL(request.url);
  if (url.pathname !== '/api/embed') {
    return env.ASSETS ? env.ASSETS.fetch(request) : answer(404, { error: 'not found' });
  }
  if (env.SEMANTIC !== 'on') return answer(404, { error: 'off' });
  if (request.method !== 'POST') return answer(405, { error: 'POST only' }, { allow: 'POST' });

  // This site's pages alone. A same-origin fetch sends Origin on a POST;
  // a form or a script elsewhere sends its own, and a browser's
  // Sec-Fetch-Site says where it came from whatever the page claims.
  const origin = request.headers.get('origin');
  const referer = request.headers.get('referer');
  const site = request.headers.get('sec-fetch-site');
  if (origin !== url.origin || (referer && !referer.startsWith(url.origin + '/')) || (site && site !== 'same-origin')) {
    return answer(403, { error: 'this site only' });
  }
  if (!(request.headers.get('content-type') ?? '').startsWith('application/json')) {
    return answer(415, { error: 'JSON only' });
  }

  const raw = await request.text();
  if (raw.length > MAX_BODY) return answer(413, { error: 'too long' });
  let q;
  try {
    const body = JSON.parse(raw);
    const fields = body && typeof body === 'object' && !Array.isArray(body) ? Object.keys(body) : null;
    if (!fields || fields.length !== 1 || fields[0] !== 'q' || typeof body.q !== 'string') throw 0;
    if ([...body.q].length > MAX_QUERY) return answer(413, { error: `at most ${MAX_QUERY} characters` });
    q = normalQuery(body.q);
  } catch {
    return answer(400, { error: 'send {"q": "..."}' });
  }
  if (!q) return answer(400, { error: 'empty' });

  // One question, one cache entry, wherever it was asked from.
  const key = new Request(`${url.origin}/api/embed/${await digest(`${EMBED_MODEL}\n${q}`)}`);
  const hit = cache && (await cache.match(key));
  if (hit) return hit;

  // An address asks this often at most; the gateway's own limit is over
  // every address together.
  if (env.PER_IP) {
    const ip = request.headers.get('cf-connecting-ip') ?? 'unknown';
    const { success } = await env.PER_IP.limit({ key: ip });
    if (!success) return answer(429, { error: 'slow down' }, { 'retry-after': '60' });
  }

  // The day's budget, counted before the model is asked: a query the
  // gateway answers from its cache is counted too, which only errs on the
  // side of spending less.
  if (env.BUDGET) {
    const day = env.BUDGET.get(env.BUDGET.idFromName('embed'));
    const spent = await day.fetch('https://budget/spend', { method: 'POST', body: String(tokensOf(q)) });
    if (spent.status === 429) return answer(429, { error: 'budget' }, { 'retry-after': String(secondsToMidnight()) });
  }

  let vector;
  try {
    const run = env.AI.run(EMBED_MODEL, { text: [q] }, {
      gateway: env.AI_GATEWAY ? { id: env.AI_GATEWAY, cacheTtl: CACHE_TTL, cacheKey: key.url } : undefined,
    });
    const got = await Promise.race([run, timeout(AI_TIMEOUT_MS)]);
    vector = got?.data?.[0];
    if (!Array.isArray(vector) || vector.length !== EMBED_DIM || !vector.every(Number.isFinite)) throw new Error('no vector');
  } catch {
    return answer(503, { error: 'unavailable' });
  }

  // Seven digits are more than an f16, which the index keeps, can hold.
  const res = answer(200, { vector: vector.map((x) => Number(x.toPrecision(7))) }, {
    'cache-control': `public, max-age=${CACHE_TTL}`,
  });
  if (cache) ctx?.waitUntil?.(cache.put(key, res.clone()));
  return res;
}

/** The day's spend, in tokens, on one Durable Object: a count that is the
    same wherever a query arrives. It forgets yesterday's at midnight UTC. */
export class Budget {
  constructor(state, env) {
    this.state = state;
    this.env = env;
  }

  async fetch(request) {
    const n = Number(await request.text()) || 0;
    const today = new Date().toISOString().slice(0, 10);
    const day = await this.state.storage.get('day');
    let spent = day === today ? (await this.state.storage.get('spent')) ?? 0 : 0;
    if (spent + n > dailyTokens(this.env)) return new Response('budget', { status: 429 });
    spent += n;
    await this.state.storage.put({ day: today, spent });
    return new Response(String(spent));
  }
}

/* The cap is in neurons, Workers AI's unit (wrangler.jsonc,
   DAILY_NEURONS), turned into tokens at bge-m3's 1 075 neurons per million
   input tokens. */
export function dailyTokens(env) {
  const neurons = Number(env.DAILY_NEURONS ?? 5000);
  return Math.floor((neurons / 1075) * 1e6);
}

/* What a query may cost, counted high: a token for every two characters,
   and the two the model adds. */
export const tokensOf = (q) => Math.ceil(q.length / 2) + 2;

function secondsToMidnight() {
  const now = Date.now();
  return Math.ceil((Math.ceil(now / (DAY * 1000)) * DAY * 1000 - now) / 1000);
}

function timeout(ms) {
  return new Promise((_, no) => setTimeout(() => no(new Error('timeout')), ms));
}

async function digest(text) {
  const bytes = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text));
  return [...new Uint8Array(bytes)].slice(0, 16).map((b) => b.toString(16).padStart(2, '0')).join('');
}

function answer(status, body, headers = {}) {
  return new Response(JSON.stringify(body), {
    status,
    headers: {
      'content-type': 'application/json',
      'cache-control': 'no-store',
      'x-content-type-options': 'nosniff',
      ...headers,
    },
  });
}
