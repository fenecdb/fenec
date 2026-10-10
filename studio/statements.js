// Every statement the studio sends, written here and nowhere else -- the
// rows' here, and those of the views past them, which load later, in
// `statements-views.js` under the same rules.
//
// A value is never spliced into a statement's text: it goes as a
// parameter, `$n`, which the server binds. A name cannot be a parameter --
// FenecQL wants it in the text -- so a name is checked against the lexer's
// rule for one (a Unicode letter or `_`, then letters, digits and `_`) and
// refused otherwise, the way the query builder does (`web/builder.js`). The
// one text a person types, a `where` clause, is theirs: it runs with their
// token's authority and no more, and it is held inside parentheses it
// cannot close, so it stays one condition among the studio's own.
//
// `studio/test/statements.test.mjs` holds each of these to its text and
// parameters, odd names and values among them.

export class StatementError extends Error {}

const NAME = /^[\p{Alphabetic}_][\p{Alphabetic}\p{N}_]*$/u;

/** `n` when FenecQL can write it as a name; refused otherwise. */
export function name(n, what = 'field') {
  if (typeof n !== 'string' || !NAME.test(n)) {
    throw new StatementError(`the ${what} name ${JSON.stringify(n)} cannot be written in FenecQL`);
  }
  return n;
}

/** Whether `n` is a name FenecQL can write. */
export const writable = (n) => typeof n === 'string' && NAME.test(n);

function whole(n, what) {
  if (!Number.isSafeInteger(n) || n < 0) throw new StatementError(`${what} must be a whole number: ${n}`);
  return n;
}

/**
 * Whether a typed clause stays inside the parentheses it is put in: every
 * `(` it opens it closes, it never closes one it did not open, and nothing
 * but its strings holds a `;`. Quotes are FenecQL's, `"` and `'`, with `\`
 * escaping the next character.
 */
export function enclosed(text) {
  let depth = 0;
  let quote = null;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (quote) {
      if (c === '\\') i++;
      else if (c === quote) quote = null;
      continue;
    }
    if (c === '"' || c === "'") quote = c;
    else if (c === '(') depth++;
    else if (c === ')' && --depth < 0) return false;
    else if (c === ';') return false;
  }
  return depth === 0 && quote === null;
}

/** The highest `$n` a typed clause names outside its strings. */
function highestParam(text) {
  let top = 0;
  let quote = null;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (quote) {
      if (c === '\\') i++;
      else if (c === quote) quote = null;
      continue;
    }
    if (c === '"' || c === "'") quote = c;
    else if (c === '$') {
      const m = /^\d+/.exec(text.slice(i + 1));
      if (m) top = Math.max(top, Number(m[0]));
    }
  }
  return top;
}

/**
 * A field's type as `/collections` writes it -- `int`, `vector<768>`,
 * `[text]`, `sparse<30522>` -- read into what the studio does with it.
 */
export function kindOf(type) {
  const t = String(type);
  if (t.startsWith('[')) return { kind: 'list', inner: kindOf(t.slice(1, -1)) };
  const v = /^vector<(\d+)/.exec(t);
  if (v) return { kind: 'vector', dim: Number(v[1]) };
  const s = /^sparse<(\d+)>/.exec(t);
  if (s) return { kind: 'sparse', dim: Number(s[1]) };
  if (['int', 'float', 'text', 'bool', 'timestamp', 'json', 'bytes', 'geo'].includes(t)) return { kind: t };
  return { kind: 'other' };
}

/** Which kinds a quick filter takes, and how a bare value reads in each. */
const QUICK = {
  text: '~',
  int: '=',
  float: '=',
  timestamp: '=',
  bool: '=',
  list: 'has',
};

export const filterable = (type) => kindOf(type).kind in QUICK;

/** A value typed into a quick filter, read as the field's type holds it. */
function quickValue(kind, raw, field) {
  const text = raw.trim();
  switch (kind) {
    case 'int': {
      if (!/^-?\d+$/.test(text) || !Number.isSafeInteger(Number(text))) {
        throw new StatementError(`${field} holds whole numbers: ${JSON.stringify(text)} is not one`);
      }
      return Number(text);
    }
    case 'float': {
      const n = Number(text);
      if (text === '' || !Number.isFinite(n)) throw new StatementError(`${field} holds numbers: ${JSON.stringify(text)} is not one`);
      return n;
    }
    case 'bool':
      if (text === 'true') return true;
      if (text === 'false') return false;
      throw new StatementError(`${field} is true or false`);
    case 'timestamp':
      if (Number.isNaN(Date.parse(text))) throw new StatementError(`${field} holds times: ${JSON.stringify(text)} is not one`);
      return text;
    default:
      // A text keeps the spaces typed inside it; only the ones around an
      // operator are the filter's.
      return raw.replace(/^\s+/, '');
  }
}

/**
 * One column's quick filter as a condition: `bind` hands each value a
 * parameter. What may be typed, for every type:
 *
 *   abc        a text holding abc (`~`); an equal number, time or boolean;
 *              a list holding the value (`has`)
 *   =v  !=v    equal, not equal
 *   >v >=v <v <=v
 *   a..b       from a to b, both included (numbers and times)
 *   null !null whether the field is null
 */
export function quickCondition(field, type, input, bind) {
  name(field);
  const { kind, inner } = kindOf(type);
  if (!(kind in QUICK)) throw new StatementError(`${field} cannot be filtered here: use the where clause`);
  const text = String(input).trim();
  if (text === '') return null;
  if (text === 'null') return `${field} is null`;
  if (text === '!null') return `${field} is not null`;
  const valueKind = kind === 'list' ? inner.kind : kind;
  const range = /^(.+?)\.\.(.+)$/.exec(text);
  if (range && ['int', 'float', 'timestamp'].includes(kind)) {
    const lo = bind(quickValue(kind, range[1], field));
    const hi = bind(quickValue(kind, range[2], field));
    return `${field} >= ${lo} and ${field} <= ${hi}`;
  }
  const op = /^(!=|>=|<=|=|>|<)/.exec(text);
  if (op && kind !== 'list') {
    return `${field} ${op[1]} ${bind(quickValue(kind, text.slice(op[1].length), field))}`;
  }
  return `${field} ${QUICK[kind]} ${bind(quickValue(valueKind, kind === 'text' ? String(input) : text, field))}`;
}

/**
 * The filter every read of a view shares: the typed clause with its own
 * parameters first, then each quick filter, each `and`ed, their values
 * numbered after the clause's. `{conditions, params}`; `bind` hands out the
 * next parameter.
 */
export function filter({ where = '', whereParams = [], quick = [] } = {}) {
  const params = [];
  const conditions = [];
  const typed = String(where).trim();
  if (typed) {
    if (!enclosed(typed)) {
      throw new StatementError('the where clause opens or closes a parenthesis it does not match, or holds a `;`');
    }
    if (!Array.isArray(whereParams)) throw new StatementError('the parameters are a JSON list: [2024, "a"]');
    const named = highestParam(typed);
    if (named > whereParams.length) {
      throw new StatementError(`the where clause names $${named}, and ${whereParams.length} parameters are given`);
    }
    conditions.push(`(${typed})`);
  }
  params.push(...whereParams);
  const bind = (v) => {
    params.push(v);
    return `$${params.length}`;
  };
  for (const q of quick) {
    const c = quickCondition(q.field, q.type, q.input, bind);
    if (c) conditions.push(`(${c})`);
  }
  return { conditions, params };
}

/** A statement's text and parameters, with what the filter adds. */
function read(collection, f, extra = []) {
  name(collection, 'collection');
  const params = [...f.params];
  const conditions = [...f.conditions];
  for (const [text, value] of extra) {
    params.push(value);
    conditions.push(text.replace('$?', `$${params.length}`));
  }
  const where = conditions.length ? ` where ${conditions.join(' and ')}` : '';
  return { head: `get ${collection}${where}`, params };
}

/**
 * A page of rows: `limit` of them, from `offset` -- or, walking on from a
 * page already read in id order, the rows after its last id (`after`), the
 * id index walked from there rather than the rows before skipped again.
 * `order` is `{field, desc}`: the id, or a `@sorted` field the index walks.
 */
export function page(collection, f, { limit, offset = 0, after = null, order = null }) {
  const extra = after !== null && !order ? [['id > $?', after]] : [];
  const { head, params } = read(collection, f, extra);
  let text = head;
  if (order) text += ` order ${name(order.field)} ${order.desc ? 'desc' : 'asc'}`;
  text += ` limit ${whole(limit, 'limit')}`;
  if (whole(offset, 'offset') > 0 && after === null) text += ` offset ${offset}`;
  return { text, params };
}

/** How many rows the filter selects. */
export function count(collection, f) {
  const { head, params } = read(collection, f);
  return { text: `${head} count`, params };
}

/** Each value's count over the rows the filter selects, `top` of each. */
export function facets(collection, f, fields, top = 8) {
  const { head, params } = read(collection, f);
  const list = fields.map((x) => `${name(x)} top ${whole(top, 'top')}`).join(', ');
  return { text: `${head} limit 0 facet ${list}`, params };
}

/**
 * One cell written: the row by its id, and only if it is still there for
 * this token -- `require 1` makes a write that matched no row a refusal
 * (412) rather than a quiet `affected 0`.
 */
export function setCell(collection, field, value, id) {
  name(collection, 'collection');
  name(field);
  if (field === 'id') throw new StatementError("a row's id is not written");
  if (value === undefined) throw new StatementError('no value to write');
  return { text: `set ${collection} {${field}: $1} where id = $2 require 1`, params: [value, id] };
}

/**
 * A new row, from the fields given a value; one left out is null. An id is
 * the server's to hand out, so the form never sends one.
 */
export function insertRow(collection, doc) {
  name(collection, 'collection');
  const fields = Object.keys(doc).filter((k) => doc[k] !== undefined && k !== 'id');
  const params = [];
  const pairs = fields.map((k) => {
    params.push(doc[k]);
    return `${name(k)}: $${params.length}`;
  });
  return { text: `insert ${collection} {${pairs.join(', ')}}`, params };
}

/** One row, by its id. */
export function rowById(collection, id) {
  name(collection, 'collection');
  return { text: `get ${collection} where id = $1 limit 1`, params: [id] };
}

/** One row deleted, by its id, and only if it is still there for this token. */
export function deleteRow(collection, id) {
  name(collection, 'collection');
  return { text: `del ${collection} where id = $1 require 1`, params: [id] };
}

/** A statement as a person reads it before it runs: its text, then each parameter. */
export function shown({ text, params }) {
  const lines = [text];
  params.forEach((p, i) => lines.push(`  $${i + 1} = ${JSON.stringify(p)}`));
  return lines.join('\n');
}
