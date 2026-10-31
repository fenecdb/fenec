// The object store, as the files recipe reaches it: one client, and the keys.
import { S3Client } from '@aws-sdk/client-s3';

export function s3From(env) {
  return new S3Client({
    region: env.S3_REGION ?? 'auto', // R2 wants "auto"; S3 proper, the bucket's region
    endpoint: env.S3_ENDPOINT, // https://<account>.r2.cloudflarestorage.com for R2
    forcePathStyle: env.S3_PATH_STYLE === '1', // a local server has no bucket.localhost
    credentials: { accessKeyId: env.S3_KEY, secretAccessKey: env.S3_SECRET },
    // Recent versions of the SDK add a CRC32 of the body to a request they
    // sign. A presigned PUT is signed before there is a body, so the checksum
    // is the empty body's, and the store answers the real bytes 400 BadDigest
    // (seen on SeaweedFS). The SDK's own option, `WHEN_REQUIRED`, leaves it
    // off; Cloudflare's R2 page for this SDK does not mention it.
    requestChecksumCalculation: 'WHEN_REQUIRED',
    responseChecksumValidation: 'WHEN_REQUIRED',
  });
}

/**
 * Where a file's bytes are. An upload lands under `pending/<id>`, where a
 * lifecycle rule can expire what is left; once the backend has checked it, it
 * is copied to `files/<id>`, a key the client never held a URL for. `prefix`
 * is the tenant on a `--dir` node (`acme/`): every tenant's file numbers its
 * rows from 1, and one bucket holds them all.
 */
export const keysOf = (prefix = '') => ({
  pending: (id) => `${prefix}pending/${id}`,
  live: (id) => `${prefix}files/${id}`,
  /** The row id and the kind of a key of this prefix, or null. */
  parse(key) {
    const m = key.startsWith(prefix) && /^(pending|files)\/(\d+)$/.exec(key.slice(prefix.length));
    return m ? { kind: m[1], id: Number(m[2]) } : null;
  },
});
