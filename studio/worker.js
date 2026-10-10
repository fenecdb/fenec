// The database fenec studio reads in the page: the browser module in a
// dedicated worker, so an index being built or a seed being written never
// holds the page's thread. local.js starts it and speaks to it; engine.js
// answers each request as a server would.
//
// Messages in: `{id, op, ...}` -- open, request, subscribe, unsubscribe,
// reset, keep -- each answered `{id, value}` or `{id, error}`; a
// subscription's events go out as `{sub, event}`.
//
// Nothing is kept unless asked: the database is in this worker's memory
// and gone with the tab. `keep` stores it in this browser's IndexedDB
// (`persist`, a write at a time after the first image) and a later visit
// brings it back (`restore`) instead of seeding again.

import { Fenec, persist, restore, putState } from './fenec.js';
import { Engine } from './engine.js';

let engine = null;
let opened = null;
let keeping = false;

const post = (m) => self.postMessage(m);

/** A database seeded afresh: the playground's statements, one block. */
async function fresh() {
  const db = await Fenec.open(opened.module, { collation: opened.collation });
  if (opened.seed?.length) await db.batch(opened.seed);
  return db;
}

/** `db` in place of the database the studio reads, its subscriptions seeded again over it. */
function swap(db) {
  const was = engine.db;
  engine.replace(db);
  was.close();
}

/** What is kept, brought up to the database as it is now; nothing when not keeping. */
async function store(db) {
  if (keeping) await persist(db, opened.key);
}

const ops = {
  async open({ module, collation, seed, key, keep }) {
    opened = { module, collation, seed, key };
    keeping = !!keep;
    let db = null;
    let restored = false;
    if (keeping) {
      db = await Fenec.open(module, { collation });
      try {
        restored = await restore(db, key);
      } catch {
        restored = false;
      }
      if (!restored) {
        db.close();
        db = null;
      }
    }
    db ??= await fresh();
    await store(db);
    engine = new Engine(db);
    return { restored };
  },

  async request({ method, path, body }) {
    const r = await engine.request(method, path, body);
    if (r.wrote) await store(engine.db);
    return { status: r.status, json: r.json, seq: r.seq };
  },

  subscribe({ sub, collection, shape }) {
    engine.subscribe(sub, collection, shape, (event) => post({ sub, event }));
    return null;
  },

  unsubscribe({ sub }) {
    engine.unsubscribe(sub);
    return null;
  },

  /**
   * Every collection back to the seed. A new database, so what is kept is
   * replaced by its image, and the writes stored after the old one with it.
   */
  async reset() {
    swap(await fresh());
    await store(engine.db);
    return null;
  },

  /** Keeps the database in this browser from now on, or forgets what is kept. */
  async keep({ on }) {
    if (on && !keeping) {
      // Stored from its image on: `persist` goes on from where it last
      // stored a database, which after a forget is nowhere, so the one
      // kept is a copy that was never stored.
      const db = await Fenec.open(opened.module, { collation: opened.collation });
      await db.loadAsync(engine.db.snapshot());
      keeping = true;
      swap(db);
      await store(db);
    } else if (!on && keeping) {
      keeping = false;
      // An image of nothing where the image was: `restore` finds no
      // database, and the writes stored after it go with the next image.
      await putState(opened.key, null);
    }
    return keeping;
  },
};

self.onmessage = async (e) => {
  const { id, op, ...args } = e.data ?? {};
  try {
    post({ id, value: await ops[op](args) });
  } catch (err) {
    post({ id, error: { message: String(err?.message ?? err), status: err?.status ?? null } });
  }
};
