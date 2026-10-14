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
//     db() {
//       // Opened with every other event held back, once.
//       return (this.fenec ??= this.ctx.blockConcurrencyWhile(async () => {
//         const db = await Fenec.open(wasm);
//         await restore(db, this.ctx.storage);
//         return db;
//       }));
//     }
//     async query(sql, params) {
//       const db = await this.db();
//       const out = db.run(sql, params);
//       await persist(db, this.ctx.storage);
//       return out;
//     }
//   }
//
// One object is one database, as a tenant is one file on a fenec-server node:
// the object's single thread is the single writer. Nothing here imports the
// engine; `fenec` is a `Fenec` from `@fenecdb/web`, opened by the caller.
//
// Two layouts, by what the storage has. A SQLite-backed object -- the only
// kind a new account makes -- has `storage.sql`, and a generation is a table
// of its own, `<key>:<gen>`: the image's rows, then a row a flush of the
// writes since. An object is billed by the row written, $1 a million, so a
// write is one row and nothing more; a new image is written and the table
// before dropped in one transaction, which workerd counts as the image's
// rows and two for the table, and none for the rows dropped -- a `DELETE`
// counts each. A key-value backed object has no `sql`: there the image and
// the writes are cut into values under its 128 KiB, and a record written
// after them names the whole generation.

/**
 * The largest value of a key-value backed object's image or writes: it
 * takes 128 KiB with the key.
 */
export const PIECE = 128 * 1024 - 512;

/**
 * The largest row of a SQLite-backed object's image or writes: it takes a
 * row of 2 MB, the BLOB in it too, and this leaves room for the row's
 * other columns. Cut at PIECE, an image took 16 times the rows.
 */
export const ROW = 2_000_000 - 1024;

/** Once the writes since the image outgrow this share of it, a new image replaces them. */
const FOLD = 0.5;

/**
 * Nor before the writes since reach this, and a checkpoint not before
 * either: a small database folded every few writes otherwise, each fold
 * its image's rows again, and a checkpoint of a 30 MB image for one
 * write is 16 rows where the write was one.
 */
const FLOOR = 64 * 1024;

/**
 * Per database, where it stands in storage and what waits to go there:
 * `image` and `log` its bytes (`pieces` their counts as values), `next`
 * the flush the writes drained since go out with, `flushing` the one under
 * way, `failed` what stopped one.
 */
const kept = new WeakMap();

/**
 * Writes the database into `storage` -- a Durable Object's `ctx.storage`
 * -- under `key`: the image the first time, then only the writes since.
 * Resolves once they are durable (`storage.sync()`), to the bytes the
 * flush carrying them wrote; to 0 when the call had none, after waiting
 * for writes of others still on their way, which it may have read.
 *
 * The writes go out at once, unless a flush before them is still waiting
 * to be durable: then they wait for it, and every request whose writes
 * land meanwhile -- a Durable Object takes in the next while one waits --
 * goes out with them, in one row. A request answers no later than the
 * object's output gate would have let it anyway.
 *
 * A flush that fails leaves the storage as it was, and the database ahead
 * of it: every call after rejects, and the database has to be opened
 * again and restored, as the writes since the last flush were never kept.
 */
export function persist(fenec, storage, options = {}) {
  const [s, fresh] = stateOf(fenec, storage, options);
  if (s.failed) return Promise.reject(failure(s));
  if (take(fenec, s) || fresh) {
    const { done } = s.next;
    kick(s);
    return done;
  }
  return settled(s).then(() => 0);
}

/**
 * Writes a new image, the graphs in it, once the writes kept since the last
 * one reach `min` bytes (64 KiB by default): a restore then takes the graphs
 * as they were rather than link every vector written since, one after
 * another. A Durable Object can call it from an alarm, when no request
 * waits. Resolves to the bytes written, 0 when it wrote no image.
 */
export function checkpoint(fenec, storage, options = {}) {
  const { min = FLOOR } = options;
  const [s, fresh] = stateOf(fenec, storage, options);
  if (s.failed) return Promise.reject(failure(s));
  take(fenec, s);
  const log = s.log + (s.next?.logBytes ?? 0);
  // Nothing since the image, or too little to pay for writing it again.
  if (!fresh && !s.next?.image && (log === 0 || log < min)) {
    kick(s);
    return settled(s).then(() => 0);
  }
  const b = batch(s);
  whole(b);
  kick(s);
  return b.done;
}

/**
 * Loads the database `persist` keeps under `key` into `fenec`, and has
 * later `persist` calls go on from it. Resolves to false when there is none.
 * Each row or value is copied into the module's memory as it is read and
 * let go of (`loadParts`), never joined into one array first.
 */
export async function restore(fenec, storage, { key = 'fenec' } = {}) {
  const found = storage.sql ? keptRows(storage.sql, key) : await keptValues(storage, key);
  if (!found) return false;
  const taken = await load(fenec, found.length, found.parts);
  fenec.journal();
  const s = state(fenec, storage, key);
  // A last write cut short is cut off by the load: appended after, a write
  // would be read back as the rest of it, so the next flush is an image.
  Object.assign(s, found.at, { torn: taken < found.length });
  kept.set(fenec, s);
  return true;
}

// ------------------------------------------------------------- the flushes

function state(fenec, storage, key) {
  return {
    fenec, storage, key, sql: !!storage.sql, piece: 0,
    gen: null, image: 0, log: 0, pieces: [0, 0], torn: false,
    next: null, flushing: null, failed: null,
  };
}

/** The database's state, made -- its journal started, an image due -- when new to `key`. */
function stateOf(fenec, storage, { key = 'fenec', piece } = {}) {
  let s = kept.get(fenec);
  const fresh = s?.key !== key;
  if (fresh) {
    fenec.journal();
    s = state(fenec, storage, key);
    kept.set(fenec, s);
    whole(batch(s));
  }
  s.piece = piece ?? (s.sql ? ROW : PIECE);
  return [s, fresh];
}

const failure = (s) =>
  new Error(`fenecdb: ${s.key} was not stored, and the database holds writes its storage does not: open it again and restore`, {
    cause: s.failed,
  });

/** The flush the writes drained now go out with, made when none waits. */
function batch(s) {
  if (s.next) return s.next;
  const b = { image: null, log: [], logBytes: 0 };
  b.done = new Promise((res, rej) => Object.assign(b, { res, rej }));
  // Rejected with no call left to wait for it, it is no unhandled error.
  b.done.catch(() => {});
  s.next = b;
  return b;
}

/** An image due in `b`, which holds whatever writes it held. */
function whole(b) {
  Object.assign(b, { image: true, log: [], logBytes: 0 });
}

/** The writes since the last drain into the flush `into()` gives; false when there were none. */
function absorb(fenec, into) {
  const { replace, bytes } = fenec.drain();
  if (!replace && bytes.length === 0) return false;
  const b = into();
  // An image taken at the flush holds them already.
  if (b.image !== true) {
    // A `compact`'s image, which the writes after it follow.
    if (replace) Object.assign(b, { image: bytes, log: [], logBytes: 0 });
    else {
      b.log.push(bytes);
      b.logBytes += bytes.length;
    }
  }
  return true;
}

const take = (fenec, s) => absorb(fenec, () => batch(s));

/** Resolves once what was drained so far is stored, rejects when it was not. */
function settled(s) {
  if (s.next) return s.next.done;
  if (s.flushing) return s.flushing.then(() => {
    if (s.failed) throw failure(s);
  });
  return Promise.resolve();
}

/**
 * The flush waiting, out now unless the one before is not yet durable:
 * then once it is, with every write drained meanwhile. Waited out with a
 * timer instead, a turn of the event loop for the requests sent to land,
 * a lone write took 3.2 ms under `wrangler dev` against 1.4; waiting for
 * the flush before costs nothing the output gate did not.
 */
function kick(s) {
  if (s.flushing || !s.next) return;
  const b = s.next;
  s.next = null;
  // Synchronous through SQL: the rows are written before `persist`
  // returns, so nothing runs between the drain and them.
  const run = (async () => {
    if (s.failed) throw failure(s);
    absorb(s.fenec, () => b);
    if (s.torn || (b.image === null && s.log + b.logBytes > Math.max(s.image * FOLD, FLOOR))) whole(b);
    const n = s.sql ? flushRows(s, b) : await flushValues(s, b);
    await s.storage.sync?.();
    return n;
  })();
  const done = run.then(b.res, (e) => {
    s.failed ??= e;
    b.rej(e);
  });
  s.flushing = done;
  done.then(() => {
    s.flushing = null;
    kick(s);
  });
}

/**
 * The image from now, the writes drained before it let go of: it holds
 * them. The module takes the image at the first chunk, which is asked for
 * here, so nothing lands between the drain and it.
 */
function snapshot(fenec) {
  fenec.drain();
  if (!fenec.snapshotChunks) return [fenec.snapshot()];
  const it = fenec.snapshotChunks();
  const first = it.next();
  return (function* () {
    if (first.done) return;
    yield first.value;
    yield* it;
  })();
}

/**
 * Cuts what it is handed into pieces of at most `size` -- `hint` bytes in
 * all, when known -- handing `emit` each as it fills, a view of a buffer
 * used again after: a mebibyte chunk of an image is half a row. A piece
 * that lies whole in one part is handed over as a view of it, not copied.
 */
function cutter(size, emit, hint = size) {
  const cap = Math.max(1, Math.min(size, hint));
  let held = null;
  let at = 0;
  let total = 0;
  return {
    add(part) {
      total += part.length;
      for (let i = 0; i < part.length; ) {
        const n = Math.min(cap - at, part.length - i);
        if (at === 0 && n === cap) emit(part.subarray(i, i + n));
        else {
          held ??= new Uint8Array(cap);
          held.set(part.subarray(i, i + n), at);
          at += n;
          if (at === cap) {
            emit(held);
            at = 0;
          }
        }
        i += n;
      }
    },
    end() {
      if (at) emit(held.subarray(0, at));
      at = 0;
      return total;
    },
  };
}

// ---------------------------------------------------- a SQLite-backed object

/** The table of generation `gen`, its name quoted: a key is any text. */
const table = (key, gen) => `"${`${key}:${gen}`.replaceAll('"', '""')}"`;

/** The generations kept under `key`, ascending: one, but for a table a crash left. */
function generations(sql, key) {
  const head = `${key}:`;
  const gens = [];
  for (const [name] of sql.exec("SELECT name FROM sqlite_master WHERE type = 'table' AND substr(name, 1, ?) = ?", head.length, head).raw()) {
    const gen = name.slice(head.length);
    if (/^[1-9][0-9]*$/.test(gen)) gens.push(Number(gen));
  }
  return gens.sort((a, b) => a - b);
}

/** Rows of `parts` into table `t`, the image's (`image` 1) or the writes' (0); returns their bytes. */
function insert(sql, t, image, parts, size, hint) {
  const cut = cutter(size, (bytes) => sql.exec(`INSERT INTO ${t} (image, data) VALUES (?, ?)`, image, bytes), hint);
  for (const p of parts) cut.add(p);
  return cut.end();
}

function flushRows(s, b) {
  const { storage, key } = s;
  const { sql } = storage;
  if (!b.image) {
    if (b.logBytes === 0) return 0;
    storage.transactionSync(() => insert(sql, table(key, s.gen), 0, b.log, s.piece, b.logBytes));
    s.log += b.logBytes;
    return b.logBytes;
  }
  // A new generation in one transaction: the table made and filled, the
  // ones before dropped. Storage that stops part way holds the one before.
  const at = storage.transactionSync(() => {
    const before = generations(sql, key);
    const gen = (before.at(-1) ?? 0) + 1;
    const t = table(key, gen);
    sql.exec(`CREATE TABLE ${t} (id INTEGER PRIMARY KEY, image INTEGER NOT NULL, data BLOB NOT NULL)`);
    const image = insert(sql, t, 1, b.image === true ? snapshot(s.fenec) : [b.image], s.piece);
    const log = insert(sql, t, 0, b.log, s.piece, b.logBytes);
    for (const g of before) sql.exec(`DROP TABLE ${table(key, g)}`);
    return { gen, image, log };
  });
  Object.assign(s, at, { torn: false });
  return at.image + at.log;
}

/** What `persist` keeps under `key` in SQL, to load: `{length, parts, at}`, or null. */
function keptRows(sql, key) {
  const gen = generations(sql, key).at(-1);
  if (gen === undefined) return null;
  const t = table(key, gen);
  const [length, image] = sql
    .exec(`SELECT coalesce(sum(length(data)), 0), coalesce(sum(CASE WHEN image THEN length(data) END), 0) FROM ${t}`)
    .raw()
    .next().value;
  if (length === 0) return null;
  // Read a row at a time, each copied into the module as it comes.
  const parts = function* () {
    for (const [data] of sql.exec(`SELECT data FROM ${t} ORDER BY id`).raw()) yield data;
  };
  return { length, parts, at: { gen, image, log: length - image } };
}

// ------------------------------------------------ a key-value backed object

const meta = (key) => `${key}:meta`;
const prefix = (key, gen, part) => `${key}:${gen}:${part}:`;
const at = (key, gen, part, n) => prefix(key, gen, part) + String(n).padStart(9, '0');

/**
 * `parts` into pieces of generation `gen`'s `part` from piece `first` on;
 * returns `[next piece, bytes]`. Each part's puts are waited for before the
 * next is taken: a part of an image is made in the module as it is asked
 * for, a mebibyte at a time, and made all at once the image did not fit a
 * Worker's 128 MB past 30 000 rows of 128 dimensions.
 */
async function putAll(storage, key, gen, part, first, parts, size, hint) {
  let n = first;
  let writes = [];
  // A copy each: a view would be cloned whole, and the cutter's buffer
  // is written again.
  const cut = cutter(size, (bytes) => writes.push(storage.put(at(key, gen, part, n++), bytes.slice())), hint);
  for (const p of parts) {
    cut.add(p);
    await Promise.all(writes);
    writes = [];
  }
  const bytes = cut.end();
  await Promise.all(writes);
  return [n, bytes];
}

/**
 * Each image goes under a generation of its own and the record saying
 * which generation is whole is written after it, so storage stopped half
 * way holds the last whole one: a Durable Object combines the puts of a
 * call into one atomic write, and this does not lean on it.
 */
async function flushValues(s, b) {
  const { storage, key } = s;
  if (s.gen === null) s.gen = (await storage.get(meta(key)))?.gen ?? 0;
  if (!b.image) {
    if (b.logBytes === 0) return 0;
    const log = await putAll(storage, key, s.gen, 'l', s.pieces[1], b.log, s.piece, b.logBytes);
    const record = { v: 1, gen: s.gen, image: [s.pieces[0], s.image], log: [log[0], s.log + log[1]] };
    await storage.put(meta(key), record);
    Object.assign(s, { log: record.log[1], pieces: [s.pieces[0], log[0]] });
    return log[1];
  }
  const gen = s.gen + 1;
  const image = await putAll(storage, key, gen, 'i', 0, b.image === true ? snapshot(s.fenec) : [b.image], s.piece);
  const log = await putAll(storage, key, gen, 'l', 0, b.log, s.piece, b.logBytes);
  await storage.put(meta(key), { v: 1, gen, image, log });
  const before = s.gen;
  Object.assign(s, { gen, image: image[1], log: log[1], pieces: [image[0], log[0]], torn: false });
  // What the generation before held is nothing now: let it go.
  if (before > 0) await drop(storage, key, before);
  return image[1] + log[1];
}

/** Deletes a generation's pieces, 128 keys a call as a Durable Object takes them. */
async function drop(storage, key, gen) {
  for (const part of ['i', 'l']) {
    const keys = [...(await storage.list({ prefix: prefix(key, gen, part) })).keys()];
    for (let i = 0; i < keys.length; i += 128) await storage.delete(keys.slice(i, i + 128));
  }
}

/** Pieces listed a page at a time, about 2 MB: read whole, they stood in the isolate at once. */
const PAGE = 16;

/** What `persist` keeps under `key` as values, to load: `{length, parts, at}`, or null. */
async function keptValues(storage, key) {
  const record = await storage.get(meta(key));
  if (!record) return null;
  const { gen, image, log } = record;
  const parts = async function* () {
    for (const [part, count] of [['i', image[0]], ['l', log[0]]]) {
      for (let n = 0; n < count; ) {
        const page = await storage.list({ prefix: prefix(key, gen, part), start: at(key, gen, part, n), limit: Math.min(PAGE, count - n) });
        const from = n;
        for (const [k, v] of page) {
          if (k !== at(key, gen, part, n)) break;
          yield v;
          n++;
        }
        if (n === from) throw new Error(`fenecdb: ${key} is missing pieces of generation ${gen} (${part})`);
      }
    }
  };
  return { length: image[1] + log[1], parts, at: { gen, image: image[1], log: log[1], pieces: [image[0], log[0]] } };
}

/**
 * `fenec.loadParts` where the module's glue has it, and the parts joined
 * and loaded at once where it is older.
 */
async function load(fenec, length, parts) {
  if (fenec.loadParts) return fenec.loadParts(length, parts);
  const bytes = new Uint8Array(length);
  let off = 0;
  for await (const p of parts()) {
    const v = p instanceof ArrayBuffer ? new Uint8Array(p) : p;
    if (off + v.length > length) break;
    bytes.set(v, off);
    off += v.length;
  }
  if (off !== length) throw new Error(`fenecdb: what is kept holds ${off} bytes where ${length} were written`);
  return fenec.loadAsync(bytes);
}
