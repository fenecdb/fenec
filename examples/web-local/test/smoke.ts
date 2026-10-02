// The page's logic in Node, headless: an in-memory IndexedDB stands in for
// the browser's, and a second open restores what the first persisted.
import 'fake-indexeddb/auto';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { open, add, finish, query } from '../src/notes.js';
import { notes, embed } from '../src/tables.js';

const check = (step: string, ok: boolean) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${step}`);
  if (!ok) process.exit(1);
};
const wasm = readFileSync(fileURLToPath(import.meta.resolve('@fenecdb/web/fenec.wasm')));

const db = await open(wasm);
check('seeded 4 notes', (await db.from(notes).count()) === 4);
check('toy embedding', [...embed('hello')].flatMap((x, i) => (x ? [i] : [])).join() === '24,36,46,48,62');
check('newest first', (await query(db).rows())[0].title === 'Book club');
check('by tag', (await query(db, { tag: 'work' }).rows()).map((r) => r.title).join() === 'Book flights,Release checklist');
check('open', (await query(db, { open: true }).rows()).length === 3);
const match = await db.from(notes).select('title').match('body', 'release docs').first();
check('match', match?.title === 'Release checklist');
const near = await db.from(notes).select('title').near('embed', embed('flights to Istanbul')).first();
check('near', near?.title === 'Book flights');
check('fuse', (await query(db, { words: 'desert fox' }).rows())[0].title === 'Book club');

const counts: number[] = [];
const stop = db.live(query(db, { open: true }), (rows) => counts.push(rows.length));
await add(db, 'Call mom', 'Ask about the weekend.', ['home']);
await new Promise((r) => setTimeout(r, 50));
stop();
check('live query', counts.at(-1) === 4);

const groceries = await db.from(notes).select('id').where('title', 'Groceries').first();
await finish(db, groceries!.id);
check('done', (await query(db, { open: true }).rows()).length === 3);

const again = await open(wasm);
check('persisted across a reopen', (await again.from(notes).count()) === 5);
