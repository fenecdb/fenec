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
import { APP, CONTROL, REPORT, R, assertClean, data, logName, quit, step, waitFor, waitStudio, log } from './lib.mjs';

assertClean('before');
const original = data('finder', 20);
mkdirSync(`${R}/data/with space`, { recursive: true });
const FILE = `${R}/data/with space/şirket 1.fenec`;
copyFileSync(original, FILE);

step('open -a <the app> <file>, the app not running');
logName('finder');
rmSync(REPORT, { force: true });
writeFileSync(CONTROL, '');
const r = spawnSync('open', ['-a', APP, '--env', `FENEC_DESKTOP_REPORT=${REPORT}`, '--env', `FENEC_DESKTOP_CONTROL=${CONTROL}`, FILE], { encoding: 'utf8' });
assert.equal(r.status, 0, r.stderr);
const url = await waitFor((l) => l.what === 'open-url', 30000, 'the open-url event');
log('arrived as', url.url);
assert.ok(url.url.startsWith('file://'));
const started = await waitFor((l) => l.what === 'server-started', 20000, 'the server for it');
assert.equal(started.path, FILE);
await waitStudio(`file:${FILE}`);
// With a file to show, no launcher over it.
assert.ok(!(await import('./lib.mjs')).lines().some((l) => l.what === 'page-text' && l.key === 'launcher'), 'no launcher window');

step('quit');
await quit({ exited: Promise.resolve() });
log('clean');
