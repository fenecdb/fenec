// Ingest, measured: events a second from 1, 4 and 16 clients through the
// ingest endpoint and straight into the node, the rollups kept from the
// change stream against the same rollups written with each beacon, and
// the rollup worker's lag behind ingest. A node and a Kestrel server of
// its own (scripts/bench-env.ts).
//
//   BENCH_SECONDS   10    each run's length
//   BENCH_EVENTS    20    events a beacon
import { appPort, log, pct, startApp, startNode, stopAll } from './bench-env.ts';

const { addSite, setupControl } = await import('../src/setup.ts');
const { db } = await import('../src/db.ts');
const { insertText, params } = await import('../src/ingest.ts');
const { fold, block, RollupWorker } = await import('../src/rollup.ts');
const { Traffic, userAgent } = await import('../src/sim.ts');
const { dayOf, DAY } = await import('../src/time.ts');
type SimEvent = import('../src/sim.ts').SimEvent;

const SECONDS = Number(process.env.BENCH_SECONDS ?? 10);
const PER = Number(process.env.BENCH_EVENTS ?? 20);

await startNode();
await startApp();
await setupControl();

/** A pool of beacons from simulated traffic, moved into the last minutes. */
function makeBeacons(n: number, seed: number): SimEvent[][] {
  const t = new Traffic({ daily: 20_000, growth: 0, seed, prefix: `b${seed}` });
  const now = Date.now();
  const d = dayOf(now) - DAY;
  const out: SimEvent[][] = [];
  let cur: SimEvent[] = [];
  for (const e of t.day(d, 0)) {
    cur.push({ ...e, at: now - 30 * 60_000 });
    if (cur.length === PER) {
      out.push(cur);
      cur = [];
      if (out.length >= n) break;
    }
  }
  return out;
}
const pool = makeBeacons(4000, 1);
log(`${pool.length} beacons of ${PER} events ready`);

let sites = 0;
async function site(rate = 10_000_000) {
  const s = { name: `bench-${++sites}`, key: `benchkey${String(sites).padStart(4, '0')}`, label: 'Bench', origins: ['https://bench.example'], rate };
  await addSite(s);
  return s;
}

interface Run {
  events: number;
  seconds: number;
  lat: number[];
  acks: { seq: number; at: number }[];
}

/** `clients` loops sending beacons for SECONDS through `send`; events a second and each beacon's latency. */
async function run(clients: number, send: (beacon: SimEvent[], batch: string) => Promise<number | null>): Promise<Run> {
  const end = performance.now() + SECONDS * 1000;
  let events = 0;
  let n = 0;
  const lat: number[] = [];
  const acks: { seq: number; at: number }[] = [];
  const t0 = performance.now();
  await Promise.all(
    Array.from({ length: clients }, async (_, c) => {
      while (performance.now() < end) {
        const b = pool[n++ % pool.length];
        const batch = `c${c}x${n.toString(36).padStart(6, '0')}${Math.random().toString(36).slice(2, 8)}`;
        const s = performance.now();
        const seq = await send(
          b.map((e, i) => ({ ...e, eid: `${batch}:${i}` })),
          batch,
        );
        lat.push(performance.now() - s);
        if (seq) acks.push({ seq, at: performance.now() });
        events += b.length;
      }
    }),
  );
  return { events, seconds: (performance.now() - t0) / 1000, lat, acks };
}

const fmt = (r: Run) => `${Math.round(r.events / r.seconds).toLocaleString('en-US')} events/s, a beacon p50 ${pct(r.lat, 50).toFixed(2)} ms, p99 ${pct(r.lat, 99).toFixed(2)} ms`;

const results: Record<string, string> = {};

// 1. Through the ingest endpoint: parse, check, limit, one block a beacon with its key.
for (const clients of [1, 4, 16]) {
  const s = await site();
  const r = await run(clients, async (b, batch) => {
    const res = await fetch(`http://127.0.0.1:${appPort}/e`, {
      method: 'POST',
      headers: { origin: 'https://bench.example', 'user-agent': userAgent('desktop', 'Chrome'), 'content-type': 'text/plain' },
      body: JSON.stringify({ k: s.key, b: batch, u: b[0].user.padEnd(8, '0'), t: Date.now(), e: b.map((e) => ({ n: e.name, p: e.path, r: e.ref ? `https://${e.ref}/` : '', t: Date.now() })) }),
    });
    await res.body?.cancel();
    if (res.status !== 204) throw new Error(`ingest ${res.status}`);
    return null;
  });
  results[`endpoint, ${clients} client${clients > 1 ? 's' : ''}`] = fmt(r);
  log(`endpoint ${clients}:`, fmt(r));
}

// 2. Straight into the node, as the endpoint writes: one keyed block a beacon.
const direct = async (siteName: string, b: SimEvent[], batch: string, extra: (b: SimEvent[]) => [string, unknown[]][] = () => []) => {
  const ctx = { device: b[0].device, browser: b[0].browser, country: b[0].country, hosts: [] };
  const beacon = { key: '', batch, user: b[0].user.padEnd(8, '0'), events: b.map((e) => ({ name: e.name, path: e.path, ref: e.ref, age: 0, props: e.props })) };
  const r = await db(siteName, 'ingest').batch([[insertText(b.length), params(beacon, ctx)], ...extra(b)], { idempotencyKey: `${siteName}:${batch}` });
  return r.seq;
};
for (const clients of [1, 4, 16]) {
  const s = await site();
  const r = await run(clients, (b, batch) => direct(s.name, b, batch));
  results[`node, ${clients} client${clients > 1 ? 's' : ''}`] = fmt(r);
  log(`node ${clients}:`, fmt(r));
}

// 3. The other design: each beacon's rollups written in its own block, as a
//    worker would write a page's, under the operator's token.
for (const clients of [1, 16]) {
  const s = await site();
  const op = db(s.name, 'operator');
  const r = await run(clients, async (b, batch) => {
    const ctx = { device: b[0].device, browser: b[0].browser, country: b[0].country, hosts: [] };
    const beacon = { key: '', batch, user: b[0].user.padEnd(8, '0'), events: b.map((e) => ({ name: e.name, path: e.path, ref: e.ref, age: 0, props: e.props })) };
    const f = fold(b.map((e) => ({ ...e, user: beacon.user, at: Date.now() })));
    // The rollup statements without the worker's guard (the first): each beacon applies its own.
    const res = await op.batch([[insertText(b.length), params(beacon, ctx)], ...block(f, 0, 0, new Map()).slice(1)], { idempotencyKey: `${s.name}:${batch}` });
    return res.seq;
  });
  results[`rollups on ingest, ${clients} client${clients > 1 ? 's' : ''}`] = fmt(r);
  log(`rollups on ingest ${clients}:`, fmt(r));
}

// 4. The worker's lag behind 16 clients writing through the node: from a
//    beacon's answer to the block that folded it landing.
{
  const s = await site();
  const applied: { seq: number; at: number }[] = [];
  const w = new RollupWorker({ site: s.name, wait: 200, onApplied: (seq) => applied.push({ seq, at: performance.now() }) });
  const running = w.run();
  await new Promise((r) => setTimeout(r, 500));
  const r = await run(16, (b, batch) => direct(s.name, b, batch));
  // Let the worker finish what was sent.
  const last = Math.max(...r.acks.map((a) => a.seq));
  const t = performance.now();
  while (w.seq < last && performance.now() - t < 120_000) await new Promise((x) => setTimeout(x, 50));
  w.stop();
  await running;
  const lags: number[] = [];
  let j = 0;
  const acks = [...r.acks].sort((a, b) => a.seq - b.seq);
  for (const a of acks) {
    while (j < applied.length && applied[j].seq < a.seq) j++;
    if (j < applied.length) lags.push(applied[j].at - a.at);
  }
  const line = `${Math.round(r.events / r.seconds).toLocaleString('en-US')} events/s in; lag p50 ${pct(lags, 50).toFixed(0)} ms, p99 ${pct(lags, 99).toFixed(0)} ms, the most ${Math.max(...lags).toFixed(0)} ms; ${w.applied.toLocaleString('en-US')} events folded in ${applied.length} blocks`;
  results['rollup lag, 16 clients'] = line;
  log('rollup lag:', line);
}

// 5. The worker catching up on a backlog it did not see arrive.
{
  const s = await site();
  await run(16, (b, batch) => direct(s.name, b, batch));
  const [c] = await db(s.name, 'operator').rows('get events select count(*) as n');
  const w = new RollupWorker({ site: s.name, wait: 0 });
  const t = performance.now();
  await w.catchUp(300);
  const secs = (performance.now() - t) / 1000 - 0.3;
  const line = `${Number(c.n).toLocaleString('en-US')} events folded in ${secs.toFixed(1)} s: ${Math.round(w.applied / secs).toLocaleString('en-US')} events/s`;
  results['worker catching up'] = line;
  log('catch-up:', line);
}

console.log('\n| run | result |\n| --- | --- |');
for (const [k, v] of Object.entries(results)) console.log(`| ${k} | ${v} |`);
stopAll();
process.exit(0);
