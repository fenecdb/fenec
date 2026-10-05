// `npm run seed`: a demo organisation, Lumen Studio, with three teams, six
// people in four roles, and a board's worth of tasks and comments -- made
// through the app's API and each person's own token, as people would.
// Every demo account's password is DEMO_PASSWORD.
import { config } from '../src/config.ts';
import { Browser, mailedLink, type Env } from '../test/harness.ts';
import { Trellis } from '../src/trellis.ts';
import { loadOrMakeKey } from '../src/jwt.ts';

export const DEMO_PASSWORD = 'trellis demo password';

export async function seed(app: string, trellis: Trellis): Promise<void> {
  const env = { app, trellis, cfg: trellis.cfg } as unknown as Env;
  const existing = await trellis.accounts.one('get orgs where slug = "lumen" limit 1');
  if (existing) return;
  const join = async (email: string, name: string) => {
    const b = new Browser(env, `192.0.2.${Math.floor(Math.random() * 200) + 1}`);
    await b.signUp(email, name, DEMO_PASSWORD);
    const link = await mailedLink(env, email, '/verify\\?token=');
    await b.post('/api/auth/verify', { token: new URL(link, 'http://x').searchParams.get('token') });
    const r = await b.signIn(email, DEMO_PASSWORD);
    if (r.status !== 200) throw new Error(`seed sign-in ${email}: ${r.status}`);
    return b;
  };
  const ines = await join('ines@lumen.studio', 'Ines Okafor');
  await ines.post('/api/orgs', { name: 'Lumen Studio', slug: 'lumen' }, ines.api);
  let t = await ines.token('lumen');
  const product = ines.access.get('lumen')!.teams[0];
  // The first team is "General"; rename it to what this studio calls it.
  const db = trellis.orgAs('lumen', t);
  await db.batch([['set teams {name: "Product"} where key = $1', [product]]]);
  const design = (await ines.post('/api/orgs/lumen/teams', { name: 'Design' }, t)).body.key as string;
  const field = (await ines.post('/api/orgs/lumen/teams', { name: 'Field ops' }, t)).body.key as string;
  t = await ines.token('lumen');

  const people: [string, string, string, string[]][] = [
    ['theo@lumen.studio', 'Theo Lindqvist', 'admin', [product, design, field]],
    ['mara@lumen.studio', 'Mara Quint', 'member', [product, design]],
    ['dev@lumen.studio', 'Devika Rao', 'member', [product]],
    ['sol@lumen.studio', 'Sol Abernathy', 'member', [field]],
    ['jun@client.example', 'Jun Park', 'guest', [product]],
  ];
  const by: Record<string, Browser> = { ines };
  const names: Record<string, string> = { ines: 'Ines Okafor' };
  for (const [email, name, role, teams] of people) {
    const b = await join(email, name);
    await ines.post('/api/orgs/lumen/invites', { email, role, teams }, t);
    const code = (await mailedLink(env, email, '/invite/lumen/')).split('/').pop();
    await b.refresh();
    await b.post('/api/invites/accept', { org: 'lumen', code }, b.api);
    by[email.split('@')[0]] = b;
    names[email.split('@')[0]] = name;
  }

  const tasks: [string, string, string, string, string | null, number, string][] = [
    ['mara', product, 'Onboarding checklist for new workspaces', 'doing', 'mara', 1, 'Five steps, each one a real action: name the team, invite two people, add three tasks, connect the calendar, pick a theme.'],
    ['dev', product, 'Sync due dates with Google Calendar', 'backlog', 'dev', 0, 'Two-way, per person. Start with read-only export as an ICS feed.'],
    ['mara', product, 'Rewrite the empty board copy', 'review', 'mara', 0, 'Say what to do next instead of "No tasks".'],
    ['dev', product, 'Keyboard shortcuts for moving cards', 'backlog', null, 0, 'J/K to move focus, [ and ] to change lane.'],
    ['ines', product, 'Pricing page: team and studio plans', 'doing', 'ines', 1, 'Two plans, monthly and yearly, VAT shown for EU visitors.'],
    ['dev', product, 'Export a board as CSV', 'done', 'dev', 0, 'All fields, ISO dates, one row per task.'],
    ['mara', product, 'Accessibility pass on the task drawer', 'review', 'dev', 0, 'Focus returns to the card on close; labels on every field.'],
    ['mara', design, 'Illustrations for the welcome emails', 'doing', 'mara', 0, 'Three spot illustrations in the lattice style, light and dark.'],
    ['theo', design, 'Icon set for task types', 'backlog', null, 0, 'Bug, chore, idea, request. 20 px grid.'],
    ['sol', field, 'Install kiosks at the Malmö venue', 'doing', 'sol', 1, 'Four kiosks, power at the north wall only.'],
    ['sol', field, 'Book the van for the Lisbon fair', 'backlog', 'sol', 0, 'Pick up Thursday, return Monday morning.'],
    ['theo', field, 'Venue walkthrough checklist', 'done', 'sol', 0, 'Exits, power, Wi-Fi, loading bay, quiet room.'],
  ];
  const ids: number[] = [];
  for (const [who, team, title, status, assignee, priority, body] of tasks) {
    const tok = await by[who].token('lumen');
    const d = trellis.orgAs('lumen', tok);
    const assigneeUid = assignee ? by[assignee].uid : null;
    const r = await d.batch([
      ['insert tasks {team: $1, title: $2, body: $3, status: $4, assignee: $5, priority: $6, author: $7, updated: now()}', [team, title, body, status, assigneeUid, priority, by[who].uid]],
    ]);
    if (!r.ok) throw new Error(`seed task: ${r.error}`);
    const [row] = await d.rows<{ id: number }>('get tasks select id where title = $1 limit 1', [title]);
    ids.push(row.id);
  }
  const comments: [string, number, string][] = [
    ['dev', 0, 'Step four needs the calendar sync first, or we hide it until then.'],
    ['mara', 0, 'Hiding it. I will add it back when the sync ships.'],
    ['jun', 0, 'From our side: please keep the checklist dismissible.'],
    ['ines', 4, 'Legal wants the VAT note above the fold.'],
    ['sol', 9, 'Power is confirmed for Wednesday. Kiosks arrive Thursday 9:00.'],
  ];
  for (const [who, i, body] of comments) {
    const tok = await by[who].token('lumen');
    const r = await trellis.orgAs('lumen', tok).batch([
      ['insert comments {task: $1, team: $2, name: $3, body: $4, at: now()}', [ids[i], tasks[i][1], names[who], body]],
    ]);
    if (!r.ok) throw new Error(`seed comment: ${r.error}`);
  }
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const cfg = config();
  const { signing, jwks } = loadOrMakeKey(cfg.keysDir, 'app', 'trellis-app-1');
  await seed(cfg.origin, new Trellis(cfg, signing, jwks.keys));
  console.log(`seeded: sign in as ines@lumen.studio with "${DEMO_PASSWORD}"`);
}
