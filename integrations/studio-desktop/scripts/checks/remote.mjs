// Saved servers: the token in the Keychain and in no file of the app's, no
// process's arguments and no URL; the server's own studio in a window; and
// every way it can fail said in words before a window opens on nothing.
//
// The token is out of the app's own files and out of reach of the pages it
// shows and of other users. It is not out of reach of another program of the
// same user: the Keychain's client here is the bundled `bun`, a general-purpose
// runtime that any program running as that user can run with a script of its
// own (`Bun.secrets.get`, with the service and the id that state.json names),
// and nothing in this app makes the Keychain ask first. What a signed build
// asks, if anything, is for the README's list to look at. So this reads the
// secret back through the app, and from here only asks that the item exists
// (attributes, no `-w`).
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createServer } from 'node:http';
import { readFileSync } from 'node:fs';
import { randomBytes } from 'node:crypto';
import {
  assertClean, cmd, data, evalIn, IDENTIFIER, lines, pressNotice, procs, quit, startApp, startServer, step, textOf, until, USER_DATA, waitFor, waitNotice, waitStudio, log, file,
} from './lib.mjs';

assertClean('before');
const REMOTE = data('remote', 30);
const OTHER = data('held', 10);

const keychainHas = (id) => spawnSync('security', ['find-generic-password', '-s', IDENTIFIER, '-a', `connection:${id}`], { stdio: 'ignore' }).status === 0;
const state = () => JSON.parse(readFileSync(`${USER_DATA}/state.json`, 'utf8'));

const app = startApp('remote', { FENEC_DESKTOP_LAUNCHER: '1' });
await waitFor((l) => l.what === 'ready', 60000, 'ready');
const launcherText = await waitFor((l) => l.what === 'page-text' && l.key === 'launcher' && /Open file/.test(l.text), 20000, 'the launcher drawn');
assert.match(launcherText.text, /New database/);
assert.match(launcherText.text, /Servers/);

/** A saved server named `name`; returns its id. */
async function save(name, url, token, tenant = '') {
  const n = lines().length;
  cmd({ cmd: 'addconn', name, url, token, tenant });
  return (await waitFor((l, i) => i >= n && l.what === 'conn-added' && l.name === name, 10000, `${name} saved`)).id;
}
/** Opens a saved server and returns what the app answered. */
async function open(id) {
  const n = lines().length;
  cmd({ cmd: 'openconn', id });
  return waitFor((l, i) => i >= n && l.what === 'openconn-result' && l.id === id, 20000, 'openconn-result');
}
const windowsOpen = async () => {
  const n = lines().length;
  cmd({ cmd: 'sessions' });
  return (await waitFor((l, i) => i >= n && l.what === 'sessions', 5000, 'sessions')).keys;
};

step('a server with a studio, and its token');
const TOKEN = `remote-${randomBytes(12).toString('hex')}`;
const remote = await startServer(['--file', REMOTE, '--studio', '--http', '127.0.0.1:0'], { FENEC_HTTP_TOKEN: TOKEN });
const id = await save('remote check', remote.origin, TOKEN);
let n = lines().length;
cmd({ cmd: 'secret', id });
const held = await waitFor((l, i) => i >= n && l.what === 'secret', 10000, 'secret');
assert.equal(held.token, TOKEN, 'the Keychain holds the token');
assert.equal(held.origin, remote.origin, 'and it holds it for the address it was saved with');
assert.ok(keychainHas(id), 'the item exists in the Keychain');
const written = readFileSync(`${USER_DATA}/state.json`, 'utf8');
assert.ok(!written.includes(TOKEN), 'state.json holds no token');
assert.deepEqual(state().connections.find((c) => c.id === id), { id, name: 'remote check', url: remote.origin, tenant: null });
assert.ok(!procs().some((p) => p.command.includes(TOKEN)), 'the token is in no process\'s arguments');

step('opened: the server\'s own studio, signed in, no sign-in form');
const KEY = `remote:${id}`;
n = lines().length;
assert.equal((await open(id)).ok, true);
const loaded = await waitStudio(KEY, n);
assert.equal(loaded.detail.signin, false);
assert.match(loaded.detail.side, /orders 30/);
assert.ok(!procs().some((p) => p.command.includes(TOKEN)), 'still in no process\'s arguments');
assert.ok((await windowsOpen()).includes(KEY));

step('the wrong token: said in words, no window, and the form to put it right');
const bad = await save('wrong token', remote.origin, 'not-the-token');
n = lines().length;
const refused = await open(bad);
assert.equal(refused.ok, false);
const wrong = await waitNotice('The server refused the saved token', 10000, n);
assert.match(wrong.message, /wrong token/);
assert.match(wrong.message, /invalid or missing token|invalid token|token/);
assert.deepEqual(wrong.actions, ['edit']);
assert.ok(!(await windowsOpen()).includes(`remote:${bad}`), 'no window opened on the refused server');
await pressNotice(wrong, 'edit');
await waitFor((l) => l.what === 'page-text' && l.key === 'launcher' && /Save changes/.test(l.text), 10000, 'the launcher on the edit form');
const form = await evalIn('launcher', `const f = document.getElementById('add'); return [f.elements.name.value, f.elements.url.value, f.elements.token.value].join('|')`);
assert.equal(form.value, `wrong token|${remote.origin}|`, 'the form is filled in, the token field empty');
// Forget token: the saved token goes, the connection stays (a blank token field only ever means keep it).
const forget = await evalIn('launcher', `const b = document.getElementById('forget-token'); const offered = !b.hidden; b.click(); return offered`);
assert.equal(forget.value, true, 'the edit form offers Forget token for a connection that has one');
await until(() => !keychainHas(bad), 10000, 'the token forgotten');
assert.ok(state().connections.some((c) => c.id === bad), 'the connection stays');
n = lines().length;
cmd({ cmd: 'secret', id: bad });
assert.equal((await waitFor((l, i) => i >= n && l.what === 'secret', 10000, 'secret')).token, '', 'and no token is left for it');
// What the edit form does: the same connection, a new token, nothing else changed.
n = lines().length;
cmd({ cmd: 'addconn', id: bad, name: 'wrong token', url: remote.origin, token: TOKEN, tenant: '' });
await waitFor((l, i) => i >= n && l.what === 'conn-added', 10000, 'conn changed');
assert.equal(state().connections.filter((c) => c.id === bad).length, 1, 'changed, not added');
n = lines().length;
assert.equal((await open(bad)).ok, true, 'opens with the new token');
await waitStudio(`remote:${bad}`, n);

step('a saved server pointed at another address, with no token typed, is refused: its token is not sent to a new place');
const THIRD = data('third', 10);
const third = await startServer(['--file', THIRD, '--studio', '--http', '127.0.0.1:0'], { FENEC_HTTP_TOKEN: 'third-token' });
const mover = await save('mover', remote.origin, TOKEN);
n = lines().length;
cmd({ cmd: 'addconn', id: mover, name: 'mover', url: third.origin, token: '', tenant: '' });
const noMove = await waitFor((l, i) => i >= n && l.what === 'conn-refused', 10000, 'the change refused');
assert.match(noMove.message, /not sent to another address/);
assert.equal(state().connections.find((c) => c.id === mover).url, remote.origin, 'the connection is as it was');
assert.ok(keychainHas(mover), 'and so is its token');
n = lines().length;
cmd({ cmd: 'secret', id: mover });
assert.equal((await waitFor((l, i) => i >= n && l.what === 'secret', 10000, 'secret')).token, TOKEN, 'unchanged');
// With a token typed for the new address, it goes.
n = lines().length;
cmd({ cmd: 'addconn', id: mover, name: 'mover', url: third.origin, token: 'third-token', tenant: '' });
await waitFor((l, i) => i >= n && l.what === 'conn-added' && l.name === 'mover', 10000, 'the change taken');
assert.equal(state().connections.find((c) => c.id === mover).url, third.origin);
n = lines().length;
cmd({ cmd: 'secret', id: mover });
const moved2 = await waitFor((l, i) => i >= n && l.what === 'secret', 10000, 'secret');
assert.equal(moved2.token, 'third-token', 'the new address has its own');
assert.equal(moved2.origin, third.origin, 'saved for the new address');
// The same address with the token left blank is an edit of the name: the token stays.
n = lines().length;
cmd({ cmd: 'addconn', id: mover, name: 'mover renamed', url: third.origin, token: '', tenant: '' });
await waitFor((l, i) => i >= n && l.what === 'conn-added' && l.name === 'mover renamed', 10000, 'the rename taken');
assert.ok(keychainHas(mover));

step('a token sealed for another address is none for this one: it is not sent, and the open says a token is needed');
const stale = await save('stale', remote.origin, TOKEN);
n = lines().length;
cmd({ cmd: 'seal', id: stale, origin: third.origin, token: TOKEN });
await waitFor((l, i) => i >= n && l.what === 'sealed' && l.id === stale, 10000, 'sealed');
n = lines().length;
cmd({ cmd: 'secret', id: stale });
const sealed = await waitFor((l, i) => i >= n && l.what === 'secret', 10000, 'secret');
assert.equal(sealed.token, '', "the token of the other address is not the token of this one");
n = lines().length;
assert.equal((await open(stale)).ok, false);
await waitNotice('This server asks for a token', 10000, n);
assert.ok(!(await windowsOpen()).includes(`remote:${stale}`), 'no window opened with it');
await third.stop();

step('two clicks on one saved server while it is being looked at: one window');
const twice = await save('twice', remote.origin, TOKEN);
n = lines().length;
cmd({ cmd: 'openconn-twice', id: twice });
const both = await waitFor((l, i) => i >= n && l.what === 'openconn-twice-result', 30000, 'both opens answered');
assert.deepEqual(both.results.map((r) => r.ok), [true, true]);
assert.equal(lines().slice(n).filter((l) => l.what === 'window-opened' && l.key === `remote:${twice}`).length, 1, 'one window, not two');

step('a server that sends the browser elsewhere: said, and the token is not sent on');
let sawAuthorization = false;
const redirecting = createServer((req, res) => {
  if (req.headers.authorization) sawAuthorization = true;
  if (req.url === '/_studio/') {
    res.writeHead(200, { 'content-type': 'text/html' });
    return void res.end('<!doctype html>');
  }
  res.writeHead(302, { location: 'http://127.0.0.1:9/login?next=/_whoami' });
  res.end();
});
await new Promise((r) => redirecting.listen(0, '127.0.0.1', r));
const moved = await save('moved', `http://127.0.0.1:${redirecting.address().port}`, 'token-for-moved');
n = lines().length;
assert.equal((await open(moved)).ok, false);
const sent = await waitNotice('The server sent us somewhere else', 10000, n);
assert.match(sent.message, /HTTP 302\) to http:\/\/127\.0\.0\.1:9\b/);
assert.ok(!sent.message.includes('login') && !sent.message.includes('next='), 'only where, not the path or query');
assert.equal(sawAuthorization, false, 'the token went to no one');
await new Promise((r) => redirecting.close(r));

step('a server that needs a token, with none saved');
const none = await save('no token', remote.origin, '');
n = lines().length;
assert.equal((await open(none)).ok, false);
const needs = await waitNotice('This server asks for a token', 10000, n);
assert.match(needs.message, /none is saved/);

step('a server with the studio off: told so, with the flag to start it with');
const off = await startServer(['--file', OTHER, '--http', '127.0.0.1:0'], { FENEC_HTTP_TOKEN: 'x' });
const nostudio = await save('studio off', off.origin, 'x');
n = lines().length;
assert.equal((await open(nostudio)).ok, false);
const nsn = await waitNotice('This server has no studio', 10000, n);
assert.match(nsn.message, /HTTP 404/);
assert.match(nsn.hint, /--studio/);
await off.stop();

step('an address nothing listens on');
const dead = await save('dead', 'http://127.0.0.1:9', '');
n = lines().length;
assert.equal((await open(dead)).ok, false);
const unreachable = await waitNotice('Could not reach the server', 15000, n);
assert.match(unreachable.message, /nothing is listening/);
assert.deepEqual(unreachable.actions, ['edit']);

step('removing a connection removes its Keychain item');
for (const c of [id, bad, none, nostudio, dead, mover, stale, twice, moved]) cmd({ cmd: 'rmconn', id: c });
await waitFor((l) => l.what === 'conn-removed' && l.id === moved, 10000, 'all removed');
assert.ok(![id, bad, mover, stale, twice, moved].some(keychainHas), 'every item is gone');
assert.deepEqual(state().connections, []);

step('quit with windows open: nothing of the app is left (the remote server is not the app\'s)');
await remote.stop();
await quit(app);
log('clean', file('remote'));
