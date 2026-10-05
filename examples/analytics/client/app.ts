// The pages' only script, and none of their first paint: the dashboard's
// "now" and the market page's prices and bars kept live by polling, and
// the filters applied as they change.
import { candleChart, full, hm, spark, type Candle } from '../src/charts.ts';

const $ = (id: string) => document.getElementById(id);

/**
 * Calls `f` every `ms` while the page is visible, the first time after
 * `after` ms: the page has loaded and gone quiet by then, and a poll does
 * not keep a measurement of its load waiting.
 */
const every = (ms: number, f: () => Promise<void>, after = 4000) =>
  setTimeout(() => {
    setInterval(() => {
      if (document.visibilityState === 'visible') f().catch(() => {});
    }, ms);
  }, after);

// The dashboard's "now", polled: 304 while the site's pulse has not changed.
const pulse = $('pulse');
if (pulse?.dataset.site) {
  const live = $('live');
  let etag = '';
  every(2000, async () => {
    const r = await fetch(`/s/${encodeURIComponent(pulse.dataset.site as string)}/now.json`, { headers: etag ? { 'if-none-match': etag } : {} });
    live?.classList.toggle('on', r.ok || r.status === 304);
    if (r.status !== 200) return;
    etag = r.headers.get('etag') ?? '';
    const p = (await r.json()) as { active: number; views: number; minutes: number[]; at: string } | null;
    if (!p) return;
    const a = $('active');
    if (a) a.textContent = full(p.active);
    const s = $('spark');
    if (s) s.innerHTML = spark(p.minutes);
    const v = $('views30');
    if (v) v.innerHTML = `<strong>${full(p.views)}</strong> pageviews in the last 30 minutes, as of ${hm(Date.parse(p.at))} UTC`;
  });
}

// A filter applies as it is ticked; the button is for pages without script.
const form = document.querySelector<HTMLFormElement>('#filters form');
form?.addEventListener('change', () => form.requestSubmit());

// The market page: last prices every second (304 while none moved), bars
// and the window's figures every five.
const market = $('market');
if (market) {
  const { sym = '', window: win = '60', interval = '1m' } = market.dataset;
  let etag = '';
  const prices = async () => {
    const r = await fetch('/markets/quotes.json', { headers: etag ? { 'if-none-match': etag } : {} });
    if (r.status !== 200) return;
    etag = r.headers.get('etag') ?? '';
    for (const q of (await r.json()) as { sym: string; px: number }[]) {
      const row = document.querySelector<HTMLTableRowElement>(`tr[data-sym="${q.sym}"]`);
      const cell = row?.querySelector('.px');
      if (!cell) continue;
      const text = q.px.toFixed(2);
      if (cell.textContent !== text) {
        cell.textContent = text;
        cell.classList.remove('flash');
        void (cell as HTMLElement).offsetWidth;
        cell.classList.add('flash');
      }
      if (q.sym === sym) {
        const last = $('last');
        if (last) last.textContent = text;
      }
    }
  };
  const barsAndRows = async () => {
    const r = await fetch(`/markets/bars.json?sym=${sym}&window=${win}&interval=${interval}`);
    if (!r.ok) return;
    const b = (await r.json()) as { bars: Candle[]; rows: string };
    const c = $('candles');
    if (c) c.innerHTML = candleChart(b.bars, `${sym}: ${interval} bars over the last ${win} minutes, with each bar's VWAP`);
    const t = $('quotes');
    if (t) t.innerHTML = b.rows;
  };
  every(1000, prices);
  every(5000, barsAndRows);
}
