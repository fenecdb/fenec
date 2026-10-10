// Writes, and what is said about them: the insert form, the delete
// confirmation that shows the statement it will send, and every refusal in
// plain words.

import { h, fill } from './dom.js';
import { insertRow, shown, kindOf, StatementError } from './statements.js';
import { readInput, multiline } from './values.js';

/**
 * A refusal in words a person can act on. The server's message is kept
 * whole -- for a 403 it names the field the policy refused -- with what
 * the status means said first.
 */
export function explain(err) {
  if (err instanceof StatementError) return err.message;
  const msg = err?.message ?? String(err);
  switch (err?.status) {
    case 401:
      return `The server no longer takes this token: ${msg}. Sign in again.`;
    case 403:
      return `Your token's policy refused this: ${msg}.`;
    case 404:
      return `Not found: ${msg}.`;
    case 409:
      return `A value is taken: ${msg}.`;
    case 412:
      return 'The row changed: it is no longer there as you saw it, so nothing was written. Reload to see it as it is now.';
    case 413:
      return `Too large for the server: ${msg}.`;
    case 422:
      return `The server refused the retry: ${msg}.`;
    case 503:
      return `The server is busy with this tenant (a move or a failover): ${msg}. Try again in a moment.`;
    case 507:
      return `The server is at its memory ceiling and takes no writes: ${msg}.`;
    default:
      return err?.status ? `${msg} (HTTP ${err.status}).` : msg;
  }
}

/** A dialog, shown modally; `close()` takes it away. */
function dialog(title, ...body) {
  const d = h('dialog', { class: 'dialog', 'aria-label': title }, h('h2', { class: 'dialog-title' }, title), ...body);
  document.body.append(d);
  d.addEventListener('close', () => d.remove());
  d.showModal();
  return d;
}

/** Whether the person confirms `stmt`, shown to them as it will be sent. */
export function confirmDelete(collection, row, stmt) {
  return new Promise((resolve) => {
    let done = false;
    const finish = (yes) => {
      if (done) return;
      done = true;
      resolve(yes);
      d.close();
    };
    const keep = h('button', { type: 'button', class: 'btn', onclick: () => finish(false) }, 'Keep it');
    const del = h('button', { type: 'button', class: 'btn danger', onclick: () => finish(true) }, 'Delete row');
    const d = dialog(
      `Delete row ${row.id} from ${collection}?`,
      h('p', {}, 'This statement will be sent. It deletes the row only if it is still there for your token.'),
      h('pre', { class: 'statement' }, shown(stmt)),
      h('div', { class: 'dialog-actions' }, keep, del),
    );
    d.addEventListener('close', () => finish(false));
    keep.focus();
  });
}

/**
 * The form for a new row, built from the schema: a box a field, typed as
 * the field is, and the statement it will send written out below as the
 * boxes fill. `submit(stmt)` sends it and resolves to an error message or
 * `null`.
 */
export function insertForm(collection, fields, submit) {
  const inputs = [];
  const rows = fields
    .filter((f) => f.name !== 'id')
    .map((f) => {
      const kind = kindOf(f.type).kind;
      const id = `new-${f.name}`;
      let input;
      if (kind === 'bool') {
        input = h('select', { id }, h('option', { value: '' }, 'null'), h('option', { value: 'true' }, 'true'), h('option', { value: 'false' }, 'false'));
      } else {
        input = h(multiline(f.type) ? 'textarea' : 'input', {
          id,
          spellcheck: 'false',
          autocomplete: 'off',
          placeholder: placeholder(kind, f),
        });
      }
      inputs.push({ f, input, kind });
      return h(
        'div',
        { class: 'form-row' },
        h('label', { for: id }, h('span', { class: 'form-name' }, f.name), h('span', { class: 'form-type' }, f.required ? `${f.type}, required` : f.type)),
        input,
      );
    });
  const preview = h('pre', { class: 'statement' });
  const problem = h('p', { class: 'form-error', role: 'alert' });
  const build = () => {
    const doc = {};
    for (const { f, input, kind } of inputs) {
      const v = input.value;
      // A box left empty is left out, so its field is null: a form cannot
      // tell an empty text typed on purpose from a box not filled in.
      if (v === '' || (kind !== 'text' && v.trim() === '')) continue;
      doc[f.name] = readInput(v, f.type, f.name);
    }
    return insertRow(collection, doc);
  };
  const update = () => {
    try {
      fill(preview, shown(build()));
      fill(problem);
    } catch (e) {
      fill(problem, explain(e));
    }
  };
  const go = h('button', { type: 'submit', class: 'btn primary' }, 'Insert row');
  const cancel = h('button', { type: 'button', class: 'btn', onclick: () => d.close() }, 'Cancel');
  const form = h(
    'form',
    {
      class: 'form',
      method: 'dialog',
      oninput: update,
      onsubmit: async (e) => {
        e.preventDefault();
        let stmt;
        try {
          stmt = build();
        } catch (err) {
          fill(problem, explain(err));
          return;
        }
        go.disabled = true;
        const why = await submit(stmt);
        go.disabled = false;
        if (why) fill(problem, why);
        else d.close();
      },
    },
    h('div', { class: 'form-fields' }, rows),
    h('p', { class: 'form-note' }, 'Empty boxes are left out, so those fields are null. The server gives the row its id.'),
    preview,
    problem,
    h('div', { class: 'dialog-actions' }, cancel, go),
  );
  const d = dialog(`New row in ${collection}`, form);
  update();
  inputs[0]?.input.focus();
  return d;
}

function placeholder(kind, f) {
  return (
    {
      int: '42',
      float: '3.14',
      timestamp: '2026-10-05T12:00:00Z',
      json: '{"key": "value"}',
      vector: `[${Array.from({ length: Math.min(3, kindOf(f.type).dim) }, () => '0.1').join(', ')}${kindOf(f.type).dim > 3 ? ', ...' : ''}]`,
      list: '["a", "b"]',
      bytes: '[104, 105]',
      sparse: '{1:0.5,3:0.25}/100',
    }[kind] ?? ''
  );
}

/** The keys, written out. */
export function help() {
  const keys = [
    ['1 to 5', 'Rows, query, schema, live, admin'],
    ['/', 'Filter with a where clause'],
    ['Alt+1, Alt+2', 'Collections, the view'],
    ['Ctrl+Enter, ⌘Enter', 'Run the query (in the editor)'],
    ['Arrows, Page Up, Page Down', 'Move between cells'],
    ['Ctrl+Home, Ctrl+End', 'First row, last row'],
    ['Enter or F2', 'Edit the cell'],
    ['Space', 'Show the whole row'],
    ['C', 'Copy the row as JSON'],
    ['S', 'Sort by the column, where its index orders it'],
    ['N', 'New row'],
    ['Delete', 'Delete the row, after you confirm'],
    ['R', 'Read the rows again'],
    ['?', 'These keys'],
  ];
  const d = dialog(
    'Keys',
    h('dl', { class: 'keys' }, keys.map(([k, what]) => [h('dt', {}, h('kbd', {}, k)), h('dd', {}, what)])),
    h('div', { class: 'dialog-actions' }, h('button', { type: 'button', class: 'btn', onclick: () => d.close() }, 'Close')),
  );
}
