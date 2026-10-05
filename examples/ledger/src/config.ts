// What the ledger reads from its environment. The defaults are for
// development only: set every secret in a deployment.
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

export const FENEC_URL = (process.env.FENEC_URL ?? 'http://127.0.0.1:8080').replace(/\/$/, '');
/** The tenant this console serves; the node serves others beside it. */
export const TENANT = process.env.LEDGER_TENANT ?? 'acme';
/** fenec-server's base for one tenant. */
export const tenantUrl = (tenant = TENANT) => `${FENEC_URL}/t/${tenant}`;

/** The operator's token: setup, the sink, corrections. The console never holds it. */
export const OPERATOR_TOKEN = process.env.FENEC_TOKEN ?? 'ledger-dev-operator';
/** The node's admin token, which creates tenants. */
export const ADMIN_TOKEN = process.env.FENEC_ADMIN_TOKEN ?? 'ledger-dev-node-admin';
/** Signs the console's session cookie. */
export const SESSION_SECRET = process.env.LEDGER_SESSION_SECRET ?? 'ledger-dev-session-secret';
/** Transfers an account may make in a minute. */
export const RATE_LIMIT = Number(process.env.LEDGER_RATE_LIMIT ?? 30);
/** Where the journal sink writes. */
export const SINK_FILE = process.env.LEDGER_SINK_FILE ?? fileURLToPath(new URL('../data/journal-sink.ndjson', import.meta.url));

export const SCHEMA = readFileSync(new URL('../schema.fenecql', import.meta.url), 'utf8');
export const CURRENCIES = ['EUR', 'GBP', 'USD'] as const;
