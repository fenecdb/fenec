// A fenec-server with the studio on, a database seeded for it, and a
// headless Chrome: what the end-to-end tests and the screenshots share.
//
// The server is the workspace's (`FENEC_SERVER`, else target/debug, else
// target/release); Chrome is `CHROME_PATH`, else the newest one puppeteer
// keeps in its cache. Each run takes a directory of its own under the
// system's temporary one and removes it.

import { spawn } from 'node:child_process';
import { createHmac } from 'node:crypto';
import { existsSync, mkdtempSync, readdirSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { open, readdir } from 'node:fs/promises';
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
 * (started with `tenants: true`), and `tenants` placed on it. `token` is
 * the router's own: the nodes' data token makes one token for both, as the
 * admin view's list of tenants needs. `extra` goes on its command line.
 */
export async function startRouter(node, tenants, { token = ROUTER, extra = [] } = {}) {
  const dir = mkdtempSync(join(tmpdir(), 'fenec-studio-router-'));
  const args = ['--listen', '127.0.0.1:0', '--directory', join(dir, 'shard.fenec'), '--token', token, '--auth-delay', '0', '--studio', ...extra];
  const child = spawn(binary('fenec-shard', 'FENEC_SHARD'), args, { stdio: ['ignore', 'ignore', 'pipe'] });
  const { url, log } = await listening(child, 'fenec-shard');
  const admin = (method, path, body) =>
    fetch(`${url}${path}`, { method, headers: { authorization: `Bearer ${token}` }, body: body && JSON.stringify(body) });
  for (let i = 0; ; i++) {
    try {
      if ((await admin('GET', '/_shard/nodes')).ok) break;
    } catch {
      /* not yet */
    }
    if (child.exitCode !== null || i > 400) throw new Error(`fenec-shard did not start: ${log()}`);
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
 * A standby fenec-shard following a primary router that is not there, so
 * its directory never arrives: every tenant's request is its 503. The
 * primary is port 1, where no test can listen -- a port found free could
 * be another test's server by the time the standby asked it. `extra` goes
 * on its command line.
 */
export async function startStandby({ extra = [] } = {}) {
  const dir = mkdtempSync(join(tmpdir(), 'fenec-studio-standby-'));
  const args = [
    '--listen', '127.0.0.1:0',
    '--directory', join(dir, 'shard.fenec'),
    '--replication-token', 'studio-test-replication',
    '--replica-of', 'http://127.0.0.1:1',
    '--auth-delay', '0',
    ...extra,
  ];
  const child = spawn(binary('fenec-shard', 'FENEC_SHARD'), args, { stdio: ['ignore', 'ignore', 'pipe'] });
  const { url, log } = await listening(child, 'fenec-shard');
  for (let i = 0; ; i++) {
    try {
      if ((await fetch(`${url}/t/any/collections`)).status === 503) break;
    } catch {
      /* not yet */
    }
    if (child.exitCode !== null || i > 400) throw new Error(`fenec-shard did not start: ${log()}`);
    await new Promise((r) => setTimeout(r, 25));
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

/**
 * Chrome's own files read once a process, before it first starts. Started
 * cold, Chrome faults its binary (298 MB in Chrome 155) and resources in a
 * page and its readahead at a time, and a fresh GitHub runner's disk
 * answers each slowly: there its first launch of a job sat in
 * `folio_wait_bit_common` for 5 to 20 s, 1 300 major faults for 136 MB,
 * of the 30 s puppeteer gives it to say where it listens -- and once past
 * them, every test of e2e.test.mjs failed in its hook, the launches after
 * it taking 0.2 s. Read here a mebibyte at a time, all 457 MB of
 * /opt/google/chrome took 2.5 to 13 s and the launch after it 0.2 to 1.6:
 * the disk's time is no longer the launch's, and the 30 s bound only what
 * they are for. In a container of three cores beside busy loops, its reads
 * held to 80 a second and its page cache dropped, the launch timed out 4
 * times in 4 without this and took 12 s with it, the libraries Chrome
 * links from outside its directory still cold. Once cached the read costs
 * 0.1 s. `pipe: true` waits for no endpoint, but its first command waits
 * on the same faults under a 180 s protocol timeout: a longer timeout, not
 * a disk read sooner. A directory that is not Chrome's own -- a wrapper
 * script's, as Debian's /usr/bin/chromium -- holds no `.pak`, and is
 * passed over; an `.app` is read whole, its resources being in its
 * framework, not beside its executable.
 */
async function warm(executable) {
  const real = realpathSync(executable);
  const app = /^(.*?\.app)\//.exec(real);
  const root = app ? app[1] : dirname(real);
  if (!app && !readdirSync(root).some((n) => n.endsWith('.pak'))) return;
  const buf = Buffer.allocUnsafe(1 << 20);
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
        while ((await f.read(buf, 0, buf.length, null)).bytesRead > 0);
      } finally {
        await f.close();
      }
    }
  };
  await walk(root);
}
let warmed;

/** A headless browser, puppeteer's way. */
export async function browser() {
  const { default: puppeteer } = await import('puppeteer-core');
  const executablePath = chromePath();
  if (!executablePath) throw new Error('no Chrome: set CHROME_PATH');
  await (warmed ??= warm(executablePath));
  return puppeteer.launch({
    executablePath,
    headless: executablePath.includes('headless-shell') ? 'shell' : true,
    args: ['--no-sandbox'],
  });
}

/**
 * Where a server started on port 0 listens, off its `listening on:
 * http://<address>` line, read to the space after it so a line that came
 * in two pieces is not read short; and what it has logged since. A port
 * found free and handed over was free only until someone else took it,
 * and a server of someone else's could answer the health checks in its
 * place.
 */
function listening(child, name) {
  let log = '';
  child.stderr.on('data', (d) => (log += d));
  return new Promise((resolve, reject) => {
    const t0 = Date.now();
    const look = () => {
      const at = /listening on: (http:\/\/\S+)\s/.exec(log);
      if (at) return resolve({ url: at[1], log: () => log });
      if (child.exitCode !== null) return reject(new Error(`${name} exited: ${log}`));
      if (Date.now() - t0 > 20_000) return reject(new Error(`${name} did not start: ${log}`));
      setTimeout(look, 25);
    };
    look();
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
  const args = [
    ...(tenants ? ['--dir', join(dir, 'tenants'), '--admin-token', ADMIN] : ['--file', join(dir, 'data.fenec')]),
    '--http', '127.0.0.1:0',
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
  const { url, log } = await listening(child, 'fenec-server');
  // And answering: the health check answers once it is up.
  for (let i = 0; ; i++) {
    try {
      const r = await fetch(`${url}/_health`);
      if (r.ok) break;
    } catch {
      /* not yet */
    }
    if (child.exitCode !== null) throw new Error(`fenec-server exited: ${log()}`);
    if (i > 400) throw new Error(`fenec-server did not start: ${log()}`);
    await new Promise((r) => setTimeout(r, 25));
  }
  return {
    url,
    dir,
    log,
    async stop() {
      child.kill('SIGTERM');
      await new Promise((r) => (child.exitCode !== null ? r() : child.once('exit', r)));
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

const SITE = join(ROOT, 'site', 'dist');
const TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.wasm': 'application/wasm',
  '.svg': 'image/svg+xml',
  '.woff2': 'font/woff2',
  '.webp': 'image/webp',
  '.png': 'image/png',
  '.json': 'application/json',
  '.txt': 'text/plain; charset=utf-8',
};

/**
 * The built site (`python3 site/build.py`, site/dist) served as Cloudflare
 * serves it: `/docs/x` is `docs/x.html`, a directory its `index.html`.
 * `null` when the site is not built. `slow` delays each answer by its
 * milliseconds, as a network would.
 */
export async function startSite({ slow = 0 } = {}) {
  if (!existsSync(join(SITE, 'playground.html'))) return null;
  const { createServer: http } = await import('node:http');
  const { readFile, stat } = await import('node:fs/promises');
  const { extname, normalize } = await import('node:path');
  const server = http(async (req, res) => {
    let path = normalize(decodeURIComponent(new URL(req.url, 'http://x').pathname)).replace(/^(\.\.[/\\])+/, '');
    let file = join(SITE, path);
    try {
      if ((await stat(file)).isDirectory()) file = join(file, 'index.html');
    } catch {
      file += '.html';
    }
    try {
      const body = await readFile(file);
      if (slow) await new Promise((r) => setTimeout(r, slow));
      res.writeHead(200, { 'content-type': TYPES[extname(file)] ?? 'application/octet-stream', 'cache-control': 'no-store' });
      res.end(body);
    } catch {
      res.writeHead(404, { 'content-type': 'text/plain' });
      res.end('not found');
    }
  });
  const port = await new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve(server.address().port)));
  return {
    url: `http://127.0.0.1:${port}`,
    stop: () => new Promise((r) => server.close(r)),
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
