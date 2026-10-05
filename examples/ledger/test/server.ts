// A fenec-server of a test's own: the crash test kills it, the benchmark
// starts one per sync policy. The binary is FENEC_SERVER, or this
// repository's release build, or its debug build.
import { spawn, type ChildProcess } from 'node:child_process';
import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { ADMIN_TOKEN, OPERATOR_TOKEN } from '../src/config.ts';
import { JWT_SECRET } from '../src/tokens.ts';

const root = fileURLToPath(new URL('../../..', import.meta.url));
const POLICY = fileURLToPath(new URL('../policy.txt', import.meta.url));

export function serverBinary(): string {
  const found = [process.env.FENEC_SERVER, join(root, 'target/release/fenec-server'), join(root, 'target/debug/fenec-server')].find(
    (b) => b && existsSync(b),
  );
  if (!found) throw new Error('no fenec-server: cargo build --release -p fenec-server, or set FENEC_SERVER');
  return found;
}

export async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const s = createServer();
    s.listen(0, '127.0.0.1', () => {
      const port = (s.address() as { port: number }).port;
      s.close(() => resolve(port));
    });
    s.on('error', reject);
  });
}

export interface Node {
  url: string;
  child: ChildProcess;
  log: () => string;
  /** SIGKILL, and wait for the process to be gone. */
  kill9(): Promise<void>;
  /** SIGTERM: the server syncs and checkpoints before it exits. */
  stop(): Promise<void>;
}

export async function startNode(opts: { dir: string; port?: number; sync: string; extra?: string[] }): Promise<Node> {
  mkdirSync(opts.dir, { recursive: true });
  const secret = join(opts.dir, '..', `${opts.dir.split('/').pop()}.jwt`);
  writeFileSync(secret, JWT_SECRET, { mode: 0o600 });
  const port = opts.port ?? (await freePort());
  const args = [
    '--dir',
    opts.dir,
    '--http',
    `127.0.0.1:${port}`,
    '--admin-token',
    ADMIN_TOKEN,
    '--replication-token',
    'ledger-test-replication',
    '--jwt-secret-file',
    secret,
    '--policy',
    POLICY,
    '--sync',
    opts.sync,
    ...(opts.extra ?? []),
  ];
  const child = spawn(serverBinary(), args, {
    env: { ...process.env, FENEC_HTTP_TOKEN: OPERATOR_TOKEN },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let out = '';
  child.stdout!.on('data', (d) => (out += d));
  child.stderr!.on('data', (d) => (out += d));
  const exited = new Promise<void>((r) => child.on('exit', () => r()));
  await new Promise<void>((resolve, reject) => {
    const t0 = Date.now();
    const look = () => {
      if (out.includes('listening on')) return resolve();
      if (child.exitCode !== null) return reject(new Error(`fenec-server exited: ${out}`));
      if (Date.now() - t0 > 30_000) return reject(new Error(`fenec-server did not start: ${out}`));
      setTimeout(look, 20);
    };
    look();
  });
  return {
    url: `http://127.0.0.1:${port}`,
    child,
    log: () => out,
    kill9: async () => {
      child.kill('SIGKILL');
      await exited;
    },
    stop: async () => {
      child.kill('SIGTERM');
      await exited;
    },
  };
}
