# @fenecdb/cloudflare

fenecdb in a Cloudflare Durable Object. The database lives in the object's
memory and is kept in its storage the way a file would hold it: an image,
then the writes since. After an eviction or a deploy, the object comes back
with its database.

```js
import { DurableObject } from 'cloudflare:workers';
import { Fenec } from '@fenecdb/web';
import { maintain, persist, release, restore, schedule } from '@fenecdb/cloudflare';
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
      // Refused once nothing of it is on its way to storage, so the next
      // database reads all this one stored.
      const wrote = await persist(db, this.ctx.storage).catch((e) => {
        if (this.#db === opened) this.#db = null;
        throw e;
      });
      // The alarm, once something is due: a row that will expire, an
      // image owed. Nothing else sets it: `setAlarm` is a row written. The
      // write is stored either way, and the next write sets an alarm not
      // set: an error here would have the client send it again.
      if (wrote > 0) await schedule(db, this.ctx.storage).catch((e) => console.error(e));
    }
  }

  async alarm() {
    const opened = this.db();
    const db = await opened;
    // Rows past their time deleted, an image when one is worth it, the
    // alarm set again for what comes due next -- or none.
    const done = await maintain(db, this.ctx.storage).catch(async (e) => {
      // Storage failed under it, or its sweep did: let go of, for the try
      // Cloudflare makes next to restore another -- once nothing of this
      // one is on its way to storage, and nothing more will be.
      await release(db);
      if (this.#db === opened) this.#db = null;
      throw e;
    });
    // Stored, but the alarm not set again: thrown, it is tried again.
    if (done.error) throw done.error;
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
- `schedule(fenec, storage, { quiet, every })`, after a write, sets the
  object's alarm when maintenance has come due, and `maintain(fenec,
  storage, { grace, every, sweep, min })` is the alarm's work
  ([Maintenance](#maintenance)). `due(fenec, storage)` says when it next
  has any, for an object that sets its alarm itself.
- `release(fenec)` lets a database go before the object opens it again:
  nothing it holds is stored after -- one never kept, too, and a `restore`
  into it is refused -- and it resolves once a flush under way has
  settled. A refused `persist`, `checkpoint` or `maintain` likewise
  answers once none is, so the database restored next reads all the first
  stored and is the only one writing.
- `checkpoint(fenec, storage, { key, min })` writes a new image now, the
  graphs in it, when the writes since the last one reach `min` bytes (64
  KiB by default). A start then restores the graph rather than link every
  vector written since -- 157 ms against 175 for the first `near` over
  50 000 rows of 128 dimensions -- and reads the image's rows alone.

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

## Maintenance

A server sweeps the rows past their `@ttl`, compacts its file and keeps
its graphs on threads of its own. An object has an alarm, one, which is a
row written each time it is set and wakes the object -- restored, if it
was evicted -- each time it rings. So the work is one call the alarm makes,
`maintain`, and the alarm is set only when there is some:

- `schedule(fenec, storage)`, after a write, sets it once something has
  come due -- a row that will expire, an image owed -- `quiet` (5 s) after
  the write at the soonest, and not when one is set no later than `every`
  (60 s) after that. A write that brings nothing due sets none, and nor
  does a read: an idle object costs nothing.
- `maintain(fenec, storage)` deletes the rows a minute (`grace`) past their
  time, 10 000 a call (`sweep`), as ordinary deletes stored as any write
  is; until then `expired()` reads them, as on a server, for a reaper to
  give back what they held. Past the bound, the alarm rings again at once:
  10 000 deletes held the object about 5 ms in the module.
- It writes a new image when the writes since the last reach half of it
  (and 64 KiB, `min`) -- where `persist` folds them, in a request -- or
  half of what storage holds is dead and a mebibyte: what a sweep or
  deletes leave, which the writes' size alone does not see. Woken to sweep
  and nothing more, it stores the deletes, one row, and no image.
- Once `schedule` has been called, a request folds the writes into an
  image only past the whole image, not half: the alarm takes that image,
  and no request waits for it.
- It sets the alarm again for the next row's time and `grace`, no sooner
  than `every` after this sweep, or sets none.

Each sweep is at most two rows -- its deletes and the alarm set again --
and one a minute at most (`every`), so rows expiring a few a second cost
two rows a minute rather than two a row.

| rows written (rows.mjs, `wrangler dev`) | an alarm after every write, a checkpoint past 64 KiB | maintenance |
| --- | ---: | ---: |
| 30 bursts of 20 single-row writes over 10 000 × 128, a second's quiet after each | 651-652 (1.085-1.087 a write) | **600** (1.000) |
| 12 bursts of 500 rows over the same | 100 (8.3 a burst) | **19** (1.6) |
| 20 000 rows past their time, swept 10 000 an alarm | never deleted | **2**, and 2 alarms |

The rows a burst writes are the same either way; the difference is the
alarm set after each, and the image it wrote once 64 KiB had been written
since the last, 5 to 7 rows of a 5.7 MB image and up each time. With
`every` shortened to a second, 2 000 rows expiring a second for ten took 10
alarms, and 34 rows with their writes'.

The module's memory never shrinks, and holds every version of a record
written since it was opened: what only a new instance gives back. Once the
dead versions reach half of what the module holds (between 4 and 16 MiB),
`maintain` writes an image without them if storage has none and answers
`reload: true`: let the database go, and restore a new one. The example
does, as it lets one go on any failure (`#retire`): calls arriving wait
while those holding it finish -- a write on its way to storage among them
-- then it is released and closed and a new one restored. Let go of at once,
an alarm whose `getAlarm` failed dropped a database while one request's
write waited to be durable and another's waited behind it: the next
request restored a second database and wrote, the first then stored the
waiting write under the same id, and the second's answered write was gone
on the next restore. An alarm that cannot be set is `maintain`'s `error`,
no failure: what it did is stored. In Node (memory.mjs), 50 000 rows of 128 dimensions with three
in five past their time held 42.7 MB of module, as each of two sweeps of
10 000 left it, the JavaScript 0.1 MB above; the third left half of
storage dead, and its image took the module to 51.6 MB -- making one costs
what a checkpoint's does -- and asked for a reload: the module restored
from it held 19.2 MB. The 20 000 rows left, written over 1 000 a request,
brought an image due once more, at 46.9 MB, and the reload after it held
19.9.

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
under workerd, stopping and starting it over the same storage. It calls
`schedule` after a write and `maintain` from its alarm, and lets the
database go when `maintain` asks for a reload; `QUIET`, `GRACE`, `EVERY`
and `SWEEP` in its environment set the times and the bound, which the
test shortens to see an idle object set no alarm, 350 rows swept over four
alarms and kept across a restart, an image written by the rule and none
without it, and a reload.
