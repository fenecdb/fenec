// What a page keeps (localStorage: the studio's query history, saved queries
// and theme) is kept in a WebKit data store, and Electrobun's window makes its
// page's view in one store shared by every window, apart only by origin. A
// file's server listens on a port the system picks, so a later window on a port
// that was used before would show the earlier file's history. A studio window
// is given a store of its own: ephemeral for a file's, `persist:remote-<id>`
// for a saved server's, kept between runs and apart from every other's.
import assert from 'node:assert/strict';
import { assertClean, cmd, data, evalIn, lines, quit, startApp, startServer, step, waitFor, waitStudio, log } from './lib.mjs';

assertClean('before');
const REMOTE = data('remote', 10);
const SMALL = data('small', 10);
const TOKEN = 'storage-check-token';
const server = await startServer(['--file', REMOTE, '--studio', '--http', '127.0.0.1:0'], { FENEC_HTTP_TOKEN: TOKEN });

const app = startApp('storage', { FENEC_DESKTOP_OPEN: SMALL });
await waitFor((l) => l.what === 'server-started', 60000, 'the file opened');
const FILE_KEY = `file:${SMALL}`;
await waitStudio(FILE_KEY);

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
const read = async (key, name) => (await evalIn(key, `return localStorage.getItem(${JSON.stringify(name)})`)).value;
const write = (key, name, value) => evalIn(key, `localStorage.setItem(${JSON.stringify(name)}, ${JSON.stringify(value)}); return 1`);
const close = async (key) => {
  const n = lines().length;
  cmd({ cmd: 'close', key });
  await waitFor((l, i) => i >= n && l.what === 'window-close' && l.key === key, 15000, `${key} closing`);
};

step('the window fills its window: the page is as large as it');
const size = await evalIn(FILE_KEY, `return [innerWidth, innerHeight]`);
assert.ok(size.value[0] >= 400 && size.value[1] >= 300, `the page is the window's size, not a corner of it: ${size.value}`);

step('two saved servers on one address do not share what their windows keep');
const a = await save('server a');
const b = await save('server b');
await openRemote(a);
await openRemote(b);
await write(`remote:${a}`, 'fenec-test-history', 'select * from a');
assert.equal(await read(`remote:${a}`, 'fenec-test-history'), 'select * from a');
assert.equal(await read(`remote:${b}`, 'fenec-test-history'), null, "server b's window does not see server a's");

step("a saved server's window keeps it between opens (the connection's own store)");
await close(`remote:${a}`);
await openRemote(a);
assert.equal(await read(`remote:${a}`, 'fenec-test-history'), 'select * from a', 'still there');

step("a file's window keeps nothing past its close: no later window on the same port sees it");
await write(FILE_KEY, 'fenec-test-history', 'select * from file');
assert.equal(await read(FILE_KEY, 'fenec-test-history'), 'select * from file');
await close(FILE_KEY);
await waitFor((l) => l.what === 'server-stopped' && l.key === FILE_KEY, 15000, 'its server stopped');
let n = lines().length;
cmd({ cmd: 'open', path: SMALL });
await waitFor((l, i) => i >= n && l.what === 'server-started', 20000, 'reopened');
await waitStudio(FILE_KEY, n);
assert.equal(await read(FILE_KEY, 'fenec-test-history'), null, 'a new window, a new store');

step("removing a saved server removes what its windows kept");
await close(`remote:${a}`);
n = lines().length;
cmd({ cmd: 'rmconn', id: a });
await waitFor((l, i) => i >= n && l.what === 'conn-removed' && l.id === a, 10000, 'removed');
// The same name again is a new connection with a new store, so the old one is
// reached by opening a window on the address under the old id: it is gone.
const a2 = await save('server a again');
await openRemote(a2);
assert.equal(await read(`remote:${a2}`, 'fenec-test-history'), null);

step('quit');
cmd({ cmd: 'rmconn', id: b });
cmd({ cmd: 'rmconn', id: a2 });
await server.stop();
await quit(app);
log('clean');
