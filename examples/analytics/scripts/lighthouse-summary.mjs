// Each page's median run from .lighthouseci/: the scores and the numbers
// the budgets are about, as a Markdown table for the README.
import { readFileSync, readdirSync } from 'node:fs';

const dir = '.lighthouseci';
const runs = readdirSync(dir)
  .filter((f) => f.startsWith('lhr-') && f.endsWith('.json'))
  .map((f) => JSON.parse(readFileSync(`${dir}/${f}`, 'utf8')));
const byUrl = new Map();
for (const r of runs) byUrl.set(r.finalDisplayedUrl ?? r.finalUrl, [...(byUrl.get(r.finalDisplayedUrl ?? r.finalUrl) ?? []), r]);

const median = (xs) => {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.floor(s.length / 2)];
};
const kb = (b) => `${(b / 1024).toFixed(1)} KB`;
const calibrated = Number(process.env.LHCI_CPU_INDEX) || 0;
console.log('| page | perf | a11y | best practices | SEO | FCP | LCP | TBT | CLS | JS (compressed) | page weight |');
console.log('| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |');
const each = [];
for (const [url, rs] of byUrl) {
  // The run whose LCP is the median, so every number in a row is one run's.
  const lcp = median(rs.map((r) => r.audits['largest-contentful-paint'].numericValue));
  const r = rs.find((x) => x.audits['largest-contentful-paint'].numericValue === lcp);
  const items = r.audits['resource-summary'].details.items;
  const of = (t) => items.find((i) => i.resourceType === t)?.transferSize ?? 0;
  const score = (c) => Math.round((r.categories[c]?.score ?? 0) * 100);
  const path = new URL(url).pathname + new URL(url).search;
  console.log(
    `| \`${path}\` | ${score('performance')} | ${score('accessibility')} | ${score('best-practices')} | ${score('seo')} | ` +
      `${(r.audits['first-contentful-paint'].numericValue / 1000).toFixed(2)} s | ${(lcp / 1000).toFixed(2)} s | ` +
      `${Math.round(r.audits['total-blocking-time'].numericValue)} ms | ${r.audits['cumulative-layout-shift'].numericValue.toFixed(3)} | ` +
      `${kb(of('script'))} | ${kb(of('total'))} |`,
  );
  each.push(
    `\`${path}\`: ` +
      rs
        .map((x) => {
          const index = Math.round(x.environment.benchmarkIndex);
          const off = calibrated && Math.abs(index - calibrated) > calibrated / 5 ? ', off' : '';
          return `${Math.round(x.audits['total-blocking-time'].numericValue)} ms (${index}${off})`;
        })
        .join(', '),
  );
}
// Each run's TBT -- the budget is on their median, which need not be the
// row's run -- and the benchmarkIndex Lighthouse measured as the run began
// (a cold one, some 7% under the warm calibration's on GitHub's runners).
// One a fifth or more off the index the slowdown was worked out from ran
// on a machine busier or idler than that, so on another device.
const slowdown = runs[0]?.configSettings.throttling.cpuSlowdownMultiplier;
console.log(
  `\nThe CPU slowed ${slowdown}x${calibrated ? ` (worked out at benchmarkIndex ${calibrated})` : ''}. ` +
    `Each run's TBT, and the benchmarkIndex it began at:\n${each.map((l) => `- ${l}`).join('\n')}`,
);
