// A webhook for a table, docs/functions: a consumer of the change stream
// (`fenec-server --cdc`) that POSTs each write it is asked for to a URL as
// signed JSON, and moves on only when the URL has said yes. The server keeps
// the consumer's place, so the relay can be stopped, killed or run on another
// machine and carry on from the last delivery that was answered.
//
//   FENEC_URL=... ADMIN_TOKEN=... HOOK_URL=https://... HOOK_SECRET=... node webhook.mjs orders
//
// What goes out is the change as the stream has it: `put` carries the whole
// document, every field of it. A receiver is told as much as the stream says,
// so send ids (`shape: 'ids'`) where the receiver has no business with the
// rest and can read the row itself.
//
// What a delivery is answered decides what happens to the stream:
//   2xx           done; the next change.
//   408, 429, 5xx, no answer
//                 tried again with a back-off, `tries` times; then the relay
//                 stops (below), for a receiver that is down is not a reason
//                 to put every change it missed away.
//   400, 413, 422 and the other 4xx
//                 the receiver refuses this change, and would refuse it again:
//                 it is kept in `dead_letters` and the stream goes on.
//   401, 403, 404, 405, 410, and any redirect
//                 stop, and say so. The receiver does not trust this sender
//                 (a secret that was rotated, a URL that moved), and every
//                 change behind this one would be put away for the same reason.
//                 A redirect is never followed: the body is signed for the
//                 address it was sent to.
// Stopping is throwing an Alert, with nothing committed: the process exits
// non-zero, whoever watches it is told, and a restart sends this change again.
import { signatures } from './webhook-verify.mjs';
import { isMain } from './main.mjs';

export class Alert extends Error {}

const sleep = (ms, signal) => new Promise((resolve) => {
  const t = setTimeout(resolve, ms);
  signal?.addEventListener('abort', () => (clearTimeout(t), resolve()), { once: true });
});

const STOP = new Set([401, 403, 404, 405, 410]);
const RETRY = (status) => status === 408 || status === 429 || status >= 500;

/**
 * Deliver the writes to `collections` (ops: put, del) at `url`, forever.
 * `secrets` is one secret, or two while one is being rotated out.
 */
export async function relay({
  fenec, adminToken, url, secrets, collections, ops = ['put', 'del'], shape = 'full', name = 'webhook',
  tries = 5, backoff = 500, wait = 10_000, signal, log = console.log,
}) {
  const headers = { authorization: `Bearer ${adminToken}`, 'content-type': 'application/json' };
  const server = (path, init) => fetch(`${fenec}${path}`, { headers, ...init });
  // Made once, where there is none. A POST to a consumer that exists moves it
  // to the last write on disk, and every change since it was stopped is lost.
  const listed = await (await server('/_changes/consumers')).json();
  if (!listed.some((c) => c.name === name)) await server(`/_changes/consumers/${name}`, { method: 'POST' });

  async function deliver(change) {
    const sent = shape === 'ids' ? { seq: change.seq, collection: change.collection, op: change.op, id: change.id ?? change.doc?.id } : change;
    const body = JSON.stringify(sent);
    for (let attempt = 1; attempt <= tries; attempt++) {
      const timestamp = Math.floor(Date.now() / 1000);
      const res = await fetch(url, {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          'x-webhook-timestamp': String(timestamp),
          'x-webhook-signature': signatures([].concat(secrets), timestamp, body),
        },
        body,
        redirect: 'manual',
        signal,
      }).catch(() => null);
      if (res?.ok) return;
      if (signal?.aborted) throw new Error('stopped'); // not delivered, not committed: the next run sends it again
      const status = res ? res.status : 0;
      if (status >= 300 && status < 400) throw new Alert(`seq ${change.seq}: ${url} redirects (${status}), and a signed body is not sent on`);
      if (STOP.has(status)) throw new Alert(`seq ${change.seq}: ${url} answers ${status}; stopped, nothing committed`);
      if (res && !RETRY(status)) {
        log(`relay: seq ${change.seq} refused (${status})`);
        await server('/query', {
          method: 'POST',
          body: JSON.stringify({ query: 'put dead_letters {seq: $1, collection: $2, op: $3, status: $4, change: $5, at: now()}',
            params: [change.seq, change.collection, change.op, status, sent] }),
        });
        log(`relay: seq ${change.seq} kept in dead_letters`);
        return;
      }
      log(`relay: seq ${change.seq} try ${attempt} of ${tries}: ${status || 'no answer'}`);
      if (attempt < tries) await sleep(Math.min(backoff * 2 ** (attempt - 1), 60_000), signal);
    }
    throw new Alert(`seq ${change.seq}: ${url} did not answer ${tries} times; stopped, nothing committed`);
  }

  while (!signal?.aborted) {
    const page = await server(`/_changes?consumer=${name}&wait=${wait}`, { signal }).catch(() => null);
    if (!page) break;
    if (!page.ok) throw new Error(`/_changes: ${page.status} ${await page.text()}`);
    const next = Number(page.headers.get('fenec-next'));
    for (const line of (await page.text()).split('\n').filter(Boolean)) {
      const change = JSON.parse(line);
      if (!collections.includes(change.collection) || !ops.includes(change.op)) continue;
      try {
        await deliver(change);
      } catch (e) {
        if (signal?.aborted) return;
        throw e;
      }
    }
    // After every delivery of the page was answered, not before.
    await server(`/_changes/consumers/${name}`, { method: 'POST', body: JSON.stringify({ since: next }) });
  }
}

if (isMain(import.meta.url)) {
  const env = process.env;
  try {
    await relay({
      fenec: env.FENEC_URL, adminToken: env.ADMIN_TOKEN, url: env.HOOK_URL,
      secrets: env.HOOK_SECRET.split(','), // two, comma between, while one is rotated out
      collections: process.argv.slice(2), shape: env.HOOK_SHAPE ?? 'full',
    });
  } catch (e) {
    console.error(e instanceof Alert ? `ALERT: ${e.message}` : e);
    process.exit(1);
  }
}
