// What Plover reads from its environment. The defaults are for development
// on one machine only: set every secret in a deployment.
import { fileURLToPath } from 'node:url';

const env = process.env;
const here = (p: string) => fileURLToPath(new URL(p, import.meta.url));

/** fenec-shard, as this server reaches it. */
export const ROUTER_URL = (env.PLOVER_ROUTER ?? 'http://127.0.0.1:8090').replace(/\/$/, '');
/** fenec-shard, as a browser reaches it: the live map and tracking talk to it directly. */
export const PUBLIC_ROUTER_URL = (env.PLOVER_PUBLIC_ROUTER ?? ROUTER_URL).replace(/\/$/, '');
/** The router's own token: places tenants and sets their quotas. setup.ts alone uses it. */
export const SHARD_TOKEN = env.PLOVER_SHARD_TOKEN ?? 'plover-dev-shard';
/** The nodes' --http-token, the operator's: setup and the tests' checks, never a request's. */
export const OPERATOR_TOKEN = env.PLOVER_OPERATOR_TOKEN ?? 'plover-dev-operator';
/** Signs every JSON Web Token the nodes take (HS256), and the session cookies. */
export const JWT_SECRET = env.PLOVER_JWT_SECRET ?? 'plover-dev-jwt-secret-of-at-least-32-bytes!';
export const COOKIE_SECRET = env.PLOVER_COOKIE_SECRET ?? 'plover-dev-cookie-secret-of-32-bytes-or-more';
/** The site's own origin, for CORS on the nodes and for cookies. */
export const SITE_ORIGIN = (env.PLOVER_ORIGIN ?? 'http://localhost:3000').replace(/\/$/, '');
export const INSECURE_COOKIES = SITE_ORIGIN.startsWith('http://');

/** The platform's tenant: accounts and companies. */
export const PLATFORM = 'plover';
/** A company's tenant. */
export const tenantOf = (slug: string) => `co-${slug}`;

/** How long a claim holds an order before another courier may take it, unless extended. */
export const LEASE_MS = Number(env.PLOVER_LEASE_MS ?? 45_000);
/** How long a claim waits at the server for an order when none is near (Fenec-Wait, 30 s at most). */
export const CLAIM_WAIT_MS = Number(env.PLOVER_CLAIM_WAIT_MS ?? 25_000);
/** How far a courier will ride to a pickup, metres. */
export const CLAIM_RADIUS_M = Number(env.PLOVER_CLAIM_RADIUS ?? 3_000);
/** Attempts before an order is a dead letter, for a dispatcher to look at. */
export const MAX_ATTEMPTS = 5;
/** scrypt's cost: 2^15 in production, less where tests sign in many times. */
export const SCRYPT_N = Number(env.PLOVER_SCRYPT_N ?? 2 ** 14);

export const POLICY_FILE = here('../policy.txt');
export const DATA_DIR = env.PLOVER_DATA ?? here('../data');
