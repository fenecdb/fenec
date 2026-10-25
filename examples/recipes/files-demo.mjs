// docs/files, run: fenec-server with files-policy.txt, an S3-compatible server
// (a container of SeaweedFS unless S3_ENDPOINT names one: MinIO, R2 with its
// keys, S3), the backend and the bucket's follower, and two people who
// upload, download, are refused each other's files and delete one.
//
//   node examples/recipes/files-demo.mjs
//
// FENEC_SERVER names the binary (target/debug/fenec-server); for your own
// bucket give S3_ENDPOINT, S3_REGION, S3_KEY, S3_SECRET and BUCKET.
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { CreateBucketCommand, HeadObjectCommand, PutObjectCommand } from '@aws-sdk/client-s3';
import { backend } from './files-backend.mjs';
import { follow } from './blobs.mjs';
import { reconcile } from './reconcile.mjs';
import { atExit, cleanup, dir, here, query, server, until } from './harness.mjs';
import { addServiceKey, init, merge, shapes, token } from './idp.mjs';
import { keysOf, s3From } from './store.mjs';
import { serviceTokens } from './service-token.mjs';

const say = (...a) => console.log(...a);
// The URL as it is shown here: the store's address is the run's, and a signature is no one's business.
const short = (url) => `<store>${new URL(url).pathname}?…`;
let container = null;
atExit(() => container && execFileSync('docker', ['rm', '-f', container], { stdio: 'ignore' }));

// Pinned: a store that changes under a recipe is a recipe that fails for another reason.
const SEAWEEDFS = 'chrislusf/seaweedfs:4.50';

try {
  // The identity provider's stand-in, the people it signs in, and a key of the
  // backend's own, whose public half the server's file holds beside the provider's.
  init(dir);
  addServiceKey(dir);
  merge(dir, join(dir, 'keys.json'));
  const as = (sub, claims = {}) => token(dir, { ...shapes.plain(sub), ...claims });
  const alice = as('alice');
  const bob = as('bob');
  const appToken = serviceTokens({ pem: readFileSync(join(dir, 'svc-1.pem')), sub: 'files-backend' });

  // fenec-server: the keys, the policy, the change stream the follower reads
  // (a buffer of 1 MiB, so that the last step can outrun it).
  const url = await server('files', [
    '--http-token', 'root', '--cdc', '--replication-buffer', '1',
    '--jwt-keys', join(dir, 'keys.json'), '--policy', join(here, 'files-policy.txt'),
  ]);
  const post = (token, path, body) => query(url, token, body.query, body.params);
  for (const stmt of readFileSync(join(here, 'files-schema.fenecql'), 'utf8').split('\n').filter(Boolean)) {
    await query(url, 'root', stmt);
  }
  await query(url, 'root', 'create collection junk (body text)');

  // The object store.
  const env = { ...process.env };
  if (!env.S3_ENDPOINT) {
    writeFileSync(join(dir, 's3.json'), JSON.stringify({ identities: [{ name: 'recipes',
      credentials: [{ accessKey: 'recipes-key', secretKey: 'recipes-secret-key' }],
      actions: ['Admin', 'Read', 'Write', 'List', 'Tagging'] }] }));
    // Pulled first, quietly, and tried again: a run that pulls as it starts writes its progress
    // to stderr, which is the output the golden holds, and Docker Hub answers an anonymous
    // runner now and then with a limit or a reset.
    for (let attempt = 1; ; attempt++) {
      try {
        execFileSync('docker', ['pull', '-q', SEAWEEDFS], { stdio: 'ignore' });
        break;
      } catch (e) {
        if (attempt === 3) throw new Error(`docker pull ${SEAWEEDFS} failed ${attempt} times`);
        await new Promise((r) => setTimeout(r, 2000 * attempt));
      }
    }
    container = execFileSync('docker', ['run', '-d', '--rm', '--name', `fenec-recipes-s3-${process.pid}`,
      '-p', '127.0.0.1:0:8333', '-v', `${join(dir, 's3.json')}:/etc/seaweedfs/s3.json:ro`,
      SEAWEEDFS, 'server', '-s3', '-s3.config=/etc/seaweedfs/s3.json', '-dir=/data'],
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim().slice(0, 12); // stderr kept off ours, and on the error
    const port = execFileSync('docker', ['port', container, '8333/tcp'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim().split(':').pop();
    Object.assign(env, { S3_ENDPOINT: `http://127.0.0.1:${port}`, S3_REGION: 'us-east-1', S3_KEY: 'recipes-key',
      S3_SECRET: 'recipes-secret-key', S3_PATH_STYLE: '1', BUCKET: 'recipes' });
    await until('the object store', async () => (await fetch(env.S3_ENDPOINT)).status === 403, 60_000);
  }
  const s3 = s3From(env);
  const keys = keysOf();
  await until('the bucket', () => s3.send(new CreateBucketCommand({ Bucket: env.BUCKET })).then(() => true,
    (e) => e.name === 'BucketAlreadyOwnedByYou'), 30_000);
  say(`fenec-server and the bucket ${env.BUCKET} are up`);

  const app = backend({ fenec: url, appToken, s3, bucket: env.BUCKET });
  await new Promise((r) => app.listen(0, '127.0.0.1', r));
  const api = `http://127.0.0.1:${app.address().port}`;
  const call = (token, method, path, body) => fetch(`${api}${path}`, {
    method, headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: body && JSON.stringify(body),
  });
  const json = async (res) => (res.status === 204 ? '' : await res.json());
  const there = (Key) => s3.send(new HeadObjectCommand({ Bucket: env.BUCKET, Key })).then(() => true, () => false);
  const gone = (Key) => until(`${Key} to go`, async () => !(await there(Key)));
  const rows = async (text) => JSON.stringify(await (await query(url, 'root', text)).json());

  // The bucket's follower, with an end of its own. What it says is kept and
  // shown where the run has waited for it, so that the output reads the same
  // however the two interleave.
  const said = [];
  const flush = () => said.splice(0).forEach((l) => say(l));
  const waitSaid = (re) => until(`the follower to say ${re}`, async () => said.some((l) => re.test(l)));
  const startFollower = () => {
    const ctl = new AbortController();
    const done = follow({ fenec: url, adminToken: 'root', s3, bucket: env.BUCKET, signal: ctl.signal, wait: 500, log: (m) => said.push(m) });
    // A follower that ends with an error fails the run, with the container removed.
    done.catch((e) => cleanup().then(() => { console.error(e); process.exit(1); }));
    return { ctl, done };
  };
  let follower = startFollower();

  say('\n-- alice adds a file');
  const bytes = Buffer.from('hello from alice\n');
  let res = await call(alice, 'POST', '/files', { name: 'hello.txt', type: 'text/plain', size: bytes.length });
  const made = await json(res);
  say(`POST /files             ${res.status} {"id":${made.id},"url":"${short(made.url)}"}`);
  say('the row the server holds:', await rows('get files select id, owner, name, type, size, state'));

  say('\n-- she sends the bytes to the bucket herself, with the URL');
  res = await fetch(made.url, { method: 'PUT', headers: made.headers, body: bytes });
  say(`PUT <presigned>         ${res.status}`);
  res = await call(alice, 'GET', `/files/${made.id}`);
  say(`GET /files/${made.id} before done ${res.status} ${JSON.stringify(await json(res))}`);
  res = await call(alice, 'POST', `/files/${made.id}/done`);
  say(`POST /files/${made.id}/done    ${res.status} ${JSON.stringify(await json(res))}`);
  res = await call(alice, 'POST', `/files/${made.id}/done`);
  say(`POST /files/${made.id}/done again ${res.status} ${JSON.stringify(await json(res))}`);
  say(`the upload copied to ${keys.live(made.id)}, ${keys.pending(made.id)} ${(await there(keys.pending(made.id))) ? 'still there' : 'gone'}`);

  say('\n-- she downloads it; bob is told there is no such file');
  res = await call(alice, 'GET', `/files/${made.id}`);
  const got = await json(res);
  say(`GET /files/${made.id} (alice)   ${res.status} {"url":"${short(got.url)}","name":"${got.name}","size":${got.size}}`);
  const download = await fetch(got.url);
  say(`GET <presigned>         ${download.status} ${JSON.stringify(await download.text())}`);
  res = await call(bob, 'GET', `/files/${made.id}`);
  say(`GET /files/${made.id} (bob)     ${res.status} ${JSON.stringify(await json(res))}`);
  res = await call(bob, 'DELETE', `/files/${made.id}`);
  say(`DELETE /files/${made.id} (bob)  ${res.status} ${JSON.stringify(await json(res))}`);
  res = await call(bob, 'GET', '/files');
  say(`GET /files (bob)        ${res.status} ${JSON.stringify(await json(res))}`);
  res = await post(alice, '/query', { query: 'get files select sum(size) where state = "ready"' });
  say(`alice's total, for a quota   ${res.status} ${JSON.stringify(await res.json())}`);

  say('\n-- a name that is not ASCII, and what a download is saved as');
  res = await call(alice, 'POST', '/files', { name: 'résumé "final".pdf', type: 'application/pdf', size: 4 });
  const pdf = await json(res);
  await fetch(pdf.url, { method: 'PUT', headers: pdf.headers, body: '%PDF' });
  await call(alice, 'POST', `/files/${pdf.id}/done`);
  const pdfGot = await json(await call(alice, 'GET', `/files/${pdf.id}`));
  const pdfDownload = await fetch(pdfGot.url);
  say(`Content-Disposition: ${pdfDownload.headers.get('content-disposition')}`);
  say(`Content-Type: ${pdfDownload.headers.get('content-type')}`);
  await pdfDownload.arrayBuffer();

  say('\n-- a token of the provider that says it is the backend');
  const forged = token(dir, { ...shapes.plain('files-backend') });
  res = await post(forged, '/query', { query: 'get files select id, owner' });
  say(`provider token, sub files-backend: read files    ${res.status} ${JSON.stringify(await res.json())}`);
  res = await post(forged, '/query', { query: 'set files {state: "ready"} where state = "pending"' });
  say(`provider token, sub files-backend: mark ready    ${res.status} ${JSON.stringify(await res.json())}`);

  say('\n-- what the rules and the bucket refuse');
  res = await call(alice, 'POST', '/files', { name: 'big.bin', type: 'application/octet-stream', size: 20 * 1024 * 1024 });
  say(`POST /files, 20 MiB     ${res.status} ${JSON.stringify(await json(res))}`);
  res = await post(alice, '/query', { query: 'put files {name: "sneaky.txt", type: "text/plain", size: 5, state: "ready"}' });
  say(`alice's own token, state ready  ${res.status} ${JSON.stringify(await res.json())}`);
  res = await call(alice, 'POST', '/files', { name: 'short.txt', type: 'text/plain', size: 5 });
  const short5 = await json(res);
  const wrong = await fetch(short5.url, { method: 'PUT', headers: short5.headers, body: Buffer.from('more than five bytes') });
  const refusal = (await wrong.text()).match(/<Code>(.*?)<\/Code>/)?.[1];
  say(`PUT 20 bytes to a URL signed for 5   ${wrong.status} ${refusal}`);
  res = await call(alice, 'POST', `/files/${short5.id}/done`);
  say(`POST /files/${short5.id}/done            ${res.status} ${JSON.stringify(await json(res))}`);
  // A store that did not hold the signed length would have kept the 20 bytes:
  // put them there as it would have, and the check on done is what is left.
  await s3.send(new PutObjectCommand({ Bucket: env.BUCKET, Key: keys.pending(short5.id), Body: Buffer.from('more than five bytes') }));
  res = await call(alice, 'POST', `/files/${short5.id}/done`);
  say(`POST /files/${short5.id}/done, 20 bytes there   ${res.status} ${JSON.stringify(await json(res))}`);
  await gone(keys.pending(short5.id));
  say(`HEAD ${keys.pending(short5.id)}: gone with its row`);
  await waitSaid(new RegExp(`row ${short5.id} deleted`));
  flush();

  say('\n-- alice deletes the file: the objects go with the row');
  res = await call(alice, 'DELETE', `/files/${made.id}`);
  say(`DELETE /files/${made.id} (alice) ${res.status}`);
  await gone(keys.live(made.id));
  say(`HEAD ${keys.live(made.id)}: gone`);
  await waitSaid(new RegExp(`row ${made.id} deleted`));
  flush();

  say('\n-- an upload URL that outlives its row');
  res = await call(alice, 'POST', '/files', { name: 'late.txt', type: 'text/plain', size: 5 });
  const late = await json(res);
  await call(alice, 'DELETE', `/files/${late.id}`);
  say(`DELETE /files/${late.id} (alice) before the upload`);
  await waitSaid(new RegExp(`row ${late.id} deleted`)); // the follower has seen the deletion
  flush();
  res = await fetch(late.url, { method: 'PUT', headers: late.headers, body: 'later' });
  say(`PUT <presigned>, after the delete   ${res.status}`);
  say(`HEAD ${keys.pending(late.id)}: ${(await there(keys.pending(late.id))) ? 'there, with no row' : 'gone'}`);
  const swept = await reconcile({ fenec: url, adminToken: 'root', s3, bucket: env.BUCKET });
  say(`reconcile deleted ${swept.join(', ') || 'nothing'}`);

  say('\n-- a pending upload nobody finished is swept');
  await query(url, 'root', 'put files {owner: "alice", name: "abandoned.txt", type: "text/plain", size: 9, state: "pending", created: now() - 7200000}');
  res = await query(url, 'root', 'del files where state = "pending" and created < now() - 3600000');
  say(`del files where state = "pending" and created < now() - 3600000: ${JSON.stringify(await res.json())}`);
  await waitSaid(/row \d+ deleted/);
  flush();

  say('\n-- the follower is stopped, a file is deleted, and the stream moves past what the server keeps');
  const second = await json(await call(alice, 'POST', '/files', { name: 'second.txt', type: 'text/plain', size: 6 }));
  await fetch(second.url, { method: 'PUT', headers: second.headers, body: 'second' });
  await call(alice, 'POST', `/files/${second.id}/done`);
  follower.ctl.abort();
  await follower.done;
  await call(alice, 'DELETE', `/files/${second.id}`);
  for (let i = 0; i < 4; i++) await query(url, 'root', 'put junk {body: $1}', ['x'.repeat(400_000)]);
  // The junk is on disk and the follower's place is outrun: what the follower will be answered.
  await until('the stream to move past the follower', async () =>
    (await fetch(`${url}/_changes?consumer=blobs&wait=0`, { headers: { authorization: 'Bearer root' } })).status === 410);
  say(`${keys.live(second.id)} is ${(await there(keys.live(second.id))) ? 'still in the bucket' : 'gone'}, its row deleted`);
  follower = startFollower();
  await gone(keys.live(second.id));
  say(`${keys.live(second.id)}: gone`);
  await waitSaid(/reconciled/);
  flush();

  follower.ctl.abort();
  await follower.done;
} finally {
  await cleanup();
}
process.exit(0);
