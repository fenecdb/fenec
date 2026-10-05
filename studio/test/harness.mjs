// A fenec-server with the studio on, a database seeded for it, and a
// headless Chrome: what the end-to-end tests and the screenshots share.
//
// The server is the workspace's (`FENEC_SERVER`, else target/debug, else
// target/release); Chrome is `CHROME_PATH`, else the newest one puppeteer
// keeps in its cache. Each run takes a directory of its own under the
// system's temporary one and removes it.

import { spawn } from 'node:child_process';
import { createHmac } from 'node:crypto';
import { existsSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { homedir, tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..', '..');

export const TOKEN = 'studio-test-server-token';
export const ADMIN = 'studio-test-admin-token';
const SECRET = 'studio-test-jwt-secret-of-at-least-32-bytes';

export const POLICY = `# collection  access      rows
orders  read,write  where owner = $jwt.sub
notes   read
docs    read
odd     read,update(order)
`;

function binary(name, env) {
  const candidates = [process.env[env], join(ROOT, `target/debug/${name}`), join(ROOT, `target/release/${name}`)];
  const found = candidates.find((p) => p && existsSync(p));
  if (!found) throw new Error(`no ${name}: cargo build -p ${name}, or set ${env}`);
  return found;
}
const serverBinary = () => binary('fenec-server', 'FENEC_SERVER');

export const ROUTER = 'studio-test-router-token';

/**
 * fenec-shard with the studio on, in front of the tenant node at `node`
 * (started with `tenants: true`), and `tenants` placed on it.
 */
export async function startRouter(node, tenants) {
  const dir = mkdtempSync(join(tmpdir(), 'fenec-studio-router-'));
  const port = await freePort();
  const args = ['--listen', `127.0.0.1:${port}`, '--directory', join(dir, 'shard.fenec'), '--token', ROUTER, '--auth-delay', '0', '--studio'];
  const child = spawn(binary('fenec-shard', 'FENEC_SHARD'), args, { stdio: ['ignore', 'ignore', 'pipe'] });
  let log = '';
  child.stderr.on('data', (d) => (log += d));
  const url = `http://127.0.0.1:${port}`;
  const admin = (method, path, body) =>
    fetch(`${url}${path}`, { method, headers: { authorization: `Bearer ${ROUTER}` }, body: body && JSON.stringify(body) });
  for (let i = 0; ; i++) {
    try {
      if ((await admin('GET', '/_shard/nodes')).ok) break;
    } catch {
      /* not yet */
    }
    if (child.exitCode !== null || i > 400) throw new Error(`fenec-shard did not start: ${log}`);
    await new Promise((r) => setTimeout(r, 25));
  }
  const r = await admin('PUT', '/_shard/nodes/n1', { addr: new URL(node.url).host, token: ADMIN });
  if (!r.ok) throw new Error(`the node was not added: ${await r.text()}`);
  for (const t of tenants) {
    const r = await admin('PUT', `/_shard/tenants/${t}`);
    if (!r.ok) throw new Error(`${t} was not placed: ${await r.text()}`);
  }
  return {
    url,
    async stop() {
      child.kill('SIGTERM');
      await new Promise((r) => (child.exitCode !== null ? r() : child.once('exit', r)));
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

/**
 * Chrome: `CHROME_PATH` (CI's own), else the newest headless shell
 * puppeteer keeps in its cache, else its newest Chrome for Testing. The
 * shell first: on macOS Chrome 148's new headless mode never answered a
 * mouse event puppeteer sent it, and every click hung.
 */
export function chromePath() {
  if (process.env.CHROME_PATH) return process.env.CHROME_PATH;
  const kinds = [
    ['chrome-headless-shell', ['chrome-headless-shell-mac-arm64/chrome-headless-shell', 'chrome-headless-shell-mac-x64/chrome-headless-shell', 'chrome-headless-shell-linux64/chrome-headless-shell']],
    ['chrome', ['chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing', 'chrome-linux64/chrome']],
  ];
  for (const [kind, paths] of kinds) {
    const cache = join(homedir(), '.cache/puppeteer', kind);
    if (!existsSync(cache)) continue;
    for (const b of readdirSync(cache).sort().reverse()) {
      for (const p of paths) if (existsSync(join(cache, b, p))) return join(cache, b, p);
    }
  }
  return null;
}

/** A headless browser, puppeteer's way. */
export async function browser() {
  const { default: puppeteer } = await import('puppeteer-core');
  const executablePath = chromePath();
  if (!executablePath) throw new Error('no Chrome: set CHROME_PATH');
  return puppeteer.launch({
    executablePath,
    headless: executablePath.includes('headless-shell') ? 'shell' : true,
    args: ['--no-sandbox'],
  });
}

async function freePort() {
  return new Promise((resolve) => {
    const s = createServer();
    s.listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });
}

const b64 = (s) => Buffer.from(s).toString('base64url');

/** An HS256 token for `claims`, an hour long unless they say. */
export function jwt(claims) {
  const now = Math.floor(Date.now() / 1000);
  const body = { iat: now, exp: now + 3600, ...claims };
  const head = b64(JSON.stringify({ alg: 'HS256', typ: 'JWT' }));
  const payload = b64(JSON.stringify(body));
  const sig = createHmac('sha256', SECRET).update(`${head}.${payload}`).digest('base64url');
  return `${head}.${payload}.${sig}`;
}

/** The server started with the studio on, and stopped by `stop()`. */
export async function startServer({ studio = true, tenants = false, extra = [] } = {}) {
  const dir = mkdtempSync(join(tmpdir(), 'fenec-studio-'));
  writeFileSync(join(dir, 'policy.txt'), POLICY);
  writeFileSync(join(dir, 'jwt.secret'), SECRET);
  const port = await freePort();
  const args = [
    ...(tenants ? ['--dir', join(dir, 'tenants'), '--admin-token', ADMIN] : ['--file', join(dir, 'data.fenec')]),
    '--http', `127.0.0.1:${port}`,
    '--http-token', TOKEN,
    '--jwt-secret-file', join(dir, 'jwt.secret'),
    '--policy', join(dir, 'policy.txt'),
    '--auth-delay', '0',
    '--sync', 'off',
    '--no-checkpoint',
    ...(studio ? ['--studio'] : []),
    ...extra,
  ];
  const child = spawn(serverBinary(), args, { stdio: ['ignore', 'ignore', 'pipe'] });
  let log = '';
  child.stderr.on('data', (d) => (log += d));
  const url = `http://127.0.0.1:${port}`;
  // Waits for the listener: the health check answers once it is up.
  for (let i = 0; ; i++) {
    try {
      const r = await fetch(`${url}/_health`);
      if (r.ok) break;
    } catch {
      /* not yet */
    }
    if (child.exitCode !== null) throw new Error(`fenec-server exited: ${log}`);
    if (i > 400) throw new Error(`fenec-server did not start: ${log}`);
    await new Promise((r) => setTimeout(r, 25));
  }
  return {
    url,
    dir,
    log: () => log,
    async stop() {
      child.kill('SIGTERM');
      await new Promise((r) => (child.exitCode !== null ? r() : child.once('exit', r)));
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

/** One statement over HTTP, with the server's token unless given another. */
export async function query(url, text, params = [], token = TOKEN) {
  const r = await fetch(`${url}/query`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: `Bearer ${token}` },
    body: JSON.stringify({ query: text, params }),
  });
  const body = await r.json();
  if (!r.ok) throw Object.assign(new Error(body.error), { status: r.status });
  return body;
}

export const OWNERS = ['alice', 'bob', 'carol', 'dave'];
export const STATUSES = ['paid', 'open', 'refunded', 'shipped'];

/**
 * `orders`: `n` rows, an owner each in turn, so a quarter are alice's;
 * `docs`, with a graph and a text index; `odd`, whose fields are named as
 * FenecQL's keywords and outside ASCII.
 */
export async function seed(url, n = 100_000) {
  await query(
    url,
    'create collection orders (customer text @hash, status text @hash, total float, placed timestamp @sorted, owner text @hash, note text, meta json, emb vector<8>)',
  );
  const day = Date.UTC(2026, 0, 1);
  const chunk = 5000;
  for (let from = 0; from < n; from += chunk) {
    const rows = [];
    for (let i = from; i < Math.min(n, from + chunk); i++) {
      rows.push({
        customer: `customer-${String(i % 997).padStart(3, '0')}`,
        status: STATUSES[(i * 7) % 4],
        total: Math.round(((i * 37) % 100000) / 10) / 10,
        placed: new Date(day + i * 61_000).toISOString(),
        owner: OWNERS[i % 4],
        note: i % 5 === 0 ? null : `order ${i}`,
        meta: { channel: i % 2 ? 'web' : 'store', items: [i % 3, i % 5] },
        emb: Array.from({ length: 8 }, (_, k) => Math.round(Math.sin(i + k) * 1000) / 1000),
      });
    }
    const r = await fetch(`${url}/orders`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', authorization: `Bearer ${TOKEN}` },
      body: JSON.stringify(rows),
    });
    if (!r.ok) throw new Error(`seeding orders: ${await r.text()}`);
  }
  await query(url, 'create collection docs (title text @text, lang text @hash, code text @unique, tags [text], emb vector<16> @hnsw)');
  for (let i = 0; i < 300; i++) {
    await query(url, 'insert docs {title: $1, lang: $2, code: $3, tags: $4, emb: $5}', [
      `document ${i} about ${['deserts', 'foxes', 'vectors'][i % 3]}`,
      ['en', 'tr', 'de'][i % 3],
      `D-${i}`,
      ['a', 'b'].slice(0, (i % 2) + 1),
      Array.from({ length: 16 }, (_, k) => Math.cos(i * 3 + k)),
    ]);
  }
  await query(url, 'create collection odd (limit int @sorted, order text @hash, ölçü float)');
  await query(url, 'insert odd {limit: $1, order: $2, ölçü: $3}', [1, '<img src=x onerror=alert(1)>', 2.5]);
  await query(url, 'insert odd {limit: $1, order: $2, ölçü: $3}', [2, '"); del odd where (true', -0.5]);
}
