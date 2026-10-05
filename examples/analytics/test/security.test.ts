// Who can read and write what: a site's dashboard token on every route of
// another site, the ingest token's one grant, beacons that are malformed,
// oversized or from elsewhere, the per-site rate limit, forged tokens, and
// the dashboard's own sessions.
import assert from 'node:assert/strict';
import { createHmac, randomBytes } from 'node:crypto';
import { before, describe, test } from 'node:test';
import { JWT_SECRET } from '../src/config.ts';
import { MAX_BODY } from '../src/ingest.ts';
import { DEMO_PASSWORD, writeEvents, type Site } from '../src/setup.ts';
import { userAgent } from '../src/sim.ts';
import { mint, sign, tokens } from '../src/tokens.ts';
import { APP, beacon, newSite, node, OPERATOR, ORIGIN, pool, rows } from './helpers.ts';

const id = () => randomBytes(9).toString('base64url');
const ev = (n = 'pageview', extra: Record<string, unknown> = {}) => ({ n, p: '/', t: Date.now(), ...extra });
const body = (site: Site, events: unknown[], extra: Record<string, unknown> = {}) => ({ k: site.key, b: id(), u: 'visitor-0001', t: Date.now(), e: events, ...extra });

let a: Site;
let b: Site;
before(async () => {
  a = await newSite('t-sec-a');
  b = await newSite('t-sec-b');
  for (const s of [a, b]) {
    await writeEvents(s.name, [{ eid: `${s.name}-1`, name: 'pageview', user: `secret-${s.name}`, path: '/private', ref: '', country: 'DE', device: 'desktop', browser: 'Firefox', at: Date.now() - 60_000, props: null }]);
  }
});

/** Every route of the node under /t/<tenant>/, as fenec-http serves them. */
function routes(t: string): [string, RequestInit][] {
  const q = (query: string) => ({ method: 'POST', body: JSON.stringify({ query }) });
  return [
    [`/t/${t}/query`, q('get events limit 5')],
    [`/t/${t}/query`, q('get events select count(*)')],
    [`/t/${t}/query`, q('get minutes limit 5')],
    [`/t/${t}/query`, q('put events {eid: "x", name: "pageview", user: "u", at: now()}')],
    [`/t/${t}/query`, q('set minutes {n: 0}')],
    [`/t/${t}/query`, q('del events')],
    [`/t/${t}/query`, q('drop collection events')],
    [`/t/${t}/batch`, { method: 'POST', body: JSON.stringify({ statements: [{ query: 'get events limit 1' }] }) }],
    [`/t/${t}/`, {}],
    [`/t/${t}/collections`, {}],
    [`/t/${t}/events`, {}],
    [`/t/${t}/events?where=user%20%3D%20%22x%22`, {}],
    [`/t/${t}/events`, { method: 'POST', body: JSON.stringify([{ eid: 'y', name: 'pageview', user: 'u', at: '2026-01-01T00:00:00Z' }]) }],
    [`/t/${t}/events`, { method: 'PATCH', body: JSON.stringify({ name: 'x' }) }],
    [`/t/${t}/events`, { method: 'DELETE' }],
    [`/t/${t}/pulse`, { headers: { accept: 'text/event-stream' } }],
    [`/t/${t}/_schema`, {}],
    [`/t/${t}/_schema/plan`, { method: 'POST', body: JSON.stringify({ format: 1, fenecql: 'create collection z (a int)' }) }],
    [`/t/${t}/_changes?since=0`, {}],
    [`/t/${t}/_changes/consumers`, {}],
    [`/t/${t}/_stats/statements`, {}],
    [`/t/${t}/_replication/status`, {}],
  ];
}

describe("a site's tokens reach their own tenant only", () => {
  test("site A's dashboard and ingest tokens are refused on every route of site B, and say nothing of it", async () => {
    for (const role of ['viewer', 'ingest'] as const) {
      const token = tokens.get(a.name, role);
      for (const [path, init] of routes(b.name)) {
        const r = await node(path, token, init);
        // 403 at the tenant's door; the replication feed asks its own token first (401).
        assert.ok(r.status === 403 || (r.status === 401 && path.includes('_replication')), `${role} ${init.method ?? 'GET'} ${path}: ${r.status} ${r.body}`);
        assert.ok(!r.body.includes('secret-'), `${path} leaked a row`);
      }
      // A tenant that does not exist is refused the same way: the refusal says nothing of which exist.
      const r = await node(`/t/no-such-site/query`, token, routes('x')[0][1]);
      assert.equal(r.status, 403);
    }
  });

  test('a token naming no tenant, another secret, no exp, an expiry past or alg none is refused', async () => {
    const forged = [
      mint({ sub: 'x', role: 'viewer' }, 600),
      mint({ sub: 'x', role: 'viewer', tenant: a.name }, 600, 'another-secret-of-at-least-32-bytes-long'),
      mint({ sub: 'x', role: 'viewer', tenant: a.name }, -60),
      (() => {
        const b64 = (o: unknown) => Buffer.from(JSON.stringify(o)).toString('base64url');
        return `${b64({ alg: 'none', typ: 'JWT' })}.${b64({ sub: 'x', role: 'viewer', tenant: a.name, exp: Math.floor(Date.now() / 1000) + 600 })}.`;
      })(),
      (() => {
        const b64 = (o: unknown) => Buffer.from(JSON.stringify(o)).toString('base64url');
        const h = b64({ alg: 'HS256', typ: 'JWT' });
        const p = b64({ sub: 'x', role: 'viewer', tenant: a.name });
        return `${h}.${p}.${createHmac('sha256', JWT_SECRET).update(`${h}.${p}`).digest('base64url')}`;
      })(),
    ];
    for (const [i, t] of forged.entries()) {
      const r = await node(`/t/${a.name}/query`, t, routes(a.name)[0][1]);
      assert.ok(r.status === 401 || r.status === 403, `token ${i}: ${r.status} ${r.body}`);
    }
  });

  test('the ingest token inserts events and does nothing else', async () => {
    const t = tokens.get(a.name, 'ingest');
    const q = async (query: string) => (await node(`/t/${a.name}/query`, t, { method: 'POST', body: JSON.stringify({ query }) })).status;
    assert.equal(await q(`put events [{eid: "${id()}", name: "pageview", user: "u-ingest", path: "/", at: now()}] if absent`), 200);
    for (const s of [
      'get events limit 1',
      'get events select count(*)',
      'get minutes limit 1',
      'get pulse limit 1',
      'set events {path: "/x"}',
      'del events',
      'put minutes {key: "x", at: now(), name: "pageview", n: 1000000}',
      'set minutes {n: n + 1000}',
      'set rollup_state {seq: 0}',
      'drop collection events',
      'create collection z (a int)',
    ]) {
      const s2 = await q(s);
      assert.ok(s2 === 403 || s2 === 404, `ingest: ${s} answered ${s2}`);
    }
    for (const [path, init] of [
      [`/t/${a.name}/_changes?since=0`, {}],
      [`/t/${a.name}/events`, {}],
      [`/t/${a.name}/events`, { method: 'PATCH', body: '{"path":"/x"}' }],
      [`/t/${a.name}/events`, { method: 'DELETE' }],
    ] as [string, RequestInit][]) {
      const r = await node(path, t, init);
      assert.ok([400, 403, 404].includes(r.status), `ingest ${init.method ?? 'GET'} ${path}: ${r.status}`);
    }
    const [mine] = await rows(a.name, 'get events select count(*) as n where path = "/x"');
    assert.equal(mine.n, 0, 'no event was changed');
  });

  test('the dashboard token reads its site and writes nothing; no JWT updates or deletes an event', async () => {
    const t = tokens.get(a.name, 'viewer');
    const q = async (query: string) => node(`/t/${a.name}/query`, t, { method: 'POST', body: JSON.stringify({ query }) });
    const r = await q('get events select user');
    assert.equal(r.status, 200);
    assert.ok(r.body.includes(`secret-${a.name}`) && !r.body.includes(`secret-${b.name}`));
    const before = await rows(a.name, 'get events select count(*) as n, count(distinct name) as names');
    for (const s of ['put events {eid: "v", name: "x", user: "u", at: now()}', 'set events {name: "y"}', 'del events', 'set minutes {n: 0}', 'set pulse {active: 1000}']) {
      const st = (await q(s)).status;
      assert.ok(st === 403 || st === 404, `${s}: ${st}`);
    }
    assert.deepEqual(await rows(a.name, 'get events select count(*) as n, count(distinct name) as names'), before);
    // Append-only: no role updates an event, the feed's included.
    const feed = tokens.get(a.name, 'feed');
    const st = (await node(`/t/${a.name}/query`, feed, { method: 'POST', body: JSON.stringify({ query: 'set events {name: "y"}' }) })).status;
    assert.ok(st === 403 || st === 404, `feed: ${st}`);
    assert.equal((await node(`/t/${a.name}/query`, OPERATOR, { method: 'POST', body: JSON.stringify({ query: 'get events select count(*)' }) })).status, 200);
  });
});

describe('beacons', () => {
  const count = async (s: Site) => Number((await rows(s.name, 'get events select count(*) as n'))[0].n);

  test('malformed, oversized and out-of-range beacons are refused, and nothing of them is written', async () => {
    const before = await count(a);
    const cases: [unknown, number, string][] = [
      ['not json', 400, 'JSON'],
      [[1, 2], 400, 'object'],
      [body(a, []), 400, 'list of events'],
      [{ ...body(a, [ev()]), k: 'NO' }, 400, 'site key'],
      [{ ...body(a, [ev()]), b: 'short' }, 400, 'batch id'],
      [{ ...body(a, [ev()]), u: 'has spaces in it' }, 400, 'visitor id'],
      [{ ...body(a, [ev()]), t: 'now' }, 400, 'time'],
      [body(a, [ev('Page View')]), 400, 'name'],
      [body(a, [ev('x'.repeat(40))]), 400, 'name'],
      [body(a, [ev('pageview', { p: 'no-slash' })]), 400, 'path'],
      [body(a, [ev('pageview', { p: `/${'a'.repeat(300)}` })]), 400, 'path'],
      [body(a, [ev('pageview', { p: '/a\u0000b' })]), 400, 'path'],
      [body(a, [ev('pageview', { t: Date.now() - 2 * 3_600_000 })]), 400, 'older'],
      [body(a, [ev('pageview', { t: Date.now() + 60_000 })]), 400, 'later'],
      [body(a, [ev('pageview', { d: [1, 2] })]), 400, 'object'],
      [body(a, [ev('pageview', { d: { 'bad key': 1 } })]), 400, 'property'],
      [body(a, [ev('pageview', { d: { a: { nested: true } } })]), 400, 'property'],
      [body(a, [ev('pageview', { d: { a: 'x'.repeat(200) } })]), 400, 'property'],
      [body(a, [ev('pageview', { d: Object.fromEntries(Array.from({ length: 9 }, (_, i) => [`k${i}`, i])) })]), 400, '8 properties'],
      [body(a, [ev('pageview', { r: 'x'.repeat(600) })]), 400, 'referrer'],
      [body(a, Array.from({ length: 51 }, () => ev())), 413, '50 events'],
      [{ ...body(a, [ev()]), k: 'unknownkey000' }, 404, 'no site'],
    ];
    for (const [b2, status, says] of cases) {
      const r = await beacon(b2, { raw: typeof b2 === 'string' });
      assert.equal(r.status, status, `${JSON.stringify(b2).slice(0, 80)}: ${r.status} ${r.body}`);
      assert.ok(r.body.includes(says), `${r.body} should say ${says}`);
    }
    // Oversized: refused by its length before it is read, and when it does not say its length.
    const big = JSON.stringify(body(a, [ev('pageview', { p: '/' + 'a'.repeat(200) })])).replace('"e":', `"pad":"${'x'.repeat(MAX_BODY)}","e":`);
    assert.equal((await beacon(big, { raw: true })).status, 413);
    const chunked = await fetch(`${APP}/e`, {
      method: 'POST',
      headers: { origin: ORIGIN, 'user-agent': userAgent('desktop', 'Chrome') },
      body: new ReadableStream({
        start(c) {
          c.enqueue(new TextEncoder().encode(big));
          c.close();
        },
      }),
      duplex: 'half',
    } as RequestInit);
    assert.equal(chunked.status, 413);
    // From a page that is not the site's, or with no origin at all.
    assert.equal((await beacon(body(a, [ev()]), { origin: 'https://evil.example' })).status, 403);
    assert.equal((await beacon(body(a, [ev()]), { origin: '' })).status, 403);
    // A crawler is answered and not counted.
    assert.equal((await beacon(body(a, [ev()]), { ua: 'Mozilla/5.0 (compatible; Googlebot/2.1)' })).status, 204);
    assert.equal(await count(a), before);
  });

  test("a site's rate limit holds under a burst, and holds back no other site", async () => {
    const slow = await newSite('t-rate', 20); // 20 events a second, a burst of 200
    const sent: number[] = [];
    await pool(
      Array.from({ length: 16 }, () => body(slow, Array.from({ length: 20 }, () => ev()))),
      16,
      async (x) => {
        sent.push((await beacon(x)).status);
      },
    );
    const ok = sent.filter((s) => s === 204).length;
    assert.equal(ok, 10, `accepted ${ok} of 16: ${sent}`);
    assert.equal(sent.filter((s) => s === 429).length, 6);
    assert.equal(await count(slow), 200);
    // Another site is not held back.
    assert.equal((await beacon(body(b, [ev()]))).status, 204);
    // And the limit refills: a second later the site takes 20 more.
    await new Promise((r) => setTimeout(r, 1100));
    assert.equal((await beacon(body(slow, Array.from({ length: 20 }, () => ev())))).status, 204);
  });
});

describe("the dashboard's own routes", () => {
  const signIn = async (name: string, password = DEMO_PASSWORD) => {
    const r = await fetch(`${APP}/signin`, { method: 'POST', redirect: 'manual', headers: { 'content-type': 'application/x-www-form-urlencoded', origin: APP }, body: `name=${name}&password=${password}` });
    return { status: r.status, cookie: (r.headers.get('set-cookie') ?? '').split(';')[0], location: r.headers.get('location') };
  };
  const get = (path: string, cookie = '') => fetch(`${APP}${path}`, { redirect: 'manual', headers: { cookie } });

  test("a signed-in person sees their own sites and not another's, on any route", async () => {
    const nadia = await signIn('nadia');
    assert.equal(nadia.status, 303);
    assert.equal((await get('/s/fieldnotes', nadia.cookie)).status, 200);
    for (const p of ['/s/tidepool', '/s/tidepool?range=90d', '/s/tidepool/now.json', `/s/${a.name}`]) {
      const r = await get(p, nadia.cookie);
      assert.equal(r.status, 404, p);
      await r.body?.cancel();
    }
    const page = await (await get('/s/fieldnotes', nadia.cookie)).text();
    assert.ok(!page.includes('Tidepool'), "the other site's name is not on the page");
  });

  test('no session, a forged cookie or a wrong password reaches no dashboard', async () => {
    assert.equal((await get('/s/fieldnotes')).status, 303);
    assert.equal((await get('/s/fieldnotes', `kestrel=${sign('nadia', 'not-the-secret')}`)).status, 303);
    assert.equal((await get('/s/fieldnotes', 'kestrel=nadia')).status, 303);
    assert.equal((await signIn('nadia', 'wrong')).status, 401);
    assert.equal((await signIn('nobody')).status, 401);
    // A sign-in posted from another site is refused.
    const r = await fetch(`${APP}/signin`, { method: 'POST', redirect: 'manual', headers: { 'content-type': 'application/x-www-form-urlencoded', origin: 'https://evil.example' }, body: 'name=nadia&password=' + DEMO_PASSWORD });
    assert.equal(r.status, 403);
  });

  test('pages carry a strict content security policy and the session cookie is HttpOnly', async () => {
    const r = await fetch(`${APP}/signin`, { method: 'POST', redirect: 'manual', headers: { 'content-type': 'application/x-www-form-urlencoded', origin: APP }, body: `name=nadia&password=${DEMO_PASSWORD}` });
    const c = r.headers.get('set-cookie') ?? '';
    assert.match(c, /HttpOnly/);
    assert.match(c, /SameSite=Lax/);
    const page = await get('/s/fieldnotes', c.split(';')[0]);
    const csp = page.headers.get('content-security-policy') ?? '';
    assert.match(csp, /default-src 'none'/);
    assert.match(csp, /script-src 'self' 'sha256-/);
    assert.ok(!csp.includes('unsafe-inline'));
    assert.equal(page.headers.get('x-robots-tag'), 'noindex');
    await page.body?.cancel();
  });
});

