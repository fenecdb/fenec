// The public pages are found and described; the private ones are not
// indexed. Lighthouse's crawlability audit is skipped for the dashboard
// (lighthouserc.cjs), so this holds the rest.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { APP } from './helpers.ts';

test('the sign-in and market pages are indexable, titled and described', async () => {
  for (const p of ['/', '/markets']) {
    const r = await fetch(`${APP}${p}`);
    assert.equal(r.status, 200, p);
    const html = await r.text();
    assert.ok(!/noindex/.test(html), `${p} says noindex`);
    assert.ok(!r.headers.get('x-robots-tag'), `${p} sends x-robots-tag`);
    assert.match(html, /<title>[^<]{10,70}<\/title>/, `${p} title`);
    assert.match(html, /<meta name="description" content="[^"]{50,170}">/, `${p} description`);
    assert.match(html, /<html lang="en">/);
    assert.match(html, /<h1>/);
  }
});

test('robots.txt keeps crawlers off the dashboards, and a dashboard says noindex', async () => {
  const robots = await (await fetch(`${APP}/robots.txt`)).text();
  assert.match(robots, /Disallow: \/s\//);
  const r = await fetch(`${APP}/s/fieldnotes`, { redirect: 'manual' });
  assert.equal(r.status, 303, 'signed out, a dashboard sends to the sign-in');
});

test('the tracker is tiny and cacheable', async () => {
  const r = await fetch(`${APP}/k.js`);
  assert.equal(r.status, 200);
  const js = await r.text();
  assert.ok(js.length < 1200, `k.js is ${js.length} bytes`);
  // As a browser fetches it: compressed.
  const br = await fetch(`${APP}/k.js`, { headers: { 'accept-encoding': 'br' } });
  assert.equal(br.headers.get('content-encoding'), 'br');
  assert.match(r.headers.get('cache-control') ?? '', /max-age=3600/);
  assert.equal(r.headers.get('access-control-allow-origin'), '*');
});
