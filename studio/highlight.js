/* The editors' highlighter: the docs' (`highlight` in build.py) for the text
   being typed. The rules are not written here -- build.py writes its own
   patterns and keyword lists in place of the marker below, so a statement
   cannot be coloured two ways, and `site/test_highlight.py` holds the two to
   the same HTML. Loaded by fenec studio's query editor -- on a server,
   and on the site's playground -- from the file build.py writes
   (`studio/highlight.js`, which the same test holds to it byte for
   byte). */

const RULES = {"types":["bool","int","float","text","bytes","timestamp","vector","f16","cosine","l2","dot","Fenec","FenecSync","Database","Collection","Value","Error","String","Vec","Option","Result"],"langs":{"fenecql":{"pattern":"(?<comment>--[^\\n]*)|(?<string>\\\"(?:[^\\\"\\\\\\n]|\\\\.)*\\\"|'(?:[^'\\\\\\n]|\\\\.)*')|(?<param>\\$\\d+)|(?<anno>@[A-Za-z_][\\w]*)|(?<word>[A-Za-z_][\\w]*)|(?<num>\\b\\d[\\d_]*(?:\\.\\d+)?\\b)","kw":["create","drop","collection","index","if","not","exists","get","put","set","del","select","from","where","near","order","limit","offset","count","ef","exact","asc","desc","and","or","in","has","is","null","true","false","collections","describe","compact","begin","commit","on","match","fuse","lookup","group","insert","set","del","rerank","absent"]}}};

const types = new Set(RULES.types);
const langs = {};
const ESC = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#x27;' };
// Python's `html.escape`, quotes and all, so the two give the same bytes.
const esc = (s) => s.replace(/[&<>"']/g, (c) => ESC[c]);

/* `code` as runs of `[kind, text]`, `kind` the class a run is coloured by
   (`t-kw`, `t-string`, ...) or `''` for plain text: what the HTML below and
   the editors' mirror are both made of, so a page that sets no markup draws
   the same colours with text nodes. */
export function tokens(code, lang) {
  const r = RULES.langs[lang];
  if (!r) return code ? [['', code]] : [];
  const l = langs[lang] ??= { re: new RegExp(r.pattern, 'g'), kw: new Set(r.kw) };
  const out = [];
  let pos = 0;
  l.re.lastIndex = 0;
  for (let m; (m = l.re.exec(code)); ) {
    const word = m[0];
    // Every alternative matches at least a character, so the walk ends.
    if (m.index > pos) out.push(['', code.slice(pos, m.index)]);
    pos = m.index + word.length;
    let kind;
    for (const k in m.groups) if (m.groups[k] !== undefined) { kind = k; break; }
    if (kind === 'word') {
      out.push([l.kw.has(word) ? 't-kw' : types.has(word) ? 't-type' : '', word]);
    } else {
      out.push([`t-${kind}`, word]);
    }
  }
  if (pos < code.length) out.push(['', code.slice(pos)]);
  return out;
}

export function highlight(code, lang) {
  let out = '';
  for (const [kind, text] of tokens(code, lang)) {
    out += kind ? `<span class="${kind}">${esc(text)}</span>` : esc(text);
  }
  return out;
}

/* A line's runs as nodes: a span a coloured run, a text node a plain one,
   the text set as text -- no markup is parsed. */
function line(code, lang) {
  const row = document.createElement('div');
  for (const [kind, text] of tokens(code, lang)) {
    if (!kind) { row.append(text); continue; }
    const span = document.createElement('span');
    span.className = kind;
    span.textContent = text;
    row.append(span);
  }
  // An empty line holds a space, or its block would have no height where
  // the textarea has a line.
  if (!row.firstChild) row.append(' ');
  return row;
}

/* A textarea coloured as it is typed: its text drawn transparent over a
   mirror of it, highlighted, in the same font, padding and wrapping (the
   CSS under `.hl`). The textarea stays the control -- caret, selection,
   IME, undo and the mobile keyboard are the browser's own -- and the mirror
   is `aria-hidden`. It is drawn again at most once a frame, however many
   inputs came in it. The page holds the two in their box already
   (`<div class="hl"><pre class="hl-mirror">` before the textarea): moved
   into one here, a textarea already being typed in would lose its focus. */
export function overlay(area, lang = 'fenecql') {
  const box = area.parentElement;
  const pre = box.querySelector('.hl-mirror');

  /* A line a block, and only the lines a keystroke changed drawn again: the
     whole text of 500 lines, highlighted and laid out at once, took 15 ms a
     keystroke. No FenecQL token spans a line (a comment ends with it, a
     string may not hold one), so a line coloured alone is coloured as in
     the whole -- test_highlight.py holds that too. */
  let shown = [], queued = false;
  const sync = () => { pre.scrollTop = area.scrollTop; pre.scrollLeft = area.scrollLeft; };
  const draw = () => {
    queued = false;
    const next = area.value.split('\n');
    const n = Math.min(shown.length, next.length);
    let a = 0, b = 0;
    while (a < n && shown[a] === next[a]) a++;
    while (b < n - a && shown[shown.length - 1 - b] === next[next.length - 1 - b]) b++;
    const rows = pre.children;
    for (let i = shown.length - b - 1; i >= a; i--) rows[i].remove();
    const at = rows[a] || null, add = document.createDocumentFragment();
    for (let i = a; i < next.length - b; i++) add.append(line(next[i], lang));
    pre.insertBefore(add, at);
    shown = next;
    sync();
  };
  const later = () => { if (!queued) { queued = true; requestAnimationFrame(draw); } };
  area.addEventListener('input', later);
  area.addEventListener('scroll', sync, { passive: true });
  // A resize can rewrap the text and move the textarea's scroll.
  new ResizeObserver(sync).observe(area);
  draw();
  box.classList.add('on');
  // A value set from script (an example picked) fires no input event.
  return later;
}
