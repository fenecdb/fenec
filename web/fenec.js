// fenecdb browser client.
//
// No dependencies, no build step, no npm. All it needs is `fenec.wasm`.
//
// Two layers, both taking the same path:
//
//   raw FenecQL -- synchronous, a single call
//     import { Fenec } from './fenec.js';
//     const db = await Fenec.open('./fenec.wasm');
//     db.run('create collection docs (title text, embed vector<3> @hnsw(cosine))');
//     const r = db.run('get docs near embed $1 limit 5', [vec]);
//
//   query builder -- dynamic filters, every value bound to a parameter
//     const rows = await db.from('docs')
//       .where('year', '>=', 2024)
//       .near('embed', vec, { ef: 128 })
//       .limit(10)
//       .rows();
//
// The builder hides nothing: `toFenecQL()` hands back the generated text
// and the parameters verbatim. Types: `fenec types data.fenec > fenec-schema.d.ts`.

const enc = new TextEncoder();
const dec = new TextDecoder();

// How many `lookup` levels one query may chain. The engine refuses a deeper
// one; failing here never sends the query.
const MAX_LOOKUP_DEPTH = 8;

export class FenecError extends Error {}

/**
 * What each database, endpoint and replica was opened with when its code
 * declares its schema (`@fenecdb/web/schema`): the tables, the relations
 * `lookup` names, the migrations.
 */
const declared = new WeakMap();

/**
 * The code's schema checked against `to` -- the database in the page or
 * the server's -- and applied where `how` says (`apply`; `follow` compares a
 * schema another owns). The schema is FenecQL text, or tables declared in
 * code (`@fenecdb/web/schema`), which write themselves as it: this module
 * imports none of that one. The engine decides; a refusal throws, naming
 * every difference and how to resolve it (`refusals`).
 */
async function checked(to, opts, how) {
  const s = opts?.schema;
  if (!s) return to;
  declared.set(to, opts);
  const fenecql = typeof s === 'string' ? s : Object.values(s).map((t) => t.toFenecQL()).join('\n');
  const description = { format: 1, fenecql, migrations: opts.migrations ?? [] };
  for (;;) {
    let out;
    try {
      out = await to.checkSchema(description, how);
    } catch (e) {
      // A migration comparing text in a collation the module has not been
      // handed: fetched, and the whole run again, none of which landed.
      if (e.collation && (await to.collation?.(...e.collation))) continue;
      throw e;
    }
    if (!out.refusals.length) return to;
    const head =
      how === 'follow'
        ? "the code's schema is not the one the server holds, and the server's is the one that counts"
        : "the database's schema differs from the code's, and nothing was applied";
    // The engine writes a rebuild as a description holds it; here, as the helper that makes one.
    const fix = (f) => f.replace(/\{"rebuild": \{"collection": "([^"]+)", "field": "([^"]+)"\}\} \(rebuild in the SDKs\)/, "rebuild('$1', '$2')");
    const lines = out.refusals.map((r) => `  - ${r.message}\n    ${fix(r.fix)}`);
    throw Object.assign(new FenecError(`${head}:\n${lines.join('\n')}`), { refusals: out.refusals });
  }
}

/**
 * Where a database opened with a schema stood once it was made: a load
 * (`restore`, `openFile`) adds the image's collections to what a database
 * holds, so one holding what its schema made and nothing since is emptied
 * first, and the one loaded checked instead.
 */
const untouched = new WeakMap();
const replaceable = (f) => untouched.get(f) === f.changeSeq;

/** A collection's name, or a table's (`fenecTable`). */
const nameOf = (n) => (n && typeof n === 'object' ? n.$name : n);

/** A second database over a database's module (`Fenec`'s static block). */
let sibling;
/** A database made empty again, its handle a new one (`Fenec`'s static block). */
let emptied;

// The module is built with WebAssembly SIMD (Chrome 91, Firefox 89, Safari
// 16.4). An engine without it fails to compile it with an opaque message; this
// probe -- one function returning a v128 -- turns that into one that says why.
const SIMD_PROBE = new Uint8Array([
  0, 97, 115, 109, 1, 0, 0, 0, 1, 5, 1, 96, 0, 1, 123, 3, 2, 1, 0, 10, 10, 1, 8, 0,
  65, 0, 253, 15, 253, 98, 11,
]);

function whyNoModule(err) {
  return WebAssembly.validate(SIMD_PROBE)
    ? err
    : new FenecError('fenec.wasm needs WebAssembly SIMD (Chrome 91+, Firefox 89+, Safari 16.4+)');
}

export class Fenec {
  #wasm; #handle; #collation;
  /** The live queries (`live`), made with the first; and whether a run of them is due. */
  #lives = null;
  #due = false;

  /**
   * The time, in milliseconds since the epoch, a statement is answered at:
   * a read of a collection whose rows expire (`@ttl`) leaves out those past
   * their time by it. The module has no clock; a test pins it.
   */
  now = Date.now;

  /**
   * Where a live query's error goes when it was given no `onError` of its
   * own (`live`); with neither, it is thrown, as `FenecSync` throws it.
   */
  onError = null;

  constructor(wasm, handle, collation = null) {
    this.#wasm = wasm;
    this.#handle = handle;
    this.#collation = collation;
  }

  static {
    // For `openFile` to try bytes in without touching the database it keeps.
    sibling = (f) => new Fenec(f.#wasm, f.#wasm.fenec_open(), f.#collation);
    emptied = (f) => {
      f.#wasm.fenec_close(f.#handle);
      f.#handle = f.#wasm.fenec_open();
    };
  }

  /**
   * Loads the WASM module and opens an empty database.
   * @param {string|BufferSource|WebAssembly.Module} src  A URL, the module
   *   bytes themselves, or the module compiled. The bytes are for Node:
   *   `fetch` cannot resolve a relative path there, so the file is read with
   *   `readFile` and handed over directly. The compiled module is for
   *   Cloudflare Workers, which import a `.wasm` file as one and refuse to
   *   compile bytes at run time.
   * @param {{collation?: string|URL|Function}} opts  Where the collation
   *   data the module does not carry comes from (`collation()`): the URL of
   *   the directory holding `<name>.bin`, or a function handed the name and
   *   returning its bytes or a `Response`. By default `collate/` beside the
   *   module, when `src` is a URL.
   */
  static async open(src = './fenec.wasm', opts = {}) {
    let mod;
    try {
      if (typeof src !== 'string') {
        mod = await WebAssembly.instantiate(src, {});
      } else {
        try {
          mod = await WebAssembly.instantiateStreaming(fetch(src), {});
        } catch {
          // Fallback for servers that return the wrong MIME type.
          mod = await WebAssembly.instantiate(await (await fetch(src)).arrayBuffer(), {});
        }
      }
    } catch (e) {
      throw whyNoModule(e);
    }
    // Instantiated from a compiled module, `instantiate` gives the instance
    // alone, and from bytes the module and the instance: read as the
    // latter, a Worker's module had no exports and `open` threw.
    const wasm = (mod instanceof WebAssembly.Instance ? mod : mod.instance).exports;
    // With `schema`, what the code declares is checked and made: a load
    // (`restore`, `openFile`) checks the database it brings in again.
    const db = new Fenec(wasm, wasm.fenec_open(), opts.collation ?? beside(src));
    return checked(db, opts, 'apply').then(
      () => {
        if (opts.schema) untouched.set(db, db.changeSeq);
        return db;
      },
      (e) => {
        db.close();
        throw e;
      },
    );
  }

  /** Connects to a remote HTTP endpoint: `Fenec.connect('http://host:8080')`. */
  static connect(url, opts = {}) {
    return connect(url, opts);
  }

  get version() {
    return this.#readString(this.#wasm.fenec_version());
  }

  /** The length prefix of a returned buffer. */
  #len(ptr) {
    return new DataView(this.#wasm.memory.buffer).getUint32(ptr, true);
  }

  /** Reads the returned buffer and frees it. */
  #readBytes(ptr) {
    const len = this.#len(ptr);
    // Copies, and has to: the array outlives the buffer freed on the next
    // line, and a view into wasm memory would read whatever lands there next.
    const out = new Uint8Array(this.#wasm.memory.buffer, ptr + 4, len).slice();
    this.#wasm.fenec_free(ptr, 4 + len);
    return out;
  }

  // Decodes straight out of wasm memory. A string does not need the copy
  // `#readBytes` makes -- the decode is itself the copy -- and going through
  // it first meant every result was copied twice, which on a large result set
  // is megabytes of pure waste. The decode must finish before the free.
  #readString(ptr) {
    const len = this.#len(ptr);
    const out = dec.decode(new Uint8Array(this.#wasm.memory.buffer, ptr + 4, len));
    this.#wasm.fenec_free(ptr, 4 + len);
    return out;
  }

  /** Copies a string, or bytes, into WASM memory. */
  #write(data) {
    const bytes = typeof data === 'string' ? enc.encode(data) : data;
    const ptr = this.#wasm.fenec_alloc(bytes.length || 1);
    new Uint8Array(this.#wasm.memory.buffer).set(bytes, ptr);
    return [ptr, bytes.length];
  }

  /**
   * Runs FenecQL.
   * @param {string} sql
   * @param {Array} params  values for `$1`, `$2`...
   * @returns {{kind:string, ...}} `{columns, rows}` for row results
   */
  run(sql, params = []) {
    return this.#run(sql, params, null);
  }

  /**
   * `run`, the parameters at `asJson` sent as JSON rather than apart; a
   * live query's own run `quiet`, which has nothing to tell them.
   */
  #run(sql, params, asJson, quiet = false) {
    // A typed array sent as JSON goes as the numbers it holds.
    if (asJson) params = params.map((p, i) => (asJson.includes(i) && ArrayBuffer.isView(p) ? Array.from(p) : p));
    // A module from before vectors went over as f32s takes five arguments,
    // and would read the JSON's `null` where each vector goes.
    const [json, vectors] = this.#wasm.fenec_query.length > 5 ? vectorsApart(params, asJson) : [params, null];
    const [sp, sl] = this.#write(sql);
    const [pp, pl] = this.#write(JSON.stringify(json));
    const [vp, vl] = vectors ? this.#write(vectors) : [0, 0];
    let out;
    try {
      // The time a read of a collection whose rows expire (`@ttl`) is
      // answered at: the module has no clock of its own.
      out = this.#readString(this.#wasm.fenec_query(this.#handle, sp, sl, pp, pl, vp, vl, this.now()));
    } finally {
      this.#wasm.fenec_free(sp, sl || 1);
      this.#wasm.fenec_free(pp, pl || 1);
      if (vectors) this.#wasm.fenec_free(vp, vl || 1);
    }
    // After an error too: the change ring says what landed, after the task.
    if (this.#lives?.size && !quiet) this.#touch();
    // Before the answer, and before an error too: a statement that failed
    // may follow ones in the same text that wrote.
    kept.get(this)?.flush();
    const res = JSON.parse(out);
    // A json field takes a list of numbers as written, not as the f32s it
    // went over apart as: the module names those, before running anything,
    // and they go again as JSON.
    if (res.kind === 'error' && res.exact && !asJson) return this.#run(sql, params, res.exact, quiet);
    if (res.kind === 'error') {
      const e = new FenecError(res.message);
      // Refused for collation data the module has not been handed: which,
      // and how many statements before this one ran (`query` runs it again
      // only when none did).
      if (res.chunks) Object.assign(e, { collation: res.chunks, ran: res.ran ?? 0 });
      throw e;
    }
    return res.kind === 'rows' ? res.result : res;
  }

  /**
   * `run`, fetching the collation data a statement is refused for and
   * running it again: the module carries `latin`, and a statement that
   * compares text of another script in a collation needs that script's
   * data first. Only a statement nothing ran before is run again -- the
   * refusal comes before it changes anything, but a statement ahead of it
   * in the same text may have.
   */
  async query(sql, params = []) {
    return this.#fetching(sql, params, false);
  }

  async #fetching(sql, params, quiet) {
    for (;;) {
      try {
        return this.#run(sql, params, null, quiet);
      } catch (e) {
        if (!e.collation || e.ran || !(await this.#fetched(e.collation))) throw e;
      }
    }
  }

  /**
   * Hands the module the collation data `names` names -- `'all'` for every
   * script's. It carries `latin`; `collate und` and `collate tr` over other
   * scripts need theirs: greek, cyrillic, middle-east, indic,
   * southeast-asia, east-asia, han, han-ext, symbols, other, smp. `query`
   * and the builder fetch what a statement needs by themselves; this is
   * for having it before the first one does. True when it fetched any.
   */
  async collation(...names) {
    return this.#fetched(names.includes('all') ? this.#collationState().chunks : names);
  }

  /** `{chunks, loaded}`: every chunk's name, and a bit each for those here. */
  #collationState() {
    return JSON.parse(this.#readString(this.#wasm.fenec_collation()));
  }

  /**
   * Fetches and hands over those of `names` the module has not got. False
   * when it had them all: what refused a statement was not their absence.
   */
  async #fetched(names) {
    const { chunks, loaded } = this.#collationState();
    const want = names.filter((name) => {
      const i = chunks.indexOf(name);
      if (i < 0) throw new FenecError(`no collation data named ${JSON.stringify(name)}; there are ${chunks.join(', ')}`);
      return !(loaded & (1 << i));
    });
    const got = await Promise.all(want.map((name) => this.#chunk(name)));
    got.forEach((bytes, k) => {
      const ptr = this.#wasm.fenec_alloc(bytes.length || 1);
      new Uint8Array(this.#wasm.memory.buffer).set(bytes, ptr);
      let at;
      try {
        at = this.#wasm.fenec_add_chunk(ptr, bytes.length);
      } finally {
        this.#wasm.fenec_free(ptr, bytes.length || 1);
      }
      if (chunks[at] !== want[k]) {
        throw new FenecError(`${want[k]}.bin is not this module's collation data (another version's?)`);
      }
    });
    return want.length > 0;
  }

  async #chunk(name) {
    const from = this.#collation;
    if (!from) {
      throw new FenecError(
        `the collation data ${name} is needed: open the module with { collation } -- ` +
          `the URL of the directory holding ${name}.bin, or a function returning its bytes`,
      );
    }
    let got = typeof from === 'function' ? await from(name) : await fetch(new URL(`${name}.bin`, from));
    if (typeof got?.arrayBuffer === 'function') {
      if (got.ok === false) throw new FenecError(`${name}.bin: HTTP ${got.status}`);
      got = await got.arrayBuffer();
    }
    return got instanceof Uint8Array ? got : new Uint8Array(got.buffer ?? got);
  }

  /**
   * The database against a schema declared in code -- its description,
   * `{format, collections, migrations}` (`@fenecdb/web/schema`'s
   * `describe`) -- in the engine: `'plan'` says what an apply would do,
   * `'apply'` runs the migrations not yet recorded and makes what only
   * adds, one block, or nothing while anything is refused. Answers
   * `{applied, ran, migrations, statements, refusals}`; `Fenec.open`'s
   * `schema` is this, and throws on a refusal.
   */
  checkSchema(description, mode = 'plan') {
    const call = this.#wasm.fenec_schema;
    if (!call || mode === 'follow') {
      throw new FenecError(call ? 'a database in the page is its own: it plans or applies' : 'this fenec.wasm was built without the schema check');
    }
    const [p, l] = this.#write(typeof description === 'string' ? description : JSON.stringify(description));
    let out;
    try {
      out = JSON.parse(this.#readString(call(this.#handle, p, l, mode === 'apply' ? 1 : 0, this.now())));
    } finally {
      this.#wasm.fenec_free(p, l || 1);
    }
    if (out.kind === 'error') throw Object.assign(new FenecError(out.message), out.chunks ? { collation: out.chunks, ran: 0 } : {});
    if (out.applied) {
      if (this.#lives?.size) this.#touch();
      kept.get(this)?.flush();
    }
    return out;
  }

  /** Returns the query result as a plain array of objects. */
  rows(sql, params = []) {
    const r = this.run(sql, params);
    return r.rows ?? [];
  }

  /**
   * Query builder. Validates the collection name and binds the query to
   * this connection.
   * @param {string} name
   * @returns {Query}
   */
  from(name) {
    return new Query({
      collection: ident(nameOf(name), 'collection'),
      exec: (sql, params) => this.query(sql, params),
      // What `useLiveQuery` finds the query's database by.
      context: this,
      rel: declared.get(this)?.relations,
    });
  }

  /**
   * Live query: `cb` is handed the rows now, and again after every write
   * to a collection the query reads -- a `run` of this page's, the
   * builder's writes, a `restore` or `openFile` loading the database.
   * `query` is a builder `Query`, a FenecQL text, or `[text, params]` (what
   * `toFenecQL` gives). The collections a builder query reads are known;
   * a text's are named with `{collections}`, and without them every write
   * runs it again. Returns the function that stops it.
   *
   * The writes of one task run each live query once, in a microtask after
   * it, from scratch: well under a millisecond in the page (`FenecSync.live`
   * says why nothing finer).
   *
   * @param {{onError?: Function, params?: Array, collections?: string[]}} opts
   */
  live(query, cb, opts = {}) {
    this.#lives ??= new Lives(this);
    return this.#lives.add(query, cb, opts, (sql, params) => this.#fetching(sql, params, true), opts.onError ?? this.onError);
  }

  /** The live queries looked at after the task that wrote: once, however many writes it made. */
  #touch() {
    if (this.#due) return;
    this.#due = true;
    queueMicrotask(() => {
      this.#due = false;
      this.#lives.tick();
    });
  }

  /**
   * What changed since `since`: `{seq, horizon, collections}`.
   *
   * When `collections === null` the cursor has fallen behind the change
   * ring and there is no telling which collection changed -- the caller
   * must treat everything as stale. Live queries are built on this.
   */
  changes(since = 0) {
    return JSON.parse(
      this.#readString(this.#wasm.fenec_changes(this.#handle, since)),
    );
  }

  /** Current value of the change counter. */
  get changeSeq() {
    // `since` is ahead of the counter: the Rust side never scans the ring.
    return this.changes(Number.MAX_SAFE_INTEGER).seq;
  }

  /**
   * Entry count of the change ring. Locally its only effect is how far
   * behind a live query may fall and still catch up; on overflow the
   * answer is simply "everything is stale" -- no data is lost.
   */
  setChangeCapacity(n) {
    this.#wasm.fenec_set_change_capacity(this.#handle, whole(n, 'capacity'));
  }

  /** Collection schemas (the `collections` output). */
  schemas() {
    return this.run('collections').collections ?? [];
  }

  /** Collection statistics. */
  stats() {
    return JSON.parse(this.#readString(this.#wasm.fenec_stats(this.#handle)));
  }

  /** Byte image of the whole database (to write into IndexedDB/OPFS). */
  snapshot() {
    const parts = [...this.snapshotChunks()];
    if (parts.length === 1) return parts[0];
    const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
    let at = 0;
    for (const p of parts) {
      out.set(p, at);
      at += p.length;
    }
    return out;
  }

  /**
   * The image a mebibyte at a time, each let go of in the module as it is
   * taken: the image is never in the module twice, nor whole here -- which
   * a Durable Object's 128 MB needs, its storage taking pieces anyway.
   */
  *snapshotChunks() {
    // A module from before chunks gives the image whole.
    if (!this.#wasm.fenec_snapshot_chunks) {
      yield this.#readBytes(this.#wasm.fenec_snapshot(this.#handle));
      return;
    }
    const n = this.#wasm.fenec_snapshot_chunks(this.#handle);
    for (let i = 0; i < n; i++) yield this.#readBytes(this.#wasm.fenec_snapshot_chunk(this.#handle));
  }

  /**
   * Starts keeping the writes for `drain()`, or with `false` stops;
   * `persist` and `openFile` start it themselves.
   */
  journal(on = true) {
    this.#wasm.fenec_journal(this.#handle, on ? 0 : 1);
  }

  /**
   * The writes since the last drain, `{replace, bytes}`: frames to append
   * to a stored image, or with `replace` an image to store instead.
   */
  drain() {
    const b = this.#readBytes(this.#wasm.fenec_drain(this.#handle));
    return { replace: b[0] === 1, bytes: b.subarray(1) };
  }

  /**
   * Restores from a byte image, and returns how many of its bytes that
   * took: all of them, or those before a last record a crash cut short. An
   * image whose collated text needs collation data the module has not been
   * handed is refused, as `run` refuses a statement (`e.collation`):
   * `restore` fetches it and loads the image again.
   */
  load(bytes) {
    if (kept.has(this)) throw new FenecError('the database is kept in a file (openFile): a load would leave the file behind');
    const ptr = this.#wasm.fenec_alloc(bytes.length);
    new Uint8Array(this.#wasm.memory.buffer).set(bytes, ptr);
    // The module keeps the bytes it is handed and reads the documents out
    // of them, rather than copy each into memory of its own: 10 000 rows of
    // 768 dimensions restored held 97 MB, and hold 66. One from before
    // copies them, and the bytes are freed here.
    const owned = this.#wasm.fenec_load_owned;
    let r;
    try {
      r = (owned ?? this.#wasm.fenec_load)(this.#handle, ptr, bytes.length);
    } finally {
      if (!owned) this.#wasm.fenec_free(ptr, bytes.length);
    }
    if (r & 2) {
      const { chunks } = this.#collationState();
      const e = new FenecError('the image needs collation data the module has not been handed');
      throw Object.assign(e, { collation: chunks.filter((_, i) => (r >>> 2) & (1 << i)), ran: 0 });
    }
    if (r !== 0) throw new FenecError('could not load image (corrupt or incompatible version)');
    // Another database now, whose counter may stand where a live query's
    // cursor did: every live query runs again, whatever the ring says.
    if (this.#lives?.size) {
      this.#lives.stale();
      this.#touch();
    }
    return this.#wasm.fenec_loaded(this.#handle) >>> 0;
  }

  /** `load`, fetching the collation data the image needs and loading it again. */
  async loadAsync(bytes) {
    for (;;) {
      try {
        return this.load(bytes);
      } catch (e) {
        if (!e.collation || !(await this.#fetched(e.collation))) throw e;
      }
    }
  }

  close() {
    this.#lives?.clear();
    this.#wasm.fenec_close(this.#handle);
  }
}

// ------------------------------------------------------------ live queries

/**
 * The live queries over one database: `Fenec.live`'s and `FenecSync.live`'s.
 * The change ring says which collections were written since a cursor
 * (`changes`), a block's once it lands whole, so the notice costs a write
 * nothing: it is asked for once after a burst of writes, and only by a
 * database holding a live query. Each holder says when (`tick`): a page's
 * own database after the task that wrote, a replica at the next frame, as a
 * seed lands in chunks and should not be shown half way.
 */
class Lives {
  #db;
  #subs = [];
  #cursor;
  #all = false;

  constructor(db) {
    this.#db = db;
    this.#cursor = db.changeSeq;
  }

  get size() {
    return this.#subs.length;
  }

  /** A live query: `cb` handed its rows now, and again after a write it may read. */
  add(query, cb, opts, exec, onError) {
    if (typeof cb !== 'function') throw new FenecError('live(query, cb): cb must be a function');
    let rows;
    let reads;
    if (query instanceof Query) {
      const bound = query.plain().bind(exec);
      rows = () => bound.rows();
      reads = query.reads;
    } else {
      const [sql, params] = typeof query === 'string' ? [query, opts.params ?? []] : Array.isArray(query) ? query : [];
      if (typeof sql !== 'string') throw new FenecError('live: a Query, a FenecQL text, or [text, params]');
      rows = async () => (await exec(sql, params ?? [])).rows ?? [];
      reads = null;
    }
    if (opts.collections) reads = opts.collections.map((c) => ident(c, 'collection'));
    // With none before it no tick has kept the cursor up: it starts here,
    // where the first run reads.
    if (this.#subs.length === 0) this.#cursor = this.#db.changeSeq;
    const entry = { rows, reads, cb, onError, on: true };
    this.#subs.push(entry);
    // The first rows right away: a subscriber should not start on a blank
    // screen. An error with no `onError` to go to is a rejection nothing
    // waits for, as it is at a tick.
    this.#run(entry);
    return () => {
      entry.on = false;
      const i = this.#subs.indexOf(entry);
      if (i >= 0) this.#subs.splice(i, 1);
    };
  }

  /** Every live query runs at the next tick: the database was replaced. */
  stale() {
    this.#all = true;
  }

  clear() {
    for (const e of this.#subs) e.on = false;
    this.#subs.length = 0;
  }

  /**
   * Runs again the live queries that read what was written since the last
   * tick -- collection granularity: anything finer (intersecting id sets)
   * would cost more than the local query itself.
   */
  async tick() {
    const info = this.#db.changes(this.#cursor);
    this.#cursor = info.seq;
    const dirty = this.#all || info.collections === null ? null : new Set(info.collections);
    this.#all = false;
    // Only reads since: nothing to run again, not even a query whose
    // collections are not known.
    if (dirty?.size === 0) return;
    let failed = null;
    for (const e of [...this.#subs]) {
      if (dirty && e.reads && !e.reads.some((c) => dirty.has(c))) continue;
      // One that throws does not keep the rest from their rows.
      await this.#run(e).catch((err) => {
        failed ??= { err };
      });
    }
    if (failed) throw failed.err;
  }

  async #run(e) {
    try {
      const rows = await e.rows();
      // Stopped while it ran: its rows go nowhere.
      if (e.on) e.cb(rows);
    } catch (err) {
      if (!e.on) return;
      if (e.onError) e.onError(err);
      else throw err;
    }
  }
}

/** `collate/` beside the module, where `make wasm` puts the collation data. */
function beside(src) {
  if (typeof src !== 'string' && !(src instanceof URL)) return null;
  try {
    return new URL('collate/', new URL(src, globalThis.location?.href));
  } catch {
    return null;
  }
}

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

  /** A POST's JSON answer; a status but `ok` among `also` is an error. */
  async #post(path, payload, also = []) {
    const headers = { 'content-type': 'application/json' };
    if (this.#token) headers.authorization = `Bearer ${this.#token}`;
    const res = await this.#fetch(`${this.#url}${path}`, { method: 'POST', headers, body: JSON.stringify(payload) });
    const text = await res.text();
    let body;
    try {
      body = text ? JSON.parse(text) : null;
    } catch {
      throw new FenecError(`server did not return JSON (${res.status}): ${text.slice(0, 200)}`);
    }
    if (!res.ok && !also.includes(res.status)) {
      throw new FenecError(body?.error ?? `HTTP ${res.status}`);
    }
    return body;
  }

  async rows(sql, params = []) {
    return (await this.run(sql, params)).rows ?? [];
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
      rel: declared.get(this)?.relations,
    });
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

// ----------------------------------------------------------- query builder
//
// Why it exists: every interface with conditional filters forced manual
// string concatenation and manual `$n` bookkeeping. The builder produces
// the same FenecQL -- but every leaf value is bound to a parameter, the only
// injection boundary is name validation, and the generated text can be
// inspected with `toFenecQL()`.
//
// The builder is transport independent: `from('docs')` works on its own and
// `bind()` attaches it to any executor (wasm, HTTP, fenec-server).

/**
 * Operator names -- both symbols and words are accepted. No prototype: a
 * name looked up in an object literal found `constructor`, and
 * `where('a', 'constructor', 1)` wrote `Object`'s source into the text.
 */
const OPS = {
  __proto__: null,
  '=': '=', eq: '=',
  '!=': '!=', ne: '!=', neq: '!=',
  '<': '<', lt: '<',
  '<=': '<=', lte: '<=', le: '<=',
  '>': '>', gt: '>',
  '>=': '>=', gte: '>=', ge: '>=',
  '~': '~', like: '~', contains: '~',
  has: 'has',
  in: 'in',
};

// FenecQL identifier: the same rule as the lexer (starts with a Unicode
// letter or `_`, then letters/digits/`_`). Names cannot be parameterised,
// so this is exactly where the injection boundary sits.
const IDENT = /^[\p{Alphabetic}_][\p{Alphabetic}\p{N}_]*$/u;
// A field, or a path into a json field: names joined by dots, as the lexer
// reads `meta.source.rank` -- where a query reads, orders or writes a field.
const PATH = /^[\p{Alphabetic}_][\p{Alphabetic}\p{N}_]*(\.[\p{Alphabetic}_][\p{Alphabetic}\p{N}_]*)*$/u;

/**
 * The `order` spec of a `lookup`: `'created'`, or `[['created','desc'], ...]`,
 * a pair taking `{ collate }` third as `order()` does. A bare string is one
 * ascending key; anything else is a list of pairs, so there is no reading
 * under which `['a','desc']` could mean two fields.
 */
function orderKeys(spec) {
  if (spec === undefined || spec === null) return [];
  if (typeof spec === 'string') return [{ field: path(spec), asc: true, collate: null }];
  return spec.map((k) => {
    const [field, dir = 'asc', opts = {}] = [k].flat();
    return { field: path(field), asc: direction(dir), collate: collation(opts.collate) };
  });
}

function direction(dir) {
  const d = String(dir).toLowerCase();
  if (d !== 'asc' && d !== 'desc') {
    throw new FenecError(`order direction must be 'asc' or 'desc': ${dir}`);
  }
  return d === 'asc';
}

// The collations the engine knows. The name is spliced into the query text,
// so it is checked against the list rather than against the name pattern.
const COLLATIONS = ['und', 'tr'];

function collation(name) {
  if (name === undefined || name === null) return null;
  if (!COLLATIONS.includes(name)) {
    throw new FenecError(`unknown collation: ${JSON.stringify(name)}; there are 'und' and 'tr'`);
  }
  return name;
}

// `name collate tr desc`: one key of an `order`.
function sortKey(o) {
  return `${o.field}${o.collate ? ` collate ${o.collate}` : ''} ${o.asc ? 'asc' : 'desc'}`;
}

function ident(name, what = 'field') {
  if (typeof name !== 'string' || !IDENT.test(name)) {
    throw new FenecError(`invalid ${what} name: ${JSON.stringify(name)}`);
  }
  return name;
}

/** A field's name, or a path into a json field: `'meta.lang'`. */
function path(name) {
  if (typeof name !== 'string' || !PATH.test(name)) {
    throw new FenecError(`invalid field name: ${JSON.stringify(name)}`);
  }
  return name;
}

/**
 * A select-list item: a field, or an aggregate spelled as FenecQL spells it
 * -- `count(*)`, `sum(total)`, `avg(f)`, `min(f)`, `max(f)` -- which answers
 * under that same name.
 */
const AGGREGATE = /^(count)\(\*?\)$|^(sum|avg|min|max)\(([A-Za-z_][A-Za-z0-9_]*)\)$/i;

function column(name) {
  const m = typeof name === 'string' ? AGGREGATE.exec(name.trim()) : null;
  if (!m) return { text: path(name), aggregate: false };
  const text = m[1] ? 'count(*)' : `${m[2].toLowerCase()}(${m[3]})`;
  return { text, aggregate: true };
}

/** `limit`, `offset`, `ef` cannot be parameterised: FenecQL wants a literal. */
function whole(n, what) {
  if (!Number.isSafeInteger(n) || n < 0) {
    throw new FenecError(`${what} must be a non-negative integer: ${n}`);
  }
  return n;
}

/**
 * Coerces a JS value into the shape fenecdb understands, before it reaches
 * JSON. `Float32Array` is the natural type for shipping embeddings, but
 * `JSON.stringify` turns it into an object -- which fenecdb would reject
 * with "a JSON object is not supported as a fenecdb value".
 */
function normalize(v, what = 'value') {
  if (v === undefined) {
    throw new FenecError(`${what} is undefined -- did you mean 'null'?`);
  }
  if (v === null || typeof v !== 'object') return v;
  if (v instanceof Date) return v.toISOString();
  if (ArrayBuffer.isView(v) && !(v instanceof DataView)) return Array.from(v);
  if (Array.isArray(v)) return v.map((x) => normalize(x, what));
  // A plain object is a json field's value, each member as a value is.
  const proto = Object.getPrototypeOf(v);
  if (proto === Object.prototype || proto === null) {
    const out = {};
    for (const [k, x] of Object.entries(v)) {
      if (x !== undefined) out[k] = normalize(x, what);
    }
    return out;
  }
  throw new FenecError(`this object cannot be used as a fenecdb value (${what})`);
}

// Whether this machine's typed arrays are little-endian, as the module
// reads the vectors `vectorsApart` hands it. Every browser's are.
const LITTLE = new Uint8Array(new Uint32Array([1]).buffer)[0] === 1;

/**
 * The parameters that are vectors, apart from the rest, as `f32`s: a
 * typed array or an array of finite numbers, which the module reads as a
 * vector either way. Each goes over as its place among the parameters and
 * its length, then its values, and the JSON holds `null` where it goes.
 * Written out as text and read back, a page of 200 768-dim vectors spent
 * most of its time on the digits, both sides of the call. A `-0` goes over
 * as `0`, as JSON writes it, so either way stores the same vector. Those
 * at `asJson` stay in the JSON: a json field keeps a list's numbers as
 * written, which the module asks for by their places.
 */
function vectorsApart(params, asJson = null) {
  const found = [];
  let size = 0;
  params.forEach((p, i) => {
    if (asJson?.includes(i)) return;
    const list = Array.isArray(p) || (ArrayBuffer.isView(p) && !(p instanceof DataView)) ? p : null;
    if (!LITTLE || !list || list.length === 0) return;
    for (let k = 0; k < list.length; k++) {
      if (typeof list[k] !== 'number' || !Number.isFinite(list[k])) return;
    }
    found.push(i);
    size += 8 + 4 * list.length;
  });
  if (found.length === 0) return [params, null];
  const bytes = new Uint8Array(size);
  const words = new Uint32Array(bytes.buffer);
  const floats = new Float32Array(bytes.buffer);
  const json = params.slice();
  let at = 0;
  for (const i of found) {
    const n = params[i].length;
    words[at] = i;
    words[at + 1] = n;
    floats.set(params[i], at + 2);
    for (let k = at + 2; k < at + 2 + n; k++) if (floats[k] === 0) floats[k] = 0;
    json[i] = null;
    at += 2 + n;
  }
  return [json, bytes];
}

function isSpec(v) {
  return (
    v !== null &&
    typeof v === 'object' &&
    !Array.isArray(v) &&
    !(v instanceof Date) &&
    !ArrayBuffer.isView(v)
  );
}

// ---------------------------------------------------------- condition tree

// What `or`, `and`, `not` and `raw` make carries this mark, which no object
// written by hand can: told apart by its `t` alone, `where({ t: 'or' })` --
// a field called `t` -- was taken for a node and failed with a TypeError.
const NODE = Symbol('fenec.condition');

function marked(node) {
  node[NODE] = true;
  return node;
}

/** `or(a, b)` / `or([a, b])` -- joins conditions with `or`. */
export function or(...conds) {
  return marked({ t: 'or', items: conds.flat().map(toCond) });
}

/** `and(a, b)` -- `where` already ands; this is only needed inside `or`. */
export function and(...conds) {
  return marked({ t: 'and', items: conds.flat().map(toCond) });
}

/** `not(condition)` */
export function not(cond) {
  return marked({ t: 'not', item: toCond(cond) });
}

/**
 * Escape hatch: everything the builder cannot express (function calls).
 * `?` placeholders are bound to parameters in order.
 *
 *   .where(raw('cosine(embed, ?) > ?', vec, 0.5))
 *
 * Every `?` in the fragment counts as a placeholder: if you need a literal
 * one, bind it as a parameter too (`raw('title ~ ?', "?")`), do not quote it.
 */
export function raw(sql, ...params) {
  if (typeof sql !== 'string') throw new FenecError('raw() expects text');
  return marked({ t: 'raw', sql, params });
}

function toCond(x) {
  if (isSpec(x) && x[NODE]) return x;
  if (isSpec(x)) return objectCond(x);
  throw new FenecError(`expected an object as a condition: ${JSON.stringify(x)}`);
}

/** `{ year: {gte: 2024}, tags: {has: 'rust'} }` -> an `and` tree */
function objectCond(obj) {
  const items = Object.entries(obj).map(([field, spec]) =>
    fieldCond(path(field), spec),
  );
  if (items.length === 0) return { t: 'and', items: [] };
  return items.length === 1 ? items[0] : { t: 'and', items };
}

/** One field's condition: a raw value is equality, an object an operator map. */
function fieldCond(field, spec) {
  if (spec === null) return { t: 'null', field, negated: false };
  if (!isSpec(spec)) return cmp(field, '=', spec);

  const items = [];
  for (const [k, v] of Object.entries(spec)) {
    if (k === 'not') {
      items.push(v === null
        ? { t: 'null', field, negated: true }
        : { t: 'not', item: fieldCond(field, v) });
      continue;
    }
    const op = OPS[k];
    if (!op) {
      throw new FenecError(`unknown operator \`${k}\` (field: ${field})`);
    }
    items.push(op === 'in' ? inCond(field, v) : cmp(field, op, v));
  }
  if (items.length === 0) {
    throw new FenecError(`empty condition object (field: ${field})`);
  }
  return items.length === 1 ? items[0] : { t: 'and', items };
}

function inCond(field, values) {
  // `{ customer: { in: from('customers').select('id').where(...) } }`: the
  // inner query runs once, before the outer one, and its one column is the
  // list -- `customer in (get customers select id where ...)`.
  if (values instanceof Query) return { t: 'sub', field, query: values };
  if (!Array.isArray(values)) {
    throw new FenecError(`\`in\` expects an array (field: ${field})`);
  }
  if (values.length === 0) {
    throw new FenecError(`\`in\` does not accept an empty array (field: ${field})`);
  }
  return { t: 'in', field, values };
}

// `= null` is always false in FenecQL; the intent is `is null`. Rather than
// silently returning an empty result, we generate the right expression.
function cmp(field, op, value) {
  if (value === null) {
    if (op === '=') return { t: 'null', field, negated: false };
    if (op === '!=') return { t: 'null', field, negated: true };
    throw new FenecError(`\`${op}\` cannot be used with null (field: ${field})`);
  }
  return { t: 'cmp', field, op, value };
}

/**
 * Flattens empty and single-child junctions. The parenthesis decision looks
 * at the child count, so pruning has to happen before rendering: `bind` has
 * side effects, and rendering a node twice only to drop one copy would
 * corrupt the parameter list.
 */
function prune(c) {
  if (c.t === 'and' || c.t === 'or') {
    const items = c.items.map(prune).filter(Boolean);
    if (items.length === 0) return null;
    return items.length === 1 ? items[0] : { t: c.t, items };
  }
  if (c.t === 'not') {
    const item = prune(c.item);
    return item ? { t: 'not', item } : null;
  }
  return c;
}

/** Renders the condition tree as FenecQL text; values go through `bind`. */
function render(c, bind, parent = null) {
  switch (c.t) {
    case 'and':
    case 'or': {
      const s = c.items.map((x) => render(x, bind, c.t)).join(` ${c.t} `);
      // `and` binds tighter than `or`: nesting different kinds needs parens.
      return parent && parent !== c.t ? `(${s})` : s;
    }
    case 'not':
      return `not (${render(c.item, bind, null)})`;
    case 'null':
      return `${c.field} is ${c.negated ? 'not ' : ''}null`;
    case 'in':
      return `${c.field} in [${c.values.map((v) => bind(v, c.field)).join(', ')}]`;
    case 'sub':
      // Its parameters numbered where its text stands, after the outer
      // query's before it.
      return `${c.field} in (${c.query[INNER](bind)})`;
    case 'cmp':
      return `${c.field} ${c.op} ${bind(c.value, c.field)}`;
    case 'raw': {
      let i = 0;
      const out = c.sql.replace(/\?/g, () => {
        if (i >= c.params.length) {
          throw new FenecError('raw(): more `?` placeholders than parameters');
        }
        return bind(c.params[i++], 'raw');
      });
      if (i !== c.params.length) {
        throw new FenecError('raw(): too many parameters given');
      }
      return out;
    }
    default:
      throw new FenecError(`unknown condition node \`${c.t}\``);
  }
}

// ------------------------------------------------------------------- query

// What a `Query` renders its text through as the inner query of an `in`.
const INNER = Symbol('inner');

/**
 * Immutable query builder: every call returns a new `Query`, so a query
 * body can be shared and branched from safely.
 */
export class Query {
  #s;

  constructor(state) {
    this.#s = { cond: [], order: [], offset: 0, lookups: [], ...state };
  }

  // Cloned through `this.constructor`: subclasses such as `FenecSync.from()`
  // keep their own behaviour along the chain (`.where(...).update(...)`
  // still goes through the optimistic write path).
  #with(patch) {
    return new this.constructor({ ...this.#s, ...patch });
  }

  /** The query's collection. */
  get collection() {
    return this.#s.collection;
  }

  /**
   * Every collection the query reads -- its own, each `lookup`'s and each
   * inner query's of an `in` -- or `null` when a `raw` fragment may read
   * one more (`in (get ...)` written by hand). A live query runs again
   * when one of them is written.
   */
  get reads() {
    const out = new Set();
    const read = (collection, cond) => {
      out.add(collection);
      return cond.every(function known(c) {
        switch (c.t) {
          case 'and':
          case 'or':
            return c.items.every(known);
          case 'not':
            return known(c.item);
          case 'sub': {
            const inner = c.query.reads;
            inner?.forEach((n) => out.add(n));
            return inner !== null;
          }
          case 'raw':
            return !/\bget\b/i.test(c.sql);
          default:
            return true;
        }
      });
    };
    const { collection, cond, lookups } = this.#s;
    const known = read(collection, cond) && lookups.every((l) => read(l.collection, l.cond));
    return known ? [...out] : null;
  }

  /**
   * The opaque context passed in with `bind`. The builder never interprets
   * it, it only carries it along the chain; subclasses keep their state here.
   */
  get context() {
    return this.#s.context;
  }

  /**
   * The same body as a **plain** `Query`, for bypassing a subclass's write
   * behaviour: the optimistic layer has to run the same filter locally and
   * then on the server, and must not call back into itself while doing so.
   */
  plain() {
    return new Query({ ...this.#s });
  }

  /** Binds the query to an executor: a `Fenec` instance or a `(sql, params)` fn. */
  bind(exec) {
    const fn = typeof exec === 'function' ? exec : (s, p) => exec.run(s, p);
    return this.#with({ exec: fn });
  }

  /**
   * `select a, b` -- no arguments, or `'*'`, means every field. Aggregates
   * go in the same list, spelled as FenecQL spells them, and answer under
   * that name:
   *
   *   db.from('orders').select('status', 'count(*)', 'sum(total)').group('status')
   */
  select(...cols) {
    const flat = cols.flat();
    if (flat.length === 0 || flat.includes('*')) {
      return this.#with({ project: null, aggregate: false });
    }
    const list = flat.map((c) => column(c));
    return this.#with({
      project: list.map((c) => c.text),
      aggregate: list.some((c) => c.aggregate),
    });
  }

  /** `group field` -- one row per value, for a select list that aggregates. */
  group(field) {
    return this.#with({ group: ident(field) });
  }

  /**
   * `where(field, op, value)` | `where(field, value)` | `where(object)`
   * Successive calls are joined with `and`.
   */
  where(...args) {
    return this.#with({ cond: [...this.#s.cond, condOf(args)] });
  }

  /** Joins everything conditioned so far to a new condition with `or`. */
  orWhere(...args) {
    const right = condOf(args);
    const left = this.#s.cond;
    if (left.length === 0) return this.#with({ cond: [right] });
    return this.#with({
      cond: [{ t: 'or', items: [{ t: 'and', items: left }, right] }],
    });
  }

  /** `near field $n [ef N] [exact]` */
  near(field, vector, opts = {}) {
    return this.#with({
      near: {
        field: ident(field),
        vector,
        ef: opts.ef === undefined ? null : whole(opts.ef, 'ef'),
        exact: !!opts.exact,
      },
    });
  }

  /** `match field $n` -- BM25 over a `@text` index. */
  match(field, query) {
    return this.#with({ match: { field: ident(field), query } });
  }

  /**
   * `fuse [k N] [candidates N]` -- with both `match` and `near`, ranks by
   * both: each side takes its own candidates and a document scores
   * `1 / (k + rank)` from each list it is on.
   *
   *   db.from('docs').match('body', text).near('embed', vector).fuse().limit(10)
   */
  fuse(opts = {}) {
    return this.#with({
      fuse: {
        k: opts.k === undefined ? null : whole(opts.k, 'k'),
        candidates:
          opts.candidates === undefined ? null : whole(opts.candidates, 'candidates'),
      },
    });
  }

  /**
   * `rerank field $n [candidates N]` -- reorders what `match` found by exact
   * vector distance. Needs a `match`; it does not need an `@hnsw` index,
   * because the vectors are read straight out of the store.
   */
  rerank(field, vector, opts = {}) {
    return this.#with({
      rerank: {
        field: ident(field),
        vector,
        candidates:
          opts.candidates === undefined
            ? null
            : whole(opts.candidates, 'candidates'),
      },
    });
  }

  /**
   * `lookup name on child [= parent] ...` -- the children of each row,
   * attached to it.
   *
   * Everything in `opts` binds to the looked-up collection, which is what
   * the clause's position means in FenecQL. `limit` in particular counts
   * children **per parent**, not rows in the page: asking twenty products
   * for three reviews each is one query, and no join expresses it.
   *
   *   db.from('products').limit(20)
   *     .lookup('reviews', { on: 'product_id', limit: 3,
   *                          order: [['created', 'desc']] })
   *
   * `on` names the child's field; the parent's key is `id` unless
   * `parentKey` says otherwise. `order` takes `[field, dir]` pairs, or a
   * bare field name for one ascending key.
   *
   * `required: true` drops a parent no child matches -- "products that have
   * a five-star review" rather than "products, with their five-star
   * reviews". It is tested before `limit`, so the page still comes back
   * full.
   *
   * Call it again to chain: the second call binds to the collection the
   * first one named, the way position scopes it in FenecQL.
   *
   *   db.from('shops')
   *     .lookup('orders', { on: 'shop_id', limit: 3 })
   *     .lookup('lines',  { on: 'order_id', limit: 5 })
   *
   * -- three shops' worth of orders, each with its own lines, in one query
   * instead of one round trip per order. `required` keeps meaning what it
   * means at its own level: on `lines` it drops the *orders* that have
   * none, and the shops stay unless `orders` says `required` as well.
   */
  lookup(name, opts = {}) {
    // A relation of `defineRelations` names the key; given `on`, the call does.
    const at = this.#s.lookups.at(-1)?.collection ?? this.#s.collection;
    const rel = !opts.on && this.#s.rel?.[at]?.[name];
    if (rel) [name, opts] = [rel.collection, { ...rel, ...opts }];
    name = nameOf(name);
    if (!opts.on) {
      throw new FenecError('lookup needs `on`: the child field holding the key');
    }
    const select = opts.select === undefined ? null : [opts.select].flat();
    const level = {
      collection: ident(name, 'collection'),
      on: ident(opts.on),
      parent: opts.parentKey === undefined ? null : ident(opts.parentKey),
      project:
        select === null || select.includes('*')
          ? null
          : select.map((c) => path(c)),
      cond: opts.where === undefined ? [] : [condOf([opts.where])],
      required: !!opts.required,
      order: orderKeys(opts.order),
      limit: opts.limit === undefined ? undefined : whole(opts.limit, 'limit'),
      offset: opts.offset === undefined ? 0 : whole(opts.offset, 'offset'),
    };
    return this.#with({ lookups: [...this.#s.lookups, level] });
  }

  /**
   * `order field asc|desc`. Successive calls add keys: when the first key
   * ties, the second decides. `{ collate: 'tr' }` puts text in Turkish order
   * rather than its bytes' -- `ç` after `c`, `ı` before `i`, `Çağla` before
   * `Zeynep` -- which is `collate tr`.
   */
  order(field, dir = 'asc', { collate } = {}) {
    // Over groups a key may be an aggregate of the list, by its name.
    return this.#with({
      order: [
        ...this.#s.order,
        { field: column(field).text, asc: direction(dir), collate: collation(collate) },
      ],
    });
  }

  limit(n) {
    return this.#with({ limit: whole(n, 'limit') });
  }

  offset(n) {
    return this.#with({ offset: whole(n, 'offset') });
  }

  /**
   * The generated FenecQL and its parameters: `[sql, params]`.
   * This is the builder's only output -- it can be inspected before running,
   * logged, or handed to another transport.
   */
  toFenecQL() {
    const params = [];
    return [this.#text(binder(params)), params];
  }

  /** The text as an inner query of `in`, its values bound by the outer `bind`. */
  [INNER](bind) {
    const { project, count, lookups } = this.#s;
    // One column is the list; the engine refuses the rest too.
    if (!project || project.length !== 1 || count || lookups.length) {
      throw new FenecError(
        `an inner query of \`in\` selects exactly one column: from('${this.#s.collection}').select('id')`,
      );
    }
    return this.#text(bind);
  }

  #text(bind) {
    const { collection, project, near, order, limit, offset, count } = this.#s;
    const { match, rerank, lookups, aggregate, group, fuse } = this.#s;
    // The engine refuses these too; failing here never sends a query.
    if (group && !aggregate) {
      throw new FenecError(`group ${group} needs an aggregate in select: 'count(*)'`);
    }
    if (aggregate) {
      const clash = near ? 'near' : match ? 'match' : lookups.length ? 'lookup' : count ? 'count' : null;
      if (clash) throw new FenecError(`aggregates cannot be combined with ${clash}`);
      if (!group && (order.length || limit !== undefined || offset)) {
        throw new FenecError('aggregates answer one row; group makes a row per value');
      }
    }
    // The engine refuses both of these too; failing here never sends a query.
    if (rerank && !match) {
      throw new FenecError('rerank needs match: it reorders what match found');
    }
    if (match && near && !fuse) {
      throw new FenecError(
        'match and near cannot be combined: both order the result; fuse() ranks by both',
      );
    }
    if (fuse && !(match && near)) {
      throw new FenecError('fuse combines match and near: the query needs both');
    }
    if (fuse && rerank) {
      throw new FenecError('fuse and rerank are two ways to use a vector with match: pick one');
    }
    // Refused in the engine too: a score spanning a parent and its children
    // has no meaning, and `count` collapses the rows they would hang from.
    if (lookups.length) {
      const clash = near ? 'near' : match ? 'match' : rerank ? 'rerank' : null;
      if (clash) throw new FenecError(`lookup cannot be combined with ${clash}`);
      // `count` collapses the rows children would hang from -- unless they
      // are only deciding who is counted.
      if (count && !lookups[0].required) {
        throw new FenecError(
          'count cannot be used with lookup unless it is required: there is ' +
            'nothing to attach children to',
        );
      }
      if (lookups.length > MAX_LOOKUP_DEPTH) {
        throw new FenecError(
          `lookup chained too deep: at most ${MAX_LOOKUP_DEPTH} levels`,
        );
      }
      // A collection may appear once in a query. Chained, that is a
      // whole-query rule and not a pairwise one: put the driving collection
      // back in scope two levels down and `on child = parent` reaches a
      // level that could be either of them.
      const seen = [collection];
      for (const l of lookups) {
        if (seen.includes(l.collection)) {
          throw new FenecError(
            `${l.collection} cannot look itself up: both sides would answer ` +
              'to the same name',
          );
        }
        seen.push(l.collection);
      }
    }
    if (count) this.#assertCountable();

    let sql = `get ${collection}`;
    if (project) sql += ` select ${project.join(', ')}`;
    const where = this.#where(bind);
    if (where) sql += ` where ${where}`;
    if (group) sql += ` group ${group}`;
    if (near) {
      sql += ` near ${near.field} ${bind(near.vector, near.field)}`;
      if (near.ef !== null) sql += ` ef ${near.ef}`;
      if (near.exact) sql += ' exact';
    }
    if (match) {
      sql += ` match ${match.field} ${bind(match.query, match.field)}`;
    }
    if (rerank) {
      sql += ` rerank ${rerank.field} ${bind(rerank.vector, rerank.field)}`;
      if (rerank.candidates !== null) sql += ` candidates ${rerank.candidates}`;
    }
    if (fuse) {
      sql += ' fuse';
      if (fuse.k !== null) sql += ` k ${fuse.k}`;
      if (fuse.candidates !== null) sql += ` candidates ${fuse.candidates}`;
    }
    for (const [i, o] of order.entries()) {
      sql += `${i === 0 ? ' order ' : ', '}${sortKey(o)}`;
    }
    if (limit !== undefined) sql += ` limit ${limit}`;
    if (offset) sql += ` offset ${offset}`;
    if (count) sql += ' count';
    // Terminal, so every clause after it belongs to the child -- and being
    // emitted last, its parameters land after the parent's, which is the
    // order `bind` numbered them in.
    for (const lookup of lookups) {
      sql += ` lookup ${lookup.collection} on ${lookup.on}`;
      if (lookup.parent) sql += ` = ${lookup.parent}`;
      // Early rather than trailing like `exact`: this one changes which rows
      // come back, so it should be read before the clauses that only shape
      // the children.
      if (lookup.required) sql += ' required';
      if (lookup.project) sql += ` select ${lookup.project.join(', ')}`;
      const root = prune({ t: 'and', items: lookup.cond });
      if (root) sql += ` where ${render(root, bind, null)}`;
      for (const [i, o] of lookup.order.entries()) {
        sql += `${i === 0 ? ' order ' : ', '}${sortKey(o)}`;
      }
      if (lookup.limit !== undefined) sql += ` limit ${lookup.limit}`;
      if (lookup.offset) sql += ` offset ${lookup.offset}`;
    }
    return sql;
  }

  /** The raw response (`{columns, rows}`). */
  async run() {
    const [sql, params] = this.toFenecQL();
    return this.#exec(sql, params);
  }

  /** Rows: an array of objects keyed by field name. */
  async rows() {
    return (await this.run()).rows ?? [];
  }

  /** The first row, or `null`. */
  async first() {
    const r = await this.limit(1).rows();
    return r.length ? r[0] : null;
  }

  /**
   * Number of matching rows (`get ... count`). Rows are not decoded, only
   * the filter runs.
   */
  async count() {
    const r = await this.#with({ count: true }).run();
    return r.rows?.[0]?.count ?? 0;
  }

  /**
   * The path the query took -- which index answered, how many rows each
   * stage read -- one line a step. The query runs to find out.
   */
  async explain() {
    const [sql, params] = this.toFenecQL();
    const r = await this.#exec(`explain ${sql}`, params);
    return (r.rows ?? []).map((row) => row.plan);
  }

  // The text of the write statements, **without running them**. The write
  // side counterpart of `toFenecQL()`: inspectable, loggable, handable to
  // another transport -- and synchronous. The optimistic layer depends on
  // that: there must be no `await` between applying locally and sending to
  // the server, or the "visible immediately" promise would be a lie, even
  // if only by a microtask.

  /** The `put` text. */
  toInsert(docs) {
    this.#assertPlain('insert');
    const list = Array.isArray(docs) ? docs : [docs];
    if (list.length === 0) throw new FenecError('cannot write an empty document list');
    const params = [];
    const bind = binder(params);
    const body = list.map((d) => renderDoc(d, bind)).join(', ');
    return [`put ${this.#s.collection} ${list.length === 1 ? body : `[${body}]`}`, params];
  }

  /** The `set` text. */
  toUpdate(patch, opts = {}) {
    this.#assertPlain('update');
    const params = [];
    const bind = binder(params);
    const body = renderDoc(patch, bind);
    const where = this.#requireFilter('update', opts, bind);
    return [`set ${this.#s.collection} ${body}${where}`, params];
  }

  /** The `del` text. */
  toDelete(opts = {}) {
    this.#assertPlain('delete');
    const params = [];
    const bind = binder(params);
    const where = this.#requireFilter('delete', opts, bind);
    return [`del ${this.#s.collection}${where}`, params];
  }

  /** `put` -- a single document or an array. Returns: documents written. */
  async insert(docs) {
    const list = Array.isArray(docs) ? docs : [docs];
    if (list.length === 0) return 0;
    return (await this.#exec(...this.toInsert(list))).count ?? 0;
  }

  /** `set` -- updates the rows matching the filter. Returns: rows affected. */
  async update(patch, opts = {}) {
    return (await this.#exec(...this.toUpdate(patch, opts))).count ?? 0;
  }

  /** `del` -- deletes the rows matching the filter. Returns: rows deleted. */
  async delete(opts = {}) {
    return (await this.#exec(...this.toDelete(opts))).count ?? 0;
  }

  // `near`/`order`/`limit` only mean something on the read path; silently
  // ignoring them in a write statement would invite the "limit(1) deletes a
  // single row" misconception.
  #assertPlain(verb) {
    const extra = this.#extraClause();
    if (extra) throw new FenecError(`${verb} cannot be used with \`${extra}\``);
    // Not among the read clauses `count` refuses, since a required lookup
    // decides what a count counts; a write has no use for one, and left
    // out, `.lookup(...).delete()` deleted every parent it filtered.
    if (this.#s.lookups.length) throw new FenecError(`${verb} cannot be used with \`lookup\``);
    if (verb === 'insert' && this.#s.cond.length) {
      throw new FenecError('insert cannot be used with `where`');
    }
  }

  // `count` does not combine with projection, ordering or pagination: they
  // are meaningless over a count, and `near` truncates to its own ceiling.
  // The engine checks the same thing; failing here never sends the query.
  #assertCountable() {
    const extra = this.#extraClause();
    if (extra) throw new FenecError(`count cannot be used with \`${extra}\``);
  }

  #extraClause() {
    const { near, match, rerank, order, limit, offset, project } = this.#s;
    return near ? 'near'
      : match ? 'match'
      : rerank ? 'rerank'
      : order.length ? 'order'
      : limit !== undefined ? 'limit'
      : offset ? 'offset'
      : project ? 'select'
      : null;
  }

  // An unfiltered `update`/`delete` covers the whole collection. That is far
  // too easy to do by accident and impossible to undo: we want it spelled out.
  #requireFilter(verb, opts, bind) {
    const where = this.#where(bind);
    if (where) return ` where ${where}`;
    if (opts && opts.all === true) return '';
    throw new FenecError(
      `an unfiltered ${verb} covers the whole collection; if you mean it, ` +
        `${verb}({ all: true })`,
    );
  }

  /** Renders the condition list as one `where` body; `null` when empty. */
  #where(bind) {
    const root = prune({ t: 'and', items: this.#s.cond });
    return root ? render(root, bind, null) : null;
  }

  #exec(sql, params) {
    if (!this.#s.exec) {
      throw new FenecError(
        'query is not bound to a connection: use db.from(...) or q.bind(db) ' +
          '(toFenecQL() if you only want the text)',
      );
    }
    return this.#s.exec(sql, params);
  }
}

/** Turns the `where` arguments into a single condition. */
function condOf(args) {
  if (args.length === 1) return toCond(args[0]);
  if (args.length === 2) return fieldCond(path(args[0]), args[1]);
  if (args.length === 3) {
    const op = OPS[args[1]];
    if (!op) throw new FenecError(`unknown operator \`${args[1]}\``);
    const field = path(args[0]);
    return op === 'in' ? inCond(field, args[2]) : cmp(field, op, args[2]);
  }
  throw new FenecError('where(field, op, value) | where(field, value) | where(object)');
}

function binder(params) {
  return (v, what) => {
    params.push(normalize(v, what));
    return `$${params.length}`;
  };
}

function renderDoc(doc, bind) {
  if (!isSpec(doc)) throw new FenecError('expected a document object');
  const pairs = Object.entries(doc)
    .filter(([, v]) => v !== undefined)
    .map(([k, v]) => `${path(k)}: ${bind(v, k)}`);
  if (pairs.length === 0) throw new FenecError('cannot write an empty document');
  return `{${pairs.join(', ')}}`;
}

/**
 * Unbound query builder. For handing the generated text to another
 * transport, or comparing it in a test: `from('docs').where(...).toFenecQL()`.
 */
export function from(name) {
  return new Query({ collection: ident(nameOf(name), 'collection') });
}

// ------------------------------------------------------------- persistence
// What is stored is what a file would hold: an image under the key, and the
// writes since, as chunks under `key#000000001`, `key#000000002`, ... A
// persist after the first stores only what was written since the last, so
// its cost follows the write rather than the database: before, every one
// wrote the whole image.

const DB_NAME = 'fenecdb';
const STORE = 'images';
/** Once the chunks outgrow this share of the image, a new image replaces them. */
const FOLD = 0.5;
/** Per database, where its chunks stand: `{key, next, image, chunks}`. */
const stored = new WeakMap();

const chunkKey = (key, n) => `${key}#${String(n).padStart(9, '0')}`;

// Sealed with a `CryptoKey` (AES-GCM), each record is `{gen, iv, data}` --
// the image -- or `{iv, data}`, a chunk, its tag over the key it is stored
// under, the image's generation and its place: a chunk moved, dropped from
// the middle, or kept from an image before is refused, as is one byte
// changed. The generation is random and new with each image, since the
// chunks' numbers start again under it.
const te = new TextEncoder();
const aad = (key, gen, n) => te.encode(`fenecdb\0${key}\0${gen}\0${n}`);

async function seal(ck, key, gen, n, bytes) {
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const data = new Uint8Array(await crypto.subtle.encrypt({ name: 'AES-GCM', iv, additionalData: aad(key, gen, n) }, ck, bytes));
  return n === 0 ? { gen, iv, data } : { iv, data };
}

async function unseal(ck, key, gen, n, rec) {
  try {
    return new Uint8Array(await crypto.subtle.decrypt({ name: 'AES-GCM', iv: rec.iv, additionalData: aad(key, gen, n) }, ck, rec.data));
  } catch {
    throw new FenecError(`${key}: record ${n} does not open: another key, or the stored bytes changed`);
  }
}

const isSealed = (rec) => rec != null && rec.data != null && rec.iv != null;
const chunkRange = (key) => IDBKeyRange.bound(`${key}#`, `${key}#\uffff`);

function idb() {
  return new Promise((res, rej) => {
    const req = indexedDB.open(DB_NAME, 1);
    req.onupgradeneeded = () => req.result.createObjectStore(STORE);
    req.onsuccess = () => res(req.result);
    req.onerror = () => rej(req.error);
  });
}

/**
 * Writes the database into IndexedDB under `key`: the image the first time,
 * then only the writes since the last call, as a chunk. Returns the bytes
 * written. With `{cryptoKey}` (AES-GCM) each is sealed before it is stored.
 */
export async function persist(fenec, key = 'default', opts = {}) {
  if (kept.has(fenec)) throw new FenecError('the database is kept in a file (openFile): persist would take the writes it appends');
  const ck = opts.cryptoKey ?? null;
  const db = await idb();
  let s = stored.get(fenec);
  let image = null;
  let chunk = null;
  if (s?.key !== key || s.ck !== ck) {
    fenec.journal();
    image = fenec.snapshot();
    s = { key, ck, gen: null, next: 1, image: 0, chunks: 0 };
    stored.set(fenec, s);
  } else {
    const { replace, bytes } = fenec.drain();
    if (replace) image = bytes;
    else if (bytes.length === 0) return 0;
    else if (s.chunks + bytes.length > s.image * FOLD) image = fenec.snapshot();
    else chunk = bytes;
  }
  const gen = image && ck ? [...crypto.getRandomValues(new Uint8Array(8))].map((b) => b.toString(16).padStart(2, '0')).join('') : s.gen;
  try {
    const rec = !ck ? (image ?? chunk) : image ? await seal(ck, key, gen, 0, image) : await seal(ck, key, gen, s.next, chunk);
    await new Promise((res, rej) => {
      const tx = db.transaction(STORE, 'readwrite');
      const os = tx.objectStore(STORE);
      if (image) {
        os.put(rec, key);
        os.delete(chunkRange(key));
      } else {
        os.put(rec, chunkKey(key, s.next));
      }
      tx.oncomplete = res;
      tx.onerror = () => rej(tx.error);
      tx.onabort = () => rej(tx.error);
    });
  } catch (e) {
    // The drained writes are nowhere now; the next call stores an image.
    stored.delete(fenec);
    throw e;
  }
  if (image) Object.assign(s, { gen, next: 1, image: image.length, chunks: 0 });
  else Object.assign(s, { next: s.next + 1, chunks: s.chunks + chunk.length });
  return (image ?? chunk).length;
}

/** Free-form state (cursors): next to the image, under a separate key. */
export async function putState(key, value) {
  const db = await idb();
  await new Promise((res, rej) => {
    const tx = db.transaction(STORE, 'readwrite');
    tx.objectStore(STORE).put(value, key);
    tx.oncomplete = res;
    tx.onerror = () => rej(tx.error);
  });
}

/** Reads back what `putState` wrote; `undefined` when absent. */
export async function getState(key) {
  const db = await idb();
  return new Promise((res, rej) => {
    const tx = db.transaction(STORE, 'readonly');
    const req = tx.objectStore(STORE).get(key);
    req.onsuccess = () => res(req.result);
    req.onerror = () => rej(req.error);
  });
}

/**
 * Restores from IndexedDB, the image and its chunks read as one file; later
 * `persist` calls go on adding chunks. Returns false when there is no record.
 * A sealed record needs the `{cryptoKey}` it was sealed with, and a key
 * refuses a record stored in the clear.
 */
export async function restore(fenec, key = 'default', opts = {}) {
  const ck = opts.cryptoKey ?? null;
  const db = await idb();
  let [image, chunks] = await new Promise((res, rej) => {
    const tx = db.transaction(STORE, 'readonly');
    const os = tx.objectStore(STORE);
    const img = os.get(key);
    const ch = os.getAll(chunkRange(key));
    tx.oncomplete = () => res([img.result, ch.result]);
    tx.onerror = () => rej(tx.error);
  });
  if (!image) return false;
  if (isSealed(image) !== !!ck) {
    throw new FenecError(ck ? `${key} is stored in the clear, not sealed with a key` : `${key} is sealed: restore needs its cryptoKey`);
  }
  const gen = ck ? image.gen : null;
  if (ck) {
    image = await unseal(ck, key, gen, 0, image);
    // Opened side by side, refused in order: the first record that does not
    // open is the one named, whichever settled first.
    const opened = await Promise.allSettled(chunks.map((c, i) => unseal(ck, key, gen, i + 1, c)));
    const bad = opened.find((r) => r.status === 'rejected');
    if (bad) throw bad.reason;
    chunks = opened.map((r) => r.value);
  }
  let bytes = image;
  if (chunks.length) {
    bytes = new Uint8Array(chunks.reduce((n, c) => n + c.length, image.length));
    bytes.set(image, 0);
    let at = image.length;
    for (const c of chunks) {
      bytes.set(c, at);
      at += c.length;
    }
  }
  if (replaceable(fenec)) emptied(fenec);
  await fenec.loadAsync(bytes);
  fenec.journal();
  stored.set(fenec, { key, ck, gen, next: chunks.length + 1, image: image.length, chunks: bytes.length - image.length });
  // The database the code declared its schema for is this one now: checked
  // again, what it adds journaled with the rest.
  await checked(fenec, declared.get(fenec), 'apply');
  return true;
}

// ------------------------------------------------------------- file (OPFS)
// A database kept in a file of the origin private file system holds what
// fenec-server holds on disk, byte for byte: an image, then every write since,
// appended as it is made. So a page's file opens with `fenec`, and a
// server's file loads in a page. `run` hands each statement's writes to the
// file and flushes it before it answers, where `persist` stores what was
// written when it is called.
//
// A new image -- a `compact`, or appended writes outgrowing half the image,
// folded as `persist` folds its chunks -- is written into a copy beside the
// file (`<name>~`) and flushed before the file is touched, then over the
// file, and the copy emptied. A crash at any point leaves one of the two
// whole: `openFile` takes the copy when its image is whole -- it holds every
// write the file does and more -- and the file otherwise.

/** Per database, the file it is kept in. */
const kept = new WeakMap();
/**
 * Nor before the writes appended since the image reach this: without it
 * every write to a small database folded, and a new image costs the file
 * three flushes where an append costs one -- 0.92 ms against 0.31 in
 * Chrome 153 (`make file-bench`).
 */
const FOLD_FLOOR = 64 * 1024;
const MAGIC = enc.encode('FENECDB\x01');
/** An image's head: the signature, then `[6][u64 counter][u64 body length]`. */
const HEAD = MAGIC.length + 17;

/** Where the image beginning with `head` ends, or -1 when it is not an image's head. */
function imageEnd(head) {
  if (head.length < HEAD || head[MAGIC.length] !== 6 || MAGIC.some((b, i) => head[i] !== b)) return -1;
  return HEAD + Number(new DataView(head.buffer, head.byteOffset).getBigUint64(MAGIC.length + 9, true));
}

function writeAt(h, bytes, at) {
  for (let done = 0; done < bytes.length; ) {
    const n = h.write(bytes.subarray(done), { at: at + done });
    if (!(n > 0)) throw new FenecError('the file took none of a write');
    done += n;
  }
}

function readAt(h, size) {
  const out = new Uint8Array(size);
  for (let done = 0; done < size; ) {
    const n = h.read(out.subarray(done), { at: done });
    if (!(n > 0)) throw new FenecError('the file ended before its size');
    done += n;
  }
  return out;
}

/** `h` holding `bytes` and nothing else, flushed. */
function overwrite(h, bytes) {
  h.truncate(0);
  writeAt(h, bytes, 0);
  h.flush();
}

function refused(name, e) {
  return new FenecError(`${name} refused a write (${e?.message ?? e}); it holds what was written before, and the database has to be opened from it again`, { cause: e });
}

/** A database kept in a file of the origin private file system (`openFile`). */
class FenecFile {
  #fenec; #main; #aside; #name; #size; #image;
  #failed = null;

  constructor(fenec, name, main, aside, size, image) {
    this.#fenec = fenec;
    this.#name = name;
    this.#main = main;
    this.#aside = aside;
    this.#size = size;
    this.#image = image;
  }

  /** The file's name in its directory. */
  get name() {
    return this.#name;
  }

  /** Bytes in the file. */
  get size() {
    return this.#size;
  }

  /**
   * Writes what the database wrote since the last call into the file and
   * flushes it; `run` calls it after every statement. Returns the bytes
   * written. Once the file has refused a write every later one is refused:
   * it holds what was written before, and the drained writes are nowhere.
   */
  flush() {
    const { replace, bytes } = this.#fenec.drain();
    if (!replace && bytes.length === 0) return 0;
    if (this.#failed) throw refused(this.#name, this.#failed);
    try {
      if (replace) return this.#rewrite(bytes);
      // Updates to the same rows would grow the file, and the replay of
      // the next open, without end.
      const appended = this.#size - this.#image + bytes.length;
      if (appended > Math.max(this.#image * FOLD, FOLD_FLOOR)) return this.#rewrite(this.#fenec.snapshot());
      writeAt(this.#main, bytes, this.#size);
      this.#main.flush();
      this.#size += bytes.length;
      return bytes.length;
    } catch (e) {
      this.#failed = e;
      throw refused(this.#name, e);
    }
  }

  /** The file's bytes, which `fenec` and a server open: to download them. */
  bytes() {
    this.flush();
    return readAt(this.#main, this.#size);
  }

  /** Flushes and lets the file go: the database is kept in it no longer. */
  close() {
    try {
      this.flush();
    } finally {
      kept.delete(this.#fenec);
      this.#fenec.journal(false);
      this.#main.close();
      this.#aside.close();
    }
  }

  /** The file holding `image` (and the writes after it) instead, beside it first. */
  #rewrite(image) {
    overwrite(this.#aside, image);
    overwrite(this.#main, image);
    this.#aside.truncate(0);
    this.#aside.flush();
    this.#size = image.length;
    this.#image = Math.max(imageEnd(image), 0);
    return image.length;
  }
}

/** Synchronous access to `name` in `dir`, which is created if absent. */
async function syncAccess(dir, name) {
  const fh = await dir.getFileHandle(name, { create: true });
  if (typeof fh.createSyncAccessHandle !== 'function') {
    throw new FenecError('openFile needs a dedicated worker: the file system hands out synchronous access nowhere else');
  }
  try {
    return await fh.createSyncAccessHandle();
  } catch (e) {
    if (e?.name !== 'NoModificationAllowedError') throw e;
    throw new FenecError(`${name} is open elsewhere (another worker or tab): one database at a time writes a file`, { cause: e });
  }
}

/**
 * Whether `bytes` load, tried in a database of their own. Only the module's
 * word that they are no database says no: anything else -- memory running
 * out, say -- is thrown, since the copy may be the one whole database left.
 */
function loads(fenec, bytes) {
  const trial = sibling(fenec);
  try {
    trial.load(bytes);
    return true;
  } catch (e) {
    // Refused for collation data: they loaded, and the text needs it.
    if (e.collation) return true;
    if (e instanceof FenecError) return false;
    throw e;
  } finally {
    trial.close();
  }
}

/**
 * Keeps the database in a file of the origin private file system, the bytes
 * fenec-server keeps on disk. A file holding some is loaded into the database,
 * which must hold nothing yet; an empty one takes the database's image. From
 * then on `run` appends every statement's writes to it and flushes it before
 * it answers.
 *
 * Only in a dedicated worker, the one place the file system hands out the
 * synchronous access this needs; and one worker at a time, since a second
 * opener is refused -- as a second process over a server's file would
 * corrupt it.
 *
 * @param {Fenec} fenec
 * @param {string} name  the file's name in `opts.dir`
 * @param {{dir?: FileSystemDirectoryHandle}} opts  the directory, the root
 *   of the origin private file system unless given
 * @returns {Promise<FenecFile>}
 */
export async function openFile(fenec, name = 'default.fenec', opts = {}) {
  if (kept.has(fenec)) throw new FenecError('the database is already kept in a file');
  if (stored.has(fenec)) throw new FenecError('persist keeps this database in IndexedDB: a file would take the writes it stores');
  const dir = opts.dir ?? (await globalThis.navigator?.storage?.getDirectory?.());
  if (!dir) throw new FenecError('openFile needs the origin private file system (navigator.storage.getDirectory)');
  const main = await syncAccess(dir, name);
  let aside = null;
  try {
    aside = await syncAccess(dir, `${name}~`);
    const beside = aside.getSize();
    if (beside > 0) {
      // A rewrite stopped: its copy was flushed before the file was
      // touched when its image is whole, and the file was never touched
      // when it is not.
      const end = imageEnd(readAt(aside, Math.min(beside, HEAD)));
      const copy = end > 0 && end <= beside ? readAt(aside, beside) : null;
      if (copy && loads(fenec, copy)) overwrite(main, copy);
      aside.truncate(0);
      aside.flush();
    }
    let size = main.getSize();
    let image;
    if (size === 0) {
      fenec.journal();
      const snap = fenec.snapshot();
      overwrite(main, snap);
      size = snap.length;
      image = imageEnd(snap);
    } else {
      // What its own schema made, and nothing since, is nothing to lose.
      if (replaceable(fenec)) emptied(fenec);
      if (fenec.schemas().length) throw new FenecError(`${name} holds a database: open it into one that holds nothing`);
      const bytes = readAt(main, size);
      const took = await fenec.loadAsync(bytes);
      // A last record a crash cut short: appended after, the next write
      // would be read back as the rest of it.
      if (took < size) {
        main.truncate(took);
        main.flush();
        size = took;
      }
      image = imageEnd(bytes);
      fenec.journal();
    }
    const file = new FenecFile(fenec, name, main, aside, size, Math.max(image, 0));
    kept.set(fenec, file);
    // Refused, the file is let go of, as it was found.
    await checked(fenec, opts.schema ? opts : declared.get(fenec), 'apply').catch((e) => {
      file.close();
      throw e;
    });
    return file;
  } catch (e) {
    main.close();
    aside?.close();
    throw e;
  }
}

// -------------------------------------------------------------------- sync
//
// Local replica + server: reads come from the local side (no network),
// writes go to the server, the feedback arrives over the subscription.
// TanStack DB's model, with two differences:
//
// 1. **No incremental dataflow.** There a collection is a `Map`, and
//    re-running a filter over 50k rows on every keystroke is unacceptable,
//    which is why differential dataflow is needed. Here the local side is
//    an indexed database -- running the query from scratch is already
//    sub-millisecond. And incremental maintenance would not even be
//    *correct* for `near`: a single insert can reorder the whole top-k.
//
// 2. **Shapes are mandatory.** The whole database lives in memory and the
//    WASM32 address space is 4 GB; a client cannot pull an entire
//    collection. A subscription is a *subset*, filtered server side.
//
// One collection per shape: a seed has to be able to say "this is the whole
// collection", otherwise the cleanup step becomes ambiguous.

/// Identity base for optimistic rows. Server ids run consecutively from 1;
/// 2^52 is both far away from them and below `Number.MAX_SAFE_INTEGER`, so
/// it survives JSON intact. Ids in this range land in a sparse map in the
/// store -- the right trade for a handful of pending rows.
const TEMP_BASE = 2 ** 52;

/** Operator spellings in a REST filter. */
const REST_OPS = {
  '=': 'eq', eq: 'eq',
  '!=': 'neq', ne: 'neq', neq: 'neq',
  '<': 'lt', lt: 'lt',
  '<=': 'lte', lte: 'lte', le: 'lte',
  '>': 'gt', gt: 'gt',
  '>=': 'gte', gte: 'gte', ge: 'gte',
  '~': 'like', like: 'like', contains: 'like',
  has: 'has',
  in: 'in',
};

/**
 * Turns a shape condition into REST query-string pairs.
 *
 * Why not the builder's condition tree: the builder emits `$1` parameters,
 * and a query string has no parameters. A free-form `?where=` would fall
 * back to string concatenation -- exactly what the builder avoids.
 * The `?field=op.value` form leaves escaping to the transport
 * (`URLSearchParams`) and type resolution to the server: no injection surface.
 *
 * The price: a shape has no `or` groups and no function calls. A shape is a
 * subset definition; once it gets complicated, a separate collection (or a
 * view) on the server is the right answer.
 */
function shapeParams(where) {
  const out = [];
  if (where == null) return out;
  if (!isSpec(where)) throw new FenecError('a shape condition must be an object');
  for (const [field, spec] of Object.entries(where)) {
    ident(field);
    if (spec === null) {
      out.push([field, 'is.null']);
      continue;
    }
    if (!isSpec(spec)) {
      out.push([field, `eq.${restValue(spec, field)}`]);
      continue;
    }
    for (const [k, v] of Object.entries(spec)) {
      if (k === 'not') {
        out.push([field, v === null ? 'not.is.null' : `not.${restOp(k2(v), field)}`]);
        continue;
      }
      out.push([field, restOp([k, v], field)]);
    }
  }
  return out;

  // `{not: {gte: 3}}` -> `['gte', 3]`
  function k2(v) {
    if (!isSpec(v)) return ['eq', v];
    const e = Object.entries(v);
    if (e.length !== 1) throw new FenecError('`not` takes a single condition');
    return e[0];
  }
  function restOp([k, v], field) {
    const op = REST_OPS[k];
    if (!op) throw new FenecError(`unknown operator \`${k}\` in shape (field: ${field})`);
    if (op === 'in') {
      // A subscription is told of its own collection's writes, and an
      // inner query's set would not follow those of the other.
      if (v instanceof Query) {
        throw new FenecError(`a shape's \`in\` takes a list, not a query (field: ${field})`);
      }
      if (!Array.isArray(v) || v.length === 0) {
        throw new FenecError(`\`in\` expects a non-empty array (field: ${field})`);
      }
      return `in.(${v.map((x) => restValue(x, field)).join(',')})`;
    }
    return `${op}.${restValue(v, field)}`;
  }
}

function restValue(v, field) {
  if (v === null || v === undefined) {
    throw new FenecError(`a shape value cannot be empty (field: ${field})`);
  }
  if (v instanceof Date) return v.toISOString();
  if (typeof v === 'object') {
    throw new FenecError(`a shape value must be a scalar (field: ${field})`);
  }
  return String(v);
}

/**
 * Turns the SSE stream into events.
 *
 * `fetch` rather than `EventSource` because of one header: `EventSource`
 * cannot carry `Authorization`, so the token would have to travel in the
 * query string -- which means into the logs and into `Referer`.
 */
async function* sseEvents(res) {
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

/**
 * `create collection` text from a schema: field name, type, collation and
 * index as is. The collation too: without it the replica's field compared
 * its text by the bytes, and paged and ordered differently from the server.
 * `@unique` is a plain hash here, as on a replica: a batch lands rows'
 * last states one at a time, and a value moved between two collided.
 */
function schemaDDL(schema) {
  const fields = schema.fields.map(
    (f) =>
      `${ident(f.name)} ${f.type}` +
      (f.collate ? ` collate ${collation(f.collate)}` : '') +
      (f.required ? ' required' : '') +
      (f.index ? ` @${f.index === 'unique' ? 'hash' : f.index}` : ''),
  );
  return `create collection if not exists ${ident(schema.name, 'collection')} (${fields.join(', ')})`;
}

function newKey() {
  if (globalThis.crypto?.randomUUID) return globalThis.crypto.randomUUID();
  return `k${Date.now().toString(36)}${Math.random().toString(36).slice(2, 10)}`;
}

/**
 * Fallback that runs the tick after at most this long when no frame
 * arrives. In a visible tab the frame lands in ~16 ms and cancels it; in a
 * hidden tab this bound is the ceiling on the delay.
 */
const TICK_FALLBACK_MS = 50;

/**
 * The sync's own collections, beside the replica's and as the native core
 * (`fenec_abi::sync`) keeps them: each shape's cursor, the writes the server
 * has not answered with what puts each back, and the rows an insert made
 * under a temporary id. In the replica, they go wherever it goes --
 * `persist`, an `openFile` -- so a page opened again sends what it had not.
 */
const SYNC_STATE = [
  'create collection if not exists _sync_shapes (collection text, shape text, cursor int, seeded bool)',
  'create collection if not exists _sync_queue (path text, body text, key text, undo text, label text)',
  'create collection if not exists _sync_temps (collection text, key text, op int, until int)',
];
const PUT_SHAPE = 'put _sync_shapes {id: $1, collection: $2, shape: $3, cursor: $4, seeded: $5}';
const PUT_OP = 'put _sync_queue {id: $1, path: $2, body: $3, key: $4, undo: $5, label: $6}';
const PUT_TEMP = 'put _sync_temps {id: $1, collection: $2, key: $3, op: $4, until: $5}';
/** An answer with no `Fenec-Seq`: no stream is known to be past it. */
const NO_SEQ = Number.MAX_SAFE_INTEGER;

/** A key's text, as the native core writes it: a text as it is, else its JSON. */
const keyText = (k) => (typeof k === 'string' ? k : JSON.stringify(k));

/**
 * Query builder that goes through the optimistic write path.
 *
 * Because `Query` clones through `this.constructor`, every step of the
 * chain stays this class: `db.from('x').where(...).delete()` still goes
 * local first, server second.
 */
class SyncQuery extends Query {
  insert(docs) {
    return this.context.write('insert', this, docs, null);
  }
  update(patch, opts = {}) {
    return this.context.write('update', this, patch, opts);
  }
  delete(opts = {}) {
    return this.context.write('delete', this, null, opts);
  }
}

/**
 * Local replica + server connection.
 *
 * ```js
 * const db = await sync({
 *   url: 'http://127.0.0.1:8080',
 *   shapes: [{ collection: 'tasks', where: { status: 'open' }, key: 'key' }],
 * });
 * await db.ready();
 *
 * const rows = await db.from('tasks').where('priority', '>=', 3).rows(); // local
 * const stop = db.live(db.from('tasks'), (rows) => render(rows));        // live
 * await db.from('tasks').insert({ title: 'new', status: 'open' });       // optimistic
 * ```
 *
 * It does what the native core does (`crates/fenec-abi/src/sync`), and the
 * two are held to one script, `integrations/sync-scenarios.json`: writes go
 * one at a time and in order, each under an `Idempotency-Key` it keeps until
 * the server answers; no answer, a 408, a 429 or a 5xx is tried again with
 * backoff, a 401 asks for a token, and any other 4xx is a refusal, put back.
 * Moving this layer onto that core instead was measured at +21 KB brotli.
 */
export class FenecSync {
  #local;
  #remote;
  #url;
  #token;
  #fetch;
  #shapes = new Map();
  #lives;
  #scheduled = null;
  #flushers = [];
  #pendingTick = null;
  #closed = false;
  #abort = new AbortController();
  #ready;
  #resolveReady;
  #persistKey;
  #cryptoKey;
  #persisting = null;
  #persistAgain = false;
  #chan = null;
  #leader = true;
  #leaderMode;
  #locks;
  #release = null;
  #onError;
  #onRefused;
  #tokenProvider;
  /** A `batch` being gathered: what each write applied. */
  #batch = null;
  /** Writes the server has not answered, in order: `{n, path, body, key, undo, label, done?, count}`. */
  #queue = [];
  /** Rows of inserts under temporary ids: `{collection, key, temp, op, until}`. */
  #temps = [];
  #nextTemp = TEMP_BASE;
  #nextN = 1;
  #sending = false;
  #sendAttempt = 0;
  #sendWaiting = false;
  #schemaBusy = false;
  #schemaAttempt = 0;
  #schemaWaiting = false;
  /** Writes left from before go before the streams open, so what they bring holds them. */
  #flushing = false;
  #offline = false;
  /** Waiting for a token after a 401. */
  #paused = false;
  #fault = null;
  #waits = new Set();
  #pushed = [];
  #listeners = null;

  constructor(local, opts) {
    this.#local = local;
    this.#url = String(opts.url).replace(/\/+$/, '');
    this.#token = opts.token ?? null;
    this.#fetch = opts.fetch ?? globalThis.fetch;
    if (typeof this.#fetch !== 'function') {
      throw new FenecError('fetch not found: pass one via opts.fetch');
    }
    this.#fetch = this.#fetch.bind(globalThis);
    this.#remote = new FenecHttp(this.#url, { token: this.#token, fetch: this.#fetch });
    this.#persistKey = opts.persist ?? null;
    this.#cryptoKey = opts.cryptoKey ?? null;
    this.#onError = opts.onError ?? null;
    this.#onRefused = opts.onRefused ?? null;
    this.#tokenProvider = opts.tokenProvider ?? null;
    this.#leaderMode = opts.leader ?? 'auto';
    // The lock manager can be supplied from outside: the default is the Web
    // Locks API, but making it injectable keeps this testable and leaves
    // room for another coordination mechanism.
    this.#locks = opts.locks ?? globalThis.navigator?.locks ?? null;
    this.#lives = new Lives(local);

    for (const raw of opts.shapes ?? []) {
      const shape = normalizeShape(raw, this.#url);
      if (this.#shapes.has(shape.collection)) {
        throw new FenecError(
          `two shapes for \`${shape.collection}\`: one collection per shape ` +
            '(a seed has to say "this is the whole collection")',
        );
      }
      this.#shapes.set(shape.collection, shape);
    }
    if (this.#shapes.size === 0) throw new FenecError('at least one shape is required');

    this.#ready = new Promise((res) => {
      this.#resolveReady = res;
    });
  }

  /** The local database. For raw FenecQL: `db.local.run(...)`. */
  get local() {
    return this.#local;
  }

  /** The remote endpoint (`FenecHttp`). For queries outside the shapes. */
  get remote() {
    return this.#remote;
  }

  /** Resolves once the first seed of every shape has landed. */
  ready() {
    return this.#ready;
  }

  /** Resolves once the server has answered every write made so far. */
  pushed() {
    return this.#queue.length ? new Promise((r) => this.#pushed.push(r)) : Promise.resolve();
  }

  /** Shape state: `{collection, cursor, seeded, connected, pending, error, leader}`. */
  status() {
    return [...this.#shapes.values()].map((s) => ({
      collection: s.collection,
      cursor: s.cursor,
      seeded: s.seeded,
      connected: s.connected,
      pending: this.#queue.length,
      error: this.#fault?.message ?? (s.error ? String(s.error.message ?? s.error) : null),
      leader: this.#leader,
    }));
  }

  /** A new token: the requests from here on carry it, and what a 401 stopped goes on. */
  setToken(token) {
    this.#token = token || null;
    this.#remote = new FenecHttp(this.#url, { token: this.#token, fetch: this.#fetch });
    if (this.#paused) {
      this.#paused = false;
      this.#fault = null;
    }
    this.#kick();
  }

  /**
   * The network is gone (`false`): the streams end and writes wait. Back
   * (`true`): what waited goes at once, the backoff forgotten. A page has it
   * from `online` and `offline` on its own.
   */
  setOnline(on) {
    if (!on) {
      this.#offline = true;
      for (const s of this.#shapes.values()) this.#cancel(s);
      return;
    }
    this.#offline = false;
    this.#sendAttempt = this.#schemaAttempt = 0;
    for (const s of this.#shapes.values()) {
      s.attempt = 0;
      s.waiting = false;
    }
    this.#sendWaiting = this.#schemaWaiting = false;
    for (const w of this.#waits) clearTimeout(w.t);
    this.#waits.clear();
    this.#flushing = this.#queue.length > 0;
    this.#kick();
  }

  /**
   * Query builder. Reads are **local** (no network); a write goes local
   * first, then to the server.
   */
  from(name) {
    const collection = ident(nameOf(name), 'collection');
    if (!this.#shapes.has(collection)) {
      throw new FenecError(
        `no shape for \`${collection}\`: that collection is not in the local ` +
          'replica. To ask the server, use db.remote.from(...)',
      );
    }
    return new SyncQuery({
      collection,
      context: this,
      exec: (sql, params) => this.#local.query(sql, params),
      rel: declared.get(this)?.relations,
    });
  }

  /**
   * Live query: re-run after every local change.
   *
   * **No** incremental maintenance, deliberately: the local query is
   * indexed and already returns in under a millisecond. An incremental
   * diff could not give the right answer for `near` anyway -- a single
   * insert can reorder the whole top-k.
   *
   * `Fenec.live`'s contract, over the replica: `query` a builder `Query`,
   * a FenecQL text or `[text, params]`. Unlike a page's own database, a
   * replica runs its live queries at the next frame (`#touch`).
   *
   * Returns: the function that ends the subscription.
   */
  live(query, cb, opts = {}) {
    return this.#lives.add(query, cb, opts, this.#localExec, opts.onError ?? this.#onError);
  }

  /**
   * Several writes as **one**: applied together, sent as one `/batch` under
   * one key, and landed by the server as one block or refused -- and put
   * back -- whole. Resolves with the number of statements once the server
   * has them.
   */
  async batch(fn) {
    if (this.#batch) throw new FenecError('nested batches are not supported');
    const b = [];
    this.#batch = b;
    try {
      await fn(this);
    } catch (e) {
      this.#batch = null;
      this.#undo(b.map((a) => a.undo));
      throw e;
    } finally {
      this.#batch = null;
    }
    if (b.length === 0) return 0;
    await this.#enqueue(b);
    return b.length;
  }

  /** Finishes any pending live-query runs (for tests). */
  async flush() {
    // When a tick is scheduled, wait for its *run* first: once the timer
    // fires `#pendingTick` is set, and that is what finishes the live queries.
    while (this.#scheduled || this.#pendingTick) {
      if (this.#pendingTick) {
        await this.#pendingTick;
        continue;
      }
      await new Promise((r) => this.#flushers.push(r));
    }
  }

  /** Closes the subscriptions and gives up the lead. The local database stays open. */
  close() {
    this.#closed = true;
    this.#abort.abort();
    for (const s of this.#shapes.values()) this.#cancel(s);
    for (const w of this.#waits) clearTimeout(w.t);
    this.#waits.clear();
    this.#chan?.close();
    this.#release?.();
    this.#lives.clear();
    if (this.#listeners) {
      for (const [k, f] of this.#listeners) globalThis.removeEventListener?.(k, f);
    }
  }

  // ------------------------------------------------------------ writes

  /**
   * `SyncQuery`'s write hook: applied to the replica, what puts it back in
   * hand, and queued for the server with an idempotency key. Resolves with
   * the rows the write reached once the server has it, and rejects -- the
   * replica put back -- when the server refuses it; a network failure or a
   * server error is not a refusal, and the write waits and goes again.
   *
   * Everything up to the first `await` is **synchronous**, so the
   * optimistic row is in the local store before the caller even awaits the
   * promise. A single microtask in between would break the "visible
   * immediately" promise.
   */
  async write(verb, q, arg, opts) {
    let applied;
    const wait = this.#optimistic(() => {
      applied = this.#apply(verb, q, arg, opts);
    });
    if (wait) await wait;
    if (!applied) return 0;
    if (this.#batch) {
      this.#batch.push(applied);
      return applied.count;
    }
    return this.#enqueue([applied]);
  }

  /**
   * One statement applied to the replica: the lines the server is sent, the
   * undo -- `{c, del, put}`, the native core's -- and the temporary rows.
   *
   * - An insert into a shape with a `key` gets a key where it has none and
   *   a temporary id, and is matched with the server's copy by the key;
   *   one naming its id is applied under it. Without either it waits for
   *   the server: with nothing to match it by, the copy would leave two.
   * - An update or a delete works by the filter, the rows it reaches read
   *   first: putting them back is the undo. A row of an insert the server
   *   has not answered has a temporary id the server never saw, so it is
   *   reached there by its key too, in a second line after the first.
   */
  #apply(verb, q, arg, opts) {
    const c = q.collection;
    const shape = this.#shapes.get(c);
    const base = q.plain();
    const key = shape.key;
    if (verb === 'insert') {
      const list = Array.isArray(arg) ? arg : [arg];
      if (list.length === 0) return null;
      const docs = list.map((d) => {
        const doc = { ...d };
        if (key && doc[key] == null) {
          delete doc[key];
          doc[key] = newKey();
        }
        return doc;
      });
      const line = base.toInsert(docs);
      const named = docs.filter((d) => d.id != null).map((d) => d.id);
      if (!key && named.length < docs.length) {
        return { lines: [line], undo: { c, del: [], put: '[]' }, temps: [], count: docs.length };
      }
      const before = named.length ? this.#local.run(...from(c).where('id', 'in', named).toFenecQL()).rows ?? [] : [];
      const held = new Set(before.map((r) => r.id));
      const temps = [];
      const fresh = [];
      let next = this.#nextTemp;
      const local = docs.map((d) => {
        if (d.id != null) {
          if (!held.has(d.id)) fresh.push(d.id);
          return d;
        }
        const t = next++;
        fresh.push(t);
        if (key) temps.push([keyText(d[key]), t]);
        const { id: _, ...rest } = d;
        return { id: t, ...rest };
      });
      this.#local.run(...from(c).toInsert(local));
      this.#nextTemp = next;
      return { lines: [line], undo: { c, del: fresh, put: JSON.stringify(before) }, temps: temps.map(([k, t]) => [c, k, t]), count: docs.length };
    }
    const stmt = verb === 'update' ? base.toUpdate(arg, opts) : base.toDelete(opts);
    // `select()` drops the projection: writing back needs every field.
    const before = this.#local.run(...base.select().toFenecQL()).rows ?? [];
    const count = this.#local.run(...stmt).count ?? 0;
    const lines = [stmt];
    if (key && before.length) {
      const ids = new Set(before.map((r) => r.id));
      const keys = this.#temps.filter((t) => t.collection === c && ids.has(t.temp)).map((t) => t.key);
      if (keys.length) {
        const k = from(c).where(key, 'in', keys);
        lines.push(verb === 'update' ? k.toUpdate(arg) : k.toDelete());
      }
    }
    return { lines, undo: { c, del: [], put: JSON.stringify(before) }, temps: [], count };
  }

  /**
   * What `#apply` made, queued: kept in the replica's `_sync_queue` and
   * `_sync_temps`, then sent -- by this tab when it leads, or by the one
   * that does. Resolves with the rows it reached once the server has it.
   */
  #enqueue(applied) {
    const lines = applied.flatMap((a) => a.lines);
    const n = this.#nextN++;
    const op = {
      n,
      path: lines.length === 1 ? '/query' : '/batch',
      body: lines.map(ndjson).join('\n'),
      key: `fenec-${newKey()}`,
      undo: JSON.stringify(applied.map((a) => a.undo)),
      label: lines[0][0].slice(0, 500),
      count: applied.reduce((s, a) => s + a.count, 0),
      done: null,
    };
    const p = new Promise((resolve, reject) => {
      op.done = { resolve, reject };
    });
    this.#keep(op);
    for (const a of applied) {
      for (const [collection, key, temp] of a.temps) {
        const t = { collection, key, temp, op: n, until: 0 };
        this.#temps.push(t);
        this.#saveTemp(t);
      }
    }
    this.#queue.push(op);
    this.#touch();
    this.#persistSoon();
    if (this.#leader) this.#kick();
    else this.#chan?.postMessage({ t: 'op', op: wire(op) });
    return p;
  }

  #keep(op) {
    this.#local.run(PUT_OP, [op.n, op.path, op.body, op.key, op.undo, op.label]);
  }

  #saveTemp(t) {
    this.#local.run(PUT_TEMP, [t.temp, t.collection, t.key, t.op, t.until]);
  }

  /** Puts back what writes did, the last first: each step's ids deleted, its rows written back. */
  #undo(steps) {
    for (const s of steps.reverse()) {
      try {
        this.#deleteLocal(s.c, s.del);
        const rows = JSON.parse(s.put);
        if (rows.length) this.#local.run(...from(s.c).toInsert(rows));
      } catch (e) {
        this.#onError?.(e);
      }
    }
    this.#touch();
  }

  /**
   * Runs `fn`, the optimistic half of a write, at once, and again once the
   * collation data it was refused for is here -- refused before it changed
   * anything. Not `async`: when `fn` runs at once, as it does unless the
   * module lacks that data, nothing is awaited, and the change is visible
   * the moment `write` is called.
   */
  #optimistic(fn) {
    try {
      fn();
      return undefined;
    } catch (e) {
      if (!e.collation || e.ran) throw e;
      return this.#local.collation(...e.collation).then((fetched) => {
        if (!fetched) throw e;
        return this.#optimistic(fn);
      });
    }
  }

  get #localExec() {
    return (sql, params) => this.#local.query(sql, params);
  }

  #deleteLocal(collection, ids) {
    if (ids.length === 0) return 0;
    return this.#local.run(...from(collection).where('id', 'in', ids).toDelete()).count ?? 0;
  }

  // ----------------------------------------------------------- driving

  /** Asks for whatever is due: the schemas, the queue's next write, the streams. */
  #kick() {
    if (this.#closed || this.#offline) return;
    if ([...this.#shapes.values()].some((s) => s.rebuild || !s.made)) {
      this.#schemas().catch((e) => this.#onError?.(e));
      return;
    }
    if (!this.#leader) return;
    this.#pump().catch((e) => this.#onError?.(e));
    if (this.#paused || (this.#flushing && this.#queue.length)) return;
    for (const s of this.#shapes.values()) this.#connect(s);
  }

  /**
   * Waits out the backoff for `attempt` -- 250 ms doubling to 15 s, 30%
   * jitter on top, so a server back up is not met by every client at once
   * -- then runs `then`. The timer does not hold up a Node process's exit.
   */
  #wait(attempt, then) {
    const base = Math.min(250 * 2 ** Math.min(attempt, 10), 15000);
    const w = {
      t: setTimeout(() => {
        this.#waits.delete(w);
        if (!this.#closed) then();
      }, base + Math.random() * base * 0.3),
    };
    w.t.unref?.();
    this.#waits.add(w);
  }

  #setFault(message, status, connection) {
    this.#fault = { message, status, connection };
  }

  #clearConnectionFault() {
    if (this.#fault?.connection) this.#fault = null;
  }

  #wantToken() {
    if (this.#paused) return;
    this.#paused = true;
    this.#setFault('the server refused the token (401): a fresh one is wanted', 401, true);
    if (this.#tokenProvider) {
      Promise.resolve()
        .then(() => this.#tokenProvider())
        .then((t) => t && !this.#closed && this.setToken(t), (e) => this.#onError?.(e));
    }
  }

  /**
   * The server's collections, made in the replica as it declares them --
   * field order is part of the record encoding, and rows cannot be decoded
   * if the two drift -- and made again when its schema changed.
   */
  async #schemas() {
    if (this.#schemaBusy || this.#schemaWaiting || this.#paused) return;
    this.#schemaBusy = true;
    const [status, , body] = await this.#call('GET', '/collections', null, {});
    this.#schemaBusy = false;
    if (this.#closed) return;
    if (status === 401) return this.#wantToken();
    try {
      if (status < 200 || status > 299) throw new FenecError(`could not read the server's collections: ${why(status, body)}`);
      await this.#make(JSON.parse(body));
      this.#schemaAttempt = 0;
    } catch (e) {
      this.#setFault(String(e.message ?? e), status, true);
      this.#onError?.(e);
      this.#schemaWaiting = true;
      this.#wait(this.#schemaAttempt++, () => {
        this.#schemaWaiting = false;
        this.#kick();
      });
      return;
    }
    this.#kick();
  }

  async #make(all) {
    for (const s of this.#shapes.values()) {
      if (s.made && !s.rebuild) continue;
      const schema = all.find((x) => x.name === s.collection);
      if (!schema) throw new FenecError(`the server has no \`${s.collection}\` collection`);
      const again = s.rebuild;
      await this.#remake(s, schema);
      if (again) this.#relay({ t: 'schema', c: s.collection, s: schema });
    }
  }

  /**
   * `shape`'s collection made as `schema` says: anew, or again over the one
   * the replica holds -- its rows kept in the fields that are left, so it
   * reads as it did until the seed writes the shape over it, and the rows of
   * writes not yet answered stay as a seed keeps them.
   */
  async #remake(s, schema) {
    const c = s.collection;
    const had = this.#local.schemas().some((x) => x.name === c);
    if (had) {
      const fields = new Set(schema.fields.map((f) => f.name));
      const rows = (this.#local.run(`get ${c}`).rows ?? []).map((r) =>
        Object.fromEntries(Object.entries(r).filter(([k, v]) => (k === 'id' || fields.has(k)) && v !== null)),
      );
      this.#local.run(`drop collection ${c}`);
      this.#local.run(schemaDDL(schema));
      await this.#insertChunked(c, rows);
      // The code's schema checked again: the server's changed under it.
      const d = declared.get(this);
      if (d) checked(this.#remote, d, 'follow').catch((e) => this.#onError?.(e));
    } else {
      this.#local.run(schemaDDL(schema));
    }
    s.fields = new Set(schema.fields.map((f) => f.name));
    s.made = true;
    s.rebuild = false;
    this.#touch();
    this.#persistSoon();
  }

  /**
   * The server's schema is not the replica's: a change said so, or rows came
   * with a field the replica has not got. The stream ends, the collection is
   * made again from `GET /collections`, and the shape is sent whole -- a
   * rename or a drop is not told apart from the rows.
   */
  #refresh(s) {
    this.#cancel(s);
    s.rebuild = true;
    s.fresh = true;
    this.#kick();
  }

  /** Whether every field `rows` name is one the replica's collection has. */
  #fits(s, rows) {
    for (const r of rows) for (const k in r) if (k !== 'id' && !s.fields.has(k)) return false;
    return true;
  }

  /** Sends the queue's next write: one at a time and in order, so the server ends holding the last. */
  async #pump() {
    if (this.#sending || this.#sendWaiting || this.#paused || this.#offline || this.#closed) return;
    const op = this.#queue[0];
    if (!op) return;
    this.#sending = true;
    const type = op.path === '/batch' ? 'application/x-ndjson' : 'application/json';
    const [status, seq, body] = await this.#call('POST', op.path, op.body, { 'content-type': type, 'idempotency-key': op.key });
    this.#sending = false;
    if (this.#closed) return;
    if (status >= 200 && status < 300) {
      this.#sendAttempt = 0;
      this.#clearConnectionFault();
      this.#landed(op, seq);
    } else if (status === 401) {
      this.#wantToken();
    } else if (status === 0 || status === 408 || status === 429 || status >= 500) {
      // No answer, or one that says to come back: the write stays, and goes
      // again under its key, which the server answers as the first time if
      // the first reached it.
      this.#setFault(why(status, body), status, true);
      this.#flushing = false;
      this.#sendWaiting = true;
      this.#wait(this.#sendAttempt++, () => {
        this.#sendWaiting = false;
        this.#kick();
      });
    } else {
      this.#refused(op, status, why(status, body));
    }
    this.#kick();
  }

  /** A request's `[status, Fenec-Seq, body]`; status 0 when none came. */
  async #call(method, path, body, headers) {
    if (this.#token) headers.authorization = `Bearer ${this.#token}`;
    try {
      const res = await this.#fetch(`${this.#url}${path}`, { method, headers, body, signal: this.#abort.signal });
      return [res.status, Number(res.headers?.get?.('fenec-seq')) || 0, await res.text()];
    } catch (e) {
      return [0, 0, String(e?.message ?? e)];
    }
  }

  /** The server took `op`: it leaves the queue, and its rows wait for the server's copies. */
  #landed(op, seq) {
    const at = this.#queue.indexOf(op);
    if (at < 0) return;
    this.#queue.splice(at, 1);
    this.#local.run('del _sync_queue where id = $1', [op.n]);
    for (const t of this.#temps) {
      if (t.op !== op.n) continue;
      t.op = 0;
      t.until = seq || NO_SEQ;
      this.#saveTemp(t);
    }
    // A stream already past it: the server's copy came, or the shape does
    // not hold the row.
    for (const s of this.#shapes.values()) this.#dropPassed(s.collection, s.cursor);
    this.#settled(op, { t: 'ans', key: op.key, status: 200, seq });
    op.done?.resolve(op.count);
  }

  /** The server refused `op`: what it did is put back, the last statement's first, and the app told. */
  #refused(op, status, message) {
    const at = this.#queue.indexOf(op);
    if (at < 0) return;
    this.#queue.splice(at, 1);
    this.#undo(JSON.parse(op.undo));
    this.#local.run('del _sync_queue where id = $1', [op.n]);
    const gone = this.#temps.filter((t) => t.op === op.n).map((t) => t.temp);
    this.#deleteLocal('_sync_temps', gone);
    this.#temps = this.#temps.filter((t) => t.op !== op.n);
    this.#setFault(message, status, false);
    this.#settled(op, { t: 'ans', key: op.key, status, message });
    const e = Object.assign(new FenecError(message), { status, query: op.label });
    if (!op.foreign) this.#onRefused?.(e);
    if (op.done) op.done.reject(e);
  }

  #settled(op, ans) {
    if (!this.#queue.length) {
      this.#flushing = false;
      for (const r of this.#pushed.splice(0)) r();
    }
    this.#touch();
    this.#persistSoon();
    this.#relay(ans);
  }

  /**
   * The temporary rows of `collection` the server answered for at or before
   * `cursor`: the server's copy is here by now, or the shape does not hold it.
   */
  #dropPassed(collection, cursor) {
    const gone = this.#temps.filter((t) => t.collection === collection && t.op === 0 && t.until <= cursor);
    if (!gone.length) return;
    const ids = gone.map((t) => t.temp);
    this.#deleteLocal(collection, ids);
    this.#deleteLocal('_sync_temps', ids);
    this.#temps = this.#temps.filter((t) => !gone.includes(t));
  }

  // ---------------------------------------------------------- applying

  /**
   * The seed: the whole shape. What the replica holds of the collection and
   * the seed does not is deleted -- written over rather than cleared first,
   * so a row that stayed keeps its vector's node -- except the rows of
   * writes the server has not answered yet, and of those it answered after
   * the seed was taken, whose copies come later.
   */
  async #applySeed(s, msg) {
    const rows = msg.rows ?? [];
    const seq = msg.seq ?? 0;
    if (!this.#fits(s, rows)) return this.#refresh(s);
    const c = s.collection;
    const held = (this.#local.run(`get ${c} select id`).rows ?? []).map((r) => r.id);
    const came = new Set(rows.map((r) => r.id));
    this.#reconcile(s, rows);
    const keep = new Set(this.#temps.filter((t) => t.collection === c && (t.op !== 0 || t.until > seq)).map((t) => t.temp));
    this.#deleteLocal(c, held.filter((id) => !came.has(id) && !keep.has(id)));
    const dropped = this.#temps.filter((t) => t.collection === c && t.op === 0 && t.until <= seq);
    this.#deleteLocal('_sync_temps', dropped.map((t) => t.temp));
    this.#temps = this.#temps.filter((t) => !dropped.includes(t));
    await this.#insertChunked(c, rows);
    s.cursor = seq;
    s.seeded = true;
    s.fresh = false;
    this.#afterApply(s);
    return true;
  }

  /** A change: the rows that changed as they are now, and the ids that left the shape. */
  async #applyChange(s, msg) {
    const puts = msg.puts ?? [];
    if (msg.schema || !this.#fits(s, puts)) return this.#refresh(s);
    this.#reconcile(s, puts);
    this.#deleteLocal(s.collection, msg.dels ?? []);
    await this.#insertChunked(s.collection, puts);
    const seq = msg.seq ?? s.cursor;
    this.#dropPassed(s.collection, seq);
    s.cursor = Math.max(seq, s.cursor);
    this.#afterApply(s);
    return true;
  }

  /**
   * When a row from the server carries the key of a pending optimistic row,
   * the one with the temporary id is dropped. The counterpart of TanStack
   * DB's `txid`; here the handle is a **business key**, because the id space
   * belongs to the server and the client cannot know it in advance.
   */
  #reconcile(s, rows) {
    if (!s.key || rows.length === 0) return;
    const c = s.collection;
    if (!this.#temps.some((t) => t.collection === c)) return;
    const drop = [];
    for (const r of rows) {
      const k = r[s.key];
      if (k == null) continue;
      const t = this.#temps.find((x) => x.collection === c && x.key === keyText(k));
      if (t && t.temp !== r.id) drop.push(t);
    }
    if (!drop.length) return;
    const ids = drop.map((t) => t.temp);
    this.#deleteLocal(c, ids);
    this.#deleteLocal('_sync_temps', ids);
    this.#temps = this.#temps.filter((t) => !drop.includes(t));
  }

  /**
   * A large seed fits in a single statement, but the generated text runs
   * into megabytes; writing in chunks keeps peak memory low.
   */
  async #insertChunked(collection, rows, size = 500) {
    for (let i = 0; i < rows.length; i += size) {
      await from(collection).bind(this.#localExec).insert(rows.slice(i, i + size));
    }
  }

  /** The cursor kept beside the rows it stands for. */
  #afterApply(s) {
    this.#local.run(PUT_SHAPE, [s.row, s.collection, s.fingerprint, s.cursor, s.seeded]);
    s.error = null;
    this.#touch();
    this.#persistSoon();
    if (this.#allSeeded) this.#resolveReady();
  }

  get #allSeeded() {
    return [...this.#shapes.values()].every((s) => s.seeded);
  }

  // -------------------------------------------------------------- live

  /**
   * Schedules the next tick; changes arriving back to back collapse into
   * a single run.
   *
   * Frame alignment is **not enough on its own**: `requestAnimationFrame`
   * never runs in a hidden tab. Tied to it alone, a backgrounded tab would
   * keep receiving data while its live queries silently stopped -- the
   * worst kind of silently wrong state. So the two **race**: in a visible
   * tab the frame arrives first and cancels the timer, in a hidden tab the
   * timer takes over.
   */
  #touch() {
    if (this.#scheduled) return;
    const raf = globalThis.requestAnimationFrame;
    const fire = () => {
      if (!this.#scheduled) return;
      clearTimeout(this.#scheduled.timer);
      if (this.#scheduled.frame !== undefined) {
        globalThis.cancelAnimationFrame?.(this.#scheduled.frame);
      }
      this.#scheduled = null;
      this.#pendingTick = this.#lives.tick().finally(() => {
        this.#pendingTick = null;
      });
      // Anyone waiting in `flush()`: the tick has *started*, so from here
      // on it can be awaited through `#pendingTick`.
      for (const f of this.#flushers.splice(0)) f();
    };
    this.#scheduled = {
      timer: setTimeout(fire, TICK_FALLBACK_MS),
      frame: raf ? raf.call(globalThis, fire) : undefined,
    };
    this.#scheduled.timer.unref?.();
  }

  // ------------------------------------------------------------ stream

  async start() {
    await this.#loadPersist();
    for (const text of SYNC_STATE) this.#local.run(text);
    this.#load();
    const schemas = this.#local.schemas();
    for (const s of this.#shapes.values()) {
      const schema = schemas.find((x) => x.name === s.collection);
      s.made = !!schema;
      s.fields = new Set(schema?.fields.map((f) => f.name));
    }
    this.#flushing = this.#queue.length > 0;
    if (this.#allSeeded) this.#resolveReady();
    // The replica as it was kept: a live query asked before it ran on the
    // empty one.
    this.#lives.stale();
    this.#touch();
    const on = globalThis.addEventListener;
    if (on) {
      this.#listeners = [['online', () => this.setOnline(true)], ['offline', () => this.setOnline(false)]];
      for (const [k, f] of this.#listeners) on.call(globalThis, k, f);
    }
    this.#elect();
    return this;
  }

  /** What the replica kept: each shape's cursor, the queue and the temporary rows. */
  #load() {
    let top = 0;
    for (const r of this.#local.run('get _sync_shapes').rows ?? []) {
      top = Math.max(top, r.id);
      const s = this.#shapes.get(r.collection);
      if (!s) continue;
      s.row = r.id;
      // Another server, filter or projection holds other rows: seeded again.
      if (r.shape === s.fingerprint) {
        s.cursor = r.cursor;
        s.seeded = r.seeded === true;
      }
    }
    for (const s of this.#shapes.values()) if (!s.row) s.row = ++top;
    this.#queue = (this.#local.run('get _sync_queue').rows ?? []).sort((a, b) => a.id - b.id).map((r) => ({
      n: r.id,
      path: r.path,
      body: r.body,
      key: r.key,
      undo: r.undo,
      label: r.label,
      count: 0,
      done: null,
    }));
    this.#nextN = (this.#queue.at(-1)?.n ?? 0) + 1;
    this.#temps = (this.#local.run('get _sync_temps').rows ?? []).map((r) => ({
      collection: r.collection,
      key: r.key,
      temp: r.id,
      op: r.op,
      until: r.until,
    }));
    this.#nextTemp = Math.max(TEMP_BASE, ...this.#temps.map((t) => t.temp + 1));
  }

  /**
   * Multiple tabs: each tab would have its own WASM instance and its own
   * subscription -- N copies, N connections. `navigator.locks` picks a
   * **single leader**; the others take the same batches over
   * `BroadcastChannel` and apply them to their own local copy.
   *
   * Only the leader sends writes. Every tab applies its own at once and
   * keeps it in its queue, as the leader does; a follower hands it to the
   * leader, which keeps it in its own queue -- persisted with its replica --
   * sends it under its key, and tells every tab the answer, which the tab
   * that made the write applies to its copy. A new leader is handed every
   * follower's queue again, and the queue its predecessor persisted; the
   * key makes a write sent twice land once.
   */
  #elect() {
    const name = `fenecdb:${this.#url}:${[...this.#shapes.keys()].join(',')}`;
    if (this.#leaderMode === false || !globalThis.BroadcastChannel || !this.#locks) {
      // A single tab (or Node): no leader election needed.
      this.#kick();
      return;
    }
    this.#leader = false;
    this.#chan = new BroadcastChannel(name);
    this.#chan.onmessage = (e) => {
      this.#onRelay(e.data).catch((err) => this.#onError?.(err));
    };
    // Ask an existing leader for a seed, so we can work without waiting
    // for our turn at the lock. **Repeated**, because the leader may be
    // downloading its own seed at that moment; it ignores the request then,
    // and a one-shot hello would go unanswered forever.
    this.#hello();
    this.#kick();
    this.#locks.request(name, { mode: 'exclusive' }, async () => {
      if (this.#closed) return;
      await this.#lead();
      // The lock stays with us until the tab closes.
      return new Promise((r) => {
        this.#release = r;
      });
    });
  }

  /** This tab leads now: the queue its predecessor kept is taken over, and the others asked for theirs. */
  async #lead() {
    if (this.#persistKey && globalThis.indexedDB && !kept.has(this.#local)) {
      const other = sibling(this.#local);
      try {
        if (await restore(other, this.#persistKey, { cryptoKey: this.#cryptoKey })) {
          for (const r of other.run('get _sync_queue').rows ?? []) this.#adopt(r);
        }
      } catch (e) {
        this.#onError?.(e);
      } finally {
        other.close();
      }
    }
    this.#leader = true;
    this.#flushing = this.#queue.length > 0;
    this.#chan.postMessage({ t: 'leader' });
    this.#kick();
  }

  /** Another tab's write, kept and sent here: nothing of it to put back in this replica. */
  #adopt(o) {
    if (this.#queue.some((q) => q.key === o.key)) return;
    const op = { n: this.#nextN++, path: o.path, body: o.body, key: o.key, undo: '[]', label: o.label, count: 0, done: null, foreign: true };
    this.#keep(op);
    this.#queue.push(op);
    this.#persistSoon();
  }

  /** Repeats the hello until the leader is seeded. */
  #hello() {
    if (this.#closed || this.#leader || this.#allSeeded) return;
    this.#chan?.postMessage({ t: 'hello' });
    const t = setTimeout(() => this.#hello(), 400);
    t.unref?.();
  }

  /** Opens `s`'s stream, unless one is open or waits to be. */
  #connect(s) {
    if (s.stream || s.waiting || this.#paused || this.#offline || this.#closed) return;
    const ctl = new AbortController();
    s.stream = ctl;
    this.#stream(s, ctl).catch((e) => this.#onError?.(e));
  }

  /** Ends `s`'s stream from this side. */
  #cancel(s) {
    s.stream?.abort();
    s.stream = null;
    s.connected = false;
  }

  async #stream(s, ctl) {
    let reason = 'the server closed it';
    try {
      const res = await this.#fetch(this.#streamUrl(s), { headers: this.#headers(), signal: ctl.signal });
      if (s.stream !== ctl) return;
      if (res.status === 401) {
        this.#cancel(s);
        return this.#wantToken();
      }
      if (!res.ok || !res.body) {
        const text = await res.text().catch(() => '');
        throw new FenecError(`could not open the subscription to \`${s.collection}\`: ${why(res.status, text)}`);
      }
      s.connected = true;
      s.attempt = 0;
      this.#clearConnectionFault();
      for await (const ev of sseEvents(res)) {
        if (s.stream !== ctl) return;
        await this.#onEvent(s, ev);
      }
    } catch (e) {
      if (s.stream !== ctl) return;
      reason = String(e?.message ?? e);
      s.error = e;
      this.#onError?.(e);
    }
    if (s.stream !== ctl) return;
    this.#cancel(s);
    this.#setFault(`the subscription to \`${s.collection}\` ended: ${reason}`, 0, true);
    s.waiting = true;
    this.#wait(s.attempt++, () => {
      s.waiting = false;
      this.#kick();
    });
  }

  async #onEvent(s, ev) {
    const msg = ev.data ? JSON.parse(ev.data) : {};
    if (ev.name === 'seed') {
      if (await this.#applySeed(s, msg)) this.#relay({ t: 'seed', c: s.collection, ...msg });
    } else if (ev.name === 'change' && s.seeded) {
      if (await this.#applyChange(s, msg)) this.#relay({ t: 'change', c: s.collection, ...msg });
    } else if (ev.name === 'error') {
      throw new FenecError(msg.error ?? 'subscription error');
    }
  }

  #streamUrl(s) {
    const p = new URLSearchParams();
    for (const [k, v] of s.params) p.append(k, v);
    if (s.select) p.set('select', s.select.join(','));
    // Seeded once, it goes on from where it stopped: the server seeds it
    // again itself when the cursor is past its ring.
    if (s.seeded && !s.fresh) p.set('since', String(s.cursor));
    const qs = p.toString();
    return `${this.#url}/${encodeURIComponent(s.collection)}/changes${qs ? `?${qs}` : ''}`;
  }

  #headers() {
    const h = { accept: 'text/event-stream' };
    if (this.#token) h.authorization = `Bearer ${this.#token}`;
    return h;
  }

  // -------------------------------------------------------------- tabs

  #relay(msg) {
    if (this.#chan && this.#leader) this.#chan.postMessage(msg);
  }

  async #onRelay(msg) {
    if (this.#leader) {
      if (msg.t === 'op') {
        this.#adopt(msg.op);
        this.#kick();
      } else if (msg.t === 'hello') {
        // Hand the new tab what we have, its own rows not among it: a
        // temporary id is this replica's alone.
        for (const s of this.#shapes.values()) {
          if (!s.seeded) continue;
          const rows = await from(s.collection).where('id', '<', TEMP_BASE).bind(this.#localExec).rows();
          this.#chan.postMessage({ t: 'seed', c: s.collection, rows, seq: s.cursor });
        }
        this.#chan.postMessage({ t: 'leader' });
      }
      return;
    }
    if (msg.t === 'leader') {
      for (const op of this.#queue) this.#chan.postMessage({ t: 'op', op: wire(op) });
      return;
    }
    if (msg.t === 'ans') {
      const op = this.#queue.find((o) => o.key === msg.key);
      if (!op) return;
      if (msg.status < 300) this.#landed(op, msg.seq);
      else this.#refused(op, msg.status, msg.message);
      return;
    }
    const s = this.#shapes.get(msg.c);
    if (!s) return;
    if (msg.t === 'schema') await this.#remake(s, msg.s);
    else if (msg.t === 'seed') await this.#applySeed(s, msg);
    // An incremental diff cannot be applied to an unseeded copy: we would
    // be left with the changed rows only, the rest missing.
    else if (msg.t === 'change' && s.seeded) await this.#applyChange(s, msg);
  }

  // ------------------------------------------------------- persistence

  /**
   * Stores the replica now -- its writes since the last time, a chunk --
   * with the sync's own collections in it: a write is kept the moment it
   * is made. One store at a time, the next gathering what came meanwhile.
   * The leader's alone: the other tabs' copies are fed by it.
   */
  #persistSoon() {
    if (!this.#persistKey || !globalThis.indexedDB || !this.#leader || kept.has(this.#local)) return;
    if (this.#persisting) {
      this.#persistAgain = true;
      return;
    }
    this.#persisting = (async () => {
      do {
        this.#persistAgain = false;
        await persist(this.#local, this.#persistKey, { cryptoKey: this.#cryptoKey });
      } while (this.#persistAgain && !this.#closed);
    })()
      .catch((e) => this.#onError?.(e))
      .finally(() => {
        this.#persisting = null;
      });
  }

  /**
   * Restores the previous session's image: the replica, its cursors and the
   * writes it had not sent, which go first.
   */
  async #loadPersist() {
    if (!this.#persistKey || !globalThis.indexedDB || kept.has(this.#local)) return;
    try {
      await restore(this.#local, this.#persistKey, { cryptoKey: this.#cryptoKey });
    } catch (e) {
      // A corrupt or incompatible cache: reseed from scratch, not an error.
      this.#onError?.(e);
    }
  }
}

/** A refusal's words: the server's `{"error": ..}`, or what the transport said. */
function why(status, body) {
  try {
    const e = JSON.parse(body)?.error;
    if (e) return e;
  } catch {
    /* not JSON */
  }
  if (status === 0) return body || 'the server could not be reached';
  return `HTTP ${status}: ${String(body).slice(0, 200)}`;
}

/** What another tab needs of a write to send it. */
function wire(op) {
  return { path: op.path, body: op.body, key: op.key, label: op.label };
}

/** A batch line: each line is exactly a `POST /query` body. */
function ndjson([sql, params]) {
  return JSON.stringify({ query: sql, params });
}

function normalizeShape(raw, url) {
  const spec = raw instanceof Query ? { collection: raw.collection } : raw;
  if (!isSpec(spec) || typeof spec.collection !== 'string') {
    throw new FenecError('a shape must be `{ collection, where?, select?, key? }`');
  }
  const collection = ident(nameOf(spec.collection), 'collection');
  if (collection.startsWith('_sync_')) throw new FenecError(`\`${collection}\` is the sync's own collection`);
  const select = spec.select ? [...new Set(['id', ...spec.select.map((c) => ident(c))])] : null;
  const params = shapeParams(spec.where);
  return {
    collection,
    key: spec.key ? ident(spec.key) : null,
    select,
    params,
    // The server, the filter and the projection, as the native core writes
    // them: a replica opened with another holds rows it should not.
    fingerprint: `${url}|${params.map(([k, v]) => `${k}=${v}&`).join('')}${select ? `|${select.join(',')}` : ''}`,
    row: 0,
    cursor: 0,
    seeded: false,
    connected: false,
    error: null,
    stream: null,
    waiting: false,
    attempt: 0,
    made: false,
    fields: new Set(),
    rebuild: false,
    fresh: false,
  };
}

/**
 * Opens the local replica and starts the subscriptions.
 *
 * - `url`      server root (`fenec-server --http`)
 * - `shapes`   `[{ collection, where?, select?, key? }]`
 * - `local`    an existing `Fenec`; otherwise opened from the `wasm` path,
 *              its collation data from `collation` (`Fenec.open`). One kept
 *              in a file (`openFile`) keeps the replica, its cursors and
 *              its unsent writes there.
 * - `token`    `Authorization: Bearer`; `tokenProvider` an async function
 *              asked for a new one when the server answers 401
 * - `persist`  IndexedDB key: the replica, its cursors and its unsent writes
 * - `cryptoKey` an AES-GCM `CryptoKey` the image and its chunks are sealed
 *              with (`persist`)
 * - `onRefused` told of each write the server refused, which was put back
 * - `leader`   `false` turns off multi-tab leader election
 * - `locks`    lock manager (defaults to `navigator.locks`)
 */
export async function sync(opts = {}) {
  if (!opts.url) throw new FenecError('sync(): `url` is required');
  const local = opts.local ?? (await Fenec.open(opts.wasm ?? './fenec.wasm', { collation: opts.collation }));
  const replica = new FenecSync(local, opts);
  // The server owns the schema: the code's is compared with it, never applied.
  await checked(replica.remote, opts, 'follow');
  if (opts.schema) declared.set(replica, opts);
  return replica.start();
}
