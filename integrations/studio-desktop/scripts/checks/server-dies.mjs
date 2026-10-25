// The server dies under an open window (killed, crashed, out of disk): the
// window closes with a message in the app's own window, the lock is the dead
// process's and so released, and the file opens again from the message.
import assert from 'node:assert/strict';
import { assertClean, data, lines, mine, pressNotice, quit, startApp, step, waitFor, waitNotice, waitStudio, log } from './lib.mjs';

assertClean('before');
const SMALL = data('small', 50);
const KEY = `file:${SMALL}`;

const app = startApp('server-dies', { FENEC_DESKTOP_OPEN: SMALL });
const started = await waitFor((l) => l.what === 'server-started', 60000, 'the server started');
await waitStudio(KEY);

step(`kill -9 the server (${started.pid})`);
process.kill(started.pid, 'SIGKILL');
const died = await waitFor((l) => l.what === 'server-died', 10000, 'server-died');
log('exit reported as', died.code);
assert.ok(died.code === null || died.code === 137, `killed by SIGKILL, not a clean exit: ${died.code}`);
const notice = await waitNotice('The database server stopped');
assert.match(notice.message, /small\.fenec/);
assert.match(notice.message, /killed by a signal|code 137/);
assert.match(notice.hint, /Nothing written before it stopped is lost/);
assert.deepEqual(notice.actions, ['retry']);
await waitFor((l) => l.what === 'window-close' && l.key === KEY, 10000, 'the studio window closing');
assert.equal(mine().filter((p) => p.command.includes(`--file ${SMALL}`)).length, 0, 'no server left');

step('Open again, from the message: a new server, the lock free');
const n = lines().length;
await pressNotice(notice, 'retry');
const again = await waitFor((l, i) => i >= n && l.what === 'server-started', 15000, 'the new server');
assert.notEqual(again.pid, started.pid);
await waitStudio(KEY, n);

step('quit');
await quit(app);
log('clean');
