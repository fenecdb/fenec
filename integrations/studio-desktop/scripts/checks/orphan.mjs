// The app is killed (SIGKILL: Activity Monitor's Force Quit, a crash). macOS has
// no parent-death signal, so a child is not told; but the system closes every
// descriptor a dead process held, and each server's stdin is a pipe the app
// held the write end of (`--exit-on-stdin-close`, src/bun/server.ts): the
// server sees it end and shuts down as on SIGTERM, its lock let go.
//
// Two files, opened one after the other: the second server is started by a
// process that already holds the first's pipe, and if that write end leaked
// into it (a descriptor not marked close-on-exec) the first would not see its
// stdin end for as long as the second ran. Both ending is no proof there was
// no leak (the second ends when the app's death closes its own stdin, and then
// lets the first's go), so the check looks at who holds what **while the app is
// alive**: the descriptor tables (lsof) say which processes hold the write end
// of each server's stdin, and it is the app's alone. A control starts a process
// that does hold another's write end and requires the same reading to find it,
// so an empty answer is not a reading that sees nothing.
//
// (An earlier version stopped the second server with SIGSTOP and required the
// first to end meanwhile. On a hosted macOS runner a stopped process made `ps`
// hang, and with it the runner's own cleanup: the job never finished, not even
// by its timeout. A process is not stopped here.)
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { assertClean, data, describe, mine, procs, startApp, startServer, step, until, waitFor, waitStudio, log } from './lib.mjs';

/** The pipes and sockets each pid holds, from lsof: pid -> [{fd, type, own, peer}], `own` and `peer` the kernel's addresses of the two ends. */
function ends(pids) {
  const r = spawnSync('lsof', ['-nP', '-p', pids.join(','), '-F', 'pftdn'], { encoding: 'utf8', maxBuffer: 1 << 26 });
  const table = new Map();
  let pid;
  let fd;
  for (const line of r.stdout.split('\n')) {
    const k = line[0];
    const v = line.slice(1);
    if (k === 'p') {
      pid = +v;
      table.set(pid, []);
    } else if (k === 'f') {
      fd = { fd: v, type: '', own: '', peer: '' };
      table.get(pid).push(fd);
    } else if (k === 't') fd.type = v;
    else if (k === 'd') fd.own = v;
    else if (k === 'n' && v.startsWith('->')) fd.peer = v.slice(2);
  }
  for (const [p, list] of table) table.set(p, list.filter((e) => (e.type === 'PIPE' || e.type === 'unix') && e.own && e.peer));
  return table;
}

/** Who holds the other end of `pid`'s stdin: the pids (of those in `table`) with a descriptor whose own address is its peer's. */
function writersOfStdin(table, pid) {
  const stdin = (table.get(pid) ?? []).find((e) => e.fd === '0');
  assert.ok(stdin, `${pid} has a pipe or socket for a stdin (lsof: ${JSON.stringify(table.get(pid))})`);
  return [...table].filter(([p, list]) => p !== pid && list.some((e) => e.own === stdin.peer)).map(([p]) => p);
}

assertClean('before');

step('the control: a process that holds another one\'s stdin write end is found');
{
  const reader = spawn('cat', [], { stdio: ['pipe', 'ignore', 'ignore'] });
  // Handed the write end as a descriptor of its own, which is the leak.
  const leaker = spawn('sleep', ['30'], { stdio: ['ignore', 'ignore', 'ignore', reader.stdin] });
  await new Promise((r) => setTimeout(r, 300));
  const found = writersOfStdin(ends([reader.pid, leaker.pid, process.pid]), reader.pid);
  reader.kill();
  leaker.kill();
  assert.deepEqual(found.sort(), [leaker.pid, process.pid].sort(), 'the check sees this process and the one that was handed the descriptor');
  log('found:', found.length, 'holders, the leaker among them');
}

const A = data('small', 50);
const B = data('second', 20);

const app = startApp('orphan', { FENEC_DESKTOP_OPEN: `${A}:${B}` });
const a = await waitFor((l) => l.what === 'server-started' && l.path === A, 60000, 'the first server started');
const b = await waitFor((l) => l.what === 'server-started' && l.path === B, 60000, 'the second server started');
await waitStudio(`file:${A}`);
await waitStudio(`file:${B}`);

step('the write end of each server\'s stdin is held by the app, and by no server');
const appPids = mine().filter((p) => !p.command.includes('fenec-server')).map((p) => p.pid);
assert.ok(appPids.length > 0, 'the app\'s own processes');
const table = ends([a.pid, b.pid, ...appPids]);
for (const [name, server] of [['A', a], ['B', b]]) {
  const writers = writersOfStdin(table, server.pid);
  log(`${name} (${server.pid}) is written to by`, writers.join(', '));
  assert.ok(writers.length > 0, `${name}'s stdin is written to by somebody (else the reading sees nothing)`);
  assert.deepEqual(writers.filter((p) => !appPids.includes(p)), [], `${name}'s stdin is held by the app alone, not by the other server`);
}

step('SIGKILL the app (everything of it but its servers): both servers end by themselves');
for (const pid of appPids) {
  try {
    process.kill(pid, 'SIGKILL');
  } catch {
    // gone with its parent
  }
}
const gone = (pid) => !procs().some((p) => p.pid === pid);
await until(() => gone(a.pid) && gone(b.pid), 30000, 'both servers ending');
log('both gone');

step('and each file is free: another program opens it');
for (const file of [A, B]) {
  const probe = await startServer(['--file', file, '--http', '127.0.0.1:0'], { FENEC_HTTP_TOKEN: 'x' });
  await probe.stop();
}
assert.equal(mine().length, 0, `nothing left:\n${describe(mine())}`);
void app;
log('clean');
