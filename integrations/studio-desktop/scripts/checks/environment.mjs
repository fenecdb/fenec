// What the server the app starts is given: the few variables a process needs
// and its token, whatever the app was started with. The server reads the
// environment for more than the token (OTEL_EXPORTER_OTLP_* turns trace export
// on, FENEC_JWT_SECRET changes what it accepts), and the app is started from
// Finder, from a shell with anything exported in it, or by another program.
//
// The test variant lets FENEC_SERVER_BIN name the server it starts; here that
// is a script that writes its own environment down and becomes the real one.
// (A shipped app does not read it: checks/release.mjs.)
import assert from 'node:assert/strict';
import { chmodSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { D, SERVER, assertClean, data, quit, startApp, step, waitFor, waitStudio, log } from './lib.mjs';

assertClean('before');
const SMALL = data('small', 20);
mkdirSync(`${D}/wrap`, { recursive: true });
const SEEN = `${D}/wrap/server-env.txt`;
const WRAP = `${D}/wrap/fenec-server`;
// `env -0` would be exact, and is not on every system; one a line is enough for names.
writeFileSync(WRAP, `#!/bin/sh\nenv > '${SEEN}'\nexec '${SERVER}' "$@"\n`);
chmodSync(WRAP, 0o755);

const LEAKS = {
  OTEL_EXPORTER_OTLP_ENDPOINT: 'http://127.0.0.1:9',
  OTEL_SERVICE_NAME: 'leak',
  FENEC_JWT_SECRET: 'leak-jwt',
  FENEC_HTTP_TOKEN: 'leak-old-token',
  AWS_SECRET_ACCESS_KEY: 'leak-aws',
  GITHUB_TOKEN: 'leak-gh',
  DYLD_INSERT_LIBRARIES: '',
};
const app = startApp('environment', { FENEC_SERVER_BIN: WRAP, FENEC_DESKTOP_OPEN: SMALL, ...LEAKS });
await waitFor((l) => l.what === 'server-started', 60000, 'the server started');
await waitStudio(`file:${SMALL}`);

step('the environment the server was started with');
const names = readFileSync(SEEN, 'utf8')
  .split('\n')
  .filter(Boolean)
  .map((l) => l.slice(0, l.indexOf('=')));
log('names:', names.join(' '));
for (const leak of ['OTEL_EXPORTER_OTLP_ENDPOINT', 'OTEL_SERVICE_NAME', 'FENEC_JWT_SECRET', 'AWS_SECRET_ACCESS_KEY', 'GITHUB_TOKEN', 'DYLD_INSERT_LIBRARIES', 'FENEC_DESKTOP_REPORT', 'FENEC_DESKTOP_CONTROL', 'FENEC_DESKTOP_OPEN', 'FENEC_SERVER_BIN']) {
  assert.ok(!names.includes(leak), `${leak} must not reach the server`);
}
const ALLOWED = /^(HOME|PATH|TMPDIR|LANG|USER|LOGNAME|TZ|LC_[A-Z]+|FENEC_HTTP_TOKEN|PWD|SHLVL|_|OLDPWD)$/;
assert.deepEqual(names.filter((n) => !ALLOWED.test(n)), [], 'only the allowlist (and what the wrapper script\'s own shell adds)');
const env = Object.fromEntries(readFileSync(SEEN, 'utf8').split('\n').filter(Boolean).map((l) => [l.slice(0, l.indexOf('=')), l.slice(l.indexOf('=') + 1)]));
assert.notEqual(env.FENEC_HTTP_TOKEN, 'leak-old-token', 'the app\'s own token, not one that was in its environment');
assert.match(env.FENEC_HTTP_TOKEN, /^[A-Za-z0-9_-]{43}$/, '32 random bytes, base64url');

step('quit');
await quit(app);
log('clean');
