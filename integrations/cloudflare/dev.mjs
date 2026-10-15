// `wrangler dev` for the tests and the benchmarks here: worker.test.js,
// rows.mjs and coldstart.mjs start a Worker through it, the example or the
// counted one beside it (bench/).

import { spawn } from 'node:child_process';
import { resolve } from 'node:path';

const here = new URL('.', import.meta.url).pathname;

/** The escape sequences a terminal's colours are written with. */
const ANSI = /\x1b\[[0-9;]*[A-Za-z]/g;

/**
 * The address `wrangler dev` says it is ready on, in what it has printed so
 * far; null until the line is there whole. Read past its colours: under
 * FORCE_COLOR the line is `Ready on \x1b[32mhttp://127.0.0.1:57886\x1b[39m`,
 * and read with them, it was never found and the harness waited for good.
 */
export function readyUrl(log) {
  return /Ready on (http:\/\/\S+)\s/.exec(log.replace(ANSI, ''))?.[1] ?? null;
}

/**
 * The address of the Worker's own workerd socket in what `wrangler dev`
 * printed under `--log-level debug`; null until it is there. `wrangler
 * dev`'s "Ready on" address is a proxy Worker of its own in front of the
 * Worker, which a measurement may want to leave out (entry/bench.mjs).
 * Neither address tells a Worker of a client gone before its answer's
 * head: workerd notices a client only as it writes to it, there and past
 * the proxy alike, where Cloudflare aborts the request's `signal`.
 */
export function innerUrl(log) {
  const m = /"userWorkerUrl":\{"protocol":"(https?:)","hostname":"([^"]+)","port":"(\d+)"\}/.exec(log);
  return m ? `${m[1]}//${m[2]}:${m[3]}` : null;
}

/** Every `wrangler dev` started and not yet ended: a test that failed ends them all. */
export const running = new Set();

/**
 * `wrangler dev` for the Worker in `dir` (beside this file, or absolute)
 * over the storage in `state`, with `vars` its environment; resolves once
 * it says it is ready to `{url, inner, log(), stop()}`, and rejects with
 * what it printed when it ends, or cannot start, before that. `inner`,
 * the Worker's own socket past wrangler's proxy ({@link innerUrl}), is read
 * where `{ inner: true }` asks for it.
 *
 * On port 0, the port read off the line saying it is ready: one found free
 * and handed over is free only until another suite on the machine takes it.
 * In a process group of its own: wrangler starts workerd beside it, and
 * killing wrangler alone left those holding its output open, and the test
 * process with them. With no colour, the line plain to read.
 */
export function serve(dir, state, vars = {}, { inner = false } = {}) {
  const child = spawn(
    resolve(here, 'node_modules/.bin/wrangler'),
    [
      'dev', '--config', 'wrangler.jsonc', '--port', '0', '--ip', '127.0.0.1', '--persist-to', state, '--show-interactive-dev-session=false',
      ...(inner ? ['--log-level', 'debug'] : []),
      ...Object.entries(vars).flatMap(([k, v]) => ['--var', `${k}:${v}`]),
    ],
    {
      cwd: resolve(here, dir),
      stdio: ['ignore', 'pipe', 'pipe'],
      detached: true,
      env: { ...process.env, WRANGLER_SEND_METRICS: 'false', FORCE_COLOR: '0', NO_COLOR: '1' },
    },
  );
  running.add(child);
  const ended = new Promise((done) => child.once('exit', done));
  ended.then(() => running.delete(child));
  let log = '';
  const url = new Promise((ready, failed) => {
    const read = (d) => {
      log += d;
      const url = readyUrl(log);
      if (url && (!inner || innerUrl(log))) ready(url);
    };
    child.stdout.on('data', read);
    child.stderr.on('data', read);
    child.once('error', (e) => failed(new Error(`wrangler dev did not start: ${e.message}\n${log}`)));
    ended.then(() => failed(new Error(`wrangler dev ended before it was ready:\n${log}`)));
  });
  return url.then((url) => ({
    url,
    inner: inner ? innerUrl(log) : null,
    log: () => log,
    async stop() {
      if (child.exitCode === null && child.signalCode === null) process.kill(-child.pid, 'SIGINT');
      await ended;
    },
  }));
}
