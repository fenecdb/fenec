// Files in an object store (S3, R2 or anything that speaks S3), their metadata
// in fenecdb: the app's small backend, docs/files. It holds no JWT library and
// checks no token itself. Each request's `Authorization` goes to fenec-server
// as it came, and the server's rules (files-policy.txt) say whether this
// person may add a file or read one; a presigned URL is made only after the
// server has said yes -- to an insert for an upload, to a read for a download.
//
//   POST   /files          {name, type, size}  a row, pending, and a URL to PUT the bytes to
//   POST   /files/:id/done                     the bytes are there: the row is ready
//   GET    /files                              the person's files
//   GET    /files/:id                          a URL to GET the bytes from
//   DELETE /files/:id                          the row; the bucket's copy goes with it (blobs.mjs)
//
// An upload lands under `pending/<id>` and, once checked, is copied to
// `files/<id>`; both keys are made from the row's id, never from the name a
// person gave the file (store.mjs). The bucket stays private: nothing here
// makes a key public, and a key is no secret either (ids count up).
import { createServer } from 'node:http';
import { CopyObjectCommand, DeleteObjectCommand, GetObjectCommand, HeadObjectCommand, PutObjectCommand } from '@aws-sdk/client-s3';
import { getSignedUrl } from '@aws-sdk/s3-request-presigner';
import { connect } from '@fenecdb/web/client';
import { keysOf } from './store.mjs';
import { isMain } from './main.mjs';

const MAX_BODY = 4096; // a request is a name, a type and a number
const TYPE = /^[a-z0-9][a-z0-9!#$&^_.+-]{0,63}\/[a-z0-9][a-z0-9!#$&^_.+-]{0,63}$/i;

/**
 * What a download is saved as. `attachment` makes a browser save the bytes
 * instead of showing them: a person fills these files and names them, and an
 * `.html` or `.svg` shown from the bucket's address runs its script there.
 * `filename*` carries a name that is not ASCII (RFC 6266, RFC 5987); the
 * plain `filename` beside it is what an old client reads.
 */
export function disposition(name) {
  const ascii = name.replace(/[^\x20-\x7e]|["\\%]/g, '_');
  const star = encodeURIComponent(name).replace(/['()*]/g, (c) => `%${c.charCodeAt(0).toString(16).toUpperCase()}`);
  return `attachment; filename="${ascii}"; filename*=UTF-8''${star}`;
}

/**
 * `appToken` is `() => Promise<token>`: the backend's own, signed on demand
 * (service-token.mjs). `prefix` is the tenant on a `--dir` node.
 */
export function backend({ fenec, appToken, s3, bucket, prefix = '', expiresIn = 300, log = console.error }) {
  const as = (token) => connect(fenec, { token });
  const keys = keysOf(prefix);

  return createServer(async (req, res) => {
    const reply = (status, body) => {
      res.writeHead(status, { 'content-type': 'application/json' });
      res.end(body === undefined ? undefined : JSON.stringify(body));
    };
    try {
      const token = (req.headers.authorization ?? '').replace(/^Bearer /, '');
      if (!token) return reply(401, { error: 'no token' });
      const me = as(token); // fenec-server verifies it, and holds every query to its rules
      const [, route, rawId, verb] = new URL(req.url, 'http://x').pathname.split('/');
      if (route !== 'files') return reply(404, { error: 'not found' });
      const id = rawId === undefined ? undefined : Number(rawId);
      if (rawId !== undefined && !Number.isSafeInteger(id)) return reply(404, { error: 'no such file' });
      const body = req.method === 'POST' ? await json(req) : {};

      if (req.method === 'POST' && id === undefined) {
        const { name, type, size } = body;
        if (typeof name !== 'string' || !name || name.length > 255 || typeof type !== 'string' || !TYPE.test(type)
          || !Number.isSafeInteger(size) || size < 1) {
          return reply(400, { error: 'a name, a type and a size' });
        }
        // The row first: if the rules refuse this person, no URL is made.
        const [file] = await me.from('files').insert(
          { name, type, size, state: 'pending', created: new Date() },
          { returning: true },
        );
        const url = await getSignedUrl(
          s3,
          new PutObjectCommand({ Bucket: bucket, Key: keys.pending(file.id), ContentType: type, ContentLength: size }),
          { expiresIn, signableHeaders: new Set(['content-type', 'content-length']) },
        );
        return reply(201, { id: file.id, url, headers: { 'content-type': type } });
      }

      if (req.method === 'POST' && verb === 'done') {
        const file = await me.from('files').where('id', id).first(); // a row this person may read
        if (!file) return reply(404, { error: 'no such file' });
        let held;
        try {
          held = await s3.send(new HeadObjectCommand({ Bucket: bucket, Key: keys.pending(file.id) }));
        } catch {
          return reply(409, { error: 'the bytes are not in the bucket yet' });
        }
        if (held.ContentLength !== file.size) {
          await me.from('files').where('id', file.id).delete(); // blobs.mjs removes the object
          return reply(422, { error: `the object is ${held.ContentLength} bytes, not the ${file.size} announced` });
        }
        // A copy to a key the client never held a URL for, so that bytes sent
        // to the upload URL after this point do not reach what is downloaded.
        // `CopySourceIfMatch` makes it that object, the one measured, and not
        // one that replaced it between the two calls.
        await s3.send(new CopyObjectCommand({
          Bucket: bucket, Key: keys.live(file.id), CopySource: `${bucket}/${keys.pending(file.id)}`,
          CopySourceIfMatch: held.ETag,
        }));
        const app = as(await appToken());
        await app.from('files').where('id', file.id).update({ state: 'ready' }, { require: 1 });
        await s3.send(new DeleteObjectCommand({ Bucket: bucket, Key: keys.pending(file.id) })).catch(() => {});
        return reply(200, { id: file.id, state: 'ready' });
      }

      if (req.method === 'GET' && id === undefined) {
        const rows = await me.from('files').select('id', 'name', 'type', 'size', 'state')
          .order('created', 'desc').limit(100).rows();
        return reply(200, rows);
      }

      if (req.method === 'GET') {
        const file = await me.from('files').where('id', id).first();
        if (!file) return reply(404, { error: 'no such file' }); // not found, whether it is another's or nobody's
        if (file.state !== 'ready') return reply(409, { error: 'not uploaded yet' });
        const url = await getSignedUrl(
          s3,
          new GetObjectCommand({
            Bucket: bucket,
            Key: keys.live(file.id),
            ResponseContentType: file.type,
            ResponseContentDisposition: disposition(file.name),
          }),
          { expiresIn },
        );
        return reply(200, { url, name: file.name, type: file.type, size: file.size });
      }

      if (req.method === 'DELETE') {
        await me.from('files').where('id', id).delete({ require: 1 });
        return reply(204);
      }
      return reply(405, { error: 'not a route' });
    } catch (e) {
      // The server's refusals of this person are theirs to be told, and
      // nothing else is: its words name collections and rules.
      if (e.status === 401) return reply(401, { error: 'not signed in' });
      if (e.status === 403) return reply(403, { error: 'not allowed' });
      if (e.status === 412) return reply(404, { error: 'no such file' }); // `require 1`: no row of this person's
      if (e.status === 413) return reply(413, { error: 'too large' });
      log('files backend:', e);
      return reply(500, { error: 'internal error' });
    }
  });
}

/** A request's JSON body, at most MAX_BODY bytes: a longer one is refused as soon as it is. */
async function json(req) {
  let s = '';
  for await (const chunk of req) {
    s += chunk;
    if (s.length > MAX_BODY) throw Object.assign(new Error('body'), { status: 413 });
  }
  try {
    return s ? JSON.parse(s) : {};
  } catch {
    return {};
  }
}

if (isMain(import.meta.url)) {
  const { readFileSync } = await import('node:fs');
  const { s3From } = await import('./store.mjs');
  const { serviceTokens } = await import('./service-token.mjs');
  const env = process.env;
  const server = backend({
    fenec: env.FENEC_URL,
    appToken: serviceTokens({ pem: readFileSync(env.SERVICE_KEY), sub: 'files-backend' }),
    s3: s3From(env),
    bucket: env.BUCKET,
    prefix: env.KEY_PREFIX ?? '',
  });
  server.listen(Number(env.PORT ?? 0), '127.0.0.1', () =>
    console.log(`files backend on http://127.0.0.1:${server.address().port}`));
}
