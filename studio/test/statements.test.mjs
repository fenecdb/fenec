// Every statement the studio sends, held to its text and its parameters:
// a value never reaches the text, whatever it holds, and a name that
// FenecQL cannot write is refused rather than spliced.
//
//   node --test studio/test/statements.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import { gzipSync } from 'node:zlib';
import { readFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { registerHooks } from 'node:module';

// The server serves web/'s client beside the studio's own files, so
// `./client.js` is found there; here it is web/client.js.
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === './client.js' && context.parentURL?.includes('/studio/')) {
      return next(new URL('../web/client.js', context.parentURL).href, context);
    }
    return next(specifier, context);
  },
});
const S = { ...(await import('../statements.js')), ...(await import('../statements-views.js')) };
const { readInput, vectorSummary } = await import('../values.js');
const { parseMetrics, claimsOf } = await import('../connect.js');

const NASTY = [
  '"); del orders where (true',
  "' or 1=1 --",
  '$1',
  '<img src=x onerror=alert(1)>',
  'line\nbreak',
  '\u0000',
  'ölçü ı İ 🦊',
  ') limit 1 offset (',
];

/** Every value went as a parameter: none of it is in the text. */
function apart(stmt, values) {
  for (const v of values) {
    if (typeof v === 'string' && v.length > 2) assert.ok(!stmt.text.includes(v), `${JSON.stringify(v)} reached the text: ${stmt.text}`);
  }
}

test('a cell is written by id, the value a parameter, the count required', () => {
  for (const v of [...NASTY, 42, -0.5, true, null, { a: [1, '"x"'] }, [0.1, 0.2]]) {
    const s = S.setCell('orders', 'note', v, 7);
    assert.equal(s.text, 'set orders {note: $1} where id = $2 require 1');
    assert.deepEqual(s.params, [v, 7]);
  }
});

test('odd names that FenecQL writes are written; the rest are refused', () => {
  assert.equal(S.setCell('odd', 'ölçü', 1, 1).text, 'set odd {ölçü: $1} where id = $2 require 1');
  assert.equal(S.setCell('odd', 'limit', 1, 1).text, 'set odd {limit: $1} where id = $2 require 1');
  assert.equal(S.setCell('odd', '_x9', 1, 1).text, 'set odd {_x9: $1} where id = $2 require 1');
  for (const bad of ['a b', 'a}', 'a: $9} where true --', '1abc', '', 'a.b', 'a-b', 'na\u0000me', '$1']) {
    assert.throws(() => S.setCell('odd', bad, 1, 1), S.StatementError, bad);
    assert.throws(() => S.setCell(bad, 'f', 1, 1), S.StatementError, bad);
    assert.throws(() => S.insertRow('odd', { [bad]: 1 }), S.StatementError, bad);
    assert.throws(() => S.quickCondition(bad, 'text', 'x', () => '$1'), S.StatementError, bad);
  }
  assert.throws(() => S.setCell('odd', 'id', 1, 1), S.StatementError);
});

test('an insert names its fields and binds each value', () => {
  const doc = { note: NASTY[0], total: 3.5, meta: { k: NASTY[3] }, owner: undefined, id: 99 };
  const s = S.insertRow('orders', doc);
  assert.equal(s.text, 'insert orders {note: $1, total: $2, meta: $3}');
  assert.deepEqual(s.params, [NASTY[0], 3.5, { k: NASTY[3] }]);
  assert.equal(S.insertRow('orders', {}).text, 'insert orders {}');
});

test('a delete is one row by id, required', () => {
  assert.deepEqual(S.deleteRow('orders', 12), { text: 'del orders where id = $1 require 1', params: [12] });
  assert.equal(S.shown(S.deleteRow('orders', 12)), 'del orders where id = $1 require 1\n  $1 = 12');
});

test('quick filters bind every value they read', () => {
  const run = (field, type, input) => {
    const params = [];
    const text = S.quickCondition(field, type, input, (v) => (params.push(v), `$${params.length}`));
    return { text, params };
  };
  assert.deepEqual(run('note', 'text', 'abc'), { text: 'note ~ $1', params: ['abc'] });
  assert.deepEqual(run('note', 'text', '=abc'), { text: 'note = $1', params: ['abc'] });
  assert.deepEqual(run('note', 'text', '!= a b'), { text: 'note != $1', params: ['a b'] });
  assert.deepEqual(run('total', 'float', '>= 2.5'), { text: 'total >= $1', params: [2.5] });
  assert.deepEqual(run('n', 'int', '3..9'), { text: 'n >= $1 and n <= $2', params: [3, 9] });
  assert.deepEqual(run('n', 'int', '-4'), { text: 'n = $1', params: [-4] });
  assert.deepEqual(run('ok', 'bool', 'false'), { text: 'ok = $1', params: [false] });
  assert.deepEqual(run('at', 'timestamp', '>2026-01-01'), { text: 'at > $1', params: ['2026-01-01'] });
  assert.deepEqual(run('tags', '[text]', 'rust'), { text: 'tags has $1', params: ['rust'] });
  assert.deepEqual(run('note', 'text', 'null'), { text: 'note is null', params: [] });
  assert.deepEqual(run('note', 'text', '!null'), { text: 'note is not null', params: [] });
  assert.deepEqual(run('note', 'text', '   '), { text: null, params: [] });
  for (const v of NASTY) {
    const got = run('note', 'text', v);
    apart(got, [v.trim()]);
    assert.match(got.text, /^note (~|=|!=|>|>=|<|<=) \$1$|^note is (not )?null$|^$/);
  }
  assert.throws(() => run('n', 'int', '1.5'), S.StatementError);
  assert.throws(() => run('n', 'int', '9007199254740993'), S.StatementError);
  assert.throws(() => run('ok', 'bool', 'yes'), S.StatementError);
  assert.throws(() => run('at', 'timestamp', 'soon'), S.StatementError);
  assert.throws(() => run('emb', 'vector<8>', '1'), S.StatementError);
  assert.throws(() => run('meta', 'json', 'x'), S.StatementError);
});

test('the typed clause stays inside its parentheses, its parameters first', () => {
  const f = S.filter({
    where: 'total > $1 and note ~ ")("',
    whereParams: [10],
    quick: [
      { field: 'note', type: 'text', input: NASTY[0] },
      { field: 'total', type: 'float', input: '<100' },
    ],
  });
  assert.deepEqual(f.conditions, ['(total > $1 and note ~ ")(")', '(note ~ $2)', '(total < $3)']);
  assert.deepEqual(f.params, [10, NASTY[0], 100]);
  const p = S.page('orders', f, { limit: 100, offset: 300 });
  assert.equal(p.text, 'get orders where (total > $1 and note ~ ")(") and (note ~ $2) and (total < $3) limit 100 offset 300');
  for (const bad of ['a = 1) or (true', 'a = 1; del orders', '(a = 1', 'a = "unclosed', 'a = 1)) limit 5 ((']) {
    assert.throws(() => S.filter({ where: bad }), S.StatementError, bad);
  }
  assert.throws(() => S.filter({ where: 'a = $2', whereParams: [1] }), /names \$2/);
  // A `$` inside a string names nothing.
  assert.doesNotThrow(() => S.filter({ where: 'a = "$5"' }));
});

test('a page walks on by id, or by offset under an order', () => {
  const f = S.filter({ quick: [{ field: 'status', type: 'text', input: '=paid' }] });
  assert.deepEqual(S.page('orders', f, { limit: 100, offset: 200, after: 4123 }), {
    text: 'get orders where (status = $1) and id > $2 limit 100',
    params: ['paid', 4123],
  });
  assert.deepEqual(S.page('orders', f, { limit: 100, offset: 200, order: { field: 'placed', desc: true } }), {
    text: 'get orders where (status = $1) order placed desc limit 100 offset 200',
    params: ['paid'],
  });
  assert.equal(S.page('orders', S.filter(), { limit: 100 }).text, 'get orders limit 100');
  assert.throws(() => S.page('orders', S.filter(), { limit: 100, order: { field: 'a b' } }), S.StatementError);
  assert.throws(() => S.page('orders', S.filter(), { limit: -1 }), S.StatementError);
  assert.throws(() => S.page('orders', S.filter(), { limit: '5; del' }), S.StatementError);
});

test('counts and facets read the same filter', () => {
  const f = S.filter({ where: 'total > $1', whereParams: [5] });
  assert.deepEqual(S.count('orders', f), { text: 'get orders where (total > $1) count', params: [5] });
  assert.deepEqual(S.facets('orders', f, ['status', 'owner'], 6), {
    text: 'get orders where (total > $1) limit 0 facet status top 6, owner top 6',
    params: [5],
  });
  assert.throws(() => S.facets('orders', f, ['a,b']), S.StatementError);
});

test('an edit reads what was typed as the field holds it', () => {
  assert.equal(readInput('42', 'int', 'n'), 42);
  assert.equal(readInput('', 'int', 'n'), null);
  assert.equal(readInput('  spaced ', 'text', 't'), '  spaced ');
  assert.deepEqual(readInput('[1, 2]', 'vector<2>', 'v'), [1, 2]);
  assert.deepEqual(readInput('{"a": 1}', 'json', 'j'), { a: 1 });
  assert.throws(() => readInput('[1]', 'vector<2>', 'v'), S.StatementError);
  assert.throws(() => readInput('1.5', 'int', 'n'), S.StatementError);
  assert.throws(() => readInput('{bad', 'json', 'j'), S.StatementError);
  assert.throws(() => readInput('[300]', 'bytes', 'b'), S.StatementError);
  assert.equal(vectorSummary([3, 4]), 'dim 2  norm 5.00  3 4');
});

test('the metrics and a token are read, not believed', () => {
  const m = parseMetrics('fenec_file_bytes 120\nfenec_reclaimable_bytes 7\nfenec_data_bytes{collection="a"} 100\nfenec_dead_bytes{collection="a"} 5\n# HELP x\n');
  assert.deepEqual(m, { file: 120, reclaimable: 7, data: { a: 100 }, dead: { a: 5 } });
  const t = ['e30', Buffer.from(JSON.stringify({ sub: 'ali', exp: 9 })).toString('base64url'), 'sig'].join('.');
  globalThis.atob ??= (s) => Buffer.from(s, 'base64').toString('binary');
  assert.deepEqual(claimsOf(t), { sub: 'ali', exp: 9 });
  assert.equal(claimsOf('opaque-server-token'), null);
  assert.equal(claimsOf('a.%%%.c'), null);
});

// ------------------------------------------------------------------ the query editor

test('the editor cuts a text at each ; outside its strings and comments, and rewrites nothing', () => {
  const text = 'get a where t = ";" -- a ; in a comment\n  limit 5;\n\n-- only a comment;\nget b where u = \'x;y\'';
  const got = S.splitStatements(text);
  assert.deepEqual(
    got.map((s) => s.text),
    // A `;` in a comment ends nothing: the comment is the next statement's.
    ['get a where t = ";" -- a ; in a comment\n  limit 5', "-- only a comment;\nget b where u = 'x;y'"],
  );
  // Where each starts, so a refusal can point at it.
  assert.equal(text.slice(got[1].at, got[1].at + 7), '-- only');
  assert.deepEqual(S.splitStatements('  ;  -- nothing\n;'), []);
  // A string left open ends at its line, as the lexer refuses it there.
  assert.deepEqual(S.splitStatements('get a where t = "open\n; get b').map((s) => s.text), ['get a where t = "open', 'get b']);
});

test('one statement goes to /query with its parameters; several, a list each, to /batch', () => {
  for (const v of NASTY) {
    const r = S.editorRequest('get orders where note = $1', JSON.stringify([v]));
    assert.deepEqual(r, { kind: 'query', text: 'get orders where note = $1', params: [v] });
  }
  assert.deepEqual(S.editorRequest('get a; get b'), { kind: 'batch', items: [['get a', []], ['get b', []]] });
  assert.deepEqual(S.editorRequest('get a where x = $1; del b where y = $1', '[[1], ["z"]]'), {
    kind: 'batch',
    items: [
      ['get a where x = $1', [1]],
      ['del b where y = $1', ['z']],
    ],
  });
  assert.throws(() => S.editorRequest('get a; get b', '[1, 2]'), /2 statements run as one batch/);
  assert.throws(() => S.editorRequest('get a', '{"a": 1}'), S.StatementError);
  assert.throws(() => S.editorRequest('get a', '[1,'), S.StatementError);
  assert.throws(() => S.editorRequest('-- nothing'), /nothing to run/);
});

test('a plan is asked of a get as typed, explain in front, the same parameters', () => {
  assert.deepEqual(S.explained({ text: '-- why\nget orders where id = $1', params: [7] }), { text: 'explain get orders where id = $1', params: [7] });
  assert.ok(S.explainable('select a from b'));
  assert.ok(!S.explainable('del orders where id = 1'));
  assert.ok(S.isExplain('  explain get a'));
  assert.throws(() => S.explained({ text: 'set a {b: 1}', params: [] }), S.StatementError);
});

// ------------------------------------------------------------------ the schema

test('a schema change names only what FenecQL writes, and checks each option', () => {
  assert.deepEqual(S.createIndex('orders', 'total', { kind: 'sorted' }), { text: 'create index on orders (total) @sorted', params: [] });
  assert.equal(S.createIndex('docs', 'title', { kind: 'text', prefix: 6, chars: true }).text, 'create index on docs (title) @text(prefix=6, chars)');
  assert.equal(S.createIndex('docs', 'title', { kind: 'text', prefix: '' }).text, 'create index on docs (title) @text');
  assert.equal(S.createIndex('docs', 'emb', { kind: 'hnsw', metric: 'l2', quant: 'int8' }).text, 'create index on docs (emb) @hnsw(l2, quant=int8)');
  assert.equal(S.createIndex('ev', 'at', { kind: 'ttl', ttl: '7d' }).text, 'create index on ev (at) @ttl(7d)');
  assert.deepEqual(S.indexSpec({ kind: 'ttl', ttl: '90m' }).json, { kind: 'ttl', ms: 5_400_000 });
  assert.equal(S.addField('odd', 'ölçü2', 'text', { collate: 'tr', index: { kind: 'hash' } }).text, 'alter collection odd add field ölçü2 text collate tr @hash');
  assert.equal(S.addField('docs', 'v', 'vector<768, f16>').text, 'alter collection docs add field v vector<768, f16>');
  assert.equal(S.renameField('orders', 'note', 'memo').text, 'alter collection orders rename field note to memo');
  assert.equal(S.dropField('orders', 'note').text, 'alter collection orders drop field note');
  assert.equal(S.dropCollection('orders').text, 'drop collection orders');
  for (const bad of ['a b', 'x) @hash; drop collection orders --', '', 'a.b', '$1']) {
    assert.throws(() => S.createIndex('orders', bad, { kind: 'hash' }), S.StatementError, bad);
    assert.throws(() => S.createIndex(bad, 'f', { kind: 'hash' }), S.StatementError, bad);
    assert.throws(() => S.addField('orders', bad, 'text'), S.StatementError, bad);
    assert.throws(() => S.renameField('orders', 'note', bad), S.StatementError, bad);
    assert.throws(() => S.dropField('orders', bad), S.StatementError, bad);
    assert.throws(() => S.dropCollection(bad), S.StatementError, bad);
  }
  for (const bad of ['text @hash', 'vector<0>', 'int; drop collection a', '[json]', 'vector<8>) @hnsw']) {
    assert.throws(() => S.addField('orders', 'f', bad), S.StatementError, bad);
  }
  for (const bad of [{ kind: 'ttl', ttl: '7 days' }, { kind: 'ttl', ttl: '0d' }, { kind: 'text', prefix: 65 }, { kind: 'hnsw', metric: 'cos' }, { kind: 'hnsw', metric: 'l2', quant: 'bit' }, { kind: 'btree' }]) {
    assert.throws(() => S.indexSpec(bad), S.StatementError, JSON.stringify(bad));
  }
  assert.throws(() => S.createIndex('orders', 'id', { kind: 'hash' }), S.StatementError);
  assert.throws(() => S.addField('orders', 'n', 'int', { collate: 'tr' }), /orders text/);
  assert.deepEqual(S.indexKinds('vector<8>'), ['hnsw']);
  assert.deepEqual(S.indexKinds('json'), []);
});

test('the plan is asked of the collection as it would be after the change, and of nothing else', () => {
  const desc = {
    format: 1,
    collections: [
      { name: 'orders', fields: [{ name: 'total', type: 'float' }, { name: 'note', type: 'text', index: { kind: 'hash' } }] },
      { name: 'docs', fields: [{ name: 'title', type: 'text' }] },
    ],
  };
  const frozen = JSON.stringify(desc);
  assert.deepEqual(S.planned(desc, { op: 'index', collection: 'orders', field: 'total', index: { kind: 'sorted' } }), {
    format: 1,
    collections: [{ name: 'orders', fields: [{ name: 'total', type: 'float', index: { kind: 'sorted' } }, { name: 'note', type: 'text', index: { kind: 'hash' } }] }],
  });
  assert.deepEqual(S.planned(desc, { op: 'add', collection: 'docs', field: 'at', type: 'timestamp', index: { kind: 'ttl', ttl: '1h' } }).collections[0].fields[1], {
    name: 'at',
    type: 'timestamp',
    index: { kind: 'ttl', ms: 3_600_000 },
  });
  assert.equal(S.planned(desc, { op: 'rename', collection: 'orders', from: 'note', to: 'memo' }).collections[0].fields[1].name, 'memo');
  assert.deepEqual(S.planned(desc, { op: 'drop-field', collection: 'orders', field: 'note' }).collections[0].fields, [{ name: 'total', type: 'float' }]);
  assert.deepEqual(S.planned(desc, { op: 'drop', collection: 'docs' }), { format: 1, collections: [] });
  assert.throws(() => S.planned(desc, { op: 'index', collection: 'nope', field: 'x', index: { kind: 'hash' } }), S.StatementError);
  assert.throws(() => S.planned(desc, { op: 'rename', collection: 'orders', from: 'gone', to: 'x' }), S.StatementError);
  // The schema read from the server is never changed in place.
  assert.equal(JSON.stringify(desc), frozen);
  const texts = S.createTexts('create collection orders (a int @hash)\ncreate index on orders (meta.k) @hash\ncreate collection ölçü (b text)\n');
  assert.deepEqual([...texts], [
    ['orders', ['create collection orders (a int @hash)', 'create index on orders (meta.k) @hash']],
    ['ölçü', ['create collection ölçü (b text)']],
  ]);
});

test('an answer\'s columns are read off its rows; the metrics off their text', async () => {
  const { columnsOf } = await import('../kit.js');
  assert.deepEqual(columnsOf([{ n: 1, id: 2, v: [0.1, 0.2, 0.3, 0.4], m: { a: 1 } }, { n: 1.5, t: 'x', ok: true, m: null }]), [
    { name: 'id', type: 'int' },
    { name: 'n', type: 'float' },
    { name: 'v', type: 'vector<4>' },
    { name: 'm', type: 'json' },
    { name: 't', type: 'text' },
    { name: 'ok', type: 'bool' },
  ]);
  const { prometheus, total, quantile } = await import('../admin.js');
  const m = prometheus(
    [
      '# TYPE fenec_statements_total counter',
      'fenec_statements_total{kind="read"} 90',
      'fenec_statements_total{kind="write"} 10',
      'fenec_statement_duration_seconds_bucket{kind="read",le="0.001"} 50',
      'fenec_statement_duration_seconds_bucket{kind="read",le="0.01"} 90',
      'fenec_statement_duration_seconds_bucket{kind="read",le="+Inf"} 100',
      'fenec_data_bytes{collection="a \\"b\\""} 7',
    ].join('\n'),
  );
  assert.equal(total(m, 'fenec_statements_total'), 100);
  assert.equal(total(m, 'fenec_statements_total', { kind: 'write' }), 10);
  assert.equal(total(m, 'fenec_nothing'), null);
  assert.equal(m.at(-1).labels.collection, 'a "b"');
  assert.equal(quantile(m, 'fenec_statement_duration_seconds', 0.5, { kind: 'read' }), 0.001);
  assert.ok(Math.abs(quantile(m, 'fenec_statement_duration_seconds', 0.7, { kind: 'read' }) - 0.0055) < 1e-12);
});

// Each view is a module of its own, fetched when it opens: what it adds,
// with the stylesheet and the modules the views share, gzipped.
test('each view past the rows loads apart from the first load, and small', () => {
  const here = dirname(fileURLToPath(import.meta.url));
  const gz = (f) => gzipSync(readFileSync(join(here, '..', f)), { level: 9 }).length;
  const shared = gz('views.css') + gz('kit.js') + gz('highlight.js') + gz('statements-views.js');
  const page = readFileSync(join(here, '..', 'index.html'), 'utf8');
  for (const view of ['editor.js', 'schema.js', 'live.js', 'admin.js', 'kit.js', 'highlight.js', 'views.css', 'statements-views.js']) {
    assert.ok(!page.includes(view), `${view} is on the first load`);
  }
  for (const view of ['editor.js', 'schema.js', 'live.js', 'admin.js']) {
    assert.ok(shared + gz(view) < 24_000, `${view}: ${shared + gz(view)} bytes gzipped`);
  }
});

// The first load: every script and style the page fetches, gzipped, under
// the 120 KB the studio is held to.
test('the first load is under 120 KB gzipped', () => {
  const here = dirname(fileURLToPath(import.meta.url));
  const files = ['index.html', 'app.css', 'app.js', 'dom.js', 'statements.js', 'grid.js', 'sidebar.js', 'values.js', 'edit.js', 'connect.js']
    .map((f) => join(here, '..', f))
    .concat(['client.js', 'builder.js', 'http.js'].map((f) => join(here, '..', '..', 'web', f)));
  const total = files.reduce((sum, f) => sum + gzipSync(readFileSync(f), { level: 9 }).length, 0);
  assert.ok(total < 120_000, `${total} bytes gzipped`);
});

// fenec-server's page never reaches the database in the page: the
// modules its first load names, and every module they import statically,
// hold no local transport, no worker and no module glue -- those load only
// where a page asks for them (the site's playground, `data-mode="local"`).
test('the server\'s first load holds nothing of the database in the page', () => {
  const here = dirname(fileURLToPath(import.meta.url));
  const where = (f) => (['client.js', 'builder.js', 'http.js', 'fenec.js'].includes(f) ? join(here, '..', '..', 'web', f) : join(here, '..', f));
  const page = readFileSync(join(here, '..', 'index.html'), 'utf8');
  const seen = new Set();
  const walk = (f) => {
    if (seen.has(f)) return;
    seen.add(f);
    const src = readFileSync(where(f), 'utf8');
    // Static imports alone: `import(...)` is a view fetched when it opens.
    for (const [, dep] of src.matchAll(/^(?:import|export)\s[^;]*?from\s+'\.\/([\w-]+\.js)'/gm)) walk(dep);
  };
  for (const [, f] of page.matchAll(/(?:src|href)="([\w-]+\.js)"/g)) walk(f);
  for (const local of ['local.js', 'engine.js', 'worker.js', 'fenec.js', 'playground.js', 'playground-data.js']) {
    assert.ok(!seen.has(local), `${local} is on fenec-server's first load`);
  }
  assert.ok(seen.has('app.js') && seen.has('connect.js') && seen.has('client.js'), [...seen].join(', '));
  assert.ok(!page.includes('local.css'), 'the playground\'s stylesheet is on fenec-server\'s first load');
  // Nor is the local transport embedded in the server: its files are the site's.
  const embedded = readFileSync(join(here, '..', '..', 'crates', 'fenec-http', 'src', 'studio.rs'), 'utf8');
  for (const local of ['local.js', 'local.css', 'engine.js', 'worker.js']) assert.ok(!embedded.includes(`"studio/${local}"`), local);
});
