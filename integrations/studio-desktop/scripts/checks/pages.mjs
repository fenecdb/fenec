// The app's own pages are held to themselves. The launcher and the
// new-database window have the RPC bridge to the main process, and Electrobun
// adds that bridge to every document a window loads: a link or a file dropped
// on one is a navigation, and a page it loaded would be handed `openPath`,
// `openConnection` and the saved servers (and, with `openConnection` and
// `overview`, a way to a token). So each is held to its own views:// folder
// by navigation rules, and a navigation anywhere else, a web page, a file, or
// another of the app's pages, is refused. (sandbox.mjs holds a studio window.)
import assert from 'node:assert/strict';
import { assertClean, cmd, evalIn, file, lines, quit, startApp, step, waitFor, waitNone, waitNotice, log } from './lib.mjs';

assertClean('before');

const app = startApp('pages', { FENEC_DESKTOP_LAUNCHER: '1' });
await waitFor((l) => l.what === 'ready', 60000, 'ready');
await waitFor((l) => l.what === 'page-text' && l.key === 'launcher' && /Open file/.test(l.text), 20000, 'the launcher drawn');
cmd({ cmd: 'newdb' });
await waitFor((l) => l.what === 'page-text' && l.key === 'newdb' && /New database/.test(l.text), 20000, 'the new-database window drawn');
const n = lines().length;
cmd({ cmd: 'open', path: file('nothing-here') });
const notice = await waitNotice('That file is gone', 15000, n);

const where = (key) => evalIn(key, `return location.href`);

/**
 * `href` asked of window `key`: refused, and the window is where it was. A web
 * page, or another of the app's, reaches the navigation rules, which report
 * the question and the answer (`allowed: false`). A file: address does not:
 * WebKit will not take a page of another scheme there, and asks nothing; for it
 * the window must simply not have moved.
 */
async function refused(key, href, from) {
  const m = lines().length;
  await evalIn(key, `location.href = ${JSON.stringify(href)}; return 1`);
  if (href.startsWith('file:')) {
    await waitNone((l, i) => i >= m && l.key === key && ((l.what === 'did-navigate') || (l.what === 'will-navigate' && /allowed[^,}]*true/.test(l.data))), 1500, `${key} went to ${href}`);
  } else {
    const nav = await waitFor((l, i) => i >= m && l.what === 'will-navigate' && l.key === key && l.data.includes(href.split('?')[0]), 10000, `${key} being asked to go to ${href}`);
    assert.match(nav.data, /allowed[^,}]*false/, `${key} -> ${href} must be refused: ${nav.data}`);
  }
  const now = await where(key);
  assert.equal(now.value, from, `${key} stays where it was after asking for ${href}`);
}

step('the launcher cannot be taken anywhere else');
const launcherAt = (await where('launcher')).value;
assert.match(launcherAt, /^views:\/\/launcher\/index\.html/);
await refused('launcher', 'https://example.com/', launcherAt);
await refused('launcher', 'file:///etc/hosts', launcherAt);
// Another page of the app has an RPC bridge of its own: not a place for this one to go to.
await refused('launcher', 'views://newdb/index.html', launcherAt);
// And it still works: nothing it needs was taken away by being refused.
const ok = await evalIn('launcher', `return document.getElementById('open') !== null`);
assert.equal(ok.value, true);

step('the new-database window, the same');
const newdbAt = (await where('newdb')).value;
assert.match(newdbAt, /^views:\/\/newdb\/index\.html/);
await refused('newdb', 'https://example.com/', newdbAt);
await refused('newdb', 'views://launcher/index.html', newdbAt);

step('and a notice window');
const noticeAt = (await where(notice.key)).value;
assert.match(noticeAt, /^views:\/\/notice\/index\.html/);
await refused(notice.key, 'https://example.com/', noticeAt);
await refused(notice.key, 'views://launcher/index.html', noticeAt);

step('a link out of an app page is not handed to the browser: only a studio window does that');
const m = lines().length;
await evalIn('launcher', `window.open("https://example.com/"); return 1`);
await waitNone((l, i) => i >= m && l.what === 'external', 1200, 'a link from an app page was handed to the browser');

step('quit');
await quit(app);
log('clean');
