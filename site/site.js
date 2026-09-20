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

  const lines = [];
  const paint = () => { log.innerHTML = lines.join('\n'); log.scrollTop = log.scrollHeight; };
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
        cap.textContent = `${m.n.toLocaleString()} vectors · projected to 2D`;
        break;
      }
      case 'phase':
        if (m.name === 'scatter') field.scatter(1600);
        if (m.name === 'build') { cap.textContent = 'building the hnsw graph'; field.build(1200); }
        break;
      case 'query':
        cap.textContent = 'the 10 nearest to the query, lit';
        field.ask({ x: m.x, y: m.y }, m.idx);
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
  log.innerHTML = [
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
      // The scale is compressed: at true ratio the slowest lane would take
      // nearly three minutes to cross. The label carries the real number.
      const fastest = Math.min(...lanes.map((l) => +l.dataset.ms));
      const dur = still ? 0 : 700 * Math.pow(ms / fastest, 0.28);
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
