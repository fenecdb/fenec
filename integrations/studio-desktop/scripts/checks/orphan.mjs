// The app is killed (SIGKILL: Activity Monitor's Force Quit, a crash). macOS has
// no parent-death signal, so a child is not told; but the system closes every
// descriptor a dead process held, and each server's stdin is a pipe the app
// held the write end of (`--exit-on-stdin-close`, src/bun/server.ts): the
// server sees it end and shuts down as on SIGTERM, its lock let go.
//
// Two files, opened one after the other: the second server is started by a
// process that already holds the first's pipe, and if that write end leaked
// into it (a descriptor not marked close-on-exec) the first would not see its
// stdin end for as long as the second ran. So the second is **stopped**
// (SIGSTOP) before the app is killed: the first must end while the second
// cannot, and only then is the second let go to end in its turn. Without
// that, a leak shows only as the first ending a moment after the second, and
// both ending is no proof there was none.
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

step(`stop server B (${b.pid}) where it is, then SIGKILL the app (everything of it but its servers)`);
process.kill(b.pid, 'SIGSTOP');
for (const p of mine().filter((p) => !p.command.includes('fenec-server'))) {
  try {
    process.kill(p.pid, 'SIGKILL');
  } catch {
    // gone with its parent
  }
}

step(`server A (${a.pid}) ends by itself while B cannot run: the end of its stdin is the event, and nothing of B's holds it open`);
const gone = (pid) => !procs().some((p) => p.pid === pid);
await until(() => gone(a.pid), 20000, 'A ending while B is stopped');
assert.equal(gone(b.pid), false, 'B is still there, stopped');
log('A is gone, B is not');

step('B is let go: it sees its own stdin end and ends too');
process.kill(b.pid, 'SIGCONT');
await until(() => gone(b.pid), 20000, 'B ending once it runs');
log('both gone');

step('and each file is free: another program opens it');
for (const file of [A, B]) {
  const probe = await startServer(['--file', file, '--http', '127.0.0.1:0'], { FENEC_HTTP_TOKEN: 'x' });
  await probe.stop();
}
assert.equal(mine().length, 0, `nothing left:\n${describe(mine())}`);
void app;
log('clean');
