// The short tokens a backend calls fenec-server with. The server verifies them
// against the keys file like any other, so the public half of this key is in
// that file beside the provider's (refresh-jwks.sh's third argument), and its
// `kid` is what a token names to be checked against it alone.
//
// Why a key of the service's own and not `--mint-token`: that flag signs with
// the first `oct` key of the file, and a file of a provider's RSA keys has
// none -- an RSA key's private half is its owner's. A token minted by hand and
// kept would also be refused by `--jwt-max-age` and, if it never expires,
// by the default of `--jwt-require-exp`. A token signed on demand lives five
// minutes.
//
// The identity is `sub` and an `iss` only your services use, never a `role`. A
// provider decides what a `sub` is and a person cannot choose it, but a provider
// is not the only thing that signs a token the file's keys verify: a Firebase
// project's owner picks the uid of a custom token, and a `sub` named
// `files-backend` from a provider would be this service to a rule that asked for
// the `sub` alone. So every rule that means a service asks for the issuer too
// (`$jwt.sub = 'files-backend' and $jwt.iss = 'urn:fenec:services'`), which no
// provider's token carries. Name the `sub` so that no provider's form
// (`user_...`, `auth0|...`, a UUID) is it, as well.
//
// What this does not do: the server binds no key to a `sub` or an `iss`.
// Whoever holds the private key (or any key the file holds) can sign any `sub`,
// `role` or `iss` that a rule of the policy asks for. Keep it where only the
// service reads it (a Worker secret, a file only its user reads), one key for
// each service so that one can be dropped alone, and rotate it by adding the
// new key to the file, deploying the service with it and taking the old out.
// Binding a key to an issuer is not something the server does.
export const SERVICES = 'urn:fenec:services';

import { createPrivateKey, createSign } from 'node:crypto';

const b64 = (v) => Buffer.from(typeof v === 'string' ? v : JSON.stringify(v)).toString('base64url');

/**
 * `() => Promise<token>`: signed with `pem`, kept until a minute before it lapses.
 * `aud` is for a server told `--jwt-audience`, which refuses a token naming none
 * of its audiences; its `--jwt-issuer` must list `iss` too.
 */
export function serviceTokens({ pem, sub, kid = 'svc-1', ttl = 300, iss = SERVICES, aud }) {
  const key = createPrivateKey(pem);
  let kept = null;
  return async () => {
    const now = Math.floor(Date.now() / 1000);
    if (kept && kept.exp - 60 > now) return kept.token;
    const head = b64({ alg: 'RS256', typ: 'JWT', kid });
    const body = b64({ iss, sub, ...(aud ? { aud } : {}), iat: now, exp: now + ttl });
    const sig = createSign('RSA-SHA256').update(`${head}.${body}`).sign(key).toString('base64url');
    kept = { token: `${head}.${body}.${sig}`, exp: now + ttl };
    return kept.token;
  };
}
