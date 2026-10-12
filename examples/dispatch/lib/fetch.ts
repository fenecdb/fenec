// `fetch` over node:http with a keep-alive agent, for every connection the
// app's server, the simulator and the tests make (`connect(url, { fetch })`).
//
// Why not Node's own: under Node 26.7, ten loops each sending a /batch every
// 50 ms through its fetch saw 60 of 150 requests take about 450 ms -- p50
// 374 ms -- against a plain node:http server as much as against
// fenec-server, where Node 22's fetch took 1.7 ms p50 and none past 100 ms.
// Over node:http the same loops take 0.55 ms p50 and 10 ms p99, so what the
// tests and the bench measure is the database and not the client. A page
// keeps the browser's fetch.
import http from 'node:http';
import { Readable } from 'node:stream';

const agent = new http.Agent({ keepAlive: true, maxSockets: 4096, maxFreeSockets: 256 });

export function nodeFetch(input: string | URL | Request, init: RequestInit = {}): Promise<Response> {
  const url = new URL(typeof input === 'string' || input instanceof URL ? input : input.url);
  const headers: Record<string, string> = {};
  new Headers(init.headers).forEach((v, k) => (headers[k] = v));
  const body = init.body == null ? null : typeof init.body === 'string' ? Buffer.from(init.body) : Buffer.from(init.body as ArrayBuffer);
  if (body) headers['content-length'] = String(body.length);
  const signal = init.signal ?? undefined;
  return new Promise((resolve, reject) => {
    const req = http.request(
      { host: url.hostname, port: url.port || 80, path: url.pathname + url.search, method: init.method ?? 'GET', headers, agent },
      (res) => {
        const h = new Headers();
        for (const [k, v] of Object.entries(res.headers)) {
          if (Array.isArray(v)) v.forEach((x) => h.append(k, x));
          else if (v !== undefined) h.set(k, v);
        }
        const status = res.statusCode ?? 0;
        const empty = status === 204 || status === 304;
        if (empty) res.resume();
        resolve(new Response(empty ? null : (Readable.toWeb(res) as ReadableStream), { status, headers: h }));
      },
    );
    // Listened to for the request's whole life: a connection the router
    // closes as a request goes out on it ends that request with an error
    // after it may have settled, and an error nobody listens for ends the
    // process. Aborted by hand rather than by `http.request`'s own `signal`,
    // which an abort mid-flight left a "socket hang up" with no listener.
    req.on('error', reject);
    if (signal) {
      const abort = () => req.destroy(signal.reason instanceof Error ? signal.reason : new DOMException('aborted', 'AbortError'));
      if (signal.aborted) abort();
      else signal.addEventListener('abort', abort, { once: true });
      req.on('close', () => signal.removeEventListener('abort', abort));
    }
    req.end(body ?? undefined);
  });
}
