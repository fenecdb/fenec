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
const S = await import('../statements.js');
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
