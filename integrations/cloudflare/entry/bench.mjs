// What the global entry adds to a request, under `wrangler dev` on this
// machine: a read by id of a tenant in us, asked of us's router directly,
// and through the entry -- its home heard (the common path), heard from
// the colo's cache alone (an isolate that has not seen the tenant), and
// never heard (every request sent to eu first, whose router relays it to
// us: the miss); us's router asked for the tenant's home as the entry asks
// it (`Fenec-Home`), for what naming it costs a router; and eu's router
// asked directly, which relays it the same way, for what that hop costs
// without the entry. Each entry path is asked
// through wrangler's "Ready on" address and past it, at the Worker's own
// workerd socket (dev.mjs's `inner`). In turns, ROUNDS of N requests each,
// every path one keep-alive connection.
//
// `wrangler dev` is workerd on this machine behind wrangler's own proxy
// Worker, every hop on loopback: what it adds is not what Cloudflare adds,
// where the client reaches the nearest colo and a router is a region away.
//
//   BUILD=release node entry/bench.mjs      (N=2000 ROUNDS=6 to choose)

import http from 'node:http';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { running, serve } from '../dev.mjs';
import * as cluster from './cluster.mjs';

const N = Number(process.env.N ?? 2000);
const ROUNDS = Number(process.env.ROUNDS ?? 6);

const agent = () => new http.Agent({ keepAlive: true, maxSockets: 1 });

/** One request's time, in microseconds; `ask` asks the router for the tenant's home, as the entry does. */
function timed(url, body, ag, ask) {
  return new Promise((done, failed) => {
    const t0 = process.hrtime.bigint();
    const headers = { Authorization: `Bearer ${cluster.DATA}`, 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(body) };
    if (ask) headers['Fenec-Home'] = '1';
    const req = http.request(
      url,
      {
        method: 'POST',
        agent: ag,
        headers,
      },
      (res) => {
        let text = '';
        res.on('data', (d) => (text += d));
        res.on('end', () => {
          if (res.statusCode !== 200) return failed(new Error(`${url}: ${res.statusCode} ${text.slice(0, 2000)}`));
          done(Number(process.hrtime.bigint() - t0) / 1000);
        });
      },
    );
    req.on('error', failed);
    req.end(body);
  });
}

const pct = (xs, p) => {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))];
};

const w = await cluster.world({ tenants: [['acme', 'us']] });
const state = await mkdtemp(join(tmpdir(), 'fenecdb-entry-bench-'));
const workers = [];
try {
  for (let i = 1; i <= 10; i++) {
    const r = await cluster.query(w.us.url, 'acme', 'put notes {title: $1}', [`note ${i}`]);
    if (r.status !== 200) throw new Error(r.text);
  }
  const regions = JSON.stringify({ us: { routers: [w.us.url, w.usStandby.url] }, eu: { routers: [w.eu.url] } });
  const front = async (vars) => {
    const f = await serve('entry', join(state, String(workers.length)), { REGIONS: regions, DEFAULT_REGION: 'eu', ...vars }, { inner: true });
    workers.push(f);
    return f;
  };
  const [heard, cached, never] = [await front({}), await front({ HOMES: '0' }), await front({ HOMES: '0', HOMES_CACHE: 'off' })];
  const paths = {
    'us router, direct': w.us.url,
    '  asking the home (Fenec-Home)': w.us.url,
    'entry, home heard': heard.url,
    '  past wrangler\'s proxy': heard.inner,
    'entry, home in the colo cache': cached.url,
    '  past wrangler\'s proxy ': cached.inner,
    'entry, never heard (eu relays)': never.url,
    '  past wrangler\'s proxy  ': never.inner,
    'eu router, direct (relays)': w.eu.url,
  };
  const body = JSON.stringify({ query: 'get notes where id = 3' });
  const agents = Object.fromEntries(Object.keys(paths).map((k) => [k, agent()]));
  // Warm: each path's connection open, the entry's home heard.
  const asks = (k) => k.includes('Fenec-Home');
  for (const [k, base] of Object.entries(paths)) for (let i = 0; i < 200; i++) await timed(`${base}/t/acme/query`, body, agents[k], asks(k));
  const all = Object.fromEntries(Object.keys(paths).map((k) => [k, []]));
  const medians = Object.fromEntries(Object.keys(paths).map((k) => [k, []]));
  for (let round = 0; round < ROUNDS; round++) {
    // Every other round the other way round, so no path always follows
    // the same one.
    const order = round % 2 ? Object.keys(paths).reverse() : Object.keys(paths);
    for (const k of order) {
      const xs = [];
      for (let i = 0; i < N; i++) {
        try {
          xs.push(await timed(`${paths[k]}/t/acme/query`, body, agents[k], asks(k)));
        } catch (e) {
          throw new Error(`${k}, round ${round}, request ${i}: ${e.message}`);
        }
      }
      all[k].push(...xs);
      medians[k].push(pct(xs, 50));
    }
  }
  const base = pct(all['us router, direct'], 50);
  console.log(`a read by id, ${ROUNDS} rounds of ${N}, us; microseconds`);
  console.log('path'.padEnd(34), 'p50'.padStart(7), 'p99'.padStart(7), '  +p50', '  round p50s');
  for (const k of Object.keys(paths)) {
    const p50 = pct(all[k], 50);
    const p99 = pct(all[k], 99);
    const rounds = medians[k].map((m) => m.toFixed(0)).join(' ');
    console.log(k.padEnd(34), p50.toFixed(0).padStart(7), p99.toFixed(0).padStart(7), `  ${(p50 - base).toFixed(0).padStart(5)}`, `  ${rounds}`);
  }
} finally {
  for (const f of workers) await f.stop();
  await w.stop();
  cluster.killAll();
  for (const c of running) process.kill(-c.pid, 'SIGKILL');
  await rm(state, { recursive: true, force: true });
}
