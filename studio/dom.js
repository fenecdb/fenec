// Elements made by hand, text set as text. Nothing here takes markup: a
// value from the database reaches the page as a text node or an attribute
// value, never as HTML, so a row holding `<img onerror=...>` shows those
// characters. The content security policy refuses inline script besides.

/**
 * `h('button', {class: 'x', onclick, 'aria-label': 'y'}, 'text', child)`.
 * A prop starting with `on` is a listener; `class`, `text` and `value` are
 * set as properties; `dataset` takes an object; everything else is an
 * attribute. `null`, `undefined` and `false` props and children are left
 * out.
 */
export function h(tag, props = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props ?? {})) {
    if (v === null || v === undefined || v === false) continue;
    if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
    else if (k === 'class') el.className = v;
    else if (k === 'text') el.textContent = v;
    else if (k === 'value') el.value = v;
    else if (k === 'dataset') Object.assign(el.dataset, v);
    else if (k === 'style') throw new Error('styles go in app.css: the policy refuses inline ones');
    else el.setAttribute(k, v === true ? '' : String(v));
  }
  append(el, children);
  return el;
}

function append(el, children) {
  for (const c of children.flat(Infinity)) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
}

/** `el` emptied, then given `children`. */
export function fill(el, ...children) {
  el.replaceChildren();
  append(el, children);
  return el;
}

/** The mark: the fennec's head as the site draws it, one path. */
export function mark(cls = 'mark') {
  const ns = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(ns, 'svg');
  svg.setAttribute('viewBox', '0 0 64 64');
  svg.setAttribute('class', cls);
  svg.setAttribute('aria-hidden', 'true');
  const path = document.createElementNS(ns, 'path');
  path.setAttribute(
    'd',
    'M6 4L14 32M6 4L26 22M26 22L32 20M32 20L38 22M38 22L58 4M58 4L50 32M14 32L17 44M17 44L32 58M32 58L47 44M47 44L50 32M26 22L24 37M14 32L24 37M24 37L32 49M38 22L40 37M50 32L40 37M40 37L32 49M32 49L32 58M24 37L17 44M40 37L47 44',
  );
  svg.append(path);
  return svg;
}

/** Bytes as a person reads them: 1 234 B, 12.4 KB, 3.1 GB. */
export function bytes(n) {
  if (!Number.isFinite(n)) return '';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let u = 0;
  while (n >= 1000 && u < units.length - 1) {
    n /= 1000;
    u++;
  }
  return u === 0 ? `${n} B` : `${n.toFixed(n < 10 ? 1 : 0)} ${units[u]}`;
}

/** A count with thin spaces between thousands, as the docs write them. */
export function number(n) {
  return Number(n).toLocaleString('en-US').replace(/,/g, ' ');
}
