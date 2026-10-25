import { spawn, execSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { cmd, lines, now, sleep, startApp, waitFor, ps , D, R, SERVER } from './drive.mjs';


const log = (...a) => console.log(...a);
const sh = (c) => { try { return execSync(c, { stdio: ['ignore', 'pipe', 'ignore'] }).toString().trim(); } catch (e) { return `(failed: ${e.status})`; } };

async function server(args, env = {}, bin = `${SERVER}`) {
  const c = spawn(bin, args, { env: { ...process.env, ...env }, stdio: ['ignore', 'ignore', 'pipe'] });
  let err = '';
  c.stderr.on('data', (b) => { err += b; });
  const origin = await new Promise((r) => { const i = setInterval(() => { const m = /listening on: (http:\/\/\S+)/.exec(err); if (m) { clearInterval(i); r(m[1]); } }, 5); });
  return { c, origin };
}

// Saved servers: a token in the keychain, a URL in state.json, and the studio of that server in a window.
// (The keychain item can be read back only by the app: its ACL names the app, so `security ... -w` from here
// would raise a password dialog, and these checks read the secret through the app instead.)
const app = startApp({ FENEC_DESKTOP_LAUNCHER: '1' });
await waitFor((l) => l.what === 'ready', 60000, 'ready');
await sleep(300);

const lw = await waitFor((l) => l.what === 'launcher' && /Open file/.test(l.text), 20000, 'launcher text');
log('\nlauncher shows:', lw.text);

// A remote server with a token, the studio on.
const T = 'remote-secret-' + Math.random().toString(36).slice(2);
const remote = await server(['--file', `${D}/data/remote.fenec`, '--studio', '--http', '127.0.0.1:0'], { FENEC_HTTP_TOKEN: T });
log('\nremote server at', remote.origin);
cmd({ cmd: 'addconn', name: 'remote spike', url: remote.origin, token: T, tenant: '' });
const added = await waitFor((l) => l.what === 'conn-added', 10000, 'conn-added');
const id = added.id;
cmd({ cmd: 'secret', name: `connection:${id}` });
const sec = await waitFor((l) => l.what === 'secret' && l.name === `connection:${id}`, 10000, 'secret');
log('keychain item read back by the app:', sec.value === T ? 'holds the token' : 'MISSING');
log('keychain item exists (attributes only, no secret read):', sh(`security find-generic-password -s dev.fenecdb.studio -a connection:${id}`).includes('dev.fenecdb.studio') ? 'yes' : 'no');
const statePath = `${process.env.HOME}/Library/Application Support/dev.fenecdb.studio/dev/state.json`;
log('token in state.json?', readFileSync(statePath, 'utf8').includes(T) ? 'YES (bad)' : 'no');
log('state.json connections:', JSON.stringify(JSON.parse(readFileSync(statePath, 'utf8')).connections));

cmd({ cmd: 'openconn', id });
const rk = `remote:${id}`;
const loaded = await waitFor((l) => l.detail?.type === 'state' && l.detail.loaded && l.key === rk, 30000, 'remote loaded');
log('remote window loaded; signin form shown:', loaded.detail.signin, '| side:', loaded.detail.side.slice(0, 60));
await sleep(800);
const last = lines().filter((l) => l.detail?.type === 'state' && l.key === rk).pop();
log('page text:', last.detail.text.replace(/\s+/g, ' ').slice(0, 120));

log('\n-- a server whose studio is off (OLD_SERVER: a v0.1.10 release binary, which has no --studio)');
let added2 = { id: null };
let r2 = { message: null };
if (process.env.OLD_SERVER) {
  const old = await server(['--file', `${D}/data/held.fenec`, '--http', '127.0.0.1:0'], { FENEC_HTTP_TOKEN: 'x' }, process.env.OLD_SERVER);
  cmd({ cmd: 'addconn', name: 'no studio', url: old.origin, token: 'x', tenant: '' });
  added2 = await waitFor((l) => l.what === 'conn-added' && l.id !== id, 10000, 'conn2');
  cmd({ cmd: 'openconn', id: added2.id });
  r2 = await waitFor((l) => l.what === 'openconn-result' && l.message, 15000, 'openconn-result');
  log(r2.ok, r2.message);
  old.c.kill('SIGTERM');
} else log('(skipped: OLD_SERVER is not set)');

log('\n-- a connection to nothing');
cmd({ cmd: 'addconn', name: 'dead', url: 'http://127.0.0.1:9', token: '', tenant: '' });
const added3 = await waitFor((l) => l.what === 'conn-added' && l.id !== id && l.id !== added2.id, 10000, 'conn3');
cmd({ cmd: 'openconn', id: added3.id });
const r3 = await waitFor((l) => l.what === 'openconn-result' && l.message && l.message !== r2.message, 15000, 'openconn-result 3');
log(r3.ok, r3.message);

log('\n-- wrong token: the studio asks for a sign-in');
cmd({ cmd: 'addconn', name: 'badtoken', url: remote.origin, token: 'wrong', tenant: '' });
const added4 = await waitFor((l) => l.what === 'conn-added' && ![id, added2.id, added3.id].includes(l.id), 10000, 'conn4');
cmd({ cmd: 'openconn', id: added4.id });
const bk = `remote:${added4.id}`;
await waitFor((l) => l.detail?.type === 'state' && l.key === bk, 20000, 'bad token page');
await sleep(2000);
const bl = lines().filter((l) => l.detail?.type === 'state' && l.key === bk).pop();
log('signin form:', bl.detail.signin, '|', bl.detail.text.replace(/\s+/g, ' ').slice(0, 200));

log('\n-- removing a connection deletes its keychain item');
for (const c of [id, added2.id, added3.id, added4.id].filter(Boolean)) cmd({ cmd: 'rmconn', id: c });
await sleep(1000);
log('after removal, attributes lookup:', sh(`security find-generic-password -s dev.fenecdb.studio -a connection:${id}`));

log('\n-- quit with windows open: the app has no server of its own to stop here, and leaves none');
remote.c.kill('SIGTERM');
cmd({ cmd: 'quit' });
await Promise.race([new Promise((r) => app.child.on('exit', r)), sleep(30000)]);
await sleep(1000);
log('processes after quit:\n' + ps());
log('(end)');
