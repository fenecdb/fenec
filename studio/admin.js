// The server's own numbers, for the token that may read them: the shapes
// of the statements that took the most time and ran the most
// (`/_stats/statements`), what `/_metrics` counts -- statements, their
// latency, writes, the file and what a compact would give back -- and on
// a router its tenants, the node each is on and its size.
//
// The page shows this view only when `/_whoami` says the token is the
// server's own; the server decides again on every request, and a section
// whose request is refused says so rather than showing nothing.

import { h, fill, bytes, number } from './dom.js';
import { call, fenecql, ms } from './kit.js';

/** How often the numbers are read again while the view is open. */
const EVERY = 5_000;
const TOP = 12;

/** Prometheus's text read into samples: `{name, labels, value}`. */
export function prometheus(text) {
  const out = [];
  for (const line of String(text).split('\n')) {
    if (!line || line.startsWith('#')) continue;
    const m = /^([a-zA-Z_:][\w:]*)(?:\{(.*)\})?\s+(\S+)/.exec(line);
    if (!m) continue;
    const labels = {};
    for (const [, k, v] of (m[2] ?? '').matchAll(/(\w+)="((?:[^"\\]|\\.)*)"/g)) labels[k] = v.replace(/\\(.)/g, '$1');
    out.push({ name: m[1], labels, value: Number(m[3]) });
  }
  return out;
}

/** The sum of a family's samples whose labels hold `match`. */
export function total(samples, name, match = {}) {
  let n = 0;
  let any = false;
  for (const s of samples) {
    if (s.name !== name) continue;
    if (Object.entries(match).some(([k, v]) => s.labels[k] !== v)) continue;
    n += s.value;
    any = true;
  }
  return any ? n : null;
}

/**
 * A quantile of a histogram, in seconds: the bucket the rank falls in,
 * interpolated across it as Prometheus's `histogram_quantile` does.
 */
export function quantile(samples, name, q, match = {}) {
  const buckets = new Map();
  for (const s of samples) {
    if (s.name !== `${name}_bucket`) continue;
    if (Object.entries(match).some(([k, v]) => s.labels[k] !== v)) continue;
    const le = s.labels.le === '+Inf' ? Infinity : Number(s.labels.le);
    buckets.set(le, (buckets.get(le) ?? 0) + s.value);
  }
  const sorted = [...buckets].sort((a, b) => a[0] - b[0]);
  const count = sorted.at(-1)?.[1] ?? 0;
  if (!count) return null;
  const rank = q * count;
  let lo = 0;
  let below = 0;
  for (const [le, n] of sorted) {
    if (n >= rank) {
      if (le === Infinity) return lo;
      return lo + (le - lo) * ((rank - below) / Math.max(1, n - below));
    }
    lo = le;
    below = n;
  }
  return lo;
}

export function mount(host, ctx) {
  const updated = h('span', { class: 'ad-when' });
  const refresh = h('button', { type: 'button', class: 'btn ghost', onclick: () => read() }, 'Read again');
  const order = { by: 'time' };
  const byTime = h('button', { type: 'button', class: 'seg', 'aria-pressed': 'true', onclick: () => sortBy('time') }, 'Slowest');
  const byCalls = h('button', { type: 'button', class: 'seg', 'aria-pressed': 'false', onclick: () => sortBy('calls') }, 'Most run');
  const statements = h('div', { class: 'ad-statements' });
  const numbers = h('div', { class: 'ad-numbers' });
  const tenants = h('section', { class: 'ad-section', hidden: true, 'aria-label': 'Tenants' });
  fill(
    host,
    h(
      'div',
      { class: 'ad' },
      h('div', { class: 'sc-bar' }, h('div', { class: 'view-head' }, h('h1', { class: 'view-title' }, 'Admin'), updated), refresh),
      h('p', { class: 'sc-note' }, 'The server’s own counts, read with your token every few seconds while this view is open.'),
      h(
        'div',
        { class: 'ad-grid' },
        h(
          'section',
          { class: 'ad-section', 'aria-label': 'Statements' },
          h('div', { class: 'sc-head' }, h('h2', {}, 'Statements by shape'), h('div', { class: 'segs', role: 'group', 'aria-label': 'Order' }, byTime, byCalls)),
          statements,
        ),
        h('section', { class: 'ad-section', 'aria-label': 'Server' }, h('h2', {}, 'Server'), numbers),
      ),
      tenants,
    ),
  );

  let shapes = null;
  let before = null;
  let timer = null;
  let reading = false;

  function sortBy(by) {
    order.by = by;
    byTime.setAttribute('aria-pressed', String(by === 'time'));
    byCalls.setAttribute('aria-pressed', String(by === 'calls'));
    drawStatements();
  }

  async function read() {
    if (reading) return;
    reading = true;
    try {
      const router = ctx.state.mode === 'router';
      const [st, me, tn] = await Promise.all([
        call(ctx, '/_stats/statements').catch((e) => ({ ok: false, status: 0, text: e.message })),
        call(ctx, '/_metrics', { root: ctx.state.mode !== 'single' }).catch((e) => ({ ok: false, status: 0, text: e.message })),
        router ? call(ctx, '/_shard/tenants?bytes', { root: true }).catch(() => null) : null,
      ]);
      shapes = st.ok ? st.json?.statements ?? [] : { refused: st };
      drawStatements();
      drawNumbers(me);
      drawTenants(tn);
      fill(updated, `read at ${new Date().toLocaleTimeString([], { hour12: false })}`);
    } finally {
      reading = false;
    }
  }

  function drawStatements() {
    if (!shapes) return;
    if (!Array.isArray(shapes)) {
      fill(statements, h('p', { class: 'hint' }, `The server did not answer /_stats/statements to this token (HTTP ${shapes.refused.status}).`));
      return;
    }
    const key = order.by === 'time' ? (s) => s.mean_ms : (s) => s.calls;
    const top = [...shapes].sort((a, b) => key(b) - key(a)).slice(0, TOP);
    if (!top.length) {
      fill(statements, h('p', { class: 'hint' }, 'No statement has run since the server started, or since the counts were reset.'));
      return;
    }
    fill(
      statements,
      h(
        'table',
        { class: 'ad-table' },
        h(
          'thead',
          {},
          h('tr', {}, h('th', {}, 'Shape'), h('th', { class: 'num' }, 'Calls'), h('th', { class: 'num' }, 'Mean'), h('th', { class: 'num' }, 'Max'), h('th', { class: 'num' }, 'Rows'), h('th', { class: 'num' }, 'Errors')),
        ),
        h(
          'tbody',
          {},
          top.map((s) => {
            // A shape over HTTP's paths (`GET /orders`) is no statement to run.
            const runnable = !/^[A-Z]+ \//.test(s.query);
            return h(
              'tr',
              {},
              h(
                'td',
                { class: 'ad-shape' },
                runnable
                  ? h('button', { type: 'button', class: 'ad-open', title: 'Open in the query editor', onclick: () => ctx.query(s.query) }, fenecql(s.query))
                  : h('span', { class: 'mono' }, s.query),
                s.tenant ? h('span', { class: 'ad-tenant' }, s.tenant) : null,
              ),
              h('td', { class: 'num' }, number(s.calls)),
              h('td', { class: 'num' }, ms(s.mean_ms)),
              h('td', { class: 'num' }, ms(s.max_ms)),
              h('td', { class: 'num' }, number(s.rows)),
              h('td', { class: `num${s.errors ? ' bad' : ''}` }, number(s.errors)),
            );
          }),
        ),
      ),
    );
  }

  function drawNumbers(r) {
    if (!r?.ok) {
      fill(numbers, h('p', { class: 'hint' }, `The server did not answer /_metrics to this token (HTTP ${r?.status ?? '–'}).`));
      return;
    }
    const m = prometheus(r.text);
    const now = Date.now();
    const rate = (name, value) => {
      const was = before?.values[name];
      if (was === undefined || value === null) return null;
      return (value - was) / ((now - before.at) / 1000);
    };
    const values = {};
    const rows = [];
    const add = (label, value, extra = null, hint = null) => rows.push([label, value, extra, hint]);
    const per = (name, value) => {
      values[name] = value;
      const r = rate(name, value);
      return r === null ? null : `${r.toFixed(r < 10 ? 1 : 0)}/s`;
    };
    const p = (q, family, match) => {
      const s = quantile(m, family, q, match);
      return s === null ? '–' : ms(s * 1000);
    };
    if (total(m, 'fenec_router_requests_total') !== null) {
      const req = total(m, 'fenec_router_requests_total');
      add('Requests', number(req), per('req', req));
      add('Latency', `p50 ${p(0.5, 'fenec_router_request_duration_seconds', {})}`, `p99 ${p(0.99, 'fenec_router_request_duration_seconds', {})}`);
      add('Upstream errors', number(total(m, 'fenec_router_upstream_errors_total') ?? 0));
      add('Nodes', number(total(m, 'fenec_router_nodes') ?? 0));
      add('Moves', number(total(m, 'fenec_router_moves_total') ?? 0));
    } else {
      const reads = total(m, 'fenec_statements_total', { kind: 'read' }) ?? 0;
      const writes = total(m, 'fenec_statements_total', { kind: 'write' }) ?? 0;
      const errors = total(m, 'fenec_statement_errors_total') ?? 0;
      add('Reads', number(reads), per('reads', reads));
      add('Writes', number(writes), per('writes', writes));
      add('Errors', number(errors), per('errors', errors));
      add('Read latency', `p50 ${p(0.5, 'fenec_statement_duration_seconds', { kind: 'read' })}`, `p99 ${p(0.99, 'fenec_statement_duration_seconds', { kind: 'read' })}`);
      add('Write latency', `p50 ${p(0.5, 'fenec_statement_duration_seconds', { kind: 'write' })}`, `p99 ${p(0.99, 'fenec_statement_duration_seconds', { kind: 'write' })}`);
      const file = total(m, 'fenec_file_bytes') ?? total(m, 'fenec_tenants_disk_bytes');
      if (file !== null) add('File', bytes(file), null, total(m, 'fenec_tenants') !== null ? `${number(total(m, 'fenec_tenants'))} tenants` : null);
      const dead = total(m, 'fenec_reclaimable_bytes');
      if (dead !== null) add('Reclaimable', bytes(dead), null, 'what a compact gives back');
      const mem = total(m, 'fenec_memory_bytes');
      if (mem !== null) add('Memory', bytes(mem));
      const docs = total(m, 'fenec_documents');
      if (docs !== null) add('Rows', number(docs));
      add('Compactions', number(total(m, 'fenec_auto_compactions_total') ?? 0), null, 'run on their own');
      const conns = total(m, 'fenec_connections');
      if (conns !== null) add('Connections', number(conns));
    }
    const started = total(m, 'fenec_start_time_seconds');
    if (started) add('Up', since(now / 1000 - started));
    before = { at: now, values };
    fill(
      numbers,
      h(
        'table',
        { class: 'ad-facts' },
        h(
          'tbody',
          {},
          rows.map(([label, value, extra, hint]) =>
            h(
              'tr',
              {},
              h('th', { scope: 'row' }, label),
              h('td', {}, h('span', { class: 'ad-value' }, value), extra ? h('span', { class: 'ad-extra' }, extra) : null, hint ? h('span', { class: 'ad-hint' }, hint) : null),
            ),
          ),
        ),
      ),
    );
  }

  function drawTenants(r) {
    if (!r) {
      tenants.hidden = true;
      return;
    }
    tenants.hidden = false;
    if (!r.ok || !Array.isArray(r.json)) {
      fill(tenants, h('h2', {}, 'Tenants'), h('p', { class: 'hint' }, `The router did not list its tenants to this token (HTTP ${r.status}): it takes the router's --token.`));
      return;
    }
    fill(
      tenants,
      h('h2', {}, 'Tenants'),
      h(
        'table',
        { class: 'ad-table ad-tenants' },
        h('thead', {}, h('tr', {}, h('th', {}, 'Tenant'), h('th', {}, 'Node'), h('th', {}, 'State'), h('th', { class: 'num' }, 'Size'), h('th', {}, 'Replica'))),
        h(
          'tbody',
          {},
          r.json.map((t) =>
            h(
              'tr',
              { dataset: { tenant: t.name } },
              h('td', { class: 'mono' }, t.name),
              h('td', { class: 'mono' }, t.node),
              h('td', { class: t.state === 'moving' ? 'sun' : 'muted' }, t.state),
              h('td', { class: 'num' }, typeof t.bytes === 'number' ? bytes(t.bytes) : '–'),
              h('td', { class: 'mono muted' }, t.replica ?? ''),
            ),
          ),
        ),
      ),
    );
  }

  function since(s) {
    if (s < 120) return `${Math.round(s)} s`;
    if (s < 7200) return `${Math.round(s / 60)} min`;
    if (s < 172800) return `${Math.round(s / 3600)} h`;
    return `${Math.round(s / 86400)} days`;
  }

  const stopTimer = () => {
    clearInterval(timer);
    timer = null;
  };

  return {
    open() {
      read();
      stopTimer();
      timer = setInterval(() => !document.hidden && read(), EVERY);
    },
    focus: () => refresh.focus(),
    hide: stopTimer,
    close: stopTimer,
  };
}
