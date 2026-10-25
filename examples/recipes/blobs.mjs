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
import { DeleteObjectCommand } from '@aws-sdk/client-s3';
import { keyOf, s3From } from './files-backend.mjs';

export async function follow({ fenec, adminToken, s3, bucket, name = 'blobs', wait = 10_000, signal }) {
  const headers = { authorization: `Bearer ${adminToken}`, 'content-type': 'application/json' };
  // Made once; asking again answers that it exists.
  await fetch(`${fenec}/_changes/consumers/${name}`, { method: 'POST', headers });
  let deleted = 0;
  while (!signal?.aborted) {
    const page = await fetch(`${fenec}/_changes?consumer=${name}&wait=${wait}`, { headers, signal }).catch(() => null);
    if (!page) break;
    if (!page.ok) throw new Error(`/_changes: ${page.status} ${await page.text()}`);
    const next = Number(page.headers.get('fenec-next'));
    for (const line of (await page.text()).split('\n').filter(Boolean)) {
      const change = JSON.parse(line);
      if (change.collection === 'files' && change.op === 'del') {
        await s3.send(new DeleteObjectCommand({ Bucket: bucket, Key: keyOf(change.id) }));
        deleted += 1;
        console.log(`blobs: row ${change.id} deleted, so object ${keyOf(change.id)} too`);
      }
    }
    // Only now: the consumer's place moves once everything it read is done.
    await fetch(`${fenec}/_changes/consumers/${name}`, { method: 'POST', headers, body: JSON.stringify({ since: next }) });
  }
  return deleted;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const env = process.env;
  await follow({ fenec: env.FENEC_URL, adminToken: env.ADMIN_TOKEN, s3: s3From(env), bucket: env.BUCKET });
}
