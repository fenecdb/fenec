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

await query('create collection if not exists docs (title text, embed vector<3> @hnsw(cosine))');
await query('put docs {title: $1, embed: $2}', ['Night at the oasis', [0.1, 0.2, 0.3]]);
await query('put docs {title: $1, embed: $2}', ['Dunes', [0.9, 0.1, 0.0]]);

const rows = await query('get docs select title near embed $1 limit 5', [[0.1, 0.2, 0.3]]);
assert.deepEqual(rows.map((r) => r.title), ['Night at the oasis', 'Dunes']);

// A refusal is a status and the server's message.
await assert.rejects(query('get nowhere'), (e) => e.status === 404);

console.log('javascript (fetch): ok');
