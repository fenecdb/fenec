// A function beside the data, docs/functions: a Cloudflare Worker that is an
// endpoint of its own. POST /signup checks what the database's rules cannot
// (an address that looks like one), then writes the signup and the job that
// welcomes it in ONE batch, so a worker never finds a job with no signup nor
// a signup that will never be welcomed. The Worker holds a token of its own
// (a secret: `wrangler secret put FENEC_TOKEN`), never a person's.
//
// The client's Idempotency-Key goes on to fenec-server as it is: a request
// sent again after a timeout is answered what the first was, and writes
// nothing twice.
export default {
  async fetch(request, env) {
    const { pathname } = new URL(request.url);
    if (request.method !== 'POST' || pathname !== '/signup') return new Response('not found', { status: 404 });

    const { email, name } = await request.json().catch(() => ({}));
    if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email ?? '') || !name) {
      return Response.json({ error: 'a name and an email address' }, { status: 400 });
    }

    const lines = [
      { query: 'put signups {email: $1, name: $2}', params: [email, name] },
      // An object goes in as a parameter: inside an object literal only constants do.
      { query: 'put jobs {kind: "welcome", payload: $1, run_at: now(), attempts: 0}', params: [{ email, name }] },
    ];
    const headers = { authorization: `Bearer ${env.FENEC_TOKEN}`, 'content-type': 'application/x-ndjson' };
    const key = request.headers.get('idempotency-key');
    if (key) headers['idempotency-key'] = key;
    const res = await fetch(`${env.FENEC_URL}/batch`, { method: 'POST', headers, body: lines.map((l) => JSON.stringify(l)).join('\n') });

    if (res.status === 409) return Response.json({ error: 'already signed up' }, { status: 409 }); // @unique(email)
    if (!res.ok) return Response.json({ error: 'the database refused', status: res.status }, { status: 502 });
    return Response.json({ ok: true, replayed: res.headers.get('idempotent-replayed') === 'true' }, { status: 201 });
  },
};
