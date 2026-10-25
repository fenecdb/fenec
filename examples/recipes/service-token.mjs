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
// The identity is `sub`, not a `role`: a provider decides what `sub` is, and a
// person cannot choose it, where a `role` claim a provider lets people set
// would be theirs to claim. Name it so that no provider's form (`user_...`,
// `auth0|...`, a UUID) is it.
import { createPrivateKey, createSign } from 'node:crypto';

const b64 = (v) => Buffer.from(typeof v === 'string' ? v : JSON.stringify(v)).toString('base64url');

/** `() => Promise<token>`: signed with `pem`, kept until a minute before it lapses. */
export function serviceTokens({ pem, sub, kid = 'svc-1', ttl = 300 }) {
  const key = createPrivateKey(pem);
  let kept = null;
  return async () => {
    const now = Math.floor(Date.now() / 1000);
    if (kept && kept.exp - 60 > now) return kept.token;
    const head = b64({ alg: 'RS256', typ: 'JWT', kid });
    const body = b64({ sub, iat: now, exp: now + ttl });
    const sig = createSign('RSA-SHA256').update(`${head}.${body}`).sign(key).toString('base64url');
    kept = { token: `${head}.${body}.${sig}`, exp: now + ttl };
    return kept.token;
  };
}
