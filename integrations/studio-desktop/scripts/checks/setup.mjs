// The files the checks open: small.fenec (the one that is edited), held.fenec
// (a second one, which a check holds from outside the app), remote.fenec (the
// one behind a "remote" server). `bun scripts/checks/setup.mjs`
import { spawnSync } from 'node:child_process';
import { D, HERE, SERVER } from './drive.mjs';

for (const [name, rows] of [['small', 50], ['held', 10], ['remote', 30]]) {
  const r = spawnSync('bun', [`${HERE}scripts/mkdata.mjs`, `${D}/data/${name}.fenec`, String(rows)], { env: { ...process.env, FENEC_SERVER_BIN: SERVER }, stdio: 'inherit' });
  if (r.status !== 0) process.exit(r.status ?? 1);
}
console.log(`data in ${D}/data; the "studio is off" check also wants an older server at $OLD_SERVER (a v0.1.10 release binary, which has no --studio)`);
