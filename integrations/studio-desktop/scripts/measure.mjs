// Cold start, memory and open time of a built app, on this machine.
//
//   bun scripts/measure.mjs "build/stable-macos-arm64/fenec studio.app" data.fenec [runs=3]
//
// Starts the app's own executable with the test hooks switched on (the
// FENEC_DESKTOP_REPORT / _CONTROL / _OPEN variables; a shipped app sets none):
// the page tells the main process when the studio shows the collections, and
// the main process writes the time into a report file. Memory is `ps` RSS of
// every process of the app: the launcher, the Bun main process, the
// fenec-server it started, and the WebKit processes that appeared with it
// (WebContent, Networking, GPU), which are separate processes on macOS.
// RSS counts shared pages in each process that maps them, so the sum is an
// upper bound.
import { execSync, spawn } from 'node:child_process';
import { appendFileSync, existsSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const [app, file, runsArg = '3'] = process.argv.slice(2);
if (!app || !file) throw new Error('usage: measure.mjs <app bundle> <file.fenec> [runs]');
const exe = join(app, 'Contents/MacOS/launcher');
const report = join(tmpdir(), `fenec-desktop-report-${process.pid}.jsonl`);
const control = join(tmpdir(), `fenec-desktop-control-${process.pid}.jsonl`);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const lines = () => (existsSync(report) ? readFileSync(report, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l)) : []);
const ps = () =>
  execSync('ps -axo pid=,ppid=,rss=,command=', { maxBuffer: 1 << 26 })
    .toString()
    .split('\n')
    .filter(Boolean)
    .map((l) => {
      const m = /^\s*(\d+)\s+(\d+)\s+(\d+)\s+(.*)$/.exec(l);
      return m ? { pid: +m[1], ppid: +m[2], rss: +m[3], cmd: m[4] } : null;
    })
    .filter(Boolean);
const webkit = () => new Set(ps().filter((p) => /com\.apple\.WebKit\.(WebContent|Networking|GPU)/.test(p.cmd)).map((p) => p.pid));

async function once() {
  rmSync(report, { force: true });
  writeFileSync(control, '');
  const before = webkit();
  const t0 = Date.now();
  const child = spawn(exe, [], {
    env: { ...process.env, FENEC_DESKTOP_REPORT: report, FENEC_DESKTOP_CONTROL: control, FENEC_DESKTOP_OPEN: file },
    stdio: 'ignore',
  });
  const waitFor = async (pred, ms, what) => {
    for (const t = Date.now(); Date.now() - t < ms; await sleep(25)) {
      const hit = lines().find(pred);
      if (hit) return hit;
    }
    throw new Error(`timeout: ${what}`);
  };
  const started = await waitFor((l) => l.what === 'server-started', 120000, 'server-started');
  const visible = await waitFor((l) => l.detail?.type === 'state' && l.detail.loaded && /orders \d/.test(l.detail.side), 120000, 'studio visible');
  await sleep(6000); // idle
  const tree = ps();
  const mine = new Set([child.pid]);
  for (let grew = true; grew; ) {
    grew = false;
    for (const p of tree) if (mine.has(p.ppid) && !mine.has(p.pid)) (mine.add(p.pid), (grew = true));
  }
  for (const pid of webkit()) if (!before.has(pid)) mine.add(pid);
  const procs = tree.filter((p) => mine.has(p.pid));
  const kb = (f) => procs.filter(f).reduce((a, p) => a + p.rss, 0);
  const row = {
    startMs: { serverStarted: started.at - t0, studioVisible: visible.at - t0 },
    rssMB: {
      total: Math.round(kb(() => true) / 1024),
      server: Math.round(kb((p) => /fenec-server/.test(p.cmd)) / 1024),
      bunAndLauncher: Math.round(kb((p) => /fenec studio.*(launcher|bun)\b/.test(p.cmd) && !/WebKit/.test(p.cmd)) / 1024),
      webkit: Math.round(kb((p) => /com\.apple\.WebKit/.test(p.cmd)) / 1024),
    },
    processes: procs.map((p) => `${p.pid} ${Math.round(p.rss / 1024)}MB ${p.cmd.replace(/^.*\/(Contents|Resources|bin|MacOS)\//, '').slice(0, 70)}`),
  };
  appendFileSync(control, JSON.stringify({ cmd: 'quit' }) + '\n');
  const t1 = Date.now();
  await Promise.race([new Promise((r) => child.on('exit', r)), sleep(60000)]);
  row.quitMs = Date.now() - t1;
  await sleep(500);
  row.leftover = ps().filter((p) => /fenec-server --file/.test(p.cmd) && p.cmd.includes(file)).length;
  return row;
}

for (let i = 0; i < Number(runsArg); i++) {
  const r = await once();
  console.log(`run ${i + 1}: server ${r.startMs.serverStarted} ms, studio visible ${r.startMs.studioVisible} ms, rss total ${r.rssMB.total} MB (server ${r.rssMB.server}, bun+launcher ${r.rssMB.bunAndLauncher}, webkit ${r.rssMB.webkit}), quit ${r.quitMs} ms, leftover servers ${r.leftover}`);
  if (i === 0) console.log(r.processes.map((p) => '   ' + p).join('\n'));
}
rmSync(report, { force: true });
rmSync(control, { force: true });
