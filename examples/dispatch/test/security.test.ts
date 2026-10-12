// What each token reaches, asked of the nodes through the router: a
// company's people never another company's tenant nor the platform's; a
// courier their own orders and position; a customer their own orders;
// writes held to the rows and fields their role is granted, before and
// after (WITH CHECK), and an order's to the steps its transitions name.
// Each refused write is checked with the operator's token to have written
// nothing.
import { after, before, test } from 'node:test';
import assert from 'node:assert/strict';
import { startWorld, tandem, hopper, cast, sleep, type World } from './harness';
import { claimNearest, readyNear } from '../lib/dispatch';
import { cancel, placeOrder } from '../lib/orders';
import { appDb, asUser, FenecError, one, operatorDb, rows } from '../lib/db';
import { mint, tokenFor, type Who } from '../lib/tokens';
import { addresses } from '../lib/city';
import { nodeFetch } from '../lib/fetch';
import { connect } from '@fenecdb/web/client';
import type { Job } from '../lib/dispatch';

let world: World;
const tn = cast(tandem);
const hp = cast(hopper);
let mine: Job; // claimed by courier 0
let theirs: Job; // claimed by courier 1
let ready: number; // nobody's yet

before(async () => {
  world = await startWorld();
  const doors = addresses();
  for (let i = 0; i < 6; i++) {
    const shop = tn.merchants[i];
    await placeOrder(tn.customer(i % 2), { ref: `sec-${i}`, merchant: shop, dropoff: doors[i * 50].point, dropoffAddr: doors[i * 50].label, items: 1, fee: 500 });
  }
  mine = (await claimNearest(tn.couriers[0], tn.merchants[0].loc, { radius: 50 }))!.job;
  theirs = (await claimNearest(tn.couriers[1], tn.merchants[1].loc, { radius: 50 }))!.job;
  ready = (await one<{ id: number }>(appDb('tandem'), 'get orders select id where status = "ready" order id limit 1'))!.id;
});
after(async () => {
  await world?.stop();
});

/** A statement's status: 200, or the refusal's. */
async function status(token: string, tenant: string, query: string, params: unknown[] = []): Promise<number> {
  const res = await nodeFetch(`${world.router}/t/${tenant}/query`, {
    method: 'POST',
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: JSON.stringify({ query, params }),
  });
  await res.text();
  return res.status;
}

const refusal = async (p: Promise<unknown>): Promise<number> => {
  try {
    await p;
    return 200;
  } catch (e) {
    if (e instanceof FenecError && e.status !== undefined) return e.status;
    throw e;
  }
};

const order = (id: number) => one<Job>(operatorDb('co-tandem'), 'get orders where id = $1', [id]);

test('a token reaches its own company alone', async () => {
  const d = tokenFor(tn.dispatcher);
  assert.equal(await status(d, 'co-tandem', 'get orders count'), 200);
  assert.equal(await status(d, 'co-hopper', 'get orders count'), 403, "another company's tenant");
  assert.equal(await status(d, 'plover', 'get users count'), 403, "the platform's");
  const c = tokenFor(tn.couriers[0]);
  assert.equal(await status(c, 'co-hopper', 'get orders count'), 403);
  const k = tokenFor(tn.customer(0));
  assert.equal(await status(k, 'co-hopper', 'get merchants count'), 403);
  // Hopper's dispatcher reads Hopper's orders, none of Tandem's.
  assert.equal(await status(tokenFor(hp.dispatcher), 'co-tandem', 'get orders count'), 403);
  // No tenant claim, a lapsed token, a token signed with another secret.
  assert.equal(await status(mint({ sub: 'd-tandem-1', role: 'dispatcher' }, 60), 'co-tandem', 'get orders count'), 403);
  assert.equal(await status(mint({ sub: 'd-tandem-1', role: 'dispatcher', tenant: 'co-tandem' }, -10), 'co-tandem', 'get orders count'), 401);
  const forged = mint({ sub: 'd-tandem-1', role: 'dispatcher', tenant: 'co-tandem' }, 60, 'another secret of at least thirty-two bytes!!');
  assert.equal(await status(forged, 'co-tandem', 'get orders count'), 401);
});

test('a courier reads their own orders, position and row, and nobody else\'s', async () => {
  const c0 = tn.couriers[0];
  const db = asUser(c0);
  assert.deepEqual((await rows<Job>(db, 'get orders where courier = $1', [c0.sub])).map((o) => o.id), [mine.id]);
  // Every other order they read is one they may claim, shown by its pickup
  // alone (the next test); one under another courier's lease not at all.
  const seen = await rows<Job>(db, 'get orders');
  assert.ok(seen.some((o) => o.id === mine.id && o.dropoff_addr === mine.dropoff_addr));
  for (const o of seen.filter((o) => o.id !== mine.id)) {
    assert.equal(o.status, 'ready');
    assert.equal(o.dropoff_addr, null, `the door of order ${o.id}`);
  }
  assert.equal((await rows(db, 'get orders where id = $1', [theirs.id])).length, 0);
  const crew = await rows<{ user: string }>(db, 'get couriers');
  assert.deepEqual(crew.map((r) => r.user), [c0.sub]);
  assert.equal(await refusal(db.run('get positions where courier = $1', [tn.couriers[1].sub])), 200);
  assert.equal((await rows(db, 'get positions where courier = $1', [tn.couriers[1].sub])).length, 0);
});

test("a courier reads a ready order's pickup and not its door until they claim it", async () => {
  const c = tn.couriers[5];
  const db = asUser(c);
  const real = (await order(ready))!;
  const seen = (await one<Job>(db, 'get orders where id = $1', [ready]))!;
  assert.equal(seen.pickup_name, real.pickup_name);
  assert.deepEqual(seen.pickup, real.pickup);
  assert.equal(seen.fee, real.fee);
  for (const hidden of ['customer', 'dropoff', 'dropoff_addr', 'ref', 'claim'] as const) {
    assert.equal(seen[hidden], null, hidden);
  }
  // Asked for by name the door is null; asked of, it matches no ready order
  // -- and its negation every one -- whatever it is, so no door is told
  // from another.
  assert.deepEqual(await rows(db, 'get orders select dropoff_addr, customer where id = $1', [ready]), [{ dropoff_addr: null, customer: null }]);
  for (const door of [real.dropoff_addr, 'nowhere']) {
    assert.equal((await rows(db, 'get orders where id = $1 and dropoff_addr = $2', [ready, door])).length, 0, door);
    assert.equal((await rows(db, 'get orders where id = $1 and not (dropoff_addr = $2)', [ready, door])).length, 1, door);
    assert.equal((await rows(db, 'get orders where id = $1 and customer = $2', [ready, real.customer])).length, 0);
  }
  // Ranked, grouped or counted by it, the doors would come out in order:
  // refused, the field named.
  for (const [q, params] of [
    ['get orders order dropoff_addr', []],
    ['get orders where run_at <= now() order distance(dropoff, $1) limit 1', [real.pickup]],
    ['get orders select customer, count() as n group customer', []],
    ['get orders facet dropoff_addr', []],
  ] as const) {
    const why = await db.run(q, [...params]).then(
      () => 'answered',
      (e: FenecError) => `${e.status} ${e.message}`,
    );
    assert.match(why, /^403 .*this token sees `(dropoff_addr|dropoff|customer)`/, q);
  }
  // The offers near a point carry what a courier chooses by.
  const near = await readyNear(c, real.pickup, 200);
  const offer = near.find((o) => o.id === ready);
  assert.ok(offer && offer.pickup_name === real.pickup_name && offer.fee === real.fee);
  // Claimed, an order is theirs and shows whole: an order placed for it at
  // a shop no other test's order waits at.
  const shop = tn.merchants[11];
  const placed = await placeOrder(tn.customer(2), { ref: 'sec-door', merchant: shop, dropoff: shop.loc, dropoffAddr: 'Door 7', items: 1, fee: 300 });
  assert.equal((await one<Job>(db, 'get orders where id = $1', [placed.id]))!.dropoff_addr, null);
  const claimed = (await claimNearest(c, shop.loc, { radius: 1 }))!.job;
  assert.equal(claimed.id, placed.id);
  assert.equal(claimed.dropoff_addr, 'Door 7', 'returning * answers the claimed order whole');
  assert.equal((await one<Job>(db, 'get orders where id = $1', [placed.id]))!.dropoff_addr, 'Door 7');
});

test("a courier's writes: their own order's fields, claims that name themselves", async () => {
  const c0 = tn.couriers[0];
  const db = asUser(c0);
  // Another courier's order, under its lease: the rule's filter leaves it out.
  const r = (await db.run('set orders {status: "delivered"} where id = $1', [theirs.id])) as { count: number };
  assert.equal(r.count, 0);
  assert.equal((await order(theirs.id))!.status, 'assigned');
  // A field no rule grants: the fee.
  assert.equal(await refusal(db.run('set orders {fee: 1} where id = $1', [mine.id])), 403);
  assert.equal((await order(mine.id))!.fee, 500);
  // A claim that names another courier: the row after the write fails the rule.
  assert.equal(await refusal(db.run('set orders {courier: $1, status: "assigned", run_at: now() + 60000, attempts: attempts + 1} where id = $2', [tn.couriers[2].sub, ready])), 403);
  // Nor one that leaves the attempts uncounted, which a dead letter counts.
  assert.equal(await refusal(db.run('set orders {courier: $1, status: "assigned", run_at: now() + 60000} where id = $2', [c0.sub, ready])), 403);
  // A ready order delivered without a claim, the courier written in or not:
  // no step of an order starts from ready but the claim.
  for (const [q, params] of [
    ['set orders {status: "delivered", courier: $1, delivered_at: now(), run_at: null} where id = $2', [c0.sub, ready]],
    ['set orders {status: "delivered", delivered_at: now(), run_at: null} where id = $1', [ready]],
  ] as const) {
    assert.equal(await refusal(db.run(q, [...params])), 403, q);
  }
  // Their own order given back ahead of the queue: a give-back waits out a backoff.
  assert.equal(await refusal(db.run('set orders {status: "ready", run_at: 0, claim: null} where id = $1', [mine.id])), 403);
  const left = (await order(ready))!;
  assert.equal(left.courier, null);
  assert.equal(left.status, 'ready');
  assert.equal((await order(mine.id))!.status, 'assigned');
  // Somebody else's position.
  assert.equal(
    await refusal(db.run('put positions {courier: $1, loc: [27.0, 38.36], seen: now(), state: "idle"}', [tn.couriers[3].sub])),
    403,
  );
  // No new orders, no deletes.
  assert.equal(await refusal(db.run('del orders where id = $1', [mine.id])), 403);
});

test("a customer reads and places their own orders, and cancels one nobody carries", async () => {
  const k0 = tn.customer(0);
  const k1 = tn.customer(1);
  const db = asUser(k0);
  const seen = await rows<Job>(db, 'get orders');
  assert.ok(seen.length > 0);
  assert.ok(seen.every((o) => o.customer === k0.sub));
  // A collection no rule names is not there for the token: 404, not 403.
  assert.equal(await refusal(db.run('get positions count')), 404, "couriers' positions");
  const shop = tn.merchants[9];
  const base = { ref: 'x', merchant: shop.code, pickup: shop.loc, pickup_name: shop.name, dropoff: shop.loc, dropoff_addr: 'x', items: 1, fee: 1, attempts: 0, created: Date.now(), promised: Date.now() };
  const insert = (doc: object) => db.run('insert orders $1', [doc]);
  assert.equal(await refusal(insert({ ...base, ref: 'as-k1', customer: k1.sub, status: 'ready', run_at: Date.now() })), 403, 'as another customer');
  assert.equal(await refusal(insert({ ...base, ref: 'pre-assigned', customer: k0.sub, status: 'assigned', courier: tn.couriers[0].sub })), 403, 'already assigned');
  assert.equal(await refusal(insert({ ...base, ref: 'retried', customer: k0.sub, status: 'ready', attempts: 4 })), 403, 'with attempts');
  assert.equal((await rows(operatorDb('co-tandem'), 'get orders where ref in ["as-k1", "pre-assigned", "retried"]')).length, 0);
  // Another customer's order is out of reach; their own ready one cancels;
  // one a courier carries does not.
  const k1s = (await rows<Job>(asUser(k1), 'get orders'))[0];
  assert.equal(await cancel(k0, k1s.id), false);
  assert.equal((await order(k1s.id))!.status === 'cancelled', false);
  const own = await one<Job>(db, 'get orders where status = "ready" limit 1');
  assert.ok(own);
  // Moved to the head of the queue, or cancelled and left claimable: not
  // the change their rule names.
  assert.equal(await refusal(db.run('set orders {run_at: 0} where id = $1', [own.id])), 403);
  assert.equal(await refusal(db.run('set orders {status: "cancelled", run_at: 0} where id = $1', [own.id])), 403);
  assert.equal(await cancel(k0, own.id), true);
  // Taken back once cancelled: their rule reaches ready orders alone.
  assert.equal(await refusal(db.run('set orders {status: "ready", run_at: now()} where id = $1 require 1', [own.id])), 412);
  assert.equal((await order(own.id))!.status, 'cancelled');
  assert.equal(mine.customer === k0.sub || mine.customer === k1.sub, true);
  const carried = mine.customer === k0.sub ? k0 : k1;
  assert.equal(await cancel(carried, mine.id), false);
  assert.equal((await order(mine.id))!.status, 'assigned');
});

test("a customer follows their own order live, and hears nothing of another's", async () => {
  const k = (await order(mine.id))!.customer;
  const who: Who = { sub: k, role: 'customer', company: 'tandem' };
  const other = tn.customer(k === tn.customer(0).sub ? 1 : 0);
  const heard: { id: number; courier_loc: unknown }[] = [];
  const conn = connect(`${world.router}/t/co-tandem`, { token: tokenFor(who), fetch: nodeFetch });
  let seeded!: () => void;
  const ready = new Promise<void>((r) => (seeded = r));
  const stop = conn.subscribe('orders', {}, (ev: { type: string; rows?: unknown[]; puts?: unknown[] }) => {
    if (ev.type === 'seed') seeded();
    if (ev.type === 'change') heard.push(...(ev.puts as { id: number; courier_loc: unknown }[]));
  });
  await ready;
  // The courier moves on with the order: its point lands on the order row.
  const c0 = tn.couriers[0];
  await asUser(c0).run('set orders {courier_loc: $1} where id = $2 and claim = $3', [[26.99, 38.35], mine.id, mine.claim]);
  // Another customer's order is assigned meanwhile: no word of it.
  const theirsReady = await one<Job>(asUser(other), 'get orders where status = "ready" limit 1');
  if (theirsReady) await asUser(tn.couriers[4]).run('set orders {courier: $1, status: "assigned", claim: "x", run_at: now() + 60000, attempts: attempts + 1} where id = $2', [tn.couriers[4].sub, theirsReady.id]);
  for (let i = 0; i < 100 && !heard.some((r) => r.id === mine.id); i++) await sleep(20);
  stop();
  assert.ok(heard.some((r) => r.id === mine.id && JSON.stringify(r.courier_loc) === '[26.99,38.35]'), 'heard their courier move');
  assert.ok(heard.every((r) => r.id === mine.id || (r as unknown as Job).customer === k), 'heard nothing of others');
});
