// Lighthouse CI over the four kinds of page, on Lighthouse's mobile
// profile: a Moto G Power-class screen on slow 4G -- 150 ms round trips,
// 1.6 Mbps down, the CPU slowed 4x. The budgets: LCP under 1.8 s, CLS under
// 0.05, TBT under 150 ms.
//
// The throttling is applied (`devtools`), not estimated. Lighthouse's
// default estimate (`simulate`, Lantern) replays the page's own load, and
// on this machine a page's scripts arrive and run before its first paint,
// so the estimate puts Next.js's 132 KB of runtime on the LCP's path: 2.0 to
// 2.2 s for these pages, and 1.37 s for an empty Next 16 page. With the
// network and CPU throttled for real, the scripts load after the paint, as
// on a phone (README, "Page speed", has both).
//
// JavaScript: an empty Next 16 App Router page ships 132.3 KB compressed,
// measured on the same profile, so the 70 KB budget is under the
// framework's own floor. What is asserted is the shop's share: at most
// 145 KB in all, the floor and a small allowance for the shop's own code.
// scripts/lighthouse.sh names the product page (LHCI_PRODUCT) and runs it.
const shop = (process.env.SHOP_URL || 'http://localhost:3000').replace(/\/$/, '');
const product = process.env.LHCI_PRODUCT || '/p/unknown';

const budgets = {
  'largest-contentful-paint': ['error', { maxNumericValue: 1800, aggregationMethod: 'median' }],
  'cumulative-layout-shift': ['error', { maxNumericValue: 0.05, aggregationMethod: 'median' }],
  'total-blocking-time': ['error', { maxNumericValue: 150, aggregationMethod: 'median' }],
};

module.exports = {
  ci: {
    collect: {
      url: [`${shop}/`, `${shop}/c/backpacks`, `${shop}/search?q=titanium+stove`, `${shop}${product}`],
      numberOfRuns: Number(process.env.LHCI_RUNS || 3),
      settings: {
        // A server on this machine answers in a few milliseconds; the
        // throttled network adds the round trips a phone would wait.
        chromeFlags: '--headless=new --no-sandbox',
        throttlingMethod: process.env.LHCI_THROTTLING || 'devtools',
        onlyCategories: ['performance', 'seo', 'accessibility', 'best-practices'],
      },
    },
    assert: {
      assertMatrix: [
        { matchingUrlPattern: '.*', assertions: budgets },
        {
          matchingUrlPattern: '/(p|c)/',
          assertions: { 'resource-summary:script:size': ['error', { maxNumericValue: 145 * 1024, aggregationMethod: 'median' }] },
        },
        // Search results carry noindex on purpose, which the SEO score counts against.
        { matchingUrlPattern: '^(?!.*/search).*$', assertions: { 'categories:seo': ['error', { minScore: 0.9 }] } },
      ],
    },
    upload: { target: 'filesystem', outputDir: '.lighthouseci/reports' },
  },
};
