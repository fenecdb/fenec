// Chrome's own files, read once before the first launch of a job
// (scripts/lighthouse.sh runs this first).
//
// Started cold, Chrome faults its binary in a page and a readahead at a
// time, and a fresh GitHub runner's disk answers each slowly: the first
// launch of a job has sat in `folio_wait_bit_common` for 5 to 20 s, 1 300
// major faults for 136 MB (fenec studio's harness, which reads Chrome the
// same way: studio/test/harness.mjs, `warm`). chrome-launcher waits for
// Chrome to say where it listens for 50 polls of 500 ms, 25 s, and then
// rejects with "waiting for dynamic debugging port in chrome-err.log" while
// Chrome is still starting. That is how the shop's CI job failed once,
// after every test had passed: 25.4 s after the last one, in Lighthouse's
// own chrome-launcher (so in scripts/cpu-slowdown.mjs, the first Chrome
// launch of the job), with Chrome still running when the job ended. The
// log holds no more than that, so the cold disk is the explanation that
// fits and not one it proves. Every launch after the first, Lighthouse
// CI's twelve among them, is of a Chrome whose files are in the page
// cache. Read here a mebibyte at a time, the disk's time is no longer the
// launch's, and the 25 s bound what they are for: Chrome failing to start.
//
//   CHROME_PATH  Chrome or Chromium, when it is not where Lighthouse looks
//
// The directory read is the one holding the executable, so a wrapper
// script's (Debian's /usr/bin/chromium) is passed over: it holds no `.pak`,
// and reading /usr/bin would warm nothing of Chrome's. Name the browser
// itself in CHROME_PATH then. An `.app` is read whole, its resources being
// in its framework, not beside its executable.
import { Buffer } from 'node:buffer';
import { readdirSync, realpathSync } from 'node:fs';
import { open, readdir } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { performance } from 'node:perf_hooks';
import { pathToFileURL } from 'node:url';

const require = createRequire(import.meta.url);
// Lighthouse's own chrome-launcher, which finds Chrome as the launches do
// (CHROME_PATH first).
const lighthouse = createRequire(require.resolve('lighthouse/package.json'));
const { Launcher } = await import(pathToFileURL(lighthouse.resolve('chrome-launcher')).href);

const executable = Launcher.getFirstInstallation();
// No Chrome is the launch's to say, in its own words.
if (!executable) process.exit(0);

const real = realpathSync(executable);
const app = /^(.*?\.app)\//.exec(real);
const root = app ? app[1] : dirname(real);
if (!app && !readdirSync(root).some((n) => n.endsWith('.pak'))) {
  console.error(`chrome: ${root} is not Chrome's own directory, so nothing of it is read ahead (CHROME_PATH=${real})`);
  process.exit(0);
}

const buf = Buffer.allocUnsafe(1 << 20);
let bytes = 0;
const walk = async (dir) => {
  for (const e of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, e.name);
    if (e.isDirectory()) await walk(path);
    if (!e.isFile()) continue;
    let f;
    try {
      f = await open(path);
    } catch {
      continue; // not ours to read, and so not Chrome's either
    }
    try {
      for (let n; (n = (await f.read(buf, 0, buf.length, null)).bytesRead) > 0; ) bytes += n;
    } catch {
      /* a file gone or unreadable halfway: the rest is still worth reading */
    } finally {
      await f.close();
    }
  }
};
const t0 = performance.now();
await walk(root);
console.error(`chrome: read ${Math.round(bytes / 1e6)} MB of ${root} in ${((performance.now() - t0) / 1000).toFixed(1)} s`);
