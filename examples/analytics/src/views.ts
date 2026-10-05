// The pages, rendered on the server as strings. The charts are SVG made
// here (src/charts.ts), so a page paints whole with no script; client/app.ts
// adds the live parts.
import { createHash } from 'node:crypto';
import { candleChart, day, esc, fmt, full, hm, retentionTable, spark, trafficChart, widthClass, type Candle } from './charts.ts';
import type { Quote, Row } from './market.ts';
import { FACETS, RANGES, type Dashboard, type Filters, type Pulse, type Range } from './queries.ts';
import { CSS } from './styles.ts';

/**
 * Runs before the filters paint: closes them on a narrow screen, where they
 * would push the charts below the fold, and marks the page as scripted so
 * the filters apply as they change. Inline, so it runs before the first
 * paint and moves nothing after it; allowed by its hash.
 */
const EARLY = "document.documentElement.classList.add('js');var f=document.getElementById('filters');if(f&&matchMedia('(max-width:760px)').matches&&!f.dataset.set)f.open=false;";

const hash = (s: string) => `'sha256-${createHash('sha256').update(s).digest('base64')}'`;
export const CSP = [
  "default-src 'none'",
  `script-src 'self' ${hash(EARLY)}`,
  `style-src ${hash(CSS)}`,
  "img-src 'self'",
  "font-src 'self'",
  "connect-src 'self'",
  "form-action 'self'",
  "frame-ancestors 'none'",
  "base-uri 'none'",
].join('; ');

const BIRD =
  '<svg viewBox="0 0 32 32" aria-hidden="true"><path d="M3 15c5-1 9-4 12-8 1 3 3 5 6 6l8-3-5 6c1 4-1 9-6 11l1 4-4-3c-5 0-9-3-10-7l-4 1 2-3c-1-1-1-2 0-4Z"/><circle class="eye" cx="22" cy="12.5" r="1.3"/></svg>';

export interface Shell {
  title: string;
  description?: string;
  body: string;
  /** Indexable: only the public pages. */
  index?: boolean;
  scripts?: boolean;
}

export function page(s: Shell): string {
  return `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>${esc(s.title)}</title><meta name="description" content="${esc(s.description ?? 'Kestrel: product analytics on fenecdb.')}">
${s.index ? '' : '<meta name="robots" content="noindex">'}<meta name="theme-color" content="#f1f3ef" media="(prefers-color-scheme: light)"><meta name="theme-color" content="#121a21" media="(prefers-color-scheme: dark)">
<link rel="icon" href="/favicon.svg" type="image/svg+xml"><link rel="preload" href="/fonts/archivo.woff2" as="font" type="font/woff2" crossorigin>
<style>${CSS}</style></head><body>${s.body}${s.scripts === false ? '' : '<script src="/app.js" defer></script>'}</body></html>`;
}

function top(opts: { user?: { display: string }; sites?: { name: string; label: string }[]; current?: string; extra?: string }): string {
  let s = `<header class="top"><a class="brand" href="/">${BIRD}Kestrel</a>`;
  if (opts.sites?.length) {
    s += `<nav class="sites" aria-label="Sites">${opts.sites
      .map((x) => `<a href="/s/${esc(x.name)}"${x.name === opts.current ? ' aria-current="page"' : ''}>${esc(x.label)}</a>`)
      .join('')}<a href="/markets"${opts.current === 'markets' ? ' aria-current="page"' : ''}>Markets</a></nav>`;
  }
  s += opts.extra ?? '';
  s += '<span class="spacer"></span>';
  if (opts.user) {
    s += `<div class="who"><span class="hide-s">${esc(opts.user.display)}</span><form method="post" action="/signout"><button>Sign out</button></form></div>`;
  }
  return `${s}</header>`;
}

export function signInPage(error?: string): string {
  return page({
    title: 'Kestrel: product analytics',
    description: 'Kestrel counts the visits to your sites as they happen and keeps them in a database you run: pages, sources, signups and who comes back.',
    index: true,
    scripts: false,
    body: `<main class="signin">${BIRD.replace('<svg', '<svg class="mark"')}
<h1>Kestrel</h1>
<p>Counts the visits to your sites as they happen and keeps them in a database you run: pages, sources, signups and who comes back.</p>
<form method="post" action="/signin">
${error ? `<p class="error" role="alert">${esc(error)}</p>` : ''}
<label>Name<input name="name" autocomplete="username" required></label>
<label>Password<input name="password" type="password" autocomplete="current-password" required></label>
<button class="primary">Sign in</button>
</form>
<p class="note">The demo's people are nadia (Fieldnotes), omar (Tidepool) and ops (both), with the password kestrel-demo. The <a href="/markets">market data view</a> needs no sign-in.</p></main>`,
  });
}

const qs = (range: Range, f: Filters, change?: { facet: string; value: string }) => {
  const p = new URLSearchParams({ range });
  for (const k of FACETS) {
    for (const v of f[k]) if (!(change && change.facet === k && change.value === v)) p.append(k, v);
  }
  return `?${p}`;
};

function sourceNote(d: Dashboard): string {
  const ms = Math.max(d.timings.series ?? 0, d.timings.totals ?? 0, d.timings.pages ?? 0, d.timings.refs ?? 0);
  return d.source === 'raw'
    ? `<span class="src raw">From the raw events, ${ms.toFixed(0)} ms</span>`
    : `<span class="src">From the rollups, ${ms.toFixed(0)} ms</span>`;
}

const LABELS: Record<string, string> = { country: 'Country', device: 'Device', browser: 'Browser' };
const COUNTRY: Record<string, string> = {
  US: 'United States', DE: 'Germany', GB: 'United Kingdom', IN: 'India', TR: 'Türkiye', FR: 'France', BR: 'Brazil', CA: 'Canada', NL: 'Netherlands',
  JP: 'Japan', ES: 'Spain', PL: 'Poland', AU: 'Australia', SE: 'Sweden', MX: 'Mexico', ZZ: 'Unknown',
};
const valueLabel = (facet: string, v: string) => (facet === 'country' ? (COUNTRY[v] ?? v) : facet === 'device' ? v[0].toUpperCase() + v.slice(1) : v);

function filtersForm(site: string, d: Dashboard, f: Filters): string {
  const set = FACETS.reduce((n, k) => n + f[k].length, 0);
  let s = `<details class="filters" id="filters" open${set ? ' data-set="1"' : ''}><summary>Filters${set ? ` (${set} set)` : ''}</summary>
<form method="get" action="/s/${esc(site)}"><input type="hidden" name="range" value="${esc(d.range)}">`;
  for (const k of FACETS) {
    const counts = d.facets[k];
    const max = Math.max(1, ...counts.map((c) => c.count));
    s += `<fieldset><legend>${LABELS[k]}</legend>`;
    // A value filtered on stays listed, even when the other filters leave none of it.
    const listed = [...counts];
    for (const v of f[k]) if (!listed.some((c) => c.value === v)) listed.push({ value: v, count: 0 });
    for (const c of listed) {
      const on = f[k].includes(c.value);
      s += `<label class="opt"><span class="bar ${widthClass(c.count / max)}"></span><input type="checkbox" name="${k}" value="${esc(c.value)}"${on ? ' checked' : ''}><span class="v">${esc(valueLabel(k, c.value))}</span><span class="c">${fmt(c.count)}</span></label>`;
    }
    if (!listed.length) s += '<p class="empty">No events in this range.</p>';
    s += '</fieldset>';
  }
  s += `<button class="apply">Apply filters</button>${set ? ` <a class="btn" href="/s/${esc(site)}?range=${esc(d.range)}">Clear</a>` : ''}</form>`;
  s += `<p class="note">Events since ${esc(day(d.rawFrom))}, ${d.source === 'raw' ? 'from the raw events' : 'from the rollups'}. ${
    d.source === 'raw' ? 'Each list counts as if its own filter were not set, so a second country can be added.' : 'A filter reads the raw events, the last 30 days of them.'
  }</p></details>`;
  s += `<script>${EARLY}</script>`;
  return s;
}

function rankTable(caption: string, first: string, rows: { name: string; views: number; visitors: number | null }[]): string {
  if (!rows.length) return `<p class="empty">No pageviews in this range.</p>`;
  const max = Math.max(1, ...rows.map((r) => r.views));
  const visitors = rows.some((r) => r.visitors !== null);
  return `<table class="rank"><caption class="sr">${esc(caption)}</caption><thead><tr><th scope="col">${esc(first)}</th>${
    visitors ? '<th scope="col" class="num">Visitors</th>' : ''
  }<th scope="col" class="num">Views</th></tr></thead><tbody>${rows
    .map(
      (r) =>
        `<tr><td><span class="bar ${widthClass(r.views / max)}"></span><span class="name" title="${esc(r.name)}">${esc(r.name)}</span></td>${
          visitors ? `<td class="num">${full(r.visitors ?? 0)}</td>` : ''
        }<td class="num">${full(r.views)}</td></tr>`,
    )
    .join('')}</tbody></table>`;
}

function funnelList(d: Dashboard): string {
  const first = d.funnel[0]?.users ?? 0;
  return `<ol class="funnel">${d.funnel
    .map((s, i) => {
      // A first step past a distinct count's bound is -1: shown as such, the bars against the second.
      const base = first > 0 ? first : (d.funnel[1]?.users ?? 0);
      const share = s.users < 0 ? 1 : base ? s.users / base : 0;
      const prev = i ? d.funnel[i - 1].users : 0;
      const drop = i && prev > 0 ? ` <span class="drop">${((s.users / prev) * 100).toFixed(1)}% of the step before</span>` : '';
      return `<li><div class="row"><span>${esc(s.label)}${drop}</span><span class="n">${s.users < 0 ? 'Over 1M' : full(s.users)}</span></div><div class="track"><div class="fill ${widthClass(share)}"></div></div></li>`;
    })
    .join('')}</ol>`;
}

export function pulseBlock(p: Pulse | null): string {
  const active = p?.active ?? 0;
  const minutes = p?.minutes ?? [];
  return `<div class="now"><span class="big" id="active">${full(active)}</span><span class="lbl"><span class="live" id="live"></span>visitors on the site in the last five minutes</span></div>
<div id="spark">${spark(minutes.length ? minutes : new Array(30).fill(0))}</div>
<p class="minor" id="views30"><strong>${full(p?.views ?? 0)}</strong> pageviews in the last 30 minutes${p?.at ? `, as of ${esc(hm(Date.parse(p.at)))} UTC` : ''}</p>`;
}

export function dashboardPage(opts: {
  site: { name: string; label: string };
  sites: { name: string; label: string }[];
  user: { display: string };
  d: Dashboard;
  filters: Filters;
  pulse: Pulse | null;
  ms: number;
}): string {
  const { site, d, filters } = opts;
  const range = RANGES[d.range];
  const ranges = `<nav class="seg" aria-label="Range">${(Object.keys(RANGES) as Range[])
    .map((r) => `<a href="/s/${esc(site.name)}${qs(r, filters)}"${r === d.range ? ' aria-current="page"' : ''}>${r}</a>`)
    .join('')}</nav>`;
  const visitorsKey = d.visitorStep === d.step ? 'Visitors' : 'Visitors a day';
  const body = `${top({ user: opts.user, sites: opts.sites, current: site.name, extra: ranges })}
<div class="pulse" id="pulse" data-site="${esc(site.name)}" aria-live="polite">${pulseBlock(opts.pulse)}</div>
<div class="layout">
<aside>${filtersForm(site.name, d, filters)}</aside>
<main>
<div class="head"><h1>${esc(site.label)}, ${esc(range.label.toLowerCase())}</h1>${sourceNote(d)}</div>
${d.notice ? `<p class="note" role="status">${esc(d.notice)}</p>` : ''}
<dl class="totals"><div><dt>Visitors</dt><dd>${d.totals.visitors < 0 ? 'Over 1M' : full(d.totals.visitors)}</dd></div><div><dt>Pageviews</dt><dd>${full(d.totals.views)}</dd></div><div><dt>Events</dt><dd>${full(d.totals.events)}</dd></div></dl>
<div class="key"><span>Pageviews</span><span class="vis">${visitorsKey}</span></div>
${trafficChart(d.series, d.step, d.visitorStep, `Pageviews and visitors, ${range.label.toLowerCase()}`)}
<div class="cols">
<section><div class="head"><h2>Top pages</h2></div>${rankTable('Top pages', 'Page', d.pages.map((p) => ({ name: p.path, views: p.views, visitors: p.visitors })))}</section>
<section><div class="head"><h2>Where visitors came from</h2></div>${rankTable(
    'Referring sites',
    'Site',
    d.refs.map((r) => ({ name: r.ref || 'Direct or unknown', views: r.views, visitors: r.visitors })),
  )}</section>
</div>
<div class="cols">
<section><div class="head"><h2>Signup funnel</h2><span class="src${d.source === 'raw' ? ' raw' : ''}">From the ${d.source === 'raw' ? 'raw events' : 'rollups'}, ${(d.timings.funnel ?? 0).toFixed(0)} ms</span></div>${funnelList(d)}
<p class="note">A visitor counts at a step when they took it no earlier than the first time they took the step before.</p></section>
<section><div class="head"><h2>Who comes back</h2><span class="src">From the rollups, ${(d.timings.retention ?? 0).toFixed(0)} ms</span></div><div class="scroll">${retentionTable(d.retention)}</div></section>
</div>
${
  d.source === 'rollups'
    ? '<p class="note">Over a day, the rollups answer: counts by minute, day, page and referrer, a row per visitor a day. They hold no visitors per page, so those columns show for a day or less, or with a filter set.</p>'
    : ''
}
</main></div>
<footer>Times are UTC. Page made in ${opts.ms.toFixed(0)} ms. Kestrel is an example of <a href="https://github.com/fenecdb/fenec/tree/main/examples/analytics">fenecdb</a>; every site, visitor and number here is invented.</footer>`;
  return page({ title: `${site.label}, ${range.label.toLowerCase()}: Kestrel`, body });
}

const pct = (a: number, b: number) => (b ? ((a - b) / b) * 100 : 0);

export function quoteRows(rows: Row[], quotes: Map<string, Quote>, sel: string, window: number, interval: string): string {
  return rows
    .map((r) => {
      const q = quotes.get(r.sym);
      const px = q?.px ?? r.px;
      const ch = pct(px, r.open);
      const cls = ch >= 0 ? 'up' : 'down';
      return `<tr data-sym="${esc(r.sym)}"${r.sym === sel ? ' aria-current="true"' : ''}><td><a href="/markets?sym=${esc(r.sym)}&amp;window=${window}&amp;interval=${esc(interval)}">${esc(
        r.sym,
      )}</a></td><td class="num px">${px.toFixed(2)}</td><td class="num ch ${cls}">${ch >= 0 ? '+' : '−'}${Math.abs(ch).toFixed(2)}%</td><td class="num hide-s">${r.high.toFixed(
        2,
      )}</td><td class="num hide-s">${r.low.toFixed(2)}</td><td class="num">${r.vwap.toFixed(2)}</td><td class="num hide-s">${fmt(r.volume)}</td></tr>`;
    })
    .join('');
}

export const WINDOWS = [30, 60, 120, 360] as const;
export const INTERVALS = ['1m', '5m', '15m'] as const;

export function marketsPage(opts: {
  user?: { display: string };
  sites?: { name: string; label: string }[];
  sym: string;
  window: number;
  interval: string;
  rows: Row[];
  quotes: Map<string, Quote>;
  bars: Candle[];
  ms: Record<string, number>;
}): string {
  const { sym, window, interval } = opts;
  const link = (w: number, i: string) => `/markets?sym=${esc(sym)}&amp;window=${w}&amp;interval=${i}`;
  const sel = opts.rows.find((r) => r.sym === sym);
  const q = opts.quotes.get(sym);
  const head = sel
    ? `<dl class="totals"><div><dt>Last</dt><dd id="last">${(q?.px ?? sel.px).toFixed(2)}</dd></div><div><dt>VWAP, last ${window} min</dt><dd>${sel.vwap.toFixed(2)}</dd></div><div><dt>High</dt><dd>${sel.high.toFixed(
        2,
      )}</dd></div><div><dt>Low</dt><dd>${sel.low.toFixed(2)}</dd></div><div><dt>Traded</dt><dd>${fmt(sel.volume)}</dd></div></dl>`
    : '<p class="empty">No ticks for this symbol in the window.</p>';
  const body = `${top({ user: opts.user, sites: opts.sites ?? [], current: 'markets' })}
<main class="layout mk" id="market" data-sym="${esc(sym)}" data-window="${window}" data-interval="${esc(interval)}">
<div>
<div class="head"><h1>${esc(sym)}</h1><span class="src raw">Bars and VWAP from ticks by bucket, ${(opts.ms.bars ?? 0).toFixed(0)} ms</span></div>
${head}
<div class="controls"><nav class="seg" aria-label="Window">${WINDOWS.map((w) => `<a href="${link(w, interval)}"${w === window ? ' aria-current="page"' : ''}>${w < 60 ? `${w}m` : `${w / 60}h`}</a>`).join('')}</nav>
<nav class="seg" aria-label="Bar width">${INTERVALS.map((i) => `<a href="${link(window, i)}"${i === interval ? ' aria-current="page"' : ''}>${i} bars</a>`).join('')}</nav>
<span class="key"><span class="vis">VWAP of each bar</span></span></div>
<div id="candles">${candleChart(opts.bars, `${sym}: ${interval} bars over the last ${window} minutes, with each bar's VWAP`)}</div>
</div>
<section class="quotes-wrap"><div class="head"><h2>All symbols, last ${window} minutes</h2><span class="src raw">One statement, ${(opts.ms.window ?? 0).toFixed(0)} ms; prices live</span></div>
<div class="scroll"><table class="quotes"><caption class="sr">Every symbol's last price, change over the window, high, low, VWAP and volume</caption><thead><tr><th scope="col">Symbol</th><th scope="col" class="num">Last</th><th scope="col" class="num">Change</th><th scope="col" class="num hide-s">High</th><th scope="col" class="num hide-s">Low</th><th scope="col" class="num">VWAP</th><th scope="col" class="num hide-s">Volume</th></tr></thead>
<tbody id="quotes">${quoteRows(opts.rows, opts.quotes, sym, window, interval)}</tbody></table></div></section>
</main>
<footer>Ticks from a seeded random walk of 50 invented symbols; prices are no one's. Times are UTC. <a href="/">Kestrel</a> is an example of <a href="https://github.com/fenecdb/fenec/tree/main/examples/analytics">fenecdb</a>.</footer>`;
  return page({
    title: `${sym} and 49 more: market data on fenecdb`,
    description: 'Ticks for 50 invented symbols as they land: one-minute bars, VWAP and the latest prices, each a FenecQL statement over the raw ticks.',
    index: true,
    body,
  });
}
