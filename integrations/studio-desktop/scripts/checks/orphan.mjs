// The app is killed (SIGKILL: Activity Monitor's Force Quit, a crash). macOS has
// no parent-death signal, so a child is not told; but the system closes every
// descriptor a dead process held, and each server's stdin is a pipe the app
// held the write end of (`--exit-on-stdin-close`, src/bun/server.ts): the
// server sees it end and shuts down as on SIGTERM, its lock let go.
//
// Two files, opened one after the other: the second server is started by a
// process that already holds the first's pipe, and if that write end leaked
// into it (a descriptor not marked close-on-exec) the first would hold on
// until the second ended, and the second would keep no more than itself. Both
// ending, and both locks free, is the check.
import assert from 'node:assert/strict';
import { assertClean, data, describe, mine, procs, startApp, startServer, step, until, waitFor, waitStudio, log } from './lib.mjs';

assertClean('before');
const A = data('small', 50);
const B = data('second', 20);

const app = startApp('orphan', { FENEC_DESKTOP_OPEN: `${A}:${B}` });
const a = await waitFor((l) => l.what === 'server-started' && l.path === A, 60000, 'the first server started');
const b = await waitFor((l) => l.what === 'server-started' && l.path === B, 60000, 'the second server started');
await waitStudio(`file:${A}`);
await waitStudio(`file:${B}`);

step(`SIGKILL the app (everything of it but its servers ${a.pid} and ${b.pid})`);
for (const p of mine().filter((p) => !p.command.includes('fenec-server'))) {
  try {
    process.kill(p.pid, 'SIGKILL');
  } catch {
    // gone with its parent
  }
}

step('both servers shut down by themselves: the end of their stdin is the event');
const gone = (pid) => !procs().some((p) => p.pid === pid);
await until(() => gone(a.pid) && gone(b.pid), 20000, 'both servers ending');
log('both gone');

step('and each file is free: another program opens it');
for (const file of [A, B]) {
  const probe = await startServer(['--file', file, '--http', '127.0.0.1:0'], { FENEC_HTTP_TOKEN: 'x' });
  await probe.stop();
}
assert.equal(mine().length, 0, `nothing left:\n${describe(mine())}`);
void app;
log('clean');
