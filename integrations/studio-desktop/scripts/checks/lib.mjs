// What every check shares: starting the test variant of the app, driving it
// through its control file and reading what it reports.
//
// The test variant (FENEC_DESKTOP_TEST_BUILD=1, `make studio-desktop-test`)
// holds src/bun/testhooks.ts, which a shipped app does not. It reads
//   FENEC_DESKTOP_REPORT   a file it appends one JSON fact a line to
//   FENEC_DESKTOP_CONTROL  a file of commands, one JSON object a line
//   FENEC_DESKTOP_OPEN / _LAUNCHER   what to show at start
// and has its own identifier (dev.fenecdb.studio.test), so userData and the
// Keychain service are its own too.
//
// Environment:
//   FENEC_DESKTOP_APP   the test variant's .app (default build-test/stable-macos-arm64/fenec studio test.app)
//   FENEC_SERVER_BIN    a fenec-server to start by hand (default bin/fenec-server)
//   SD_DIR              where data, logs and reports go (default a fresh directory under the system's temp one)
import { execSync, spawn, spawnSync } from 'node:child_process';
import { appendFileSync, closeSync, existsSync, mkdirSync, openSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { join } from 'node:path';

export const HERE = new URL('../..', import.meta.url).pathname;
export const APP = process.env.FENEC_DESKTOP_APP ?? `${HERE}build-test/stable-macos-arm64/fenec studio test.app`;
export const EXE = `${APP}/Contents/MacOS/launcher`;
export const SERVER = process.env.FENEC_SERVER_BIN ?? `${HERE}bin/fenec-server`;
export const IDENTIFIER = 'dev.fenecdb.studio.test';

export const D = process.env.SD_DIR ?? join(tmpdir(), 'fenec-desktop-checks');
mkdirSync(`${D}/data`, { recursive: true });
mkdirSync(`${D}/logs`, { recursive: true });
/** D as the app sees it (realpath: /tmp is /private/tmp on macOS), for the keys of its windows. */
export const R = realpathSync(D);
export const REPORT = `${D}/report.jsonl`;
export const CONTROL = `${D}/control.jsonl`;
/** Where the test variant keeps its state.json. */
export const USER_DATA = join(homedir(), 'Library', 'Application Support', IDENTIFIER, 'stable');

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export const now = () => Date.now();
export const log = (...a) => console.log(...a);
export const step = (title) => console.log(`\n-- ${title}`);

export function sh(cmd) {
  try {
    return execSync(cmd, { stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: 1 << 26 }).toString().trim();
  } catch {
    return '';
  }
}

/** Every process on the machine: {pid, ppid, rss (KB), command}. */
export function procs() {
  // A bound: a `ps` that hangs would otherwise hang the check, and a hosted runner's own cleanup with it.
  return execSync('ps -axww -o pid=,ppid=,rss=,command=', { maxBuffer: 1 << 26, timeout: 30000 })
    .toString()
    .split('\n')
    .map((l) => /^\s*(\d+)\s+(\d+)\s+(\d+)\s+(.*)$/.exec(l))
    .filter(Boolean)
    .map((m) => ({ pid: +m[1], ppid: +m[2], rss: +m[3], command: m[4] }));
}
/** The test variant's processes (the launcher, Bun, the servers it bundles) and servers started by hand on this run's files. */
export const mine = () => procs().filter((p) => p.command.startsWith(APP) || p.command.includes(`fenec-server --file ${R}/`) || p.command.includes(`fenec-server --file ${D}/`));
export const describe = (list) => list.map((p) => `${p.pid} (ppid ${p.ppid}) ${p.command.replace(APP, "<APP>").slice(0, 200)}`).join('\n');

export function lines() {
  if (!existsSync(REPORT)) return [];
  return readFileSync(REPORT, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l));
}
export function cmd(o) {
  appendFileSync(CONTROL, JSON.stringify(o) + '\n');
}

let current = '';
/** Names the log a failure prints the tail of, for a check that starts the app some other way than startApp. */
export const logName = (name) => void (current = name);
/** What a failure prints: the tail of the app's own log and of what it reported. */
function context() {
  const tail = (file, n) => (existsSync(file) ? readFileSync(file, 'utf8').split('\n').slice(-n).join('\n') : '(none)');
  return `\n--- app log (${current}) ---\n${tail(`${D}/logs/${current}.app.log`, 25)}\n--- last reports ---\n${lines().slice(-12).map((l) => JSON.stringify(l).slice(0, 220)).join('\n')}`;
}

/** The first report line `pred` accepts, waiting for it as long as `ms`: an event is waited for, never slept to. */
export async function waitFor(pred, ms = 30000, label = 'a report') {
  const t0 = now();
  for (;;) {
    const hit = lines().find(pred);
    if (hit) return hit;
    if (now() - t0 > ms) throw new Error(`timeout waiting for ${label}${context()}`);
    await sleep(40);
  }
}
/** Polls `fn` (which may be async) until it returns something truthy, which is returned. */
export async function until(fn, ms = 10000, label = 'a condition') {
  const t0 = now();
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (now() - t0 > ms) throw new Error(`timeout waiting for ${label}${context()}`);
    await sleep(80);
  }
}
/** Asserts nothing arrives within `ms`: the one place a check has to wait for a thing that must not happen. */
export async function waitNone(pred, ms, label) {
  const t0 = now();
  while (now() - t0 < ms) {
    const hit = lines().find(pred);
    if (hit) throw new Error(`${label}: ${JSON.stringify(hit)}${context()}`);
    await sleep(40);
  }
}

let evals = 0;
/**
 * Runs `js` (statements; `return` a value) in the window `key` -- a studio
 * window's key, `launcher`, `newdb` or `notice:<n>` -- through
 * executeJavascript, not the page's own eval(): the studio's CSP forbids that.
 * The result comes back through the event-only bridge.
 */
export async function evalIn(key, js, ms = 15000) {
  // A script reaches the page as one line (executeJavascript), where a `//`
  // comment would swallow what follows it: the eval would never answer.
  if (/(^|\n)\s*\/\//.test(js)) throw new Error('an eval script holds a // comment: use /* */ or none');
  const id = `e${++evals}`;
  const wrapped = `(async()=>{try{const v=await (async()=>{${js}})();window.__electrobunSendToHost({type:"eval",id:${JSON.stringify(id)},ok:true,value:v})}catch(e){window.__electrobunSendToHost({type:"eval",id:${JSON.stringify(id)},ok:false,value:String(e)})}})()`;
  cmd({ cmd: 'eval', key, js: wrapped });
  const hit = await waitFor((l) => l.detail?.type === 'eval' && l.detail.id === id, ms, `eval ${id} in ${key}`);
  return hit.detail;
}

/** A page's visible text, whitespace squeezed. */
export const textOf = async (key) => (await evalIn(key, `return document.body.innerText.replace(/\\s+/g,' ')`)).value;

/** The studio in window `key` showing a database, signed in, in a report after the `after`th. */
export async function waitStudio(key, after = 0, ms = 30000) {
  return waitFor((l, i) => i >= after && l.detail?.type === 'state' && l.detail.loaded && l.key === key, ms, `the studio loaded in ${key}`);
}
/** The last state a studio window reported. */
export const lastState = (key) => lines().filter((l) => l.detail?.type === 'state' && l.key === key).pop()?.detail;

/** The notice window the app showed next (after `after` reports), as its report, once its page has drawn it. */
export async function waitNotice(title, ms = 15000, after = 0) {
  const shown = await waitFor((l, i) => i >= after && l.what === 'notice' && (title instanceof RegExp ? title.test(l.title) : l.title === title), ms, `a notice "${title}"`);
  await waitFor((l) => l.what === 'notice-ready' && l.key === shown.key, ms, `${shown.key} drawn`);
  return shown;
}
/** Presses a notice's button, as a click would. */
export const pressNotice = (notice, id) => evalIn(notice.key, `document.getElementById("notice-${id}").click()`);

/**
 * The test variant, started with `env` added to the hooks' own. Its output
 * goes to logs/<name>.app.log, which a failing run keeps.
 */
export function startApp(name, env = {}) {
  current = name;
  rmSync(REPORT, { force: true });
  writeFileSync(CONTROL, '');
  if (!existsSync(EXE)) throw new Error(`no app at ${APP}: build it with \`make studio-desktop-test\``);
  const fd = openSync(`${D}/logs/${name}.app.log`, 'w');
  const child = spawn(EXE, [], {
    env: { ...process.env, FENEC_DESKTOP_REPORT: REPORT, FENEC_DESKTOP_CONTROL: CONTROL, ...env },
    stdio: ['ignore', fd, fd],
  });
  closeSync(fd);
  const exited = new Promise((r) => child.on('exit', (code, signal) => r({ code, signal })));
  return { child, exited, name };
}

/** Asks the app to quit and waits until none of its processes is left. Throws if any is. */
export async function quit(app, ms = 40000) {
  cmd({ cmd: 'quit' });
  const t0 = now();
  while (mine().length && now() - t0 < ms) await sleep(100);
  const left = mine();
  if (left.length) {
    for (const p of left) {
      try {
        process.kill(p.pid, 'SIGKILL');
      } catch {
        // gone already
      }
    }
    throw new Error(`the app left processes behind after quit:\n${describe(left)}`);
  }
  await Promise.race([app.exited, sleep(5000)]);
}

/** A fenec-server started by hand, as another program holding a file or a remote server would be. */
export async function startServer(args, env = {}, bin = SERVER) {
  const child = spawn(bin, args, { env: { ...process.env, ...env }, stdio: ['ignore', 'ignore', 'pipe'] });
  let err = '';
  child.stderr.on('data', (b) => {
    err += b;
  });
  const exited = new Promise((r) => child.on('exit', (code) => r(code)));
  let poll;
  const origin = await Promise.race([
    new Promise((resolve) => {
      poll = setInterval(() => {
        const m = /listening on: (http:\/\/\S+)/.exec(err);
        if (m) resolve(m[1]);
      }, 10);
    }),
    exited.then((code) => {
      throw new Error(`the server exited ${code}: ${err}`);
    }),
  ]).finally(() => clearInterval(poll));
  return {
    child,
    origin,
    exited,
    err: () => err,
    async stop() {
      if (child.exitCode === null) child.kill('SIGTERM');
      await exited;
    },
  };
}

/** The data file `name` the checks open. */
export const file = (name) => `${R}/data/${name}.fenec`;

/** A fresh data file `name` of `rows` orders (the collections the studio's own tests use), replacing any. */
export function data(name, rows = 50) {
  const path = file(name);
  const r = spawnSync(process.execPath, [`${HERE}scripts/mkdata.mjs`, path, String(rows)], {
    env: { ...process.env, FENEC_SERVER_BIN: SERVER },
    stdio: ['ignore', 'ignore', 'pipe'],
    encoding: 'utf8',
  });
  if (r.status !== 0) throw new Error(`could not make ${path}: ${r.stderr}`);
  return path;
}

/** Every process of the app and of the servers on this run's files must be gone before a check starts. */
export function assertClean(when) {
  const left = mine();
  if (left.length) throw new Error(`${when}: processes left behind:\n${describe(left)}`);
}
