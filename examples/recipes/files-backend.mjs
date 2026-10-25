// Files in an object store (S3, R2 or anything that speaks S3), their metadata
// in fenecdb: the app's small backend, docs/files. It holds no JWT library and
// checks no token itself. Each request's `Authorization` goes to fenec-server
// as it came, and the server's rules (files-policy.txt) say whether this
// person may add a file, see it or delete it; a presigned URL is made only
// after the server has said yes.
//
//   POST   /files          {name, type, size}  a row, pending, and a URL to PUT the bytes to
//   POST   /files/:id/done                     the bytes are there: the row is ready
//   GET    /files                              the person's files
//   GET    /files/:id                          a URL to GET the bytes from
//   DELETE /files/:id                          the row; the bucket's copy goes with it (blobs.mjs)
//
// The object's key is `files/<id>`, never the name a person gave it: the
// stream of changes names a deleted row by its id alone.
import { createServer } from 'node:http';
import { GetObjectCommand, HeadObjectCommand, PutObjectCommand, S3Client } from '@aws-sdk/client-s3';
import { getSignedUrl } from '@aws-sdk/s3-request-presigner';
import { connect } from '@fenecdb/web/client';

export const keyOf = (id) => `files/${id}`;

export function s3From(env) {
  return new S3Client({
    region: env.S3_REGION ?? 'auto', // R2 wants "auto"; S3 proper, the bucket's region
    endpoint: env.S3_ENDPOINT, // https://<account>.r2.cloudflarestorage.com for R2
    forcePathStyle: env.S3_PATH_STYLE === '1', // a local server has no bucket.localhost
    credentials: { accessKeyId: env.S3_KEY, secretAccessKey: env.S3_SECRET },
    // Since 3.729 the SDK adds a CRC32 of the body to a request it signs. A
    // presigned PUT is signed before there is a body, so the checksum is the
    // empty body's, and the bucket answers 400 BadDigest to the bytes that
    // follow (seen on SeaweedFS; R2's docs ask for these two settings too).
    requestChecksumCalculation: 'WHEN_REQUIRED',
    responseChecksumValidation: 'WHEN_REQUIRED',
  });
}

export function backend({ fenec, appToken, s3, bucket, expiresIn = 300 }) {
  const as = (token) => connect(fenec, { token });
  const app = as(appToken);

  const bearer = (req) => (req.headers.authorization ?? '').replace(/^Bearer /, '');

  return createServer(async (req, res) => {
    const reply = (status, body) => {
      res.writeHead(status, { 'content-type': 'application/json' });
      res.end(body === undefined ? undefined : JSON.stringify(body));
    };
    try {
      const token = bearer(req);
      if (!token) return reply(401, { error: 'no token' });
      const me = as(token); // fenec-server verifies it, and holds every query to its rules
      const [, route, id, verb] = new URL(req.url, 'http://x').pathname.split('/');
      if (route !== 'files') return reply(404, { error: 'not found' });
      const body = ['POST', 'PUT'].includes(req.method) ? JSON.parse((await text(req)) || '{}') : {};

      if (req.method === 'POST' && !id) {
        const { name, type, size } = body;
        // The row first: if the rules refuse this person, no URL is made.
        const [file] = await me.from('files').insert(
          { name, type, size, state: 'pending', created: new Date() },
          { returning: true },
        );
        const url = await getSignedUrl(
          s3,
          new PutObjectCommand({ Bucket: bucket, Key: keyOf(file.id), ContentType: type, ContentLength: size }),
          { expiresIn, signableHeaders: new Set(['content-type', 'content-length']) },
        );
        return reply(201, { id: file.id, url, headers: { 'content-type': type } });
      }

      if (req.method === 'POST' && verb === 'done') {
        const file = await me.from('files').where('id', Number(id)).first(); // a row this person may read
        if (!file) return reply(404, { error: 'no such file' });
        let held;
        try {
          held = await s3.send(new HeadObjectCommand({ Bucket: bucket, Key: keyOf(file.id) }));
        } catch {
          return reply(409, { error: 'the bytes are not in the bucket yet' });
        }
        if (held.ContentLength !== file.size) {
          await me.from('files').where('id', file.id).delete(); // blobs.mjs removes the object
          return reply(422, { error: `the object is ${held.ContentLength} bytes, not the ${file.size} announced` });
        }
        await app.from('files').where('id', file.id).update({ state: 'ready' }, { require: 1 });
        return reply(200, { id: file.id, state: 'ready' });
      }

      if (req.method === 'GET' && !id) {
        const rows = await me.from('files').select('id', 'name', 'type', 'size', 'state')
          .order('created', 'desc').limit(100).rows();
        return reply(200, rows);
      }

      if (req.method === 'GET') {
        const file = await me.from('files').where('id', Number(id)).first();
        if (!file) return reply(404, { error: 'no such file' }); // not found, whether it is another's or nobody's
        if (file.state !== 'ready') return reply(409, { error: 'not uploaded yet' });
        const url = await getSignedUrl(
          s3,
          new GetObjectCommand({
            Bucket: bucket,
            Key: keyOf(file.id),
            ResponseContentType: file.type,
            ResponseContentDisposition: `attachment; filename="${encodeURIComponent(file.name)}"`,
          }),
          { expiresIn },
        );
        return reply(200, { url, name: file.name, type: file.type, size: file.size });
      }

      if (req.method === 'DELETE') {
        await me.from('files').where('id', Number(id)).delete({ require: 1 });
        return reply(204);
      }
      return reply(405, { error: 'not a route' });
    } catch (e) {
      // 412: `require 1` found no row of this person's. The server's own
      // refusals (401, 403) are passed on as they came.
      if (e.status === 412) return reply(404, { error: 'no such file' });
      return reply(e.status ?? 500, { error: e.message });
    }
  });
}

const text = async (req) => {
  let s = '';
  for await (const chunk of req) s += chunk;
  return s;
};

if (import.meta.url === `file://${process.argv[1]}`) {
  const env = process.env;
  const server = backend({
    fenec: env.FENEC_URL,
    appToken: env.APP_TOKEN,
    s3: s3From(env),
    bucket: env.BUCKET,
  });
  server.listen(Number(env.PORT ?? 0), '127.0.0.1', () =>
    console.log(`files backend on http://127.0.0.1:${server.address().port}`));
}
