// `npm run bench`: Plover's numbers, over a cluster of its own (two tenant
// nodes and fenec-shard, --sync 250) and through the router, as the app
// reaches it. A company placed with no quota, so what is measured is the
// database and not the plan:
//
//   1. an order placed -> claimed, with N couriers waiting at the server;
//   2. a claim of the nearest when orders are ready, N couriers at once;
//   3. the ten nearest idle couriers to a pickup, and who is within 2 km,
//      over 60, 1 000 and 10 000 couriers' positions;
//   4. position updates a second: a courier's beat (its position and its
//      lease, one /batch under its token) from K phones at once, and the
//      app writing 100 positions a statement.
import { mkdtempSync, rmSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Cluster } from '../lib/cluster';
import { DATA_DIR, OPERATOR_TOKEN, SHARD_TOKEN } from '../lib/config';
import { appDb, asUser, operatorDb, routeTo, rows } from '../lib/db';
import { claimNearest, deliver, heartbeat, queue } from '../lib/dispatch';
import { placeOrder } from '../lib/orders';
import { setup } from '../lib/setup';
import { addresses, bounds, metresBetween, rng, type Point } from '../lib/city';
import { COMPANIES, merchants, people, type CompanySpec } from '../lib/world';
import type { Who } from '../lib/tokens';

const base = Number(process.env.PLOVER_BENCH_PORT ?? 18470);
const dir = mkdtempSync(join(tmpdir(), 'plover-bench-'));
const cluster = new Cluster({ dir, basePort: base, nodes: 2, operatorToken: OPERATOR_TOKEN, shardToken: SHARD_TOKEN });
await cluster.start();
routeTo(cluster.routerUrl);
// A company of 200 couriers with no quota.
const spec: CompanySpec = { ...COMPANIES[0], slug: 'bench', name: 'Bench Couriers', couriers: 200, customers: 200, plan: 'pro' };
await setup({ router: cluster.routerUrl, shardToken: SHARD_TOKEN, companies: [spec] });
await fetch(`${cluster.routerUrl}/_shard/tenants/co-bench/quota`, {
  method: 'PUT',
  headers: { authorization: `Bearer ${SHARD_TOKEN}` },
  body: JSON.stringify({ bytes: 0, rows: 0, streams: 0, requests: 0, writes: 0, write_bytes: 0 }),
});
const folk = people(spec);
const couriers: Who[] = folk.filter((p) => p.role === 'courier').map((p) => ({ sub: p.uid, role: 'courier', company: 'bench' }));
const customers: Who[] = folk.filter((p) => p.role === 'customer').map((p) => ({ sub: p.uid, role: 'customer', company: 'bench' }));
const shops = merchants(spec);
const doors = addresses();
const [W, S, E, N] = bounds();
const spot = (r: () => number): Point => [W + (E - W) * r(), S + (N - S) * r()];
const q = (xs: number[], p: number) => [...xs].sort((a, b) => a - b)[Math.min(xs.length - 1, Math.floor(p * xs.length))];
const ms = (v: number) => `${v.toFixed(2)} ms`;
const results: Record<string, unknown> = {};
const report = (name: string, o: Record<string, unknown>) => {
  results[name] = o;
  console.log(`${name.padEnd(58)} ${Object.entries(o).map(([k, v]) => `${k} ${v}`).join(', ')}`);
};
let refs = 0;
/** When each order was sent, by its reference: a claim may land before the insert's answer does. */
const sent = new Map<string, number>();
const place = (r: () => number) => {
  const shop = shops[Math.floor(r() * shops.length)];
  const door = doors[Math.floor(r() * doors.length)];
  const ref = `b-${refs++}`;
  sent.set(ref, performance.now());
  return placeOrder(customers[Math.floor(r() * customers.length)], { ref, merchant: shop, dropoff: door.point, dropoffAddr: door.label, items: 1, fee: 500 });
};
queue.leaseMs = 60_000;

// 1. Placed -> claimed, N couriers waiting at the server; and which of
// them it went to, against the one nearest its pickup.
for (const n of [1, 10, 50, 100]) {
  const stop = new AbortController();
  const lat: number[] = [];
  const excess: number[] = [];
  const failed: string[] = [];
  let nearestTook = 0;
  let pending: (() => void) | null = null;
  const where = couriers.slice(0, n).map((_, k) => spot(rng(k + 1)));
  const fleet = couriers.slice(0, n).map(async (who, k) => {
    const at = where[k];
    while (!stop.signal.aborted) {
      const got = await claimNearest(who, at, { radius: 20_000, wait: 20_000, signal: stop.signal }).catch((e) => {
        if (!stop.signal.aborted) failed.push(String(e.code ?? e.message));
        return null;
      });
      if (!got) continue;
      const t = sent.get(got.job.ref);
      if (t !== undefined) lat.push(performance.now() - t);
      const best = Math.min(...where.map((p) => metresBetween(p, got.job.pickup)));
      const mine = metresBetween(at, got.job.pickup);
      excess.push(mine - best);
      if (mine - best < 1) nearestTook++;
      await deliver(who, got.job, 1000).catch((e) => failed.push(String(e.code ?? e.message)));
      pending?.();
    }
  });
  // Every courier holding its claim before the first order.
  await new Promise((r) => setTimeout(r, 1500));
  const r = rng(n * 31);
  for (let i = 0; i < 200; i++) {
    const claimed = new Promise<void>((res) => (pending = res));
    await place(r);
    await claimed;
  }
  stop.abort();
  await Promise.allSettled(fleet);
  report(`order sent -> claimed, ${String(n).padStart(3)} couriers waiting`, {
    p50: ms(q(lat, 0.5)),
    p99: ms(q(lat, 0.99)),
    orders: lat.length,
    'the nearest took it': `${Math.round((nearestTook / excess.length) * 100)}%`,
    'metres further, median': Math.round(q(excess, 0.5)),
    ...(failed.length ? { 'requests failed': failed.join(' ') } : {}),
  });
}

// 2. A claim of the nearest with orders ready: one statement.
for (const n of [1, 16, 50]) {
  const r = rng(n * 7);
  for (let i = 0; i < 600; i++) await place(r);
  const lat: number[] = [];
  await Promise.all(
    couriers.slice(0, n).map(async (who, k) => {
      const rr = rng(1000 + k);
      for (let i = 0; i < Math.ceil(600 / n); i++) {
        const t0 = performance.now();
        const got = await claimNearest(who, spot(rr), { radius: 20_000 });
        lat.push(performance.now() - t0);
        if (got) await deliver(who, got.job, 1000);
      }
    }),
  );
  report(`claim the nearest, ${String(n).padStart(2)} couriers at once`, { p50: ms(q(lat, 0.5)), p99: ms(q(lat, 0.99)), claims: lat.length });
}

// 3. The nearest couriers to a pickup, over 60, 1 000 and 10 000 positions.
const op = operatorDb('co-bench');
let placed = 0;
for (const total of [60, 1_000, 10_000]) {
  const r = rng(total);
  const docs = [];
  for (; placed < total; placed++) {
    docs.push({ courier: `p-${placed}`, loc: spot(r), seen: Date.now() + 3_600_000, state: r() < 0.5 ? 'idle' : 'pickup', heading: 0, vehicle: 'bike' });
  }
  for (let i = 0; i < docs.length; i += 1000) await op.run('put positions $1', [docs.slice(i, i + 1000)]);
  const app = appDb('bench');
  const nearest: number[] = [];
  const radius: number[] = [];
  for (let i = 0; i < 400; i++) {
    const at = spot(r);
    let t0 = performance.now();
    await rows(app, 'get positions select courier, state, vehicle, loc, distance(loc, $1) as m where state = "idle" near loc $1 limit 10', [at]);
    nearest.push(performance.now() - t0);
    t0 = performance.now();
    await rows(app, 'get positions select state, count(*) as n where distance(loc, $1) <= $2 group state', [at, 2000]);
    radius.push(performance.now() - t0);
  }
  report(`10 nearest idle couriers, ${String(total).padStart(6)} positions, HTTP`, { p50: ms(q(nearest, 0.5)), p99: ms(q(nearest, 0.99)) });
  report(`who is within 2 km, by state, ${String(total).padStart(6)} positions`, { p50: ms(q(radius, 0.5)), p99: ms(q(radius, 0.99)) });
}

// 4. Position updates: a courier's beat from K phones at once, for 5 s.
for (const k of [1, 8, 32, 64]) {
  const lat: number[] = [];
  const until = performance.now() + 5_000;
  await Promise.all(
    couriers.slice(0, k).map(async (who, i) => {
      const r = rng(i + 77);
      while (performance.now() < until) {
        const t0 = performance.now();
        await heartbeat(who, { at: spot(r), state: 'idle', heading: 90, vehicle: 'bike' });
        lat.push(performance.now() - t0);
      }
    }),
  );
  report(`position beats, ${String(k).padStart(2)} phones at once`, { 'beats a second': Math.round(lat.length / 5), p50: ms(q(lat, 0.5)), p99: ms(q(lat, 0.99)) });
}
{
  // The app writing a hundred positions a statement, as a gateway that
  // gathers the phones' would -- under its own token, not theirs.
  const r = rng(5);
  const lat: number[] = [];
  const until = performance.now() + 5_000;
  let n = 0;
  while (performance.now() < until) {
    const docs = Array.from({ length: 100 }, (_, i) => ({ courier: `p-${(n + i) % 10_000}`, loc: spot(r), seen: Date.now() + 3_600_000, state: 'idle', heading: 0 }));
    const t0 = performance.now();
    await op.run('put positions $1 if absent else set {loc: new.loc, seen: new.seen, state: new.state, heading: new.heading}', [docs]);
    lat.push(performance.now() - t0);
    n += 100;
  }
  report('positions upserted 100 a statement, one writer', { 'positions a second': Math.round(n / 5), p50: ms(q(lat, 0.5)), p99: ms(q(lat, 0.99)) });
}

// 5. What couriers waiting at the server cost the writes beside them: a
// position beat, one phone in turn, beside 0, 50 and 150 claims held where
// no order will come. Each courier's claim is a group of its own -- its
// token's rule names its `sub` -- and a write wakes every group's head,
// whatever collection it wrote: these beats write `positions`, the claims
// read `orders`.
for (const held of [0, 50, 150]) {
  const stop = new AbortController();
  const nowhere: Point = [W + 0.0005, S + 0.0005];
  const waiting = couriers.slice(0, held).map((who) =>
    asUser(who)
      .batch([['set orders {courier: $1} where run_at <= now() and distance(pickup, $2) <= 1 order run_at limit 1', [who.sub, nowhere]]], { wait: 30_000, signal: stop.signal })
      .catch(() => {}),
  );
  await new Promise((r) => setTimeout(r, 1500));
  const lat: number[] = [];
  const who = couriers[199];
  const r = rng(held + 3);
  for (let i = 0; i < 1500; i++) {
    const t0 = performance.now();
    await heartbeat(who, { at: spot(r), state: 'idle' });
    lat.push(performance.now() - t0);
  }
  stop.abort();
  await Promise.all(waiting);
  report(`a beat beside ${String(held).padStart(3)} claims held`, { p50: ms(q(lat, 0.5)), p99: ms(q(lat, 0.99)) });
}

await cluster.stop();
rmSync(dir, { recursive: true, force: true });
mkdirSync(DATA_DIR, { recursive: true });
writeFileSync(join(DATA_DIR, 'bench.json'), JSON.stringify(results, null, 2));
console.log(`\n${join(DATA_DIR, 'bench.json')}`);
process.exit(0);
