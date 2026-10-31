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
// `toUpdate`, `toUpsert`, `toDelete`, or an endpoint -- `rows`, `first`,
// `count`, `explain`, `insert`, `update`, `upsert`, `delete` -- whose
// statement is the one it
// sends. Arguments are JSON, and what JSON has no word for is an object of
// one `$` key, which no field name can start with:
//
//   {"$or": [c, ...]}, {"$and": [c, ...]}, {"$not": c}   or(), and(), not()
//   {"$raw": [text, param, ...]}                          raw()
//   {"$inc": n}                                           inc(n), a value
//   {"$expr": [text, param, ...]}                         expr(), a value or a column
//   {"$bucket": [field, interval]}                        bucket(field, interval)
//   {"$bucket": [field, interval, zone]}                  bucket(field, interval, zone)
//   {"$countDistinct": field}                             countDistinct(field)
//   {"$distance": [field, [lon, lat]]}                    distance(field, point)
//   {"$first": [field]}, {"$first": [field, by]}          first(field [, by]); "$last" too
//   {"$as": [column, name]}                               column.as(name)
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
import {
  from, or, and, not, raw, inc, expr, bucket, countDistinct, distance, first, last, FenecError,
} from './fenec.js';

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
  if ('$inc' in x) return inc(x.$inc);
  if ('$expr' in x) return expr(x.$expr[0], ...x.$expr.slice(1).map(arg));
  if ('$bucket' in x) return bucket(...x.$bucket);
  if ('$countDistinct' in x) return countDistinct(x.$countDistinct);
  if ('$distance' in x) return distance(...x.$distance);
  if ('$first' in x) return first(...x.$first);
  if ('$last' in x) return last(...x.$last);
  if ('$as' in x) return arg(x.$as[0]).as(x.$as[1]);
  if ('$date' in x) return new Date(x.$date);
  if ('$f32' in x) return Float32Array.from(x.$f32);
  return Object.fromEntries(Object.entries(x).map(([k, v]) => [k, arg(v)]));
}

const TEXT = new Set(['toFenecQL', 'toInsert', 'toUpdate', 'toUpsert', 'toDelete']);
const ENDPOINTS = new Set(['rows', 'first', 'count', 'explain', 'insert', 'update', 'upsert', 'delete']);

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
c('a group key may be a path', ['from', 'orders'], ['select', 'count(*)'], ['group', 'meta.status'], Q);

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
c('a page goes as parameters, one text whatever its size', docs, ['where', 'year', 2024], ['order', 'title'], ['limit', 50], ['offset', 100], Q);

// near, match, fuse, rerank.
c('near', docs, ['near', 'embed', [0.1, 0.2, 0.3]], ['limit', 5], Q);
c('near with ef', docs, ['near', 'embed', [1, 0], { ef: 128 }], ['limit', 5], Q);
c('near exact', docs, ['near', 'embed', [1, 0], { exact: true }], Q);
c('near with ef and exact', docs, ['near', 'embed', { $f32: [0.5, 0.25] }, { ef: 64, exact: true }], Q);
c('near exact false says nothing', docs, ['near', 'embed', [1, 0], { exact: false }], Q);
c('a negative ef is refused', docs, ['near', 'embed', [1], { ef: -3 }], Q);
c('near with where, order, limit and offset', docs, ['select', 'title'], ['where', 'year', '>=', 2024], ['near', 'embed', [1, 2], { ef: 64 }], ['order', 'year', 'desc'], ['limit', 10], ['offset', 10], Q);
c('a later near replaces the earlier', docs, ['near', 'a', [1]], ['near', 'b', [2]], Q);
// Points: a radius, a ring, a box, the nearest, and a distance as a column.
const places = ['from', 'places'];
c('a radius over a point', places, ['where', { loc: { distance: { from: [13.4, 52.5], lte: 500 } } }], Q);
c('a ring: two comparisons of one distance', places, ['where', { loc: { distance: { from: [13.4, 52.5], gt: 100, '<': 1000 } } }], Q);
c('a box over a point', places, ['where', { loc: { within: [13.3, 52.4, 13.5, 52.6] } }], Q);
c('a box beside other conditions', places, ['where', { kind: 'cafe', loc: { within: [170, -20, -170, -10] } }], Q);
c('the nearest, the distance a column', places, ['select', 'name', { $as: [{ $distance: ['loc', [13.4, 52.5]] }, 'm'] }], ['where', { loc: { distance: { from: [13.4, 52.5], lte: 2000 } } }], ['near', 'loc', [13.4, 52.5]], ['limit', 10], Q);
c('a distance takes a from', places, ['where', { loc: { distance: { lte: 500 } } }], Q);
c('a distance compares metres', places, ['where', { loc: { distance: { from: [1, 2], has: 3 } } }], Q);
c('a distance needs a comparison', places, ['where', { loc: { distance: { from: [1, 2] } } }], Q);
c('a distance field is a field', places, ['select', { $distance: ['loc; del places', [1, 2]] }], Q);
c('match', docs, ['match', 'body', 'business trip'], ['limit', 10], Q);
c('match with a filter', docs, ['select', 'title'], ['where', 'year', '>=', 2023], ['match', 'body', 'rust'], Q);
c('match fuzzy', docs, ['match', 'body', 'databse', { fuzzy: true }], ['limit', 10], Q);
c('match fuzzy of one edit', docs, ['match', 'body', 'databse', { fuzzy: 1 }], Q);
c('match prefix', docs, ['match', 'body', 'datab', { prefix: true }], ['limit', 10], Q);
c('match fuzzy and prefix', docs, ['select', 'title'], ['highlight', 'body'], ['match', 'body', 'datbas', { fuzzy: 2, prefix: true }], ['limit', 5], Q);
c('match fuzzy and prefix off write nothing', docs, ['match', 'body', 'x', { fuzzy: false, prefix: false }], Q);
c('match fuzzy of no edit', docs, ['match', 'body', 'x', { fuzzy: 0 }], Q);
c('fuzzy past two edits is refused', docs, ['match', 'body', 'x', { fuzzy: 3 }], Q);
c('fuzzy of negative edits is refused', docs, ['match', 'body', 'x', { fuzzy: -1 }], Q);
c('fuzzy is true, false or a number of edits', docs, ['match', 'body', 'x', { fuzzy: 'yes' }], Q);
c('prefix is true or false', docs, ['match', 'body', 'x', { prefix: 1 }], Q);
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

// highlight and snippet: select-list items, their tags parameters.
c('highlight after the fields select names', docs, ['select', 'title'], ['highlight', 'body'], ['match', 'body', 'rust'], ['limit', 10], Q);
c('highlight with tags', docs, ['highlight', 'body', { pre: '<mark>', post: '</mark>' }], ['match', 'body', 'rust'], Q);
c('a snippet of twenty words', docs, ['select', 'title'], ['snippet', 'body', 20], ['match', 'body', 'business trip'], Q);
c('a snippet with an ellipsis and tags', docs, ['snippet', 'body', 12, { ellipsis: '…', pre: '<b>', post: '</b>' }], ['match', 'body', 'x'], Q);
c('a snippet with tags and no ellipsis', docs, ['snippet', 'body', 5, { pre: '[', post: ']' }], ['match', 'body', 'x'], Q);
c('a snippet with an ellipsis alone', docs, ['snippet', 'body', 5, { ellipsis: '...' }], ['match', 'body', 'x'], Q);
c('marks after every field, in the order called', docs, ['highlight', 'title'], ['snippet', 'body', 8], ['where', 'year', 2024], ['match', 'body', 'x'], Q);
c('marks go with fuse', docs, ['select', 'title'], ['highlight', 'body'], ['match', 'body', 'rust'], ['near', 'embed', [1, 0]], ['fuse'], ['limit', 5], Q);
c('highlight needs match', docs, ['highlight', 'body'], Q);
c('highlight takes no near alone', docs, ['highlight', 'body'], ['near', 'embed', [1, 0]], Q);
c('a snippet needs match', docs, ['snippet', 'body', 3], Q);
c('highlight takes both tags or neither', docs, ['highlight', 'body', { pre: '<b>' }], ['match', 'body', 'x'], Q);
c('a tag is text', docs, ['highlight', 'body', { pre: 1, post: 2 }], ['match', 'body', 'x'], Q);
c('a snippet of no words is refused', docs, ['snippet', 'body', 0], ['match', 'body', 'x'], Q);
c('a snippet of negative words is refused', docs, ['snippet', 'body', -2], ['match', 'body', 'x'], Q);
c('an ellipsis is text', docs, ['snippet', 'body', 4, { ellipsis: 3 }], ['match', 'body', 'x'], Q);
c('a highlight asked twice is refused', docs, ['highlight', 'body'], ['highlight', 'body', { pre: '<b>', post: '</b>' }], ['match', 'body', 'x'], Q);
c('a highlight field is a name, not a path', docs, ['highlight', 'meta.text'], ['match', 'body', 'x'], Q);

// facet: counts over every matched row, beside the page.
c('facet', docs, ['where', 'year', '>=', 2024], ['facet', 'brand'], ['facet', 'color', { top: 5 }], ['limit', 20], Q);
c('facet of a path', docs, ['facet', 'meta.lang', { top: 3 }], Q);
c('facet beside match and a page', docs, ['select', 'title'], ['match', 'body', 'phone'], ['where', 'price', '<', 500], ['facet', 'brand', { top: 10 }], ['limit', 20], ['offset', 20], Q);
c('facet beside a lookup', ['from', 'products'], ['facet', 'brand'], ['lookup', 'reviews', { on: 'product_id', limit: 2 }], Q);
c('facet beside count', docs, ['where', 'year', 2024], ['facet', 'brand'], ['count']);
c('rows sends facets', docs, ['facet', 'brand'], ['limit', 5], ['rows']);
c('facet takes no near', docs, ['near', 'embed', [1, 0]], ['facet', 'brand'], Q);
c('facet takes no fuse', docs, ['match', 'body', 'x'], ['near', 'embed', [1, 0]], ['fuse'], ['facet', 'brand'], Q);
c('a facet asked twice is refused', docs, ['facet', 'brand'], ['facet', 'brand', { top: 2 }], Q);
c('a facet of top 0 is refused', docs, ['facet', 'brand', { top: 0 }], Q);
c('a negative facet top is refused', docs, ['facet', 'brand', { top: -1 }], Q);
c('a facet name is checked', docs, ['facet', 'brand; del docs'], Q);
c('facet takes no aggregate', ['from', 'orders'], ['select', 'status', 'count(*)'], ['group', 'status'], ['facet', 'status'], Q);
c('delete takes no facet', docs, ['where', 'a', 1], ['facet', 'b'], ['toDelete']);
// A facet's own conditions left out, and ranges of a number.
c('a disjunctive facet', docs, ['where', 'brand', 'in', ['acme', 'zeta']], ['where', 'year', 2024], ['facet', 'brand', { top: 10, disjunctive: true }], ['facet', 'color'], ['limit', 20], Q);
c('a range facet', docs, ['where', 'year', 2024], ['facet', 'price', { ranges: [0, 25, 50.5, 100] }], ['limit', 0], Q);
c('a disjunctive range facet', docs, ['where', 'price', '>=', 50], ['facet', 'price', { ranges: [-10, 0, 50, 1000], disjunctive: true }], ['count']);
c('range bounds must rise', docs, ['facet', 'price', { ranges: [0, 50, 50] }], Q);
c('range bounds are numbers', docs, ['facet', 'price', { ranges: [0, '50'] }], Q);
c('a range facet takes two bounds', docs, ['facet', 'price', { ranges: [5] }], Q);
c('a range facet takes no top', docs, ['facet', 'price', { top: 3, ranges: [0, 5] }], Q);
c('disjunctive is a boolean', docs, ['facet', 'brand', { disjunctive: 'yes' }], Q);

// Aggregates and group.
c('aggregates grouped, ordered by one', ['from', 'orders'], ['select', 'status', 'count(*)', 'SUM(total)', 'avg(total)'], ['where', 'year', 2024], ['group', 'status'], ['order', 'sum(total)', 'desc'], ['limit', 3], Q);
c('aggregates over the whole collection', ['from', 'orders'], ['select', 'min(at)', 'max(at)', 'Count()'], Q);
c('aggregate names keep their field as written', ['from', 'orders'], ['select', ' Max(Total_2) '], Q);
c('group needs an aggregate', ['from', 'orders'], ['select', 'status'], ['group', 'status'], Q);
c('a whole-collection aggregate folds the rows its order and limit pick', ['from', 'orders'], ['select', 'count(*)', 'avg(distance)'], ['where', 'courier', 5], ['order', 'at', 'desc'], ['limit', 50], ['offset', 2], Q);
c('an aggregate takes no near', ['from', 'orders'], ['select', 'count(*)'], ['near', 'v', [1, 0]], Q);
c('an aggregate takes no match', ['from', 'orders'], ['select', 'sum(total)'], ['match', 'body', 'x'], Q);
c('an aggregate hangs a lookup off its groups', ['from', 'orders'], ['select', 'courier', 'count(*)'], ['where', 'status', 'done'], ['group', 'courier'], ['order', 'count', 'desc'], ['limit', 10], ['lookup', 'couriers', { on: 'user', parentKey: 'courier', select: ['name', 'code'] }], Q);
c('an aggregate that is not there', ['from', 'orders'], ['select', 'median(total)'], Q);
c('an aggregate of two fields', ['from', 'orders'], ['select', 'sum(a b)'], Q);
c('an aggregate of a path', ['from', 'orders'], ['select', 'sum(meta.total)', 'Count( Distinct meta.user )'], Q);

// Expressions in the select list, in aggregates and as group keys.
const ticks = ['from', 'ticks'];
c('a column worked out, named, its values bound first', ticks, ['select', 'sym', { $as: [{ $expr: ['px * ?', 2] }, 'scaled'] }], ['where', 'qty', '>', 1], Q);
c('a column worked out with no name', ticks, ['select', { $expr: ['px * qty'] }], Q);
c('one-minute bars', ticks, ['select', { $as: [{ $bucket: ['at', '1m'] }, 'minute'] }, { $as: [{ $first: ['px', 'at'] }, 'open'] }, 'max(px)', 'min(px)', { $as: [{ $last: ['px', 'at'] }, 'close'] }, 'sum(qty)'], ['where', 'sym', 'S07'], ['group', 'minute'], ['order', 'minute', 'desc'], ['limit', 60], Q);
c('first and last without by, as written', ticks, ['select', 'first(px)', 'LAST(px)', { $first: ['meta.px'] }], Q);
c('vwap: an expression over aggregates', ticks, ['select', 'sym', { $as: [{ $expr: ['sum(px * qty) / sum(qty)'] }, 'vwap'] }], ['group', 'sym'], ['order', 'vwap', 'desc'], Q);
c('distinct users an hour', ['from', 'events'], ['select', { $as: [{ $bucket: ['at', '1h'] }, 'hour'] }, { $countDistinct: 'user' }], ['where', 'at', '>=', { $date: '2026-09-19T00:00:00.000Z' }], ['group', 'hour'], ['order', 'count(distinct user)', 'desc'], Q);
c('a day by a zone\'s clock, the zone a parameter', ['from', 'orders'], ['select', { $as: [{ $bucket: ['at', '1d', 'Europe/Istanbul'] }, 'day'] }, 'count(*)'], ['where', 'status', 'done'], ['group', 'day'], ['order', 'day'], Q);
c('a bucket\'s zone is a name or an offset', ['from', 'orders'], ['select', { $as: [{ $bucket: ['at', '1d', ''] }, 'day'] }, 'count(*)'], ['group', 'day'], Q);
c('group by several keys and an expression', ['from', 'events'], ['select', 'name', 'country', 'count(*)'], ['group', 'name', 'country', { $bucket: ['at', '1d'] }], Q);
c('group by a list of keys', ['from', 'events'], ['select', 'name', 'count(*)'], ['group', ['name', 'meta.plan']], Q);
c('an expression in group takes its values bound', ['from', 'events'], ['select', 'count(*)'], ['where', 'name', 'buy'], ['group', { $expr: ['coalesce(country, ?)', 'none'] }], Q);
c('calendar intervals', ['from', 'events'], ['select', { $as: [{ $bucket: ['at', '1mo'] }, 'm'] }, { $as: [{ $bucket: ['at', '1w'] }, 'w'] }, 'count(*)'], ['group', 'm', 'w'], Q);
c('a bucket takes a duration', ['from', 'events'], ['select', 'count(*)'], ['group', { $bucket: ['at', '1 m'] }], Q);
c('a bucket takes a duration past zero', ['from', 'events'], ['select', 'count(*)'], ['group', { $bucket: ['at', '0h'] }], Q);
c('a bucket takes a unit it knows', ['from', 'events'], ['select', 'count(*)'], ['group', { $bucket: ['at', '5q'] }], Q);
c('a bucket field is a field', ['from', 'events'], ['select', 'count(*)'], ['group', { $bucket: ['at; del events', '1m'] }], Q);
c('a column name is a name', ticks, ['select', { $as: [{ $expr: ['px * qty'] }, 'a b'] }], Q);
c('a column name is no path', ticks, ['select', { $as: [{ $expr: ['px * qty'] }, 'a.b'] }], Q);
c('a distinct count reads a field', ticks, ['select', { $countDistinct: 'a, b' }], Q);
c('first reads a field', ticks, ['select', { $first: ['px', 'at desc'] }], Q);
c('group takes at least one key', ticks, ['select', 'count(*)'], ['group'], Q);

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
// A lookup level that folds each parent's children.
c('lookup folds each parent\'s children', ['from', 'shops'], ['lookup', 'orders', { on: 'shop', select: ['count(*)', 'sum(total)', 'count(distinct courier)'], where: { status: 'paid' } }], Q);
c('lookup folds the children its order and limit pick', ['from', 'couriers'], ['lookup', 'orders', { on: 'courier', order: [['at', 'desc']], limit: 50, select: ['count(*)', 'avg(distance)'] }], Q);
c('lookup groups each parent\'s children', ['from', 'shops'], ['lookup', 'lines', { on: 'shop', select: ['product', 'sum(qty)'], group: ['product'], order: [['sum(qty)', 'desc'], 'product'], limit: 3, offset: 1 }], Q);
c('lookup folds an expression, its values bound in order', ['from', 'shops'], ['where', 'city', 'x'], ['lookup', 'orders', { on: 'shop', select: [{ $as: [{ $expr: ['sum(total) / ?', 2] }, 'half'] }, { $as: [{ $bucket: ['at', '1d'] }, 'day']}, 'count(*)'], group: ['day'], where: { status: 'paid' }, limit: 7 }], Q);
c('a lookup off a level\'s groups', ['from', 'shops'], ['lookup', 'orders', { on: 'shop', select: ['courier', 'count(*)'], group: 'courier' }], ['lookup', 'couriers', { on: 'user', parentKey: 'courier', required: true, select: ['name'] }], Q);
c('lookup group needs an aggregate', ['from', 'shops'], ['lookup', 'orders', { on: 'shop', select: ['status'], group: ['status'] }], Q);
c('lookup selects fields or aggregates', ['from', 'shops'], ['lookup', 'orders', { on: 'shop', select: [{ $as: [{ $expr: ['total * 2'] }, 'x'] }] }], Q);
c('lookup needs on', ['from', 'products'], ['lookup', 'reviews', { limit: 3 }], Q);
c('lookup names are names', ['from', 'products'], ['lookup', 'reviews; del products', { on: 'product_id' }], Q);
c('lookup on is a name', ['from', 'products'], ['lookup', 'reviews', { on: 'meta.pid' }], Q);
c('a collection cannot look itself up', ['from', 'products'], ['lookup', 'products', { on: 'parent_id' }], Q);
// A key over several fields: a field of each side for each part, in order.
c('lookup on several fields pairs them in order', ['from', 'orders'], ['where', 'org', 7], ['lookup', 'lines', { on: ['org', 'order'], parentKey: ['org', 'id'], where: { qty: { gte: 2 } }, order: [['line', 'desc']], limit: 5 }], Q);
c('lookup on one field in a list is that field', ['from', 'orders'], ['lookup', 'lines', { on: ['order'], parentKey: ['id'] }], Q);
c('lookup on several fields chains and is required', ['from', 'shops'], ['lookup', 'orders', { on: ['org', 'shop'], parentKey: ['org', 'code'], required: true }], ['lookup', 'lines', { on: ['org', 'order'], parentKey: ['org', 'id'], limit: 2 }], Q);
c('lookup on several fields names as many parent fields', ['from', 'orders'], ['lookup', 'lines', { on: ['org', 'order'] }], Q);
c('lookup pairs as many fields of each side', ['from', 'orders'], ['lookup', 'lines', { on: ['org', 'order'], parentKey: ['org', 'id', 'n'] }], Q);
c('lookup on a list of no field', ['from', 'orders'], ['lookup', 'lines', { on: [] }], Q);
c('lookup fields in a list are names', ['from', 'orders'], ['lookup', 'lines', { on: ['org', 'order) = (id'], parentKey: ['org', 'id'] }], Q);
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
c('delete with a limit picks its rows', docs, ['limit', 1], ['delete', { all: true }]);
c('delete takes no near', docs, ['near', 'embed', [1]], ['delete', { all: true }]);
c('delete with an order writes in it', docs, ['where', 'a', 1], ['order', 'a'], ['toDelete']);
c('delete takes no offset', docs, ['where', 'a', 1], ['offset', 2], ['toDelete']);
c('delete takes no match', docs, ['match', 'body', 'x'], ['toDelete', { all: true }]);
c('delete takes no lookup', docs, ['where', 'a', 1], ['lookup', 'notes', { on: 'doc_id', required: true }], ['delete']);
c('a write keeps the parameters of its where', docs, ['where', { $raw: ['n + ? > ?', 1, 2] }], ['where', 'tags', 'in', ['a', 'b']], ['update', { n: 0 }]);

// Values worked out over the row: inc() and expr().
c('update with inc counts from nothing', ['from', 'hits'], ['where', 'key', 'ip1'], ['update', { n: { $inc: 1 } }]);
c('inc takes any number', ['from', 'hits'], ['where', 'id', 1], ['toUpdate', { n: { $inc: -2.5 } }]);
c('inc into a path', docs, ['where', 'id', 1], ['toUpdate', { 'meta.views': { $inc: 1 } }]);
c('inc takes only a number', ['from', 'hits'], ['where', 'id', 1], ['toUpdate', { n: { $inc: '1' } }]);
c('inc in an insert is refused', ['from', 'hits'], ['toInsert', { n: { $inc: 1 } }]);
c('expr binds its placeholders in order', docs, ['where', 'id', 7], ['update', { total: { $expr: ['price * ? + ?', 1.2, 3] }, at: { $expr: ['now()'] }, title: 'x' }]);
c('expr beside inc and plain values', ['from', 'hits'], ['where', { $raw: ['n < ?', 100] }], ['toUpdate', { n: { $inc: 1 }, seen: { $expr: ['now() + ?', 30000] }, key: 'k' }]);
c('expr in an insert', ['from', 'locks'], ['toInsert', { name: 'job', at: { $expr: ['now()'] } }]);
c('expr with more placeholders than parameters', docs, ['where', 'id', 1], ['toUpdate', { n: { $expr: ['n + ? + ?', 1] } }]);
c('expr with more parameters than placeholders', docs, ['where', 'id', 1], ['toUpdate', { n: { $expr: ['n + ?', 1, 2] } }]);
c('a compare-and-set names the version it read', docs, ['where', 'id', 4], ['where', 'version', 3], ['update', { v: 'new', version: { $inc: 1 } }]);

// put ... if absent: a lock taken, or not, in one statement.
c('insert if absent', ['from', 'locks'], ['insert', { name: 'job', owner: 'a', at: { $expr: ['now()'] } }, { ifAbsent: true }]);
c('insert of several if absent', ['from', 'locks'], ['toInsert', [{ id: 1, owner: 'a' }, { id: 2, owner: 'a' }], { ifAbsent: true }]);
c('insert with ifAbsent false is a put', ['from', 'locks'], ['toInsert', { id: 1 }, { ifAbsent: false }]);

// put ... if absent else set: an upsert, the documents' parameters first,
// then the patch's; in an expression `new.f` is the document's own field.
const hits = ['from', 'hits'];
c('upsert one document with inc', hits, ['upsert', { key: 'ip1', n: 1 }, { n: { $inc: 1 } }]);
c('upsert several adds their own counts', hits, ['toUpsert', [{ key: 'a', n: 2 }, { key: 'b', n: 3 }], { n: { $expr: ['n + new.n'] }, seen: { $expr: ['now()'] } }]);
c('upsert binds a plain value in the patch', hits, ['toUpsert', { key: 'a', owner: 'x' }, { owner: 'y', 'meta.by': 'z' }]);
c('upsert require', hits, ['upsert', { id: 4, n: 1 }, { n: { $expr: ['n + ?', 1] } }, { require: 1 }]);
c('upsert of no documents', hits, ['toUpsert', [], { n: 1 }]);
c('upsert of an empty patch', hits, ['toUpsert', { key: 'a' }, {}]);
c('upsert takes no where', hits, ['where', 'key', 'a'], ['upsert', { key: 'a' }, { n: 1 }]);
c('inc in an upsert document is refused', hits, ['toUpsert', { n: { $inc: 1 } }, { n: 1 }]);

// `{ require: n }`: the write must write exactly n rows, or it and its
// batch are put back (412).
c('update require', ['from', 'accounts'], ['where', 'name', 'a'], ['where', 'balance', { gte: 5 }], ['toUpdate', { balance: { $inc: -5 } }, { require: 1 }]);
c('update require through the endpoint', ['from', 'accounts'], ['where', 'name', 'b'], ['update', { balance: { $inc: 5 } }, { require: 1 }]);
c('update of every row require', ['from', 'accounts'], ['toUpdate', { frozen: true }, { all: true, require: 3 }]);
c('delete require', docs, ['where', 'id', 7], ['toDelete', { require: 1 }]);
c('delete require none', docs, ['where', 'a', { lt: 0 }], ['delete', { require: 0 }]);
c('insert require', ['from', 'journal'], ['toInsert', [{ tx: 1, amount: -5 }, { tx: 1, amount: 5 }], { require: 2 }]);
c('insert if absent require', ['from', 'locks'], ['insert', { name: 'job', owner: 'a' }, { ifAbsent: true, require: 1 }]);
c('require takes no negative', docs, ['where', 'id', 1], ['toDelete', { require: -1 }]);

// `require(n)` on a read: the rows it answers, after limit, must number n,
// or it and its batch are put back (412): a checkout's guard.
c('get require', ['from', 'products'], ['where', 'sku', 'tee'], ['where', 'price', 12], ['require', 1], Q);
c('get require counts after limit', ['from', 'coupons'], ['where', 'code', 'X1'], ['limit', 1], ['require', 1], Q);
c('get require none', ['from', 'holds'], ['where', 'sku', 'tee'], ['where', 'state', 'held'], ['require', 0], Q);
c('get require goes before a lookup', ['from', 'orders'], ['where', 'id', 3], ['require', 1], ['lookup', 'lines', { on: 'order_id', limit: 5 }], Q);
c('get require over groups', ['from', 'orders'], ['select', 'status', 'count(*)'], ['group', 'status'], ['require', 2], Q);
c('rows sends require', ['from', 'products'], ['where', 'sku', 'tee'], ['require', 1], ['rows']);
c('first keeps require', ['from', 'products'], ['where', 'sku', 'tee'], ['require', 1], ['first']);
c('a later require replaces the earlier', docs, ['require', 2], ['require', 1], Q);
c('get require takes no negative', docs, ['require', -1], Q);
c('count takes no require', docs, ['where', 'a', 1], ['require', 1], ['count']);
c('a whole-collection aggregate takes no require', ['from', 'orders'], ['select', 'count(*)'], ['require', 1], Q);
c('a write takes require as its option', docs, ['where', 'id', 1], ['require', 1], ['delete']);

// `{ reason }` beside a write's `{ require: n }`, and `require(n, { reason })`
// on a read: `require n else '<reason>'`, which the 412 says as its
// `reason`. Written into the text -- never a parameter, as the count is
// not -- a quote doubled and a backslash escaped; 1 to 200 bytes of UTF-8.
c('update require with a reason', ['from', 'accounts'], ['where', 'name', 'a'], ['where', 'balance', { gte: 5 }], ['toUpdate', { balance: { $inc: -5 } }, { require: 1, reason: 'insufficient funds' }]);
c('delete require with a reason through the endpoint', docs, ['where', 'id', 7], ['delete', { require: 1, reason: 'no such doc' }]);
c('insert if absent require with a reason', ['from', 'likes'], ['toInsert', { post: 1, user: 'u' }, { ifAbsent: true, require: 1, reason: 'liked already' }]);
c('an insert with a reason answers what it made', ['from', 'locks'], ['insert', { name: 'job' }, { ifAbsent: true, returning: ['id'], require: 1, reason: 'held' }]);
c('upsert require with a reason', hits, ['toUpsert', { key: 'a', n: 1 }, { n: { $inc: 1 } }, { require: 1, reason: 'one counter' }]);
c('a claim of one or none says why', ['from', 'jobs'], ['where', 'queue', 'mail'], ['order', 'run_at'], ['limit', 1], ['toUpdate', { owner: 'w1' }, { returning: true, require: 1, reason: 'no job ready' }]);
c('a reason doubles its quotes and escapes its backslashes', docs, ['where', 'id', 1], ['toDelete', { require: 1, reason: "it's a \\ b" }]);
c('a reason keeps letters past ASCII', docs, ['where', 'id', 1], ['toDelete', { require: 1, reason: 'böyle bir kayıt yok' }]);
c('a reason of 200 bytes', docs, ['where', 'id', 1], ['toDelete', { require: 1, reason: 'ş'.repeat(100) }]);
c('a reason past 200 bytes is refused', docs, ['where', 'id', 1], ['toDelete', { require: 1, reason: `${'ş'.repeat(100)}x` }]);
c('an empty reason is refused', docs, ['where', 'id', 1], ['toDelete', { require: 1, reason: '' }]);
c('a reason goes with require', docs, ['where', 'id', 1], ['toUpdate', { a: 1 }, { reason: 'why' }]);
c('get require with a reason', ['from', 'accounts'], ['where', 'id', 3], ['limit', 1], ['require', 1, { reason: 'no such account' }], Q);
c('rows sends require with its reason', ['from', 'coupons'], ['where', 'code', 'X1'], ['require', 1, { reason: 'no such coupon' }], ['rows']);
c('a get reason goes before a lookup', ['from', 'orders'], ['where', 'id', 3], ['require', 1, { reason: 'no such order' }], ['lookup', 'lines', { on: 'order_id', limit: 5 }], Q);
c('a later require takes its own reason or none', docs, ['require', 1, { reason: 'one' }], ['require', 2], Q);
c('a get reason past 200 bytes is refused', docs, ['require', 1, { reason: 'x'.repeat(201) }], Q);
// A reason is written as given but its quote and backslash: a double quote,
// a line end and a character of four bytes go in as they are -- 200 bytes
// of four-byte characters are 50 of them, 100 UTF-16 units. A backtick is
// refused: the refusal quotes its statement with them, and the reason is
// read back as its words up to the first "(`".
c('a reason keeps its double quote', docs, ['where', 'id', 1], ['toDelete', { require: 1, reason: 'say "no"' }]);
c('a reason runs over lines', docs, ['where', 'id', 1], ['toDelete', { require: 1, reason: 'one\ntwo' }]);
c('a reason of characters of four bytes', docs, ['where', 'id', 1], ['toDelete', { require: 1, reason: '🦊'.repeat(50) }]);
c('a reason of characters of four bytes past 200 is refused', docs, ['where', 'id', 1], ['toDelete', { require: 1, reason: `${'🦊'.repeat(50)}x` }]);
c('a reason holds no backtick', docs, ['where', 'id', 1], ['toDelete', { require: 1, reason: 'see `docs`' }]);
c('a get reason holds no backtick', docs, ['require', 1, { reason: 'a (` b' }], Q);

// `order` and `limit` pick the rows an update or a delete writes, and
// `{ returning }` answers them: a job queue's claim, ack and pop.
const jobs = ['from', 'jobs'];
const ready = ['where', { $raw: ['run_at <= now()'] }];
const claim = { owner: 'w1', run_at: { $expr: ['now() + ?', 30000] }, attempts: { $inc: 1 } };
c('a claim picks the oldest ready rows and answers them', jobs, ready, ['order', 'run_at'], ['limit', 10], ['toUpdate', claim, { returning: true }]);
c('a claim through the endpoint', jobs, ready, ['order', 'run_at'], ['limit', 10], ['update', claim, { returning: true }]);
c('a claim by priority, then age', jobs, ready, ['order', 'priority', 'desc'], ['order', 'run_at'], ['limit', 5], ['toUpdate', { owner: 'w2' }, { returning: ['id', 'payload.to', 'attempts'] }]);
c('a claim of one or none', jobs, ready, ['order', 'run_at'], ['limit', 1], ['toUpdate', { owner: 'w3' }, { returning: ['*'], require: 1 }]);
c('an ack names the owner it was claimed by', jobs, ['where', 'id', 7], ['where', 'owner', 'w1'], ['delete', { require: 1 }]);
c('a pop takes no filter: its limit bounds it', jobs, ['order', 'id'], ['limit', 5], ['toDelete', { returning: true }]);
c('a pop through the endpoint', jobs, ['order', 'run_at', 'desc'], ['limit', 2], ['delete', { returning: ['id'] }]);
c('an order alone does not bound a write', jobs, ['order', 'id'], ['toUpdate', { a: 1 }]);
c('returning without a pick', jobs, ['where', 'id', 3], ['toUpdate', { owner: null }, { returning: ['owner', 'run_at'] }]);
c('returning takes the fields to answer', jobs, ['where', 'id', 3], ['toDelete', { returning: [] }]);
c('returning star takes no other field', jobs, ['where', 'id', 3], ['toDelete', { returning: ['*', 'id'] }]);
c('returning takes field names', jobs, ['where', 'id', 3], ['toDelete', { returning: ['a b'] }]);
c('insert takes no order', jobs, ['order', 'run_at'], ['toInsert', { a: 1 }]);
// An insert's and an upsert's `{ returning }`: the rows written, ids and all.
const orders = ['from', 'orders'];
c('an insert answers the ids it made', orders, ['toInsert', { item: 'tea' }, { returning: ['id'] }]);
c('an insert through the endpoint answers its rows', orders, ['insert', [{ item: 'tea' }, { item: 'pie', n: 2 }], { returning: true }]);
c('an insert if absent answers what it made, before its require', ['from', 'locks'], ['toInsert', { name: 'job', owner: 'a' }, { ifAbsent: true, returning: ['id', 'owner'], require: 1 }]);
c('an upsert answers the rows it made and set', hits, ['upsert', { key: 'ip1', n: 1 }, { n: { $inc: 1 } }, { returning: ['id', 'n'] }]);
c('an insert returning takes field names', orders, ['toInsert', { a: 1 }, { returning: ['a b'] }]);
c('upsert takes no limit', jobs, ['limit', 1], ['toUpsert', { id: 1 }, { a: 1 }]);
c('a write still takes no offset', jobs, ['where', 'a', 1], ['order', 'id'], ['limit', 2], ['offset', 1], ['toUpdate', { a: 1 }]);

// The FenecQL reference pages' examples, as each SDK's builder writes them:
// site/builder_tabs.py renders a case's chain in every language for the
// tabs of a page's example (`{{builder "docs: ..."}}`), the build holds the
// case's statement, its parameters written in, to the page's, and each
// SDK's tests run the rendered code. Each ends at an endpoint, as an app's
// query does. A page's `$n` stands for any parameter the case binds.
const vec = [0.9, 0.1, 0];
const ords = ['from', 'orders'];
const arts = ['from', 'articles'];
const prods = ['from', 'products'];
const bars = [
  { $as: [{ $bucket: ['at', '1m'] }, 'bar'] }, { $as: [{ $first: ['px', 'at'] }, 'open'] },
  { $as: [{ $expr: ['max(px)'] }, 'high'] }, { $as: [{ $expr: ['min(px)'] }, 'low'] },
  { $as: [{ $last: ['px', 'at'] }, 'close'] }, { $as: [{ $expr: ['sum(qty)'] }, 'volume'] },
];
// fenecql-aggregates.html
c('docs: aggregates over every order', ords, ['select', 'count(*)', 'sum(total)', 'avg(total)', 'min(at)', 'max(at)'], ['rows']);
c('docs: aggregates of each status', ords, ['select', 'status', 'count(*)', 'avg(total)'], ['where', 'year', 2026], ['group', 'status'], ['rows']);
c('docs: the ten customers who spent most', ords, ['select', 'customer', 'sum(total)'], ['group', 'customer'], ['order', 'sum(total)', 'desc'], ['limit', 10], ['rows']);
c('docs: distinct users an hour and event', ['from', 'events'], ['select', { $as: [{ $bucket: ['at', '1h'] }, 'hour'] }, 'name', { $as: [{ $countDistinct: 'user' }, 'users'] }], ['group', 'hour', 'name'], ['rows']);
c('docs: one-minute bars of a symbol', ['from', 'ticks'], ['select', ...bars], ['where', 'sym', 'S07'], ['group', 'bar'], ['rows']);
c('docs: vwap of each symbol', ['from', 'ticks'], ['select', 'sym', { $as: [{ $expr: ['sum(px * qty) / sum(qty)'] }, 'vwap'] }], ['group', 'sym'], ['rows']);
c('docs: totals of a json path', ['from', 'events'], ['select', 'props.plan', 'sum(props.v)'], ['group', 'props.plan'], ['rows']);
c('docs: the three statuses that took most', ords, ['select', 'status', 'count(*)', 'sum(total)'], ['where', 'year', 2026], ['group', 'status'], ['order', 'sum(total)', 'desc'], ['limit', 3], ['rows']);
c('docs: one-minute bars since a time', ['from', 'ticks'], ['select', ...bars], ['where', 'sym', 'S07'], ['where', 'at', '>=', '2026-05-03T09:00:00Z'], ['group', 'bar'], ['rows']);
// fenecql-indexes.html
c('docs: the newest articles', arts, ['order', 'published', 'desc'], ['limit', 20], ['rows']);
c('docs: the newest articles since a day', arts, ['where', 'published', '>=', '2026-09-01'], ['order', 'published', 'desc'], ['limit', 20], ['rows']);
c('docs: articles counted over a range of titles', arts, ['where', 'title', '>=', 'k'], ['where', 'title', '<', 'l'], ['count']);
c('docs: the nearest articles since a day', arts, ['where', 'published', '>=', '2026-01-01'], ['near', 'embed', vec], ['limit', 10], ['rows']);
const posts = ['from', 'posts'];
c('docs: an owner\'s newest posts', posts, ['where', 'owner', 7], ['order', 'created', 'desc'], ['limit', 20], ['rows']);
c('docs: three owners\' newest posts', posts, ['where', 'owner', 'in', [7, 8, 9]], ['order', 'created', 'desc'], ['limit', 20], ['rows']);
c('docs: an owner\'s posts counted since a time', posts, ['where', 'owner', 7], ['where', 'created', '>=', '2026-10-01'], ['count']);
const sessions = ['from', 'sessions'];
c('docs: a session seen', sessions, ['insert', { user: 'ada', seen: { $expr: ['now()'] } }]);
c('docs: a session seen again', sessions, ['where', 'user', 'ada'], ['update', { seen: { $expr: ['now()'] } }]);
c('docs: a user\'s sessions', sessions, ['where', 'user', 'ada'], ['rows']);
// languages.html: the builder's tour, from a read to a delete.
const tour = ['from', 'docs'];
c('docs: a page of documents with their comments', tour, ['where', 'year', '>=', 2024], ['where', { $or: [{ lang: 'tr' }, { tags: { has: 'rust' } }] }], ['order', 'title', 'asc', { collate: 'tr' }], ['limit', 20], ['lookup', 'comments', { on: 'doc_id', limit: 3 }], ['rows']);
c('docs: documents matching a misspelt word', tour, ['where', 'year', '>=', 2024], ['match', 'title', 'databse', { fuzzy: true }], ['limit', 10], ['rows']);
c('docs: the document of a slug', tour, ['where', 'slug', 'intro'], ['first']);
c('docs: documents counted since a year', tour, ['where', 'year', '>=', 2024], ['count']);
c('docs: a document inserted', tour, ['insert', { slug: 'intro', title: 'Ownership in Rust', year: 2026 }]);
c('docs: a document inserted unless its slug is held', tour, ['insert', { slug: 'intro', title: 'Ownership in Rust' }, { ifAbsent: true }]);
c('docs: a document\'s views counted', tour, ['where', 'slug', 'intro'], ['update', { views: { $inc: 1 } }]);
c('docs: the documents before a year deleted', tour, ['where', 'year', '<', 2000], ['delete']);
// fenecql-lookup.html
c('docs: products with their newest reviews', prods, ['limit', 20], ['lookup', 'reviews', { on: 'product_id', order: [['created', 'desc']], limit: 3 }], ['rows']);
c('docs: shops, their orders and the orders\' lines', ['from', 'shops'], ['limit', 20], ['lookup', 'orders', { on: 'shop_id', limit: 3 }], ['lookup', 'lines', { on: 'order_id', limit: 5 }], ['rows']);
c('docs: products with a five-star review', prods, ['limit', 20], ['lookup', 'reviews', { on: 'product_id', required: true, where: { stars: 5 } }], ['rows']);
c('docs: products with a five-star review counted', prods, ['lookup', 'reviews', { on: 'product_id', required: true, where: { stars: 5 } }], ['count']);
c('docs: products with their reviews', prods, ['lookup', 'reviews', { on: 'product_id' }], ['rows']);
c('docs: the names of dear products with their good reviews', prods, ['select', 'name'], ['where', 'price', '>=', 10000], ['lookup', 'reviews', { on: 'product_id', select: ['body'], where: { stars: { gte: 4 } }, limit: 3 }], ['rows']);
c('docs: products with their tags by sku', prods, ['lookup', 'tags', { on: 'sku', parentKey: 'sku' }], ['rows']);
c('docs: orders with their lines by a key of two fields', ords, ['where', 'org', 7], ['limit', 20], ['lookup', 'lines', { on: ['org', 'order'], parentKey: ['org', 'id'], order: 'line', limit: 5 }], ['rows']);
const named = (e, as) => ({ $as: [{ $expr: [e] }, as] });
c('docs: shops with their orders folded', ['from', 'shops'], ['limit', 20], ['lookup', 'orders', { on: 'shop', select: [named('count(*)', 'n'), named('sum(total)', 'revenue')] }], ['rows']);
c('docs: couriers with their latest fifty deliveries folded', ['from', 'couriers'], ['limit', 20], ['lookup', 'deliveries', { on: 'courier', select: [named('count(*)', 'n'), named('avg(km)', 'km')], order: [['at', 'desc']], limit: 50 }], ['rows']);
c('docs: each shop\'s three best sellers', ['from', 'shops'], ['lookup', 'lines', { on: 'shop', select: ['product', named('sum(qty)', 'sold')], group: ['product'], order: [['sold', 'desc']], limit: 3 }], ['rows']);
c('docs: the busiest couriers with their names', ords, ['select', 'courier', named('count(*)', 'n')], ['where', 'at', '>=', '2026-10-01'], ['group', 'courier'], ['order', 'n', 'desc'], ['limit', 10], ['lookup', 'couriers', { on: 'user', parentKey: 'courier', select: ['name'] }], ['rows']);
// fenecql-reading.html
c('docs: articles counted since a year', arts, ['where', 'year', '>=', 2024], ['count']);
c('docs: articles a page at a time', arts, ['where', 'year', 2024], ['order', 'published', 'desc'], ['limit', 20], ['offset', 40], ['rows']);
c('docs: a page of matches with its brands counted', prods, ['match', 'title', 'tent'], ['limit', 20], ['facet', 'brand', { top: 10 }], ['rows']);
c('docs: ready jobs claimed a page at a time', ['from', 'jobs'], ['where', { $raw: ['run_at <= now()'] }], ['order', 'run_at'], ['limit', 10], ['update', { owner: 'w1' }, { returning: true }]);
c('docs: a column worked out', ['from', 'ticks'], ['select', 'sym', named('px * qty', 'notional')], ['where', 'qty', '>', 100], ['rows']);
c('docs: columns named and worked out', ['from', 'ticks'], ['select', named('px', 'price'), { $as: [{ $bucket: ['at', '1m'] }, 'minute'] }, { $as: [{ $expr: ['greatest(px, ?)', 100] }, 'floor'] }], ['rows']);
c('docs: a column of a case', ords, ['select', 'id', { $as: [{ $expr: ['case when total >= ? then ? else ? end', 100, 'large', 'small'] }, 'size'] }], ['rows']);
// fenecql-schema.html
const docsJson = ['from', 'docs'];
c('docs: a document with a json field', docsJson, ['insert', { title: 'a', meta: { lang: 'tr', source: { site: 'x', rank: 3 } } }]);
c('docs: documents by paths', docsJson, ['where', 'meta.lang', 'tr'], ['where', 'meta.source.rank', '>=', 2], ['rows']);
c('docs: a path selected and ordered by', docsJson, ['select', 'title', 'meta.source.site'], ['order', 'meta.source.rank', 'desc'], ['rows']);
c('docs: a path set', docsJson, ['where', 'title', 'a'], ['update', { 'meta.lang': 'en' }]);
const events = ['from', 'events'];
c('docs: an event at a time written out', events, ['insert', { name: 'login', t: '2026-09-19T12:34:56Z' }]);
c('docs: an event at epoch milliseconds', events, ['insert', { name: 'logout', t: 1758285296000 }]);
c('docs: events between a day and now', events, ['where', 't', '>=', '2026-01-01'], ['where', { $raw: ['t < now()'] }], ['rows']);
const people = ['from', 'people'];
c('docs: people in the root collation', people, ['order', 'name', 'asc', { collate: 'und' }], ['limit', 20], ['rows']);
c('docs: people in Turkish order', people, ['order', 'city', 'desc', { collate: 'tr' }], ['order', 'name', 'asc', { collate: 'tr' }], ['rows']);
c('docs: people counted by city in order', people, ['select', 'city', 'count(*)'], ['group', 'city'], ['order', 'city', 'asc', { collate: 'und' }], ['rows']);
c('docs: the page of people after a name', people, ['where', 'name', '>', 'Ada'], ['order', 'name'], ['limit', 20], ['rows']);
const users = ['from', 'users'];
c('docs: a user', users, ['insert', { email: 'Alice@Example.com', name: 'İsmail' }]);
c('docs: a user by an email in any case', users, ['where', 'email', 'alice@example.COM'], ['rows']);
c('docs: a user whose email is taken', users, ['insert', { email: 'ALICE@example.com' }]);
c('docs: a user unless the email is taken', users, ['insert', { email: 'alice@example.com' }, { ifAbsent: true }]);
c('docs: a user by a name in any case', users, ['where', 'name', 'ismail'], ['rows']);
c('docs: users in Turkish order', users, ['select', 'name'], ['order', 'name'], ['limit', 20], ['rows']);
// fenecql-search.html
c('docs: articles matching words', arts, ['match', 'body', 'ownership'], ['limit', 10], ['rows']);
c('docs: articles matching words since a year', arts, ['where', 'year', '>=', 2024], ['match', 'body', 'ownership'], ['limit', 10], ['rows']);
c('docs: articles matching words reranked', arts, ['match', 'body', 'ownership'], ['rerank', 'embed', vec, { candidates: 500 }], ['limit', 10], ['rows']);
c('docs: matches with their marks', arts, ['select', 'title'], ['highlight', 'body'], ['match', 'body', 'ownership'], ['limit', 10], ['rows']);
c('docs: matches with their marks tagged', arts, ['select', 'title'], ['highlight', 'body', { pre: '<mark>', post: '</mark>' }], ['match', 'body', 'ownership'], ['limit', 10], ['rows']);
c('docs: matches with a snippet', arts, ['select', 'title'], ['snippet', 'body', 20, { ellipsis: '…' }], ['match', 'body', 'ownership'], ['limit', 10], ['rows']);
c('docs: matches with a snippet tagged', arts, ['select', 'title'], ['snippet', 'body', 20, { ellipsis: '…', pre: '<b>', post: '</b>' }], ['match', 'body', 'ownership'], ['limit', 10], ['rows']);
c('docs: tents counted by price range', prods, ['where', 'category', 'tents'], ['limit', 24], ['facet', 'price', { ranges: [0, 2500, 5000, 10000, 25000], disjunctive: true }], ['rows']);
c('docs: a page of products with brands and colours counted', prods, ['where', 'price', '<', 500], ['limit', 20], ['facet', 'brand'], ['facet', 'color'], ['rows']);
c('docs: a page of matches with brands and tags counted', prods, ['where', 'price', '<', 500], ['match', 'title', 'tent'], ['limit', 20], ['facet', 'brand', { top: 10 }], ['facet', 'tags'], ['rows']);
c('docs: phones counted by brand', prods, ['where', 'category', 'phones'], ['facet', 'brand'], ['count']);
c('docs: products counted by origin alone', prods, ['limit', 0], ['facet', 'meta.origin', { top: 5 }], ['rows']);
c('docs: red tents of two brands with their facets', prods, ['where', 'category', 'tents'], ['where', 'brand', 'in', ['acme', 'nova']], ['where', 'color', 'red'], ['order', 'price'], ['limit', 24], ['facet', 'brand', { disjunctive: true }], ['facet', 'color', { disjunctive: true }], ['facet', 'material'], ['rows']);
// fenecql-vector.html
c('docs: the nearest articles', arts, ['near', 'embed', vec], ['limit', 10], ['rows']);
c('docs: the nearest articles since a year', arts, ['where', 'year', '>=', 2024], ['near', 'embed', vec, { ef: 200 }], ['limit', 10], ['rows']);
c('docs: the nearest articles exactly', arts, ['near', 'embed', vec, { exact: true }], ['limit', 10], ['rows']);
c('docs: the nearest articles filtered', arts, ['select', 'title', 'year'], ['where', 'year', '>=', 2024], ['where', 'tags', 'has', 'rust'], ['where', { $not: { title: { '~': 'draft' } } }], ['near', 'embed', vec, { ef: 128 }], ['limit', 10], ['rows']);
// fenecql-writing.html
c('docs: an article set by id', arts, ['where', 'id', 42], ['update', { year: 2025 }]);
c('docs: an article\'s views counted', arts, ['where', 'id', 42], ['update', { views: { $inc: 1 } }]);
c('docs: the articles before a year deleted', arts, ['where', 'year', '<', 2000], ['delete']);
c('docs: a product that must be there at its price', prods, ['where', 'sku', 'tee'], ['where', 'price', 1200], ['where', 'stock', '>', 0], ['require', 1], ['rows']);
c('docs: a coupon that must still be usable', ['from', 'coupons'], ['where', 'code', 'X1'], ['where', { $raw: ['uses < max_uses'] }], ['limit', 1], ['require', 1], ['rows']);
c('docs: no hold that must be there', ['from', 'holds'], ['where', 'order_ref', 'o-17'], ['where', 'state', 'held'], ['require', 0], ['rows']);
c('docs: a debit that must find the money, or says why', ['from', 'accounts'], ['where', 'name', 'ada'], ['where', 'balance', '>=', 30], ['update', { balance: { $expr: ['balance - ?', 30] } }, { require: 1, reason: 'insufficient funds' }]);
c('docs: a hold deleted that must be there, or says why', ['from', 'holds'], ['where', 'order_ref', 'o-17'], ['where', 'state', 'held'], ['delete', { require: 1, reason: "it's not held" }]);
c('docs: a coupon that must still be usable, or says why', ['from', 'coupons'], ['where', 'code', 'X1'], ['where', { $raw: ['uses < max_uses'] }], ['limit', 1], ['require', 1, { reason: 'coupon used up' }], ['rows']);
const queued = (to) => ({ payload: { to }, run_at: { $expr: ['now()'] }, attempts: 0 });
c('docs: two jobs queued', ['from', 'jobs'], ['insert', [queued('ada'), queued('bo')]]);
c('docs: the ten oldest ready jobs claimed', ['from', 'jobs'], ['where', { $raw: ['run_at <= now()'] }], ['order', 'run_at'], ['limit', 10], ['update', { owner: 'w1', run_at: { $expr: ['now() + ?', 30000] }, attempts: { $expr: ['attempts + 1'] } }, { returning: true }]);
c('docs: a job acknowledged', ['from', 'jobs'], ['where', 'id', 1], ['where', 'owner', 'w1'], ['delete', { require: 1 }]);
c('docs: ready jobs popped', ['from', 'jobs'], ['where', { $raw: ['run_at <= now()'] }], ['order', 'run_at'], ['limit', 10], ['delete', { returning: ['id', 'payload.to'] }]);

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
