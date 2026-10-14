// The example's Durable Object class in Node, over a SQLite-backed
// object's storage as fakes.js stands for it: what it does when requests
// and its alarm interleave, which `wrangler dev` gives no way to hold in
// place. `cloudflare:workers` is a class that keeps `ctx` and `env`, and
// the `.wasm` import the module compiled, as a Worker imports it. Needs
// web/fenec.wasm (make wasm), node:sqlite and `module.registerHooks`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import * as nodeModule from 'node:module';
import { Rows } from './fakes.js';

const wasm = await readFile(new URL('../../web/fenec.wasm', import.meta.url)).catch(() => null);
const sqlite = await import('node:sqlite').catch(() => null);
const skip = !wasm
  ? 'no web/fenec.wasm (make wasm)'
  : !sqlite
    ? 'no node:sqlite'
    : !nodeModule.registerHooks && 'no module.registerHooks';
// CI builds the module and runs a Node with both: skipped there, these
// would pass unrun.
if (skip && process.env.CI) throw new Error(`example.test.js cannot run: ${skip}`);

let Tenant;
let Fenec;
if (!skip) {
  const durable = 'export class DurableObject { constructor(ctx, env) { this.ctx = ctx; this.env = env; } }';
  nodeModule.registerHooks({
    resolve(specifier, context, next) {
      if (specifier === 'cloudflare:workers') {
        return { url: `data:text/javascript,${encodeURIComponent(durable)}`, shortCircuit: true };
      }
      return next(specifier, context);
    },
    load(url, context, next) {
      if (!url.endsWith('.wasm')) return next(url, context);
      const source = `import { readFileSync } from 'node:fs'; export default new WebAssembly.Module(readFileSync(new URL(${JSON.stringify(url)})));`;
      return { format: 'module', source, shortCircuit: true };
    },
  });
  ({ Tenant } = await import('./example/worker.js'));
  ({ Fenec } = await import('../../web/fenec.js'));
}

/** An object over `storage`: events are not held back, nothing else runs here. */
const object = (storage) => new Tenant({ storage, blockConcurrencyWhile: (fn) => fn() }, { QUIET: '0' });

/** The turns pending now, run. */
const turns = () => new Promise(setImmediate);

async function settledYet(promise) {
  let done = false;
  promise.then(() => (done = true), () => (done = true));
  await turns();
  return done;
}

/** Each row the storage holds, as a database restored from it reads them. */
async function stored(storage) {
  const { restore } = await import('./index.js');
  const db = await Fenec.open(wasm);
  assert.equal(await restore(db, storage), true);
  return db.rows('get t select id, n order n');
}

/** A request's write, answered with the id it took. */
const write = (t, n) => t.query('put t {n: $1} returning id', [n]);

async function begun() {
  const storage = new Rows(sqlite.DatabaseSync);
  const t = object(storage);
  for (const sql of ['create collection t (n int)', 'put t {n: 1}']) assert.equal((await t.query(sql)).status, 200);
  return [storage, t];
}

/** Every answer 200, each its own id, and every write in storage under it. */
async function allKept(storage, answers) {
  assert.deepEqual(answers.map((a) => a.status), answers.map(() => 200), JSON.stringify(answers));
  const ids = answers.map((a) => a.body.rows[0].id);
  assert.equal(new Set(ids).size, ids.length, `ids ${ids}`);
  assert.deepEqual(await stored(storage), [{ id: 1, n: 1 }, ...ids.map((id, i) => ({ id, n: i + 2 }))]);
}

test('an alarm whose getAlarm fails keeps the database the requests are writing through', { skip }, async () => {
  const [storage, t] = await begun();
  // The alarm waiting for `getAlarm`, read on its first alarm once woken.
  let fail;
  storage.getAlarm = () => new Promise((_, no) => (fail = no));
  const ringing = t.alarm().then(() => 'done', (e) => e.message);
  await turns();
  assert.ok(fail, 'the alarm asks for the alarm set');
  // A request's write on its way to being durable, another's behind it.
  const durable = storage.holdSync();
  const c = write(t, 2);
  const d = write(t, 3);
  await turns();
  fail(new Error('getAlarm went away'));
  // Thrown for Cloudflare to try the alarm again, the database kept.
  assert.equal(await ringing, 'getAlarm went away');
  const e = write(t, 4);
  await turns();
  durable();
  await allKept(storage, await Promise.all([c, d, e]));
});

test('a failed maintain lets the database go once no request holds it', { skip }, async () => {
  const [storage, t] = await begun();
  const first = await t.db();
  // A sweep's statement failing, storage sound.
  first.schemas = () => {
    throw new Error('a sweep that failed');
  };
  const durable = storage.holdSync();
  const c = write(t, 2);
  const d = write(t, 3);
  await turns();
  assert.equal(await t.alarm().then(() => 'done', (e) => e.message), 'a sweep that failed');
  // The next request waits for the two holding the database.
  const e = write(t, 4);
  assert.equal(await settledYet(e), false, 'answered while the first database held writes');
  durable();
  // Both of theirs stored and answered, then a database restored for the
  // next, which writes after them: let go of at once, the second restored
  // while the first still held a write waiting, and the first stored it
  // over the id the second had given its own.
  await allKept(storage, await Promise.all([c, d, e]));
  assert.notEqual(await t.db(), first, 'restored again');
});
