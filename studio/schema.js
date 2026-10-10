// A collection's schema: the statements that make it, its fields, and the
// changes a person makes to it -- an index, a field added, renamed or
// dropped, the collection dropped.
//
// Every change is shown before it runs, twice: the exact statement it will
// send, and what the server's declared-schema plan (`POST /_schema/plan`)
// says of the schema as it would be after it -- the statements the engine
// would write for it, or what it refuses to guess (a field gone is a drop
// or a rename? it does not say). A change that destroys data runs only
// once its name is typed. The statement goes through `/batch` under an
// `Idempotency-Key`, as the studio's other writes do, with the page's
// token: the server refuses a schema change to a scoped one.

import { h, fill } from './dom.js';
import * as S from './statements-views.js';
import { call, refused, statementsBlock, dialog } from './kit.js';

/** An index as `/_schema` describes it, `{kind: 'hnsw', metric: 'cosine', m: 16}`, written as FenecQL names it. */
function indexText(ix) {
  if (!ix) return '';
  const { kind, ...rest } = ix;
  const args = Object.entries(rest).map(([k, v]) => (v === true ? k : `${k}=${v}`));
  return `@${kind}${args.length ? `(${args.join(', ')})` : ''}`;
}

export function mount(host, ctx) {
  const title = h('h1', { class: 'view-title' });
  const actions = h('div', { class: 'sc-actions' });
  const note = h('p', { class: 'sc-note' });
  const body = h('div', { class: 'sc-body' });
  fill(host, h('div', { class: 'sc' }, h('div', { class: 'sc-bar' }, h('div', { class: 'view-head' }, title, h('span', { class: 'view-count' }, 'schema')), actions), note, body));

  let name = null;
  let described = null;
  let texts = new Map();
  let reading = 0;

  /** Whether this token may change the schema: the server's own, or an open server. */
  const owner = () => ctx.state.who?.kind === 'full' || ctx.state.who?.kind === 'open';

  async function load(next) {
    name = next;
    const mine = ++reading;
    fill(title, name ?? 'No collection');
    fill(actions);
    if (!name) {
      fill(body, h('p', { class: 'hint' }, 'Pick a collection on the left.'));
      return;
    }
    let schema;
    let text;
    try {
      [schema, text] = await Promise.all([call(ctx, '/_schema'), call(ctx, '/_schema?as=fenecql')]);
      if (!schema.ok) throw refused(schema);
      if (!text.ok) throw refused(text);
    } catch (e) {
      if (mine === reading) fill(body, h('p', { class: 'ed-error' }, ctx.explain(e)));
      return;
    }
    if (mine !== reading) return;
    described = schema.json;
    texts = S.createTexts(text.json?.fenecql);
    draw();
  }

  function collection() {
    return described?.collections?.find((c) => c.name === name) ?? null;
  }

  function draw() {
    const c = collection();
    const lines = texts.get(name) ?? [];
    fill(
      note,
      owner()
        ? 'Each change shows the statement it sends and the declared-schema plan of it before it runs.'
        : 'Your token reads this schema. Changing it takes the server’s token.',
    );
    if (!c) {
      fill(body, h('p', { class: 'hint' }, `${name} is the database's own collection, or your token does not read it.`));
      return;
    }
    if (owner()) {
      fill(
        actions,
        h('button', { type: 'button', class: 'btn', onclick: () => change('index') }, 'Add index'),
        h('button', { type: 'button', class: 'btn', onclick: () => change('add') }, 'Add field'),
        h('button', { type: 'button', class: 'btn ghost danger', onclick: () => change('drop') }, 'Drop collection'),
      );
    }
    const copy = h(
      'button',
      {
        type: 'button',
        class: 'btn ghost small',
        onclick: () => navigator.clipboard?.writeText(lines.join('\n')).then(() => ctx.say('Copied the statements.'), () => ctx.say('The browser did not allow the copy.', 'error')),
      },
      'Copy',
    );
    const rows = c.fields.map((f) => {
      const kinds = S.indexKinds(f.type);
      const idx = f.index ? indexText(f.index) : null;
      const paths = (f.paths ?? []).map((p) => `${f.name}.${p.path} ${indexText(p.index)}`);
      return h(
        'tr',
        { dataset: { field: f.name } },
        h('td', { class: 'mono' }, f.name),
        h('td', { class: 'mono muted' }, f.type, f.collate ? ` collate ${f.collate}` : '', f.required ? ' required' : ''),
        h('td', { class: 'mono sun' }, idx ?? '', paths.length ? h('span', { class: 'sc-paths' }, paths.join(', ')) : null),
        h(
          'td',
          { class: 'sc-row-actions' },
          owner()
            ? [
                !f.index && kinds.length ? h('button', { type: 'button', class: 'btn ghost small', 'aria-label': `Add an index on ${f.name}`, onclick: () => change('index', f.name) }, 'Index') : null,
                h('button', { type: 'button', class: 'btn ghost small', 'aria-label': `Rename ${f.name}`, onclick: () => change('rename', f.name) }, 'Rename'),
                h('button', { type: 'button', class: 'btn ghost small danger', 'aria-label': `Drop ${f.name}`, onclick: () => change('drop-field', f.name) }, 'Drop'),
              ]
            : null,
        ),
      );
    });
    fill(
      body,
      h('div', { class: 'sc-head' }, h('h2', {}, 'As FenecQL'), copy, h('button', { type: 'button', class: 'btn ghost small', onclick: () => ctx.query(`get ${name} limit 100`) }, 'Query it')),
      statementsBlock(lines, 'statement q-block sc-create'),
      h('h2', {}, 'Fields'),
      h(
        'table',
        { class: 'sc-fields' },
        h('thead', {}, h('tr', {}, h('th', {}, 'Field'), h('th', {}, 'Type'), h('th', {}, 'Index'), h('th', {}, h('span', { class: 'sr' }, 'Changes')))),
        h('tbody', {}, h('tr', {}, h('td', { class: 'mono' }, 'id'), h('td', { class: 'mono muted' }, 'int'), h('td', { class: 'mono muted' }, 'its own'), h('td')), rows),
      ),
    );
  }

  // ------------------------------------------------------------- a change

  /**
   * The dialog for one change: its inputs, the statement they make, the
   * plan of it, and -- for a drop -- the name to type. `op` is `index`,
   * `add`, `rename`, `drop-field` or `drop`.
   */
  function change(op, field = null) {
    const c = collection();
    if (!c) return;
    const inputs = h('div', { class: 'form-fields sc-form' });
    const statement = h('div', { class: 'sc-statement' });
    const plan = h('div', { class: 'sc-plan', 'aria-live': 'polite' });
    const problem = h('p', { class: 'form-error', role: 'alert' });
    const destructive = op === 'drop' || op === 'drop-field';
    const confirm = h('input', { id: 'sc-confirm', type: 'text', spellcheck: 'false', autocomplete: 'off' });
    const confirmRow = h('div', { class: 'sc-confirm' }, h('label', { for: 'sc-confirm' }));
    const apply = h('button', { type: 'submit', class: `btn ${destructive ? 'danger' : 'primary'}`, disabled: true }, { index: 'Create index', add: 'Add field', rename: 'Rename field', 'drop-field': 'Drop field', drop: 'Drop collection' }[op]);
    const box = (id, label, el, hint) => h('div', { class: 'form-row' }, h('label', { for: id }, h('span', { class: 'form-name' }, label), hint ? h('span', { class: 'form-type' }, hint) : null), el);
    const select = (id, options, value) => {
      const s = h('select', { id }, options.map((o) => (Array.isArray(o) ? h('option', { value: o[0], selected: o[0] === value }, o[1]) : h('option', { value: o, selected: o === value }, o))));
      return s;
    };
    const fieldsWith = (pred) => c.fields.filter(pred).map((f) => [f.name, `${f.name}  ${f.type}`]);

    // The inputs each change takes, built again when what they offer
    // depends on another (an index's options on its kind).
    const v = {};
    const build = () => {
      const rows = [];
      if (op === 'index') {
        const can = c.fields.filter((f) => !f.index && S.indexKinds(f.type).length);
        v.field ??= field ?? can[0]?.name ?? null;
        const f = c.fields.find((x) => x.name === v.field);
        rows.push(box('sc-field', 'Field', (v.fieldEl = select('sc-field', can.map((x) => [x.name, `${x.name}  ${x.type}`]), v.field))));
        const kinds = f ? S.indexKinds(f.type) : [];
        if (!kinds.includes(v.kind)) v.kind = kinds[0] ?? null;
        rows.push(box('sc-kind', 'Index', (v.kindEl = select('sc-kind', kinds.map((k) => [k, `@${k}`]), v.kind))));
        rows.push(...options(v.kind));
        if (!can.length) rows.push(h('p', { class: 'hint' }, 'Every field that takes an index has one.'));
      } else if (op === 'add') {
        rows.push(box('sc-name', 'Name', (v.nameEl = h('input', { id: 'sc-name', type: 'text', value: v.name ?? '', spellcheck: 'false', autocomplete: 'off' }))));
        const types = h('datalist', { id: 'sc-types' }, ['text', 'int', 'float', 'bool', 'timestamp', 'json', 'bytes', '[text]', 'vector<768>', 'sparse<30522>', 'geo'].map((t) => h('option', { value: t })));
        rows.push(box('sc-type', 'Type', h('div', {}, (v.typeEl = h('input', { id: 'sc-type', type: 'text', value: v.type ?? 'text', list: 'sc-types', spellcheck: 'false', autocomplete: 'off' })), types)));
        let kinds = [];
        try {
          kinds = S.indexKinds(S.typeText(v.type ?? 'text'));
        } catch {
          /* said below, with the statement */
        }
        if (v.kind && !kinds.includes(v.kind)) v.kind = '';
        rows.push(box('sc-kind', 'Index', (v.kindEl = select('sc-kind', [['', 'none'], ...kinds.map((k) => [k, `@${k}`])], v.kind ?? ''))));
        rows.push(...options(v.kind));
        if ((v.type ?? 'text').trim() === 'text') rows.push(box('sc-collate', 'Collation', (v.collateEl = select('sc-collate', [['', 'bytes'], ['und', 'und, ICU’s root'], ['tr', 'tr, Turkish']], v.collate ?? ''))));
      } else if (op === 'rename') {
        v.field ??= field ?? c.fields[0]?.name;
        rows.push(box('sc-field', 'Field', (v.fieldEl = select('sc-field', fieldsWith(() => true), v.field))));
        rows.push(box('sc-name', 'New name', (v.nameEl = h('input', { id: 'sc-name', type: 'text', value: v.name ?? '', spellcheck: 'false', autocomplete: 'off' }))));
      } else if (op === 'drop-field') {
        v.field ??= field ?? c.fields[0]?.name;
        rows.push(box('sc-field', 'Field', (v.fieldEl = select('sc-field', fieldsWith(() => true), v.field))));
        rows.push(h('p', { class: 'sc-warn' }, 'Its values are gone from every read at once, and from the file at the next compact. Its index goes with it.'));
      } else {
        rows.push(h('p', { class: 'sc-warn' }, `Every row of ${c.name}, its indexes and its schema are deleted. This cannot be undone.`));
      }
      fill(inputs, rows);
    };
    const options = (kind) => {
      if (kind === 'ttl') return [box('sc-ttl', 'Expires after', (v.ttlEl = h('input', { id: 'sc-ttl', type: 'text', value: v.ttl ?? '30d', spellcheck: 'false', autocomplete: 'off' })), '30s, 15m, 12h, 7d')];
      if (kind === 'text') {
        return [
          box('sc-prefix', 'Prefixes', (v.prefixEl = h('input', { id: 'sc-prefix', type: 'text', inputmode: 'numeric', value: v.prefix ?? '', placeholder: 'off', spellcheck: 'false', autocomplete: 'off' })), 'up to N characters, for inflected words'),
          box('sc-chars', 'Characters', h('label', { class: 'sc-check' }, (v.charsEl = h('input', { id: 'sc-chars', type: 'checkbox', checked: !!v.chars })), ' each character of Han, kana and Hangul')),
        ];
      }
      if (kind === 'hnsw') {
        return [
          box('sc-metric', 'Metric', (v.metricEl = select('sc-metric', ['cosine', 'l2', 'dot'], v.metric ?? 'cosine'))),
          box('sc-quant', 'Codes', (v.quantEl = select('sc-quant', [['none', 'full vectors'], ['int8', 'int8'], ['bit', 'bit (cosine)']], v.quant ?? 'none'))),
        ];
      }
      return [];
    };
    /** What the inputs say now, read into `v`. */
    const read = () => {
      for (const k of ['field', 'kind', 'name', 'type', 'ttl', 'prefix', 'metric', 'quant', 'collate']) {
        const el = v[`${k}El`];
        if (el && el.isConnected) v[k] = el.value;
      }
      if (v.charsEl?.isConnected) v.chars = v.charsEl.checked;
    };
    const spec = () => (v.kind ? { kind: v.kind, ttl: v.ttl, prefix: v.prefix, chars: v.chars, metric: v.metric, quant: v.quant } : null);
    /** The statement, and the change the plan is asked of. */
    const made = () => {
      switch (op) {
        case 'index':
          if (!v.field) throw new S.StatementError('no field here takes an index');
          return [S.createIndex(c.name, v.field, spec()), { op, collection: c.name, field: v.field, index: spec() }];
        case 'add':
          return [
            S.addField(c.name, v.name ?? '', v.type ?? '', { index: spec(), collate: v.collate || null }),
            { op, collection: c.name, field: v.name, type: v.type, index: spec(), collate: v.collate || null },
          ];
        case 'rename':
          return [S.renameField(c.name, v.field, v.name ?? ''), { op, collection: c.name, from: v.field, to: v.name }];
        case 'drop-field':
          return [S.dropField(c.name, v.field), { op, collection: c.name, field: v.field }];
        default:
          return [S.dropCollection(c.name), { op, collection: c.name }];
      }
    };
    const must = () => (op === 'drop' ? c.name : op === 'drop-field' ? v.field : null);

    let current = null;
    let planned = false;
    let timer;
    let asked = 0;
    let shown = null;
    const update = () => {
      read();
      let stmt;
      let ch;
      let body;
      try {
        [stmt, ch] = made();
        body = JSON.stringify(S.planned(described, ch));
      } catch (e) {
        shown = null;
        planned = false;
        current = null;
        fill(statement, h('p', { class: 'form-error' }, ctx.explain(e)));
        fill(plan);
        refresh();
        return;
      }
      // The same change as the one shown keeps its plan: asked again, the
      // button that applies it would wait for the answer once more.
      if (shown === stmt.text + body) return refresh();
      shown = stmt.text + body;
      fill(problem);
      planned = false;
      current = stmt;
      fill(statement, statementsBlock([stmt.text]));
      const need = must();
      confirmRow.hidden = !need;
      if (need) fill(confirmRow.firstChild, 'Type ', h('code', {}, need), ' to confirm');
      clearTimeout(timer);
      const mine = ++asked;
      fill(plan, h('p', { class: 'hint' }, 'Asking for the plan…'));
      timer = setTimeout(async () => {
        let r;
        try {
          r = await call(ctx, '/_schema/plan', { method: 'POST', body });
        } catch (e) {
          r = { ok: false, status: 0, json: { error: e.message }, text: e.message };
        }
        if (mine !== asked) return;
        fill(plan, planOf(r));
        planned = true;
        refresh();
      }, 200);
      refresh();
    };
    const refresh = () => {
      const need = must();
      apply.disabled = !current || !planned || (need !== null && confirm.value !== need);
    };

    const form = h(
      'form',
      {
        class: 'form',
        oninput: (e) => (e.target === confirm ? refresh() : update()),
        onchange: (e) => {
          // A choice that changes what the others offer draws them again.
          // Nothing else: a text box's change comes as it loses the focus,
          // to the very button that applies it, and asking the plan again
          // there held the button back.
          if (['sc-field', 'sc-kind', 'sc-type'].includes(e.target.id)) {
            read();
            build();
            update();
          }
        },
        onsubmit: async (e) => {
          e.preventDefault();
          if (apply.disabled || !current) return;
          apply.disabled = true;
          try {
            await ctx.state.db.batch([[current.text, current.params]], { idempotencyKey: crypto.randomUUID() });
          } catch (err) {
            fill(problem, ctx.explain(err));
            refresh();
            return;
          }
          d.close();
          ctx.say(`Ran: ${current.text}`);
          await ctx.refresh();
          if (op === 'drop') await load(ctx.state.current?.name ?? null);
          else await load(name);
        },
      },
      inputs,
      h('h3', {}, 'The statement'),
      statement,
      h('h3', {}, 'The declared-schema plan'),
      plan,
      confirmRow,
      problem,
      h('div', { class: 'dialog-actions' }, h('button', { type: 'button', class: 'btn', onclick: () => d.close() }, 'Cancel'), apply),
    );
    const titles = {
      index: `Add an index to ${c.name}`,
      add: `Add a field to ${c.name}`,
      rename: `Rename a field of ${c.name}`,
      'drop-field': `Drop a field of ${c.name}`,
      drop: `Drop ${c.name}?`,
    };
    const d = dialog(titles[op], form);
    d.classList.add('wide');
    confirmRow.append(confirm);
    build();
    update();
    (inputs.querySelector('input, select') ?? confirm).focus();
  }

  /** `/_schema/plan`'s answer in words: what it would write, and what it will not guess. */
  function planOf(r) {
    if (!r.ok) return h('p', { class: 'form-error' }, `The plan was refused (HTTP ${r.status}): ${r.json?.error ?? r.text}`);
    const p = r.json ?? {};
    const out = [];
    if (p.statements?.length) {
      out.push(h('p', { class: 'hint' }, 'The engine would bring the schema to it with:'), statementsBlock(p.statements));
    }
    for (const x of p.refusals ?? []) {
      out.push(h('div', { class: 'sc-refusal' }, h('p', {}, h('b', {}, x.kind.replace(/_/g, ' ')), x.field ? ` (${x.field})` : '', `: ${x.message}`), x.fix ? h('p', { class: 'hint' }, x.fix) : null));
    }
    if (!out.length) out.push(h('p', { class: 'hint' }, 'Nothing: a collection the description leaves out is one the plan leaves alone.'));
    return out;
  }

  return {
    open(next) {
      if (next !== name || !described) load(next);
    },
    focus: () => host.querySelector('button, [tabindex]')?.focus(),
    hide() {},
    close() {},
    /** For a schema changed elsewhere. */
    reload: () => load(name),
  };
}
