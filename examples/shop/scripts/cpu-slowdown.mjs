// The CPU slowdown that brings this machine's Chrome to the device the
// budgets are for (lighthouserc.cjs, `cpuTarget`), printed with the
// machine's benchmarkIndex for scripts/lighthouse.sh. Applied throttling
// slows the machine's own CPU by a factor, so a fixed factor measures a
// different device on every machine.
//
// The measure is Lighthouse's own benchmark -- the `benchmarkIndex` in
// every report's `environment` -- run in a Chrome launched as Lighthouse
// launches one, warmed up as DevTools' calibrated CPU throttling warms it
// (whose targets are scores of this benchmark): unthrottled, then under the
// slowdown worked out from it, which is corrected until the throttled score
// lands on the target.
//
//   LHCI_CPU_TARGET  another target than lighthouserc.cjs's (264: DevTools' low-tier phone)
//   CHROME_PATH      Chrome or Chromium, when it is not where Lighthouse looks
import { pageFunctions } from 'lighthouse/core/lib/page-functions.js';
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';

const require = createRequire(import.meta.url);
// Lighthouse's own chrome-launcher, whose default flags (no component
// updates, no background networking) its runs have; @lhci/cli's is older.
const lighthouse = createRequire(require.resolve('lighthouse/package.json'));
const { launch } = await import(pathToFileURL(lighthouse.resolve('chrome-launcher')).href);
const rc = require('../lighthouserc.cjs');
const target = rc.cpuTarget;
const chromeFlags = rc.ci.collect.settings.chromeFlags.split(' ');

const median = (xs) => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];
// Chrome takes the rate to the hundredth.
const hundredths = (x) => Math.round(x * 100) / 100;
const near = (a, b, within) => Math.abs(a - b) <= b * within;

const chrome = await launch({ chromeFlags });
let ws;
try {
  const pages = await (await fetch(`http://127.0.0.1:${chrome.port}/json/list`)).json();
  ws = new WebSocket(pages.find((p) => p.type === 'page').webSocketDebuggerUrl);
  await new Promise((ok, no) => {
    ws.onopen = ok;
    ws.onerror = () => no(new Error('could not reach Chrome over the DevTools protocol'));
  });
  let last = 0;
  const waiting = new Map();
  ws.onmessage = (e) => {
    const m = JSON.parse(e.data);
    waiting.get(m.id)?.(m);
    waiting.delete(m.id);
  };
  const send = (method, params = {}) =>
    new Promise((ok, no) => {
      const id = ++last;
      waiting.set(id, (m) => (m.error ? no(new Error(`${method}: ${m.error.message}`)) : ok(m.result)));
      ws.send(JSON.stringify({ id, method, params }));
    });
  const bench = async () =>
    (await send('Runtime.evaluate', { expression: `(${pageFunctions.computeBenchmarkIndex})()`, returnByValue: true })).result
      .value;
  // The median of three benchmarks, a second each: one is as noisy as the machine.
  const reading = async (rate) => {
    await send('Emulation.setCPUThrottlingRate', { rate });
    const xs = [];
    for (let i = 0; i < 3; i++) xs.push(await bench());
    return median(xs);
  };

  await bench(); // V8 warms up
  // Until two readings agree: at Chrome's first launch on a fresh runner
  // the readings scored 9 and 17% under those five minutes later (two
  // runners of ten), which would have slowed the runs too little.
  const unthrottled = [await reading(1)];
  while (unthrottled.length < 4 && !near(unthrottled.at(-1), unthrottled.at(-2) ?? 0, 0.05)) unthrottled.push(await reading(1));
  const index = unthrottled.at(-1);
  // The throttled score is close to the unthrottled one over the rate, not
  // exactly it: corrected twice at most, by what the score over the rate says.
  let rate = hundredths(Math.max(1, index / target));
  const throttled = [rate > 1 ? await reading(rate) : index];
  for (let i = 0; i < 2 && rate > 1 && !near(throttled.at(-1), target, 0.03); i++) {
    rate = hundredths(Math.max(1, (rate * throttled.at(-1)) / target));
    throttled.push(await reading(rate));
  }
  if (index < target) console.error(`cpu: this machine scores under the target ${target}, so it is measured unthrottled and slower than the device`);
  console.error(`cpu: benchmarkIndex ${unthrottled.join(', ')}; slowed ${rate}x it scores ${throttled.at(-1)} (target ${target})`);
  console.log(`${rate} ${index}`);
} finally {
  ws?.close();
  chrome.kill();
}
