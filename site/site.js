/* fenecdb — the dune field.
   Everything here is either the real engine running or a real measurement
   being drawn. No framework, no build step: the same constraint the database
   keeps. Motion is skipped wholesale when the visitor asks for less of it. */

const still = matchMedia('(prefers-reduced-motion: reduce)').matches;
const root = document.documentElement;

requestAnimationFrame(() => root.classList.add('loaded'));

/* ---------------------------------------------------------------- the menu */

/* Under 860 px the header is the mark and one button, which opens every
   header link and, on a docs page, the docs' nav, moved into the panel: one
   menu, where "Contents" beside a row of links overflowed a phone and the
   row was cut down by hiding links. While it is open the page behind is
   inert and does not scroll; Escape, a link followed, or the window
   widening past the breakpoint shuts it. */
const header = document.querySelector('.top');
const menuBtn = document.querySelector('.menu-toggle');
const menu = document.getElementById('menu');
const side = document.getElementById('side');
const sideInner = side?.querySelector('.side-inner');
const narrowNav = matchMedia('(max-width: 860px)');
if (header && menuBtn && menu) {
  const isOpen = () => header.classList.contains('open');
  const setOpen = (open, focusBack) => {
    header.classList.toggle('open', open);
    root.classList.toggle('menu-open', open);
    menuBtn.setAttribute('aria-expanded', String(open));
    for (const el of document.body.children) if (el !== header) el.inert = open;
    if (open) {
      // The page being read, in the middle of the panel and focused there.
      const here = menu.querySelector('.side-list a.here') || menu.querySelector('a[aria-current]');
      menu.scrollTop = here && here.closest('.side-inner')
        ? here.offsetTop - (menu.clientHeight - here.offsetHeight) / 2 : 0;
      (here || menu.querySelector('a'))?.focus({ preventScroll: true });
    } else if (focusBack) {
      menuBtn.focus();
    }
  };
  menuBtn.addEventListener('click', () => setOpen(!isOpen(), false));
  addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && isOpen()) { e.preventDefault(); setOpen(false, true); }
  });
  // A link followed shuts it: an anchor on this page would leave it over
  // what it scrolled to, and a page kept in the back/forward cache would
  // come back with it open.
  menu.addEventListener('click', (e) => { if (e.target.closest('a')) setOpen(false, false); });
  addEventListener('pageshow', (e) => { if (e.persisted && isOpen()) setOpen(false, false); });

  // The docs' nav is in the sidebar on a wide screen and in the menu on a
  // narrow one: the same links, never both.
  const place = () => {
    if (sideInner) (narrowNav.matches ? menu : side).append(sideInner);
    if (!narrowNav.matches && isOpen()) setOpen(false, false);
  };
  narrowNav.addEventListener('change', place);
  place();
}

/* Where the sidebar was scrolled to, for the next page to put back before it
   paints (SIDE_RESTORE in build.py): a link in it loads a page, and the
   sidebar began at its top again, the link just followed out of sight. */
if (side) {
  addEventListener('pagehide', () => {
    if (narrowNav.matches) return;
    try { sessionStorage.setItem('fenec-side', String(side.scrollTop)); } catch {}
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
    // A long table scrolls on its own: its entry is kept in view by the
    // table's scrollTop, never scrollIntoView, which would move the page.
    const toc = active.link.closest('.toc');
    const a = active.link;
    if (toc && toc.scrollHeight > toc.clientHeight) {
      const y = a.offsetTop; // the table is sticky, so it is the link's offsetParent
      if (y < toc.scrollTop + 40 || y + a.offsetHeight > toc.scrollTop + toc.clientHeight - 40) {
        toc.scrollTop = y - (toc.clientHeight - a.offsetHeight) / 2;
      }
    }
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

/* ========================================================= the fennec's ears */

/* The hero's rings are CSS, so they cost no script; this only stops them
   while the hero is off screen, where they would keep repainting a picture
   no one sees. */
const ears = document.querySelector('.ears');
if (ears && !still) {
  new IntersectionObserver((e) => {
    ears.classList.toggle('paused', !e.some((x) => x.isIntersecting));
  }).observe(ears);
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
  // The editor's colours come with their own module, after the page: until
  // then the textarea is plain text, laid out as it will be coloured.
  let hl = null, redraw = () => {};
  import('./highlight.js').then((m) => { hl = m; redraw = m.overlay(sqlBox); }, () => {});

  el.querySelector('#pg-egs').addEventListener('click', (e) => {
    const b = e.target.closest('button[data-eg]');
    if (!b) return;
    sqlBox.value = EXAMPLES[+b.dataset.eg][1];
    redraw();
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
      const msg = String(err.message || err);
      out.innerHTML = `<p class="pg-err">${hl ? hl.quoted(msg) : escapeHtml(msg)}</p>`;
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

/* ============================================================== sessions */

/* One small session in each language, played as text: what is set up shows
   at once, the statements are typed, then the answer lands. Text rather than
   a video -- a few hundred bytes each, sharp at any size, and selectable. It
   goes through the languages on its own, each fading into the next, and a
   click on a mark picks one and stays there. Without script the list under
   it is the section. */
const sessions = document.getElementById('sessions');
if (sessions) {
  const marks = sessions.querySelector('.session-marks');
  const out = sessions.querySelector('.session-out');
  const title = sessions.querySelector('.cast-title');
  const rows = sessions.querySelector('.session-rows');
  const CPS = 140;          // characters a second, typed
  const LINE = 0.18;        // the pause at the end of a typed line
  const LANDS = 0.35;       // the pause before output lands
  const HOLD = 3.6;         // the whole session, held before the next
  const FADE = 0.35;

  // A highlighted <pre> as runs of [class, text], so a prefix of it can be
  // drawn with its colours.
  const runs = (pre) => pre ? [...pre.childNodes].map((n) => [n.nodeType === 1 ? n.className : '', n.textContent]) : [];
  const html = (rs) => rs.map(([c, s]) => c ? `<span class="${c}">${escapeHtml(s)}</span>` : escapeHtml(s)).join('');

  const list = [...sessions.querySelectorAll('.session-script > li')].map((li) => {
    const shell = li.hasAttribute('data-shell');
    const typed = runs(li.querySelector('.typed'));
    // When each character of the typed text is drawn: a shell's output
    // lines land whole, a pause after the command; the rest is typed.
    const text = typed.map(([, s]) => s).join('');
    const at = new Float32Array(text.length + 1);
    let t = 0.5, start = 0, typing = false;
    for (const ln of text.split('\n')) {
      const cmd = !shell || ln.startsWith('$ ') || typing;
      typing = shell && cmd && ln.endsWith('\\');
      if (!cmd) t += LANDS;
      for (let i = 0; i <= ln.length; i++) {
        if (cmd && i < ln.length) t += 1 / CPS;
        at[start + i] = t;
      }
      if (cmd) t += LINE;
      start += ln.length + 1;
    }
    const answer = shell || li.dataset.group ? null : rows.textContent.replace(/^\n/, '');
    return {
      li, typed, at, shell,
      name: li.dataset.name, mark: li.dataset.mark, file: li.dataset.file, group: li.dataset.group || '',
      given: html(runs(li.querySelector('.given'))),
      answer, answerAt: t + LANDS, end: t + LANDS + HOLD,
    };
  });
  // The cycle goes through the languages; the frameworks and the imports
  // play when picked.
  const cycle = list.filter((s) => !s.group).length;

  let group = '';
  marks.innerHTML = list.map((s, i) => {
    const head = s.group !== group ? `<span class="session-group">${escapeHtml((group = s.group))}</span>` : '';
    return `${head}<button type="button" data-i="${i}"><span class="glyph">${escapeHtml(s.mark)}</span>${escapeHtml(s.name)}<span class="fill"></span></button>`;
  }).join('');
  const buttons = [...marks.querySelectorAll('button')];
  sessions.classList.add('on');

  // The panel takes each session's whole height as it begins, while it is
  // faded out, and holds it while it types, so nothing below moves as the
  // lines come in. Held at the tallest one, PHP's helper left a phone's
  // panel two thirds empty for every other language.
  const draw = (s, t) => {
    let n = 0;
    while (n < s.at.length - 1 && s.at[n] <= t) n++;
    let left = n, typed = '';
    for (const [c, txt] of s.typed) {
      if (left <= 0) break;
      const part = txt.slice(0, left);
      left -= part.length;
      typed += c ? `<span class="${c}">${escapeHtml(part)}</span>` : escapeHtml(part);
    }
    const done = t >= s.answerAt;
    out.innerHTML = (s.given ? `<span class="given">${s.given}</span>\n\n` : '') + typed +
      (n < s.at.length - 1 || !s.answer ? '<span class="caret"></span>' : '') +
      (s.answer && done ? `\n\n<span class="answer">${escapeHtml(s.answer)}</span>` : '');
  };
  let heights = [];
  const fit = () => {
    out.style.height = '';
    heights = list.map((s) => { draw(s, Infinity); return out.scrollHeight; });
    shown = -1;
  };

  let at = 0, t = 0, auto = !still, playing = false, last = 0, inView = false, shown = -1;
  const show = () => {
    const s = list[at];
    if (shown !== at) {
      shown = at;
      out.style.height = `${heights[at]}px`;
      title.textContent = s.file;
      buttons.forEach((b, j) => b.setAttribute('aria-pressed', String(j === at)));
      // Keep the mark in sight in its own row, without moving the page.
      const b = buttons[at];
      const left = b.offsetLeft - (marks.clientWidth - b.offsetWidth) / 2;
      marks.scrollTo({ left: Math.max(0, left), behavior: still ? 'auto' : 'smooth' });
    }
    draw(s, still ? Infinity : t);
    out.style.opacity = still ? 1 : Math.min(1, t / FADE, auto ? (s.end - t) / FADE + 1 : 1);
    buttons.forEach((b, j) => {
      b.lastChild.style.transform = `scaleX(${j === at ? Math.min(1, t / s.end) : 0})`;
    });
  };
  const frame = (now) => {
    if (!playing) return;
    t += Math.min(0.1, (now - last) / 1000);
    last = now;
    if (auto && t >= list[at].end + FADE) { at = (at + 1) % cycle; t = 0; }
    show();
    requestAnimationFrame(frame);
  };
  const set = (on) => {
    if (on === playing || still) return;
    playing = on;
    if (on) { last = performance.now(); requestAnimationFrame(frame); }
  };
  marks.addEventListener('click', (e) => {
    const b = e.target.closest('button');
    if (!b) return;
    at = +b.dataset.i; t = 0; auto = false;
    show();
  });

  fit();
  show();
  addEventListener('resize', () => { fit(); show(); }, { passive: true });
  if (!still) {
    new IntersectionObserver((e) => {
      inView = e.some((x) => x.isIntersecting);
      set(inView && !document.hidden);
    }, { threshold: 0.35 }).observe(sessions);
    addEventListener('visibilitychange', () => set(inView && !document.hidden));
  }
}

/* ========================================================== moving pictures */

/* Each section's scenes (`motion.js`) on a canvas of its own: they play while
   the section is in view and the tab is shown, and stop otherwise. A story
   holds its last frame a moment, still moving where it moves, and fades into
   its start again; a stream runs on and never starts again. With more than
   one scene the steps under the picture show which is playing and jump to
   another. */
const motions = [...document.querySelectorAll('.motion[data-scenes]')];
if (motions.length) {
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
    // `t` is the scene's own time; `clock` the figure's, which runs on
    // across a scene's start so nothing drawn by it jumps there.
    let mod, at = 0, t = 0, clock = 0, playing = false, last = 0, steps = [], inView = false;

    // A phone gets the scene's narrow stage: the wide one scaled down to a
    // phone turned its words to specks.
    const narrow = matchMedia('(max-width: 600px)');
    const fit = () => {
      const dpr = Math.min(devicePixelRatio || 1, 2);
      const { w: sw, h: sh } = mod.stage(keys[at], narrow.matches);
      canvas.style.aspectRatio = `${sw} / ${sh}`;
      const w = canvas.getBoundingClientRect().width;
      canvas.width = Math.round(w * dpr);
      canvas.height = Math.round(w * dpr * sh / sw);
    };
    const draw = () => {
      mod.render(canvas.getContext('2d'), keys[at], t, canvas.width, canvas.height, narrow.matches, clock);
      const d = mod.cycle(keys[at]);
      steps.forEach((b, i) => {
        b.toggleAttribute('aria-current', i === at);
        b.firstChild.style.transform = `scaleX(${i < at ? 1 : i === at ? Math.min(1, t / d) : 0})`;
      });
    };
    const frame = (now) => {
      if (!playing) return;
      const dt = Math.min(0.1, (now - last) / 1000);
      last = now;
      t += dt; clock += dt;
      if (t >= mod.cycle(keys[at])) {
        t = 0;
        if (keys.length > 1) { at = (at + 1) % keys.length; fit(); }
      }
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
          at = +b.dataset.i; t = still ? mod.still(keys[at]) : 0;
          fit(); draw();
        });
      }
      fit();
      // Still: the frame that says everything the scene does, and nothing moves.
      if (still) t = clock = mod.still(keys[0]);
      draw();
      addEventListener('resize', () => { fit(); draw(); }, { passive: true });
      narrow.addEventListener('change', () => { fit(); draw(); });
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

/* ============================================================ language tabs */

/* A block of examples, one per language: a tab for each, the reader's
   choice kept for the next page and the next visit. Without script every
   example shows, one under another, each under its name. */
for (const box of document.querySelectorAll('[data-langs]')) {
  const panels = [...box.querySelectorAll(':scope > .lang')];
  const bar = document.createElement('div');
  bar.className = 'lang-tabs';
  bar.setAttribute('role', 'tablist');
  bar.setAttribute('aria-label', 'Language');
  const tabs = panels.map((p, i) => {
    const b = document.createElement('button');
    b.type = 'button';
    b.setAttribute('role', 'tab');
    b.textContent = p.dataset.name;
    b.addEventListener('click', () => show(i, true));
    bar.append(b);
    return b;
  });
  const show = (i, keep) => {
    panels.forEach((p, j) => { p.hidden = j !== i; });
    tabs.forEach((b, j) => b.setAttribute('aria-selected', String(j === i)));
    if (keep) { try { localStorage.setItem('fenec-lang', panels[i].dataset.name); } catch {} }
  };
  box.prepend(bar);
  box.classList.add('on');
  let start = 0;
  try {
    const kept = localStorage.getItem('fenec-lang');
    const at = panels.findIndex((p) => p.dataset.name === kept);
    if (at >= 0) start = at;
  } catch {}
  show(start, false);
}
