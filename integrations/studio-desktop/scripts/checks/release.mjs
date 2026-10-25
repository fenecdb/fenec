// The shipped app holds none of what the checks drive the test variant with:
// no hook that reads a file of commands, none that reports, no environment
// variable that changes what it does. The test variant is the positive
// control: the same strings are in it, so a rename here cannot pass this
// check by looking for something that was never there. (Where there is no test
// variant -- the signing job builds only the app it ships -- the shipped bundle
// is held to the markers alone.)
//
// An environment variable the shipped app reads for itself is a hook, unless
// it is read behind `__TESTABLE__`: this list is every one there is.
//
//   node scripts/checks/release.mjs [release .app] [test .app]
//
// A stable build is a self-extracting wrapper: the app is a .tar.zst in the
// wrapper's Resources until its first launch, and this reads the main
// process's bundle out of it without running anything.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync, readdirSync } from 'node:fs';
import { HERE, log } from './lib.mjs';

const release = process.argv[2] ?? `${HERE}build/stable-macos-arm64/fenec studio.app`;
const test = process.argv[3] ?? `${HERE}build-test/stable-macos-arm64/fenec studio test.app`;

/** The main process's bundle and the notice page, out of an app's payload, or of the app itself once it has been launched. */
function bundle(app) {
  const resources = `${app}/Contents/Resources`;
  const payload = readdirSync(resources).find((f) => f.endsWith('.tar.zst'));
  const read = (inner) =>
    payload
      ? execFileSync('tar', ['-xOf', `${resources}/${payload}`, `${app.split('/').pop()}/Contents/Resources/app/${inner}`], { maxBuffer: 1 << 28 }).toString('utf8')
      : execFileSync('cat', [`${resources}/app/${inner}`], { maxBuffer: 1 << 28 }).toString('utf8');
  return read('bun/index.js');
}

const HOOKS = ['FENEC_DESKTOP_REPORT', 'FENEC_DESKTOP_CONTROL', 'FENEC_DESKTOP_OPEN', 'FENEC_DESKTOP_LAUNCHER', 'FENEC_DESKTOP_HOLD_OPEN_URL', 'FENEC_SERVER_BIN', '"close-direct"', 'reaped-strays'];

const shipped = bundle(release);
const testing = existsSync(test) ? bundle(test) : null;
for (const marker of HOOKS) {
  if (testing) assert.ok(testing.includes(marker), `the test variant has ${marker} (so this check can see it)`);
  assert.ok(!shipped.includes(marker), `the shipped app must not hold ${marker}`);
}
// The constants the build sets: neither is left in the shipped code as a name to test.
assert.ok(!shipped.includes('__TESTABLE__'), '__TESTABLE__ was replaced by the bundler');
assert.ok(!shipped.includes('testhooks'), 'no module of hooks in the bundle');
log(`shipped bundle ${(shipped.length / 1024).toFixed(0)} KB holds none of ${HOOKS.length} hook markers` + (testing ? `; the test variant ${(testing.length / 1024).toFixed(0)} KB holds all` : ' (no test variant here to compare with)'));
