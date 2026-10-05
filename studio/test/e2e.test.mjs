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

/** The text of row `i`'s cell under `field`, once its block is in, in the grid under `within`. */
async function cell(p, i, field, within = 'body') {
  return p.waitForFunction(
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

/** A view opened from its tab, its module loaded. */
async function view(p, name, ready) {
  await p.click(`.view-tab[data-view="${name}"]`);
  await p.waitForSelector(ready, { timeout: 10_000 });
}

/** The query editor's text replaced, its parameters set, and run with Ctrl+Enter. */
async function runQuery(p, text, params = '[]') {
  await p.$eval('#query', (e) => (e.value = ''));
  await p.type('#query', text);
  await p.$eval('#query-params', (e, v) => (e.value = v), params);
  await p.focus('#query');
  await p.keyboard.down('Control');
  await p.keyboard.press('Enter');
  await p.keyboard.up('Control');
}

test('the query editor: parameters, the plan, a batch refused at its statement', async () => {
  const p = await page();
  await signIn(p, TOKEN);
  await view(p, 'query', '#query');
  // It says what it is: the token's authority, no more.
  assert.match(await p.$eval('#query-note', (e) => e.textContent), /with your token: it may do what the token may, and nothing more/);
  // Coloured by the docs' rules, drawn as text nodes over the textarea.
  await runQuery(p, 'get orders where owner = $1 and total > $2 order placed desc limit 20', '["carol", 500]');
  await p.waitForFunction(() => document.querySelector('.ed-meta .ed-status')?.textContent === '200', { timeout: 10_000 });
  assert.ok(await p.$('.hl.on .hl-mirror span.t-kw'));
  assert.equal(await p.$eval('.hl-mirror', (e) => e.textContent.trim()), 'get orders where owner = $1 and total > $2 order placed desc limit 20');
  const want = await query(server.url, 'get orders where owner = $1 and total > $2 order placed desc limit 20', ['carol', 500]);
  assert.equal(await cell(p, 0, 'id', '.ed'), String(want[0].id));
  assert.equal(await cell(p, 0, 'owner', '.ed'), 'carol');
  const meta = await p.$eval('.ed-meta', (e) => e.textContent);
  assert.match(meta, /took/);
  assert.match(meta, new RegExp(`rows${want.length}`));
  assert.match(await p.$eval('.ed-rid', (e) => e.textContent), /^[0-9a-f]{16}$/);

  // The JSON, as the server answered it.
  await p.click('.ed-tab[data-tab="json"]');
  assert.deepEqual(JSON.parse(await p.$eval('.ed-json', (e) => e.textContent)), want);

  // The plan: explain asked of the same text, with the same parameters.
  await p.click('.ed-tab[data-tab="plan"]');
  await p.waitForSelector('.plan-steps li', { timeout: 10_000 });
  const plan = await p.$$eval('.plan-steps li', (ls) => ls.map((l) => [l.querySelector('.plan-kind').textContent, l.querySelector('.plan-what').textContent]));
  const explained = await query(server.url, 'explain get orders where owner = $1 and total > $2 order placed desc limit 20', ['carol', 500]);
  assert.deepEqual(plan, explained.map((r) => /^([a-z]+):\s*(.*)$/s.exec(r.plan).slice(1)));
  assert.match(plan.flat().join(' '), /hash index on owner/);

  // A write: its Fenec-Seq beside the answer.
  await runQuery(p, 'set odd {ölçü: $1} where limit = 2', '[3.25]');
  await p.waitForFunction(() => /Wrote 1 row/.test(document.querySelector('.ed-out')?.textContent ?? ''), { timeout: 10_000 });
  assert.match(await p.$eval('.ed-meta', (e) => e.textContent), /change\d+/);

  // Several statements are one /batch; the one that stops it is named, and
  // none of its writes land.
  const before = await count('get odd count');
  await runQuery(p, 'insert odd {limit: $1};\nget nope limit 1', '[[40], []]');
  await p.waitForSelector('.ed-refusal', { timeout: 10_000 });
  const refusal = await p.$eval('.ed-refusal', (e) => e.textContent);
  assert.match(refusal, /HTTP 404/);
  assert.match(refusal, /nope/);
  assert.match(refusal, /Statement 2 of 2 stopped the batch/);
  assert.equal(await count('get odd count'), before);
  // The editor points at it.
  assert.equal(await p.$eval('#query', (e) => e.value.slice(e.selectionStart, e.selectionEnd)), 'get nope limit 1');

  // Kept in history, and saved by name, in this browser.
  await p.click('.ed-actions .btn:not(.primary)');
  await p.waitForSelector('dialog[open] #save-name');
  await p.type('#save-name', 'refused batch');
  await p.click('dialog[open] button[type=submit]');
  await p.waitForFunction(() => /refused batch/.test(document.querySelector('.ed-list')?.textContent ?? ''));
  const kept = await p.evaluate(() => [localStorage.getItem('fenec-studio-saved'), localStorage.getItem('fenec-studio-history'), JSON.stringify(localStorage)]);
  assert.match(kept[0], /get nope/);
  assert.match(kept[1], /owner = \$1/);
  assert.ok(!kept[2].includes(TOKEN), 'the token reached localStorage');

  assert.deepEqual(p.problems, []);
  await p.close();
});

test('the schema: an index through its preview and plan; a drop only once its name is typed', async () => {
  await query(server.url, 'create collection scratch (n int)');
  const p = await page();
  await signIn(p, TOKEN);
  await open(p, 'odd');
  await view(p, 'schema', '.sc-create');
  assert.equal(await p.$eval('.sc-create', (e) => e.textContent), 'create collection odd (limit int @sorted, order text @hash, ölçü float)');

  // An index on ölçü: the statement, then what the declared-schema plan says.
  await p.click('button[aria-label="Add an index on ölçü"]');
  await p.waitForSelector('dialog[open] #sc-kind');
  await p.select('#sc-kind', 'sorted');
  await p.waitForFunction(() => document.querySelector('dialog[open] .sc-statement')?.textContent === 'create index on odd (ölçü) @sorted');
  await p.waitForFunction(() => /create index on odd \(ölçü\) @sorted/.test(document.querySelector('dialog[open] .sc-plan')?.textContent ?? ''), { timeout: 10_000 });
  await p.waitForFunction(() => !document.querySelector('dialog[open] button[type=submit]').disabled);
  await p.click('dialog[open] button[type=submit]');
  await p.waitForFunction(() => /ölçü float @sorted/.test(document.querySelector('.sc-create')?.textContent ?? ''), { timeout: 10_000 });
  const odd = (await query(server.url, 'describe odd'))[0];
  assert.equal(odd.fields.find((f) => f.name === 'ölçü').index, 'sorted');

  // A collection dropped: its name typed before the button does anything.
  await p.click('.coll[data-name="scratch"] .coll-line');
  await p.waitForFunction(() => document.querySelector('.sc .view-title')?.textContent === 'scratch');
  await p.waitForFunction(() => /create collection scratch/.test(document.querySelector('.sc-create')?.textContent ?? ''), { timeout: 10_000 });
  await p.click('.sc-actions .btn.danger');
  await p.waitForSelector('dialog[open] #sc-confirm');
  await p.waitForFunction(() => /drop collection scratch/.test(document.querySelector('dialog[open] .sc-statement')?.textContent ?? ''));
  // The plan of it: a collection the description leaves out is left alone.
  await p.waitForFunction(() => /leaves alone/.test(document.querySelector('dialog[open] .sc-plan')?.textContent ?? ''), { timeout: 10_000 });
  const go = 'dialog[open] button[type=submit]';
  assert.equal(await p.$eval(go, (b) => b.disabled), true);
  await p.type('#sc-confirm', 'scratc');
  assert.equal(await p.$eval(go, (b) => b.disabled), true);
  await p.type('#sc-confirm', 'h');
  assert.equal(await p.$eval(go, (b) => b.disabled), false);
  await p.click(go);
  await p.waitForFunction(() => !document.querySelector('.coll[data-name="scratch"]'), { timeout: 10_000 });
  assert.ok(!(await query(server.url, 'collections')).some((c) => c.name === 'scratch'));

  assert.deepEqual(p.problems, []);
  await p.close();
});

test('the live view: rows arrive as they are written, and pause', async () => {
  const p = await page();
  await signIn(p, TOKEN);
  await open(p, 'docs');
  await view(p, 'live', '#live-where');
  await p.waitForFunction(() => /300 rows of docs/.test(document.querySelector('.lv-status')?.textContent ?? ''), { timeout: 10_000 });
  await query(server.url, 'insert docs {title: $1, lang: $2, code: $3}', ['a fox at dusk', 'en', 'LIVE-1']);
  await p.waitForFunction(() => [...document.querySelectorAll('.lv .row[data-mark="new"]')].some((r) => r.textContent.includes('a fox at dusk')), { timeout: 10_000 });
  assert.match(await p.$eval('.lv-log', (e) => e.textContent), /1 new/);

  // Paused, a change waits; resumed, it lands.
  await p.click('.lv .actions .btn');
  await query(server.url, 'set docs {title: $1} where code = $2', ['a fox at night', 'LIVE-1']);
  await p.waitForFunction(() => /Paused: 1 change waits/.test(document.querySelector('.lv-status')?.textContent ?? ''), { timeout: 10_000 });
  assert.ok(!(await p.$eval('.lv .grid', (e) => e.textContent)).includes('a fox at night'));
  await p.click('.lv .actions .btn');
  await p.waitForFunction(() => [...document.querySelectorAll('.lv .row[data-mark="changed"]')].some((r) => r.textContent.includes('a fox at night')), { timeout: 10_000 });
  await query(server.url, 'del docs where code = $1', ['LIVE-1']);
  await p.waitForFunction(() => /1 deleted/.test(document.querySelector('.lv-log')?.textContent ?? ''), { timeout: 10_000 });

  // The admin view, for the server's token.
  await view(p, 'admin', '.ad-table');
  await p.waitForFunction(() => /File/.test(document.querySelector('.ad-numbers')?.textContent ?? ''), { timeout: 10_000 });
  const numbers = await p.$eval('.ad-numbers', (e) => e.textContent);
  for (const what of ['Reads', 'Writes', 'Read latency', 'File', 'Reclaimable', 'Compactions']) assert.match(numbers, new RegExp(what));
  // The shapes run most: the seed's 300 inserts among them, as typed.
  await p.click('.seg:not([aria-pressed="true"])');
  await p.waitForFunction(() => /insert docs \{title: \$1/.test(document.querySelector('.ad-statements')?.textContent ?? ''));

  assert.deepEqual(p.problems, []);
  await p.close();
});

test('as one user: no admin view, and the live view shows no other user\'s row', async () => {
  const p = await page();
  const alice = jwt({ sub: 'alice', role: 'analyst' });
  await signIn(p, alice);
  // No admin tab, and its key opens nothing.
  assert.deepEqual(await p.$$eval('.view-tab', (ts) => ts.map((t) => t.dataset.view)), ['rows', 'query', 'schema', 'live']);
  await p.keyboard.press('5');
  assert.equal(await p.$('.ad'), null);

  await open(p, 'orders');
  await view(p, 'live', '#live-where');
  // The whole of alice's orders is past what a seed may hold.
  await p.waitForFunction(() => /Narrow them/.test(document.querySelector('.lv-status')?.textContent ?? ''), { timeout: 10_000 });
  await p.type('#live-where', 'customer = "customer-001"');
  await p.keyboard.press('Enter');
  const mine = await count('get orders where customer = $1 and owner = $2 count', ['customer-001', 'alice']);
  await p.waitForFunction((n) => new RegExp(`^${n} rows? of orders`).test(document.querySelector('.lv-status')?.textContent ?? ''), { timeout: 10_000 }, mine);
  // bob's write first, then alice's: only hers arrives.
  await query(server.url, 'insert orders {customer: $1, owner: $2, note: $3}', ['customer-001', 'bob', 'bob was here']);
  await query(server.url, 'insert orders {customer: $1, owner: $2, note: $3}', ['customer-001', 'alice', 'alice was here']);
  await p.waitForFunction(() => document.querySelector('.lv .grid')?.textContent.includes('alice was here'), { timeout: 10_000 });
  // bob's row moved to alice's is a new row of hers; alice's moved to bob's leaves.
  await query(server.url, 'set orders {owner: $1} where note = $2', ['bob', 'alice was here']);
  await p.waitForFunction(() => /1 deleted/.test(document.querySelector('.lv-log')?.textContent ?? ''), { timeout: 10_000 });
  const text = await p.$eval('.lv', (e) => e.textContent);
  assert.ok(!text.includes('bob was here'), 'bob\'s row reached alice');
  const owners = await p.$$eval('.lv .row:not([hidden])', (rows) => {
    const names = [...document.querySelectorAll('.lv .grid-head .col-name')].map((e) => e.textContent);
    return rows.map((r) => r.children[names.indexOf('owner')]?.textContent);
  });
  assert.ok(owners.length > 0 && owners.every((o) => o === 'alice'), owners.join(','));

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

test('through a router whose token is the nodes\': the admin view lists the tenants, their nodes and sizes', async () => {
  const node = await startServer({ tenants: true, studio: false });
  const router = await startRouter(node, ['acme', 'globex'], { token: TOKEN });
  try {
    await notes(router.url, [
      ['acme', 3],
      ['globex', 1],
    ]);
    const p = await page();
    await p.goto(`${router.url}/_studio/`);
    await p.waitForSelector('#token');
    await p.type('#token', TOKEN);
    await p.click('.signin button[type=submit]');
    await waitCount(p, 3);
    await view(p, 'admin', '.ad-tenants');
    const rows = await p.$$eval('.ad-tenants tbody tr', (trs) => trs.map((tr) => [...tr.children].map((td) => td.textContent)));
    assert.deepEqual(rows.map((r) => r.slice(0, 3)), [
      ['acme', 'n1', 'active'],
      ['globex', 'n1', 'active'],
    ]);
    for (const r of rows) assert.match(r[3], /^\d+(\.\d)? (B|KB|MB)$/);
    // The router's own counts, and the tenant's statements through it.
    await p.waitForFunction(() => /Requests/.test(document.querySelector('.ad-numbers')?.textContent ?? ''), { timeout: 10_000 });
    await p.waitForFunction(() => /get notes/.test(document.querySelector('.ad-statements')?.textContent ?? ''), { timeout: 10_000 });
    assert.deepEqual(p.problems, []);
    await p.close();
  } finally {
    await router.stop();
    await node.stop();
  }
});
