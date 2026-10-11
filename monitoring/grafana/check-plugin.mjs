// `make grafana-check`, the plugin's half: the "fenecdb data (plugin)"
// dashboard as Grafana serves it, through fenecdb-datasource, against
// fenec-server. Every panel's query, the variables' and the annotation's are
// asked of Grafana's /api/ds/query as an anonymous viewer would ask them --
// the browser's part done here: the variables' values beside the query,
// nothing written into its text -- and the same statement, bound here by
// hand the way the plugin's backend binds it, is sent to fenec-server
// directly with the server's token. The frames must hold the rows column
// by column, in the select list's order. Then the alert rule must have
// evaluated, an instance a symbol, firing exactly where the server's rows
// say; and what the data source must refuse, and must not show.
//
//   GRAFANA_URL       http://127.0.0.1:3000
//   FENEC_URL         http://127.0.0.1:8080
//   FENEC_HTTP_TOKEN  the server's token, for the direct runs
//   GRAFANA_ADMIN     admin:admin, for the alert rules and the data source's settings
import assert from 'node:assert/strict';

const GRAFANA = (process.env.GRAFANA_URL ?? 'http://127.0.0.1:3000').replace(/\/$/, '');
const FENEC = (process.env.FENEC_URL ?? 'http://127.0.0.1:8080').replace(/\/$/, '');
const TOKEN = process.env.FENEC_HTTP_TOKEN;
if (!TOKEN) throw new Error('set FENEC_HTTP_TOKEN');
const ADMIN = `Basic ${Buffer.from(process.env.GRAFANA_ADMIN ?? 'admin:admin').toString('base64')}`;
const DAY = 86_400_000;
const DS = { type: 'fenecdb-datasource', uid: 'fenecdb' };

async function json(url, init) {
  const res = await fetch(url, init);
  const text = await res.text();
  return { status: res.status, text, body: text ? JSON.parse(text) : null };
}

let dashboard;
for (let i = 0; ; i++) {
  try {
    const r = await json(`${GRAFANA}/api/dashboards/uid/fenecdb-data-plugin`);
    if (r.status === 200) {
      dashboard = r.body.dashboard;
      break;
    }
  } catch {
    // not listening yet
  }
  if (i === 120) throw new Error(`no fenecdb-data-plugin dashboard at ${GRAFANA} after two minutes`);
  await new Promise((r) => setTimeout(r, 1000));
}

// The dashboard's range and a panel's interval, as in check.mjs.
const to = Date.now();
const from = to - (to % DAY) - 7 * DAY;
const intervalMs = 3_600_000;
const maxDataPoints = 1000;

async function viaPlugin(target, vars = {}) {
  const r = await json(`${GRAFANA}/api/ds/query`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      queries: [{ ...target, refId: 'A', datasource: DS, vars, intervalMs, maxDataPoints }],
      from: String(from),
      to: String(to),
    }),
  });
  const result = r.body?.results?.A;
  return { status: r.status, result, frames: result?.frames ?? [] };
}

// The statement as the plugin's backend sends it, made here by hand: the
// parameters' list first, then each name a `$n` in the order first named,
// a list after `in` a bracket of them. (None of the dashboard's statements
// holds a `$` in a string or a comment, which the backend's walk skips.)
function bind(target, vars, range = { from, to, intervalMs }) {
  const params = (target.params ?? []).map((p) => p.value);
  const slots = new Map();
  const builtins = { __from: range.from, __to: range.to, __interval_ms: range.intervalMs, __max_data_points: maxDataPoints };
  const query = target.queryText.replace(/(\bin\s+)?\$(?:\{(\w+)\}|([A-Za-z_]\w*))/g, (whole, inKw, a, b) => {
    const name = a ?? b;
    if (!slots.has(name)) {
      const v = name in builtins ? builtins[name] : vars[name];
      assert.ok(v !== undefined, `${name} has no value here`);
      const values = Array.isArray(v) ? v : [v];
      slots.set(name, values.map((x) => params.push(x)));
    }
    const s = slots.get(name).map((n) => `$${n}`).join(', ');
    return inKw ? `${inKw}[${s}]` : s;
  });
  return { query, params };
}

async function direct(target, vars) {
  const body = bind(target, vars);
  const res = await fetch(`${FENEC}/query`, { method: 'POST', headers: { authorization: `Bearer ${TOKEN}` }, body: JSON.stringify(body) });
  const rows = await res.json();
  assert.equal(res.status, 200, `fenec-server answered ${res.status}: ${JSON.stringify(rows)}`);
  return { rows, body };
}

// A value as a frame holds it: a time as epoch milliseconds.
const cell = (v) => (typeof v === 'string' && /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d{3})?Z$/.test(v) ? Date.parse(v) : v);

async function same(name, target, vars) {
  const g = await viaPlugin(target, vars);
  assert.equal(g.status, 200, `${name}: Grafana answered ${g.status}: ${JSON.stringify(g.result)}`);
  assert.ok(!g.result.error, `${name}: ${g.result.error}`);
  const { rows, body } = await direct(target, vars);
  assert.ok(rows.length > 0, `${name}: fenec-server has no rows for it`);
  const frame = g.frames[0];
  // The statement the plugin sent, as the query inspector shows it.
  assert.equal(frame.schema.meta.executedQueryString.split('\n')[0], body.query, `${name}: the statement sent`);
  const fields = frame.schema.fields;
  const labelled = fields.some((f) => f.labels);
  if (!labelled) {
    // The columns in the select list's order, no transformation between.
    assert.deepEqual(
      fields.map((f) => f.name),
      Object.keys(rows[0]),
      `${name}: the frame's fields in the select list's order`
    );
    fields.forEach((f, i) => {
      assert.deepEqual(frame.data.values[i], rows.map((r) => cell(r[f.name]) ?? null), `${name}: ${f.name}`);
    });
    console.log(`ok  ${name}: ${rows.length} rows, ${fields.length} columns in the select list's order (${frame.schema.meta.type})`);
    return rows;
  }
  // Long to wide: a series a label, each value where the rows have one.
  const [time, label, value] = Object.keys(rows[0]);
  const times = frame.data.values[0];
  assert.deepEqual(times, [...new Set(rows.map((r) => cell(r[time])))], `${name}: the series' times`);
  const series = fields.slice(1);
  assert.deepEqual(series.map((f) => f.labels[label]).sort(), [...new Set(rows.map((r) => r[label]))].sort(), `${name}: a series a ${label}`);
  series.forEach((f, i) => {
    const want = times.map((t) => rows.find((r) => cell(r[time]) === t && r[label] === f.labels[label])?.[value] ?? null);
    assert.deepEqual(frame.data.values[i + 1], want, `${name}: ${f.name} ${JSON.stringify(f.labels)}`);
  });
  console.log(`ok  ${name}: ${rows.length} rows as ${series.length} series by ${label} (long to wide)`);
  return rows;
}

// The variables: the symbol's first column, and the symbols' three most traded.
const variable = (n) => dashboard.templating.list.find((v) => v.name === n).query;
const symbols = await same('variable sym', variable('sym'), {});
const traded = await same('variable syms', variable('syms'), {});
const vars = { sym: symbols[0].sym, syms: traded.slice(0, 3).map((r) => r.sym) };

const panels = dashboard.panels.flatMap((p) => (p.type === 'row' ? (p.panels ?? []) : [p]));
let asked = 0;
for (const panel of panels) {
  for (const target of panel.targets ?? []) {
    await same(panel.title.replace('$syms', vars.syms.join(', ')).replace('$sym', vars.sym), target, vars);
    asked++;
  }
}
assert.equal(asked, 7, 'every panel of the dashboard asked');
for (const a of dashboard.annotations.list) {
  await same(`annotations: ${a.name.replace('$sym', vars.sym)}`, a.target, vars);
}

// Asked again with nothing written since: answered from the answer the
// plugin kept, sent with If-None-Match and answered 304.
{
  const again = await viaPlugin(variable('sym'), {});
  assert.equal(again.frames[0].schema.meta.custom.fenecdb.notModified, true, 'the variable asked again was not a 304');
  console.log('ok  a query asked again unchanged: a 304, answered from the kept answer');
}

// The alert rule: evaluated in Grafana's backend, an instance a symbol, and
// firing where the last ten minutes of the server's rows traded more than
// the threshold.
{
  const rule = (await json(`${GRAFANA}/api/v1/provisioning/alert-rules/fenecdb-volume`, { headers: { authorization: ADMIN } })).body;
  const query = rule.data.find((d) => d.refId === 'A').model;
  const threshold = rule.data.find((d) => d.refId === 'C').model.conditions[0].evaluator.params[0];
  let state;
  for (let i = 0; ; i++) {
    const r = await json(`${GRAFANA}/api/prometheus/grafana/api/v1/rules`, { headers: { authorization: ADMIN } });
    state = r.body.data.groups.flatMap((g) => g.rules).find((x) => x.name === rule.title);
    if (state?.lastEvaluation && !state.lastEvaluation.startsWith('0001') && state.alerts?.length) {
      break;
    }
    if (i === 60) throw new Error(`the rule did not evaluate in a minute: ${JSON.stringify(state)}`);
    await new Promise((r) => setTimeout(r, 1000));
  }
  assert.equal(state.health, 'ok', `the rule's health: ${state.health} ${state.lastError ?? ''}`);
  // The same statement over the rule's last six hours, bound by hand.
  const now = Date.now();
  const res = await fetch(`${FENEC}/query`, {
    method: 'POST',
    headers: { authorization: `Bearer ${TOKEN}` },
    body: JSON.stringify(bind(query, {}, { from: now - rule.data[0].relativeTimeRange.from * 1000, to: now, intervalMs: query.intervalMs })),
  });
  const rows = await res.json();
  const last = new Map();
  for (const r of rows) last.set(r.sym, r.volume);
  const firing = [...last].filter(([, v]) => v > threshold).map(([s]) => s).sort();
  const alerts = state.alerts.map((a) => [a.labels.sym, a.state]);
  assert.equal(alerts.length, last.size, `an instance a symbol: ${alerts.length} for ${last.size}`);
  assert.deepEqual(alerts.filter(([, s]) => s === 'Alerting').map(([s]) => s).sort(), firing, 'the instances firing');
  console.log(`ok  the alert rule evaluated: ${alerts.length} instances by sym, ${firing.length} firing as the server's rows say`);
}

// What the data source refuses: a write, before it is sent; and a symbol
// trying to be FenecQL, which stays one value and matches nothing.
{
  const candles = panels.find((p) => p.type === 'candlestick').targets[0];
  const write = await viaPlugin({ ...candles, queryText: 'put ticks {sym: "HACK", px: 1}' }, vars);
  assert.match(write.result.error ?? '', /the data source runs reads/, `a write: ${JSON.stringify(write.result)}`);
  const n = (await (await fetch(`${FENEC}/query`, {
    method: 'POST',
    headers: { authorization: `Bearer ${TOKEN}` },
    body: JSON.stringify({ query: 'get ticks select count(*) as n where sym = "HACK"' }),
  })).json())[0].n;
  assert.equal(n, 0, 'the refused write left no row');
  console.log('ok  a write through the data source: refused before it is sent');
  for (const evil of [`${vars.sym}" or sym != "`, `${vars.sym}\\" or 1 = 1 --`, `${vars.sym}"], "query": "get ticks", "params": ["`]) {
    const g = await viaPlugin(candles, { ...vars, sym: evil });
    assert.ok(!g.result.error, `${evil}: ${g.result.error}`);
    assert.equal(g.frames[0].data.values[0]?.length ?? 0, 0, `${evil} matched rows`);
  }
  console.log('ok  symbols trying to close their string: each one value, no rows');
}

// The token stays in Grafana's server: in no page's settings, and the data
// source's own says only that one is set. Its health check reads with it.
{
  const boot = await json(`${GRAFANA}/api/frontend/settings`);
  assert.ok(!/eyJ[\w-]+\.eyJ/.test(boot.text), 'a token in the frontend settings');
  const settings = await json(`${GRAFANA}/api/datasources/uid/fenecdb`, { headers: { authorization: ADMIN } });
  assert.equal(settings.body.secureJsonFields?.token, true, 'the token is not set');
  assert.ok(!/eyJ[\w-]+\.eyJ/.test(settings.text), "a token in the data source's settings");
  const health = await json(`${GRAFANA}/api/datasources/uid/fenecdb/health`, { headers: { authorization: ADMIN } });
  assert.equal(health.body.status, 'OK', `health: ${health.text}`);
  console.log(`ok  the token: in no browser's reach; health: ${health.body.message}`);
  const explain = await json(`${GRAFANA}/api/datasources/uid/fenecdb/resources/explain`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ query: { ...panels[1].targets[0], vars }, from, to, intervalMs, maxDataPoints }),
  });
  assert.equal(explain.status, 200, `explain: ${explain.text}`);
  assert.ok(explain.body.plan.length > 0, 'explain gave no plan');
  console.log(`ok  explain, as the editor asks it: ${explain.body.plan.length} lines of plan`);
}
