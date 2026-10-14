# @fenecdb/cloudflare

fenecdb in a Cloudflare Durable Object. The database lives in the object's
memory and is kept in its storage the way a file would hold it: an image,
then the writes since. After an eviction or a deploy, the object comes back
with its database.

```js
import { DurableObject } from 'cloudflare:workers';
import { Fenec } from '@fenecdb/web';
import { persist, restore } from '@fenecdb/cloudflare';
import wasm from '@fenecdb/web/fenec.wasm';

export class Tenant extends DurableObject {
  #db = null;

  db() {
    // Opened once, with every other event held back while it restores.
    return (this.#db ??= this.ctx.blockConcurrencyWhile(async () => {
      const db = await Fenec.open(wasm);
      await restore(db, this.ctx.storage);
      return db;
    }));
  }

  async query(sql, params) {
    const opened = this.db();
    const db = await opened;
    try {
      return db.run(sql, params);
    } finally {
      // Kept before the answer goes -- a statement that failed may follow
      // ones in the same text that wrote -- or let go of: not stored, the
      // database is ahead of its storage, and the next request restores.
      await persist(db, this.ctx.storage).catch((e) => {
        if (this.#db === opened) this.#db = null;
        throw e;
      });
    }
  }
}
```

One object is one database, as a tenant is one file on a fenec-server node:
the object's single thread is the single writer.

- `persist(fenec, storage, { key, piece })` writes the image the first
  time and only the writes since after that, until they outgrow half the
  image (and 64 KiB) and a new image replaces them. It resolves once they
  are durable.
- `restore(fenec, storage, { key })` loads what `persist` kept, and later
  `persist` calls go on from it.
- `checkpoint(fenec, storage, { key, min })` writes a new image now, the
  graphs in it, when the writes since the last one reach `min` bytes (64
  KiB by default): call it from an alarm set after a write. A start then
  restores the graph rather than link every vector written since -- 157 ms
  against 175 for the first `near` over 50 000 rows of 128 dimensions --
  and reads the image's rows alone.

## Where it is kept

A SQLite-backed object -- the only kind a new account can make -- has
`ctx.storage.sql`, and `persist` keeps the database in a table a
generation, `<key>:<gen>` (`fenec:1`, `fenec:2`, ...): the image's rows,
then a row each time it stores writes. An object is billed by the row
written ($1.00 a million past the 50 million a month included), so a write
costs one row and nothing more: no record beside it naming where the
database stands, which the rows themselves say. Rows are cut under the 2
MB a row takes (`ROW`), a 27.6 MB image in 14. A new image is written,
and the table before it dropped, in one `transactionSync`: storage that
stops part way holds the generation before, whole. workerd counts a
dropped table as no rows written where a `DELETE` counts each, so a new
image costs its rows and two for the table: 16 for that image, where its
values took 479 with the deletes of the ones before.

Requests that land while a flush is on its way to being durable go out
together, in one row, after it: a Durable Object takes the next request in
while one waits. Under `wrangler dev`, single-row writes from 8 writers at
once cost 0.52 rows each and from 32 0.15, one writer one (3.4 each
before: a value, a record naming it and the example's alarm). Each `persist`
still resolves only once its own writes are durable, so the object's
output gate covers every answer, and a lone write goes out at once.

A key-value backed object (one made with `new_classes` before SQLite)
has no `sql`, and `persist` keeps the database in values under its 128
KiB (`PIECE`), with a record written last naming the whole generation.

The SQL layout is new in this version, and a database kept by an earlier
one on a SQLite-backed object is in values, which `restore` does not look
at there. To move one, restore through the key-value methods alone and
persist through the storage as it is:

```js
const s = this.ctx.storage;
const values = { get: (k) => s.get(k), put: (k, v) => s.put(k, v), delete: (k) => s.delete(k), list: (o) => s.list(o) };
await restore(db, values);
await persist(db, s);
```

## When storing fails

A `persist` that fails leaves the storage as it was and the database in
memory ahead of it, holding writes nobody kept. Every `persist` on it after
rejects, and the request is answered with the error: drop the database,
and the next request opens it again and restores what storage holds --
answered from memory, it would have shown writes that were never stored.

## Memory

An object holds about 60 000 rows of 128 dimensions, or 12 000 of 768,
within a Worker's 128 MB: the module's memory grows and never gives back,
so it keeps an index's vectors in chunks of a mebibyte rather than one
array doubled as it grows, reads a restored image's documents where they
lie, and makes an image a mebibyte at a time as storage takes it. A
restore copies each row or value into the module's memory as it reads it
(`loadParts`): read whole and joined first, a 27.6 MB image (50 000 rows
of 128) held 60 MB of JavaScript beside the module's 42 at the peak, and
holds 4. See [Serverless and
Cloudflare](https://fenecdb.com/docs/serverless).

## The example

`example/` is a Worker giving each tenant a database of its own:
`POST /t/<tenant>/query` with `{sql, params}`. `npx wrangler dev --config
example/wrangler.jsonc` serves it locally, and `worker.test.js` runs it
under workerd, stopping and starting it over the same storage. It sets an
alarm after a write, one at a time (`setAlarm` is a row written too), and
the alarm checkpoints.
