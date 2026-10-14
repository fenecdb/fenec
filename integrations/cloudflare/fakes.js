// Stand-ins for a Durable Object's storage, for test.js and memory.mjs: each
// holds to the limits of the kind it stands for, counts the rows it writes
// as Cloudflare bills them, and can be made to stop after a number of
// operations, as storage that goes away part way would.

import assert from 'node:assert/strict';

/**
 * A key-value backed object's storage, in memory: a value of 128 KiB at
 * most, 128 keys a call, values cloned on the way in and out. Each key put
 * or deleted is a row written. `failAfter` puts, it throws.
 */
export class Values {
  map = new Map();
  puts = 0;
  rows = 0;
  failAfter = Infinity;
  async get(key) {
    return structuredClone(this.map.get(key));
  }
  async put(key, value) {
    if (++this.puts > this.failAfter) throw new Error('storage went away');
    const v = structuredClone(value);
    if (v instanceof Uint8Array) assert.ok(v.length <= 128 * 1024, `${key}: ${v.length} bytes`);
    this.map.set(key, v);
    this.rows++;
  }
  /** Resolves once what was written is durable: at once, in memory. */
  async sync() {}
  async delete(keys) {
    assert.ok(keys.length <= 128);
    for (const k of keys) this.rows += this.map.delete(k) ? 1 : 0;
  }
  async list({ prefix = '', start = '', limit = Infinity } = {}) {
    const keys = [...this.map.keys()].filter((k) => k.startsWith(prefix) && k >= start).sort().slice(0, limit);
    return new Map(keys.map((k) => [k, structuredClone(this.map.get(k))]));
  }
}

/**
 * A SQLite-backed object's storage: `sql.exec` and `transactionSync` over
 * node:sqlite, a BLOB of 2 MB at most, a statement's bindings at most 100.
 * Rows written are counted as workerd counts them under `wrangler dev`: a
 * row inserted or deleted one, a table made two, a table dropped none.
 * Each `exec` and each commit is an operation, and past `failAfter` of
 * them it throws; a transaction it stops is rolled back, and every `exec`
 * outside one is its own, as SQLite's autocommit has it.
 */
export class Rows {
  ops = 0;
  rows = 0;
  failAfter = Infinity;
  #tx = false;

  constructor(DatabaseSync, file = ':memory:') {
    this.db = new DatabaseSync(file);
    const fake = this;
    this.sql = {
      exec(query, ...bindings) {
        fake.#op();
        assert.ok(bindings.length <= 100);
        for (const b of bindings) if (ArrayBuffer.isView(b)) assert.ok(b.byteLength <= 2_000_000, `a BLOB of ${b.byteLength} bytes`);
        const stmt = fake.db.prepare(query);
        if (/^\s*(SELECT|PRAGMA)\b/i.test(query)) return cursor(stmt.iterate(...bindings));
        const { changes } = stmt.run(...bindings);
        fake.rows += /^\s*CREATE\b/i.test(query) ? 2 : /^\s*DROP\b/i.test(query) ? 0 : changes;
        return cursor([][Symbol.iterator]());
      },
    };
  }

  async sync() {}

  #op() {
    if (++this.ops > this.failAfter) throw new Error('storage went away');
  }

  transactionSync(fn) {
    assert.ok(!this.#tx, 'transactions do not nest');
    this.#tx = true;
    this.db.exec('BEGIN');
    try {
      const out = fn();
      this.#op();
      this.db.exec('COMMIT');
      return out;
    } catch (e) {
      this.db.exec('ROLLBACK');
      throw e;
    } finally {
      this.#tx = false;
    }
  }

  /** The tables there are, but SQLite's own. */
  tables() {
    return this.db.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name").all().map((r) => r.name);
  }
}

/** A cursor as `sql.exec` gives one: rows as objects, or as arrays through `raw()`. */
function cursor(it) {
  return {
    [Symbol.iterator]: () => it,
    raw: () => (function* () {
      for (const row of it) yield Object.values(row);
    })(),
    one() {
      const { value, done } = it.next();
      assert.ok(!done, 'one row');
      return value;
    },
    toArray: () => [...it],
  };
}
