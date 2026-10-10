// The playground: fenec studio over a database in the page, as the site
// serves it (site/dist, `python3 site/build.py`), in a headless Chrome.
// The studio is the playground's frame; every test drives it there --
// browse, sort, filter, edit, a query and its plan, a live row, the schema,
// reset and keep -- and holds the views only a server has to be absent.
//
//   cd studio && npm ci && npm run e2e      (make studio-test builds the site first)
//
// Without site/dist the suite is skipped, and under CI it fails instead.

import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { startSite, browser as launch } from './harness.mjs';

const built = existsSync(new URL('../../site/dist/studio/index.html', import.meta.url));
if (!built && process.env.CI) throw new Error('site/dist is not built: python3 site/build.py');
const skip = () => (built ? false : 'site/dist is not built: python3 site/build.py');

let site;
let browser;

test.before(async () => {
  if (!built) return;
  site = await startSite();
  browser = await launch();
});

test.after(async () => {
  await browser?.close();
  await site?.stop();
});
const digits = (s) => Number(String(s).replace(/\D/g, ''));

/** The playground at `width`, its studio's frame once the first example's rows are in. */
async function open({ width = 1280, height = 800, fresh = true } = {}) {
  const page = await browser.newPage();
  await page.setViewport({ width, height });
  page.problems = [];
  page.on('pageerror', (e) => page.problems.push(`error: ${e.message}`));
  page.on('console', (m) => {
    if (m.type() === 'error') page.problems.push(m.text());
  });
  page.on('dialog', async (d) => {
    page.problems.push(`a script opened a dialog: ${d.message()}`);
    await d.dismiss();
  });
  const started = Date.now();
  await page.goto(`${site.url}/playground`);
  const frame = await (await page.waitForSelector('iframe.pg-frame')).contentFrame();
  if (fresh) await frame.evaluate(() => localStorage.clear());
  await frame.waitForSelector('.ed .row:not(.pending)', { timeout: 20_000 });
  page.took = Date.now() - started;
  return { page, frame };
}

/** The text of row `i`'s cell under `field` in the grid under `within`, once its block is in. */
async function cell(frame, i, field, within = 'body') {
  const h = await frame.waitForFunction(
    (i, field, within) => {
      const root = document.querySelector(within);
      const names = [...root.querySelectorAll('.grid-head .col-name')].map((e) => e.textContent);
      const row = root.querySelector(`.row[aria-rowindex="${i + 3}"]:not(.pending):not([hidden])`);
      const at = names.indexOf(field);
      return row && at >= 0 ? row.children[at].textContent : null;
    },
    { timeout: 10_000 },
    i,
    field,
    within,
  );
  return h.jsonValue();
}

async function view(frame, name, ready) {
  await frame.click(`.view-tab[data-view="${name}"]`);
  await frame.waitForSelector(ready, { timeout: 10_000 });
}

async function waitCount(frame, n) {
  await frame.waitForFunction((n) => Number(document.querySelector('.view-count')?.textContent.replace(/\D/g, '')) === n, { timeout: 10_000 }, n);
}

/** The query editor's text replaced and run with Ctrl+Enter. */
async function runQuery(page, frame, text, params = '[]') {
  await frame.$eval('#query', (e) => (e.value = ''));
  await frame.type('#query', text);
  await frame.$eval('#query-params', (e, v) => (e.value = v), params);
  await frame.focus('#query');
  await page.keyboard.down('Control');
  await page.keyboard.press('Enter');
  await page.keyboard.up('Control');
}

/** A statement run in the editor, its JSON answer read back. */
async function answer(page, frame, text, params = '[]') {
  await view(frame, 'query', '#query');
  await frame.evaluate(() => document.querySelector('.ed-meta').replaceChildren());
  await runQuery(page, frame, text, params);
  await frame.waitForSelector('.ed-meta .ed-status', { timeout: 10_000 });
  await frame.click('.ed-tab[data-tab="json"]');
  return JSON.parse(await frame.$eval('.ed-json', (e) => e.textContent));
}

test('the first screen: an example run in the editor, the database said to be in this tab', { skip: skip() }, async (t) => {
  const { page, frame } = await open();
  t.diagnostic(`the first example's rows ${page.took} ms after the page was asked for`);
  // The first example, run: a search by words over products.
  assert.match(await frame.$eval('#query', (e) => e.value), /match description "pour over coffee"/);
  assert.equal(await cell(frame, 0, 'name', '.ed'), 'Pour over dripper');
  const examples = await frame.$$eval('.ed-examples .ed-item-name', (es) => es.map((e) => e.textContent));
  assert.ok(examples.length >= 10, examples.join(', '));
  assert.ok(examples.includes('Search by meaning') && examples.includes('Visitors by day'), examples.join(', '));
  // Where the token is on a server, the line that says where this runs.
  assert.equal(await frame.$eval('.who.here .here-long', (e) => e.textContent), 'Running in your browser.');
  assert.match(await frame.$eval('.here-server', (e) => e.textContent), /On your server: fenec-server --studio/);
  assert.match(await page.$eval('.pg-say', (e) => e.textContent), /Nothing you\s+type leaves your browser/);
  // No sign-in, no token, no tenant and no admin view: they are a server's.
  assert.deepEqual(await frame.$$eval('.view-tab', (ts) => ts.map((t) => t.dataset.view)), ['rows', 'query', 'schema', 'live']);
  for (const gone of ['.who:not(.here)', '.tenant-pick', '.server-host', '.signin', '.horizon.warn']) assert.equal(await frame.$(gone), null, gone);
  assert.ok(!(await frame.$eval('.top', (e) => e.textContent)).includes('Sign out'));
  await frame.focus('.ed .grid');
  await page.keyboard.press('5');
  assert.equal(await frame.$('.ad'), null);
  // The sidebar's collections and their counts; no file to size.
  assert.deepEqual(await frame.$$eval('.coll', (cs) => cs.map((c) => [c.dataset.name, Number(c.querySelector('.coll-count').textContent.replace(/\D/g, ''))])), [
    ['products', 50],
    ['orders', 400],
    ['events', 12000],
  ]);
  assert.equal(await frame.$('.side-foot dl'), null);
  // An example picked runs in the editor.
  await frame.click('.ed-examples li:nth-child(2) .ed-item');
  await frame.waitForFunction(() => /near taste/.test(document.querySelector('#query').value));
  await frame.waitForFunction(() => document.querySelector('.ed .row[aria-rowindex="3"] .cell')?.textContent === 'Hand grinder', { timeout: 10_000 });
  assert.deepEqual(page.problems, []);
  await page.close();
});

test('rows: browse, sort, filter, edit, insert and delete', { skip: skip() }, async () => {
  const { page, frame } = await open();
  await view(frame, 'rows', '.main .row:not(.pending)');
  await waitCount(frame, 50);
  assert.equal(await cell(frame, 0, 'id'), '1');
  // Sorted by the @sorted price, then descending.
  await frame.click('button[aria-label="Sort by price"]');
  await frame.waitForSelector('.head-cell[aria-sort="ascending"]');
  assert.equal(await cell(frame, 0, 'price'), '12');
  await frame.click('button[aria-label="Sort by price"]');
  await frame.waitForSelector('.head-cell[aria-sort="descending"]');
  assert.equal(await cell(frame, 0, 'price'), '299');
  // A typed clause with its parameters, then a quick filter beside it,
  // and the counts by value over what they select.
  await frame.type('#where', 'price < $1');
  await frame.$eval('#params', (e) => (e.value = '[50]'));
  await frame.focus('#where');
  await page.keyboard.press('Enter');
  await waitCount(frame, 34);
  await frame.type('.grid-quick input[aria-label="Filter category"]', '=coffee');
  await page.keyboard.press('Enter');
  await waitCount(frame, 5);
  assert.deepEqual(await frame.$$eval('.facet[aria-label="category by value"] .chip-value', (cs) => cs.map((c) => c.textContent)), ['coffee']);

  // A cell written in place, and read back as the database holds it.
  await frame.click('.coll[data-name="products"] .coll-line');
  await waitCount(frame, 50);
  await cell(frame, 0, 'name');
  await frame.click('.main .row[aria-rowindex="3"] .cell:nth-child(2)');
  await page.keyboard.press('Enter');
  await frame.waitForSelector('.cell-edit');
  await frame.$eval('.cell-edit', (e) => (e.value = ''));
  await frame.type('.cell-edit', 'Burr grinder, "the good one"');
  await page.keyboard.press('Enter');
  await frame.waitForFunction(() => !document.querySelector('.cell-edit'));
  assert.equal(await cell(frame, 0, 'name'), 'Burr grinder, "the good one"');

  // A delete shows its statement first; an insert comes from the form.
  await frame.click('.main .row[aria-rowindex="4"] .cell:nth-child(2)');
  await page.keyboard.press('Delete');
  await frame.waitForSelector('dialog[open] .statement');
  assert.equal(await frame.$eval('dialog[open] .statement', (e) => e.textContent), 'del products where id = $1 require 1\n  $1 = 2');
  await frame.click('dialog[open] .btn.danger');
  await waitCount(frame, 49);
  await frame.focus('.grid');
  await page.keyboard.press('n');
  await frame.waitForSelector('dialog[open] #new-name');
  await frame.type('#new-name', 'Desert lantern');
  await frame.type('#new-price', '42');
  await frame.click('dialog[open] button[type=submit]');
  await waitCount(frame, 50);

  const rows = await answer(page, frame, 'get products select id, name where id <= 2 or name = $1', '["Desert lantern"]');
  assert.deepEqual(rows.map((r) => r.name), ['Burr grinder, "the good one"', 'Desert lantern']);
  assert.deepEqual(page.problems, []);
  await page.close();
});

test('the query editor: parameters, the plan, a batch refused at its statement', { skip: skip() }, async () => {
  const { page, frame } = await open();
  await runQuery(page, frame, 'get orders select customer, total where customer = $1 order placed desc limit 5', '["ada"]');
  await frame.waitForFunction(() => /rows5/.test(document.querySelector('.ed-meta')?.textContent ?? ''), { timeout: 10_000 });
  assert.equal(await cell(frame, 0, 'customer', '.ed'), 'ada');
  // The plan: explain of the same text, each step under its kind.
  await frame.click('.ed-tab[data-tab="plan"]');
  await frame.waitForSelector('.plan-steps li', { timeout: 10_000 });
  assert.match(await frame.$eval('.plan-steps', (e) => e.textContent), /hash index on customer/);
  // A write answers with the change it left the database at.
  await runQuery(page, frame, 'set products {rating: 5} where category = "books"');
  await frame.waitForFunction(() => /Wrote 6 rows/.test(document.querySelector('.ed-out')?.textContent ?? ''), { timeout: 10_000 });
  assert.match(await frame.$eval('.ed-meta', (e) => e.textContent), /change\d+/);
  // Several statements are one block; the one that stops it is named and
  // none of its writes land.
  await runQuery(page, frame, 'insert products {name: $1};\nget nope limit 1', '[["x"], []]');
  await frame.waitForSelector('.ed-refusal', { timeout: 10_000 });
  const refusal = await frame.$eval('.ed-refusal', (e) => e.textContent);
  assert.match(refusal, /HTTP 404/);
  assert.match(refusal, /Statement 2 of 2 stopped the batch/);
  assert.equal(await frame.$eval('#query', (e) => e.value.slice(e.selectionStart, e.selectionEnd)), 'get nope limit 1');
  const [{ count }] = await answer(page, frame, 'get products where name = "x" count');
  assert.equal(count, 0);
  assert.deepEqual(page.problems, []);
  await page.close();
});

test('the live view: a row inserted arrives, and the schema shows a change before it runs', { skip: skip() }, async () => {
  const { page, frame } = await open();
  await frame.click('.coll[data-name="products"] .coll-line');
  await view(frame, 'live', '#live-where');
  await frame.waitForFunction(() => /50 rows of products/.test(document.querySelector('.lv-status')?.textContent ?? ''), { timeout: 10_000 });
  // Written in the editor while the live view is open behind it.
  await view(frame, 'query', '#query');
  await runQuery(page, frame, 'insert products {name: "A fox at dusk", category: "books", price: 9}');
  await frame.waitForFunction(() => /Wrote 1 row/.test(document.querySelector('.ed-out')?.textContent ?? ''), { timeout: 10_000 });
  await view(frame, 'live', '.lv-log li');
  await frame.waitForFunction(() => /1 new/.test(document.querySelector('.lv-log')?.textContent ?? ''), { timeout: 10_000 });
  await frame.waitForFunction(() => document.querySelector('.lv .grid')?.textContent.includes('A fox at dusk'), { timeout: 10_000 });
  assert.match(await frame.$eval('.lv-status', (e) => e.textContent), /51 rows of products/);

  // The schema as the engine writes it, and an index added through its plan.
  await view(frame, 'schema', '.sc-create');
  assert.match(await frame.$eval('.sc-create', (e) => e.textContent), /^create collection products \(name text, category text @hash, description text @text/);
  await frame.click('button[aria-label="Add an index on rating"]');
  await frame.waitForSelector('dialog[open] #sc-kind');
  await frame.select('#sc-kind', 'sorted');
  await frame.waitForFunction(() => /create index on products \(rating\) @sorted/.test(document.querySelector('dialog[open] .sc-plan')?.textContent ?? ''), { timeout: 10_000 });
  await frame.waitForFunction(() => !document.querySelector('dialog[open] button[type=submit]').disabled);
  await frame.click('dialog[open] button[type=submit]');
  await frame.waitForFunction(() => /rating float @sorted/.test(document.querySelector('.sc-create')?.textContent ?? ''), { timeout: 10_000 });
  assert.deepEqual(page.problems, []);
  await page.close();
});

test('reset data, and the database kept in this browser across a reload', { skip: skip() }, async () => {
  const { page, frame } = await open();
  await answer(page, frame, 'insert products {name: "Kept lamp"}; drop collection events');
  // Reset: every collection back to its seed.
  await frame.click('.here-reset');
  await frame.waitForSelector('dialog[open] .btn.danger');
  await frame.click('dialog[open] .btn.danger');
  await frame.waitForFunction(() => /back to its seed/.test(document.querySelector('.toasts')?.textContent ?? ''), { timeout: 10_000 });
  let [{ count }] = await answer(page, frame, 'get products where name = "Kept lamp" count');
  assert.equal(count, 0);
  [{ count }] = await answer(page, frame, 'get events count');
  assert.equal(count, 12000);

  // Kept: written, the page opened again, and the write is there.
  await frame.click('.who.here');
  await frame.waitForSelector('dialog[open] #here-keep');
  await frame.click('#here-keep');
  await frame.waitForFunction(() => /kept in this browser/.test(document.querySelector('.toasts')?.textContent ?? ''), { timeout: 10_000 });
  await frame.click('dialog[open] .dialog-actions .btn:not(.danger)');
  await answer(page, frame, 'insert products {name: "Kept lamp"}');
  await page.reload();
  let again = await (await page.waitForSelector('iframe.pg-frame')).contentFrame();
  await again.waitForSelector('.ed .row:not(.pending)', { timeout: 20_000 });
  [{ count }] = await answer(page, again, 'get products where name = "Kept lamp" count');
  assert.equal(count, 1);
  // Forgotten: the next visit seeds again.
  await again.click('.who.here');
  await again.waitForSelector('dialog[open] #here-keep:checked');
  assert.match(await again.$eval('dialog[open] .facts', (e) => e.textContent), /Brought back from this browser/);
  await again.click('#here-keep');
  await again.waitForFunction(() => /Nothing is kept/.test(document.querySelector('.toasts')?.textContent ?? ''), { timeout: 10_000 });
  await page.reload();
  again = await (await page.waitForSelector('iframe.pg-frame')).contentFrame();
  await again.waitForSelector('.ed .row:not(.pending)', { timeout: 20_000 });
  [{ count }] = await answer(page, again, 'get products where name = "Kept lamp" count');
  assert.equal(count, 0);
  assert.deepEqual(page.problems, []);
  await page.close();
});

test('on a phone: the studio takes the width, every view one tap away', { skip: skip() }, async () => {
  const { page, frame } = await open({ width: 390, height: 844 });
  // No page scrolls sideways, and every tab is in the bar.
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth));
  const bar = await frame.$eval('.top', (e) => e.getBoundingClientRect().right);
  const tabs = await frame.$$eval('.view-tab', (ts) => ts.map((t) => [t.dataset.view, t.getBoundingClientRect().right]));
  assert.deepEqual(tabs.map(([v]) => v), ['rows', 'query', 'schema', 'live']);
  for (const [v, right] of tabs) assert.ok(right <= bar, `${v} ends at ${right}, past ${bar}`);
  // The frame fills the screen under the header and the line above it.
  const frameBox = await page.$eval('.pg-window', (e) => e.getBoundingClientRect().toJSON());
  assert.ok(frameBox.bottom >= 844 - 1 && frameBox.bottom <= 844 + 1, JSON.stringify(frameBox));
  // The width the page lays out in: a phone's scrollbars take none, a
  // desktop Linux Chrome's classic one takes its own off the 390.
  assert.equal(frameBox.width, await page.evaluate(() => document.documentElement.clientWidth));
  // The collections in their drawer.
  await frame.click('.side-toggle');
  await frame.waitForSelector('.side-open .coll[data-name="orders"]');
  // In, once it has slid all the way: a click as it moves lands nowhere.
  await frame.waitForFunction(() => document.querySelector('.side-host').getBoundingClientRect().left >= 0);
  await frame.click('.coll[data-name="orders"] .coll-line');
  await frame.waitForFunction(() => document.querySelector('.ed #query')?.value.includes('orders'), { timeout: 10_000 });
  assert.deepEqual(page.problems, []);
  await page.close();
});
