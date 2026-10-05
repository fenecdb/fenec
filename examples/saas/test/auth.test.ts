// Accounts and sessions: passwords hashed by the app, emails made one
// before the unique index sees them, the same answer for an unknown email,
// rate limits that hold under concurrency, refresh tokens that rotate and
// give a stolen copy away, and links that work once and lapse.
import assert from 'node:assert/strict';
import { after, before, describe, test } from 'node:test';
import { checkPassword, digest, hashPassword, normaliseEmail } from '../src/passwords.ts';
import { LIMITED, WRONG } from '../src/trellis.ts';
import { Browser, mailedLink, PASSWORD, person, query, start, type Env } from './harness.ts';

let env: Env;
before(async () => {
  env = await start(19300);
});
after(() => env.stop());

const operator = () => env.cfg.operatorToken;

describe('passwords', () => {
  test('are scrypt hashes with a salt each, checked in constant time', async () => {
    const a = await hashPassword('hunter2 hunter2', 2 ** 12);
    const b = await hashPassword('hunter2 hunter2', 2 ** 12);
    assert.match(a, /^scrypt\$12\$[A-Za-z0-9_-]{22}\$[A-Za-z0-9_-]{43}$/);
    assert.notEqual(a, b, 'a salt each');
    assert.equal(await checkPassword('hunter2 hunter2', a), true);
    assert.equal(await checkPassword('hunter2 hunter3', a), false);
  });

  test('the database holds the hash, never the password', async () => {
    await person(env, 'stored@x.io', 'Stored');
    const r = await query(env, 'accounts', operator(), 'get users select password where email = "stored@x.io"');
    const [row] = r.body as { password: string }[];
    assert.match(row.password, /^scrypt\$/);
    assert.ok(!row.password.includes(PASSWORD));
  });
});

describe('emails', () => {
  test('are normalised before the unique index compares their bytes', () => {
    assert.equal(normaliseEmail('  Alice@X.io '), 'alice@x.io');
    // é written whole, and as e and a combining acute accent
    assert.equal(normaliseEmail('José@x.io'), normaliseEmail('José@x.io'));
    assert.equal(normaliseEmail('not an address'), null);
  });

  test('Alice@X.io and alice@x.io are one account', async () => {
    const a = await person(env, 'Alice@X.io', 'Alice');
    const again = new Browser(env, '10.9.9.1');
    const up = await again.signUp('alice@x.io', 'Alice again');
    assert.equal(up.status, 202, 'the same answer as a new address');
    const rows = (await query(env, 'accounts', operator(), 'get users where email = "alice@x.io"')).body as unknown[];
    assert.equal(rows.length, 1);
    const mail = await env.trellis.outbox('alice@x.io');
    assert.match(mail[0].subject, /tried to sign up/);
    const s = await new Browser(env, '10.9.9.2').signIn('ALICE@x.io');
    assert.equal(s.status, 200);
    assert.equal((s.body.user as { uid: string }).uid, a.uid);
  });

  test('José composed and decomposed is one account', async () => {
    await person(env, 'José@x.io', 'José');
    const s = await new Browser(env, '10.9.9.3').signIn('José@X.io');
    assert.equal(s.status, 200);
  });
});

describe('sign-in', () => {
  test('an unknown email and a wrong password get the same answer', async () => {
    await person(env, 'known@x.io', 'Known');
    const wrong = await new Browser(env, '10.8.0.1').signIn('known@x.io', 'not the password');
    const unknown = await new Browser(env, '10.8.0.2').signIn('nobody@x.io', 'not the password');
    assert.equal(wrong.status, 401);
    assert.equal(unknown.status, 401);
    assert.deepEqual(wrong.body, unknown.body);
    assert.equal(wrong.body.error, WRONG);
    assert.equal(wrong.headers.get('set-cookie'), null);
    assert.equal(unknown.headers.get('set-cookie'), null);
  });

  test('an unknown email takes as long as a wrong password', async () => {
    // Ten accounts, so no one account meets its limit; alternated, so a
    // machine getting busier slows both alike.
    const known: string[] = [];
    for (let i = 0; i < 10; i++) {
      await person(env, `timing${i}@x.io`, 'Timing');
      known.push(`timing${i}@x.io`);
    }
    const time = async (email: string, ip: string) => {
      const t0 = performance.now();
      const r = await new Browser(env, ip).signIn(email, 'not the password');
      assert.equal(r.status, 401);
      return performance.now() - t0;
    };
    const k: number[] = [];
    const u: number[] = [];
    for (let i = 0; i < 40; i++) {
      k.push(await time(known[i % 10], `10.7.${i}.1`));
      u.push(await time(`ghost${i}@x.io`, `10.7.${i}.2`));
    }
    const median = (xs: number[]) => xs.sort((a, b) => a - b)[xs.length >> 1];
    const [mk, mu] = [median(k), median(u)];
    console.log(`  median sign-in: wrong password ${mk.toFixed(2)} ms, unknown email ${mu.toFixed(2)} ms`);
    assert.ok(Math.abs(mk - mu) / mk < 0.25, `${mk} against ${mu}`);
  });

  test('an account takes 8 attempts in ten minutes, from any number of addresses', async () => {
    await person(env, 'target@x.io', 'Target');
    const answers = await Promise.all(
      Array.from({ length: 16 }, (_, i) => new Browser(env, `10.6.${i}.1`).signIn('target@x.io', 'guess')),
    );
    const statuses = answers.map((a) => a.status).sort();
    assert.equal(statuses.filter((s) => s === 401).length, env.cfg.accountLimit);
    assert.equal(statuses.filter((s) => s === 429).length, 16 - env.cfg.accountLimit);
    // Locked for the right password too, until the window lapses or a reset.
    const right = await new Browser(env, '10.6.99.1').signIn('target@x.io');
    assert.equal(right.status, 429);
    assert.equal(right.body.error, LIMITED);
    // An unknown address is limited the same way: no difference to see.
    const ghosts = await Promise.all(Array.from({ length: 10 }, (_, i) => new Browser(env, `10.5.${i}.1`).signIn('ghost-limit@x.io', 'guess')));
    assert.equal(ghosts.filter((a) => a.status === 429).length, 10 - env.cfg.accountLimit);
  });

  test('an address takes 30 attempts in ten minutes, over any number of accounts', async () => {
    const b = new Browser(env, '10.4.0.1');
    const answers = await Promise.all(Array.from({ length: 40 }, (_, i) => b.signIn(`spray${i}@x.io`, 'guess')));
    assert.equal(answers.filter((a) => a.status === 401).length, env.cfg.ipLimit);
    assert.equal(answers.filter((a) => a.status === 429).length, 40 - env.cfg.ipLimit);
  });

  test('a successful sign-in clears the account counter', async () => {
    await person(env, 'clears@x.io', 'Clears', '10.3.0.1');
    for (let i = 0; i < env.cfg.accountLimit - 1; i++) await new Browser(env, '10.3.0.2').signIn('clears@x.io', 'typo');
    assert.equal((await new Browser(env, '10.3.0.3').signIn('clears@x.io')).status, 200);
    assert.equal((await new Browser(env, '10.3.0.4').signIn('clears@x.io', 'typo')).status, 401);
  });
});

describe('sessions', () => {
  test('a refresh rotates the token, and the used one is dead', async () => {
    const b = await person(env, 'rotate@x.io', 'Rotate');
    const first = b.jar.get('trellis_rt')!;
    assert.equal((await b.refresh()).status, 200);
    const second = b.jar.get('trellis_rt')!;
    assert.notEqual(first, second);
    const rows = (await query(env, 'accounts', operator(), 'get refresh where hash = $1', [first])).body as unknown[];
    assert.equal(rows.length, 0, 'kept as a hash, never as the token');
    assert.equal(((await query(env, 'accounts', operator(), 'get refresh where hash = $1', [digest(first)])).body as unknown[]).length, 1);
  });

  test('a refresh token used twice revokes its whole family', async () => {
    const b = await person(env, 'stolen@x.io', 'Victim');
    const stolen = b.jar.get('trellis_rt')!;
    assert.equal((await b.refresh()).status, 200); // the victim rotates
    const thief = new Browser(env, '10.2.0.9');
    thief.jar.set('trellis_rt', stolen);
    const t = await thief.refresh();
    assert.equal(t.status, 401, 'the copy is refused');
    const v = await b.refresh();
    assert.equal(v.status, 401, 'and the victim\'s current token went with the family');
    const log = (await query(env, 'accounts', operator(), 'get security_log where user = $1 and kind = "refresh.reuse"', [b.uid])).body as unknown[];
    assert.equal(log.length, 1);
  });

  test('two refreshes racing with one token: at most one wins, and the family ends', async () => {
    const b = await person(env, 'race@x.io', 'Race');
    const rt = b.jar.get('trellis_rt')!;
    const tabs = Array.from({ length: 8 }, (_, i) => {
      const t = new Browser(env, `10.1.0.${i}`);
      t.jar.set('trellis_rt', rt);
      return t;
    });
    const answers = await Promise.all(tabs.map((t) => t.refresh()));
    assert.ok(answers.filter((a) => a.status === 200).length <= 1);
    for (const t of tabs) assert.equal((await t.refresh()).status, 401);
  });

  test('an unknown refresh token is refused and mints nothing', async () => {
    const b = new Browser(env, '10.0.9.9');
    b.jar.set('trellis_rt', 'made-up');
    assert.equal((await b.refresh()).status, 401);
    assert.equal((await b.post('/api/auth/refresh')).status, 401);
  });

  test('signing out revokes the family', async () => {
    const b = await person(env, 'out@x.io', 'Out');
    const rt = b.jar.get('trellis_rt')!;
    assert.equal((await b.post('/api/auth/signout')).status, 200);
    const again = new Browser(env, '10.0.9.8');
    again.jar.set('trellis_rt', rt);
    assert.equal((await again.refresh()).status, 401);
  });

  test('a cross-origin request is refused', async () => {
    const res = await fetch(`${env.app}/api/auth/signin`, {
      method: 'POST',
      headers: { origin: 'https://evil.example', 'content-type': 'application/json' },
      body: JSON.stringify({ email: 'known@x.io', password: PASSWORD }),
    });
    assert.equal(res.status, 403);
    const form = await fetch(`${env.app}/api/auth/signin`, { method: 'POST', body: 'email=known@x.io' });
    assert.equal(form.status, 415);
  });
});

describe('links', () => {
  test('a verification link works once', async () => {
    const b = new Browser(env, '10.0.8.1');
    await b.signUp('verify@x.io', 'Verify');
    const token = new URL(await mailedLink(env, 'verify@x.io', '/verify\\?token='), 'http://x').searchParams.get('token');
    assert.equal((await b.post('/api/auth/verify', { token })).status, 200);
    assert.equal((await b.post('/api/auth/verify', { token })).status, 400);
  });

  test('a password reset works once, signs every session out, and a lapsed one fails', async () => {
    const b = await person(env, 'reset@x.io', 'Reset');
    const other = await new Browser(env, '10.0.7.2').signIn('reset@x.io');
    assert.equal(other.status, 200);
    assert.equal((await new Browser(env, '10.0.7.3').post('/api/auth/forgot', { email: 'Reset@X.io' })).status, 202);
    const token = new URL(await mailedLink(env, 'reset@x.io', '/reset\\?token='), 'http://x').searchParams.get('token');
    const r = await b.post('/api/auth/reset', { token, password: 'a brand new password' });
    assert.equal(r.status, 200);
    assert.equal((await b.post('/api/auth/reset', { token, password: 'and another one again' })).status, 400, 'once');
    assert.equal((await b.refresh()).status, 401, 'sessions revoked');
    assert.equal((await new Browser(env, '10.0.7.4').signIn('reset@x.io')).status, 401, 'the old password is gone');
    assert.equal((await new Browser(env, '10.0.7.5').signIn('reset@x.io', 'a brand new password')).status, 200);

    // A link 31 minutes old: the row is still on disk until the sweep, and
    // out of every read and delete already (@ttl(30m)).
    const code = 'lapsed-reset-code';
    const user = (await query(env, 'accounts', operator(), 'get users select uid where email = "reset@x.io"')).body as { uid: string }[];
    const ins = await query(env, 'accounts', operator(), 'insert resets {hash: $1, user: $2, at: now() - 1860000}', [digest(code), user[0].uid]);
    assert.equal(ins.status, 200);
    assert.equal((await b.post('/api/auth/reset', { token: code, password: 'too late for this one' })).status, 400);
    const fresh = 'fresh-reset-code';
    await query(env, 'accounts', operator(), 'insert resets {hash: $1, user: $2, at: now() - 1740000}', [digest(fresh), user[0].uid]);
    assert.equal((await b.post('/api/auth/reset', { token: fresh, password: 'just in time, this' })).status, 200, '29 minutes is still in time');
  });

  test('forgot answers the same for an unknown address', async () => {
    const a = await new Browser(env, '10.0.6.1').post('/api/auth/forgot', { email: 'known@x.io' });
    const b = await new Browser(env, '10.0.6.2').post('/api/auth/forgot', { email: 'nobody-at-all@x.io' });
    assert.equal(a.status, b.status);
    assert.deepEqual(a.body, b.body);
  });
});

describe('the security log', () => {
  test('is append-only for the app\'s own token', async () => {
    const appToken = env.trellis.tokens.app('accounts');
    const del = await query(env, 'accounts', appToken, 'del security_log');
    assert.equal(del.status, 403);
    const set = await query(env, 'accounts', appToken, 'set security_log {kind: "nothing"}');
    assert.equal(set.status, 403);
    const ins = await query(env, 'accounts', appToken, 'insert security_log {user: "x", kind: "test", ip: "", detail: "", at: now()}');
    assert.equal(ins.status, 200);
  });

  test('records sign-ins and failures', async () => {
    const b = await person(env, 'logged@x.io', 'Logged');
    await new Browser(env, '10.0.5.1').signIn('logged@x.io', 'typo');
    const r = await b.call('GET', '/api/me/security', undefined, b.api);
    const kinds = (r.body.events as { kind: string }[]).map((e) => e.kind);
    assert.ok(kinds.includes('signin'));
    assert.ok(kinds.includes('signin.failed'));
    assert.ok(kinds.includes('signup'));
  });
});
