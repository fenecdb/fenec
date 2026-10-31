// The bucket follows the table, docs/files: a consumer of the change stream
// (`fenec-server --cdc`) that deletes an object once its row is gone, whoever
// deleted the row -- the backend, a person's own token from a page, the sweep
// of pending uploads. The server keeps the consumer's place, so a restart
// carries on where it stopped; a crash between the delete and the commit
// deletes again, which an object store answers as it answered the first time.
//
//   FENEC_URL=... ADMIN_TOKEN=... S3_ENDPOINT=... S3_KEY=... S3_SECRET=... BUCKET=... node blobs.mjs
//
// /_changes takes the server's own token and no scoped one: it carries every
// row of every person, documents and all.
//
// A deletion names its row by id alone, which is why a key is made from the
// id. If the consumer is further behind than the server keeps (410), the
// deletions it missed are gone from the stream and from every read: it
// reconciles instead (reconcile.mjs), then goes on from the first write the
// server still has.
import { DeleteObjectCommand } from '@aws-sdk/client-s3';
import { reconcile } from './reconcile.mjs';
import { keysOf, s3From } from './store.mjs';
import { isMain } from './main.mjs';

const RETRIES = 8; // answers of the server missed one after another
const pause = (ms, signal) => new Promise((resolve) => {
  const t = setTimeout(resolve, ms);
  signal?.addEventListener('abort', () => (clearTimeout(t), resolve()), { once: true });
});

export async function follow({ fenec, adminToken, s3, bucket, prefix = '', name = 'blobs', wait = 10_000, signal, log = console.log }) {
  const headers = { authorization: `Bearer ${adminToken}`, 'content-type': 'application/json' };
  const keys = keysOf(prefix);
  // PATCH moves the consumer; a POST only makes it.
  const move = (since) => fetch(`${fenec}/_changes/consumers/${name}`, { method: 'PATCH', headers, body: JSON.stringify({ since }) });
  const commit = async (since) => {
    const res = await move(since);
    if (!res.ok) throw new Error(`commit of ${name} at ${since}: ${res.status}`);
  };
  // Made at every start: a POST makes the consumer (201) and leaves one that
  // exists where it stands (200), so a restart carries on from the last commit.
  const made = await fetch(`${fenec}/_changes/consumers/${name}`, { method: 'POST', headers });
  if (!made.ok) throw new Error(`/_changes/consumers/${name}: ${made.status}`);
  // A server restarted, or not answering for a while, is waited for with a back-off;
  // after RETRIES answers missed the follower ends with an error, so that whatever
  // supervises it starts it again. It ends with a zero only at its own signal.
  let missed = 0;
  while (!signal?.aborted) {
    const page = await fetch(`${fenec}/_changes?consumer=${name}&wait=${wait}`, { headers, signal }).catch(() => null);
    if (!page) {
      if (signal?.aborted) break;
      if (++missed >= RETRIES) throw new Error(`/_changes: no answer ${missed} times`);
      await pause(Math.min(500 * 2 ** missed, 30_000), signal);
      continue;
    }
    missed = 0;
    if (page.status === 410) {
      const { since } = await page.json();
      log(`blobs: behind what the server keeps; reconciling, then on from ${since}`);
      const gone = await reconcile({ fenec, adminToken, s3, bucket, prefix });
      log(`blobs: reconciled, ${gone.length} deleted: ${gone.join(', ') || 'none'}`);
      // The first write the server keeps can be past what is on disk yet (a commit
      // there is 409), and can move on while writes are not: not at once, then.
      const moved = await move(since);
      if (!moved.ok && moved.status !== 409) throw new Error(`commit of ${name} at ${since}: ${moved.status}`);
      await pause(500, signal);
      continue;
    }
    if (!page.ok) throw new Error(`/_changes: ${page.status}`);
    const next = Number(page.headers.get('fenec-next'));
    for (const line of (await page.text()).split('\n').filter(Boolean)) {
      const change = JSON.parse(line);
      if (change.collection === 'files' && change.op === 'del') {
        // Either key: a file deleted before it was done has only its upload.
        for (const Key of [keys.pending(change.id), keys.live(change.id)]) {
          await s3.send(new DeleteObjectCommand({ Bucket: bucket, Key }));
        }
        log(`blobs: row ${change.id} deleted, so its objects too`);
      }
    }
    // Only now: the consumer's place moves once everything it read is done.
    await commit(next);
  }
}

if (isMain(import.meta.url)) {
  const env = process.env;
  await follow({
    fenec: env.FENEC_URL, adminToken: env.ADMIN_TOKEN, s3: s3From(env), bucket: env.BUCKET, prefix: env.KEY_PREFIX ?? '',
  });
}
