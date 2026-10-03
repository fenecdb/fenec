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

/* --------------------------------------------------------------- scenes */

const SCENES = [
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
    /* One write from a phone with no network to another device's screen:
       kept in the phone's file, sent once under its key when the network is
       back, and streamed out by the server. The last frame says all three
       at once, for a reader who asked for no motion. */
    key: 'flow',
    title: 'Local and server, one flow',
    sub: 'A write made offline, sent once, on the other screen.',
    d: 9,
    draw(c, t) {
      const a = ease(span(t, 0.1, 0.8));
      const LINE_Y = 405;
      const TASKS = ['Pack water', 'Check the compass', 'Feed the camels'];
      const NEW = 'Rest at noon';
      const online = t >= 3.6;
      const tap = ease(span(t, 1.0, 1.4));
      const queued = t >= 1.7 && t < 5.4 ? 1 : 0;
      const answered = ease(span(t, 5.2, 5.6));
      const arrived = ease(span(t, 6.8, 7.2));
      const under = (s, x, alpha, color = P.sand) => {
        c.font = `500 17px ${SANS}`;
        const w = c.measureText(s).width + 17 * 1.4;
        chip(c, s, x - w / 2, 668, { size: 17, color, alpha });
      };
      const rows = (x, y, w, alpha) => TASKS.forEach((s, i) => {
        box(c, x, y + i * 54, w, 44, { r: 8, fill: P.panel, alpha });
        text(c, s, x + 14, y + i * 54 + 28, { size: 17, color: P.sand, alpha });
      });

      // The phone, its list, and the file it keeps on the device.
      const px = 80, py = 186, pw = 220, ph = 440;
      box(c, px, py, pw, ph, { r: 30, fill: '#100B20', stroke: P.rule, alpha: a, line: 2 });
      box(c, px + pw / 2 - 34, py + 12, 68, 14, { r: 7, fill: P.panel2, stroke: null, alpha: a });
      text(c, '9:41', px + 22, py + 44, { size: 14, font: MONO, color: P.dim, alpha: a });
      dot(c, px + pw - 90, py + 39, 5, online ? P.oasis : P.ember, a);
      text(c, online ? 'online' : 'offline', px + pw - 20, py + 44, { size: 14, font: MONO, color: online ? P.oasis : P.ember, align: 'right', alpha: a });
      text(c, 'Tasks', px + 16, py + 80, { size: 20, weight: 600, color: P.star, alpha: a });
      rows(px + 16, py + 94, pw - 32, a);
      const ny = py + 94 + 3 * 54;
      box(c, px + 16, ny + (1 - tap) * 8, pw - 32, 44, { r: 8, fill: P.panel, stroke: answered > 0.5 ? P.oasis : P.sun, alpha: tap });
      text(c, NEW, px + 30, ny + 28 + (1 - tap) * 8, { size: 17, color: P.hot, alpha: tap });
      text(c, answered > 0.5 ? 'synced' : 'queued', px + pw - 28, ny + 27, { size: 13, font: MONO, color: answered > 0.5 ? P.oasis : P.dim, align: 'right', alpha: tap });
      // Into the file: the queue is a collection of the replica's own.
      const fy = py + ph - 96;
      box(c, px + 16, fy, pw - 32, 70, { r: 8, fill: '#0C0819', alpha: a });
      text(c, 'app.fenec', px + 30, fy + 26, { size: 14, font: MONO, color: P.dim, alpha: a });
      text(c, '_sync_queue', px + 30, fy + 52, { size: 15, font: MONO, color: P.sand, alpha: a });
      text(c, String(queued), px + pw - 30, fy + 52, { size: 15, font: MONO, color: queued ? P.sun : P.dim, align: 'right', alpha: a });
      const sink = span(t, 1.3, 1.7);
      if (sink > 0 && sink < 1) dot(c, px + pw / 2, lerp(ny + 44, fy + 8, inOut(sink)), 5, P.sun, 1 - sink * 0.4);
      under('written offline, kept in the file', px + pw / 2, ease(span(t, 1.8, 2.4)));

      // The way to the server: nothing while there is no network.
      const sx = 540, sw = 200, sy = 330, sh = 150;
      const off = a * (1 - ease(span(t, 3.6, 4)));
      line(c, px + pw, LINE_Y, sx, LINE_Y, P.rule, 2, a, online ? null : [5, 7]);
      text(c, 'no network', (px + pw + sx) / 2, LINE_Y - 16, { size: 15, font: MONO, color: P.ember, align: 'center', alpha: off * 0.9 });
      const req = ease(span(t, 4, 4.4));
      text(c, 'POST /query', (px + pw + sx) / 2, LINE_Y - 44, { size: 15, font: MONO, color: P.sand2, align: 'center', alpha: req });
      text(c, 'Idempotency-Key: 7f3a9c', (px + pw + sx) / 2, LINE_Y - 18, { size: 15, font: MONO, color: P.sun, align: 'center', alpha: req });
      const go = span(t, 4.2, 5);
      if (go > 0 && go < 1) dot(c, lerp(px + pw, sx, inOut(go)), LINE_Y, 7, P.sun);

      // The server: its ears catch the write as it lands.
      const flash = Math.sin(Math.PI * span(t, 5, 5.6));
      box(c, sx, sy, sw, sh, { r: 16, fill: P.panel, stroke: flash > 0 ? P.oasis : P.sun, alpha: a, line: 2 });
      mark(c, sx + sw / 2, sy + 62, 1.05, a, t - 4.5);
      text(c, 'fenec-server', sx + sw / 2, sy + sh - 18, { size: 18, font: MONO, color: P.hot, align: 'center', alpha: a });
      text(c, '200  Fenec-Seq: 4182', sx + sw / 2, sy + sh + 36, { size: 15, font: MONO, color: P.oasis, align: 'center', alpha: answered });
      const back = span(t, 5, 5.4);
      if (back > 0 && back < 1) dot(c, lerp(sx, px + pw, inOut(back)), LINE_Y + 14, 5, P.oasis);
      under('sent once, under its key', sx + sw / 2, ease(span(t, 5.6, 6.2)));

      // The other device, subscribed all along: the change reaches its list.
      const bx = 940, by = 200, bw = 260, bh = 360;
      line(c, sx + sw, LINE_Y, bx, LINE_Y, P.rule, 2, a);
      text(c, 'GET /tasks/changes', (sx + sw + bx) / 2, LINE_Y - 18, { size: 15, font: MONO, color: P.sand2, align: 'center', alpha: a });
      const out = span(t, 5.8, 6.8);
      if (out > 0 && out < 1) dot(c, lerp(sx + sw, bx, inOut(out)), LINE_Y, 7, P.oasis);
      box(c, bx, by, bw, bh, { r: 16, fill: '#100B20', alpha: a });
      box(c, bx, by, bw, 44, { r: 16, fill: P.panel2, alpha: a });
      [P.ember, P.sun, P.oasis].forEach((col, i) => dot(c, bx + 22 + i * 16, by + 22, 5, col, a));
      text(c, 'my-app.com', bx + 80, by + 28, { size: 14, font: MONO, color: P.dim, alpha: a });
      text(c, 'Tasks', bx + 20, by + 84, { size: 20, weight: 600, color: P.star, alpha: a });
      rows(bx + 20, by + 100, bw - 40, a);
      const ry = by + 100 + 3 * 54;
      box(c, bx + 20, ry + (1 - arrived) * 8, bw - 40, 44, { r: 8, fill: P.panel, stroke: P.sun, alpha: arrived });
      text(c, NEW, bx + 34, ry + 28 + (1 - arrived) * 8, { size: 17, color: P.hot, alpha: arrived });
      under('on the other screen', bx + bw / 2, ease(span(t, 7.2, 7.8)));
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
