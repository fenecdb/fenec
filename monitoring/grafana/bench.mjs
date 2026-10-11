// What a panel costs each way, against a running monitoring/ stack (`make
// grafana-bench` starts one): fenec-server directly with the server's
// token, Grafana through the Infinity data source, and Grafana through
// fenecdb-datasource -- a new answer each time (the range's end moved a
// millisecond a run, as a dashboard's `now` moves), and the same request
// again, which the plugin sends with If-None-Match and answers from its
// kept answer on a 304. The plugin twice: with the stack's read-only JWT,
// whose 304 the server works out by running the query and comparing its
// digest, and with the server's token, whose 304 the server answers
// without running it (a data source made for the run and deleted after).
// The median of RUNS each, the ways in turns.
//
//   GRAFANA_URL       http://127.0.0.1:3000
//   FENEC_URL         http://127.0.0.1:8080
//   FENEC_HTTP_TOKEN  the server's token
//   GRAFANA_ADMIN     admin:admin
//   RUNS              21
import assert from 'node:assert/strict';

const GRAFANA = (process.env.GRAFANA_URL ?? 'http://127.0.0.1:3000').replace(/\/$/, '');
const FENEC = (process.env.FENEC_URL ?? 'http://127.0.0.1:8080').replace(/\/$/, '');
const TOKEN = process.env.FENEC_HTTP_TOKEN;
if (!TOKEN) throw new Error('set FENEC_HTTP_TOKEN');
const ADMIN = { authorization: `Basic ${Buffer.from(process.env.GRAFANA_ADMIN ?? 'admin:admin').toString('base64')}` };
const RUNS = Number(process.env.RUNS ?? 21);
const DAY = 86_400_000;
const to0 = Date.now();
const from = to0 - (to0 % DAY) - 7 * DAY;
const intervalMs = 3_600_000;

async function json(url, init) {
  const res = await fetch(url, init);
  const text = await res.text();
  return { status: res.status, body: text ? JSON.parse(text) : null };
}

const plugin = (await json(`${GRAFANA}/api/dashboards/uid/fenecdb-data-plugin`)).body.dashboard;
const infinity = (await json(`${GRAFANA}/api/dashboards/uid/fenecdb-data`)).body.dashboard;
const targets = (d) => d.panels.filter((p) => p.targets).map((p) => [p.title, p.targets[0]]);
const sym = (await json(`${FENEC}/query`, { method: 'POST', headers: { authorization: `Bearer ${TOKEN}` }, body: JSON.stringify({ query: 'get ticks select sym, count(*) as n group sym order sym limit 1' }) })).body[0].sym;

// A data source of the plugin's with the server's token, for this run only.
const made = await json(`${GRAFANA}/api/datasources`, {
  method: 'POST',
  headers: { ...ADMIN, 'content-type': 'application/json' },
  body: JSON.stringify({ name: 'fenecdb (bench, server token)', type: 'fenecdb-datasource', access: 'proxy', url: 'http://fenecdb:8080', secureJsonData: { token: TOKEN } }),
});
assert.equal(made.status, 200, JSON.stringify(made.body));
const serverUid = made.body.datasource.uid;

async function timed(fn) {
  const t0 = performance.now();
  const out = await fn();
  return [performance.now() - t0, out];
}

async function viaPlugin(uid, target, to) {
  const r = await json(`${GRAFANA}/api/ds/query`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      queries: [{ ...target, refId: 'A', datasource: { type: 'fenecdb-datasource', uid }, vars: { sym, syms: [sym] }, intervalMs, maxDataPoints: 1000 }],
      from: String(from),
      to: String(to),
    }),
  });
  assert.ok(!r.body.results.A.error, r.body.results.A.error);
  return r.body.results.A.frames[0].schema.meta.custom.fenecdb.notModified;
}

async function viaInfinity(target, to) {
  const data = target.url_options.data
    .replaceAll('$__interval_ms', String(intervalMs))
    .replace(/\$\{(\w+):doublequote\}/g, () => JSON.stringify(sym));
  const r = await json(`${GRAFANA}/api/ds/query`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      queries: [{ ...target, url_options: { ...target.url_options, data }, refId: 'A', datasource: { type: 'yesoreyeram-infinity-datasource', uid: 'fenecdb-data' }, intervalMs, maxDataPoints: 1000 }],
      from: String(from),
      to: String(to),
    }),
  });
  assert.ok(!r.body.results.A.error, r.body.results.A.error);
}

async function direct(target, to) {
  const body = target.url_options.data
    .replaceAll('${__from}', String(from))
    .replaceAll('${__to}', String(to))
    .replaceAll('$__interval_ms', String(intervalMs))
    .replace(/\$\{(\w+):doublequote\}/g, () => JSON.stringify(sym));
  const res = await fetch(`${FENEC}/query`, { method: 'POST', headers: { authorization: `Bearer ${TOKEN}` }, body });
  assert.equal(res.status, 200);
  await res.text();
}

const median = (xs) => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];
const ways = ['direct', 'infinity', 'plugin, new', 'plugin, 304 (JWT)', 'plugin, 304 (server token)'];
console.log(['panel', ...ways].join('\t'));
try {
  const inf = new Map(targets(infinity));
  for (const [title, target] of targets(plugin)) {
    const t = inf.get(title);
    const times = ways.map(() => []);
    // The 304 runs ask one fixed range, answered once before the first run.
    await viaPlugin('fenecdb', target, to0);
    await viaPlugin(serverUid, target, to0);
    for (let i = 1; i <= RUNS; i++) {
      if (t) {
        times[0].push((await timed(() => direct(t, to0 + i)))[0]);
        times[1].push((await timed(() => viaInfinity(t, to0 + i)))[0]);
      }
      const [fresh, nm] = await timed(() => viaPlugin('fenecdb', target, to0 + i));
      assert.equal(nm, false);
      times[2].push(fresh);
      const [kept, nm2] = await timed(() => viaPlugin('fenecdb', target, to0));
      assert.equal(nm2, true, `${title}: not a 304`);
      times[3].push(kept);
      const [kept2, nm3] = await timed(() => viaPlugin(serverUid, target, to0));
      assert.equal(nm3, true, `${title}: not a 304 with the server's token`);
      times[4].push(kept2);
    }
    console.log([title, ...times.map((x) => (x.length ? median(x).toFixed(1) : '-'))].join('\t'));
  }
} finally {
  await fetch(`${GRAFANA}/api/datasources/uid/${serverUid}`, { method: 'DELETE', headers: ADMIN });
}
