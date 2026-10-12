// `npm run grafana-token`: a token a company for Grafana's data sources
// (grafana/provisioning), into data/grafana/<company>.jwt -- role `app`,
// which reads and writes nothing, for a day. Run it again when they lapse.
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { DATA_DIR, tenantOf } from '../lib/config';
import { mint } from '../lib/tokens';
import { COMPANIES } from '../lib/world';

const dir = join(DATA_DIR, 'grafana');
mkdirSync(dir, { recursive: true });
for (const c of COMPANIES) {
  const file = join(dir, `${c.slug}.jwt`);
  writeFileSync(file, mint({ sub: 'grafana', role: 'app', tenant: tenantOf(c.slug) }, 86_400), { mode: 0o644 });
  console.log(file);
}
