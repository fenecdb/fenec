// What the views opened after the rows share: a request as the server
// answers it -- status, time, `Fenec-Seq`, `X-Request-Id` -- and a statement
// drawn in the docs' colours with text nodes alone. Fetched with the first
// of them, never on the first load.

import { h } from './dom.js';
import { tokens } from './highlight.js';

/**
 * One request with the page's token, to the database the studio has open
 * (`base`) or, `root: true`, to the server itself -- a router's own paths.
 * Answers `{status, ok, ms, json, text, seq, requestId}`; the network
 * failing throws, a refusal does not.
 */
export async function call(ctx, path, { method = 'GET', body = null, type = 'application/json', root = false } = {}) {
  const headers = {};
  if (ctx.state.token) headers.authorization = `Bearer ${ctx.state.token}`;
  if (body !== null) headers['content-type'] = type;
  const started = performance.now();
  const res = await fetch(`${root ? ctx.state.server : ctx.state.base}${path}`, { method, headers, body });
  const text = await res.text();
  const ms = performance.now() - started;
  let json = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    /* Prometheus's text, or a proxy's page */
  }
  const seq = res.headers.get('fenec-seq');
  return { status: res.status, ok: res.ok, ms, json, text, seq: seq === null ? null : Number(seq), requestId: res.headers.get('x-request-id') };
}

/** A refused answer as the error `explain` words: its status, and a batch's `at`. */
export function refused(r) {
  const e = new Error(r.json?.error ?? (r.text.slice(0, 200) || `HTTP ${r.status}`));
  e.status = r.status;
  if (typeof r.json?.at === 'number') e.at = r.json.at;
  if (typeof r.json?.completed === 'number') e.completed = r.json.completed;
  return e;
}

/** FenecQL in the docs' colours: a `<code>` of spans and text nodes. */
export function fenecql(text, cls = 'q') {
  const el = h('code', { class: cls });
  for (const [kind, part] of tokens(text, 'fenecql')) {
    el.append(kind ? h('span', { class: kind }, part) : part);
  }
  return el;
}

/** Statements, one a line, highlighted, in a `<pre>`. */
export function statementsBlock(lines, cls = 'statement q-block') {
  const pre = h('pre', { class: cls });
  lines.forEach((line, i) => {
    if (i) pre.append('\n');
    pre.append(fenecql(line, 'q'));
  });
  return pre;
}

/**
 * The columns of rows a statement answered, which no schema describes --
 * a `select` names its own, a `lookup` nests -- read off the rows: each
 * key in the order it is first met, `id` first, each typed by its values
 * as the grid draws a type (`int`, `float`, `text`, `bool`, `vector<N>`,
 * `json`), so a list of numbers shows as a vector and a number keeps to
 * the right. The first 500 rows are enough to name them.
 */
export function columnsOf(rows) {
  const seen = new Map();
  for (const row of rows.slice(0, 500)) {
    if (!row || typeof row !== 'object' || Array.isArray(row)) continue;
    for (const [k, v] of Object.entries(row)) {
      if (!seen.has(k)) seen.set(k, new Set());
      if (v !== null && v !== undefined) seen.get(k).add(typeOf(v));
    }
  }
  const cols = [...seen].map(([name, types]) => ({ name, type: types.size === 1 ? [...types][0] : types.size === 0 ? 'text' : merge(types) }));
  const id = cols.findIndex((c) => c.name === 'id');
  if (id > 0) cols.unshift(...cols.splice(id, 1));
  return cols;
}

function typeOf(v) {
  if (typeof v === 'boolean') return 'bool';
  if (typeof v === 'number') return Number.isInteger(v) ? 'int' : 'float';
  if (typeof v === 'string') return 'text';
  if (Array.isArray(v) && v.length > 3 && v.every((x) => typeof x === 'number')) return `vector<${v.length}>`;
  return 'json';
}

/** Ints and floats in one column are floats; anything else mixed is JSON. */
function merge(types) {
  return [...types].every((t) => t === 'int' || t === 'float') ? 'float' : 'json';
}

/** A duration as a person reads it: 0.42 ms, 12 ms, 1.4 s. */
export function ms(n) {
  if (!Number.isFinite(n)) return '–';
  if (n < 1) return `${n.toFixed(2)} ms`;
  if (n < 100) return `${n.toFixed(1)} ms`;
  if (n < 10_000) return `${Math.round(n)} ms`;
  return `${(n / 1000).toFixed(1)} s`;
}

/** A modal dialog with a title; `close()` removes it. */
export function dialog(title, ...body) {
  const d = h('dialog', { class: 'dialog', 'aria-label': title }, h('h2', { class: 'dialog-title' }, title), ...body);
  document.body.append(d);
  d.addEventListener('close', () => d.remove());
  d.showModal();
  return d;
}

/** `localStorage`, read and written in a try: a private window, or a site's data blocked, keeps nothing and says nothing. */
export const kept = {
  get(key, fallback) {
    try {
      const v = JSON.parse(localStorage.getItem(key) ?? 'null');
      return v ?? fallback;
    } catch {
      return fallback;
    }
  },
  set(key, value) {
    try {
      localStorage.setItem(key, JSON.stringify(value));
      return true;
    } catch {
      return false;
    }
  },
};
