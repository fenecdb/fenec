// A node of the measurement's own (scripts/db.sh over a new directory) and,
// when asked, a Kestrel server in front of it, so a measurement neither
// reads nor writes the demo's data. Set FENEC_URL before anything reads
// src/config.ts: import this first, then the rest dynamically.
import { spawn, type ChildProcess } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const here = new URL('..', import.meta.url).pathname;
export const benchPort = Number(process.env.BENCH_PORT ?? 18690);
export const appPort = benchPort + 1;
export const dir = process.env.BENCH_DIR ?? mkdtempSync(join(tmpdir(), 'kestrel-bench-'));
process.env.FENEC_URL = `http://127.0.0.1:${benchPort}`;
process.env.KESTREL_URL = `http://127.0.0.1:${appPort}`;

const children: ChildProcess[] = [];

async function until(url: string, what: string) {
  for (let i = 0; i < 600; i++) {
    try {
      const r = await fetch(url);
      await r.body?.cancel();
      return;
    } catch {
      await new Promise((r) => setTimeout(r, 100));
    }
  }
  throw new Error(`${what} did not start`);
}

/** fenec-server over `dir`, as scripts/db.sh starts it. */
export async function startNode(): Promise<ChildProcess> {
  const p = spawn('sh', [join(here, 'scripts/db.sh')], {
    env: { ...process.env, FENEC_PORT: String(benchPort), FENEC_DIR: join(dir, 'tenants'), FENEC_AUDIT: join(dir, 'audit.log') },
    stdio: ['ignore', 'ignore', 'inherit'],
  });
  children.push(p);
  await until(`http://127.0.0.1:${benchPort}/_health`, 'fenec-server');
  return p;
}

/** Kestrel's server in a process of its own: the ingest endpoint, no workers, no feed, no demo traffic. */
export async function startApp(): Promise<ChildProcess> {
  const p = spawn('npx', ['tsx', join(here, 'src/server.ts')], {
    cwd: here,
    env: { ...process.env, PORT: String(appPort), KESTREL_ROLLUPS: '0', KESTREL_FEED: '0', KESTREL_DEMO_TRAFFIC: '0' },
    stdio: ['ignore', 'ignore', 'inherit'],
  });
  children.push(p);
  await until(`http://127.0.0.1:${appPort}/robots.txt`, 'Kestrel');
  return p;
}

export function stopAll(keep = !!process.env.BENCH_DIR) {
  for (const c of children) c.kill('SIGTERM');
  if (!keep) setTimeout(() => rmSync(dir, { recursive: true, force: true }), 1500);
}

process.on('exit', () => {
  for (const c of children) c.kill('SIGTERM');
});

export const pct = (xs: number[], p: number) => {
  if (!xs.length) return NaN;
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))];
};

/** Resident memory of the node's process, MB (ps). */
export async function nodeRss(): Promise<number> {
  const { execSync } = await import('node:child_process');
  const out = execSync(`ps -o rss= -p $(pgrep -f "127.0.0.1:${benchPort}" | head -1)`).toString().trim();
  return Number(out) / 1024;
}

export function log(...a: unknown[]) {
  console.log(`[${new Date().toISOString().slice(11, 19)}]`, ...a);
}
