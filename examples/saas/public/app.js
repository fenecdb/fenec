// Trellis in the browser. The page holds no secret but a five-minute access
// token in memory: the refresh token is an HttpOnly cookie only
// /api/auth/ ever sees. With the access token it reads and writes its
// organisation straight from fenecdb (through /db/, a pipe to the router),
// and opens subscriptions there for the live board -- the database holds
// every request to the person's role and teams.
//
// Everything here is drawn with DOM calls, never innerHTML over data: a
// task title is text, whatever it holds.
import { connect, FenecError } from '/vendor/fenec/client.js';

const app = document.getElementById('app');

// ------------------------------------------------------------------ helpers

function h(tag, attrs = {}, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs ?? {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k === 'class') el.className = v;
    else if (k === 'dataset') Object.assign(el.dataset, v);
    else if (v === true) el.setAttribute(k, '');
    else el.setAttribute(k, String(v));
  }
  for (const kid of kids.flat(Infinity)) {
    if (kid === null || kid === undefined || kid === false) continue;
    el.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  }
  return el;
}

const LATTICE_SVG =
  '<svg viewBox="0 0 24 24" aria-hidden="true"><g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M3 8l13 13M8 3l13 13M3 16l5 5M16 3l5 5M3 16L16 3M8 21L21 8"/></g></svg>';

function wordmark(href = '/') {
  const a = h('a', { class: 'wordmark', href, 'data-nav': true });
  a.innerHTML = LATTICE_SVG; // a constant, not data
  a.append('trellis');
  return a;
}

const initials = (name) =>
  String(name || '?')
    .split(/\s+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((w) => w[0].toUpperCase())
    .join('');

const when = (iso) =>
  iso ? new Date(iso).toLocaleString(undefined, { day: 'numeric', month: 'short', hour: '2-digit', minute: '2-digit' }) : '';

function message(el, text, error = false) {
  el.textContent = text ?? '';
  el.className = error ? 'message error' : 'message';
  el.setAttribute('role', error ? 'alert' : 'status');
}

async function api(method, path, body, token) {
  const headers = {};
  if (body !== undefined) headers['content-type'] = 'application/json';
  if (token) headers.authorization = `Bearer ${token}`;
  const res = await fetch(path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body), credentials: 'same-origin' });
  let data = {};
  try {
    data = await res.json();
  } catch {
    // an empty answer
  }
  return { status: res.status, ok: res.ok, data };
}

/** A form that posts its fields, shows what came back, and calls `done` on success. */
function submitter(form, out, run) {
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const button = form.querySelector('button[type=submit]');
    button.disabled = true;
    message(out, '');
    try {
      const r = await run(Object.fromEntries(new FormData(form)));
      if (r && r.error) message(out, r.error, true);
      else if (r && r.message) message(out, r.message);
    } catch (err) {
      message(out, err.message || 'Something went wrong. Try again.', true);
    } finally {
      button.disabled = false;
    }
  });
}

// ----------------------------------------------------------------- session

const session = { user: null, orgs: [], api: null, access: new Map() };
const ROLE_RANK = { guest: 0, member: 1, admin: 2, owner: 3 };
const can = (slug, role) => ROLE_RANK[session.access.get(slug)?.role ?? 'guest'] >= ROLE_RANK[role];

/**
 * A refresh, one tab at a time: the refresh token is used once, so two tabs
 * rotating it at once would look like a stolen copy and end the session.
 * The Web Locks API makes them take turns; the second finds the cookie the
 * first was handed.
 */
async function refresh(org) {
  const run = () => api('POST', '/api/auth/refresh', org ? { org } : {});
  const r = navigator.locks ? await navigator.locks.request('trellis-refresh', run) : await run();
  if (r.status === 200 || (r.status === 403 && r.data.user)) {
    session.user = r.data.user;
    session.orgs = r.data.orgs;
    session.api = r.data.api;
    if (r.data.access) session.access.set(org, r.data.access);
  } else {
    session.user = null;
  }
  return r;
}

let refreshTimer = 0;
const tokenListeners = new Set();

/** The access token for an organisation, refreshed half a minute before it lapses. */
async function accessFor(slug) {
  const a = session.access.get(slug);
  if (a && a.exp * 1000 - Date.now() > 30_000) return a;
  const r = await refresh(slug);
  if (r.status !== 200) return null;
  clearTimeout(refreshTimer);
  const next = session.access.get(slug);
  refreshTimer = setTimeout(async () => {
    session.access.delete(slug);
    const fresh = await accessFor(slug);
    if (fresh) for (const f of tokenListeners) f(fresh);
  }, Math.max(5_000, next.exp * 1000 - Date.now() - 30_000));
  return next;
}

function dbOf(access) {
  return connect(`${location.origin}${access.db}`, { token: access.token });
}

/**
 * A write to the database: its own idempotency key, sent again with that
 * key after a dropped connection or a 503 (a tenant being moved answers
 * writes so for the moment the copy takes), so it lands once.
 */
async function write(access, statements) {
  const key = crypto.randomUUID();
  for (let attempt = 0; ; attempt++) {
    try {
      return await dbOf(access).batch(statements, { idempotencyKey: key });
    } catch (err) {
      const retry = !(err instanceof FenecError) || err.status === undefined || err.status >= 500;
      if (!retry || attempt >= 6) throw err;
      await new Promise((r) => setTimeout(r, 200 * 2 ** attempt));
    }
  }
}

/**
 * A subscription to a shape, kept open: its seed and every change after,
 * through the client's own (`db.subscribe`), which opens a stream that ends
 * -- a tenant moved to another node ends it -- again with backoff. The
 * server ends a stream at its token's `exp` with a 401, which the client
 * hands `onError` and does not retry: here that is a fresh token and the
 * stream again. It is also opened again at every refresh, a belt: a team
 * taken away stops reaching the board at the refresh rather than at the
 * old token's `exp`. `onState` hears 'on' and 'retry'.
 */
function subscribe(slug, collection, shape, onEvent, onState) {
  let closed = false;
  let current = null;
  const open = async () => {
    current?.();
    current = null;
    const access = await accessFor(slug);
    if (!access || closed) return;
    current = dbOf(access).subscribe(collection, shape, onEvent, {
      onState: (s) => onState(s === 'open' ? 'on' : 'retry'),
      onError: (err) => {
        if (err?.status !== 401 || closed) return;
        onState('retry');
        session.access.delete(slug);
        setTimeout(open, 250);
      },
    });
  };
  tokenListeners.add(open);
  open();
  return () => {
    closed = true;
    tokenListeners.delete(open);
    current?.();
  };
}

// ------------------------------------------------------------------ routes

let cleanup = [];
const onLeave = (f) => cleanup.push(f);

function go(path, replace = false) {
  if (replace) history.replaceState(null, '', path);
  else history.pushState(null, '', path);
  render();
}

document.addEventListener('click', (e) => {
  const a = e.target.closest('a[data-nav]');
  if (!a || e.metaKey || e.ctrlKey || e.shiftKey || a.target) return;
  e.preventDefault();
  go(a.getAttribute('href'));
});
window.addEventListener('popstate', () => render());

async function render() {
  for (const f of cleanup.splice(0)) f();
  const path = location.pathname;
  const q = new URLSearchParams(location.search);
  const open = new Set(['/signin', '/signup', '/forgot', '/reset', '/verify']);
  if (!session.user && !open.has(path)) {
    const r = await refresh();
    if (r.status !== 200) {
      if (path.startsWith('/invite/')) sessionStorage.setItem('trellis-after', path);
      return go('/signin', true);
    }
  }
  let m;
  if (path === '/signin') return show(signIn(q.get('error')));
  if (path === '/signup') return show(signUp());
  if (path === '/forgot') return show(forgot());
  if (path === '/reset') return show(reset(q.get('token') ?? ''));
  if (path === '/verify') return show(await verify(q.get('token') ?? ''));
  if (path === '/orgs/new') return show(newOrg());
  if ((m = /^\/invite\/([a-z0-9-]+)\/([^/]+)$/.exec(path))) return show(await acceptInvite(m[1], m[2]));
  if (path === '/account') return show(await workspace(session.orgs[0]?.slug, 'account', account));
  if ((m = /^\/o\/([a-z0-9-]+)(?:\/(board|members|search|audit))?(?:\/([^/]+))?$/.exec(path))) {
    const [, slug, page = 'board', arg] = m;
    const views = { board: (s, ctx) => boardView(s, ctx, arg), members, search: (s, ctx) => search(s, ctx, q.get('q') ?? ''), audit };
    return show(await workspace(slug, page, views[page]));
  }
  if (path === '/') {
    const after = sessionStorage.getItem('trellis-after');
    if (after) {
      sessionStorage.removeItem('trellis-after');
      return go(after, true);
    }
    return go(session.orgs[0] ? `/o/${session.orgs[0].slug}` : '/orgs/new', true);
  }
  show(h('main', { class: 'auth-form' }, h('h1', {}, 'There is nothing here'), h('a', { href: '/', 'data-nav': true }, 'Go to your boards')));
}

function show(el) {
  app.replaceChildren(el);
  const title = el.querySelector('h1')?.textContent;
  document.title = title ? `${title} – Trellis` : 'Trellis';
}

// ------------------------------------------------------------- auth pages

function authPage(title, lede, form, ...below) {
  const art = h(
    'section',
    { class: 'auth-art lattice' },
    wordmark(),
    h('div', {}, h('p', { class: 'claim' }, 'Work that grows in the open.'), h('p', {}, 'Boards, tasks and conversations for teams who would rather ship than chase status.')),
  );
  return h('div', { class: 'auth' }, art, h('main', { class: 'auth-form' }, h('h1', {}, title), lede ? h('p', { class: 'muted' }, lede) : null, form, ...below));
}

function field(label, attrs, hint) {
  const id = `f-${attrs.name}`;
  return [h('label', { for: id }, label, hint ? h('span', { class: 'hint' }, hint) : null), h('input', { id, ...attrs })];
}

function signIn(error) {
  const out = h('p', { class: 'message' });
  const form = h(
    'form',
    {},
    field('Email', { name: 'email', type: 'email', autocomplete: 'email', required: true }),
    field('Password', { name: 'password', type: 'password', autocomplete: 'current-password', required: true }),
    h('div', { class: 'actions' }, h('button', { type: 'submit' }, 'Sign in'), h('a', { href: '/forgot', 'data-nav': true }, 'Forgot your password?')),
    out,
  );
  if (error) message(out, error, true);
  submitter(form, out, async (d) => {
    const r = await api('POST', '/api/auth/signin', d);
    if (r.status !== 200) return r.data;
    session.user = r.data.user;
    session.orgs = r.data.orgs;
    session.api = r.data.api;
    go('/', true);
  });
  return authPage(
    'Sign in',
    null,
    form,
    h('p', { class: 'or' }, 'or'),
    h('a', { class: 'button quiet', href: '/api/auth/sso' }, 'Continue with single sign-on'),
    h('p', { class: 'foot' }, 'New to Trellis? ', h('a', { href: '/signup', 'data-nav': true }, 'Create an account')),
  );
}

function signUp() {
  const out = h('p', { class: 'message' });
  const form = h(
    'form',
    {},
    field('Your name', { name: 'name', type: 'text', autocomplete: 'name', required: true }),
    field('Work email', { name: 'email', type: 'email', autocomplete: 'email', required: true }),
    field('Password', { name: 'password', type: 'password', autocomplete: 'new-password', minlength: 10, required: true }, 'At least 10 characters.'),
    h('div', { class: 'actions' }, h('button', { type: 'submit' }, 'Create account')),
    out,
  );
  submitter(form, out, async (d) => (await api('POST', '/api/auth/signup', d)).data);
  return authPage('Create your account', 'We will mail you a link to confirm your address.', form, h('p', { class: 'foot' }, 'Have an account? ', h('a', { href: '/signin', 'data-nav': true }, 'Sign in')));
}

function forgot() {
  const out = h('p', { class: 'message' });
  const form = h(
    'form',
    {},
    field('Email', { name: 'email', type: 'email', autocomplete: 'email', required: true }),
    h('div', { class: 'actions' }, h('button', { type: 'submit' }, 'Send reset link')),
    out,
  );
  submitter(form, out, async (d) => (await api('POST', '/api/auth/forgot', d)).data);
  return authPage('Reset your password', 'The link works once, for 30 minutes.', form, h('p', { class: 'foot' }, h('a', { href: '/signin', 'data-nav': true }, 'Back to sign in')));
}

function reset(token) {
  const out = h('p', { class: 'message' });
  const form = h(
    'form',
    {},
    field('New password', { name: 'password', type: 'password', autocomplete: 'new-password', minlength: 10, required: true }, 'At least 10 characters. Every other session will be signed out.'),
    h('div', { class: 'actions' }, h('button', { type: 'submit' }, 'Set password')),
    out,
  );
  submitter(form, out, async (d) => {
    const r = await api('POST', '/api/auth/reset', { token, password: d.password });
    if (r.status === 200) setTimeout(() => go('/signin'), 1200);
    return r.data;
  });
  return authPage('Choose a new password', null, form);
}

async function verify(token) {
  const r = await api('POST', '/api/auth/verify', { token });
  const ok = r.status === 200;
  return authPage(ok ? 'Your email is confirmed' : 'This link did not work', r.data.message ?? r.data.error, h('a', { class: 'button', href: '/', 'data-nav': true }, ok ? 'Continue' : 'Go to Trellis'));
}

function newOrg() {
  const out = h('p', { class: 'message' });
  const slug = h('input', { id: 'f-slug', name: 'slug', type: 'text', required: true, pattern: '[a-z0-9][a-z0-9-]{1,38}[a-z0-9]', autocomplete: 'off' });
  const name = h('input', { id: 'f-name', name: 'name', type: 'text', required: true, autocomplete: 'organization' });
  name.addEventListener('input', () => {
    if (!slug.dataset.touched) slug.value = name.value.toLowerCase().normalize('NFKD').replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '').slice(0, 40);
  });
  slug.addEventListener('input', () => (slug.dataset.touched = '1'));
  const form = h(
    'form',
    {},
    h('label', { for: 'f-name' }, 'Organisation name'),
    name,
    h('label', { for: 'f-slug' }, 'Address', h('span', { class: 'hint' }, 'Lowercase letters, digits and dashes. It names your organisation\'s own database.')),
    slug,
    h('div', { class: 'actions' }, h('button', { type: 'submit' }, 'Create organisation'), session.orgs.length ? h('a', { href: '/', 'data-nav': true }, 'Cancel') : null),
    out,
  );
  submitter(form, out, async (d) => {
    const r = await api('POST', '/api/orgs', d, session.api.token);
    if (r.status !== 201) return r.data;
    await refresh();
    go(`/o/${r.data.slug}`);
  });
  const unverified = session.user && !session.user.verified;
  return authPage(
    'Start an organisation',
    unverified ? 'Confirm your email first: the link is in your inbox.' : 'Each organisation gets a database of its own. You will be its owner.',
    form,
    h('p', { class: 'foot' }, 'Joining a team instead? Open the invitation link from your mail.'),
  );
}

async function acceptInvite(slug, code) {
  const out = h('p', { class: 'message' });
  const button = h('button', { type: 'button' }, 'Accept invitation');
  button.addEventListener('click', async () => {
    button.disabled = true;
    const r = await api('POST', '/api/invites/accept', { org: slug, code }, session.api.token);
    button.disabled = false;
    if (r.status !== 200) return message(out, r.data.error, true);
    await refresh();
    go(`/o/${slug}`);
  });
  return authPage('You are invited', `Join ${slug} on Trellis as ${session.user.email}.`, h('div', {}, h('div', { class: 'actions' }, button), out));
}

// --------------------------------------------------------------- workspace

async function workspace(slug, page, view) {
  if (!slug) return newOrg();
  const access = await accessFor(slug);
  if (!access) {
    return authPage('You are not in this organisation', 'Ask one of its admins for an invitation.', h('a', { class: 'button', href: '/', 'data-nav': true }, 'Go to your boards'));
  }
  const db = dbOf(access);
  const teams = await db.rows('get teams select key, name order created limit 200');
  const ctx = { slug, access, db, teams, teamName: (k) => teams.find((t) => t.key === k)?.name ?? 'Unknown team' };
  const shell = h('div', { class: 'shell' });
  const nav = (href, label, current, extra) =>
    h('li', {}, h('a', { href, 'data-nav': true, 'aria-current': current ? 'page' : null }, extra ?? null, label));
  const orgSwitch = h(
    'select',
    { id: 'org-switch', 'aria-label': 'Organisation', onchange: (e) => go(e.target.value === '+' ? '/orgs/new' : `/o/${e.target.value}`) },
    session.orgs.map((o) => h('option', { value: o.slug, selected: o.slug === slug }, o.name)),
    h('option', { value: '+' }, 'Start an organisation…'),
  );
  const rail = h(
    'nav',
    { class: 'rail', 'aria-label': 'Workspace' },
    h('div', {}, wordmark(`/o/${slug}`)),
    orgSwitch,
    h('div', {}, h('h2', {}, 'Teams'), h('ul', {}, ctx.teams.map((t) => nav(`/o/${slug}/board/${t.key}`, t.name, page === 'board' && ctx.current === t.key, h('span', { class: 'swatch', dataset: { hue: teamHue(t.key) } }))))),
    h(
      'div',
      {},
      h('h2', {}, 'Organisation'),
      h(
        'ul',
        {},
        nav(`/o/${slug}/search`, 'Search', page === 'search'),
        nav(`/o/${slug}/members`, 'People', page === 'members'),
        can(slug, 'admin') ? nav(`/o/${slug}/audit`, 'Audit log', page === 'audit') : null,
        nav('/account', 'Account and security', page === 'account'),
      ),
    ),
    h('p', { class: 'who' }, h('strong', {}, session.user.name), `${access.role} · ${session.user.email}`),
  );
  const menu = h('button', { class: 'quiet', type: 'button', 'aria-expanded': 'false', 'aria-controls': 'rail' }, 'Menu');
  rail.id = 'rail';
  menu.addEventListener('click', () => {
    const open = shell.dataset.menu !== 'open';
    shell.dataset.menu = open ? 'open' : '';
    menu.setAttribute('aria-expanded', String(open));
  });
  rail.addEventListener('click', (e) => {
    if (e.target.closest('a')) shell.dataset.menu = '';
  });
  const main = h('main', { class: 'work', id: 'main' });
  shell.append(h('header', { class: 'topbar' }, wordmark(`/o/${slug}`), menu), rail, main);
  const content = await view(slug, ctx);
  // The rail is drawn before the view knows its team; mark it now.
  for (const a of rail.querySelectorAll('a')) a.toggleAttribute('aria-current', false);
  const here = rail.querySelector(`ul a[href="${CSS.escape(location.pathname)}"]`) ?? (page === 'board' && ctx.current ? rail.querySelector(`ul a[href="/o/${slug}/board/${ctx.current}"]`) : null);
  if (here) here.setAttribute('aria-current', 'page');
  main.append(content);
  return shell;
}

/**
 * A team's colour, from its key: one of six in style.css. A class rather
 * than a style attribute, which the page's CSP refuses.
 */
function teamHue(key) {
  let n = 0;
  for (const c of key) n = (n * 31 + c.charCodeAt(0)) >>> 0;
  return String(n % 6);
}

// ------------------------------------------------------------------- board

const LANES = [
  ['backlog', 'To do'],
  ['doing', 'In progress'],
  ['review', 'In review'],
  ['done', 'Done'],
];

async function boardView(slug, ctx, teamKey) {
  const team = ctx.teams.find((t) => t.key === teamKey) ?? ctx.teams[0];
  if (!team) {
    return h('div', {}, h('h1', {}, 'No teams yet'), h('p', { class: 'muted' }, can(slug, 'admin') ? 'Create one from People.' : 'An admin adds you to a team.'));
  }
  ctx.current = team.key;
  const people = await ctx.db.rows('get members select user, name, role order name limit 500');
  const nameOf = (u) => people.find((p) => p.user === u)?.name ?? 'Former member';
  const tasks = new Map();
  const live = h('span', { class: 'live', 'data-state': 'off' }, 'Connecting');
  const lanes = new Map();
  const mineRecent = new Set();

  const boardEl = h('section', { class: 'board lattice', 'aria-label': `${team.name} board` });
  for (const [status, label] of LANES) {
    const count = h('span', {}, '0');
    const list = h('div', { class: 'lane-cards' });
    const lane = h('section', { class: 'lane', dataset: { status }, 'aria-label': label }, h('header', { class: 'lane-head' }, h('h2', {}, label), count), list);
    if (can(slug, 'member')) {
      lane.addEventListener('dragover', (e) => {
        e.preventDefault();
        lane.dataset.over = '';
      });
      lane.addEventListener('dragleave', () => delete lane.dataset.over);
      lane.addEventListener('drop', async (e) => {
        e.preventDefault();
        delete lane.dataset.over;
        const id = Number(e.dataTransfer.getData('text/plain'));
        if (!tasks.has(id) || tasks.get(id).status === status) return;
        mineRecent.add(id);
        await write(ctx.access, [['set tasks {status: $2, updated: now()} where id = $1', [id, status]]]);
      });
    }
    lanes.set(status, { list, count });
    boardEl.append(lane);
  }

  const draw = (changed = []) => {
    for (const [status, { list, count }] of lanes) {
      const rows = [...tasks.values()].filter((t) => t.status === status).sort((a, b) => (b.priority ?? 0) - (a.priority ?? 0) || String(b.updated).localeCompare(String(a.updated)));
      count.textContent = String(rows.length);
      list.replaceChildren(
        ...(rows.length
          ? rows.map((t) => {
              const card = h(
                'button',
                { class: 'card', type: 'button', draggable: can(slug, 'member') ? 'true' : null, onclick: () => openTask(slug, ctx, t.id, people, nameOf) },
                h('span', { class: 't' }, t.title),
                h(
                  'span',
                  { class: 'meta' },
                  t.assignee ? h('span', { class: 'avatar', title: nameOf(t.assignee) }, initials(nameOf(t.assignee))) : null,
                  t.priority === 1 ? h('span', { class: 'pri', dataset: { p: '1' } }, 'Urgent') : null,
                  h('span', {}, when(t.updated)),
                ),
              );
              card.addEventListener('dragstart', (e) => e.dataTransfer.setData('text/plain', String(t.id)));
              if (changed.includes(t.id) && !mineRecent.has(t.id)) card.classList.add('bloom');
              return card;
            })
          : [h('p', { class: 'empty-lane' }, status === 'backlog' ? 'Nothing waiting.' : 'Nothing here.')]),
      );
    }
    for (const id of changed) mineRecent.delete(id);
  };

  // The board is the team's tasks as a subscription keeps them: a seed,
  // then each change. The token's `teams` claim holds it to teams Mia is
  // in, whatever the shape asks for.
  const stop = subscribe(
    slug,
    'tasks',
    { team: `eq.${team.key}` },
    (ev) => {
      if (ev.type === 'seed') {
        tasks.clear();
        for (const r of ev.rows) tasks.set(r.id, r);
        draw();
      } else {
        const changed = [];
        for (const r of ev.puts) {
          tasks.set(r.id, r);
          changed.push(r.id);
        }
        for (const id of ev.dels) tasks.delete(id);
        draw(changed);
      }
    },
    (state) => {
      live.dataset.state = state;
      live.textContent = state === 'on' ? 'Live' : 'Reconnecting';
    },
  );
  onLeave(stop);

  let add = null;
  if (can(slug, 'member')) {
    const input = h('input', { type: 'text', name: 'title', placeholder: 'Add a task to To do', 'aria-label': 'New task title', required: true, maxlength: 200 });
    add = h('form', { class: 'add-task' }, input, h('button', { type: 'submit' }, 'Add task'));
    add.addEventListener('submit', async (e) => {
      e.preventDefault();
      const title = input.value.trim();
      if (!title) return;
      input.value = '';
      // The author is named: the policy pins a member's to their own id,
      // and an admin's rule has no filter to pin it by.
      await write(ctx.access, [['insert tasks {team: $1, title: $2, status: "backlog", priority: 0, author: $3, updated: now()}', [team.key, title, session.user.uid]]]);
    });
  }
  return h(
    'div',
    {},
    h('div', { class: 'work-head' }, h('div', {}, h('h1', {}, team.name), h('p', {}, can(slug, 'member') ? 'Drag a card to move it, or open it to edit.' : 'You can read this board and comment on tasks.')), live),
    add ? h('div', { class: 'work-head' }, add) : null,
    boardEl,
  );
}

/** A task in the drawer: its fields for those who may edit them, and its thread. */
async function openTask(slug, ctx, id, people, nameOf) {
  const [task] = await ctx.db.rows('get tasks where id = $1 limit 1', [id]);
  if (!task) return;
  const editable = can(slug, 'member');
  const me = session.user.uid;
  const out = h('p', { class: 'message' });
  const dialog = h('dialog', { class: 'drawer', 'aria-labelledby': 'task-title' });
  const close = () => {
    dialog.close();
    dialog.remove();
  };
  const fields = editable
    ? h(
        'form',
        {},
        h('label', { for: 't-title' }, 'Title'),
        h('input', { id: 't-title', name: 'title', type: 'text', value: task.title, required: true, maxlength: 200 }),
        h('label', { for: 't-body' }, 'Details'),
        Object.assign(h('textarea', { id: 't-body', name: 'body' }), { value: task.body ?? '' }),
        h(
          'div',
          { class: 'grid2' },
          h('div', {}, h('label', { for: 't-status' }, 'Status'), h('select', { id: 't-status', name: 'status' }, LANES.map(([v, l]) => h('option', { value: v, selected: task.status === v }, l)))),
          h(
            'div',
            {},
            h('label', { for: 't-assignee' }, 'Assignee'),
            h('select', { id: 't-assignee', name: 'assignee' }, h('option', { value: '' }, 'Nobody'), people.filter((p) => p.role !== 'guest').map((p) => h('option', { value: p.user, selected: task.assignee === p.user }, p.name))),
          ),
          h('div', {}, h('label', { for: 't-priority' }, 'Priority'), h('select', { id: 't-priority', name: 'priority' }, h('option', { value: '0', selected: task.priority !== 1 }, 'Normal'), h('option', { value: '1', selected: task.priority === 1 }, 'Urgent'))),
          h('div', {}, h('label', { for: 't-due' }, 'Due'), h('input', { id: 't-due', name: 'due', type: 'date', value: task.due ? task.due.slice(0, 10) : '' })),
        ),
        h('div', { class: 'button-row' }, h('button', { type: 'submit' }, 'Save task'), can(slug, 'admin') ? h('button', { type: 'button', class: 'danger', onclick: async () => {
          await write(ctx.access, [['del tasks where id = $1 require 1', [id]]]);
          close();
        } }, 'Delete task') : null),
        out,
      )
    : h('div', {}, h('p', {}, task.body || h('span', { class: 'muted' }, 'No details.')), h('p', { class: 'readonly-note' }, 'Guests read tasks and comment on them.'));
  if (editable) {
    submitter(fields, out, async (d) => {
      await write(ctx.access, [
        [
          'set tasks {title: $2, body: $3, status: $4, assignee: $5, priority: $6, due: $7, updated: now()} where id = $1 require 1',
          [id, d.title, d.body, d.status, d.assignee || null, Number(d.priority), d.due ? new Date(d.due).toISOString() : null],
        ],
      ]);
      return { message: 'Saved.' };
    });
  }

  const thread = h('ol', { class: 'thread', 'aria-label': 'Comments' });
  const comments = new Map();
  const drawThread = () => {
    const rows = [...comments.values()].sort((a, b) => a.id - b.id);
    thread.replaceChildren(
      ...(rows.length
        ? rows.map((c) => {
            const mine = c.author === me;
            const body = h('p', {}, c.body);
            const li = h('li', { class: mine ? 'mine' : '' }, h('div', { class: 'by' }, h('b', {}, c.name || nameOf(c.author)), ` · ${when(c.at)}${c.edited ? ' · edited' : ''}`), body);
            if (mine) {
              li.append(
                h('button', { type: 'button', class: 'quiet', onclick: async () => {
                  const text = prompt('Edit your comment', c.body);
                  if (text && text.trim()) await write(ctx.access, [['set comments {body: $2, edited: now()} where id = $1 require 1', [c.id, text.trim()]]]);
                } }, 'Edit'),
              );
            }
            return li;
          })
        : [h('li', { class: 'muted' }, 'No comments yet.')]),
    );
  };
  const stop = subscribe(
    slug,
    'comments',
    { task: `eq.${id}` },
    (ev) => {
      if (ev.type === 'seed') {
        comments.clear();
        for (const r of ev.rows) comments.set(r.id, r);
      } else {
        for (const r of ev.puts) comments.set(r.id, r);
        for (const d of ev.dels) comments.delete(d);
      }
      drawThread();
    },
    () => {},
  );
  const say = h('textarea', { name: 'body', id: 'c-body', required: true, maxlength: 4000 });
  const commentForm = h('form', {}, h('label', { for: 'c-body' }, 'Add a comment'), say, h('div', { class: 'button-row' }, h('button', { type: 'submit' }, 'Post comment')));
  commentForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    const text = say.value.trim();
    if (!text) return;
    say.value = '';
    await write(ctx.access, [['insert comments {task: $1, team: $2, name: $3, body: $4, at: now()}', [id, task.team, session.user.name, text]]]);
  });
  dialog.append(
    h(
      'div',
      { class: 'drawer-body' },
      h('div', { class: 'drawer-top' }, h('h2', { id: 'task-title' }, task.title), h('button', { type: 'button', class: 'quiet', onclick: close }, 'Close')),
      h('p', { class: 'muted' }, `${ctx.teamName(task.team)} · opened by ${nameOf(task.author)}`),
      fields,
      h('h3', { class: 'spaced' }, 'Comments'),
      thread,
      commentForm,
    ),
  );
  dialog.addEventListener('close', () => {
    stop();
    dialog.remove();
  });
  document.body.append(dialog);
  dialog.showModal();
}

// ------------------------------------------------------------------ search

/** Text with the [start, end) UTF-16 spans `highlight()` returned marked, as nodes. */
function marked(text, spans) {
  const out = [];
  let at = 0;
  for (const [s, e] of spans ?? []) {
    if (s > at) out.push(text.slice(at, s));
    out.push(h('mark', {}, text.slice(s, e)));
    at = e;
  }
  out.push(text.slice(at));
  return out;
}

async function search(slug, ctx, q) {
  const input = h('input', { type: 'search', name: 'q', value: q, placeholder: 'Search tasks and comments', 'aria-label': 'Search', autofocus: true });
  const form = h('form', { class: 'add-task', role: 'search' }, input, h('button', { type: 'submit' }, 'Search'));
  form.addEventListener('submit', (e) => {
    e.preventDefault();
    go(`/o/${slug}/search?q=${encodeURIComponent(input.value)}`);
  });
  const results = h('ol', { class: 'list results' });
  let note = 'Search finds words in titles, details and comments of the teams you are in.';
  if (q.trim()) {
    // BM25 here is taken over the rows this token may read: another
    // team's tasks neither appear nor move these scores.
    const [byTitle, byBody, byComment] = await Promise.all([
      ctx.db.rows('get tasks select id, team, title, status, highlight(title) match title $1 limit 20', [q]),
      ctx.db.rows('get tasks select id, team, title, status, snippet(body, 18) match body $1 limit 20', [q]),
      ctx.db.rows('get comments select task, team, name, snippet(body, 18) match body $1 limit 20', [q]),
    ]);
    const seen = new Set();
    const items = [];
    for (const r of [...byTitle, ...byBody]) {
      if (seen.has(r.id)) continue;
      seen.add(r.id);
      const snip = r['snippet(body)'];
      items.push([r._score, h('li', {}, h('a', { href: `/o/${slug}/board/${r.team}`, 'data-nav': true }, ...marked(r.title, r['highlight(title)'])), snip ? h('p', {}, ...marked(snip.text, snip.marks)) : null, h('p', { class: 'where' }, `Task in ${ctx.teamName(r.team)}`))]);
    }
    for (const c of byComment) {
      const snip = c['snippet(body)'];
      items.push([c._score, h('li', {}, h('p', {}, ...marked(snip.text, snip.marks)), h('p', { class: 'where' }, `${c.name || 'Someone'} commented in ${ctx.teamName(c.team)}`))]);
    }
    items.sort((a, b) => b[0] - a[0]);
    results.append(...items.map(([, el]) => el));
    note = items.length ? `${items.length} found for “${q}”.` : `Nothing in your teams matches “${q}”. Try fewer words.`;
  }
  return h('div', {}, h('div', { class: 'work-head' }, h('h1', {}, 'Search')), form, h('p', { class: 'muted spaced-sm' }, note), results);
}

// ----------------------------------------------------------------- members

async function members(slug, ctx) {
  const admin = can(slug, 'admin');
  const rows = await ctx.db.rows('get members select user, name, email, role, teams, joined order name limit 500');
  const out = h('p', { class: 'message' });
  const list = h(
    'ul',
    { class: 'list' },
    rows.map((m) => {
      const teams = (m.teams ?? []).map((k) => ctx.teamName(k)).join(', ') || 'No team';
      const controls = [];
      if (admin && m.role !== 'owner' && m.user !== session.user.uid) {
        const role = h('select', { 'aria-label': `Role of ${m.name}` }, ['admin', 'member', 'guest'].map((r) => h('option', { value: r, selected: m.role === r }, r)));
        role.addEventListener('change', async () => {
          const r = await api('PATCH', `/api/orgs/${slug}/members/${m.user}`, { role: role.value }, ctx.access.token);
          message(out, r.data.message ?? r.data.error, !r.ok);
        });
        const teamBox = h('fieldset', {}, h('legend', { class: 'sr' }, `Teams of ${m.name}`), ctx.teams.map((t) => h('label', {}, h('input', { type: 'checkbox', value: t.key, checked: (m.teams ?? []).includes(t.key) }), t.name)));
        teamBox.addEventListener('change', async () => {
          const chosen = [...teamBox.querySelectorAll('input:checked')].map((i) => i.value);
          const r = await api('PATCH', `/api/orgs/${slug}/members/${m.user}`, { teams: chosen }, ctx.access.token);
          message(out, r.ok ? `${m.name}'s teams are saved. They see the change at their next refresh, within five minutes.` : r.data.error, !r.ok);
        });
        const remove = h('button', { type: 'button', class: 'danger', onclick: async () => {
          if (!confirm(`Remove ${m.name} from this organisation?`)) return;
          const r = await api('DELETE', `/api/orgs/${slug}/members/${m.user}`, undefined, ctx.access.token);
          if (r.ok) go(location.pathname, true);
          else message(out, r.data.error, true);
        } }, 'Remove');
        controls.push(h('details', { class: 'manage' }, h('summary', {}, `Change ${m.name.split(' ')[0]}'s role or teams`), h('div', { class: 'row' }, role, remove), teamBox));
      }
      return h('li', {}, h('div', { class: 'row' }, h('div', { class: 'grow' }, h('strong', {}, m.name), h('div', { class: 'muted' }, m.email), h('div', { class: 'muted' }, teams)), h('span', { class: 'pill' }, m.role)), ...controls);
    }),
  );
  const parts = [h('div', { class: 'work-head' }, h('div', {}, h('h1', {}, 'People'), h('p', {}, `${rows.length} in this organisation`))), out, list];
  if (admin) {
    const inviteOut = h('p', { class: 'message' });
    const invite = h(
      'form',
      {},
      h(
        'div',
        { class: 'inline-form' },
        h('div', {}, h('label', { for: 'i-email' }, 'Email'), h('input', { id: 'i-email', name: 'email', type: 'email', required: true })),
        h('div', {}, h('label', { for: 'i-role' }, 'Role'), h('select', { id: 'i-role', name: 'role' }, ['member', 'guest', 'admin'].map((r) => h('option', { value: r }, r)))),
      ),
      h('fieldset', {}, h('legend', {}, 'Teams'), ctx.teams.map((t) => h('label', {}, h('input', { type: 'checkbox', name: 'teams', value: t.key }), t.name))),
      h('div', { class: 'button-row' }, h('button', { type: 'submit' }, 'Send invitation')),
      inviteOut,
    );
    invite.addEventListener('submit', async (e) => {
      e.preventDefault();
      const fd = new FormData(invite);
      const r = await api('POST', `/api/orgs/${slug}/invites`, { email: fd.get('email'), role: fd.get('role'), teams: fd.getAll('teams') }, ctx.access.token);
      message(inviteOut, r.data.message ?? r.data.error, !r.ok);
      if (r.ok) invite.reset();
    });
    const teamOut = h('p', { class: 'message' });
    const team = h('form', { class: 'inline-form' }, h('div', {}, h('label', { for: 'n-team' }, 'Team name'), h('input', { id: 'n-team', name: 'name', type: 'text', required: true, maxlength: 60 })), h('button', { type: 'submit' }, 'Create team'));
    team.addEventListener('submit', async (e) => {
      e.preventDefault();
      const r = await api('POST', `/api/orgs/${slug}/teams`, { name: new FormData(team).get('name') }, ctx.access.token);
      if (!r.ok) return message(teamOut, r.data.error, true);
      session.access.delete(slug);
      go(`/o/${slug}/board/${r.data.key}`);
    });
    parts.push(
      h('section', { class: 'section' }, h('h2', {}, 'Invite someone'), h('p', { class: 'muted' }, 'The invitation is mailed, works once, and lapses after three days.'), invite),
      h('section', { class: 'section' }, h('h2', {}, 'Create a team'), team, teamOut),
    );
  }
  return h('div', {}, ...parts);
}

// ------------------------------------------------------------------- audit

async function audit(slug, ctx) {
  const rows = await ctx.db.rows('get audit order at desc limit 200');
  const people = await ctx.db.rows('get members select user, name limit 500');
  const name = (u) => people.find((p) => p.user === u)?.name ?? u;
  const ACTIONS = {
    'org.created': 'created the organisation',
    'invite.created': 'invited',
    'member.joined': 'joined as',
    'member.role': 'changed a role',
    'member.teams': 'changed teams',
    'member.removed': 'removed a member',
    'team.created': 'created a team',
  };
  return h(
    'div',
    {},
    h('div', { class: 'work-head' }, h('div', {}, h('h1', {}, 'Audit log'), h('p', {}, 'What admins did here. No one can edit or delete these rows, admins and this app included.'))),
    h(
      'div',
      { class: 'scroll-x' },
      h(
        'table',
        {},
        h('thead', {}, h('tr', {}, h('th', { scope: 'col' }, 'When'), h('th', { scope: 'col' }, 'Who'), h('th', { scope: 'col' }, 'What'), h('th', { scope: 'col' }, 'Detail'))),
        h('tbody', {}, rows.map((r) => h('tr', {}, h('td', {}, when(r.at)), h('td', {}, name(r.actor)), h('td', {}, ACTIONS[r.action] ?? r.action), h('td', {}, [r.target?.startsWith('u_') ? name(r.target) : r.target?.startsWith('t_') ? '' : r.target, r.detail].filter(Boolean).join(' · '))))),
      ),
    ),
  );
}

// ----------------------------------------------------------------- account

async function account() {
  const r = await api('GET', '/api/me/security', undefined, session.api.token);
  const KINDS = {
    signin: 'Signed in',
    'signin.sso': 'Signed in with single sign-on',
    'signin.failed': 'A sign-in failed',
    'signin.limited': 'Sign-ins paused after too many attempts',
    signup: 'Account created',
    verified: 'Email confirmed',
    'reset.requested': 'Password reset requested',
    reset: 'Password changed',
    signout: 'Signed out',
    'refresh.reuse': 'A copied session was refused, and that session ended',
  };
  const out = h('button', { type: 'button', class: 'quiet', onclick: async () => {
    await api('POST', '/api/auth/signout', {});
    session.user = null;
    session.access.clear();
    go('/signin');
  } }, 'Sign out');
  return h(
    'div',
    {},
    h('div', { class: 'work-head' }, h('div', {}, h('h1', {}, 'Account and security'), h('p', {}, `${session.user.name} · ${session.user.email}`)), out),
    h('section', { class: 'section' }, h('h2', {}, 'Recent activity'), h('ul', { class: 'list' }, (r.data.events ?? []).map((e) => h('li', {}, h('div', { class: 'row' }, h('span', { class: 'grow' }, KINDS[e.kind] ?? e.kind), h('span', { class: 'muted' }, `${when(e.at)} · ${e.ip}`)))))),
  );
}

render();
