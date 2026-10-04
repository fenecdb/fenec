// The embeddings of the queries the tests ask (search-fixture.json), made
// once with the model the Worker calls, so the search by meaning is tested
// with no network: search.test.mjs and the endpoint's stand-in
// (search-mock.mjs) read them here, by the query as the Worker normalises it.

import { readFileSync } from 'node:fs';
import { normalQuery, EMBED_MODEL } from './search-vectors.js';
import { unpackF16 } from './search-f16.mjs';

const fixture = JSON.parse(readFileSync(new URL('./search-fixture.json', import.meta.url), 'utf8'));
if (fixture.model !== EMBED_MODEL) throw new Error(`search-fixture.json is ${fixture.model}'s, not ${EMBED_MODEL}'s`);

/** The query's vector, or null when the fixture does not hold it. */
export function fixtureVector(q) {
  const v = fixture.queries[normalQuery(q)];
  return v ? Array.from(unpackF16(v)) : null;
}
