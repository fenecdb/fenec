// A .fenec file opened from Finder, with the app not running: LaunchServices
// starts it and hands it the file, which arrives as an `open-url` event with a
// file: URL (src/bun/index.ts). `open -a <app> <file>` is that same path, and
// `--env` is how the hooks reach an app LaunchServices started. The file
// association itself (Info.plist's CFBundleDocumentTypes) is Electrobun's,
// from app.fileAssociations; this holds that what arrives is opened, spaces
// and non-ASCII letters in the path included.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { copyFileSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { APP, CONTROL, REPORT, R, assertClean, cmd, data, lines, logName, quit, step, until, waitFor, waitStudio, log } from './lib.mjs';

assertClean('before');
const original = data('finder', 20);
mkdirSync(`${R}/data/with space`, { recursive: true });
const FILE = `${R}/data/with space/şirket 1.fenec`;
copyFileSync(original, FILE);

/**
 * `open -a <the app> <file>`, the app not running. With `hold`, the test
 * variant keeps the file back until the launcher is up, as a slow launch
 * delivers it after the 400 ms the app waits for it.
 */
async function launch(hold) {
  logName('finder');
  rmSync(REPORT, { force: true });
  writeFileSync(CONTROL, '');
  const env = ['--env', `FENEC_DESKTOP_REPORT=${REPORT}`, '--env', `FENEC_DESKTOP_CONTROL=${CONTROL}`, ...(hold ? ['--env', 'FENEC_DESKTOP_HOLD_OPEN_URL=launcher'] : [])];
  const r = spawnSync('open', ['-a', APP, ...env, FILE], { encoding: 'utf8' });
  assert.equal(r.status, 0, r.stderr);
  const url = await waitFor((l) => l.what === 'open-url', 30000, 'the open-url event');
  log('arrived as', url.url);
  assert.ok(url.url.startsWith('file://'));
  const started = await waitFor((l) => l.what === 'server-started', 30000, 'the server for it');
  assert.equal(started.path, FILE);
  await waitStudio(`file:${FILE}`);
  // With a file to show, no launcher over it: none was shown, or, if the file was
  // handed over after it had been (a slow launch), it is closed once it is.
  await until(async () => {
    const n = lines().length;
    cmd({ cmd: 'sessions' });
    const seen = await waitFor((l, i) => i >= n && l.what === 'sessions', 5000, 'sessions');
    return !seen.pages.includes('launcher');
  }, 15000, 'no launcher window over the file');
  return lines().some((l) => l.what === 'window-created' && l.key === 'launcher');
}

step('open -a <the app> <file>, the app not running');
// Whether the launcher was shown first depends on how slow the launch was; that none is left over the file does not.
await launch(false);
await quit({ exited: Promise.resolve() });

step('the same, the file handed over late: the launcher was shown first, and is closed when the file comes');
assert.equal(await launch(true), true, 'the launcher was up before the file came (the test holds it back until it is)');
step('quit');
await quit({ exited: Promise.resolve() });
log('clean');
