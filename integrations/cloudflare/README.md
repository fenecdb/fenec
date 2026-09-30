# @fenecdb/cloudflare

fenecdb in a Cloudflare Durable Object. The database lives in the object's
memory and is kept in its storage the way a file would hold it: an image,
then the writes since, each cut into pieces a value can hold. After an
eviction or a deploy, the object comes back with its database.

```js
import { DurableObject } from 'cloudflare:workers';
import { Fenec } from '@fenecdb/web';
import { persist, restore } from '@fenecdb/cloudflare';
import wasm from '@fenecdb/web/fenec.wasm';

export class Tenant extends DurableObject {
  async db() {
    if (!this.fenec) {
      this.fenec = await Fenec.open(wasm);
      await restore(this.fenec, this.ctx.storage);
    }
    return this.fenec;
  }

  async query(sql, params) {
    const db = await this.db();
    const out = db.run(sql, params);
    await persist(db, this.ctx.storage);
    return out;
  }
}
```

One object is one database, as a tenant is one file on a fenec-pg node:
the object's single thread is the single writer.

- `persist(fenec, storage, { key, piece })` writes the image the first
  time and only the writes since after that, until they outgrow half the
  image and a new image replaces them.
- `restore(fenec, storage, { key })` loads what `persist` kept, and later
  `persist` calls go on from it.

Each image is written under a generation of its own, and the record saying
which generation is whole is written last. Storage that stops part way
therefore still holds the last whole database. A Durable Object combines a
call's puts into one atomic write anyway; this does not depend on it.

`example/` is a Worker giving each tenant a database of its own:
`POST /t/<tenant>/query` with `{sql, params}`. `npx wrangler dev --config
example/wrangler.jsonc` serves it locally, and `worker.test.js` runs it
under workerd, stopping and starting it over the same storage.

`checkpoint(fenec, storage)` writes a new image now, the graphs in it:
call it from an alarm once writes stop. A start then restores the graph
rather than link every vector written since the last image, 36 ms against
792 for the first `near` over 10 000 rows of 128 dimensions.

An object holds about 50 000 rows of 128 dimensions, or 8 000 of 768
written a few hundred a request, within a Worker's 128 MB: the module's
memory grows and never gives back, so a large request's buffers stay, and
an image is written a mebibyte at a time so it never stands whole. See
[Serverless and Cloudflare](https://fenecdb.com/docs/serverless).

A value is at most 128 KiB by default, which a key-value backed object
takes. A SQLite-backed object takes up to 2 MB, so `piece` can be larger
there, for fewer rows.
