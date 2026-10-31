// Backups: an image of a database taken through a server -- one started for
// the backup alone when nothing runs on the file, the window's own when one
// does -- into a folder of the user's; verified; restored into a new file that
// opens, never over one that is there, the open database's least of all;
// sealed with a key file the app makes with `fenec key`; a new database
// started from a backup; and the refusal for a server that syncs nothing.
import assert from 'node:assert/strict';
import { copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync } from 'node:fs';
import { assertClean, cmd, data, forgetServeItems, lines, procs, quit, R, startApp, step, waitFor, waitStudio, log } from './lib.mjs';

assertClean('before');
const FILE = data('bk', 30);
const ARCHIVE = `${R}/bk-backups/bk`;
const OUT = `${R}/bk-restored`;
const KEYS = `${R}/bk-keys`;
for (const d of [`${R}/bk-backups`, OUT, KEYS]) rmSync(d, { recursive: true, force: true });
mkdirSync(OUT, { recursive: true });
mkdirSync(KEYS, { recursive: true });

async function ask(c, what, ms = 60000) {
  const n = lines().length;
  cmd(c);
  return waitFor((l, i) => i >= n && l.what === what, ms, what);
}
const serversOn = (path) => procs().filter((p) => /fenec-server /.test(p.command) && p.command.includes(`--file ${path} `));
const noScratch = (dir) => assert.deepEqual(readdirSync(dir).filter((f) => f.includes('.restoring-')), [], `no scratch file left in ${dir}`);

const app = startApp('backups', { FENEC_DESKTOP_LAUNCHER: '1' });
await waitFor((l) => l.what === 'ready', 60000, 'ready');

step('where its backups go; a key file beside the backups is refused');
assert.equal((await ask({ cmd: 'set-archive', path: FILE, dir: ARCHIVE }, 'set-archive-result')).ok, true);
assert.equal((await ask({ cmd: 'backup', path: `${R}/data/never-set-up.fenec` }, 'backup-result')).ok, false, 'a database with no folder set up is told so');

step('a backup with nothing running on the file: a server for that alone, stopped after');
let n = lines().length;
const first = await ask({ cmd: 'backup', path: FILE }, 'backup-result');
assert.equal(first.ok, true, first.message);
assert.match(first.message, /at change \d+/);
assert.ok(lines().slice(n).some((l) => l.what === 'task-server-started' && l.path === FILE), 'a server started for it');
assert.ok(lines().slice(n).some((l) => l.what === 'task-server-stopped' && l.path === FILE && l.code === 0), 'and stopped with exit 0');
assert.ok(!lines().slice(n).some((l) => l.what === 'window-opened'), 'no window');
assert.equal(serversOn(FILE).length, 0, 'nothing left running on it');
assert.equal(readdirSync(ARCHIVE).filter((f) => f.startsWith('image-')).length, 1);

step('a backup of the file open in a window: through the window\'s own server, no second one');
n = lines().length;
cmd({ cmd: 'open', path: FILE });
await waitStudio(`file:${FILE}`, n);
n = lines().length;
assert.equal((await ask({ cmd: 'backup', path: FILE }, 'backup-result')).ok, true);
assert.ok(!lines().slice(n).some((l) => l.what === 'task-server-started'), 'the window\'s server took it');
assert.equal(serversOn(FILE).length, 1);

step('verify: what the folder can restore');
const verified = await ask({ cmd: 'verify', path: FILE }, 'verify-result');
assert.equal(verified.ok, true, verified.message);
assert.match(verified.message, /2 images/);

step('restore into a new file: made, and opened');
n = lines().length;
const restored = await ask({ cmd: 'restore', path: FILE, folder: OUT, name: 'bk-copy' }, 'restore-result');
assert.equal(restored.ok, true, restored.message);
assert.equal(restored.path, `${OUT}/bk-copy.fenec`);
const copy = await waitStudio(`file:${OUT}/bk-copy.fenec`, n);
assert.match(copy.detail.side, /orders 30/, 'what the database held');

step('two restores into one name at once: one file, made whole; the other refused');
n = lines().length;
const twice = await ask({ cmd: 'restore-twice', path: FILE, folder: OUT, name: 'bk-twice' }, 'restore-twice-result', 120000);
assert.equal(twice.results.filter((r) => r.ok).length, 1, JSON.stringify(twice.results));
assert.match(twice.results.find((r) => !r.ok).message, /exists in that folder already/, 'the second waited for the first, and found its file');
await waitStudio(`file:${OUT}/bk-twice.fenec`, n);
noScratch(OUT);

step('never over a file: the same name again, and the open database\'s own name');
const before = readFileSync(`${OUT}/bk-copy.fenec`);
const again = await ask({ cmd: 'restore', path: FILE, folder: OUT, name: 'bk-copy' }, 'restore-result');
assert.equal(again.ok, false);
assert.match(again.message, /exists in that folder already/);
assert.deepEqual(readFileSync(`${OUT}/bk-copy.fenec`), before);
const liveBefore = statSync(FILE).size;
const over = await ask({ cmd: 'restore', path: FILE, folder: `${R}/data`, name: 'bk' }, 'restore-result');
assert.equal(over.ok, false, 'the open, served file is not written over');
assert.equal(statSync(FILE).size, liveBefore);
noScratch(OUT);
noScratch(`${R}/data`);

step('sealed with a new key: made with fenec key, beside nothing of the backups; read only with it');
const key = await ask({ cmd: 'make-key', folder: KEYS, name: 'bk.key' }, 'make-key-result');
assert.equal(key.ok, true, key.message);
assert.equal(statSync(`${KEYS}/bk.key`).mode & 0o777, 0o600);
assert.equal((await ask({ cmd: 'make-key', folder: KEYS, name: 'bk.key' }, 'make-key-result')).ok, false, 'a key is never written over');
const SEALED = `${R}/bk-backups/bk-sealed`;
assert.equal((await ask({ cmd: 'set-archive', path: FILE, dir: SEALED, keyFile: `${SEALED}/k.key` }, 'set-archive-result')).ok, false, 'a key file that is not there');
// A key anywhere under the folder that was picked goes wherever the backups are copied: refused.
copyFileSync(`${KEYS}/bk.key`, `${R}/bk-backups/near.key`);
const near = await ask({ cmd: 'set-archive', path: FILE, dir: `${R}/bk-backups/bk`, keyFile: `${R}/bk-backups/near.key` }, 'set-archive-result');
assert.equal(near.ok, false, 'a key beside the backups\' folder, in the folder that was picked');
assert.match(near.message, /inside/);
assert.equal((await ask({ cmd: 'set-archive', path: FILE, dir: SEALED, keyFile: `${KEYS}/bk.key` }, 'set-archive-result')).ok, true);
assert.equal((await ask({ cmd: 'backup', path: FILE }, 'backup-result')).ok, true);
const image = readdirSync(SEALED).find((f) => f.startsWith('image-'));
assert.ok(!readFileSync(`${SEALED}/${image}`).subarray(0, 7).equals(Buffer.from('FENECDB')), 'not a database in the clear');
assert.equal((await ask({ cmd: 'verify', path: FILE }, 'verify-result')).ok, true);
n = lines().length;
const unsealed = await ask({ cmd: 'restore', path: FILE, folder: OUT, name: 'bk-unsealed' }, 'restore-result');
assert.equal(unsealed.ok, true, unsealed.message);
await waitStudio(`file:${OUT}/bk-unsealed.fenec`, n);

step('a new database from a folder of backups');
n = lines().length;
const made = await ask({ cmd: 'create', folder: OUT, name: 'from-backup', start: 'archive', archive: { dir: ARCHIVE, keyFile: null } }, 'create-result');
assert.equal(made.ok, true, made.message);
const fromBackup = await waitStudio(`file:${OUT}/from-backup.fenec`, n);
assert.match(fromBackup.detail.side, /orders 30/);
noScratch(OUT);

step('a database served with writes the system syncs is not backed up while it runs, and says why');
const OFF = data('bk-off', 3);
assert.equal((await ask({ cmd: 'set-archive', path: OFF, dir: `${R}/bk-backups/off` }, 'set-archive-result')).ok, true);
n = lines().length;
assert.equal((await ask({ cmd: 'serve', path: OFF, sync: 'off' }, 'serve-result')).ok, true);
await waitFor((l, i) => i >= n && l.what === 'served' && l.path === OFF, 30000, 'served');
const refused = await ask({ cmd: 'backup', path: OFF }, 'backup-result');
assert.equal(refused.ok, false);
assert.match(refused.message, /When the system writes/);
assert.ok(!existsSync(`${R}/bk-backups/off`) || readdirSync(`${R}/bk-backups/off`).length === 0, 'nothing written');

step('quit');
await quit(app);
forgetServeItems();
log('clean');
