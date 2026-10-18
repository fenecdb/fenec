// fenecdb over HTTP: `connect`, `FenecHttp` and its live queries, with the
// query builder of `builder.js`. Nothing here reaches the engine, so a page
// whose queries run on a server loads this and not the module
// (`@fenecdb/web/client`).

import { FenecError, Query, checked, declared, ident, nameOf, normalize, rowsOf, statementOf, whole } from './builder.js';

// ----------------------------------------------------------- HTTP endpoint
//
// The same query builder, against a remote server. The builder produces
// FenecQL text and `POST /query` takes it as is, so query code moves between
// wasm and HTTP unchanged. The REST surface (`GET /<name>?year=gte.2024`)
// is for driverless clients; the builder does not use it.

/** `Fenec-Wait` for `{ wait }`: milliseconds from 0, 30 000 at most at the server. */
function held(ms) {
  if (typeof ms !== 'number' || !Number.isFinite(ms) || ms < 0) {
    throw new FenecError(`wait is milliseconds, a number from 0 (got ${ms})`);
  }
  return Math.floor(ms);
}

export class FenecHttp {
  #url;
  #token;
  #fetch;

  /**
   * Where a live query's error goes when it was given no `onError` of its
   * own (`live`); with neither, it is a rejection nothing waits for.
   */
  onError = null;

  #key;
  #last;
  #reads;

  constructor(url, opts = {}) {
    this.#url = String(url).replace(/\/+$/, '');
    this.#token = opts.token ?? null;
    this.#fetch = opts.fetch ?? globalThis.fetch;
    this.#key = opts.idempotencyKey ?? null;
    // Shared with every copy `withIdempotencyKey` makes, as Go's and
    // .NET's clients share theirs. `at` is the furthest position an answer
    // from home named -- `Fenec-Seq` and the history it is a change of --
    // which a marked read is sent after: an answer that arrives late is not
    // the last.
    this.#last = opts[LAST] ?? { seq: null, at: null };
    this.#reads = opts.reads ?? 'home';
    if (this.#reads !== 'home' && this.#reads !== 'nearest') {
      throw new FenecError(`reads is 'home' or 'nearest' (got ${this.#reads})`);
    }
    if (typeof this.#fetch !== 'function') {
      throw new FenecError('fetch not found: pass one via opts.fetch');
    }
  }

  /**
   * The change the last write through this client, or a copy of it, left
   * the database at (`Fenec-Seq`); `null` before any.
   */
  get seq() {
    return this.#last.seq;
  }

  /**
   * A copy whose writes carry `key` as their `Idempotency-Key`: sent again
   * after a timeout, a write is answered as it was the first time and not
   * made twice (`replayed` on its answer). One key a write: the same key
   * with another request is refused (422). What a builder's writes take,
   * since they reach `run` with no options of their own:
   * `db.withIdempotencyKey(k).from('orders').insert(order)`.
   */
  withIdempotencyKey(key) {
    if (typeof key !== 'string' || !key) throw new FenecError('an idempotency key is a text, not empty');
    const copy = new this.constructor(this.#url, {
      token: this.#token,
      fetch: this.#fetch,
      idempotencyKey: key,
      reads: this.#reads,
      [LAST]: this.#last,
    });
    // The schema `connect` checked, and its relations, go with it.
    if (declared.has(this)) declared.set(copy, declared.get(this));
    copy.onError = this.onError;
    return copy;
  }

  /**
   * Runs FenecQL. The return shape matches `run` on the wasm path; a write
   * also says the change it left the database at (`seq`) and whether the
   * answer is the one kept for its key (`replayed`). `{ idempotencyKey }`
   * sends one with this statement. `{ wait: ms }` holds a `set` or a `del`
   * that writes nothing at the server until it can -- a job enqueued, a
   * delayed one come due, a lease lapsed -- or for that long, at most
   * 30 000: a claim waiting for a job (`Fenec-Wait`). `{ signal }` gives
   * it up, as any `fetch`. `{ read: true }`: the text only reads, and goes
   * near on a client reading near, as the builder's reads do.
   */
  async run(sql, params = [], opts = {}) {
    const { body, seq, replayed } = await this.#send(
      '/query',
      'application/json',
      JSON.stringify({ query: sql, params: params.map((p) => normalize(p)) }),
      opts.idempotencyKey ?? this.#key,
      [],
      opts,
    );
    // The endpoint returns rows as a plain array; the builder expects `{rows}`.
    // A write's (`returning`) says the change it left the database at too.
    if (Array.isArray(body)) return seq === null ? { rows: body } : { rows: body, seq, replayed };
    if (body && typeof body.affected === 'number') {
      return { kind: 'affected', count: body.affected, seq, replayed };
    }
    return body;
  }

  /**
   * `POST /batch`: the statements in order under one write lock, as one
   * block -- their writes all land, or at the first error none of them do.
   * Each is a builder query (a read), a FenecQL text, or `[text, params]`,
   * which a builder's `toInsert`, `toUpdate` and `toDelete` make. Answers
   * each statement's result in order as the server writes it -- `{rows}`
   * (and `facets`), `{affected}` or `{message}` -- with `seq` and
   * `replayed`. With `{ idempotencyKey }` a retry after a timeout is
   * answered as the first try was and writes nothing twice.
   *
   * A statement that stops it throws a `FenecError` whose `at` is that
   * statement's place (from 0), `status` the refusal's kind -- 412 a
   * write's `require` not met -- and `completed` how many stayed applied:
   * none, but for a batch holding a `compact`, whose statements run on
   * their own. `{ wait, signal }` as `run`'s: held while its `set`s and
   * `del`s write nothing. A batch of builder queries alone is a read, as is
   * one given `{ read: true }`.
   */
  async batch(items, opts = {}) {
    if (!Array.isArray(items) || items.length === 0) {
      throw new FenecError('batch takes a list of statements: queries, texts or [text, params]');
    }
    const lines = items.map((item, i) => {
      const [sql, params] = statementOf(item, i);
      return JSON.stringify({ query: sql, params: params.map((p) => normalize(p)) });
    });
    const read = opts.read ?? items.every((item) => item instanceof Query);
    const { body, seq, replayed } = await this.#send(
      '/batch',
      'application/x-ndjson',
      lines.join('\n'),
      opts.idempotencyKey ?? this.#key,
      [],
      { ...opts, read },
    );
    return { results: body?.results ?? [], seq, replayed };
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
    return (await this.#send(path, 'application/json', JSON.stringify(payload), null, also)).body;
  }

  /**
   * A POST: its JSON answer, the change a write left the database at, and
   * whether the answer is the one kept for `key`.
   */
  async #send(path, type, payload, key, also = [], { wait, signal, read, [AFTER]: after } = {}) {
    const headers = { 'content-type': type };
    if (this.#token) headers.authorization = `Bearer ${this.#token}`;
    if (key) headers['idempotency-key'] = key;
    if (wait !== undefined && wait !== null) headers['fenec-wait'] = String(held(wait));
    // A read near: marked for the tenant's read replica in the router's
    // region, after the furthest position home named to this client, which
    // the replica waits for -- that change of that history; a live
    // query's run after its streams' position where that is further.
    let near = read === true && this.#reads === 'nearest';
    const of = after?.at && later(after.at, this.#last.at) ? after : this.#last;
    let at = of.at;
    const mark = () => {
      headers['fenec-after'] = String(at.seq);
      if (at.history) headers['fenec-history'] = at.history;
      else delete headers['fenec-history'];
      // A live run waits a second, not five: a replica later than home's
      // fsync and a link is behind, and home answers in a round trip.
      if (after) headers['fenec-wait'] = '1000';
    };
    if (near) {
      headers['fenec-read'] = 'replica';
      if (at) mark();
    }
    const init = { method: 'POST', headers, body: payload };
    if (signal) init.signal = signal;
    let res = await this.#request(`${this.#url}${path}`, init);
    // 504: no copy holds that write in the wait -- home had none either,
    // lost in a failover. Home says where it stands, which the client
    // takes as its position, lower or not; asked home again with no mark,
    // it reads what home holds. Taken only over the position this request
    // was sent after -- a write answered meanwhile is further, and set back
    // the next read near would miss it -- and never of an earlier term of
    // the same lineage: a router whose copy of the directory lags a
    // failover sends the read to the old primary, fenced, which names the
    // branch it abandoned. Another lineage's is a tenant made again under
    // the name, which starts again at term 1: refused, every read near
    // waited its whole wait for good. Whichever the read was sent after,
    // the client's or a live query's, is the one set back.
    if (near && res.status === 504 && headers['fenec-after']) {
      const told = await res.json().catch(() => null);
      let took = false;
      if (told && Number.isSafeInteger(told.seq)) {
        const p = position(told.seq, typeof told.history === 'string' ? told.history : null);
        if ((took = of.at === at && (p.term >= at.term || another(p, at)))) of.at = p;
      }
      // A live run is asked near again, after where home stands: an
      // unmarked answer names no position for the next run to go after.
      if (after && took) {
        at = of.at;
        mark();
        res = await this.#request(`${this.#url}${path}`, init);
      }
      if (res.status === 504) {
        delete headers['fenec-read'];
        delete headers['fenec-after'];
        delete headers['fenec-history'];
        if (after) delete headers['fenec-wait'];
        res = await this.#request(`${this.#url}${path}`, init);
        near = false;
      }
    }
    const text = await res.text();
    let body;
    try {
      body = text ? JSON.parse(text) : null;
    } catch {
      throw new FenecError(`server did not return JSON (${res.status}): ${text.slice(0, 200)}`);
    }
    if (!res.ok && !also.includes(res.status)) {
      // The status is the refusal's kind: 412 a write's `require` not met,
      // 409 a value taken, 403 outside the token's rules, 422 a key sent
      // with another request, 507 and 429 a tenant's quota. A batch says
      // which statement stopped it (`at`) and how many stayed applied
      // (`completed`).
      throw refused(res, body);
    }
    const got = res.headers?.get?.('fenec-seq');
    const seq = got ? Number(got) : null;
    // A position from home moves the client's; a marked answer's, maybe a
    // replica's, a live run's own alone: its next run goes after the rows
    // it showed, which so never go back.
    if (seq !== null) {
      const p = position(seq, res.headers?.get?.('fenec-history') ?? null);
      const to = near ? after : this.#last;
      if (!near) this.#last.seq = seq;
      if (to && later(p, to.at)) to.at = p;
    }
    return { body, seq, replayed: res.headers?.get?.('idempotent-replayed') === 'true' };
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
      exec: (sql, params, opts) => this.run(sql, params, opts),
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
   * query again -- under a scoped token, a change naming nothing, at a
   * write to a row the token may read and at no other; the writes of one burst run it once, after the run under
   * way. A stream that ends is opened again (250 ms, doubling to 15 s), and
   * its seed runs the query, since writes may have come between.
   *
   * Reading near, the streams stay home and each run is a read near, sent
   * after the furthest position they named, or the client's own: a read
   * replica answers it once it holds the change that ran it, or it goes
   * home.
   *
   * @param {{onError?: Function, params?: Array, collections?: string[]}} opts
   */
  live(query, cb, opts = {}) {
    if (typeof cb !== 'function') throw new FenecError('live(query, cb): cb must be a function');
    const near = this.#reads === 'nearest';
    // The furthest position the streams named, ordered as the client's:
    // a stream opened again after a failover names the later term, and
    // the old primary's moves nothing.
    const seen = { at: null };
    const marked = { read: true, [AFTER]: seen };
    let rows;
    let reads;
    if (query instanceof Query) {
      const bound = query.plain().bind((sql, params) => this.run(sql, params, marked));
      rows = () => bound.rows();
      reads = query.reads;
    } else {
      const [sql, params] = typeof query === 'string' ? [query, opts.params ?? []] : Array.isArray(query) ? query : [];
      if (typeof sql !== 'string') throw new FenecError('live: a Query, a FenecQL text, or [text, params]');
      rows = async () => rowsOf(await this.run(sql, params ?? [], marked));
      reads = null;
    }
    if (opts.poll !== undefined) return this.#poll(query, cb, opts);
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
          if (!res.ok || !res.body) throw await refusal(res, collection);
          // The history each event's `seq` is of, as the head names it;
          // the seed's `seq` is at or past the head's `Fenec-Seq`.
          const history = res.headers.get('fenec-history');
          // None (a server from before, or no replica): term-0 positions
          // never pass the client's, so the runs go home.
          if (!history) marked.read = false;
          for await (const ev of sseEvents(res)) {
            if (stop.signal.aborted) return;
            if (ev.name === 'error') throw streamError(ev.data);
            const p = near && position(JSON.parse(ev.data).seq, history);
            if (p?.seq > 0 && later(p, seen.at)) seen.at = p;
            attempt = 0;
            if (ev.name === 'seed' && !seeded) {
              seeded = true;
              unseeded--;
            }
            if (unseeded === 0) run();
          }
        } catch (err) {
          report(err);
          // A token refused -- or lapsed under an open stream, which the
          // server ends at its `exp` -- is refused again however often it
          // is sent, and each refusal waits longer: the live query stops
          // there, for the app to open it again with a fresh token.
          if (err?.status === 401) return stop.abort();
        }
        if (stop.signal.aborted) return;
        await new Promise((r) => setTimeout(r, Math.min(15000, 250 * 2 ** attempt)));
      }
    };
    for (const c of collections) follow(c);
    return () => stop.abort();
  }

  /**
   * A shape's rows, and every change to them (`GET /<collection>/changes`):
   * `onEvent` is handed `{ type: 'seed', seq, rows }` -- the whole shape,
   * which replaces what the caller held -- then `{ type: 'change', seq,
   * puts, dels, schema }` as each write lands. `shape` is the REST
   * surface's: a field's filter (`{ team: 'eq.design' }`), `where` (a
   * FenecQL condition) and `select` (a list or a text); a scoped token's
   * rules are ANDed in by the server. A stream that ends -- a tenant moved,
   * the network -- is opened again (250 ms, doubling to 15 s) and seeds
   * again; one refused or ended for its token (401, as the server ends one
   * at its token's `exp`) is not, for the app to subscribe again with a
   * fresh one. `onError` hears why a stream ended, `onState` `'open'` at
   * each seed and `'retry'` as it waits to open again. Returns the
   * function that stops it.
   *
   * @param {string} collection
   * @param {Record<string, string | string[]>} [shape]
   * @param {(ev: object) => void} onEvent
   * @param {{onError?: Function, onState?: Function}} [opts]
   */
  subscribe(collection, shape, onEvent, opts = {}) {
    if (typeof onEvent !== 'function') throw new FenecError('subscribe(collection, shape, onEvent): onEvent must be a function');
    const c = ident(collection, 'collection');
    const q = new URLSearchParams();
    for (const [k, v] of Object.entries(shape ?? {})) {
      if (k === 'since') throw new FenecError('subscribe: each stream seeds; `since` is not taken');
      q.append(k, Array.isArray(v) ? v.join(',') : String(v));
    }
    const qs = q.toString();
    const url = `${this.#url}/${encodeURIComponent(c)}/changes${qs ? `?${qs}` : ''}`;
    const headers = { accept: 'text/event-stream' };
    if (this.#token) headers.authorization = `Bearer ${this.#token}`;
    const onError = opts.onError ?? this.onError;
    const stop = new AbortController();
    (async () => {
      for (let attempt = 0; !stop.signal.aborted; attempt++) {
        try {
          const res = await this.#request(url, { headers, signal: stop.signal });
          if (!res.ok || !res.body) throw await refusal(res, c);
          for await (const ev of sseEvents(res)) {
            if (stop.signal.aborted) return;
            if (ev.name === 'error') throw streamError(ev.data);
            const msg = JSON.parse(ev.data);
            if (ev.name === 'seed') {
              attempt = 0;
              opts.onState?.('open');
              onEvent({ type: 'seed', seq: msg.seq, rows: msg.rows ?? [] });
            } else if (ev.name === 'change') {
              onEvent({ type: 'change', seq: msg.seq, puts: msg.puts ?? [], dels: msg.dels ?? [], schema: !!msg.schema });
            }
          }
        } catch (err) {
          if (stop.signal.aborted) return;
          if (onError) onError(err);
          else Promise.reject(err);
          // A token refused is refused again: stopped, for a fresh one.
          if (err?.status === 401) return stop.abort();
        }
        if (stop.signal.aborted) return;
        opts.onState?.('retry');
        await new Promise((r) => setTimeout(r, Math.min(15000, 250 * 2 ** attempt)));
      }
    })();
    return () => stop.abort();
  }

  /**
   * `live` with `{ poll: ms }`: no stream. The query is asked every `ms`
   * with the tag of the last answer (`If-None-Match`), and the server
   * answers 304 without running it while nothing it reads was written --
   * so a page that thousands view holds no stream each: a stream is a
   * server thread and a wake-up at every write (60 streams took a write's
   * p50 from 0.26 to 0.94 ms, 500 to 3.1). A read of rows that expire,
   * or calling `now()`, is run every time.
   *
   * Home on a client reading near too: nothing names a change home took
   * since the tag, as a stream's events do, so a replica behind would
   * answer 304 while home holds a write the query reads.
   */
  #poll(query, cb, opts) {
    const [sql, params] =
      query instanceof Query ? query.plain().toFenecQL() : typeof query === 'string' ? [query, opts.params ?? []] : query;
    const every = whole(opts.poll, 'poll');
    if (every < 100) throw new FenecError('live: poll is every 100 ms at the most often');
    const onError = opts.onError ?? this.onError;
    const stop = new AbortController();
    const headers = { 'content-type': 'application/json' };
    if (this.#token) headers.authorization = `Bearer ${this.#token}`;
    const body = JSON.stringify({ query: sql, params: (params ?? []).map((p) => normalize(p)) });
    let tag = '"0"';
    const round = async () => {
      try {
        const res = await this.#request(`${this.#url}/query`, {
          method: 'POST',
          headers: { ...headers, 'if-none-match': tag },
          body,
          signal: stop.signal,
        });
        if (res.status === 304) return;
        const got = await res.json();
        if (!res.ok) throw refused(res, got);
        tag = res.headers.get('etag') ?? '"0"';
        if (!stop.signal.aborted) cb(rowsOf(Array.isArray(got) ? { rows: got } : got));
      } catch (err) {
        if (stop.signal.aborted) return;
        if (onError) onError(err);
        else Promise.reject(err);
        // As a stream's: a refused token is not asked again.
        if (err?.status === 401) stop.abort();
      }
    };
    (async () => {
      while (!stop.signal.aborted) {
        await round();
        await new Promise((r) => setTimeout(r, every));
      }
    })();
    return () => stop.abort();
  }

  /** Collection schemas. */
  async schemas() {
    return this.run('collections');
  }
}

/** Why a subscription could not open: the server's words, and its status. */
async function refusal(res, collection) {
  const text = await res.text().catch(() => '');
  let said = text;
  try {
    said = JSON.parse(text)?.error ?? text;
  } catch {
    // not JSON: the text as it came
  }
  let body = null;
  try {
    body = JSON.parse(text);
  } catch {
    // no JSON to read a quota from
  }
  return refused(res, body, `could not open the subscription to \`${collection}\`: ${said || `HTTP ${res.status}`}`);
}

/**
 * A refused request as the error it is: its status, where a batch stopped
 * and what stayed applied, where a tenant's quota refused it the quota
 * (`bytes` and `rows` answer 507; `streams`, `requests`, `writes` and
 * `write_bytes` 429), and how many seconds to wait before coming back
 * (`Retry-After`: a quota's 429, a tenant being moved or failed over, a
 * server at its ceiling of connections), so a client backs off by what the
 * server says rather than by a guess.
 */
function refused(res, body, message) {
  const e = new FenecError(message ?? body?.error ?? `HTTP ${res.status}`);
  e.status = res.status;
  if (typeof body?.at === 'number') e.at = body.at;
  if (typeof body?.completed === 'number') e.completed = body.completed;
  const quota = body?.quota ?? res.headers?.get?.('fenec-quota');
  if (quota) e.quota = quota;
  // The wait to the millisecond where the body says it: `Retry-After` is
  // whole seconds, and a bucket of 600 rows a second refills a row in 1.7 ms.
  const ms = Number(body?.retry_after_ms);
  const wait = Number.isFinite(ms) && ms > 0
    ? ms / 1000
    : Number(res.headers?.get?.('retry-after') ?? body?.retry_after);
  if (Number.isFinite(wait) && wait > 0) e.retryAfter = wait;
  return e;
}

/**
 * A stream's `error` event as the error it is: `status` 401 where the
 * server ended it at its token's `exp`, as it refuses a request.
 */
function streamError(data) {
  let msg = {};
  try {
    msg = JSON.parse(data) ?? {};
  } catch {
    // an event that is not JSON
  }
  const e = new FenecError(msg.error ?? 'subscription error');
  if (typeof msg.status === 'number') e.status = msg.status;
  return e;
}

/** The option a copy of a client is handed its original's `seq` under. */
const LAST = Symbol('last');

/**
 * The option a live query's run is handed its streams' position under:
 * kept apart from the client's, which its other reads near wait for.
 */
const AFTER = Symbol('after');

/**
 * A position an answer named: its change, the history it is a change of
 * (`<term>-<id>`, `Fenec-History`; none for a database that never had a
 * replica), that history's term, one more at each promotion, and its
 * lineage: the id's top half, which every fork of the tenant keeps and a
 * tenant made again under its name does not.
 */
function position(seq, history) {
  const [t, id] = history ? history.split('-') : [];
  return { seq, history, term: Number(t) || 0, lineage: id ? id.slice(0, 8) : null };
}

/** Whether `p` is of another lineage than `q`: a tenant made again. */
function another(p, q) {
  return !!(p.lineage && q.lineage && p.lineage !== q.lineage);
}

/**
 * Whether position `p` is past `q`: of another lineage -- home's word on a
 * tenant made again, whose terms start again -- or a later term of the
 * same, or further in one.
 */
function later(p, q) {
  return !q || another(p, q) || p.term > q.term || (p.term === q.term && p.seq > q.seq);
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
