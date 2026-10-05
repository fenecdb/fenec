// The studio in a headless Chrome against a real fenec-server: signed in
// with the server's token it browses, sorts, filters, edits and deletes;
// signed in as one user, no other user's row reaches the grid, a count or
// a facet. Every page is watched for a content security policy violation
// and for a script a row's value could have run.
//
//   cd studio && npm ci && npm run e2e      (make studio-test)
//
// The server is target/debug/fenec-server unless FENEC_SERVER names one;
// Chrome is CHROME_PATH, or the one puppeteer keeps (harness.mjs).

import test from 'node:test';
import assert from 'node:assert/strict';
import { startServer, startRouter, seed, browser as launch, jwt, query, TOKEN, ADMIN } from './harness.mjs';

const ROWS = 100_000;
let server;
let browser;

test.before(async () => {
  server = await startServer();
  await seed(server.url, ROWS);
  browser = await launch();
});

test.after(async () => {
  await browser?.close();
  await server?.stop();
});

const digits = (s) => Number(String(s).replace(/\D/g, ''));
const count = async (text, params = [], token = TOKEN) => (await query(server.url, text, params, token))[0].count;

/** A page with everything that must not happen recorded on it. */
async function page() {
  const p = await browser.newPage();
  await p.setViewport({ width: 1280, height: 800 });
  p.problems = [];
  p.on('pageerror', (e) => p.problems.push(`error: ${e.message}`));
  p.on('dialog', async (d) => {
    p.problems.push(`a script opened a dialog: ${d.message()}`);
    await d.dismiss();
  });
  p.on('console', (m) => {
    if (m.type() === 'error' && /Content Security Policy|Refused to/.test(m.text())) p.problems.push(m.text());
  });
  await p.evaluateOnNewDocument(() => {
    document.addEventListener('securitypolicyviolation', (e) => {
      console.error(`Content Security Policy: ${e.violatedDirective} ${e.blockedURI}`);
    });
  });
  return p;
}

async function signIn(p, token) {
  await p.goto(`${server.url}/_studio/`);
  await p.waitForSelector('#token');
  await p.type('#token', token);
  await p.click('.signin button[type=submit]');
  await p.waitForSelector('.row:not(.pending)');
}

/** The text of row `i`'s cell under `field`, once its block is in. */
async function cell(p, i, field) {
  return p.waitForFunction(
    (i, field) => {
      const names = [...document.querySelectorAll('.grid-head .col-name')].map((e) => e.textContent);
      const row = document.querySelector(`.row[aria-rowindex="${i + 3}"]:not(.pending):not([hidden])`);
      const at = names.indexOf(field);
      return row && at >= 0 ? row.children[at].textContent : null;
    },
    { timeout: 10_000 },
    i,
    field,
  ).then((h) => h.jsonValue());
}

/** A collection opened from the sidebar, its rows in. */
async function open(p, name) {
  await p.click(`.coll[data-name="${name}"] .coll-line`);
  await p.waitForFunction((name) => document.querySelector('.view-title')?.textContent === name && document.querySelector('.row:not(.pending):not([hidden])'), { timeout: 10_000 }, name);
}

const statusLine = (p) => p.$eval('.view-count', (e) => e.textContent);

/** Waits for the view to count `n` rows; failing, says what the page shows. */
async function waitCount(p, n) {
  try {
    await p.waitForFunction((n) => Number(document.querySelector('.view-count')?.textContent.replace(/\D/g, '')) === n, { timeout: 10_000 }, n);
  } catch (e) {
    const shown = await p.evaluate(() =>
      ['.view-count', '.form-error', '.toasts', '.status'].map((s) => `${s}: ${document.querySelector(s)?.textContent ?? '-'}`).join('; '),
    );
    throw new Error(`waiting for ${n} rows: ${shown}`, { cause: e });
  }
}

test('with the server token: browse, sort, filter, edit, insert and delete', async () => {
  const p = await page();
  await signIn(p, TOKEN);

  // The sidebar: every collection with its rows, and the file's size.
  assert.equal(digits(await p.$eval('.coll[data-name="orders"] .coll-count', (e) => e.textContent)), ROWS);
  assert.equal(digits(await p.$eval('.coll[data-name="docs"] .coll-count', (e) => e.textContent)), 300);
  assert.match(await p.$eval('.side-foot', (e) => e.textContent), /File/);
  assert.equal(await cell(p, 0, 'id'), '1');
  assert.equal(digits(await statusLine(p)), ROWS);

  // Far down, read by offset, then on by id.
  await p.$eval('.grid-scroll', (e) => (e.scrollTop = 28 * 70_000));
  assert.equal(await cell(p, 70_000, 'id'), '70001');

  // Sorted by the @sorted field, descending on a second click.
  await p.click('.head-cell[aria-sort] button[aria-label="Sort by placed"]');
  await p.waitForSelector('.head-cell[aria-sort="ascending"]');
  await p.click('button[aria-label="Sort by placed"]');
  await p.waitForSelector('.head-cell[aria-sort="descending"]');
  const latest = (await query(server.url, 'get orders order placed desc limit 1'))[0];
  assert.equal(await cell(p, 0, 'placed'), latest.placed);

  // A typed clause with its parameters, then a quick filter beside it.
  await p.type('#where', 'total > $1 and owner = $2');
  await p.$eval('#params', (e) => (e.value = ''));
  await p.type('#params', '[900, "bob"]');
  await p.keyboard.press('Enter');
  const typed = await count('get orders where total > $1 and owner = $2 count', [900, 'bob']);
  await waitCount(p, typed);
  await p.type('.grid-quick input[aria-label="Filter status"]', '=shipped');
  await p.keyboard.press('Enter');
  const both = await count('get orders where total > $1 and owner = $2 and status = $3 count', [900, 'bob', 'shipped']);
  await waitCount(p, both);
  // The facets count what the filter selects.
  const facet = await p.$$eval('.facet[aria-label="owner by value"] .chip', (cs) => cs.map((c) => c.textContent));
  assert.deepEqual(facet.map((t) => t.replace(/\d|\s/g, '')), ['bob']);

  // Back to the whole collection.
  await open(p, 'orders');
  await waitCount(p, ROWS);

  // A cell written in place: its value goes as a parameter, so a text
  // shaped as FenecQL is stored as it reads.
  const note = 'she said "hi"); del orders where (true';
  await cell(p, 0, 'note');
  await p.click('.row[aria-rowindex="3"] .cell:nth-child(7)');
  await p.keyboard.press('Enter');
  await p.waitForSelector('.cell-edit');
  await p.$eval('.cell-edit', (e) => (e.value = ''));
  await p.type('.cell-edit', note);
  await p.keyboard.press('Enter');
  await p.waitForFunction(() => !document.querySelector('.cell-edit'));
  assert.equal((await query(server.url, 'get orders where id = 1'))[0].note, note);
  assert.equal(await cell(p, 0, 'note'), note);
  assert.equal(await count('get orders count'), ROWS);

  // A delete shows the statement it sends, and sends it once confirmed.
  await p.click('.row[aria-rowindex="4"] .cell:nth-child(2)');
  await p.keyboard.press('Delete');
  await p.waitForSelector('dialog[open] .statement');
  assert.equal(await p.$eval('dialog[open] .statement', (e) => e.textContent), 'del orders where id = $1 require 1\n  $1 = 2');
  await p.click('dialog[open] .btn.danger');
  await waitCount(p, ROWS - 1);
  assert.deepEqual(await query(server.url, 'get orders where id = 2'), []);

  // A new row from the form the schema builds.
  await p.keyboard.press('n');
  await p.waitForSelector('dialog[open] #new-customer');
  await p.type('#new-customer', 'customer-new');
  await p.type('#new-total', '12.5');
  assert.equal(await p.$eval('dialog[open] .statement', (e) => e.textContent), 'insert orders {customer: $1, total: $2}\n  $1 = "customer-new"\n  $2 = 12.5');
  await p.click('dialog[open] button[type=submit]');
  await waitCount(p, ROWS);
  assert.equal(await count('get orders where customer = $1 count', ['customer-new']), 1);

  // A row deleted underneath: the edit is refused as a changed row (412).
  await query(server.url, 'del orders where id = 3');
  // Row 2 is gone, so id 3 is the grid's second row.
  assert.equal(await cell(p, 1, 'id'), '3');
  await p.click('.row[aria-rowindex="4"] .cell:nth-child(7)');
  await p.keyboard.press('Enter');
  await p.waitForSelector('.cell-edit');
  await p.type('.cell-edit', 'x');
  await p.keyboard.press('Enter');
  await p.waitForFunction(() => /The row changed/.test(document.querySelector('.toasts').textContent), { timeout: 10_000 });
  await p.keyboard.press('Escape');

  // A value holding markup is shown as text and runs nothing.
  await open(p, 'odd');
  assert.equal(await cell(p, 0, 'order'), '<img src=x onerror=alert(1)>');
  assert.equal(await p.$$eval('.grid img', (l) => l.length), 0);

  assert.deepEqual(p.problems, []);
  await p.close();
});

test('as one user: no other user\'s row in the grid, the counts or the facets', async () => {
  const p = await page();
  const alice = jwt({ sub: 'alice', role: 'analyst' });
  await signIn(p, alice);
  const mine = await count('get orders where owner = $1 count', ['alice']);

  // The identity, from the server and the token.
  assert.match(await p.$eval('.who', (e) => e.textContent), /alice as analyst/);
  assert.match(await p.$eval('.who', (e) => e.textContent), /expires in/);

  // The sidebar's count and the grid's are the user's rows; the sizes of
  // the file are not the user's to see.
  assert.equal(digits(await p.$eval('.coll[data-name="orders"] .coll-count', (e) => e.textContent)), mine);
  assert.equal(digits(await statusLine(p)), mine);
  assert.equal(await p.$('.side-foot dl'), null);

  // Every facet, every chip, holds alice's rows alone.
  const owners = await p.$$eval('.facet[aria-label="owner by value"] .chip', (cs) => cs.map((c) => c.textContent));
  assert.equal(owners.length, 1);
  assert.match(owners[0], /^alice/);
  assert.equal(digits(owners[0]), mine);
  const statuses = await p.$$eval('.facet[aria-label="status by value"] .chip .chip-count', (cs) => cs.map((c) => Number(c.textContent.replace(/\D/g, ''))));
  assert.equal(statuses.reduce((a, b) => a + b, 0), mine);

  // Rows from the top, the middle and the end of the grid: alice's each.
  const seen = new Set();
  for (const at of [0, 0.37, 0.71, 1]) {
    await p.$eval('.grid-scroll', (e, at) => (e.scrollTop = at * (e.scrollHeight - e.clientHeight)), at);
    const last = Math.min(mine - 1, Math.floor(at * mine));
    await cell(p, Math.max(0, last - 5), 'owner');
    for (const o of await p.$$eval('.row:not(.pending):not([hidden])', (rows) => {
      const names = [...document.querySelectorAll('.grid-head .col-name')].map((e) => e.textContent);
      return rows.map((r) => r.children[names.indexOf('owner')].textContent);
    })) seen.add(o);
  }
  assert.deepEqual([...seen], ['alice']);
  const text = await p.$eval('body', (b) => b.textContent);
  for (const other of ['bob', 'carol', 'dave']) assert.ok(!text.includes(other), other);

  // A typed clause cannot widen the token's filter: it is ANDed with it.
  await p.type('#where', 'owner = $1 or true');
  await p.$eval('#params', (e) => (e.value = '["bob"]'));
  await p.keyboard.press('Enter');
  await waitCount(p, mine);

  // Its rules, as the server takes them.
  await p.click('.who');
  await p.waitForSelector('dialog[open] .rules');
  const rules = await p.$eval('dialog[open] .rules', (e) => e.textContent);
  assert.match(rules, /orders.*owner = \$jwt\.sub/);
  await p.keyboard.press('Escape');

  // A write the policy refuses names the field it refused.
  await open(p, 'odd');
  await cell(p, 0, 'limit');
  await p.click('.row[aria-rowindex="3"] .cell:nth-child(2)');
  await p.keyboard.press('Enter');
  await p.waitForSelector('.cell-edit');
  await p.$eval('.cell-edit', (e) => (e.value = '7'));
  await p.keyboard.press('Enter');
  await p.waitForFunction(() => /policy refused.*limit/.test(document.querySelector('.toasts').textContent), { timeout: 10_000 });
  assert.equal((await query(server.url, 'get odd where id = 1'))[0].limit, 1);

  assert.deepEqual(p.problems, []);
  await p.close();
});

test('scrolling 100 000 rows takes no task over 50 ms', async (t) => {
  const p = await page();
  await signIn(p, TOKEN);
  const { longest, frameMax } = await p.evaluate(async () => {
    const tasks = [];
    new PerformanceObserver((l) => tasks.push(...l.getEntries().map((e) => e.duration))).observe({ type: 'longtask' });
    const el = document.querySelector('.grid-scroll');
    // The longest frame too: a long task is one over 50 ms, so none is
    // reported as 0, and the frames say how near to it the drawing came.
    let before = performance.now();
    let frameMax = 0;
    const frame = () =>
      new Promise((r) =>
        requestAnimationFrame((now) => {
          frameMax = Math.max(frameMax, now - before);
          before = now;
          r();
        }),
      );
    // Down the whole collection in 400 steps, a frame each, then a fling
    // back up a screen at a time.
    for (let k = 1; k <= 400; k++) {
      el.scrollTop = (k / 400) * (el.scrollHeight - el.clientHeight);
      await frame();
    }
    for (let k = 0; k < 120; k++) {
      el.scrollTop -= el.clientHeight;
      await frame();
    }
    await new Promise((r) => setTimeout(r, 500));
    return { longest: Math.max(0, ...tasks), frameMax };
  });
  t.diagnostic(`long tasks: the longest ${longest} ms; the longest frame ${frameMax.toFixed(1)} ms`);
  assert.ok(longest < 50, `a task took ${longest} ms`);
  assert.deepEqual(p.problems, []);
  await p.close();
});

/** `notes` in each tenant, as many rows as given, at `url` (a node or a router). */
async function notes(url, tenants) {
  for (const [t, n] of tenants) {
    await query(`${url}/t/${t}`, 'create collection notes (title text)');
    for (let i = 0; i < n; i++) await query(`${url}/t/${t}`, 'insert notes {title: $1}', [`${t} ${i}`]);
  }
}

test('on a tenant node, a token naming two tenants picks between them', async () => {
  const node = await startServer({ tenants: true });
  try {
    for (const t of ['acme', 'globex']) {
      const r = await fetch(`${node.url}/_admin/tenants/${t}`, { method: 'PUT', headers: { authorization: `Bearer ${ADMIN}` } });
      assert.equal(r.status, 201);
    }
    await notes(node.url, [
      ['acme', 3],
      ['globex', 5],
    ]);
    const p = await page();
    await p.goto(`${node.url}/_studio/`);
    await p.waitForSelector('#token');
    await p.type('#token', jwt({ sub: 'alice', tenant: ['acme', 'globex'] }));
    await p.click('.signin button[type=submit]');
    await p.waitForSelector('select.tenant');
    assert.deepEqual(await p.$$eval('select.tenant option', (os) => os.map((o) => o.value)), ['acme', 'globex']);
    await waitCount(p, 3);
    await p.select('select.tenant', 'globex');
    await waitCount(p, 5);
    assert.equal(await cell(p, 0, 'title'), 'globex 0');
    assert.deepEqual(p.problems, []);
    await p.close();
  } finally {
    await node.stop();
  }
});

test('through a router: the tenant a token names, or the one typed', async () => {
  const node = await startServer({ tenants: true, studio: false });
  const router = await startRouter(node, ['acme', 'globex']);
  try {
    await notes(router.url, [
      ['acme', 2],
      ['globex', 4],
    ]);
    // A user's token names its tenant: the studio goes straight to it.
    const p = await page();
    await p.goto(`${router.url}/_studio/`);
    await p.waitForSelector('#token');
    await p.type('#token', jwt({ sub: 'alice', tenant: 'acme' }));
    await p.click('.signin button[type=submit]');
    await waitCount(p, 2);
    assert.deepEqual(await p.$$eval('select.tenant option', (os) => os.map((o) => o.value)), ['acme']);
    assert.equal(await cell(p, 1, 'title'), 'acme 1');
    await p.click('.top-end .btn:last-child');

    // The nodes' token names none, and may not list the router's: typed.
    await p.waitForSelector('#token');
    await p.type('#token', TOKEN);
    await p.click('.signin button[type=submit]');
    await p.waitForFunction(() => /name one/.test(document.querySelector('.form-error')?.textContent ?? ''));
    await p.type('#tenant', 'globex');
    await p.click('.signin button[type=submit]');
    await waitCount(p, 4);
    assert.deepEqual(p.problems, []);
    await p.close();
  } finally {
    await router.stop();
    await node.stop();
  }
});
