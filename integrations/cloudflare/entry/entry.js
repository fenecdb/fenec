// The global entry: one URL for every tenant, wherever it lives. A plain
// Worker sends `/t/<tenant>/...` to the routers of the region the tenant
// lives in, and their answer back as it comes -- a stream as a stream, a
// held write for as long as it is held.
//
// Where a tenant lives is learnt from the routers' answers, not asked of
// anyone: every request asks for its tenant's home (`Fenec-Home: 1`), and
// the router that forwards it to the tenant's node names the node's region
// in its answer (`fenec-shard`'s `region::HOME_HEADER`). A tenant this
// isolate has not seen goes to the region nearest the client, whose router
// serves it or relays it home -- one hop, which every router makes for a
// tenant of another region -- and its next request goes home. A tenant
// moved across regions since is relayed by the router it was sent to, as
// a client sending it there would be, and the answer names the new home:
// a stale home costs a hop, never an error. So nothing remote is asked on
// the way to a router: no lookup per request, no token reaching
// `/_shard/`, and no unknown tenant name turned into a question for the
// global router.
//
// Kept apart from worker.js so Node tests read it without workerd.

/** A tenant's name, as a node takes one (fenec-http's `check_name`). */
const TENANT = /^[a-z0-9_-]{1,64}$/;

/** The header a request asks for its tenant's home in, and the answer names it in. */
export const HOME = 'fenec-home';

/**
 * The header the entry names its client in for the routers: `<address>
 * <mark>`, the mark 16 bytes of HMAC-SHA256 of {@link CLIENT_MARK_TEXT}
 * under the entry's token, base64url, as a router names its client to a
 * node (`Fenec-Router`, fenec-http's `audit::router_mark`). Behind the
 * entry every request reaches a router from Cloudflare's addresses, so the
 * router's wait after a refused token would key every client to them. A
 * Worker controls `CF-Connecting-IP` and `X-Real-IP` on what it sends to an
 * origin outside Cloudflare (Cloudflare's "HTTP request headers" page), so
 * any Worker of any account can claim any address from Cloudflare's
 * ranges: a router may believe the address only beside a secret it shares
 * with this entry, never by the peer's address.
 */
export const CLIENT = 'fenec-client';

/** What the client mark is an HMAC of. */
export const CLIENT_MARK_TEXT = 'fenec-client: a request through the entry, the client named beside it';

/** How many tenants' homes an isolate keeps by default. */
export const HOMES = 50_000;

/**
 * A body up to this is read whole before it goes, so it can go to a
 * region's next router where the first cannot be reached; a longer one is
 * streamed to one router, its length kept (a router refuses a chunked
 * body, 411). 1 MiB: what isolates sharing 128 MB hold for a request's
 * retry, against a load's pages of many megabytes.
 */
export const BUFFER_AT = 1 << 20;

/**
 * Answers Cloudflare gives a Worker's fetch where the origin was never
 * reached -- refused (521), not connected (522), unreachable (523), its
 * TLS failed (525, 526), no tunnel connector (530) -- so the router cannot
 * have run the request. fenecdb sends none of them.
 */
const UNREACHED = new Set([521, 522, 523, 525, 526, 530]);

/** Methods a router may be sent twice (Go's net/http rule, the router's own towards a node). */
const REPEATABLE = new Set(['GET', 'HEAD', 'OPTIONS']);

/**
 * Request headers never sent on: hop by hop (RFC 9110 7.6.1), what fetch
 * writes itself, the marks one router believes from another alone -- a
 * client relaying itself, or one telling a router it verified a token --
 * the entry's own, set here, and every way a client names an address,
 * which only the entry's mark may (and Cloudflare's own `cf-*`).
 */
const DROPPED = new Set([
  'connection',
  'keep-alive',
  'proxy-connection',
  'te',
  'trailer',
  'transfer-encoding',
  'upgrade',
  'proxy-authorization',
  'proxy-authenticate',
  'host',
  'content-length',
  'fenec-router',
  'fenec-global',
  'fenec-verified',
  HOME,
  CLIENT,
  'x-forwarded-for',
  'x-forwarded-host',
  'x-forwarded-proto',
  'x-real-ip',
  'forwarded',
  'true-client-ip',
  'cdn-loop',
]);

/**
 * Answer headers never sent back: hop by hop, and whether a node verified
 * a token, which a router tells only a router relaying for a client.
 */
const ANSWER_DROPPED = ['connection', 'keep-alive', 'proxy-connection', 'te', 'trailer', 'transfer-encoding', 'upgrade', 'fenec-verified'];

/**
 * The headers fenec-http's `cors::headers` writes under `--http-cors`: the
 * entry answers a preflight itself with them, and puts them on its own
 * answers, never on a router's -- two `Access-Control-Allow-Origin`s are
 * refused whole by a browser. entry.test.js holds them to a router's.
 */
export function corsHeaders(origin) {
  return {
    'Access-Control-Allow-Origin': origin,
    'Access-Control-Allow-Methods': 'GET, POST, PATCH, DELETE, OPTIONS',
    'Access-Control-Allow-Headers':
      'content-type, authorization, idempotency-key, fenec-after, fenec-wait, if-none-match, mcp-protocol-version, mcp-method, mcp-name',
    'Access-Control-Expose-Headers': 'fenec-seq, fenec-next, idempotent-replayed, etag, x-request-id, fenec-quota, retry-after',
    'Access-Control-Max-Age': '600',
    Vary: 'Origin',
  };
}

/** A value of the environment given as JSON or as what JSON makes (wrangler.jsonc's vars, or `--var`). */
function parsed(value, what) {
  if (value === undefined || value === null || value === '') return null;
  if (typeof value !== 'string') return value;
  try {
    return JSON.parse(value);
  } catch {
    throw new Error(`${what} is not JSON: ${value}`);
  }
}

/** A router's URL as a request's is made from it: `https://host[:port]`, no slash after. */
function origin(url, what) {
  const u = new URL(url);
  if (u.protocol !== 'https:' && u.protocol !== 'http:') throw new Error(`${what}: ${url} is not http(s)`);
  return u.origin;
}

/**
 * The entry's configuration out of its environment:
 *
 * - `REGIONS`: `{"<region>": {"routers": [...], "continents": [...]}}`, the
 *   regions by the names the routers give them, each one's routers in the
 *   order to try -- the one keeping its directory, then its standbys -- and
 *   the continents (Cloudflare's `request.cf.continent`: AF, AN, AS, EU, NA,
 *   OC, SA) whose clients send a tenant the entry has not seen there first.
 * - `DEFAULT_REGION`: the region such a tenant goes to from anywhere else;
 *   the first of `REGIONS` without it.
 * - `CORS`: the routers' `--http-cors`, for the preflights the entry
 *   answers and its own answers; none without it, preflights going on.
 * - `ADMIN`: the global router and its standbys, to send `/_shard/` to;
 *   without it `/_shard/` is a 404 here, as every other path but `/t/` is.
 * - `HOMES`: how many tenants' homes an isolate keeps (50 000).
 * - `HOMES_CACHE`: `off` keeps them in the isolate alone, not shared with
 *   the colo's other isolates through the Cache API.
 * - `ENTRY_TOKEN` (a secret): the key of the client's mark ({@link CLIENT});
 *   without it no client is named.
 */
export function configure(env) {
  const regions = new Map();
  const byContinent = new Map();
  for (const [name, r] of Object.entries(parsed(env.REGIONS, 'REGIONS') ?? {})) {
    if (!TENANT.test(name)) throw new Error(`REGIONS: \`${name}\` is not a region's name, [a-z0-9_-]`);
    const list = Array.isArray(r) ? r : r?.routers;
    if (!Array.isArray(list) || !list.length) throw new Error(`REGIONS: region \`${name}\` names no router`);
    const region = { name, routers: list.map((u) => origin(u, `REGIONS.${name}`)) };
    regions.set(name, region);
    for (const c of Array.isArray(r) ? [] : (r.continents ?? [])) byContinent.set(String(c).toUpperCase(), region);
  }
  if (!regions.size) throw new Error('REGIONS names no region: {"<region>": {"routers": ["https://..."]}}');
  const fallback = env.DEFAULT_REGION || regions.keys().next().value;
  if (!regions.has(fallback)) throw new Error(`DEFAULT_REGION \`${fallback}\` is not one of REGIONS`);
  const admin = parsed(env.ADMIN, 'ADMIN');
  const homes = env.HOMES === undefined || env.HOMES === '' ? HOMES : Number(env.HOMES);
  if (!Number.isInteger(homes) || homes < 0) throw new Error(`HOMES is a count of tenants: ${env.HOMES}`);
  return {
    regions,
    byContinent,
    fallback: regions.get(fallback),
    cors: env.CORS || null,
    admin: admin ? { name: 'global', routers: [].concat(admin).map((u) => origin(u, 'ADMIN')) } : null,
    homes,
    share: env.HOMES_CACHE !== 'off',
    token: env.ENTRY_TOKEN || null,
  };
}

/** base64url without padding, as fenec-http's `b64url_encode`. */
function b64url(bytes) {
  let s = '';
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/=+$/, '').replace(/\+/g, '-').replace(/\//g, '_');
}

/** The mark the entry names its client beside ({@link CLIENT}): worked out once an isolate. */
export async function clientMark(token) {
  const enc = new TextEncoder();
  const key = await crypto.subtle.importKey('raw', enc.encode(token), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  const mac = new Uint8Array(await crypto.subtle.sign('HMAC', key, enc.encode(CLIENT_MARK_TEXT)));
  return b64url(mac.subarray(0, 16));
}

/**
 * The headers a request goes to a router with: `mark` names the client,
 * where there is one. A header a client names in `Connection` cannot be
 * told apart here -- the runtime keeps `Connection` from the Worker -- and
 * goes on: a router reads no header it does not know.
 */
export function sentHeaders(incoming, mark) {
  const out = new Headers();
  for (const [k, v] of incoming) {
    if (DROPPED.has(k) || k.startsWith('cf-')) continue;
    out.append(k, v);
  }
  out.set(HOME, '1');
  // The client's address as Cloudflare saw it: the client cannot set it.
  const ip = incoming.get('cf-connecting-ip');
  if (mark && ip && !/\s/.test(ip)) out.set(CLIENT, `${ip} ${mark}`);
  // The router takes a client's request id, or makes one: Cloudflare's ray
  // id, which every Cloudflare log of the request carries, where the client
  // sent none.
  if (!out.has('x-request-id') && incoming.has('cf-ray')) out.set('x-request-id', incoming.get('cf-ray'));
  return out;
}

/** Whether `--http-cors <cors>` lets the request's origin in (fenec-http's `cors::allows`). */
function allows(cors, request) {
  return cors === '*' || (cors !== null && request.headers.get('origin') === cors);
}

/** Sixteen hex characters, as a router makes a request id. */
function madeId() {
  const b = crypto.getRandomValues(new Uint8Array(8));
  return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
}

/**
 * The entry's own answer: `{"error"}` as a router's, `retry_after` beside
 * `Retry-After` where the client should come back (every SDK reads the
 * body), the request's id, and the CORS headers a router would give.
 */
function own(cfg, request, status, message, retry) {
  const headers = { 'Content-Type': 'application/json; charset=utf-8' };
  headers['X-Request-Id'] = request.headers.get('x-request-id') ?? request.headers.get('cf-ray') ?? madeId();
  const body = { error: message };
  if (retry) {
    body.retry_after = retry;
    headers['Retry-After'] = String(retry);
  }
  if (cfg?.cors) Object.assign(headers, corsHeaders(cfg.cors));
  return new Response(JSON.stringify(body), { status, headers });
}

/**
 * Where an isolate's requests go: the tenants' homes it has heard, and the
 * router of each region that answered last, tried first.
 */
export class State {
  constructor() {
    this.homes = new Map();
    this.answered = new Map();
    this.mark = null;
    this.cache = null;
  }

  /**
   * Notes `tenant`'s home: whether it changed. Forgotten first-heard first
   * past `limit` -- not by use, which would be a delete and a set on every
   * request -- and a tenant forgotten is heard again at the cost of a hop.
   */
  learn(tenant, region, limit) {
    if (this.homes.get(tenant) === region) return false;
    this.homes.delete(tenant);
    if (limit === 0) return true;
    if (this.homes.size >= limit) this.homes.delete(this.homes.keys().next().value);
    this.homes.set(tenant, region);
    return true;
  }
}

/** The colo's copy of a tenant's home: a URL of the entry's own host that no request reaches. */
function homeKey(url, tenant) {
  return `${url.origin}/_entry/homes/${tenant}`;
}

/** The name the Cache API keeps the homes under. */
const CACHE = 'fenec-entry-homes';

async function cached(state, url, tenant) {
  try {
    state.cache ??= caches.open(CACHE);
    const hit = await (await state.cache).match(homeKey(url, tenant));
    return hit ? await hit.text() : null;
  } catch {
    // A cache that cannot be read is a home not heard: a hop.
    return null;
  }
}

async function remember(state, url, tenant, region) {
  try {
    state.cache ??= caches.open(CACHE);
    const headers = { 'Cache-Control': 'max-age=86400' };
    await (await state.cache).put(homeKey(url, tenant), new Response(region, { headers }));
  } catch {
    // Not shared: the colo's other isolates hear it themselves, a hop each.
  }
}

/**
 * Sends `request` to `region`'s routers, from the one that answered last,
 * moving to the next only where the one asked cannot have run it, or where
 * running it twice does no harm; its answer, or the entry's own. What went
 * wrong with a router is logged with the router named, and told the client
 * without it: the routers' hostnames are the operator's.
 */
async function forward(request, url, region, cfg, state) {
  const mark = cfg.token ? await (state.mark ??= clientMark(cfg.token)) : null;
  const headers = sentHeaders(request.headers, mark);
  const method = request.method;
  const tries = region.routers.length;
  let body = method === 'GET' || method === 'HEAD' ? null : request.body;
  let again = body === null;
  if (body !== null) {
    const length = request.headers.get('content-length');
    // A chunked body is read whole: a router takes none.
    if (length === null || (tries > 1 && Number(length) <= BUFFER_AT)) {
      body = await request.arrayBuffer();
      again = true;
    }
  }
  const repeatable = REPEATABLE.has(method) || headers.has('idempotency-key');
  const start = (state.answered.get(region.name) ?? 0) % tries;
  let why = '';
  for (let k = 0; k < tries; k++) {
    const i = (start + k) % tries;
    const router = region.routers[i];
    let answer = null;
    try {
      answer = await fetch(router + url.pathname + url.search, {
        method,
        headers,
        body,
        redirect: 'manual',
        // Cancelled with the client: a stream, a held write, a long
        // batch, and the router's connection with them.
        signal: request.signal,
      });
    } catch (e) {
      // Gone: nobody reads what this says.
      if (request.signal?.aborted) return new Response(null, { status: 499 });
      why = String(e?.message ?? e);
    }
    if (answer && !UNREACHED.has(answer.status)) {
      state.answered.set(region.name, i);
      return answer;
    }
    if (answer) {
      why = `Cloudflare answered ${answer.status}`;
      // Read nothing of it, and let its connection go.
      await answer.body?.cancel().catch(() => {});
    }
    console.warn(`region ${region.name}: ${router}: ${why}`);
    // The next request of the region goes to the next router either way.
    state.answered.set(region.name, (i + 1) % tries);
    // A fetch that failed may have reached the router: workerd says the
    // same of a connection refused and of one reset after the request was
    // written. Sent again to the region's next router, a write would reach
    // its node twice -- unless its key makes it once there.
    if (!answer && !repeatable) {
      return own(cfg, request, 502, `region \`${region.name}\`'s router did not answer, and may have run the request (${why}): it is not sent again`);
    }
    if (!again) break;
  }
  return own(cfg, request, 503, `region \`${region.name}\`'s routers did not answer (${why})`, 1);
}

/** {@link forward}, an error it throws -- the client's body lost on its way in, say -- answered as the entry's own, not the runtime's page. */
async function sent(request, url, region, cfg, state) {
  try {
    return await forward(request, url, region, cfg, state);
  } catch (e) {
    if (request.signal?.aborted) return new Response(null, { status: 499 });
    return own(cfg, request, 502, `the entry could not send the request: ${e?.message ?? e}`);
  }
}

/** The answer as the client is given it: the router's, less its hops and its marks. */
function passed(answer) {
  const out = new Response(answer.body, answer);
  for (const h of ANSWER_DROPPED) out.headers.delete(h);
  return out;
}

/** The region a tenant not heard of goes to first: the client's continent's, or the default. */
function nearest(cfg, request) {
  return cfg.byContinent.get(request.cf?.continent) ?? cfg.fallback;
}

/**
 * The Worker: `{ fetch }` over an isolate's {@link State}. `configure` is
 * read once an isolate, and its error answers every request.
 */
export function entry() {
  const state = new State();
  let read = null;
  return {
    async fetch(request, env, ctx) {
      let cfg;
      try {
        if (read?.env !== env) read = { env, cfg: configure(env) };
        cfg = read.cfg;
      } catch (e) {
        return own(null, request, 500, `the entry is not configured: ${e.message}`);
      }
      const url = new URL(request.url);
      const segs = url.pathname.split('/').filter(Boolean);
      if (segs[0] === '_shard' && cfg.admin) return passed(await sent(request, url, cfg.admin, cfg, state));
      // `/_admin/` is a node's, `/_replication` a router's directory for its
      // standbys, `/_metrics` and `/_studio/` the operator's: none of them
      // goes out on the public entry.
      if (segs[0] !== 't' || segs.length < 2) {
        return own(cfg, request, 404, 'the entry serves /t/<tenant>/...');
      }
      const tenant = segs[1];
      if (!TENANT.test(tenant)) return own(cfg, request, 404, `no tenant \`${tenant}\``);
      // A tenant's replication stream is a standby node's, on the private network.
      if (segs[2] === '_replication') return own(cfg, request, 404, 'a tenant\'s replication stream is not served here');
      // A preflight answered here, as a router answers its own: no hop to
      // the region, and a page reads an answer from a region that is down.
      if (request.method === 'OPTIONS' && allows(cfg.cors, request)) {
        return new Response(null, { status: 204, headers: corsHeaders(cfg.cors) });
      }
      let home = state.homes.get(tenant);
      if (home === undefined && cfg.share) {
        home = (await cached(state, url, tenant)) ?? undefined;
        if (home && cfg.regions.has(home)) state.learn(tenant, home, cfg.homes);
      }
      const region = cfg.regions.get(home) ?? nearest(cfg, request);
      const answer = await sent(request, url, region, cfg, state);
      // The tenant's home as the router placing it names it; a region the
      // entry does not know is not one to send anything to.
      const named = answer.headers.get(HOME);
      if (named && named !== home && cfg.regions.has(named) && state.learn(tenant, named, cfg.homes) && cfg.share) {
        ctx?.waitUntil?.(remember(state, url, tenant, named));
      }
      return passed(answer);
    },
  };
}
