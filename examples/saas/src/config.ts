// What Trellis reads from its environment. The defaults are for development
// on one machine only: set every secret in a deployment.
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const env = process.env;
const here = (p: string) => fileURLToPath(new URL(p, import.meta.url));

export interface Config {
  /** fenec-shard, as this server reaches it. */
  routerUrl: string;
  /** fenec-shard, as a browser reaches it (the board talks to it directly). */
  publicRouterUrl: string;
  /** The router's own token: places tenants. Only org creation uses it. */
  shardToken: string;
  /** The nodes' --http-token: applies a new tenant's schema, nothing else. */
  operatorToken: string;
  /** Where the signing key and the JWKS the nodes read live. */
  keysDir: string;
  /** This server's own origin, for redirects, cookies and CORS on the nodes. */
  origin: string;
  /** The tenant holding accounts, sessions and the security log. */
  accountsTenant: string;
  /** Seconds an access token lives. */
  accessTtl: number;
  /** Sign-in attempts a window (10 minutes): per address, and per account. */
  ipLimit: number;
  accountLimit: number;
  /** Take the client's address from X-Forwarded-For (behind a proxy you run). */
  trustProxy: boolean;
  /** Cookies without `Secure`, for plain http on localhost. */
  insecureCookies: boolean;
  /** scrypt's cost: 2^15 in production, less in tests that sign in thousands of times. */
  scryptN: number;
  /** The identity provider, for "Continue with Single sign-on". Empty: off. */
  oidcIssuer: string;
  oidcClientId: string;
  /** Show the development mailbox (/dev/mail): every mail the app "sent". */
  devMail: boolean;
}

export function config(over: Partial<Config> = {}): Config {
  const routerUrl = (env.TRELLIS_ROUTER ?? 'http://127.0.0.1:8090').replace(/\/$/, '');
  return {
    routerUrl,
    publicRouterUrl: (env.TRELLIS_PUBLIC_ROUTER ?? routerUrl).replace(/\/$/, ''),
    shardToken: env.TRELLIS_SHARD_TOKEN ?? 'trellis-dev-shard',
    operatorToken: env.TRELLIS_OPERATOR_TOKEN ?? 'trellis-dev-operator',
    keysDir: env.TRELLIS_KEYS ?? here('../data/keys'),
    origin: (env.TRELLIS_ORIGIN ?? 'http://127.0.0.1:3000').replace(/\/$/, ''),
    accountsTenant: 'accounts',
    accessTtl: Number(env.TRELLIS_ACCESS_TTL ?? 300),
    ipLimit: Number(env.TRELLIS_IP_LIMIT ?? 30),
    accountLimit: Number(env.TRELLIS_ACCOUNT_LIMIT ?? 8),
    trustProxy: env.TRELLIS_TRUST_PROXY === '1',
    insecureCookies: (env.TRELLIS_ORIGIN ?? 'http://').startsWith('http://'),
    scryptN: Number(env.TRELLIS_SCRYPT_N ?? 2 ** 15),
    oidcIssuer: (env.TRELLIS_OIDC_ISSUER ?? 'http://127.0.0.1:3001').replace(/\/$/, ''),
    oidcClientId: env.TRELLIS_OIDC_CLIENT ?? 'trellis',
    devMail: env.TRELLIS_DEV_MAIL !== '0',
    ...over,
  };
}

export const POLICY_FILE = here('../policy.txt');
export const ACCOUNTS_SCHEMA = readFileSync(here('../schema/accounts.fenecql'), 'utf8');
export const ORG_SCHEMA = readFileSync(here('../schema/org.fenecql'), 'utf8');
