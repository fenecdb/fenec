// Where the studio connects and as whom.
//
// The token lives in this tab's sessionStorage and nowhere else: not in
// localStorage, which every tab of the origin shares and which outlives
// the browser; not in a cookie, which the browser would send on its own;
// not in a URL, which lands in history, logs and `Referer`. Closing the tab
// forgets it. It is sent as `Authorization: Bearer` to the server named
// here, which the page's policy (`connect-src`) limits to this origin and
// the one `--studio-connect` gave.

import { connect } from './client.js';

const KEY = 'fenec-studio';

/** `{server, token, tenant}` this tab connected with, or `null`. */
export function saved() {
  try {
    const s = JSON.parse(sessionStorage.getItem(KEY) ?? 'null');
    return s && typeof s.server === 'string' ? s : null;
  } catch {
    return null;
  }
}

export function save(s) {
  try {
    sessionStorage.setItem(KEY, JSON.stringify({ server: s.server, token: s.token, tenant: s.tenant ?? null }));
  } catch {
    // A tab that cannot store asks again after a reload.
  }
}

export function forget() {
  try {
    sessionStorage.removeItem(KEY);
  } catch {
    /* nothing kept */
  }
}

/** This page's own server: the studio is served by the server it reads. */
export const here = () => location.origin;

/**
 * A JSON Web Token's claims, read and not verified -- what the page shows
 * of it (subject, role, tenant, expiry). The server verifies every request;
 * nothing here decides anything on the strength of a claim.
 */
export function claimsOf(token) {
  const parts = String(token ?? '').split('.');
  if (parts.length !== 3) return null;
  try {
    const b64 = parts[1].replace(/-/g, '+').replace(/_/g, '/');
    const text = new TextDecoder().decode(Uint8Array.from(atob(b64 + '='.repeat((4 - (b64.length % 4)) % 4)), (c) => c.charCodeAt(0)));
    const claims = JSON.parse(text);
    return claims && typeof claims === 'object' && !Array.isArray(claims) ? claims : null;
  } catch {
    return null;
  }
}

async function getJson(url, token) {
  const headers = token ? { authorization: `Bearer ${token}` } : {};
  const res = await fetch(url, { headers });
  let body = null;
  try {
    body = await res.json();
  } catch {
    /* not JSON */
  }
  return { status: res.status, body };
}

/** A refusal or an unreachable server, in words. */
export class ConnectError extends Error {}

/**
 * What the server at `server` is and whom it takes `token` for:
 *
 *   {mode: 'single',  who}             one database
 *   {mode: 'tenants', who, tenants}    a node of tenants, `/t/<t>/`
 *   {mode: 'router',  tenants}         fenec-shard in front of the nodes
 *
 * `who` is `/_whoami`'s answer. `tenants` is what the token may reach: the
 * ones its claim names, else the node's or the router's list where the
 * token may read it, else `null` -- the tenant is typed.
 */
export async function probe(server, token) {
  let got;
  try {
    got = await getJson(`${server}/_whoami`, token);
  } catch (e) {
    // A router on another origin answers its own paths with no CORS
    // header, where a tenant's are its node's, which may have one: with a
    // tenant the token names, the studio goes straight to it.
    const named = await tenantList(server, token, null, null);
    if (named?.length) return { mode: 'router', tenants: named };
    throw new ConnectError(`could not reach ${server}: ${e.message}`);
  }
  if (got.status === 401) throw new ConnectError(`the server refused the token: ${got.body?.error ?? 'invalid or missing token'}`);
  if (got.status === 200 && got.body?.node === 'single') return { mode: 'single', who: got.body };
  if (got.status === 200 && got.body?.node === 'tenants') {
    return { mode: 'tenants', who: got.body, tenants: await tenantList(server, token, got.body, '/_admin/tenants') };
  }
  const router = got.status === 404 && /\/t\/<tenant>/.test(got.body?.error ?? '');
  if (router) return { mode: 'router', tenants: await tenantList(server, token, null, '/_shard/tenants') };
  throw new ConnectError(`${server} did not answer as a fenecdb server (${got.status}): ${got.body?.error ?? 'no /_whoami'}`);
}

async function tenantList(server, token, who, path) {
  const named = who?.tenants ?? claimsOf(token)?.tenant;
  if (Array.isArray(named)) return named.filter((t) => typeof t === 'string');
  if (typeof named === 'string') return [named];
  if (!path) return null;
  // The node's admin list or the router's directory, where this token is
  // the one that reads it; refused, the tenant is typed.
  try {
    const got = await getJson(`${server}${path}`, token);
    if (got.status !== 200 || !Array.isArray(got.body)) return null;
    return got.body.map((t) => (typeof t === 'string' ? t : t?.name)).filter((t) => typeof t === 'string');
  } catch {
    return null;
  }
}

/** The client for one database: the server's, or a tenant's under it. */
export function clientFor(server, token, tenant) {
  const base = tenant ? `${server}/t/${encodeURIComponent(tenant)}` : server;
  return { base, db: new Remote(server, base, token) };
}

/**
 * A database on a server, as the studio reads one: `FenecHttp`'s `run`,
 * `batch` and `subscribe`, a view's whole answer (`request`, whose code
 * comes with the views, kit.js) and the file's sizes. local.js's database
 * in the page has the same surface.
 */
export class Remote {
  local = false;

  constructor(server, base, token) {
    this.at = { server, base, token };
    this.http = connect(base, { token: token || null });
  }

  get seq() {
    return this.http.seq;
  }

  set onError(f) {
    this.http.onError = f;
  }

  run(text, params) {
    return this.http.run(text, params);
  }

  batch(items, opts) {
    return this.http.batch(items, opts);
  }

  subscribe(collection, shape, onEvent, opts) {
    return this.http.subscribe(collection, shape, onEvent, opts);
  }

  request(path, opts) {
    return import('./kit.js').then((k) => k.fetched(this.at, path, opts));
  }

  stats() {
    return metrics(this.at.base, this.at.token);
  }
}

/** `/_whoami` for a tenant, through the router or on its node. */
export async function whoamiAt(base, token) {
  const got = await getJson(`${base}/_whoami`, token);
  if (got.status !== 200) throw new ConnectError(got.body?.error ?? `HTTP ${got.status}`);
  return got.body;
}

/** `GET <base>/<path>` with the token, its JSON or `null` when refused. */
export async function maybe(base, path, token) {
  try {
    const got = await getJson(`${base}${path}`, token);
    return got.status === 200 ? got.body : null;
  } catch {
    return null;
  }
}

/** `/_metrics` (Prometheus text) when the token may read it, else `null`. */
export async function metrics(base, token) {
  try {
    const res = await fetch(`${base}/_metrics`, { headers: token ? { authorization: `Bearer ${token}` } : {} });
    if (!res.ok) return null;
    return parseMetrics(await res.text());
  } catch {
    return null;
  }
}

/** The file's size and what a collection's records and dead ones take. */
export function parseMetrics(text) {
  const out = { file: null, reclaimable: null, data: {}, dead: {} };
  for (const line of text.split('\n')) {
    const m = /^(fenec_[a-z_]+)(?:\{collection="((?:[^"\\]|\\.)*)"\})?\s+(\S+)$/.exec(line);
    if (!m) continue;
    const [, metric, collection, value] = m;
    const n = Number(value);
    if (metric === 'fenec_file_bytes' && !collection) out.file = n;
    else if (metric === 'fenec_reclaimable_bytes' && !collection) out.reclaimable = n;
    else if (metric === 'fenec_data_bytes' && collection !== undefined) out.data[collection] = n;
    else if (metric === 'fenec_dead_bytes' && collection !== undefined) out.dead[collection] = n;
  }
  return out;
}
