// The example Worker with its storage counted, for rows.mjs and
// worker.test.js: every row a statement writes as workerd counts it
// (`rowsWritten`, what Cloudflare bills), every key put or deleted through
// the key-value methods -- a row each in the hidden table they keep -- and
// every `setAlarm`, billed as a row, every alarm that rang, and every time
// the database was opened and restored (`opens`).
// `GET /t/<tenant>/rows` answers the counts, with the alarm set and the
// generations kept (`tables`); `?reset` zeroes the counts.

import { Tenant as Example } from '../example/worker.js';
import example from '../example/worker.js';

export class Tenant extends Example {
  counts = { sql: 0, kv: 0, alarms: 0, rang: 0, opens: 0 };
  #opened = null;

  constructor(ctx, env) {
    super(ctx, env);
    const counts = this.counts;
    const real = ctx.storage;
    const sql = real.sql && {
      exec(query, ...bindings) {
        const cursor = real.sql.exec(query, ...bindings);
        // A statement that writes runs to its end in `exec`.
        counts.sql += cursor.rowsWritten;
        return cursor;
      },
    };
    const storage = new Proxy(real, {
      get(target, p) {
        if (p === 'sql') return sql;
        if (p === 'put') return (k, v) => ((counts.kv += typeof k === 'string' ? 1 : Object.keys(k).length), target.put(k, v));
        if (p === 'delete') return (k) => ((counts.kv += Array.isArray(k) ? k.length : 1), target.delete(k));
        if (p === 'setAlarm') return (...a) => (counts.alarms++, target.setAlarm(...a));
        const v = Reflect.get(target, p);
        return typeof v === 'function' ? v.bind(target) : v;
      },
    });
    // The base class takes only the state workerd made, so its storage is
    // swapped for the counted one after.
    const counted = new Proxy(ctx, {
      get(target, p) {
        if (p === 'storage') return storage;
        const v = Reflect.get(target, p);
        return typeof v === 'function' ? v.bind(target) : v;
      },
    });
    Object.defineProperty(this, 'ctx', { value: counted, writable: true, configurable: true });
  }

  async alarm() {
    this.counts.rang++;
    return super.alarm();
  }

  db() {
    const opened = super.db();
    if (opened !== this.#opened) {
      this.#opened = opened;
      this.counts.opens++;
    }
    return opened;
  }

  async rows(reset) {
    const s = this.ctx.storage;
    const tables = s.sql
      ? [...s.sql.exec("SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'fenec:%' ORDER BY name").raw()].map(([n]) => n)
      : [];
    const out = { ...this.counts, alarm: await s.getAlarm(), tables };
    if (reset) Object.assign(this.counts, { sql: 0, kv: 0, alarms: 0, rang: 0, opens: 0 });
    return out;
  }
}

export default {
  async fetch(req, env) {
    const url = new URL(req.url);
    const m = url.pathname.match(/^\/t\/([a-z0-9_-]+)\/rows$/);
    if (m) return Response.json(await env.TENANT.get(env.TENANT.idFromName(m[1])).rows(url.searchParams.has('reset')));
    return example.fetch(req, env);
  },
};
