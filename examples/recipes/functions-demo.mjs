// docs/functions, run: three ways to react to a write with no code in the
// database -- a Worker that is an endpoint (needs wrangler: WRANGLER=path/to/
// wrangler, or one on PATH), a job queue with a worker, and a webhook relay
// of the change stream -- against one fenec-server.
//
//   node examples/recipes/functions-demo.mjs
import { spawn } from 'node:child_process';
import { timingSafeEqual } from 'node:crypto';
import { copyFileSync, mkdirSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { createServer as listener } from 'node:net';
import { join } from 'node:path';
import { cleanup, dir, here, query, server, until } from './harness.mjs';
import { relay, sign } from './webhook.mjs';
import { work } from './welcome-worker.mjs';

const say = (...a) => console.log(...a);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const root = (url, text, params) => query(url, 'root', text, params).then((r) => r.json());

try {
  const url = await server('functions', ['--http-token', 'root', '--cdc']);
  for (const stmt of [
    'create collection signups (email text @unique, name text)',
    'create collection jobs (kind text, payload json, run_at timestamp @sorted, owner text, attempts int)',
    'create collection orders (customer text, total int)',
    'create collection dead_letters (seq int, collection text, op text, change json, at timestamp)',
  ]) await root(url, stmt);

  // ---- 1. a function: a Worker that is an endpoint
  say('== 1. a Cloudflare Worker, under wrangler dev (workerd)');
  const wrangler = process.env.WRANGLER ?? 'wrangler';
  const probe = spawn(wrangler, ['--version'], { stdio: 'ignore' });
  const haveWrangler = await new Promise((r) => (probe.on('error', () => r(false)), probe.on('exit', (c) => r(c === 0))));
  if (!haveWrangler) {
    say('(skipped: no wrangler; set WRANGLER=path/to/wrangler)');
  } else {
    const wdir = join(dir, 'worker');
    mkdirSync(wdir);
    for (const f of ['index.mjs', 'wrangler.jsonc']) copyFileSync(join(here, 'worker', f), join(wdir, f));
    writeFileSync(join(wdir, '.dev.vars'), `FENEC_TOKEN=root\nFENEC_URL=${url}\n`);
    const port = await new Promise((resolve) => {
      const s = listener().listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => resolve(p)); });
    });
    const dev = spawn(wrangler, ['dev', '--port', String(port), '--ip', '127.0.0.1'], { cwd: wdir, stdio: 'ignore' });
    try {
      const fn = `http://127.0.0.1:${port}`;
      await until('wrangler dev', () => fetch(`${fn}/nothing`).then((r) => r.status === 404), 60_000);
      const signup = async (body, key) => {
        const res = await fetch(`${fn}/signup`, {
          method: 'POST', body: JSON.stringify(body),
          headers: { 'content-type': 'application/json', ...(key && { 'idempotency-key': key }) },
        });
        return `${res.status} ${await res.text()}`;
      };
      say('POST /signup  ada               ', await signup({ email: 'ada@example.test', name: 'Ada' }, 'k-1'));
      say('POST /signup  ada, sent again   ', await signup({ email: 'ada@example.test', name: 'Ada' }, 'k-1'));
      say('POST /signup  ada, a new request', await signup({ email: 'ada@example.test', name: 'Ada' }, 'k-2'));
      say('POST /signup  not an address    ', await signup({ email: 'ada', name: 'Ada' }));
      say('signups', JSON.stringify(await root(url, 'get signups select email')),
        ' jobs', JSON.stringify(await root(url, 'get jobs select kind, owner, attempts')));
    } finally {
      dev.kill();
    }
  }

  // ---- 2. a job queue
  say('\n== 2. a worker of the jobs the signups left');
  await root(url, 'put jobs {kind: "welcome", payload: {email: "bo@example.test", name: "Bo"}, run_at: now(), attempts: 0}');
  // A worker that took a job and died: its lease, two seconds, lapses by itself.
  const taken = await root(url, 'set jobs {owner: "dead-worker:1", run_at: now() + 2000, attempts: attempts + 1} where run_at <= now() order run_at limit 1 returning id, payload.email');
  say('a worker took', JSON.stringify(taken), 'and died');
  const stop = new AbortController();
  const started = Date.now();
  const sent = [];
  const working = work({
    fenec: url, token: 'root', name: 'w', lease: 30_000, wait: 5_000, signal: stop.signal,
    handlers: { welcome: async ({ email }) => { sent.push(email); say(`   mail to ${email} after ${((Date.now() - started) / 1000).toFixed(1)} s`); } },
    log: (m) => say(m),
  });
  await until('both mails', async () => sent.length === 2, 15_000);
  await sleep(300);
  say('jobs left', JSON.stringify(await root(url, 'get jobs select kind')));
  stop.abort();
  await working;

  // ---- 3. a webhook
  say('\n== 3. a webhook for orders');
  const secret = 'whsec-demo';
  const seen = new Map();
  const receiver = createServer(async (req, res) => {
    let body = '';
    for await (const c of req) body += c;
    const change = JSON.parse(body);
    const key = req.headers['idempotency-key'];
    const given = Buffer.from(req.headers['x-signature-256'] ?? '');
    const want = Buffer.from(sign(secret, body));
    const good = given.length === want.length && timingSafeEqual(given, want); // constant time
    seen.set(key, (seen.get(key) ?? 0) + 1);
    const status = !good ? 401 : change.doc?.total === 13 ? 400 : seen.get(key) === 1 && change.doc?.total === 40 ? 500 : 200;
    say(`   receiver: seq ${change.seq} ${change.collection} ${change.op} total ${change.doc?.total} signature ${good ? 'ok' : 'BAD'} -> ${status}`);
    res.writeHead(status).end();
  });
  await new Promise((r) => receiver.listen(0, '127.0.0.1', r));
  const hook = `http://127.0.0.1:${receiver.address().port}/hook`;
  const run = () => {
    const ctl = new AbortController();
    const done = relay({ fenec: url, adminToken: 'root', url: hook, secret, collections: ['orders'], ops: ['put'],
      backoff: 100, tries: 3, wait: 500, signal: ctl.signal, log: (m) => say(m) });
    return { stop: () => (ctl.abort(), done) };
  };
  let relayer = run();
  await sleep(300);
  await root(url, 'put orders {customer: "ada", total: 40}');
  await until('the first order', async () => seen.size >= 1 && [...seen.values()].some((n) => n >= 2));
  await root(url, 'put orders {customer: "bo", total: 13}');
  await until('the second order', async () => (await root(url, 'get dead_letters')).length === 1);
  await sleep(500);
  say('dead_letters', JSON.stringify(await root(url, 'get dead_letters select seq, collection, op')));
  say('-- the relay stopped; an order is written meanwhile; a new relay carries on from the place the server kept');
  await relayer.stop();
  await root(url, 'put orders {customer: "cy", total: 7}');
  relayer = run();
  await until('the third order', async () => seen.size >= 3);
  await sleep(300);
  await relayer.stop();
  receiver.close();
} finally {
  await cleanup();
}
process.exit(0);
