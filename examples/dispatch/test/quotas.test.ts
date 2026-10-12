// A company is held to its plan, and its neighbour is not slowed by it.
// Tandem (Pro) and Hopper (Starter) side by side on one node: Hopper's fleet
// sending positions ten times faster than its plan's writes a second is
// refused (429, `writes`) while Tandem's couriers are answered as before; a
// bulk import past its bytes a second waits out its debt; an idle fleet
// larger than the plan's streams cannot all wait at the server for orders;
// and a company at its rows takes no new order (507) but still deletes.
import { after, before, test } from 'node:test';
import assert from 'node:assert/strict';
import { startWorld, tandem, hopper, cast, quantile, sleep, type World } from './harness';
import { heartbeat } from '../lib/dispatch';
import { placeOrder } from '../lib/orders';
import { asUser, FenecError, operatorDb, one } from '../lib/db';
import { setQuota } from '../lib/setup';
import { PLANS } from '../lib/plans';
import { addresses, bounds, rng, type Point } from '../lib/city';
import { SHARD_TOKEN } from '../lib/config';
import type { Who } from '../lib/tokens';

let world: World;
const tn = cast(tandem);
const hp = cast(hopper);

before(async () => {
  // One node for both: what one tenant takes of a node is what the other
  // would feel.
  world = await startWorld({ basePort: 18430, node: 'n1' });
});
after(async () => {
  await world?.stop();
});

const spot = (r: () => number): Point => {
  const [w, s, e, n] = bounds();
  return [w + (e - w) * r(), s + (n - s) * r()];
};

interface Tally {
  ok: number;
  refused: number;
  quotas: Set<string>;
  ms: number[];
}

/** A fleet beating every `every` ms until `stop`; a refused beat waits its Retry-After. */
function fleet(couriers: Who[], every: number, stop: { done: boolean }, tally: Tally) {
  return Promise.all(
    couriers.map(async (who, k) => {
      const r = rng(k + 5);
      while (!stop.done) {
        const t0 = performance.now();
        const got = await heartbeat(who, { at: spot(r), state: 'idle' });
        if (got.throttled) {
          tally.refused++;
          tally.quotas.add('writes');
          await sleep(got.throttled * 1000);
          continue;
        }
        tally.ok++;
        tally.ms.push(performance.now() - t0);
        await sleep(every);
      }
    }),
  );
}

const tally = (): Tally => ({ ok: 0, refused: 0, quotas: new Set(), ms: [] });

test('a starter fleet writing ten times its plan is refused; the pro company beside it is not', async () => {
  const quiet = tally();
  let stop = { done: false };
  let run: Promise<unknown> = fleet(tn.couriers.slice(0, 10), 50, stop, quiet);
  await sleep(2_000);
  stop.done = true;
  await run;
  assert.equal(quiet.refused, 0);

  const steady = tally();
  const flood = tally();
  stop = { done: false };
  run = Promise.all([fleet(tn.couriers.slice(0, 10), 50, stop, steady), fleet(hp.couriers.slice(0, 20), 50, stop, flood)]);
  const t0 = performance.now();
  await sleep(3_000);
  stop.done = true;
  await run;
  const secs = (performance.now() - t0) / 1000;
  const rate = PLANS.starter.quota.writes;
  console.log(
    `  tandem alone: ${quiet.ok} beats, p50 ${quantile(quiet.ms, 0.5).toFixed(2)} ms, p99 ${quantile(quiet.ms, 0.99).toFixed(2)};` +
      ` beside hopper: ${steady.ok} beats, p50 ${quantile(steady.ms, 0.5).toFixed(2)} ms, p99 ${quantile(steady.ms, 0.99).toFixed(2)};` +
      ` hopper: ${flood.ok} landed, ${flood.refused} refused in ${secs.toFixed(1)} s`,
  );
  assert.ok(flood.refused > 0, 'hopper is refused');
  // A bucket a second deep: the plan's rate, and a second's worth at once.
  assert.ok(flood.ok <= rate * (secs + 1), `hopper landed ${flood.ok} in ${secs.toFixed(1)} s against ${rate} a second`);
  assert.equal(steady.refused, 0, 'tandem is never refused');
  assert.ok(
    quantile(steady.ms, 0.5) <= quantile(quiet.ms, 0.5) * 3 + 2,
    `tandem's median beat ${quantile(steady.ms, 0.5).toFixed(2)} ms beside against ${quantile(quiet.ms, 0.5).toFixed(2)} alone`,
  );
});

test('a bulk import past the bytes a second waits out its debt', async () => {
  const dispatcher = asUser(hp.dispatcher);
  const doors = addresses();
  // A day's orders from a merchant's system, 120 a statement: about 50 KB.
  const page = (k: number) =>
    Array.from({ length: 120 }, (_, i) => {
      const shop = hp.merchants[i % hp.merchants.length];
      const door = doors[(k * 120 + i) * 7];
      return {
        ref: `import-${k}-${i}`,
        merchant: shop.code,
        customer: hp.customers[i % hp.customers.length].uid,
        pickup: shop.loc,
        pickup_name: shop.name,
        dropoff: door.point,
        dropoff_addr: door.label,
        items: 2,
        fee: 450,
        status: 'cancelled',
        attempts: 0,
        created: Date.now(),
        promised: Date.now(),
      };
    });
  let refusal: FenecError | null = null;
  let landed = 0;
  for (let k = 0; k < 6 && !refusal; k++) {
    try {
      await dispatcher.run('put orders $1', [page(k)]);
      landed++;
    } catch (e) {
      if (!(e instanceof FenecError)) throw e;
      refusal = e;
    }
  }
  assert.ok(refusal, `refused after ${landed} pages`);
  assert.equal(refusal.status, 429);
  assert.equal(refusal.quota, 'write_bytes');
  assert.ok((refusal.retryAfter ?? 0) >= 1);
  // Waited out, the next page lands.
  await sleep(refusal.retryAfter! * 1000);
  await dispatcher.run('put orders $1', [page(9)]);
});

test("an idle fleet larger than the plan's streams cannot all wait at the server", async () => {
  const streams = PLANS.starter.quota.streams;
  const couriers = hp.couriers.slice(0, 20);
  // 45 claims from 20 couriers' phones -- two or three each, as a phone
  // that lost its answer and asked again would -- where no order is: each
  // waits at the server. Sent 40 ms apart, under the plan's 40 writes a
  // second, since a held claim takes a write of the rate as it comes in
  // before it holds a stream: sent at once, the 41st to 45th were refused
  // for `writes` rather than for `streams`.
  await sleep(1_100);
  const nowhere: Point = [26.951, 38.331];
  const answers = await Promise.all(
    Array.from({ length: streams + 5 }, async (_, i) => {
      await sleep(i * 40);
      const who = couriers[i % couriers.length];
      return asUser(who)
        .batch([['set orders {courier: $1} where run_at <= now() and distance(pickup, $2) <= 1 order run_at limit 1', [who.sub, nowhere]]], { wait: 4_000 })
        .then(() => 'held')
        .catch((e: FenecError) => `${e.status} ${e.quota}`);
    }),
  );
  assert.equal(answers.filter((a) => a === 'held').length, streams);
  assert.deepEqual(answers.slice(streams), Array(5).fill('429 streams'));
});

test('a company at its rows takes no new order, and deletes still land', async () => {
  const usage = await (await fetch(`${world.router}/_shard/tenants/co-hopper/quota`, { headers: { authorization: `Bearer ${SHARD_TOKEN}` } })).json();
  const rows = usage.usage.rows as number;
  await setQuota({ router: world.router, shardToken: SHARD_TOKEN }, 'co-hopper', { ...PLANS.starter.quota, rows: rows + 3 });
  const shop = hp.merchants[0];
  const door = addresses()[5];
  const place = (i: number) =>
    placeOrder(hp.customer(i), { ref: `full-${i}`, merchant: shop, dropoff: door.point, dropoffAddr: door.label, items: 1, fee: 300 });
  for (let i = 0; i < 3; i++) await place(i);
  await assert.rejects(place(3), (e: FenecError) => e.status === 507 && e.quota === 'rows');
  // The dispatcher clears cancelled orders, and the company has room again.
  const cleared = await asUser(hp.dispatcher).run('del orders where status = "cancelled" and ref ~ "import-0-"');
  assert.ok((cleared as { count: number }).count > 0);
  await place(4);
  const kept = await one<{ count: number }>(operatorDb('co-hopper'), 'get orders where ref ~ "full-" count');
  assert.equal(kept!.count, 4);
  await setQuota({ router: world.router, shardToken: SHARD_TOKEN }, 'co-hopper', PLANS.starter.quota);
});
