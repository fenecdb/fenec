// Is a studio window sandboxed (no RPC to the main process) and held to its origin?
import { cmd, evalIn, lines, sleep, startApp, waitFor, ps , D, R, SERVER } from './drive.mjs';

const KEY = `file:${R}/data/small.fenec`;
const app = startApp({ FENEC_DESKTOP_OPEN: `${D}/data/small.fenec`, FENEC_DESKTOP_LAUNCHER: '1' });
await waitFor((l) => l.what === 'server-started', 60000, 'server-started');
await waitFor((l) => l.detail?.type === 'state' && l.detail.loaded && l.key === KEY, 30000, 'loaded');
await sleep(1000);
const keys = await evalIn(KEY, `return Object.getOwnPropertyNames(window).filter(k => /electrobun/i.test(k)).join(',') + ' | rpc-bridge: ' + (typeof window.__electrobunBunBridge) + ', internal: ' + (typeof window.__electrobunInternalBridge)`);
console.log('studio window globals:', JSON.stringify(keys));

console.log('\n-- navigation away from the server\'s origin');
const before = lines().length;
const nav = await evalIn(KEY, `location.href = "https://example.com/"; await new Promise(r => setTimeout(r, 2500)); return location.href + ' | title: ' + document.title`);
console.log('after trying to navigate to https://example.com:', JSON.stringify(nav));
for (const l of lines().slice(before)) if (/navigate|new-window/.test(l.what)) console.log('  event', l.what, l.data);

console.log('\n-- window.open to another origin');
const before2 = lines().length;
const open = await evalIn(KEY, `const w = window.open("https://example.com/"); await new Promise(r => setTimeout(r, 1500)); return 'window.open returned ' + (w ? 'a window' : 'null')`);
console.log(JSON.stringify(open));
for (const l of lines().slice(before2)) if (/navigate|new-window/.test(l.what)) console.log('  event', l.what, l.data);

cmd({ cmd: 'quit' });
await Promise.race([new Promise((r) => app.child.on('exit', r)), sleep(30000)]);
await sleep(800);
console.log('\nprocesses after quit:\n' + ps() + '(end)');
