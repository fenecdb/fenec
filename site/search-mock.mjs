// The built site with a stand-in for its embedding endpoint, for trying
// the search by meaning without a Cloudflare account.
//
//   node site/search-mock.mjs [--port 8789] [--off] [--slow ms]
//
// Serves site/dist as the asset router does (`/docs/x` is docs/x.html) and
// answers POST /api/embed from search-fixture.json: the embeddings of the
// queries the tests ask, made once with the model the Worker calls. A query
// the fixture does not hold is a 503, as the Worker answers when the model
// fails, and the page keeps to the words; --off answers 404 to everything,
// as the Worker does before the owner turns it on; --slow holds each answer
// back, past the page's timeout to see it give up.

import { createServer } from 'node:http';
import { readFileSync, existsSync, statSync } from 'node:fs';
import { extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';
import { normalQuery } from './search-vectors.js';
import { fixtureVector } from './search-fixture.mjs';

const here = fileURLToPath(new URL('.', import.meta.url));
const dist = join(here, 'dist');
const arg = (name, fallback) => {
  const i = process.argv.indexOf(name);
  return i > 0 ? process.argv[i + 1] : fallback;
};
const port = Number(arg('--port', 8789));
const off = process.argv.includes('--off');
const slow = Number(arg('--slow', 0));

const TYPES = {
  '.html': 'text/html; charset=utf-8', '.js': 'text/javascript', '.css': 'text/css',
  '.wasm': 'application/wasm', '.svg': 'image/svg+xml', '.json': 'application/json',
  '.gz': 'application/gzip', '.txt': 'text/plain; charset=utf-8', '.bin': 'application/octet-stream',
};

createServer(async (req, res) => {
  const url = new URL(req.url, 'http://localhost');
  if (url.pathname === '/api/embed') {
    if (off || req.method !== 'POST') return send(res, off ? 404 : 405, { error: off ? 'off' : 'POST only' });
    let body = '';
    for await (const chunk of req) body += chunk;
    let q;
    try { q = normalQuery(JSON.parse(body).q); } catch { return send(res, 400, { error: 'send {"q": "..."}' }); }
    const vector = fixtureVector(q);
    if (slow) await new Promise((r) => setTimeout(r, slow));
    return vector ? send(res, 200, { vector }) : send(res, 503, { error: 'unavailable' });
  }
  let path = normalize(join(dist, decodeURIComponent(url.pathname)));
  if (!path.startsWith(dist)) return send(res, 403, { error: 'no' });
  if (existsSync(path) && statSync(path).isDirectory()) path = join(path, 'index.html');
  else if (!existsSync(path) && existsSync(path + '.html')) path += '.html';
  if (!existsSync(path)) return send(res, 404, { error: 'not found' });
  res.writeHead(200, { 'content-type': TYPES[extname(path)] ?? 'application/octet-stream' });
  res.end(readFileSync(path));
}).listen(port, () => console.log(`http://localhost:${port}${off ? ' (endpoint off)' : ''}`));

function send(res, status, body) {
  res.writeHead(status, { 'content-type': 'application/json' });
  res.end(JSON.stringify(body));
}
