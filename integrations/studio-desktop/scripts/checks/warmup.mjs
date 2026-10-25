// The first launch of a build, which is not like the others.
//
// A stable Electrobun build is a self-extractor: the first time it runs it
// unpacks the app next to itself (about 2 s, with an installer window), starts
// the app as a process of its own and stays in its own event loop afterwards.
// Quitting the app then leaves that first process behind (Electrobun 2.0.4,
// seen unsigned, in place and from `open`; not verified for a signed and
// notarized build). So the checks start from a build that has been launched
// once, and this does it: start, wait for the app, quit it, end the helper.
import { cmd, describe, mine, sleep, startApp, step, waitFor, log, now } from './lib.mjs';

const app = startApp('warmup', { FENEC_DESKTOP_LAUNCHER: '1' });
await waitFor((l) => l.what === 'ready', 90000, 'the app, after unpacking itself');
await waitFor((l) => l.what === 'page-text' && l.key === 'launcher' && /Open file/.test(l.text), 20000, 'the launcher drawn');
log('the app is up');

step('quit');
cmd({ cmd: 'quit' });
// The app itself ends within a moment; what may stay is the unpacker.
const t0 = now();
while (now() - t0 < 20000 && mine().some((p) => !p.command.endsWith('/Contents/MacOS/launcher'))) await sleep(100);
const left = mine();
const real = left.filter((p) => !p.command.endsWith('/Contents/MacOS/launcher'));
if (real.length) throw new Error(`the app did not quit:\n${describe(real)}`);
if (left.length) {
  log(`the unpacker is still running (${left.map((p) => p.pid).join(', ')}): Electrobun's first launch; ended`);
  for (const p of left) process.kill(p.pid, 'SIGTERM');
  await sleep(500);
}
await Promise.race([app.exited, sleep(2000)]);
if (mine().length) throw new Error(`left behind:\n${describe(mine())}`);
log('clean');
