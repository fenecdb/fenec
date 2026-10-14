// A Worker that gives each tenant a database of its own, in a Durable
// Object: `POST /t/<tenant>/query` with `{sql, params}` runs FenecQL there
// and answers its rows. `npx wrangler dev` in this directory serves it;
// worker.test.js starts it twice, to see the databases come back.

import { DurableObject } from 'cloudflare:workers';
// In an app: `@fenecdb/web`, `@fenecdb/web/fenec.wasm` and
// `@fenecdb/cloudflare`. Here the files beside them, so the test runs what
// the repository holds.
import { Fenec } from '../../../web/fenec.js';
import wasm from '../../../web/fenec.wasm';
import { checkpoint, persist, restore } from '../index.js';

/**
 * How long after a write the alarm folds the writes into a new image: an
 * object idle about ten seconds is evicted, and woken by the alarm it would
 * restore the writes one after another only to fold them. `QUIET` in the
 * environment sets it, 0 for no alarm: coldstart.mjs times a start from
 * the writes as they left the storage.
 */
const QUIET = 5_000;

export class Tenant extends DurableObject {
  /** The database, as the promise of it: opened once, by whichever request came first. */
  #db = null;
  /** Whether an alarm is set: `setAlarm` is a row written, and a write needs one alarm, not one each. */
  #alarm = false;

  db() {
    // Opened and restored with every other event held back: opened by
    // whichever request found the object cold, two requests could each
    // restore an engine of their own and append to the storage as if alone.
    return (this.#db ??= this.ctx.blockConcurrencyWhile(async () => {
      // A Worker imports the module compiled, and `open` takes it so.
      const db = await Fenec.open(wasm);
      await restore(db, this.ctx.storage);
      return db;
    }).catch((e) => {
      this.#db = null;
      throw e;
    }));
  }

  /**
   * Runs FenecQL, and keeps what it wrote before the answer goes, as a
   * server's `--sync always` would. A database whose writes were not kept
   * is let go of, and the next request restores what storage holds: kept
   * in memory, it answered with writes the storage never had.
   */
  async query(sql, params) {
    const opened = this.db();
    const db = await opened;
    let out;
    let failed = null;
    try {
      out = db.run(sql, params);
    } catch (e) {
      // A statement that failed may follow ones in the same text that wrote.
      failed = e;
    }
    let wrote;
    try {
      wrote = await persist(db, this.ctx.storage);
    } catch (e) {
      if (this.#db === opened) {
        this.#db = null;
        db.close();
      }
      return { status: 503, body: { error: `not stored: ${e.message}` } };
    }
    const quiet = Number(this.env.QUIET ?? QUIET);
    if (wrote > 0 && quiet > 0 && !this.#alarm) {
      this.#alarm = true;
      await this.ctx.storage.setAlarm(Date.now() + quiet);
    }
    if (failed) return { status: 400, body: { error: failed.message } };
    return { status: 200, body: out };
  }

  /**
   * A new image once writes stop for a while, for the next start to
   * restore the graph rather than link what came after it -- when the
   * writes since the last image are worth one.
   */
  async alarm() {
    this.#alarm = false;
    await this.checkpoint();
  }

  /** The alarm's work at once, for the benchmark. */
  async checkpoint(options) {
    const opened = this.db();
    const db = await opened;
    try {
      return await checkpoint(db, this.ctx.storage, options);
    } catch (e) {
      if (this.#db === opened) {
        this.#db = null;
        db.close();
      }
      throw e;
    }
  }
}

export default {
  async fetch(req, env) {
    const m = new URL(req.url).pathname.match(/^\/t\/([a-z0-9_-]+)\/(query|checkpoint)$/);
    if (!m || req.method !== 'POST') return new Response('not found', { status: 404 });
    const tenant = env.TENANT.get(env.TENANT.idFromName(m[1]));
    if (m[2] === 'checkpoint') return Response.json({ bytes: await tenant.checkpoint({ min: 1 }) });
    let sql;
    let params;
    try {
      ({ sql, params = [] } = await req.json());
    } catch (e) {
      return Response.json({ error: e.message }, { status: 400 });
    }
    const { status, body } = await tenant.query(sql, params);
    return Response.json(body, { status });
  },
};
