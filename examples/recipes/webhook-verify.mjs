// What a receiver of webhook.mjs's deliveries does, docs/functions. Three
// checks, in this order, before the body is trusted:
//
//   1. The signature: HMAC-SHA-256 over `<timestamp>.<body>` with the secret
//      the two share, compared in constant time. The timestamp is part of
//      what is signed, so it cannot be moved to make an old body new.
//   2. The timestamp: within a few minutes of now. A body someone captured and
//      sends again an hour later has a good signature and a stale time.
//   3. The `seq` inside the body, which the signature covers: a change already
//      done is answered 200 and not done again. A header outside the signature
//      (an Idempotency-Key, say) is whatever the sender of this request wrote.
//
// A secret is rotated by the sender signing with the new and the old (two
// `v1=` values) while the receiver learns the new: it accepts a body one of
// whose signatures one of its secrets makes.
import { createHmac, timingSafeEqual } from 'node:crypto';

export const sign = (secret, timestamp, body) => createHmac('sha256', secret).update(`${timestamp}.${body}`).digest('hex');

/** The header a sender sets: one `v1=` for each secret it signs with. */
export const signatures = (secrets, timestamp, body) => secrets.map((s) => `v1=${sign(s, timestamp, body)}`).join(',');

const same = (a, b) => a.length === b.length && timingSafeEqual(Buffer.from(a), Buffer.from(b));

/** `{ok: true}` or `{ok: false, why}`; `now` is seconds, a parameter so that a test can move it. */
export function verify({ secrets, headers, body, tolerance = 300, now = () => Date.now() / 1000 }) {
  const timestamp = headers['x-webhook-timestamp'];
  if (!/^\d{1,12}$/.test(timestamp ?? '')) return { ok: false, why: 'no timestamp' };
  const given = (headers['x-webhook-signature'] ?? '').split(',').map((p) => p.trim().replace(/^v1=/, ''));
  const good = secrets.some((secret) => given.some((g) => same(g, sign(secret, timestamp, body))));
  if (!good) return { ok: false, why: 'bad signature' };
  if (Math.abs(now() - Number(timestamp)) > tolerance) return { ok: false, why: 'stale' };
  return { ok: true };
}

/**
 * A request handler for node:http. `secrets` is a list, or a function that
 * answers one (read at each delivery, so that a secret can be rotated). `done`
 * is the set of `seq`s already done
 * (a table, in a receiver that is run twice), `act(change)` what the receiver
 * is for.
 */
export function receiver({ secrets, act, done = new Set(), tolerance, now, log = () => {}, captured }) {
  return async (req, res) => {
    let body = '';
    for await (const chunk of req) {
      body += chunk;
      if (body.length > 1 << 20) return void res.writeHead(413).end();
    }
    const checked = verify({ secrets: typeof secrets === 'function' ? secrets() : secrets, headers: req.headers, body, tolerance, now });
    if (!checked.ok) {
      log(`rejected: ${checked.why}`);
      return void res.writeHead(401).end();
    }
    captured?.push({ headers: req.headers, body }); // for a demonstration of a replay
    const change = JSON.parse(body);
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
  };
}
