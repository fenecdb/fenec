// Streams the tenant's journal into LEDGER_SINK_FILE until stopped:
// `npm run sink`. An operator's job, with the operator's token.
import { OPERATOR_TOKEN, SINK_FILE, TENANT, tenantUrl } from '../src/config.ts';
import { Sink } from '../src/sink.ts';

const sink = new Sink({ base: tenantUrl(), token: OPERATOR_TOKEN, file: SINK_FILE, wait: 5000 });
for (const s of ['SIGINT', 'SIGTERM'] as const) process.on(s, () => sink.stop());
console.log(`streaming ${TENANT}'s journal into ${SINK_FILE}`);
await sink.run((e) => console.error('sink:', e instanceof Error ? e.message : e));
sink.close();
