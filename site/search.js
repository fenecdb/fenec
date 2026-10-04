/* Search, by fenecdb in the reader's tab.

   Loaded only when the search is opened (`site.js`): the module, the
   engine and the index arrive then and not before, so a page that is only
   read pays for the button alone. The index is an image the build wrote
   with the same engine (`search-index.mjs`), handed to `load`; every
   keystroke is a `match` over it, answered here, and nothing leaves the
   tab -- the only requests are the site's own files.

   Nothing from the index becomes markup: a heading and a snippet are put in
   as text, and only the spans `highlight()` and `snippet()` name are
   wrapped, each in a <mark> made here. */

import { Fenec } from './fenec.js';
import { search, grouped } from './search-query.js';

const at = (p) => new URL(p, import.meta.url);
const WASM = './fenec.wasm';
const INDEX = './search.fenec';
const CSS = './search.css';
// Where the index's URLs start: the site's root, beside this module.
const ROOT = at('./');

const css = new Promise((resolve) => {
  const link = document.createElement('link');
  link.rel = 'stylesheet';
  link.href = at(CSS).href;
  link.onload = link.onerror = resolve;
  document.head.append(link);
});

let ui = null;
let db = null;
let ready = null;
let section = null;
let options = [];
let active = -1;
let returnTo = null;
let openedAt = 0;
let firstShown = false;

/** Opens the dialog; `since` is when the reader asked, for the timing. */
export async function open(from, since = performance.now()) {
  returnTo = from && from.isConnected ? from : document.activeElement;
  await css;
  ui ??= build();
  openedAt = since;
  firstShown = false;
  if (!ui.dialog.open) {
    // The words typed last stay, selected; a part of the site chosen does not.
    section = null;
    document.documentElement.classList.add('search-open');
    ui.dialog.showModal();
  }
  ui.input.focus();
  ui.input.select();
  ready ??= load();
  ready.then(run, failed);
}

async function load() {
  status('Loading the search…');
  const t = performance.now();
  const [engine, image] = await Promise.all([
    Fenec.open(at(WASM).href),
    fetch(at(INDEX)).then(unpacked),
  ]);
  engine.load(image);
  db = engine;
  ui.dialog.dataset.loadMs = (performance.now() - t).toFixed(1);
}

/* The image is shipped gzipped (`build.py`), since the edge serves a file
   of no type it knows uncompressed; a server that decoded it on the way
   hands over the image itself, which starts with no gzip header. */
async function unpacked(r) {
  if (!r.ok) throw new Error(`the index answered ${r.status}`);
  const bytes = new Uint8Array(await r.arrayBuffer());
  if (bytes[0] !== 0x1f || bytes[1] !== 0x8b) return bytes;
  const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream('gzip'));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

function failed(e) {
  ready = null; // the next open tries again
  status(`The search could not load: ${e?.message ?? e}`);
}

/* ------------------------------------------------------------- the dialog */

function el(tag, props = {}, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === 'class') e.className = v;
    else if (k === 'text') e.textContent = v;
    else e.setAttribute(k, v);
  }
  e.append(...kids);
  return e;
}

function build() {
  const input = el('input', {
    type: 'text', id: 'search-q', class: 'search-q', role: 'combobox',
    'aria-autocomplete': 'list', 'aria-expanded': 'false', 'aria-controls': 'search-list',
    'aria-labelledby': 'search-title', placeholder: 'Search the docs',
    autocomplete: 'off', autocapitalize: 'off', spellcheck: 'false', enterkeyhint: 'go',
  });
  const close = el('button', { type: 'button', class: 'search-close', 'aria-label': 'Close search' },
    el('kbd', { text: 'esc', 'aria-hidden': 'true' }), el('span', { class: 'search-x', text: 'Close', 'aria-hidden': 'true' }));
  const chips = el('div', { class: 'search-chips', role: 'group', 'aria-label': 'Only this part of the site' });
  const note = el('p', { class: 'search-status', role: 'status', 'aria-live': 'polite' });
  const list = el('div', { id: 'search-list', class: 'search-list', role: 'listbox', 'aria-label': 'Results' });
  const timing = el('span', { class: 'search-time' });
  const foot = el('footer', { class: 'search-foot' },
    el('span', { class: 'search-keys', 'aria-hidden': 'true' },
      el('kbd', { text: '↑' }), el('kbd', { text: '↓' }), ' to move ',
      el('kbd', { text: '↵' }), ' to open ', el('kbd', { text: 'esc' }), ' to close'),
    el('span', { class: 'search-by' }, 'Searched in this tab by fenecdb', timing));
  const dialog = el('dialog', { class: 'search', 'aria-labelledby': 'search-title' },
    el('div', { class: 'search-box' },
      el('h2', { id: 'search-title', class: 'search-title', text: 'Search the docs' }),
      el('div', { class: 'search-head' }, icon(), input, close),
      chips, note, list, foot));
  document.body.append(dialog);

  input.addEventListener('input', run);
  input.addEventListener('keydown', keys);
  close.addEventListener('click', () => dialog.close());
  // A click on the backdrop lands on the dialog itself.
  dialog.addEventListener('click', (e) => { if (e.target === dialog) dialog.close(); });
  dialog.addEventListener('keydown', trap);
  dialog.addEventListener('cancel', (e) => { e.preventDefault(); dialog.close(); });
  dialog.addEventListener('close', () => {
    document.documentElement.classList.remove('search-open');
    const back = returnTo?.isConnected ? returnTo : document.querySelector('.search-open-btn');
    back?.focus({ preventScroll: true });
  });
  list.addEventListener('click', (e) => {
    const o = e.target.closest('[role="option"]');
    if (o) go(options[Number(o.dataset.i)]);
  });
  list.addEventListener('mousemove', (e) => {
    const o = e.target.closest('[role="option"]');
    if (o && Number(o.dataset.i) !== active) select(Number(o.dataset.i), false);
  });
  chips.addEventListener('click', (e) => {
    const b = e.target.closest('button[data-section]');
    if (!b) return;
    section = b.dataset.section === section || b.dataset.section === '' ? null : b.dataset.section;
    run();
  });
  return { dialog, input, chips, note, list, timing };
}

function icon() {
  const ns = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(ns, 'svg');
  svg.setAttribute('viewBox', '0 0 20 20');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('class', 'search-icon');
  const c = document.createElementNS(ns, 'circle');
  c.setAttribute('cx', '8.5'); c.setAttribute('cy', '8.5'); c.setAttribute('r', '5.5');
  const l = document.createElementNS(ns, 'path');
  l.setAttribute('d', 'M12.6 12.6 17 17');
  svg.append(c, l);
  return svg;
}

/* A modal dialog keeps the page out of reach; this keeps Tab inside it too,
   where it would otherwise go on to the browser's own controls. */
function trap(e) {
  if (e.key !== 'Tab') return;
  const stops = [...ui.dialog.querySelectorAll('input, button')].filter((b) => !b.disabled && b.offsetParent);
  if (!stops.length) return;
  const [first, last] = [stops[0], stops[stops.length - 1]];
  if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
  else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
}

function keys(e) {
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    if (!options.length) return;
    const step = e.key === 'ArrowDown' ? 1 : -1;
    select((active + step + options.length) % options.length);
  } else if (e.key === 'Enter') {
    e.preventDefault();
    if (options[active]) go(options[active]);
  } else if (e.key === 'Escape') {
    // Handled here rather than left to the dialog's cancel, which a browser
    // may refuse to fire twice without a click between.
    e.preventDefault();
    ui.dialog.close();
  }
}

function select(i, scroll = true) {
  const prev = ui.list.querySelector('[aria-selected="true"]');
  prev?.setAttribute('aria-selected', 'false');
  active = i;
  const o = document.getElementById(`search-opt-${i}`);
  if (!o) { ui.input.removeAttribute('aria-activedescendant'); return; }
  o.setAttribute('aria-selected', 'true');
  ui.input.setAttribute('aria-activedescendant', o.id);
  if (scroll) o.scrollIntoView({ block: 'nearest' });
}

function go(hit) {
  const to = new URL(hit.url || './', ROOT);
  ui.dialog.close();
  location.assign(to.href);
}

/* ---------------------------------------------------------------- answers */

function status(text) {
  if (ui) ui.note.textContent = text;
}

function run() {
  if (!db) return;
  const q = ui.input.value;
  const t = performance.now();
  const { hits, facets } = search(db, q, section);
  const ms = performance.now() - t;
  render(q, hits, facets);
  ui.timing.textContent = q.trim() ? ` · ${ms < 1 ? ms.toFixed(2) : ms.toFixed(1)} ms` : '';
  ui.dialog.dataset.queryMs = ms.toFixed(3);
  if (hits.length && !firstShown) {
    firstShown = true;
    ui.dialog.dataset.firstResultMs = (performance.now() - openedAt).toFixed(1);
  }
}

function render(q, hits, facets) {
  // The arrows go down the list as it is shown: grouped by page.
  const groups = grouped(hits);
  options = groups.flatMap((g) => g.hits);
  active = -1;
  ui.input.removeAttribute('aria-activedescendant');
  ui.input.setAttribute('aria-expanded', String(hits.length > 0));

  ui.chips.replaceChildren();
  if (facets.length) {
    const total = facets.reduce((n, f) => n + f.count, 0);
    ui.chips.append(chip('', 'All', total, section === null));
    for (const f of facets) ui.chips.append(chip(f.value, f.value, f.count, section === f.value));
    // A group chosen before this query matched nothing in it stays, at 0.
    if (section && !facets.some((f) => f.value === section)) ui.chips.append(chip(section, section, 0, true));
  }

  const out = [];
  groups.forEach((g, gi) => {
    const label = el('div', { id: `search-group-${gi}`, class: 'search-page' },
      el('span', { class: 'search-page-title' }, marked(g.title, g.titleMarks)),
      el('span', { class: 'search-page-url', text: '/' + g.page }));
    const group = el('div', { role: 'group', class: 'search-group', 'aria-labelledby': label.id }, label);
    for (const h of g.hits) {
      const s = h.snippet;
      group.append(el('div', {
        role: 'option', id: `search-opt-${options.indexOf(h)}`, class: 'search-hit',
        'aria-selected': 'false', 'data-i': String(options.indexOf(h)),
      },
      el('span', { class: 'search-hit-head' }, marked(h.heading, h.heading === h.title ? h.titleMarks : h.headingMarks)),
      el('span', { class: 'search-hit-text' }, marked(s.text, s.marks))));
    }
    out.push(group);
  });
  ui.list.replaceChildren(...out);
  ui.list.scrollTop = 0;
  if (hits.length) select(0, false);

  if (!q.trim()) status('Type to search every page of the docs.');
  else if (!hits.length) status(`Nothing found for “${q.trim()}”${section ? ` under ${section}` : ''}.`);
  else status(`${hits.length} section${hits.length === 1 ? '' : 's'} on ${groups.length} page${groups.length === 1 ? '' : 's'}.`);
}

function chip(value, label, count, on) {
  return el('button', { type: 'button', class: 'search-chip', 'data-section': value, 'aria-pressed': String(on) },
    label, el('span', { class: 'search-chip-n', text: String(count) }));
}

/* `text` as text nodes, each `[start, end]` span of it (UTF-16 offsets, as
   the engine gives them) in a <mark>. Nothing of the text is parsed. */
function marked(text, marks = []) {
  const f = document.createDocumentFragment();
  let at = 0;
  for (const [a, b] of marks) {
    if (a < at || b <= a || b > text.length) continue;
    if (a > at) f.append(text.slice(at, a));
    f.append(el('mark', { text: text.slice(a, b) }));
    at = b;
  }
  f.append(text.slice(at));
  return f;
}
