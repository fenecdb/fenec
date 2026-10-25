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
import { backend, keyOf, s3From } from './files-backend.mjs';
import { follow } from './blobs.mjs';
import { atExit, cleanup, dir, here, server, until } from './harness.mjs';
import { init, shapes, token } from './idp.mjs';

const say = (...a) => console.log(...a);
const short = (url) => url.replace(/\?.*$/, '?X-Amz-Algorithm=…&X-Amz-Signature=…');
let container = null;
atExit(() => container && execFileSync('docker', ['rm', '-f', container], { stdio: 'ignore' }));

try {
  // The identity provider's stand-in, and the people it signs in.
  init(dir);
  const as = (sub, claims = {}) => token(dir, { ...shapes.plain(sub), ...claims });
  const alice = as('alice');
  const bob = as('bob');

  // fenec-server: the JWKS, the policy, the change stream the follower reads.
  const url = await server('files', [
    '--http-token', 'root', '--cdc',
    '--jwt-keys', join(dir, 'jwks.json'), '--policy', join(here, 'files-policy.txt'),
  ]);
  const post = (token, path, body) => fetch(`${url}${path}`, {
    method: 'POST', headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  await post('root', '/query', { query: readFileSync(join(here, 'files-schema.fenecql'), 'utf8') });

  // The object store.
  const env = { ...process.env };
  if (!env.S3_ENDPOINT) {
    writeFileSync(join(dir, 's3.json'), JSON.stringify({ identities: [{ name: 'recipes',
      credentials: [{ accessKey: 'recipes-key', secretKey: 'recipes-secret-key' }],
      actions: ['Admin', 'Read', 'Write', 'List', 'Tagging'] }] }));
    container = execFileSync('docker', ['run', '-d', '--rm', '--name', `fenec-recipes-s3-${process.pid}`,
      '-p', '127.0.0.1:0:8333', '-v', `${join(dir, 's3.json')}:/etc/seaweedfs/s3.json:ro`,
      'chrislusf/seaweedfs', 'server', '-s3', '-s3.config=/etc/seaweedfs/s3.json', '-dir=/data'],
      { encoding: 'utf8' }).trim().slice(0, 12);
    const port = execFileSync('docker', ['port', container, '8333/tcp'], { encoding: 'utf8' }).trim().split(':').pop();
    Object.assign(env, { S3_ENDPOINT: `http://127.0.0.1:${port}`, S3_REGION: 'us-east-1', S3_KEY: 'recipes-key',
      S3_SECRET: 'recipes-secret-key', S3_PATH_STYLE: '1', BUCKET: 'recipes' });
    await until('the object store', async () => (await fetch(env.S3_ENDPOINT)).status === 403, 60_000);
  }
  const s3 = s3From(env);
  await until('the bucket', () => s3.send(new CreateBucketCommand({ Bucket: env.BUCKET })).then(() => true,
    (e) => e.name === 'BucketAlreadyOwnedByYou'), 30_000);
  say(`fenec-server ${url}, bucket ${env.BUCKET} at ${env.S3_ENDPOINT}`);

  // The backend: a token of its own, role app, for marking a row ready.
  const app = backend({ fenec: url, appToken: as('files-backend', { role: 'app' }), s3, bucket: env.BUCKET });
  await new Promise((r) => app.listen(0, '127.0.0.1', r));
  const api = `http://127.0.0.1:${app.address().port}`;
  const call = (token, method, path, body) => fetch(`${api}${path}`, {
    method, headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: body && JSON.stringify(body),
  });
  const json = async (res) => (res.status === 204 ? '' : await res.json());

  // The bucket's follower, with an end of its own.
  const stop = new AbortController();
  const following = follow({ fenec: url, adminToken: 'root', s3, bucket: env.BUCKET, signal: stop.signal, wait: 1000 });

  say('\n-- alice adds a file');
  const bytes = Buffer.from('hello from alice\n');
  let res = await call(alice, 'POST', '/files', { name: 'hello.txt', type: 'text/plain', size: bytes.length });
  const made = await json(res);
  say(`POST /files             ${res.status} {"id":${made.id},"url":"${short(made.url)}"}`);
  const rows = await (await fetch(`${url}/query`, { method: 'POST', headers: { authorization: 'Bearer root', 'content-type': 'application/json' },
    body: JSON.stringify({ query: 'get files select id, owner, name, type, size, state' }) })).json();
  say('the row the server holds:', JSON.stringify(rows));

  say('\n-- she sends the bytes to the bucket herself, with the URL');
  res = await fetch(made.url, { method: 'PUT', headers: made.headers, body: bytes });
  say(`PUT <presigned>         ${res.status}`);
  res = await call(alice, 'GET', `/files/${made.id}`);
  say(`GET /files/${made.id} before done ${res.status} ${JSON.stringify(await json(res))}`);
  res = await call(alice, 'POST', `/files/${made.id}/done`);
  say(`POST /files/${made.id}/done    ${res.status} ${JSON.stringify(await json(res))}`);

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
  await s3.send(new PutObjectCommand({ Bucket: env.BUCKET, Key: keyOf(short5.id), Body: Buffer.from('more than five bytes') }));
  res = await call(alice, 'POST', `/files/${short5.id}/done`);
  say(`POST /files/${short5.id}/done, 20 bytes there   ${res.status} ${JSON.stringify(await json(res))}`);
  await until('the object to go', () => s3.send(new HeadObjectCommand({ Bucket: env.BUCKET, Key: keyOf(short5.id) })).then(() => false, (e) => e.$metadata?.httpStatusCode === 404));
  say(`HEAD ${keyOf(short5.id)}: 404, the row having gone`);

  say('\n-- alice deletes the file: the object goes with the row');
  await s3.send(new HeadObjectCommand({ Bucket: env.BUCKET, Key: keyOf(made.id) }));
  res = await call(alice, 'DELETE', `/files/${made.id}`);
  say(`DELETE /files/${made.id} (alice) ${res.status}`);
  await until('the object to go', () => s3.send(new HeadObjectCommand({ Bucket: env.BUCKET, Key: keyOf(made.id) })).then(() => false, (e) => e.$metadata?.httpStatusCode === 404));
  say(`HEAD ${keyOf(made.id)}: 404`);

  say('\n-- a pending upload nobody finished is swept');
  await fetch(`${url}/query`, { method: 'POST', headers: { authorization: 'Bearer root', 'content-type': 'application/json' },
    body: JSON.stringify({ query: 'put files {owner: "alice", name: "abandoned.txt", type: "text/plain", size: 9, state: "pending", created: now() - 7200000}' }) });
  res = await fetch(`${url}/query`, { method: 'POST', headers: { authorization: 'Bearer root', 'content-type': 'application/json' },
    body: JSON.stringify({ query: 'del files where state = "pending" and created < now() - 3600000' }) });
  say(`del files where state = "pending" and created < now() - 3600000: ${JSON.stringify(await res.json())}`);

  stop.abort();
  await following;
} finally {
  await cleanup();
}
process.exit(0);
