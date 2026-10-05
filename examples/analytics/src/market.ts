// The market data view: ticks for 50 invented symbols from a seeded
// generator, bars and VWAP by `bucket`, and a quote a symbol kept as the
// ticks land.
import { MARKETS } from './config.ts';
import { db } from './db.ts';
import { rng } from './sim.ts';
import { iso, MINUTE } from './time.ts';

export interface Tick {
  sym: string;
  at: number;
  px: number;
  qty: number;
}

const SYLLABLES = ['KA', 'VO', 'RI', 'TEL', 'MOR', 'SAN', 'LUX', 'BRI', 'NO', 'QUA', 'DEX', 'FI', 'ZEN', 'PAL', 'GRO', 'HAL'];

/** 50 invented symbols, each with a starting price and how much it moves. */
export const SYMBOLS: { sym: string; start: number; vol: number; weight: number }[] = (() => {
  const r = rng(7);
  const out: { sym: string; start: number; vol: number; weight: number }[] = [];
  const seen = new Set<string>();
  while (out.length < 50) {
    const sym = (SYLLABLES[Math.floor(r() * SYLLABLES.length)] + SYLLABLES[Math.floor(r() * SYLLABLES.length)]).slice(0, 4);
    if (seen.has(sym) || sym.length < 3) continue;
    seen.add(sym);
    out.push({ sym, start: Math.round(Math.exp(2 + r() * 4) * 100) / 100, vol: 0.15 + r() * 0.65, weight: 0.2 + r() ** 2 * 3 });
  }
  return out.sort((a, b) => a.sym.localeCompare(b.sym));
})();

/**
 * A random walk a symbol: each tick moves the price by a normal step
 * scaled by the symbol's yearly volatility over the time since its last
 * tick (geometric Brownian motion), rounded to the cent. Busier symbols
 * tick more often; a trade's size is heavy-tailed.
 */
export class TickGenerator {
  readonly #r: () => number;
  readonly px = new Map<string, number>();
  readonly #last = new Map<string, number>();
  readonly #total: number;

  constructor(seed = 42) {
    this.#r = rng(seed);
    for (const s of SYMBOLS) this.px.set(s.sym, s.start);
    this.#total = SYMBOLS.reduce((t, s) => t + s.weight, 0);
  }

  #normal(): number {
    const u = 1 - this.#r();
    return Math.sqrt(-2 * Math.log(u)) * Math.cos(2 * Math.PI * this.#r());
  }

  /** `n` ticks spread evenly from `from` to `to`, in time order. */
  ticks(from: number, to: number, n: number): Tick[] {
    const out: Tick[] = [];
    for (let i = 0; i < n; i++) {
      const at = Math.floor(from + ((to - from) * (i + this.#r())) / n);
      let x = this.#r() * this.#total;
      let s = SYMBOLS[0];
      for (const c of SYMBOLS) {
        x -= c.weight;
        if (x < 0) {
          s = c;
          break;
        }
      }
      const dt = Math.max(1, at - (this.#last.get(s.sym) ?? at - 1000)) / (365 * 24 * 3_600_000);
      this.#last.set(s.sym, at);
      const p = (this.px.get(s.sym) ?? s.start) * Math.exp(s.vol * Math.sqrt(dt) * this.#normal() * 6);
      const px = Math.max(0.01, Math.round(p * 100) / 100);
      this.px.set(s.sym, px);
      const qty = Math.max(1, Math.round(Math.exp(this.#r() * 6)));
      out.push({ sym: s.sym, at, px, qty });
    }
    return out;
  }
}

const shapes = new Map<number, string>();
function tickText(n: number): string {
  let s = shapes.get(n);
  if (!s) {
    s = `insert ticks [${Array.from({ length: n }, (_, i) => `{sym: $${i * 4 + 1}, at: $${i * 4 + 2}, px: $${i * 4 + 3}, qty: $${i * 4 + 4}}`).join(', ')}]`;
    shapes.set(n, s);
  }
  return s;
}

/**
 * Ticks written under the feed's token as one block, with each symbol's
 * quote: made the first time, then its price, high and low moved -- by the
 * latest tick, so a batch written late cannot move a quote back.
 */
export async function writeTicks(ticks: Tick[], tenant = MARKETS): Promise<number | null> {
  if (!ticks.length) return null;
  const statements: [string, unknown[]][] = [];
  for (let i = 0; i < ticks.length; i += 500) {
    const part = ticks.slice(i, i + 500);
    statements.push([tickText(part.length), part.flatMap((t) => [t.sym, iso(t.at), t.px, t.qty])]);
  }
  const by = new Map<string, { last: Tick; first: Tick; high: number; low: number; n: number }>();
  for (const t of ticks) {
    const q = by.get(t.sym);
    if (!q) by.set(t.sym, { last: t, first: t, high: t.px, low: t.px, n: 1 });
    else {
      if (t.at >= q.last.at) q.last = t;
      q.high = Math.max(q.high, t.px);
      q.low = Math.min(q.low, t.px);
      q.n++;
    }
  }
  const fresh: unknown[] = [];
  const docs: string[] = [];
  for (const [sym, q] of by) {
    docs.push(`{sym: $${fresh.length + 1}, px: $${fresh.length + 2}, open: $${fresh.length + 2}, high: $${fresh.length + 2}, low: $${fresh.length + 2}, n: 0, at: $${fresh.length + 3}}`);
    fresh.push(sym, q.first.px, iso(q.first.at));
  }
  statements.push([`put quotes [${docs.join(', ')}] if absent`, fresh]);
  for (const [sym, q] of by) {
    statements.push([
      'set quotes {px: case when at <= $2 then $3 else px end, at: greatest(at, $2), high: greatest(high, $4), low: least(low, $5), n: n + $6} where sym = $1',
      [sym, iso(q.last.at), q.last.px, q.high, q.low, q.n],
    ]);
  }
  const r = await db(tenant, 'feed').batch(statements);
  return r.seq;
}

/** The ticks a live feed writes: `rate` a second, a batch every `every` ms, until stopped. */
export function runFeed(opts: { rate?: number; every?: number; onError?: (e: unknown) => void } = {}): () => void {
  const gen = new TickGenerator(Date.now() & 0xffff);
  const every = opts.every ?? 500;
  const rate = opts.rate ?? 200;
  let last = Date.now();
  let stopped = false;
  let busy = false;
  const seeded = (async () => {
    // Carry on from the prices the quotes hold, so a restart does not jump.
    try {
      for (const q of await db(MARKETS, 'viewer').rows('get quotes select sym, px limit 100')) gen.px.set(q.sym, q.px);
    } catch (e) {
      opts.onError?.(e);
    }
  })();
  const timer = setInterval(async () => {
    if (busy || stopped) return;
    busy = true;
    try {
      await seeded;
      const now = Date.now();
      const n = Math.round(((now - last) / 1000) * rate);
      if (n > 0) await writeTicks(gen.ticks(last, now, n));
      last = now;
    } catch (e) {
      opts.onError?.(e);
    } finally {
      busy = false;
    }
  }, every);
  return () => {
    stopped = true;
    clearInterval(timer);
  };
}

export interface Row {
  sym: string;
  open: number;
  px: number;
  high: number;
  low: number;
  volume: number;
  vwap: number;
  trades: number;
}

/** Every symbol over the last `minutes`: open, last, high, low, volume, VWAP, as one statement. */
export async function windowRows(minutes: number, now = Date.now(), tenant = MARKETS): Promise<Row[]> {
  return (await db(tenant, 'viewer').rows(
    'get ticks select sym, first(px by at) as open, last(px by at) as px, max(px) as high, min(px) as low, sum(qty) as volume, sum(px * qty) / sum(qty) as vwap, count(*) as trades where at >= $1 group sym order sym',
    [iso(now - minutes * MINUTE)],
  )) as Row[];
}

export interface Bar {
  bar: string;
  open: number;
  high: number;
  low: number;
  close: number;
  volume: number;
  vwap: number;
}

/** One symbol's bars of `interval` over the last `minutes`. */
export async function bars(sym: string, minutes: number, interval: string, now = Date.now(), tenant = MARKETS): Promise<Bar[]> {
  if (!/^\d{1,3}[mh]$/.test(interval)) throw new Error('an interval is minutes or hours, as 5m or 1h');
  return (await db(tenant, 'viewer').rows(
    `get ticks select bucket(at, ${interval}) as bar, first(px by at) as open, max(px) as high, min(px) as low, last(px by at) as close, sum(qty) as volume, sum(px * qty) / sum(qty) as vwap where sym = $1 and at >= $2 group bar order bar`,
    [sym, iso(now - minutes * MINUTE)],
  )) as Bar[];
}

export interface Quote {
  sym: string;
  px: number;
  open: number;
  high: number;
  low: number;
  n: number;
  at: string;
}
