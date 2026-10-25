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
// between the two would look like one. An id is not given to a second row --
// until the database is restored to a backup older than some of its ids, which
// hands those ids out again: reconcile before the restore's first upload, not
// after, or an object of the lost rows becomes the object of the new ones.
//
// It deletes, so it refuses what looks like a mistake before it does:
//   --dry-run            says what it would delete and deletes nothing
//   a database with no file row at all, over a bucket that has objects: the
//     wrong server, a file not yet restored (--force goes on)
//   more than --max-delete-percent N (50) of the objects, once there are ten to
//     go: a table emptied, a prefix of another tenant's (--force goes on)
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

/**
 * The keys deleted, in order (with `dryRun`, the keys that would be). `prefix`
 * is the tenant on a `--dir` node, and `fenec` that tenant's address.
 */
export async function reconcile({ fenec, adminToken, s3, bucket, prefix = '', dryRun = false, maxDeletePercent = 50, force = false }) {
  const keys = keysOf(prefix);
  const listed = [];
  for await (const key of objects(s3, bucket, prefix)) if (keys.parse(key)) listed.push(key);
  const rows = await states(fenec, adminToken);
  const doomed = listed.sort().filter((key) => {
    const { kind, id } = keys.parse(key);
    // An upload belongs to a pending row; what a ready row left under pending/
    // (the copy was made, the delete failed) and what has no row go.
    return !(kind === 'pending' ? rows.get(id) === 'pending' : rows.has(id));
  });
  if (!force) {
    if (rows.size === 0 && listed.length > 0) {
      throw new Error(`reconcile: the database holds no file rows and the bucket ${listed.length} objects; the wrong server, or a restore not done? (--force goes on)`);
    }
    if (doomed.length >= 10 && doomed.length * 100 > listed.length * maxDeletePercent) {
      throw new Error(`reconcile: ${doomed.length} of ${listed.length} objects would go, more than ${maxDeletePercent}%; --force goes on`);
    }
  }
  if (dryRun) return doomed;
  for (const Key of doomed) await s3.send(new DeleteObjectCommand({ Bucket: bucket, Key }));
  return doomed;
}

if (isMain(import.meta.url)) {
  const env = process.env;
  const args = process.argv.slice(2);
  const percent = args.indexOf('--max-delete-percent');
  try {
    const gone = await reconcile({
      fenec: env.FENEC_URL, adminToken: env.ADMIN_TOKEN, s3: s3From(env), bucket: env.BUCKET, prefix: env.KEY_PREFIX ?? '',
      dryRun: args.includes('--dry-run'), force: args.includes('--force'),
      ...(percent >= 0 && { maxDeletePercent: Number(args[percent + 1]) }),
    });
    const verb = args.includes('--dry-run') ? 'would be deleted' : 'deleted';
    console.log(`reconcile: ${gone.length} objects with no row ${verb}${gone.length ? `: ${gone.join(', ')}` : ''}`);
  } catch (e) {
    console.error(e.message);
    process.exit(1);
  }
}
