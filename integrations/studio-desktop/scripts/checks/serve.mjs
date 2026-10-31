// Serving a database to the apps on this Mac: the serve sheet, a server on a
// port, its token in the Keychain and in no file, argument or URL, a window on
// that server rather than a second one, start and stop as the server's own
// shutdown, its output, what another program holding the file or the port
// does to it, a server that dies, and what a relaunch starts: nothing.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { createServer } from 'node:net';
import {
  assertClean, cmd, data, evalIn, forgetServeItems, IDENTIFIER, lines, mine, pressNotice, procs, quit, sleep, startApp, startServer, step, until, USER_DATA, waitFor, waitNone, waitNotice, waitStudio, log,
} from './lib.mjs';

assertClean('before');
const FILE = data('served', 40);
const OTHER = data('other', 5);
const HELD = data('held-by-another', 5);

const state = () => JSON.parse(readFileSync(`${USER_DATA}/state.json`, 'utf8'));
const entry = (path) => state().databases.find((d) => d.path === path);
const keychainHas = (name) => spawnSync('security', ['find-generic-password', '-s', IDENTIFIER, '-a', name], { stdio: 'ignore' }).status === 0;
/** What the app answered to `c`, by the report it writes for it. */
async function ask(c, what, ms = 30000) {
  const n = lines().length;
  cmd(c);
  return waitFor((l, i) => i >= n && l.what === what && (l.path === undefined || c.path === undefined || l.path === c.path), ms, what);
}
const status = async (url, token) => (await fetch(url, { headers: token ? { authorization: `Bearer ${token}` } : {} })).status;
/** The fenec-server processes on `path`: the app's (under its bundle) and any other. */
const serversOn = (path) => procs().filter((p) => /fenec-server /.test(p.command) && p.command.includes(`--file ${path} `));
const freePort = () =>
  new Promise((resolve) => {
    const s = createServer().listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });

let app = startApp('serve', { FENEC_DESKTOP_LAUNCHER: '1' });
await waitFor((l) => l.what === 'ready', 60000, 'ready');
await waitFor((l) => l.what === 'page-text' && l.key === 'launcher' && /Databases/.test(l.text), 20000, 'the main window drawn');
const sections = async () => (await evalIn('launcher', `return [...document.querySelectorAll('.view')].filter((v) => getComputedStyle(v).display !== 'none').map((v) => v.id)`)).value;
assert.deepEqual(await sections(), ['databases'], 'one section drawn at a time: a hidden one is not shown by its own display rule');
// The file is in the list once it was opened: open it, close it.
let n = lines().length;
cmd({ cmd: 'open', path: FILE });
await waitStudio(`file:${FILE}`, n);
cmd({ cmd: 'close', key: `file:${FILE}` });
await waitFor((l, i) => i >= n && l.what === 'server-stopped' && l.key === `file:${FILE}`, 15000, 'its window\'s server stopped');

step('the serve sheet, from the Databases list: the command it shows holds no token, and Show shows the Keychain\'s');
await until(async () => (await evalIn('launcher', `return document.querySelector('tr[data-path="${FILE}"]') !== null`)).value, 10000, 'the row');
await evalIn('launcher', `[...document.querySelector('tr[data-path="${FILE}"]').querySelectorAll('button')].find((b) => b.textContent === 'Serve…').click()`);
const command = await until(async () => (await evalIn('launcher', `return document.getElementById('sheet').open && document.getElementById('command').textContent`)).value, 10000, 'the sheet open, with its command');
// No backups set up for it: no replication token, which would answer /_changes to the data token too.
assert.match(command, /^FENEC_HTTP_TOKEN=•••• fenec-server --file /);
assert.match(command, /--http 127\.0\.0\.1:0/);
assert.match(command, /--exit-on-stdin-close$/);
await evalIn('launcher', `document.getElementById('tok-show').click()`);
const shown = await until(async () => {
  const t = (await evalIn('launcher', `return document.getElementById('tok-shown').textContent`)).value;
  return /^[A-Za-z0-9_-]{43}$/.test(t) && t;
}, 10000, 'the token shown');
const TOKEN = (await ask({ cmd: 'serve-token', path: FILE }, 'serve-token')).token;
assert.equal(shown, TOKEN, 'what Show shows is what the Keychain keeps');
const id = entry(FILE).id;
assert.ok(keychainHas(`serve:${id}`), 'kept in the Keychain under serve:<id>, before any server has it');
assert.ok(!readFileSync(`${USER_DATA}/state.json`, 'utf8').includes(TOKEN), 'state.json holds no token');

step('Start server: any free port, this Mac, the token from the Keychain, no second server');
n = lines().length;
await evalIn('launcher', `document.getElementById('sheet-start').click()`);
const started = await waitFor((l, i) => i >= n && l.what === 'served' && l.path === FILE, 30000, 'served');
assert.match(started.local, /^http:\/\/127\.0\.0\.1:\d+$/);
assert.equal(started.origin, started.local);
assert.equal(await status(`${started.local}/_whoami`, TOKEN), 200, 'an app with the token gets in');
assert.equal(await status(`${started.local}/_whoami`), 401, 'one without it does not');
assert.ok(!procs().some((p) => p.command.includes(TOKEN)), 'the token is in no process\'s arguments');
assert.equal(serversOn(FILE).length, 1, 'one server on the file');
assert.deepEqual(entry(FILE).serve, { port: null, reach: 'mac', studio: true, compact: true, mcp: false, metrics: false });
await waitFor((l, i) => i >= n && l.what === 'page-text' && l.key === 'launcher' && /Running for/.test(l.text), 15000, 'the Servers list shows it running');
assert.deepEqual(await sections(), ['servers'], 'the sheet hands over to the Servers list, alone');

step('Open: a window on the served server, not a second server; closing it leaves the server running');
n = lines().length;
cmd({ cmd: 'open', path: FILE });
const onServed = await waitFor((l, i) => i >= n && (l.what === 'window-on-served' || l.what === 'server-started') && l.path === FILE, 20000, 'the window');
assert.equal(onServed.what, 'window-on-served');
assert.equal(onServed.origin, started.local);
const loaded = await waitStudio(`file:${FILE}`, n);
assert.equal(loaded.detail.signin, false, 'signed in by the app, with the served token');
assert.match(loaded.detail.side, /orders 40/);
assert.equal(serversOn(FILE).length, 1, 'still one server on the file');
cmd({ cmd: 'close', key: `file:${FILE}` });
await waitFor((l, i) => i >= n && l.what === 'window-close' && l.key === `file:${FILE}`, 10000, 'the window closed');
await waitNone((l, i) => i >= n && (l.what === 'served-stopped' || l.what === 'server-stopped') && (l.path === FILE || l.key === `file:${FILE}`), 1500, 'the server stopped with the window');
assert.equal(await status(`${started.local}/_whoami`, TOKEN), 200, 'the apps still reach it');
assert.equal((await ask({ cmd: 'serve', path: FILE }, 'serve-result')).ok, false, 'served already: not twice');

step('its output, and its figures from its own metrics');
const out = await ask({ cmd: 'server-log', path: FILE }, 'server-log');
assert.ok(out.lines.some((l) => /^fenec-http \S+ listening on: http:\/\/127\.0\.0\.1:/.test(l)), out.lines.join('\n'));
assert.ok(!out.lines.join('\n').includes(TOKEN), 'no token in its output');
const fig = (await ask({ cmd: 'figures', path: FILE }, 'figures')).figures;
assert.equal(fig.live, true);
// mkdata.mjs makes the orders and the collections beside them.
assert.ok(fig.rows >= 40 && fig.collections >= 2, JSON.stringify(fig));
assert.ok(fig.fileBytes > 0);

step('Restart with other settings: a port asked for, the network; the token is the same, so the apps keep working');
const PORT = await freePort();
n = lines().length;
assert.equal((await ask({ cmd: 'restart-served', path: FILE, settings: { port: PORT, reach: 'lan', metrics: true } }, 'restart-served-result')).ok, true);
const again = await waitFor((l, i) => i >= n && l.what === 'served' && l.path === FILE, 30000, 'served again');
assert.equal(again.origin, `http://0.0.0.0:${PORT}`);
assert.equal(again.local, `http://127.0.0.1:${PORT}`);
assert.match(again.metrics, /^http:\/\/127\.0\.0\.1:\d+$/);
assert.ok(lines().slice(n).some((l) => l.what === 'served-stopped' && l.path === FILE && l.code === 0), 'the first stopped with exit 0');
assert.equal(await status(`${again.local}/_whoami`, TOKEN), 200, 'the same token');
assert.equal(await status(`${again.metrics}/_metrics`, TOKEN), 200, 'its metrics on a port of their own');
assert.ok(serversOn(FILE)[0].command.includes(`--http 0.0.0.0:${PORT}`));

step('New token: kept now, taken at the next start; the old one is refused after it');
assert.equal((await ask({ cmd: 'new-token', path: FILE }, 'new-token-result')).ok, true);
const NEW = (await ask({ cmd: 'serve-token', path: FILE }, 'serve-token')).token;
assert.notEqual(NEW, TOKEN);
assert.equal(await status(`${again.local}/_whoami`, TOKEN), 200, 'the running server has the old one until it starts again');
n = lines().length;
await ask({ cmd: 'restart-served', path: FILE }, 'restart-served-result');
const third = await waitFor((l, i) => i >= n && l.what === 'served' && l.path === FILE, 30000, 'served a third time');
assert.equal(third.port, PORT, 'with the settings it last ran with');
assert.equal(await status(`${third.local}/_whoami`, TOKEN), 401);
assert.equal(await status(`${third.local}/_whoami`, NEW), 200);

step('a port another program answers on is taken, said so, with the way to change it');
const blocker = createServer((c) => c.end());
await new Promise((r) => blocker.listen(0, '127.0.0.1', r));
const TAKEN = blocker.address().port;
n = lines().length;
cmd({ cmd: 'open', path: OTHER });
await waitStudio(`file:${OTHER}`, n);
cmd({ cmd: 'close', key: `file:${OTHER}` });
await waitFor((l, i) => i >= n && l.what === 'server-stopped' && l.key === `file:${OTHER}`, 15000, 'stopped');
n = lines().length;
cmd({ cmd: 'serve', path: OTHER, settings: { port: TAKEN } });
const taken = await waitNotice(`Port ${TAKEN} is taken`, 15000, n);
assert.deepEqual(taken.actions, ['settings']);
assert.equal(serversOn(OTHER).length, 0, 'nothing left running on it');
blocker.close();

step('a file another program serves: said so, and no second writer');
const holder = await startServer(['--file', HELD, '--http', '127.0.0.1:0'], { FENEC_HTTP_TOKEN: 'x' });
n = lines().length;
cmd({ cmd: 'serve', path: HELD });
await waitNotice('This database is open in another program', 15000, n);
assert.equal(serversOn(HELD).length, 1, 'only the other program\'s');
await holder.stop();

step('a served database\'s server that dies: said, with Start again, which starts it with the same settings');
n = lines().length;
process.kill(third.pid, 'SIGKILL');
const died = await waitNotice('A database server stopped', 15000, n);
assert.deepEqual(died.actions, ['start']);
assert.match(entry(FILE).lastRan.why, /exited/);
await pressNotice(died, 'start');
const back = await waitFor((l, i) => i >= n && l.what === 'served' && l.path === FILE, 30000, 'served again after Start again');
assert.equal(back.port, PORT);

step('Stop: the server\'s own shutdown, exit 0, and the file is free for another program');
n = lines().length;
assert.equal((await ask({ cmd: 'stop-served', path: FILE }, 'stop-served-result')).ok, true);
const stopped = await waitFor((l, i) => i >= n && l.what === 'served-stopped' && l.path === FILE, 15000, 'stopped');
assert.equal(stopped.code, 0);
assert.equal(serversOn(FILE).length, 0);
const free = await startServer(['--file', FILE, '--http', '127.0.0.1:0'], { FENEC_HTTP_TOKEN: 'x' });
await free.stop();
assert.equal(entry(FILE).lastRan.why, 'stopped');

step('served again, and the app quits: the quit stops it, and nothing is left running');
n = lines().length;
await ask({ cmd: 'serve', path: FILE, settings: { port: PORT, reach: 'lan', metrics: true } }, 'serve-result');
await waitFor((l, i) => i >= n && l.what === 'served' && l.path === FILE, 30000, 'served');
await quit(app);
assert.equal(serversOn(FILE).length, 0, 'no server outlives the app');
assert.equal(entry(FILE).lastRan.why, 'the app quit');

step('a relaunch starts nothing: the server is listed stopped, with the settings it ran with');
app = startApp('serve-relaunch', { FENEC_DESKTOP_LAUNCHER: '1' });
await waitFor((l) => l.what === 'ready', 60000, 'ready');
await waitFor((l) => l.what === 'page-text' && l.key === 'launcher' && /Databases/.test(l.text), 20000, 'the main window drawn');
await sleep(1500); // nothing is to happen: a server that started itself would be up by now
assert.equal(serversOn(FILE).length, 0, 'no server started by itself');
assert.equal((await ask({ cmd: 'served', path: FILE }, 'served-state')).running, false);
await evalIn('launcher', `[...document.querySelectorAll('.nav')].find((b) => b.dataset.section === 'servers').click()`);
const listed = await until(async () => {
  const t = (await evalIn('launcher', `return document.getElementById('servers').innerText`)).value;
  return /served\.fenec/.test(t) && /Stopped/.test(t) && t;
}, 10000, 'the Servers list');
assert.match(listed, new RegExp(`Port ${PORT} when started`));

step('Remove from Servers: its settings and its Keychain item go');
await ask({ cmd: 'remove-served', path: FILE }, 'served-removed');
assert.ok(!keychainHas(`serve:${id}`), 'the Keychain item is gone');
assert.equal(entry(FILE).serve, undefined);
await quit(app);
assert.equal(mine().length, 0);
forgetServeItems();
log('clean');
