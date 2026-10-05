// The rollup workers alone, one a site, until stopped: what `npm start`
// runs beside the dashboard, for a deployment that runs them apart.
//
//   KESTREL_ROLLUP_ONCE=1   stop once every site has caught up (for scripts and measurements)
import { RollupWorker } from '../src/rollup.ts';
import { Sites } from '../src/users.ts';

const once = process.env.KESTREL_ROLLUP_ONCE === '1';
const sites = await new Sites().all();
const t0 = performance.now();
await Promise.all(
  sites.map(async (s) => {
    const w = new RollupWorker({ site: s.name, wait: once ? 0 : 1000, onError: (e) => console.error(s.name, e) });
    if (!once) return w.run();
    await w.catchUp();
    console.log(`${s.name}: ${w.applied} events folded in ${((performance.now() - t0) / 1000).toFixed(1)} s, at change ${w.seq}`);
  }),
);
