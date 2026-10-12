// `npm run simulate`: Merrow's couriers riding and its customers ordering,
// through the router of `npm run cluster`, until Ctrl-C. Every courier
// writes through its own token, as its phone would.
//
//   PLOVER_SPEED   how much faster than the clock (default 6)
//   PLOVER_HOUR    the hour of the day the orders follow (default: the dinner rush, 19.5)
//   PLOVER_BEAT    seconds between a courier's beats (default 2.5)
//   PLOVER_BUSY    orders this many times the company's usual rate (default 2)
import { ROUTER_URL } from '../lib/config';
import { routeTo } from '../lib/db';
import { runCompany } from '../lib/sim';
import { COMPANIES } from '../lib/world';

routeTo(ROUTER_URL);
const speed = Number(process.env.PLOVER_SPEED ?? 6);
const opts = {
  speed,
  beat: Number(process.env.PLOVER_BEAT ?? 2.5),
  busy: Number(process.env.PLOVER_BUSY ?? 2),
  hour: process.env.PLOVER_HOUR === 'now' ? undefined : Number(process.env.PLOVER_HOUR ?? 19.5),
  log: (s: string) => console.log(s),
};
const stop = new AbortController();
const runs = COMPANIES.map((c) => ({ c, ...runCompany(c, opts, stop.signal) }));
console.log(`Merrow at ${speed}x: ${COMPANIES.map((c) => `${c.name} (${c.couriers} couriers)`).join(', ')}`);
const every = setInterval(() => {
  for (const { c, stats } of runs) console.log(`${c.slug.padEnd(7)} ${JSON.stringify(stats)}`);
}, 10_000);
const end = async () => {
  clearInterval(every);
  stop.abort();
  await Promise.race([Promise.all(runs.map((r) => r.done)), new Promise((r) => setTimeout(r, 3000))]);
  process.exit(0);
};
process.on('SIGINT', end);
process.on('SIGTERM', end);
