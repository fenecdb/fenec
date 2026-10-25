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
// a stale home costs a hop, never an error. A read its client marked for a
// replica (`Fenec-Read: replica`) goes to the region nearest the client
// first whatever the home, whose router answers it from the tenant's read
// replica there or relays it home. So nothing remote is asked on
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
 * <seconds> <mark>`, the mark 16 bytes of HMAC-SHA256 of `fenec-client
 * <address> <seconds>` under the entry's token, base64url ({@link
 * clientMark}), the seconds the Unix time it was made at. Behind the entry
 * every request reaches a router from Cloudflare's addresses, so the
 * router's wait after a refused token would key every client to them. A
 * Worker controls `CF-Connecting-IP` and `X-Real-IP` on what it sends to an
 * origin outside Cloudflare (Cloudflare's "HTTP request headers" page), so
 * any Worker of any account can claim any address from Cloudflare's
 * ranges: a router may believe the address only beside a secret it shares
 * with this entry, never by the peer's address. Bound to the address and
 * the time, a mark seen once names that address alone, and for a minute:
 * a mark of the token alone, as a router's to a node, would let whoever saw
 * one claim any address for good. A router started with the same token
 * (`fenec-shard --entry-token`, `audit::entry`) checks the mark and a time
 * within a minute of its clock, and takes the address for the request's;
 * every router drops the header from what it forwards.
 */
export const CLIENT = 'fenec-client';

/** How many tenants' homes an isolate keeps by default. */
export const HOMES = 50_000;

/**
 * A body up to this is read whole before it goes, so it can go to a next
 * router where the first cannot be reached; a longer one is streamed to
 * one router, its length kept (a router refuses a chunked body, 411), and
 * one sent with no length past it is refused here (411). 1 MiB: what
 * isolates sharing 128 MB hold for a request's retry, against a load's
 * pages of many megabytes and the routers' 64 MiB ceiling.
 */
export const BUFFER_AT = 1 << 20;

/**
 * How long a region whose routers all failed is asked last by a request
 * that may go to two -- a read, a keyed write -- for a tenant whose home is
 * not known; past it such a request asks the region first again, and goes
 * on where it still fails. Any other request asks it last until one of its
 * routers answers, whatever the time: one that may have reached a router
 * cannot be sent again, so sent first to a region still down it was a 502
 * though it never ran (`DOWN_MS` in the environment sets it).
 */
export const DOWN_MS = 10_000;

/** The mark every answer a router writes carries (`fenec-shard`'s `region::SHARD_HEADER`). */
export const SHARD = 'fenec-shard';

/**
 * The mark of a router's 503 written as it took the connection, before it
 * read a byte of the request -- past its `--max-connections`, or at its
 * thread limit (`fenec_http::accept::UNSERVED_HEADER`): the request never
 * ran there, so it goes on to the region's next router whatever it is, and
 * the router is not the one that answered last. Taken as any router's
 * answer, a router at its thread limit kept every request from the standby
 * beside it, where a reset of its connection had sent a read there.
 */
export const UNSERVED = 'fenec-unserved';

/**
 * The mark of a standby router's 409 refusing a `/_shard/` change
 * (`fenec-shard`'s `region::FOLLOWS_HEADER`): it follows another router's
 * directory and ran nothing, so the change goes on to the next router, and
 * the standby is not the one that answered last.
 */
export const FOLLOWS = 'fenec-follows';

/**
 * Answers Cloudflare gives a Worker's fetch where the origin was never
 * reached -- refused (521), unreachable (523), its TLS failed (525, 526),
 * no tunnel connector (530) -- so the router cannot have run the request.
 * Not 522: Cloudflare's page counts a lost acknowledgement of the request's
 * bytes among its time-outs. fenecdb sends none of them.
 */
const UNREACHED = new Set([521, 523, 525, 526, 530]);

/** Methods a router may be sent twice (Go's net/http rule, the router's own towards a node). */
const REPEATABLE = new Set(['GET', 'HEAD', 'OPTIONS']);

/**
 * Whether `request` is a read its client marked for a replica
 * (`Fenec-Read: replica`), as fenec-http's `marks_read` takes the value:
 * every router and node runs it read-only, wherever it lands.
 */
export function marked(request) {
  return request.headers.get('fenec-read')?.trim().toLowerCase() === 'replica';
}

/**
 * Whether `request` may reach a router twice: a method that reads, a key
 * that makes a write once, or -- to a tenant's path, `tenant` -- a read
 * marked for a replica, which runs read-only on every route there. Not to
 * `/_shard/`: the mark is a tenant's read's, and a change of the global
 * router's sent with it ran as any, so one a router may have run was sent
 * to its standby again.
 */
function repeatable(request, tenant) {
  return REPEATABLE.has(request.method) || request.headers.has('idempotency-key') || (tenant && marked(request));
}

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
 * Answer headers never sent back: hop by hop; whether a node verified a
 * token, which a router tells only a router relaying for a client; the
 * tenant's home, which the entry asked for itself and a client did not --
 * a router names it on a refusal too, so to anyone with a tenant's name;
 * and every cookie. No router or node sets one, but Cloudflare Access in
 * front of a router's tunnel does, outside strict service token
 * authentication: a `CF_Authorization` cookie for the service token's
 * session, which passed on would let any client of the entry reach the
 * router's hostname past Access without the token. And {@link UNSERVED},
 * which says what became of the entry's connection to the router, not the
 * client's.
 */
const ANSWER_DROPPED = ['connection', 'keep-alive', 'proxy-connection', 'te', 'trailer', 'transfer-encoding', 'upgrade', 'fenec-verified', HOME, 'set-cookie', UNSERVED];

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
      'content-type, authorization, idempotency-key, fenec-after, fenec-history, fenec-wait, fenec-read, if-none-match, mcp-protocol-version, mcp-method, mcp-name',
    'Access-Control-Expose-Headers':
      'fenec-seq, fenec-history, fenec-next, idempotent-replayed, etag, x-request-id, fenec-quota, retry-after',
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
 *   Set it wherever the routers have `--http-cors`: without it the entry's
 *   own answers -- a region down, a path refused -- carry none, and a page
 *   reads them as the network failing.
 * - `ADMIN`: the global router and its standbys, to send `/_shard/` to;
 *   without it `/_shard/` is a 404 here, as every other path but `/t/` is.
 * - `HOMES`: how many tenants' homes an isolate keeps (50 000).
 * - `HOMES_CACHE`: `off` keeps them in the isolate alone, not shared with
 *   the colo's other isolates through the Cache API.
 * - `DOWN_MS`: {@link DOWN_MS}.
 * - `ENTRY_TOKEN` (a secret): the key of the client's mark ({@link CLIENT});
 *   without it no client is named.
 * - `ACCESS_CLIENT_ID`, `ACCESS_CLIENT_SECRET` (secrets): a Cloudflare
 *   Access service token, sent to every router as `CF-Access-Client-Id` and
 *   `CF-Access-Client-Secret`. A router's tunnel hostname is public; with an
 *   Access application on it that admits this token alone, nothing but the
 *   entry reaches it. Both or neither.
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
  const downMs = env.DOWN_MS === undefined || env.DOWN_MS === '' ? DOWN_MS : Number(env.DOWN_MS);
  if (!Number.isInteger(downMs) || downMs < 0) throw new Error(`DOWN_MS is milliseconds: ${env.DOWN_MS}`);
  // One half of a service token would have Access refuse every request,
  // which reads as every region down: refused here, where it says why.
  if (!env.ACCESS_CLIENT_ID !== !env.ACCESS_CLIENT_SECRET) {
    throw new Error('ACCESS_CLIENT_ID and ACCESS_CLIENT_SECRET are a service token together: set both or neither');
  }
  // The secret goes with every request: never in clear, to an address
  // Access is not even in front of.
  if (env.ACCESS_CLIENT_ID) {
    const routers = [...regions.values()].flatMap((r) => r.routers.map((u) => [`REGIONS.${r.name}`, u]));
    for (const u of [].concat(admin ?? [])) routers.push(['ADMIN', origin(u, 'ADMIN')]);
    for (const [what, u] of routers) {
      if (!u.startsWith('https:')) throw new Error(`${what}: ${u} is not https, and the Access service token would go to it in clear`);
    }
  }
  return {
    regions,
    byContinent,
    fallback: regions.get(fallback),
    cors: env.CORS || null,
    admin: admin ? { name: 'global', routers: [].concat(admin).map((u) => origin(u, 'ADMIN')) } : null,
    homes,
    share: env.HOMES_CACHE !== 'off',
    downMs,
    token: env.ENTRY_TOKEN || null,
    // The token's HMAC key, imported at the first request naming a client.
    key: null,
    access: env.ACCESS_CLIENT_ID ? { id: env.ACCESS_CLIENT_ID, secret: env.ACCESS_CLIENT_SECRET } : null,
  };
}

/** base64url without padding, as fenec-http's `b64url_encode`. */
function b64url(bytes) {
  let s = '';
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/=+$/, '').replace(/\+/g, '-').replace(/\//g, '_');
}

const enc = new TextEncoder();

/** The HMAC key the client's mark is made under. */
export function markKey(token) {
  return crypto.subtle.importKey('raw', enc.encode(token), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
}

/** The mark of `address` at `seconds`: 16 bytes of HMAC-SHA256 of `fenec-client <address> <seconds>`, base64url. */
export async function clientMark(key, address, seconds) {
  const mac = new Uint8Array(await crypto.subtle.sign('HMAC', await key, enc.encode(`fenec-client ${address} ${seconds}`)));
  return b64url(mac.subarray(0, 16));
}

/**
 * The headers a request goes to a router with: `client` the {@link CLIENT}
 * header's value, where there is one, and `access` the Access service
 * token, where there is one -- a client's own `CF-Access-*` dropped with
 * every `cf-*` header, so no client sends the entry's. A header a client
 * names in `Connection` cannot be told apart here -- the runtime keeps
 * `Connection` from the Worker -- and goes on: a router reads no header it
 * does not know.
 */
export function sentHeaders(incoming, client, access = null) {
  const out = new Headers();
  for (const [k, v] of incoming) {
    if (DROPPED.has(k) || k.startsWith('cf-')) continue;
    out.append(k, v);
  }
  out.set(HOME, '1');
  if (client) out.set(CLIENT, client);
  if (access) {
    out.set('CF-Access-Client-Id', access.id);
    out.set('CF-Access-Client-Secret', access.secret);
  }
  // The router takes a client's request id, or makes one: Cloudflare's ray
  // id, which every Cloudflare log of the request carries, where the client
  // sent none.
  if (!out.has('x-request-id') && incoming.has('cf-ray')) out.set('x-request-id', incoming.get('cf-ray'));
  return out;
}

/** The {@link CLIENT} header's value: the client's address as Cloudflare saw it -- the client cannot set it -- marked now. */
async function clientNamed(cfg, incoming) {
  const ip = incoming.get('cf-connecting-ip');
  if (!cfg.token || !ip || /\s/.test(ip)) return null;
  cfg.key ??= markKey(cfg.token);
  const seconds = Math.floor(Date.now() / 1000);
  return `${ip} ${seconds} ${await clientMark(cfg.key, ip, seconds)}`;
}

/**
 * A path decoded as fenec-http decodes one (`http::percent_decode`): `%XX`
 * a byte, `+` a space, an invalid sequence left as it is, the bytes read as
 * UTF-8. The router and the node route by the path so decoded, from the
 * target the entry sends as it came, so the entry judges a path by the
 * segments they will see: `/t/acme/%5Freplication` is a tenant's
 * replication stream to them, which WHATWG's URL leaves encoded.
 */
export function percentDecode(s) {
  const b = enc.encode(s);
  const out = new Uint8Array(b.length);
  let n = 0;
  const hex = (c) => (c >= 48 && c <= 57 ? c - 48 : c >= 65 && c <= 70 ? c - 55 : c >= 97 && c <= 102 ? c - 87 : -1);
  for (let i = 0; i < b.length; ) {
    if (b[i] === 0x2b) {
      out[n++] = 0x20;
      i += 1;
    } else if (b[i] === 0x25 && i + 2 < b.length && hex(b[i + 1]) >= 0 && hex(b[i + 2]) >= 0) {
      out[n++] = (hex(b[i + 1]) << 4) | hex(b[i + 2]);
      i += 3;
    } else {
      out[n++] = b[i++];
    }
  }
  return new TextDecoder().decode(out.subarray(0, n));
}

/** The segments of a path as fenec-http's `Request::segments` has them, decoded first. */
export function segments(pathname) {
  return percentDecode(pathname).split('/').filter(Boolean);
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
 * Where an isolate's requests go: the tenants' homes it has heard, the
 * router of each region that answered last, tried first, and when each
 * region last had no router answer.
 */
export class State {
  constructor() {
    this.homes = new Map();
    this.answered = new Map();
    this.down = new Map();
    this.cache = null;
    this.warned = false;
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

/** A request the entry refuses before sending it anywhere. */
class Refusal extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

/** A body sent with no length, read up to `cap`; null past it. */
async function readUpTo(stream, cap) {
  const reader = stream.getReader();
  const parts = [];
  let n = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    n += value.byteLength;
    if (n > cap) {
      await reader.cancel().catch(() => {});
      return null;
    }
    parts.push(value);
  }
  const out = new Uint8Array(n);
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.byteLength;
  }
  return out;
}

/**
 * What a request goes to the routers with, made once however many it is
 * tried at: its headers, its body, whether the body can go again, and
 * whether the request may run twice.
 */
async function prepare(request, cfg, tenant) {
  const headers = sentHeaders(request.headers, await clientNamed(cfg, request.headers), cfg.access);
  const method = request.method;
  let body = null;
  let again = true;
  if (method !== 'GET' && method !== 'HEAD' && request.body !== null) {
    const length = request.headers.get('content-length');
    if (length === null) {
      // A router takes no chunked body, so it is read here: up to what is
      // kept for a retry, which also bounds what the isolates' 128 MB hold.
      body = await readUpTo(request.body, BUFFER_AT);
      if (body === null) throw new Refusal(411, `a body of more than ${BUFFER_AT} bytes is sent with its Content-Length`);
    } else if (Number(length) <= BUFFER_AT) {
      body = await request.arrayBuffer();
    } else {
      body = request.body;
      again = false;
    }
  }
  return { method, headers, body, again, repeatable: repeatable(request, tenant) };
}

/**
 * How an answer at a router's URL is taken: the router's (`router`), one
 * Cloudflare gives for a router never reached (`unrun`), the router's 503
 * written as it took the connection, the request never read (`unserved`,
 * {@link UNSERVED}), or one something else in front of the router gave --
 * a tunnel's connector that could not reach it answers 502, Cloudflare
 * 520, 522 or 524 -- which may have reached it (`unknown`). Every answer a
 * router writes, its own and those it forwards, carries its mark ({@link
 * SHARD}), which nothing else in front of it writes: `X-Request-Id`, which
 * every answer of a router carries too, a load balancer may write on its
 * own 502.
 */
export function judged(answer) {
  if (UNREACHED.has(answer.status)) return 'unrun';
  if (answer.status >= 500 && !answer.headers.has(SHARD)) return 'unknown';
  if (answer.status === 503 && answer.headers.has(UNSERVED)) return 'unserved';
  return 'router';
}

/**
 * Sends `sent` to `region`'s routers, from the one that answered last,
 * moving to the next only where the one asked cannot have run it, or where
 * running it twice does no harm: `{answer}`, `{gone}` where the client
 * went, or why none answered -- `fail` `resend` where the request may go to
 * another region, `ran` where it may have run, `spent` where its body
 * cannot go again. `guess`: the region is one the tenant's home is guessed
 * to be reached through, and another follows it. What went wrong with a
 * router is logged with the router named, and told the client without it:
 * the routers' hostnames are the operator's.
 */
async function toRegion(request, url, region, sent, state, guess) {
  const tries = region.routers.length;
  const start = (state.answered.get(region.name) ?? 0) % tries;
  let why = '';
  // A router's 503 written as it took the connection ({@link UNSERVED}),
  // and a standby's 409 refusing a change ({@link FOLLOWS}): neither ran
  // the request, so the next router is asked, and the 503 -- or else the
  // 409 -- is the client's answer where no router does better.
  let refused = null;
  let following = null;
  const drop = async (a) => a?.body?.cancel().catch(() => {});
  const best = async () => {
    if (!refused) return following;
    await drop(following);
    return refused;
  };
  for (let k = 0; k < tries; k++) {
    const i = (start + k) % tries;
    const router = region.routers[i];
    let answer = null;
    let kind = 'unknown';
    try {
      answer = await fetch(router + url.pathname + url.search, {
        method: sent.method,
        headers: sent.headers,
        body: sent.body,
        redirect: 'manual',
        // Cancelled with the client: a stream, a held write, a long
        // batch, and the router's connection with them.
        signal: request.signal,
      });
    } catch (e) {
      if (request.signal?.aborted) return { gone: true };
      why = String(e?.message ?? e);
    }
    if (answer) {
      kind = judged(answer);
      // A standby router refusing a `/_shard/` change it cannot take: taken
      // for the router that answered, a change meeting the busy primary's
      // 503 went on to the standby, was its 409 -- telling an operator to
      // promote a standby beside a living primary -- and every change after
      // it from the isolate went to the standby first, the primary never
      // asked again.
      if (kind === 'router' && answer.headers.has(FOLLOWS)) kind = 'follows';
      if (kind === 'router') {
        await drop(refused);
        await drop(following);
        state.answered.set(region.name, i);
        state.down.delete(region.name);
        // A router that could not reach the tenant's home -- it relays to
        // another region, cut off from this one -- answers 5xx naming no
        // home; a read goes on to the next region, whose router may reach
        // it. The home's own failure names the home, and is the answer.
        if (guess && answer.status >= 500 && !answer.headers.has(HOME) && sent.repeatable && sent.again) {
          console.warn(`region ${region.name}: ${router}: answered ${answer.status}, naming no home`);
          return { fail: 'resend', why: `answered ${answer.status}`, answer };
        }
        return { answer };
      }
      if (kind === 'unserved') {
        why = `refused the connection unserved (${answer.headers.get(UNSERVED)})`;
        await drop(refused);
        refused = answer;
      } else if (kind === 'follows') {
        why = 'follows another router';
        await drop(following);
        following = answer;
      } else {
        why = `answered ${answer.status}, not by the router`;
        // Read nothing of it, and let its connection go.
        await drop(answer);
      }
    }
    console.warn(`region ${region.name}: ${router}: ${why}`);
    // The next request of the region goes to the next router either way.
    state.answered.set(region.name, (i + 1) % tries);
    // A fetch that failed may have reached the router: workerd says the
    // same of a connection refused and of one reset after the request was
    // written. Sent again, a write would reach its node twice -- unless its
    // key makes it once there.
    if (kind === 'unknown' && !sent.repeatable) {
      await drop(refused);
      await drop(following);
      state.down.set(region.name, Date.now());
      return { fail: 'ran', why };
    }
    if (!sent.again) {
      // A router there refused it, unrun: its answer, a 503 to come back
      // from where one was given.
      if (kind === 'unserved' || kind === 'follows') return { answer: await best() };
      await drop(refused);
      await drop(following);
      state.down.set(region.name, Date.now());
      return { fail: 'spent', why };
    }
  }
  // A router there, refusing: the region is not down, and a guess goes on
  // to the next region for a 503 unserved, whose router relays home a
  // request that ran nowhere; a standby's 409 is a `/_shard/` change's,
  // never a guess's.
  if (refused || following) {
    const answer = await best();
    return guess && answer === refused ? { fail: 'resend', why, answer } : { answer };
  }
  state.down.set(region.name, Date.now());
  return { fail: 'resend', why };
}

/**
 * Sends `request` to the first of `regions` whose routers answer: its
 * answer, or the entry's own. A request goes on to the next region only
 * where none of this one's routers can have run it or it may run twice --
 * `regions` is one, the tenant's home, wherever the home is known, but for
 * a read marked for a replica ({@link near}). `tenant` where the path is a
 * tenant's, not the global router's `/_shard/`.
 */
async function send(request, url, regions, cfg, state, tenant) {
  let sent;
  try {
    sent = await prepare(request, cfg, tenant);
  } catch (e) {
    if (request.signal?.aborted) return new Response(null, { status: 499 });
    if (e instanceof Refusal) return own(cfg, request, e.status, e.message);
    // The client's body lost on its way in, say: the entry's own answer,
    // not the runtime's page.
    return own(cfg, request, 502, `the entry could not read the request: ${e?.message ?? e}`);
  }
  let why = '';
  // A router's answer a guessed region gave and the next was asked past:
  // the client's, where no region does better.
  let kept = null;
  for (let n = 0; n < regions.length; n++) {
    const region = regions[n];
    const r = await toRegion(request, url, region, sent, state, n + 1 < regions.length);
    // Gone: nobody reads what this says.
    if (r.gone) return new Response(null, { status: 499 });
    if (r.answer && !r.fail) {
      await kept?.body?.cancel().catch(() => {});
      return r.answer;
    }
    if (r.answer) {
      await kept?.body?.cancel().catch(() => {});
      kept = r.answer;
    }
    why = r.why;
    if (r.fail === 'ran') {
      await kept?.body?.cancel().catch(() => {});
      return own(cfg, request, 502, `region \`${region.name}\`'s router did not answer, and may have run the request (${why}): it is not sent again`);
    }
    if (r.fail === 'spent') {
      await kept?.body?.cancel().catch(() => {});
      return own(cfg, request, 503, `region \`${region.name}\`'s routers did not answer (${why})`, 1);
    }
  }
  if (kept) return kept;
  const names = regions.map((r) => `\`${r.name}\``).join(', ');
  return own(cfg, request, 503, `region ${names}'s routers did not answer (${why})`, 1);
}

/** The answer as the client is given it: the router's, less its hops, its marks and any cookie. */
export function passed(answer) {
  const out = new Response(answer.body, answer);
  for (const h of ANSWER_DROPPED) out.headers.delete(h);
  return out;
}

/**
 * The regions a tenant whose home is not known is sent to, in turn: the
 * client's continent's or the default first, then the rest -- any region's
 * router relays a tenant home, and only the home's places it on a node, so
 * no write lands anywhere else -- each whose routers last failed asked
 * after the others: by a request that may reach two routers for {@link
 * DOWN_MS} after the failure, by any other until one of its routers
 * answers.
 */
function guesses(cfg, request, state) {
  const first = cfg.byContinent.get(request.cf?.continent) ?? cfg.fallback;
  const all = [first, ...[...cfg.regions.values()].filter((r) => r !== first)];
  const now = Date.now();
  const again = repeatable(request, true);
  const isDown = (r) => state.down.has(r.name) && (!again || now - state.down.get(r.name) < cfg.downMs);
  return [...all.filter((r) => !isDown(r)), ...all.filter(isDown)];
}

/**
 * The regions a read marked for a replica of a tenant at home in `home` is
 * sent to: the region of the client's continent first, whose router
 * answers it from the tenant's read replica there or relays it home -- a
 * hop, where it keeps none -- then the home, where that region's routers
 * fail or answer a 5xx naming no home. A marked read runs read-only
 * wherever it lands, so sending it to two is safe. Sent to the known home
 * alone, every marked read went to the home once one unmarked request had
 * taught the colo where that was, and the replica near the client was
 * never asked. A continent no region names goes home alone: the default
 * region is where a tenant not heard of is guessed, no nearer to such a
 * client than the home -- an Asian client's read of a us tenant went
 * through eu, where it had no replica, to us. A region found down is
 * passed over for {@link DOWN_MS}.
 */
function near(cfg, request, state, home) {
  const nearest = cfg.byContinent.get(request.cf?.continent);
  if (!nearest || nearest === home) return [home];
  const down = state.down.has(nearest.name) && Date.now() - state.down.get(nearest.name) < cfg.downMs;
  return down ? [home] : [nearest, home];
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
      // As the router and the node will read the path: decoded.
      const segs = segments(url.pathname);
      if (segs[0] === '_shard' && cfg.admin) return passed(await send(request, url, [cfg.admin], cfg, state, false));
      // `/_admin/` is a node's, `/_replication` a router's directory for its
      // standbys, `/_metrics` and `/_studio/` the operator's: none of them
      // goes out on the public entry.
      if (segs[0] !== 't' || segs.length < 2) {
        return own(cfg, request, 404, 'the entry serves /t/<tenant>/...');
      }
      const tenant = segs[1];
      if (!TENANT.test(tenant)) return own(cfg, request, 404, `no tenant \`${tenant}\``);
      // A tenant's replication stream is a standby node's, on the private network.
      if (segs[2] === '_replication') return own(cfg, request, 404, "a tenant's replication stream is not served here");
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
      // A known home alone: another region's router would relay to the
      // same routers -- but for a read marked for a replica, which the
      // nearest region's router may answer itself. Not known, every region
      // in turn.
      const known = cfg.regions.get(home);
      const regions = !known ? guesses(cfg, request, state) : marked(request) ? near(cfg, request, state, known) : [known];
      const answer = await send(request, url, regions, cfg, state, true);
      // The tenant's home as the router placing it names it; a region the
      // entry does not know is not one to send anything to.
      const named = answer.headers.get(HOME);
      if (named && named !== home && cfg.regions.has(named) && state.learn(tenant, named, cfg.homes) && cfg.share) {
        ctx?.waitUntil?.(remember(state, url, tenant, named));
      }
      if (!cfg.cors && !state.warned && answer.headers.has('access-control-allow-origin')) {
        state.warned = true;
        console.warn(
          'the routers send CORS headers and CORS is not set: the entry answers no preflight, and its own answers ' +
            '(a region down, a path refused) carry none, which a page reads as the network failing. Set CORS to their --http-cors.',
        );
      }
      return passed(answer);
    },
  };
}
