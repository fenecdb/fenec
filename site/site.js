/* fenecdb — the dune field.
   Everything here is either the real engine running or a real measurement
   being drawn. No framework, no build step: the same constraint the database
   keeps. Motion is skipped wholesale when the visitor asks for less of it. */

const still = matchMedia('(prefers-reduced-motion: reduce)').matches;
const root = document.documentElement;

requestAnimationFrame(() => root.classList.add('loaded'));

/* ------------------------------------------------------------ docs sidebar */

const toggle = document.querySelector('.side-toggle');
const side = document.getElementById('side');
if (toggle && side) {
  toggle.addEventListener('click', () => {
    const open = side.classList.toggle('open');
    toggle.setAttribute('aria-expanded', String(open));
  });
}

/* Table of contents: the active entry is the last heading that has passed
   under the sticky header. */
const tocLinks = [...document.querySelectorAll('.toc a')];
if (tocLinks.length) {
  const marks = tocLinks
    .map((a) => ({ link: a, el: document.getElementById(decodeURIComponent(a.hash.slice(1))) }))
    .filter((m) => m.el);
  let queued = false;
  const mark = () => {
    queued = false;
    let active = marks[0];
    for (const m of marks) {
      if (m.el.getBoundingClientRect().top <= 100) active = m;
      else break;
    }
    if (innerHeight + scrollY >= document.body.scrollHeight - 4) active = marks[marks.length - 1];
    for (const m of marks) m.link.classList.toggle('here', m === active);
  };
  const schedule = () => { if (!queued) { queued = true; requestAnimationFrame(mark); } };
  addEventListener('scroll', schedule, { passive: true });
  addEventListener('resize', schedule, { passive: true });
  mark();
}

/* ------------------------------------------------------- scroll reveals */

const seen = new IntersectionObserver((entries) => {
  for (const e of entries) {
    if (!e.isIntersecting) continue;
    e.target.classList.add('seen');
    seen.unobserve(e.target);
  }
}, { rootMargin: '0px 0px -12% 0px' });
document.querySelectorAll('.rise, .stratum').forEach((el) => seen.observe(el));

/* ================================================================= the sky */

/* Neither sky canvas should burn a frame once the reader has scrolled past
   it. One observer drives both. */
const skyAwake = { on: true, subs: [] };
{
  const hero = document.querySelector('.hero');
  if (hero && typeof IntersectionObserver === 'function') {
    new IntersectionObserver((e) => {
      const on = e.some((x) => x.isIntersecting);
      if (on === skyAwake.on) return;
      skyAwake.on = on;
      if (on) for (const f of skyAwake.subs) f();
    }, { rootMargin: '80px' }).observe(hero);
  }
  addEventListener('visibilitychange', () => {
    const on = !document.hidden && skyAwake.on;
    if (on) for (const f of skyAwake.subs) f();
  });
}

function fitCanvas(c) {
  const dpr = Math.min(devicePixelRatio || 1, 2);
  const r = c.getBoundingClientRect();
  c.width = Math.max(1, Math.round(r.width * dpr));
  c.height = Math.max(1, Math.round(r.height * dpr));
  const ctx = c.getContext('2d');
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  return { ctx, w: r.width, h: r.height };
}

const starCanvas = document.querySelector('.stars');
if (starCanvas) {
  let stars = [], ctx, w, h, t0 = performance.now();
  const build = () => {
    ({ ctx, w, h } = fitCanvas(starCanvas));
    const n = Math.round(Math.min(w * h / 7000, 220));
    stars = Array.from({ length: n }, () => ({
      x: Math.random() * w,
      // Thinner near the horizon, where the afterglow washes them out.
      y: Math.pow(Math.random(), 1.7) * h * 0.82,
      r: Math.random() * 1.25 + 0.35,
      p: Math.random() * Math.PI * 2,
      s: 0.5 + Math.random() * 1.6,
    }));
  };
  const draw = (now) => {
    const t = (now - t0) / 1000;
    ctx.clearRect(0, 0, w, h);
    for (const st of stars) {
      const fade = 1 - st.y / (h * 0.95);
      const tw = still ? 0.8 : 0.62 + 0.38 * Math.sin(t * st.s + st.p);
      ctx.globalAlpha = Math.max(0, fade * tw * 0.95);
      ctx.fillStyle = st.r > 1.15 ? '#FFE9BC' : '#FFF4DC';
      ctx.beginPath();
      ctx.arc(st.x, st.y, st.r, 0, 7);
      ctx.fill();
      if (st.r > 1.1) {
        ctx.globalAlpha *= 0.3;
        ctx.beginPath();
        ctx.arc(st.x, st.y, st.r * 3.4, 0, 7);
        ctx.fill();
      }
    }
    ctx.globalAlpha = 1;
    running = !still && skyAwake.on && !document.hidden;
    if (running) requestAnimationFrame(draw);
  };
  let running = false;
  const wake = () => { if (!running) { running = true; requestAnimationFrame(draw); } };
  skyAwake.subs.push(wake);
  build();
  wake();
  addEventListener('resize', () => { build(); wake(); }, { passive: true });
}

/* Sand on the wind. Grains stream right to left and settle toward the ridge. */
const sandCanvas = document.querySelector('.sandfield');
if (sandCanvas && !still) {
  let grains = [], ctx, w, h;
  const build = () => {
    ({ ctx, w, h } = fitCanvas(sandCanvas));
    const n = Math.round(Math.min(w / 7, 180));
    grains = Array.from({ length: n }, () => spawn(Math.random() * w));
  };
  const spawn = (x) => ({
    x, y: h * (0.35 + Math.random() * 0.62),
    v: 24 + Math.random() * 96,
    r: Math.random() * 1.5 + 0.35,
    a: 0.06 + Math.random() * 0.4,
    p: Math.random() * Math.PI * 2,
  });
  let last = performance.now();
  const draw = (now) => {
    const dt = Math.min((now - last) / 1000, 0.05);
    last = now;
    ctx.clearRect(0, 0, w, h);
    for (const g of grains) {
      g.x -= g.v * dt;
      g.p += dt * 1.7;
      g.y += Math.sin(g.p) * 0.28;
      if (g.x < -8) Object.assign(g, spawn(w + Math.random() * 60));
      ctx.globalAlpha = g.a;
      ctx.fillStyle = g.v > 90 ? '#FFCE73' : '#F0E2C4';
      ctx.fillRect(g.x, g.y, g.r * (1 + g.v / 80), g.r);
    }
    ctx.globalAlpha = 1;
    running = skyAwake.on && !document.hidden;
    if (running) requestAnimationFrame(draw);
    else last = performance.now();
  };
  let running = false;
  const wake = () => { if (!running) { running = true; last = performance.now(); requestAnimationFrame(draw); } };
  skyAwake.subs.push(wake);
  build();
  wake();
  addEventListener('resize', () => { build(); wake(); }, { passive: true });
}

/* Dune parallax: the near ridge travels fastest, as it would from a car. */
const ridges = [...document.querySelectorAll('.dunes .ridge')];
if (ridges.length && !still) {
  const rates = [0.16, 0.1, 0.055, 0.02];
  let queued = false;
  const move = () => {
    queued = false;
    const y = scrollY;
    ridges.forEach((r, i) => { r.style.setProperty('--py', `${y * (rates[i] ?? 0.05)}px`); });
  };
  addEventListener('scroll', () => { if (!queued) { queued = true; requestAnimationFrame(move); } }, { passive: true });
  move();
}

/* ====================================================== the fennec in space */

/* WebGL draws the head where the layout left room for it; the SVG in that
   room is what shows until the first frame, and what stays where WebGL
   cannot run or the visitor asked for less motion and less data. */
const space = document.querySelector('.space');
const markRoom = document.querySelector('.hero-mark');
if (space && markRoom && !navigator.connection?.saveData) {
  const go = () => import('./scene.js').then(({ start }) => {
    const scene = start(space, markRoom, {
      still,
      onFirstFrame: () => document.querySelector('.hero').classList.add('gl'),
    });
    if (scene) document.addEventListener('hero-typed', () => scene.ask());
  }).catch(() => {});
  // After the first paint: the headline and the SVG mark come first.
  if ('requestIdleCallback' in window) requestIdleCallback(go, { timeout: 600 });
  else setTimeout(go, 200);
}

/* ====================================================== typing the headline */

const heroQ = document.querySelector('.hero-q');
if (heroQ) {
  const nodes = [];
  const walk = document.createTreeWalker(heroQ, NodeFilter.SHOW_TEXT);
  for (let n = walk.nextNode(); n; n = walk.nextNode()) nodes.push({ node: n, text: n.data });
  const total = nodes.reduce((s, n) => s + n.text.length, 0);

  const caret = document.createElement('span');
  caret.className = 'caret';

  if (still || total > 400) {
    heroQ.classList.add('typed');
    heroQ.append(caret);
  } else {
    for (const n of nodes) n.node.data = '';
    let i = 0, ni = 0, shown = 0;
    const tick = () => {
      // Three characters a frame, with a beat at each line break.
      const step = 3;
      while (shown < step && ni < nodes.length) {
        const cur = nodes[ni];
        if (cur.node.data.length >= cur.text.length) { ni++; continue; }
        const ch = cur.text[cur.node.data.length];
        cur.node.data += ch;
        cur.node.parentNode.insertBefore(caret, cur.node.nextSibling);
        shown++;
        i++;
        if (ch === '\n') { shown = 99; }
      }
      shown = 0;
      if (i < total) setTimeout(tick, nodes[ni]?.node.data.endsWith('\n') ? 150 : 22);
      else { heroQ.classList.add('typed'); document.dispatchEvent(new Event('hero-typed')); }
    };
    setTimeout(tick, 420);
  }
}

/* ======================================================= counting a number up */

function countUp(el, value, unit, ms = 900) {
  el.classList.remove('idle');
  const dec = (value.split('.')[1] || '').length;
  const target = parseFloat(value.replace(/,/g, ''));
  const write = (v) => {
    const txt = v.toLocaleString(undefined, { minimumFractionDigits: dec, maximumFractionDigits: dec });
    el.innerHTML = unit ? `${txt}<small>${unit}</small>` : txt;
  };
  // Write the real value first. The HNSW build that follows holds the main
  // thread, and a requestAnimationFrame that never gets to run would leave the
  // stat showing a dash while its number was already known.
  write(target);
  if (still || !isFinite(target)) return;
  const t0 = performance.now();
  let settled = false;
  const land = () => { if (!settled) { settled = true; write(target); } };
  const step = (now) => {
    if (settled) return;
    const k = Math.min((now - t0) / ms, 1);
    write(target * (1 - Math.pow(1 - k, 3)));
    if (k < 1) requestAnimationFrame(step);
    else land();
  };
  requestAnimationFrame(step);
  // A backgrounded tab throttles requestAnimationFrame to a crawl, which would
  // otherwise leave the counter parked on a number it was only passing through.
  setTimeout(land, ms + 150);
}

/* ============================================ the vector field visualisation */

function vectorField(canvas, onQuery) {
  /* The live run, played like a film of itself. The points are the real
     5 000 vectors' 2D shadow; each stage plays as the engine reaches it: rows
     stream in from the left, the graph links as the index builds, and then
     the queries the engine ran replay one after another -- each one entering
     the graph, walking it toward its answer, and lighting the ten rows the
     engine returned, with the time it took and how many of the ten an exact
     scan agrees with. It loops while in view and stops when it is not. */
  let ctx, w, h, pts = [], nb = [], edges = [], hubs = [], hubNb = new Map();
  let loaded = 0, linked = 0, queries = [], qi = 0, qt = 0, playing = false, inView = true;
  let last = 0, raf = 0;
  const HUES = ['#F4A93C', '#FF7A3D', '#E86A8A', '#B97BE0', '#4FE0C4', '#FFCE73'];
  const STEP = 0.12;            // seconds a hop of the walk takes
  const QUERY_FOR = 6.2;        // seconds each replayed query holds the stage

  const fit = () => { ({ ctx, w, h } = fitCanvas(canvas)); };
  fit();
  addEventListener('resize', () => { fit(); paint(); }, { passive: true });

  // Each point's two nearest in the shadow, found through a grid: the graph
  // the picture draws. The engine's own graph is in 128 dimensions; this is
  // how such a graph looks, not a copy of it.
  const link = () => {
    const G = 48, cells = new Map(), key = (x, y) => x * 1000 + y;
    pts.forEach((p, i) => {
      const k = key(Math.floor(p.x * G), Math.floor(p.y * G));
      (cells.get(k) || cells.set(k, []).get(k)).push(i);
    });
    nb = pts.map((p, i) => {
      const cx = Math.floor(p.x * G), cy = Math.floor(p.y * G), near = [];
      for (let dx = -1; dx <= 1; dx++) for (let dy = -1; dy <= 1; dy++) {
        for (const j of cells.get(key(cx + dx, cy + dy)) || []) {
          if (j !== i) near.push([(pts[j].x - p.x) ** 2 + (pts[j].y - p.y) ** 2, j]);
        }
      }
      near.sort((a, b) => a[0] - b[0]);
      return near.slice(0, 3).map(([, j]) => j);
    });
    // An upper layer, as HNSW keeps: one point in forty, each linked to its
    // four nearest of the others, so a walk crosses the field in long steps
    // and then closes in through the fine graph.
    hubs = pts.map((_, i) => i).filter((i) => i % 40 === 0);
    hubNb = new Map(hubs.map((i) => [i, hubs.filter((j) => j !== i)
      .map((j) => [(pts[j].x - pts[i].x) ** 2 + (pts[j].y - pts[i].y) ** 2, j])
      .sort((a, b) => a[0] - b[0]).slice(0, 4).map(([, j]) => j)]));
    edges = [];
    nb.forEach((list, i) => list.slice(0, 2).forEach((j) => { if (i < j || !nb[j].includes(i)) edges.push([i, j]); }));
    // Linked in the order the build sweeps them: left to right.
    edges.sort((a, b) => pts[a[0]].x - pts[b[0]].x);
  };

  // A query's walk: from a far entry, across the upper layer to whichever
  // neighbour is closer to the query, then down through the fine graph the
  // same way until none is closer; then to the nearest row the engine found.
  const walk = (q) => {
    const d = (i) => (pts[i].x - q.x) ** 2 + (pts[i].y - q.y) ** 2;
    let at = hubs[0];
    for (const h of hubs) if (d(h) > d(at)) at = h;
    const path = [at];
    const greedy = (next) => {
      for (let g = 0; g < 30; g++) {
        let best = at;
        for (const n of next(at)) if (d(n) < d(best)) best = n;
        if (best === at) return;
        path.push((at = best));
      }
    };
    greedy((i) => hubNb.get(i) || []);
    greedy((i) => nb[i]);
    if (q.idx.length && q.idx[0] !== at) path.push(q.idx[0]);
    return path;
  };

  const X = (i) => pts[i].x * w, Y = (i) => pts[i].y * h;

  const paint = () => {
    if (!ctx) return;
    ctx.clearRect(0, 0, w, h);
    const q = queries.length ? queries[qi] : null;
    // How far into its replay the current query is.
    const hops = q ? q.path.length : 0;
    const walked = q ? Math.max(0, (qt - 0.5) / STEP) : 0;
    const litFrom = 0.6 + hops * STEP;
    const fade = q ? 1 - Math.min(1, Math.max(0, (qt - (QUERY_FOR - 0.5)) / 0.5)) : 0;

    // The graph, as far as it is linked.
    if (linked > 0) {
      const n = Math.floor(edges.length * linked);
      ctx.globalAlpha = 0.16;
      ctx.strokeStyle = '#CDB894';
      ctx.lineWidth = 0.6;
      ctx.beginPath();
      for (let e = 0; e < n; e++) {
        const [a, b] = edges[e];
        ctx.moveTo(X(a), Y(a));
        ctx.lineTo(X(b), Y(b));
      }
      ctx.stroke();
    }

    // The rows: streaming in from the left as they are written.
    const lit = new Set();
    if (q) q.idx.forEach((i, k) => { if (qt > litFrom + k * 0.09) lit.add(i); });
    for (let i = 0; i < pts.length; i++) {
      const p = pts[i];
      const k = Math.min(1, Math.max(0, loaded * 1.25 - p.order * 0.25));
      if (k <= 0) continue;
      const e = 1 - Math.pow(1 - k, 3);
      const x = (p.x * e - 0.08 * (1 - e)) * w;
      const on = lit.has(i);
      ctx.globalAlpha = on ? fade : 0.5 * e;
      ctx.fillStyle = on ? '#FFF4DC' : HUES[p.c % HUES.length];
      ctx.beginPath();
      ctx.arc(x, p.y * h, on ? 3.6 : 1.55, 0, 7);
      ctx.fill();
    }

    if (q) {
      const qx = q.x * w, qy = q.y * h;
      const appear = Math.min(1, qt / 0.4) * fade;
      // The walk, a hop at a time.
      ctx.globalAlpha = 0.95 * fade;
      ctx.strokeStyle = '#4FE0C4';
      ctx.lineWidth = 2;
      ctx.beginPath();
      for (let k = 0; k < hops - 1 && k < walked; k++) {
        const a = q.path[k], b = q.path[k + 1];
        const f = Math.min(1, walked - k);
        ctx.moveTo(X(a), Y(a));
        ctx.lineTo(X(a) + (X(b) - X(a)) * f, Y(a) + (Y(b) - Y(a)) * f);
      }
      ctx.stroke();
      for (let k = 0; k < hops && k <= walked; k++) {
        ctx.globalAlpha = 0.9 * fade;
        ctx.fillStyle = '#4FE0C4';
        ctx.beginPath();
        ctx.arc(X(q.path[k]), Y(q.path[k]), 3.2, 0, 7);
        ctx.fill();
      }
      // The ten found, joined to the query.
      ctx.globalAlpha = 0.5 * fade;
      ctx.strokeStyle = '#FFCE73';
      ctx.lineWidth = 1;
      ctx.beginPath();
      for (const i of lit) { ctx.moveTo(qx, qy); ctx.lineTo(X(i), Y(i)); }
      ctx.stroke();
      for (const i of lit) {
        ctx.globalAlpha = 0.25 * fade;
        ctx.fillStyle = '#FFF4DC';
        ctx.beginPath();
        ctx.arc(X(i), Y(i), 9, 0, 7);
        ctx.fill();
      }
      // The query itself, arriving with a ring.
      if (qt < 1.2) {
        ctx.globalAlpha = (1 - qt / 1.2) * 0.7;
        ctx.strokeStyle = '#4FE0C4';
        ctx.lineWidth = 2;
        ctx.beginPath();
        ctx.arc(qx, qy, 6 + qt * 70, 0, 7);
        ctx.stroke();
      }
      ctx.globalAlpha = appear;
      ctx.fillStyle = '#4FE0C4';
      ctx.beginPath();
      ctx.arc(qx, qy, 5, 0, 7);
      ctx.fill();
      ctx.globalAlpha = 0.3 * appear;
      ctx.beginPath();
      ctx.arc(qx, qy, 14, 0, 7);
      ctx.fill();
    }
    ctx.globalAlpha = 1;
  };

  const frame = (now) => {
    raf = 0;
    const dt = Math.min(0.1, (now - last) / 1000);
    last = now;
    if (queries.length && playing) {
      const before = qt;
      qt += dt;
      if (before === 0) onQuery?.(queries[qi], qi, queries.length);
      if (qt >= QUERY_FOR) { qt = 0; qi = (qi + 1) % queries.length; }
    }
    paint();
    if (playing && inView && !document.hidden) raf = requestAnimationFrame(frame);
  };
  const run = () => { if (!raf && !still) { last = performance.now(); raf = requestAnimationFrame(frame); } };

  const animate = (ms, set) => new Promise((done) => {
    if (still) { set(1); paint(); done(); return; }
    const t0 = performance.now();
    const step = (now) => {
      const k = Math.min((now - t0) / ms, 1);
      set(1 - Math.pow(1 - k, 3));
      paint();
      if (k < 1) requestAnimationFrame(step); else done();
    };
    requestAnimationFrame(step);
  });

  if (typeof IntersectionObserver === 'function') {
    new IntersectionObserver((e) => {
      inView = e.some((x) => x.isIntersecting);
      if (inView && playing) run();
    }).observe(canvas);
  }
  addEventListener('visibilitychange', () => { if (!document.hidden && playing) run(); });

  return {
    /* `points` are already projected into the unit square. */
    seed(points) {
      pts = points;
      // Rows arrive left to right, a little out of order, as a stream does.
      pts.forEach((p, i) => { p.order = Math.min(1, p.x * 0.85 + ((i * 7919) % 100) / 100 * 0.15); });
      link();
      loaded = 0; linked = 0; queries = []; paint();
    },
    load(ms) { return animate(ms, (v) => { loaded = v; }); },
    build(ms) { return animate(ms, (v) => { linked = v; }); },
    /* Replays the engine's queries in turn, from the first. */
    play(list) {
      queries = list.filter((q) => q.idx.length).map((q) => ({ ...q, path: walk(q) }));
      qi = 0; qt = still ? QUERY_FOR - 1 : 0; playing = queries.length > 0;
      if (still) { paint(); if (queries.length) onQuery?.(queries[0], 0, queries.length); }
      else run();
    },
  };
}

/* ============================================================== the console */

const rig = document.getElementById('rig');
if (rig) {
  let started = false;
  const start = () => { if (!started) { started = true; runDemo(rig); } };
  if (typeof IntersectionObserver === 'function') {
    const watch = new IntersectionObserver((e) => {
      if (!e.some((x) => x.isIntersecting)) return;
      watch.disconnect();
      requestAnimationFrame(() => requestAnimationFrame(start));
    }, { rootMargin: '0px 0px -100px 0px' });
    watch.observe(rig);
  } else start();
}

async function runDemo(el) {
  const log = el.querySelector('.rig-log');
  const note = el.querySelector('.rig-note');
  const cap = el.querySelector('.field-cap');
  const steps = [...el.querySelectorAll('.rig-steps li')];
  // The strip under the picture: which stage the run is at, as a film's
  // chapters show where it is. The search stage counts the replayed queries.
  let reached = 0;
  const stage = (n, k = 1) => {
    if (n < reached) return;   // a stage's animation can end after the next began
    reached = n;
    steps.forEach((s, i) => {
      s.toggleAttribute('aria-current', i === n);
      s.style.setProperty('--k', i < n ? 1 : i === n ? k : 0);
    });
  };
  const field = vectorField(el.querySelector('.field'), (q, i, n) => {
    stage(3, (i + 1) / n);
    const agrees = q.found == null ? '' : `, ${q.found} of 10 as an exact scan finds them`;
    // A worker's clock steps by 0.1 ms unless the page is cross-origin
    // isolated: a query under a step reads as 0, which it was not.
    const took = q.ms < 0.1 ? 'under 0.1 ms' : `${q.ms.toFixed(2)} ms`;
    cap.textContent = `query ${i + 1} of ${n}: ${took}${agrees}`;
  });
  stage(0, 0.5);

  const lines = [];
  // The log is the page's to show or not; the home page shows the steps and
  // the numbers instead.
  const paint = () => { if (log) { log.innerHTML = lines.join('\n'); log.scrollTop = log.scrollHeight; } };
  const put = (k, v, unit) => {
    const dd = el.querySelector(`[data-stat="${k}"]`);
    if (!dd) return;
    countUp(dd, v, unit);
    const row = dd.closest('.stat');
    row.classList.remove('lit');
    void row.offsetWidth;
    row.classList.add('lit');
  };

  el.dataset.state = 'running';
  note.textContent = 'running in this tab';
  lines.push('<i>starting the engine in a worker</i>');
  paint();

  let worker;
  try {
    worker = new Worker(new URL('./engine-worker.js', import.meta.url), { type: 'module' });
  } catch {
    return offline(el, field, 'this browser cannot start a module worker');
  }
  worker.onerror = () => offline(el, field, 'the engine worker failed to load');

  worker.onmessage = async (e) => {
    const m = e.data;
    switch (m.t) {
      case 'log': lines.push(m.html); paint(); break;
      case 'amend': lines[lines.length - 1] += m.html; paint(); break;
      case 'stat': put(m.k, m.v, m.unit); break;
      case 'points': {
        const pts = new Array(m.n);
        for (let i = 0; i < m.n; i++) {
          pts[i] = { x: m.xy[i * 2], y: m.xy[i * 2 + 1], nx: m.xy[i * 2], c: m.cl[i] };
        }
        field.seed(pts);
        stage(0);
        cap.textContent = `${m.n.toLocaleString()} vectors of 128 dimensions, their 2D shadow`;
        break;
      }
      case 'phase':
        if (m.name === 'scatter') { stage(1, 0.5); cap.textContent = 'writing the rows'; field.load(1600).then(() => stage(1)); }
        if (m.name === 'build') { stage(2, 0.5); cap.textContent = 'linking the hnsw graph'; field.build(1400).then(() => stage(2)); }
        break;
      case 'queries':
        field.play(m.list);
        break;
      case 'done':
        el.dataset.state = 'done';
        note.textContent = m.note;
        worker.terminate();
        break;
      case 'failed':
        worker.terminate();
        offline(el, field, m.message);
        break;
    }
  };

  worker.postMessage({ cmd: 'demo' });
}

/* Without the engine the panel still has something true to show: the numbers
   the benchmark harness measured, clearly labelled as such. */
function offline(el, field, reason) {
  const log = el.querySelector('.rig-log');
  el.dataset.state = 'failed';
  el.querySelector('.rig-note').textContent = 'published measurements';
  el.querySelector('.field-cap').textContent = 'the live run did not start';
  if (log) log.innerHTML = [
    `<i>the live run stopped: ${escapeHtml(reason)}</i>`,
    '<i>WebAssembly needs an http origin — a page opened from disk cannot</i>',
    '<i>stream the module. the numbers beside this are the measured ones</i>',
    '<i>from the benchmark harness: Apple M-series, 100 000 × 128.</i>',
  ].join('\n');
  const put = (k, v, u) => {
    const dd = el.querySelector(`[data-stat="${k}"]`);
    if (dd) countUp(dd, v, u);
  };
  put('boot', '110', 'ms');
  put('rows', '100000', ' × 128');
  put('build', '10.2', 's');
  put('query', '0.139', 'ms');
  put('recall', '100', '%');
}

/* ============================================================== the race */

const race = document.querySelector('.race');
if (race) {
  const lanes = [...race.querySelectorAll('.lane')];
  const run = () => {
    for (const lane of lanes) {
      const ms = +lane.dataset.ms;
      const fill = lane.querySelector('.lane-fill');
      const out = lane.querySelector('.lane-time');
      lane.classList.remove('done');
      // The scale is compressed -- 16 times as long would be 11 s to watch --
      // but not so far that the gap stops showing. The label is the number.
      const fastest = Math.min(...lanes.map((l) => +l.dataset.ms));
      const dur = still ? 0 : 700 * Math.pow(ms / fastest, 0.6);
      fill.style.transition = 'none';
      fill.style.right = '100%';
      void lane.offsetWidth;
      fill.style.transition = `right ${dur}ms cubic-bezier(.3,.05,.2,1)`;
      fill.style.right = '0%';
      setTimeout(() => {
        lane.classList.add('done');
        countUp(out, lane.dataset.value, lane.dataset.unit, 420);
      }, dur + 40);
      out.classList.add('idle');
      out.textContent = '—';
    }
  };

  const watch = new IntersectionObserver((e) => {
    if (!e.some((x) => x.isIntersecting)) return;
    watch.disconnect();
    run();
  }, { rootMargin: '0px 0px -18% 0px' });
  watch.observe(race);
  race.parentElement.querySelector('.race-replay')?.addEventListener('click', run);
}

/* ================================================================== maths */

function escapeHtml(s) {
  return s.replace(/[&<>]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c]));
}


/* ============================================================= playground */

const pg = document.getElementById('pg');
if (pg) playground(pg);

function playground(el) {
  const sqlBox = el.querySelector('#pg-sql');
  const runBtn = el.querySelector('#pg-run');
  const status = el.querySelector('#pg-status');
  const out = el.querySelector('#pg-out');
  const schemaBox = el.querySelector('#pg-schema');

  const EXAMPLES = [
    ['a filter', 'get notes select body, topic, year\n  where year >= 2023 and topic = "vectors"\n  limit 10'],
    ['counting', 'get notes where year >= 2023 count'],
    ['nearest ten', 'get notes select body, topic\n  near embed $1\n  limit 10'],
    ['filter + vector', 'get notes select body, year\n  where topic = "storage"\n  near embed $1\n  limit 5'],
    ['text contains', 'get notes select body where body ~ "sync" limit 10'],
    ['ordering', 'get notes select body, year order year desc, body asc limit 10'],
    ['describe', 'describe notes'],
    ['write one', 'put notes {body: "a note of my own", topic: "wasm", year: 2026}'],
  ];
  el.querySelector('#pg-egs').innerHTML = EXAMPLES
    .map(([name], i) => `<li><button type="button" data-eg="${i}">${name}</button></li>`).join('');
  el.querySelector('#pg-egs').addEventListener('click', (e) => {
    const b = e.target.closest('button[data-eg]');
    if (!b) return;
    sqlBox.value = EXAMPLES[+b.dataset.eg][1];
    sqlBox.focus();
    run();
  });

  let worker, seq = 0, pending = new Map(), probe = null;
  const say = (text, cls = '') => { status.className = 'pg-status ' + cls; status.textContent = text; };
  const ask = (cmd, extra = {}) => new Promise((resolve, reject) => {
    const id = ++seq;
    pending.set(id, { resolve, reject });
    worker.postMessage({ cmd, id, ...extra });
  });

  try {
    worker = new Worker(new URL('./engine-worker.js', import.meta.url), { type: 'module' });
  } catch {
    el.dataset.state = 'failed';
    say('this browser cannot start a module worker', 'err');
    return;
  }
  worker.onerror = () => { el.dataset.state = 'failed'; say('the engine worker failed to load', 'err'); };
  worker.onmessage = (e) => {
    const m = e.data;
    if (m.t === 'schema') return drawSchema(m.schema);
    if (m.t !== 'result') return;
    const p = pending.get(m.id);
    pending.delete(m.id);
    if (!p) return;
    if (m.error) p.reject(new Error(m.error));
    else p.resolve(m);
  };

  (async () => {
    try {
      const o = await ask('open');
      say(`engine ready in ${o.ms.toFixed(0)} ms · seeding…`);
      const s = await ask('seed', { n: 400, dim: 64 });
      // A query vector that actually sits in the data, so `near` means something.
      probe = null;
      el.dataset.state = 'ready';
      runBtn.disabled = false;
      say(`${s.n} notes × ${s.dim} dims seeded in ${s.ms.toFixed(0)} ms`, 'ok');
      out.innerHTML = '<p class="pg-muted">Ready. Run the query above, or pick one on the left.</p>';
    } catch (err) {
      el.dataset.state = 'failed';
      say(String(err.message || err), 'err');
    }
  })();

  function drawSchema(list) {
    if (!list || !list.length) { schemaBox.innerHTML = '<p class="pg-muted">no collections</p>'; return; }
    schemaBox.innerHTML = list.map((c) => `
      <div><span class="pg-coll">${escapeHtml(c.name)}</span>
        <ul class="pg-fields">${(c.fields || []).map((f) => `<li><b>${escapeHtml(String(f.name ?? ''))}</b>
          ${escapeHtml(String(f.type ?? ''))}${f.index && f.index !== 'none'
            ? `<i>@${escapeHtml(String(f.index))}</i>` : ''}</li>`).join('')}
        </ul></div>`).join('');
  }

  /* `near` needs a vector. Rather than make the visitor paste 64 numbers, a
     row's own embedding is read back and bound to $1. */
  async function vectorParam() {
    if (probe) return probe;
    const r = await ask('exec', { sql: 'get notes select embed limit 1' });
    probe = r.rows && r.rows[0] ? Object.values(r.rows[0])[0] : null;
    return probe;
  }

  async function run() {
    if (runBtn.disabled) return;
    const sql = sqlBox.value.trim();
    if (!sql) return;
    runBtn.disabled = true;
    el.dataset.state = 'busy';
    say('running…');
    try {
      const params = /\$1/.test(sql) ? [await vectorParam()] : [];
      const r = await ask('exec', { sql, params });
      draw(r);
      say(`${r.ms.toFixed(3)} ms`, 'ok');
    } catch (err) {
      out.innerHTML = `<p class="pg-err">${escapeHtml(String(err.message || err))}</p>`;
      say('query error', 'err');
    } finally {
      runBtn.disabled = false;
      el.dataset.state = 'ready';
    }
  }

  function draw(r) {
    if (r.kind === 'schemas') return drawTable(
      ['collection', 'field', 'type', 'index'],
      (r.collections || []).flatMap((c) => (c.fields || []).map((f, i) => ({
        collection: i ? '' : c.name, field: f.name, type: f.type,
        index: f.index && f.index !== 'none' ? '@' + f.index : '',
      }))));

    if (r.rows && r.rows.length) {
      const cols = r.columns && r.columns.length ? r.columns : Object.keys(r.rows[0]);
      return drawTable(cols, r.rows);
    }
    if (r.rows) return void (out.innerHTML = '<p class="pg-note">0 rows</p>');
    if (r.count != null) {
      out.innerHTML = `<p class="pg-note">${r.count} ${r.count === 1 ? 'row' : 'rows'} affected</p>`;
      return;
    }
    out.innerHTML = '<p class="pg-note">ok</p>';
  }

  function drawTable(cols, rows) {
    if (!rows.length) { out.innerHTML = '<p class="pg-note">0 rows</p>'; return; }
    const cell = (v) => {
      if (v == null) return '<td class="pg-muted">null</td>';
      if (Array.isArray(v)) {
        return `<td class="vec">[${v.slice(0, 3).map((n) => (+n).toFixed(3)).join(', ')}` +
               `${v.length > 3 ? `, … ${v.length} values` : ''}]</td>`;
      }
      if (typeof v === 'number') return `<td class="num">${v}</td>`;
      return `<td>${escapeHtml(String(v))}</td>`;
    };
    out.innerHTML = `<table><thead><tr>${cols.map((c) => `<th>${escapeHtml(c)}</th>`).join('')}</tr></thead>
      <tbody>${rows.map((row) => `<tr>${cols.map((c) => cell(row[c])).join('')}</tr>`).join('')}</tbody></table>`;
  }

  runBtn.addEventListener('click', run);
  sqlBox.addEventListener('keydown', (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') { e.preventDefault(); run(); }
  });
}

/* ============================================================ the screencast */

/* A recorded session, played back as text: the commands are typed, their
   output lands a line at a time. Text rather than a video file -- a few
   hundred bytes instead of megabytes, sharp at any size, and selectable.
   Without script the transcript underneath is the page. */
const cast = document.getElementById('cast');
if (cast) {
  const out = cast.querySelector('.cast-out');
  const bar = cast.querySelector('.cast-chapters');
  const btn = cast.querySelector('.cast-play');
  const PROMPT = /^(\$ |fenec[=-]# )/;

  const chapters = [...cast.querySelectorAll('.cast-script li')].map((li) => {
    const lines = li.querySelector('pre').textContent.replace(/\n$/, '').split('\n');
    const steps = [];
    let typing = false;
    for (const line of lines) {
      const m = line.match(PROMPT);
      if (m) steps.push({ prompt: m[0], cmd: line.slice(m[0].length) });
      else if (typing) steps.push({ prompt: '', cmd: line });
      else steps.push({ text: line });
      typing = (m || typing) && /\\$/.test(line);
    }
    return { title: li.dataset.title, steps };
  });

  bar.innerHTML = chapters.map((c, i) =>
    `<li><button type="button" data-i="${i}"><span class="cast-fill"></span>${escapeHtml(c.title)}</button></li>`).join('');
  const fills = [...bar.querySelectorAll('.cast-fill')];
  const buttons = [...bar.querySelectorAll('button')];
  cast.classList.add('on');

  let at = 0, playing = false, run = 0;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const line = (cls) => { const s = document.createElement('span'); s.className = cls; out.append(s); return s; };
  const mark = (i, k) => {
    fills.forEach((f, j) => { f.style.transform = `scaleX(${j < i ? 1 : j === i ? k : 0})`; });
    buttons.forEach((b, j) => b.toggleAttribute('aria-current', j === i));
  };

  // Shows chapter `i` whole, at once: reduced motion, or a jump while paused.
  const show = (i) => {
    out.textContent = '';
    for (const s of chapters[i].steps) {
      if (s.text !== undefined) line('o').textContent = s.text + '\n';
      else { line('p').textContent = s.prompt; line('c').textContent = s.cmd + '\n'; }
    }
    mark(i, 1);
  };

  const play = async (i) => {
    const me = ++run;
    const alive = () => me === run && playing;
    for (; ; i = (i + 1) % chapters.length) {
      at = i;
      out.textContent = '';
      const { steps } = chapters[i];
      for (let k = 0; k < steps.length; k++) {
        const s = steps[k];
        if (s.text !== undefined) {
          line('o').textContent = s.text + '\n';
          await sleep(45);
        } else {
          line('p').textContent = s.prompt;
          const c = line('c');
          for (let j = 0; j < s.cmd.length; j += 2) {
            c.textContent = s.cmd.slice(0, j + 2);
            await sleep(26);
            if (!alive()) return;
          }
          c.textContent = s.cmd + '\n';
          await sleep(s.cmd.endsWith('\\') ? 120 : 520);
        }
        if (!alive()) return;
        out.scrollTop = out.scrollHeight;
        mark(i, (k + 1) / steps.length);
      }
      await sleep(2600);
      if (!alive()) return;
    }
  };

  const set = (on) => {
    playing = on;
    cast.classList.toggle('paused', !on);
    btn.setAttribute('aria-label', on ? 'Pause' : 'Play');
    if (on) play(at); else run++;
  };
  btn.addEventListener('click', () => set(!playing));
  bar.addEventListener('click', (e) => {
    const b = e.target.closest('button');
    if (!b) return;
    at = +b.dataset.i;
    if (playing) play(at); else show(at);
  });

  show(0);
  set(false);
  if (!still) {
    // Starts the first time it is in view, and stops while it is not.
    let started = false;
    new IntersectionObserver((e) => {
      const on = e.some((x) => x.isIntersecting);
      if (on && !started) { started = true; set(true); }
      else if (!on && playing) set(false), started = false;
    }, { threshold: 0.45 }).observe(cast);
  }
}

/* ========================================================== moving pictures */

/* Each section's scenes (`motion.js`) on a canvas of its own: they loop while
   the section is in view and stop when it is not. A scene holds its last
   frame a moment before the next begins, and with more than one the steps
   under the picture show which is playing and jump to another. */
const motions = [...document.querySelectorAll('.motion[data-scenes]')];
if (motions.length) {
  const HOLD = 2.2;
  let M = null;
  const load = () => (M ??= Promise.all([
    import('./motion.js'),
    // The canvas draws text: without the faces loaded it would draw it in
    // the fallback's metrics and never redraw.
    document.fonts.load('700 40px "Bricolage Grotesque"').catch(() => {}),
    document.fonts.load('500 20px "Bricolage Grotesque"').catch(() => {}),
    document.fonts.load('400 20px "IBM Plex Mono"').catch(() => {}),
  ]).then(([m]) => m));

  for (const fig of motions) {
    const canvas = fig.querySelector('canvas');
    const keys = fig.dataset.scenes.split(' ');
    let mod, at = 0, t = 0, playing = false, last = 0, steps = [], inView = false;

    const fit = () => {
      const dpr = Math.min(devicePixelRatio || 1, 2);
      const w = canvas.getBoundingClientRect().width;
      canvas.width = Math.round(w * dpr);
      canvas.height = Math.round(w * dpr * mod.FRAME.h / mod.FRAME.w);
    };
    // A scene plays to just before its fade, holds there, then fades out.
    const local = () => {
      const d = mod.SCENE[keys[at]].d;
      return t < d - 0.4 ? t : t < d - 0.4 + HOLD ? d - 0.41 : t - HOLD;
    };
    const draw = () => {
      mod.render(canvas.getContext('2d'), keys[at], local(), canvas.width, canvas.height);
      const d = mod.SCENE[keys[at]].d + HOLD;
      steps.forEach((b, i) => {
        b.toggleAttribute('aria-current', i === at);
        b.firstChild.style.transform = `scaleX(${i < at ? 1 : i === at ? Math.min(1, t / d) : 0})`;
      });
    };
    const frame = (now) => {
      if (!playing) return;
      t += Math.min(0.1, (now - last) / 1000);
      last = now;
      if (t >= mod.SCENE[keys[at]].d + HOLD) { t = 0; at = (at + 1) % keys.length; }
      draw();
      requestAnimationFrame(frame);
    };
    const set = (on) => {
      if (on === playing) return;
      playing = on;
      if (on) { last = performance.now(); requestAnimationFrame(frame); }
    };

    const init = () => load().then((m) => {
      mod = m;
      if (keys.length > 1) {
        const ol = document.createElement('ol');
        ol.className = 'motion-steps';
        ol.innerHTML = keys.map((k, i) =>
          `<li><button type="button" data-i="${i}"><span></span>${escapeHtml(mod.SCENE[k].title)}</button></li>`).join('');
        fig.append(ol);
        steps = [...ol.querySelectorAll('button')];
        ol.addEventListener('click', (e) => {
          const b = e.target.closest('button');
          if (!b) return;
          at = +b.dataset.i; t = still ? mod.SCENE[keys[at]].d - 0.5 : 0;
          draw();
        });
      }
      fit();
      // Still: each scene as it ends, whole, and nothing moves.
      if (still) t = mod.SCENE[keys[0]].d - 0.5;
      draw();
      addEventListener('resize', () => { fit(); draw(); }, { passive: true });
      if (still) return;
      new IntersectionObserver((e) => {
        inView = e.some((x) => x.isIntersecting);
        set(inView && !document.hidden);
      }, { threshold: 0.35 }).observe(canvas);
      addEventListener('visibilitychange', () => set(inView && !document.hidden));
    });
    // Loaded as the section comes near, not with the page.
    const near = new IntersectionObserver((e) => {
      if (!e.some((x) => x.isIntersecting)) return;
      near.disconnect();
      init();
    }, { rootMargin: '600px' });
    near.observe(fig);
  }
}
