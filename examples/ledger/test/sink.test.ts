// The change stream into a sink outside the database: every journal entry
// reaches the file at least once, a crash before the commit hands some over
// again, and the deduplication key leaves the file holding the journal,
// entry for entry.
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { OPERATOR_TOKEN } from '../src/config.ts';
import { readSink, Sink } from '../src/sink.ts';
import type { HttpStore } from '../src/store.ts';
import { freshTenant } from './helpers.ts';
import { mix, percentile, prepare } from './workload.ts';

/** Steps the sink until it holds the change the database stands at now. */
async function drain(sink: Sink, operator: HttpStore) {
  const { seq } = await operator.snapshot(['get journal limit 0']);
  while (sink.cursor < (seq ?? 0)) await sink.step();
}

async function journal(operator: HttpStore) {
  return operator.rows<{ entry: string; tx: string; account: string; amount: number }>('get journal select entry, tx, account, amount');
}

test('every entry reaches the sink once, through a crash between its sync and its commit', async () => {
  const { operator, ledger } = await freshTenant('sink');
  const l = ledger();
  const world = await prepare(l, operator, { n: 20, frozen: 2 });
  const file = join(mkdtempSync(join(tmpdir(), 'ledger-sink-')), 'journal.ndjson');
  const opts = { base: operator.base, token: OPERATOR_TOKEN, file, wait: 100, limit: 300 };

  let sink = new Sink(opts);
  await sink.open();
  let stop = false;
  let crashed = false;
  let redelivered = 0;
  const running = (async () => {
    while (!stop) {
      if (!crashed && sink.written > 400) {
        // The sink wrote and synced a page, then died before it moved the
        // consumer: started again, it is handed the same writes.
        await sink.step({ crashBefore: 'commit' });
        sink.close();
        sink = new Sink(opts);
        await sink.open();
        crashed = true;
        await sink.step();
        redelivered = sink.duplicates;
        continue;
      }
      await sink.step();
    }
  })();
  await mix(l, world, { ops: 3000, workers: 8, seed: 21 });
  stop = true;
  await running;
  await drain(sink, operator);

  assert.ok(crashed, 'the crash happened during the load');
  assert.ok(redelivered > 0, 'the writes after the last commit came again');
  const rows = await journal(operator);
  const { entries, lines } = readSink(file);
  assert.equal(lines, entries.size, 'no entry twice in the file');
  assert.equal(entries.size, rows.length, 'as many entries as the journal');
  for (const r of rows) {
    const s = entries.get(r.entry);
    assert.ok(s, `${r.entry} reached the sink`);
    assert.equal(s.amount, r.amount);
    assert.equal(s.account, r.account);
    assert.equal(s.tx, r.tx);
  }
  // And the file alone balances: every movement's entries sum to zero.
  const byTx = new Map<string, number>();
  for (const s of entries.values()) byTx.set(s.tx, (byTx.get(s.tx) ?? 0) + s.amount);
  for (const [tx, sum] of byTx) assert.equal(sum, 0, tx);
  sink.close();
});

test('a sink whose file is lost copies the journal again before it streams on', async () => {
  const { operator, ledger } = await freshTenant('sink-lost');
  const l = ledger();
  await prepare(l, operator, { n: 4 });
  const dir = mkdtempSync(join(tmpdir(), 'ledger-sink-'));
  const first = new Sink({ base: operator.base, token: OPERATOR_TOKEN, file: join(dir, 'a.ndjson'), wait: 50 });
  await first.open();
  await drain(first, operator);
  first.close();
  // The same consumer, a new empty file: what it had committed is gone with the old one.
  const second = new Sink({ base: operator.base, token: OPERATOR_TOKEN, file: join(dir, 'b.ndjson'), wait: 50 });
  await second.open();
  await drain(second, operator);
  assert.equal(readSink(join(dir, 'b.ndjson')).entries.size, (await journal(operator)).length);
  second.close();
  assert.ok(readFileSync(join(dir, 'b.ndjson'), 'utf8').length > 0);
});

test('change-stream lag: an acknowledged transfer is in the sink within milliseconds', async () => {
  const { operator, ledger } = await freshTenant('sink-lag');
  const l = ledger();
  const world = await prepare(l, operator, { n: 8 });
  const file = join(mkdtempSync(join(tmpdir(), 'ledger-sink-')), 'journal.ndjson');
  // When each transfer's answer came back, and when its entries were in the
  // file: the sink can be first, since the stream hands a write over once
  // it is on disk, as the answer is sent.
  const acked = new Map<string, number>();
  const synced = new Map<string, number>();
  const sink = new Sink({
    base: operator.base,
    token: OPERATOR_TOKEN,
    file,
    wait: 5000,
    status: false,
    onWrite: (lines) => {
      const now = performance.now();
      for (const s of lines) if (s.entry.endsWith(':cr')) synced.set(s.tx, now);
    },
  });
  await sink.open();
  await drain(sink, operator);
  let stop = false;
  const running = (async () => {
    while (!stop) await sink.step();
  })();
  const n = Number(process.env.LEDGER_LAG_TRANSFERS ?? 300);
  const eur = world.accounts.filter((a) => a.currency === 'EUR');
  for (let i = 0; i < n; i++) {
    const [a, b] = [eur[i % 2], eur[2 + (i % 2)]];
    const ref = `lag-${i}-xxxx`;
    const r = await l.transfer({ ref, from: a.ext, to: b.ext, amount: 1, currency: a.currency });
    assert.ok(r.ok);
    acked.set(ref, performance.now());
  }
  // The last transfer's write wakes the waiting read; one more step ends it.
  stop = true;
  await l.transfer({ ref: 'lag-end-xxxx', from: eur[0].ext, to: eur[1].ext, amount: 1, currency: 'EUR' });
  await running;
  await drain(sink, operator);
  sink.close();
  const lag = [...acked].map(([tx, at]) => synced.get(tx)! - at);
  assert.ok(
    lag.every((x) => Number.isFinite(x)),
    'every transfer reached the sink',
  );
  assert.equal(lag.length, n);
  const p50 = percentile(lag, 50);
  const p99 = percentile(lag, 99);
  console.log(`# change-stream lag over ${lag.length} transfers: p50 ${p50.toFixed(2)} ms, p99 ${p99.toFixed(2)} ms`);
  // A bound only on what must happen: an entry on disk reaches the sink.
  assert.ok(p99 < 5000);
});
