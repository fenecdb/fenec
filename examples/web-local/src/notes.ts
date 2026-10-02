// The database in the page: opened with the schema, restored from
// IndexedDB, and every write persisted. The page (main.ts) and the smoke
// test (test/smoke.ts) share it.
import { Fenec, persist, restore, type SchemaOf } from '@fenecdb/web';
import { notes, SEEDS, embed } from './tables.js';

export type Db = Fenec<SchemaOf<{ notes: typeof notes }>>;
export const KEY = 'notes'; // the IndexedDB key

/** `wasm` is the module's URL in a page, its bytes in Node. */
export async function open(wasm: string | BufferSource): Promise<Db> {
  const db = await Fenec.open(wasm, { schema: { notes } });
  // The stored database in its place, checked against the schema; or seeds.
  if (!(await restore(db, KEY))) {
    for (const s of SEEDS) await add(db, s.title, s.body, s.tags, s.done, s.at);
  }
  return db;
}

export async function add(db: Db, title: string, body: string, tags: string[], done = false, at: string | Date = new Date()) {
  await db.from(notes).insert({ title, body, tags, done, at, embed: embed(`${title} ${body}`) });
  await persist(db, KEY); // only this write, not the whole image
}

export async function finish(db: Db, id: number) {
  await db.from(notes).where('id', id).update({ done: true });
  await persist(db, KEY);
}

/**
 * What the list shows: newest first, or -- with search words -- `match`
 * and `near` over the toy embedding, their ranks fused.
 */
export function query(db: Db, { words = '', tag = '', open = false } = {}) {
  let q = db.from(notes).select('id', 'title', 'body', 'tags', 'done', 'at');
  if (tag) q = q.where('tags', 'has', tag);
  if (open) q = q.where('done', false);
  return words.trim()
    ? q.match('body', words).near('embed', embed(words)).fuse().limit(20)
    : q.order('at', 'desc').limit(20);
}
