// The builder's golden suite: integrations/builder-golden.json, the text
// and parameters every builder makes of the same chain of calls. The
// JavaScript builder is the reference: this file holds the chains, runs
// them through it and writes what it made (`make builder-golden`), and
// web/fenec.test.js runs every case of the file through it again, so the
// file cannot drift from it. The Python, Go and .NET builders each read
// the file in their own tests and are held to it, text and parameters
// exactly, refusals by their message.
//
// A case is a chain of steps, `{op, args}`, starting with `from` and
// ending with what turns it into a statement: `toFenecQL`, `toInsert`,
// `toUpdate`, `toDelete`, or an endpoint -- `rows`, `first`, `count`,
// `explain`, `insert`, `update`, `delete` -- whose statement is the one it
// sends. Arguments are JSON, and what JSON has no word for is an object of
// one `$` key, which no field name can start with:
//
//   {"$or": [c, ...]}, {"$and": [c, ...]}, {"$not": c}   or(), and(), not()
//   {"$raw": [text, param, ...]}                          raw()
//   {"$date": "2026-09-19T12:34:56.000Z"}                 a date
//   {"$f32": [0.5, 0.25]}                                 a Float32Array
//
// Each language names the steps its own way (`orWhere` is `or_where` in
// Python and `OrWhere` in Go and C#, `parentKey` is `parent_key` ...) and
// reads an object condition as its builder takes one; the chain is the
// same, and so must be the statement.
//
//   node web/golden.mjs           writes integrations/builder-golden.json
//   node web/golden.mjs --check   fails if the file is not what it would write

import { readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { from, or, and, not, raw, FenecError } from './fenec.js';

export const GOLDEN = fileURLToPath(new URL('../integrations/builder-golden.json', import.meta.url));

// ------------------------------------------------------------------ the run

/** A step's argument as the JS builder takes it. */
function arg(x) {
  if (Array.isArray(x)) return x.map(arg);
  if (x === null || typeof x !== 'object') return x;
  if ('$or' in x) return or(...x.$or.map(arg));
  if ('$and' in x) return and(...x.$and.map(arg));
  if ('$not' in x) return not(arg(x.$not));
  if ('$raw' in x) return raw(x.$raw[0], ...x.$raw.slice(1).map(arg));
  if ('$date' in x) return new Date(x.$date);
  if ('$f32' in x) return Float32Array.from(x.$f32);
  return Object.fromEntries(Object.entries(x).map(([k, v]) => [k, arg(v)]));
}

const TEXT = new Set(['toFenecQL', 'toInsert', 'toUpdate', 'toDelete']);
const ENDPOINTS = new Set(['rows', 'first', 'count', 'explain', 'insert', 'update', 'delete']);

// What each endpoint is answered, as the HTTP transport hands it over.
function answer(sql) {
  if (/^(put|set|del) /.test(sql)) return { kind: 'affected', count: 0 };
  if (sql.endsWith(' count')) return { rows: [{ count: 0 }] };
  return { rows: [] };
}

/** Runs one case's steps: `{text, params}`, or `{error}` for a refusal. */
export async function run(steps) {
  try {
    const [first, ...rest] = steps;
    if (first.op !== 'from') throw new Error(`a chain starts with from, not ${first.op}`);
    let q = from(...first.args.map(arg));
    for (const [i, { op, args = [] }] of rest.entries()) {
      const last = i === rest.length - 1;
      const a = args.map(arg);
      if (TEXT.has(op) || ENDPOINTS.has(op)) {
        if (!last) throw new Error(`${op} ends a chain`);
        if (TEXT.has(op)) {
          const [text, params] = q[op](...a);
          return { text, params: JSON.parse(JSON.stringify(params)) };
        }
        const sent = [];
        await q.bind((sql, params) => (sent.push([sql, params]), answer(sql)))[op](...a);
        if (sent.length !== 1) throw new Error(`${op} sent ${sent.length} statements`);
        return { text: sent[0][0], params: JSON.parse(JSON.stringify(sent[0][1])) };
      }
      if (typeof q[op] !== 'function') throw new Error(`no builder step ${op}`);
      q = q[op](...a);
    }
    throw new Error('a chain ends with a statement: toFenecQL or an endpoint');
  } catch (e) {
    if (e instanceof FenecError) return { error: e.message };
    throw e;
  }
}

// ---------------------------------------------------------------- the cases
//
// `c(name, [op, ...args], ...)`: the first step is always `from`.

const cases = [];
const c = (name, ...steps) =>
  cases.push({ name, steps: steps.map(([op, ...args]) => (args.length ? { op, args } : { op })) });
const Q = ['toFenecQL'];
const docs = ['from', 'docs'];

// Reading: the select list.
c('a bare get', docs, Q);
c('select names the fields', docs, ['select', 'title', 'year'], Q);
c('select of a star is every field', docs, ['select', '*'], Q);
c('a star among names is still every field', docs, ['select', 'title', '*'], Q);
c('select takes a path into a json field', docs, ['select', 'title', 'meta.source.site'], Q);
c('a later select replaces the earlier', docs, ['select', 'a'], ['select', 'b', 'c'], Q);

// where: each operator, as a symbol and as a word.
for (const op of ['=', '!=', '<', '<=', '>', '>=']) {
  c(`where with ${op}`, docs, ['where', 'year', op, 2024], Q);
}
c('where with ~ matches a substring', docs, ['where', 'title', '~', 'oasis'], Q);
c('where with has asks a list', docs, ['where', 'tags', 'has', 'rust'], Q);
c(
  'word operators are the symbols',
  docs,
  ['where', 'a', 'eq', 1],
  ['where', 'b', 'ne', 2],
  ['where', 'c', 'neq', 3],
  ['where', 'd', 'lt', 4],
  ['where', 'e', 'lte', 5],
  ['where', 'f', 'le', 6],
  ['where', 'g', 'gt', 7],
  ['where', 'h', 'gte', 8],
  ['where', 'i', 'ge', 9],
  ['where', 'j', 'like', 'x'],
  ['where', 'k', 'contains', 'y'],
  Q,
);
c('an unknown operator is refused', docs, ['where', 'year', 'between', 1], Q);
c('an operator is no name an object inherits', docs, ['where', 'year', 'constructor', 1], Q);
c('an operator in an object is no name an object inherits', docs, ['where', { year: { toString: 1 } }], Q);
c('two arguments are equality', docs, ['where', 'category', 'book'], Q);
c('two arguments take an operator object', docs, ['where', 'year', { gte: 2020, lt: 2030 }], Q);
c('successive where calls join with and', docs, ['where', 'a', 1], ['where', 'b', 2], Q);
c('an object condition joins its fields with and', docs, ['where', { year: { gte: 2024 }, tags: { has: 'rust' } }], Q);
c('an object condition of one field', docs, ['where', { category: 'book' }], Q);
c('an operator object takes every operator', docs, ['where', { n: { eq: 1, ne: 2, lt: 3, lte: 4, gt: 5, gte: 6, like: 'x', has: 'y' } }], Q);
c('an unknown operator in an object is refused', docs, ['where', { year: { after: 2020 } }], Q);
c('an empty operator object is refused', docs, ['where', { year: {} }], Q);
c('an empty object condition is no condition', docs, ['where', {}], Q);
c('a field called t is a field', docs, ['where', { t: 'and' }], Q);

// Values: every one a parameter, never text.
c(
  'every kind of value is a parameter',
  docs,
  ['where', 's', '=', 'çağ "quoted" \\ \n'],
  ['where', 'i', '=', -42],
  ['where', 'big', '=', 9007199254740991],
  ['where', 'f', '=', 2.5],
  ['where', 'yes', '=', true],
  ['where', 'no', '=', false],
  ['where', 'tags', '=', ['a', 'b']],
  ['where', 'meta', '=', { lang: 'tr', rank: 2, at: null }],
  Q,
);
c('a date is its ISO text', docs, ['where', 't', '>=', { $date: '2026-09-19T12:34:56.789Z' }], Q);
c('a float32 array is its numbers', docs, ['where', 'embed', '=', { $f32: [0.5, -0.25, 1] }], Q);
c('a text value is never in the text', docs, ['where', 'title', '"; del docs; --'], Q);

// in, and null.
c('in takes a list', docs, ['where', 'year', 'in', [2023, 2024]], Q);
c('in from an operator object', docs, ['where', { lang: { in: ['tr', 'en', null] } }], Q);
c('an empty in is refused', docs, ['where', 'year', 'in', []], Q);
c('in wants a list', docs, ['where', 'year', 'in', 2024], Q);
c('an object asking null is is null', docs, ['where', { summary: null }], Q);
c('= null is is null', docs, ['where', 'summary', '=', null], Q);
c('!= null is is not null', docs, ['where', 'summary', '!=', null], Q);
c('two arguments with null are is null', docs, ['where', 'summary', null], Q);
c('not null in an object is is not null', docs, ['where', { summary: { not: null } }], Q);
c('another operator with null is refused', docs, ['where', 'year', '<', null], Q);

// or, and, not, raw.
c('not wraps its condition', docs, ['where', { $not: { a: 1 } }], Q);
c('not in an operator object', docs, ['where', { year: { not: { gte: 3, lt: 9 } } }], Q);
c('an and inside an or is parenthesised', docs, ['where', { $or: [{ a: 1, b: 2 }, { c: 3 }] }], Q);
c('an or inside an and is parenthesised', docs, ['where', 'year', 2024], ['where', { $or: [{ a: 1 }, { b: 2 }] }], Q);
c('and inside or, and or inside and', docs, ['where', { $or: [{ $and: [{ a: 1 }, { $or: [{ b: 2 }, { c: 3 }] }] }, { $not: { $or: [{ d: 4 }, { e: 5 }] } }] }], Q);
c('an or of one is its condition', docs, ['where', { $or: [{ a: 1 }] }], Q);
c('orWhere covers everything before it', docs, ['where', 'a', 1], ['where', 'b', 2], ['orWhere', { c: 3 }], Q);
c('orWhere first is a where', docs, ['orWhere', 'a', '>', 1], Q);
c('orWhere with three arguments', docs, ['where', 'a', 1], ['orWhere', 'b', '<', 2], Q);
c('raw binds its placeholders in order', docs, ['where', { $raw: ['cosine(embed, ?) > ?', [0.1, 0.2], 0.5] }], Q);
c('raw beside the other conditions', docs, ['where', 'a', 1], ['where', { $raw: ['b + ? > c', 2] }], ['where', 'd', 3], Q);
c('raw with more placeholders than parameters', docs, ['where', { $raw: ['a = ? and b = ?', 1] }], Q);
c('raw with more parameters than placeholders', docs, ['where', { $raw: ['a = ?', 1, 2] }], Q);

// Names: the injection boundary.
c('a path into a json field goes where a field does', docs, ['select', 'title', 'meta.source.site'], ['where', 'meta.lang', '=', 'tr'], ['where', { 'meta.source.rank': { gte: 2 } }], ['order', 'meta.source.rank', 'desc'], Q);
c('a collection name with a statement after it', ['from', 't; drop collection x'], Q);
c('a collection is no path', ['from', 'a.b'], Q);
c('a select item holding two', docs, ['select', 'a, b'], Q);
c('a where field holding a condition', docs, ['where', 'a or 1=1', 1], Q);
for (const bad of ['meta.', '.meta', 'meta..lang', 'meta.1x', '1x', 'a b', '']) {
  c(`the field name ${JSON.stringify(bad)} is refused`, docs, ['where', bad, 1], Q);
}
c('an object condition names its fields too', docs, ['where', { 'a-b': 1 }], Q);
c('names are Unicode letters, digits and _', ['from', 'páginas'], ['where', 'año', 2024], ['where', 'şehir_2', 'İzmir'], ['where', '_x', 1], ['select', 'résumé', 'हिंदी'], Q);
c('a near field is a name, not a path', docs, ['near', 'meta.v', [1, 0]], Q);
c('a group field is a name, not a path', ['from', 'orders'], ['select', 'count(*)'], ['group', 'meta.status'], Q);

// order, limit, offset.
c('order is ascending unless told', docs, ['order', 'year'], Q);
c('order adds a key each call', docs, ['order', 'year', 'desc'], ['order', 'title'], Q);
c('a direction in capitals', docs, ['order', 'year', 'DESC'], Q);
c('an unknown direction is refused', docs, ['order', 'year', 'down'], Q);
c('order a desc is no field', docs, ['order', 'a desc'], Q);
c('order takes a collation', docs, ['order', 'title', 'desc', { collate: 'tr' }], ['order', 'name', 'asc', { collate: 'und' }], ['order', 'year'], Q);
c('a collation that is not there is refused', docs, ['order', 'title', 'asc', { collate: 'de' }], Q);
c('a collation is never spliced in', docs, ['order', 'title', 'asc', { collate: 'tr desc; del docs' }], Q);
c('limit and offset', docs, ['select', 'title'], ['limit', 10], ['offset', 5], Q);
c('limit 0 is a limit, offset 0 is none', docs, ['limit', 0], ['offset', 0], Q);
c('a later limit replaces the earlier', docs, ['limit', 10], ['limit', 3], Q);
c('a negative limit is refused', docs, ['limit', -1], Q);
c('a negative offset is refused', docs, ['offset', -2], Q);
c('a limit past 2^53 is refused', docs, ['limit', 9007199254740992], Q);

// near, match, fuse, rerank.
c('near', docs, ['near', 'embed', [0.1, 0.2, 0.3]], ['limit', 5], Q);
c('near with ef', docs, ['near', 'embed', [1, 0], { ef: 128 }], ['limit', 5], Q);
c('near exact', docs, ['near', 'embed', [1, 0], { exact: true }], Q);
c('near with ef and exact', docs, ['near', 'embed', { $f32: [0.5, 0.25] }, { ef: 64, exact: true }], Q);
c('near exact false says nothing', docs, ['near', 'embed', [1, 0], { exact: false }], Q);
c('a negative ef is refused', docs, ['near', 'embed', [1], { ef: -3 }], Q);
c('near with where, order, limit and offset', docs, ['select', 'title'], ['where', 'year', '>=', 2024], ['near', 'embed', [1, 2], { ef: 64 }], ['order', 'year', 'desc'], ['limit', 10], ['offset', 10], Q);
c('a later near replaces the earlier', docs, ['near', 'a', [1]], ['near', 'b', [2]], Q);
c('match', docs, ['match', 'body', 'business trip'], ['limit', 10], Q);
c('match with a filter', docs, ['select', 'title'], ['where', 'year', '>=', 2023], ['match', 'body', 'rust'], Q);
c('match and near need fuse', docs, ['match', 'body', 'x'], ['near', 'embed', [1]], Q);
c('fuse ranks by match and near', docs, ['match', 'body', 'rust wasm'], ['near', 'embed', [1, 0, 0]], ['fuse'], ['limit', 10], Q);
c('fuse with k and candidates', docs, ['where', 'lang', 'en'], ['match', 'body', 'rust'], ['near', 'embed', [1, 0, 0]], ['fuse', { k: 20, candidates: 50 }], ['limit', 10], Q);
c('fuse with candidates alone', docs, ['match', 'body', 'x'], ['near', 'embed', [1]], ['fuse', { candidates: 40 }], Q);
c('fuse without match', docs, ['near', 'embed', [1]], ['fuse'], Q);
c('fuse and rerank', docs, ['match', 'body', 'x'], ['near', 'embed', [1]], ['fuse'], ['rerank', 'embed', [1]], Q);
c('a negative fuse k is refused', docs, ['fuse', { k: -1 }], Q);
c('rerank with a candidate budget', docs, ['match', 'body', 'rust'], ['rerank', 'embed', [1, 0, 0], { candidates: 500 }], ['limit', 5], Q);
c('rerank without a budget', docs, ['match', 'body', 'x'], ['rerank', 'embed', [1, 0, 0]], Q);
c('rerank needs match', docs, ['rerank', 'embed', [1, 0, 0]], Q);
c('a negative rerank budget is refused', docs, ['match', 'body', 'x'], ['rerank', 'embed', [1], { candidates: -5 }], Q);

// Aggregates and group.
c('aggregates grouped, ordered by one', ['from', 'orders'], ['select', 'status', 'count(*)', 'SUM(total)', 'avg(total)'], ['where', 'year', 2024], ['group', 'status'], ['order', 'sum(total)', 'desc'], ['limit', 3], Q);
c('aggregates over the whole collection', ['from', 'orders'], ['select', 'min(at)', 'max(at)', 'Count()'], Q);
c('aggregate names keep their field as written', ['from', 'orders'], ['select', ' Max(Total_2) '], Q);
c('group needs an aggregate', ['from', 'orders'], ['select', 'status'], ['group', 'status'], Q);
c('a whole-collection aggregate takes no limit', ['from', 'orders'], ['select', 'count(*)'], ['limit', 3], Q);
c('an aggregate takes no near', ['from', 'orders'], ['select', 'count(*)'], ['near', 'v', [1, 0]], Q);
c('an aggregate takes no match', ['from', 'orders'], ['select', 'sum(total)'], ['match', 'body', 'x'], Q);
c('an aggregate takes no lookup', ['from', 'orders'], ['select', 'count(*)'], ['lookup', 'lines', { on: 'order_id' }], Q);
c('an aggregate that is not there', ['from', 'orders'], ['select', 'median(total)'], Q);
c('an aggregate of two fields', ['from', 'orders'], ['select', 'sum(a b)'], Q);
c('an aggregate of a path', ['from', 'orders'], ['select', 'sum(meta.total)'], Q);

// lookup.
c('lookup is terminal: its clauses bind to the child', ['from', 'products'], ['where', 'year', 2024], ['limit', 20], ['lookup', 'reviews', { on: 'product_id', select: ['stars', 'body'], where: { stars: { gte: 4 } }, order: [['created', 'desc']], limit: 3, offset: 1 }], Q);
c('lookup names the parent key', ['from', 'products'], ['lookup', 'reviews', { on: 'sku', parentKey: 'code' }], Q);
c('lookup required', ['from', 'products'], ['lookup', 'reviews', { on: 'product_id', required: true, where: { stars: 5 } }], Q);
c('lookup select of one name, or a star', ['from', 'products'], ['lookup', 'reviews', { on: 'product_id', select: 'stars' }], ['lookup', 'votes', { on: 'review_id', select: ['*'] }], Q);
c('lookup order as a name, and pairs with a collation', ['from', 'articles'], ['lookup', 'remarks', { on: 'article_id', order: [['body', 'asc', { collate: 'tr' }], 'id', ['at', 'DESC']] }], Q);
c('lookup order as one name', ['from', 'articles'], ['lookup', 'remarks', { on: 'article_id', order: 'created' }], Q);
c('lookup where takes or', ['from', 'products'], ['lookup', 'reviews', { on: 'product_id', where: { $or: [{ stars: 5 }, { verified: true }] } }], Q);
c('lookup limit 0 and offset 0', ['from', 'products'], ['lookup', 'reviews', { on: 'product_id', limit: 0, offset: 0 }], Q);
c('a lookup chain numbers its parameters in order', ['from', 'shops'], ['where', 'city', 'İzmir'], ['lookup', 'orders', { on: 'shop_id', where: { total: { gt: 100 } }, limit: 3 }], ['lookup', 'lines', { on: 'order_id', required: true, where: { qty: { gte: 2 } }, limit: 5 }], Q);
c('lookup needs on', ['from', 'products'], ['lookup', 'reviews', { limit: 3 }], Q);
c('lookup names are names', ['from', 'products'], ['lookup', 'reviews; del products', { on: 'product_id' }], Q);
c('lookup on is a name', ['from', 'products'], ['lookup', 'reviews', { on: 'meta.pid' }], Q);
c('a collection cannot look itself up', ['from', 'products'], ['lookup', 'products', { on: 'parent_id' }], Q);
c('a chain names a collection once', ['from', 'a'], ['lookup', 'b', { on: 'a_id' }], ['lookup', 'a', { on: 'b_id' }], Q);
c(
  'a chain at most eight deep',
  ['from', 'l0'],
  ...[1, 2, 3, 4, 5, 6, 7, 8, 9].map((i) => ['lookup', `l${i}`, { on: `l${i - 1}_id` }]),
  Q,
);
c('lookup takes no near', ['from', 'products'], ['near', 'embed', [1]], ['lookup', 'reviews', { on: 'product_id' }], Q);
c('lookup takes no match', ['from', 'products'], ['match', 'body', 'x'], ['lookup', 'reviews', { on: 'product_id' }], Q);
c('a negative lookup limit is refused', ['from', 'products'], ['lookup', 'reviews', { on: 'product_id', limit: -1 }], Q);

// The endpoints: what each sends.
c('rows sends the query', docs, ['where', 'year', '>=', 2024], ['limit', 5], ['rows']);
c('first asks for one row', docs, ['where', 'year', 2024], ['first']);
c('first replaces the limit', docs, ['order', 'year', 'desc'], ['limit', 10], ['first']);
c('count is a clause', docs, ['where', 'year', '>=', 2024], ['count']);
c('count over a required lookup', ['from', 'products'], ['where', 'year', 2024], ['lookup', 'reviews', { on: 'product_id', required: true, where: { stars: 5 } }], ['count']);
c('count needs a required lookup', ['from', 'products'], ['lookup', 'reviews', { on: 'product_id' }], ['count']);
c('count takes no select', docs, ['select', 'title'], ['count']);
c('count takes no limit', docs, ['limit', 5], ['count']);
c('count takes no offset', docs, ['offset', 1], ['count']);
c('count takes no order', docs, ['order', 'year'], ['count']);
c('count takes no near', docs, ['near', 'embed', [1, 2]], ['count']);
c('count takes no match', docs, ['match', 'body', 'x'], ['count']);
c('count takes no aggregate', ['from', 'orders'], ['select', 'count(*)'], ['count']);
c('explain runs the query under explain', docs, ['where', 'tag', 'rust'], ['near', 'embed', [1, 0]], ['limit', 3], ['explain']);

// Writing.
c('insert one document', docs, ['insert', { title: 'a', year: 2024 }]);
c('insert several', docs, ['insert', [{ title: 'a' }, { title: 'b', year: 2 }]]);
c('insert takes every kind of value', docs, ['insert', { title: 'çağ', n: -1, f: 0.5, ok: true, gone: null, tags: ['x', 'y'], embed: { $f32: [0.5, 0.25, 1] }, at: { $date: '1970-01-01T00:00:00.000Z' }, meta: { lang: 'tr', n: [1, 2] } }]);
c('insert writes a path', docs, ['toInsert', { 'meta.lang': 'tr', title: 'x' }]);
c('insert of no documents', docs, ['toInsert', []]);
c('insert of an empty document', docs, ['toInsert', {}]);
c('insert of a field that is no name', docs, ['toInsert', { 'a b': 1 }]);
c('insert takes no where', docs, ['where', 'a', 1], ['insert', { a: 1 }]);
c('insert takes no lookup', docs, ['lookup', 'notes', { on: 'doc_id' }], ['insert', { a: 1 }]);
c('update with a filter numbers the patch first', docs, ['where', 'id', 3], ['where', 'year', '<', 2000], ['update', { title: 'new', 'meta.lang': 'en' }]);
c('update of every row says so', docs, ['update', { archived: true }, { all: true }]);
c('an unfiltered update is refused', docs, ['update', { archived: true }]);
c('update with all false is unfiltered', docs, ['toUpdate', { a: 1 }, { all: false }]);
c('update takes no select', docs, ['select', 'title'], ['where', 'id', 1], ['update', { a: 1 }]);
c('update of an empty patch', docs, ['where', 'id', 1], ['toUpdate', {}]);
c('delete with a filter', docs, ['where', 'year', '<', 2000], ['delete']);
c('delete with or', docs, ['where', { $or: [{ a: 1 }, { b: { in: [2, 3] } }] }], ['toDelete']);
c('delete of every row says so', docs, ['delete', { all: true }]);
c('an unfiltered delete is refused', docs, ['delete']);
c('delete takes no limit', docs, ['limit', 1], ['delete', { all: true }]);
c('delete takes no near', docs, ['near', 'embed', [1]], ['delete', { all: true }]);
c('delete takes no order', docs, ['where', 'a', 1], ['order', 'a'], ['toDelete']);
c('delete takes no offset', docs, ['where', 'a', 1], ['offset', 2], ['toDelete']);
c('delete takes no match', docs, ['match', 'body', 'x'], ['toDelete', { all: true }]);
c('delete takes no lookup', docs, ['where', 'a', 1], ['lookup', 'notes', { on: 'doc_id', required: true }], ['delete']);
c('a write keeps the parameters of its where', docs, ['where', { $raw: ['n + ? > ?', 1, 2] }], ['where', 'tags', 'in', ['a', 'b']], ['update', { n: 0 }]);

// ------------------------------------------------------------------ writing

async function generate() {
  const out = [];
  for (const k of cases) out.push({ ...k, ...(await run(k.steps)) });
  return out;
}

/** The file's text: one case a line, so a change reads as the cases it moved. */
export function format(list) {
  return `[\n${list.map((k) => `  ${JSON.stringify(k)}`).join(',\n')}\n]\n`;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const text = format(await generate());
  const names = new Set(cases.map((k) => k.name));
  if (names.size !== cases.length) throw new Error('two cases share a name');
  if (process.argv.includes('--check')) {
    const have = await readFile(GOLDEN, 'utf8').catch(() => '');
    if (have !== text) {
      console.error(`${GOLDEN} is not what the builder makes: make builder-golden`);
      process.exit(1);
    }
  } else {
    await writeFile(GOLDEN, text);
    console.log(`${cases.length} cases -> ${GOLDEN}`);
  }
}
