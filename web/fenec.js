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

/** A second database over a database's module (`Fenec`'s static block). */
let sibling;

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

  constructor(wasm, handle, collation = null) {
    this.#wasm = wasm;
    this.#handle = handle;
    this.#collation = collation;
  }

  static {
    // For `openFile` to try bytes in without touching the database it keeps.
    sibling = (f) => new Fenec(f.#wasm, f.#wasm.fenec_open(), f.#collation);
  }

  /**
   * Loads the WASM module and opens an empty database.
   * @param {string|BufferSource} src  A URL, or the module bytes themselves.
   *   The bytes are for Node: `fetch` cannot resolve a relative path there,
   *   so the file is read with `readFile` and handed over directly.
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
    const wasm = mod.instance.exports;
    return new Fenec(wasm, wasm.fenec_open(), opts.collation ?? beside(src));
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

  /** Copies a string into WASM memory. */
  #write(str) {
    const bytes = enc.encode(str);
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
    const [sp, sl] = this.#write(sql);
    const [pp, pl] = this.#write(JSON.stringify(params));
    let out;
    try {
      out = this.#readString(this.#wasm.fenec_query(this.#handle, sp, sl, pp, pl));
    } finally {
      this.#wasm.fenec_free(sp, sl || 1);
      this.#wasm.fenec_free(pp, pl || 1);
    }
    // Before the answer, and before an error too: a statement that failed
    // may follow ones in the same text that wrote.
    kept.get(this)?.flush();
    const res = JSON.parse(out);
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
    for (;;) {
      try {
        return this.run(sql, params);
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
      collection: ident(name, 'collection'),
      exec: (sql, params) => this.query(sql, params),
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
    return this.#readBytes(this.#wasm.fenec_snapshot(this.#handle));
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
    let r;
    try {
      r = this.#wasm.fenec_load(this.#handle, ptr, bytes.length);
    } finally {
      this.#wasm.fenec_free(ptr, bytes.length);
    }
    if (r & 2) {
      const { chunks } = this.#collationState();
      const e = new FenecError('the image needs collation data the module has not been handed');
      throw Object.assign(e, { collation: chunks.filter((_, i) => (r >>> 2) & (1 << i)), ran: 0 });
    }
    if (r !== 0) throw new FenecError('could not load image (corrupt or incompatible version)');
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
    this.#wasm.fenec_close(this.#handle);
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
    const headers = { 'content-type': 'application/json' };
    if (this.#token) headers.authorization = `Bearer ${this.#token}`;
    const res = await this.#fetch(`${this.#url}/query`, {
      method: 'POST',
      headers,
      body: JSON.stringify({ query: sql, params: params.map((p) => normalize(p)) }),
    });
    const text = await res.text();
    let body;
    try {
      body = text ? JSON.parse(text) : null;
    } catch {
      throw new FenecError(`server did not return JSON (${res.status}): ${text.slice(0, 200)}`);
    }
    if (!res.ok) {
      throw new FenecError(body?.error ?? `HTTP ${res.status}`);
    }
    // The endpoint returns rows as a plain array; the builder expects `{rows}`.
    if (Array.isArray(body)) return { rows: body };
    if (body && typeof body.affected === 'number') {
      return { kind: 'affected', count: body.affected };
    }
    return body;
  }

  async rows(sql, params = []) {
    return (await this.run(sql, params)).rows ?? [];
  }

  /** Query builder -- identical to the one on the wasm path. */
  from(name) {
    return new Query({
      collection: ident(name, 'collection'),
      exec: (sql, params) => this.run(sql, params),
    });
  }

  /** Collection schemas. */
  async schemas() {
    return this.run('collections');
  }
}

/** Connects to a remote fenecdb HTTP endpoint (`fenec-pg --http`). */
export function connect(url, opts = {}) {
  return new FenecHttp(url, opts);
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
// `bind()` attaches it to any executor (wasm, HTTP, fenec-pg).

/** Operator names -- both symbols and words are accepted. */
const OPS = {
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

/**
 * The `order` spec of a `lookup`: `'created'`, or `[['created','desc'], ...]`,
 * a pair taking `{ collate }` third as `order()` does. A bare string is one
 * ascending key; anything else is a list of pairs, so there is no reading
 * under which `['a','desc']` could mean two fields.
 */
function orderKeys(spec) {
  if (spec === undefined || spec === null) return [];
  if (typeof spec === 'string') return [{ field: ident(spec), asc: true, collate: null }];
  return spec.map((k) => {
    const [field, dir = 'asc', opts = {}] = [k].flat();
    return { field: ident(field), asc: direction(dir), collate: collation(opts.collate) };
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

/**
 * A select-list item: a field, or an aggregate spelled as FenecQL spells it
 * -- `count(*)`, `sum(total)`, `avg(f)`, `min(f)`, `max(f)` -- which answers
 * under that same name.
 */
const AGGREGATE = /^(count)\(\*?\)$|^(sum|avg|min|max)\(([A-Za-z_][A-Za-z0-9_]*)\)$/i;

function column(name) {
  const m = typeof name === 'string' ? AGGREGATE.exec(name.trim()) : null;
  if (!m) return { text: ident(name), aggregate: false };
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
  throw new FenecError(`an object cannot be used as a fenecdb value (${what})`);
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

/** `or(a, b)` / `or([a, b])` -- joins conditions with `or`. */
export function or(...conds) {
  return { t: 'or', items: conds.flat().map(toCond) };
}

/** `and(a, b)` -- `where` already ands; this is only needed inside `or`. */
export function and(...conds) {
  return { t: 'and', items: conds.flat().map(toCond) };
}

/** `not(condition)` */
export function not(cond) {
  return { t: 'not', item: toCond(cond) };
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
  return { t: 'raw', sql, params };
}

const NODES = new Set(['and', 'or', 'not', 'raw', 'cmp', 'in', 'null']);

function toCond(x) {
  if (isSpec(x) && typeof x.t === 'string' && NODES.has(x.t)) return x;
  if (isSpec(x)) return objectCond(x);
  throw new FenecError(`expected an object as a condition: ${JSON.stringify(x)}`);
}

/** `{ year: {gte: 2024}, tags: {has: 'rust'} }` -> an `and` tree */
function objectCond(obj) {
  const items = Object.entries(obj).map(([field, spec]) =>
    fieldCond(ident(field), spec),
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
          : select.map((c) => ident(c)),
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
    const params = [];
    const bind = binder(params);

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
    return [sql, params];
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
  if (args.length === 2) return fieldCond(ident(args[0]), args[1]);
  if (args.length === 3) {
    const op = OPS[args[1]];
    if (!op) throw new FenecError(`unknown operator \`${args[1]}\``);
    const field = ident(args[0]);
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
    .map(([k, v]) => `${ident(k)}: ${bind(v, k)}`);
  if (pairs.length === 0) throw new FenecError('cannot write an empty document');
  return `{${pairs.join(', ')}}`;
}

/**
 * Unbound query builder. For handing the generated text to another
 * transport, or comparing it in a test: `from('docs').where(...).toFenecQL()`.
 */
export function from(name) {
  return new Query({ collection: ident(name, 'collection') });
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
 * written.
 */
export async function persist(fenec, key = 'default') {
  if (kept.has(fenec)) throw new FenecError('the database is kept in a file (openFile): persist would take the writes it appends');
  const db = await idb();
  let s = stored.get(fenec);
  let image = null;
  let chunk = null;
  if (s?.key !== key) {
    fenec.journal();
    image = fenec.snapshot();
    s = { key, next: 1, image: 0, chunks: 0 };
    stored.set(fenec, s);
  } else {
    const { replace, bytes } = fenec.drain();
    if (replace) image = bytes;
    else if (bytes.length === 0) return 0;
    else if (s.chunks + bytes.length > s.image * FOLD) image = fenec.snapshot();
    else chunk = bytes;
  }
  try {
    await new Promise((res, rej) => {
      const tx = db.transaction(STORE, 'readwrite');
      const os = tx.objectStore(STORE);
      if (image) {
        os.put(image, key);
        os.delete(chunkRange(key));
      } else {
        os.put(chunk, chunkKey(key, s.next));
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
  if (image) Object.assign(s, { next: 1, image: image.length, chunks: 0 });
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
 */
export async function restore(fenec, key = 'default') {
  const db = await idb();
  const [image, chunks] = await new Promise((res, rej) => {
    const tx = db.transaction(STORE, 'readonly');
    const os = tx.objectStore(STORE);
    const img = os.get(key);
    const ch = os.getAll(chunkRange(key));
    tx.oncomplete = () => res([img.result, ch.result]);
    tx.onerror = () => rej(tx.error);
  });
  if (!image) return false;
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
  await fenec.loadAsync(bytes);
  fenec.journal();
  stored.set(fenec, { key, next: chunks.length + 1, image: image.length, chunks: bytes.length - image.length });
  return true;
}

// ------------------------------------------------------------- file (OPFS)
// A database kept in a file of the origin private file system holds what
// fenec-pg holds on disk, byte for byte: an image, then every write since,
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
 * fenec-pg keeps on disk. A file holding some is loaded into the database,
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
 */
function schemaDDL(schema) {
  const fields = schema.fields.map(
    (f) =>
      `${ident(f.name)} ${f.type}` +
      (f.collate ? ` collate ${collation(f.collate)}` : '') +
      (f.required ? ' required' : '') +
      (f.index ? ` @${f.index}` : ''),
  );
  return `create collection if not exists ${ident(schema.name, 'collection')} (${fields.join(', ')})`;
}

function newKey() {
  if (globalThis.crypto?.randomUUID) return globalThis.crypto.randomUUID();
  return `k${Date.now().toString(36)}${Math.random().toString(36).slice(2, 10)}`;
}

// The backoff wait must not hold up process exit: on Node the timer is
// unreferenced (browsers have no `unref`, so it is a no-op).
/**
 * Fallback that runs the tick after at most this long when no frame
 * arrives. In a visible tab the frame lands in ~16 ms and cancels it; in a
 * hidden tab this bound is the ceiling on the delay.
 */
const TICK_FALLBACK_MS = 50;

const sleep = (ms) =>
  new Promise((r) => {
    setTimeout(r, ms).unref?.();
  });

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
 */
export class FenecSync {
  #local;
  #remote;
  #url;
  #token;
  #fetch;
  #shapes = new Map();
  #subs = [];
  #liveCursor = 0;
  #scheduled = null;
  #flushers = [];
  #pending = new Map();
  #nextTemp = TEMP_BASE;
  #queue = null;
  #closed = false;
  #abort = null;
  #ready;
  #resolveReady;
  #persistKey = null;
  #chan = null;
  #leader = true;
  #leaderMode = 'auto';
  #locks = null;
  #onError;
  #persistTimer = null;
  #pendingTick = null;

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
    this.#onError = opts.onError ?? null;
    this.#leaderMode = opts.leader ?? 'auto';
    // The lock manager can be supplied from outside: the default is the Web
    // Locks API, but making it injectable keeps this testable and leaves
    // room for another coordination mechanism.
    this.#locks = opts.locks ?? globalThis.navigator?.locks ?? null;
    this.#abort = new AbortController();
    this.#liveCursor = local.changeSeq;

    for (const raw of opts.shapes ?? []) {
      const shape = normalizeShape(raw);
      if (this.#shapes.has(shape.collection)) {
        throw new FenecError(
          `two shapes for \`${shape.collection}\`: one collection per shape ` +
            '(a seed has to say "this is the whole collection")',
        );
      }
      this.#shapes.set(shape.collection, shape);
      this.#pending.set(shape.collection, new Map());
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

  /** Shape state: `{collection, cursor, seeded, connected, pending, error}`. */
  status() {
    return [...this.#shapes.values()].map((s) => ({
      collection: s.collection,
      cursor: s.cursor,
      seeded: s.seeded,
      connected: s.connected,
      pending: this.#pending.get(s.collection).size,
      error: s.error ? String(s.error.message ?? s.error) : null,
      leader: this.#leader,
    }));
  }

  /**
   * Query builder. Reads are **local** (no network); a write goes local
   * first, then to the server.
   */
  from(name) {
    const collection = ident(name, 'collection');
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
   * Returns: the function that ends the subscription.
   */
  live(query, cb, opts = {}) {
    const entry = {
      collection: query.collection,
      query: query.plain().bind((sql, params) => this.#local.query(sql, params)),
      cb,
      onError: opts.onError ?? this.#onError,
    };
    this.#subs.push(entry);
    // The first value right away: a subscriber should not start on a blank screen.
    this.#runLive(entry);
    return () => {
      const i = this.#subs.indexOf(entry);
      if (i >= 0) this.#subs.splice(i, 1);
    };
  }

  /**
   * Sends several writes in **a single round trip**.
   *
   * **Not a transaction.** fenecdb has no transactions and this batch does
   * not invent one. The local side is rolled back exactly; the server side
   * cannot be: it stops at the first error and reports how many were
   * applied in that error. The gain is twofold -- one round trip instead
   * of N, and no other writer slipping in between.
   */
  async batch(fn) {
    if (this.#queue) throw new FenecError('nested batches are not supported');
    const q = { ops: [], undo: [] };
    this.#queue = q;
    try {
      await fn(this);
    } catch (e) {
      this.#queue = null;
      this.#rollback(q.undo);
      throw e;
    }
    this.#queue = null;
    if (q.ops.length === 0) return 0;
    try {
      const out = await this.#post('/batch', q.ops.map(ndjson).join('\n'), 'application/x-ndjson');
      return out.ok ?? q.ops.length;
    } catch (e) {
      this.#rollback(q.undo);
      throw e;
    }
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

  /** Closes the subscriptions. The local database stays open. */
  close() {
    this.#closed = true;
    this.#abort.abort();
    this.#chan?.close();
    this.#subs.length = 0;
  }

  // ------------------------------------------------------------ writes

  /**
   * `SyncQuery`'s write hook. Three steps, always in the same order:
   * **first collect what it takes to undo the local change**, then apply
   * it locally, then send it to the server. If the server rejects it the
   * local side is rolled back exactly -- without the network, because the
   * undo information is already in hand.
   *
   * Everything up to the first `await` is **synchronous**. An `async`
   * function runs its body synchronously up to the first `await`, so the
   * optimistic row is in the local store before the caller even awaits the
   * promise. A single microtask in between would break the "visible
   * immediately" promise.
   */
  async write(verb, q, arg, opts) {
    const collection = q.collection;
    const shape = this.#shapes.get(collection);
    const base = q.plain();

    let undo;
    let count;
    let stmt;

    if (verb === 'insert') {
      const list = Array.isArray(arg) ? arg : [arg];
      if (list.length === 0) return 0;
      const docs = list.map((d) => {
        const doc = { ...d };
        if (shape.key && doc[shape.key] === undefined) doc[shape.key] = newKey();
        return doc;
      });
      stmt = base.toInsert(docs);
      count = docs.length;

      if (shape.key) {
        // An optimistic row's id is temporary: the server will hand out its
        // own. What matches the two is the business key -- which is why an
        // insert into a keyless shape is *not* applied optimistically (see
        // below), or the server's row would leave two copies locally.
        const temps = docs.map(() => this.#nextTemp++);
        // `await undefined` would itself put a microtask in between.
        const wait = this.#optimistic(() => this.#local.run(...base.toInsert(docs.map((d, i) => ({ ...d, id: temps[i] })))));
        if (wait) await wait;
        const pend = this.#pending.get(collection);
        docs.forEach((d, i) => pend.set(String(d[shape.key]), temps[i]));
        undo = () => {
          docs.forEach((d) => pend.delete(String(d[shape.key])));
          this.#deleteLocal(collection, temps);
        };
      } else {
        // No key: the optimistic apply is skipped and the row arrives over
        // the subscription. One round trip of delay beats a silent duplicate.
        undo = () => {};
      }
    } else {
      // Update and delete work by id, and reading the previous state is
      // enough -- `update` creates no rows and `delete` deletes none it
      // created, so this id set is the whole of the change.
      // `select()` drops the projection: writing back needs every field.
      let before;
      stmt = verb === 'update' ? base.toUpdate(arg, opts) : base.toDelete(opts);
      const wait = this.#optimistic(() => {
        before = this.#local.run(...base.select().toFenecQL()).rows ?? [];
        count = this.#local.run(...stmt).count ?? 0;
      });
      if (wait) await wait;
      undo = () => {
        // An unconditional builder: `before` already carries *which* rows
        // changed, by id. Re-applying the filter (and running into
        // `insert`'s ban on filters) would be wrong.
        if (before.length) this.#local.run(...from(collection).toInsert(before));
      };
    }

    this.#touch();

    if (this.#queue) {
      this.#queue.ops.push(stmt);
      this.#queue.undo.push(undo);
      return count;
    }
    try {
      const r = await this.#remote.run(...stmt);
      return r.count ?? count;
    } catch (e) {
      undo();
      this.#touch();
      throw e;
    }
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

  #rollback(undos) {
    for (const u of undos.reverse()) {
      try {
        u();
      } catch (e) {
        this.#onError?.(e);
      }
    }
    this.#touch();
  }

  get #localExec() {
    return (sql, params) => this.#local.query(sql, params);
  }

  #deleteLocal(collection, ids) {
    if (ids.length === 0) return 0;
    const q = from(collection).where('id', 'in', ids);
    return this.#local.run(...q.toDelete()).count ?? 0;
  }

  // ---------------------------------------------------------- applying

  /**
   * The seed: "this is the whole collection". It clears first -- the one
   * collection per shape rule exists precisely to make that possible.
   */
  async #applySeed(shape, msg) {
    const rows = msg.rows ?? [];
    this.#local.run(...from(shape.collection).toDelete({ all: true }));
    this.#pending.get(shape.collection).clear();
    await this.#insertChunked(shape.collection, rows);
    shape.cursor = msg.seq ?? 0;
    shape.seeded = true;
    this.#afterApply(shape);
  }

  async #applyChange(shape, msg) {
    const puts = msg.puts ?? [];
    const dels = msg.dels ?? [];
    await this.#reconcile(shape, puts);
    this.#deleteLocal(shape.collection, dels);
    await this.#insertChunked(shape.collection, puts);
    shape.cursor = msg.seq ?? shape.cursor;
    this.#afterApply(shape);
  }

  /**
   * When a row from the server carries the same key as a pending
   * optimistic row, the one with the temporary id is dropped. The
   * counterpart of TanStack DB's `txid`; here the handle is a **business
   * key**, because the id space belongs to the server and the client
   * cannot know it in advance.
   */
  async #reconcile(shape, rows) {
    if (!shape.key || rows.length === 0) return;
    const pend = this.#pending.get(shape.collection);
    if (pend.size === 0) return;
    const drop = [];
    for (const r of rows) {
      const k = r[shape.key];
      if (k === undefined || k === null) continue;
      const temp = pend.get(String(k));
      if (temp === undefined) continue;
      pend.delete(String(k));
      if (temp !== r.id) drop.push(temp);
    }
    this.#deleteLocal(shape.collection, drop);
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

  #afterApply(shape) {
    shape.error = null;
    this.#touch();
    this.#schedulePersist();
    if (!this.#allSeeded) return;
    this.#resolveReady();
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
      this.#pendingTick = this.#tick().finally(() => {
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

  /**
   * Which live queries get re-run: collection granularity. Anything finer
   * (intersecting id sets) would cost more than the local query itself.
   */
  async #tick() {
    const info = this.#local.changes(this.#liveCursor);
    this.#liveCursor = info.seq;
    const dirty = info.collections === null ? null : new Set(info.collections);
    for (const entry of [...this.#subs]) {
      if (dirty && !dirty.has(entry.collection)) continue;
      await this.#runLive(entry);
    }
  }

  async #runLive(entry) {
    try {
      entry.cb(await entry.query.rows());
    } catch (e) {
      if (entry.onError) entry.onError(e);
      else throw e;
    }
  }

  // ------------------------------------------------------------ stream

  async start() {
    for (const shape of this.#shapes.values()) {
      await this.#ensureSchema(shape.collection);
    }
    await this.#loadPersist();
    this.#elect();
    return this;
  }

  /**
   * The schema is fetched from the server and recreated locally **exactly**
   * as is: field order is part of the record encoding, and rows cannot be
   * decoded if the two sides drift apart. That is also precisely why
   * writing the schema out a second time by hand is not wanted.
   */
  async #ensureSchema(collection) {
    if (this.#local.schemas().some((s) => s.name === collection)) return;
    const all = await this.#get('/collections');
    const schema = all.find((s) => s.name === collection);
    if (!schema) throw new FenecError(`the server has no \`${collection}\` collection`);
    this.#local.run(schemaDDL(schema));
  }

  /**
   * Multiple tabs: each tab would have its own WASM instance and its own
   * subscription -- N copies, N connections. `navigator.locks` picks a
   * **single leader**; the others take the same batches over
   * `BroadcastChannel` and apply them to their own local copy. The apply
   * path is the same either way, only the transport differs.
   *
   * Writes do not go through the leader: every tab sends its own writes
   * straight to the server. Leadership only concerns the *read* stream.
   */
  #elect() {
    const name = `fenecdb:${this.#url}:${[...this.#shapes.keys()].join(',')}`;
    if (this.#leaderMode === false || !globalThis.BroadcastChannel || !this.#locks) {
      // A single tab (or Node): no leader election needed.
      this.#startStreams();
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
    this.#locks.request(name, { mode: 'exclusive' }, () => {
      if (this.#closed) return;
      this.#leader = true;
      this.#startStreams();
      // The lock stays with us until the tab closes: a promise that never settles.
      return new Promise(() => {});
    });
  }

  /** Repeats the hello until the leader is seeded. */
  #hello() {
    if (this.#closed || this.#leader || this.#allSeeded) return;
    this.#chan?.postMessage({ t: 'hello' });
    const t = setTimeout(() => this.#hello(), 400);
    t.unref?.();
  }

  #startStreams() {
    for (const shape of this.#shapes.values()) {
      this.#loop(shape).catch((e) => this.#onError?.(e));
    }
  }

  async #loop(shape) {
    let delay = 250;
    while (!this.#closed) {
      try {
        const res = await this.#fetch(this.#streamUrl(shape), {
          headers: this.#headers(),
          signal: this.#abort.signal,
        });
        if (!res.ok || !res.body) {
          const text = await res.text().catch(() => '');
          throw new FenecError(`could not open subscription (${res.status}): ${text.slice(0, 200)}`);
        }
        shape.connected = true;
        shape.error = null;
        delay = 250;
        for await (const ev of sseEvents(res)) {
          if (this.#closed) break;
          await this.#onEvent(shape, ev);
        }
      } catch (e) {
        if (this.#closed || e?.name === 'AbortError') return;
        shape.error = e;
        this.#onError?.(e);
      }
      shape.connected = false;
      if (this.#closed) return;
      // Exponential backoff + jitter: when the server restarts, not every
      // client should come back at the same instant.
      await sleep(delay + Math.random() * delay * 0.3);
      delay = Math.min(delay * 2, 15000);
    }
  }

  async #onEvent(shape, ev) {
    const msg = ev.data ? JSON.parse(ev.data) : {};
    if (ev.name === 'seed') {
      await this.#applySeed(shape, msg);
      this.#relay({ t: 'seed', c: shape.collection, ...msg });
    } else if (ev.name === 'change') {
      await this.#applyChange(shape, msg);
      this.#relay({ t: 'change', c: shape.collection, ...msg });
    } else if (ev.name === 'error') {
      throw new FenecError(msg.error ?? 'subscription error');
    }
  }

  #streamUrl(shape) {
    const p = new URLSearchParams();
    for (const [k, v] of shape.params) p.append(k, v);
    if (shape.select) p.set('select', shape.select.join(','));
    // If we already have a seed, resume where we left off: the server
    // reseeds on its own when the cursor is too old.
    if (shape.seeded) p.set('since', String(shape.cursor));
    const qs = p.toString();
    return `${this.#url}/${encodeURIComponent(shape.collection)}/changes${qs ? `?${qs}` : ''}`;
  }

  #headers() {
    const h = { accept: 'text/event-stream' };
    if (this.#token) h.authorization = `Bearer ${this.#token}`;
    return h;
  }

  async #get(path) {
    return this.#call('GET', path, null, null);
  }

  async #post(path, body, type) {
    return this.#call('POST', path, body, type);
  }

  async #call(method, path, body, type) {
    const headers = {};
    if (type) headers['content-type'] = type;
    if (this.#token) headers.authorization = `Bearer ${this.#token}`;
    const res = await this.#fetch(`${this.#url}${path}`, {
      method,
      headers,
      body,
      signal: this.#abort.signal,
    });
    const text = await res.text();
    let out;
    try {
      out = text ? JSON.parse(text) : null;
    } catch {
      throw new FenecError(`server did not return JSON (${res.status}): ${text.slice(0, 200)}`);
    }
    if (!res.ok) {
      const e = new FenecError(out?.error ?? `HTTP ${res.status}`);
      // A half-finished batch: how many were applied rides on the error.
      if (typeof out?.completed === 'number') e.completed = out.completed;
      throw e;
    }
    return out;
  }

  // -------------------------------------------------------------- tabs

  #relay(msg) {
    if (this.#chan && this.#leader) this.#chan.postMessage(msg);
  }

  async #onRelay(msg) {
    if (msg.t === 'hello') {
      if (!this.#leader) return;
      // Hand the new tab what we have: it does not need to open its own
      // subscription.
      for (const shape of this.#shapes.values()) {
        if (!shape.seeded) continue;
        const rows = await from(shape.collection).bind(this.#localExec).rows();
        this.#chan.postMessage({ t: 'seed', c: shape.collection, rows, seq: shape.cursor });
      }
      return;
    }
    if (this.#leader) return; // the leader is fed by its own stream
    const shape = this.#shapes.get(msg.c);
    if (!shape) return;
    if (msg.t === 'seed') {
      await this.#applySeed(shape, msg);
      return;
    }
    // An incremental diff cannot be applied to an unseeded copy: we would
    // be left with the changed rows only, the rest missing. It is skipped
    // until the seed arrives, and the cursor is not advanced either.
    if (msg.t === 'change' && shape.seeded) await this.#applyChange(shape, msg);
  }

  // ------------------------------------------------------- persistence

  #schedulePersist() {
    if (!this.#persistKey || this.#persistTimer) return;
    this.#persistTimer = setTimeout(() => {
      this.#persistTimer = null;
      this.#savePersist().catch((e) => this.#onError?.(e));
    }, 2000);
    this.#persistTimer.unref?.();
  }

  async #savePersist() {
    if (!this.#persistKey || !globalThis.indexedDB) return;
    await persist(this.#local, this.#persistKey);
    await putState(`${this.#persistKey}:cursors`, {
      cursors: Object.fromEntries(
        [...this.#shapes.values()].map((s) => [s.collection, s.cursor]),
      ),
    });
  }

  /**
   * Restores the previous session's image. The cursor is stored alongside
   * the image, so the client is not reseeded from scratch but resumes
   * where it stopped -- as long as the cursor is not behind the server's horizon.
   */
  async #loadPersist() {
    if (!this.#persistKey || !globalThis.indexedDB) return;
    try {
      if (!(await restore(this.#local, this.#persistKey))) return;
      const state = await getState(`${this.#persistKey}:cursors`);
      for (const shape of this.#shapes.values()) {
        const cursor = state?.cursors?.[shape.collection];
        if (typeof cursor === 'number') {
          shape.cursor = cursor;
          shape.seeded = true;
        }
      }
      this.#liveCursor = this.#local.changeSeq;
      if (this.#allSeeded) this.#resolveReady();
    } catch (e) {
      // A corrupt or incompatible cache: reseed from scratch, not an error.
      this.#onError?.(e);
    }
  }

}

/** A batch line: each line is exactly a `POST /query` body. */
function ndjson([sql, params]) {
  return JSON.stringify({ query: sql, params });
}

function normalizeShape(raw) {
  const spec = raw instanceof Query ? { collection: raw.collection } : raw;
  if (!isSpec(spec) || typeof spec.collection !== 'string') {
    throw new FenecError('a shape must be `{ collection, where?, select?, key? }`');
  }
  const collection = ident(spec.collection, 'collection');
  const select = spec.select ? [...new Set(['id', ...spec.select.map((c) => ident(c))])] : null;
  return {
    collection,
    key: spec.key ? ident(spec.key) : null,
    select,
    params: shapeParams(spec.where),
    cursor: 0,
    seeded: false,
    connected: false,
    error: null,
  };
}

/**
 * Opens the local replica and starts the subscriptions.
 *
 * - `url`      server root (`fenec-pg --http`)
 * - `shapes`   `[{ collection, where?, select?, key? }]`
 * - `local`    an existing `Fenec`; otherwise opened from the `wasm` path,
 *              its collation data from `collation` (`Fenec.open`)
 * - `token`    `Authorization: Bearer`
 * - `persist`  IndexedDB key: the image **and the cursors** are stored
 * - `leader`   `false` turns off multi-tab leader election
 * - `locks`    lock manager (defaults to `navigator.locks`)
 */
export async function sync(opts = {}) {
  if (!opts.url) throw new FenecError('sync(): `url` is required');
  const local = opts.local ?? (await Fenec.open(opts.wasm ?? './fenec.wasm', { collation: opts.collation }));
  return new FenecSync(local, opts).start();
}

