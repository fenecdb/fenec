// Kestrel's server: the ingest endpoint, the dashboard and the market page,
// on node:http with no framework, and -- unless told otherwise -- the rollup
// workers, the market feed and a trickle of demo traffic beside them.
//
//   PORT                     3000
//   KESTREL_ROLLUPS=0        run no rollup workers here (scripts/rollup.ts runs them apart)
//   KESTREL_ROLLUP_SITES     the sites whose workers run here, by commas (default: every site)
//   KESTREL_FEED=0           write no ticks
//   KESTREL_DEMO_TRAFFIC=0   send no demo beacons
//
// Why no framework: the dashboard is server-rendered HTML and SVG with a
// few kilobytes of script for the live parts. An empty Next.js App Router
// page ships 132 KB of compressed JavaScript before any of its own (the
// shop example measured it); this one ships under 5 KB, and its first
// paint needs none.
import { existsSync, readFileSync } from 'node:fs';
import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import { fileURLToPath } from 'node:url';
import zlib, { brotliCompressSync, gzipSync } from 'node:zlib';
import { INSECURE_COOKIES, SESSION_SECRET } from './config.ts';
import { agent, country, MAX_BODY, parseBeacon, RateLimit, Refused, write } from './ingest.ts';
import { pulses, quotes } from './live.ts';
import { bars, runFeed, SYMBOLS, windowRows } from './market.ts';
import { dashboard, FACETS, isRange, noFilters, type Filters } from './queries.ts';
import { RollupWorker } from './rollup.ts';
import { sign, unsign } from './tokens.ts';
import { startDemoTraffic } from './traffic.ts';
import { signIn, Sites, user } from './users.ts';
import { CSP, dashboardPage, INTERVALS, marketsPage, quoteRows, signInPage, WINDOWS } from './views.ts';

const PORT = Number(process.env.PORT ?? 3000);
const pub = (f: string) => fileURLToPath(new URL(`../public/${f}`, import.meta.url));
const sites = new Sites();
const limits = new RateLimit();

export const stats = { beacons: 0, events: 0, written: 0, duplicates: 0, refused: 0, limited: 0 };

/**
 * The body compressed as the browser asks: brotli at a quality that costs
 * a page about a millisecond, else gzip. A dashboard's HTML is 38 to 47 KB
 * as text, 7.5 to 9 brotli -- on a phone's slow link that is most of its time.
 */
function compressed(req: IncomingMessage | undefined, body: Buffer): { body: Buffer; encoding?: string } {
  const accept = String(req?.headers['accept-encoding'] ?? '');
  if (body.length < 1024) return { body };
  if (/\bbr\b/.test(accept)) return { body: brotliCompressSync(body, { params: { [zlib.constants.BROTLI_PARAM_QUALITY]: 5 } }), encoding: 'br' };
  if (/\bgzip\b/.test(accept)) return { body: gzipSync(body, { level: 6 }), encoding: 'gzip' };
  return { body };
}

function send(res: ServerResponse, status: number, body: string, type = 'text/html; charset=utf-8', headers: Record<string, string> = {}) {
  const c = compressed(res.req, Buffer.from(body));
  res.writeHead(status, {
    'content-type': type,
    'content-security-policy': CSP,
    'x-content-type-options': 'nosniff',
    'referrer-policy': 'same-origin',
    vary: 'Accept-Encoding',
    ...(c.encoding ? { 'content-encoding': c.encoding } : {}),
    ...headers,
  });
  res.end(c.body);
}

/** A built file, read once, with its compressed forms made once. */
const files = new Map<string, { raw: Buffer; br: Buffer; gzip: Buffer }>();
function file(path: string) {
  let f = files.get(path);
  if (!f) {
    const raw = readFileSync(path);
    f = { raw, br: brotliCompressSync(raw), gzip: gzipSync(raw, { level: 9 }) };
    files.set(path, f);
  }
  return f;
}

const redirect = (res: ServerResponse, to: string, headers: Record<string, string> = {}) => {
  res.writeHead(303, { location: to, ...headers });
  res.end();
};

function cookie(req: IncomingMessage, name: string): string | undefined {
  for (const part of (req.headers.cookie ?? '').split(';')) {
    const [k, ...v] = part.trim().split('=');
    if (k === name) return decodeURIComponent(v.join('='));
  }
  return undefined;
}

async function signedIn(req: IncomingMessage) {
  const name = unsign(cookie(req, 'kestrel'), SESSION_SECRET);
  return name ? user(name) : null;
}

/** A form post from this site only: a cross-site form cannot sign anyone in or out. */
function sameOrigin(req: IncomingMessage): boolean {
  const o = req.headers.origin;
  return !o || o === `http://${req.headers.host}` || o === `https://${req.headers.host}`;
}

async function body(req: IncomingMessage, max: number): Promise<string> {
  let size = 0;
  const parts: Buffer[] = [];
  for await (const chunk of req) {
    size += chunk.length;
    if (size > max) throw new Refused(413, `a body is ${max} bytes at most`);
    parts.push(chunk as Buffer);
  }
  return Buffer.concat(parts).toString('utf8');
}

function readFilters(u: URL): Filters {
  const f = noFilters();
  for (const k of FACETS) f[k] = [...new Set(u.searchParams.getAll(k).filter((v) => /^[A-Za-z0-9 _.-]{1,40}$/.test(v)))].slice(0, 20);
  return f;
}

/** POST /e: a beacon. Answers 204, or the refusal's status and why. */
async function ingest(req: IncomingMessage, res: ServerResponse) {
  const origin = req.headers.origin ?? '';
  const cors = (allowed: boolean): Record<string, string> => (allowed && origin ? { 'access-control-allow-origin': origin, vary: 'Origin' } : {});
  try {
    if (Number(req.headers['content-length'] ?? 0) > MAX_BODY) throw new Refused(413, `a beacon is ${MAX_BODY} bytes at most`);
    const b = parseBeacon(await body(req, MAX_BODY));
    const site = await sites.byKey(b.key);
    if (!site) throw new Refused(404, 'no site has this key');
    if (!site.origins.includes(origin)) {
      stats.refused++;
      return send(res, 403, 'this site takes beacons from its own pages only', 'text/plain');
    }
    const ua = agent(String(req.headers['user-agent'] ?? ''));
    if (!ua) return send(res, 204, '', 'text/plain', cors(true)); // a crawler: not a visit
    if (!limits.take(site.name, b.events.length, site.rate)) {
      stats.limited++;
      return send(res, 429, 'too many events from this site: slow down', 'text/plain', { ...cors(true), 'retry-after': '1' });
    }
    const hosts = site.origins.map((o) => new URL(o).hostname.replace(/^www\./, ''));
    const w = await write(site.name, b, { ...ua, country: country(req.headers), hosts });
    stats.beacons++;
    stats.events += b.events.length;
    stats.written += w.written;
    if (w.duplicate) stats.duplicates++;
    send(res, 204, '', 'text/plain', cors(true));
  } catch (e) {
    if (e instanceof Refused) {
      stats.refused++;
      return send(res, e.status, e.message, 'text/plain', cors(true));
    }
    console.error('ingest:', e);
    send(res, 503, 'the beacon could not be written: send it again', 'text/plain', cors(true));
  }
}

const statics: Record<string, { file: string; type: string; cache: string }> = {
  '/app.js': { file: pub('app.js'), type: 'text/javascript; charset=utf-8', cache: 'public, max-age=300' },
  '/k.js': { file: pub('k.js'), type: 'text/javascript; charset=utf-8', cache: 'public, max-age=3600' },
  '/fonts/archivo.woff2': { file: pub('fonts/archivo.woff2'), type: 'font/woff2', cache: 'public, max-age=31536000, immutable' },
};

const FAVICON =
  '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><path fill="#46678a" d="M3 15c5-1 9-4 12-8 1 3 3 5 6 6l8-3-5 6c1 4-1 9-6 11l1 4-4-3c-5 0-9-3-10-7l-4 1 2-3c-1-1-1-2 0-4Z"/></svg>';

async function marketPage(req: IncomingMessage, res: ServerResponse, u: URL) {
  const t0 = performance.now();
  const sym = SYMBOLS.some((s) => s.sym === u.searchParams.get('sym')) ? (u.searchParams.get('sym') as string) : SYMBOLS[0].sym;
  const window = WINDOWS.includes(Number(u.searchParams.get('window')) as (typeof WINDOWS)[number]) ? Number(u.searchParams.get('window')) : 60;
  const interval = INTERVALS.includes(u.searchParams.get('interval') as (typeof INTERVALS)[number]) ? (u.searchParams.get('interval') as string) : '1m';
  const ms: Record<string, number> = {};
  const timed = async <T>(k: string, p: Promise<T>) => {
    const t = performance.now();
    const v = await p;
    ms[k] = performance.now() - t;
    return v;
  };
  const [rows, b, q, who] = await Promise.all([
    timed('window', windowRows(window)),
    timed('bars', bars(sym, window, interval)),
    quotes.first('all'),
    signedIn(req),
  ]);
  const mine = who ? (await sites.all()).filter((s) => who.sites.includes(s.name)) : [];
  send(res, 200, marketsPage({ user: who ?? undefined, sites: mine, sym, window, interval, rows, quotes: q ?? new Map(), bars: b, ms }), undefined, {
    'cache-control': 'no-store',
    'server-timing': `db;dur=${Math.max(ms.window, ms.bars).toFixed(1)}, total;dur=${(performance.now() - t0).toFixed(1)}`,
  });
}

export async function handle(req: IncomingMessage, res: ServerResponse) {
  const u = new URL(req.url ?? '/', 'http://x');
  const path = u.pathname;
  try {
    if (path === '/e') {
      if (req.method === 'OPTIONS') {
        return send(res, 204, '', 'text/plain', {
          'access-control-allow-origin': req.headers.origin ?? '*',
          'access-control-allow-methods': 'POST',
          'access-control-allow-headers': 'content-type',
          'access-control-max-age': '86400',
        });
      }
      if (req.method !== 'POST') return send(res, 405, 'beacons are posted', 'text/plain', { allow: 'POST, OPTIONS' });
      return await ingest(req, res);
    }
    const st = statics[path];
    if (st && req.method === 'GET') {
      if (!existsSync(st.file)) return send(res, 404, 'not built: npm run build', 'text/plain');
      const f = file(st.file);
      const accept = String(req.headers['accept-encoding'] ?? '');
      // A font is compressed already.
      const enc = st.type.startsWith('font/') ? '' : /\bbr\b/.test(accept) ? 'br' : /\bgzip\b/.test(accept) ? 'gzip' : '';
      res.writeHead(200, {
        'content-type': st.type,
        'cache-control': st.cache,
        vary: 'Accept-Encoding',
        ...(enc ? { 'content-encoding': enc } : {}),
        ...(path === '/k.js' ? { 'access-control-allow-origin': '*' } : {}),
      });
      return void res.end(enc === 'br' ? f.br : enc === 'gzip' ? f.gzip : f.raw);
    }
    if (path === '/favicon.svg') return send(res, 200, FAVICON, 'image/svg+xml', { 'cache-control': 'public, max-age=86400' });
    if (path === '/robots.txt') return send(res, 200, 'User-agent: *\nDisallow: /s/\nAllow: /\n', 'text/plain');
    if (path === '/markets') return await marketPage(req, res, u);
    if (path === '/markets/quotes.json') {
      const { value, version } = quotes.current('all');
      const etag = `"q${version}"`;
      if (req.headers['if-none-match'] === etag) {
        res.writeHead(304, { etag });
        return void res.end();
      }
      return send(res, 200, JSON.stringify(value ? [...value.values()] : []), 'application/json', { etag, 'cache-control': 'no-cache' });
    }
    if (path === '/markets/bars.json') {
      const sym = u.searchParams.get('sym') ?? '';
      const window = Number(u.searchParams.get('window'));
      const interval = u.searchParams.get('interval') ?? '';
      if (!SYMBOLS.some((s) => s.sym === sym) || !WINDOWS.includes(window as (typeof WINDOWS)[number]) || !INTERVALS.includes(interval as (typeof INTERVALS)[number])) {
        return send(res, 400, 'sym, window and interval are the page’s', 'text/plain');
      }
      const [b, rows] = await Promise.all([bars(sym, window, interval), windowRows(window)]);
      const q = quotes.current('all').value ?? new Map();
      return send(res, 200, JSON.stringify({ bars: b, rows: quoteRows(rows, q, sym, window, interval) }), 'application/json', { 'cache-control': 'no-store' });
    }
    if (path === '/' && req.method === 'GET') {
      const who = await signedIn(req);
      if (who?.sites.length) return redirect(res, `/s/${who.sites[0]}`);
      return send(res, 200, signInPage());
    }
    // The sign-in page whoever asks: to sign in as someone else.
    if (path === '/signin' && req.method === 'GET') return send(res, 200, signInPage());
    if (path === '/signin' && req.method === 'POST') {
      if (!sameOrigin(req)) return send(res, 403, 'a sign-in comes from this site', 'text/plain');
      const form = new URLSearchParams(await body(req, 4096));
      const who = await signIn(form.get('name') ?? '', form.get('password') ?? '');
      if (!who) return send(res, 401, signInPage('That name and password do not match. Check both and try again.'));
      const secure = INSECURE_COOKIES ? '' : '; Secure';
      return redirect(res, who.sites.length ? `/s/${who.sites[0]}` : '/markets', {
        'set-cookie': `kestrel=${encodeURIComponent(sign(who.name, SESSION_SECRET))}; Path=/; HttpOnly; SameSite=Lax; Max-Age=43200${secure}`,
      });
    }
    if (path === '/signout' && req.method === 'POST') {
      if (!sameOrigin(req)) return send(res, 403, 'a sign-out comes from this site', 'text/plain');
      return redirect(res, '/', { 'set-cookie': 'kestrel=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0' });
    }
    const m = /^\/s\/([a-z0-9-]{1,40})(\/now\.json)?$/.exec(path);
    if (m && req.method === 'GET') {
      const t0 = performance.now();
      const who = await signedIn(req);
      if (!who) return redirect(res, '/');
      // Another person's site is not found, not forbidden: its name says nothing.
      const site = who.sites.includes(m[1]) ? await sites.byName(m[1]) : undefined;
      if (!site) return send(res, 404, 'No site of yours has this name.', 'text/plain');
      if (m[2]) {
        const { value, version } = pulses.current(site.name);
        const etag = `"p${version}"`;
        if (req.headers['if-none-match'] === etag) {
          res.writeHead(304, { etag });
          return void res.end();
        }
        return send(res, 200, JSON.stringify(value), 'application/json', { etag, 'cache-control': 'private, no-cache' });
      }
      const range = u.searchParams.get('range');
      const filters = readFilters(u);
      const [d, pulse, all] = await Promise.all([dashboard(site.name, isRange(range) ? range : '24h', filters), pulses.first(site.name, 300), sites.all()]);
      const mine = all.filter((s) => who.sites.includes(s.name));
      const ms = performance.now() - t0;
      return send(res, 200, dashboardPage({ site, sites: mine, user: who, d, filters, pulse, ms }), undefined, {
        'cache-control': 'private, no-store',
        'x-robots-tag': 'noindex',
        'server-timing': `db;dur=${Math.max(...Object.values(d.timings)).toFixed(1)}, total;dur=${ms.toFixed(1)}`,
      });
    }
    send(res, 404, 'Not found.', 'text/plain');
  } catch (e) {
    console.error(req.method, path, e);
    if (!res.headersSent) send(res, 500, 'Something went wrong on our side. Try again in a moment.', 'text/plain');
    else res.end();
  }
}

/** The rollup workers, one a site, kept running and joined by new sites. */
function startRollups() {
  const running = new Map<string, RollupWorker>();
  const only = process.env.KESTREL_ROLLUP_SITES?.split(',').filter(Boolean);
  const tend = async () => {
    try {
      for (const s of await sites.all()) {
        if (running.has(s.name) || (only && !only.includes(s.name))) continue;
        const w = new RollupWorker({ site: s.name, onError: (e) => console.error(`rollup ${s.name}:`, (e as Error).message ?? e) });
        running.set(s.name, w);
        void w.run();
      }
    } catch (e) {
      console.error('rollups:', (e as Error).message);
    }
  };
  void tend();
  setInterval(tend, 30_000).unref();
}

const isMain = process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1];
if (isMain) {
  // A request's failure is that request's: logged, never the process's end.
  process.on('unhandledRejection', (e) => console.error('unhandled:', e));
  if (!existsSync(pub('app.js'))) console.warn('public/app.js is missing: npm run build');
  const server = createServer({ keepAliveTimeout: 65_000 }, (req, res) => void handle(req, res));
  server.listen(PORT, () => console.log(`Kestrel on http://127.0.0.1:${PORT}`));
  if (process.env.KESTREL_ROLLUPS !== '0') startRollups();
  if (process.env.KESTREL_FEED !== '0') runFeed({ onError: (e) => console.error('feed:', (e as Error).message) });
  if (process.env.KESTREL_DEMO_TRAFFIC !== '0') startDemoTraffic(`http://127.0.0.1:${PORT}`);
}
