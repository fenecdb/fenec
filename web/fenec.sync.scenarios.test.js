// integrations/sync-scenarios.json run against FenecSync: every scenario's
// server events fed to it in order, through a transport the script plays --
// a fetch whose streams it writes to and whose requests it answers, a clock
// whose timers it runs out -- and what the replica holds, what was sent and
// what was told checked after every step. crates/fenec-abi/tests/scenarios.rs
// runs the same file against the native core, so the two are held to one
// behaviour; where they differ on purpose the file says so, and each side
// checks its own expectation.
//
// The replica is persisted (`persist`, over an in-memory IndexedDB), so a
// restart is the page opened again over what it stored. The names that
// passed are written to target/sync-scenarios/js.txt, which
// integrations/sync-scenarios-check.mjs holds to the file beside the core's.
// Skipped without web/fenec.wasm (make wasm), except under CI, where a skip
// would be a silent one.

import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { readFile, mkdir, writeFile } from 'node:fs/promises';
import { Fenec, sync, inc } from './fenec.js';
import { fakeIndexedDB, KeyRange } from './idb.fake.js';

const wasm = await readFile(new URL('./fenec.wasm', import.meta.url)).catch(() => null);
const file = JSON.parse(await readFile(new URL('../integrations/sync-scenarios.json', import.meta.url), 'utf8'));
const URL_ = 'http://server';

const skip = wasm ? false : process.env.CI ? false : 'no web/fenec.wasm (make wasm)';

// --------------------------------------------------------------- matching

/**
 * Expected against actual: an object's members a subset, a list whole, a
 * string `<name>` any value, the same each time the name comes again and
 * another than any other name's.
 */
function matches(exp, act, binds, at) {
  if (typeof exp === 'string' && /^<.+>$/.test(exp)) {
    const got = JSON.stringify(act ?? null);
    if (binds.has(exp)) {
      if (binds.get(exp) !== got) throw new Error(`${at}: ${exp} was ${binds.get(exp)}, now ${got}`);
      return;
    }
    for (const [k, v] of binds) if (v === got) throw new Error(`${at}: ${exp} is ${got}, which ${k} already is`);
    binds.set(exp, got);
    return;
  }
  if (Array.isArray(exp)) {
    if (!Array.isArray(act) || act.length !== exp.length) {
      throw new Error(`${at}: ${exp.length} items expected, ${JSON.stringify(act)} came`);
    }
    exp.forEach((x, i) => matches(x, act[i], binds, `${at}[${i}]`));
    return;
  }
  if (exp !== null && typeof exp === 'object') {
    if (act === null || typeof act !== 'object') throw new Error(`${at}: expected an object, got ${JSON.stringify(act)}`);
    for (const [k, v] of Object.entries(exp)) matches(v, act[k] ?? null, binds, `${at}.${k}`);
    return;
  }
  if (exp !== (act ?? null)) throw new Error(`${at}: expected ${JSON.stringify(exp)}, got ${JSON.stringify(act)}`);
}

/** A step's data with each fixture (`@name`) and each bound `<name>` put in. */
function resolve(v, binds) {
  if (typeof v === 'string' && v.startsWith('@')) return structuredClone(file.fixtures[v.slice(1)]);
  if (typeof v === 'string' && /^<.+>$/.test(v)) {
    if (!binds.has(v)) throw new Error(`${v} is used before it is bound`);
    return JSON.parse(binds.get(v));
  }
  if (Array.isArray(v)) return v.map((x) => resolve(x, binds));
  if (v !== null && typeof v === 'object') {
    return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, k.startsWith('expect') ? x : resolve(x, binds)]));
  }
  return v;
}

/** The fields of a write in alphabetical order, as the native runner's JSON reads them. */
function sorted(o, at) {
  const keys = Object.keys(o);
  assert.deepEqual(keys, [...keys].sort(), `${at}: fields in alphabetical order, or the platforms render them apart`);
}

// ------------------------------------------------------------------- run

/** The app, its replica, and the network as the script plays it. */
class Run {
  constructor(sc) {
    this.shapes = sc.shapes ?? file.shapes;
    this.collections = JSON.stringify(sc.collections ?? file.collections);
    this.up = true;
    this.streamStatus = null;
    this.open = [];
    this.posts = [];
    this.timers = new Set();
    this.db = null;
    this.idb = fakeIndexedDB();
    this.clear();
  }

  clear() {
    this.requests = [];
    this.streams = [];
    this.refused = [];
    this.waits = [];
    this.tokenAsked = false;
    this.error = null;
  }

  fetch = (url, init = {}) => {
    const path = String(url).slice(URL_.length);
    const h = init.headers ?? {};
    if (h.accept === 'text/event-stream') {
      this.streams.push({ path, auth: h.authorization ?? null });
      if (!this.up) return Promise.reject(new TypeError('connection refused'));
      if (this.streamStatus) {
        const status = this.streamStatus;
        this.streamStatus = null;
        return Promise.resolve(new Response('{"error":"refused"}', { status }));
      }
      const c = path.slice(1).split('/')[0];
      let ctl;
      const body = new ReadableStream({ start: (c) => void (ctl = c) });
      const s = { c, ctl };
      this.open.push(s);
      init.signal?.addEventListener('abort', () => {
        this.open = this.open.filter((x) => x !== s);
        try {
          ctl.error(new DOMException('aborted', 'AbortError'));
        } catch {
          /* already ended */
        }
      });
      return Promise.resolve(new Response(body, { status: 200, headers: { 'content-type': 'text/event-stream' } }));
    }
    const r = { method: init.method ?? 'GET', path, auth: h.authorization ?? null, key: h['idempotency-key'] ?? null };
    if (init.body != null) {
      r.body = String(init.body).split('\n').map((l) => JSON.parse(l));
      r.lines = r.body.length;
    }
    this.requests.push(r);
    if (r.method === 'GET') {
      if (!this.up) return Promise.reject(new TypeError('connection refused'));
      return Promise.resolve(new Response(this.collections, { status: 200 }));
    }
    return new Promise((res, rej) => this.posts.push({ res, rej }));
  };

  /** Timers the core would ask for: a backoff waits for `fire`, a frame's tick runs at once. */
  clock() {
    const real = { setTimeout: globalThis.setTimeout, clearTimeout: globalThis.clearTimeout };
    globalThis.setTimeout = (fn, ms = 0) => {
      if (ms <= 60) {
        const h = setImmediate(fn);
        return { unref() {}, h };
      }
      const t = { fn, ms, unref() {} };
      this.timers.add(t);
      this.waits.push(ms);
      return t;
    };
    globalThis.clearTimeout = (t) => {
      if (t?.h) clearImmediate(t.h);
      else if (t && this.timers.has(t)) this.timers.delete(t);
      else real.clearTimeout(t);
    };
    return () => Object.assign(globalThis, real);
  }

  async settle() {
    for (let i = 0; i < 60; i++) await new Promise((r) => setImmediate(r));
  }

  async start() {
    this.open = [];
    this.posts = [];
    this.timers.clear();
    globalThis.indexedDB = this.idb;
    globalThis.IDBKeyRange = KeyRange;
    try {
      this.db = await sync({
        url: URL_,
        token: 't0',
        local: await Fenec.open(wasm),
        leader: false,
        persist: 'scenario',
        shapes: structuredClone(this.shapes),
        fetch: this.fetch,
        tokenProvider: () => {
          this.tokenAsked = true;
          return new Promise(() => {});
        },
        onRefused: (e) => this.refused.push(e.status),
      });
    } catch (e) {
      this.db = null;
      this.error = String(e.message ?? e);
    }
  }

  event(n, name, data) {
    const c = this.shapes[n].collection;
    const s = this.open.find((x) => x.c === c);
    if (!s) throw new Error(`no stream open for \`${c}\``);
    const bytes = new TextEncoder().encode(`event: ${name}\ndata: ${JSON.stringify(data)}\n\n`);
    // Cut as a network cuts it.
    for (let i = 0; i < bytes.length; i += 7) s.ctl.enqueue(bytes.slice(i, i + 7));
  }

  write(w, on) {
    if (w.batch) {
      return this.db.batch(async (t) => {
        for (const x of w.batch) await this.write(x, t);
      });
    }
    const t = on ?? this.db;
    // `"require": n`: the builders' `{ require: n }`.
    const opts = w.require === undefined ? {} : { require: w.require };
    if (w.insert) {
      w.docs.forEach((d, i) => sorted(d, `docs[${i}]`));
      return t.from(w.insert).insert(w.docs, opts);
    }
    const c = w.update ?? w.delete ?? w.get;
    let q = t.from(c);
    sorted(w.where ?? {}, 'where');
    for (const [k, v] of Object.entries(w.where ?? {})) q = q.where(k, v);
    // A read in a batch: the builders' `.require(n)`.
    if (w.get) return (w.require === undefined ? q : q.require(w.require)).rows();
    if (w.update) {
      sorted(w.set, 'set');
      // `{"$inc": n}` is inc(n), as the builders' golden file writes it.
      const set = Object.fromEntries(
        Object.entries(w.set).map(([k, v]) => [k, v !== null && typeof v === 'object' && '$inc' in v ? inc(v.$inc) : v]),
      );
      return q.update(set, opts);
    }
    return q.delete(opts);
  }

  async step(st) {
    this.clear();
    switch (st.do) {
      case 'start':
        if (st.shapes) this.shapes = st.shapes;
        await this.start();
        break;
      case 'restart':
        await this.settle();
        this.db.close();
        if (st.shapes) this.shapes = st.shapes;
        await this.start();
        break;
      case 'seed':
      case 'change': {
        const data = { seq: st.seq ?? 0 };
        for (const k of ['rows', 'puts', 'dels', 'schema']) if (k in st) data[k] = st[k];
        this.event(st.shape ?? 0, st.do, data);
        break;
      }
      case 'drop':
        for (const s of this.open.splice(0)) s.ctl.error(new TypeError('connection reset'));
        break;
      case 'fire': {
        const due = [...this.timers];
        this.timers.clear();
        for (const t of due) t.fn();
        break;
      }
      case 'answer': {
        const p = this.posts.shift();
        if (!p) throw new Error('no request waits for an answer');
        if (!st.status) p.rej(new TypeError('connection reset'));
        else {
          const body = st.body !== undefined ? JSON.stringify(st.body) : '{}';
          p.res(new Response(body, { status: st.status, headers: st.seq ? { 'fenec-seq': String(st.seq) } : {} }));
        }
        break;
      }
      case 'write':
        // A refusal rejects the promise too, and is told to `onRefused`.
        this.write(st.write).catch((e) => {
          if (e.status === undefined) this.error = String(e.message ?? e);
        });
        break;
      case 'net':
        this.up = st.up;
        break;
      case 'stream_status':
        this.streamStatus = st.status;
        break;
      case 'online':
        this.db.setOnline(st.online);
        break;
      case 'token':
        this.db.setToken(st.token);
        break;
      case 'schema':
        this.collections = JSON.stringify(st.collections);
        break;
      default:
        throw new Error(`a step of no kind: ${st.do}`);
    }
    await this.settle();
  }

  rows(c) {
    try {
      return [...(this.db.local.run(`get ${c}`).rows ?? [])].sort((a, b) => a.id - b.id);
    } catch {
      return [];
    }
  }

  check(e, binds) {
    if (e.requests) matches(e.requests, this.requests, binds, 'requests');
    if (e.streams) matches(e.streams, this.streams, binds, 'streams');
    for (const [c, rows] of Object.entries(e.rows ?? {})) matches(rows, this.rows(c), binds, `rows.${c}`);
    if ('pending' in e) matches(e.pending, this.db.status()[0].pending, binds, 'pending');
    if (e.refused) matches(e.refused, this.refused, binds, 'refused');
    if ('token_asked' in e) matches(e.token_asked, this.tokenAsked, binds, 'token_asked');
    if (e.waits) {
      assert.equal(this.waits.length, e.waits.length, `waits: ${e.waits.length} expected, ${JSON.stringify(this.waits)} asked`);
      e.waits.forEach(([lo, hi], i) => {
        const ms = this.waits[i];
        assert.ok(ms >= lo && ms <= hi, `waits: ${ms} ms is not within [${lo}, ${hi}]`);
      });
    }
    if ('ready' in e) matches(e.ready, this.db.status().every((s) => s.seeded), binds, 'ready');
    if (e.error) assert.ok(this.error?.includes(e.error), `error: expected one saying \`${e.error}\`, got ${this.error}`);
    else assert.equal(this.error, null, `an error no expectation names: ${this.error}`);
  }
}

const passed = [];

for (const sc of file.scenarios) {
  test(sc.name, { skip, concurrency: false }, async () => {
    assert.ok(wasm, 'web/fenec.wasm is missing: under CI every scenario runs (make wasm)');
    const run = new Run(sc);
    const restore = run.clock();
    const binds = new Map();
    try {
      for (const [i, raw] of sc.steps.entries()) {
        if ((raw.expect_js || raw.expect_native) && !sc.differs) throw new Error(`step ${i}: a platform's own expectation, and no \`differs\` saying why`);
        if (raw.only && raw.only !== 'js') continue;
        const st = resolve(raw, binds);
        await run.step(st);
        try {
          run.check({ ...st.expect, ...st.expect_js }, binds);
        } catch (e) {
          e.message = `step ${i} (${st.do}): ${e.message}`;
          throw e;
        }
      }
      passed.push(sc.name);
    } finally {
      run.db?.close();
      restore();
    }
  });
}

after(async () => {
  if (skip) return;
  const dir = new URL('../target/sync-scenarios/', import.meta.url);
  await mkdir(dir, { recursive: true });
  await writeFile(new URL('js.txt', dir), passed.map((n) => `${n}\n`).join(''));
});
