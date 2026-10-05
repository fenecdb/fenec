// The ledger's invariants under concurrency: 16 clients making 20 000
// random operations -- transfers, refused ones among them, retries with the
// same key, refunds of one payment racing each other, holds captured,
// released and lapsing -- in process and over HTTP, and then every check.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { Ledger } from '../src/ledger.ts';
import { freshTenant, localStore } from './helpers.ts';
import { check, mix, prepare, type Tally } from './workload.ts';

const OPS = Number(process.env.LEDGER_TEST_OPS ?? 20_000);

function refusedEveryWay(t: Tally) {
  // The mix asked for each refusal; each was refused, and nothing else
  // went wrong.
  for (const why of ['funds', 'recipient', 'source', 'refund_exceeds'])
    assert.ok((t.refusals.get(why) ?? 0) > 0, `some ${why} refusals: ${[...t.refusals]}`);
  assert.equal(t.refusals.get('invalid') ?? 0, 0, 'no refusal for a reason the mix did not ask for');
  assert.ok(t.replays > 0, 'retries were answered as already made');
  assert.ok(t.holds.size > 0 && t.captured > 0);
}

test(`in process: 16 clients, ${OPS} operations, every invariant`, async () => {
  const store = await localStore();
  const ledger = new Ledger(store, { rateLimit: 1e9 });
  const world = await prepare(ledger, store, { n: 40, frozen: 4 });
  // No Idempotency-Key in process: a retry is told `duplicate` by the
  // movement's @unique ref.
  const t = await mix(ledger, world, { ops: OPS, workers: 16, keyed: false, seed: 11 });
  await check(store, world, t);
  refusedEveryWay(t);
});

test(`over HTTP: 16 clients, ${OPS} operations, every invariant`, async () => {
  const { app, operator } = await freshTenant('inv');
  const ledger = new Ledger(app, { rateLimit: 1e9 });
  const world = await prepare(ledger, operator, { n: 40, frozen: 4 });
  const t = await mix(ledger, world, { ops: OPS, workers: 16, seed: 12 });
  await check(operator, world, t);
  refusedEveryWay(t);
  // Over HTTP a retry with its key is answered with the first answer.
  assert.ok(t.replays > 0);
});
