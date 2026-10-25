// Launch-to-studio time, idle memory and quit time of the built app, on this machine.
//
//   node scripts/measure.mjs data.fenec [runs=3]
//   node scripts/mkdata.mjs /tmp/x.fenec 100000      # a file to open
//
// Runs the test variant (FENEC_DESKTOP_APP names another): it is the shipped
// app plus the hooks that tell this script when the studio shows the
// collections. Not run by the checks and not in CI: a number from a shared
// runner says nothing. Run it after one launch of the build (`warmup`: the
// first launch unpacks the app), on a machine with nothing else busy, and
// give the file and the machine with the figures.
//
// Memory is `ps` RSS of every process of the app -- the launcher, the Bun
// main process, the fenec-server it started -- and of the WebKit processes
// that appeared with it (WebContent, Networking, GPU), which are separate on
// macOS. RSS counts shared pages in each process that maps them, so the sum
// is an upper bound.
import { mine, now, procs, quit, sleep, startApp, waitFor } from './checks/lib.mjs';

const [file, runsArg = '3'] = process.argv.slice(2);
if (!file) throw new Error('usage: measure.mjs <file.fenec> [runs]');
const webkit = () => new Set(procs().filter((p) => /com\.apple\.WebKit\.(WebContent|Networking|GPU)/.test(p.command)).map((p) => p.pid));
const MB = (kb) => Math.round(kb / 1024);

async function once() {
  const before = webkit();
  const t0 = now();
  const app = startApp('measure', { FENEC_DESKTOP_OPEN: file });
  const started = await waitFor((l) => l.what === 'server-started', 120000, 'server-started');
  const visible = await waitFor((l) => l.detail?.type === 'state' && l.detail.loaded && /orders \d/.test(l.detail.side), 120000, 'the studio showing orders');
  await sleep(6000); // idle
  const all = procs();
  const mineNow = new Set(mine().map((p) => p.pid));
  for (const pid of webkit()) if (!before.has(pid)) mineNow.add(pid);
  const used = all.filter((p) => mineNow.has(p.pid));
  const sum = (f) => MB(used.filter(f).reduce((a, p) => a + p.rss, 0));
  const row = {
    serverMs: started.at - t0,
    studioMs: visible.at - t0,
    rss: {
      total: sum(() => true),
      server: sum((p) => /fenec-server/.test(p.command)),
      bunAndLauncher: sum((p) => /Contents\/MacOS\/(bun|launcher)/.test(p.command)),
      webkit: sum((p) => /com\.apple\.WebKit/.test(p.command)),
    },
  };
  const q0 = now();
  await quit(app);
  row.quitMs = now() - q0;
  return row;
}

const rows = [];
for (let i = 0; i < Number(runsArg); i++) {
  const r = await once();
  rows.push(r);
  console.log(`run ${i + 1}: server listening ${r.serverMs} ms, studio visible ${r.studioMs} ms, idle rss ${r.rss.total} MB (server ${r.rss.server}, bun+launcher ${r.rss.bunAndLauncher}, webkit ${r.rss.webkit}), quit ${r.quitMs} ms`);
}
const med = (k) => rows.map(k).sort((a, b) => a - b)[Math.floor(rows.length / 2)];
console.log(`median of ${rows.length}: studio visible ${med((r) => r.studioMs)} ms, idle rss ${med((r) => r.rss.total)} MB, quit ${med((r) => r.quitMs)} ms`);
