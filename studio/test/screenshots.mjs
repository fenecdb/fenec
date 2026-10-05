// Screenshots of the studio, light and dark, wide and narrow:
//
//   node studio/test/screenshots.mjs <out-dir> [url token]
//
// against a server of its own, seeded as the tests seed one, or against the
// one at `url` signed in with `token`. The docs' images are made this way.

import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { startServer, seed, browser as launch, TOKEN } from './harness.mjs';

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
      await new Promise((r) => setTimeout(r, 300));
      await page.screenshot({ path: join(out, `grid-${width}-${scheme}.png`) });
      await page.close();
    }
  }
} finally {
  await browser.close();
  await server?.stop();
}
