// Both ways the app opens its database, headless in Node: in the page
// (an in-memory IndexedDB standing in for the browser's), and synced with
// the fenec-server at FENEC_URL, which another client writes to meanwhile.
import 'fake-indexeddb/auto';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { connect } from '@fenecdb/web';
import { local, synced } from '../src/open.js';
import { notes, embed } from '../src/tables.js';

const check = (step: string, ok: boolean) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${step}`);
  if (!ok) process.exit(1);
};
const wasm = readFileSync(fileURLToPath(import.meta.resolve('@fenecdb/web/fenec.wasm')));
const url = process.env.FENEC_URL ?? 'http://127.0.0.1:8080';
const token = process.env.FENEC_TOKEN ?? 'secret';

/** Resolves once `live` hands over rows `pass` accepts, or fails after 5 s. */
function until<T>(live: (cb: (rows: T[]) => void) => () => void, pass: (rows: T[]) => boolean) {
  return new Promise<boolean>((resolve) => {
    const timer = setTimeout(() => (stop(), resolve(false)), 5000);
    const stop = live((rows) => {
      if (pass(rows)) {
        clearTimeout(timer);
        queueMicrotask(() => stop());
        resolve(true);
      }
    });
  });
}

for (const mode of ['local', 'synced'] as const) {
  const db = mode === 'local' ? await local(wasm) : await synced(url, token, wasm, { leader: false });
  const q = db.from(notes).select('id', 'title', 'tags', 'done');
  check(`${mode}: seeded 4 notes`, (await db.from(notes).count()) === 4);
  check(`${mode}: newest first`, (await q.order('at', 'desc').first())?.title === 'Book club');
  check(`${mode}: by tag`, (await q.where('tags', 'has', 'work').order('at', 'desc').rows()).map((r) => r.title).join() === 'Book flights,Release checklist');
  check(`${mode}: match`, (await q.match('body', 'release docs').first())?.title === 'Release checklist');
  check(`${mode}: near`, (await q.near('embed', embed('flights to Istanbul')).first())?.title === 'Book flights');
  check(`${mode}: fuse`, (await q.match('body', 'desert fox').near('embed', embed('desert fox')).fuse().first())?.title === 'Book club');

  // The live query sees a write: this page's own, or -- synced -- another client's, through the server.
  const open = q.where('done', false);
  const call = { title: 'Call mom', body: 'Ask about the weekend.', tags: ['home'], done: false, at: new Date(), embed: embed('Call mom') };
  const seen = until<{ title: string | null }>(
    (cb) => db.live(open, cb),
    (rows) => rows.some((r) => r.title === 'Call mom'),
  );
  if (mode === 'local') await db.from(notes).insert(call);
  else await (await connect(url, { token })).from('notes').insert(call);
  check(`${mode}: live query`, await seen);

  await db.from(notes).where('title', 'Groceries').update({ done: true });
  check(`${mode}: done`, (await open.rows()).length === 3);
  if ('pushed' in db) {
    await db.pushed();
    const server = connect(url, { token });
    check('synced: the server has the write', (await server.from('notes').where('done', false).count()) === 3);
  } else {
    await new Promise((r) => setTimeout(r, 50)); // the persist the live query started
    const again = await local(wasm);
    check('local: persisted across a reopen', (await again.from(notes).count()) === 5);
  }
}
process.exit(0);
