// Lighthouse CI over the four kinds of page, on Lighthouse's mobile
// profile -- a Moto G Power-class screen on slow 4G, 150 ms round trips and
// 1.6 Mbps down -- with the CPU of a mid-tier phone. The budgets: LCP under
// 1.8 s, CLS under 0.05, TBT under 150 ms.
//
// The CPU. Applied throttling slows the machine's own CPU by a factor, so
// Lighthouse's fixed 4x measures a different phone on every machine, and
// GitHub's runners come with different CPUs. At 4x, ten runners at once, the
// four pages' median TBT was 103-133 ms on an AMD EPYC 7763 (a benchmarkIndex
// of 2 350-2 510), 64-85 on an EPYC 9V74 or a Xeon 8573C (2 740-3 350), 54-62
// on a Xeon 6973P-C (3 250-3 450) and 40-57 on an EPYC 9V45 (3 860-4 410).
// Over 67 CI runs a run's four pages moved together (the product page against
// the other three's mean: a correlation of 0.77), and the product page's
// 194 ms that failed a run came from the slowest band, two of its three runs
// slower still. A median of three runs takes out a run, not the machine. So
// the slowdown is worked out on the machine before the runs
// (scripts/cpu-slowdown.mjs; LHCI_CPU_SLOWDOWN gives one instead): the
// factor that brings Lighthouse's own benchmark, the report's benchmarkIndex,
// to `cpuTarget`, as DevTools' calibrated CPU throttling does.
//
// The target is DevTools' "mid-tier mobile", a benchmarkIndex of 1 000 (a
// Pixel 5); Lighthouse says its 4x moves a desktop into the mid-tier mobile
// bracket. That is 2.5 to 4.4x on the runners, and on ten of them at once
// every page's median TBT was 37-63 ms. DevTools' "low-tier mobile", 264, is
// the Moto G Power whose screen the profile has: 7.5 to 15.5x on the
// runners, and there these pages block for 242 to 400 ms
// (LHCI_CPU_TARGET=264), over the budget. The shop meets its TBT budget on a
// mid-tier phone and not on a low-tier one. The benchmark is not the pages'
// work: at one score the runners' CPUs still differ in TBT by up to 1.5
// times (and an M1 blocks for a third of what they do), which the budget
// leaves room for. README, "Lighthouse", has every number.
//
// The throttling is applied (`devtools`), not estimated. Lighthouse's
// default estimate (`simulate`, Lantern) replays the page's own load, and
// on this machine a page's scripts arrive and run before its first paint,
// so the estimate puts Next.js's 132 KB of runtime on the LCP's path: 2.0 to
// 2.2 s for these pages, and 1.37 s for an empty Next 16 page at 4x. With the
// network and CPU throttled for real, the scripts load after the paint, as
// on a phone (README, "Lighthouse", has both).
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
  cpuTarget: Number(process.env.LHCI_CPU_TARGET || 1000),
  ci: {
    collect: {
      url: [`${shop}/`, `${shop}/c/backpacks`, `${shop}/search?q=titanium+stove`, `${shop}${product}`],
      numberOfRuns: Number(process.env.LHCI_RUNS || 3),
      settings: {
        // A server on this machine answers in a few milliseconds; the
        // throttled network adds the round trips a phone would wait.
        chromeFlags: '--headless=new --no-sandbox',
        throttlingMethod: process.env.LHCI_THROTTLING || 'devtools',
        // Lighthouse's own 4x where lhci runs without scripts/lighthouse.sh.
        throttling: { cpuSlowdownMultiplier: Number(process.env.LHCI_CPU_SLOWDOWN || 4) },
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
