// Reconciliation at a million journal entries: 500 000 transfers made
// through the ledger itself (16 clients, `--sync 250`), then the
// reconciliation's one snapshot timed, and what it costs the transfers
// that wait for its lock meanwhile.
//
//   npm run recon-bench
//   LEDGER_RECON_TRANSFERS=500000  LEDGER_BENCH_LOG=recon.log
import { appendFileSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { OPERATOR_TOKEN } from '../src/config.ts';
import { Ledger } from '../src/ledger.ts';
import { reconcile, unbalanced } from '../src/reconcile.ts';
import { createTenant } from '../src/setup.ts';
import { HttpStore } from '../src/store.ts';
import { Tokens } from '../src/tokens.ts';
import { startNode } from '../test/server.ts';
import { percentile, prepare, rng } from '../test/workload.ts';

const TRANSFERS = Number(process.env.LEDGER_RECON_TRANSFERS ?? 500_000);
const LOG = process.env.LEDGER_BENCH_LOG;
const say = (s: string) => {
  console.log(s);
  if (LOG) appendFileSync(LOG, s + '\n');
};

const dir = join(mkdtempSync(join(tmpdir(), 'ledger-recon-')), 'tenants');
const node = await startNode({ dir, sync: '250' });
try {
  await createTenant('recon', node.url);
  const base = `${node.url}/t/recon`;
  const ledger = new Ledger(new HttpStore(base, new Tokens('recon').app()), { rateLimit: 1e9 });
  const operator = new HttpStore(base, OPERATOR_TOKEN);
  const world = await prepare(ledger, operator, { n: 1000, opening: 1_000_000_000, currencies: ['EUR', 'GBP', 'USD'] });

  // The load, through the ledger's own blocks.
  let made = 0;
  const t0 = performance.now();
  await Promise.all(
    Array.from({ length: 16 }, async (_, w) => {
      const r = rng(w + 101);
      const byCur = new Map<string, typeof world.accounts>();
      for (const a of world.accounts) byCur.set(a.currency, [...(byCur.get(a.currency) ?? []), a]);
      while (made < TRANSFERS) {
        const i = made++;
        const from = r.pick(world.accounts);
        const same = byCur.get(from.currency)!;
        let to = r.pick(same);
        while (to.ext === from.ext) to = r.pick(same);
        const res = await ledger.transfer({ ref: `tr-r${i}`, from: from.ext, to: to.ext, amount: r.int(1, 5000), currency: from.currency });
        if (!res.ok) throw new Error(res.error);
        if (i % 50_000 === 0) say(`loaded ${i} transfers, ${((performance.now() - t0) / 1000).toFixed(0)} s`);
      }
    }),
  );
  say(
    `loaded ${TRANSFERS} transfers in ${((performance.now() - t0) / 1000).toFixed(1)} s (${Math.round(TRANSFERS / ((performance.now() - t0) / 1000))}/s)`,
  );

  // The snapshot alone, five times.
  const times: number[] = [];
  let entries = 0;
  for (let i = 0; i < 5; i++) {
    const rep = await reconcile(operator);
    if (!rep.ok) throw new Error(`out of balance: ${JSON.stringify(rep).slice(0, 500)}`);
    times.push(rep.ms);
    entries = rep.entries;
  }
  say(
    `reconcile over ${entries} entries and ${world.accounts.length + world.frozen.length + 3} accounts: ${times.map((t) => t.toFixed(0)).join(', ')} ms`,
  );
  const tb = performance.now();
  const bad = await unbalanced(operator);
  say(`every movement's legs summed (group by tx): ${(performance.now() - tb).toFixed(0)} ms, ${bad.length} unbalanced`);

  // Transfers from 4 clients, alone and beside a reconciliation every second.
  const run = async (seconds: number, beside: boolean) => {
    const lat: number[] = [];
    const until = performance.now() + seconds * 1000;
    let n = 0;
    const recon = beside
      ? (async () => {
          while (performance.now() < until) {
            await reconcile(operator);
            await new Promise((r) => setTimeout(r, 1000));
          }
        })()
      : Promise.resolve();
    await Promise.all([
      recon,
      ...Array.from({ length: 4 }, async (_, w) => {
        const r = rng(w + 900 + (beside ? 50 : 0));
        while (performance.now() < until) {
          const [a, b] = [world.accounts[r.int(0, 332) * 3], world.accounts[r.int(0, 332) * 3 + 3]];
          if (!b || a.currency !== b.currency) continue;
          const s = performance.now();
          await ledger.transfer({ ref: `tr-x${beside ? 'b' : 'a'}${w}-${n++}`, from: a.ext, to: b.ext, amount: 1, currency: a.currency });
          lat.push(performance.now() - s);
        }
      }),
    ]);
    return lat;
  };
  for (const beside of [false, true]) {
    const lat = await run(10, beside);
    say(
      `transfers ${beside ? 'beside a reconciliation a second' : 'alone'}: ${Math.round(lat.length / 10)}/s, p50 ${percentile(lat, 50).toFixed(2)} ms, p99 ${percentile(lat, 99).toFixed(2)} ms, max ${Math.max(...lat).toFixed(1)} ms`,
    );
  }
} finally {
  await node.stop();
}
