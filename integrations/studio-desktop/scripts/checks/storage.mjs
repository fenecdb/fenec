// What a page keeps (localStorage: the studio's query history, saved queries
// and theme) is kept in a WebKit data store, and Electrobun's window makes its
// page's view in one store shared by every window, apart only by origin. A
// file's server listens on a port the system picks, so a later window on a port
// that was used before would show the earlier file's history. A studio window
// is given a store of its own: ephemeral for a file's, `persist:remote-<id>` for
// a saved server's, kept between runs, apart from every other's, and emptied
// when the connection is removed.
//
// A second window on the same origin is how a store is read from outside the
// window that wrote it (the `store-window` hook: a window on a running
// server's origin, in the data store it is told): under one shared store it
// would see what the first wrote, and these fail.
import assert from 'node:assert/strict';
import { assertClean, cmd, data, evalIn, lines, quit, startApp, startServer, step, until, waitFor, waitStudio, log } from './lib.mjs';

assertClean('before');
const REMOTE = data('remote', 10);
const SMALL = data('small', 10);
const TOKEN = 'storage-check-token';
const server = await startServer(['--file', REMOTE, '--studio', '--http', '127.0.0.1:0'], { FENEC_HTTP_TOKEN: TOKEN });

const app = startApp('storage', { FENEC_DESKTOP_OPEN: SMALL });
await waitFor((l) => l.what === 'server-started', 60000, 'the file opened');
const FILE_KEY = `file:${SMALL}`;
await waitStudio(FILE_KEY);

let windowsMade = 0;
const NAME = 'fenec-test-history';
const read = async (key) => (await evalIn(key, `return localStorage.getItem(${JSON.stringify(NAME)})`)).value;
const write = (key, value) => evalIn(key, `localStorage.setItem(${JSON.stringify(NAME)}, ${JSON.stringify(value)}); return 1`);
const close = async (key) => {
  const n = lines().length;
  cmd({ cmd: 'close', key });
  await waitFor((l, i) => i >= n && l.what === 'window-close' && l.key === key, 15000, `${key} closing`);
};
/** A new window on a server's origin, in the store `spec` names; its key. */
async function storeWindow(spec) {
  const key = `store:${++windowsMade}`;
  const n = lines().length;
  cmd({ cmd: 'store-window', key, ...spec });
  await waitFor((l, i) => i >= n && l.what === 'store-window-opened' && l.key === key, 20000, `${key} opened`);
  await waitStudio(key, n);
  return key;
}
const save = async (name) => {
  const n = lines().length;
  cmd({ cmd: 'addconn', name, url: server.origin, token: TOKEN, tenant: '' });
  return (await waitFor((l, i) => i >= n && l.what === 'conn-added' && l.name === name, 10000, `${name} saved`)).id;
};
const openRemote = async (id) => {
  const n = lines().length;
  cmd({ cmd: 'openconn', id });
  await waitFor((l, i) => i >= n && l.what === 'openconn-result' && l.id === id && l.ok, 20000, 'openconn-result');
  await waitStudio(`remote:${id}`, n);
};
const remote = (id) => ({ connection: id, origin: server.origin, token: TOKEN });

step('the window fills its window: the page is as large as it');
const size = await evalIn(FILE_KEY, `return [innerWidth, innerHeight]`);
assert.ok(size.value[0] >= 400 && size.value[1] >= 300, `the page is the window's size, not a corner of it: ${size.value}`);

step('the control: two windows in one persistent store see each other\'s writes, so a window opened by the hook can read what another wrote');
// A file's store is ephemeral and per window (a second window given the same name gets a store of its
// own as well), so the control for "does not see it" is a saved server's, which two windows share.
const control = await save('control');
const writer = await storeWindow(remote(control));
await write(writer, 'select * from control');
const reader = await storeWindow(remote(control));
assert.equal(await read(reader), 'select * from control', 'the same persistent store shows it');
await close(writer);
await close(reader);
let n = lines().length;
cmd({ cmd: 'rmconn', id: control });
await waitFor((l, i) => i >= n && l.what === 'conn-removed' && l.id === control, 15000, 'control removed');

step("a file's window has a store of its own: another window on the same origin does not see it");
await write(FILE_KEY, 'select * from file');
assert.equal(await read(FILE_KEY), 'select * from file');
// Another window on the file server's origin, in a store of its own: under one shared store (the default
// store of Electrobun's window, shared by origin) it would see the file window's history.
const stranger = await storeWindow({ of: FILE_KEY });
assert.equal(await read(stranger), null, "a window of another store on the same origin does not see the file window's");
await close(stranger);
// And the other direction: what the stranger writes is not in the file window's.
const stranger2 = await storeWindow({ of: FILE_KEY });
await write(stranger2, 'select * from stranger');
assert.equal(await read(FILE_KEY), 'select * from file', "the file window's own history is as it was");
await close(stranger2);

step('two saved servers on one address do not share what their windows keep');
const a = await save('server a');
const b = await save('server b');
await openRemote(a);
await openRemote(b);
await write(`remote:${a}`, 'select * from a');
assert.equal(await read(`remote:${a}`), 'select * from a');
assert.equal(await read(`remote:${b}`), null, "server b's window does not see server a's");

step("a saved server's window keeps it between opens (the connection's own store)");
await close(`remote:${a}`);
await openRemote(a);
assert.equal(await read(`remote:${a}`), 'select * from a', 'still there');

step('removing a connection empties its store, a window open on it included');
await write(`remote:${b}`, 'select * from b');
// A window on the connection's store, from outside, before: it holds what was written.
await close(`remote:${a}`);
const beforeA = await storeWindow(remote(a));
assert.equal(await read(beforeA), 'select * from a', "the connection's store holds it before its removal (the control)");
await close(beforeA);
n = lines().length;
cmd({ cmd: 'rmconn', id: a });
await waitFor((l, i) => i >= n && l.what === 'conn-removed' && l.id === a, 15000, 'a removed');
// The same store, reopened under the removed id, is empty.
const afterA = await storeWindow(remote(a));
await until(async () => (await read(afterA)) === null, 10000, "the removed connection's store emptied");
await close(afterA);
// b's window is open while it is removed: it is closed first, and its store is emptied too.
n = lines().length;
cmd({ cmd: 'rmconn', id: b });
await waitFor((l, i) => i >= n && l.what === 'window-close' && l.key === `remote:${b}`, 15000, "b's window closed by its removal");
await waitFor((l, i) => i >= n && l.what === 'conn-removed' && l.id === b, 15000, 'b removed');
const afterB = await storeWindow(remote(b));
await until(async () => (await read(afterB)) === null, 10000, "the store of a connection removed with its window open emptied");

step('quit');
await server.stop();
await quit(app);
log('clean');
