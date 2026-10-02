// The two ways to open the app's database. The components cannot tell them
// apart: both have `from` and `live`, and the same queries run on both.
import { Fenec, connect, persist, restore, sync, type SyncOptions } from '@fenecdb/web';
import { notes, SEEDS, embed } from './tables.js';

type NotesQuery = {
  count(): Promise<number>;
  insert(doc: (typeof notes)['$inferInsert']): Promise<unknown>;
};

async function seed(q: NotesQuery) {
  if ((await q.count()) > 0) return;
  for (const s of SEEDS) await q.insert({ ...s, embed: embed(`${s.title} ${s.body}`) });
}

/** A database in the page, kept in this browser's IndexedDB. */
export async function local(wasm: string | BufferSource) {
  const db = await Fenec.open(wasm, { schema: { notes } });
  if (!(await restore(db, 'notes'))) await seed(db.from(notes));
  // Kept after every write to notes: only the writes since, not the image.
  db.live(db.from(notes).select('id').limit(1), () => void persist(db, 'notes'));
  return db;
}

/**
 * A replica of the server's notes: reads from the page, writes shown at
 * once and sent, other clients' writes streamed in. The server owns the
 * schema; here the app brings it there first, and its seeds (with the
 * server's token), as a deploy step would.
 */
export async function synced(url: string, token: string, wasm: string | BufferSource, extra: Partial<SyncOptions> = {}) {
  const server = await connect(url, { token, schema: { notes }, migrate: true });
  await seed(server.from(notes));
  const db = await sync({ url, token, wasm, schema: { notes }, shapes: [{ collection: notes }], persist: 'notes-replica', ...extra });
  await db.ready();
  return db;
}
