// Live reads shared by every viewer of a page: one `FenecHttp.live` a site
// for its "now", and one for the market's quotes, each polling (`{ poll }`)
// rather than holding a stream. A poll is answered 304 by the server, the
// query not run, while nothing it reads was written -- `pulse` and `quotes`
// have no @ttl, so their answers can be tagged -- and a stream would be a
// server thread for each, woken by every write.
//
// The browser polls this process in turn, with an ETag of its own: a
// hundred open dashboards are one poll of the database a second, not a
// hundred, and a browser holds no connection open between polls.
import { MARKETS } from './config.ts';
import { db } from './db.ts';
import type { Quote } from './market.ts';
import type { Pulse } from './queries.ts';

interface Feed<T> {
  latest: T | null;
  version: number;
  stop: (() => void) | null;
  restartAt: number;
  idleSince: number;
  listeners: Set<(v: T) => void>;
}

/**
 * One live query per key, started on the first listener and stopped a
 * minute after the last one left. Tokens live ten minutes, so the query is
 * started again with a fresh client every four.
 */
export class LiveHub<T> {
  #feeds = new Map<string, Feed<T>>();

  constructor(
    readonly start: (key: string, cb: (v: T) => void, onError: (e: unknown) => void) => () => void,
    readonly poll = 1000,
  ) {
    setInterval(() => this.#tend(), 5000).unref();
  }

  #feed(key: string): Feed<T> {
    let f = this.#feeds.get(key);
    if (!f) {
      f = { latest: null, version: 0, stop: null, restartAt: 0, idleSince: 0, listeners: new Set() };
      this.#feeds.set(key, f);
    }
    if (!f.stop) this.#run(key, f);
    return f;
  }

  #run(key: string, f: Feed<T>) {
    f.restartAt = Date.now() + 240_000;
    f.stop = this.start(
      key,
      (v) => {
        f.latest = v;
        f.version++;
        for (const l of f.listeners) l(v);
      },
      (e) => console.error(`live ${key}:`, (e as Error).message ?? e),
    );
  }

  #tend() {
    const now = Date.now();
    for (const [key, f] of this.#feeds) {
      if (!f.stop) continue;
      if (f.listeners.size === 0 && f.idleSince && now - f.idleSince > 60_000) {
        f.stop();
        f.stop = null;
        this.#feeds.delete(key);
      } else if (now > f.restartAt) {
        f.stop();
        this.#run(key, f);
      }
    }
  }

  /** The latest value, starting the query if it is not running. */
  current(key: string): { value: T | null; version: number } {
    const f = this.#feed(key);
    f.idleSince = f.idleSince || Date.now();
    if (f.listeners.size === 0) f.idleSince = Date.now();
    return { value: f.latest, version: f.version };
  }

  /** Waits up to `ms` for the first value. */
  async first(key: string, ms = 1500): Promise<T | null> {
    const f = this.#feed(key);
    if (f.latest) return f.latest;
    return new Promise((resolve) => {
      const done = (v: T | null) => {
        clearTimeout(t);
        f.listeners.delete(on);
        if (f.listeners.size === 0) f.idleSince = Date.now();
        resolve(v);
      };
      const on = (v: T) => done(v);
      const t = setTimeout(() => done(f.latest), ms);
      f.listeners.add(on);
    });
  }

  listen(key: string, l: (v: T) => void): () => void {
    const f = this.#feed(key);
    f.listeners.add(l);
    f.idleSince = 0;
    return () => {
      f.listeners.delete(l);
      if (f.listeners.size === 0) f.idleSince = Date.now();
    };
  }
}

export const pulses = new LiveHub<Pulse>((site, cb, onError) =>
  db(site, 'viewer').live(
    'get pulse select active, views, minutes, at where name = "now" limit 1',
    (rows) => {
      if (rows[0]) cb(rows[0] as Pulse);
    },
    { poll: 1000, onError },
  ),
);

export const quotes = new LiveHub<Map<string, Quote>>((_, cb, onError) =>
  db(MARKETS, 'viewer').live('get quotes order sym limit 100', (rows) => cb(new Map((rows as Quote[]).map((q) => [q.sym, q]))), {
    poll: 1000,
    onError,
  }),
);
