/* The page's moving pictures: each section shows its feature working.

   A scene is a function of its own time alone, drawn onto the canvas in its
   section, so a section plays its scene while it is in view and stops when
   it is not. A story (a write reaching the screens, a tenant moved) plays
   once, holds its last frame and starts again; a stream (writers, traffic,
   security) opens and then runs on, each event drawn from its own number,
   so it never starts again and never jumps. Every number on screen is one the docs measure; the words and
   the measurements stay in the section's text, and the scene shows them
   happening. What moves is the mark's own light: the teal that runs through
   the logo, with its glow, so a write, a request and a search all travel the
   same way.

   Each scene is drawn on one of two stages: 1280 wide for a screen, 480 wide
   for a phone, where the same story is laid out narrower and taller so its
   words stay readable (a wide stage scaled to a phone turned them to specks).
   One frame says everything the scene does at once, for a reader who asked
   for no motion: a story's last, a stream's `still`. */

import { POINTS, EDGES, FLOW, RUN, COLORS } from './fennec.js';

const W = 1280, NW = 480;
const P = {
  night: '#070A18', night2: '#1B1233', panel: '#150F26', panel2: '#1D1430', rule: '#3A2A3F',
  sun: '#F4A93C', hot: '#FFCE73', ember: '#FF7A3D', oasis: '#4FE0C4', star: '#FFF4DC',
  sand: '#F0E2C4', sand2: '#CDB894', dim: '#96856A', purple: '#C79BF2', red: '#FF6B5B',
  deep: '#100B20',
};
const GLOW = 'rgba(79,224,196,.95)';
const SANS = '"Bricolage Grotesque", system-ui, sans-serif';
const MONO = '"IBM Plex Mono", ui-monospace, monospace';

const clamp = (x, a = 0, b = 1) => Math.min(b, Math.max(a, x));
const span = (t, a, b) => clamp((t - a) / (b - a));
const ease = (x) => 1 - Math.pow(1 - clamp(x), 3);
const inOut = (x) => { x = clamp(x); return x < 0.5 ? 4 * x * x * x : 1 - Math.pow(-2 * x + 2, 3) / 2; };
const lerp = (a, b, k) => a + (b - a) * k;
// A flash that rises at `a` and settles to `rest` over `len` seconds.
const flash = (t, a, len = 0.9, rest = 0) => t < a ? 0 : lerp(1, rest, ease((t - a) / len));

// A number in [0, 1) for event `i` of stream `s`: the same on every frame,
// so a stream drawn from it is a function of its time alone.
const hash = (i, s = 0) => { const x = Math.sin(i * 127.1 + s * 311.7) * 43758.5453; return x - Math.floor(x); };

function rng(seed) { return () => ((seed = (seed * 16807) % 2147483647) / 2147483647); }

/* --------------------------------------------------------------- drawing */

function text(c, s, x, y, { size = 22, weight = 400, color = P.sand, font = SANS, align = 'left', alpha = 1, base = 'alphabetic' } = {}) {
  if (alpha <= 0) return;
  c.save();
  c.globalAlpha *= alpha;
  c.font = `${weight} ${size}px ${font}`;
  c.fillStyle = color;
  c.textAlign = align;
  c.textBaseline = base;
  c.fillText(s, x, y);
  c.restore();
}
const mono = (c, s, x, y, o = {}) => text(c, s, x, y, { size: 14, font: MONO, color: P.dim, ...o });

// `glow` lights the outline with the mark's light, 0 to 1.
function box(c, x, y, w, h, { r = 12, fill = P.panel, stroke = P.rule, alpha = 1, line = 1.5, glow = 0, dash = null } = {}) {
  if (alpha <= 0) return;
  c.save();
  c.globalAlpha *= alpha;
  c.beginPath();
  c.roundRect(x, y, w, h, r);
  if (fill) { c.fillStyle = fill; c.fill(); }
  if (stroke) {
    if (glow > 0) { c.shadowColor = GLOW; c.shadowBlur = 20 * glow; }
    if (dash) c.setLineDash(dash);
    c.strokeStyle = stroke; c.lineWidth = line; c.stroke();
  }
  c.restore();
}

function dot(c, x, y, r, color, alpha = 1) {
  if (alpha <= 0) return;
  c.save();
  c.globalAlpha *= alpha;
  c.fillStyle = color;
  c.beginPath();
  c.arc(x, y, r, 0, Math.PI * 2);
  c.fill();
  c.restore();
}

function wire(c, pts, color = P.rule, width = 1.5, alpha = 1, dash = null) {
  if (alpha <= 0) return;
  c.save();
  c.globalAlpha *= alpha;
  c.strokeStyle = color;
  c.lineWidth = width;
  c.lineJoin = 'round';
  if (dash) c.setLineDash(dash);
  c.beginPath();
  pts.forEach((p, i) => (i ? c.lineTo(...p) : c.moveTo(...p)));
  c.stroke();
  c.restore();
}
const line = (c, x1, y1, x2, y2, ...o) => wire(c, [[x1, y1], [x2, y2]], ...o);

function chip(c, s, x, y, { color = P.sand, fill = P.panel2, size = 17, alpha = 1, stroke = P.rule, font = SANS, weight = 500, center = false } = {}) {
  c.font = `${weight} ${size}px ${font}`;
  const w = c.measureText(s).width + size * 1.4;
  const h = size * 2;
  if (center) x -= w / 2;
  box(c, x, y - h / 2, w, h, { r: h / 2, fill, stroke, alpha });
  text(c, s, x + size * 0.7, y + size * 0.36, { size, weight, color, alpha, font });
  return w;
}

/* The light: a point of the mark's teal with its glow, and the same light
   running along a wire, a short bright stretch that crosses it and trails
   off its end as `run` goes from 0 to 1. */
function glow(c, x, y, r = 5, alpha = 1) {
  if (alpha <= 0) return;
  c.save();
  c.globalAlpha *= alpha;
  const g = c.createRadialGradient(x, y, 0, x, y, r * 3.4);
  g.addColorStop(0, 'rgba(79,224,196,.42)');
  g.addColorStop(1, 'rgba(79,224,196,0)');
  c.fillStyle = g;
  c.beginPath(); c.arc(x, y, r * 3.4, 0, 7); c.fill();
  c.shadowColor = GLOW; c.shadowBlur = r * 1.6;
  c.fillStyle = P.oasis;
  c.beginPath(); c.arc(x, y, r, 0, 7); c.fill();
  c.restore();
}

function along(pts, k) {
  const len = [];
  let total = 0;
  for (let i = 1; i < pts.length; i++) total += len[i - 1] = Math.hypot(pts[i][0] - pts[i - 1][0], pts[i][1] - pts[i - 1][1]);
  let d = clamp(k) * total;
  for (let i = 0; i < len.length; i++) {
    if (d <= len[i] || i === len.length - 1) {
      const f = len[i] ? Math.min(1, d / len[i]) : 0;
      return [lerp(pts[i][0], pts[i + 1][0], f), lerp(pts[i][1], pts[i + 1][1], f)];
    }
    d -= len[i];
  }
  return pts[0];
}

function streak(c, pts, run, { alpha = 1, tail = 0.22, w = 3 } = {}) {
  const k = run * (1 + tail);   // `run` 0 to 1: the head across, then the tail off the end
  if (k <= 0 || k >= 1 + tail || alpha <= 0) return;
  c.save();
  c.globalAlpha *= alpha;
  c.strokeStyle = P.oasis; c.lineWidth = w; c.lineCap = 'round'; c.lineJoin = 'round';
  c.shadowColor = GLOW; c.shadowBlur = 8;
  const a = Math.max(0, k - tail), b = Math.min(1, k);
  c.beginPath();
  for (let i = 0; i <= 12; i++) { const p = along(pts, lerp(a, b, i / 12)); i ? c.lineTo(...p) : c.moveTo(...p); }
  c.stroke();
  c.restore();
  if (k < 1) glow(c, ...along(pts, k), w + 1.5, alpha);
}

// The mark, from the table the logo is drawn from, its light running through
// it once as the page's own mark's does (`t` in seconds).
function mark(c, x, y, s, alpha = 1, t = 0) {
  if (alpha <= 0) return;
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
  c.shadowColor = GLOW; c.shadowBlur = 4;
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

// A file, as the hero draws one: a page with its corner folded.
function file(c, x, y, s, { stroke = P.sun, alpha = 1, glow: g = 0, dash = null } = {}) {
  if (alpha <= 0) return;
  c.save();
  c.globalAlpha *= alpha;
  c.translate(x, y);
  c.scale(s, s);
  c.lineJoin = 'round';
  c.beginPath();
  c.moveTo(-12, -16); c.lineTo(3, -16); c.lineTo(12, -7); c.lineTo(12, 16); c.lineTo(-12, 16); c.closePath();
  c.fillStyle = P.panel2; c.fill();
  c.moveTo(3, -16); c.lineTo(3, -7); c.lineTo(12, -7);
  if (g > 0) { c.shadowColor = GLOW; c.shadowBlur = 14 * g; }
  if (dash) c.setLineDash(dash);
  c.strokeStyle = stroke; c.lineWidth = 1.8 / s * 1.2; c.stroke();
  c.restore();
}

// A client: a small screen on a stand.
function client(c, x, y, { stroke = P.rule, alpha = 1, glow: g = 0 } = {}) {
  box(c, x - 14, y - 11, 28, 20, { r: 4, fill: P.panel, stroke, alpha, glow: g });
  line(c, x - 6, y + 13, x + 6, y + 13, stroke, 1.5, alpha);
}

function disk(c, x, y, { alpha = 1, glow: g = 0 } = {}) {
  if (alpha <= 0) return;
  c.save();
  c.globalAlpha *= alpha;
  c.strokeStyle = g > 0.05 ? P.oasis : P.sand2;
  c.lineWidth = 1.8;
  if (g > 0) { c.shadowColor = GLOW; c.shadowBlur = 18 * g; }
  c.fillStyle = P.panel;
  c.beginPath();
  c.ellipse(x, y + 14, 34, 9, 0, 0, Math.PI);
  c.lineTo(x - 34, y - 14);
  c.ellipse(x, y - 14, 34, 9, 0, Math.PI, 0, true);
  c.closePath();
  c.fill(); c.stroke();
  c.beginPath(); c.ellipse(x, y - 14, 34, 9, 0, 0, Math.PI * 2); c.stroke();
  c.restore();
}

// A padlock, closed: a sealed file's.
function lock(c, x, y, color, alpha = 1) {
  if (alpha <= 0) return;
  box(c, x - 6, y - 2, 12, 9, { r: 2, fill: P.panel, stroke: color, alpha, line: 1.6 });
  c.save();
  c.globalAlpha *= alpha;
  c.strokeStyle = color; c.lineWidth = 1.6;
  c.beginPath(); c.arc(x, y - 3, 3.6, Math.PI, 0); c.stroke();
  c.restore();
}

// A bucket: object storage, where a sync tool copies the archive.
function bucket(c, x, y, { alpha = 1, glow: g = 0 } = {}) {
  if (alpha <= 0) return;
  c.save();
  c.globalAlpha *= alpha;
  c.strokeStyle = g > 0.05 ? P.oasis : P.sand2;
  c.lineWidth = 1.8; c.lineJoin = 'round';
  if (g > 0) { c.shadowColor = GLOW; c.shadowBlur = 18 * g; }
  c.fillStyle = P.panel;
  c.beginPath();
  c.moveTo(x - 22, y - 16); c.lineTo(x - 16, y + 20); c.lineTo(x + 16, y + 20); c.lineTo(x + 22, y - 16);
  c.ellipse(x, y - 16, 22, 6, 0, 0, Math.PI, true);
  c.fill(); c.stroke();
  c.beginPath(); c.ellipse(x, y - 16, 22, 6, 0, 0, Math.PI * 2); c.stroke();
  c.restore();
}

// A text whose words light as `lit` rises when they are among `terms`.
function words(c, s, x, y, { size = 16, lit = 0, terms, color = P.sand, alpha = 1 } = {}) {
  c.font = `400 ${size}px ${SANS}`;
  const space = c.measureText(' ').width;
  for (const w of s.split(' ')) {
    const hit = terms.has(w.toLowerCase());
    text(c, w, x, y, { size, color: hit && lit > 0.5 ? P.oasis : color, alpha });
    if (hit && lit > 0) {
      c.font = `400 ${size}px ${SANS}`;
      const ww = c.measureText(w).width;
      c.save(); c.shadowColor = GLOW; c.shadowBlur = 6;
      line(c, x, y + 4, x + ww, y + 4, P.oasis, 2, alpha * lit);
      c.restore();
    }
    c.font = `400 ${size}px ${SANS}`;
    x += c.measureText(w).width + space;
  }
}

/* --------------------------------------------------------------- scenes */

const SCENES = [
  {
    /* One write in the app's database, and every screen whose live query
       reads what it wrote is drawn again: the light runs out to each of them
       and the new row lights in place. A screen over another collection is
       not run at all. All four are screens of one app, over one database: a
       write reaches another device through sync, which is the next section. */
    key: 'state',
    d: 8,
    h: 560, nh: 720,
    draw(c, t, n) {
      const a = ease(span(t, 0.1, 0.8));
      const L = n ? {
        fx: 140, fy: 108, fw: 200, fh: 62, mx: 240, my: 48, ms: 0.9, chipY: 214,
        cards: [[20, 290], [250, 290], [20, 500], [250, 500]], cw: 210, ch: 180, rh: 28, rg: 6,
      } : {
        fx: 90, fy: 250, fw: 200, fh: 64, mx: 190, my: 165, ms: 1.35, chipY: 372,
        cards: [[400, 180], [615, 180], [830, 180], [1045, 180]], cw: 195, ch: 260, rh: 36, rg: 10,
      };
      const fcx = L.fx + L.fw / 2;
      // Each card's wire from the file, along a bus so none crosses a card.
      const paths = n ? [
        [[340, 139], [466, 139], [466, 262], [125, 262], [125, 290]],
        [[340, 139], [466, 139], [466, 262], [355, 262], [355, 290]],
        [[340, 139], [466, 139], [466, 262], [8, 262], [8, 590], [20, 590]],
        [[340, 139], [474, 139], [474, 590], [460, 590]],
      ] : L.cards.map(([x, y]) => [[L.fx + L.fw, 282], [345, 282], [345, 130], [x + L.cw / 2, 130], [x + L.cw / 2, y]]);

      // The database: the mark over the app's file.
      const landed = 1.7;
      mark(c, L.mx, L.my, L.ms, a, t - (landed - 0.6));
      box(c, L.fx, L.fy, L.fw, L.fh, { r: 10, fill: P.deep, stroke: P.sun, alpha: a, line: 1.8, glow: flash(t, landed, 1, 0) });
      mono(c, 'app.fenec', fcx, L.fy + L.fh / 2 + 6, { size: 16, color: P.hot, align: 'center', alpha: a });

      // The write: a new task, into the file.
      const w = ease(span(t, 0.7, 1.1));
      chip(c, 'a write: “Rest at noon”', fcx, L.chipY, { size: 16, center: true, alpha: w, color: P.hot });
      streak(c, [[fcx, L.chipY - 17], [fcx, L.fy + L.fh]], span(t, 1.25, landed), { tail: 0.4 });

      // The screens. Three read the tasks; the fourth reads the notes.
      const cards = [
        { title: 'Tasks', coll: 'todos', rows: ['Pack water', 'Feed the camels'] },
        { title: 'Open', coll: 'todos', count: true },
        { title: 'Today', coll: 'todos', rows: ['Pack water'] },
        { title: 'Notes', coll: 'notes', rows: ['Oasis at dawn', 'Map of the dunes'], other: true },
      ];
      cards.forEach((cd, i) => {
        const [x, y] = L.cards[i];
        const go = landed + 0.5 + i * 0.12;
        const arrive = go + 0.7;
        const lit = ease(span(t, arrive, arrive + 0.35));
        const ca = a * (cd.other ? 0.55 : 1);
        wire(c, paths[i], P.rule, 1.5, a * (cd.other ? 0.6 : 1), cd.other ? [4, 6] : null);
        if (!cd.other) streak(c, paths[i], span(t, go, arrive), { tail: 0.18 });
        box(c, x, y, L.cw, L.ch, {
          r: 14, fill: P.deep, alpha: ca, line: 1.6,
          stroke: !cd.other && lit > 0 ? P.oasis : P.rule, glow: cd.other ? 0 : flash(t, arrive, 1.2, 0.3) * lit,
        });
        text(c, cd.title, x + 16, y + 30, { size: n ? 17 : 18, weight: 600, color: cd.other ? P.sand2 : P.star, alpha: ca });
        mono(c, cd.coll, x + L.cw - 14, y + 29, { size: 13, align: 'right', alpha: ca });
        const rx = x + 12, rw = L.cw - 24;
        if (cd.count) {
          const k = lit;
          text(c, '2', x + L.cw / 2, y + L.ch / 2 + 22 - k * 16, { size: n ? 54 : 64, weight: 650, color: P.star, align: 'center', alpha: ca * (1 - k) });
          text(c, '3', x + L.cw / 2, y + L.ch / 2 + 38 - k * 16, { size: n ? 54 : 64, weight: 650, color: P.oasis, align: 'center', alpha: ca * k });
          mono(c, 'open tasks', x + L.cw / 2, y + L.ch / 2 + (n ? 48 : 58), { size: 13, align: 'center', alpha: ca });
        } else {
          cd.rows.forEach((s, j) => {
            const ry = y + 44 + j * (L.rh + L.rg);
            box(c, rx, ry, rw, L.rh, { r: 7, fill: P.panel, alpha: ca });
            text(c, s, rx + 12, ry + L.rh / 2 + 5, { size: n ? 14 : 15.5, color: P.sand, alpha: ca });
          });
          if (!cd.other) {
            const ry = y + 44 + cd.rows.length * (L.rh + L.rg) + (1 - lit) * 8;
            box(c, rx, ry, rw, L.rh, { r: 7, fill: P.panel, stroke: P.oasis, alpha: ca * lit, glow: flash(t, arrive, 1.4, 0.4) });
            text(c, 'Rest at noon', rx + 12, ry + L.rh / 2 + 5, { size: n ? 14 : 15.5, color: P.oasis, alpha: ca * lit });
          }
        }
        // Whether its query ran again.
        const said = ease(span(t, arrive + 0.5, arrive + 1));
        mono(c, cd.other ? 'not run' : 'ran again', x + L.cw - 14, y + L.ch - 14, { size: 13, align: 'right', color: cd.other ? P.dim : P.oasis, alpha: ca * said });
      });

      const end = ease(span(t, 4.4, 5));
      if (n) mono(c, 'React · SwiftUI · Compose · Flutter', 240, 702, { size: 14, align: 'center', color: P.sand2, alpha: end });
      else {
        mono(c, 'each screen a live query', fcx, 446, { size: 15, align: 'center', color: P.sand2, alpha: end });
        mono(c, 'React · SwiftUI · Compose · Flutter', fcx, 472, { size: 14, align: 'center', alpha: end });
      }
    },
  },
  {
    /* One write from a phone with no network to another device's screen:
       kept in the phone's file, sent once under its key when the network is
       back, and streamed out by the server. */
    key: 'flow',
    d: 9,
    h: 560, nh: 820,
    draw(c, t, n) {
      const a = ease(span(t, 0.1, 0.8));
      const TASKS = ['Pack water', 'Check the compass', 'Feed the camels'];
      const NEW = 'Rest at noon';
      // The wide stage keeps the layout it was drawn with, 150 px lower.
      if (!n) c.translate(0, -150);
      const L = n ? {
        px: 20, py: 20, pw: 200, ph: 420,
        sx: 270, sy: 50, sw: 190, sh: 150,
        bx: 250, by: 350, bw: 210, bh: 330,
        w1: [[220, 125], [270, 125]],
        w2: [[460, 125], [472, 125], [472, 420], [460, 420]],
        net: [365, 228], req: [[365, 228], [365, 250]], get: [355, 336], ans: [365, 276],
        under: [[240, 718], [240, 758], [240, 798]],
      } : {
        px: 80, py: 186, pw: 220, ph: 440,
        sx: 540, sy: 330, sw: 200, sh: 150,
        bx: 940, by: 200, bw: 260, bh: 360,
        w1: [[300, 405], [540, 405]],
        w2: [[740, 405], [940, 405]],
        net: [420, 389], req: [[420, 361], [420, 387]], get: [840, 387], ans: [640, 516],
        under: [[190, 668], [640, 668], [1070, 668]],
      };
      const { px, py, pw, ph, sx, sy, sw, sh, bx, by, bw, bh } = L;
      const ms = n ? 0.95 : 1.05;
      const online = t >= 3.6;
      const tap = ease(span(t, 1.0, 1.4));
      const queued = t >= 1.7 && t < 5.4 ? 1 : 0;
      const answered = ease(span(t, 5.2, 5.6));
      const arrived = ease(span(t, 6.8, 7.2));
      const under = (s, [x, y], alpha) => chip(c, s, x, y, { size: 17, alpha, center: true });
      const rows = (x, y, w, alpha) => TASKS.forEach((s, i) => {
        box(c, x, y + i * 54, w, 44, { r: 8, fill: P.panel, alpha });
        text(c, s, x + 14, y + i * 54 + 28, { size: n ? 15 : 17, color: P.sand, alpha });
      });

      // The phone, its list, and the file it keeps on the device.
      box(c, px, py, pw, ph, { r: 30, fill: P.deep, stroke: P.rule, alpha: a, line: 2 });
      box(c, px + pw / 2 - 34, py + 12, 68, 14, { r: 7, fill: P.panel2, stroke: null, alpha: a });
      mono(c, '9:41', px + 22, py + 44, { alpha: a });
      dot(c, px + pw - 90, py + 39, 5, online ? P.oasis : P.ember, a);
      mono(c, online ? 'online' : 'offline', px + pw - 20, py + 44, { color: online ? P.oasis : P.ember, align: 'right', alpha: a });
      text(c, 'Tasks', px + 16, py + 80, { size: 20, weight: 600, color: P.star, alpha: a });
      rows(px + 16, py + 94, pw - 32, a);
      const ny = py + 94 + 3 * 54;
      box(c, px + 16, ny + (1 - tap) * 8, pw - 32, 44, { r: 8, fill: P.panel, stroke: answered > 0.5 ? P.oasis : P.sun, alpha: tap });
      text(c, NEW, px + 30, ny + 28 + (1 - tap) * 8, { size: n ? 15 : 17, color: P.hot, alpha: tap });
      mono(c, answered > 0.5 ? 'synced' : 'queued', px + pw - (n ? 24 : 28), ny + 27, { size: n ? 11.5 : 13, color: answered > 0.5 ? P.oasis : P.dim, align: 'right', alpha: tap });
      // Into the file: the queue is a collection of the replica's own.
      const fy = py + ph - 96;
      box(c, px + 16, fy, pw - 32, 70, { r: 8, fill: '#0C0819', alpha: a, glow: flash(t, 1.7, 0.8) });
      mono(c, 'app.fenec', px + 30, fy + 26, { alpha: a });
      mono(c, '_sync_queue', px + 30, fy + 52, { size: 15, color: P.sand, alpha: a });
      mono(c, String(queued), px + pw - 30, fy + 52, { size: 15, color: queued ? P.oasis : P.dim, align: 'right', alpha: a });
      streak(c, [[px + pw / 2, ny + 44], [px + pw / 2, fy]], span(t, 1.3, 1.7), { tail: 0.5 });
      under('written offline, kept in the file', L.under[0], ease(span(t, 1.8, 2.4)));

      // The way to the server: nothing while there is no network.
      const off = a * (1 - ease(span(t, 3.6, 4)));
      wire(c, L.w1, P.rule, 2, a, online ? null : [5, 7]);
      mono(c, 'no network', L.net[0], L.net[1], { size: 15, color: P.ember, align: 'center', alpha: off * 0.9 });
      const req = ease(span(t, 4, 4.4)) * (n ? 1 - ease(span(t, 5.3, 5.6)) : 1);
      mono(c, 'POST /query', L.req[0][0], L.req[0][1], { size: 15, color: P.sand2, align: 'center', alpha: req });
      mono(c, 'Idempotency-Key: 7f3a9c', L.req[1][0], L.req[1][1], { size: 15, color: P.sun, align: 'center', alpha: req });
      streak(c, L.w1, span(t, 4.2, 5));

      // The server: its ears catch the write as it lands.
      box(c, sx, sy, sw, sh, { r: 16, fill: P.panel, stroke: P.sun, alpha: a, line: 2, glow: flash(t, 5, 0.9) });
      mark(c, sx + sw / 2, sy + 62, ms, a, t - 4.4);
      mono(c, 'fenec-server', sx + sw / 2, sy + sh - 18, { size: n ? 16 : 18, color: P.hot, align: 'center', alpha: a });
      mono(c, '200  Fenec-Seq: 4182', L.ans[0], L.ans[1], { size: 15, color: P.oasis, align: 'center', alpha: answered });
      streak(c, [...L.w1].reverse().map(([x, y]) => [x, y + 14]), span(t, 5, 5.4), { w: 2 });
      under('sent once, under its key', L.under[1], ease(span(t, 5.6, 6.2)));

      // The other device, subscribed all along: the change reaches its list.
      wire(c, L.w2, P.rule, 2, a);
      mono(c, 'GET /tasks/changes', L.get[0], L.get[1], { size: 15, color: P.sand2, align: 'center', alpha: a });
      streak(c, L.w2, span(t, 5.8, 6.8));
      box(c, bx, by, bw, bh, { r: 16, fill: P.deep, alpha: a, glow: flash(t, 6.8, 1, 0) });
      box(c, bx, by, bw, 44, { r: 16, fill: P.panel2, alpha: a });
      [P.ember, P.sun, P.oasis].forEach((col, i) => dot(c, bx + 22 + i * 16, by + 22, 5, col, a));
      mono(c, 'my-app.com', bx + 80, by + 28, { alpha: a });
      text(c, 'Tasks', bx + 20, by + 84, { size: 20, weight: 600, color: P.star, alpha: a });
      rows(bx + 20, by + 100, bw - 40, a);
      const ry = by + 100 + 3 * 54;
      box(c, bx + 20, ry + (1 - arrived) * 8, bw - 40, 44, { r: 8, fill: P.panel, stroke: P.oasis, alpha: arrived, glow: flash(t, 7, 1.2, 0.35) * arrived });
      text(c, NEW, bx + 34, ry + 28 + (1 - arrived) * 8, { size: n ? 15 : 17, color: P.oasis, alpha: arrived });
      under('on the other screen', L.under[2], ease(span(t, 7.2, 7.8)));
    },
  },
  {
    /* Sixteen writers and one file, as a stream that does not end: a write
       comes from whichever writer has one, takes the lock for a moment and
       goes in, one at a time; each fsync runs after the lock is let go, so
       the writes that landed while the disk was busy are covered by the next
       one together. The readers beside them never stop. Every event is
       drawn from its own number, so the scene never starts again. */
    key: 'writers',
    stream: true, still: 9.05,
    h: 560, nh: 600,
    draw(c, t, n) {
      const a = ease(span(t, 0.1, 0.8));
      // Write i lands near T0 + i * DT, in bursts and lulls, and always after
      // write i - 1 (the wobble's slope stays under one): one at a time.
      const T0 = 1.0, DT = 0.2;
      const lands = (i) => T0 + DT * (i + 1.4 * Math.sin(i * 0.3) + 0.35 * (hash(i, 1) - 0.5));
      const by = (i) => Math.floor(hash(i, 2) * 16);
      // Fsync k near S0 + k * SD; each covers what landed before it.
      const S0 = 1.7, SD = 1.2;
      const syncAt = (k) => S0 + k * SD + (hash(k, 3) - 0.5) * 0.5;
      const synced = (i) => {
        let k = Math.max(0, Math.floor((lands(i) - S0) / SD) - 1);
        while (syncAt(k) <= lands(i) + 0.05) k++;
        return syncAt(k);
      };
      const recent = [];
      for (let i = Math.max(0, Math.floor((t - 3.2 - T0) / DT - 2)); i <= (t + 0.8 - T0) / DT + 2; i++) {
        if (lands(i) >= t - 3.2 && lands(i) <= t + 0.8) recent.push(i);
      }
      const syncs = [];
      for (let k = Math.max(0, Math.floor((t - 1.6 - S0) / SD) - 1); syncAt(k) <= t; k++) if (syncAt(k) > t - 1.6) syncs.push(syncAt(k));

      const L = n ? {
        writer: (k) => [44 + (k % 8) * 56, 58 + Math.floor(k / 8) * 50], wl: [240, 24],
        gate: [240, 172], fx: 34, fy: 220, fw: 412, fh: 70, cx: 48,
        disk: [116, 410], readers: [270, 330, 390, 450].map((x) => [x, 470]),
        from: (r) => [r[0], 290], to: (r) => [r[0], 455], rl: [360, 520], gl: [300, 177], chip: 568,
      } : {
        writer: (k) => [70 + (k % 4) * 56, 172 + Math.floor(k / 4) * 56], wl: [154, 136],
        gate: [390, 255], fx: 470, fy: 220, fw: 400, fh: 70, cx: 484,
        disk: [670, 410], readers: [176, 236, 296, 356].map((y) => [1120, y]),
        from: () => [870, 255], to: (r) => [r[0] - 18, r[1]], rl: [1120, 136], gl: [390, 208], chip: 500,
      };
      // The file's tail: the newest write at the right, the older ones
      // sliding out to the left as each lands.
      const cap = Math.floor((L.fw - 28) / 13);
      let last = -1;
      for (const i of recent) if (lands(i) <= t) last = i;
      const head = last < 0 ? 0 : last + ease((t - lands(last)) / 0.25);
      const scroll = Math.max(0, head - cap);
      const cell = (i) => L.cx + (i - scroll) * 13;
      const fmid = L.fy + L.fh / 2;
      const inside = (x) => clamp(x, L.fx + 8, L.fx + L.fw - 8);

      // The file, and the disk under it.
      mono(c, 'app.fenec', L.fx, L.fy - 12, { size: 15, color: P.hot, alpha: a });
      box(c, L.fx, L.fy, L.fw, L.fh, { r: 10, fill: P.deep, stroke: P.sun, alpha: a, line: 1.8 });
      disk(c, ...L.disk, { alpha: a, glow: Math.max(0, ...syncs.map((s) => flash(t, s, 0.7))) });
      mono(c, 'disk', L.disk[0], L.disk[1] + 46, { align: 'center', alpha: a });

      // The lock: one write through at a time.
      const [gx, gy] = L.gate;
      if (n) { line(c, gx - 40, gy, gx - 9, gy, P.sand2, 3, a); line(c, gx + 9, gy, gx + 40, gy, P.sand2, 3, a); }
      else { line(c, gx, gy - 40, gx, gy - 9, P.sand2, 3, a); line(c, gx, gy + 9, gx, gy + 40, P.sand2, 3, a); }
      mono(c, 'one at a time', L.gl[0], L.gl[1], { align: n ? 'left' : 'center', color: P.sand2, alpha: a });

      // The writers: a dot while a write waits, lit once it is on disk.
      mono(c, '16 writers', L.wl[0], L.wl[1], { align: 'center', color: P.sand2, alpha: a });
      const lit = new Array(16).fill(0), waiting = new Array(16).fill(false);
      for (const i of recent) {
        const k = by(i), s = synced(i);
        if (t >= lands(i) - 0.7 && t < s) waiting[k] = true;
        if (t >= s) lit[k] = Math.max(lit[k], flash(t, s, 0.8) * 0.8);
      }
      for (let k = 0; k < 16; k++) {
        const [x, y] = L.writer(k);
        client(c, x, y, { stroke: lit[k] > 0.05 ? P.oasis : P.sand2, alpha: a, glow: lit[k] });
        if (waiting[k]) dot(c, x + 12, y - 10, 3, P.sun, a);
      }

      // The writes: to the lock, through it, and into the file.
      for (const i of recent) {
        const [wx, wy] = L.writer(by(i));
        const T = lands(i);
        const x = inside(cell(i));
        const into = n ? [[gx, gy - 12], [gx, gy + 12], [x + 5, L.fy]] : [[gx - 12, gy], [gx + 12, gy], [x, fmid]];
        const k1 = span(t, T - 0.7, T - 0.12);
        if (k1 > 0 && k1 < 1) glow(c, ...along([[wx, wy], into[0]], inOut(k1)), 4);
        streak(c, into, span(t, T - 0.12, T), { tail: 0.3, w: 2.5 });
      }
      c.save();
      c.beginPath(); c.rect(L.fx + 8, L.fy, L.fw - 16, L.fh); c.clip();
      for (let i = Math.max(0, Math.floor(scroll) - 1); i <= last; i++) {
        const s = synced(i), on = ease(span(t, s, s + 0.25));
        box(c, cell(i), L.fy + 12, 10, L.fh - 24, { r: 2, fill: on > 0 ? `rgba(79,224,196,${0.85 * on})` : null, stroke: on > 0.5 ? P.oasis : P.sand, alpha: a, line: 1.2 });
      }
      c.restore();

      // Each fsync: one flash to the disk for every write since the last.
      for (const s of syncs) {
        const ids = recent.filter((i) => synced(i) === s);
        if (!ids.length) continue;
        const x1 = inside(cell(ids[0])), x2 = inside(cell(ids[ids.length - 1]) + 10);
        const yb = L.fy + L.fh + 8;
        wire(c, [[x1, yb - 3], [x1, yb], [x2, yb], [x2, yb - 3]], P.oasis, 2, a * flash(t, s, 1.5));
        streak(c, [[(x1 + x2) / 2, yb], L.disk.map((v, q) => v - (q ? 24 : 0))], span(t, s, s + 0.35), { tail: 0.5 });
      }
      chip(c, 'one fsync covers several writes', L.disk[0] + (n ? 124 : 0), L.chip, { size: n ? 15 : 17, center: true, color: P.oasis, alpha: ease(span(t, 3.1, 3.7)) });

      // The readers: a shared lock each, reading all along.
      mono(c, n ? 'readers keep reading' : '4 readers', L.rl[0], L.rl[1], { align: 'center', color: P.sand2, alpha: a });
      L.readers.forEach((r, q) => {
        client(c, ...r, { stroke: P.oasis, alpha: a, glow: 0.3 });
        const pts = [L.from(r), L.to(r)];
        wire(c, pts, P.rule, 1.2, a * 0.7);
        for (let j = 0; j < 3; j++) glow(c, ...along(pts, (t * 1.1 + q * 0.27 + j / 3) % 1), 2.6, a * 0.9);
      });
      if (!n) chip(c, 'readers keep reading', 1120, 420, { size: 15, center: true, alpha: ease(span(t, 2, 2.6)) });
    },
  },
  {
    /* Requests straight to the server and its mapped file: the cache a
       database usually wants in front is crossed out and gone, and each
       answer comes back decoded from the file's own bytes. After that the
       requests never stop: each is drawn from its own number, from whichever
       client sent it to whichever row it asked for. */
    key: 'traffic',
    stream: true, still: 7.3,
    h: 560, nh: 690,
    draw(c, t, n) {
      const a = ease(span(t, 0.1, 0.8));
      const L = n ? {
        client: (k) => [44 + k * 56, 46], cache: [150, 112, 180, 70],
        sx: 140, sy: 226, sw: 200, sh: 130, mark: [240, 270, 0.85],
        fx: 50, fy: 440, fw: 380, fh: 180, page: (p) => [80 + (p % 6) * 56, 478 + Math.floor(p / 6) % 3 * 40],
        in: [240, 226], out: [240, 356], said: [240, 420], end: 664,
      } : {
        client: (k) => [100, 130 + k * 44], cache: [250, 220, 150, 120],
        sx: 520, sy: 210, sw: 200, sh: 140, mark: [620, 260, 1.0],
        fx: 880, fy: 170, fw: 320, fh: 220, page: (p) => [906 + (p % 6) * 48, 212 + Math.floor(p / 6) % 4 * 38],
        in: [520, 280], out: [720, 280], said: [1040, 150], end: 490,
      };
      const pages = n ? 18 : 24, PW = n ? 44 : 38, PH = 30;

      // The clients and their wires to the server.
      for (let k = 0; k < 8; k++) {
        const [x, y] = L.client(k);
        wire(c, [[x + (n ? 0 : 16), y + (n ? 14 : 0)], L.in], P.rule, 1.2, a * 0.8);
        client(c, x, y, { stroke: P.sand2, alpha: a });
      }

      // The server, and the mapped file behind it with nothing between.
      box(c, L.sx, L.sy, L.sw, L.sh, { r: 16, fill: P.panel, stroke: P.sun, alpha: a, line: 2 });
      mark(c, ...L.mark, a, t - 1.6);
      mono(c, 'fenec-server', L.sx + L.sw / 2, L.sy + L.sh - 18, { size: n ? 15 : 17, color: P.hot, align: 'center', alpha: a });
      wire(c, [L.out, n ? [240, L.fy] : [L.fx, 280]], P.rule, 2, a);
      box(c, L.fx, L.fy, L.fw, L.fh, { r: 14, fill: P.deep, stroke: P.sun, alpha: a, line: 1.8 });
      mono(c, 'app.fenec', L.fx + 16, L.fy + 26, { size: 15, color: P.hot, alpha: a });
      mono(c, 'mapped into memory', L.fx + L.fw - 16, L.fy + 26, { size: 13, align: 'right', alpha: a });

      // Requests: in as sand, answered in the mark's light. Request j is
      // sent near R0 + j * RD, in bursts and lulls.
      const R0 = 2.2, RD = 0.1;
      const sent = (j) => R0 + RD * (j + 1.2 * Math.sin(j * 0.21) + 0.3 * (hash(j, 4) - 0.5));
      const lit = new Array(pages).fill(0);
      for (let j = Math.max(0, Math.floor((t - 1.4 - R0) / RD) - 2); j <= (t - R0) / RD + 2; j++) {
        const T = sent(j);
        if (T > t || T < t - 1.4) continue;
        const [cx, cy] = L.client(Math.floor(hash(j, 5) * 8)), p = Math.floor(hash(j, 6) * pages);
        const [px, py] = L.page(p), pc = [px + PW / 2, py + PH / 2];
        const from = [cx + (n ? 0 : 16), cy + (n ? 14 : 0)];
        const k1 = span(t, T, T + 0.4);
        if (k1 > 0 && k1 < 1) dot(c, ...along([from, L.in], inOut(k1)), 3.2, P.sand, a);
        streak(c, [L.out, pc], span(t, T + 0.4, T + 0.55), { tail: 0.4, w: 2 });
        if (t > T + 0.55) lit[p] = Math.max(lit[p], flash(t, T + 0.55, 0.7));
        const k2 = span(t, T + 0.6, T + 1.0);
        if (k2 > 0 && k2 < 1) glow(c, ...along([L.in, from], inOut(k2)), 3.6, a);
      }
      for (let p = 0; p < pages; p++) {
        const [x, y] = L.page(p);
        box(c, x, y, PW, PH, { r: 4, fill: lit[p] > 0.02 ? `rgba(79,224,196,${0.55 * lit[p]})` : P.panel, stroke: lit[p] > 0.3 ? P.oasis : P.rule, alpha: a, line: 1.2 });
      }
      mono(c, 'a row decoded from the file’s bytes', L.said[0], L.said[1], { size: n ? 13 : 15, align: 'center', color: P.sand2, alpha: ease(span(t, 3.2, 3.8)) });

      // The cache that is not there: shown, crossed out, gone.
      const [qx, qy, qw, qh] = L.cache;
      const ca = a * (1 - ease(span(t, 1.5, 2.2)));
      box(c, qx, qy, qw, qh, { r: 12, fill: P.night, stroke: P.dim, alpha: ca, dash: [6, 6] });
      mono(c, 'cache', qx + qw / 2, qy + qh / 2 + 6, { size: 17, align: 'center', alpha: ca });
      const x1 = ease(span(t, 0.7, 1.1));
      if (x1 > 0) {
        line(c, qx + 14, qy + 14, lerp(qx + 14, qx + qw - 14, x1), lerp(qy + 14, qy + qh - 14, x1), P.red, 3, ca * 0.8);
        line(c, qx + qw - 14, qy + 14, lerp(qx + qw - 14, qx + 14, x1), lerp(qy + 14, qy + qh - 14, x1), P.red, 3, ca * 0.8);
      }
      chip(c, 'no cache to warm, evict or keep in step', n ? 240 : 640, L.end, { size: n ? 15 : 17, center: true, color: P.oasis, alpha: ease(span(t, 4.2, 4.8)) });
    },
  },
  {
    /* Tenants, a file each, on nodes behind a router; each has a copy that
       follows it on another node. A tenant moves to another node; then a
       node's lease lapses, it stops writing, and its tenants' copies are
       promoted where they are. */
    key: 'scale',
    d: 10,
    h: 560, nh: 800,
    draw(c, t, n) {
      const a = ease(span(t, 0.1, 0.8));
      const L = n ? {
        router: [160, 14, 160, 50], node: (i) => [20, 104 + i * 220, 440, 200],
        slot: (i, row, s) => [150 + s * 80, 104 + i * 220 + (row ? 150 : 72)],
        rows: [80, 156], path: (i, x, y) => [[160, 39], [8, 39], [8, y], [x - 20, y]], end: 772,
      } : {
        router: [540, 24, 200, 56], node: (i) => [85 + i * 390, 150, 330, 310],
        slot: (i, row, s) => [85 + i * 390 + 85 + s * 70, row ? 376 : 232],
        rows: [210, 330], path: (i, x, y) => [[640, 80], [250 + i * 390, 150], [x, y - 26]], end: 512,
      };
      // Where each tenant lives, and where its copy follows it: three
      // copies a node, none on its own tenant's node.
      const T = [
        { n: 't1', home: [0, 0], copy: [1, 0] }, { n: 't4', home: [0, 1], copy: [2, 0] }, { n: 't7', home: [0, 2], copy: [2, 2] },
        { n: 't2', home: [1, 0], copy: [0, 0] }, { n: 't5', home: [1, 1], copy: [2, 1] }, { n: 't8', home: [1, 2], copy: [0, 1] },
        { n: 't3', home: [2, 0], copy: [1, 1] }, { n: 't6', home: [2, 1], copy: [0, 2] }, { n: 't9', home: [2, 2], copy: [1, 2] },
      ];
      const MOVE = [3.2, 4.4], FAIL = 5.4, PROMOTE = 6.4;
      const down = ease(span(t, FAIL, FAIL + 0.5));
      const fs = n ? 1.0 : 1.1;
      const named = (s, x, y, o) => mono(c, s, x, y + (n ? 32 : 36), { size: 13, align: 'center', ...o });

      // The router and its wires.
      const [rx, ry, rw, rh] = L.router;
      box(c, rx, ry, rw, rh, { r: 12, fill: P.panel2, stroke: P.sun, alpha: a, line: 2 });
      mono(c, 'router', rx + rw / 2, ry + rh / 2 + 6, { size: 17, color: P.hot, align: 'center', alpha: a });
      [0, 1, 2].forEach((i) => {
        const [x, y, w, h] = L.node(i);
        const d = i === 1 ? down : 0;
        const pts = n ? L.path(i, 40, y + 100) : [[640, 80], [x + w / 2, y]];
        wire(c, pts, d > 0.5 ? P.red : P.rule, 1.5, a * (1 - d * 0.5), d > 0.5 ? [4, 6] : null);
        box(c, x, y, w, h, { r: 16, fill: P.panel, stroke: d > 0.5 ? P.red : P.rule, alpha: a, line: 2 });
        text(c, `node ${i + 1}`, x + 18, y + 30, { size: 18, weight: 600, color: d > 0.5 ? P.red : P.sand, alpha: a });
        mono(c, 'tenants', n ? x + 16 : x + w - 18, n ? y + L.rows[0] - 4 : y + 30, { size: 13, align: n ? 'left' : 'right', alpha: a });
        mono(c, 'copies', n ? x + 16 : x + w - 18, n ? y + L.rows[1] - 4 : y + 172, { size: 13, align: n ? 'left' : 'right', alpha: a * 0.8 });
        if (!n) line(c, x + 18, y + 150, x + w - 18, y + 150, P.rule, 1, a * 0.6, [3, 5]);
        if (d > 0) mono(c, 'lease lapsed, stopped writing', n ? x + w - 16 : x + w / 2, n ? y + 30 : y + h - 16, { size: 13, align: n ? 'right' : 'center', color: P.red, alpha: a * d });
      });

      // The tenants and their copies.
      T.forEach((tn, k) => {
        const [hi, hs] = tn.home;
        let [x, y] = L.slot(hi, 0, hs);
        const moved = tn.n === 't1';
        if (moved) {
          const m = inOut(span(t, ...MOVE));
          const [tx, ty] = L.slot(2, 0, 3);
          const arc = Math.sin(Math.PI * m) * (n ? 70 : 150);
          x = lerp(x, tx, m) + (n ? arc : 0); y = lerp(y, ty, m) - (n ? 0 : arc);
          if (m > 0 && m < 1) glow(c, x, y, 6);
        }
        const gone = hi === 1 ? down : 0;
        // A request routed to it, early on.
        const ask = [1.1, 1.5, 1.9, 2.3][[0, 4, 8, 1].indexOf(k)];
        if (ask !== undefined) streak(c, L.path(hi, x, y), span(t, ask, ask + 0.6), { tail: 0.25, w: 2.5 });
        const hit = ask !== undefined ? flash(t, ask + 0.6, 0.8) * (t > ask + 0.6 ? 1 : 0) : 0;
        file(c, x, y, fs, { stroke: gone > 0.5 ? P.red : P.sun, alpha: a * (1 - gone * 0.6), glow: hit + (moved ? flash(t, MOVE[1], 1) * (t > MOVE[1] ? 1 : 0) : 0) });
        named(tn.n, x, y, { color: P.hot, alpha: a * (1 - gone * 0.6) });
        if (moved) mono(c, 'moved', x, y - (n ? 26 : 30), { align: 'center', size: 12, color: P.oasis, alpha: ease(span(t, MOVE[1], MOVE[1] + 0.4)) });

        // Its copy; a copy of a failed node's tenant is promoted.
        const [ci, cs] = tn.copy;
        const [cx, cy] = L.slot(ci, 1, cs);
        const up = hi === 1 ? ease(span(t, PROMOTE + cs * 0.25, PROMOTE + 0.5 + cs * 0.25)) : 0;
        const gp = hi === 1 ? flash(t, PROMOTE + cs * 0.25 + 0.6, 1.4, 0.45) * (t > PROMOTE + cs * 0.25 + 0.6 ? 1 : 0) : 0;
        if (hi === 1) streak(c, L.path(ci, cx, cy), span(t, PROMOTE + cs * 0.25, PROMOTE + 0.6 + cs * 0.25), { tail: 0.25, w: 2.5 });
        file(c, cx, cy, fs * 0.9, { stroke: up > 0.5 ? P.oasis : P.dim, alpha: a * (0.6 + 0.4 * up), glow: gp, dash: up > 0.5 ? null : [3, 3] });
        named(tn.n, cx, cy, { color: up > 0.5 ? P.oasis : P.dim, alpha: a * (0.7 + 0.3 * up) });
        if (hi === 1) named('promoted', cx, cy + 16, { size: 12, color: P.oasis, alpha: up });
      });

      // What is happening, a line at a time.
      const say = [
        ['a file a tenant, each with a copy on another node', 0.9, MOVE[0] - 0.2],
        ['a tenant moved to another node', MOVE[0], FAIL - 0.2],
        ['node 2 stops writing as its lease lapses', FAIL, PROMOTE + 0.3],
        ['its tenants take writes on their copies', PROMOTE + 0.5, 99],
      ];
      for (const [s, from, to] of say) {
        const k = ease(span(t, from, from + 0.4)) * (1 - ease(span(t, to, to + 0.3)));
        chip(c, s, n ? 240 : 640, L.end, { size: n ? 15 : 17, center: true, color: to === 99 ? P.oasis : P.sand, alpha: k });
      }
    },
  },
  {
    /* A question in plain words. `near` finds the documents nearest it by
       meaning; `match` finds the ones holding its words, which light; and
       `fuse` adds each one's 1 / (60 + rank) from both lists, so what both
       found comes first. The documents, their places and both rankings are
       an example; the arithmetic of the fusion is fenecdb's. */
    key: 'search',
    d: 10,
    h: 560, nh: 830,
    draw(c, t, n) {
      const a = ease(span(t, 0.1, 0.8));
      const terms = new Set(['warm', 'places', 'to', 'sleep', 'outdoors']);
      // Where each document sits by meaning; the query's place is Q.
      const DOCS = {
        1: ['Sleep outdoors', 300, 305, 'up'], 2: ['Down bag', 330, 360, 'right'],
        3: ['Warm places', 215, 250, 'up'], 4: ['Heated tent', 265, 365, 'left'],
        5: ['Campfire', 370, 310, 'right'], 6: ['Warm soup', 110, 170, 'up'],
        8: ['Quiet places', 450, 150, 'up'],
      };
      const NEAR = [2, 4, 1, 5, 3], MATCH = [1, 3, 6, 8];
      const fused = Object.keys(DOCS).map(Number).map((id) => {
        const r1 = NEAR.indexOf(id) + 1, r2 = MATCH.indexOf(id) + 1;
        return { id, r1, r2, s: (r1 ? 1 / (60 + r1) : 0) + (r2 ? 1 / (60 + r2) : 0) };
      }).sort((x, y) => y.s - x.s || x.id - y.id).slice(0, 5);

      const k = n ? 0.82 : 1, ox = n ? -6 : 0, oy = n ? -6 : 0;
      const at = (x, y) => [ox + x * k, oy + y * k + (n ? 20 : 0)];
      const Q = at(300, 350);
      const COL = n ? [10, 167, 324] : [600, 830, 1060], cw = n ? 146 : 200;
      const top = n ? 448 : 112, row0 = n ? 506 : 172, step = n ? 52 : 60, rh = n ? 44 : 50;
      const ts = n ? 13.5 : 16;

      // The question.
      const qa = ease(span(t, 0.3, 0.9));
      chip(c, '“warm places to sleep outdoors”', n ? 240 : 30, n ? 28 : 46, { size: n ? 15 : 18, color: P.star, alpha: qa, center: n });

      // The documents, by meaning: a sky of them, these few named.
      const r = rng(11);
      for (let i = 0; i < 46; i++) {
        let x, y;
        do { x = 40 + r() * 490; y = 100 + r() * 410; } while (Math.hypot(x - 300, y - 350) < 150);
        dot(c, ...at(x, y), 2.2, P.sand2, a * 0.45);
      }
      const nearK = (id) => { const i = NEAR.indexOf(id); return i < 0 ? 0 : ease(span(t, 1.9 + i * 0.18, 2.3 + i * 0.18)); };
      const matchK = (id) => { const i = MATCH.indexOf(id); return i < 0 ? 0 : ease(span(t, 3.9 + i * 0.25, 4.2 + i * 0.25)); };
      for (const [id, [s, x0, y0, side]] of Object.entries(DOCS)) {
        const [x, y] = at(x0, y0);
        const nk = nearK(+id), mk = matchK(+id);
        streak(c, [Q, [x, y]], span(t, 1.7 + NEAR.indexOf(+id) * 0.18, 2.1 + NEAR.indexOf(+id) * 0.18), { tail: 0.5, w: 2 });
        if (nk > 0.5) line(c, ...Q, x, y, P.oasis, 1, a * 0.35);
        if (mk > 0) box(c, x - 9, y - 9, 18, 18, { r: 9, fill: null, stroke: P.sun, alpha: a * mk, line: 1.4 });
        if (nk > 0) glow(c, x, y, 4, a * nk); else dot(c, x, y, 4, P.sand, a);
        const o = { size: n ? 12 : 13, color: nk > 0.5 ? P.oasis : P.sand2, alpha: a };
        if (side === 'up') mono(c, s, x, y - 13, { ...o, align: 'center' });
        else if (side === 'left') mono(c, s, x - 12, y + 18, { ...o, align: 'right' });
        else mono(c, s, x + 12, y + 5, o);
      }
      // The query lands among them, and its rings go out.
      const land = ease(span(t, 1.0, 1.5));
      if (land > 0) {
        streak(c, [n ? [240, 46] : [180, 64], Q], span(t, 1.0, 1.5), { tail: 0.4, w: 2.5 });
        for (const d of [0, 0.35]) {
          const rr = span(t, 1.5 + d, 2.6 + d);
          if (rr > 0 && rr < 1) box(c, Q[0] - 90 * k * rr, Q[1] - 90 * k * rr, 180 * k * rr, 180 * k * rr, { r: 90 * k * rr, fill: null, stroke: P.oasis, alpha: (1 - rr) * 0.8, line: 1.5 });
        }
        if (t > 1.5) glow(c, ...Q, 6, a);
      }

      // The three rankings.
      const head = [['near', 'by meaning'], ['match', 'by the words'], ['fuse', '1 / (60 + rank), added']];
      head.forEach(([h, sub], i) => {
        const ha = ease(span(t, [1.8, 3.7, 5.3][i], [2.2, 4.1, 5.7][i]));
        text(c, h, COL[i], top, { size: n ? 16 : 19, weight: 600, color: i === 2 ? P.oasis : P.star, alpha: ha });
        mono(c, sub, COL[i], top + (n ? 18 : 22), { size: n ? 10.5 : 13, alpha: ha });
      });
      const rowAt = (col, i) => [COL[col], row0 + i * step];
      const card = (x, y, id, rank, alpha, { lit = 0, stroke = P.rule, score = null, g = 0 } = {}) => {
        box(c, x, y, cw, rh, { r: 8, fill: P.panel, stroke, alpha, glow: g });
        mono(c, String(rank), x + 10, y + (score ? 20 : rh / 2 + 5), { size: 12, alpha });
        words(c, DOCS[id][0], x + (n ? 24 : 30), y + (score ? 20 : rh / 2 + 5), { size: ts, lit, terms, alpha });
        if (score) mono(c, score, x + (n ? 24 : 30), y + rh - 9, { size: n ? 10.5 : 12, color: P.oasis, alpha });
      };
      NEAR.forEach((id, i) => card(...rowAt(0, i), id, i + 1, ease(span(t, 2.3 + i * 0.15, 2.6 + i * 0.15)), { stroke: P.rule }));
      MATCH.forEach((id, i) => {
        const ma = ease(span(t, 3.9 + i * 0.25, 4.2 + i * 0.25));
        card(...rowAt(1, i), id, i + 1, ma, { lit: ease(span(t, 4.2 + i * 0.25, 4.6 + i * 0.25)) });
      });
      // Fusion: each document flies in from the lists it is on.
      fused.forEach((f, i) => {
        const go = 5.6 + i * 0.25, m = inOut(span(t, go, go + 0.7));
        if (m <= 0) return;
        const [tx, ty] = rowAt(2, i);
        for (const [col, rank] of [[0, f.r1], [1, f.r2]]) {
          if (!rank || m >= 1) continue;
          const [sx, sy] = rowAt(col, rank - 1);
          box(c, lerp(sx, tx, m), lerp(sy, ty, m), cw, rh, { r: 8, fill: P.panel2, stroke: P.oasis, alpha: 0.7 * (1 - m * 0.5), glow: 0.5 });
        }
        if (m < 1) return;
        const both = f.r1 && f.r2;
        const parts = [f.r1 && `1/${60 + f.r1}`, f.r2 && `1/${60 + f.r2}`].filter(Boolean).join(' + ');
        card(tx, ty, f.id, i + 1, a, { lit: 1, stroke: both ? P.oasis : P.rule, g: both ? flash(t, go + 0.7, 1, 0.4) : 0, score: parts });
      });
      chip(c, 'found by both, ranked first', n ? 240 : 920, n ? 800 : 500, { size: n ? 15 : 17, center: true, color: P.oasis, alpha: ease(span(t, 7.4, 8)) });
    },
  },
  {
    /* Security, as a stream: two users' requests carry tokens signed with
       the server's key, pass the token check and then the rules, and each
       reads back only its own rows; a write of a row outside its rules is
       refused (403). A token signed with another key is refused at the door
       (401), and each refusal from that address waits twice as long as the
       last, 100 ms up to 5 s, and goes into the audit log. Meanwhile sealed
       backups leave for a bucket. The waits are the server's own, in real
       seconds; the users, the rows and the address are an example. */
    key: 'security',
    stream: true, still: 17.6,
    h: 560, nh: 860,
    draw(c, t, n) {
      const a = ease(span(t, 0.1, 0.8));
      const OWNERS = ['ada', 'ben', 'ada', 'ben', 'ben', 'ada'];
      const L = n ? {
        user: [[80, 60], [240, 60], [400, 60]],
        path: ([x, y]) => [[x, y + 16], [x, 128], [240, 170]],
        sx: 110, sy: 170, sw: 260, sh: 150, mark: [240, 226, 0.8], G: [240, 170], R: [240, 320],
        fx: 30, fy: 392, fw: 420, fh: 172, row: (r) => [48 + (r % 2) * 200, 412 + Math.floor(r / 2) * 48, 184, 36],
        toFile: [[240, 320], [240, 392]],
        seal: [[300, 564], [300, 604], [396, 604]], bucket: [424, 600], sealSay: [36, 608],
        lx: 30, ly: 672, lw: 420, said: [240, 846], refuse: [258, 374],
      } : {
        user: [[120, 140], [120, 270], [120, 420]],
        path: ([x, y]) => [[x + 18, y], [380, y], [470, 265]],
        sx: 470, sy: 150, sw: 230, sh: 230, mark: [585, 222, 1.05], G: [470, 265], R: [700, 265],
        fx: 840, fy: 140, fw: 310, fh: 262, row: (r) => [858, 158 + r * 38, 274, 30],
        toFile: [[700, 265], [840, 265]],
        seal: [[995, 406], [995, 470], [1180, 470]], bucket: [1212, 468], sealSay: [1090, 504],
        lx: 440, ly: 410, lw: 290, said: [995, 96], refuse: [770, 300],
      };
      const [G, R] = [L.G, L.R];

      // Reads and writes from the two users, one every 1.5 s or so; one in
      // four is a write of the other user's row, which the rules refuse.
      const A0 = 1.1, AG = 1.5;
      const ask = (m) => A0 + m * AG + (hash(m, 7) - 0.5) * 0.6;
      const asks = [];
      for (let m = Math.max(0, Math.floor((t - 2.4 - A0) / AG)); m <= (t - A0) / AG + 1; m++) {
        const T = ask(m);
        if (T <= t && T > t - 2.4) asks.push({ T, who: hash(m, 8) < 0.5 ? 0 : 1, bad: m > 2 && hash(m, 9) < 0.25 });
      }
      // The token signed with another key: refused, and each refusal waits
      // twice the last, 5 s at most. Attempt k starts at `tries[k]`.
      const wait = (k) => Math.min(5, 0.1 * 2 ** k);
      const tries = [2.4];
      for (let k = 0; k < 8; k++) tries.push(tries[k] + 0.6 + wait(k) + 0.6 + 0.8);
      const tryAt = (k) => k < 8 ? tries[k] : tries[7] + (k - 7) * (0.6 + 5 + 0.6 + 0.8);
      let k = 0;
      while (tryAt(k + 1) <= t) k++;
      const refusedAt = (j) => tryAt(j) + 0.6 + wait(j);
      // Sealed backups, one every 2.6 s or so.
      const B0 = 1.6, BG = 2.6;
      const sealAt = (b) => B0 + b * BG + (hash(b, 10) - 0.5) * 0.8;

      // The server: the token checked at its door, the rules at its back.
      box(c, L.sx, L.sy, L.sw, L.sh, { r: 16, fill: P.panel, stroke: P.sun, alpha: a, line: 2 });
      mark(c, ...L.mark, a, t - 0.4);
      mono(c, 'fenec-server', L.sx + L.sw / 2, L.sy + L.sh - (n ? 16 : 22), { size: n ? 15 : 17, color: P.hot, align: 'center', alpha: a });
      const okG = Math.max(0, ...asks.map((q) => t > q.T + 0.5 ? flash(t, q.T + 0.5, 0.6) : 0));
      const badR = Math.max(0, ...asks.filter((q) => q.bad).map((q) => t > q.T + 0.8 ? flash(t, q.T + 0.8, 0.9) : 0));
      const okR = Math.max(0, ...asks.filter((q) => !q.bad).map((q) => t > q.T + 0.8 ? flash(t, q.T + 0.8, 0.6) : 0));
      const noG = t > refusedAt(k) ? flash(t, refusedAt(k), 0.9) : 0;
      // A door is a bar across the way in; its name sits outside the box.
      const door = (p, lit, bad, label, side) => {
        const [x, y] = p, v = !n;
        line(c, x - (v ? 0 : 22), y - (v ? 22 : 0), x + (v ? 0 : 22), y + (v ? 22 : 0), bad > lit ? P.ember : lit > 0.05 ? P.oasis : P.sand2, 4, a);
        if (Math.max(lit, bad) > 0) glow(c, x, y, 5, Math.max(lit, bad) * 0.8);
        mono(c, label, x + (n ? 30 : 14 * side), y + (n ? (side < 0 ? -8 : 18) : -30), { size: 12.5, align: n || side > 0 ? 'left' : 'right', color: P.sand2, alpha: a });
      };
      door(G, okG, noG, 'token', -1);
      door(R, okR, badR, 'rules', 1);

      // The rows, a user's each, in the app's file.
      mono(c, 'app.fenec', L.fx, L.fy - 12, { size: 15, color: P.hot, alpha: a });
      mono(c, 'notes', L.fx + L.fw, L.fy - 12, { size: 13, align: 'right', alpha: a });
      box(c, L.fx, L.fy, L.fw, L.fh, { r: 12, fill: P.deep, stroke: P.sun, alpha: a, line: 1.8 });
      wire(c, L.toFile, P.rule, 2, a);
      const rowLit = OWNERS.map((o) => Math.max(0, ...asks.filter((q) => !q.bad && ['ada', 'ben'][q.who] === o)
        .map((q) => t > q.T + 1.0 ? flash(t, q.T + 1.0, 1.4) : 0)));
      OWNERS.forEach((o, r) => {
        const [x, y, w, h] = L.row(r), on = rowLit[r];
        box(c, x, y, w, h, { r: 6, fill: on > 0.02 ? `rgba(79,224,196,${0.32 * on})` : P.panel, stroke: on > 0.3 ? P.oasis : P.rule, alpha: a, glow: on * 0.6 });
        mono(c, `owner ${o}`, x + 12, y + h / 2 + 5, { size: 12.5, color: on > 0.3 ? P.oasis : P.dim, alpha: a });
        line(c, x + (n ? 104 : 120), y + h / 2, x + w - 14, y + h / 2, on > 0.3 ? P.oasis : P.rule, 4, a * 0.8);
      });
      text(c, 'each token reads its own rows', ...L.said, { size: n ? 15 : 16, align: 'center', color: P.oasis, alpha: ease(span(t, 2.6, 3.2)) });

      // The users, their tokens on them.
      const names = ['ada', 'ben', 'another key'];
      L.user.forEach((u, q) => {
        const [x, y] = u;
        wire(c, L.path(u), P.rule, 1.2, a * 0.8, q === 2 ? [4, 6] : null);
        const back = q < 2 ? Math.max(0, ...asks.filter((e) => e.who === q && !e.bad).map((e) => t > e.T + 1.6 ? flash(t, e.T + 1.6, 1) : 0)) : 0;
        client(c, x, y, { stroke: back > 0.05 ? P.oasis : P.sand2, alpha: a, glow: back });
        // The token: a ticket, the server's key's in sand, another's in ember.
        box(c, x + 16, y - 26, 22, 13, { r: 3, fill: P.panel2, stroke: q === 2 ? P.ember : P.sand, alpha: a, line: 1.3, dash: q === 2 ? [3, 2] : null });
        dot(c, x + 22, y - 19.5, 2, q === 2 ? P.ember : P.sand, a);
        mono(c, names[q], x, y + (n ? 34 : 38), { size: 13, align: 'center', color: q === 2 ? P.ember : P.sand2, alpha: a });
      });

      // The two users' requests: in to the door, through the rules, to
      // their rows and back in the mark's light, or refused at the rules.
      for (const q of asks) {
        const pts = L.path(L.user[q.who]), T = q.T;
        const k1 = span(t, T, T + 0.5);
        if (k1 > 0 && k1 < 1) dot(c, ...along(pts, inOut(k1)), 3.6, q.bad ? P.sun : P.sand, a);
        streak(c, [G, R], span(t, T + 0.5, T + 0.8), { tail: 0.4, w: 2.5 });
        if (q.bad) {
          // WITH CHECK: the row it writes is not its own, refused (403).
          const back = span(t, T + 0.8, T + 1.5);
          if (back > 0 && back < 1) dot(c, ...along([R, G, ...[...pts].reverse()], inOut(back)), 3.6, P.ember, a);
          const say = ease(span(t, T + 0.8, T + 1.0)) * (1 - ease(span(t, T + 2.0, T + 2.3)));
          mono(c, '403: not its row',...L.refuse, { size: 13, align: n ? 'left' : 'center', color: P.ember, alpha: a * say });
        } else {
          streak(c, L.toFile, span(t, T + 0.8, T + 1.0), { tail: 0.4, w: 2.5 });
          streak(c, [...[...L.toFile].reverse(), G, ...[...pts].reverse()], span(t, T + 1.05, T + 1.6), { tail: 0.25, w: 2.5 });
        }
      }

      // The other key: to the door, held there while the refusal waits,
      // then sent back with 401.
      const op = L.path(L.user[2]);
      for (let j = Math.max(0, k - 1); j <= k; j++) {
        const T = tryAt(j), d = wait(j), at = T + 0.6, out = at + d;
        const k1 = span(t, T, at);
        if (k1 > 0 && k1 < 1) dot(c, ...along(op, inOut(k1)), 3.6, P.ember, a);
        if (t >= at && t < out) {
          // The wait, as a ring closing around the held request.
          const [hx, hy] = along(op, 0.93), f = (t - at) / d;
          dot(c, hx, hy, 3.6, P.ember, a);
          c.save(); c.globalAlpha *= a; c.strokeStyle = P.ember; c.lineWidth = 2;
          c.beginPath(); c.arc(hx, hy, 11, -Math.PI / 2, -Math.PI / 2 + f * Math.PI * 2); c.stroke(); c.restore();
        }
        const back = span(t, out, out + 0.6);
        if (back > 0 && back < 1) dot(c, ...along([...op].reverse(), inOut(back)), 3.6, P.ember, a);
        const say = ease(span(t, at, at + 0.2)) * (1 - ease(span(t, out + 0.9, out + 1.2)));
        const ms = d < 1 ? `${Math.round(d * 1000)} ms` : `${d} s`;
        const [wx, wy] = n ? [L.user[2][0] - 2, 128] : [300, L.user[2][1] + 30];
        mono(c, t < out ? `waits ${ms}` : `401 after ${ms}`, wx, wy, { size: 13, align: n ? 'right' : 'center', color: P.ember, alpha: a * say });
      }
      if (!n) mono(c, 'each refusal waits twice the last, 5 s at most', 40, L.user[2][1] + 84, { size: 12.5, color: P.dim, alpha: ease(span(t, 4, 4.6)) });

      // The audit log: a line for each refusal of a token.
      const lh = 22, rows = 3;
      box(c, L.lx, L.ly, L.lw, 38 + rows * lh, { r: 10, fill: '#0C0819', stroke: P.rule, alpha: a });
      mono(c, 'audit log', L.lx + 14, L.ly + 24, { size: 13, color: P.sand2, alpha: a });
      const logged = [];
      for (let j = 0; j <= k; j++) if (refusedAt(j) <= t) logged.push(j);
      logged.slice(-rows).reverse().forEach((j, i) => {
        const fresh = flash(t, refusedAt(j), 1.2);
        mono(c, 'refused  GET /notes  198.51.100.4', L.lx + 14, L.ly + 50 + i * lh, { size: n ? 12.5 : 12, color: fresh > 0.2 ? P.ember : P.dim, alpha: a * ease((t - refusedAt(j)) / 0.3) });
      });

      // Backups: sealed with a key you hold, on their way to a bucket.
      const [bx, by] = L.bucket;
      wire(c, L.seal, P.rule, 1.2, a * 0.6, [3, 5]);
      let landed = 0;
      for (let b = Math.max(0, Math.floor((t - 2.4 - B0) / BG)); b <= (t - B0) / BG + 1; b++) {
        const T = sealAt(b), m = span(t, T, T + 2.0);
        if (t > T + 2.0) landed = Math.max(landed, flash(t, T + 2.0, 1));
        if (m <= 0 || m >= 1) continue;
        const [x, y] = along(L.seal, m);
        const sealed = ease(span(t, T + 0.15, T + 0.45));
        file(c, x, y, 0.7, { stroke: P.sand2, alpha: a });
        lock(c, x + 6, y + 4, P.oasis, a * sealed);
      }
      bucket(c, bx, by, { alpha: a, glow: landed });
      mono(c, 'sealed backups, to a bucket', ...L.sealSay, { size: 13, align: n ? 'left' : 'center', color: P.sand2, alpha: a });
    },
  },
];

const STARS = (() => {
  const r = rng(3);
  return Array.from({ length: 160 }, () => [r(), r(), r() * 1.3 + 0.3, r() * 6.28]);
})();

export const SCENE = Object.fromEntries(SCENES.map((s) => [s.key, s]));

/* How long scene `key` runs before it starts again: its story, then its last
   frame held, still moving where it moves (a frozen hold was the stutter
   before each start); a stream never starts again. */
const HOLD = 2.2;
export function cycle(key) {
  const s = SCENE[key];
  return s.stream ? Infinity : s.d + HOLD;
}

/* The moment that says everything the scene does, for a reader who asked
   for no motion. */
export function still(key) {
  const s = SCENE[key];
  return s.stream ? s.still : s.d;
}

/* The stage scene `key` is drawn on: a screen's, or a phone's. */
export function stage(key, narrow) {
  const s = SCENE[key];
  return narrow ? { w: NW, h: s.nh } : { w: W, h: s.h };
}

/* Draws scene `key` at `t` seconds of its own into a context of any size.
   The stars twinkle by `clock`, the figure's own time, which runs on across
   a scene's start: by the scene's they jumped as it began again. */
export function render(c, key, t, width, height, narrow = false, clock = t) {
  const scene = SCENE[key];
  const { w, h } = stage(key, narrow);
  const s = Math.min(width / w, height / h);
  c.setTransform(1, 0, 0, 1, 0, 0);
  c.clearRect(0, 0, width, height);
  c.setTransform(s, 0, 0, s, (width - w * s) / 2, (height - h * s) / 2);
  for (const [x, y, r, ph] of STARS) dot(c, x * w, y * h, r, P.star, 0.14 + 0.12 * Math.sin(clock * 1.3 + ph));
  c.save();
  const end = cycle(key);
  c.globalAlpha = ease(span(t, 0, 0.45)) * (1 - ease(span(t, end - 0.6, end)));
  scene.draw(c, t, narrow);
  c.restore();
}
