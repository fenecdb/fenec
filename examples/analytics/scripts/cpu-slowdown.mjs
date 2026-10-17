// The CPU slowdown that runs this machine's Chrome at the speed of the
// device the budgets are for (lighthouserc.cjs), printed for Lighthouse's
// `throttling.cpuSlowdownMultiplier`. Applied throttling multiplies the
// machine's own speed, so a constant slowdown measures a different device
// on every machine, and GitHub's runners come in CPUs far apart.
//
// The speed is Lighthouse's own benchmark (`benchmarkIndex`, in every
// report's `environment`), run in a Chrome launched as Lighthouse launches
// one: unthrottled, then under the slowdown worked out from it, which is
// corrected until the throttled benchmark lands on the target -- the way
// DevTools' calibrated CPU throttling finds its rates.
//
//   LHCI_CPU_TARGET  the benchmark the throttled CPU should score (lighthouserc.cjs)
//   CHROME_PATH      Chrome or Chromium, when it is not where Lighthouse looks
import { pageFunctions } from 'lighthouse/core/lib/page-functions.js';
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';

const require = createRequire(import.meta.url);
// Lighthouse's own chrome-launcher, whose default flags (no component
// updates, no background networking) are those its runs have.
const lighthouse = createRequire(require.resolve('lighthouse/package.json'));
const { launch } = await import(pathToFileURL(lighthouse.resolve('chrome-launcher')).href);
const rc = require('../lighthouserc.cjs');
const target = rc.cpuTarget;
const chromeFlags = rc.ci.collect.settings.chromeFlags.split(' ');

const median = (xs) => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];
// Chrome takes the rate to the hundredth.
const hundredths = (x) => Math.round(x * 100) / 100;

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
  // A second each, and three a reading: one is as noisy as the runner.
  const bench = async () =>
    (await send('Runtime.evaluate', { expression: `(${pageFunctions.computeBenchmarkIndex})()`, returnByValue: true })).result
      .value;
  const reading = async (rate) => {
    await send('Emulation.setCPUThrottlingRate', { rate });
    const xs = [];
    for (let i = 0; i < 3; i++) xs.push(await bench());
    return median(xs);
  };

  await bench(); // V8 warms up
  const index = await reading(1);
  let rate = hundredths(Math.max(1, index / target));
  let throttled = rate > 1 ? await reading(rate) : index;
  // The throttled score is close to the unthrottled one over the rate, not
  // exactly it: correct twice at most, as the score over the rate says.
  for (let i = 0; i < 2 && rate > 1 && Math.abs(throttled - target) > target * 0.03; i++) {
    rate = hundredths(Math.max(1, (rate * throttled) / target));
    throttled = await reading(rate);
  }
  if (index < target)
    console.error(`cpu: benchmarkIndex ${index} is under the target ${target}: this machine is slower than the device, not slowed`);
  console.error(`cpu: benchmarkIndex ${index}, slowed ${rate}x scores ${throttled} (target ${target})`);
  console.log(rate);
} finally {
  ws?.close();
  chrome.kill();
}
