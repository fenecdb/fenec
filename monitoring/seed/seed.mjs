// The data the "fenecdb data" dashboard (grafana/fenecdb-data.json) reads:
// thirty days of a product site's events, made by examples/analytics'
// traffic simulator, and the trades of twenty invented symbols since
// midnight UTC a week ago. docker-compose.yml runs it once fenec-server is
// up and before Grafana starts; a database that already holds the
// collections keeps them.
//
// It also mints the token Grafana's data source reads with: an HS256 JWT
// with the role `grafana`, which policy.txt lets read `events` and `ticks`
// and nothing else. It is written to a file only Grafana mounts, and
// provisioning puts it into the data source's secure settings -- encrypted
// in Grafana's database, never in a dashboard.
//
// Node strips sim.ts's types as it imports it (22.18 and later), so there
// is nothing to build or install.
//
//   FENEC_URL            http://127.0.0.1:8080
//   FENEC_HTTP_TOKEN     the server's token: makes the collections, writes
//   FENEC_JWT_SECRET     the secret fenec-server checks JWTs with, or
//   FENEC_JWT_SECRET_FILE  the file holding it
//   GRAFANA_TOKEN_FILE   where the data source's token goes (none: printed)
import { createHmac } from 'node:crypto';
import { chownSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import { DAY_MS, rng, Traffic } from '../../examples/analytics/src/sim.ts';

const BASE = (process.env.FENEC_URL ?? 'http://127.0.0.1:8080').replace(/\/$/, '');
const TOKEN = need('FENEC_HTTP_TOKEN');
// As fenec-server reads --jwt-secret-file: the file less its line ending.
const SECRET = process.env.FENEC_JWT_SECRET_FILE
  ? readFileSync(process.env.FENEC_JWT_SECRET_FILE, 'utf8').replace(/[\r\n]+$/, '')
  : need('FENEC_JWT_SECRET');
if (Buffer.byteLength(SECRET) < 32) throw new Error('FENEC_JWT_SECRET: fenec-server takes a secret of 32 bytes or more');
// Grafana's image runs as this user; the file is its alone.
const GRAFANA_UID = 472;

function need(name) {
  const v = process.env[name];
  if (!v) throw new Error(`set ${name}`);
  return v;
}

async function query(text, params = []) {
  const res = await fetch(`${BASE}/query`, {
    method: 'POST',
    headers: { authorization: `Bearer ${TOKEN}`, 'content-type': 'application/json' },
    body: JSON.stringify({ query: text, params }),
  });
  const body = await res.json();
  if (!res.ok) throw new Error(`${text.slice(0, 60)}: ${res.status} ${body.error}`);
  return body;
}

// The server reads its file before it listens, and compose starts this as
// soon as the server's container runs.
for (let i = 0; ; i++) {
  try {
    if ((await fetch(`${BASE}/_health`)).ok) break;
  } catch {
    // not listening yet
  }
  if (i === 240) throw new Error(`${BASE} did not answer /_health in a minute`);
  await new Promise((r) => setTimeout(r, 250));
}

const now = Date.now();
const today = now - (now % DAY_MS);
const have = new Set((await query('collections')).map((c) => c.name));

// A page of documents a statement, the page a parameter: the text is the
// same whatever it holds, and parsed once.
async function load(collection, docs) {
  for (let i = 0; i < docs.length; i += 5000) await query(`put ${collection} $1`, [docs.slice(i, i + 5000)]);
}

if (!have.has('events')) {
  const t0 = performance.now();
  // `at` is @sorted, so a time range reads only its rows, and a count by
  // `bucket(at, ...)` over a range reads the index alone.
  await query(
    'create collection events (name text, user text, path text, ref text, country text, device text, browser text, at timestamp @sorted, props json)',
  );
  const traffic = new Traffic({ daily: 700, growth: 0.01, seed: 7, prefix: 'v' });
  const events = [];
  for (let d = 30; d >= 0; d--) {
    for (const e of traffic.day(today - d * DAY_MS, 30 - d)) {
      if (e.at > now) break;
      // The simulator's times have fractions of a millisecond, which a
      // timestamp field refuses rather than round.
      const at = Math.floor(e.at);
      events.push({ name: e.name, user: e.user, path: e.path, ref: e.ref, country: e.country, device: e.device, browser: e.browser, at, props: e.props });
    }
  }
  await load('events', events);
  console.log(`events: ${events.length} rows in ${Math.round(performance.now() - t0)} ms`);
}

if (!have.has('ticks')) {
  const t0 = performance.now();
  await query('create collection ticks (sym text @hash, at timestamp @sorted, px float, qty int)');
  // A random walk a symbol (geometric Brownian motion over the time since
  // its last trade, made three times as lively as its volatility), busier
  // symbols trading more often, a trade's size heavy-tailed.
  const r = rng(42);
  const symbols = Array.from({ length: 20 }, (_, i) => ({
    sym: `S${String(i + 1).padStart(2, '0')}`,
    px: Math.round(Math.exp(2 + r() * 4) * 100) / 100,
    vol: 0.15 + r() * 0.65,
    weight: 0.2 + r() ** 2 * 3,
    last: today - 7 * DAY_MS,
  }));
  const total = symbols.reduce((t, s) => t + s.weight, 0);
  // From midnight UTC a week ago, as the dashboard opens on `now-7d/d`,
  // 60 000 trades a day.
  const from = today - 7 * DAY_MS;
  const n = Math.round(((now - from) / DAY_MS) * 60_000);
  const ticks = [];
  for (let i = 0; i < n; i++) {
    const at = Math.floor(from + ((now - from) * (i + r())) / n);
    let x = r() * total;
    const s = symbols.find((c) => (x -= c.weight) < 0) ?? symbols[symbols.length - 1];
    const years = Math.max(1, at - s.last) / (365 * DAY_MS);
    const normal = Math.sqrt(-2 * Math.log(1 - r())) * Math.cos(2 * Math.PI * r());
    s.px = Math.max(0.01, Math.round(s.px * Math.exp(s.vol * Math.sqrt(years) * normal * 3) * 100) / 100);
    s.last = at;
    ticks.push({ sym: s.sym, at, px: s.px, qty: Math.max(1, Math.round(Math.exp(r() * 6))) });
  }
  await load('ticks', ticks);
  console.log(`ticks: ${ticks.length} rows in ${Math.round(performance.now() - t0)} ms`);
}

// A year: the data source keeps it until Grafana is provisioned again,
// and this runs again with every `docker compose up`.
const b64 = (o) => Buffer.from(JSON.stringify(o)).toString('base64url');
const head = b64({ alg: 'HS256', typ: 'JWT' });
const claims = b64({ sub: 'grafana', role: 'grafana', iat: Math.floor(now / 1000), exp: Math.floor(now / 1000) + 365 * 86_400 });
const jwt = `${head}.${claims}.${createHmac('sha256', SECRET).update(`${head}.${claims}`).digest('base64url')}`;
const file = process.env.GRAFANA_TOKEN_FILE;
if (file) {
  writeFileSync(`${file}.new`, jwt, { mode: 0o400 });
  chownSync(`${file}.new`, GRAFANA_UID, 0);
  renameSync(`${file}.new`, file);
  console.log(`grafana's token: ${file}`);
} else {
  console.log(jwt);
}
