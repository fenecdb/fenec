// No order is ever carried by two couriers: 50 courier clients claiming the
// nearest ready order at once, each claim a /batch under the courier's own
// token, every order acked by delivering it -- and the same with 50
// couriers waiting at the server (`Fenec-Wait`) while orders trickle in.
import { after, before, test } from 'node:test';
import assert from 'node:assert/strict';
import { startWorld, tandem, cast, sleep, type World } from './harness';
import { claimNearest, claimStats, deliver, type Job } from '../lib/dispatch';
import { placeOrder } from '../lib/orders';
import { appDb, rows, throttled } from '../lib/db';
import { addresses, bounds, type Point } from '../lib/city';
import { rng } from '../lib/city';
import type { Who } from '../lib/tokens';

let world: World;
const t = cast(tandem);

before(async () => {
  world = await startWorld({ basePort: 18410 });
});
after(async () => {
  await world?.stop();
});

/** Orders from the merchants to customers' doors, `n` of them, refs tagged. */
async function place(n: number, tag: string, gap = 0): Promise<number[]> {
  const ids: number[] = [];
  const doors = addresses();
  const r = rng(n * 7 + tag.length);
  const one = async (i: number) => {
    const shop = t.merchants[i % t.merchants.length];
    const door = doors[Math.floor(r() * doors.length)];
    const placed = await placeOrder(t.customer(i % 100), {
      ref: `${tag}-${i}`,
      merchant: shop,
      dropoff: door.point,
      dropoffAddr: door.label,
      items: 1,
      fee: 500,
    });
    ids.push(placed.id);
  };
  if (gap) {
    for (let i = 0; i < n; i++) {
      await one(i);
      await sleep(gap);
    }
  } else {
    for (let i = 0; i < n; i += 16) await Promise.all(Array.from({ length: Math.min(16, n - i) }, (_, k) => one(i + k)));
  }
  return ids;
}

const somewhere = (r: () => number): Point => {
  const [w, s, e, n] = bounds();
  return [w + (e - w) * r(), s + (n - s) * r()];
};

test('50 couriers claim at once: every order taken once and delivered once', async () => {
  const ids = await place(400, 'burst');
  const taken = new Map<number, string[]>();
  const acked: number[] = [];
  const couriers = t.couriers.slice(0, 50);
  await Promise.all(
    couriers.map(async (who: Who, k) => {
      const r = rng(1000 + k);
      for (let misses = 0; misses < 3; ) {
        const got = await claimNearest(who, somewhere(r), { radius: 20_000 });
        if (!got) {
          misses++;
          continue;
        }
        misses = 0;
        taken.set(got.job.id, [...(taken.get(got.job.id) ?? []), who.sub]);
        assert.equal(got.job.courier, who.sub);
        assert.ok(await deliver(who, got.job, 1000), `${who.sub} delivers ${got.job.id}`);
        acked.push(got.job.id);
      }
    }),
  );
  console.log(`  claims: ${JSON.stringify(claimStats)}, 429s: ${JSON.stringify(throttled)}`);
  const twice = [...taken].filter(([, by]) => by.length > 1);
  assert.deepEqual(twice, [], 'no order claimed twice');
  assert.equal(acked.length, ids.length);
  assert.deepEqual([...taken.keys()].sort((a, b) => a - b), [...ids].sort((a, b) => a - b));
  // The database's own account: each order delivered, by the courier who
  // claimed it, claimed once.
  const db = await rows<Job>(appDb('tandem'), 'get orders select id, status, courier, attempts where ref ~ "burst-"');
  assert.equal(db.length, 400);
  for (const o of db) {
    assert.equal(o.status, 'delivered');
    assert.equal(o.attempts, 1, `order ${o.id} claimed once`);
    assert.deepEqual(taken.get(o.id), [o.courier]);
  }
});

test('50 couriers waiting at the server: each order placed goes to exactly one', async () => {
  const taken = new Map<number, string[]>();
  let placing = true;
  const couriers = t.couriers.slice(0, 50);
  const total = 150;
  let delivered = 0;
  const fleet = Promise.all(
    couriers.map(async (who: Who, k) => {
      const r = rng(2000 + k);
      const at = somewhere(r);
      while (placing || delivered < total) {
        const got = await claimNearest(who, at, { radius: 20_000, wait: 2_000 });
        if (!got) {
          if (!placing && delivered >= total) return;
          continue;
        }
        taken.set(got.job.id, [...(taken.get(got.job.id) ?? []), who.sub]);
        assert.ok(await deliver(who, got.job, 800));
        delivered++;
      }
    }),
  );
  // Let every courier reach the server and wait there before the first order.
  await sleep(500);
  const ids = await place(total, 'trickle', 5);
  placing = false;
  await fleet;
  assert.deepEqual([...taken].filter(([, by]) => by.length > 1), [], 'no order claimed twice');
  assert.deepEqual([...taken.keys()].sort((a, b) => a - b), [...ids].sort((a, b) => a - b));
  console.log(`  claims: ${JSON.stringify(claimStats)}, 429s: ${JSON.stringify(throttled)}`);
  const [once] = await rows<{ count: number }>(appDb('tandem'), 'get orders where ref ~ "trickle-" and attempts = 1 and status = "delivered" count');
  assert.equal(once.count, total);
});
