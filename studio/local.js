// The studio's way to a database in the page -- the site's playground --
// beside connect.js's way to one on a server, with the same surface: `run`,
// `batch`, `subscribe`, `request` and `stats`. The database is the browser
// module in a dedicated worker (worker.js), which answers each request as
// a server would (engine.js), so every view works unchanged over it.
//
// Only a page that asks for it loads this module: fenec-server's studio
// never does, and its first load holds none of it, nor the worker.

import { h, fill } from './dom.js';

/** Fetched and compiled as the worker's modules load, rather than after. */
async function compile(url) {
  try {
    return await WebAssembly.compileStreaming(fetch(url));
  } catch {
    // A server that sends it as another type: compiled from its bytes.
    return WebAssembly.compile(await (await fetch(url)).arrayBuffer());
  }
}

/** A refusal as a server's is thrown by `FenecHttp`: its words, its status, a batch's statement. */
function refusal(json, status) {
  const e = new Error(json?.error ?? `HTTP ${status}`);
  e.status = status;
  if (typeof json?.at === 'number') e.at = json.at;
  if (typeof json?.completed === 'number') e.completed = json.completed;
  return e;
}

/** Whether the database is kept in this browser, by its key: a viewer's choice, so localStorage. */
const keepKey = (key) => `${key}:keep`;
function remembered(key) {
  try {
    return localStorage.getItem(keepKey(key)) === '1';
  } catch {
    return false;
  }
}

export class Local {
  local = true;
  /** The change the last write left the database at, as a server's `Fenec-Seq`. */
  seq = null;
  onError = null;
  /** Whether the database came back from this browser rather than from its seed. */
  restored = false;
  #worker;
  #key;
  #kept;
  /** Where the docs say how the studio runs on a server. */
  #docs = null;
  #next = 0;
  #waiting = new Map();
  #subs = new Map();

  /**
   * The database in the page: the module at `wasm` compiled here while the
   * worker starts, its collation data at `collation`, `seed` (a batch of
   * `[text, params]`, or the function that makes one) written into it
   * unless it is kept in this browser under `key` and comes back.
   */
  static async open({ wasm, collation, seed = [], key = 'fenec-playground', docs = null }) {
    const worker = new Worker(new URL('./worker.js', import.meta.url), { type: 'module', name: 'fenec' });
    const db = new Local(worker, key);
    db.#docs = docs;
    // The module fetched and compiled while the worker's modules load, and
    // the seed made meanwhile.
    const compiled = compile(wasm);
    const statements = typeof seed === 'function' ? seed() : seed;
    const module = await compiled;
    const { restored } = await db.#call('open', {
      module,
      collation: new URL(collation, location.href).href,
      seed: statements,
      key,
      keep: db.#kept,
    });
    db.restored = restored;
    return db;
  }

  constructor(worker, key) {
    this.#worker = worker;
    this.#key = key;
    this.#kept = remembered(key);
    worker.onmessage = (e) => this.#message(e.data);
    worker.onerror = (e) => {
      const err = new Error(`the database's worker stopped: ${e.message ?? 'it did not load'}`);
      for (const w of this.#waiting.values()) w.reject(err);
      this.#waiting.clear();
      this.onError?.(err);
    };
  }

  #call(op, args = {}) {
    const id = ++this.#next;
    return new Promise((resolve, reject) => {
      this.#waiting.set(id, { resolve, reject });
      this.#worker.postMessage({ id, op, ...args });
    });
  }

  #message(m) {
    if (m.sub !== undefined) {
      const s = this.#subs.get(m.sub);
      if (!s) return;
      if (m.event.type === 'error') {
        const e = new Error(m.event.message);
        e.status = m.event.status;
        (s.opts.onError ?? this.onError)?.(e);
        return;
      }
      if (m.event.type === 'seed') s.opts.onState?.('open');
      s.onEvent(m.event);
      return;
    }
    const w = this.#waiting.get(m.id);
    if (!w) return;
    this.#waiting.delete(m.id);
    if (m.error) w.reject(Object.assign(new Error(m.error.message), { status: m.error.status ?? undefined }));
    else w.resolve(m.value);
  }

  /** As `Remote.request`: `{status, ok, ms, json, text, seq, requestId}`, a refusal answered, not thrown. */
  async request(path, { method = 'GET', body = null } = {}) {
    const started = performance.now();
    const r = await this.#call('request', { method, path, body });
    const ms = performance.now() - started;
    if (r.seq !== null) this.seq = r.seq;
    return { status: r.status, ok: r.status < 300, ms, json: r.json, text: JSON.stringify(r.json), seq: r.seq, requestId: null };
  }

  /** As `FenecHttp.run`: `{rows}` (and `facets`), `{kind: 'affected', count, seq}`, or the answer as it is. */
  async run(text, params = []) {
    const r = await this.request('/query', { method: 'POST', body: JSON.stringify({ query: text, params }) });
    if (!r.ok) throw refusal(r.json, r.status);
    if (Array.isArray(r.json)) return { rows: r.json };
    if (typeof r.json?.affected === 'number') return { kind: 'affected', count: r.json.affected, seq: r.seq, replayed: false };
    return r.json;
  }

  /**
   * As `FenecHttp.batch`: one block, `{results, seq, replayed}`. A key is
   * not kept: nothing here sends a write twice.
   */
  async batch(items) {
    const lines = items.map((item) => {
      const [query, params = []] = typeof item === 'string' ? [item] : item;
      return JSON.stringify({ query, params });
    });
    const r = await this.request('/batch', { method: 'POST', body: lines.join('\n') });
    if (!r.ok) throw refusal(r.json, r.status);
    return { results: r.json?.results ?? [], seq: r.seq, replayed: false };
  }

  /** As `FenecHttp.subscribe`: a seed, then each change; returns the function that stops it. */
  subscribe(collection, shape, onEvent, opts = {}) {
    const sub = ++this.#next;
    this.#subs.set(sub, { onEvent, opts });
    this.#call('subscribe', { sub, collection, shape: shape ?? {} }).catch((e) => (opts.onError ?? this.onError)?.(e));
    return () => {
      if (!this.#subs.delete(sub)) return;
      this.#call('unsubscribe', { sub }).catch(() => {});
    };
  }

  /** The file's sizes are a server's: there is no file here. */
  stats() {
    return null;
  }

  /** Every collection back to its seed. */
  reset() {
    return this.#call('reset');
  }

  get kept() {
    return this.#kept;
  }

  /** Keeps the database in this browser, or forgets it; remembered for the next visit. */
  async keep(on) {
    this.#kept = await this.#call('keep', { on });
    try {
      if (this.#kept) localStorage.setItem(keepKey(this.#key), '1');
      else localStorage.removeItem(keepKey(this.#key));
    } catch {
      /* this browser keeps nothing for the page: the database is kept for this visit */
    }
    return this.#kept;
  }

  /**
   * What the top bar shows of a database in the page, where a server's
   * studio shows the token: the line that says where it runs, and what a
   * person does with it -- keep it, put it back to its seed. `ctx` is the
   * studio's (app.js): `say`, `explain` and `reload`, which reads the
   * collections and the view again.
   */
  controls(ctx) {
    const line = h(
      'button',
      { type: 'button', class: 'who here', title: 'What runs here, and how to keep it', 'aria-label': 'Running in your browser', 'aria-haspopup': 'dialog', onclick: () => this.#dialog(ctx) },
      h('span', { class: 'here-dot', 'aria-hidden': 'true' }),
      h('span', { class: 'who-name here-long' }, 'Running in your browser.'),
      h('span', { class: 'who-name here-short' }, 'In this tab'),
    );
    const server = h('span', { class: 'here-server' }, 'On your server: ', h('code', {}, 'fenec-server --studio'));
    const reset = h('button', { type: 'button', class: 'btn ghost here-reset', onclick: () => this.#confirmReset(ctx) }, 'Reset data');
    return [line, server, reset];
  }

  /** Every collection back to its seed, once the person says so. */
  #confirmReset(ctx, after = null) {
    const d = h(
      'dialog',
      { class: 'dialog', 'aria-label': 'Reset data' },
      h('h2', { class: 'dialog-title' }, 'Reset data?'),
      h('p', {}, 'Every collection goes back to the rows it was seeded with. Collections you made are dropped, and your writes are gone.'),
      h(
        'div',
        { class: 'dialog-actions' },
        h('button', { type: 'button', class: 'btn', onclick: () => d.close() }, 'Keep my changes'),
        h(
          'button',
          {
            type: 'button',
            class: 'btn danger',
            onclick: async (e) => {
              e.target.disabled = true;
              try {
                await this.reset();
                d.close();
                after?.();
                ctx.say('The data is back to its seed.');
                await ctx.reload();
              } catch (err) {
                e.target.disabled = false;
                ctx.say(ctx.explain(err), 'error');
              }
            },
          },
          'Reset data',
        ),
      ),
    );
    document.body.append(d);
    d.addEventListener('close', () => d.remove());
    d.showModal();
    d.querySelector('.btn').focus();
  }

  #dialog(ctx) {
    const keep = h('input', { type: 'checkbox', id: 'here-keep', checked: this.#kept });
    const kept = h('dd', {});
    const said = () =>
      fill(kept, this.#kept ? 'In this browser (IndexedDB): it is here again on your next visit, as you left it.' : 'Nowhere: closing the tab forgets it.');
    said();
    keep.addEventListener('change', async () => {
      keep.disabled = true;
      try {
        await this.keep(keep.checked);
        ctx.say(this.#kept ? 'The database is kept in this browser.' : 'Nothing is kept in this browser now.');
      } catch (err) {
        keep.checked = this.#kept;
        ctx.say(ctx.explain(err), 'error');
      }
      keep.disabled = false;
      said();
    });
    const d = h(
      'dialog',
      { class: 'dialog', 'aria-label': 'Running in your browser' },
      h('h2', { class: 'dialog-title' }, 'Running in your browser'),
      h(
        'dl',
        { class: 'facts' },
        h('dt', {}, 'Engine'),
        h('dd', {}, 'fenecdb compiled to WebAssembly, on a worker of this tab: the engine fenec-server runs. Nothing you type is sent anywhere.'),
        h('dt', {}, 'Data'),
        h('dd', {}, this.restored ? 'Brought back from this browser.' : 'products, orders and events, seeded as the page opened.'),
        h('dt', {}, 'Kept'),
        kept,
      ),
      h('label', { class: 'here-keep', for: 'here-keep' }, keep, ' Keep it in this browser'),
      h('h3', {}, 'On your server'),
      h('p', {}, 'The same studio, over your own data, served by the server itself:'),
      h('pre', { class: 'statement' }, 'fenec-server --file data.fenec --studio'),
      h(
        'p',
        { class: 'hint' },
        'Then open /_studio/ on it and sign in with its token. ',
        this.#docs ? h('a', { href: this.#docs, target: '_top' }, 'How the studio runs on a server') : null,
      ),
      h(
        'div',
        { class: 'dialog-actions' },
        h('button', { type: 'button', class: 'btn ghost danger', onclick: () => this.#confirmReset(ctx, () => d.close()) }, 'Reset data'),
        h('button', { type: 'button', class: 'btn', onclick: () => d.close() }, 'Close'),
      ),
    );
    document.body.append(d);
    d.addEventListener('close', () => d.remove());
    d.showModal();
    keep.focus();
  }
}
