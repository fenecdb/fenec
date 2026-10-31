// What a receiver of webhook.mjs's deliveries does, docs/functions. Three
// checks, in this order, before the body is trusted:
//
//   1. The signature: HMAC-SHA-256 over `<timestamp>.<body>` with the secret
//      the two share, over the body's bytes as they arrived, compared in
//      constant time. The timestamp is part of what is signed, so it cannot be
//      moved to make an old body new.
//   2. The timestamp: within a few minutes of now. A body someone captured and
//      sends again an hour later has a good signature and a stale time.
//   3. The `seq` inside the body, which the signature covers: a change already
//      done is answered 200 and not done again. A header outside the signature
//      (an Idempotency-Key, say) is whatever the sender of this request wrote.
//
// A secret is rotated by the sender signing with the new and the old (two
// `v1=` values) while the receiver learns the new: it accepts a body one of
// whose signatures one of its secrets makes.
//
// Everything the sender controls is checked for what it is before it is
// compared: a signature is 64 hex digits and is compared as the 32 bytes they
// are (a header of accents the same length in characters is not 64 digits, and
// `timingSafeEqual` throws on buffers of two lengths), and the body is bytes,
// never a string put together from chunks (a character of three bytes split
// across two chunks is two broken ones, and a body that was signed whole no
// longer verifies).
import { createHmac, timingSafeEqual } from 'node:crypto';

/** The signature of `body` (a string or the bytes received) at `timestamp`, as 32 bytes. */
const mac = (secret, timestamp, body) => createHmac('sha256', secret).update(`${timestamp}.`).update(body).digest();

export const sign = (secret, timestamp, body) => mac(secret, timestamp, body).toString('hex');

/** The header a sender sets: one `v1=` for each secret it signs with. */
export const signatures = (secrets, timestamp, body) => secrets.map((s) => `v1=${sign(s, timestamp, body)}`).join(',');

const V1 = /^v1=([0-9a-f]{64})$/;

/** `{ok: true}` or `{ok: false, why}`; `body` is the bytes received, `now` seconds, a parameter so that a test can move it. */
export function verify({ secrets, headers, body, tolerance = 300, now = () => Date.now() / 1000 }) {
  const timestamp = headers['x-webhook-timestamp'];
  if (!/^\d{1,12}$/.test(timestamp ?? '')) return { ok: false, why: 'no timestamp' };
  const given = (headers['x-webhook-signature'] ?? '').split(',').map((p) => V1.exec(p.trim())?.[1]).filter(Boolean)
    .map((hex) => Buffer.from(hex, 'hex'));
  const good = secrets.some((secret) => {
    const want = mac(secret, timestamp, body);
    return given.some((g) => timingSafeEqual(g, want)); // 32 bytes each
  });
  if (!good) return { ok: false, why: 'bad signature' };
  if (Math.abs(now() - Number(timestamp)) > tolerance) return { ok: false, why: 'stale' };
  return { ok: true };
}

/**
 * A request handler for node:http. `secrets` is a list, or a function that
 * answers one (read at each delivery, so that a secret can be rotated). `done`
 * is the set of `seq`s already done: a Set here, to be short, and in a receiver
 * that is restarted or run twice a table keyed by the sender and the `seq`,
 * kept as long as the tolerance (an older delivery is stale and refused before
 * it is looked up). `act(change)` is what the receiver is for.
 */
export function receiver({ secrets, act, done = new Set(), tolerance, now, log = () => {}, captured }) {
  const MAX = 1 << 20;
  return async (req, res) => {
    try {
      const chunks = [];
      let size = 0;
      for await (const chunk of req) {
        size += chunk.length;
        if (size > MAX) return void res.writeHead(413).end();
        chunks.push(chunk);
      }
      const body = Buffer.concat(chunks); // bytes: verified as they came, parsed after
      const checked = verify({ secrets: typeof secrets === 'function' ? secrets() : secrets, headers: req.headers, body, tolerance, now });
      if (!checked.ok) {
        log(`rejected: ${checked.why}`);
        return void res.writeHead(401).end();
      }
      captured?.push({ headers: req.headers, body }); // for a demonstration of a replay
      let change;
      try {
        change = JSON.parse(body.toString('utf8'));
      } catch {
        return void res.writeHead(400).end(); // signed by the sender, and not JSON: the sender's to look at
      }
      if (done.has(change.seq)) {
        log(`seq ${change.seq} seen: nothing done`);
        return void res.writeHead(200).end();
      }
      try {
        await act(change);
      } catch (e) {
        log(`seq ${change.seq}: answered ${e.status ?? 500}`);
        return void res.writeHead(e.status ?? 500).end();
      }
      done.add(change.seq);
      log(`seq ${change.seq} ${change.collection} ${change.op}: done`);
      res.writeHead(200).end();
    } catch (e) {
      // Whatever else a request can do, it answers: nothing a client sends ends the process.
      log(`failed: ${e.message}`);
      if (!res.headersSent) res.writeHead(400);
      res.end();
    }
  };
}
