// SIGKILL the app's main process: the server is orphaned (nothing can stop a
// child on macOS) and keeps `<file>.lock`; the next launch of the app stops it.
import { execSync, spawn } from 'node:child_process';
import { cmd, lines, sleep, startApp, waitFor, ps, D, R, SERVER } from './drive.mjs';

const sh = (c) => { try { return execSync(c, { stdio: ['ignore', 'pipe', 'ignore'] }).toString().trim(); } catch { return ''; } };

const app = startApp({ FENEC_DESKTOP_OPEN: `${D}/data/small.fenec` });
const st = await waitFor((l) => l.what === 'server-started', 60000, 'server-started');
const bun = sh(`ps -axo pid=,command= | grep "dev-macos-arm64/fenec studio-dev.app/Contents/MacOS/bun" | grep -v grep | awk '{print $1}' | head -1`);
console.log('app main process', bun, 'server', st.pid);
process.kill(+bun, 'SIGKILL');
await sleep(1500);
console.log('after SIGKILL of the main process, server alive (pid, ppid):', sh(`ps -p ${st.pid} -o pid=,ppid=`) || 'no');
const direct = spawn(SERVER, ['--file', `${D}/data/small.fenec`, '--http', '127.0.0.1:0'], { stdio: ['ignore', 'ignore', 'pipe'], env: { ...process.env, FENEC_HTTP_TOKEN: 'x' } });
let err = '';
direct.stderr.on('data', (b) => { err += b; });
await new Promise((r) => direct.on('exit', r));
console.log('a second server on that file meanwhile:', err.trim().split('\n').pop());

console.log('\n-- the next launch');
const again = startApp({ FENEC_DESKTOP_LAUNCHER: '1' });
await waitFor((l) => l.what === 'ready', 60000, 'ready');
const reaped = lines().find((l) => l.what === 'reaped-strays');
await sleep(300);
console.log('reaped on start:', reaped ? JSON.stringify(reaped.strays) : '(nothing)', '| the orphan is alive:', sh(`ps -p ${st.pid} -o pid=`) ? 'yes' : 'no');
cmd({ cmd: 'quit' });
await Promise.race([new Promise((r) => again.child.on('exit', r)), sleep(30000)]);
await sleep(800);
console.log('processes after quit:\n' + ps() + '(end)');
