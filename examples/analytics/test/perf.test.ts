// Budgets that would catch a regression in CI, over the demo's data: the
// dashboard's pages through Kestrel, and how long an event takes to reach
// the rollups through the worker the server runs. Generous, since a CI
// runner is slower and busier than a laptop; npm run bench:ingest and
// bench:queries are the measurements (README, "Measured").
import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { test } from 'node:test';
import { DEMO_PASSWORD, DEMO_SITES } from '../src/setup.ts';
import { APP, beacon, rows } from './helpers.ts';

const pct = (xs: number[], p: number) => [...xs].sort((a, b) => a - b)[Math.min(xs.length - 1, Math.floor((p / 100) * xs.length))];
const BUDGET_MS = Number(process.env.KESTREL_PAGE_BUDGET_MS ?? 400);

async function cookie(): Promise<string> {
  const r = await fetch(`${APP}/signin`, { method: 'POST', redirect: 'manual', headers: { 'content-type': 'application/x-www-form-urlencoded', origin: APP }, body: `name=nadia&password=${DEMO_PASSWORD}` });
  return (r.headers.get('set-cookie') ?? '').split(';')[0];
}

test(`each dashboard range answers within ${BUDGET_MS} ms at the median`, async () => {
  const c = await cookie();
  for (const range of ['1h', '24h', '7d', '30d', '90d']) {
    const times: number[] = [];
    for (let i = 0; i < 12; i++) {
      const t = performance.now();
      const r = await fetch(`${APP}/s/fieldnotes?range=${range}`, { headers: { cookie: c } });
      assert.equal(r.status, 200);
      await r.text();
      if (i >= 2) times.push(performance.now() - t);
    }
    console.log(`  ${range}: p50 ${pct(times, 50).toFixed(1)} ms, the most ${Math.max(...times).toFixed(1)} ms`);
    assert.ok(pct(times, 50) < BUDGET_MS, `${range}: ${pct(times, 50).toFixed(1)} ms`);
  }
});

test('an event reaches the rollups within three seconds of its beacon', async () => {
  const site = DEMO_SITES[0];
  const lags: number[] = [];
  for (let i = 0; i < 5; i++) {
    const [before] = await rows(site.name, 'get rollup_state select events limit 1');
    const t = performance.now();
    const b = { k: site.key, b: randomBytes(9).toString('base64url'), u: 'perf-visitor', t: Date.now(), e: [{ n: 'perf_probe', p: '/', t: Date.now() }] };
    assert.equal((await beacon(b, { origin: site.origins[0] })).status, 204);
    for (;;) {
      const [now] = await rows(site.name, 'get rollup_state select events limit 1');
      if (Number(now.events) > Number(before.events)) break;
      if (performance.now() - t > 10_000) assert.fail('the event never reached the rollups');
      await new Promise((r) => setTimeout(r, 20));
    }
    lags.push(performance.now() - t);
  }
  console.log(`  beacon to rollups: p50 ${pct(lags, 50).toFixed(0)} ms, the most ${Math.max(...lags).toFixed(0)} ms`);
  assert.ok(pct(lags, 50) < 3000);
});
