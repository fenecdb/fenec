// useLiveQuery: `npm test` here, after `npm install`.
//
// Rendered with react-dom into jsdom. A stand-in database pins down the
// contract -- when it subscribes, what it renders meanwhile, when it lets go
// -- and a real one checks the point of it all: a row another client writes
// through the server shows up in the component, with no code of the
// component's asking. That half needs web/fenec.wasm (make wasm) and the
// fenec-pg binary (cargo build), and skips itself without them.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { access, mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { JSDOM } from 'jsdom';

const dom = new JSDOM('<!doctype html><html><body></body></html>');
globalThis.window = dom.window;
globalThis.document = dom.window.document;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;

const { createElement: h, act, Component } = await import('react');
const { createRoot } = await import('react-dom/client');
const { FenecProvider, useFenec, useLiveQuery } = await import('./index.js');

// ---------------------------------------------------------------- stand-in

function standIn() {
  const subs = [];
  return {
    subs,
    live(query, cb, opts) {
      const sub = { query, cb, opts, stopped: false };
      subs.push(sub);
      return () => {
        sub.stopped = true;
      };
    },
  };
}

function query(text, context) {
  return { context, toFenecQL: () => [text, []] };
}

async function render(element) {
  const container = document.createElement('div');
  const root = createRoot(container);
  await act(async () => root.render(element));
  return {
    container,
    rerender: (el) => act(async () => root.render(el)),
    unmount: () => act(async () => root.unmount()),
  };
}

function Titles({ q, seen = [] }) {
  const rows = useLiveQuery(q);
  seen.push(rows);
  if (rows === undefined) return h('p', null, 'loading');
  return h(
    'ul',
    null,
    rows.map((r) => h('li', { key: r.id }, r.title)),
  );
}

const text = (container) => container.textContent;

test('undefined until the first answer, then the rows, then the next ones', async () => {
  const db = standIn();
  const seen = [];
  const view = await render(h(Titles, { q: query('get tasks', db), seen }));
  assert.equal(text(view.container), 'loading');
  assert.equal(db.subs.length, 1);
  await act(async () => db.subs[0].cb([{ id: 1, title: 'one' }]));
  assert.equal(text(view.container), 'one');
  await act(async () => db.subs[0].cb([{ id: 1, title: 'one' }, { id: 2, title: 'two' }]));
  assert.equal(text(view.container), 'onetwo');
  assert.equal(seen[0], undefined);
  await view.unmount();
});

test('a query built again on every render does not subscribe again; another one does', async () => {
  const db = standIn();
  const view = await render(h(Titles, { q: query('get tasks', db) }));
  await act(async () => db.subs[0].cb([{ id: 1, title: 'one' }]));
  await view.rerender(h(Titles, { q: query('get tasks', db) }));
  assert.equal(db.subs.length, 1);
  assert.equal(text(view.container), 'one');

  await view.rerender(h(Titles, { q: query('get tasks where done = false', db) }));
  assert.equal(db.subs.length, 2);
  assert.equal(db.subs[0].stopped, true);
  // The old query's rows are not the new one's.
  assert.equal(text(view.container), 'loading');
  await act(async () => db.subs[1].cb([{ id: 2, title: 'two' }]));
  assert.equal(text(view.container), 'two');
  await view.unmount();
});

test('unmounting lets go, and an answer still on its way goes nowhere', async () => {
  const db = standIn();
  const view = await render(h(Titles, { q: query('get tasks', db) }));
  await view.unmount();
  assert.equal(db.subs[0].stopped, true);
  await act(async () => db.subs[0].cb([{ id: 1, title: 'late' }]));
});

test('a query that fails reaches the error boundary', async () => {
  class Boundary extends Component {
    state = { error: null };
    static getDerivedStateFromError(error) {
      return { error };
    }
    render() {
      return this.state.error ? h('p', null, `failed: ${this.state.error.message}`) : this.props.children;
    }
  }
  const db = standIn();
  const quiet = console.error;
  console.error = () => {};
  try {
    const view = await render(h(Boundary, null, h(Titles, { q: query('get tasks', db) })));
    await act(async () => db.subs[0].opts.onError(new Error('no such field')));
    assert.equal(text(view.container), 'failed: no such field');
    await view.unmount();
  } finally {
    console.error = quiet;
  }
});

test("the provider's database serves a query that brings none", async () => {
  const db = standIn();
  function Seen() {
    assert.equal(useFenec(), db);
    return h(Titles, { q: query('get tasks') });
  }
  const view = await render(h(FenecProvider, { db }, h(Seen)));
  assert.equal(db.subs.length, 1);
  await view.unmount();
  const quiet = console.error;
  console.error = () => {};
  try {
    await assert.rejects(render(h(() => useFenec() && null)), /no FenecProvider/);
  } finally {
    console.error = quiet;
  }
});

// ---------------------------------------------------------------- real

const wasm = await readFile(new URL('../../web/fenec.wasm', import.meta.url)).catch(() => null);
async function binary() {
  for (const p of ['../../target/debug/fenec-pg', '../../target/release/fenec-pg']) {
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
const bin = await binary();

test(
  'a row another client writes shows up in the component',
  { skip: !wasm ? 'no web/fenec.wasm (make wasm)' : !bin ? 'no fenec-pg (cargo build)' : false },
  async () => {
    const { Fenec, sync, connect } = await import('../../web/fenec.js');
    const dir = await mkdtemp(join(tmpdir(), 'fenec-react-'));
    const proc = spawn(bin, ['--listen', '127.0.0.1:0', '--http', '127.0.0.1:0', '--file', join(dir, 'r.fenec')], {
      stdio: ['ignore', 'ignore', 'pipe'],
    });
    try {
      const url = await new Promise((res, rej) => {
        let buf = '';
        const timer = setTimeout(() => rej(new Error(`no listener: ${buf}`)), 10000);
        proc.stderr.on('data', (d) => {
          buf += d;
          const m = buf.match(/fenec-http \S+ listening on: (http:\/\/\S+)\s+\[[^\]]*\]/);
          if (m) {
            clearTimeout(timer);
            res(m[1]);
          }
        });
      });
      const remote = connect(url);
      await remote.run('create collection tasks (title text, done bool @hash)');
      await remote.from('tasks').insert({ title: 'write the hook', done: false });

      const db = await sync({
        url,
        local: await Fenec.open(wasm),
        leader: false,
        shapes: [{ collection: 'tasks' }],
      });
      await db.ready();

      function Open() {
        const rows = useLiveQuery(useFenec().from('tasks').where('done', false).order('title'));
        return h('p', null, rows === undefined ? 'loading' : rows.map((r) => r.title).join(', '));
      }
      const view = await render(h(FenecProvider, { db }, h(Open)));
      const until = async (want) => {
        const deadline = Date.now() + 10000;
        while (text(view.container) !== want) {
          assert.ok(Date.now() < deadline, `stayed at "${text(view.container)}", not "${want}"`);
          await act(async () => new Promise((r) => setTimeout(r, 20)));
        }
      };
      await until('write the hook');
      // Another client, through the server: the replica hears of it, and
      // the component renders it without asking.
      await remote.from('tasks').insert({ title: 'test the hook', done: false });
      await until('test the hook, write the hook');
      await remote.from('tasks').where('title', 'write the hook').update({ done: true });
      await until('test the hook');
      await view.unmount();
      db.close();
    } finally {
      proc.kill('SIGKILL');
      await rm(dir, { recursive: true, force: true });
    }
  },
);
