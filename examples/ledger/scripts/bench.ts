// Transfers a second and their latency: one client and sixteen, under
// `--sync always` and `--sync 250`, each against a fenec-server of its own
// over a new directory. Every transfer is the whole block -- the rate
// limit's two statements, the two guards, the debit, the credit, the two
// entries and the record -- sent as one /batch with an Idempotency-Key.
//
//   npm run bench                      every cell, 10 s each
//   LEDGER_BENCH_SECONDS=30 npm run bench
//   LEDGER_BENCH_LOG=bench.log         where progress goes (stdout too)
import { appendFileSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { OPERATOR_TOKEN } from '../src/config.ts';
import { Ledger } from '../src/ledger.ts';
import { createTenant } from '../src/setup.ts';
import { HttpStore } from '../src/store.ts';
import { Tokens } from '../src/tokens.ts';
import { startNode } from '../test/server.ts';
import { percentile, prepare, rng } from '../test/workload.ts';

const SECONDS = Number(process.env.LEDGER_BENCH_SECONDS ?? 10);
const LOG = process.env.LEDGER_BENCH_LOG;
const say = (s: string) => {
  console.log(s);
  if (LOG) appendFileSync(LOG, s + '\n');
};

const rows: string[] = [];
for (const sync of ['always', '250']) {
  for (const clients of [1, 16]) {
    const dir = join(mkdtempSync(join(tmpdir(), 'ledger-bench-')), 'tenants');
    const node = await startNode({ dir, sync });
    try {
      await createTenant('bench', node.url);
      const base = `${node.url}/t/bench`;
      const ledger = new Ledger(new HttpStore(base, new Tokens('bench').app()), { rateLimit: 1e9 });
      const world = await prepare(ledger, new HttpStore(base, OPERATOR_TOKEN), { n: 64, opening: 1_000_000_000, currencies: ['EUR'] });
      const latency: number[] = [];
      let refused = 0;
      const until = performance.now() + SECONDS * 1000;
      let n = 0;
      await Promise.all(
        Array.from({ length: clients }, async (_, w) => {
          const r = rng(w + 1);
          while (performance.now() < until) {
            const from = r.pick(world.accounts);
            let to = r.pick(world.accounts);
            while (to.ext === from.ext) to = r.pick(world.accounts);
            const key = `b${sync}-${clients}-${n++}`;
            const t0 = performance.now();
            const res = await ledger.transfer(
              { ref: `tr-${key}`, from: from.ext, to: to.ext, amount: r.int(1, 5000), currency: 'EUR' },
              key,
            );
            latency.push(performance.now() - t0);
            if (!res.ok) refused++;
          }
        }),
      );
      const rate = latency.length / SECONDS;
      const row = `| \`--sync ${sync}\` | ${clients} | ${Math.round(rate).toLocaleString('en-GB')} | ${percentile(latency, 50).toFixed(2)} ms | ${percentile(latency, 99).toFixed(2)} ms | ${refused} |`;
      say(
        `sync ${sync}, ${clients} client(s): ${Math.round(rate)} transfers/s, p50 ${percentile(latency, 50).toFixed(2)} ms, p99 ${percentile(latency, 99).toFixed(2)} ms, ${refused} refused`,
      );
      rows.push(row);
    } finally {
      await node.stop();
    }
  }
}
say('\n| sync | clients | transfers/s | p50 | p99 | refused |\n| --- | --- | --- | --- | --- | --- |\n' + rows.join('\n'));
