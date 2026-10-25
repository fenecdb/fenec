// The bucket held to the table, docs/files: every object under `pending/` and
// `files/` with no row to be there is deleted. Run it from cron every hour, and
// it is also what a consumer of the change stream does when it was answered 410.
//
// Why a consumer alone is not enough:
//   - An upload URL outlives the row it was made for. A row deleted (by the
//     person, by the sweep) while the URL still has minutes to live, and a PUT
//     with it, leaves an object nobody will ever delete: storage outside every
//     quota the database holds.
//   - A consumer that fell further behind than `--replication-buffer` keeps is
//     answered 410. A deletion is not in a read of the table, so "start over
//     from a read" cannot find the objects whose rows are gone: this can.
//
//   FENEC_URL=... ADMIN_TOKEN=... S3_ENDPOINT=... S3_KEY=... S3_SECRET=... BUCKET=... node reconcile.mjs
//
// The objects are listed first and the rows read after: a row is made before
// its URL, so an object listed has a row made earlier, and a row deleted since
// is what makes it an orphan. Read the other way round, an upload that landed
// between the two would look like one. An id is never given to a second row.
import { DeleteObjectCommand, ListObjectsV2Command } from '@aws-sdk/client-s3';
import { keysOf, s3From } from './store.mjs';
import { isMain } from './main.mjs';

async function* objects(s3, bucket, prefix) {
  let token;
  do {
    const page = await s3.send(new ListObjectsV2Command({ Bucket: bucket, Prefix: prefix, ContinuationToken: token }));
    for (const o of page.Contents ?? []) yield o.Key;
    token = page.NextContinuationToken;
  } while (token);
}

/** Every file row's state, by id, read in pages of a thousand. */
async function states(fenec, adminToken) {
  const rows = new Map();
  for (let after = 0; ;) {
    const res = await fetch(`${fenec}/query`, {
      method: 'POST',
      headers: { authorization: `Bearer ${adminToken}`, 'content-type': 'application/json' },
      body: JSON.stringify({ query: 'get files select id, state where id > $1 order id limit 1000', params: [after] }),
    });
    if (!res.ok) throw new Error(`reconcile: ${res.status}`);
    const page = await res.json();
    for (const r of page) rows.set(r.id, r.state);
    if (page.length < 1000) return rows;
    after = page[page.length - 1].id;
  }
}

/** The keys deleted, in order. `prefix` is the tenant on a `--dir` node, and `fenec` that tenant's address. */
export async function reconcile({ fenec, adminToken, s3, bucket, prefix = '' }) {
  const keys = keysOf(prefix);
  const listed = [];
  for await (const key of objects(s3, bucket, prefix)) if (keys.parse(key)) listed.push(key);
  const rows = await states(fenec, adminToken);
  const deleted = [];
  for (const key of listed.sort()) {
    const { kind, id } = keys.parse(key);
    // An upload belongs to a pending row; what a ready row left under pending/
    // (the copy was made, the delete failed) and what has no row go.
    const kept = kind === 'pending' ? rows.get(id) === 'pending' : rows.has(id);
    if (kept) continue;
    await s3.send(new DeleteObjectCommand({ Bucket: bucket, Key: key }));
    deleted.push(key);
  }
  return deleted;
}

if (isMain(import.meta.url)) {
  const env = process.env;
  const gone = await reconcile({
    fenec: env.FENEC_URL, adminToken: env.ADMIN_TOKEN, s3: s3From(env), bucket: env.BUCKET, prefix: env.KEY_PREFIX ?? '',
  });
  console.log(`reconcile: ${gone.length} objects with no row deleted${gone.length ? `: ${gone.join(', ')}` : ''}`);
}
