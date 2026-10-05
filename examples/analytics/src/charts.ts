// Charts as SVG and HTML text, made on the server for the first paint and in
// the browser for live updates (client/app.ts bundles this file). No chart
// library: these are a few shapes -- columns with a step line, a sparkline,
// candles, and a table of shaded cells -- and a library that draws them
// would weigh more than the whole page.
//
// A plot is an SVG stretched to its box (`preserveAspectRatio="none"`, a
// unit a bucket), so it fills a phone's width as it fills a desktop's; its
// strokes keep their width (`vector-effect`), and its labels are HTML
// beside it, so text never stretches. Colours come from CSS classes: the
// theme and dark mode reach them, and a strict content security policy
// allows them where an inline style would not.

export const esc = (s: unknown) =>
  String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c] as string);

export const fmt = (n: number) => (Math.abs(n) >= 10_000 ? `${(n / 1000).toFixed(Math.abs(n) >= 100_000 ? 0 : 1)}k` : Math.round(n).toLocaleString('en-US'));
export const full = (n: number) => Math.round(n).toLocaleString('en-US');

/** Evenly spaced gridlines from 0 to a round bound at or over `max`. */
export function ticks(max: number, count = 4): number[] {
  if (max <= 0) return [0, 1];
  const raw = max / count;
  const mag = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((s) => s >= raw) ?? raw;
  const out: number[] = [];
  for (let k = 0; k * step <= max + step * 0.999; k++) out.push(k * step);
  return out;
}

const pad2 = (n: number) => String(n).padStart(2, '0');
const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
export const day = (t: number) => {
  const d = new Date(t);
  return `${MONTHS[d.getUTCMonth()]} ${d.getUTCDate()}`;
};
export const hm = (t: number) => {
  const d = new Date(t);
  return `${pad2(d.getUTCHours())}:${pad2(d.getUTCMinutes())}`;
};

/** A bucket's start as a short label, by the bucket's width (UTC). */
export function when(t: number, step: number): string {
  if (step >= 86_400_000) return day(t);
  if (step >= 3_600_000 && new Date(t).getUTCHours() === 0) return day(t);
  return hm(t);
}

/** Labels under a plot: a slot a bucket, the text in every `every`th. */
function xAxis(labels: string[], every: number): string {
  let s = '<div class="xa" aria-hidden="true">';
  // Every other label is dropped on a phone's width (`.alt`).
  labels.forEach((l, i) => (s += i % every === 0 ? `<span${(i / every) % 2 ? ' class="alt"' : ''}>${esc(l)}</span>` : '<span></span>'));
  return `${s}</div>`;
}

/** Labels beside a plot, top to bottom, spaced as its gridlines. */
function yAxis(values: string[]): string {
  return `<div class="ya" aria-hidden="true">${values.map((v) => `<span>${esc(v)}</span>`).join('')}</div>`;
}

const gridlines = (n: number, levels: number) =>
  Array.from({ length: levels }, (_, k) => {
    const y = ((100 * k) / (levels - 1)).toFixed(2);
    return `<line class="grid" x1="0" x2="${n}" y1="${y}" y2="${y}"/>`;
  }).join('');

export interface TimePoint {
  t: number;
  views: number;
  visitors: number | null;
}

/**
 * Pageviews as columns, visitors as a step line over them, a bucket a
 * column. `visitorStep` wider than `step` (daily visitors over 6-hour
 * columns) draws each level across the columns its day covers.
 */
export function trafficChart(points: TimePoint[], step: number, visitorStep: number, title: string): string {
  const n = Math.max(points.length, 1);
  const max = Math.max(1, ...points.map((p) => Math.max(p.views, p.visitors ?? 0)));
  const grid = ticks(max);
  const top = grid[grid.length - 1];
  const y = (v: number) => (100 * (1 - v / top)).toFixed(2);
  const total = points.reduce((a, p) => a + p.views, 0);
  const peak = points.reduce((a, p) => (p.views > a.views ? p : a), points[0] ?? { t: 0, views: 0, visitors: 0 });
  let s = `<svg class="plot" viewBox="0 0 ${n} 100" preserveAspectRatio="none" role="img" aria-label="${esc(
    `${title}: ${full(total)} pageviews, the most ${full(peak.views)} at ${when(peak.t, step)}`,
  )}">`;
  s += gridlines(n, grid.length);
  points.forEach((p, i) => {
    const tip = `${when(p.t, step)}: ${full(p.views)} pageviews${p.visitors !== null && visitorStep === step ? `, ${full(p.visitors)} visitors` : ''}`;
    // A full-height hit area, so a short column still shows its numbers.
    s += `<g><title>${esc(tip)}</title><rect class="hit" x="${i}" y="0" width="1" height="100"/><rect class="col" x="${i + 0.16}" y="${y(p.views)}" width="0.68" height="${(100 - Number(y(p.views))).toFixed(2)}"/></g>`;
  });
  let d = '';
  for (let i = 0; i < points.length; ) {
    const v = points[i].visitors;
    let j = i + 1;
    if (visitorStep > step) while (j < points.length && Math.floor(points[j].t / visitorStep) === Math.floor(points[i].t / visitorStep)) j++;
    if (v !== null) d += `${d ? 'L' : 'M'}${i} ${y(v)}H${j}`;
    i = j;
  }
  if (d) s += `<path class="line" d="${d}"/>`;
  s += '</svg>';
  const every = Math.ceil(n / (n > 40 ? 8 : 6));
  return `<figure class="tc"><div class="area">${yAxis(grid.map(fmt).reverse())}${s}</div>${xAxis(
    points.map((p) => when(p.t, step)),
    every,
  )}</figure>`;
}

/** The last half hour's pageviews a minute, small; the newest minute marked. */
export function spark(minutes: number[]): string {
  const n = Math.max(minutes.length, 1);
  const max = Math.max(1, ...minutes);
  let s = `<svg class="spark" viewBox="0 0 ${n} 40" preserveAspectRatio="none" role="img" aria-label="Pageviews a minute over the last ${n} minutes, ${full(minutes[minutes.length - 1] ?? 0)} this minute">`;
  minutes.forEach((m, i) => {
    const h = Math.max(m ? 3 : 1, (m / max) * 40);
    s += `<rect class="${i === minutes.length - 1 ? 'now' : 'col'}" x="${i + 0.12}" y="${(40 - h).toFixed(2)}" width="0.76" height="${h.toFixed(2)}"/>`;
  });
  return `${s}</svg>`;
}

export interface Candle {
  bar: string;
  open: number;
  high: number;
  low: number;
  close: number;
  volume: number;
  vwap: number;
}

/** Candles with their VWAP as a line, and volume under them. */
export function candleChart(bars: Candle[], label: string): string {
  const n = Math.max(bars.length, 1);
  const hi = Math.max(0, ...bars.map((b) => b.high));
  const lo = Math.min(hi, ...bars.map((b) => b.low));
  const span = hi - lo || hi * 0.01 || 1;
  const top = hi + span * 0.06;
  const bottom = lo - span * 0.06;
  const y = (v: number) => (100 * (1 - (v - bottom) / (top - bottom))).toFixed(2);
  const vmax = Math.max(1, ...bars.map((b) => b.volume));
  let s = `<svg class="plot" viewBox="0 0 ${n} 100" preserveAspectRatio="none" role="img" aria-label="${esc(label)}">${gridlines(n, 5)}`;
  let vol = `<svg class="vols" viewBox="0 0 ${n} 100" preserveAspectRatio="none" aria-hidden="true">`;
  bars.forEach((b, i) => {
    const up = b.close >= b.open;
    const t = Date.parse(b.bar);
    const tip = `${hm(t)}: open ${b.open.toFixed(2)}, high ${b.high.toFixed(2)}, low ${b.low.toFixed(2)}, close ${b.close.toFixed(2)}, VWAP ${b.vwap.toFixed(2)}, ${full(b.volume)} traded`;
    const y1 = y(Math.max(b.open, b.close));
    const h = Math.max(0.4, Number(y(Math.min(b.open, b.close))) - Number(y1));
    s += `<g class="${up ? 'up' : 'down'}"><title>${esc(tip)}</title><rect class="hit" x="${i}" y="0" width="1" height="100"/><line class="wick" x1="${i + 0.5}" x2="${i + 0.5}" y1="${y(b.high)}" y2="${y(b.low)}"/><rect class="body" x="${i + 0.18}" y="${y1}" width="0.64" height="${h.toFixed(2)}"/></g>`;
    const vh = ((b.volume / vmax) * 100).toFixed(2);
    vol += `<rect class="${up ? 'up' : 'down'}" x="${i + 0.18}" y="${(100 - Number(vh)).toFixed(2)}" width="0.64" height="${vh}"/>`;
  });
  if (bars.length) s += `<path class="vwap" d="${bars.map((b, i) => `${i ? 'L' : 'M'}${i + 0.5} ${y(b.vwap)}`).join('')}"/>`;
  s += '</svg>';
  vol += '</svg>';
  const levels = [0, 1, 2, 3, 4].map((k) => (top - ((top - bottom) * k) / 4).toFixed(2));
  return `<figure class="cc"><div class="area">${s}${yAxis(levels)}</div><div class="area vol">${vol}<div class="ya"></div></div>${xAxis(
    bars.map((b) => hm(Date.parse(b.bar))),
    Math.ceil(n / 7),
  )}</figure>`;
}

export interface CohortRow {
  week: number;
  size: number;
  active: number[];
}

/**
 * Retention as a table: a row a cohort, a column a week since, each cell
 * the share of the cohort that came back, shaded by it. A table, not a
 * picture, so a screen reader reads it as one.
 */
export function retentionTable(cohorts: CohortRow[]): string {
  const weeks = Math.max(0, ...cohorts.map((c) => c.active.length));
  let s = '<table class="ret"><caption>The share of each week\'s new visitors who came back 1, 2, 3 or more weeks after; this week, dashed, is not over.</caption><thead><tr><th scope="col">First came</th><th scope="col" class="num">Visitors</th>';
  for (let w = 0; w < weeks; w++) s += `<th scope="col" class="num">${w === 0 ? '0' : `+${w}`}</th>`;
  s += '</tr></thead><tbody>';
  for (const c of cohorts) {
    s += `<tr><th scope="row">Week of ${day(c.week)}</th><td class="num">${full(c.size)}</td>`;
    for (let w = 0; w < weeks; w++) {
      if (w >= c.active.length) {
        s += '<td class="none"></td>';
        continue;
      }
      const share = c.size ? c.active[w] / c.size : 0;
      const shade = w === 0 ? 9 : Math.min(8, Math.ceil(Math.sqrt(share) * 14));
      // The week now running is not over: its share will grow.
      const part = w > 0 && w === c.active.length - 1 ? ' part' : '';
      s += `<td class="num h${shade}${part}" title="${full(c.active[w])} of ${full(c.size)}">${w === 0 ? '100%' : `${(share * 100).toFixed(share < 0.1 ? 1 : 0)}%`}</td>`;
    }
    s += '</tr>';
  }
  return `${s}</tbody></table>`;
}

/** A width class for a share, in steps of 5%: `.w0` to `.w100`. */
export const widthClass = (share: number) => `w${Math.round(Math.max(0, Math.min(1, share)) * 20) * 5}`;
