// docs/functions, run: three ways to react to a write with no code in the
// database -- a Worker that is an endpoint (needs wrangler: WRANGLER=path/to/
// wrangler, or one on PATH, and it is left out where there is none), a job
// queue with a worker, and a signed webhook relay of the change stream. Each
// runs against a fenec-server of its own, so what one writes does not number
// what another says.
//
//   node examples/recipes/functions-demo.mjs            all three
//   ONLY=3 node examples/recipes/functions-demo.mjs     one of them
import { spawn } from 'node:child_process';
import { createPrivateKey } from 'node:crypto';
import { copyFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { createServer, request as httpRequest } from 'node:http';
import { join } from 'node:path';
import { cleanup, dir, here, query, server, until } from './harness.mjs';
import { addServiceKey, init, merge, shapes, token } from './idp.mjs';
import { serviceTokens } from './service-token.mjs';
import { Alert, relay } from './webhook.mjs';
import { receiver } from './webhook-verify.mjs';
import { work } from './welcome-worker.mjs';

const say = (...a) => console.log(...a);
const text = async (res) => JSON.stringify(await res.json());

// The services' keys: the public halves in the file the server reads, the
// private halves each service's own.
init(dir);
addServiceKey(dir);
merge(dir, join(dir, 'keys.json'));
const pem = readFileSync(join(dir, 'svc-1.pem'));
const tokenFor = (sub) => serviceTokens({ pem, sub });

/** A server over a new file, with the keys, the policy and these collections. */
async function fresh(name, schema, flags = []) {
  const url = await server(name, ['--http-token', 'root', '--cdc', '--jwt-keys', join(dir, 'keys.json'),
    '--policy', join(here, 'functions-policy.txt'), ...flags]);
  for (const stmt of schema) await query(url, 'root', stmt);
  return url;
}
const want = (n) => !process.env.ONLY || process.env.ONLY.split(',').includes(String(n));
const JOBS = 'create collection jobs (kind text, payload json, run_at timestamp @sorted, owner text, attempts int, error text)';

try {
  // ---- 1. a function: a Worker that is an endpoint
  if (want(1)) say('== 1. a Cloudflare Worker, under wrangler dev (workerd)');
  const wrangler = process.env.WRANGLER ?? 'wrangler';
  const probe = spawn(wrangler, ['--version'], { stdio: 'ignore' });
  const haveWrangler = await new Promise((r) => (probe.on('error', () => r(false)), probe.on('exit', (c) => r(c === 0))));
  if (!want(1)) {
    // not asked for
  } else if (!haveWrangler) {
    say('(skipped: no wrangler; set WRANGLER=path/to/wrangler)');
  } else {
    const url = await fresh('worker', ['create collection signups (email text @unique, name text)', JOBS]);
    const wdir = join(dir, 'worker');
    mkdirSync(wdir);
    for (const f of ['index.mjs', 'wrangler.jsonc']) copyFileSync(join(here, 'worker', f), join(wdir, f));
    const der = createPrivateKey(pem).export({ type: 'pkcs8', format: 'der' }).toString('base64');
    writeFileSync(join(wdir, '.dev.vars'), `SERVICE_KEY=${der}\nFENEC_URL=${url}\n`);
    // On port 0, and the inspector's too, and the address read off the line wrangler says it is
    // ready on: a port found free and handed over is free only until another process takes it.
    // In a process group of its own, so that stopping it stops the workerd it starts.
    const dev = spawn(wrangler, ['dev', '--port', '0', '--inspector-port', '0', '--ip', '127.0.0.1', '--show-interactive-dev-session=false'], {
      cwd: wdir, stdio: ['ignore', 'pipe', 'pipe'], detached: true,
      env: { ...process.env, WRANGLER_SEND_METRICS: 'false', FORCE_COLOR: '0', NO_COLOR: '1' },
    });
    const ended = new Promise((done) => dev.once('exit', done));
    let log = '';
    const ready = new Promise((found, failed) => {
      const read = (d) => { log += d; const m = /Ready on (http:\/\/\S+)\s/.exec(log); if (m) found(m[1]); };
      dev.stdout.on('data', read);
      dev.stderr.on('data', read);
      dev.once('error', (e) => failed(new Error(`wrangler dev did not start: ${e.message}`)));
      ended.then(() => failed(new Error(`wrangler dev ended before it was ready:\n${log}`)));
    });
    try {
      const fn = await Promise.race([ready, new Promise((_, no) => setTimeout(() => no(new Error(`wrangler dev not ready:\n${log}`)), 90_000))]);
      await until('the Worker to answer', () => fetch(`${fn}/nothing`).then((r) => r.status === 404), 60_000);
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
      say('signups', await text(await query(url, 'root', 'get signups select email')),
        ' jobs', await text(await query(url, 'root', 'get jobs select kind, owner, attempts')));
      // What the Worker's own token is answered: the rules of its identity, and no more.
      const own = await tokenFor('signup-function')();
      say('with the Worker\'s token, read signups   ', String((await query(url, own, 'get signups')).status));
      say('with the Worker\'s token, delete jobs    ', await text(await query(url, own, 'del jobs')));
      say('with the Worker\'s token, insert a signup', await text(await query(url, own, 'put signups {email: "x@example.test", name: "X"}')));
      // A token of the provider that says it is the Worker: the same sub, and no issuer of the services'.
      const forged = token(dir, { ...shapes.plain('signup-function') });
      const forgery = await query(url, forged, 'put signups {email: "y@example.test", name: "Y"}');
      say('with a provider\'s token, sub signup-function: insert a signup', String(forgery.status), await text(forgery));
    } finally {
      if (dev.exitCode === null && dev.signalCode === null) process.kill(-dev.pid, 'SIGINT');
      await ended;
    }
  }

  // ---- 2. a job queue
  if (want(2)) {
    say('\n== 2. a worker of the jobs the signups leave');
    const url = await fresh('queue', [JOBS]);
    const post = tokenFor('signup-function');
    // What the Worker above writes, written the way it does: as its own token.
    const asWorker = async (email, name) => query(url, await post(), 'put jobs {kind: "welcome", payload: $1, run_at: now(), attempts: 0}', [{ email, name }]);
    await asWorker('ada@example.test', 'Ada');
    await asWorker('bo@example.test', 'Bo');
    await asWorker('poison@example.test', 'Poison');
    // A worker that took the first job and died: its lease, three seconds, lapses by itself.
    const taken = await query(url, 'root', 'set jobs {owner: "dead-worker:1", run_at: now() + 3000, attempts: attempts + 1} where run_at <= now() order run_at limit 1 returning id, payload.email');
    say('a worker took', await text(taken), 'and died');
    const stop = new AbortController();
    const started = Date.now();
    const sent = [];
    const working = work({
      fenec: url, token: tokenFor('welcome-worker'), id: 'w1', lease: 30_000, wait: 5_000, backoff: 300, signal: stop.signal,
      handlers: {
        welcome: async ({ email }) => {
          if (email.startsWith('poison')) throw new Error('mailbox refused');
          sent.push(email);
          say(`   mail to ${email} after ${Math.round((Date.now() - started) / 1000)} s`);
        },
      },
      log: say,
    });
    // Both mails sent and acked, and the one job left the dead letter.
    const settled = async () => {
      const left = await (await query(url, 'root', 'get jobs select id, run_at')).json();
      return sent.length === 2 && left.length === 1 && left[0].run_at === null;
    };
    await until('both mails and the dead letter', settled, 20_000);
    say('jobs left', await text(await query(url, 'root', 'get jobs select id, run_at, attempts, error')).then((t) => t.replace(/"run_at":"[^"]*"/, '"run_at":…')));
    stop.abort();
    await working;
  }

  // ---- 3. a webhook
  if (want(3)) {
    say('\n== 3. a webhook for orders');
    const url = await fresh('hook', ['create collection orders (customer text, total int, note text)',
      'create collection dead_letters (seq int, collection text, op text, status int, change json, at timestamp)']);
    let secrets = ['whsec-1'];
    let skew = 0; // seconds the receiver's clock is ahead, to show a replay an hour late
    const failedOnce = new Set();
    const captured = [];
    const done = new Set();
    const handle = receiver({
      secrets: () => secrets, done, captured, now: () => Date.now() / 1000 + skew, log: (m) => say(`   receiver: ${m}`),
      act: async (change) => {
        if (change.doc?.total === 40 && !failedOnce.has(change.seq)) { failedOnce.add(change.seq); throw Object.assign(new Error('x'), { status: 500 }); }
        if (change.doc?.total === 13) throw Object.assign(new Error('x'), { status: 400 });
        if (!change.doc) say(`   receiver: it was sent ${JSON.stringify(change)}`);
        if (change.doc?.note) say(`   receiver: a note of ${change.doc.note.length} characters, ${Buffer.byteLength(change.doc.note)} bytes`);
      },
    });
    const receiverServer = createServer((req, res) =>
      (req.url === '/moved' ? res.writeHead(302, { location: '/elsewhere' }).end() : handle(req, res)));
    await new Promise((r) => receiverServer.listen(0, '127.0.0.1', r));
    const hook = `http://127.0.0.1:${receiverServer.address().port}/hook`;

    const run = (opts = {}) => {
      const ctl = new AbortController();
      const finished = relay({ fenec: url, adminToken: 'root', url: hook, secrets, collections: ['orders'], ops: ['put'],
        backoff: 100, tries: 3, wait: 500, signal: ctl.signal, log: (m) => say(m), ...opts })
        .then(() => null, (e) => e);
      return { stop: async () => (ctl.abort(), finished), finished };
    };

    // A relay has made its consumer when the server lists it: a write before that is before its place.
    const consumerMade = (name) => until(`consumer ${name}`, async () =>
      (await (await fetch(`${url}/_changes/consumers`, { headers: { authorization: 'Bearer root' } })).json()).some((c) => c.name === name));
    // A relay has committed what it delivered when its consumer's place is at the last `seq` done.
    // Stopped before that, a delivery answered while the abort arrived goes out again at the next
    // start, which is what the receiver's `seq` is for and what a run of this demo should not depend on.
    const caughtUp = (name) => until(`consumer ${name} caught up`, async () =>
      (await (await fetch(`${url}/_changes/consumers`, { headers: { authorization: 'Bearer root' } })).json())
        .some((c) => c.name === name && c.since >= Math.max(...done)));
    let relayer = run();
    await consumerMade('webhook');
    await query(url, 'root', 'put orders {customer: "ada", total: 40}');
    await until('the first order', async () => done.size >= 1);
    await query(url, 'root', 'put orders {customer: "bo", total: 13}');
    await until('the second order', async () => (await (await query(url, 'root', 'get dead_letters')).json()).length === 1);
    say('dead_letters', await text(await query(url, 'root', 'get dead_letters select seq, collection, op, status')));

    say('-- the first delivery, captured, sent again');
    const first = captured[0];
    const resend = () => fetch(hook, { method: 'POST', headers: { 'content-type': 'application/json', 'x-webhook-timestamp': first.headers['x-webhook-timestamp'], 'x-webhook-signature': first.headers['x-webhook-signature'] }, body: first.body });
    say(`   replayed at once: ${(await resend()).status}`);
    skew = 3600;
    say(`   replayed an hour later: ${(await resend()).status}`);
    skew = 0;

    say('-- a signature of accents, as long in characters as a real one, and a body of three-byte characters');
    const accents = await new Promise((resolve) => {
      const req = httpRequest(hook, { method: 'POST', headers: {
        'x-webhook-timestamp': String(Math.floor(Date.now() / 1000)), 'x-webhook-signature': `v1=${'é'.repeat(64)}`,
      } }, (res) => { res.resume(); resolve(res.statusCode); });
      req.end('{}');
    });
    say(`   a signature of 64 accents: ${accents}`);
    await query(url, 'root', 'put orders {customer: "fa", total: 5, note: $1}', ['€'.repeat(70_000)]);
    await until('the large order', async () => done.size >= 2);
    await caughtUp('webhook');

    say('-- the relay stopped; an order is written meanwhile; a new relay carries on from the place the server kept');
    await relayer.stop();
    await query(url, 'root', 'put orders {customer: "cy", total: 7}');
    relayer = run();
    await until('the third order', async () => done.size >= 3);
    await caughtUp('webhook');
    await relayer.stop();

    say('-- the receiver rotates its secret before the relay has heard');
    secrets = ['whsec-2'];
    await query(url, 'root', 'put orders {customer: "di", total: 8}');
    relayer = run({ secrets: ['whsec-1'] });
    const alert = await relayer.finished;
    say(`${alert instanceof Alert ? 'ALERT' : 'error'}: ${alert.message}`);
    say('-- the relay is given both secrets for a while, and the same change goes out');
    relayer = run({ secrets: ['whsec-2', 'whsec-1'] });
    await until('the fourth order', async () => done.size >= 4);
    await caughtUp('webhook');
    await relayer.stop();

    say('-- only ids, for a receiver that has no business with the rest');
    relayer = run({ name: 'webhook-ids', shape: 'ids', collections: ['orders'], ops: ['del', 'put'], secrets: ['whsec-2'] });
    await consumerMade('webhook-ids');
    await query(url, 'root', 'del orders where customer = "di"');
    await until('the ids', async () => done.size >= 5);
    await caughtUp('webhook-ids');
    await relayer.stop();

    say('-- a receiver whose address has moved: a redirect is not followed');
    relayer = run({ name: 'webhook-moved', url: hook.replace('/hook', '/moved'), secrets: ['whsec-2'] });
    await consumerMade('webhook-moved');
    await query(url, 'root', 'put orders {customer: "ed", total: 9}');
    const moved = await relayer.finished;
    say(`${moved instanceof Alert ? 'ALERT' : 'error'}: ${moved.message}`);
    receiverServer.close();
  }
} finally {
  await cleanup();
}
process.exit(0);
