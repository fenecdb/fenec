import { spawn } from 'node:child_process';
import { cmd, evalIn, lines, now, ps, sleep, startApp, waitFor , D, R, SERVER } from './drive.mjs';


const FILE = `${R}/data/small.fenec`;
const KEY = `file:${FILE}`;
const log = (...a) => console.log(...a);

const t0 = now();
const app = startApp({ FENEC_DESKTOP_OPEN: `${D}/data/small.fenec` });
await waitFor((l) => l.what === 'ready', 60000, 'ready');
log('ready after', now() - t0, 'ms from spawn (includes hutch build)');
const loaded = await waitFor((l) => l.detail?.type === 'state' && l.detail.loaded && l.key === KEY, 30000, 'studio loaded');
log('studio loaded; side =', loaded.detail.side, ' signin =', loaded.detail.signin);

log('\n-- UI insert through the studio\'s own New row form');
const ins = await evalIn(KEY, `const sleep=ms=>new Promise(r=>setTimeout(r,ms));
const btn=t=>[...document.querySelectorAll('button')].find(b=>b.textContent.trim()===t);
btn('New row').click(); await sleep(300);
const i=document.getElementById('new-customer'); i.value='desktop-spike'; i.dispatchEvent(new Event('input',{bubbles:true}));
const s=document.getElementById('new-owner'); s.value='spike'; s.dispatchEvent(new Event('input',{bubbles:true}));
await sleep(100); btn('Insert row').click(); await sleep(1500);
return document.body.innerText.replace(/\\s+/g,' ').slice(0,260)`);
log(JSON.stringify(ins));

log('\n-- processes now');
log(ps());

log('-- same file again, while open in this app');
cmd({ cmd: 'open', path: FILE });
const again = await waitFor((l) => l.what === 'open-result', 15000, 'open-result');
log(again.ok, again.message);

log('\n-- close the window');
cmd({ cmd: 'close', key: KEY });
const stopped = await waitFor((l) => l.what === 'server-stopped', 15000, 'server stopped').catch((e) => { console.log(app.out.join('').split('\n').slice(-25).join('\n')); throw e; });
log('server stopped, exit code', stopped.code);
await sleep(300);
log('processes after close:\n' + ps());

log('-- reopen: the row must still be there');
cmd({ cmd: 'open', path: FILE });
const re = await waitFor((l, i) => l.detail?.type === 'state' && l.detail.loaded && l.key === KEY && l.at > now() - 20000 && l.detail.side, 30000, 'reloaded');
await sleep(1500);
const last = lines().filter((l) => l.detail?.type === 'state' && l.key === KEY).pop();
log('sidebar after reopen:', last.detail.side);

log('\n-- a file held by another process (a hand-started fenec-server)');
const other = `${D}/data/held.fenec`;
const holder = spawn(`${SERVER}`, ['--file', other, '--http', '127.0.0.1:0'], { stdio: 'ignore', env: { ...process.env, FENEC_HTTP_TOKEN: 'x' } });
await sleep(1200);
cmd({ cmd: 'open', path: other });
const refused = await waitFor((l) => l.what === 'open-result' && l.ts !== again.ts, 15000, 'second open-result').catch(() => null);
const r2 = await waitFor((l) => l.what === 'refused', 15000, 'refused');
log('refused:', r2.message);
holder.kill('SIGTERM');

log('\n-- quit the app');
cmd({ cmd: 'quit' });
await Promise.race([new Promise((r) => app.child.on('exit', r)), sleep(40000)]);
await sleep(1500);
log('processes after quit:\n' + ps());
log('(end of ps)');
