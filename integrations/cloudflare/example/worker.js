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
import { checkpoint, maintain, persist, release, restore, schedule } from '../index.js';

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
  /** Calls using the database now, and what to call once none does, while it is let go of. */
  #holders = 0;
  #idle = null;

  db() {
    return (this.#db ?? this.#hold(this.#restored()));
  }

  /**
   * A database opened and restored, with every other event held back:
   * opened by whichever request found the object cold, two requests could
   * each restore an engine of their own and append to the storage as if
   * alone.
   */
  #restored() {
    return this.ctx.blockConcurrencyWhile(async () => {
      // A Worker imports the module compiled, and `open` takes it so.
      const db = await Fenec.open(wasm);
      await restore(db, this.ctx.storage);
      return db;
    });
  }

  /** `opening` as the database, until it fails: the next call opens one again. */
  #hold(opening) {
    this.#db = opening;
    opening.catch(() => {
      if (this.#db === opening) this.#db = null;
    });
    return opening;
  }

  /** `fn` with the database, counted among the calls holding it. */
  async #use(fn) {
    const opened = this.db();
    const db = await opened;
    this.#holders++;
    try {
      return await fn(db, opened);
    } finally {
      if (--this.#holders === 0) this.#idle?.();
    }
  }

  /**
   * Lets the database go and restores it again from storage -- once its
   * writes were not stored, once `maintain` failed, or asked for a module
   * without the dead records it holds. Calls from now on wait while the
   * ones holding it finish, a write on its way to storage among them;
   * then it is released -- nothing of it is stored after -- and closed, and
   * a new one restored. Let go of at once, an alarm whose `getAlarm` had
   * failed dropped it while a request's flush waited to be durable and
   * another's waited behind it: the next request restored a second
   * database and wrote, then the first stored the waiting write over the
   * same id, and the second's write, answered, was gone on the next
   * restore.
   */
  #retire(opened, db) {
    if (this.#db !== opened) return;
    const idle = this.#holders === 0 ? Promise.resolve() : new Promise((done) => (this.#idle = done));
    this.#hold(
      idle.then(async () => {
        this.#idle = null;
        await release(db);
        db.close();
        return this.#restored();
      }),
    );
  }

  /** What `maintain` and `schedule` are told, from the environment where it says. */
  #options() {
    const { GRACE, EVERY, SWEEP } = this.env;
    return { grace: knob(GRACE), every: knob(EVERY), sweep: knob(SWEEP) };
  }

  /**
   * Runs FenecQL, and keeps what it wrote before the answer goes, as a
   * server's `--sync always` would. A database whose writes were not kept
   * is let go of, and restored from what storage holds: kept in memory, it
   * answered with writes the storage never had.
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
        this.#retire(opened, db);
        return { status: 503, body: { error: `not stored: ${e.message}` } };
      }
      const quiet = knob(this.env.QUIET) ?? QUIET;
      // A write may bring maintenance due: a row that will expire, an
      // image owed. Nothing else does, so a read sets nothing, and neither
      // does a write that brings nothing due: `setAlarm` is a row written.
      if (wrote > 0 && quiet > 0) {
        // The write is kept: an alarm not set is set by the next write,
        // and an error here answered for a stored write would have its
        // client send it again.
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
   * comes due next, or none. Thrown, Cloudflare tries the alarm again.
   */
  async alarm() {
    const done = await this.#use(async (db, opened) => {
      let done;
      try {
        done = await maintain(db, this.ctx.storage, this.#options());
      } catch (e) {
        // Its storage failed, or a statement of the sweep: tried again over
        // a database restored from what storage holds.
        this.#retire(opened, db);
        throw e;
      }
      // Its module's memory holds dead records an image left out: restored
      // again, without them.
      if (done.reload) this.#retire(opened, db);
      return done;
    });
    // What it did is stored, and the alarm was not set again: thrown, the
    // alarm is tried again, the database kept.
    if (done.error) throw done.error;
  }

  /** A new image now, for the benchmark. */
  checkpoint(options) {
    return this.#use(async (db, opened) => {
      try {
        return await checkpoint(db, this.ctx.storage, options);
      } catch (e) {
        this.#retire(opened, db);
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
