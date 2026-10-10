// fenec studio: a browser's view of a fenecdb server -- its collections,
// their rows, a row's whole value -- and the writes a person makes there.
//
// It holds no authority of its own. Every read and write is a statement
// sent over the server's HTTP surface with the token pasted in, as any
// client sends one: a scoped token's filter is ANDed into each of them on
// the server, so the grid, the counts and the facets hold that token's
// rows and no others. Every value goes as a parameter (`statements.js`),
// every write through `/batch` with an `Idempotency-Key`, and every value
// from the database reaches the page as text (`dom.js`).

import { h, fill, mark, number } from './dom.js';
import * as S from './statements.js';
import { Grid, sortable } from './grid.js';
import { Sidebar } from './sidebar.js';
import { tree, readInput } from './values.js';
import { explain, confirmDelete, insertForm, help } from './edit.js';
import { saved, save, forget, here, claimsOf, probe, clientFor, whoamiAt, metrics, ConnectError } from './connect.js';

const root = document.getElementById('app');
const state = {
  server: null,
  token: '',
  tenant: null,
  mode: null,
  who: null,
  claims: null,
  tenants: null,
  db: null,
  base: null,
  collections: [],
  current: null,
  view: { where: '', params: '[]', quick: {}, order: null },
  total: 0,
};

// ------------------------------------------------------------------ theme

const THEME = 'fenec-studio-theme';

function theme(next) {
  // A viewer's convenience, so localStorage; the token never goes there.
  let t = next;
  try {
    if (next === undefined) t = localStorage.getItem(THEME);
    else if (next === null) localStorage.removeItem(THEME);
    else localStorage.setItem(THEME, next);
  } catch {
    /* kept for this page alone */
  }
  if (t === 'light' || t === 'dark') document.documentElement.dataset.theme = t;
  else delete document.documentElement.dataset.theme;
  return t ?? null;
}
theme();

const dark = () =>
  document.documentElement.dataset.theme === 'dark' ||
  (!document.documentElement.dataset.theme && matchMedia('(prefers-color-scheme: dark)').matches);

// ------------------------------------------------------------------ toasts

let toasts;
function say(text, kind = 'info') {
  if (!toasts) return;
  const t = h('div', { class: `toast ${kind}`, role: kind === 'error' ? 'alert' : 'status' }, text);
  toasts.append(t);
  setTimeout(() => t.remove(), kind === 'error' ? 9000 : 3500);
}

// ------------------------------------------------------------------ sign in

function signIn(problem = '') {
  document.title = 'Sign in · fenec studio';
  const last = saved();
  const server = h('input', { id: 'server', name: 'server', type: 'url', value: last?.server ?? here(), required: true, spellcheck: 'false', autocomplete: 'off' });
  const token = h('input', { id: 'token', name: 'token', type: 'password', value: '', autocomplete: 'off', spellcheck: 'false' });
  const tenant = h('input', { id: 'tenant', name: 'tenant', value: last?.tenant ?? '', spellcheck: 'false', autocomplete: 'off', placeholder: 'acme' });
  const error = h('p', { class: 'form-error', role: 'alert' }, problem);
  const go = h('button', { type: 'submit', class: 'btn primary wide' }, 'Connect');
  const form = h(
    'form',
    {
      class: 'signin',
      onsubmit: async (e) => {
        e.preventDefault();
        go.disabled = true;
        fill(error);
        try {
          await connectTo(server.value.replace(/\/+$/, ''), token.value.trim(), tenant.value.trim() || null);
        } catch (err) {
          fill(error, err instanceof ConnectError ? err.message : explain(err));
          go.disabled = false;
        }
      },
    },
    h('div', { class: 'signin-brand' }, mark('mark big'), h('span', {}, 'fenec studio')),
    h('h1', {}, 'Connect to a fenecdb server'),
    h('label', { for: 'server' }, 'Server'),
    server,
    h('label', { for: 'token' }, 'Token'),
    token,
    h('p', { class: 'hint' }, 'The server token, or a JSON Web Token its policy holds to its rows. Leave it empty for a server that asks for none.'),
    h('label', { for: 'tenant' }, 'Tenant ', h('span', { class: 'optional' }, 'if your token names none')),
    tenant,
    error,
    go,
    h('p', { class: 'hint small' }, 'Your token stays in this tab: it is kept in session storage, sent only to this server, and forgotten when the tab closes.'),
  );
  fill(root, h('main', { class: 'signin-page' }, form));
  token.focus();
}

async function connectTo(server, token, tenant) {
  const found = await probe(server, token);
  Object.assign(state, { server, token, mode: found.mode, tenants: found.tenants ?? null, claims: claimsOf(token) });
  let pick = tenant;
  if (found.mode !== 'single') {
    if (!pick && found.tenants?.length) pick = found.tenants[0];
    if (!pick) throw new ConnectError('This server serves tenants: name one to open.');
  } else pick = null;
  await openTenant(pick, found.who);
  save({ server, token, tenant: pick });
}

async function openTenant(tenant, who) {
  const { base, db } = clientFor(state.server, state.token, tenant);
  state.who = who && !tenant ? who : await whoamiAt(base, state.token);
  Object.assign(state, { tenant, base, db });
  db.onError = (e) => say(explain(e), 'error');
  layout();
  await loadCollections();
}

// ------------------------------------------------------------------ what the token may do

function rulesFor(collection) {
  return (state.who?.rules ?? []).filter((r) => r.collection === '*' || r.collection === collection);
}

/** Whether the token may `op` (read, insert, update, delete) in `collection`. */
function may(op, collection) {
  if (state.who?.read_only && op !== 'read') return false;
  if (state.who?.kind !== 'scoped') return true;
  if ((op === 'update' || op === 'delete') && (state.who.append_only ?? []).some((a) => a === '*' || a === collection)) return false;
  return rulesFor(collection).some((r) => r.grants.includes(op));
}

// ------------------------------------------------------------------ the frame

let grid;
let sidebar;
let parts;

function layout() {
  toasts = h('div', { class: 'toasts', 'aria-live': 'polite' });
  const where = h('input', {
    class: 'where',
    id: 'where',
    placeholder: 'year >= $1 and tags has "rust"',
    spellcheck: 'false',
    autocomplete: 'off',
    'aria-label': 'Where clause, FenecQL',
  });
  const params = h('input', { class: 'params', id: 'params', value: '[]', spellcheck: 'false', autocomplete: 'off', 'aria-label': 'Parameters, a JSON list' });
  const whereForm = h(
    'form',
    {
      class: 'where-form',
      onsubmit: (e) => {
        e.preventDefault();
        state.view.where = where.value;
        state.view.params = params.value;
        reload();
      },
    },
    h('label', { class: 'where-label', for: 'where' }, 'where'),
    where,
    h('label', { class: 'where-label', for: 'params' }, 'with'),
    params,
    h('button', { type: 'submit', class: 'btn' }, 'Run'),
  );
  const title = h('h1', { class: 'view-title' });
  const total = h('span', { class: 'view-count' });
  const newRow = h('button', { type: 'button', class: 'btn', onclick: () => insert() }, 'New row');
  const refresh = h('button', { type: 'button', class: 'btn ghost', onclick: () => reload(), title: 'Read the rows again (R)' }, 'Reload');
  const facets = h('div', { class: 'facets', 'aria-label': 'Counts by value' });
  const gridHost = h('div', { class: 'grid-host' });
  const status = h('div', { class: 'status', 'aria-live': 'polite' });
  const inspector = h('aside', { class: 'inspector', 'aria-label': 'Row', hidden: true });
  const sideHost = h('div', { class: 'side-host' });
  const whoBtn = h('button', { type: 'button', class: 'who', onclick: () => identity() });
  const horizon = h('div', { class: 'horizon-fill' });
  const tenantPick = tenantControl();
  const menu = h('button', {
    type: 'button',
    class: 'btn ghost side-toggle',
    'aria-label': 'Collections',
    'aria-expanded': 'false',
    onclick: () => {
      const open = !document.body.classList.contains('side-open');
      document.body.classList.toggle('side-open', open);
      menu.setAttribute('aria-expanded', String(open));
      if (open) sidebar.focus();
    },
  }, 'Collections');
  const themeBtn = h('button', {
    type: 'button',
    class: 'btn ghost',
    onclick: () => {
      theme(dark() ? 'light' : 'dark');
      themeBtn.textContent = dark() ? 'Light' : 'Dark';
    },
    title: 'Switch the theme',
  }, dark() ? 'Light' : 'Dark');
  const out = h('button', { type: 'button', class: 'btn ghost', onclick: () => signOut() }, 'Sign out');
  const tabs = h(
    'nav',
    { class: 'views', 'aria-label': 'Views' },
    VIEWS.filter(([v]) => v !== 'admin' || adminAllowed()).map(([v, label, key]) =>
      h('button', { type: 'button', class: 'view-tab', dataset: { view: v }, title: `${label} (${key})`, onclick: () => showView(v) }, label),
    ),
  );
  const top = h(
    'header',
    { class: 'top' },
    menu,
    h('div', { class: 'brand' }, mark(), h('span', { class: 'brand-name' }, 'fenec studio')),
    h('div', { class: 'where-at' }, h('span', { class: 'server-host', title: state.server }, new URL(state.server).host), tenantPick),
    tabs,
    h('div', { class: 'top-end' }, whoBtn, themeBtn, out),
  );
  const views = h('div', { class: 'view-host' });
  const main = h(
    'main',
    { class: 'main', id: 'main' },
    h('div', { class: 'toolbar' }, h('div', { class: 'view-head' }, title, total), whereForm, h('div', { class: 'actions' }, refresh, newRow)),
    facets,
    gridHost,
    status,
  );
  fill(root, h('div', { class: 'app' }, top, h('div', { class: 'horizon', 'aria-hidden': 'true' }, horizon), h('div', { class: 'body', dataset: { view: 'rows' } }, sideHost, main, views, inspector), toasts));
  parts = { where, params, title, total, newRow, facets, status, inspector, whoBtn, horizon, tabs, views };
  closeViews();
  view = 'rows';
  sidebar = new Sidebar(sideHost, (name) => {
    document.body.classList.remove('side-open');
    pick(name);
  });
  grid = new Grid(gridHost, {
    onSort: (field) => sortBy(field),
    onQuick: (field, text) => {
      state.view.quick[field] = text;
      reload();
    },
    onEdit: (row, field, text) => writeCell(row, field, text),
    onDelete: (row) => remove(row),
    onCopy: (row) => copy(row),
    onInspect: (row) => {
      parts.inspector.hidden = !parts.inspector.hidden;
      inspect(row);
    },
    onActive: (row) => inspect(row),
    onError: (err) => say(explain(err), 'error'),
  });
  tick();
}

function tenantControl() {
  if (state.mode === 'single') return null;
  const go = (t) => openTenant(t).then(() => save({ server: state.server, token: state.token, tenant: t })).catch((e) => say(explain(e), 'error'));
  if (state.tenants?.length) {
    const sel = h(
      'select',
      { class: 'tenant', 'aria-label': 'Tenant', onchange: () => go(sel.value) },
      state.tenants.map((t) => h('option', { value: t, selected: t === state.tenant }, t)),
    );
    return h('span', { class: 'tenant-pick' }, h('span', { class: 'sep', 'aria-hidden': 'true' }, '/'), sel);
  }
  const input = h('input', { class: 'tenant', value: state.tenant ?? '', 'aria-label': 'Tenant', spellcheck: 'false', autocomplete: 'off' });
  input.addEventListener('change', () => input.value.trim() && go(input.value.trim()));
  return h('span', { class: 'tenant-pick' }, h('span', { class: 'sep', 'aria-hidden': 'true' }, '/'), input);
}

// ------------------------------------------------------------------ identity

function identityLine() {
  const w = state.who;
  const c = state.claims ?? {};
  if (w?.kind === 'open') return 'Open server, no token';
  if (w?.kind === 'full') return 'Server token';
  const role = Array.isArray(c.role) ? c.role.join(', ') : c.role;
  return [w?.sub ?? c.sub ?? 'Token with no subject', role ? `as ${role}` : null].filter(Boolean).join(' ');
}

function left() {
  const exp = state.claims?.exp;
  return typeof exp === 'number' ? exp - Date.now() / 1000 : null;
}

function clock(s) {
  if (s <= 0) return 'expired';
  if (s < 600) return `expires in ${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, '0')}`;
  if (s < 5400) return `expires in ${Math.round(s / 60)} min`;
  if (s < 172800) return `expires in ${Math.round(s / 3600)} h`;
  return `expires in ${Math.round(s / 86400)} days`;
}

let warned = false;
let ticking;
function tick() {
  clearInterval(ticking);
  const paint = () => {
    if (!parts) return;
    const s = left();
    const line = identityLine();
    fill(parts.whoBtn, h('span', { class: 'who-name' }, line), s === null ? null : h('span', { class: 'who-exp' }, clock(s)));
    parts.whoBtn.classList.toggle('warn', s !== null && s < 300 && s > 0);
    parts.whoBtn.classList.toggle('dead', s !== null && s <= 0);
    parts.whoBtn.title = 'Who the server takes this token for';
    // The horizon: what is left of the token's life, from its `iat` (or
    // an hour before its `exp`) to its `exp`, drawn as a line that shortens.
    // A token with no exp has no horizon to draw.
    if (s === null) parts.horizon.style.transform = 'scaleX(0)';
    else {
      const iat = typeof state.claims.iat === 'number' ? state.claims.iat : state.claims.exp - 3600;
      const span = Math.max(1, state.claims.exp - iat);
      parts.horizon.style.transform = `scaleX(${Math.max(0, Math.min(1, s / span))})`;
    }
    parts.horizon.parentElement.classList.toggle('warn', s !== null && s < 300);
    if (s !== null && s < 300 && s > 0 && !warned) {
      warned = true;
      say(`Your token expires in ${Math.ceil(s / 60)} min. Sign in with a new one to keep working.`, 'warn');
    }
    if (s !== null && s <= 0 && warned !== 'dead') {
      warned = 'dead';
      say('Your token has expired: the server refuses it now. Sign in with a new one.', 'error');
    }
  };
  paint();
  ticking = setInterval(paint, 1000);
}

function identity() {
  const w = state.who ?? {};
  const c = state.claims ?? {};
  const s = left();
  const rows = [
    ['Access', { open: 'Everything: the server asks for no token', full: "Everything: the server's own token", scoped: 'What its policy rules allow, below' }[w.kind] ?? '–'],
    ['Subject', w.sub ?? c.sub ?? '–'],
    ['Role', c.role === undefined ? '–' : [c.role].flat().join(', ')],
    ['Tenant', state.tenant ?? (w.tenants ? w.tenants.join(', ') : '–')],
    ['Expires', typeof c.exp === 'number' ? `${new Date(c.exp * 1000).toLocaleString()}, ${clock(s)}` : 'never'],
  ];
  const rules = (w.rules ?? []).map((r) =>
    h(
      'tr',
      {},
      h('td', {}, r.collection),
      h('td', {}, r.grants.join(', '), r.fields ? ` (${r.fields.join(', ')})` : ''),
      h('td', { class: 'mono' }, r.rows ?? 'every row'),
    ),
  );
  const d = h(
    'dialog',
    { class: 'dialog', 'aria-label': 'Your token' },
    h('h2', { class: 'dialog-title' }, 'Your token'),
    h('dl', { class: 'facts' }, rows.map(([k, v]) => [h('dt', {}, k), h('dd', {}, v)])),
    w.kind === 'scoped'
      ? [
          h('h3', {}, 'Rules that apply to it'),
          rules.length
            ? h('table', { class: 'rules' }, h('thead', {}, h('tr', {}, h('th', {}, 'Collection'), h('th', {}, 'May'), h('th', {}, 'Rows'))), h('tbody', {}, rules))
            : h('p', {}, 'None: this token reads and writes nothing.'),
          w.append_only?.length ? h('p', { class: 'hint' }, `Append-only: ${w.append_only.join(', ')}. No update or delete there.`) : null,
        ]
      : null,
    h('div', { class: 'dialog-actions' }, h('button', { type: 'button', class: 'btn', onclick: () => d.close() }, 'Close')),
  );
  document.body.append(d);
  d.addEventListener('close', () => d.remove());
  d.showModal();
}

/** Every view opened let go of: a live view's stream ends with it. */
function closeViews() {
  for (const m of Object.values(mounted)) m.view?.close?.();
  mounted = {};
}

function signOut() {
  closeViews();
  forget();
  clearInterval(ticking);
  Object.assign(state, { token: '', who: null, claims: null, db: null });
  parts = null;
  warned = false;
  signIn();
}

// ------------------------------------------------------------------ collections

/** The collections the token reads, their counts and the file's sizes, into the sidebar. */
async function listCollections() {
  const db = state.db;
  const list = await db.run('collections');
  state.collections = (list.rows ?? list).filter((c) => c && typeof c.name === 'string');
  let counts = {};
  const countable = state.collections.filter((c) => S.writable(c.name));
  if (countable.length) {
    // One /batch of reads: one round trip, under one read lock, so the
    // counts are of one moment.
    try {
      const got = await db.batch(countable.map((c) => S.count(c.name, S.filter())).map((s) => [s.text, s.params]));
      countable.forEach((c, i) => (counts[c.name] = got.results[i]?.rows?.[0]?.count ?? null));
    } catch {
      for (const c of countable) counts[c.name] = null;
    }
  }
  const full = state.who?.kind !== 'scoped';
  const stats = full && state.mode === 'single' ? await metrics(state.base, state.token) : null;
  sidebar.show({ collections: state.collections, counts, stats });
  if (state.current) {
    state.current = state.collections.find((c) => c.name === state.current.name) ?? null;
    if (state.current) sidebar.select(state.current.name);
  }
}

async function loadCollections() {
  state.current = null;
  await listCollections();
  const hash = new URLSearchParams(location.hash.slice(1));
  const wanted = hash.get('c');
  const first = state.collections.find((c) => c.name === wanted) ?? state.collections[0];
  const v = VIEWS.some(([x]) => x === hash.get('v')) ? hash.get('v') : 'rows';
  if (first) {
    if (v === 'rows') await openCollection(first.name);
    else {
      choose(first.name);
      await showView(v);
    }
  } else {
    fill(parts.title, 'No collections');
    fill(parts.status, 'This token reads no collection here.');
    if (v !== 'rows') await showView(v);
  }
}

/** `name` the current collection: the sidebar, the title and the address say so. */
function choose(name) {
  const c = state.collections.find((x) => x.name === name);
  if (!c) return null;
  state.current = c;
  document.title = `${c.name} · fenec studio`;
  sidebar.select(name);
  remember();
  return c;
}

/** The view and the collection in the address, so a reload comes back to them. */
function remember() {
  const q = new URLSearchParams();
  if (view !== 'rows') q.set('v', view);
  if (state.current) q.set('c', state.current.name);
  history.replaceState(null, '', `#${q}`);
}

/** A collection picked in the sidebar: its rows, or the view open now shows it. */
function pick(name) {
  if (view === 'rows') return openCollection(name);
  if (!choose(name)) return;
  mounted[view]?.view?.open?.(name, { picked: true });
}

let shownRows = null;
async function openCollection(name) {
  const c = choose(name);
  if (!c) return;
  shownRows = name;
  state.view = { where: '', params: '[]', quick: {}, order: null };
  parts.where.value = '';
  parts.params.value = '[]';
  parts.newRow.hidden = !may('insert', c.name);
  fill(parts.title, c.name);
  await reload();
  grid.el.focus({ preventScroll: true });
}

// ------------------------------------------------------------------ views
//
// The rows are the first load. The query editor, the schema, the live view
// and the admin page are modules of their own, fetched the first time each
// is opened, with their stylesheet: a person who only browses rows never
// downloads them. Each is `mount(host, ctx)`, answering `{open(name,
// {picked}), close()}`; it reads and writes through `ctx`, with the token
// the rows use and no other.

const VIEWS = [
  ['rows', 'Rows', 1],
  ['query', 'Query', 2],
  ['schema', 'Schema', 3],
  ['live', 'Live', 4],
  ['admin', 'Admin', 5],
];
const LOAD = {
  query: () => import('./editor.js'),
  schema: () => import('./schema.js'),
  live: () => import('./live.js'),
  admin: () => import('./admin.js'),
};
let view = 'rows';
let mounted = {};
let styled = null;

/** Whether the admin view is shown: to the server's own token, or a server with none. */
function adminAllowed() {
  return state.who?.kind === 'full' || state.who?.kind === 'open';
}

/** The views' stylesheet, linked once, resolved once it applies. */
function styles() {
  styled ??= new Promise((resolve, reject) => {
    const link = h('link', { rel: 'stylesheet', href: 'views.css' });
    link.addEventListener('load', resolve);
    link.addEventListener('error', () => {
      styled = null;
      link.remove();
      reject(new Error('the views’ stylesheet did not load'));
    });
    document.head.append(link);
  });
  return styled;
}

const ctx = {
  state,
  say: (text, kind) => say(text, kind),
  explain: (e) => explain(e),
  may: (op, c) => may(op, c),
  fieldsOf: (c) => fieldsOf(c),
  /** The collections listed again, after a schema change. */
  refresh: () => listCollections(),
  /** The query editor opened on `text`. */
  query: async (text) => {
    await showView('query');
    mounted.query?.view?.load?.(text);
  },
};

async function showView(name) {
  if (!VIEWS.some(([v]) => v === name) || (name === 'admin' && !adminAllowed())) name = 'rows';
  const was = view;
  view = name;
  for (const b of parts.tabs.children) {
    if (b.dataset.view === name) b.setAttribute('aria-current', 'page');
    else b.removeAttribute('aria-current');
  }
  root.querySelector('.body').dataset.view = name;
  for (const [k, m] of Object.entries(mounted)) m.host.hidden = k !== name;
  if (was !== name) mounted[was]?.view?.hide?.();
  remember();
  if (name === 'rows') {
    if (state.current && shownRows !== state.current.name) await openCollection(state.current.name);
    else grid.el.focus({ preventScroll: true });
    return;
  }
  if (!mounted[name]) {
    const label = VIEWS.find(([v]) => v === name)[1];
    const host = h('section', { class: 'view', 'aria-label': label }, h('p', { class: 'view-loading' }, `Opening ${label.toLowerCase()}…`));
    parts.views.append(host);
    mounted[name] = { host, view: null, ready: Promise.all([styles(), LOAD[name]()]).then(([, mod]) => mod.mount(host, ctx)) };
  }
  const m = mounted[name];
  m.host.hidden = false;
  try {
    m.view = await m.ready;
  } catch (e) {
    fill(m.host, h('p', { class: 'view-loading' }, `This view did not load: ${e.message}. Reload the page to try again.`));
    delete mounted[name];
    return;
  }
  if (view === name) m.view.open?.(state.current?.name ?? null, { picked: false });
}

function fieldsOf(c) {
  return [{ name: 'id', type: 'int', index: null }, ...c.fields];
}

/** The view's filter, as statements take it; a refusal is said and `null`. */
function currentFilter() {
  const c = state.current;
  let params;
  try {
    params = JSON.parse(state.view.params || '[]');
  } catch {
    throw new S.StatementError('the parameters are a JSON list: [2024, "rust"]');
  }
  const quick = fieldsOf(c)
    .filter((f) => (state.view.quick[f.name] ?? '').trim() !== '')
    .map((f) => ({ field: f.name, type: f.type, input: state.view.quick[f.name] }));
  return S.filter({ where: state.view.where, whereParams: params, quick });
}

let reading = 0;
async function reload() {
  const c = state.current;
  if (!c) return;
  const mine = ++reading;
  const db = state.db;
  let f;
  try {
    f = currentFilter();
  } catch (e) {
    say(explain(e), 'error');
    return;
  }
  const hashed = c.fields.filter((x) => /^hash\b/.test(x.index ?? '') && S.writable(x.name)).map((x) => x.name).slice(0, 4);
  const started = performance.now();
  let total;
  let facets = {};
  try {
    const items = [S.count(c.name, f)];
    if (hashed.length) items.push(S.facets(c.name, f, hashed, 6));
    const got = await db.batch(items.map((s) => [s.text, s.params]));
    total = got.results[0]?.rows?.[0]?.count ?? 0;
    facets = got.results[1]?.facets ?? {};
  } catch (e) {
    if (mine !== reading) return;
    say(explain(e), 'error');
    total = 0;
  }
  if (mine !== reading) return;
  const took = performance.now() - started;
  state.total = total;
  fill(parts.total, `${number(total)} ${total === 1 ? 'row' : 'rows'}`);
  showFacets(facets);
  const order = state.view.order;
  grid.show({
    fields: fieldsOf(c),
    total,
    order,
    quick: state.view.quick,
    readOnly: !may('update', c.name),
    source: async (block, size, after) => {
      const stmt = S.page(c.name, f, { limit: size, offset: block * size, after, order });
      const res = await db.run(stmt.text, stmt.params);
      return res.rows ?? [];
    },
  });
  const ord = order ? `, by ${order.field} ${order.desc ? 'descending' : 'ascending'}` : ', by id';
  fill(parts.status, `${number(total)} ${total === 1 ? 'row' : 'rows'}${ord}. Counted in ${took.toFixed(0)} ms.`, state.db.seq !== null ? ` Change ${state.db.seq}.` : '');
}

function showFacets(facets) {
  const c = state.current;
  const groups = Object.entries(facets).map(([field, values]) => {
    const active = state.view.quick[field] ?? '';
    return h(
      'div',
      { class: 'facet', role: 'group', 'aria-label': `${field} by value` },
      h('span', { class: 'facet-name' }, field),
      values.map((v) => {
        const want = v.value === null ? 'null' : `=${v.value}`;
        const on = active === want;
        return h(
          'button',
          {
            type: 'button',
            class: `chip${on ? ' on' : ''}`,
            'aria-pressed': on ? 'true' : 'false',
            onclick: () => {
              state.view.quick[field] = on ? '' : want;
              reload();
            },
          },
          h('span', { class: 'chip-value' }, v.value === null ? 'null' : String(v.value)),
          h('span', { class: 'chip-count' }, number(v.count)),
        );
      }),
    );
  });
  fill(parts.facets, groups);
  parts.facets.hidden = groups.length === 0;
}

function sortBy(field) {
  const o = state.view.order;
  const f = fieldsOf(state.current).find((x) => x.name === field);
  if (!f || !sortable(f)) return;
  // Ascending, descending, then back to id order.
  if (!o || o.field !== field) state.view.order = { field, desc: false };
  else if (!o.desc) state.view.order = { field, desc: true };
  else state.view.order = null;
  if (field === 'id' && state.view.order && !state.view.order.desc) state.view.order = null;
  reload();
}

// ------------------------------------------------------------------ the row

function inspect(row) {
  const ins = parts.inspector;
  if (ins.hidden) return;
  if (!row) {
    fill(ins, h('p', { class: 'hint' }, 'No row here yet.'));
    return;
  }
  const c = state.current;
  const del = may('delete', c.name);
  fill(
    ins,
    h(
      'div',
      { class: 'ins-head' },
      h('h2', {}, `Row ${row.id}`),
      h('button', { type: 'button', class: 'btn ghost', onclick: () => copy(row) }, 'Copy as JSON'),
      del ? h('button', { type: 'button', class: 'btn ghost danger', onclick: () => remove(row) }, 'Delete') : null,
      h('button', { type: 'button', class: 'btn ghost', 'aria-label': 'Close the row', onclick: () => ((ins.hidden = true), grid.el.focus()) }, 'Close'),
    ),
    h(
      'dl',
      { class: 'ins-fields' },
      fieldsOf(c).map((f) => [h('dt', {}, f.name, h('span', { class: 'field-type' }, f.type)), h('dd', {}, tree(row[f.name], f.type))]),
    ),
  );
}

async function copy(row) {
  try {
    await navigator.clipboard.writeText(JSON.stringify(row, null, 2));
    say(`Copied row ${row.id} as JSON.`);
  } catch {
    say('The browser did not allow the copy: select the text in the row panel instead.', 'error');
  }
}

/** One statement written through `/batch`, under a key of its own. */
async function write(stmt) {
  return state.db.batch([[stmt.text, stmt.params]], { idempotencyKey: crypto.randomUUID() });
}

async function writeCell(row, field, text) {
  const c = state.current;
  const f = c.fields.find((x) => x.name === field);
  let stmt;
  try {
    stmt = S.setCell(c.name, field, readInput(text, f.type, field), row.id);
  } catch (e) {
    say(explain(e), 'error');
    return null;
  }
  try {
    await write(stmt);
  } catch (e) {
    say(explain(e), 'error');
    return null;
  }
  // The row as the server holds it now: a json field's numbers as written,
  // a time as the server writes it.
  try {
    const again = S.rowById(c.name, row.id);
    const got = await state.db.run(again.text, again.params);
    say(`Wrote ${field} of row ${row.id}.`);
    return got.rows?.[0] ?? { ...row };
  } catch {
    return { ...row };
  }
}

async function remove(row) {
  const c = state.current;
  if (!may('delete', c.name)) {
    say(`Your token may not delete rows of ${c.name}.`, 'error');
    return;
  }
  const stmt = S.deleteRow(c.name, row.id);
  if (!(await confirmDelete(c.name, row, stmt))) {
    grid.el.focus();
    return;
  }
  try {
    await write(stmt);
    say(`Deleted row ${row.id}.`);
    await reload();
  } catch (e) {
    say(explain(e), 'error');
  }
  grid.el.focus();
}

function insert() {
  const c = state.current;
  if (!c || !may('insert', c.name)) return;
  insertForm(c.name, c.fields, async (stmt) => {
    try {
      await write(stmt);
    } catch (e) {
      return explain(e);
    }
    say(`Inserted a row into ${c.name}.`);
    reload();
    return null;
  });
}

// ------------------------------------------------------------------ keys

addEventListener('keydown', (e) => {
  if (!parts || document.querySelector('dialog[open]')) return;
  if (e.altKey && e.key === '1') {
    e.preventDefault();
    sidebar.focus();
    return;
  }
  if (e.altKey && e.key === '2') {
    e.preventDefault();
    if (view === 'rows') grid.el.focus();
    else mounted[view]?.view?.focus?.();
    return;
  }
  const typing = e.target.closest?.('input, textarea, select');
  if (typing || e.metaKey || e.ctrlKey || e.altKey) return;
  // A view by its number, as the tabs are in the top bar.
  const to = VIEWS.find(([v, , key]) => String(key) === e.key && (v !== 'admin' || adminAllowed()));
  if (to) {
    e.preventDefault();
    showView(to[0]);
    return;
  }
  const rows = view === 'rows';
  const act = {
    '/': rows && (() => parts.where.focus()),
    n: rows && (() => insert()),
    r: rows && (() => reload()),
    '?': () => help(),
  }[e.key];
  if (act) {
    e.preventDefault();
    act();
  }
});

// ------------------------------------------------------------------ start

const last = saved();
if (last) {
  connectTo(last.server, last.token, last.tenant).catch((e) => signIn(e instanceof ConnectError ? e.message : explain(e)));
} else {
  signIn();
}
