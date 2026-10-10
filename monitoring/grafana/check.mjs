// `make grafana-check`: the "fenecdb data" dashboard as Grafana serves it,
// against fenec-server. Every panel's query and the symbol variable's are
// asked of Grafana's /api/ds/query -- through the Infinity data source, its
// token and its allowed URL -- as an anonymous viewer, then the same
// FenecQL is sent to fenec-server directly with the server's token, and
// the frames must hold the rows, column by column. Then what the data
// source must refuse: a write, a URL other than POST /query, and a variable
// trying to close its string.
//
//   GRAFANA_URL       http://127.0.0.1:3000
//   FENEC_URL         http://127.0.0.1:8080
//   FENEC_HTTP_TOKEN  the server's token, for the direct runs
import assert from 'node:assert/strict';

const GRAFANA = (process.env.GRAFANA_URL ?? 'http://127.0.0.1:3000').replace(/\/$/, '');
const FENEC = (process.env.FENEC_URL ?? 'http://127.0.0.1:8080').replace(/\/$/, '');
const TOKEN = process.env.FENEC_HTTP_TOKEN;
if (!TOKEN) throw new Error('set FENEC_HTTP_TOKEN');
const DAY = 86_400_000;

async function json(url, init) {
  const res = await fetch(url, init);
  const text = await res.text();
  return { status: res.status, body: text ? JSON.parse(text) : null };
}

// Grafana starts once the seed is done, and installs the plugin before it
// listens; the dashboard is there once provisioning has read it.
let dashboard;
for (let i = 0; ; i++) {
  try {
    const r = await json(`${GRAFANA}/api/dashboards/uid/fenecdb-data`);
    if (r.status === 200) {
      dashboard = r.body.dashboard;
      break;
    }
  } catch {
    // not listening yet
  }
  if (i === 120) throw new Error(`no fenecdb-data dashboard at ${GRAFANA} after two minutes`);
  await new Promise((r) => setTimeout(r, 1000));
}

// The dashboard's range, `now-7d/d` to now in UTC, and the interval of a
// panel about 1 200 px wide: Grafana's frontend works $__interval_ms out
// from the panel's width, so any whole interval stands for it here.
const to = Date.now();
const from = to - (to % DAY) - 7 * DAY;
const intervalMs = 3_600_000;

// What Grafana's frontend writes into the body before it sends it: the
// variables, a text one through the doublequote format as Grafana 13's
// registry has it -- its quotes around the value and a backslash before
// each `"` in it, a backslash left as it is. ${__from} and ${__to} are left
// for the plugin's backend, which writes them in itself, as it must for an
// alert rule, which has no frontend.
function frontend(target, vars) {
  const data = target.url_options.data
    .replaceAll('$__interval_ms', String(intervalMs))
    .replace(/\$\{(\w+):doublequote\}/g, (_, name) => `"${vars[name].replaceAll('"', '\\"')}"`);
  return { ...target, url_options: { ...target.url_options, data } };
}

async function viaGrafana(target) {
  const r = await json(`${GRAFANA}/api/ds/query`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      queries: [{ ...target, refId: 'A', datasource: { type: 'yesoreyeram-infinity-datasource', uid: 'fenecdb-data' }, intervalMs, maxDataPoints: 1000 }],
      from: String(from),
      to: String(to),
    }),
  });
  const result = r.body?.results?.A;
  return { status: r.status, result, frames: result?.frames ?? [] };
}

async function direct(target) {
  const body = target.url_options.data.replaceAll('${__from}', String(from)).replaceAll('${__to}', String(to));
  const res = await fetch(`${FENEC}/query`, { method: 'POST', headers: { authorization: `Bearer ${TOKEN}` }, body });
  const rows = await res.json();
  assert.equal(res.status, 200, `fenec-server answered ${res.status}: ${JSON.stringify(rows)}`);
  return rows;
}

// A column as the direct rows give it: a timestamp as epoch milliseconds,
// as the frame holds it, numbers and text as they are.
function column(c, rows) {
  return rows.map((row) => (row[c.selector] == null ? null : c.type === 'timestamp' ? Date.parse(row[c.selector]) : row[c.selector]));
}

async function same(name, target) {
  const g = await viaGrafana(target);
  assert.equal(g.status, 200, `${name}: Grafana answered ${g.status}: ${JSON.stringify(g.result)}`);
  assert.ok(!g.result.error, `${name}: ${g.result.error}`);
  assert.equal(g.frames.length, 1, `${name}: ${g.frames.length} frames`);
  const frame = g.frames[0];
  const rows = await direct(target);
  assert.ok(rows.length > 0, `${name}: fenec-server has no rows for it`);
  // The plugin orders a frame's fields by name (the dashboard's Organize
  // transformation puts them back), so they are matched by name.
  const fields = frame.schema.fields.map((f) => f.name);
  assert.deepEqual([...fields].sort(), target.columns.map((c) => c.text).sort(), `${name}: the frame's fields`);
  for (const c of target.columns) {
    assert.deepEqual(frame.data.values[fields.indexOf(c.text)], column(c, rows), `${name}: ${c.text} against fenec-server's rows`);
  }
  console.log(`ok  ${name}: ${rows.length} rows, ${target.columns.length} columns as fenec-server answers them`);
  return rows;
}

async function refused(name, target, why) {
  const g = await viaGrafana(target);
  const text = JSON.stringify(g.result);
  assert.ok(g.status >= 400 || g.result?.error, `${name}: not refused: ${text}`);
  assert.match(text, why, `${name}: refused for another reason: ${text}`);
  console.log(`ok  ${name}: refused (${g.result?.status ?? g.status})`);
}

// The symbol variable first: the candles' query names its value.
const sym = dashboard.templating.list.find((v) => v.name === 'sym');
const symbols = await same('variable sym', sym.query.infinityQuery);
const vars = { sym: symbols[0].sym };

const panels = dashboard.panels.flatMap((p) => (p.type === 'row' ? (p.panels ?? []) : [p]));
let asked = 0;
for (const panel of panels) {
  for (const target of panel.targets ?? []) {
    await same(panel.title.replace('$sym', vars.sym), frontend(target, vars));
    asked++;
  }
}
assert.equal(asked, 6, 'every panel of the dashboard asked');

// The data source's token reads; it does not write.
const candles = panels.find((p) => p.type === 'candlestick').targets[0];
const write = { ...candles, url_options: { ...candles.url_options, data: JSON.stringify({ query: 'put ticks {sym: "HACK", px: 1}' }) } };
await refused('a write through the data source', write, /403/);
const hack = await direct({ url_options: { data: JSON.stringify({ query: 'get ticks select count(*) as n where sym = "HACK"' }) } });
assert.equal(hack[0].n, 0, 'the refused write left no row');

// The data source reaches POST /query and nothing else: its allowed URL
// holds every query's to that prefix, host and path.
await refused('another path of the server', { ...candles, url: '/_whoami', url_options: { method: 'GET' } }, /allowed/i);

// A symbol written to close its string and send a statement of its own --
// what a link's ?var-sym= can hold. Its quotes are escaped, so it stays one
// value, which no symbol is, and the candles have no rows.
const evil = `${vars.sym}"], "query": "get ticks select count(*) as n", "params": ["`;
const g = await viaGrafana(frontend(candles, { sym: evil }));
assert.ok(!g.result.error, `the injected symbol: ${g.result.error}`);
assert.equal(g.frames[0].data.values[0]?.length ?? 0, 0, 'the injected symbol matched rows');
console.log('ok  a symbol trying to close its string: one value, no rows');
// The format leaves a backslash as it is, so `\"` does end the string --
// with the symbol the body's last string, what follows can only make a body
// that is not JSON, which fenec-server refuses before it runs anything.
await refused('a symbol ending its string with \\"', frontend(candles, { sym: `${vars.sym}\\", 1, 2], "query": "get ticks select count(*) as n", "params": [` }), /400/);
