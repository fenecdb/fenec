/* A query's embedding, asked of the site's endpoint (`worker.js`), for the
   search by meaning. The page never waits on it: the words are searched
   at once, and the vector, when it comes, ranks the same query again.
   Anything but a vector in time -- no endpoint, a refusal, a slow answer,
   no network -- is null, and the search stays with the words. */

import { EMBED_DIM, MAX_QUERY, normalQuery } from './search-vectors.js';

/* How long a reader's pause is given to the endpoint. Past it the answer
   is no longer worth a list that moves under the reader's eyes. */
export const TIMEOUT_MS = 800;

/* A question is asked of the meaning; a word or two is looked up. One or
   two words are a name the reader knows -- `compact`, `replica failover`
   -- and the words find its section first, where the meaning put
   `compact`'s second (the heading counts three times among the words, and
   not beside the meaning). Three words and more read as a question. */
export const MIN_WORDS = 3;

export const wordsIn = (text) => String(text).trim().split(/\s+/).filter(Boolean).length;

export function asker({ endpoint = '/api/embed', fetch = globalThis.fetch, timeout = TIMEOUT_MS, now = Date.now } = {}) {
  const known = new Map(); // normalised query -> vector or null
  let offUntil = 0; // a refusal that holds for a while: off until then

  return async function vectorFor(text) {
    const q = normalQuery(text);
    if (!q || wordsIn(q) < MIN_WORDS || [...String(text).trim()].length > MAX_QUERY || now() < offUntil) return null;
    if (known.has(q)) return known.get(q);
    let vector = null;
    try {
      const r = await fetch(endpoint, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ q }),
        credentials: 'same-origin',
        signal: AbortSignal.timeout(timeout),
      });
      if (r.status === 404 || r.status === 405 || r.status === 501) {
        // No endpoint here (a local server, or off): not asked again.
        offUntil = Infinity;
      } else if (r.status === 429) {
        // A budget spent or a limit reached: the words alone, for a while.
        const wait = Number(r.headers.get('retry-after')) || 60;
        offUntil = now() + Math.min(wait, 3600) * 1000;
      } else if (r.ok) {
        const got = (await r.json())?.vector;
        if (Array.isArray(got) && got.length === EMBED_DIM && got.every(Number.isFinite)) vector = got;
      }
    } catch {
      // A timeout or no network: this query goes without, the next may not.
      return null;
    }
    known.set(q, vector);
    return vector;
  };
}
