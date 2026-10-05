// Tokens that must not work: forged, expired, without an expiry, `alg:
// none`, HS256 "signed" with the RSA public key, a key id nobody published,
// another tenant's, no tenant at all -- against fenec-server through the
// router, against this app's own API, and as the identity provider's ID
// tokens. And single sign-on end to end.
import assert from 'node:assert/strict';
import { createHmac, generateKeyPairSync, type KeyObject } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { after, before, describe, test } from 'node:test';
import { loadOrMakeKey, signJwt, verifyJwt, type Jwk } from '../src/jwt.ts';
import { Oidc } from '../src/oidc.ts';
import { Browser, direct, organisation, person, query, start, type Env } from './harness.ts';

let env: Env;
let owner: Browser;
let good: string;
let appKey: { kid: string; key: KeyObject };
let appJwk: Jwk;
const now = () => Math.floor(Date.now() / 1000);
const b64 = (v: unknown) => Buffer.from(typeof v === 'string' ? v : JSON.stringify(v)).toString('base64url');

before(async () => {
  env = await start(19400);
  owner = await person(env, 'owner@acme.io', 'Owner');
  await organisation(owner, 'acme');
  await organisation(owner, 'globex');
  good = await owner.token('acme');
  const k = loadOrMakeKey(env.cfg.keysDir, 'app', 'trellis-app-1');
  appKey = k.signing;
  appJwk = k.jwks.keys[0];
});
after(() => env.stop());

const claims = (over: Record<string, unknown> = {}) => ({
  iss: 'trellis',
  sub: owner.uid,
  tenant: 'o-acme',
  role: ['owner', 'admin', 'member', 'guest'],
  teams: [],
  iat: now(),
  exp: now() + 300,
  ...over,
});

function hs256(header: Record<string, unknown>, body: Record<string, unknown>, secret: string | Buffer): string {
  const h = b64(header);
  const p = b64(body);
  return `${h}.${p}.${createHmac('sha256', secret).update(`${h}.${p}`).digest('base64url')}`;
}

function attackerKey(): KeyObject {
  return generateKeyPairSync('rsa', { modulusLength: 2048 }).privateKey;
}

describe('fenec-server refuses', () => {
  const cases: [string, () => string, number][] = [
    ['the real token (control)', () => good, 200],
    ['a forged signature: another RSA key under the published key id', () => signJwt(claims(), { kid: appKey.kid, key: attackerKey() }), 401],
    ['an expired token', () => signJwt(claims({ exp: now() - 120 }), appKey), 401],
    ['a token with no exp', () => signJwt(claims({ exp: undefined }), appKey), 401],
    ['a token not valid yet (nbf)', () => signJwt(claims({ nbf: now() + 3600 }), appKey), 401],
    ['alg: none', () => `${b64({ alg: 'none', typ: 'JWT' })}.${b64(claims())}.`, 401],
    ['HS256 signed with the public key as PEM', () => hs256({ alg: 'HS256', kid: appKey.kid }, claims(), readFileSync(join(env.cfg.keysDir, 'app.jwks.json'))), 401],
    ['HS256 signed with the modulus', () => hs256({ alg: 'HS256', kid: appKey.kid }, claims(), Buffer.from(appJwk.n!, 'base64url')), 401],
    ['HS256 with no key id', () => hs256({ alg: 'HS256' }, claims(), appJwk.n!), 401],
    ['a key id nobody published', () => signJwt(claims(), { kid: 'trellis-app-2', key: appKey.key }), 401],
    ['a body changed after signing', () => good.split('.').map((p, i) => (i === 1 ? b64(claims({ role: ['owner'], teams: ['everything'] })) : p)).join('.'), 401],
    ['a token naming no tenant', () => signJwt(claims({ tenant: undefined }), appKey), 403],
    ['a token for another tenant', () => signJwt(claims({ tenant: 'o-globex' }), appKey), 403],
    ['this app\'s API token', () => owner.api!, 403],
  ];
  for (const [name, token, want] of cases) {
    test(`${name}: ${want}`, async () => {
      const r = await query(env, 'o-acme', token(), 'get members limit 1');
      assert.equal(r.status, want, r.text);
      // A good token clears the node's count of refusals from this
      // address, so the next case does not wait out this one's (below).
      assert.equal((await query(env, 'o-acme', good, 'get members limit 1')).status, 200);
    });
  }

  test('a refused token waits, longer each time from one address', async () => {
    const forged = signJwt(claims(), { kid: appKey.kid, key: attackerKey() });
    const waits: number[] = [];
    for (let i = 0; i < 4; i++) {
      const t0 = performance.now();
      assert.equal((await query(env, 'o-acme', forged, 'get members')).status, 401);
      waits.push(performance.now() - t0);
    }
    console.log(`  refusals waited ${waits.map((w) => w.toFixed(0)).join(', ')} ms`);
    for (let i = 0; i < 4; i++) assert.ok(waits[i] >= 100 * 2 ** i * 0.95, `refusal ${i + 1} waited ${waits[i]}`);
    const t0 = performance.now();
    assert.equal((await query(env, 'o-acme', good, 'get members')).status, 200);
    assert.ok(performance.now() - t0 < 100, 'a good token does not wait');
    const t1 = performance.now();
    await query(env, 'o-acme', forged, 'get members');
    assert.ok(performance.now() - t1 < 190, 'and its success cleared the count');
  });

  test('an organisation\'s token reaches no other tenant, by any route', async () => {
    for (const path of ['/t/o-globex/members', '/t/o-globex/members/changes', '/t/accounts/users', '/t/o-globex/_changes?since=0']) {
      const r = await direct(env, 'GET', path, good);
      assert.equal(r.status, 403, `${path}: ${r.text}`);
    }
    for (const q of ['get members', 'get users']) {
      assert.equal((await query(env, 'o-globex', good, q)).status, 403);
      assert.equal((await query(env, 'accounts', good, q)).status, 403);
    }
    const batch = await fetch(`${env.cfg.routerUrl}/t/o-globex/batch`, {
      method: 'POST',
      headers: { authorization: `Bearer ${good}`, 'content-type': 'application/x-ndjson' },
      body: JSON.stringify({ query: 'get members' }),
    });
    assert.equal(batch.status, 403);
  });

  test('no token, or the router\'s and nodes\' admin routes with a person\'s token', async () => {
    assert.equal((await query(env, 'o-acme', undefined, 'get members')).status, 401);
    assert.equal((await direct(env, 'GET', '/_shard/tenants', good)).status, 401);
    // fetch resolves `..` itself; sent encoded, the node takes it as a name.
    assert.ok([403, 404].includes((await direct(env, 'GET', '/t/o-acme/%2e%2e/accounts/users', good)).status));
    const node = env.cluster.nodeUrl('n1');
    assert.equal((await fetch(`${node}/_admin/tenants`, { headers: { authorization: `Bearer ${good}` } })).status, 401);
  });
});

describe('the app refuses', () => {
  test('an organisation\'s token for another organisation\'s API', async () => {
    const r = await owner.post('/api/orgs/globex/invites', { email: 'x@y.io', role: 'member', teams: [] }, good);
    assert.equal(r.status, 401);
  });

  test('an organisation\'s token where an API token goes, and the reverse', async () => {
    assert.equal((await owner.post('/api/orgs', { name: 'x', slug: 'xyz' }, good)).status, 401);
    assert.equal((await owner.post('/api/orgs/acme/teams', { name: 'x' }, owner.api)).status, 401);
  });

  test('the app\'s own token, an expired one, a forged one', async () => {
    const app = env.trellis.tokens.app('o-acme');
    assert.equal((await owner.post('/api/orgs/acme/teams', { name: 'x' }, app)).status, 401);
    const expired = signJwt(claims({ exp: now() - 1 }), appKey);
    assert.equal((await owner.post('/api/orgs/acme/teams', { name: 'x' }, expired)).status, 401);
    const forged = signJwt(claims(), { kid: appKey.kid, key: attackerKey() });
    assert.equal((await owner.post('/api/orgs/acme/teams', { name: 'x' }, forged)).status, 401);
    const none = `${b64({ alg: 'none' })}.${b64(claims())}.`;
    assert.equal((await owner.post('/api/orgs/acme/teams', { name: 'x' }, none)).status, 401);
  });
});

describe('single sign-on', () => {
  async function idTokenAttacks(): Promise<[string, string, RegExp][]> {
    const idp = loadOrMakeKey(env.cfg.keysDir, 'idp', 'idp-1');
    const base = { iss: env.idp, aud: env.cfg.oidcClientId, sub: 's1', email: 'x@y.io', email_verified: true, iat: now(), exp: now() + 300 };
    return [
      ['forged', signJwt(base, { kid: 'idp-1', key: attackerKey() }), /bad signature/],
      ['alg none', `${b64({ alg: 'none' })}.${b64(base)}.`, /algorithm/],
      ['HS256 with the public key', hs256({ alg: 'HS256', kid: 'idp-1' }, base, idp.jwks.keys[0].n!), /algorithm/],
      ['an unknown key id', signJwt(base, { kid: 'idp-9', key: idp.signing.key }), /no key/],
      ['another audience', signJwt({ ...base, aud: 'someone-else' }, idp.signing), /audience/],
      ['another issuer', signJwt({ ...base, iss: 'https://evil.example' }, idp.signing), /issuer/],
      ['expired', signJwt({ ...base, exp: now() - 600 }, idp.signing), /expired/],
      ['no exp', signJwt({ ...base, exp: undefined }, idp.signing), /no exp/],
    ];
  }

  test('ID tokens that must not verify', async () => {
    const oidc = new Oidc(env.idp, env.cfg.oidcClientId, `${env.app}/api/auth/sso/callback`);
    for (const [name, token, why] of await idTokenAttacks()) {
      await assert.rejects(oidc.verify(token), why, name);
    }
    const idp = loadOrMakeKey(env.cfg.keysDir, 'idp', 'idp-1');
    const ok = signJwt({ iss: env.idp, aud: env.cfg.oidcClientId, sub: 's', iat: now(), exp: now() + 60 }, idp.signing);
    assert.equal((await oidc.verify(ok)).sub, 's');
    assert.throws(() => verifyJwt(ok, []), /no key/);
  });

  async function ssoAs(b: Browser, email: string, stateFrom?: Browser) {
    const start = await b.call('GET', '/api/auth/sso');
    assert.equal(start.status, 302);
    const to = new URL(start.headers.get('location')!);
    const form = new URLSearchParams(to.searchParams);
    form.set('email', email);
    form.set('name', 'Sam Single');
    const idp = await fetch(`${env.idp}/authorize`, { method: 'POST', body: form, redirect: 'manual' });
    assert.equal(idp.status, 302);
    const back = new URL(idp.headers.get('location')!);
    const who = stateFrom ?? b;
    return who.call('GET', `${back.pathname}${back.search}`);
  }

  test('signs in, makes a confirmed account, and finds it again', async () => {
    const b = new Browser(env, '10.20.0.1');
    const r = await ssoAs(b, 'Sam@Single.io');
    assert.equal(r.status, 302);
    assert.equal(r.headers.get('location'), '/');
    const s = await b.refresh();
    assert.equal(s.status, 200);
    const user = s.body.user as { email: string; verified: boolean; uid: string };
    assert.equal(user.email, 'sam@single.io');
    assert.equal(user.verified, true);
    const again = new Browser(env, '10.20.0.2');
    await ssoAs(again, 'sam@single.io');
    assert.equal(((await again.refresh()).body.user as { uid: string }).uid, user.uid);
  });

  test('a callback in a browser that did not start it is refused', async () => {
    const victim = new Browser(env, '10.20.0.3');
    await victim.call('GET', '/api/auth/sso'); // the victim's own state cookie
    const r = await ssoAs(new Browser(env, '10.20.0.4'), 'attacker@evil.io', victim);
    assert.equal(r.status, 302);
    assert.match(r.headers.get('location')!, /^\/signin\?error=/);
    assert.equal(victim.jar.has('trellis_rt'), false);
  });
});
