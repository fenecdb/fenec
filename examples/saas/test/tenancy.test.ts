// Who may do what, role by role: every operation tried with each person's
// own token, straight against fenec-server through the router (what anyone
// holding their token could send) and through this app's API, and the
// outcome checked with the operator's token -- a refused write must have
// written nothing. Then invitations: once, to the address they were sent
// to, and not after they lapse.
import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { digest } from '../src/passwords.ts';
import { Browser, direct, join_, mailedLink, organisation, person, query, start, type Env } from './harness.ts';

let env: Env;
const T = 'o-acme';
let teamA: string;
let teamB: string;
const people: Record<string, Browser> = {};
const tokens: Record<string, string> = {};

const op = (q: string, params: unknown[] = []) => query(env, T, env.cfg.operatorToken, q, params);
async function opRows<R = Record<string, unknown>>(q: string, params: unknown[] = []): Promise<R[]> {
  const r = await op(q, params);
  assert.equal(r.status, 200, r.text);
  return r.body as R[];
}

before(async () => {
  env = await start(19500);
  people.owner = await person(env, 'olivia@acme.io', 'Olivia');
  await organisation(people.owner, 'acme', 'Acme');
  teamA = people.owner.access.get('acme')?.teams[0] ?? (await people.owner.refresh('acme'), people.owner.access.get('acme')!.teams[0]);
  const made = await people.owner.post('/api/orgs/acme/teams', { name: 'Design' }, await people.owner.token('acme'));
  assert.equal(made.status, 201);
  teamB = made.body.key as string;
  for (const [key, email, name, role, teams] of [
    ['admin', 'adam@acme.io', 'Adam', 'admin', []],
    ['member', 'mia@acme.io', 'Mia', 'member', [teamA]],
    ['guest', 'gus@acme.io', 'Gus', 'guest', [teamA]],
    ['both', 'max@acme.io', 'Max', 'member', [teamA, teamB]],
    ['owner2', 'oscar@acme.io', 'Oscar', 'member', [teamA]],
  ] as const) {
    people[key] = await person(env, email, name);
    await join_(env, people.owner, 'acme', people[key], email, role, [...teams]);
  }
  // Oscar is a second owner, so an owner's change to an owner can be tried.
  assert.equal((await op('set members {role: "owner"} where user = $1 require 1', [people.owner2.uid])).status, 200);
  people.outsider = await person(env, 'gina@globex.io', 'Gina');
  await organisation(people.outsider, 'globex', 'Globex');
  for (const k of ['owner', 'admin', 'member', 'guest', 'both']) tokens[k] = await people[k].token('acme');
  tokens.outsider = await people.outsider.token('globex');
});
after(() => env.stop());

/** A task made for one check, by the operator. */
async function task(team: string): Promise<number> {
  const r = await op('insert tasks {team: $1, title: "Probe", status: "backlog", author: $2, updated: now()}', [team, people.owner.uid]);
  assert.equal(r.status, 200);
  const [row] = await opRows<{ id: number }>('get tasks select id order id desc limit 1');
  return row.id;
}

async function comment(taskId: number, team: string, author: string): Promise<number> {
  assert.equal((await op('insert comments {task: $1, team: $2, author: $3, body: "first", at: now()}', [taskId, team, author])).status, 200);
  return (await opRows<{ id: number }>('get comments select id order id desc limit 1'))[0].id;
}

const q = (who: string, text: string, params: unknown[] = []) => query(env, who === 'outsider' ? T : T, tokens[who], text, params);
const wrote = (r: { status: number; body: unknown }) => r.status === 200 && (r.body as { affected?: number }).affected! > 0;

type Check = (who: string) => Promise<boolean>;
const checks: [string, Check][] = [
  ['read tasks of their own team', async (w) => {
    const id = await task(teamA);
    const r = await q(w, 'get tasks where id = $1', [id]);
    return r.status === 200 && (r.body as unknown[]).length === 1;
  }],
  ['read tasks of another team', async (w) => {
    const id = await task(teamB);
    const r = await q(w, 'get tasks where id = $1', [id]);
    return r.status === 200 && (r.body as unknown[]).length === 1;
  }],
  ['create a task in their team', async (w) => wrote(await q(w, 'insert tasks {team: $1, title: "Mine", status: "backlog", updated: now()}', [teamA]))],
  ['create a task in another team', async (w) => {
    const before = (await opRows('get tasks where team = $1 count', [teamB]))[0];
    const r = await q(w, 'insert tasks {team: $1, title: "Theirs", status: "backlog", updated: now()}', [teamB]);
    const after_ = (await opRows('get tasks where team = $1 count', [teamB]))[0];
    assert.equal(wrote(r), JSON.stringify(before) !== JSON.stringify(after_));
    return wrote(r);
  }],
  ['edit a task\'s title', async (w) => {
    const id = await task(teamA);
    const r = await q(w, 'set tasks {title: "Edited"} where id = $1', [id]);
    const [row] = await opRows<{ title: string }>('get tasks select title where id = $1', [id]);
    assert.equal(wrote(r), row.title === 'Edited');
    return wrote(r);
  }],
  ['move a task to another team', async (w) => {
    const id = await task(teamA);
    const r = await q(w, 'set tasks {team: $2} where id = $1', [id, teamB]);
    const [row] = await opRows<{ team: string }>('get tasks select team where id = $1', [id]);
    assert.equal(wrote(r), row.team === teamB);
    return wrote(r);
  }],
  ['delete a task', async (w) => {
    const id = await task(teamA);
    const r = await q(w, 'del tasks where id = $1', [id]);
    const left = await opRows('get tasks where id = $1', [id]);
    assert.equal(wrote(r), left.length === 0);
    return wrote(r);
  }],
  ['comment as themselves', async (w) => {
    const id = await task(teamA);
    return wrote(await q(w, 'insert comments {task: $1, team: $2, body: "Looks good", at: now()}', [id, teamA]));
  }],
  ['comment as someone else', async (w) => {
    const id = await task(teamA);
    const r = await q(w, 'insert comments {task: $1, team: $2, author: $3, body: "Not me", at: now()}', [id, teamA, people.owner2.uid]);
    const left = await opRows('get comments where task = $1', [id]);
    assert.equal(wrote(r), left.length === 1);
    return wrote(r);
  }],
  ['edit their own comment', async (w) => {
    const id = await task(teamA);
    const c = await comment(id, teamA, people[w].uid!);
    return wrote(await q(w, 'set comments {body: "Edited", edited: now()} where id = $1', [c]));
  }],
  ['edit someone else\'s comment', async (w) => {
    const id = await task(teamA);
    const c = await comment(id, teamA, people.both.uid!);
    const r = await q(w, 'set comments {body: "Hijacked"} where id = $1', [c]);
    const [row] = await opRows<{ body: string }>('get comments select body where id = $1', [c]);
    assert.equal(wrote(r), row.body === 'Hijacked');
    return wrote(r);
  }],
  ['delete a comment', async (w) => {
    const id = await task(teamA);
    const c = await comment(id, teamA, people.both.uid!);
    return wrote(await q(w, 'del comments where id = $1', [c]));
  }],
  ['read the member list', async (w) => {
    const r = await q(w, 'get members select user, role');
    return r.status === 200 && (r.body as unknown[]).length >= 6;
  }],
  ['change a guest\'s role', async (w) => {
    const r = await q(w, 'set members {role: "member"} where user = $1', [people.guest.uid]);
    const [row] = await opRows<{ role: string }>('get members select role where user = $1', [people.guest.uid]);
    assert.equal(wrote(r), row.role === 'member');
    await op('set members {role: "guest"} where user = $1', [people.guest.uid]);
    return wrote(r);
  }],
  ['make someone an owner', async (w) => {
    const r = await q(w, 'set members {role: "owner"} where user = $1', [people.guest.uid]);
    const [row] = await opRows<{ role: string }>('get members select role where user = $1', [people.guest.uid]);
    assert.equal(wrote(r), row.role === 'owner');
    await op('set members {role: "guest"} where user = $1', [people.guest.uid]);
    return wrote(r);
  }],
  ['change an owner\'s role', async (w) => {
    const r = await q(w, 'set members {role: "member"} where user = $1', [people.owner2.uid]);
    const [row] = await opRows<{ role: string }>('get members select role where user = $1', [people.owner2.uid]);
    assert.equal(wrote(r), row.role === 'member');
    await op('set members {role: "owner"} where user = $1', [people.owner2.uid]);
    return wrote(r);
  }],
  ['read the audit log', async (w) => {
    const r = await q(w, 'get audit limit 5');
    return r.status === 200 && (r.body as unknown[]).length > 0;
  }],
  ['write an audit row naming themselves', async (w) => {
    return wrote(await q(w, 'insert audit {actor: $1, action: "note", target: "", detail: "", at: now()}', [people[w].uid]));
  }],
  ['write an audit row naming someone else', async (w) => {
    return wrote(await q(w, 'insert audit {actor: $1, action: "framed", target: "", detail: "", at: now()}', [people.member.uid]));
  }],
  ['rewrite or delete an audit row', async (w) => {
    const before = await opRows('get audit count');
    const a = await q(w, 'set audit {detail: "rewritten"} where id = 1');
    const b = await q(w, 'del audit where id = 1');
    assert.deepEqual(await opRows('get audit count'), before);
    return wrote(a) || wrote(b);
  }],
  ['read invitations', async (w) => {
    const r = await q(w, 'get invites limit 1');
    return r.status === 200 && (r.body as unknown[]).length > 0;
  }],
  ['read another organisation', async (w) => {
    const r = await query(env, 'o-globex', tokens[w], 'get members');
    return r.status === 200;
  }],
  ['read the accounts tenant', async (w) => (await query(env, 'accounts', tokens[w], 'get users')).status === 200],
  ['change the schema', async (w) => (await q(w, 'create collection loot (x text)')).status === 200],
  ['read the change stream', async (w) => (await direct(env, 'GET', `/t/${T}/_changes?since=0&limit=1`, tokens[w])).status === 200],
  ['invite a member (app)', async (w) => {
    const r = await people[w].post('/api/orgs/acme/invites', { email: `new-${w}@acme.io`, role: 'member', teams: [teamA] }, tokens[w]);
    return r.status === 201;
  }],
  ['invite an owner (app)', async (w) => {
    const r = await people[w].post('/api/orgs/acme/invites', { email: `boss-${w}@acme.io`, role: 'owner', teams: [] }, tokens[w]);
    return r.status === 201;
  }],
  ['change a member\'s teams (app)', async (w) => {
    const r = await people[w].call('PATCH', `/api/orgs/acme/members/${people.guest.uid}`, { teams: [teamA, teamB] }, tokens[w]);
    await op('set members {teams: [$1]} where user = $2', [teamA, people.guest.uid]);
    return r.status === 200;
  }],
  ['create a team (app)', async (w) => (await people[w].post('/api/orgs/acme/teams', { name: `Team of ${w}` }, tokens[w])).status === 201],
];

const ROLES = ['owner', 'admin', 'member', 'guest', 'outsider'] as const;
const Y = true;
const N = false;
// owner, admin, member, guest, someone of another organisation
const expected: Record<string, boolean[]> = {
  'read tasks of their own team': [Y, Y, Y, Y, N],
  'read tasks of another team': [Y, Y, N, N, N],
  'create a task in their team': [Y, Y, Y, N, N],
  'create a task in another team': [Y, Y, N, N, N],
  'edit a task\'s title': [Y, Y, Y, N, N],
  'move a task to another team': [Y, Y, N, N, N],
  'delete a task': [Y, Y, N, N, N],
  'comment as themselves': [Y, Y, Y, Y, N],
  'comment as someone else': [N, N, N, N, N],
  'edit their own comment': [Y, Y, Y, Y, N],
  'edit someone else\'s comment': [N, N, N, N, N],
  'delete a comment': [Y, Y, N, N, N],
  'read the member list': [Y, Y, Y, Y, N],
  'change a guest\'s role': [Y, Y, N, N, N],
  'make someone an owner': [Y, N, N, N, N],
  'change an owner\'s role': [Y, N, N, N, N],
  'read the audit log': [Y, Y, N, N, N],
  'write an audit row naming themselves': [Y, Y, N, N, N],
  'write an audit row naming someone else': [N, N, N, N, N],
  'rewrite or delete an audit row': [N, N, N, N, N],
  'read invitations': [Y, Y, N, N, N],
  'read another organisation': [N, N, N, N, Y],
  'read the accounts tenant': [N, N, N, N, N],
  'change the schema': [N, N, N, N, N],
  'read the change stream': [N, N, N, N, N],
  'invite a member (app)': [Y, Y, N, N, N],
  'invite an owner (app)': [N, N, N, N, N],
  'change a member\'s teams (app)': [Y, Y, N, N, N],
  'create a team (app)': [Y, Y, N, N, N],
};

test('every operation, every role: the matrix', async () => {
  const lines = ['| operation | owner | admin | member | guest | other org |', '|---|---|---|---|---|---|'];
  const wrong: string[] = [];
  for (const [name, check] of checks) {
    const row: string[] = [];
    for (const [i, role] of ROLES.entries()) {
      const got = await check(role);
      row.push(got ? 'yes' : 'no');
      if (got !== expected[name][i]) wrong.push(`${name} / ${role}: ${got ? 'allowed' : 'refused'}`);
    }
    lines.push(`| ${name} | ${row.join(' | ')} |`);
  }
  console.log(lines.join('\n'));
  assert.deepEqual(wrong, []);
});

test('a member edits a task but not its team, even between two teams of theirs', async () => {
  const id = await task(teamA);
  const t = tokens.both;
  assert.equal((await query(env, T, t, 'set tasks {title: "Max was here", status: "doing"} where id = $1', [id])).status, 200);
  const moved = await query(env, T, t, 'set tasks {team: $2} where id = $1', [id, teamB]);
  assert.equal(moved.status, 403, moved.text);
  // Through REST and /batch too: the grant judges the fields, by any route.
  const patch = await direct(env, 'PATCH', `/t/${T}/tasks?id=eq.${id}`, t, { team: teamB });
  assert.equal(patch.status, 403, patch.text);
  const batch = await fetch(`${env.cfg.routerUrl}/t/${T}/batch`, {
    method: 'POST',
    headers: { authorization: `Bearer ${t}`, 'content-type': 'application/x-ndjson' },
    body: [{ query: 'set tasks {title: "both"} where id = $1', params: [id] }, { query: 'set tasks {team: $2} where id = $1', params: [id, teamB] }]
      .map((l) => JSON.stringify(l))
      .join('\n'),
  });
  assert.equal(batch.status, 403);
  const [row] = await opRows<{ team: string; title: string }>('get tasks select team, title where id = $1', [id]);
  assert.deepEqual(row, { team: teamA, title: 'Max was here' }, 'the batch was put back whole');
});

test('an insert pinned to its author: a member\'s task is theirs even when they leave the field out', async () => {
  assert.equal((await query(env, T, tokens.member, 'insert tasks {team: $1, title: "Pinned", status: "backlog", updated: now()}', [teamA])).status, 200);
  const [row] = await opRows<{ author: string }>('get tasks select author where title = "Pinned"');
  assert.equal(row.author, people.member.uid);
});

test('an invitation works once, for its address, until it lapses', async () => {
  const owner = people.owner;
  const t = await owner.token('acme');
  // Reused: the second redemption writes nothing and puts its block back.
  const nina = await person(env, 'nina@acme.io', 'Nina');
  assert.equal((await owner.post('/api/orgs/acme/invites', { email: 'nina@acme.io', role: 'member', teams: [teamA] }, t)).status, 201);
  const code = (await mailedLink(env, 'nina@acme.io', '/invite/acme/')).split('/').pop()!;
  await nina.refresh();
  assert.equal((await nina.post('/api/invites/accept', { org: 'acme', code }, nina.api)).status, 200);
  const omar = await person(env, 'omar@acme.io', 'Omar');
  const reused = await omar.post('/api/invites/accept', { org: 'acme', code }, omar.api);
  assert.equal(reused.status, 403, 'another address');
  const again = await nina.post('/api/invites/accept', { org: 'acme', code }, nina.api);
  assert.equal(again.status, 409);
  assert.equal(again.body.error, 'This invitation was used already.');
  assert.equal((await opRows('get members where email = "nina@acme.io"')).length, 1);
  assert.equal((await opRows('get redemptions where invite = $1', [digest(code)])).length, 1);

  // Lapsed: an invitation made 73 hours ago is out of every read (@ttl(72h)).
  const late = 'lapsed-invite-code';
  await op('insert invites {hash: $1, email: "omar@acme.io", role: "member", teams: [], by: "x", at: now() - 262800000}', [digest(late)]);
  const r = await omar.post('/api/invites/accept', { org: 'acme', code: late }, omar.api);
  assert.equal(r.status, 410);

  // The same code redeemed twice at once: exactly one joins.
  const pair = [await person(env, 'twin@acme.io', 'Twin'), null];
  assert.equal((await owner.post('/api/orgs/acme/invites', { email: 'twin@acme.io', role: 'guest', teams: [teamA] }, t)).status, 201);
  const twinCode = (await mailedLink(env, 'twin@acme.io', '/invite/acme/')).split('/').pop()!;
  const twin = pair[0]!;
  await twin.refresh();
  const both = await Promise.all(Array.from({ length: 6 }, () => twin.post('/api/invites/accept', { org: 'acme', code: twinCode }, twin.api)));
  assert.equal(both.filter((a) => a.status === 200).length, 1);
  assert.equal((await opRows('get members where email = "twin@acme.io"')).length, 1);
});

test('a role change counts from the next refresh', async () => {
  const before = people.guest.access.get('acme')!.role;
  assert.equal(before, 'guest');
  const r = await people.admin.call('PATCH', `/api/orgs/acme/members/${people.guest.uid}`, { role: 'member' }, tokens.admin);
  assert.equal(r.status, 200);
  await people.guest.refresh('acme');
  assert.equal(people.guest.access.get('acme')!.role, 'member');
  const audit = await opRows<{ action: string; actor: string }>('get audit where action = "member.role" order id desc limit 1');
  assert.equal(audit[0].actor, people.admin.uid);
  await op('set members {role: "guest"} where user = $1', [people.guest.uid]);
});

test('a removed member gets no new token', async () => {
  const r = await people.admin.call('DELETE', `/api/orgs/acme/members/${people.both.uid}`, undefined, tokens.admin);
  assert.equal(r.status, 200);
  const again = await people.both.refresh('acme');
  assert.equal(again.status, 403);
  const orgs = ((await people.both.refresh()).body.orgs as { slug: string }[]).map((o) => o.slug);
  assert.ok(!orgs.includes('acme'));
});
