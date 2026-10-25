// Drives the dev app (`hutch electrobun dev`) through its test control file and
// reads its report file. The app reads these only when FENEC_DESKTOP_CONTROL /
// FENEC_DESKTOP_REPORT are set, which startApp() does and a shipped app never does.
//
//   bun scripts/checks/setup.mjs          # the data files the checks open
//   bun scripts/checks/files.mjs          # open, edit, close, reopen, a second open, quit
//
// SD_DIR is where the data and the report live (default /tmp/fenec-desktop-checks),
// FENEC_SERVER_BIN the server (default bin/fenec-server), HUTCH the hutch launcher.
import { appendFileSync, existsSync, mkdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { spawn, execSync } from 'node:child_process';

export const D = process.env.SD_DIR ?? '/tmp/fenec-desktop-checks';
mkdirSync(`${D}/data`, { recursive: true });
/** D as the app sees it (realpath: /tmp is /private/tmp on macOS), for the keys of its windows. */
export const R = realpathSync(D);
export const HERE = new URL('../..', import.meta.url).pathname;
export const SERVER = process.env.FENEC_SERVER_BIN ?? `${HERE}bin/fenec-server`;
export const REPORT = `${D}/report.jsonl`;
export const CONTROL = `${D}/control.jsonl`;
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export const now = () => Date.now();

export function reset() {
  rmSync(REPORT, { force: true });
  writeFileSync(CONTROL, '');
}
export function lines() {
  if (!existsSync(REPORT)) return [];
  return readFileSync(REPORT, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l));
}
export function cmd(o) {
  appendFileSync(CONTROL, JSON.stringify(o) + '\n');
}
export async function waitFor(pred, ms = 30000, label = 'condition') {
  const t0 = now();
  for (;;) {
    const hit = lines().find(pred);
    if (hit) return hit;
    if (now() - t0 > ms) throw new Error(`timeout waiting for ${label}`);
    await sleep(50);
  }
}
/** Runs `js` (statements, `return` a value) in the studio window `key`, through executeJavascript, not the page's eval: the studio's CSP forbids that. */
export const evalIn = async (key, js, ms = 15000) => {
  const n = lines().filter((l) => l.detail?.type === 'eval').length;
  const wrapped = `(async()=>{try{const v=await (async()=>{${js}})();window.__electrobunSendToHost({type:"eval",ok:true,value:v})}catch(e){window.__electrobunSendToHost({type:"eval",ok:false,value:String(e)})}})()`;
  cmd({ cmd: 'eval', key, js: wrapped });
  const t0 = now();
  for (;;) {
    const ev = lines().filter((l) => l.detail?.type === 'eval');
    if (ev.length > n) return ev[n].detail;
    if (now() - t0 > ms) throw new Error('timeout waiting for eval');
    await sleep(50);
  }
};

export function startApp(env = {}) {
  reset();
  const child = spawn(process.env.HUTCH ?? `${process.env.HOME}/.hutch/bin/hutch`, ['electrobun', 'dev'], {
    cwd: HERE,
    env: { ...process.env, FENEC_SERVER_BIN: SERVER, FENEC_DESKTOP_REPORT: REPORT, FENEC_DESKTOP_CONTROL: CONTROL, ...env },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  const out = [];
  child.stdout.on('data', (b) => out.push(String(b)));
  child.stderr.on('data', (b) => out.push(String(b)));
  return { child, out };
}

/** The processes of the app and of the servers it started on D's files: empty when nothing is left. */
export const ps = () =>
  execSync(`ps -axo pid,ppid,rss,command | grep -E "fenec-server --file ${R}|fenec studio-dev|hutch-engine" | grep -v grep | cut -c1-170 || true`).toString();
