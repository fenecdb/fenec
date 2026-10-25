// Runs the scripted checks one after another, each in a process of its own,
// and says which passed. `make studio-desktop-test` runs it after the unit
// tests; it needs a window server (a logged-in Mac, or a CI runner's) and
// the test variant of the app built (FENEC_DESKTOP_TEST_BUILD=1).
//
//   node scripts/checks/run.mjs            # all of them
//   ONLY=files,remote node scripts/checks/run.mjs
//
// Each check's output goes to $SD_DIR/logs/<check>.log and, as the check
// goes, the app's own to <check>.app.log; a CI run uploads that directory
// when it fails. A check that leaves a process behind is killed here, so the
// next one starts clean, and fails for it.
import { spawn } from 'node:child_process';
import { closeSync, mkdirSync, openSync, rmSync, writeSync } from 'node:fs';
import { homedir } from 'node:os';
import { D, HERE, IDENTIFIER, describe, mine } from './lib.mjs';

const ALL = ['release', 'warmup', 'files', 'server-dies', 'orphan', 'sandbox', 'pages', 'storage', 'environment', 'remote', 'newdb', 'finder'];
const only = (process.env.ONLY ?? '').split(',').filter(Boolean);
const names = only.length ? ALL.filter((n) => only.includes(n)) : ALL;
const LIMIT_MS = 240_000;

// The test variant's own state, never a real install's: a different identifier.
rmSync(`${homedir()}/Library/Application Support/${IDENTIFIER}`, { recursive: true, force: true });
rmSync(`${D}/data`, { recursive: true, force: true });
mkdirSync(`${D}/data`, { recursive: true });
mkdirSync(`${D}/logs`, { recursive: true });

const results = [];
for (const name of names) {
  const t0 = Date.now();
  const log = openSync(`${D}/logs/${name}.log`, 'w');
  const child = spawn(process.execPath, [`${HERE}scripts/checks/${name}.mjs`], { stdio: ['ignore', 'pipe', 'pipe'] });
  const echo = (b) => {
    process.stdout.write(b);
    writeSync(log, b);
  };
  child.stdout.on('data', echo);
  child.stderr.on('data', echo);
  console.log(`\n===== ${name}`);
  const timer = setTimeout(() => {
    echo(`\n${name} did not finish in ${LIMIT_MS / 1000} s\n`);
    child.kill('SIGKILL');
  }, LIMIT_MS);
  const code = await new Promise((r) => child.on('exit', (c) => r(c)));
  clearTimeout(timer);
  closeSync(log);

  let left = mine();
  if (left.length) {
    console.log(`${name} left processes behind; killing them:\n${describe(left)}`);
    for (const p of left) {
      try {
        process.kill(p.pid, 'SIGKILL');
      } catch {
        // gone
      }
    }
  }
  results.push({ name, ok: code === 0 && left.length === 0, ms: Date.now() - t0 });
}

console.log('\n===== results');
for (const r of results) console.log(`${r.ok ? 'PASS' : 'FAIL'}  ${r.name}  (${(r.ms / 1000).toFixed(1)} s)`);
const failed = results.filter((r) => !r.ok);
if (failed.length) {
  console.log(`\n${failed.length} of ${results.length} failed; logs in ${D}/logs`);
  for (const r of failed) console.log(`  ${D}/logs/${r.name}.log, ${D}/logs/${r.name}.app.log`);
  process.exit(1);
}
console.log(`\nall ${results.length} checks passed`);
