// Screenshots of the studio, light and dark, wide and narrow:
//
//   node studio/test/screenshots.mjs <out-dir> [url token]
//
// against a server of its own, seeded as the tests seed one, or against the
// one at `url` signed in with `token`. The docs' images are made this way:
// the rows wide and on a phone, then at 1280 px the query editor, the
// schema with an index being added, the live rows as they are written, and
// the admin view.

import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { startServer, seed, browser as launch, query, TOKEN } from './harness.mjs';

const out = process.argv[2] ?? 'screenshots';
mkdirSync(out, { recursive: true });
let server = null;
let url = process.argv[3];
let token = process.argv[4] ?? TOKEN;
if (!url) {
  server = await startServer();
  await seed(server.url, 100_000);
  url = server.url;
  token = TOKEN;
}
const pause = (ms) => new Promise((r) => setTimeout(r, ms));

async function signedIn(page) {
  await page.goto(`${url}/_studio/`);
  await page.waitForSelector('#token');
  await page.type('#token', token);
  await page.click('.signin button[type=submit]');
  await page.waitForSelector('.row:not(.pending)');
}

const browser = await launch();
try {
  for (const [width, height] of [
    [1280, 800],
    [390, 844],
  ]) {
    for (const scheme of ['light', 'dark']) {
      const page = await browser.newPage();
      await page.setViewport({ width, height, deviceScaleFactor: 2 });
      await page.emulateMediaFeatures([{ name: 'prefers-color-scheme', value: scheme }]);
      await page.goto(`${url}/_studio/`);
      await page.waitForSelector('#token');
      if (width > 400) await page.screenshot({ path: join(out, `signin-${width}-${scheme}.png`) });
      await page.type('#token', token);
      await page.click('.signin button[type=submit]');
      await page.waitForSelector('.row:not(.pending)');
      if (width > 400) {
        // A collection's fields open in the sidebar, a row in the panel.
        await page.click('.coll[data-name="orders"] .twisty');
        await page.click('.row:nth-child(3) .cell:nth-child(4)');
        await page.keyboard.press('Space');
        await page.waitForSelector('.inspector:not([hidden]) .ins-fields');
      }
      await pause(300);
      await page.screenshot({ path: join(out, `grid-${width}-${scheme}.png`) });
      await page.close();
    }
  }

  // The views past the rows, at 1280 px.
  for (const scheme of ['light', 'dark']) {
    const page = await browser.newPage();
    await page.setViewport({ width: 1280, height: 800, deviceScaleFactor: 2 });
    await page.emulateMediaFeatures([{ name: 'prefers-color-scheme', value: scheme }]);
    await signedIn(page);
    const shot = (name) => page.screenshot({ path: join(out, `${name}-1280-${scheme}.png`) });

    // A query with its parameters, then its plan.
    await page.click('.view-tab[data-view="query"]');
    await page.waitForSelector('#query');
    await page.$eval('#query', (e) => (e.value = ''));
    await page.type('#query', 'get orders select id, customer, status, total, placed\n  where status = $1 and total > $2\n  order placed desc limit 200\n  facet owner');
    await page.$eval('#query-params', (e) => (e.value = '["open", 900]'));
    await page.focus('#query');
    await page.keyboard.down('Control');
    await page.keyboard.press('Enter');
    await page.keyboard.up('Control');
    await page.waitForSelector('.ed .row:not(.pending)');
    await pause(300);
    await shot('query');
    await page.click('.ed-tab[data-tab="plan"]');
    await page.waitForSelector('.plan-steps li');
    await pause(200);
    await shot('plan');

    // An index added to a collection, the statement and the plan shown.
    await page.click('.view-tab[data-view="schema"]');
    await page.waitForSelector('.sc-create');
    await page.click('button[aria-label="Add an index on total"]');
    await page.waitForSelector('dialog[open] #sc-kind');
    await page.select('#sc-kind', 'sorted');
    await page.waitForFunction(() => /create index/.test(document.querySelector('dialog[open] .sc-plan')?.textContent ?? ''));
    await pause(200);
    await shot('schema');
    await page.keyboard.press('Escape');

    // The live rows of a narrow shape, a few writes landing.
    await page.click('.view-tab[data-view="live"]');
    await page.waitForSelector('#live-where');
    await page.type('#live-where', 'customer = "customer-042"');
    await page.keyboard.press('Enter');
    await page.waitForFunction(() => /rows of orders/.test(document.querySelector('.lv-status')?.textContent ?? ''));
    const ids = (await query(url, 'get orders select id where customer = $1 limit 3', ['customer-042'], token)).map((r) => r.id);
    await query(url, 'insert orders {customer: $1, status: $2, total: $3, owner: $4, placed: $5}', ['customer-042', 'open', 42.5, 'alice', new Date().toISOString()], token);
    await query(url, 'set orders {status: $1} where id = $2', ['shipped', ids[1]], token);
    await query(url, 'del orders where id = $1', [ids[2]], token);
    await pause(500);
    await shot('live');

    // The admin view: the statements by shape, the server's numbers.
    await page.click('.view-tab[data-view="admin"]');
    await page.waitForSelector('.ad-table');
    await page.waitForFunction(() => /File/.test(document.querySelector('.ad-numbers')?.textContent ?? ''));
    await pause(300);
    await shot('admin');
    await page.close();
  }
} finally {
  await browser.close();
  await server?.stop();
}
