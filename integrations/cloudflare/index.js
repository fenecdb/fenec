// fenecdb in a Cloudflare Durable Object: the database in the object's
// memory, kept in its storage as a file would hold it -- an image, then the
// writes since -- so the object comes back with it after an eviction or a
// deploy.
//
//   import { DurableObject } from 'cloudflare:workers';
//   import { Fenec } from '@fenecdb/web';
//   import { persist, restore } from '@fenecdb/cloudflare';
//   import wasm from '@fenecdb/web/fenec.wasm';
//
//   export class Tenant extends DurableObject {
//     async db() {
//       if (!this.fenec) {
//         this.fenec = await Fenec.open(wasm);
//         await restore(this.fenec, this.ctx.storage);
//       }
//       return this.fenec;
//     }
//     async query(sql, params) {
//       const db = await this.db();
//       const out = db.run(sql, params);
//       await persist(db, this.ctx.storage);
//       return out;
//     }
//   }
//
// One object is one database, as a tenant is one file on a fenec-pg node:
// the object's single thread is the single writer. Nothing here imports the
// engine; `fenec` is a `Fenec` from `@fenecdb/web`, opened by the caller.

/**
 * The largest value a piece is: a key-value backed object takes values of
 * 128 KiB at most, a SQLite-backed one 2 MB with the key. A piece under the
 * smaller works on both; a SQLite-backed object can pass `piece` larger, for
 * fewer rows.
 */
export const PIECE = 128 * 1024 - 512;

/** Once the writes since the image outgrow this share of it, a new image replaces them. */
const FOLD = 0.5;

/** Per database, where it stands in storage: `{key, gen, image, log}`. */
const kept = new WeakMap();

const meta = (key) => `${key}:meta`;
const prefix = (key, gen, part) => `${key}:${gen}:${part}:`;
const at = (key, gen, part, n) => prefix(key, gen, part) + String(n).padStart(9, '0');

/** `bytes` cut into pieces of `size`, stored from piece `first` on. */
function pieces(storage, key, gen, part, first, bytes, size) {
  const writes = [];
  let n = first;
  for (let i = 0; i < bytes.length; i += size, n++) {
    // A copy each: a view of the whole would be cloned whole.
    writes.push(storage.put(at(key, gen, part, n), bytes.slice(i, i + size)));
  }
  return { writes, next: n };
}

/**
 * Writes the database into `storage` -- a Durable Object's `ctx.storage`,
 * or anything with its `get`, `put`, `delete` and `list` -- under `key`:
 * the image the first time, then only the writes since the last call.
 * Returns the bytes written.
 *
 * Each image goes under a generation of its own and the record saying
 * which generation is whole is written after it, so storage stopped half
 * way holds the last whole one: a Durable Object combines the puts of a
 * call into one atomic write, and this does not lean on it.
 */
export async function persist(fenec, storage, { key = 'fenec', piece = PIECE } = {}) {
  let s = kept.get(fenec);
  let image = null;
  let log = null;
  if (s?.key !== key) {
    fenec.journal();
    image = fenec.snapshot();
    const old = await storage.get(meta(key));
    s = { key, gen: old?.gen ?? 0, image: [0, 0], log: [0, 0] };
    kept.set(fenec, s);
  } else {
    const { replace, bytes } = fenec.drain();
    if (replace) image = bytes;
    else if (bytes.length === 0) return 0;
    else if (s.log[1] + bytes.length > s.image[1] * FOLD) image = fenec.snapshot();
    else log = bytes;
  }
  try {
    if (image) {
      const gen = s.gen + 1;
      const { writes, next } = pieces(storage, key, gen, 'i', 0, image, piece);
      await Promise.all(writes);
      const record = { v: 1, gen, image: [next, image.length], log: [0, 0] };
      await storage.put(meta(key), record);
      const before = s.gen;
      Object.assign(s, record);
      // What the generation before held is nothing now: let it go.
      if (before > 0) await drop(storage, key, before);
    } else {
      const { writes, next } = pieces(storage, key, s.gen, 'l', s.log[0], log, piece);
      await Promise.all(writes);
      const record = { v: 1, gen: s.gen, image: s.image, log: [next, s.log[1] + log.length] };
      await storage.put(meta(key), record);
      Object.assign(s, record);
    }
  } catch (e) {
    // The drained writes are nowhere now; the next call stores an image.
    kept.delete(fenec);
    throw e;
  }
  return (image ?? log).length;
}

/** Deletes a generation's pieces, 128 keys a call as a Durable Object takes them. */
async function drop(storage, key, gen) {
  for (const part of ['i', 'l']) {
    const keys = [...(await storage.list({ prefix: prefix(key, gen, part) })).keys()];
    for (let i = 0; i < keys.length; i += 128) await storage.delete(keys.slice(i, i + 128));
  }
}

/** A part's first `count` pieces, in order, as one run of bytes. */
async function read(storage, key, gen, part, [count, bytes]) {
  const out = new Uint8Array(bytes);
  if (count === 0) return out;
  const got = await storage.list({ prefix: prefix(key, gen, part) });
  let n = 0;
  let off = 0;
  for (const [k, v] of got) {
    // Pieces past the record are a write it never took: left for the next
    // one to write over.
    if (k !== at(key, gen, part, n)) break;
    if (n === count) break;
    out.set(v, off);
    off += v.length;
    n++;
  }
  if (n !== count || off !== bytes) {
    throw new Error(`fenecdb: ${key} is missing pieces of generation ${gen} (${part})`);
  }
  return out;
}

/**
 * Loads the database `persist` keeps under `key` into `fenec`, and has
 * later `persist` calls go on from it. Returns false when there is none.
 */
export async function restore(fenec, storage, { key = 'fenec' } = {}) {
  const record = await storage.get(meta(key));
  if (!record) return false;
  const image = await read(storage, key, record.gen, 'i', record.image);
  const log = await read(storage, key, record.gen, 'l', record.log);
  let bytes = image;
  if (log.length) {
    bytes = new Uint8Array(image.length + log.length);
    bytes.set(image, 0);
    bytes.set(log, image.length);
  }
  await fenec.loadAsync(bytes);
  fenec.journal();
  kept.set(fenec, { key, gen: record.gen, image: record.image, log: record.log });
  return true;
}
