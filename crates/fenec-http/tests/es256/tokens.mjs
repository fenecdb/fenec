// Writes tokens.txt: a JWKS as an identity provider publishes one -- two
// P-256 keys, an RSA key, a P-384 key and an Ed25519 key -- and tokens
// node:crypto signed with their private halves, which tests/es256.rs holds
// fenec-server's verifier to. A library outside the crates signs them, so
// the verifier is not checked against itself; the private keys live only
// in this process and are thrown away with it.
//
//   node crates/fenec-http/tests/es256/tokens.mjs > crates/fenec-http/tests/es256/tokens.txt
//
// Each line after the JWKS is `<name> <token>`. Every token's `exp` is
// 4102444800 (2100-01-01), so the server under test takes it on the real
// clock.
import { generateKeyPairSync, sign } from 'node:crypto';

const b64 = (b) => Buffer.from(b).toString('base64url');
const pair = (type, options) => generateKeyPairSync(type, options);

const e1 = pair('ec', { namedCurve: 'P-256' });
const e2 = pair('ec', { namedCurve: 'P-256' });
const r1 = pair('rsa', { modulusLength: 2048 });
const p384 = pair('ec', { namedCurve: 'P-384' });
const ed = pair('ed25519');

const pub = (k, extra) => ({ ...k.publicKey.export({ format: 'jwk' }), ...extra });
const jwks = {
  keys: [
    pub(e1, { kid: 'e1', alg: 'ES256', use: 'sig' }),
    // No `alg`: a key's kind and curve say what it verifies.
    pub(e2, { kid: 'e2', use: 'sig' }),
    pub(r1, { kid: 'r1', alg: 'RS256', use: 'sig' }),
    pub(p384, { kid: 'e384', alg: 'ES384', use: 'sig' }),
    pub(ed, { kid: 'ed', alg: 'EdDSA', use: 'sig' }),
  ],
};

const EXP = 4102444800;

/// A token of `header` and `claims`: `hash` and `key` as `crypto.sign`
/// takes them, the signature `R || S` unless told DER.
function token(header, claims, hash, key, dsaEncoding = 'ieee-p1363') {
  const input = `${b64(JSON.stringify(header))}.${b64(JSON.stringify(claims))}`;
  const sig = sign(hash, Buffer.from(input), { key: key.privateKey, dsaEncoding });
  return `${input}.${b64(sig)}`;
}

const lines = [
  // Named by its kid.
  ['alice', token({ alg: 'ES256', typ: 'JWT', kid: 'e1' }, { sub: 'alice', exp: EXP }, 'sha256', e1)],
  // No kid: checked against each ES256 key, e2 the second.
  ['bob', token({ alg: 'ES256', typ: 'JWT' }, { sub: 'bob', exp: EXP }, 'sha256', e2)],
  // An RS256 token beside them, by its kid: one file, both kinds.
  ['carol', token({ alg: 'RS256', typ: 'JWT', kid: 'r1' }, { sub: 'carol', exp: EXP }, 'sha256', r1)],
  // e1's signature under e2's kid.
  ['other-kid', token({ alg: 'ES256', kid: 'e2' }, { sub: 'alice', exp: EXP }, 'sha256', e1)],
  // e1's signature naming the RSA key: a key verifies its own algorithm.
  ['rsa-kid', token({ alg: 'ES256', kid: 'r1' }, { sub: 'alice', exp: EXP }, 'sha256', e1)],
  // The signature DER-encoded, as X.509 and TLS write ECDSA, not JWS.
  ['der', token({ alg: 'ES256', kid: 'e1' }, { sub: 'alice', exp: EXP }, 'sha256', e1, 'der')],
  // Algorithms this server does not take, signed right.
  ['es384', token({ alg: 'ES384', kid: 'e384' }, { sub: 'alice', exp: EXP }, 'sha384', p384)],
  ['eddsa', token({ alg: 'EdDSA', kid: 'ed' }, { sub: 'alice', exp: EXP }, null, ed)],
];

console.log(JSON.stringify(jwks));
for (const [name, t] of lines) console.log(`${name} ${t}`);
