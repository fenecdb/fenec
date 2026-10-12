// A courier's lease: killed mid-delivery, the courier holds its order until
// the lease lapses and not a moment longer -- a courier waiting at the
// server is handed it then, with nothing written to announce it -- and the
// dead one's late writes are refused. An order given back waits out its
// backoff; one claimed too often is a dead letter.
import { after, before, test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import { startWorld, tandem, cast, type World } from './harness';
import { claimNearest, deliver, giveBack, heartbeat, pickUp, queue, type Job } from '../lib/dispatch';
import { placeOrder } from '../lib/orders';
import { appDb, one } from '../lib/db';
import { addresses } from '../lib/city';
import { MAX_ATTEMPTS } from '../lib/config';

let world: World;
const t = cast(tandem);
const LEASE = 1_500;

before(async () => {
  world = await startWorld();
  queue.leaseMs = LEASE;
});
after(async () => {
  await world?.stop();
});

let n = 0;
async function order(shopIndex: number): Promise<{ id: number; at: [number, number] }> {
  const shop = t.merchants[shopIndex];
  const door = addresses()[shopIndex * 101];
  const { id } = await placeOrder(t.customer(shopIndex), {
    ref: `lease-${n++}`,
    merchant: shop,
    dropoff: door.point,
    dropoffAddr: door.label,
    items: 1,
    fee: 400,
  });
  return { id, at: shop.loc };
}

const job = (id: number) => one<Job>(appDb('tandem'), 'get orders where id = $1', [id]);

test('a courier killed mid-delivery: the order goes to the next courier as its lease lapses', async () => {
  const { id, at } = await order(3);
  const [a, b] = t.couriers;
  const child = spawn(
    process.execPath,
    ['--import', 'tsx', fileURLToPath(new URL('./courier.ts', import.meta.url)), world.router, a.sub, 'tandem', String(at[0]), String(at[1]), String(LEASE), '300'],
    { stdio: ['ignore', 'pipe', 'inherit'] },
  );
  const exited = new Promise((r) => child.on('exit', r));
  let claim = '';
  let lastBeat = 0;
  let beats = 0;
  for await (const line of createInterface({ input: child.stdout! })) {
    const ev = JSON.parse(line);
    if (ev.event === 'claimed') {
      assert.equal(ev.id, id);
      claim = ev.claim;
    }
    if (ev.event === 'beat') {
      assert.equal(ev.kept, true);
      lastBeat = ev.t;
      if (++beats === 3) break;
    }
  }
  // Mid-delivery: picked up, three beats sent, then gone without a word.
  child.kill('SIGKILL');
  await exited;
  const held = await job(id);
  assert.equal(held!.status, 'picked');
  assert.equal(held!.courier, a.sub);

  // B waits at the server; no write will say the lease lapsed.
  const got = await claimNearest(b, at, { radius: 2_000, wait: 10_000 });
  const took = Date.now();
  assert.ok(got, 'the order comes back');
  assert.equal(got.job.id, id);
  assert.equal(got.how, 'held');
  assert.equal(got.job.attempts, 2);
  console.log(`  the order came back ${took - lastBeat} ms after the dead courier's last beat, its lease ${LEASE} ms`);
  assert.ok(took - lastBeat >= LEASE - 50, `not before the lease: ${took - lastBeat} ms after the last beat`);
  assert.ok(took - lastBeat < LEASE + 2_000, `soon after it: ${took - lastBeat} ms after the last beat`);

  // The dead courier's writes, sent late, find the order is not theirs.
  const stale = { id, claim };
  assert.equal(await pickUp(a, stale), false);
  assert.equal(await deliver(a, stale, 1_000), false);
  assert.deepEqual(await heartbeat(a, { at, state: 'dropoff', job: stale }), { kept: false });

  assert.equal(await deliver(b, got.job, 1_200), true);
  const done = await job(id);
  assert.equal(done!.status, 'delivered');
  assert.equal(done!.courier, b.sub);
});

test('an order given back waits out its backoff, then the next claim takes it', async () => {
  const { id, at } = await order(7);
  const c = t.couriers[2];
  const first = await claimNearest(c, at, { radius: 2_000 });
  assert.equal(first?.job.id, id);
  assert.equal(await giveBack(c, first.job, 'the merchant had closed'), true);
  const back = await job(id);
  assert.equal(back!.status, 'ready');
  assert.equal(back!.error, 'the merchant had closed');
  // attempts 1: back in 1 s. A claim now finds nothing near...
  assert.equal(await claimNearest(t.couriers[3], at, { radius: 2_000 }), null);
  // ...and one held at the server is handed it once the second has passed.
  const t0 = Date.now();
  const again = await claimNearest(t.couriers[3], at, { radius: 2_000, wait: 5_000 });
  assert.equal(again?.job.id, id);
  assert.ok(Date.now() - t0 >= 900, `after its backoff: ${Date.now() - t0} ms`);
  assert.equal(again.job.attempts, 2);
  assert.equal(await deliver(t.couriers[3], again.job, 900), true);
});

test(`an order claimed ${MAX_ATTEMPTS} times without a delivery is a dead letter`, async () => {
  const { id, at } = await order(11);
  for (let i = 1; i <= MAX_ATTEMPTS; i++) {
    const got = await claimNearest(t.couriers[10 + i], at, { radius: 2_000, wait: 5_000 });
    assert.equal(got?.job.id, id);
    assert.equal(got.job.attempts, i);
    // Each courier drops it: its lease lapses.
  }
  // The next claim draws it a sixth time and marks it dead rather than run it.
  assert.equal(await claimNearest(t.couriers[20], at, { radius: 2_000, wait: LEASE + 1_000 }), null);
  const dead = await job(id);
  assert.equal(dead!.status, 'dead');
  assert.equal(dead!.run_at, null);
  assert.match(dead!.error ?? '', /claimed 6 times/);
  assert.equal(await claimNearest(t.couriers[21], at, { radius: 2_000 }), null);
});
