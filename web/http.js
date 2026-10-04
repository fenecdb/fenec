// fenecdb over HTTP: `connect`, `FenecHttp` and its live queries, with the
// query builder of `builder.js`. Nothing here reaches the engine, so a page
// whose queries run on a server loads this and not the module
// (`@fenecdb/web/client`).

import { FenecError, Query, checked, declared, ident, nameOf, normalize, rowsOf } from './builder.js';

// ----------------------------------------------------------- HTTP endpoint
//
// The same query builder, against a remote server. The builder produces
// FenecQL text and `POST /query` takes it as is, so query code moves between
// wasm and HTTP unchanged. The REST surface (`GET /<name>?year=gte.2024`)
// is for driverless clients; the builder does not use it.

export class FenecHttp {
  #url;
  #token;
  #fetch;

  /**
   * Where a live query's error goes when it was given no `onError` of its
   * own (`live`); with neither, it is a rejection nothing waits for.
   */
  onError = null;

  constructor(url, opts = {}) {
    this.#url = String(url).replace(/\/+$/, '');
    this.#token = opts.token ?? null;
    this.#fetch = opts.fetch ?? globalThis.fetch;
    if (typeof this.#fetch !== 'function') {
      throw new FenecError('fetch not found: pass one via opts.fetch');
    }
  }

  /** Runs FenecQL. The return shape matches `run` on the wasm path. */
  async run(sql, params = []) {
    const body = await this.#post('/query', { query: sql, params: params.map((p) => normalize(p)) });
    // The endpoint returns rows as a plain array; the builder expects `{rows}`.
    if (Array.isArray(body)) return { rows: body };
    if (body && typeof body.affected === 'number') {
      return { kind: 'affected', count: body.affected };
    }
    return body;
  }

  /**
   * `fetch`, called on its own: as this object's method -- `this.#fetch(..)`
   * -- a browser's `fetch` throws "Illegal invocation", which Node's does
   * not, and `connect` without a `fetch` of its own failed in a page.
   */
  #request(url, init) {
    const fetch = this.#fetch;
    return fetch(url, init);
  }

  /** A POST's JSON answer; a status but `ok` among `also` is an error. */
  async #post(path, payload, also = []) {
    const headers = { 'content-type': 'application/json' };
    if (this.#token) headers.authorization = `Bearer ${this.#token}`;
    const res = await this.#request(`${this.#url}${path}`, { method: 'POST', headers, body: JSON.stringify(payload) });
    const text = await res.text();
    let body;
    try {
      body = text ? JSON.parse(text) : null;
    } catch {
      throw new FenecError(`server did not return JSON (${res.status}): ${text.slice(0, 200)}`);
    }
    if (!res.ok && !also.includes(res.status)) {
      // The status is the refusal's kind: 412 a write's `require` not met,
      // 409 a value taken, 403 outside the token's rules.
      const e = new FenecError(body?.error ?? `HTTP ${res.status}`);
      e.status = res.status;
      throw e;
    }
    return body;
  }

  async rows(sql, params = []) {
    return rowsOf(await this.run(sql, params));
  }

  /**
   * The server's database against a schema declared in code, in the
   * server (`/_schema`): `'follow'` -- what a client that does not own it
   * does -- says what the code declares and the server lacks, `'plan'`
   * what an apply would do, `'apply'` runs it, with the server's token.
   */
  checkSchema(description, mode = 'follow') {
    const path = { follow: 'plan?mode=follow', plan: 'plan', apply: 'apply' }[mode];
    return this.#post(`/_schema/${path}`, description, [409]);
  }

  /** Query builder -- identical to the one on the wasm path. */
  from(name) {
    return new Query({
      collection: ident(nameOf(name), 'collection'),
      exec: (sql, params) => this.run(sql, params),
      // What `useLiveQuery` finds the query's endpoint by.
      context: this,
      rel: declared.get(this)?.relations,
    });
  }

  /**
   * Live query over the server: `cb` is handed the rows once the server
   * has opened a subscription to every collection the query reads, and
   * again after every write to one of them -- `Fenec.live`'s contract, the
   * query run on the server each time. `query` is a builder `Query`, a
   * FenecQL text or `[text, params]`; a text names what it reads with
   * `{collections}`, since nothing here could know a write it may read.
   * Returns the function that stops it.
   *
   * A subscription (`GET /<collection>/changes`) keeps a shape's rows, and
   * a query's rows are not a shape -- an order, a page, a `near`, a
   * `lookup`. So each stream is of a shape that holds nothing
   * (`where=false`, `select=id`): its seed is empty and a write to the
   * collection is a change naming the ids written, which is what runs the
   * query again; the writes of one burst run it once, after the run under
   * way. A stream that ends is opened again (250 ms, doubling to 15 s), and
   * its seed runs the query, since writes may have come between.
   *
   * @param {{onError?: Function, params?: Array, collections?: string[]}} opts
   */
  live(query, cb, opts = {}) {
    if (typeof cb !== 'function') throw new FenecError('live(query, cb): cb must be a function');
    let rows;
    let reads;
    if (query instanceof Query) {
      const bound = query.plain().bind((sql, params) => this.run(sql, params));
      rows = () => bound.rows();
      reads = query.reads;
    } else {
      const [sql, params] = typeof query === 'string' ? [query, opts.params ?? []] : Array.isArray(query) ? query : [];
      if (typeof sql !== 'string') throw new FenecError('live: a Query, a FenecQL text, or [text, params]');
      rows = async () => rowsOf(await this.run(sql, params ?? []));
      reads = null;
    }
    if (opts.collections) reads = opts.collections.map((c) => ident(c, 'collection'));
    if (!reads) {
      throw new FenecError('live over HTTP: name the collections a text reads, { collections: [...] }');
    }
    const onError = opts.onError ?? this.onError;
    const stop = new AbortController();
    const report = (err) => {
      if (stop.signal.aborted) return;
      if (onError) onError(err);
      else Promise.reject(err);
    };
    let running = false;
    let again = false;
    const run = async () => {
      if (running) {
        again = true;
        return;
      }
      running = true;
      try {
        do {
          again = false;
          const got = await rows();
          if (!stop.signal.aborted) cb(got);
        } while (again && !stop.signal.aborted);
      } catch (err) {
        report(err);
      } finally {
        running = false;
      }
    };
    // The first rows wait for every stream's first seed: a write made
    // before a stream opened is then in them.
    const collections = [...new Set(reads)];
    let unseeded = collections.length;
    const headers = { accept: 'text/event-stream' };
    if (this.#token) headers.authorization = `Bearer ${this.#token}`;
    const follow = async (collection) => {
      const url = `${this.#url}/${encodeURIComponent(collection)}/changes?select=id&where=false`;
      let seeded = false;
      for (let attempt = 0; !stop.signal.aborted; attempt++) {
        try {
          const res = await this.#request(url, { headers, signal: stop.signal });
          if (!res.ok || !res.body) {
            const text = await res.text().catch(() => '');
            throw new FenecError(`could not open the subscription to \`${collection}\`: ${text || `HTTP ${res.status}`}`);
          }
          for await (const ev of sseEvents(res)) {
            if (stop.signal.aborted) return;
            if (ev.name === 'error') throw new FenecError(JSON.parse(ev.data).error ?? 'subscription error');
            attempt = 0;
            if (ev.name === 'seed' && !seeded) {
              seeded = true;
              unseeded--;
            }
            if (unseeded === 0) run();
          }
        } catch (err) {
          report(err);
        }
        if (stop.signal.aborted) return;
        await new Promise((r) => setTimeout(r, Math.min(15000, 250 * 2 ** attempt)));
      }
    };
    for (const c of collections) follow(c);
    return () => stop.abort();
  }

  /** Collection schemas. */
  async schemas() {
    return this.run('collections');
  }
}

/**
 * Connects to a remote fenecdb HTTP endpoint (`fenec-server --http`). With
 * `schema`, a promise: the server's schema is checked against the code's,
 * and with `migrate` -- and the server's token -- brought to it.
 */
export function connect(url, opts = {}) {
  const http = new FenecHttp(url, opts);
  return opts.schema ? checked(http, opts, opts.migrate ? 'apply' : 'follow') : http;
}

/**
 * Turns the SSE stream into events.
 *
 * `fetch` rather than `EventSource` because of one header: `EventSource`
 * cannot carry `Authorization`, so the token would have to travel in the
 * query string -- which means into the logs and into `Referer`.
 */
export async function* sseEvents(res) {
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buf = '';
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return;
      buf = (buf + decoder.decode(value, { stream: true })).replace(/\r\n/g, '\n');
      let i;
      while ((i = buf.indexOf('\n\n')) >= 0) {
        const block = buf.slice(0, i);
        buf = buf.slice(i + 2);
        let name = '';
        let data = '';
        for (const line of block.split('\n')) {
          if (line.startsWith('event:')) name = line.slice(6).trim();
          else if (line.startsWith('data:')) data += line.slice(5).trim();
        }
        if (name) yield { name, data };
      }
    }
  } finally {
    try {
      await reader.cancel();
    } catch {
      /* already closed */
    }
  }
}
