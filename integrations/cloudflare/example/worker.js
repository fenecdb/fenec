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
import { checkpoint, maintain, persist, restore, schedule } from '../index.js';

/**
 * How long after the write that made maintenance due the alarm rings, at
 * the soonest: an object idle about ten seconds is evicted, and the writes
 * of a burst should not each be followed by a sweep or an image. `QUIET`
 * in the environment sets it, 0 for no alarm: coldstart.mjs times a start
 * from the writes as they left the storage.
 */
const QUIET = 5_000;

/** A number from the environment, or undefined for the library's own: the tests shorten them. */
const knob = (v) => (v === undefined || v === '' ? undefined : Number(v));

export class Tenant extends DurableObject {
  /** The database, as the promise of it: opened once, by whichever request came first. */
  #db = null;
  /** Calls using a database now, and the one `maintain` asked to be opened again. */
  #holders = 0;
  #reload = null;

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
   * `fn` with the database. The last call to let go of one `maintain` asked
   * to be opened again drops it, and the next call restores a new one: let
   * go of while a request still used it, its writes would have gone out
   * beside the new one's, two writers of one storage.
   */
  async #use(fn) {
    const opened = this.db();
    const db = await opened;
    this.#holders++;
    try {
      return await fn(db, opened);
    } finally {
      if (--this.#holders === 0 && this.#reload === opened) this.#drop(opened, db);
    }
  }

  /** Lets the database go -- its module's memory with it -- for the next call to restore. */
  #drop(opened, db) {
    if (this.#db !== opened) return;
    this.#db = null;
    this.#reload = null;
    db.close();
  }

  /** What `maintain` and `schedule` are told, from the environment where it says. */
  #options() {
    const { GRACE, EVERY, SWEEP } = this.env;
    return { grace: knob(GRACE), every: knob(EVERY), sweep: knob(SWEEP) };
  }

  /**
   * Runs FenecQL, and keeps what it wrote before the answer goes, as a
   * server's `--sync always` would. A database whose writes were not kept
   * is let go of, and the next request restores what storage holds: kept
   * in memory, it answered with writes the storage never had.
   */
  query(sql, params) {
    return this.#use(async (db, opened) => {
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
        this.#drop(opened, db);
        return { status: 503, body: { error: `not stored: ${e.message}` } };
      }
      const quiet = knob(this.env.QUIET) ?? QUIET;
      // A write may bring maintenance due: a row that will expire, an
      // image owed. Nothing else does, so a read sets nothing, and neither
      // does a write that brings nothing due: `setAlarm` is a row written.
      if (wrote > 0 && quiet > 0) {
        // The write is kept: an alarm not set is set by the next write.
        await schedule(db, this.ctx.storage, { quiet, ...this.#options() }).catch((e) =>
          console.error(`fenecdb: no alarm set: ${e.message}`),
        );
      }
      if (failed) return { status: 400, body: { error: failed.message } };
      return { status: 200, body: out };
    });
  }

  /**
   * Deletes the rows past their time, writes a new image when the rule
   * says the writes since are worth one, and sets the alarm again for what
   * comes due next, or none. Thrown, the alarm is tried again, with the
   * database restored from what storage holds.
   */
  async alarm() {
    await this.#use(async (db, opened) => {
      try {
        const done = await maintain(db, this.ctx.storage, this.#options());
        // Its module's memory holds dead records an image left out: the
        // next call restores a new one, without them.
        if (done.reload) this.#reload = opened;
      } catch (e) {
        this.#drop(opened, db);
        throw e;
      }
    });
  }

  /** A new image now, for the benchmark. */
  checkpoint(options) {
    return this.#use(async (db, opened) => {
      try {
        return await checkpoint(db, this.ctx.storage, options);
      } catch (e) {
        this.#drop(opened, db);
        throw e;
      }
    });
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
