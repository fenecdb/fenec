// `npm run shots [dir]`: screenshots of a running `npm run dev` at 1280 and
// 390 px wide, signed in as the demo owner, through headless Chrome's
// DevTools protocol (no browser library to install). CHROME_PATH names the
// browser.
import { spawn } from 'node:child_process';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { DEMO_PASSWORD } from './seed.ts';

const out = process.argv[2] ?? 'data/shots';
const app = process.env.TRELLIS_ORIGIN ?? 'http://127.0.0.1:3000';
const chrome = process.env.CHROME_PATH ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
mkdirSync(out, { recursive: true });

const port = 9333;
const proc = spawn(chrome, ['--headless=new', `--remote-debugging-port=${port}`, `--user-data-dir=${mkdtempSync(join(tmpdir(), 'trellis-chrome-'))}`, '--hide-scrollbars', 'about:blank'], { stdio: 'ignore' });
let targets: { type: string; webSocketDebuggerUrl: string }[] = [];
for (let i = 0; i < 100 && !targets.length; i++) {
  try {
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
  for (let i = 0; i < 100; i++) {
    const r = await evaluate(`!!document.querySelector(${JSON.stringify(selector)})`);
    if (r.result?.value) return;
    await sleep(100);
  }
  throw new Error(`no ${selector}`);
}
async function size(width: number, mobile: boolean) {
  await send('Emulation.setDeviceMetricsOverride', { width, height: mobile ? 844 : 860, deviceScaleFactor: mobile ? 2 : 1, mobile });
}
async function shot(name: string, full = false) {
  await sleep(700);
  const r = (await send('Page.captureScreenshot', { format: 'png', captureBeyondViewport: full })) as { data: string };
  writeFileSync(join(out, `${name}.png`), Buffer.from(r.data, 'base64'));
  console.log(`${out}/${name}.png`);
}
const go = async (path: string, selector: string) => {
  await send('Page.navigate', { url: `${app}${path}` });
  await waitFor(selector);
};

await send('Page.enable');
for (const [w, mobile, scheme] of [
  [1280, false, 'light'],
  [390, true, 'light'],
  [1280, false, 'dark'],
] as const) {
  await size(w, mobile);
  await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: scheme }] });
  const tag = scheme === 'dark' ? `${w}-dark` : `${w}`;
  await go('/signin', 'input[name=email]');
  await shot(`signin-${tag}`);
  await evaluate(`(() => {
    document.querySelector('input[name=email]').value = 'ines@lumen.studio';
    document.querySelector('input[name=password]').value = ${JSON.stringify(DEMO_PASSWORD)};
    document.querySelector('form button[type=submit]').click();
  })()`);
  await waitFor('.board .card');
  await shot(`board-${tag}`);
  await evaluate(`document.querySelector('.lane[data-status=doing] .card').click()`);
  await waitFor('dialog.drawer .thread li');
  await shot(`task-${tag}`);
  await go('/o/lumen/search?q=calendar', '.results li');
  await shot(`search-${tag}`);
  await go('/o/lumen/members', '.list li');
  await shot(`people-${tag}`);
  await go('/o/lumen/audit', 'table tbody tr');
  await shot(`audit-${tag}`);
  await evaluate(`fetch('/api/auth/signout', {method: 'POST', headers: {'content-type': 'application/json'}, body: '{}'})`);
}
ws.close();
proc.kill();
