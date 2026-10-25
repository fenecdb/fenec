// A webhook for a table, docs/functions: a consumer of the change stream
// (`fenec-server --cdc`) that POSTs each write it is asked for to a URL as
// signed JSON, and moves on only when the URL has said yes. The server keeps
// the consumer's place, so the relay can be stopped, killed or run on another
// machine and carry on from the last delivery that was answered.
//
//   FENEC_URL=... ADMIN_TOKEN=... HOOK_URL=https://... HOOK_SECRET=... node webhook.mjs orders
//
// What a receiver can rely on:
//   X-Signature-256: sha256=<HMAC-SHA256 of the body, with the shared secret>
//   Idempotency-Key: <seq>, the change's number in this database: a delivery
//   sent again (the relay died between the answer and the commit) carries the
//   same key, and a receiver that has seen it answers 200 and does nothing.
import { createHmac } from 'node:crypto';

const sleep = (ms, signal) => new Promise((resolve) => {
  const t = setTimeout(resolve, ms);
  signal?.addEventListener('abort', () => (clearTimeout(t), resolve()), { once: true });
});

export const sign = (secret, body) => `sha256=${createHmac('sha256', secret).update(body).digest('hex')}`;

/** Deliver the writes to `collections` (ops: put, del) at `url`, forever. */
export async function relay({
  fenec, adminToken, url, secret, collections, ops = ['put', 'del'], name = 'webhook',
  tries = 5, backoff = 500, wait = 10_000, signal, log = console.log,
}) {
  const headers = { authorization: `Bearer ${adminToken}`, 'content-type': 'application/json' };
  const server = (path, init) => fetch(`${fenec}${path}`, { headers, ...init });
  await server(`/_changes/consumers/${name}`, { method: 'POST' }); // made once; a second ask says it exists

  async function deliver(change) {
    const body = JSON.stringify(change);
    for (let attempt = 1; attempt <= tries; attempt++) {
      const res = await fetch(url, {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'x-signature-256': sign(secret, body), 'idempotency-key': String(change.seq) },
        body,
        signal,
      }).catch(() => null);
      if (res?.ok) return true;
      if (signal?.aborted) throw new Error('stopped'); // not delivered, not committed: the next run sends it again
      // 4xx other than 408 and 429 is the receiver saying "never": trying
      // again would only hold up every change behind this one.
      if (res && res.status >= 400 && res.status < 500 && ![408, 429].includes(res.status)) {
        log(`relay: seq ${change.seq} refused (${res.status})`);
        break;
      }
      log(`relay: seq ${change.seq} try ${attempt} of ${tries}: ${res ? res.status : 'no answer'}`);
      await sleep(backoff * 2 ** (attempt - 1), signal);
    }
    // Kept where a person will look, and the stream goes on.
    await server('/query', {
      method: 'POST',
      body: JSON.stringify({ query: 'put dead_letters {seq: $1, collection: $2, op: $3, change: $4, at: now()}',
        params: [change.seq, change.collection, change.op, change] }),
    });
    log(`relay: seq ${change.seq} kept in dead_letters`);
    return false;
  }

  while (!signal?.aborted) {
    const page = await server(`/_changes?consumer=${name}&wait=${wait}`, { signal }).catch(() => null);
    if (!page) break;
    if (!page.ok) throw new Error(`/_changes: ${page.status} ${await page.text()}`);
    const next = Number(page.headers.get('fenec-next'));
    for (const line of (await page.text()).split('\n').filter(Boolean)) {
      const change = JSON.parse(line);
      if (collections.includes(change.collection) && ops.includes(change.op)) {
        try {
          await deliver(change);
        } catch (e) {
          if (signal?.aborted) return;
          throw e;
        }
      }
    }
    // After every delivery of the page was answered, not before.
    await server(`/_changes/consumers/${name}`, { method: 'POST', body: JSON.stringify({ since: next }) });
  }
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const env = process.env;
  await relay({
    fenec: env.FENEC_URL, adminToken: env.ADMIN_TOKEN, url: env.HOOK_URL, secret: env.HOOK_SECRET,
    collections: process.argv.slice(2),
  });
}
