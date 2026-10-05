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
console.log('| page | perf | a11y | best practices | SEO | FCP | LCP | TBT | CLS | JS (compressed) | page weight |');
console.log('| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |');
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
}
