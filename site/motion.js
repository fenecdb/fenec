/* The page's moving pictures: each section shows its feature working.

   A scene is a function of its own time alone, drawn onto the canvas in its
   section, so a section loops its scenes while it is in view and stops when
   it is not, and a reader can step between them. Every number on screen is
   one the docs measure. The section's heading carries the words; the scene
   carries the picture. */

import { POINTS, EDGES, FLOW, RUN, COLORS } from './fennec.js';

const W = 1280, H = 720;
const P = {
  night: '#070A18', night2: '#1B1233', panel: '#150F26', panel2: '#1D1430', rule: '#3A2A3F',
  sun: '#F4A93C', hot: '#FFCE73', ember: '#FF7A3D', oasis: '#4FE0C4', star: '#FFF4DC',
  sand: '#F0E2C4', sand2: '#CDB894', dim: '#96856A', purple: '#C79BF2', red: '#FF6B5B',
};
const SANS = '"Bricolage Grotesque", system-ui, sans-serif';
const MONO = '"IBM Plex Mono", ui-monospace, monospace';

const clamp = (x, a = 0, b = 1) => Math.min(b, Math.max(a, x));
const span = (t, a, b) => clamp((t - a) / (b - a));
const ease = (x) => 1 - Math.pow(1 - clamp(x), 3);
const inOut = (x) => { x = clamp(x); return x < 0.5 ? 4 * x * x * x : 1 - Math.pow(-2 * x + 2, 3) / 2; };
const lerp = (a, b, k) => a + (b - a) * k;

function rng(seed) { return () => ((seed = (seed * 16807) % 2147483647) / 2147483647); }

/* --------------------------------------------------------------- drawing */

function text(c, s, x, y, { size = 22, weight = 400, color = P.sand, font = SANS, align = 'left', alpha = 1, base = 'alphabetic' } = {}) {
  c.save();
  c.globalAlpha *= alpha;
  c.font = `${weight} ${size}px ${font}`;
  c.fillStyle = color;
  c.textAlign = align;
  c.textBaseline = base;
  c.fillText(s, x, y);
  c.restore();
}

function box(c, x, y, w, h, { r = 12, fill = P.panel, stroke = P.rule, alpha = 1, line = 1.5 } = {}) {
  c.save();
  c.globalAlpha *= alpha;
  c.beginPath();
  c.roundRect(x, y, w, h, r);
  if (fill) { c.fillStyle = fill; c.fill(); }
  if (stroke) { c.strokeStyle = stroke; c.lineWidth = line; c.stroke(); }
  c.restore();
}

function dot(c, x, y, r, color, alpha = 1) {
  c.save();
  c.globalAlpha *= alpha;
  c.fillStyle = color;
  c.beginPath();
  c.arc(x, y, r, 0, Math.PI * 2);
  c.fill();
  c.restore();
}

function line(c, x1, y1, x2, y2, color, width = 1.5, alpha = 1, dash = null) {
  c.save();
  c.globalAlpha *= alpha;
  c.strokeStyle = color;
  c.lineWidth = width;
  if (dash) c.setLineDash(dash);
  c.beginPath();
  c.moveTo(x1, y1);
  c.lineTo(x2, y2);
  c.stroke();
  c.restore();
}

// FenecQL and shell, coloured as the site's code blocks are; `chars` types it.
const KW = new Set('create collection get put where near limit select and or in lookup on order desc from with format csv header copy'.split(' '));
function code(c, src, x, y, { size = 20, chars = Infinity, alpha = 1, lead = 1.55 } = {}) {
  c.save();
  c.globalAlpha *= alpha;
  c.font = `400 ${size}px ${MONO}`;
  c.textBaseline = 'alphabetic';
  const cw = c.measureText('M').width;
  let n = 0;
  src.split('\n').forEach((ln, row) => {
    const re = /("[^"]*"?)|(@\w+)|(\$\d+)|(\b\d[\d.]*\b)|(\w+)|(\s+)|(.)/g;
    let col = 0, m;
    while ((m = re.exec(ln))) {
      const tok = m[0];
      const color = m[1] ? P.hot : m[2] ? P.purple : m[3] ? P.purple : m[4] ? P.sun : m[5] && KW.has(tok) ? P.oasis : P.sand;
      const shown = tok.slice(0, Math.max(0, chars - n));
      c.fillStyle = color;
      c.fillText(shown, x + col * cw, y + row * size * lead);
      col += tok.length;
      n += tok.length;
      if (n >= chars) break;
    }
    n += 1;
  });
  c.restore();
}

// A code panel sized to its source, with a label above it.
function codePanel(c, src, x, y, { size = 19, chars, alpha = 1, label, w } = {}) {
  const lines = src.split('\n');
  c.font = `400 ${size}px ${MONO}`;
  const cw = c.measureText('M').width;
  const width = w ?? Math.max(...lines.map((l) => l.length)) * cw + 48;
  const height = lines.length * size * 1.55 + 32;
  box(c, x, y, width, height, { fill: '#0C0819', alpha });
  if (label) text(c, label, x + 18, y - 10, { size: 14, font: MONO, color: P.dim, alpha });
  code(c, src, x + 24, y + 22 + size, { size, chars, alpha });
  return { width, height };
}

function chip(c, s, x, y, { color = P.sand, fill = P.panel2, size = 17, alpha = 1, stroke = P.rule, font = SANS, weight = 500 } = {}) {
  c.font = `${weight} ${size}px ${font}`;
  const w = c.measureText(s).width + size * 1.4;
  const h = size * 2;
  box(c, x, y - h / 2, w, h, { r: h / 2, fill, stroke, alpha });
  text(c, s, x + size * 0.7, y + size * 0.36, { size, weight, color, alpha, font });
  return w;
}

// The mark, from the table the logo is drawn from, its light running through
// it once as the page's own mark's does (`t` in seconds).
function mark(c, x, y, s, alpha = 1, t = 0) {
  c.save();
  c.globalAlpha *= alpha;
  c.translate(x - 32 * s, y - 32 * s);
  c.scale(s, s);
  c.lineCap = 'round';
  c.lineJoin = 'round';
  c.strokeStyle = COLORS.line;
  c.lineWidth = 2;
  c.beginPath();
  for (const [a, b] of EDGES) { c.moveTo(...POINTS[a]); c.lineTo(...POINTS[b]); }
  c.stroke();
  // The light, as the page's own mark runs it: out from the right ear
  // through every edge, wave by wave, once (`t` from the scene's start).
  c.strokeStyle = COLORS.light;
  c.lineWidth = 3;
  c.beginPath();
  for (const { from, to, delay } of FLOW) {
    const k = (t - 0.6 - delay) / RUN;          // after the scene fades in
    if (k <= 0 || k >= 1.45) continue;
    const a = Math.max(0, k - 0.45), b = Math.min(1, k);
    const p = POINTS[from], q = POINTS[to];
    c.moveTo(p[0] + (q[0] - p[0]) * a, p[1] + (q[1] - p[1]) * a);
    c.lineTo(p[0] + (q[0] - p[0]) * b, p[1] + (q[1] - p[1]) * b);
  }
  c.stroke();
  c.restore();
}

/* ------------------------------------------------------------ the data */

/* One cloud of documents for the vector scenes: three neighbourhoods, a
   language each mostly, linked to their nearest as an HNSW layer is. */
const CLOUD = (() => {
  const r = rng(11);
  const centres = [[790, 330, 'tr'], [1060, 300, 'en'], [940, 540, 'de']];
  const pts = [];
  for (let i = 0; i < 120; i++) {
    const [cx, cy, lang] = centres[i % 3];
    const a = r() * Math.PI * 2, d = Math.pow(r(), 0.7) * 150;
    const other = r() < 0.18 ? centres[Math.floor(r() * 3)][2] : lang;
    pts.push({ x: cx + Math.cos(a) * d * 1.25, y: cy + Math.sin(a) * d * 0.8, lang: other });
  }
  const dist = (a, b) => Math.hypot(a.x - b.x, a.y - b.y);
  const edges = [];
  // Each point's four nearest, and one long link as an upper layer gives,
  // so the graph is one piece and a walk can cross between neighbourhoods.
  pts.forEach((p, i) => {
    p.nb = pts.map((q, j) => [dist(p, q), j]).filter(([, j]) => j !== i).sort((a, b) => a[0] - b[0]).slice(0, 4).map(([, j]) => j);
    if (i % 4 === 0) p.nb.push(Math.floor(r() * pts.length));
  });
  pts.forEach((p, i) => { for (const j of p.nb) if (!pts[j].nb.includes(i)) pts[j].nb.push(i); });
  pts.forEach((p, i) => { for (const j of p.nb) if (j > i) edges.push([i, j]); });
  const query = { x: 868, y: 390 };
  // From a far corner to the nearest that passes: the walk visits rows the
  // filter rejects as well, it only does not count them.
  const walk = (ok) => {
    const qd = (i) => dist(pts[i], query);
    const top = pts.map((p, i) => [qd(i), i]).filter(([, i]) => ok(pts[i])).sort((a, b) => a[0] - b[0]).slice(0, 5).map(([, i]) => i);
    let at = 0;
    for (let i = 0; i < pts.length; i++) if (pts[i].x > 1100 && pts[i].y < 300) { at = i; break; }
    // The shortest route by distance travelled, so the walk steps from
    // neighbour to neighbour rather than leaping.
    const best = new Map([[at, 0]]), prev = new Map([[at, -1]]), done = new Set();
    while (true) {
      let n = -1;
      for (const [k, v] of best) if (!done.has(k) && (n < 0 || v < best.get(n))) n = k;
      if (n < 0 || n === top[0]) break;
      done.add(n);
      for (const m of pts[n].nb) {
        const d = best.get(n) + dist(pts[n], pts[m]) ** 1.6;
        if (!best.has(m) || d < best.get(m)) { best.set(m, d); prev.set(m, n); }
      }
    }
    const path = [];
    for (let n = top[0]; n !== -1; n = prev.get(n)) path.unshift(n);
    return { path, top };
  };
  return { pts, edges, query, all: walk(() => true), tr: walk((p) => p.lang === 'tr') };
})();

const LANG = { tr: P.sun, en: P.oasis, de: P.purple };
const TITLES = ['Night at the oasis', 'Dune sea crossing', 'Caravan routes', 'Wells of the Sahara', 'Fennec ears'];
const SCORES = ['0.982', '0.961', '0.944', '0.930', '0.917'];

function cloud(c, t, { reveal = 1, dimOthers = 0, walkAt = -1, walkOf = CLOUD.all, edgesAlpha = 0.18, topAt = -1 } = {}) {
  const { pts, edges, query } = CLOUD;
  for (const [a, b] of edges) {
    const pa = pts[a], pb = pts[b];
    const fa = dimOthers && (pa.lang !== 'tr' || pb.lang !== 'tr') ? 1 - dimOthers * 0.85 : 1;
    line(c, pa.x, pa.y, pb.x, pb.y, P.sand2, 1, edgesAlpha * reveal * fa);
  }
  pts.forEach((p, i) => {
    const k = ease(span(reveal, i / pts.length * 0.6, i / pts.length * 0.6 + 0.4));
    const fa = dimOthers && p.lang !== 'tr' ? 1 - dimOthers * 0.85 : 1;
    dot(c, p.x, p.y, 4.2, LANG[p.lang], k * fa);
  });
  if (walkAt >= 0) {
    const { path } = walkOf;
    const hops = walkAt * path.length;
    for (let h = 0; h < path.length - 1 && h < hops - 1; h++) {
      const a = pts[path[h]], b = pts[path[h + 1]];
      const k = clamp(hops - 1 - h);
      line(c, a.x, a.y, lerp(a.x, b.x, k), lerp(a.y, b.y, k), P.oasis, 3, 0.95);
    }
    for (let h = 0; h < path.length && h < hops; h++) dot(c, pts[path[h]].x, pts[path[h]].y, 7, P.oasis, 0.9);
  }
  if (topAt >= 0) {
    walkOf.top.forEach((i, n) => {
      const k = ease(span(topAt, n * 0.12, n * 0.12 + 0.4));
      c.save();
      c.globalAlpha *= k;
      c.strokeStyle = P.star;
      c.lineWidth = 2.5;
      c.beginPath();
      c.arc(pts[i].x, pts[i].y, 12, 0, Math.PI * 2);
      c.stroke();
      c.restore();
    });
  }
  return query;
}

function queryStar(c, q, alpha) {
  dot(c, q.x, q.y, 22, P.oasis, 0.12 * alpha);
  dot(c, q.x, q.y, 9, P.star, alpha);
  text(c, 'your query', q.x + 16, q.y - 14, { size: 16, color: P.oasis, alpha, weight: 500 });
}

/* --------------------------------------------------------------- scenes */

const SCENES = [
  {
    key: 'tab',
    title: 'A database inside the browser tab',
    sub: '192 KB of gzipped WebAssembly. No server, no dependencies.',
    d: 7,
    draw(c, t) {
      const x = 120, y = 170, w = 700, h = 430;
      const k = ease(span(t, 0.2, 1));
      box(c, x, y + (1 - k) * 30, w, h, { r: 16, fill: '#100B20', alpha: k });
      box(c, x, y + (1 - k) * 30, w, 46, { r: 16, fill: P.panel2, alpha: k });
      [P.ember, P.sun, P.oasis].forEach((col, i) => dot(c, x + 24 + i * 18, y + 23 + (1 - k) * 30, 5.5, col, k));
      box(c, x + 92, y + 11 + (1 - k) * 30, 300, 25, { r: 12, fill: '#0C0819', alpha: k });
      text(c, 'my-app.com', x + 108, y + 29 + (1 - k) * 30, { size: 15, font: MONO, color: P.dim, alpha: k });
      // The page: a search box and results, served by the database inside.
      const a = ease(span(t, 0.8, 1.6));
      box(c, x + 40, y + 80, 380, 44, { r: 10, fill: '#0C0819', alpha: a });
      text(c, 'Search notes…', x + 60, y + 108, { size: 18, color: P.dim, alpha: a });
      for (let i = 0; i < 4; i++) {
        const b = ease(span(t, 3.2 + i * 0.25, 3.7 + i * 0.25));
        box(c, x + 40, y + 146 + i * 62, 380, 48, { r: 10, fill: P.panel, alpha: b });
        text(c, TITLES[i], x + 60, y + 176 + i * 62, { size: 18, color: P.sand, alpha: b, weight: 500 });
        text(c, SCORES[i], x + 400, y + 176 + i * 62, { size: 15, font: MONO, color: P.oasis, alpha: b, align: 'right' });
      }
      // The engine, as a glowing core in the corner of the page.
      const e = ease(span(t, 1.4, 2.4));
      const ex = x + 560, ey = y + 220;
      dot(c, ex, ey, 110, P.sun, 0.06 * e);
      dot(c, ex, ey, 70, P.sun, 0.08 * e);
      mark(c, ex, ey - 6, 1.5 * (0.85 + 0.15 * e), e, t);
      text(c, 'fenec.wasm', ex, ey + 74, { size: 18, font: MONO, color: P.hot, align: 'center', alpha: e });
      text(c, '192 KB gzipped', ex, ey + 100, { size: 16, font: MONO, color: P.dim, align: 'center', alpha: e });
      // Rows going in, and the search lighting the results.
      for (let i = 0; i < 6; i++) {
        const p = span(t, 2 + i * 0.12, 2.8 + i * 0.12);
        if (p <= 0 || p >= 1) continue;
        dot(c, lerp(x + 420, ex, inOut(p)), lerp(y + 170 + i * 30, ey, inOut(p)), 4, P.sand, 1 - p * 0.5);
      }
      // Kept in the browser: IndexedDB, or a file of the origin.
      const s = ease(span(t, 4.4, 5.2));
      const sx = 900, sy = 420;
      line(c, ex + 60, ey + 30, sx, sy, P.sun, 2, s * 0.8, [6, 6]);
      box(c, sx, sy - 36, 260, 72, { fill: P.panel, alpha: s });
      text(c, 'persist(db)', sx + 20, sy - 6, { size: 18, font: MONO, color: P.oasis, alpha: s });
      text(c, 'kept in IndexedDB', sx + 20, sy + 20, { size: 16, color: P.sand2, alpha: s });
      const n = ease(span(t, 5, 5.8));
      chip(c, 'works offline', 900, 250, { color: P.hot, alpha: n, size: 18 });
      chip(c, 'no server needed', 900, 300, { color: P.sand, alpha: n, size: 18 });
    },
  },
  {
    key: 'vectors',
    title: 'Vectors are a column type',
    sub: 'Declare the field, mark it @hnsw, and every row is a point in space.',
    d: 7,
    draw(c, t) {
      const src = 'create collection docs (\n  title text,\n  lang  text @hash,\n  embed vector<384> @hnsw(cosine)\n)';
      codePanel(c, src, 80, 170, { size: 20, chars: Math.floor(span(t, 0.3, 2.2) * src.length), alpha: ease(span(t, 0, 0.4)) });
      // The table: a row's vector shown as the strip of numbers it is.
      const r = rng(5);
      const strips = TITLES.map(() => Array.from({ length: 22 }, () => r()));
      const tk = ease(span(t, 2.2, 2.8));
      const tx = 80, ty = 420;
      text(c, 'title', tx, ty, { size: 15, font: MONO, color: P.dim, alpha: tk });
      text(c, 'embed', tx + 260, ty, { size: 15, font: MONO, color: P.dim, alpha: tk });
      TITLES.forEach((title, i) => {
        const k = ease(span(t, 2.4 + i * 0.12, 2.9 + i * 0.12));
        const y = ty + 22 + i * 46;
        box(c, tx - 12, y - 4, 560, 40, { r: 8, fill: P.panel, alpha: k });
        text(c, title, tx, y + 22, { size: 17, color: P.sand, alpha: k, weight: 500 });
        // The strip flies off to become its point.
        const fly = inOut(span(t, 4 + i * 0.15, 5.2 + i * 0.15));
        const target = CLOUD.pts[CLOUD.all.top[i]];
        strips[i].forEach((v, j) => {
          const sx = tx + 260 + j * 12, sy = y + 6;
          const px = lerp(sx, target.x, fly), py = lerp(sy, target.y, fly);
          c.save();
          c.globalAlpha = k * (1 - fly * 0.9);
          c.fillStyle = v > 0.5 ? P.sun : P.ember;
          c.globalAlpha *= 0.35 + v * 0.65;
          c.fillRect(px, py, 10 * (1 - fly) + 2, 26 * (1 - fly) + 2);
          c.restore();
        });
      });
      cloud(c, t, { reveal: span(t, 4.6, 6.6), edgesAlpha: 0 });
      const lk = ease(span(t, 5.6, 6.2));
      text(c, 'similar meaning, near each other', 1000, 680, { size: 17, color: P.sand2, align: 'center', alpha: lk });
    },
  },
  {
    key: 'near',
    title: 'near walks a graph',
    sub: 'The nearest documents in a fraction of a millisecond, without reading every row.',
    d: 8,
    draw(c, t) {
      const a = ease(span(t, 0, 0.5));
      const src = 'get docs\n  select title\n  near embed $1\n  limit 5';
      codePanel(c, src, 80, 170, { size: 22, chars: Math.floor(span(t, 0.2, 1.4) * src.length), alpha: a });
      const q = cloud(c, t, { reveal: 1, edgesAlpha: 0.22 * ease(span(t, 0.4, 1.4)), walkAt: span(t, 2.2, 4.6), topAt: span(t, 4.6, 5.6) });
      queryStar(c, q, ease(span(t, 1.4, 2)));
      text(c, 'starts anywhere, steps to whichever neighbour is closer', 80, 420, { size: 18, color: P.oasis, alpha: ease(span(t, 2.4, 3)) * (1 - ease(span(t, 4.8, 5.2))) });
      TITLES.forEach((title, i) => {
        const k = ease(span(t, 5 + i * 0.15, 5.5 + i * 0.15));
        box(c, 80, 400 + i * 50, 440, 40, { r: 8, fill: P.panel, alpha: k });
        text(c, title, 100, 426 + i * 50, { size: 17, color: P.sand, alpha: k, weight: 500 });
        text(c, SCORES[i], 500, 426 + i * 50, { size: 15, font: MONO, color: P.oasis, align: 'right', alpha: k });
      });
    },
  },
  {
    key: 'filter',
    title: 'Filters and search, together',
    sub: 'Narrow by any field. The page still fills with the nearest that match.',
    d: 7,
    draw(c, t) {
      const src = 'get docs\n  select title\n  where lang = "tr"\n  near embed $1\n  limit 5';
      codePanel(c, src, 80, 170, { size: 22, chars: t < 0.6 ? 24 : 24 + Math.floor(span(t, 0.6, 1.6) * 20) + Math.floor(span(t, 1.6, 2) * 100), alpha: 1 });
      const dim = ease(span(t, 1.8, 2.6));
      const q = cloud(c, t, { reveal: 1, dimOthers: dim, walkAt: span(t, 2.8, 4.8), walkOf: CLOUD.tr, topAt: span(t, 4.8, 5.6) });
      queryStar(c, q, 1);
      [['tr', P.sun], ['en', P.oasis], ['de', P.purple]].forEach(([l, col], i) => {
        const off = l !== 'tr' ? 1 - dim * 0.7 : 1;
        dot(c, 100 + i * 90, 470, 7, col, off);
        text(c, l, 114 + i * 90, 476, { size: 17, font: MONO, color: col, alpha: off });
      });
      text(c, 'an index answers the filter;', 80, 540, { size: 19, color: P.sand2, alpha: ease(span(t, 3, 3.6)) });
      text(c, 'the walk only counts what passes', 80, 570, { size: 19, color: P.sand2, alpha: ease(span(t, 3.2, 3.8)) });
    },
  },
  {
    key: 'race',
    title: '16 times faster than pgvector',
    sub: 'A million vectors, the same client and index settings, 99.1% recall.',
    d: 8,
    draw(c, t) {
      const lanes = [
        { name: 'fenec-pg', ms: 0.147, col: P.oasis },
        { name: 'PostgreSQL + pgvector', ms: 2.353, col: P.sun },
      ];
      const p = span(t, 0.4, 2.6);
      lanes.forEach((l, i) => {
        const y = 210 + i * 92;
        const k = ease(span(t, 0.2 + i * 0.15, 0.7 + i * 0.15));
        text(c, l.name, 80, y, { size: 22, weight: 600, color: i ? P.sand : P.oasis, alpha: k });
        box(c, 80, y + 16, 860, 30, { r: 15, fill: '#0C0819', alpha: k });
        // A bar's length is its time: the slow one keeps going.
        const shown = Math.min(l.ms, p * 2.353);
        box(c, 80, y + 16, Math.max(30, 860 * shown / 2.353), 30, { r: 15, fill: l.col, stroke: null, alpha: k * (i ? 0.75 : 1) });
        text(c, `${shown.toFixed(3)} ms`, 1200, y + 40, { size: 30, weight: 650, color: i ? P.sand : P.oasis, align: 'right', alpha: k });
      });
      text(c, 'nearest ten, median latency', 80, 400, { size: 16, color: P.dim, alpha: ease(span(t, 1, 1.5)) });
      const tiles = [
        ['Load and index', '47.2 s', '117.8 s'],
        ['Eight clients at once', '17 001 q/s', '2 097 q/s'],
        ['On disk', '612 MB', '1 432 MB'],
        ['Memory held', '721 MB', '1 132 MB'],
      ];
      tiles.forEach(([label, ours, theirs], i) => {
        const k = ease(span(t, 0.7 + i * 0.15, 1.2 + i * 0.15));
        const x = 80 + i * 285, y = 450;
        box(c, x, y + (1 - k) * 20, 265, 170, { fill: P.panel, alpha: k });
        text(c, label, x + 22, y + 40 + (1 - k) * 20, { size: 17, color: P.dim, alpha: k });
        text(c, ours, x + 22, y + 94 + (1 - k) * 20, { size: 36, weight: 650, color: P.oasis, alpha: k });
        text(c, `pgvector ${theirs}`, x + 22, y + 136 + (1 - k) * 20, { size: 17, color: P.sand2, alpha: k });
      });
    },
  },
  {
    key: 'postgres',
    title: 'It speaks PostgreSQL',
    sub: 'Your driver, pgvector\'s library and your tools connect unchanged.',
    d: 8,
    draw(c, t) {
      const cx = 640, cy = 420;
      const k = ease(span(t, 0.2, 0.9));
      box(c, cx - 130, cy - 110, 260, 220, { r: 20, fill: P.panel, stroke: P.sun, alpha: k, line: 2 });
      mark(c, cx, cy - 18, 1.6, k, t);
      text(c, 'fenec-pg', cx, cy + 82, { size: 22, font: MONO, color: P.hot, align: 'center', alpha: k });
      const left = ['psql', 'psycopg', 'asyncpg', 'pgx', 'node-postgres', 'tokio-postgres', 'JDBC'];
      const right = ['pgvector for Python', 'pgvector for Go', 'DBeaver', 'DuckDB', 'LangChain', 'LlamaIndex'];
      const draw = (names, side) => names.forEach((n, i) => {
        const a = ease(span(t, 0.8 + i * 0.18, 1.3 + i * 0.18));
        const y = 210 + i * (420 / (names.length - 1)) * 0.98;
        const x = side < 0 ? 90 : 960;
        c.font = `500 18px ${SANS}`;
        const w = c.measureText(n).width + 26;
        const bx = side < 0 ? x + 230 - w : x;
        chip(c, n, bx, y, { alpha: a, size: 18 });
        const ax = side < 0 ? x + 238 : x - 8, bx2 = side < 0 ? cx - 132 : cx + 132;
        line(c, ax, y, bx2, cy + (y - cy) * 0.25, P.rule, 1.5, a);
        // Messages on the wire, both ways.
        const ph = ((t * 0.6 + i * 0.37) % 1);
        if (t > 2) {
          const px = lerp(ax, bx2, ph), py = lerp(y, cy + (y - cy) * 0.25, ph);
          dot(c, px, py, 4, side < 0 ? P.oasis : P.sun, a * Math.sin(ph * Math.PI));
        }
      });
      draw(left, -1);
      draw(right, 1);
      const w = ease(span(t, 3.4, 4));
      chip(c, 'PostgreSQL wire protocol v3', cx - 150, 196, { color: P.oasis, alpha: w, size: 17, font: MONO, weight: 400 });
      const b = ease(span(t, 4.6, 5.2));
      chip(c, 'vector, halfvec, sparsevec as pgvector sends them', cx - 238, 650, { color: P.sand, alpha: b, size: 17 });
    },
  },
  {
    key: 'security',
    title: 'Every request is checked',
    sub: 'Login, token, rows, audit, and backups sealed at rest.',
    d: 9,
    draw(c, t) {
      const gates = [
        ['Login', 'SCRAM-SHA-256'],
        ['Token', 'JWT, your provider\'s keys'],
        ['Rows', 'owner = $jwt.sub'],
        ['Audit', 'a JSON line'],
        ['At rest', 'ChaCha20-Poly1305'],
      ];
      const gx = (i) => 300 + i * 200;
      gates.forEach(([g, s], i) => {
        const k = ease(span(t, 0.2 + i * 0.15, 0.7 + i * 0.15));
        line(c, gx(i), 190, gx(i), 470, P.rule, 3, k);
        text(c, g, gx(i), 520, { size: 21, weight: 600, color: P.star, align: 'center', alpha: k });
        text(c, s, gx(i), 548, { size: 15, color: P.dim, align: 'center', alpha: k });
      });
      // Alice's read: through every gate, her rows only.
      const p = span(t, 1.2, 4.4);
      const rx = lerp(110, gx(4) + 60, inOut(p));
      const lit = (i) => rx > gx(i);
      gates.forEach((_, i) => { if (lit(i) && i < 4) dot(c, gx(i), 260, 10, P.oasis, 0.9); });
      if (t > 1 && t < 6.2) chip(c, 'GET /notes  alice', rx - 80, 260, { color: P.oasis, size: 17, font: MONO, weight: 400, alpha: 1 - span(t, 5.6, 6.2) });
      // The rows the rule lets through.
      const rows = [['alice', 'buy dates', true], ['bob', 'water the cactus', false], ['alice', 'fix the tent', true]];
      const rk = ease(span(t, 2.8, 3.3));
      rows.forEach(([who, body, mine], i) => {
        const y = 600 + i * 34;
        const a = rk * (mine ? 1 : 1 - ease(span(t, 3.4, 3.9)) * 0.8);
        text(c, `${who}  ${body}`, gx(2) - 90, y, { size: 16, font: MONO, color: mine ? P.sand : P.dim, alpha: a });
        if (!mine) line(c, gx(2) - 92, y - 6, gx(2) + 110, y - 6, P.red, 2, rk * ease(span(t, 3.4, 3.9)));
      });
      // A write in someone else's name: stopped at the rows.
      const q = span(t, 5, 6.6);
      if (q > 0) {
        const x = lerp(110, gx(2) - 10, inOut(Math.min(q * 1.2, 1)));
        chip(c, 'write as bob', x - 70, 380, { color: P.red, stroke: P.red, size: 17, font: MONO, weight: 400 });
        if (q > 0.85) text(c, '403', gx(2) - 10, 440, { size: 30, weight: 700, color: P.red, alpha: ease(span(q, 0.85, 1)) });
      }
      // The audit log fills, and the backup is sealed.
      const lk = ease(span(t, 6.4, 7));
      const bx = gx(4), by = 380;
      box(c, bx - 60, by - 40, 120, 80, { r: 10, fill: P.panel2, stroke: P.sun, alpha: lk });
      c.save();
      c.globalAlpha *= lk;
      c.strokeStyle = P.hot;
      c.lineWidth = 4;
      c.beginPath();
      c.arc(bx, by - 10, 14, Math.PI, 0);
      c.stroke();
      c.restore();
      box(c, bx - 22, by - 10, 44, 32, { r: 6, fill: P.hot, stroke: null, alpha: lk });
      text(c, 'backup', bx, by + 64, { size: 15, font: MONO, color: P.sand2, align: 'center', alpha: lk });
      const lg = ease(span(t, 6.8, 7.4));
      chip(c, '{"event":"refused","status":403}', gx(3) - 150, 330, { size: 15, font: MONO, weight: 400, color: P.sand2, alpha: lg });
    },
  },
  {
    key: 'scale',
    title: 'Replicate and scale out',
    sub: 'Replicas follow in 0.2 ms. Tenants move and fail over on their own.',
    d: 9,
    draw(c, t) {
      // A primary feeding two replicas.
      const k = ease(span(t, 0.2, 0.8));
      const node = (x, y, label, col, a, sub) => {
        box(c, x - 90, y - 40, 180, 80, { fill: P.panel, stroke: col, alpha: a, line: 2 });
        text(c, label, x, y + 2, { size: 20, weight: 600, color: P.star, align: 'center', alpha: a });
        if (sub) text(c, sub, x, y + 26, { size: 14, font: MONO, color: P.dim, align: 'center', alpha: a });
      };
      const part1 = 1 - ease(span(t, 4.2, 4.8));
      node(260, 330, 'primary', P.sun, k * part1, 'writes');
      [[620, 230], [620, 430]].forEach(([x, y], i) => {
        const a = ease(span(t, 0.5 + i * 0.2, 1 + i * 0.2)) * part1;
        node(x, y, 'replica', P.oasis, a, 'reads');
        line(c, 350, 330, x - 92, y, P.rule, 2, a);
        for (let j = 0; j < 3; j++) {
          const ph = (t * 0.9 + j / 3) % 1;
          dot(c, lerp(350, x - 92, ph), lerp(330, y, ph), 5, P.sun, a * (t > 1 ? 1 : 0));
        }
      });
      chip(c, '0.20 ms behind, median', 780, 330, { color: P.oasis, size: 18, alpha: ease(span(t, 1.6, 2.2)) * part1 });
      chip(c, 'no acknowledged write lost in ten failovers', 780, 390, { color: P.sand, size: 17, alpha: ease(span(t, 2.2, 2.8)) * part1 });

      // Then tenants, a file each, spread across nodes behind a router.
      const b = ease(span(t, 4.6, 5.2));
      if (b <= 0) return;
      box(c, 540, 175, 200, 56, { fill: P.panel2, stroke: P.sun, alpha: b, line: 2 });
      text(c, 'router', 640, 211, { size: 20, weight: 600, color: P.star, align: 'center', alpha: b });
      const failed = ease(span(t, 6.4, 6.9));
      const nodes = [300, 640, 980];
      nodes.forEach((x, i) => {
        const down = i === 1 ? failed : 0;
        box(c, x - 140, 300, 280, 250, { fill: P.panel, stroke: down ? P.red : P.rule, alpha: b * (1 - down * 0.5), line: 2 });
        text(c, `node ${i + 1}`, x - 120, 334, { size: 18, weight: 600, color: down ? P.red : P.sand, alpha: b });
        line(c, 640, 231, x, 300, P.rule, 1.5, b * (1 - down * 0.7));
        if (down) {
          line(c, x - 30, 410, x + 30, 470, P.red, 5, down);
          line(c, x + 30, 410, x - 30, 470, P.red, 5, down);
        }
      });
      // Nine tenants; node 2's three move to the others when it fails.
      for (let n = 0; n < 9; n++) {
        const home = n % 3, slot = Math.floor(n / 3);
        let x = nodes[home] - 90 + slot * 90, y = 400;
        if (home === 1) {
          const to = n === 1 ? 0 : n === 4 ? 2 : 0;
          const mv = inOut(span(t, 6.9 + slot * 0.15, 7.6 + slot * 0.15));
          const tx = nodes[to] - 90 + slot * 90, ty = 480;
          x = lerp(x, tx, mv); y = lerp(y, ty, mv);
        }
        box(c, x - 34, y - 26, 68, 52, { r: 8, fill: P.panel2, stroke: P.sun, alpha: b });
        text(c, `t${n + 1}`, x, y + 7, { size: 17, font: MONO, color: P.hot, align: 'center', alpha: b });
      }
      chip(c, '20 tenants failed over in 60 ms', 470, 610, { color: P.oasis, size: 18, alpha: ease(span(t, 7.6, 8.1)) });
    },
  },
  {
    key: 'live',
    title: 'Live in the page',
    sub: 'A local replica reads offline and redraws as the server changes.',
    d: 8,
    draw(c, t) {
      const k = ease(span(t, 0.2, 0.8));
      // The server.
      box(c, 840, 200, 360, 380, { fill: P.panel, alpha: k });
      text(c, 'fenec-pg', 870, 240, { size: 20, font: MONO, color: P.hot, alpha: k });
      // The browser.
      box(c, 80, 180, 560, 420, { r: 16, fill: '#100B20', alpha: k });
      box(c, 80, 180, 560, 46, { r: 16, fill: P.panel2, alpha: k });
      [P.ember, P.sun, P.oasis].forEach((col, i) => dot(c, 104 + i * 18, 203, 5.5, col, k));
      text(c, 'db.live(tasks, draw)', 110, 268, { size: 18, font: MONO, color: P.oasis, alpha: k });
      const tasks = ['Pack water', 'Check the compass', 'Feed the camels'];
      const added = ease(span(t, 2.6, 3.1));
      const mine = ease(span(t, 4.6, 5));
      const synced = ease(span(t, 6, 6.4));
      const list = [...tasks];
      const rowsY = (i) => 300 + i * 56;
      list.forEach((s, i) => {
        box(c, 110, rowsY(i), 500, 44, { r: 8, fill: P.panel, alpha: k });
        text(c, s, 130, rowsY(i) + 28, { size: 18, color: P.sand, alpha: k });
      });
      // A write on the server, seen in the page.
      const w = span(t, 1.4, 2.6);
      if (w > 0) {
        chip(c, 'put tasks {title: "Find the oasis"}', 860, 300, { size: 15, font: MONO, weight: 400, color: P.sand, alpha: ease(span(t, 1.2, 1.6)) });
        if (w < 1) dot(c, lerp(860, 610, inOut(w)), lerp(330, rowsY(3) + 22, inOut(w)), 7, P.sun);
      }
      box(c, 110, rowsY(3), 500, 44, { r: 8, fill: P.panel, stroke: P.sun, alpha: added });
      text(c, 'Find the oasis', 130, rowsY(3) + 28, { size: 18, color: P.hot, alpha: added });
      // A write in the page: shown at once, then confirmed.
      box(c, 110, rowsY(4), 500, 44, { r: 8, fill: P.panel, stroke: synced ? P.oasis : P.rule, alpha: mine });
      text(c, 'Rest at noon', 130, rowsY(4) + 28, { size: 18, color: P.sand, alpha: mine });
      dot(c, 588, rowsY(4) + 22, 6, synced > 0.5 ? P.oasis : P.dim, mine);
      text(c, synced > 0.5 ? 'synced' : 'pending', 572, rowsY(4) + 28, { size: 14, color: synced > 0.5 ? P.oasis : P.dim, align: 'right', alpha: mine });
      const s = span(t, 5, 6);
      if (s > 0 && s < 1) dot(c, lerp(610, 860, inOut(s)), lerp(rowsY(4) + 22, 380, inOut(s)), 7, P.oasis);
      const w1 = chip(c, 'reads with no network', 80, 650, { size: 17, color: P.sand, alpha: ease(span(t, 6.4, 7)) });
      chip(c, 'writes shown before the server answers', 100 + w1, 650, { size: 17, color: P.sand, alpha: ease(span(t, 6.7, 7.3)) });
    },
  },
];

const STARS = (() => {
  const r = rng(3);
  return Array.from({ length: 160 }, () => [r() * W, r() * H, r() * 1.3 + 0.3, r() * 6.28]);
})();

/* The frame a section shows: the scenes' 1280 x 720 stage less the band a
   title took, so it sits under the section's own heading. */
export const FRAME = { w: W, h: 560, top: 150 };

export const SCENE = Object.fromEntries(SCENES.map((s) => [s.key, s]));

/* Draws scene `key` at `t` seconds of its own into a context of any size. */
export function render(c, key, t, width, height) {
  const scene = SCENE[key];
  const s = Math.min(width / FRAME.w, height / FRAME.h);
  c.setTransform(1, 0, 0, 1, 0, 0);
  c.clearRect(0, 0, width, height);
  c.setTransform(s, 0, 0, s, (width - FRAME.w * s) / 2, (height - FRAME.h * s) / 2);
  for (const [x, y, r, ph] of STARS) if (y < FRAME.h) dot(c, x, y, r, P.star, 0.14 + 0.12 * Math.sin(t * 1.3 + ph));
  c.translate(0, -FRAME.top);
  c.save();
  c.globalAlpha = ease(span(t, 0, 0.45)) * (1 - ease(span(t, scene.d - 0.4, scene.d)));
  scene.draw(c, t);
  c.restore();
}
