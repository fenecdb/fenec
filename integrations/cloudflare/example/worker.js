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

export class Tenant extends DurableObject {
  async db() {
    if (!this.fenec) {
      // A Worker imports the module compiled, and `open` takes it so.
      this.fenec = await Fenec.open(wasm);
      await restore(this.fenec, this.ctx.storage);
    }
    return this.fenec;
  }

  async query(sql, params) {
    const db = await this.db();
    const out = db.run(sql, params);
    // Kept before the answer goes, as a server's `--sync always` would.
    await persist(db, this.ctx.storage);
    // A new image once writes stop for a while, for the next start to
    // restore the graph rather than link what came after it.
    await this.ctx.storage.setAlarm(Date.now() + 10_000);
    return out;
  }

  async alarm() {
    await checkpoint(await this.db(), this.ctx.storage);
  }

  /** The alarm's work at once, for the benchmark. */
  async checkpoint() {
    return checkpoint(await this.db(), this.ctx.storage);
  }
}

export default {
  async fetch(req, env) {
    const m = new URL(req.url).pathname.match(/^\/t\/([a-z0-9_-]+)\/(query|checkpoint)$/);
    if (!m || req.method !== 'POST') return new Response('not found', { status: 404 });
    const tenant = env.TENANT.get(env.TENANT.idFromName(m[1]));
    try {
      if (m[2] === 'checkpoint') return Response.json({ bytes: await tenant.checkpoint() });
      const { sql, params = [] } = await req.json();
      return Response.json(await tenant.query(sql, params));
    } catch (e) {
      return Response.json({ error: e.message }, { status: 400 });
    }
  },
};
