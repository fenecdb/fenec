// The query editor: FenecQL as a person types it, run as typed.
//
// It has no more authority than the token: the text goes to `/query` --
// several statements to `/batch`, one block that lands whole or not at all
// -- with the token the rest of the studio sends, so the server's scope
// rewrite holds a scoped token to its rows here as everywhere. Nothing is
// rewritten on the way but the cut at each `;`; the parameters are the
// panel's JSON, bound by the server. The colours are the docs' own rules
// (`highlight.js`, written by site/build.py) drawn over the textarea, which
// stays the control: its caret, selection, IME and undo are the browser's.
// History and saved queries stay in this browser's localStorage; the token
// is never kept there.

import { h, fill, number } from './dom.js';
import { Grid } from './grid.js';
import { overlay } from './highlight.js';
import { writable } from './statements.js';
import { editorRequest, splitStatements, explainable, isExplain, explained, StatementError } from './statements-views.js';
import { call, refused, fenecql, columnsOf, ms, dialog, kept } from './kit.js';

const HISTORY = 'fenec-studio-history';
const SAVED = 'fenec-studio-saved';
const KEEP = 100;
/** Past this the JSON view shows the start of the answer, not all of it. */
const JSON_SHOWN = 2_000_000;

export function mount(host, ctx) {
  const area = h('textarea', {
    id: 'query',
    class: 'ed-text',
    spellcheck: 'false',
    autocomplete: 'off',
    autocapitalize: 'off',
    'aria-label': 'FenecQL',
    'aria-describedby': 'query-note',
  });
  const mirror = h('pre', { class: 'hl-mirror q', 'aria-hidden': 'true' });
  const params = h('textarea', {
    id: 'query-params',
    class: 'ed-params-text',
    spellcheck: 'false',
    autocomplete: 'off',
    'aria-label': 'Parameters, a JSON list',
    value: '[]',
  });
  const runBtn = h('button', { type: 'button', class: 'btn primary', onclick: () => run() }, 'Run');
  const saveBtn = h('button', { type: 'button', class: 'btn', onclick: () => saveAs() }, 'Save');
  const tabs = {};
  const tabBar = h(
    'div',
    { class: 'ed-tabs', role: 'tablist', 'aria-label': 'The answer as' },
    ['rows', 'json', 'plan'].map((t) => {
      tabs[t] = h('button', { type: 'button', role: 'tab', class: 'ed-tab', 'aria-selected': 'false', dataset: { tab: t }, onclick: () => showTab(t) }, { rows: 'Rows', json: 'JSON', plan: 'Plan' }[t]);
      return tabs[t];
    }),
  );
  const meta = h('div', { class: 'ed-meta', 'aria-live': 'polite' });
  const picks = h('div', { class: 'ed-picks', role: 'group', 'aria-label': 'Statements of the batch', hidden: true });
  const gridHost = h('div', { class: 'grid-host ed-grid', hidden: true });
  const out = h('div', { class: 'ed-out' });
  const savedList = h('ul', { class: 'ed-list', 'aria-label': 'Saved queries' });
  const historyList = h('ul', { class: 'ed-list', 'aria-label': 'History' });
  const clear = h('button', { type: 'button', class: 'btn ghost small', onclick: () => (kept.set(HISTORY, []), drawLists()) }, 'Clear');

  fill(
    host,
    h(
      'div',
      { class: 'ed' },
      h(
        'div',
        { class: 'ed-main' },
        h(
          'div',
          { class: 'ed-bar' },
          h('h1', { class: 'view-title' }, 'Query'),
          h('p', { class: 'ed-note', id: 'query-note' }, 'Runs what you type, as you typed it, with your token: it may do what the token may, and nothing more.'),
          h('div', { class: 'ed-actions' }, h('span', { class: 'ed-key', 'aria-hidden': 'true' }, h('kbd', {}, navigator.platform?.startsWith('Mac') ? '⌘↵' : 'Ctrl+↵')), saveBtn, runBtn),
        ),
        h(
          'div',
          { class: 'ed-inputs' },
          h('div', { class: 'hl ed-hl' }, mirror, area),
          h(
            'div',
            { class: 'ed-params' },
            h('label', { for: 'query-params' }, 'Parameters'),
            params,
            h('p', { class: 'hint' }, 'A JSON list, bound to $1, $2, ... For several statements, a list each: [[1], ["a"]].'),
          ),
        ),
        h('div', { class: 'ed-result-bar' }, tabBar, meta),
        picks,
        h('div', { class: 'ed-body' }, out, gridHost),
      ),
      h(
        'aside',
        { class: 'ed-side', 'aria-label': 'Saved and past queries' },
        h('h2', {}, 'Saved'),
        savedList,
        h('div', { class: 'ed-side-head' }, h('h2', {}, 'History'), clear),
        historyList,
        h('p', { class: 'hint small' }, 'Kept in this browser, never the token.'),
      ),
    ),
  );

  const redraw = overlay(area, 'fenecql');
  const grid = new Grid(gridHost, {
    onSort: () => {},
    onQuick: () => {},
    onEdit: async () => null,
    onDelete: () => {},
    onCopy: (row) => navigator.clipboard?.writeText(JSON.stringify(row, null, 2)).then(() => ctx.say('Copied the row as JSON.'), () => {}),
    onInspect: () => {},
    onActive: () => {},
    onError: (e) => ctx.say(ctx.explain(e), 'error'),
  });

  // What the last run sent and was answered.
  let last = null;
  let pick = 0;
  let tab = 'rows';
  let running = false;
  const plans = new Map();

  const keys = (e) => {
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      run();
    }
  };
  area.addEventListener('keydown', keys);
  params.addEventListener('keydown', keys);

  const set = (text) => {
    area.value = text;
    redraw();
  };

  // ------------------------------------------------------------- running

  async function run() {
    if (running) return;
    // A selection runs alone, as in any editor of statements.
    const sel = area.selectionStart !== area.selectionEnd ? area.value.slice(area.selectionStart, area.selectionEnd) : null;
    const source = sel ?? area.value;
    const base = sel ? area.selectionStart : 0;
    let req;
    try {
      req = editorRequest(source, params.value);
    } catch (e) {
      showError(e instanceof StatementError ? { message: e.message } : { message: String(e) });
      return;
    }
    remember(source.trim(), params.value.trim());
    running = true;
    runBtn.disabled = true;
    fill(meta, h('span', { class: 'ed-running' }, 'Running…'));
    let r;
    try {
      r =
        req.kind === 'query'
          ? await call(ctx, '/query', { method: 'POST', body: JSON.stringify({ query: req.text, params: req.params }) })
          : await call(ctx, '/batch', {
              method: 'POST',
              type: 'application/x-ndjson',
              body: req.items.map(([query, p]) => JSON.stringify({ query, params: p })).join('\n'),
            });
    } catch (e) {
      running = false;
      runBtn.disabled = false;
      showError({ message: `The server did not answer: ${e.message}` });
      return;
    }
    running = false;
    runBtn.disabled = false;
    const statements = splitStatements(source).map((s) => ({ ...s, at: s.at + base }));
    last = { req, r, statements };
    pick = 0;
    plans.clear();
    if (!r.ok) {
      const at = req.kind === 'batch' ? r.json?.at : 0;
      const where = statements[typeof at === 'number' ? at : 0];
      if (where) point(where, r.json?.error);
      showError({ status: r.status, message: r.json?.error ?? r.text, at: req.kind === 'batch' ? at : null, completed: r.json?.completed, count: statements.length, r });
      return;
    }
    // A statement whose rows are its plan opens on them.
    const only = req.kind === 'query' ? req.text : null;
    show(only && isExplain(only) ? 'plan' : tab === 'plan' && !(only && explainable(only)) ? 'rows' : tab);
  }

  /** The statement a refusal names selected in the editor, at the position it gives when it gives one. */
  function point(stmt, message) {
    const m = /position (\d+)/.exec(message ?? '');
    const from = stmt.at + (m ? Math.min(Number(m[1]), stmt.text.length) : 0);
    const to = m ? from : stmt.at + stmt.text.length;
    try {
      area.setSelectionRange(from, Math.max(from, to));
    } catch {
      /* a value changed under it */
    }
  }

  // ------------------------------------------------------------- the answer

  /** The answer of statement `i`: a batch's `results[i]`, or `/query`'s body. */
  function answerOf(i) {
    if (!last?.r.ok) return null;
    return last.req.kind === 'batch' ? last.r.json?.results?.[i] : last.r.json;
  }

  function textOf(i) {
    return last.req.kind === 'batch' ? last.req.items[i][0] : last.req.text;
  }

  function paramsOf(i) {
    return last.req.kind === 'batch' ? last.req.items[i][1] : last.req.params;
  }

  function show(next = tab) {
    tab = next;
    for (const [t, b] of Object.entries(tabs)) b.setAttribute('aria-selected', String(t === tab));
    drawMeta();
    drawPicks();
    const body = answerOf(pick);
    gridHost.hidden = true;
    fill(out);
    out.hidden = false;
    if (tab === 'json') {
      const all = last.req.kind === 'batch' ? last.r.json : body;
      const text = JSON.stringify(all, null, 2) ?? 'null';
      fill(out, h('pre', { class: 'ed-json', tabindex: '0', 'aria-label': 'The answer as JSON' }, text.length > JSON_SHOWN ? `${text.slice(0, JSON_SHOWN)}\n…` : text));
      return;
    }
    if (tab === 'plan') return drawPlan();
    const rows = rowsOf(body);
    if (rows) {
      out.hidden = true;
      gridHost.hidden = false;
      grid.show({
        fields: columnsOf(rows),
        rows,
        plain: true,
        empty: 'The statement answered no rows.',
      });
      if (body?.facets) {
        out.hidden = false;
        fill(out, facetsOf(body.facets));
      }
      return;
    }
    if (body && typeof body.affected === 'number') {
      fill(out, h('p', { class: 'ed-said' }, `Wrote ${number(body.affected)} ${body.affected === 1 ? 'row' : 'rows'}.`));
    } else if (body && typeof body.message === 'string') {
      fill(out, h('p', { class: 'ed-said' }, body.message));
    } else {
      fill(out, h('pre', { class: 'ed-json' }, JSON.stringify(body, null, 2) ?? 'null'));
    }
  }

  function rowsOf(body) {
    if (Array.isArray(body)) return body;
    if (body && Array.isArray(body.rows)) return body.rows;
    return null;
  }

  function facetsOf(facets) {
    return h(
      'div',
      { class: 'facets ed-facets' },
      Object.entries(facets).map(([field, values]) =>
        h(
          'div',
          { class: 'facet' },
          h('span', { class: 'facet-name' }, field),
          values.map((v) => h('span', { class: 'chip' }, h('span', { class: 'chip-value' }, v.value === null ? 'null' : String(v.value)), h('span', { class: 'chip-count' }, number(v.count)))),
        ),
      ),
    );
  }

  function drawMeta() {
    const r = last.r;
    const bits = [
      h('span', { class: `ed-status ${r.ok ? 'ok' : 'bad'}`, title: 'HTTP status' }, String(r.status)),
      h('span', { class: 'ed-fact' }, h('span', { class: 'ed-label' }, 'took'), ms(r.ms)),
    ];
    if (last.req.kind === 'batch') bits.push(h('span', { class: 'ed-fact' }, h('span', { class: 'ed-label' }, 'batch of'), String(last.req.items.length)));
    const rows = rowsOf(answerOf(pick));
    if (rows) bits.push(h('span', { class: 'ed-fact' }, h('span', { class: 'ed-label' }, 'rows'), number(rows.length)));
    if (r.seq !== null) bits.push(h('span', { class: 'ed-fact', title: 'Fenec-Seq: the change this write left the database at' }, h('span', { class: 'ed-label' }, 'change'), String(r.seq)));
    if (r.requestId) bits.push(h('span', { class: 'ed-fact', title: 'X-Request-Id: the id of this request in the server’s logs' }, h('span', { class: 'ed-label' }, 'request'), h('span', { class: 'ed-rid' }, r.requestId)));
    fill(meta, bits);
  }

  function drawPicks() {
    const n = last.req.kind === 'batch' ? last.req.items.length : 0;
    picks.hidden = n < 2;
    if (n < 2) return fill(picks);
    fill(
      picks,
      last.req.items.map(([text], i) =>
        h(
          'button',
          {
            type: 'button',
            class: `ed-pick${i === pick ? ' on' : ''}`,
            'aria-pressed': String(i === pick),
            title: text,
            onclick: () => {
              pick = i;
              show();
            },
          },
          h('span', { class: 'ed-pick-n' }, String(i + 1)),
          fenecql(text.split('\n')[0].slice(0, 60)),
        ),
      ),
    );
  }

  /** The plan: the rows of an `explain` typed, or `explain` asked of a `get`, once a run. */
  async function drawPlan() {
    const text = textOf(pick);
    if (isExplain(text)) return fill(out, steps(rowsOf(answerOf(pick)) ?? [], text));
    if (!explainable(text)) {
      return fill(out, h('p', { class: 'ed-said' }, 'A plan is shown for a get: the path it took, which index and how many rows each step read.'));
    }
    const asked = explained({ text, params: paramsOf(pick) });
    const key = pick;
    if (!plans.has(key)) {
      fill(out, h('p', { class: 'ed-said' }, 'Asking the server how it ran…'));
      plans.set(
        key,
        call(ctx, '/query', { method: 'POST', body: JSON.stringify({ query: asked.text, params: asked.params }) }).then((r) => {
          if (!r.ok) throw refused(r);
          return r.json;
        }),
      );
    }
    try {
      const rows = await plans.get(key);
      if (tab === 'plan' && pick === key) fill(out, steps(rowsOf(rows) ?? [], asked.text));
    } catch (e) {
      plans.delete(key);
      if (tab === 'plan') fill(out, h('p', { class: 'ed-error' }, ctx.explain(e)));
    }
  }

  function steps(rows, asked) {
    return h(
      'div',
      { class: 'ed-plan' },
      h('p', { class: 'hint' }, 'The path the server took, a step a line, in the order the steps ran. ', h('span', { class: 'ed-label' }, 'Asked as'), ' ', fenecql(asked.split('\n')[0].slice(0, 120))),
      h(
        'ol',
        { class: 'plan-steps' },
        rows.map((r) => {
          const line = String(r.plan ?? JSON.stringify(r));
          const m = /^([a-z]+):\s*(.*)$/s.exec(line);
          return h('li', {}, m ? [h('span', { class: 'plan-kind' }, m[1]), h('span', { class: 'plan-what' }, m[2])] : line);
        }),
      ),
    );
  }

  function showError({ status = null, message, at = null, completed = null, count = 1, r = null }) {
    for (const b of Object.values(tabs)) b.setAttribute('aria-selected', 'false');
    picks.hidden = true;
    gridHost.hidden = true;
    out.hidden = false;
    if (r) {
      last = last ?? { r, req: { kind: 'query' } };
      fill(
        meta,
        h('span', { class: 'ed-status bad', title: 'HTTP status' }, String(status)),
        h('span', { class: 'ed-fact' }, h('span', { class: 'ed-label' }, 'took'), ms(r.ms)),
        r.requestId ? h('span', { class: 'ed-fact' }, h('span', { class: 'ed-label' }, 'request'), h('span', { class: 'ed-rid' }, r.requestId)) : null,
      );
    } else fill(meta);
    const where =
      at !== null && at !== undefined
        ? h(
            'p',
            { class: 'ed-at' },
            `Statement ${at + 1} of ${count} stopped the batch. `,
            completed ? `The ${completed} before it stayed applied: a batch holding a compact runs each statement on its own.` : 'None of its writes landed: a batch lands whole or not at all.',
          )
        : null;
    fill(
      out,
      h(
        'div',
        { class: 'ed-refusal', role: 'alert' },
        h('p', { class: 'ed-error' }, status ? `${refusalWord(status)} (HTTP ${status}): ` : '', message),
        where,
      ),
    );
  }

  function refusalWord(status) {
    return (
      { 400: 'The statement does not read', 401: 'The token was refused', 403: 'Your token may not', 404: 'Not found', 409: 'A value is taken', 412: 'A require was not met', 413: 'Too large', 503: 'The server is busy' }[status] ??
      'Refused'
    );
  }

  function showTab(t) {
    if (!last?.r.ok) return;
    show(t);
  }

  // ------------------------------------------------------------- kept

  function remember(text, p) {
    if (!text) return;
    const list = kept.get(HISTORY, []).filter((x) => x && typeof x.text === 'string');
    if (list[0]?.text === text && list[0]?.params === p) return;
    list.unshift({ text, params: p, at: Date.now() });
    kept.set(HISTORY, list.slice(0, KEEP));
    drawLists();
  }

  function entry(item, onPick, extra) {
    return h(
      'li',
      {},
      h(
        'button',
        { type: 'button', class: 'ed-item', title: item.text, onclick: () => onPick(item) },
        item.name ? h('span', { class: 'ed-item-name' }, item.name) : null,
        fenecql(item.text.split('\n').find((l) => l.trim()) ?? '', 'q ed-item-text'),
      ),
      extra,
    );
  }

  function drawLists() {
    const use = (item) => {
      set(item.text);
      params.value = item.params || '[]';
      area.focus();
    };
    const saved = kept.get(SAVED, []).filter((x) => x && typeof x.text === 'string');
    fill(
      savedList,
      saved.length
        ? saved.map((s) =>
            entry(s, use, h('button', { type: 'button', class: 'btn ghost small danger', 'aria-label': `Forget ${s.name}`, onclick: () => (kept.set(SAVED, kept.get(SAVED, []).filter((x) => x.name !== s.name)), drawLists()) }, 'Forget')),
          )
        : h('li', { class: 'ed-none' }, 'None yet: Save keeps the query in the editor under a name.'),
    );
    const past = kept.get(HISTORY, []).filter((x) => x && typeof x.text === 'string');
    fill(historyList, past.length ? past.slice(0, 30).map((x) => entry(x, use)) : h('li', { class: 'ed-none' }, 'What you run is listed here.'));
  }

  function saveAs() {
    const text = area.value.trim();
    if (!text) return ctx.say('Type a query to save.', 'error');
    const nameBox = h('input', { id: 'save-name', type: 'text', value: '', spellcheck: 'false', autocomplete: 'off', required: true });
    const problem = h('p', { class: 'form-error', role: 'alert' });
    const d = dialog(
      'Save the query',
      h(
        'form',
        {
          class: 'form',
          onsubmit: (e) => {
            e.preventDefault();
            const name = nameBox.value.trim();
            if (!name) return fill(problem, 'Give it a name.');
            const list = kept.get(SAVED, []).filter((x) => x?.name !== name);
            list.unshift({ name, text, params: params.value.trim() });
            if (!kept.set(SAVED, list)) return fill(problem, 'This browser keeps nothing for this page: its site data is blocked.');
            drawLists();
            d.close();
            ctx.say(`Saved ${name}.`);
          },
        },
        h('label', { for: 'save-name' }, 'Name'),
        nameBox,
        h('pre', { class: 'statement q-block' }, fenecql(text)),
        problem,
        h('div', { class: 'dialog-actions' }, h('button', { type: 'button', class: 'btn', onclick: () => d.close() }, 'Cancel'), h('button', { type: 'submit', class: 'btn primary' }, 'Save')),
      ),
    );
    nameBox.focus();
  }

  // ------------------------------------------------------------- start

  drawLists();
  const first = kept.get(HISTORY, [])[0];
  const start = ctx.state.current && writable(ctx.state.current.name) ? `get ${ctx.state.current.name} limit 100` : first?.text ?? '';
  set(start);
  fill(out, h('p', { class: 'ed-said' }, 'Run a statement to see its answer here: its rows, its JSON and, for a get, the plan.'));

  return {
    /** A collection picked: its name typed at the caret, or a first statement over it. */
    open(name, { picked } = {}) {
      if (picked && name && writable(name)) {
        if (!area.value.trim()) set(`get ${name} limit 100`);
        else {
          area.focus();
          // Typed as a person types, so undo takes it back.
          if (!document.execCommand?.('insertText', false, name)) area.setRangeText(name, area.selectionStart, area.selectionEnd, 'end');
          redraw();
        }
      }
      area.focus();
    },
    load(text) {
      set(text);
      area.focus();
    },
    focus: () => area.focus(),
    hide() {},
    close() {},
  };
}
