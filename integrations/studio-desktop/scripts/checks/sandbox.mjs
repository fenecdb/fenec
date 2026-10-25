// A studio window is a page a server serves, so it is held to what the
// studio needs: no bridge to the main process, no leaving its server's
// origin, links out go to the browser (here: reported, not opened).
import assert from 'node:assert/strict';
import { assertClean, data, evalIn, lines, quit, startApp, step, waitFor, waitNone, waitStudio, log } from './lib.mjs';

assertClean('before');
const SMALL = data('small', 50);
const KEY = `file:${SMALL}`;

const app = startApp('sandbox', { FENEC_DESKTOP_OPEN: SMALL });
const started = await waitFor((l) => l.what === 'server-started', 60000, 'the server started');
await waitStudio(KEY);

step('no RPC bridge to the main process in the studio window');
const globals = await evalIn(KEY, `return Object.getOwnPropertyNames(window).filter(k => /electrobun/i.test(k)).sort()`);
log('globals with electrobun in the name:', JSON.stringify(globals.value));
// What a sandboxed view's preload (preload-sandboxed.js) defines, and no more:
// the page's way out is the event bridge, which carries events and host
// messages one way and calls nothing. The full preload adds the RPC bridge to
// Bun; if a later Electrobun puts anything else in a sandboxed page, this
// says so before a release does.
const SANDBOXED = ['__electrobunEventBridge', '__electrobunInternalBridge', '__electrobunPlatform', '__electrobunSendToHost', '__electrobunWebviewId', '__electrobunWindowId'];
assert.deepEqual(globals.value.filter((k) => !SANDBOXED.includes(k)), [], 'nothing beyond the sandboxed preload');
const rpc = await evalIn(KEY, `return [typeof window.__electrobunBunBridge, typeof window.electrobun, typeof window.__electrobunRpcSocketPort, typeof window.__electrobunSecretKey].join(',')`);
assert.equal(rpc.value, 'undefined,undefined,undefined,undefined');

step('the session is handed over as the studio asks: window.fenecHost, frozen, in the page\'s own world, and never stored');
const host = await evalIn(KEY, `
const d = Object.getOwnPropertyDescriptor(window, 'fenecHost');
const h = window.fenecHost;
const buttons = [...document.querySelectorAll('button')].map(b => b.textContent.trim());
return {
  defined: !!d,
  descriptor: d && { writable: d.writable, enumerable: d.enumerable, configurable: d.configurable },
  frozen: !!h && Object.isFrozen(h) && Object.isFrozen(h.session),
  server: h && h.session.server,
  origin: location.origin,
  tokenLength: h && h.session.token.length,
  tenant: h && h.session.tenant,
  sessionStorage: Object.keys(sessionStorage),
  localStorage: Object.keys(localStorage),
  signOut: buttons.includes('Sign out'),
  signIn: /Connect to a fenecdb server/.test(document.body.innerText),
};`);
assert.ok(host.ok, host.value);
assert.equal(host.value.defined, true, 'the preload ran in the same world as the page: the page sees it');
assert.deepEqual(host.value.descriptor, { writable: false, enumerable: false, configurable: false });
assert.equal(host.value.frozen, true);
assert.equal(host.value.server, host.value.origin, 'the origin that served the page');
assert.equal(host.value.tokenLength, 43, '32 random bytes, base64url');
assert.equal(host.value.tenant, null);
assert.deepEqual(host.value.sessionStorage, [], 'the token is not in session storage');
assert.deepEqual(host.value.localStorage.filter((k) => /token|session|fenec-studio/i.test(k)), [], 'nor local storage');
assert.equal(host.value.signOut, false, 'under an app the studio has no Sign out: the app holds the connection');
assert.equal(host.value.signIn, false, 'and no sign-in form');

step("navigating away from the server's origin is refused");
const m1 = lines().length;
await evalIn(KEY, `location.href = "https://example.com/"; return 1`);
const nav = await waitFor((l, i) => i >= m1 && l.what === 'will-navigate' && /example\.com/.test(l.data), 10000, 'the navigation being asked about');
assert.match(nav.data, /allowed[^,}]*false/, `refused: ${nav.data}`);
const where = await evalIn(KEY, `return location.href + ' | ' + document.title`);
log('after location.href = https://example.com:', where.value);
assert.ok(where.value.startsWith(started.origin), `still on ${started.origin}: ${where.value}`);

step('window.open to another origin goes to the browser, not into the window');
const m2 = lines().length;
await evalIn(KEY, `window.open("https://example.com/"); return 1`);
const ext = await waitFor((l, i) => i >= m2 && l.what === 'external', 10000, 'the link out');
assert.equal(ext.url, 'https://example.com/');
assert.ok((await evalIn(KEY, `return location.href`)).value.startsWith(started.origin), 'the window stayed');

step('a page cannot ask for links without end: a window is given one in three seconds');
const mRepeat = lines().length;
await evalIn(KEY, `window.open("https://example.com/a"); window.open("https://example.com/b"); return 1`);
await waitNone((l, i) => i >= mRepeat && l.what === 'external', 1500, 'a second link within the gap was handed to the browser');

step('an address that is not http(s) is not handed to the system at all');
const m3 = lines().length;
await evalIn(KEY, `window.open("file:///etc/passwd"); window.open("javascript:alert(1)"); return 1`);
await waitNone((l, i) => i >= m3 && l.what === 'external', 1200, 'a non-web link was handed to the system');

step('quit');
await quit(app);
log('clean');
