// Every panel of the Grafana dashboard (grafana/plover.json) is a statement
// the company's tenant answers under the token Grafana is given -- role
// `app`, through the router -- with Grafana's built-ins bound as the
// plugin's backend binds them: each `$__name` a parameter of its own,
// never written into the text. Run against two days of history.
import { after, before, test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { startWorld, type World } from './harness';
import { mint } from '../lib/tokens';
import { nodeFetch } from '../lib/fetch';

let world: World;
before(async () => {
  world = await startWorld({ basePort: 18450, historyDays: 2 });
});
after(async () => {
  await world?.stop();
});

interface Target {
  refId: string;
  queryText: string;
  format: string;
}
const dashboard = JSON.parse(readFileSync(new URL('../grafana/plover.json', import.meta.url), 'utf8')) as { panels: { title: string; targets: Target[] }[] };

/** `$__from` and the rest made `$1`, `$2`, ... in the order they first appear. */
function bind(text: string, values: Record<string, number>): [string, unknown[]] {
  const names: string[] = [];
  const bound = text.replace(/\$__(\w+)/g, (_, name: string) => {
    if (!(name in values)) throw new Error(`no built-in $__${name}`);
    if (!names.includes(name)) names.push(name);
    return `$${names.indexOf(name) + 1}`;
  });
  return [bound, names.map((n) => values[n])];
}

for (const panel of dashboard.panels) {
  test(`the panel "${panel.title}" runs under Grafana's token`, async () => {
    const to = Date.now();
    const values = { from: to - 2 * 86_400_000, to, interval_ms: 1_800_000, max_data_points: 1000 };
    const token = mint({ sub: 'grafana', role: 'app', tenant: 'co-tandem' }, 300);
    for (const t of panel.targets) {
      // The plugin's formats, not Grafana's frame types: `timeseries` was
      // refused (400) by the data source.
      assert.ok(['auto', 'table', 'time_series'].includes(t.format), `${panel.title}: format ${t.format}`);
      const [query, params] = bind(t.queryText, values);
      const res = await nodeFetch(`${world.router}/t/co-tandem/query`, {
        method: 'POST',
        headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
        body: JSON.stringify({ query, params }),
      });
      const body = await res.text();
      assert.equal(res.status, 200, `${panel.title}: ${body}`);
      const rows = JSON.parse(body) as unknown[];
      // Open orders and positions are empty in a world nobody rides in.
      if (!/positions|status in/.test(query)) assert.ok(rows.length > 0, `${panel.title} answers rows`);
    }
  });
}

test("Grafana's token reads and writes nothing else", async () => {
  const token = mint({ sub: 'grafana', role: 'app', tenant: 'co-tandem' }, 300);
  const res = await nodeFetch(`${world.router}/t/co-tandem/query`, {
    method: 'POST',
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: JSON.stringify({ query: 'set orders {fee: 0} where id = 1' }),
  });
  assert.equal(res.status, 403);
  const other = await nodeFetch(`${world.router}/t/co-hopper/query`, {
    method: 'POST',
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: JSON.stringify({ query: 'get orders count' }),
  });
  assert.equal(other.status, 403);
});
