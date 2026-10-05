// Makes the tenants ready and writes the demo's history: `npm run setup`
// after `npm run db`.
//
//   KESTREL_SEED_DAYS   90    days of invented traffic for each demo site
//   KESTREL_SEED_SCALE  1     new visitors a day: 900 for Fieldnotes, 150 for Tidepool, times this
//
// The history goes straight into the raw events with the node's token; the
// rollup worker (in `npm start`) folds it from the change stream. Events
// older than 30 days leave the raw collection as they land (@ttl) and live
// on in the rollups alone, as they would after a month of real traffic.
import { addSite, addUser, DEMO_PASSWORD, DEMO_SITES, setupControl, writeEvents } from '../src/setup.ts';
import { db } from '../src/db.ts';
import { TickGenerator, writeTicks } from '../src/market.ts';
import { MARKETS } from '../src/config.ts';
import { DAY_MS, Traffic } from '../src/sim.ts';
import { dayOf, MINUTE } from '../src/time.ts';

const days = Number(process.env.KESTREL_SEED_DAYS ?? 90);
const scale = Number(process.env.KESTREL_SEED_SCALE ?? 1);

await setupControl();
for (const s of DEMO_SITES) await addSite(s);
await addUser('nadia', 'Nadia Ferreira', DEMO_PASSWORD, ['fieldnotes']);
await addUser('omar', 'Omar Lindqvist', DEMO_PASSWORD, ['tidepool']);
await addUser('ops', 'Ingrid Sato', DEMO_PASSWORD, ['fieldnotes', 'tidepool']);

const profiles = { fieldnotes: { daily: 900, growth: 0.006, seed: 11, prefix: 'f' }, tidepool: { daily: 150, growth: 0.002, seed: 23, prefix: 't' } };
const now = Date.now();
for (const s of DEMO_SITES) {
  const [has] = await db(s.name, 'operator').rows('get rollup_state select events limit 1');
  const [raw] = await db(s.name, 'operator').rows('get events select count(*) as n');
  if ((has && Number(has.events) > 0) || Number(raw?.n) > 0) {
    console.log(`${s.name}: has events already`);
    continue;
  }
  const p = profiles[s.name as keyof typeof profiles];
  const traffic = new Traffic({ ...p, daily: Math.max(1, Math.round(p.daily * scale)) });
  let total = 0;
  const start = dayOf(now) - days * DAY_MS;
  for (let i = 0; i <= days; i++) {
    // Today up to now only: what has not happened yet is not history.
    const events = traffic.day(start + i * DAY_MS, i).filter((e) => e.at < now - MINUTE);
    total += events.length;
    await writeEvents(s.name, events);
  }
  console.log(`${s.name}: ${total} events over ${days} days`);
}

// Two hours of ticks, so the market page has bars before the live feed starts.
const [q] = await db(MARKETS, 'operator').rows('get quotes select count(*) as n');
if (!Number(q?.n)) {
  const gen = new TickGenerator(42);
  const from = now - 2 * 3_600_000;
  for (let t = from; t < now; t += 60_000) await writeTicks(gen.ticks(t, Math.min(now, t + 60_000), 6000));
  console.log('markets: two hours of ticks for 50 symbols');
}
console.log(`ready; sign in as nadia (Fieldnotes), omar (Tidepool) or ops (both), password ${DEMO_PASSWORD}`);
