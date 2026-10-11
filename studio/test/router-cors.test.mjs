// A page on another origin talking to fenec-shard (`--http-cors`) in a
// headless Chrome, through the real client (`web/client.js`): the router's
// own answers -- an unknown tenant's 404, a standby's 503, an unreachable
// node's 502 -- reach the page as the refusals they are, where they read as
// the network failing; a node's answer through the router names one origin
// though the node allows `*` too, which a browser would refuse whole; and a
// page on an origin the router does not allow reads nothing.
//
//   cd studio && npm ci && node --test test/router-cors.test.mjs   (make studio-test)

import test from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { startServer, startRouter, startStandby, browser as launch, query, TOKEN } from './harness.mjs';

const WEB = join(dirname(fileURLToPath(import.meta.url)), '..', '..', 'web');

/** The app's origin: a blank page, and the client's modules under /web/. */
async function startApp() {
  const server = createServer(async (req, res) => {
    const path = new URL(req.url, 'http://x').pathname;
    const name = path.match(/^\/web\/([a-z.]+\.js)$/)?.[1];
    if (name) {
      try {
        const body = await readFile(join(WEB, name));
        res.writeHead(200, { 'content-type': 'text/javascript; charset=utf-8' });
        return res.end(body);
      } catch {
        /* a 404 below */
      }
    }
    if (path === '/') {
      res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
      return res.end('<!doctype html><title>app</title>');
    }
    res.writeHead(404);
    res.end();
  });
  const port = await new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve(server.address().port)));
  return { port, url: `http://127.0.0.1:${port}`, stop: () => new Promise((r) => server.close(r)) };
}

/** What `connect(url).rows(text)` comes to in the page: its rows, or what it threw. */
function outcome(p, url, text) {
  return p.evaluate(
    async (url, text, token) => {
      const { connect, FenecError } = await import(`${location.origin}/web/client.js`);
      try {
        return { rows: await connect(url, { token }).rows(text) };
      } catch (e) {
        return { status: e.status ?? null, retryAfter: e.retryAfter ?? null, message: e.message, refused: e instanceof FenecError };
      }
    },
    url,
    text,
    TOKEN,
  );
}

/** A raw fetch's status and the headers a page on another origin can read. */
function seen(p, url) {
  return p.evaluate(async (url) => {
    const r = await fetch(url, { method: 'POST', headers: { 'content-type': 'application/json', 'idempotency-key': 'k' }, body: '{}' });
    return { status: r.status, requestId: r.headers.get('x-request-id'), retryAfter: r.headers.get('retry-after') };
  }, url);
}

test('a page on another origin reads the router\'s own answers and a node\'s through it', async () => {
  const app = await startApp();
  const cors = ['--http-cors', app.url];
  // The node allows every origin as well: its header and the router's
  // both kept, the answer named two and Chrome refused it.
  const node = await startServer({ tenants: true, studio: false, extra: ['--http-cors', '*'] });
  const router = await startRouter(node, ['acme'], { extra: cors });
  const standby = await startStandby({ extra: cors });
  const browser = await launch();
  try {
    await query(`${router.url}/t/acme`, 'create collection notes (title text)');
    await query(`${router.url}/t/acme`, 'insert notes {title: $1}', ['first']);
    const p = await browser.newPage();
    await p.goto(`${app.url}/`);

    const forwarded = await outcome(p, `${router.url}/t/acme`, 'get notes');
    assert.deepEqual(forwarded.rows?.map((r) => r.title), ['first'], JSON.stringify(forwarded));

    const unknown = await outcome(p, `${router.url}/t/nosuch`, 'get notes');
    assert.equal(unknown.refused, true, JSON.stringify(unknown));
    assert.equal(unknown.status, 404);
    assert.match(unknown.message, /no tenant `nosuch`/);
    const raw404 = await seen(p, `${router.url}/t/nosuch/query`);
    assert.equal(raw404.status, 404);
    assert.match(raw404.requestId ?? '', /\S/);

    const early = await outcome(p, `${standby.url}/t/acme`, 'get notes');
    assert.equal(early.refused, true, JSON.stringify(early));
    assert.equal(early.status, 503);
    assert.equal(early.retryAfter, 1);
    assert.equal((await seen(p, `${standby.url}/t/acme/query`)).retryAfter, '1');

    // A node the router cannot reach.
    await node.stop();
    const gone = await outcome(p, `${router.url}/t/acme`, 'get notes');
    assert.equal(gone.refused, true, JSON.stringify(gone));
    assert.equal(gone.status, 502);

    // Another origin -- `localhost` is not `127.0.0.1` to a browser -- is
    // told nothing: the fetch fails as the network would.
    await p.goto(`http://localhost:${app.port}/`);
    const other = await outcome(p, `${router.url}/t/nosuch`, 'get notes');
    assert.equal(other.refused, false, JSON.stringify(other));
    assert.equal(other.status, null);
    await p.close();
  } finally {
    await browser.close();
    await standby.stop();
    await router.stop();
    await node.stop().catch(() => {});
    await app.stop();
  }
});
