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
      else heroQ.classList.add('typed');
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

function vectorField(canvas) {
  let ctx, w, h, pts = [], edgesTo = null, q = null, lit = new Set();
  let grown = 0, sweep = -1, ring = -1, raf = 0;

  const HUES = ['#F4A93C', '#FF7A3D', '#E86A8A', '#B97BE0', '#4FE0C4', '#FFCE73'];

  const fit = () => {
    ({ ctx, w, h } = fitCanvas(canvas));
  };
  fit();
  addEventListener('resize', () => { fit(); }, { passive: true });

  const draw = () => {
    raf = 0;
    if (!ctx) return;
    ctx.clearRect(0, 0, w, h);

    const n = Math.floor(pts.length * grown);

    // Grains
    for (let i = 0; i < n; i++) {
      const p = pts[i];
      const on = lit.has(i);
      const near = sweep >= 0 ? 1 - Math.min(Math.abs(p.nx - sweep) * 7, 1) : 0;
      ctx.globalAlpha = on ? 1 : 0.46 + near * 0.5;
      ctx.fillStyle = on ? '#FFF4DC' : HUES[p.c % HUES.length];
      const r = (on ? 3.4 : 1.55) + near * 1.2;
      ctx.beginPath();
      ctx.arc(p.x * w, p.y * h, r, 0, 7);
      ctx.fill();
      if (on) {
        ctx.globalAlpha = 0.28;
        ctx.beginPath();
        ctx.arc(p.x * w, p.y * h, 9, 0, 7);
        ctx.fill();
      }
    }

    // The query point, its reach, and the neighbours it found
    if (q) {
      ctx.globalAlpha = 0.5;
      ctx.strokeStyle = '#FFCE73';
      ctx.lineWidth = 1;
      for (const i of lit) {
        ctx.beginPath();
        ctx.moveTo(q.x * w, q.y * h);
        ctx.lineTo(pts[i].x * w, pts[i].y * h);
        ctx.stroke();
      }
      if (ring >= 0 && ring < 1) {
        ctx.globalAlpha = (1 - ring) * 0.75;
        ctx.strokeStyle = '#4FE0C4';
        ctx.lineWidth = 2;
        ctx.beginPath();
        ctx.arc(q.x * w, q.y * h, ring * Math.max(w, h) * 0.55, 0, 7);
        ctx.stroke();
      }
      ctx.globalAlpha = 1;
      ctx.fillStyle = '#4FE0C4';
      ctx.beginPath();
      ctx.arc(q.x * w, q.y * h, 4.5, 0, 7);
      ctx.fill();
      ctx.globalAlpha = 0.35;
      ctx.beginPath();
      ctx.arc(q.x * w, q.y * h, 13, 0, 7);
      ctx.fill();
    }
    ctx.globalAlpha = 1;
  };

  const invalidate = () => { if (!raf) raf = requestAnimationFrame(draw); };

  const animate = (from, to, ms, set) => new Promise((done) => {
    if (still) { set(to); invalidate(); done(); return; }
    const t0 = performance.now();
    const step = (now) => {
      const k = Math.min((now - t0) / ms, 1);
      set(from + (to - from) * (1 - Math.pow(1 - k, 3)));
      invalidate();
      if (k < 1) requestAnimationFrame(step);
      else done();
    };
    requestAnimationFrame(step);
  });

  return {
    /* `points` are already projected into the unit square. */
    seed(points) { pts = points; grown = 0; lit = new Set(); q = null; invalidate(); },
    scatter(ms) { return animate(0, 1, ms, (v) => { grown = v; }); },
    /* A band of light crossing the field while the graph is built. */
    async build(ms) {
      await animate(-0.1, 1.1, ms, (v) => { sweep = v; });
      sweep = -1; invalidate();
    },
    async ask(point, neighbours) {
      q = point; lit = new Set(neighbours);
      await animate(0, 1, still ? 0 : 900, (v) => { ring = v; });
      ring = -1; invalidate();
    },
    clearQuery() { q = null; lit = new Set(); invalidate(); },
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
  const field = vectorField(el.querySelector('.field'));
  const stat = (k) => el.querySelector(`[data-stat="${k}"]`);

  const N = 2000, DIM = 128, CLUSTERS = 16, PROBES = 40;

  const lines = [];
  const say = (t, tag = '') => {
    lines.push(tag ? `<${tag}>${t}</${tag}>` : t);
    log.innerHTML = lines.join('\n');
    log.scrollTop = log.scrollHeight;
  };
  const amend = (t) => {
    lines[lines.length - 1] += t;
    log.innerHTML = lines.join('\n');
    log.scrollTop = log.scrollHeight;
  };
  const put = (k, v, unit) => {
    const dd = stat(k);
    if (!dd) return;
    countUp(dd, v, unit);
    const row = dd.closest('.stat');
    row.classList.remove('lit');
    void row.offsetWidth;
    row.classList.add('lit');
  };
  const breathe = (ms = 16) => new Promise((r) => setTimeout(r, ms));

  el.dataset.state = 'running';
  note.textContent = 'running in this tab';

  let Fenec;
  try { ({ Fenec } = await import('./fenec.js')); }
  catch { return offline('the client module could not be loaded'); }

  try {
    say('<i>booting fenec.wasm</i> ');
    const t0 = performance.now();
    const db = await Fenec.open('./fenec.wasm');
    const boot = performance.now() - t0;
    amend(`<b>ok</b> <i>in ${boot.toFixed(0)} ms</i>`);
    put('boot', boot.toFixed(0), 'ms');
    await breathe();

    say('');
    say('<em>create collection</em> notes (body text, topic int @hash,');
    say('       embed <span style="color:#C79BF2">vector&lt;128&gt;</span>)');
    db.run(`create collection notes (body text, topic int @hash, embed vector<${DIM}>)`);
    await breathe();

    // Clustered, the way an embedding model actually outputs. Uniformly random
    // vectors are the pathological case for any ANN index and would
    // misrepresent recall in both directions.
    say('');
    say(`<i>generating ${N.toLocaleString()} vectors in ${CLUSTERS} clusters</i> `);
    await breathe();
    const rand = mulberry32(0x5eed);
    const centres = Array.from({ length: CLUSTERS }, () => unit(DIM, rand));
    const docs = new Array(N);
    for (let i = 0; i < N; i++) {
      const c = i % CLUSTERS;
      docs[i] = { body: `note ${i}`, topic: c, embed: jitter(centres[c], 0.55, rand) };
    }
    amend('<b>ok</b>');

    // A real 2D shadow of the 128-dimensional data: two fixed random
    // directions, orthonormalised. Nothing is laid out by hand.
    const project = makeProjection(DIM, mulberry32(0xd17e));
    const raw = docs.map((d) => project(d.embed));
    const pts = normalise(raw).map((p, i) => ({ ...p, c: i % CLUSTERS }));
    field.seed(pts);
    cap.textContent = `${N.toLocaleString()} vectors · projected to 2D`;
    await breathe();

    say('');
    say('<em>put</em> notes [ … ] ');
    const t1 = performance.now();
    const scattering = field.scatter(1400);
    for (let i = 0; i < N; i += 1000) {
      await db.from('notes').insert(docs.slice(i, i + 1000));
      amend('.');
      await breathe();
    }
    const write = performance.now() - t1;
    amend(` <b>${N.toLocaleString()} rows</b> <i>in ${write.toFixed(0)} ms</i>`);
    put('rows', String(N), ` × ${DIM}`);
    await scattering;

    say('');
    say('<i>the tab pauses here — the hnsw build is synchronous</i>');
    say('<em>create index on</em> notes (embed) <span style="color:#C79BF2">@hnsw</span>(cosine) ');
    cap.textContent = 'building the hnsw graph';
    await breathe(60);
    const t2 = performance.now();
    db.run('create index on notes (embed) @hnsw(cosine)');
    const build = performance.now() - t2;
    amend(`<b>ok</b> <i>in ${(build / 1000).toFixed(2)} s</i>`);
    put('build', (build / 1000).toFixed(2), 's');
    await field.build(900);

    say('');
    say('<em>get</em> notes <em>near</em> embed $1 <em>limit</em> 10');
    cap.textContent = 'the 10 nearest to the query, lit';

    const probes = Array.from({ length: PROBES }, () =>
      jitter(centres[Math.floor(rand() * CLUSTERS)], 0.55, rand));

    const times = [];
    let hit = 0, total = 0, last = null;
    for (let i = 0; i < PROBES; i++) {
      const t = performance.now();
      const ann = db.run('get notes select body near embed $1 limit 10', [probes[i]]);
      times.push(performance.now() - t);
      if (i % 5 === 0) {
        const exact = db.run('get notes select body near embed $1 exact limit 10', [probes[i]]);
        const truth = new Set(exact.rows.map((r) => r.body));
        hit += ann.rows.filter((r) => truth.has(r.body)).length;
        total += truth.size;
      }
      last = { probe: probes[i], rows: ann.rows };
    }
    times.sort((a, b) => a - b);
    const p50 = times[Math.floor(times.length / 2)];
    const recall = total ? (hit / total) * 100 : 0;
    amend(`  <b>10 rows</b> <i>· p50 ${p50.toFixed(3)} ms over ${PROBES} queries</i>`);
    put('query', p50.toFixed(3), 'ms');
    put('recall', recall.toFixed(recall === 100 ? 0 : 1), '%');

    // Draw the last query for real: its own point, its own ten neighbours.
    const qp = normaliseOne(project(last.probe), raw);
    const idx = last.rows
      .map((r) => Number(String(r.body).replace('note ', '')))
      .filter((i) => Number.isInteger(i) && i >= 0 && i < N);
    await field.ask(qp, idx);

    say('');
    say(`<i>recall@10 against an exact scan: </i><b>${recall.toFixed(recall === 100 ? 0 : 1)}%</b>`);
    say('<i>nothing left this tab. no server was contacted.</i>');

    el.dataset.state = 'done';
    note.textContent = `${N.toLocaleString()} × ${DIM}, clustered`;
    db.close?.();
  } catch (err) {
    offline(String(err && err.message ? err.message : err));
  }

  function offline(reason) {
    el.dataset.state = 'failed';
    note.textContent = 'published measurements';
    cap.textContent = 'the live run did not start';
    say('');
    say(`<i>the live run stopped: ${escapeHtml(reason)}</i>`);
    say('<i>WebAssembly needs an http origin — a page opened from disk cannot</i>');
    say('<i>stream the module. the numbers beside this are the measured ones</i>');
    say('<i>from the benchmark harness: Apple M-series, 100 000 × 128.</i>');
    put('boot', '110', 'ms');
    put('rows', '100000', ' × 128');
    put('build', '10.2', 's');
    put('query', '0.139', 'ms');
    put('recall', '100', '%');
  }
}

/* ============================================================== the race */

const race = document.querySelector('.race');
if (race) {
  const lanes = [...race.querySelectorAll('.lane')];
  const run = () => {
    for (const lane of lanes) {
      const ms = +lane.dataset.ms;
      const fill = lane.querySelector('.lane-fill');
      const runner = lane.querySelector('.lane-runner');
      const dust = lane.querySelector('.lane-dust');
      const out = lane.querySelector('.lane-time');
      lane.classList.remove('done');
      // The scale is compressed: at true ratio the slowest lane would take
      // nearly three minutes to cross. The label carries the real number.
      const fastest = Math.min(...lanes.map((l) => +l.dataset.ms));
      const dur = still ? 0 : 700 * Math.pow(ms / fastest, 0.28);
      for (const el of [fill, runner, dust]) {
        el.style.transition = 'none';
        if (el === fill) el.style.right = '100%';
        else el.style.left = '0';
      }
      void lane.offsetWidth;
      const ease = 'cubic-bezier(.3,.05,.2,1)';
      fill.style.transition = `right ${dur}ms ${ease}`;
      runner.style.transition = `left ${dur}ms ${ease}`;
      dust.style.transition = `left ${dur}ms ${ease}`;
      fill.style.right = '0%';
      runner.style.left = '100%';
      dust.style.left = '100%';
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

/* A seeded generator keeps every visitor's run comparable to every other's. */
function mulberry32(a) {
  return function () {
    a |= 0; a = (a + 0x6D2B79F5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function gauss(rand) {
  const u = Math.max(rand(), 1e-9), v = rand();
  return Math.sqrt(-2 * Math.log(u)) * Math.cos(2 * Math.PI * v);
}

function unit(dim, rand) {
  const v = new Array(dim);
  let n = 0;
  for (let i = 0; i < dim; i++) { const g = gauss(rand); v[i] = g; n += g * g; }
  n = Math.sqrt(n) || 1;
  for (let i = 0; i < dim; i++) v[i] /= n;
  return v;
}

function jitter(centre, spread, rand) {
  const d = centre.length, v = new Array(d);
  let n = 0;
  for (let i = 0; i < d; i++) {
    const g = centre[i] + gauss(rand) * spread / Math.sqrt(d);
    v[i] = g; n += g * g;
  }
  n = Math.sqrt(n) || 1;
  for (let i = 0; i < d; i++) v[i] /= n;
  return v;
}

/* Two random directions, Gram-Schmidt'ed: an honest linear projection of the
   real vectors rather than a layout invented for the picture. */
function makeProjection(dim, rand) {
  const a = unit(dim, rand);
  let b = unit(dim, rand);
  let dot = 0;
  for (let i = 0; i < dim; i++) dot += a[i] * b[i];
  let n = 0;
  for (let i = 0; i < dim; i++) { b[i] -= dot * a[i]; n += b[i] * b[i]; }
  n = Math.sqrt(n) || 1;
  for (let i = 0; i < dim; i++) b[i] /= n;
  return (v) => {
    let x = 0, y = 0;
    for (let i = 0; i < dim; i++) { x += v[i] * a[i]; y += v[i] * b[i]; }
    return { x, y };
  };
}

function bounds(raw) {
  let x0 = Infinity, x1 = -Infinity, y0 = Infinity, y1 = -Infinity;
  for (const p of raw) {
    if (p.x < x0) x0 = p.x; if (p.x > x1) x1 = p.x;
    if (p.y < y0) y0 = p.y; if (p.y > y1) y1 = p.y;
  }
  return { x0, x1, y0, y1 };
}

function normalise(raw) {
  const b = bounds(raw);
  const sx = (b.x1 - b.x0) || 1, sy = (b.y1 - b.y0) || 1;
  return raw.map((p) => ({
    x: 0.06 + ((p.x - b.x0) / sx) * 0.88,
    y: 0.08 + ((p.y - b.y0) / sy) * 0.8,
    nx: (p.x - b.x0) / sx,
  }));
}

function normaliseOne(p, raw) {
  const b = bounds(raw);
  const sx = (b.x1 - b.x0) || 1, sy = (b.y1 - b.y0) || 1;
  return {
    x: Math.min(Math.max(0.06 + ((p.x - b.x0) / sx) * 0.88, 0.02), 0.98),
    y: Math.min(Math.max(0.08 + ((p.y - b.y0) / sy) * 0.8, 0.02), 0.98),
  };
}
