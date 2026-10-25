// A function beside the data, docs/functions: a Cloudflare Worker that is an
// endpoint of its own. POST /signup checks what the database's rules cannot
// (an address that looks like one), then writes the signup and the job that
// welcomes it in ONE batch, so a worker never finds a job with no signup nor
// a signup that will never be welcomed.
//
// The Worker is a service of its own to fenec-server: a key (the secret
// SERVICE_KEY, PKCS#8 in base64) whose public half is in the server's keys
// file, and a token signed with it for five minutes, `sub` signup-function.
// functions-policy.txt lets that sub insert into two collections and nothing
// else, so a Worker that leaked its secret could add rows, not read them. Not a
// person's token, and not the server's own.
//
// The client's Idempotency-Key goes on to fenec-server, with the caller's name
// in front of it. The server keeps a key under this token's subject, which is
// every caller's alike: left as it came, one caller's key could collide with
// another's (a 422, or a replayed answer that was not theirs).
const b64url = (bytes) => btoa(String.fromCharCode(...bytes)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
const text = (s) => new TextEncoder().encode(s);

let kept = null; // the token, per isolate, until a minute before it lapses

async function serviceToken(env) {
  const now = Math.floor(Date.now() / 1000);
  if (kept && kept.exp - 60 > now) return kept.token;
  const key = await crypto.subtle.importKey('pkcs8', Uint8Array.from(atob(env.SERVICE_KEY), (c) => c.charCodeAt(0)),
    { name: 'RSASSA-PKCS1-v1_5', hash: 'SHA-256' }, false, ['sign']);
  const head = b64url(text(JSON.stringify({ alg: 'RS256', typ: 'JWT', kid: 'svc-1' })));
  const body = b64url(text(JSON.stringify({ sub: 'signup-function', iat: now, exp: now + 300 })));
  const sig = new Uint8Array(await crypto.subtle.sign('RSASSA-PKCS1-v1_5', key, text(`${head}.${body}`)));
  kept = { token: `${head}.${body}.${b64url(sig)}`, exp: now + 300 };
  return kept.token;
}

export default {
  async fetch(request, env) {
    const { pathname } = new URL(request.url);
    if (request.method !== 'POST' || pathname !== '/signup') return new Response('not found', { status: 404 });

    const { email, name } = await request.json().catch(() => ({}));
    if (typeof email !== 'string' || email.length > 254 || !/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email)
      || typeof name !== 'string' || !name || name.length > 100) {
      return Response.json({ error: 'a name and an email address' }, { status: 400 });
    }

    const lines = [
      { query: 'put signups {email: $1, name: $2}', params: [email, name] },
      // An object goes in as a parameter: inside an object literal only constants do.
      { query: 'put jobs {kind: "welcome", payload: $1, run_at: now(), attempts: 0}', params: [{ email, name }] },
    ];
    const headers = { authorization: `Bearer ${await serviceToken(env)}`, 'content-type': 'application/x-ndjson' };
    const key = request.headers.get('idempotency-key');
    if (key) {
      // The caller: who signed in, where there is a sign-in; the address otherwise.
      const caller = request.headers.get('cf-connecting-ip') ?? 'unknown';
      headers['idempotency-key'] = `${caller}:${key.slice(0, 100)}`;
    }
    const res = await fetch(`${env.FENEC_URL}/batch`, { method: 'POST', headers, body: lines.map((l) => JSON.stringify(l)).join('\n') });

    if (res.status === 409) return Response.json({ error: 'already signed up' }, { status: 409 }); // @unique(email)
    if (!res.ok) return Response.json({ error: 'try again later' }, { status: 502 }); // not the server's words
    return Response.json({ ok: true, replayed: res.headers.get('idempotent-replayed') === 'true' }, { status: 201 });
  },
};
