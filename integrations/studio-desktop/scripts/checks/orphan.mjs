// The app is killed (SIGKILL: Activity Monitor's Force Quit, a crash). macOS
// has no parent-death signal, so its server is orphaned and keeps the file's
// lock; the next launch of the app stops it (src/bun/strays.ts). The gap goes
// away with `fenec-server --exit-on-stdin-close`, and with it this check.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { assertClean, data, describe, mine, procs, quit, startApp, startServer, step, until, USER_DATA, waitFor, waitStudio, log, lines } from './lib.mjs';

assertClean('before');
const SMALL = data('small', 50);

const first = startApp('orphan-1', { FENEC_DESKTOP_OPEN: SMALL });
const started = await waitFor((l) => l.what === 'server-started', 60000, 'the server started');
await waitStudio(`file:${SMALL}`);

step(`SIGKILL the app (everything of it but the server ${started.pid})`);
for (const p of mine().filter((p) => !p.command.includes('fenec-server'))) {
  try {
    process.kill(p.pid, 'SIGKILL');
  } catch {
    // gone with its parent
  }
}
// Left to launchd once its parent is gone: that is the event.
const orphan = await until(() => procs().find((p) => p.pid === started.pid && p.ppid === 1), 15000, 'the server reparented to launchd');
assert.ok(orphan, 'the server outlived the app, left to launchd');
log('orphaned:', describe([orphan]));
assert.ok(JSON.parse(readFileSync(`${USER_DATA}/servers.json`, 'utf8')).some((e) => e.pid === started.pid), 'it was noted in servers.json');

step('another program on the file is refused meanwhile: the orphan holds the lock');
await assert.rejects(startServer(['--file', SMALL, '--http', '127.0.0.1:0'], { FENEC_HTTP_TOKEN: 'x' }), /it is open already/);

step('the next launch stops it');
const second = startApp('orphan-2', { FENEC_DESKTOP_LAUNCHER: '1' });
await waitFor((l) => l.what === 'ready', 60000, 'ready');
// Reported once the reaper has waited for it to be gone, and the app does not
// open anything before: the event, then the process.
const reaped = await waitFor((l) => l.what === 'reaped-strays', 30000, 'reaped-strays');
assert.deepEqual(reaped.strays, [SMALL]);
assert.deepEqual(reaped.left, [], 'nothing survived');
await until(() => !procs().some((p) => p.pid === started.pid), 10000, 'the orphan gone');
assert.deepEqual(JSON.parse(readFileSync(`${USER_DATA}/servers.json`, 'utf8')), [], 'and no longer noted');
log('reaped', reaped.strays);

step('and the file opens');
const probe = await startServer(['--file', SMALL, '--http', '127.0.0.1:0'], { FENEC_HTTP_TOKEN: 'x' });
await probe.stop();

step('quit');
await quit(second);
log('clean');
