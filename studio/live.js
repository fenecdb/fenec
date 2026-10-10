// A collection's rows as they change: a subscription (`GET /<c>/changes`,
// the client's `subscribe`) to the rows a `where` selects, its seed first
// and then each write, the rows it wrote coloured until the eye has caught
// them and the ones it deleted struck through before they go.
//
// The stream is the server's, opened with the page's token: under a scoped
// token the server ANDs the token's own filter into the shape and keeps the
// ids it sent, so a row the token may not read never reaches the page, nor
// does the deletion of one. A shape's seed is every row it holds, so one
// past `MOST` rows is refused here before it is opened: narrow it first.

import { h, fill, number } from './dom.js';
import { Grid } from './grid.js';
import * as S from './statements.js';

const MOST = 10_000;
/** How long a written row stays coloured, and a deleted one struck through. */
const SHOWN = 2_600;
const LOG = 200;

export function mount(host, ctx) {
  const title = h('h1', { class: 'view-title' });
  const where = h('input', { id: 'live-where', class: 'where', placeholder: 'status = "open"', spellcheck: 'false', autocomplete: 'off', 'aria-label': 'Where clause of the rows to follow, FenecQL' });
  const follow = h('button', { type: 'submit', class: 'btn' }, 'Follow');
  const pause = h('button', { type: 'button', class: 'btn', 'aria-pressed': 'false', onclick: () => toggle() }, 'Pause');
  const status = h('p', { class: 'lv-status', 'aria-live': 'polite' });
  const gridHost = h('div', { class: 'grid-host' });
  const log = h('ol', { class: 'lv-log', 'aria-label': 'Changes, the latest first', reversed: true });
  fill(
    host,
    h(
      'div',
      { class: 'lv' },
      h(
        'div',
        { class: 'toolbar' },
        h('div', { class: 'view-head' }, title, h('span', { class: 'view-count' }, 'live')),
        h(
          'form',
          {
            class: 'where-form',
            onsubmit: (e) => {
              e.preventDefault();
              start();
            },
          },
          h('label', { class: 'where-label', for: 'live-where' }, 'where'),
          where,
          follow,
        ),
        h('div', { class: 'actions' }, pause),
      ),
      status,
      h('div', { class: 'lv-body' }, gridHost, h('aside', { class: 'lv-side', 'aria-label': 'Changes' }, h('h2', {}, 'Changes'), log)),
    ),
  );

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

  let name = null;
  let stop = null;
  let paused = false;
  let waiting = [];
  let seq = null;
  let state = 'idle';
  // The rows by id, and the order they are shown in: the last written first.
  let rows = new Map();
  let order = [];
  const marks = new Map();
  let epoch = 0;

  const say = (...parts) => fill(status, parts);

  async function start() {
    stop?.();
    stop = null;
    const mine = ++epoch;
    rows = new Map();
    order = [];
    marks.clear();
    waiting = [];
    fill(log);
    const c = ctx.state.collections.find((x) => x.name === name);
    if (!c) {
      say('Pick a collection on the left to follow its rows.');
      grid.show({ fields: [], rows: [], plain: true, empty: '' });
      return;
    }
    fill(title, c.name);
    const text = where.value.trim();
    grid.show({ fields: ctx.fieldsOf(c), rows: [], plain: true, empty: 'No rows yet: they appear here as they are written.', mark: (row) => marks.get(row.id)?.kind ?? '' });
    // The seed is every row the shape holds: counted first, with the same
    // filter and the same token, and refused past MOST.
    let held;
    try {
      const f = S.filter({ where: text });
      const q = S.count(c.name, f);
      const got = await ctx.state.db.run(q.text, q.params);
      held = got.rows?.[0]?.count ?? 0;
    } catch (e) {
      if (mine === epoch) say(ctx.explain(e));
      return;
    }
    if (mine !== epoch) return;
    if (held > MOST) {
      say(`These are ${number(held)} rows, and a subscription sends every one before the first change. Narrow them to ${number(MOST)} or fewer with a where clause.`);
      where.focus();
      return;
    }
    say(`Opening a subscription to ${number(held)} ${held === 1 ? 'row' : 'rows'}…`);
    state = 'opening';
    stop = ctx.state.db.subscribe(
      c.name,
      text ? { where: text } : {},
      (ev) => {
        if (mine !== epoch) return;
        if (paused) {
          waiting.push(ev);
          line();
        } else apply(ev);
      },
      {
        onError: (e) => mine === epoch && say(ctx.explain(e)),
        onState: (s) => {
          if (mine !== epoch) return;
          state = s;
          line();
        },
      },
    );
  }

  function apply(ev) {
    const now = Date.now();
    if (ev.type === 'seed') {
      rows = new Map(ev.rows.map((r) => [r.id, r]));
      order = ev.rows.map((r) => r.id).sort((a, b) => b - a);
      marks.clear();
      seq = ev.seq;
      entry(ev.seq, `seed, ${number(ev.rows.length)} ${ev.rows.length === 1 ? 'row' : 'rows'}`);
    } else {
      seq = ev.seq;
      let made = 0;
      let changed = 0;
      for (const r of ev.puts) {
        if (rows.has(r.id)) changed++;
        else made++;
        marks.set(r.id, { kind: rows.has(r.id) ? 'changed' : 'new', until: now + SHOWN });
        rows.set(r.id, r);
        order = [r.id, ...order.filter((id) => id !== r.id)];
      }
      for (const id of ev.dels) {
        if (!rows.has(id)) continue;
        marks.set(id, { kind: 'gone', until: now + SHOWN });
      }
      const what = [made && `${made} new`, changed && `${changed} written`, ev.dels.length && `${ev.dels.length} deleted`, ev.schema && 'the schema changed'].filter(Boolean);
      entry(ev.seq, what.length ? what.join(', ') : 'a write to a row of yours');
      later();
    }
    draw();
  }

  /** The marks that ran out let go of, a deleted row with its mark. */
  let timer = null;
  function later() {
    clearTimeout(timer);
    const next = Math.min(...[...marks.values()].map((m) => m.until));
    if (!Number.isFinite(next)) return;
    timer = setTimeout(() => {
      const now = Date.now();
      for (const [id, m] of marks) {
        if (m.until > now) continue;
        marks.delete(id);
        if (m.kind === 'gone') {
          rows.delete(id);
          order = order.filter((x) => x !== id);
        }
      }
      draw();
      later();
    }, Math.max(0, next - Date.now()) + 20);
  }

  function draw() {
    grid.setRows(order.map((id) => rows.get(id)).filter(Boolean));
    line();
  }

  function line() {
    const c = name;
    const n = order.length;
    const parts = [];
    if (state === 'retry') parts.push('The stream ended; opening it again. ');
    else if (state === 'opening') parts.push('Opening… ');
    if (seq !== null) parts.push(`${number(n)} ${n === 1 ? 'row' : 'rows'} of ${c}${where.value.trim() ? ' where ' + where.value.trim() : ''}, at change ${seq}. `);
    if (paused) parts.push(h('b', {}, `Paused: ${waiting.length} ${waiting.length === 1 ? 'change waits' : 'changes wait'}.`));
    say(parts);
  }

  function entry(at, what) {
    const time = new Date().toLocaleTimeString([], { hour12: false });
    log.prepend(h('li', {}, h('span', { class: 'lv-seq' }, `change ${at}`), h('span', { class: 'lv-what' }, what), h('time', {}, time)));
    while (log.children.length > LOG) log.lastChild.remove();
  }

  function toggle() {
    paused = !paused;
    pause.setAttribute('aria-pressed', String(paused));
    fill(pause, paused ? 'Resume' : 'Pause');
    if (!paused) {
      const queued = waiting;
      waiting = [];
      for (const ev of queued) apply(ev);
    }
    line();
  }

  return {
    open(next) {
      if (next === name && stop) return;
      name = next;
      fill(title, name ?? 'No collection');
      where.value = '';
      start();
    },
    focus: () => where.focus(),
    hide() {},
    close() {
      epoch++;
      stop?.();
      stop = null;
      clearTimeout(timer);
    },
  };
}
