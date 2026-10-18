// JavaScript with fetch alone (Node 18 and newer, Deno, Bun, a browser).
// Run by ../run-tests.sh.
import assert from 'node:assert/strict';

const url = process.env.FENEC_URL ?? 'http://127.0.0.1:8080';
const token = process.env.FENEC_TOKEN;

async function query(q, params = []) {
  const res = await fetch(`${url}/query`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      ...(token && { authorization: `Bearer ${token}` }),
    },
    body: JSON.stringify({ query: q, params }),
  });
  const body = await res.json();
  if (!res.ok) throw Object.assign(new Error(body.error), { status: res.status });
  return body;
}

await query('create collection if not exists docs (title text @text, year int @sorted)');
await query('put docs {title: $1, year: $2}', ['Dunes', 2021]);
await query('put docs {title: $1, year: $2}', ['Night at the oasis', 2024]);

const rows = await query('get docs select title, year where year >= $1 order year desc limit $2', [2020, 5]);
assert.deepEqual(rows, [
  { title: 'Night at the oasis', year: 2024 },
  { title: 'Dunes', year: 2021 },
]);

const hits = await query('get docs select title match title $1 fuzzy', ['oasys']);
assert.deepEqual(hits.map((h) => h.title), ['Night at the oasis']);
assert.ok(hits[0]._score > 0);

// A refusal is a status and the server's message.
await assert.rejects(query('get nowhere'), (e) => e.status === 404);

console.log('javascript (fetch): ok');
