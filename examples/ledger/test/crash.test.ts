// A server killed with SIGKILL in the middle of the load, under
// `--sync always`, and started again over the same files: every movement it
// acknowledged is there, every invariant holds, an acknowledged key is
// answered as before, and a transfer whose answer never came is made at
// most once when sent again with its key.
import assert from 'node:assert/strict';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { OPERATOR_TOKEN } from '../src/config.ts';
import { Ledger } from '../src/ledger.ts';
import { createTenant } from '../src/setup.ts';
import { HttpStore } from '../src/store.ts';
import { Tokens } from '../src/tokens.ts';
import { startNode } from './server.ts';
import { check, mix, prepare } from './workload.ts';

const KILL_AT = Number(process.env.LEDGER_CRASH_AFTER ?? 1500);

test(`kill -9 after ${KILL_AT} acknowledged movements under --sync always, then restart`, async () => {
  const dir = join(mkdtempSync(join(tmpdir(), 'ledger-crash-')), 'tenants');
  let node = await startNode({ dir, sync: 'always' });
  const port = new URL(node.url).port;
  await createTenant('crash', node.url);
  const tokens = new Tokens('crash');
  const base = `${node.url}/t/crash`;
  const ledger = new Ledger(new HttpStore(base, tokens.app()), { rateLimit: 1e9 });
  const operator = new HttpStore(base, OPERATOR_TOKEN);
  const world = await prepare(ledger, operator, { n: 40, frozen: 4 });

  let acked = 0;
  let killed: Promise<void> | null = null;
  const t = await mix(ledger, world, {
    ops: 1e9,
    workers: 16,
    seed: 31,
    onMade: () => {
      if (++acked === KILL_AT) killed = node.kill9();
    },
    stop: () => killed !== null,
    onError: () => killed !== null,
  });
  await killed;
  assert.ok(acked >= KILL_AT);
  const unknown = t.pending.size;

  node = await startNode({ dir, sync: 'always', port: Number(port) });
  try {
    // Every acknowledged movement survived, with both its entries.
    const refs = new Set((await operator.rows<{ ref: string }>('get transfers select ref')).map((r) => r.ref));
    const lost = [...t.made.keys()].filter((ref) => !ref.startsWith('hd-') && !refs.has(ref));
    assert.deepEqual(lost, [], 'no acknowledged movement lost');

    // An acknowledged key is answered as before: its key and answer landed
    // in the same block as the transfer.
    let replayed = 0;
    for (const [key, input] of [...t.inputs].filter(([, i]) => t.made.has(i.ref)).slice(0, 200)) {
      const r = await ledger.transfer(input, key);
      assert.ok(r.ok && r.replayed, `${key} replayed after the restart`);
      replayed++;
    }
    assert.ok(replayed > 0);

    // The transfers in flight when it died: sent again with their keys,
    // each lands once at most -- it had landed (replayed), or it lands now.
    for (const [key, input] of t.pending) {
      const before = refs.has(input.ref);
      const r = await ledger.transfer(input, key);
      if (before) assert.ok(r.ok && r.replayed, `${key} was in the ledger: replayed`);
      else if (r.ok) t.made.set(input.ref, { kind: 'transfer', amount: input.amount });
    }

    // Holds the crash left held lapse, and the reaper gives them back once.
    await new Promise((r) => setTimeout(r, 500));
    await ledger.reap();
    await check(operator, world, t, { complete: false, holds: false });
    console.log(`# killed after ${acked} acknowledged, ${unknown} transfers in flight; ${replayed} keys replayed after the restart`);
  } finally {
    await node.stop();
  }
});
