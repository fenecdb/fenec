// fenecdb's query builder, its error and the schema check: what every
// way of reaching a database shares -- the database in the page
// (`fenec.js`), a server over HTTP (`http.js`, and `client.js`, the entry
// that leaves the engine out) and a synced replica.
//
// No dependencies, no build step. `@fenecdb/web` and `@fenecdb/web/client`
// export what a page uses of it; the rest of its exports are for the
// modules beside it.

// How many `lookup` levels one query may chain. The engine refuses a deeper
// one; failing here never sends the query.
const MAX_LOOKUP_DEPTH = 8;

export class FenecError extends Error {}

/**
 * What each database, endpoint and replica was opened with when its code
 * declares its schema (`@fenecdb/web/schema`): the tables, the relations
 * `lookup` names, the migrations.
 */
export const declared = new WeakMap();

/**
 * The code's schema checked against `to` -- the database in the page or
 * the server's -- and applied where `how` says (`apply`; `follow` compares a
 * schema another owns). The schema is FenecQL text, or tables declared in
 * code (`@fenecdb/web/schema`), which write themselves as it: this module
 * imports none of that one. The engine decides; a refusal throws, naming
 * every difference and how to resolve it (`refusals`).
 */
export async function checked(to, opts, how) {
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

/** A collection's name, or a table's (`fenecTable`). */
export const nameOf = (n) => (n && typeof n === 'object' ? n.$name : n);

/**
 * A response's rows, with what `facet` counted beside them as `facets`
 * when the query asked for any: the array a page renders and its sidebar,
 * through `rows()`, a live query and `useLiveQuery` alike.
 */
export function rowsOf(res) {
  const rows = res?.rows ?? [];
  if (res?.facets) rows.facets = res.facets;
  return rows;
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

export function collation(name) {
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

export function ident(name, what = 'field') {
  if (typeof name !== 'string' || !IDENT.test(name)) {
    throw new FenecError(`invalid ${what} name: ${JSON.stringify(name)}`);
  }
  return name;
}

/** A field's name, or a path into a json field: `'meta.lang'`. */
export function path(name) {
  if (typeof name !== 'string' || !PATH.test(name)) {
    throw new FenecError(`invalid field name: ${JSON.stringify(name)}`);
  }
  return name;
}

/**
 * A select-list item: a field, or an aggregate of one spelled as FenecQL
 * spells it -- `count(*)`, `count(distinct user)`, `sum(total)`, `avg(f)`,
 * `min(f)`, `max(f)`, `first(f)`, `last(f)`, a path where a field goes --
 * which answers under that same name.
 */
const NAMES = '[A-Za-z_][A-Za-z0-9_]*(?:\\.[A-Za-z_][A-Za-z0-9_]*)*';
const AGGREGATE = new RegExp(
  `^(count)\\(\\*?\\)$|^count\\(\\s*distinct\\s+(${NAMES})\\s*\\)$|^(sum|avg|min|max|first|last)\\((${NAMES})\\)$`,
  'i',
);
// What makes an expression an aggregate's: a call of one.
const AGGREGATE_CALL = /\b(count|sum|avg|min|max|first|last)\s*\(/i;

function column(name) {
  if (name !== null && typeof name === 'object' && name[EXPR] === 'expr') {
    return { node: name, aggregate: AGGREGATE_CALL.test(name.sql) };
  }
  const m = typeof name === 'string' ? AGGREGATE.exec(name.trim()) : null;
  if (!m) return { text: path(name), aggregate: false };
  const text = m[1]
    ? 'count(*)'
    : m[2]
      ? `count(distinct ${m[2]})`
      : `${m[3].toLowerCase()}(${m[4]})`;
  return { text, aggregate: true };
}

/** A select item or a group key as text, an expression's values bound. */
function columnText(c, bind) {
  if (c.text !== undefined) return c.text;
  const text = value('select', c.node, bind, null);
  return c.node.name ? `${text} as ${c.node.name}` : text;
}

/** `limit`, `offset`, `ef` cannot be parameterised: FenecQL wants a literal. */
export function whole(n, what) {
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
export function normalize(v, what = 'value') {
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

export function isSpec(v) {
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

// ------------------------------------------------------- values that compute

// What `inc` and `expr` make: a value worked out over the row a `set`
// writes, rendered as FenecQL with its values as parameters. Marked as a
// condition node is, so a json field's object value is never taken for one.
const EXPR = Symbol('fenec.expr');

/**
 * What `expr()`, `bucket()`, `countDistinct()`, `first()` and `last()`
 * make: an expression, which `as` names as a column of a select list --
 * `bucket('at', '1m').as('minute')`, `expr('px * qty').as('notional')`.
 */
class Computed {
  constructor(sql, params, name) {
    this[EXPR] = 'expr';
    this.sql = sql;
    this.params = params;
    if (name !== undefined) this.name = name;
  }

  /** The name the column answers under: `select ... as <name>`. */
  as(name) {
    return new Computed(this.sql, this.params, ident(name, 'column'));
  }
}

/**
 * `{ n: inc(1) }` in an update: the field plus `by`, counting from 0 where
 * it is null -- `n: coalesce(n, 0) + $1` -- worked out under the server's
 * write lock, so increments from many clients all land.
 */
export function inc(by = 1) {
  if (typeof by !== 'number' || !Number.isFinite(by)) {
    throw new FenecError(`inc() takes a number: ${JSON.stringify(by)}`);
  }
  return { [EXPR]: 'inc', by };
}

/**
 * A value as a FenecQL expression over the row it is written into, `?`s
 * bound to the parameters in order: `{ at: expr('now()') }`,
 * `{ total: expr('price * ?', 1.2) }`, `{ entry: expr('? + ":dr"', ref) }`
 * (`+` joins two texts, and only two texts).
 */
export function expr(sql, ...params) {
  if (typeof sql !== 'string') throw new FenecError('expr() expects text');
  return new Computed(sql, params);
}

// `15m`, `1h`, `1d`, `1w`, `3mo`, `1y`: what `bucket` takes, written into
// the text as a literal, since it is a part of the statement's shape.
const INTERVAL = /^[1-9][0-9]*(ms|s|m|h|d|w|mo|y)$/;

/**
 * `bucket(field, interval)`: the start of the interval a timestamp falls
 * in -- `'15m'`, `'1h'`, `'1d'`, `'1w'` (from a Monday), `'1mo'`, `'1y'`,
 * in UTC -- for a select list or a group:
 *
 *   from('ticks').select(bucket('at', '1m').as('minute'), 'count(*)').group('minute')
 */
export function bucket(field, interval) {
  if (typeof interval !== 'string' || !INTERVAL.test(interval)) {
    throw new FenecError(
      `bucket() takes an interval such as '15m', '1h', '1d', '1w' or '1mo': ${JSON.stringify(interval)}`,
    );
  }
  return new Computed(`bucket(${path(field)}, ${interval})`, []);
}

/** `count(distinct field)`: how many distinct values the rows hold. */
export function countDistinct(field) {
  return new Computed(`count(distinct ${path(field)})`, []);
}

/**
 * `first(field)`, or `first(field by key)`: the value of the row least by
 * `key` -- by the order the rows were written without one -- that has a
 * value; a bar's open is `first('px', 'at')`.
 */
export function first(field, by) {
  return pick('first', field, by);
}

/** `last(field [by key])`: as `first`, the row greatest by `key`. */
export function last(field, by) {
  return pick('last', field, by);
}

function pick(fn, field, by) {
  return new Computed(`${fn}(${path(field)}${by === undefined ? '' : ` by ${path(by)}`})`, []);
}

/** A document's value: `inc`'s and `expr`'s text, or a parameter. */
function value(k, v, bind, write) {
  if (!(v !== null && typeof v === 'object' && v[EXPR])) return bind(v, k);
  if (v[EXPR] === 'inc') {
    if (write === 'insert') {
      throw new FenecError(`inc() reads the row it changes: use it in update (field: ${k})`);
    }
    return `coalesce(${k}, 0) + ${bind(v.by, k)}`;
  }
  let i = 0;
  const out = v.sql.replace(/\?/g, () => {
    if (i >= v.params.length) {
      throw new FenecError('expr(): more `?` placeholders than parameters');
    }
    return bind(v.params[i++], k);
  });
  if (i !== v.params.length) throw new FenecError('expr(): too many parameters given');
  return out;
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
export function render(c, bind, parent = null) {
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
    this.#s = { cond: [], order: [], offset: 0, lookups: [], marks: [], facets: [], ...state };
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
   * that name; an expression -- `expr()`, `bucket()`, `countDistinct()`,
   * `first()`, `last()` -- answers under the name its `as` gives it:
   *
   *   db.from('orders').select('status', 'count(*)', 'sum(total)').group('status')
   *   db.from('ticks').select('sym', expr('sum(px * qty) / sum(qty)').as('vwap')).group('sym')
   */
  select(...cols) {
    const flat = cols.flat();
    if (flat.length === 0 || flat.includes('*')) {
      return this.#with({ project: null, aggregate: false });
    }
    const list = flat.map((c) => column(c));
    return this.#with({ project: list, aggregate: list.some((c) => c.aggregate) });
  }

  /**
   * `highlight(field)` in the select list: where the terms `match` found
   * stand in the field's text -- `[start, end]` pairs of UTF-16 offsets, a
   * JavaScript string's own -- or, given `{ pre, post }`, the text with
   * each mark between them. The text is not escaped: a page that renders it
   * as HTML builds it from the offsets, or escapes it first. Answers under
   * `highlight(field)`, after the fields `select` named.
   *
   *   db.from('docs').select('title').highlight('body', { pre: '<mark>', post: '</mark>' })
   *     .match('body', text)
   */
  highlight(field, opts = {}) {
    return this.#mark({ field: ident(field), words: null, ...tags(opts, 'highlight') });
  }

  /**
   * `snippet(field, words)`: the window of `words` words around the densest
   * marks, `{ text, marks }` -- or the marked text, given `{ pre, post }` --
   * with `ellipsis` where it leaves text out. Answers under
   * `snippet(field)`.
   */
  snippet(field, words, opts = {}) {
    const mark = { field: ident(field), words: whole(words, 'snippet words'), ...tags(opts, 'snippet') };
    if (mark.words === 0) throw new FenecError('snippet shows at least one word');
    if (opts.ellipsis !== undefined) mark.ellipsis = text(opts.ellipsis, 'snippet ellipsis');
    return this.#mark(mark);
  }

  #mark(mark) {
    // Each answers under its label, and a row holds a name once.
    const kind = (m) => (m.words === null ? 'highlight' : 'snippet');
    if (this.#s.marks.some((m) => kind(m) === kind(mark) && m.field === mark.field)) {
      throw new FenecError(`${kind(mark)}(${mark.field}) is asked twice`);
    }
    return this.#with({ marks: [...this.#s.marks, mark] });
  }

  /**
   * `facet field [top N]`: each value the field holds over every row the
   * query matches -- not only the page -- and how many rows hold it, most
   * first; `top` keeps the commonest. A list counts once a row for each
   * value. The counts come back beside the rows: `run()`'s `facets`, and
   * `rows().facets`.
   *
   *   db.from('products').match('title', 'phone').where('price', '<', 500)
   *     .facet('brand', { top: 10 }).facet('color').limit(20)
   *
   * `{ ranges: [0, 2500, 5000] }` counts the rows whose number falls in
   * each range -- from a bound, included, to the next, excluded -- every
   * range in order, `value` its `[from, to]`. `{ disjunctive: true }`
   * counts over the rows the query selects with the filter's own
   * conditions on the field left out, so a brand chosen still lists the
   * other brands to add.
   */
  facet(field, opts = {}) {
    const f = { field: path(field), top: opts.top === undefined ? null : whole(opts.top, 'facet top') };
    if (f.top === 0) throw new FenecError(`facet ${f.field} top 0 answers nothing`);
    if (this.#s.facets.some((g) => g.field === f.field)) {
      throw new FenecError(`facet ${f.field} is asked twice`);
    }
    if (opts.ranges !== undefined) {
      const r = opts.ranges;
      // The engine's rule, refused before anything is sent.
      if (
        f.top !== null ||
        !Array.isArray(r) ||
        r.length < 2 ||
        r.some((b, i) => typeof b !== 'number' || !Number.isFinite(b) || (i > 0 && !(b > r[i - 1])))
      ) {
        throw new FenecError(
          `facet ${f.field} ranges takes 2 to 10 001 numbers, each above the one before, and no top: every range answers, in order`,
        );
      }
      f.ranges = r.map(String);
    }
    if (opts.disjunctive !== undefined && typeof opts.disjunctive !== 'boolean') {
      throw new FenecError(`facet ${f.field} disjunctive is true or false`);
    }
    f.disjunctive = opts.disjunctive === true;
    return this.#with({ facets: [...this.#s.facets, f] });
  }

  /**
   * `group a, b` -- one row per distinct set of the keys' values, for a
   * select list that aggregates. A key is a field or a path, a name the
   * list gives a column with `as`, or an expression: `bucket('at', '1h')`.
   */
  group(...keys) {
    const flat = keys.flat();
    if (flat.length === 0) throw new FenecError('group takes at least one key');
    return this.#with({
      group: flat.map((k) => (k !== null && typeof k === 'object' && k[EXPR] === 'expr'
        ? { node: k }
        : { text: path(k) })),
    });
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
        { field: column(field).text ?? path(field), asc: direction(dir), collate: collation(collate) },
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
   * `require n`: the rows the query answers -- after `limit` -- must number
   * `n`, or it is refused (412, `unmet`) and the batch it is in put back,
   * as a write's `{ require: n }` is. A checkout's guard on a read:
   *
   *   from('products').where({ sku, price }).require(1)    // still that price
   *   from('coupons').where('code', c).limit(1).require(1)  // one exists
   */
  require(n) {
    return this.#with({ require: requireClause({ require: n }) });
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
    const { project, count, lookups, marks, facets } = this.#s;
    // One column is the list; the engine refuses the rest too.
    if (!project || project.length !== 1 || count || lookups.length || marks.length || facets.length) {
      throw new FenecError(
        `an inner query of \`in\` selects exactly one column: from('${this.#s.collection}').select('id')`,
      );
    }
    if (this.#s.require) {
      throw new FenecError('an inner query of `in` takes no require: the query around it does');
    }
    return this.#text(bind);
  }

  #text(bind) {
    const { collection, project, near, order, limit, offset, count } = this.#s;
    const { match, rerank, lookups, aggregate, group, fuse, marks, facets } = this.#s;
    // The engine refuses these too; failing here never sends a query.
    if (group && !aggregate) {
      const keys = group.map((k) => k.text ?? k.node.sql).join(', ');
      throw new FenecError(`group ${keys} needs an aggregate in select: 'count(*)'`);
    }
    if (aggregate) {
      const clash = near ? 'near' : match ? 'match' : lookups.length ? 'lookup' : count ? 'count' : null;
      if (clash) throw new FenecError(`aggregates cannot be combined with ${clash}`);
      if (!group && (order.length || limit !== undefined || offset)) {
        throw new FenecError('aggregates answer one row; group makes a row per value');
      }
      if (!group && this.#s.require) {
        throw new FenecError('require counts the rows a query answers, and an aggregate answers one');
      }
    }
    if (marks.length) {
      const what = marks[0].words === null ? 'highlight' : 'snippet';
      if (!match) throw new FenecError(`${what} needs match: it marks the terms match found`);
      if (aggregate) throw new FenecError(`${what} marks a row's text; aggregates answer groups`);
    }
    if (facets.length) {
      if (near) throw new FenecError('facet counts the rows a filter or match selects, and near ranks every row: ask the facets without near');
      if (aggregate) throw new FenecError('facet cannot be combined with aggregates: group counts by value');
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
    // The marks after the fields `select` named, or after every field.
    const items = marks.map((m) => {
      let s = `${m.words === null ? 'highlight' : 'snippet'}(${m.field}`;
      if (m.words !== null) s += `, ${m.words}`;
      if (m.ellipsis !== undefined || (m.words !== null && m.pre !== undefined)) {
        s += `, ${bind(m.ellipsis ?? '', 'snippet')}`;
      }
      if (m.pre !== undefined) s += `, ${bind(m.pre, m.field)}, ${bind(m.post, m.field)}`;
      return `${s})`;
    });
    // The list's values are bound before the filter's, as they come before it
    // in the text (the marks' tags were bound above).
    const cols = project?.map((c) => columnText(c, bind));
    if (project || items.length) sql += ` select ${[...(cols ?? ['*']), ...items].join(', ')}`;
    const where = this.#where(bind);
    if (where) sql += ` where ${where}`;
    if (group) sql += ` group ${group.map((k) => columnText(k, bind)).join(', ')}`;
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
    // Before a lookup, whose clauses are the children's.
    if (this.#s.require) sql += this.#s.require;
    if (count) sql += ' count';
    if (facets.length) {
      const one = (f) =>
        f.field +
        (f.top === null ? '' : ` top ${f.top}`) +
        (f.ranges ? ` ranges [${f.ranges.join(', ')}]` : '') +
        (f.disjunctive ? ' disjunctive' : '');
      sql += ` facet ${facets.map(one).join(', ')}`;
    }
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

  /** The raw response (`{columns, rows}`, and `facets` when asked). */
  async run() {
    const [sql, params] = this.toFenecQL();
    return this.#exec(sql, params);
  }

  /**
   * Rows: an array of objects keyed by field name -- and, when the query
   * asked for facets, the counts as its `facets`.
   */
  async rows() {
    return rowsOf(await this.run());
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

  /**
   * The `put` text. `{ ifAbsent: true }`: `put ... if absent`, which passes
   * over a document whose id or `@unique` value a row holds, and counts
   * only what it wrote -- a lock taken, or not, in one statement.
   * `{ require: n }` on any write: `... require n`, refused (412) and its
   * batch put back unless it wrote exactly `n` rows.
   */
  toInsert(docs, opts = {}) {
    this.#assertPlain('insert');
    const list = Array.isArray(docs) ? docs : [docs];
    if (list.length === 0) throw new FenecError('cannot write an empty document list');
    const params = [];
    const bind = binder(params);
    const body = list.map((d) => renderDoc(d, bind, 'insert')).join(', ');
    const absent = opts?.ifAbsent === true ? ' if absent' : '';
    const required = requireClause(opts);
    return [`put ${this.#s.collection} ${list.length === 1 ? body : `[${body}]`}${absent}${required}`, params];
  }

  /** The `set` text. A value may be `inc(n)` or `expr(text, ...params)`. */
  toUpdate(patch, opts = {}) {
    this.#assertPlain('update');
    const params = [];
    const bind = binder(params);
    const body = renderDoc(patch, bind, 'update');
    const where = this.#requireFilter('update', opts, bind);
    return [`set ${this.#s.collection} ${body}${where}${requireClause(opts)}`, params];
  }

  /**
   * The upsert's text, `put ... if absent else set {patch}`: a document
   * whose id or first `@unique` value a row holds sets that row by the
   * patch, every other one is inserted. The patch is an update's: a value
   * may be `inc(n)` or `expr(text, ...params)`, and in an expression
   * `new.f` reads the document's own `f` -- so a counter is one statement,
   * not a read, a choice and a write that two clients can interleave:
   *
   *   hits.upsert({ key, n: 1 }, { n: expr('n + new.n') })
   *   hits.upsert({ key, n: 1 }, { n: inc(1) })
   *
   * The documents' parameters are numbered first, then the patch's.
   */
  toUpsert(docs, patch, opts = {}) {
    this.#assertPlain('upsert');
    const list = Array.isArray(docs) ? docs : [docs];
    if (list.length === 0) throw new FenecError('cannot write an empty document list');
    const params = [];
    const bind = binder(params);
    const body = list.map((d) => renderDoc(d, bind, 'insert')).join(', ');
    const set = renderDoc(patch, bind, 'update');
    const required = requireClause(opts);
    return [`put ${this.#s.collection} ${list.length === 1 ? body : `[${body}]`} if absent else set ${set}${required}`, params];
  }

  /** The `del` text. */
  toDelete(opts = {}) {
    this.#assertPlain('delete');
    const params = [];
    const bind = binder(params);
    const where = this.#requireFilter('delete', opts, bind);
    return [`del ${this.#s.collection}${where}${requireClause(opts)}`, params];
  }

  /**
   * `put` -- a single document or an array. Returns: documents written,
   * which with `{ ifAbsent: true }` leaves out those already held.
   */
  async insert(docs, opts = {}) {
    const list = Array.isArray(docs) ? docs : [docs];
    if (list.length === 0) return 0;
    return (await this.#exec(...this.toInsert(list, opts))).count ?? 0;
  }

  /**
   * `put ... if absent else set` -- a single document or an array, each
   * inserted or, where a row holds its id or `@unique` value, that row set
   * by the patch. Returns: rows set and made together.
   */
  async upsert(docs, patch, opts = {}) {
    const list = Array.isArray(docs) ? docs : [docs];
    if (list.length === 0) return 0;
    return (await this.#exec(...this.toUpsert(list, patch, opts))).count ?? 0;
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
    if (this.#s.require) {
      throw new FenecError(`${verb} takes require as its option: ${verb}(..., { require: n })`);
    }
    const extra = this.#extraClause();
    if (extra) throw new FenecError(`${verb} cannot be used with \`${extra}\``);
    // Not among the read clauses `count` refuses, since a required lookup
    // decides what a count counts; a write has no use for one, and left
    // out, `.lookup(...).delete()` deleted every parent it filtered.
    if (this.#s.lookups.length) throw new FenecError(`${verb} cannot be used with \`lookup\``);
    if (this.#s.facets.length) throw new FenecError(`${verb} cannot be used with \`facet\``);
    if ((verb === 'insert' || verb === 'upsert') && this.#s.cond.length) {
      throw new FenecError(`${verb} cannot be used with \`where\``);
    }
  }

  // `count` does not combine with projection, ordering or pagination: they
  // are meaningless over a count, and `near` truncates to its own ceiling.
  // The engine checks the same thing; failing here never sends the query.
  #assertCountable() {
    const extra = this.#extraClause() ?? (this.#s.require ? 'require' : null);
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

/** A mark's tags, `{ pre, post }`: both or neither, each text. */
function tags(opts, what) {
  if (opts.pre === undefined && opts.post === undefined) return {};
  if (opts.pre === undefined || opts.post === undefined) {
    throw new FenecError(`${what} takes both pre and post, or neither`);
  }
  return { pre: text(opts.pre, `${what} pre`), post: text(opts.post, `${what} post`) };
}

function text(v, what) {
  if (typeof v !== 'string') throw new FenecError(`${what} must be text: ${JSON.stringify(v)}`);
  return v;
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

// ` require n` for a write's `{ require: n }`: the rows it must write, a
// whole number from 0 -- not a parameter, as `limit` is not, so a statement
// keeps its shape.
function requireClause(opts) {
  const n = opts?.require;
  if (n === undefined || n === null) return '';
  if (typeof n !== 'number' || !Number.isSafeInteger(n) || n < 0) {
    throw new FenecError(`require takes a count of rows, a whole number from 0 (got ${String(n)})`);
  }
  return ` require ${n}`;
}

function renderDoc(doc, bind, write) {
  if (!isSpec(doc)) throw new FenecError('expected a document object');
  const pairs = Object.entries(doc)
    .filter(([, v]) => v !== undefined)
    .map(([k, v]) => `${path(k)}: ${value(k, v, bind, write)}`);
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
