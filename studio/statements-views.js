// The statements of the views past the rows -- the query editor's cut of a
// text into statements, and every schema change -- held to the same rules
// as `statements.js`: a name checked against FenecQL's, never spliced
// otherwise, and a value a parameter. A module of its own so the first
// load, which holds only the rows' statements, carries none of it.
//
// `studio/test/statements.test.mjs` holds these to their text too.

import { name, kindOf, StatementError } from './statements.js';

export { StatementError };

// ------------------------------------------------------------------ the query editor
//
// The editor sends what a person typed, as they typed it: these only cut a
// text into its statements, at the `;` outside strings and comments, and
// read the parameters panel. A statement is never rewritten.

/**
 * The statements of a text: each `;` outside a string or a `--` comment
 * ends one, and a piece holding nothing but blanks and comments is no
 * statement. Each is `{text, at}`, `at` the offset its text starts at.
 */
export function splitStatements(source) {
  const text = String(source);
  const out = [];
  let start = 0;
  let quote = null;
  const push = (end) => {
    const piece = text.slice(start, end);
    if (!blank(piece)) {
      const lead = piece.length - piece.trimStart().length;
      out.push({ text: piece.trim(), at: start + lead });
    }
  };
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (quote) {
      // A string ends at its quote, or at the line's end, where the lexer
      // refuses it: the `;` after is not swallowed into it.
      if (c === '\\') i++;
      else if (c === quote || c === '\n') quote = null;
      continue;
    }
    if (c === '"' || c === "'") quote = c;
    else if (c === '-' && text[i + 1] === '-') {
      const nl = text.indexOf('\n', i);
      i = nl < 0 ? text.length : nl;
    } else if (c === ';') {
      push(i);
      start = i + 1;
    }
  }
  push(text.length);
  return out;
}

/** Whether a piece of text holds only blanks and `--` comments. */
function blank(piece) {
  return piece.replace(/--[^\n]*/g, '').trim() === '';
}

/**
 * What the editor sends: one statement goes to `/query` with the
 * parameters, a JSON list; several go as one `/batch`, the parameters a
 * list of lists, one for each statement in turn (or `[]`, none for any).
 * `{kind: 'query', text, params}` or `{kind: 'batch', items: [[text, params], ...]}`.
 */
export function editorRequest(source, paramsText = '[]') {
  const statements = splitStatements(source);
  if (statements.length === 0) throw new StatementError('nothing to run: type a statement');
  let params;
  try {
    params = JSON.parse(String(paramsText).trim() || '[]');
  } catch {
    throw new StatementError('the parameters are a JSON list: [2024, "rust"]');
  }
  if (!Array.isArray(params)) throw new StatementError('the parameters are a JSON list: [2024, "rust"]');
  if (statements.length === 1) return { kind: 'query', text: statements[0].text, params };
  if (params.length === 0) return { kind: 'batch', items: statements.map((s) => [s.text, []]) };
  if (params.length !== statements.length || !params.every(Array.isArray)) {
    throw new StatementError(
      `${statements.length} statements run as one batch: give their parameters as ${statements.length} lists, one each, as [[1], ["a"]]`,
    );
  }
  return { kind: 'batch', items: statements.map((s, i) => [s.text, params[i]]) };
}

const LEAD = /^(\s|--[^\n]*(\n|$))*/;

/** Whether `text` is a read whose path `explain` shows: a `get` or a `select`. */
export const explainable = (text) => /^(get|select)\b/i.test(String(text).replace(LEAD, ''));

/** Whether `text` is an `explain` already, whose rows are its plan. */
export const isExplain = (text) => /^explain\b/i.test(String(text).replace(LEAD, ''));

/** A read's plan, as the server says it: the same text with `explain` in front, the same parameters. */
export function explained({ text, params }) {
  if (!explainable(text)) throw new StatementError('a plan is shown for a get');
  return { text: `explain ${String(text).replace(LEAD, '')}`, params };
}

// ------------------------------------------------------------------ the schema
//
// A schema change names collections, fields, indexes and types, none of
// which can be a parameter: each is checked here against what FenecQL
// writes, and a number or a duration against its form, before it reaches
// a statement's text. Each change also says what the schema is once it is
// made, as a description `/_schema/plan` reads, so the page shows the
// declared-schema plan of it before anything is applied.

const SCALAR = ['int', 'float', 'text', 'bool', 'timestamp'];

/** A field's type as `alter collection ... add field` writes it, or refused. */
export function typeText(type) {
  const t = String(type).trim();
  if (['int', 'float', 'text', 'bool', 'timestamp', 'json', 'bytes', 'geo'].includes(t)) return t;
  const list = /^\[(\w+)\]$/.exec(t);
  if (list && SCALAR.includes(list[1])) return t;
  const v = /^vector<(\d{1,5})(, f16)?>$/.exec(t);
  if (v && Number(v[1]) > 0) return t;
  const s = /^sparse<(\d{1,7})>$/.exec(t);
  if (s && Number(s[1]) > 0) return t;
  throw new StatementError(`${JSON.stringify(t)} is not a type: int, float, text, bool, timestamp, json, bytes, geo, vector<N>, sparse<N> or [text]`);
}

/** The indexes a field of `type` can carry, by the name its `@` takes. */
export function indexKinds(type) {
  const { kind } = kindOf(type);
  return (
    {
      text: ['hash', 'unique', 'sorted', 'text'],
      int: ['hash', 'unique', 'sorted'],
      float: ['hash', 'unique', 'sorted'],
      timestamp: ['hash', 'unique', 'sorted', 'ttl'],
      bool: ['hash'],
      vector: ['hnsw'],
      sparse: ['inverted'],
      geo: ['geo'],
    }[kind] ?? []
  );
}

const DURATION = /^(\d{1,9})(ms|s|m|h|d)$/;
const UNIT = { ms: 1, s: 1000, m: 60_000, h: 3_600_000, d: 86_400_000 };

/**
 * An index as FenecQL writes it (`text`) and as a description holds it
 * (`json`): `{kind: 'hnsw', metric: 'cosine', quant: 'int8'}` is
 * `@hnsw(cosine, quant=int8)`; `{kind: 'ttl', ttl: '7d'}` is `@ttl(7d)`,
 * `{kind: 'ttl', ms: 604800000}`; `{kind: 'text', prefix: 6, chars: true}`
 * is `@text(prefix=6, chars)`.
 */
export function indexSpec(spec) {
  const kind = spec?.kind;
  switch (kind) {
    case 'hash':
    case 'unique':
    case 'sorted':
    case 'inverted':
      return { text: `@${kind}`, json: { kind } };
    case 'ttl': {
      const m = DURATION.exec(String(spec.ttl ?? '').trim());
      if (!m || Number(m[1]) === 0) throw new StatementError('a ttl is a duration past zero: 30s, 15m, 12h, 7d');
      return { text: `@ttl(${m[1]}${m[2]})`, json: { kind: 'ttl', ms: Number(m[1]) * UNIT[m[2]] } };
    }
    case 'text': {
      const args = [];
      const json = { kind: 'text' };
      const prefix = spec.prefix === undefined || spec.prefix === null || String(spec.prefix).trim() === '' ? 0 : Number(spec.prefix);
      if (prefix !== 0) {
        if (!Number.isInteger(prefix) || prefix < 1 || prefix > 64) throw new StatementError('a text index keeps prefixes of 1 to 64 characters');
        args.push(`prefix=${prefix}`);
        json.prefix = prefix;
      }
      if (spec.chars) {
        args.push('chars');
        json.chars = true;
      }
      return { text: args.length ? `@text(${args.join(', ')})` : '@text', json };
    }
    case 'hnsw': {
      const metric = spec.metric ?? 'cosine';
      if (!['cosine', 'l2', 'dot'].includes(metric)) throw new StatementError(`${JSON.stringify(metric)} is not a metric: cosine, l2 or dot`);
      const quant = spec.quant ?? 'none';
      if (!['none', 'int8', 'bit'].includes(quant)) throw new StatementError(`${JSON.stringify(quant)} is not a quantization: none, int8 or bit`);
      if (quant === 'bit' && metric !== 'cosine') throw new StatementError('bit codes are for cosine alone');
      const json = { kind: 'hnsw', metric };
      if (quant !== 'none') json.quant = quant;
      return { text: `@hnsw(${metric}${quant !== 'none' ? `, quant=${quant}` : ''})`, json };
    }
    default:
      throw new StatementError(`${JSON.stringify(kind)} is not an index: hash, unique, sorted, ttl, text, hnsw or inverted`);
  }
}

/** `create index on <c> (<f>) @...`. */
export function createIndex(collection, field, spec) {
  name(collection, 'collection');
  name(field);
  if (field === 'id') throw new StatementError('the id has an index of its own');
  return { text: `create index on ${collection} (${field}) ${indexSpec(spec).text}`, params: [] };
}

/** `alter collection <c> add field <f> <type> [collate ..] [@index]`. */
export function addField(collection, field, type, { index = null, collate = null } = {}) {
  name(collection, 'collection');
  name(field);
  if (field === 'id') throw new StatementError('every row has an id already');
  let text = `alter collection ${collection} add field ${field} ${typeText(type)}`;
  if (collate) {
    if (!['und', 'tr'].includes(collate)) throw new StatementError('a collation is und or tr');
    if (kindOf(type).kind !== 'text') throw new StatementError('a collation orders text');
    text += ` collate ${collate}`;
  }
  if (index) text += ` ${indexSpec(index).text}`;
  return { text, params: [] };
}

/** `alter collection <c> rename field <a> to <b>`. */
export function renameField(collection, from, to) {
  name(collection, 'collection');
  name(from);
  name(to);
  if (from === to) throw new StatementError('the new name is the name it has');
  if (to === 'id' || from === 'id') throw new StatementError('the id keeps its name');
  return { text: `alter collection ${collection} rename field ${from} to ${to}`, params: [] };
}

/** `alter collection <c> drop field <f>`. */
export function dropField(collection, field) {
  name(collection, 'collection');
  name(field);
  if (field === 'id') throw new StatementError('the id is not dropped');
  return { text: `alter collection ${collection} drop field ${field}`, params: [] };
}

/** `drop collection <c>`. */
export function dropCollection(collection) {
  name(collection, 'collection');
  return { text: `drop collection ${collection}`, params: [] };
}

/**
 * The description `/_schema/plan` is sent for a change: the collection it
 * changes as `/_schema` describes it, with the change made. The others are
 * left out, and a collection the code does not declare is one the plan
 * leaves alone, so the plan speaks of this change and nothing else.
 * `change` is `{op: 'index', collection, field, index}`, `{op: 'add',
 * collection, field, type, index, collate}`, `{op: 'rename', collection,
 * from, to}`, `{op: 'drop-field', collection, field}` or `{op: 'drop',
 * collection}`.
 */
export function planned(description, change) {
  const found = (description?.collections ?? []).find((c) => c.name === change.collection);
  if (!found) throw new StatementError(`no collection ${JSON.stringify(change.collection)} in the schema`);
  const c = structuredClone(found);
  const field = (n) => {
    const f = c.fields.find((x) => x.name === n);
    if (!f) throw new StatementError(`${change.collection} has no field ${JSON.stringify(n)}`);
    return f;
  };
  switch (change.op) {
    case 'index':
      field(change.field).index = indexSpec(change.index).json;
      break;
    case 'add': {
      const f = { name: name(change.field), type: typeText(change.type) };
      if (change.collate) f.collate = change.collate;
      if (change.index) f.index = indexSpec(change.index).json;
      c.fields.push(f);
      break;
    }
    case 'rename':
      field(change.from).name = name(change.to);
      break;
    case 'drop-field':
      field(change.field);
      c.fields = c.fields.filter((f) => f.name !== change.field);
      break;
    case 'drop':
      return { format: 1, collections: [] };
    default:
      throw new StatementError(`no such change: ${change.op}`);
  }
  return { format: 1, collections: [c] };
}

/**
 * The statements `GET /_schema?as=fenecql` answers, by the collection each
 * makes: its `create collection`, then each `create index on` it.
 */
export function createTexts(fenecql) {
  const out = new Map();
  for (const line of String(fenecql ?? '').split('\n')) {
    const m = /^create (?:collection|index on) (\S+?) \(/u.exec(line);
    if (!m) continue;
    if (!out.has(m[1])) out.set(m[1], []);
    out.get(m[1]).push(line);
  }
  return out;
}
