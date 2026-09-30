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
import { persist, restore } from '../index.js';

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
    return out;
  }
}

export default {
  async fetch(req, env) {
    const m = new URL(req.url).pathname.match(/^\/t\/([a-z0-9_-]+)\/query$/);
    if (!m || req.method !== 'POST') return new Response('not found', { status: 404 });
    const { sql, params = [] } = await req.json();
    const tenant = env.TENANT.get(env.TENANT.idFromName(m[1]));
    try {
      return Response.json(await tenant.query(sql, params));
    } catch (e) {
      return Response.json({ error: e.message }, { status: 400 });
    }
  },
};
