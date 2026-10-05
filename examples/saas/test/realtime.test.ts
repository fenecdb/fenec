// Live boards and search, scoped: a subscription opened with one person's
// token hears of their teams' rows and of nothing else, a row leaving their
// teams is a deletion and then silence, and `match` ranks over the rows the
// token may read -- another team's 200 tasks holding the same word move no
// score of theirs. The same through the app's /db/ pipe.
import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { sseEvents } from './sse.ts';
import { Browser, join_, organisation, person, query, start, type Env } from './harness.ts';

let env: Env;
const T = 'o-acme';
let teamA: string;
let teamB: string;
let owner: Browser;
let mia: Browser;
let bea: Browser;
let miaT: string;
let beaT: string;

before(async () => {
  env = await start(19600);
  owner = await person(env, 'olivia@acme.io', 'Olivia');
  await organisation(owner, 'acme', 'Acme');
  await owner.refresh('acme');
  teamA = owner.access.get('acme')!.teams[0];
  teamB = (await owner.post('/api/orgs/acme/teams', { name: 'Design' }, await owner.token('acme'))).body.key as string;
  mia = await person(env, 'mia@acme.io', 'Mia');
  bea = await person(env, 'bea@acme.io', 'Bea');
  await join_(env, owner, 'acme', mia, 'mia@acme.io', 'member', [teamA]);
  await join_(env, owner, 'acme', bea, 'bea@acme.io', 'member', [teamB]);
  miaT = await mia.token('acme');
  beaT = await bea.token('acme');
});
after(() => env.stop());

interface Change {
  name: string;
  data: { rows?: { id: number; team: string; title: string }[]; puts?: { id: number; team: string; title: string }[]; dels?: number[] };
}

/** A subscription read into a list until stopped. */
function subscribe(url: string, token: string) {
  const ac = new AbortController();
  const events: Change[] = [];
  let seeded!: () => void;
  const ready = new Promise<void>((r) => (seeded = r));
  const done = (async () => {
    const res = await fetch(url, { headers: { authorization: `Bearer ${token}`, accept: 'text/event-stream' }, signal: ac.signal });
    assert.equal(res.status, 200, await (res.ok ? Promise.resolve('') : res.text()));
    try {
      for await (const ev of sseEvents(res)) {
        events.push({ name: ev.name, data: JSON.parse(ev.data) });
        if (ev.name === 'seed') seeded();
      }
    } catch (e) {
      if (!ac.signal.aborted) throw e;
    }
  })();
  return {
    events,
    ready,
    /** Waits until `pred` holds over what has come, for at most 10 s. */
    async until(pred: (e: Change[]) => boolean) {
      const t0 = Date.now();
      while (!pred(events)) {
        if (Date.now() - t0 > 10_000) throw new Error(`timed out; got ${JSON.stringify(events)}`);
        await new Promise((r) => setTimeout(r, 5));
      }
    },
    async stop() {
      ac.abort();
      await done.catch(() => {});
    },
  };
}

const rowsOf = (events: Change[]) => events.flatMap((e) => [...(e.data.rows ?? []), ...(e.data.puts ?? [])]);

test('a subscription hears its own teams and nothing else', async () => {
  const s = subscribe(`${env.cfg.routerUrl}/t/${T}/tasks/changes`, miaT);
  await s.ready;
  for (let i = 0; i < 20; i++) {
    assert.equal((await query(env, T, beaT, 'insert tasks {team: $1, title: $2, status: "backlog", updated: now()}', [teamB, `Secret ${i}`])).status, 200);
  }
  assert.equal((await query(env, T, miaT, 'insert tasks {team: $1, title: "Ours", status: "backlog", updated: now()}', [teamA])).status, 200);
  // A write Mia's subscription must hear arrives after all of Bea's: if any
  // of hers were going to leak, they would be in by now.
  await s.until((e) => rowsOf(e).some((r) => r.title === 'Ours'));
  await s.stop();
  const seen = rowsOf(s.events);
  assert.ok(seen.every((r) => r.team === teamA), JSON.stringify(seen));
  assert.ok(!JSON.stringify(s.events).includes('Secret'));
});

test('a task leaving the team is a deletion, then silence', async () => {
  const r = await query(env, T, miaT, 'insert tasks {team: $1, title: "Moving", status: "backlog", updated: now()}', [teamA]);
  assert.equal(r.status, 200);
  const [{ id }] = (await query(env, T, miaT, 'get tasks select id where title = "Moving"')).body as { id: number }[];
  const s = subscribe(`${env.cfg.routerUrl}/t/${T}/tasks/changes`, miaT);
  await s.ready;
  // The owner moves it to Design, then edits it there.
  assert.equal((await query(env, T, await owner.token('acme'), 'set tasks {team: $2} where id = $1', [id, teamB])).status, 200);
  await s.until((e) => e.some((x) => x.data.dels?.includes(id)));
  assert.equal((await query(env, T, beaT, 'set tasks {title: "Renamed in Design"} where id = $1', [id])).status, 200);
  assert.equal((await query(env, T, miaT, 'insert tasks {team: $1, title: "Marker", status: "backlog", updated: now()}', [teamA])).status, 200);
  await s.until((e) => rowsOf(e).some((x) => x.title === 'Marker'));
  await s.stop();
  assert.ok(!JSON.stringify(s.events).includes('Renamed in Design'));
});

test('a shape naming another team is still held to the token', async () => {
  const s = subscribe(`${env.cfg.routerUrl}/t/${T}/tasks/changes?team=eq.${teamB}`, miaT);
  await s.ready;
  assert.deepEqual(s.events[0].data.rows, []);
  await query(env, T, beaT, 'insert tasks {team: $1, title: "Still secret", status: "backlog", updated: now()}', [teamB]);
  await query(env, T, miaT, 'insert tasks {team: $1, title: "Not in this shape", status: "backlog", updated: now()}', [teamA]);
  // Nothing should arrive; the subscription closes on our side after a
  // write it would have heard if it were unscoped has landed.
  const seqAfter = (await query(env, T, beaT, 'get tasks where team = $1 count', [teamB])).status;
  assert.equal(seqAfter, 200);
  await s.stop();
  assert.ok(!JSON.stringify(s.events).includes('Still secret'));
});

test('a subscription for another organisation is refused', async () => {
  const res = await fetch(`${env.cfg.routerUrl}/t/accounts/users/changes`, { headers: { authorization: `Bearer ${miaT}` } });
  assert.equal(res.status, 403);
  await res.body?.cancel();
});

test('through the app\'s /db/ pipe: the same rules, the same stream', async () => {
  const s = subscribe(`${env.app}/db/t/${T}/tasks/changes`, miaT);
  await s.ready;
  await query(env, T, beaT, 'insert tasks {team: $1, title: "Piped secret", status: "backlog", updated: now()}', [teamB]);
  await query(env, T, miaT, 'insert tasks {team: $1, title: "Piped ours", status: "backlog", updated: now()}', [teamA]);
  await s.until((e) => rowsOf(e).some((r) => r.title === 'Piped ours'));
  await s.stop();
  assert.ok(!JSON.stringify(s.events).includes('Piped secret'));
  const viaPipe = await fetch(`${env.app}/db/t/${T}/query`, {
    method: 'POST',
    headers: { authorization: `Bearer ${miaT}`, 'content-type': 'application/json' },
    body: JSON.stringify({ query: 'get tasks where team = $1', params: [teamB] }),
  });
  assert.deepEqual(await viaPipe.json(), []);
  for (const path of ['/db/t/accounts/query', '/db/_shard/tenants', '/db/t/o-acme/../accounts/query', '/db/t/o-acme/_admin/tenants']) {
    const r = await fetch(`${env.app}${path}`, { method: 'POST', headers: { authorization: `Bearer ${miaT}` }, body: '{}' });
    assert.ok([403, 404].includes(r.status), `${path}: ${r.status}`);
  }
});

test('match ranks over the rows the token may read', async () => {
  const add = (token: string, team: string, title: string) =>
    query(env, T, token, 'insert tasks {team: $1, title: $2, status: "backlog", updated: now()}', [team, title]);
  assert.equal((await add(miaT, teamA, 'Book the offsite venue')).status, 200);
  assert.equal((await add(miaT, teamA, 'Draft the agenda')).status, 200);
  const score = async (token: string) => {
    const r = await query(env, T, token, 'get tasks select title match title "offsite venue" limit 50');
    assert.equal(r.status, 200, r.text);
    return r.body as { title: string; _score: number }[];
  };
  const before_ = await score(miaT);
  const operator = env.cfg.operatorToken;
  const opBefore = (await score(operator)).find((r) => r.title === 'Book the offsite venue')!._score;
  for (let i = 0; i < 200; i++) assert.equal((await add(beaT, teamB, `Offsite venue shortlist ${i}`)).status, 200);
  const after_ = await score(miaT);
  assert.deepEqual(after_, before_, 'neither the rows nor the scores Mia sees moved');
  assert.ok(after_.every((r) => !r.title.startsWith('Offsite venue shortlist')));
  const opAfter = (await score(operator)).find((r) => r.title === 'Book the offsite venue')!._score;
  console.log(`  "Book the offsite venue": ${before_[0]._score} for Mia before and after; ${opBefore} -> ${opAfter} over the whole collection`);
  assert.notEqual(opAfter, opBefore, 'over the whole collection the score did move: the statistics are per scope');

  // A facet beside the match counts Mia's rows alone.
  const f = await query(env, T, miaT, 'get tasks match title "offsite" facet team limit 1');
  const facets = (f.body as { facets: Record<string, { value: string; count: number }[]> }).facets;
  assert.deepEqual(facets.team.map((x) => x.value), [teamA]);
});

test('comments are searched the same way', async () => {
  const [{ id }] = (await query(env, T, miaT, 'get tasks select id where team = $1 limit 1', [teamA])).body as { id: number }[];
  const [{ id: idB }] = (await query(env, T, beaT, 'get tasks select id where team = $1 limit 1', [teamB])).body as { id: number }[];
  await query(env, T, miaT, 'insert comments {task: $1, team: $2, body: "The quarterly numbers look fine", at: now()}', [id, teamA]);
  await query(env, T, beaT, 'insert comments {task: $1, team: $2, body: "The quarterly numbers are confidential", at: now()}', [idB, teamB]);
  const r = await query(env, T, miaT, 'get comments select body, highlight(body) match body "quarterly confidential"');
  const rows = r.body as { body: string; 'highlight(body)': [number, number][] }[];
  assert.deepEqual(rows.map((x) => x.body), ['The quarterly numbers look fine']);
});
