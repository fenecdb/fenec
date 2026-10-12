// `npm run shots [dir]`: screenshots of a running Plover -- the cluster,
// `npm run simulate` and the site -- at 1440 and 390 px wide, signed in as
// each demo account, through headless Chrome's DevTools protocol (no
// browser library to install). CHROME_PATH names the browser; PLOVER_ORIGIN
// the site.
import { spawn } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { demoAccounts } from '../lib/demo';
import { DEMO_PASSWORD } from '../lib/world';

const out = process.argv[2] ?? 'data/shots';
const site = process.env.PLOVER_ORIGIN ?? 'http://localhost:3000';
const chrome = process.env.CHROME_PATH ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
mkdirSync(out, { recursive: true });

// DevTools on port 0: Chrome takes a port the system picks and writes it,
// a line of its own, into DevToolsActivePort in its profile -- a fixed one
// met any other Chrome, or anything else, on it.
const profile = mkdtempSync(join(tmpdir(), 'plover-chrome-'));
const proc = spawn(chrome, ['--headless=new', '--remote-debugging-port=0', `--user-data-dir=${profile}`, '--hide-scrollbars', 'about:blank'], { stdio: 'ignore' });
let targets: { type: string; webSocketDebuggerUrl: string }[] = [];
for (let i = 0; i < 100 && !targets.length; i++) {
  try {
    const port = /^(\d+)\n/.exec(readFileSync(join(profile, 'DevToolsActivePort'), 'utf8'))![1];
    targets = (await (await fetch(`http://127.0.0.1:${port}/json`)).json()).filter((t: { type: string }) => t.type === 'page');
  } catch {
    await new Promise((r) => setTimeout(r, 100));
  }
}
const ws = new WebSocket(targets[0].webSocketDebuggerUrl);
await new Promise((r) => ws.addEventListener('open', r));
let n = 0;
const waiting = new Map<number, (v: unknown) => void>();
ws.addEventListener('message', (e) => {
  const m = JSON.parse(String(e.data));
  if (m.id && waiting.has(m.id)) waiting.get(m.id)!(m.result ?? m.error);
});
const send = (method: string, params: object = {}) =>
  new Promise<Record<string, unknown>>((r) => {
    const id = ++n;
    waiting.set(id, r as (v: unknown) => void);
    ws.send(JSON.stringify({ id, method, params }));
  });
const evaluate = async (expression: string) => (await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true })) as { result?: { value?: unknown } };
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
async function waitFor(selector: string) {
  for (let i = 0; i < 150; i++) {
    const r = await evaluate(`!!document.querySelector(${JSON.stringify(selector)})`);
    if (r.result?.value) return;
    await sleep(100);
  }
  throw new Error(`no ${selector}`);
}
async function size(width: number, mobile: boolean) {
  await send('Emulation.setDeviceMetricsOverride', { width, height: mobile ? 844 : 900, deviceScaleFactor: mobile ? 3 : 2, mobile });
}
async function shot(name: string, wait = 1200) {
  await sleep(wait);
  const r = (await send('Page.captureScreenshot', { format: 'png' })) as { data: string };
  writeFileSync(join(out, `${name}.png`), Buffer.from(r.data, 'base64'));
  console.log(`${out}/${name}.png`);
}
const go = async (path: string, selector: string) => {
  await send('Page.navigate', { url: `${site}${path}` });
  await waitFor(selector);
};
const signIn = async (email: string) => {
  await go('/signin', 'input[name=email]');
  await evaluate(`(() => {
    document.querySelector('input[name=email]').value = ${JSON.stringify(email)};
    document.querySelector('input[name=password]').value = ${JSON.stringify(DEMO_PASSWORD)};
    document.querySelector('form.form button[type=submit]').click();
  })()`);
};
const signOut = () => evaluate(`fetch('/api/signout', {method: 'POST'})`);

process.on('exit', () => proc.kill());
await send('Page.enable');
const demo = demoAccounts().filter((d) => d.company === 'tandem');
const by = (role: string) => demo.find((d) => d.role === role)!;

await size(1440, false);
await go('/signin', 'input[name=email]');
await shot('signin-1440', 600);
await signIn(by('dispatcher').email);
await waitFor('.strip');
await shot('dispatch-1440', 3000);
// An order waiting -- or on the road, when none waits -- and the couriers
// nearest its pickup.
await evaluate(`document.querySelector('.ticket') || document.querySelectorAll('.tabs button')[1].click()`);
await waitFor('.ticket');
await evaluate(`document.querySelector('.ticket').click()`);
await waitFor('.nearest li');
await shot('dispatch-order-1440', 2500);
await go('/insights', '.chart');
await shot('insights-1440');
await signOut();

await signIn(by('customer').email);
await waitFor('.shops');
await shot('orders-1440');
// An order from the nearest shop, followed as a courier takes it and rides.
await evaluate(`document.querySelector('.shops form button').click()`);
await waitFor('.timeline');
await shot('track-1440', 12_000);
await signOut();

await size(390, true);
await signIn(by('courier').email);
await waitFor('.phone');
await shot('courier-390', 1500);
await evaluate(`document.querySelector('.switch')?.click()`);
await waitFor('.job h1');
await shot('courier-online-390', 6000);
await signOut();
await signIn(by('dispatcher').email);
await waitFor('.strip');
await shot('dispatch-390', 2500);
await signOut();

ws.close();
proc.kill();
process.exit(0);
