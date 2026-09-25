// A database kept in a file of the origin private file system (`openFile`).
// Node has no such file system, so a directory of in-memory files stands in
// for it: the synchronous access handle's read, write, truncate, getSize,
// flush and close, one handle a file at a time. A write lands as it is made,
// the way a crashed page leaves its file -- the operating system still holds
// what the browser wrote -- and a "crash" stops a write halfway.
//
// The last test hands files between a page and a real `fenec-pg`, both
// ways; it is skipped when the binary (cargo build) is missing.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { readFile, writeFile, access, mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Fenec, FenecError, openFile, persist } from './fenec.js';

const wasm = await readFile(new URL('./fenec.wasm', import.meta.url)).catch(() => null);
const skip = !wasm && 'no web/fenec.wasm (make wasm)';
const open = () => Fenec.open(wasm);

/**
 * Files by name. `crashAfter(n)` lets n more writes, truncates and flushes
 * through and stops the next -- a write halfway -- and every one after it,
 * until `restart()`, which also lets go of the handles a dead page held.
 */
function fakeDir() {
  const files = new Map();
  let budget = Infinity;
  const spend = () => {
    if (budget <= 0) throw new Error('crash');
    budget--;
  };
  return {
    files,
    crashAfter(n) {
      budget = n;
    },
    restart() {
      budget = Infinity;
      for (const f of files.values()) f.open = false;
    },
    put(name, bytes) {
      files.set(name, { bytes: new Uint8Array(bytes), open: false });
    },
    bytes(name) {
      return files.get(name)?.bytes ?? new Uint8Array(0);
    },
    async getFileHandle(name, { create = false } = {}) {
      if (!files.has(name)) {
        if (!create) throw Object.assign(new Error(name), { name: 'NotFoundError' });
        files.set(name, { bytes: new Uint8Array(0), open: false });
      }
      const f = files.get(name);
      const resize = (n) => {
        const b = new Uint8Array(n);
        b.set(f.bytes.subarray(0, Math.min(n, f.bytes.length)));
        f.bytes = b;
      };
      return {
        async createSyncAccessHandle() {
          if (f.open) throw Object.assign(new Error('locked'), { name: 'NoModificationAllowedError' });
          f.open = true;
          return {
            getSize: () => f.bytes.length,
            read(buf, { at = 0 } = {}) {
              const n = Math.max(0, Math.min(buf.length, f.bytes.length - at));
              buf.set(f.bytes.subarray(at, at + n));
              return n;
            },
            write(buf, { at = 0 } = {}) {
              let n = buf.length;
              let crash = false;
              if (budget <= 0) {
                n = buf.length >> 1;
                crash = true;
              } else budget--;
              if (at + n > f.bytes.length) resize(at + n);
              f.bytes.set(buf.subarray(0, n), at);
              if (crash) throw new Error('crash');
              return n;
            },
            truncate(n) {
              spend();
              resize(n);
            },
            flush() {
              spend();
            },
            close() {
              f.open = false;
            },
          };
        },
      };
    },
  };
}

/** A database opened from `name` in `dir`, as a page opens it again. */
async function reopen(dir, name) {
  dir.restart();
  const db = await open();
  const file = await openFile(db, name, { dir });
  return { db, file };
}

const titles = (db) => db.rows('get docs select title order title').map((r) => r.title);

test('a new file takes the image, then each statement as it answers', { skip }, async () => {
  const dir = fakeDir();
  const db = await open();
  const file = await openFile(db, 'a.fenec', { dir });
  const empty = file.size;
  assert.ok(empty > 0, 'the image of an empty database');
  db.run('create collection docs (title text @hash)');
  const created = file.size;
  assert.ok(created > empty);
  db.run('put docs {title: "a"}');
  assert.ok(file.size > created, 'in the file before run returned');
  assert.equal(file.size, dir.bytes('a.fenec').length);
  // A read writes nothing.
  const before = file.size;
  db.rows('get docs');
  assert.equal(file.size, before);

  const { db: again } = await reopen(dir, 'a.fenec');
  assert.deepEqual(titles(again), ['a']);
});

test('the file is what a server keeps: it loads as one', { skip }, async () => {
  const dir = fakeDir();
  const db = await open();
  const file = await openFile(db, 'b.fenec', { dir });
  db.run('create collection docs (title text, embed vector<3> @hnsw(cosine))');
  db.run('put docs [{title: "x", embed: [1, 0, 0]}, {title: "y", embed: [0, 1, 0]}]');
  const bytes = file.bytes();
  assert.equal(new TextDecoder().decode(bytes.subarray(0, 7)), 'FENECDB');
  const other = await open();
  assert.equal(other.load(bytes), bytes.length);
  assert.deepEqual(titles(other), ['x', 'y']);
});

test('a write cut short by a crash is cut off, and the next lands after it', { skip }, async () => {
  const dir = fakeDir();
  const db = await open();
  await openFile(db, 'c.fenec', { dir });
  db.run('create collection docs (title text)');
  db.run('put docs {title: "kept"}');
  const whole = dir.bytes('c.fenec').length;
  dir.crashAfter(0);
  assert.throws(() => db.run('put docs {title: "torn"}'), /refused a write/);
  assert.ok(dir.bytes('c.fenec').length > whole, 'half the record reached the file');

  let { db: again, file } = await reopen(dir, 'c.fenec');
  assert.deepEqual(titles(again), ['kept']);
  assert.equal(file.size, whole, 'cut back to the last whole record');
  again.run('put docs {title: "after"}');
  ({ db: again } = await reopen(dir, 'c.fenec'));
  assert.deepEqual(titles(again), ['after', 'kept']);
});

test("a compact's image replaces the file through the copy beside it", { skip }, async () => {
  const dir = fakeDir();
  const db = await open();
  const file = await openFile(db, 'd.fenec', { dir });
  db.run('create collection docs (n int @hash, title text)');
  for (let i = 0; i < 50; i++) db.run('put docs {n: $1, title: "t"}', [i]);
  db.run('del docs where n < 40');
  const grown = file.size;
  db.run('compact');
  assert.ok(file.size < grown, `${file.size} < ${grown}`);
  assert.equal(dir.bytes('d.fenec~').length, 0, 'the copy is emptied once the file holds it');
  const { db: again } = await reopen(dir, 'd.fenec');
  assert.equal(again.rows('get docs count')[0].count, 10);
});

/** `rows` documents whose image is well past the fold's floor: 64 KB appended. */
function fill(db, rows) {
  db.run('create collection docs (n int @hash, title text)');
  const pad = 'x'.repeat(40);
  for (let at = 0; at < rows; at += 500) {
    const page = Array.from({ length: Math.min(500, rows - at) }, (_, i) => `{n: ${at + i}, title: "${pad}"}`);
    db.run(`put docs [${page.join(', ')}]`);
  }
}

test('writes appended past half the image are folded into a new one', { skip }, async () => {
  const dir = fakeDir();
  const db = await open();
  const file = await openFile(db, 'e.fenec', { dir });
  fill(db, 4000);
  let folds = 0;
  for (let i = 0; i < 6000; i++) {
    const before = file.size;
    db.run('set docs {title: $1} where n = 7', [`update ${i}`]);
    if (file.size < before) folds++;
    // What the next open replays past the image. The image itself keeps
    // what the updates left behind, as a checkpoint does: `compact` is what
    // gives that back.
    const image = imageOf(dir.bytes('e.fenec'));
    assert.ok(file.size - image <= Math.max(image / 2, 64 * 1024), `${file.size - image} bytes past an image of ${image}`);
  }
  assert.ok(folds > 0, 'it folded');
  const { db: again } = await reopen(dir, 'e.fenec');
  assert.deepEqual(again.rows('get docs where n = 7 select title'), [{ title: 'update 5999' }]);
  assert.equal(again.rows('get docs count')[0].count, 4000);
});

test('a small database appends: the fold waits for 64 KB', { skip }, async () => {
  const dir = fakeDir();
  const db = await open();
  const file = await openFile(db, 's.fenec', { dir });
  db.run('create collection docs (title text)');
  let shrank = false;
  for (let i = 0; i < 500; i++) {
    const before = file.size;
    db.run('put docs {title: $1}', [`row ${i}`]);
    shrank ||= file.size < before;
  }
  assert.ok(!shrank && file.size < 64 * 1024, `${file.size} bytes, appended`);
});

test('a crash anywhere in a rewrite loses no write it answered', { skip }, async () => {
  let crashes = 0;
  for (let k = 0; ; k++) {
    const dir = fakeDir();
    const db = await open();
    await openFile(db, 'f.fenec', { dir });
    db.run('create collection docs (n int @hash, title text)');
    for (let i = 0; i < 30; i++) db.run('put docs {n: $1, title: "t"}', [i]);
    db.run('del docs where n < 20');
    dir.crashAfter(k);
    let crashed = false;
    try {
      db.run('compact');
    } catch {
      crashed = true;
    }
    const { db: again, file } = await reopen(dir, 'f.fenec');
    const ns = again.rows('get docs select n order n').map((r) => r.n);
    assert.deepEqual(ns, Array.from({ length: 10 }, (_, i) => 20 + i), `a crash after ${k} steps`);
    assert.equal(dir.bytes('f.fenec~').length, 0, `the copy is emptied after ${k} steps`);
    // The file goes on taking writes where it stands.
    again.run('put docs {n: 99, title: "after"}');
    assert.equal(file.size, dir.bytes('f.fenec').length);
    if (!crashed) break;
    crashes++;
  }
  // The copy's truncate, write and flush, the file's, and the copy's
  // truncate and flush: a crash at each.
  assert.equal(crashes, 8);
});

/** Where the image at the front of a file ends: its head's body length on. */
function imageOf(bytes) {
  return 25 + Number(new DataView(bytes.buffer, bytes.byteOffset).getBigUint64(17, true));
}

test('a crash in a fold keeps every answered write, and the one in flight whole or not at all', { skip }, async () => {
  for (let k = 0; k <= 8; k++) {
    const dir = fakeDir();
    const db = await open();
    const file = await openFile(db, 'g.fenec', { dir });
    fill(db, 4000);
    // Titles of one length, so every update appends as many bytes, until
    // the next would outgrow half the image: that one folds.
    const title = (i) => `v${String(i).padStart(5, '0')}`;
    let i = 0;
    for (;;) {
      const before = file.size;
      db.run('set docs {title: $1} where n = 1', [title(i++)]);
      const image = imageOf(dir.bytes('g.fenec'));
      if (file.size - image + (file.size - before) > Math.max(image * 0.5, 64 * 1024)) break;
      assert.ok(i < 50000, 'no fold in sight');
    }
    const size = file.size;
    dir.crashAfter(k);
    let crashed = false;
    try {
      db.run('set docs {title: $1} where n = 1', [title(i)]);
    } catch {
      crashed = true;
    }
    assert.equal(crashed, k < 8, `a fold is 8 steps; a crash after ${k}`);
    if (!crashed) assert.ok(file.size < size, 'it folded');
    const { db: again } = await reopen(dir, 'g.fenec');
    const got = again.rows('get docs where n = 1 select title')[0].title;
    if (crashed) assert.ok(got === title(i - 1) || got === title(i), `after ${k} steps: ${got}`);
    else assert.equal(got, title(i));
    assert.equal(again.rows('get docs count')[0].count, 4000);
  }
});

test('a file that refused a write refuses every later one, and reads go on', { skip }, async () => {
  const dir = fakeDir();
  const db = await open();
  await openFile(db, 'h.fenec', { dir });
  db.run('create collection docs (title text)');
  db.run('put docs {title: "a"}');
  dir.crashAfter(0);
  assert.throws(() => db.run('put docs {title: "b"}'), FenecError);
  dir.crashAfter(Infinity);
  assert.throws(() => db.run('put docs {title: "c"}'), /refused a write/);
  assert.equal(db.rows('get docs count')[0].count, 3, 'what the database holds');
  const { db: again } = await reopen(dir, 'h.fenec');
  assert.deepEqual(titles(again), ['a'], 'what the file holds');
});

test('one keeper a file and a database: the rest are refused', { skip }, async () => {
  const dir = fakeDir();
  const db = await open();
  const file = await openFile(db, 'i.fenec', { dir });
  await assert.rejects(openFile(db, 'j.fenec', { dir }), /already kept/);
  await assert.rejects(persist(db, 'k'), /kept in a file/);
  assert.throws(() => db.load(db.snapshot()), /kept in a file/);
  await assert.rejects(openFile(await open(), 'i.fenec', { dir }), /open elsewhere/);

  db.run('create collection docs (title text)');
  file.close();
  await assert.rejects(openFile(await open(), 'i.fenec', { dir: noSync(dir) }), /dedicated worker/);
  const full = await open();
  full.run('create collection other (n int)');
  await assert.rejects(openFile(full, 'i.fenec', { dir }), /holds a database/);
});

test('close lets the file go and stops keeping the writes', { skip }, async () => {
  const dir = fakeDir();
  const db = await open();
  const file = await openFile(db, 'l.fenec', { dir });
  db.run('create collection docs (title text)');
  file.close();
  const size = dir.bytes('l.fenec').length;
  db.run('put docs {title: "after"}');
  assert.equal(dir.bytes('l.fenec').length, size);
  assert.equal(db.drain().bytes.length, 0, 'no journal left to grow');
  // And the file opens in the next worker.
  const { db: again } = await reopen(dir, 'l.fenec');
  assert.deepEqual(again.rows('get docs'), []);
});

/** `dir` as the main thread sees it: no synchronous access. */
function noSync(dir) {
  return {
    async getFileHandle(name, opts) {
      const { createSyncAccessHandle, ...rest } = await dir.getFileHandle(name, opts);
      return rest;
    },
  };
}

// ------------------------------------------------------------ with a server

async function binary(name) {
  for (const p of [`../target/debug/${name}`, `../target/release/${name}`]) {
    const url = new URL(p, import.meta.url);
    try {
      await access(url);
      return url.pathname;
    } catch {
      /* not there */
    }
  }
  return null;
}
const bin = await binary('fenec-pg');
const cli = await binary('fenec');

// A test that fails or times out never reaches its kill; the server would
// hold the test runner open. Live ones are killed for certain at exit.
const alive = new Set();
process.on('exit', () => {
  for (const p of alive) p.kill('SIGKILL');
});

/** `fenec-pg --http` over `file`; reads the address off stderr. */
async function serve(file) {
  const proc = spawn(bin, ['--listen', '127.0.0.1:0', '--http', '127.0.0.1:0', '--sync', 'always', '--file', file], {
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  alive.add(proc);
  const url = await new Promise((res, rej) => {
    let buf = '';
    const timer = setTimeout(() => rej(new Error(`server did not start: ${buf}`)), 10000);
    proc.stderr.on('data', (d) => {
      buf += d;
      const m = buf.match(/fenec-http \S+ listening on: (http:\/\/\S+)\s+\[[^\]]*\]/);
      if (m) {
        clearTimeout(timer);
        res(m[1]);
      }
    });
    proc.on('exit', (c) => {
      clearTimeout(timer);
      rej(new Error(`server exited with ${c}: ${buf}`));
    });
  });
  const run = async (query, params = []) => {
    const res = await fetch(`${url}/query`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ query, params }),
    });
    const body = await res.json();
    if (!res.ok) throw new Error(body.error);
    return body;
  };
  const kill = () =>
    new Promise((res) => {
      alive.delete(proc);
      if (proc.exitCode !== null || proc.signalCode !== null) return res();
      proc.on('exit', res);
      proc.kill('SIGKILL');
    });
  return { run, kill };
}

test("a page's file opens in fenec-pg, and a server's file in a page", { skip: skip || (!bin && 'no fenec-pg binary (cargo build)') }, async () => {
  const tmp = await mkdtemp(join(tmpdir(), 'fenec-file-'));
  try {
    const dir = fakeDir();
    const db = await open();
    const file = await openFile(db, 'm.fenec', { dir });
    db.run('create collection docs (title text @hash, embed vector<3> @hnsw(cosine))');
    db.run('put docs [{title: "a", embed: [1, 0, 0]}, {title: "b", embed: [0, 1, 0]}]');
    db.run('set docs {title: "c"} where title = "b"');
    const path = join(tmp, 'm.fenec');
    await writeFile(path, file.bytes());
    if (cli) {
      const { stdout } = await promisify(execFile)(cli, [path, '-c', 'get docs select title order title']);
      assert.match(stdout, /^title\n-----\na\nc\n\(2 rows/, 'the command line reads it');
    }

    const server = await serve(path);
    try {
      const got = await server.run('get docs select title order title');
      assert.deepEqual(got.map((r) => r.title), ['a', 'c']);
      // Killed with its writes in the tail: what a page gets from a server.
      await server.run('put docs {title: "d", embed: [0, 0, 1]}');
    } finally {
      await server.kill();
    }

    const back = fakeDir();
    back.put('n.fenec', await readFile(path));
    const page = await open();
    await openFile(page, 'n.fenec', { dir: back });
    assert.deepEqual(titles(page), ['a', 'c', 'd']);
    const near = page.rows('get docs near embed $1 limit 1 select title', [[0, 0, 1]]);
    assert.equal(near[0].title, 'd');
    page.run('put docs {title: "e", embed: [1, 1, 0]}');
    assert.deepEqual(titles((await reopen(back, 'n.fenec')).db), ['a', 'c', 'd', 'e']);
  } finally {
    await rm(tmp, { recursive: true, force: true });
  }
});
