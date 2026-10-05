// The tests' server, seeded, left running for a person to look at:
//
//   node studio/test/serve.mjs [rows]
//
// prints the studio's address and a token of each kind, and stops on
// Ctrl+C, its directory removed. The studio's files are embedded, so a
// change to them shows after `cargo build -p fenec-server` and a restart.

import { startServer, seed, jwt, TOKEN } from './harness.mjs';

const s = await startServer();
await seed(s.url, Number(process.argv[2] ?? 100_000));
console.log(`studio:  ${s.url}/_studio/`);
console.log(`server token:  ${TOKEN}`);
console.log(`alice:  ${jwt({ sub: 'alice', role: 'analyst', exp: Math.floor(Date.now() / 1000) + 8 * 3600 })}`);
const stop = async () => {
  await s.stop();
  process.exit(0);
};
process.on('SIGINT', stop);
process.on('SIGTERM', stop);
setInterval(() => {}, 1 << 30);
