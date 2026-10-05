// The ingest endpoint's work: a beacon read, checked, limited, and written
// as one block under the site's ingest token.
//
// A beacon is one browser's batch of events, as the tracker (client/
// tracker.ts) sends it: `{k: site key, b: batch id, u: visitor id, t: the
// browser's clock as it sent, e: [{n: name, p: path, r: referrer, t: when,
// d: properties}]}`. It is written with `Idempotency-Key: <site>:<batch>`,
// so a retry of a batch that landed is answered and not written again, and
// each event's id is `<batch>:<place>`, `@unique` and written `if absent`,
// so a batch sent again under another key is not counted twice either.
import { FenecError } from '@fenecdb/web/client';
import { COUNTRY_HEADER } from './config.ts';
import { db } from './db.ts';

export const MAX_BODY = 16 * 1024;
export const MAX_EVENTS = 50;
/** How old an event may say it is: the tracker holds events a few seconds, a retry up to a minute. */
export const MAX_AGE_MS = 3_600_000;

export interface Beacon {
  key: string;
  batch: string;
  user: string;
  events: { name: string; path: string; ref: string; age: number; props: Record<string, string | number | boolean> | null }[];
}

export class Refused extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
  }
}

const ID = /^[A-Za-z0-9_-]{8,40}$/;
const NAME = /^[a-z][a-z0-9_]{0,31}$/;
// eslint-disable-next-line no-control-regex
const CONTROL = /[\u0000-\u001f\u007f]/;

/** The beacon in `body`, every field checked, or a `Refused` naming the first that is not. */
export function parseBeacon(body: string): Beacon {
  if (Buffer.byteLength(body) > MAX_BODY) throw new Refused(413, `a beacon is ${MAX_BODY} bytes at most`);
  let v: unknown;
  try {
    v = JSON.parse(body);
  } catch {
    throw new Refused(400, 'a beacon is JSON');
  }
  if (!isObject(v)) throw new Refused(400, 'a beacon is a JSON object');
  const { k, b, u, t, e } = v;
  if (typeof k !== 'string' || !/^[a-z0-9]{8,40}$/.test(k)) throw new Refused(400, '`k` is the site key');
  if (typeof b !== 'string' || !ID.test(b)) throw new Refused(400, '`b` is the batch id: 8 to 40 of A-Z a-z 0-9 _ -');
  if (typeof u !== 'string' || !ID.test(u)) throw new Refused(400, '`u` is the visitor id: 8 to 40 of A-Z a-z 0-9 _ -');
  if (typeof t !== 'number' || !Number.isFinite(t)) throw new Refused(400, '`t` is the time the beacon was sent, in ms');
  if (!Array.isArray(e) || e.length === 0) throw new Refused(400, '`e` is a list of events');
  if (e.length > MAX_EVENTS) throw new Refused(413, `a beacon holds ${MAX_EVENTS} events at most`);
  const events = e.map((x, i) => {
    if (!isObject(x)) throw new Refused(400, `event ${i} is not an object`);
    const { n, p, r, t: et, d } = x;
    if (typeof n !== 'string' || !NAME.test(n)) throw new Refused(400, `event ${i}: a name is a-z, 0-9 and _, 32 at most`);
    if (typeof p !== 'string' || !p.startsWith('/') || p.length > 256 || CONTROL.test(p)) {
      throw new Refused(400, `event ${i}: a path starts with / and is 256 characters at most`);
    }
    if (r !== undefined && (typeof r !== 'string' || r.length > 512)) throw new Refused(400, `event ${i}: a referrer is a URL of 512 characters at most`);
    if (typeof et !== 'number' || !Number.isFinite(et)) throw new Refused(400, `event ${i}: \`t\` is its time in ms`);
    const age = t - et;
    if (age < -1000 || age > MAX_AGE_MS) throw new Refused(400, `event ${i} is older than an hour, or later than the beacon`);
    return { name: n, path: p.split('?')[0].split('#')[0], ref: refHost(r), age: Math.max(0, age), props: props(d, i) };
  });
  return { key: k, batch: b, user: u, events };
}

function props(d: unknown, i: number): Beacon['events'][number]['props'] {
  if (d === undefined || d === null) return null;
  if (!isObject(d)) throw new Refused(400, `event ${i}: properties are an object`);
  const keys = Object.keys(d);
  if (keys.length > 8) throw new Refused(400, `event ${i}: 8 properties at most`);
  const out: Record<string, string | number | boolean> = {};
  for (const key of keys) {
    const val = d[key];
    if (!NAME.test(key)) throw new Refused(400, `event ${i}: a property's name is a-z, 0-9 and _`);
    if (typeof val === 'string' ? val.length > 128 || CONTROL.test(val) : typeof val === 'number' ? !Number.isFinite(val) : typeof val !== 'boolean') {
      throw new Refused(400, `event ${i}: a property is a text of 128 characters at most, a number or true or false`);
    }
    out[key] = val as string | number | boolean;
  }
  return out;
}

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/** The referring site's host name, or '' for none. */
export function refHost(r: unknown): string {
  if (typeof r !== 'string' || !r) return '';
  try {
    const u = new URL(r);
    return u.protocol === 'http:' || u.protocol === 'https:' ? u.hostname.replace(/^www\./, '') : '';
  } catch {
    return '';
  }
}

/** A user agent's device class and browser, or null for a crawler. */
export function agent(ua: string): { device: string; browser: string } | null {
  if (!ua || /bot|crawl|spider|slurp|headless|lighthouse|preview/i.test(ua)) return null;
  const device = /iPad|Tablet|Android(?!.*Mobile)/.test(ua) ? 'tablet' : /Mobi|iPhone|Android/.test(ua) ? 'mobile' : 'desktop';
  const browser = /Edg\//.test(ua)
    ? 'Edge'
    : /OPR\//.test(ua)
      ? 'Opera'
      : /Firefox\//.test(ua)
        ? 'Firefox'
        : /SamsungBrowser\//.test(ua)
          ? 'Samsung'
          : /Chrome\/|CriOS\//.test(ua)
            ? 'Chrome'
            : /Safari\//.test(ua)
              ? 'Safari'
              : 'Other';
  return { device, browser };
}

/** The visitor's country: the trusted proxy's header, else the browser's first language's region. */
export function country(headers: Record<string, string | string[] | undefined>): string {
  if (COUNTRY_HEADER) {
    const c = String(headers[COUNTRY_HEADER] ?? '').toUpperCase();
    if (/^[A-Z]{2}$/.test(c)) return c;
  }
  const m = /^[a-z]{2,3}-([A-Za-z]{2})\b/.exec(String(headers['accept-language'] ?? ''));
  return m ? m[1].toUpperCase() : 'ZZ';
}

/**
 * A token bucket a site: `rate` events a second, a burst of ten seconds'
 * worth. In this process's memory -- several ingest processes would each
 * allow the rate (README, "Limits").
 */
export class RateLimit {
  #buckets = new Map<string, { tokens: number; at: number }>();

  /** Takes `n` of `site`'s tokens; false, taking none, if it has fewer. */
  take(site: string, n: number, rate: number, now = performance.now()): boolean {
    const burst = rate * 10;
    const b = this.#buckets.get(site) ?? { tokens: burst, at: now };
    b.tokens = Math.min(burst, b.tokens + ((now - b.at) / 1000) * rate);
    b.at = now;
    this.#buckets.set(site, b);
    if (b.tokens < n) return false;
    b.tokens -= n;
    return true;
  }
}

export interface Context {
  device: string;
  browser: string;
  country: string;
  /** The site's own hosts: a referrer from one of them is no referrer. */
  hosts: string[];
  now?: number;
}

/**
 * The statement that writes a beacon's events: its documents are its one
 * parameter, so every beacon is the same text, parsed once and kept by the
 * node. Written out, ten parameters an event, it was a text per beacon size.
 */
export const INSERT = 'put events $1 if absent';

/** A beacon's events as documents: the parameter of `INSERT`. */
export function params(b: Beacon, ctx: Context): unknown[] {
  const now = ctx.now ?? Date.now();
  const docs = b.events.map((e, i) => ({
    eid: `${b.batch}:${i}`,
    name: e.name,
    user: b.user,
    path: e.path,
    ref: ctx.hosts.includes(e.ref) ? '' : e.ref,
    country: ctx.country,
    device: ctx.device,
    browser: ctx.browser,
    at: new Date(now - e.age).toISOString(),
    props: e.props,
  }));
  return [docs];
}

export interface Written {
  /** Events the block wrote: fewer than sent when some were written before. */
  written: number;
  /** The answer kept for the key, or a key used before with this batch: written already. */
  duplicate: boolean;
  seq: number | null;
}

/**
 * The beacon written as one block under the site's ingest token, keyed by
 * its batch. A key that comes back 422 -- kept with another request -- is
 * the same batch sent again with a later clock: it landed, and the events'
 * own ids keep it from landing twice anyway.
 */
export async function write(site: string, b: Beacon, ctx: Context): Promise<Written> {
  try {
    const r = await db(site, 'ingest').batch([[INSERT, params(b, ctx)]], {
      idempotencyKey: `${site}:${b.batch}`,
    });
    const a = r.results[0] as { affected?: number };
    return { written: r.replayed ? 0 : (a.affected ?? 0), duplicate: r.replayed, seq: r.seq };
  } catch (e) {
    if (e instanceof FenecError && e.status === 422) return { written: 0, duplicate: true, seq: null };
    throw e;
  }
}
