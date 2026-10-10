// fenecdb's schema in code: collections declared in TypeScript the way
// Drizzle ORM 1.0 declares tables, beside `fenec types`, which reads them
// out of a database instead.
//
//   import { fenecTable, text, boolean, timestamp, index } from '@fenecdb/web/schema';
//
//   export const todos = fenecTable('todos', {
//     title: text().notNull(),
//     done: boolean(),
//     at: timestamp(),
//   }, (t) => [index('todos_at').on(t.at)]);
//
//   const db = await Fenec.open('./fenec.wasm', { schema: { todos } });
//   type Todo = typeof todos.$inferSelect;
//
// A declaration compiles to the description every fenecdb SDK compiles its
// own to (`describe`, `toJSON`): JSON, `{"format": 1, "collections": [...],
// "migrations": [...]}` -- or, as a table writes itself (`toFenecQL`), the
// FenecQL that declares it, which the description holds as `fenecql`.
// Comparing it with a database is the engine's, in the module
// (`Fenec.checkSchema`) and the server (`/_schema`), so what is applied at
// an open and what is refused is decided in one place for every language:
// this file only declares.
//
// A module of its own, so a page that declares nothing loads none of it.
// fenec.js reads the tables it is handed as FenecQL (`toFenecQL`), and never
// imports it. Its error comes from the builder's module, which is all of
// the client it imports: a page that declares its schema and reaches a
// server (`@fenecdb/web/client`) loads no engine for it.

import { FenecError } from './builder.js';

/** The description's format, as `fenec_core::declared` reads it. */
export const FORMAT = 1;

// FenecQL's identifier: the name goes into the statements unquoted.
const IDENT = /^[\p{Alphabetic}_][\p{Alphabetic}\p{N}_]*$/u;
const COLLATIONS = ['tr', 'und'];
// pgvector's operator classes, as Drizzle's `index().using('hnsw', t.embed.op(...))` names them.
const OPS = {
  vector_cosine_ops: 'cosine', vector_l2_ops: 'l2', vector_ip_ops: 'dot',
  halfvec_cosine_ops: 'cosine', halfvec_l2_ops: 'l2', halfvec_ip_ops: 'dot',
};
const UNITS = { ms: 1, s: 1e3, m: 6e4, h: 36e5, d: 864e5 };

function name(n, what) {
  if (typeof n !== 'string' || !IDENT.test(n)) {
    throw new FenecError(`${what} name ${JSON.stringify(n)} is not a FenecQL name`);
  }
  return n;
}

/** A builder with some of its state changed: each step makes a new one, as Drizzle's do. */
const copy = (of, patch) => Object.assign(Object.create(Object.getPrototypeOf(of)), of, patch);

const nameOf = (t) => (t && typeof t === 'object' ? t.$name : t);

// ----------------------------------------------------------------- columns

/**
 * A column: a field's type, whether it is required, its collation and the
 * index `.unique()` gives it. Nullable unless `.notNull()`, as in Drizzle
 * and in FenecQL, whose `required` it is.
 */
class Column {
  constructor(type, kind) {
    this.type = type;
    this.kind = kind;
    this.required = false;
    this.collation = null;
    this.index = null;
    this.name = null;
    this.table = null;
  }
  /** `required` in FenecQL: written by every write, and never null. */
  notNull() {
    return copy(this, { required: true });
  }
  /** `@unique`: a field's one index, as `uniqueIndex().on(col)` gives it. */
  unique() {
    return copy(this, { index: { kind: 'unique' } });
  }
  /** `collate tr` or `collate und`: the order its text compares in. */
  collate(c) {
    if (!COLLATIONS.includes(c)) throw new FenecError(`unknown collation ${JSON.stringify(c)}: 'tr' or 'und'`);
    if (this.type !== 'text' && this.type !== '[text]') throw new FenecError('collate orders text: the column is not text');
    return copy(this, { collation: c });
  }
  /** `[type]`: a list of the type, as Drizzle's `.array()`. */
  array() {
    if (this.kind === 'json') throw new FenecError('a list of json is a json column holding a list');
    if (this.type.startsWith('[')) throw new FenecError('a list of lists has no FenecQL type');
    return copy(this, { type: `[${this.type}]`, kind: 'array' });
  }
  /** The type the column reads as, for TypeScript alone. */
  $type() {
    return this;
  }
  /** A vector column under one of pgvector's operator classes, for `index().using('hnsw', ...)`. */
  op(opclass) {
    if (this.kind !== 'vector') throw new FenecError('op() names a vector column\'s distance');
    const metric = OPS[opclass];
    if (!metric) throw new FenecError(`unknown operator class ${JSON.stringify(opclass)}: ${Object.keys(OPS).join(', ')}`);
    return { column: this, metric };
  }
  /** A path into a json column, `t.meta.path('source.rank')`, for an index. */
  path(keys) {
    if (this.kind !== 'json') throw new FenecError('path() reads into a json column');
    if (typeof keys !== 'string' || !keys.split('.').every((k) => IDENT.test(k))) {
      throw new FenecError(`invalid path ${JSON.stringify(keys)}`);
    }
    return { column: this, path: keys };
  }
  default() {
    throw new FenecError('FenecQL has no column defaults: a write leaves a field it does not name null');
  }
  primaryKey() {
    throw new FenecError('every document has an `id` of its own: declare no key column');
  }
  references() {
    throw new FenecError('fenecdb has no foreign keys: a relation is defineRelations(), answered by lookup');
  }
  /** The field as the description writes it. */
  toJSON() {
    const out = { name: this.name, type: this.type };
    if (this.required) out.required = true;
    if (this.collation) out.collate = this.collation;
    if (this.index) out.index = this.index;
    if (this.paths?.length) out.paths = this.paths;
    return out;
  }
}

const column = (type, kind = type) => {
  // Drizzle 1.0's builders take no name: the key is the field's name.
  return (...args) => {
    if (typeof args[0] === 'string') throw new FenecError(`the key names the field: ${kind}() takes no name`);
    return new Column(type, kind);
  };
};

const dimensions = (opts, what) => {
  const n = opts?.dimensions;
  if (!Number.isInteger(n) || n <= 0) throw new FenecError(`${what}({ dimensions }) takes a whole number past zero`);
  return n;
};

/** `text`. */
export const text = column('text');
/** `int`: a 64-bit integer, read as a number. */
export const integer = column('int');
/** `float`: an f64, PostgreSQL's double precision. */
export const doublePrecision = column('float');
/** `bool`. */
export const boolean = column('bool');
/** `timestamp`: read as ISO-8601 text; written as a Date, text or milliseconds. */
export const timestamp = column('timestamp');
/** `bytes`. */
export const bytea = column('bytes');
/** `bytes`, by fenecdb's own name. */
export const bytes = bytea;
/** `json`: any value JSON holds. */
export const json = column('json');
/** `json`: fenecdb has one json type, which is jsonb over the wire. */
export const jsonb = json;
/** `vector<N>`, pgvector's `vector`. */
export const vector = (opts) => new Column(`vector<${dimensions(opts, 'vector')}>`, 'vector');
/** `vector<N, f16>`, pgvector's `halfvec`: half the memory a component. */
export const halfvec = (opts) => new Column(`vector<${dimensions(opts, 'halfvec')}, f16>`, 'vector');
/** `sparse<N>`, pgvector's `sparsevec`. */
export const sparsevec = (opts) => new Column(`sparse<${dimensions(opts, 'sparsevec')}>`, 'sparse');
/** `geo`: a point, `[lon, lat]` in degrees. */
export const geo = column('geo');
/**
 * `geo`, as Drizzle declares a PostGIS point: `geometry({ type: 'point' })`,
 * its `mode` `'tuple'` and its `srid` 4326 if given -- a point read as
 * `[lon, lat]`, in degrees.
 */
export const geometry = (opts) => {
  const { type, mode = 'tuple', srid = 4326, ...rest } = opts ?? {};
  if (type !== 'point' || mode !== 'tuple' || srid !== 4326 || Object.keys(rest).length) {
    throw new FenecError("a geo column holds points, [lon, lat] in degrees: geometry({ type: 'point' })");
  }
  return new Column('geo', 'geo');
};

// ----------------------------------------------------------------- indexes

/**
 * An index, Drizzle's way: `index('name').on(t.col)` is `@sorted` (a btree
 * in PostgreSQL), `.using('hash', t.col)` `@hash`, `.using('hnsw',
 * t.embed.op('vector_cosine_ops'))` `@hnsw`, and fenecdb's own
 * `.using('bm25', t.body)` `@text`, `.using('inverted', t.splade)`
 * `@inverted` and `.using('gist', t.loc)` `@geo`, PostGIS's index of a point;
 * `.with({...})` takes their options and `.ttl('30d')` makes an
 * ordered index's rows expire. fenecdb does not name its indexes: a name is
 * checked to be the table's one of it, and kept nowhere.
 */
class Index {
  constructor(name, unique) {
    if (name !== undefined && typeof name !== 'string') throw new FenecError('an index\'s name is text');
    this.indexName = name ?? null;
    this.unique = unique;
    this.method = null;
    this.target = null;
    this.options = {};
    this.expiry = null;
  }
  on(...targets) {
    if (targets.length !== 1) throw new FenecError('a fenecdb index is on one field');
    return copy(this, { method: this.unique ? 'hash' : 'btree', target: targets[0] });
  }
  using(method, ...targets) {
    if (!['btree', 'hash', 'hnsw', 'bm25', 'inverted', 'gist'].includes(method)) {
      throw new FenecError(`unknown index method ${JSON.stringify(method)}: btree, hash, hnsw, bm25, inverted or gist`);
    }
    if (targets.length !== 1) throw new FenecError('a fenecdb index is on one field');
    if (this.unique && method !== 'hash' && method !== 'btree') throw new FenecError('a unique index is a hash');
    return copy(this, { method, target: targets[0] });
  }
  with(options) {
    return copy(this, { options: { ...this.options, ...options } });
  }
  /** `@ttl`: a row is gone this long after its timestamp, `'30m'`, `'7d'`. */
  ttl(duration) {
    const m = /^(\d+)(ms|s|m|h|d)$/.exec(String(duration));
    if (!m || Number(m[1]) <= 0) throw new FenecError(`a ttl is a duration such as '30m' or '7d', not ${JSON.stringify(duration)}`);
    return copy(this, { expiry: Number(m[1]) * UNITS[m[2]] });
  }
  /** The index as the description writes it, and the field or path it is on. */
  resolve() {
    const t = this.target;
    if (!t) throw new FenecError(`index ${this.indexName ?? ''} is on no column: .on(t.col) or .using(method, t.col)`);
    const col = t instanceof Column ? t : t.column;
    if (!(col instanceof Column)) throw new FenecError('an index is on a column of the table');
    const path = t instanceof Column ? null : (t.path ?? null);
    const opts = Object.keys(this.options);
    const allowed = { hnsw: ['m', 'ef_construction', 'ef_search', 'quant'], bm25: ['k1', 'b', 'prefix', 'prefix_min', 'chars'] }[this.method] ?? [];
    const stray = opts.find((k) => !allowed.includes(k));
    if (stray) throw new FenecError(`${this.method} indexes take no ${stray}`);
    let index;
    if (this.unique) index = { kind: 'unique' };
    else if (this.method === 'btree') index = this.expiry ? { kind: 'ttl', ms: this.expiry } : { kind: 'sorted' };
    else if (this.method === 'hash') index = { kind: 'hash' };
    else if (this.method === 'inverted') index = { kind: 'inverted' };
    else if (this.method === 'gist') index = { kind: 'geo' };
    else if (this.method === 'hnsw') {
      index = { kind: 'hnsw', metric: t.metric ?? 'cosine' };
      for (const k of allowed) if (this.options[k] !== undefined) index[k] = this.options[k];
    } else {
      index = { kind: 'text' };
      for (const k of allowed) if (this.options[k] !== undefined) index[k] = this.options[k];
    }
    if (this.expiry && index.kind !== 'ttl') throw new FenecError('ttl() expires the rows of an ordered index: index().on(t.at).ttl(...)');
    // What each method indexes, as the engine has it: refused here, where it is declared.
    const takes = { ttl: ['timestamp'], hnsw: ['vector'], text: ['text'], inverted: ['sparse'], geo: ['geo'] }[index.kind];
    if (path === null && takes && !takes.includes(col.kind)) {
      throw new FenecError(`${col.table}.${col.name} is ${col.type}: ${this.expiry ? 'ttl() expires a timestamp' : `${this.method} indexes ${takes[0]}`}`);
    }
    return { col, path, index };
  }
}

/** An index: `@sorted` by `.on(col)`, the others by `.using(method, col)`. */
export const index = (name) => new Index(name, false);
/** A unique index: `@unique`. */
export const uniqueIndex = (name) => new Index(name, true);

// ------------------------------------------------------------------ tables

const TABLE = Symbol('fenecdb table');

/**
 * A collection: `fenecTable(name, columns, (t) => [indexes])`, as
 * `pgTable` declares a table. The table holds its columns by name; `id` is
 * every document's own and is not declared.
 */
export function fenecTable(tableName, columns, extra) {
  const tname = name(tableName, 'a table');
  const table = Object.create(Table.prototype);
  if (!columns || typeof columns !== 'object' || !Object.keys(columns).length) {
    throw new FenecError(`${tname} declares no column`);
  }
  for (const [key, c] of Object.entries(columns)) {
    if (!(c instanceof Column)) throw new FenecError(`${tname}.${key} is not a column: text(), integer() ...`);
    if (key === 'toJSON') throw new FenecError(`${tname}.toJSON: the name is the table's own`);
    if (key === 'id') throw new FenecError(`${tname}.id: every document has an \`id\` of its own; declare no key column`);
    name(key, 'a column');
    // Each table its own column, so one declared once may serve two.
    table[key] = Object.assign(Object.create(Column.prototype), c, { name: key, table: tname, paths: [] });
  }
  const named = new Set();
  for (const ix of extra?.(table) ?? []) {
    if (!(ix instanceof Index)) throw new FenecError(`${tname}: the third argument returns indexes`);
    if (ix.indexName !== null) {
      if (named.has(ix.indexName)) throw new FenecError(`${tname} names two indexes ${ix.indexName}`);
      named.add(ix.indexName);
    }
    const { col, path, index: kind } = ix.resolve();
    if (table[col.name] !== col) throw new FenecError(`${tname}: index ${ix.indexName ?? ''} is on another table's column`);
    if (path === null) {
      if (col.index) {
        throw new FenecError(`${tname}.${col.name} has two indexes: fenecdb takes one a field (${col.index.kind} and ${kind.kind})`);
      }
      col.index = kind;
    } else {
      if (col.paths.some((p) => p.path === path)) throw new FenecError(`${tname}.${col.name}.${path} has two indexes`);
      col.paths.push({ path, index: kind });
    }
  }
  Object.defineProperties(table, {
    $name: { value: tname },
    [TABLE]: { value: true },
  });
  return Object.freeze(table);
}

/** `fenecTable` by fenecdb's own word. */
export const collection = fenecTable;

class Table {
  /** The collection as the description writes it. */
  toJSON() {
    return { name: this.$name, fields: Object.values(this).map((c) => c.toJSON()) };
  }
  /**
   * The collection as FenecQL writes it -- what a `schema.fenecql` file
   * holds, and what an open hands the engine: `create collection`, then a
   * `create index` for each path. It reads back as the description does.
   */
  toFenecQL() {
    const cols = Object.values(this);
    const fields = cols.map((c) => {
      let f = `${c.name} ${c.type}`;
      if (c.required) f += ' required';
      if (c.collation) f += ` collate ${c.collation}`;
      if (c.index) f += ` ${indexText(c.index)}`;
      return f;
    });
    const lines = [`create collection ${this.$name} (${fields.join(', ')})`];
    for (const c of cols) for (const p of c.paths) lines.push(`create index on ${this.$name} (${c.name}.${p.path}) ${indexText(p.index)}`);
    return lines.join('\n');
  }
}

/** An index as FenecQL declares it: what the declaration said, the engine's defaults for the rest. */
function indexText(ix) {
  const opts = (keys) => keys.filter((k) => ix[k] !== undefined).map((k) => (k === 'chars' ? (ix[k] ? 'chars' : null) : `${k}=${ix[k]}`)).filter(Boolean);
  switch (ix.kind) {
    case 'ttl': {
      const unit = Object.entries(UNITS).reverse().find(([, n]) => ix.ms % n === 0);
      return `@ttl(${ix.ms / unit[1]}${unit[0]})`;
    }
    case 'hnsw':
      return `@hnsw(${[ix.metric, ...opts(['m', 'ef_construction', 'ef_search', 'quant'])].join(', ')})`;
    case 'text': {
      const o = opts(['k1', 'b', 'prefix', 'prefix_min', 'chars']);
      return o.length ? `@text(${o.join(', ')})` : '@text';
    }
    default:
      return `@${ix.kind}`;
  }
}

/**
 * A schema as FenecQL, `schema.fenecql`: every table's `create` statements.
 * The form a schema written by hand takes, and the one `Fenec.open`'s
 * `schema` takes as text.
 */
export function toFenecQL(schema) {
  return Object.values(schema).map((t) => t.toFenecQL()).join('\n');
}

/** The table's columns by name, as Drizzle's `getColumns`. */
export function getColumns(table) {
  return { ...table };
}

/**
 * The description of `schema` -- tables by any key -- and its migrations:
 * what `Fenec.checkSchema` and `POST /_schema/...` take.
 */
export function describe(schema, migrations = []) {
  const collections = Object.values(schema ?? {});
  for (const t of collections) if (!t?.[TABLE]) throw new FenecError('a schema is tables: { todos, reviews }');
  return { format: FORMAT, collections: collections.map((t) => t.toJSON()), migrations: migrations.map(migration) };
}

// -------------------------------------------------------------- migrations

function migration(m) {
  if (typeof m === 'string' && m.trim()) return m;
  if (m && typeof m === 'object' && m.rebuild) return m;
  throw new FenecError('a migration is FenecQL text, or rename(), drop(), dropTable() or rebuild()');
}

/** `alter collection <t> rename field <from> to <to>`: a field renamed, its values kept. */
export const rename = (table, from, to) =>
  `alter collection ${name(nameOf(table), 'a table')} rename field ${name(from, 'a column')} to ${name(to, 'a column')}`;
/** `alter collection <t> drop field <f>`: a field and its values gone. */
export const drop = (table, field) => `alter collection ${name(nameOf(table), 'a table')} drop field ${name(field, 'a column')}`;
/** `drop collection <t>`: a collection and its documents gone. */
export const dropTable = (table) => `drop collection ${name(nameOf(table), 'a table')}`;
/**
 * A field made again as the code declares it -- its index, its collation --
 * and its values copied: what changes an index, which nothing changes in
 * place. Written out against the declaration by the engine.
 */
export const rebuild = (table, field) => ({ rebuild: { collection: name(nameOf(table), 'a table'), field: name(field, 'a column') } });

// ---------------------------------------------------------------- relations

/**
 * Drizzle 1.0's `defineRelations`, answered by `lookup`: a relation is
 * `on <to> = <from>`, so `to` is the child's key -- `id`, or a field with a
 * hash or unique index -- and a relation `lookup` cannot answer is refused
 * here. `many` attaches every child, `one` the first.
 *
 *   const relations = defineRelations({ products, reviews }, (r) => ({
 *     products: { reviews: r.many.reviews({ from: r.products.id, to: r.reviews.productId }) },
 *   }));
 */
export function defineRelations(tables, fn) {
  const byKey = {};
  for (const [key, t] of Object.entries(tables)) {
    if (!t?.[TABLE]) throw new FenecError(`${key} is not a table`);
    byKey[key] = t;
  }
  const r = { many: {}, one: {} };
  for (const [key, t] of Object.entries(byKey)) {
    r[key] = { id: { table: t, name: 'id' } };
    for (const c of Object.values(t)) r[key][c.name] = { table: t, name: c.name };
    for (const kind of ['many', 'one']) r[kind][key] = (spec) => relation(t, kind, spec);
  }
  const out = {};
  for (const [key, rels] of Object.entries(fn(r) ?? {})) {
    const from = byKey[key];
    if (!from) throw new FenecError(`relations of ${key}, which is not among the tables`);
    out[from.$name] = {};
    for (const [rel, made] of Object.entries(rels)) {
      if (made.from.table !== from) throw new FenecError(`${key}.${rel}: from is a column of ${key}`);
      out[from.$name][rel] = made.lookup;
    }
  }
  return Object.freeze(out);
}

function relation(target, kind, spec) {
  const { from, to } = spec ?? {};
  if (!from?.table || !to?.table || Array.isArray(from) || Array.isArray(to)) {
    throw new FenecError('a relation is { from: r.a.col, to: r.b.col }: one column each, as lookup takes one key');
  }
  if (to.table !== target) throw new FenecError(`a relation to ${target.$name} reaches it by one of its columns`);
  const key = to.name === 'id' || ['hash', 'unique'].includes(target[to.name]?.index?.kind);
  if (!key) {
    throw new FenecError(
      `${target.$name}.${to.name} has no hash or unique index: lookup finds children by \`id\` or by one, never by a scan`,
    );
  }
  const lookup = { collection: target.$name, on: to.name };
  if (from.name !== 'id') lookup.parentKey = from.name;
  if (kind === 'one') lookup.limit = 1;
  return { from, lookup: Object.freeze(lookup) };
}
