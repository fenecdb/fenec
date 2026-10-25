// A file: opened by a server of the app's own, edited through the studio's own
// form, closed (the server stops, gracefully), opened again with the edit
// there; opened twice, and while another program holds it.
import assert from 'node:assert/strict';
import { readFileSync, writeFileSync } from 'node:fs';
import {
  assertClean, cmd, data, evalIn, file, lastState, lines, mine, pressNotice, quit, startApp, startServer, step, textOf, until, waitFor, waitNotice, waitStudio, log,
} from './lib.mjs';

assertClean('before');
const SMALL = data('small', 50);
const HELD = data('held', 10);
const KEY = `file:${SMALL}`;

const app = startApp('files', { FENEC_DESKTOP_OPEN: SMALL });
const started = await waitFor((l) => l.what === 'server-started', 60000, 'the server started');
const loaded = await waitStudio(KEY);
assert.equal(loaded.detail.signin, false, 'the studio is signed in: no sign-in form');
assert.match(loaded.detail.side, /orders 50/);
log('studio loaded, signed in, orders 50; server pid', started.pid);

step('an edit through the studio\'s own New row form');
// What the page runs reaches it as one line: no // comments in it, and every statement ends in a ;.
// `ask` reads the row back through the server, as the page itself would, with the token it was handed.
const ASK = `const session = JSON.parse(sessionStorage.getItem('fenec-studio')); const ask = async () => { const r = await fetch('/query', {method: 'POST', headers: {'content-type': 'application/json', authorization: 'Bearer ' + session.token}, body: JSON.stringify({query: 'get orders where customer = $1', params: ['desktop']})}); return r.json(); };`;
const ins = await evalIn(KEY, `${ASK}
const until = async (f) => { for (let i = 0; i < 200; i++) { const v = await f(); if (v) return v; await new Promise(r => setTimeout(r, 50)); } throw new Error('timed out: ' + document.body.innerText.slice(0, 200)); };
const btn = t => [...document.querySelectorAll('button')].find(b => b.textContent.trim() === t);
btn('New row').click();
const i = await until(() => document.getElementById('new-customer')); i.value = 'desktop'; i.dispatchEvent(new Event('input', {bubbles: true}));
const s = document.getElementById('new-owner'); s.value = 'check'; s.dispatchEvent(new Event('input', {bubbles: true}));
(await until(() => btn('Insert row'))).click();
return await until(async () => { const rows = await ask(); return rows.length ? rows : null; });`, 40000);
assert.ok(ins.ok, ins.value);
assert.equal(ins.value.length, 1, 'the row is in the database');
assert.equal(ins.value[0].owner, 'check');

step('the same file again, while it is open in this app: its window is shown, no second server');
cmd({ cmd: 'open', path: SMALL });
const again = await waitFor((l) => l.what === 'open-result' && l.path === SMALL, 15000, 'open-result');
assert.equal(again.ok, true);
assert.match(again.message, /already open in a window of this app/);
assert.equal(mine().filter((p) => p.command.includes(`--file ${SMALL}`)).length, 1, 'one server for the file');

step('close the window (the red button): the server shuts down gracefully');
cmd({ cmd: 'close', key: KEY });
const stopped = await waitFor((l) => l.what === 'server-stopped', 15000, 'server-stopped');
assert.equal(stopped.code, 0, 'exit code 0: SIGTERM synced and checkpointed');
assert.equal(mine().filter((p) => p.command.includes(`--file ${SMALL}`)).length, 0, 'the server is gone');

step('reopen: the edit survived');
const n = lines().length;
cmd({ cmd: 'open', path: SMALL });
await waitFor((l, i) => i >= n && l.what === 'server-started', 15000, 'the second server');
await waitStudio(KEY, n);
const kept = await evalIn(KEY, `${ASK} return await ask()`);
assert.equal(kept.value.length, 1, 'the row written before the close is there after the reopen');
await until(() => /orders 51/.test(lastState(KEY)?.side ?? ''), 15000, 'orders 51 after the reopen');

step('close, then open at once, while the first server is still writing its checkpoint: it waits, and is not told the file is held');
{
  const k = lines().length;
  cmd({ cmd: 'close', key: KEY });
  await waitFor((l, i) => i >= k && l.what === 'window-close' && l.key === KEY, 15000, 'the window closing');
  cmd({ cmd: 'open', path: SMALL });
  await waitFor((l, i) => i >= k && l.what === 'server-started', 20000, 'the file opening again');
  assert.ok(!lines().slice(k).some((l) => l.what === 'notice'), 'no message about another program');
  await waitStudio(KEY, k);
}

step('a file another program holds: a message in the app\'s own window, and nothing opened');
const holder = await startServer(['--file', HELD, '--http', '127.0.0.1:0'], { FENEC_HTTP_TOKEN: 'x' });
const before = lines().length;
cmd({ cmd: 'open', path: HELD });
const held = await waitNotice(/open in another program/, 15000, before);
assert.match(held.message, /held\.fenec/);
assert.match(held.hint, /corrupt/);
assert.match(held.detail, /it is open already/);
assert.deepEqual(held.actions, ['retry']);
assert.match(await textOf(held.key), /open in another program/, 'and the window shows it');
cmd({ cmd: 'sessions' });
assert.ok(!(await waitFor((l, i) => i >= before && l.what === 'sessions', 5000, 'sessions')).keys.includes(`file:${HELD}`), 'no window for the held file');

step('Try again, once the other program let go');
await holder.stop();
const t = lines().length;
await pressNotice(held, 'retry');
await waitFor((l, i) => i >= t && l.what === 'server-started' && l.path === HELD, 15000, 'the held file opening');
await waitStudio(`file:${HELD}`, t);
log('opened after the holder stopped');

step('a file that is gone, a folder, and a file that is not a database (which is left as it was)');
const m = lines().length;
cmd({ cmd: 'open', path: `${file('nothing-here')}` });
assert.match((await waitNotice('That file is gone', 10000, m)).message, /nothing-here\.fenec/);
cmd({ cmd: 'open', path: SMALL.replace(/small\.fenec$/, '') });
await waitNotice('That is not a database file', 10000, m);
const NOTES = file('notes').replace(/\.fenec$/, '.txt');
const notes = 'plain text, not a database\n';
writeFileSync(NOTES, notes);
cmd({ cmd: 'open', path: NOTES });
const wrong = await waitNotice('That is not a fenecdb database', 15000, m);
assert.match(wrong.detail, /invalid fenecdb signature/);
assert.equal(readFileSync(NOTES, 'utf8'), notes, 'the server changed nothing in it');

step('quit: no server, no helper left');
await quit(app);
log('clean');
