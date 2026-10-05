// What the tests share: a site of their own on the running node, beacons
// sent as the tracker sends them, and the node's raw answers.
import { randomBytes } from 'node:crypto';
import { FENEC_URL, MARKETS_SCHEMA, OPERATOR_TOKEN } from '../src/config.ts';
import { db } from '../src/db.ts';
import { addSite, createTenant, type Site } from '../src/setup.ts';
import type { SimEvent } from '../src/sim.ts';
import { userAgent } from '../src/sim.ts';

export const APP = (process.env.KESTREL_URL ?? 'http://127.0.0.1:3000').replace(/\/$/, '');
export const ORIGIN = 'https://test.example';

/** A site of its own for a test: a new tenant, registered so the ingest endpoint takes its key. */
export async function newSite(prefix: string, rate = 100_000): Promise<Site> {
  const name = `${prefix}-${randomBytes(4).toString('hex')}`;
  const site: Site = { name, key: randomBytes(10).toString('hex'), label: name, origins: [ORIGIN], rate };
  await addSite(site);
  return site;
}

export async function newMarket(): Promise<string> {
  const name = `mkt-${randomBytes(4).toString('hex')}`;
  await createTenant(name, MARKETS_SCHEMA);
  return name;
}

export interface Sent {
  status: number;
  body: string;
}

/** A beacon as the tracker sends it: text/plain, from the site's origin, with a browser's user agent. */
export async function beacon(
  body: unknown,
  opts: { origin?: string; ua?: string; lang?: string; raw?: boolean } = {},
): Promise<Sent> {
  const res = await fetch(`${APP}/e`, {
    method: 'POST',
    headers: {
      'content-type': 'text/plain;charset=UTF-8',
      origin: opts.origin ?? ORIGIN,
      'user-agent': opts.ua ?? userAgent('desktop', 'Firefox'),
      'accept-language': opts.lang ?? 'en-US,en;q=0.8',
    },
    body: opts.raw ? (body as string) : JSON.stringify(body),
  });
  return { status: res.status, body: await res.text() };
}

/** The body of a beacon for `events`, all of one visitor, sent `now`. */
export function beaconBody(site: Site, batch: string, events: SimEvent[], now = Date.now()) {
  return {
    k: site.key,
    b: batch,
    u: events[0].user.padEnd(8, '0'),
    t: now,
    e: events.map((x) => ({ n: x.name, p: x.path, r: x.ref ? `https://${x.ref}/x` : '', t: x.at, d: x.props ?? undefined })),
  };
}

/** Rows of a statement under the node's own token. */
export const rows = (site: string, text: string, params: unknown[] = []) => db(site, 'operator').rows(text, params) as Promise<Record<string, unknown>[]>;

/** A request to the node itself, as `token`. */
export async function node(path: string, token: string, init: RequestInit = {}): Promise<{ status: number; body: string }> {
  const res = await fetch(`${FENEC_URL}${path}`, {
    ...init,
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json', ...(init.headers ?? {}) },
    signal: AbortSignal.timeout(5000),
  });
  // A stream (a subscription, a wait) is answered by its head: 200 is enough to know.
  const body = res.headers.get('content-type')?.includes('event-stream') ? '' : await res.text();
  if (!res.bodyUsed) await res.body?.cancel().catch(() => {});
  return { status: res.status, body };
}

export const OPERATOR = OPERATOR_TOKEN;

/** Runs `f` over `items`, `n` at a time. */
export async function pool<T>(items: T[], n: number, f: (x: T, i: number) => Promise<void>): Promise<void> {
  let next = 0;
  await Promise.all(
    Array.from({ length: n }, async () => {
      while (next < items.length) {
        const i = next++;
        await f(items[i], i);
      }
    }),
  );
}
