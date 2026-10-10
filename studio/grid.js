// The data grid: a collection's rows, as many as it holds, drawn only where
// they are seen.
//
// The rows are read a block of `BLOCK` at a time and kept by block; the
// page holds a row element for each row in view and a few beyond, moved
// as the view scrolls, so 100 000 rows cost what 40 do. In id order a block
// after one already read is asked for by its last id (`id > $n`), which
// walks the id index from there; a block reached by a jump, or under an
// order, by its offset. A browser caps an element's height (Firefox near
// 17.9 million pixels, Chrome near 33), so past `TALLEST` the scroll range
// stands for the rows in proportion rather than a pixel a pixel.

import { h, fill } from './dom.js';
import { cellText, multiline, editText } from './values.js';
import { kindOf, filterable } from './statements.js';

const ROW = 28;
const BLOCK = 100;
const OVERSCAN = 8;
const KEEP = 60;
const TALLEST = 8_000_000;
const PARALLEL = 4;

const WIDTH = {
  id: 84, int: 112, float: 120, bool: 76, timestamp: 200, text: 220, json: 240,
  vector: 300, list: 200, bytes: 180, sparse: 200, geo: 210, other: 160,
};

/** What a quick filter box suggests, by the field's type. */
const HINT = { text: 'contains', int: '= > a..b', float: '= > a..b', timestamp: '> 2026-01-01', bool: 'true, false', list: 'has' };

/** Whether a field's index orders it, so `order` walks it: `@sorted`, `@ttl`. */
export const sortable = (f) => f.name === 'id' || /^(sorted|ttl)/.test(f.index ?? '');

export class Grid {
  /**
   * `source(block, after)` reads a block: `{rows}`; `after` is the last id
   * of the block before when it is held and the view is in id order.
   * `hooks`: onSort(field), onQuick(field, text), onEdit(row, field, text),
   * onDelete(row), onCopy(row), onInspect(row), onActive(row, moved),
   * onError(err).
   */
  constructor(host, hooks) {
    this.hooks = hooks;
    this.fields = [];
    this.total = 0;
    this.blocks = new Map();
    this.loading = new Map();
    this.queue = [];
    this.active = { row: 0, col: 0 };
    this.pool = [];
    this.editing = null;
    this.epoch = 0;
    this.quick = {};

    this.scroller = h('div', { class: 'grid-scroll' });
    this.head = h('div', { class: 'grid-head', role: 'row', 'aria-rowindex': '1' });
    this.quickRow = h('div', { class: 'grid-quick', role: 'row', 'aria-rowindex': '2' });
    this.canvas = h('div', { class: 'grid-canvas', role: 'rowgroup' });
    this.empty = h('div', { class: 'grid-empty', hidden: true });
    this.scroller.append(h('div', { class: 'grid-top', role: 'rowgroup' }, this.head, this.quickRow), this.canvas, this.empty);
    this.el = h(
      'div',
      {
        class: 'grid',
        role: 'grid',
        tabindex: '0',
        'aria-label': 'Rows',
        'aria-multiselectable': 'false',
        onkeydown: (e) => this.#key(e),
        onfocus: () => this.#paintActive(),
      },
      this.scroller,
    );
    host.append(this.el);
    let queued = false;
    this.scroller.addEventListener(
      'scroll',
      () => {
        if (queued) return;
        queued = true;
        requestAnimationFrame(() => {
          queued = false;
          this.draw();
        });
      },
      { passive: true },
    );
    new ResizeObserver(() => this.draw()).observe(this.scroller);
    this.canvas.addEventListener('mousedown', (e) => this.#pick(e));
    this.canvas.addEventListener('dblclick', (e) => {
      const at = this.#cellAt(e);
      if (at) this.edit();
    });
  }

  /**
   * A new view: its fields, how many rows it has, how a block is read and
   * the order. Every block read before is let go of.
   */
  show({ fields, total, source, order, quick, readOnly, rows = null, plain = false, empty = null, mark = null }) {
    this.epoch++;
    this.fields = fields;
    // `rows`: every row held already -- a query's answer, a live shape --
    // drawn as a collection's are, with nothing to read. `plain` leaves out
    // the quick filters and the sorting, which are statements of the
    // collection's; `mark(row)` names a row's state (`data-mark`), which
    // the live view colours as it changes.
    this.local = rows;
    this.plain = plain;
    this.mark = mark;
    this.emptyText = empty;
    this.total = rows ? rows.length : total;
    total = this.total;
    this.source = source;
    this.order = order;
    this.quick = { ...quick };
    this.readOnly = readOnly || !!rows;
    this.blocks.clear();
    this.loading.clear();
    this.queue = [];
    this.#cancelEdit();
    this.widths = fields.map((f) => (f.name === 'id' ? WIDTH.id : WIDTH[kindOf(f.type).kind] ?? WIDTH.other));
    this.template = this.widths.map((w) => `${w}px`).join(' ');
    this.el.setAttribute('aria-rowcount', String(total + 2));
    this.el.setAttribute('aria-colcount', String(fields.length));
    this.active = { row: Math.min(this.active.row, Math.max(0, total - 1)), col: Math.min(this.active.col, fields.length - 1) };
    this.#header();
    for (const r of this.pool) r.el.remove();
    this.pool = [];
    this.empty.hidden = total > 0;
    fill(this.empty, total > 0 ? '' : (this.emptyText ?? 'No rows match. Clear a filter, or add a row with N.'));
    this.scroller.scrollTop = 0;
    this.draw();
  }

  /**
   * The rows held in memory, again: the view keeps its scroll and its
   * active cell, and every row in view is drawn anew, its mark with it.
   */
  setRows(rows) {
    this.local = rows;
    this.total = rows.length;
    this.el.setAttribute('aria-rowcount', String(this.total + 2));
    this.empty.hidden = this.total > 0;
    fill(this.empty, this.total > 0 ? '' : (this.emptyText ?? ''));
    for (const slot of this.pool) slot.at = -1;
    this.draw();
  }

  /** The row at `i` where it is held. */
  row(i) {
    if (this.local) return this.local[i];
    const b = this.blocks.get(Math.floor(i / BLOCK));
    return Array.isArray(b) ? b[i % BLOCK] : undefined;
  }

  get activeRow() {
    return this.row(this.active.row);
  }

  /** A row written: the copy held is replaced, and drawn again. */
  replace(i, row) {
    const b = this.blocks.get(Math.floor(i / BLOCK));
    if (Array.isArray(b)) b[i % BLOCK] = row;
    this.draw();
  }

  // ------------------------------------------------------------- the head

  #header() {
    const cells = this.fields.map((f, c) => {
      const s = !this.plain && sortable(f);
      const dir = this.order?.field === f.name ? (this.order.desc ? 'descending' : 'ascending') : s ? 'none' : null;
      const label = [h('span', { class: 'col-name' }, f.name), h('span', { class: 'col-type' }, f.type)];
      return h(
        'div',
        { class: 'cell head-cell', role: 'columnheader', 'aria-colindex': String(c + 1), 'aria-sort': dir, title: f.index ? `${f.type}  @${f.index}` : f.type },
        s
          ? h('button', { class: 'sort', type: 'button', onclick: () => this.hooks.onSort(f.name), 'aria-label': `Sort by ${f.name}` }, label, h('span', { class: 'sort-mark', 'aria-hidden': 'true' }, dir === 'ascending' ? '↑' : dir === 'descending' ? '↓' : ''))
          : label,
      );
    });
    fill(this.head, cells);
    this.head.style.gridTemplateColumns = this.template;
    const inputs = this.fields.map((f, c) => {
      const ok = filterable(f.type);
      const input = h('input', {
        class: 'quick',
        type: 'search',
        value: this.quick[f.name] ?? '',
        placeholder: ok ? HINT[kindOf(f.type).kind] : '',
        title: ok ? 'Type a value, or =, !=, >, >=, <, <=, a..b, null, !null' : 'Filter this field with the where clause',
        disabled: !ok,
        'aria-label': `Filter ${f.name}`,
        spellcheck: 'false',
        autocomplete: 'off',
      });
      let timer;
      const send = () => {
        clearTimeout(timer);
        if ((this.quick[f.name] ?? '') !== input.value) {
          this.quick[f.name] = input.value;
          this.hooks.onQuick(f.name, input.value);
        }
      };
      input.addEventListener('input', () => {
        clearTimeout(timer);
        timer = setTimeout(send, 450);
      });
      input.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') send();
        if (e.key === 'ArrowDown' || e.key === 'Escape') {
          e.preventDefault();
          this.el.focus();
        }
        e.stopPropagation();
      });
      return h('div', { class: 'cell', role: 'gridcell', 'aria-colindex': String(c + 1) }, input);
    });
    fill(this.quickRow, inputs);
    this.quickRow.hidden = this.plain;
    this.quickRow.style.gridTemplateColumns = this.template;
    const width = `${this.widths.reduce((a, b) => a + b, 0)}px`;
    this.head.style.width = width;
    this.quickRow.style.width = width;
    this.canvas.style.width = width;
  }

  // ------------------------------------------------------------- geometry

  #height() {
    return Math.min(this.total * ROW, TALLEST);
  }

  /** The first row in view, as a fraction where the range is scaled. */
  #firstAt(scrollTop) {
    const view = this.scroller.clientHeight - this.#topHeight();
    const full = this.total * ROW;
    if (full <= TALLEST) return scrollTop / ROW;
    const range = Math.max(1, this.#height() - view);
    const rows = Math.max(0, this.total - view / ROW);
    return (Math.min(scrollTop, range) / range) * rows;
  }

  #scrollFor(row) {
    const view = this.scroller.clientHeight - this.#topHeight();
    const full = this.total * ROW;
    if (full <= TALLEST) return row * ROW;
    const range = Math.max(1, this.#height() - view);
    const rows = Math.max(1, this.total - view / ROW);
    return (row / rows) * range;
  }

  #topHeight() {
    return this.head.offsetHeight + this.quickRow.offsetHeight;
  }

  // ------------------------------------------------------------- drawing

  /** The rows in view drawn, and the blocks they need asked for. */
  draw() {
    this.canvas.style.height = `${this.#height()}px`;
    const top = this.scroller.scrollTop;
    const view = Math.max(ROW, this.scroller.clientHeight - this.#topHeight());
    const firstExact = this.#firstAt(top);
    const first = Math.max(0, Math.floor(firstExact) - OVERSCAN);
    const last = Math.min(this.total - 1, Math.ceil(firstExact + view / ROW) + OVERSCAN);
    const scaled = this.total * ROW > TALLEST;
    const offset = scaled ? top - (firstExact - Math.floor(firstExact)) * ROW - Math.floor(firstExact) * ROW : 0;
    const need = last - first + 1;
    while (this.pool.length < need) {
      const el = h('div', { class: 'row', role: 'row' });
      el.style.gridTemplateColumns = this.template;
      this.canvas.append(el);
      this.pool.push({ el, at: -1, row: undefined, cells: [] });
    }
    for (let k = 0; k < this.pool.length; k++) {
      const slot = this.pool[k];
      const i = first + k;
      if (k >= need || i > last) {
        slot.el.hidden = true;
        slot.at = -1;
        continue;
      }
      slot.el.hidden = false;
      slot.el.style.transform = `translateY(${i * ROW + offset}px)`;
      const row = this.row(i);
      if (slot.at !== i || slot.row !== row) this.#paintRow(slot, i, row);
    }
    this.#paintActive();
    if (this.total > 0 && !this.local) this.#want(Math.floor(first / BLOCK), Math.floor(last / BLOCK));
  }

  #paintRow(slot, i, row) {
    slot.at = i;
    slot.row = row;
    slot.el.setAttribute('aria-rowindex', String(i + 3));
    slot.el.classList.toggle('pending', row === undefined);
    if (this.mark) slot.el.dataset.mark = (row && this.mark(row)) || '';
    if (slot.cells.length !== this.fields.length) {
      slot.cells = this.fields.map((f, c) => h('div', { class: 'cell', role: 'gridcell', 'aria-colindex': String(c + 1) }));
      slot.el.replaceChildren(...slot.cells);
      slot.el.style.gridTemplateColumns = this.template;
    }
    this.fields.forEach((f, c) => {
      const cell = slot.cells[c];
      const kind = f.name === 'id' ? 'id' : kindOf(f.type).kind;
      cell.className = `cell k-${kind}`;
      if (row === undefined) {
        cell.textContent = '';
        return;
      }
      const text = cellText(row[f.name], f.type);
      if (text === null) {
        cell.textContent = 'null';
        cell.classList.add('v-null');
      } else {
        cell.textContent = text;
      }
    });
  }

  #paintActive() {
    for (const slot of this.pool) {
      const on = slot.at === this.active.row;
      slot.el.classList.toggle('active-row', on);
      slot.el.setAttribute('aria-selected', on ? 'true' : 'false');
      slot.cells.forEach((cell, c) => {
        const here = on && c === this.active.col;
        cell.classList.toggle('active', here);
        if (here) {
          cell.id = 'grid-active';
          this.el.setAttribute('aria-activedescendant', 'grid-active');
        } else if (cell.id) cell.removeAttribute('id');
      });
    }
  }

  // ------------------------------------------------------------- reading

  #want(from, to) {
    for (let b = from; b <= to; b++) {
      if (!this.blocks.has(b) && !this.loading.has(b) && !this.queue.includes(b)) this.queue.push(b);
    }
    // The blocks in view first, nearest the top of it first: a jump leaves
    // queued blocks it scrolled past behind.
    this.queue = this.queue.filter((b) => b >= from - 1 && b <= to + 1).sort((a, b) => a - b);
    this.#pump();
    if (this.blocks.size > KEEP) {
      const mid = (from + to) / 2;
      const far = [...this.blocks.keys()].sort((a, b) => Math.abs(b - mid) - Math.abs(a - mid));
      for (const b of far.slice(0, this.blocks.size - KEEP)) this.blocks.delete(b);
    }
  }

  #pump() {
    while (this.loading.size < PARALLEL && this.queue.length) {
      const b = this.queue.shift();
      const epoch = this.epoch;
      const before = this.blocks.get(b - 1);
      const after = !this.order && Array.isArray(before) && before.length === BLOCK ? before[BLOCK - 1].id : null;
      const p = this.source(b, BLOCK, after)
        .then((rows) => {
          if (epoch !== this.epoch) return;
          this.blocks.set(b, rows);
          this.draw();
          if (b === Math.floor(this.active.row / BLOCK)) this.hooks.onActive(this.activeRow);
        })
        .catch((err) => {
          if (epoch === this.epoch) this.hooks.onError(err);
        })
        .finally(() => {
          if (epoch !== this.epoch) return;
          this.loading.delete(b);
          this.#pump();
        });
      this.loading.set(b, p);
    }
  }

  // ------------------------------------------------------------- keys

  #cellAt(e) {
    const cell = e.target.closest('.cell');
    const rowEl = e.target.closest('.row');
    if (!cell || !rowEl) return null;
    const slot = this.pool.find((s) => s.el === rowEl);
    if (!slot || slot.at < 0) return null;
    return { row: slot.at, col: slot.cells.indexOf(cell) };
  }

  #pick(e) {
    if (e.target.closest('.cell-edit')) return;
    const at = this.#cellAt(e);
    if (!at) return;
    this.#cancelEdit();
    this.moveTo(at.row, at.col);
    this.el.focus({ preventScroll: true });
  }

  /** The active cell moved to `row`, `col`, and scrolled into view. */
  moveTo(row, col = this.active.col) {
    if (this.total === 0) return;
    row = Math.max(0, Math.min(this.total - 1, row));
    col = Math.max(0, Math.min(this.fields.length - 1, col));
    const moved = row !== this.active.row;
    this.active = { row, col };
    const view = this.scroller.clientHeight - this.#topHeight();
    const firstExact = this.#firstAt(this.scroller.scrollTop);
    const rowsInView = Math.floor(view / ROW);
    if (row < firstExact) this.scroller.scrollTop = this.#scrollFor(row);
    else if (row >= firstExact + rowsInView) this.scroller.scrollTop = this.#scrollFor(row - rowsInView + 1);
    // Across too: the column's left edge, or its right one, in view.
    const left = this.widths.slice(0, col).reduce((a, b) => a + b, 0);
    const right = left + this.widths[col];
    if (left < this.scroller.scrollLeft) this.scroller.scrollLeft = left;
    else if (right > this.scroller.scrollLeft + this.scroller.clientWidth) this.scroller.scrollLeft = right - this.scroller.clientWidth;
    this.draw();
    this.hooks.onActive(this.activeRow, moved);
  }

  #key(e) {
    if (this.editing) return;
    const page = Math.max(1, Math.floor((this.scroller.clientHeight - this.#topHeight()) / ROW) - 1);
    const { row, col } = this.active;
    const mod = e.metaKey || e.ctrlKey;
    const go = {
      ArrowDown: () => this.moveTo(row + 1),
      ArrowUp: () => (row === 0 ? this.#toQuick() : this.moveTo(row - 1)),
      ArrowRight: () => this.moveTo(row, col + 1),
      ArrowLeft: () => this.moveTo(row, col - 1),
      PageDown: () => this.moveTo(row + page),
      PageUp: () => this.moveTo(row - page),
      Home: () => (mod ? this.moveTo(0, 0) : this.moveTo(row, 0)),
      End: () => (mod ? this.moveTo(this.total - 1, this.fields.length - 1) : this.moveTo(row, this.fields.length - 1)),
      Enter: () => this.edit(),
      F2: () => this.edit(),
      ' ': () => this.activeRow && this.hooks.onInspect(this.activeRow),
      Delete: () => this.activeRow && this.hooks.onDelete(this.activeRow),
      Backspace: () => this.activeRow && this.hooks.onDelete(this.activeRow),
    }[e.key];
    if (go) {
      e.preventDefault();
      go();
      return;
    }
    if ((mod && e.key === 'c') || (!mod && e.key === 'c' && !e.altKey)) {
      if (this.activeRow && window.getSelection()?.toString() === '') {
        e.preventDefault();
        this.hooks.onCopy(this.activeRow);
      }
    } else if (!mod && e.key === 's' && !this.plain) {
      const f = this.fields[col];
      if (f && sortable(f)) this.hooks.onSort(f.name);
    }
  }

  #toQuick() {
    const input = this.quickRow.querySelectorAll('input')[this.active.col];
    if (input && !input.disabled) input.focus();
  }

  // ------------------------------------------------------------- editing

  /** The active cell opened for writing, in place. */
  edit() {
    const row = this.activeRow;
    const f = this.fields[this.active.col];
    if (!row || !f || f.name === 'id' || this.readOnly) return;
    const slot = this.pool.find((s) => s.at === this.active.row);
    if (!slot) return;
    const cell = slot.cells[this.active.col];
    const many = multiline(f.type);
    const input = h(many ? 'textarea' : 'input', {
      class: 'cell-edit',
      value: editText(row[f.name], f.type),
      'aria-label': `New value of ${f.name}`,
      spellcheck: 'false',
    });
    const box = h('div', { class: `cell-editor${many ? ' tall' : ''}` }, input, h('div', { class: 'cell-hint' }, many ? 'Ctrl+Enter writes, Esc leaves it' : 'Enter writes, Esc leaves it'));
    cell.append(box);
    this.editing = { box, input, row, field: f.name, at: this.active.row };
    input.focus();
    input.select?.();
    input.addEventListener('keydown', async (e) => {
      e.stopPropagation();
      if (e.key === 'Escape') {
        e.preventDefault();
        this.#cancelEdit();
        this.el.focus();
      } else if (e.key === 'Enter' && (!many || e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        const ed = this.editing;
        input.disabled = true;
        const written = await this.hooks.onEdit(ed.row, ed.field, input.value);
        input.disabled = false;
        if (written) {
          this.#cancelEdit();
          this.replace(ed.at, written);
          this.el.focus();
        } else {
          input.focus();
        }
      }
    });
  }

  #cancelEdit() {
    if (!this.editing) return;
    this.editing.box.remove();
    this.editing = null;
  }
}
