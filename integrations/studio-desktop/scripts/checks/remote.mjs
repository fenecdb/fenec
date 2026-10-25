// Saved servers: the token in the Keychain and nowhere else, the server's own
// studio in a window, and every way it can fail said in words before a window
// opens on nothing.
//
// The Keychain item can be read back only by the app (its access list names
// the app), so this reads the secret through the app, and from here only asks
// that the item exists (attributes, no `-w`).
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { randomBytes } from 'node:crypto';
import {
  assertClean, cmd, data, evalIn, IDENTIFIER, lines, pressNotice, procs, quit, startApp, startServer, step, textOf, USER_DATA, waitFor, waitNotice, waitStudio, log, file,
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
cmd({ cmd: 'secret', name: `connection:${id}` });
assert.equal((await waitFor((l, i) => i >= n && l.what === 'secret', 10000, 'secret')).value, TOKEN, 'the Keychain holds the token');
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
// What the edit form does: the same connection, a new token, nothing else changed.
n = lines().length;
cmd({ cmd: 'addconn', id: bad, name: 'wrong token', url: remote.origin, token: TOKEN, tenant: '' });
await waitFor((l, i) => i >= n && l.what === 'conn-added', 10000, 'conn changed');
assert.equal(state().connections.filter((c) => c.id === bad).length, 1, 'changed, not added');
n = lines().length;
assert.equal((await open(bad)).ok, true, 'opens with the new token');
await waitStudio(`remote:${bad}`, n);

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
for (const c of [id, bad, none, nostudio, dead]) cmd({ cmd: 'rmconn', id: c });
await waitFor((l) => l.what === 'conn-removed' && l.id === dead, 10000, 'all removed');
assert.ok(!keychainHas(id) && !keychainHas(bad), 'both items are gone');
assert.deepEqual(state().connections, []);

step('quit with windows open: nothing of the app is left (the remote server is not the app\'s)');
await remote.stop();
await quit(app);
log('clean', file('remote'));
