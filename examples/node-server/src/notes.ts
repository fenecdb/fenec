// Notes on a fenec-server, from Node. `@fenecdb/web/client` is the query
// builder and the HTTP client alone: no WebAssembly is fetched or loaded,
// every query runs on the server.
//
//   npm start                            seeds if empty, then lists
//   npm start -- add <title> <body> [tag ...]
//   npm start -- list [--tag T] [--open]
//   npm start -- search <words>
//   npm start -- done <id>
//   npm start -- watch                   the open notes again after every change
//   npm start -- smoke                   what CI runs
import { connect } from '@fenecdb/web/client';
import { notes, SEEDS, embed } from './tables.js';

// Every request the client makes, to show none of them is for a .wasm.
const requested: string[] = [];
const fetchAndNote: typeof fetch = (input, init) => {
  requested.push(String(input instanceof Request ? input.url : input));
  return fetch(input, init);
};

const db = await connect(process.env.FENEC_URL ?? 'http://127.0.0.1:8080', {
  token: process.env.FENEC_TOKEN ?? 'secret', // a dev default; never ship one
  schema: { notes },
  migrate: true, // the server's token may make what is missing
  fetch: fetchAndNote,
});

async function add(title: string, body: string, tags: string[], done = false, at: string | Date = new Date()) {
  await db.from(notes).insert({ title, body, tags, done, at, embed: embed(`${title} ${body}`) });
}

async function seed() {
  if ((await db.from(notes).count()) === 0) {
    for (const s of SEEDS) await add(s.title, s.body, s.tags, s.done, s.at);
  }
}

function list({ tag, open }: { tag?: string; open?: boolean } = {}) {
  let q = db.from(notes).select('id', 'title', 'tags', 'done', 'at').order('at', 'desc').limit(20);
  if (tag) q = q.where('tags', 'has', tag);
  if (open) q = q.where('done', false);
  return q;
}

async function search(words: string) {
  const q = db.from(notes).select('id', 'title');
  return {
    match: await q.match('body', words).limit(5).rows(),
    fuse: await q.match('body', words).near('embed', embed(words)).fuse().limit(5).rows(),
  };
}

function show(rows: { id: number; title: string | null; tags: string[] | null; done: boolean | null }[]) {
  for (const r of rows) {
    console.log(`[${r.done ? 'x' : ' '}] ${String(r.id).padStart(3)}  ${(r.title ?? '').padEnd(20)} ${(r.tags ?? []).join(', ')}`);
  }
}

async function smoke() {
  const check = (step: string, ok: boolean) => {
    console.log(`${ok ? 'ok  ' : 'FAIL'} ${step}`);
    if (!ok) process.exit(1);
  };
  await seed();
  check('seeded 4 notes', (await db.from(notes).count()) === 4);
  const hello = [...embed('hello')].flatMap((x, i) => (x ? [i] : []));
  check('toy embedding', hello.join() === '24,36,46,48,62');
  check('newest first', (await list().rows())[0].title === 'Book club');
  check('by tag', (await list({ tag: 'work' }).rows()).map((r) => r.title).join() === 'Book flights,Release checklist');
  check('open', (await list({ open: true }).rows()).length === 3);
  check('match', (await search('release docs')).match[0].title === 'Release checklist');
  const near = await db.from(notes).select('title').near('embed', embed('flights to Istanbul')).first();
  check('near', near?.title === 'Book flights');
  check('fuse', (await search('desert fox')).fuse[0].title === 'Book club');

  // A live query over the server: run again there after every write to notes.
  const seen = await new Promise<number>((resolve) => {
    let first = true;
    const timer = setTimeout(() => (stop(), resolve(-1)), 5000);
    const stop = db.live(list({ open: true }), (rows) => {
      if (first) {
        first = false;
        void add('Call mom', 'Ask about the weekend.', ['home']);
      } else if (rows.length === 4) {
        clearTimeout(timer);
        stop();
        resolve(rows.length);
      }
    });
  });
  check('live query', seen === 4);

  await db.from(notes).where('title', 'Groceries').update({ done: true });
  check('done', (await list({ open: true }).rows()).length === 3);
  check('no WebAssembly fetched', requested.length > 0 && !requested.some((u) => u.endsWith('.wasm')));
}

const [cmd, ...args] = process.argv.slice(2);
switch (cmd) {
  case 'add':
    await add(args[0], args[1], args.slice(2));
    break;
  case 'list': {
    const i = args.indexOf('--tag');
    show(await list({ tag: i >= 0 ? args[i + 1] : undefined, open: args.includes('--open') }).rows());
    break;
  }
  case 'search': {
    const found = await search(args.join(' '));
    console.log('match:', found.match.map((r) => r.title).join(', '));
    console.log('fuse: ', found.fuse.map((r) => r.title).join(', '));
    break;
  }
  case 'done':
    await db.from(notes).where('id', Number(args[0])).update({ done: true });
    break;
  case 'watch':
    db.live(list({ open: true }), (rows) => (console.log('--'), show(rows)));
    break;
  case 'smoke':
    await smoke();
    break;
  default:
    await seed();
    show(await list().rows());
}
