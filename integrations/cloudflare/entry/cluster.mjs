// Real fenec-server nodes and fenec-shard routers for the entry's tests and
// its bench: two regions keeping their own directory behind a global
// router, as crates/fenec-shard/tests/region_leases.rs sets them up, each
// process on port 0 and found by the address it says it took.

import { spawn } from 'node:child_process';
import { access, mkdtemp, rm } from 'node:fs/promises';
import http from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const root = resolve(new URL('.', import.meta.url).pathname, '../../..');

/** The binaries: FENEC_SERVER and FENEC_SHARD, else the debug build's (BUILD=release the release one's). */
export function binaries() {
  const build = process.env.BUILD === 'release' ? 'release' : 'debug';
  return {
    server: process.env.FENEC_SERVER ?? join(root, 'target', build, 'fenec-server'),
    shard: process.env.FENEC_SHARD ?? join(root, 'target', build, 'fenec-shard'),
  };
}

/** Whether both binaries are there. */
export async function built() {
  const { server, shard } = binaries();
  return Promise.all([access(server), access(shard)]).then(
    () => true,
    () => false,
  );
}

/** Every process started and not yet ended: a test that failed ends them all. */
export const processes = new Set();

/**
 * `binary` with `args`, resolved once it says where it listens: `{url, addr,
 * stop(), kill(), log()}`. `kill()` is SIGKILL: a router gone as a machine
 * goes, its port refusing from then on.
 */
export function start(binary, args) {
  const child = spawn(binary, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  processes.add(child);
  const ended = new Promise((done) => child.once('exit', done));
  ended.then(() => processes.delete(child));
  let log = '';
  const url = new Promise((ready, failed) => {
    const read = (d) => {
      log += d;
      const m = /listening on: (http:\/\/\S+)/.exec(log);
      if (m) ready(m[1]);
    };
    child.stdout.on('data', read);
    child.stderr.on('data', read);
    child.once('error', (e) => failed(new Error(`${binary} did not start: ${e.message}`)));
    ended.then(() => failed(new Error(`${binary} ${args.join(' ')} ended before it listened:\n${log}`)));
  });
  return url.then((url) => ({
    url,
    addr: url.slice('http://'.length),
    log: () => log,
    async stop() {
      if (child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');
      await ended;
    },
    async kill() {
      if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
      await ended;
    },
  }));
}

/**
 * One request, every header as given and every header of the answer as it
 * came (`rawHeaders`), duplicates kept -- what fetch would merge or refuse
 * to send. `body` a string; `stream` leaves the answer unread, `res` in it.
 */
export function request(url, { method = 'GET', headers = {}, body, signal, stream = false } = {}) {
  return new Promise((done, failed) => {
    const req = http.request(url, { method, headers, signal, agent: false }, (res) => {
      const raw = [];
      for (let i = 0; i < res.rawHeaders.length; i += 2) raw.push([res.rawHeaders[i].toLowerCase(), res.rawHeaders[i + 1]]);
      const all = (name) => raw.filter(([k]) => k === name).map(([, v]) => v);
      if (stream) return done({ status: res.statusCode, raw, all, header: (n) => all(n)[0], res });
      let text = '';
      res.setEncoding('utf8');
      res.on('data', (d) => (text += d));
      res.on('end', () => done({ status: res.statusCode, raw, all, header: (n) => all(n)[0], text, json: () => JSON.parse(text) }));
      res.on('error', failed);
    });
    req.on('error', failed);
    if (body !== undefined) req.setHeader('Content-Length', Buffer.byteLength(body));
    req.end(body);
  });
}

export const ROOT = 'routers-token';
export const DIRS = 'directory-replication';
export const DATA = 'data-token';
export const ORIGIN = 'https://app.example';

/** A statement for a tenant, through `base` (a router or the entry). */
export async function query(base, tenant, sql, params, headers = {}) {
  return request(`${base}/t/${tenant}/query`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${DATA}`, 'Content-Type': 'application/json', ...headers },
    body: JSON.stringify(params ? { query: sql, params } : { query: sql }),
  });
}

/** `/_shard/` of a router with the routers' token: its status and body. */
export async function shard(base, method, path, body) {
  return request(`${base}${path}`, {
    method,
    headers: { Authorization: `Bearer ${ROOT}`, 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

/** Waits for `ok()` to hold, every 50 ms; fails with `what` past `ms` -- a bound on what must happen. */
export async function until(ok, what, ms = 60_000) {
  const end = Date.now() + ms;
  for (;;) {
    const got = await ok();
    if (got) return got;
    if (Date.now() > end) throw new Error(`waited ${ms} ms for ${what}`);
    await new Promise((r) => setTimeout(r, 50));
  }
}

/**
 * The world: the global router (keeping no node), us's router and its
 * standby, eu's router, a node in each region; `tenants` made, each
 * `[name, region]`, with `schema`. `node` adds flags to each node's.
 */
export async function world({ tenants = [], schema = 'create collection notes (title text)', node = [] } = {}) {
  const { server, shard: router } = binaries();
  const dir = await mkdtemp(join(tmpdir(), 'fenecdb-entry-'));
  const on = (name) => join(dir, name);
  const routerArgs = (file, more) => [
    '--listen', '127.0.0.1:0', '--directory', on(file), '--token', ROOT, '--replication-token', DIRS,
    '--http-cors', ORIGIN, ...more,
  ];
  const global = await start(router, routerArgs('global.fenec', []));
  const regional = (region, file, more = []) => start(router, routerArgs(file, ['--region', region, '--global', global.url, ...more]));
  const us = await regional('us', 'us.fenec');
  const usStandby = await regional('us', 'us-standby.fenec', ['--replica-of', us.url]);
  const eu = await regional('eu', 'eu.fenec');
  const nodes = {};
  for (const region of ['us', 'eu']) {
    nodes[region] = await start(server, [
      '--dir', on(`${region}-node`), '--http', '127.0.0.1:0', '--admin-token', `adm-${region}`,
      '--http-token', DATA, '--replication-token', 'tenant-replication', ...node,
    ]);
  }
  const w = { dir, global, us, usStandby, eu, nodes };
  try {
    for (const [region, routers] of [['us', [us.url, usStandby.url]], ['eu', [eu.url]]]) {
      const r = await shard(global.url, 'PUT', `/_shard/regions/${region}`, { routers });
      if (r.status !== 200) throw new Error(`region ${region}: ${r.status} ${r.text}`);
    }
    // Each region's routers hear of their region from the global router.
    for (const r of [us, usStandby, eu]) {
      await until(async () => (await shard(r.url, 'GET', '/_shard/regions')).text.includes('"name":"us"'), `${r.url}'s copy of the global directory`);
    }
    for (const region of ['us', 'eu']) {
      const body = { addr: nodes[region].addr, token: `adm-${region}`, region };
      const r = await shard(global.url, 'PUT', `/_shard/nodes/${region}1`, body);
      if (r.status !== 201) throw new Error(`node ${region}1: ${r.status} ${r.text}`);
    }
    for (const [name, region] of tenants) {
      const r = await shard(global.url, 'PUT', `/_shard/tenants/${name}`, { region, schema });
      if (r.status !== 201) throw new Error(`tenant ${name}: ${r.status} ${r.text}`);
    }
    // us's standby places us's tenants once its directory has come.
    for (const [name, region] of tenants) {
      if (region !== 'us') continue;
      await until(async () => (await query(usStandby.url, name, 'get notes count')).status === 200, `${name} on us's standby`);
    }
  } catch (e) {
    await end(w);
    throw e;
  }
  w.stop = () => end(w);
  return w;
}

async function end(w) {
  for (const p of [w.global, w.us, w.usStandby, w.eu, ...Object.values(w.nodes)]) await p?.stop().catch(() => {});
  await rm(w.dir, { recursive: true, force: true });
}

/** Ends every process still running: a test cut short by its timeout runs no `finally`. */
export function killAll() {
  for (const child of processes) child.kill('SIGKILL');
}
