// The dashboard's questions, measured at scale: a site of N events over
// 30 days, loaded with the rollup worker keeping up beside the load, then
// every range asked 25 times -- each question's time and the whole
// dashboard's -- and the raw path forced over a week and a month to show
// what the switch to the rollups saves. A node of its own.
//
//   BENCH_SIZES   1000000            events, by commas (10000000 takes a few minutes and a few GB of disk)
//   BENCH_RUNS    25
import { statSync } from 'node:fs';
import { join } from 'node:path';
import { dir, log, nodeRss, pct, startNode, stopAll } from './bench-env.ts';

const { addSite, setupControl, writeEvents } = await import('../src/setup.ts');
const { dashboard, noFilters } = await import('../src/queries.ts');
const { RollupWorker } = await import('../src/rollup.ts');
const { Traffic } = await import('../src/sim.ts');
const { DAY, dayOf, MINUTE } = await import('../src/time.ts');
type Range = import('../src/queries.ts').Range;

const sizes = (process.env.BENCH_SIZES ?? '1000000').split(',').map(Number);
const RUNS = Number(process.env.BENCH_RUNS ?? 25);

await startNode();
await setupControl();
const out: string[] = [];

for (const N of sizes) {
  const name = `q${N}`;
  await addSite({ name, key: `benchq${N}`.slice(0, 20).padEnd(10, '0'), label: 'Bench', origins: ['https://bench.example'], rate: 1 });
  // About 4.1 events for each new visitor over 30 days at this mix (returns included): new visitors a day for N.
  const traffic = new Traffic({ daily: Math.round(N / (30 * 4.1)), growth: 0, seed: 99, prefix: 'q' });
  const now = Date.now();
  const start = dayOf(now) - 29 * DAY;
  const worker = new RollupWorker({ site: name, wait: 200, onError: (e) => log('worker:', e) });
  const running = worker.run();
  let loaded = 0;
  let head = 0;
  const t0 = performance.now();
  for (let i = 0; i < 30 && loaded < N; i++) {
    const events = traffic.day(start + i * DAY, i).filter((e) => e.at < now - MINUTE);
    head = await writeEvents(name, events.slice(0, N - loaded));
    loaded += Math.min(events.length, N - loaded);
    // Keep the worker within reach of the stream's buffer (--replication-buffer).
    while (head - worker.seq > 400_000) await new Promise((r) => setTimeout(r, 100));
    log(`${name}: day ${i + 1}, ${loaded.toLocaleString('en-US')} events, worker at ${worker.seq.toLocaleString('en-US')} of ${head.toLocaleString('en-US')}`);
  }
  const loadSecs = (performance.now() - t0) / 1000;
  while (worker.seq < head) await new Promise((r) => setTimeout(r, 100));
  const foldSecs = (performance.now() - t0) / 1000;
  worker.stop();
  await running;
  const file = statSync(join(dir, 'tenants', `${name}.fenec`)).size / 1e6;
  out.push(`\n### ${loaded.toLocaleString('en-US')} events over 30 days\n`);
  out.push(`Loaded in ${loadSecs.toFixed(1)} s with the worker beside it, folded ${foldSecs.toFixed(1)} s after the start (${Math.round(loaded / foldSecs).toLocaleString('en-US')} events/s); the file ${file.toFixed(0)} MB, the node ${(await nodeRss()).toFixed(0)} MB resident.\n`);
  out.push('| range | from | dashboard p50 | p99 | slowest questions at p50 |\n| --- | --- | --- | --- | --- |');
  const cases: [string, Range, ReturnType<typeof noFilters>][] = [
    ['1h', '1h', noFilters()],
    ['24h', '24h', noFilters()],
    ['7d', '7d', noFilters()],
    ['30d', '30d', noFilters()],
    ['90d', '90d', noFilters()],
    // Every device: the same rows, through the raw path.
    ['7d, raw', '7d', { ...noFilters(), device: ['desktop', 'mobile', 'tablet'] }],
    ['30d, raw', '30d', { ...noFilters(), device: ['desktop', 'mobile', 'tablet'] }],
    ['24h, one country', '24h', { ...noFilters(), country: ['DE'] }],
  ];
  for (const [label, range, f] of cases) {
    const totals: number[] = [];
    const per: Record<string, number[]> = {};
    let source = '';
    for (let i = 0; i < RUNS + 2; i++) {
      const t = performance.now();
      const d = await dashboard(name, range, f);
      if (i < 2) continue; // warm
      totals.push(performance.now() - t);
      source = d.source;
      for (const [k, v] of Object.entries(d.timings)) (per[k] ??= []).push(v);
    }
    const slow = Object.entries(per)
      .map(([k, v]) => [k, pct(v, 50)] as const)
      .sort((a, b) => b[1] - a[1])
      .slice(0, 2);
    const line = `| ${label} | ${source} | ${pct(totals, 50).toFixed(1)} ms | ${pct(totals, 99).toFixed(1)} ms | ${slow.map(([k, v]) => `${k} ${v.toFixed(1)} ms`).join(', ')} |`;
    out.push(line);
    log(line);
  }
}
console.log(out.join('\n'));
stopAll();
process.exit(0);
