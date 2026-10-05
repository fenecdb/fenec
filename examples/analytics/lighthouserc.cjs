// Lighthouse CI over the dashboard (a day from the raw events, 90 days from
// the rollups), the market page and the sign-in page, on Lighthouse's
// mobile profile with the throttling applied (`devtools`): a Moto G
// Power-class screen on slow 4G, 150 ms round trips, 1.6 Mbps, the CPU
// slowed 4x. The shop's budgets: LCP under 1.8 s, CLS under 0.05, TBT under
// 150 ms; performance and accessibility at 0.9 at least, SEO at 0.9 on
// every page.
//
// The dashboard is behind a sign-in: scripts/lighthouse.sh signs in and
// hands the session cookie over (LHCI_COOKIE). A dashboard says `noindex`
// on purpose -- it is someone's private numbers -- which Lighthouse's
// is-crawlable audit counts against SEO; that audit is skipped, and
// test/seo.test.ts holds the public pages to being indexable instead.
//
// JavaScript: the dashboard ships one script of 1.8 KB brotli, the market
// page the same, and no page needs it to paint; the budget is 10 KB.
const app = (process.env.KESTREL_URL || 'http://127.0.0.1:3000').replace(/\/$/, '');

const budgets = {
  'largest-contentful-paint': ['error', { maxNumericValue: 1800, aggregationMethod: 'median' }],
  'cumulative-layout-shift': ['error', { maxNumericValue: 0.05, aggregationMethod: 'median' }],
  'total-blocking-time': ['error', { maxNumericValue: 150, aggregationMethod: 'median' }],
  'resource-summary:script:size': ['error', { maxNumericValue: 10 * 1024, aggregationMethod: 'median' }],
  'categories:performance': ['error', { minScore: 0.9, aggregationMethod: 'median' }],
  'categories:accessibility': ['error', { minScore: 0.9, aggregationMethod: 'median' }],
  'categories:seo': ['error', { minScore: 0.9, aggregationMethod: 'median' }],
};

module.exports = {
  ci: {
    collect: {
      // The sign-in page as /signin: signed in, / sends to the dashboard.
      url: [`${app}/s/fieldnotes?range=24h`, `${app}/s/fieldnotes?range=90d`, `${app}/markets`, `${app}/signin`],
      numberOfRuns: Number(process.env.LHCI_RUNS || 3),
      settings: {
        chromeFlags: '--headless=new --no-sandbox',
        throttlingMethod: process.env.LHCI_THROTTLING || 'devtools',
        onlyCategories: ['performance', 'seo', 'accessibility', 'best-practices'],
        skipAudits: ['is-crawlable'],
        extraHeaders: process.env.LHCI_COOKIE ? JSON.stringify({ Cookie: process.env.LHCI_COOKIE }) : undefined,
      },
    },
    assert: { assertMatrix: [{ matchingUrlPattern: '.*', assertions: budgets }] },
    upload: { target: 'filesystem', outputDir: '.lighthouseci/reports' },
  },
};
