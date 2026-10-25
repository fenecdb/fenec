// What the demos share: a fenec-server on a port the system picks (found by
// the `listening on:` line on its stderr), the waits, and the clean-up.
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

export const here = dirname(fileURLToPath(import.meta.url));
export const binary = process.env.FENEC_SERVER ?? join(here, '../../target/debug/fenec-server');
export const dir = mkdtempSync(join(tmpdir(), 'fenec-recipes-'));

const children = [];
const onExit = [];
export const atExit = (fn) => onExit.push(fn);

export async function cleanup() {
  for (const c of children) c.kill();
  for (const fn of onExit.reverse()) await fn();
  rmSync(dir, { recursive: true, force: true });
}
for (const sig of ['SIGINT', 'SIGTERM']) process.on(sig, () => cleanup().then(() => process.exit(130)));

/** A server over a new file in the demo's directory; resolves to its URL. */
export function server(name, args) {
  const child = spawn(binary, ['--file', join(dir, `${name}.fenec`), '--http', '127.0.0.1:0', ...args],
    { stdio: ['ignore', 'ignore', 'pipe'] });
  children.push(child);
  return new Promise((resolve, reject) => {
    let err = '';
    child.stderr.on('data', (d) => {
      err += d;
      const m = err.match(/listening on: (http:\/\/\S+)/);
      if (m) resolve(m[1]);
    });
    child.on('exit', (code) => reject(new Error(`fenec-server exited ${code}: ${err}`)));
  });
}

export async function until(what, fn, ms = 10_000) {
  const end = Date.now() + ms;
  for (;;) {
    const v = await fn().catch(() => null);
    if (v) return v;
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 50));
  }
}

/** A statement to `/query`, as `token`; resolves to the response. */
export const query = (url, token, text, params = []) => fetch(`${url}/query`, {
  method: 'POST',
  headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
  body: JSON.stringify({ query: text, params }),
});
