// Both runners of integrations/sync-scenarios.json ran every scenario, and
// every one held: the native core's (crates/fenec-abi/tests/scenarios.rs)
// and the browser's (web/fenec.sync.scenarios.test.js) each write the names
// that passed to target/sync-scenarios/. A runner that skipped, or a
// scenario one of them never reached, fails here rather than passing quietly.
//
//   node integrations/sync-scenarios-check.mjs   (make sync-scenarios-check)

import { readFileSync } from 'node:fs';

const root = new URL('../', import.meta.url);
const { scenarios } = JSON.parse(readFileSync(new URL('integrations/sync-scenarios.json', root), 'utf8'));
const names = scenarios.map((s) => s.name);
const problems = [];
const twice = names.filter((n, i) => names.indexOf(n) !== i);
if (twice.length) problems.push(`named twice: ${twice.join('; ')}`);

for (const runner of ['rust', 'js']) {
  let ran;
  try {
    ran = readFileSync(new URL(`target/sync-scenarios/${runner}.txt`, root), 'utf8').split('\n').filter(Boolean);
  } catch {
    problems.push(`${runner}: no target/sync-scenarios/${runner}.txt -- the runner did not run`);
    continue;
  }
  const missing = names.filter((n) => !ran.includes(n));
  const unknown = ran.filter((n) => !names.includes(n));
  if (missing.length) problems.push(`${runner}: ${missing.length} not passed:\n  ${missing.join('\n  ')}`);
  if (unknown.length) problems.push(`${runner}: ran what the file no longer holds:\n  ${unknown.join('\n  ')}`);
}

if (problems.length) {
  console.error(`sync scenarios: ${problems.join('\n')}`);
  process.exit(1);
}
const differ = scenarios.filter((s) => s.differs).length;
console.log(`sync scenarios: all ${names.length} held by both runners (${differ} where the platforms differ on purpose)`);
