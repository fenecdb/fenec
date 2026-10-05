// A value as the grid shows it, as the inspector unfolds it, and as an
// edit reads it back. A vector is summarised -- its dimension, its length
// and its first components -- and never written out whole: 768 numbers in a
// cell are noise, and a page of them is megabytes of text.

import { h } from './dom.js';
import { kindOf, StatementError } from './statements.js';

const FIRST = 3;

/** `‖v‖`, in the order a reader would sum it. */
export function norm(v) {
  let s = 0;
  for (const x of v) s += x * x;
  return Math.sqrt(s);
}

const num = (x) => (Number.isInteger(x) ? String(x) : x.toPrecision(3).replace(/\.?0+$/, '').replace(/^-/, '−'));

/** A vector in one line: `dim 768  norm 1.00  0.012 −0.334 0.051 …`. */
export function vectorSummary(v) {
  const first = v.slice(0, FIRST).map(num).join(' ');
  return `dim ${v.length}  norm ${norm(v).toFixed(2)}  ${first}${v.length > FIRST ? ' …' : ''}`;
}

/** The one line a cell shows. */
export function cellText(value, type) {
  if (value === null || value === undefined) return null;
  const { kind } = kindOf(type);
  if (kind === 'vector' && Array.isArray(value)) return vectorSummary(value);
  if (kind === 'bytes' && Array.isArray(value)) return `${value.length} B  ${value.slice(0, 8).map((b) => b.toString(16).padStart(2, '0')).join(' ')}${value.length > 8 ? ' …' : ''}`;
  if (typeof value === 'object') return compact(value);
  return String(value);
}

/** JSON in one line, cut once it is longer than a cell can show. */
function compact(value) {
  const s = JSON.stringify(value);
  return s.length > 160 ? `${s.slice(0, 157)}…` : s;
}

/**
 * A value unfolded: objects and lists as `<details>` a level each, the
 * first two open, a vector as its summary and a list of its components
 * that opens on request. Built of text nodes alone.
 */
export function tree(value, type, depth = 0) {
  const { kind } = kindOf(type ?? '');
  if (kind === 'vector' && Array.isArray(value)) {
    return h(
      'details',
      { class: 'tree vec' },
      h('summary', {}, vectorSummary(value)),
      h('div', { class: 'vec-all' }, value.map(num).join('  ')),
    );
  }
  if (value === null || typeof value !== 'object') return leaf(value);
  const entries = Array.isArray(value) ? value.map((v, i) => [i, v]) : Object.entries(value);
  const open = depth < 2 && entries.length <= 50;
  const label = Array.isArray(value) ? `list of ${entries.length}` : `object of ${entries.length}`;
  const d = h('details', { class: 'tree', open }, h('summary', {}, label));
  for (const [k, v] of entries) {
    d.append(h('div', { class: 'tree-row' }, h('span', { class: 'tree-key' }, String(k)), tree(v, null, depth + 1)));
  }
  return d;
}

function leaf(v) {
  if (v === null || v === undefined) return h('span', { class: 'v-null' }, 'null');
  if (typeof v === 'string') return h('span', { class: 'v-text' }, JSON.stringify(v));
  return h('span', { class: `v-${typeof v}` }, String(v));
}

/** A value as an edit shows it: JSON for anything with a structure. */
export function editText(value, type) {
  if (value === null || value === undefined) return '';
  const { kind } = kindOf(type);
  if (['json', 'vector', 'list', 'bytes'].includes(kind) || typeof value === 'object') return JSON.stringify(value);
  return String(value);
}

/** Whether a field edits in a box of several lines. */
export const multiline = (type) => ['json', 'vector', 'list', 'bytes'].includes(kindOf(type).kind);

/**
 * What a person typed, as the field's type holds it. An empty box is null
 * for every type but text, where it is the empty text: `null` is typed out
 * there, or picked with the form's null switch.
 */
export function readInput(text, type, field) {
  const { kind, dim } = kindOf(type);
  const t = text.trim();
  if (kind !== 'text' && t === '') return null;
  const json = () => {
    try {
      return JSON.parse(t);
    } catch {
      throw new StatementError(`${field} takes JSON here, and this is not JSON`);
    }
  };
  switch (kind) {
    case 'text':
      return text;
    case 'int':
      if (!/^-?\d+$/.test(t) || !Number.isSafeInteger(Number(t))) throw new StatementError(`${field} holds whole numbers`);
      return Number(t);
    case 'float': {
      const n = Number(t);
      if (!Number.isFinite(n)) throw new StatementError(`${field} holds numbers`);
      return n;
    }
    case 'bool':
      if (t === 'true' || t === 'false') return t === 'true';
      throw new StatementError(`${field} is true or false`);
    case 'timestamp':
      if (Number.isNaN(Date.parse(t))) throw new StatementError(`${field} holds a time, as 2026-10-05T12:00:00Z`);
      return t;
    case 'vector': {
      const v = json();
      if (!Array.isArray(v) || v.length !== dim || !v.every(Number.isFinite)) {
        throw new StatementError(`${field} holds ${dim} numbers, as [0.1, 0.2, ...]`);
      }
      return v;
    }
    case 'bytes': {
      const v = json();
      if (!Array.isArray(v) || !v.every((b) => Number.isInteger(b) && b >= 0 && b < 256)) {
        throw new StatementError(`${field} holds bytes, as [104, 105]`);
      }
      return v;
    }
    case 'list': {
      const v = json();
      if (!Array.isArray(v)) throw new StatementError(`${field} holds a list, as ["a", "b"]`);
      return v;
    }
    case 'json':
      return json();
    default:
      return t;
  }
}
