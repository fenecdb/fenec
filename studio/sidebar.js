// The collections: a tree of them, each with its row count and its fields,
// each field with its type and index. Arrow keys walk it as any tree: up and
// down between what is shown, right to open a collection's fields, left to
// close them, Enter to show its rows.

import { h, fill, bytes, number } from './dom.js';

/** `@hash`, `@sorted`, `@hnsw(cosine, m=16)`, as the schema names them. */
export function indexLabel(index) {
  if (!index) return null;
  const name = index.replace(/\(.*$/, '');
  const known = { hash: '@hash', unique: '@unique', sorted: '@sorted', text: '@text', hnsw: '@hnsw', inverted: '@inverted', ttl: '@ttl' };
  return known[name] ? `${known[name]}${index.slice(name.length)}` : `@${index}`;
}

export class Sidebar {
  /** `open(name)` shows a collection's rows. */
  constructor(host, open) {
    this.open = open;
    this.collections = [];
    this.counts = {};
    this.stats = null;
    this.expanded = new Set();
    this.current = null;
    this.focusName = null;
    this.query = '';
    this.search = h('input', {
      class: 'side-search',
      type: 'search',
      placeholder: 'Find a collection',
      'aria-label': 'Find a collection',
      spellcheck: 'false',
      autocomplete: 'off',
      oninput: () => {
        this.query = this.search.value.trim().toLowerCase();
        this.render();
      },
      onkeydown: (e) => {
        if (e.key === 'ArrowDown' || e.key === 'Enter') {
          e.preventDefault();
          const first = this.tree.querySelector('[role=treeitem]');
          if (first) {
            if (e.key === 'Enter') this.open(first.dataset.name);
            else first.focus();
          }
        }
      },
    });
    this.tree = h('ul', { class: 'tree-list', role: 'tree', 'aria-label': 'Collections', onkeydown: (e) => this.#key(e) });
    this.foot = h('div', { class: 'side-foot' });
    this.el = h('nav', { class: 'side', 'aria-label': 'Collections' }, this.search, this.tree, this.foot);
    host.append(this.el);
  }

  show({ collections, counts, stats }) {
    this.collections = collections;
    this.counts = counts ?? {};
    this.stats = stats ?? null;
    this.render();
  }

  select(name) {
    this.current = name;
    this.render();
  }

  /** The tree drawn again, the item that had the focus keeping it. */
  render() {
    const had = this.el.contains(document.activeElement) && document.activeElement?.dataset?.key;
    const shown = this.collections.filter((c) => !this.query || c.name.toLowerCase().includes(this.query));
    const items = shown.map((c) => {
      const open = this.expanded.has(c.name);
      const count = this.counts[c.name];
      const size = this.stats?.data?.[c.name];
      const dead = this.stats?.dead?.[c.name];
      const item = h(
        'li',
        {
          role: 'treeitem',
          class: `coll${c.name === this.current ? ' current' : ''}`,
          tabindex: (had ? had === `c:${c.name}` : c.name === (this.current ?? shown[0]?.name)) ? '0' : '-1',
          'aria-expanded': open ? 'true' : 'false',
          'aria-selected': c.name === this.current ? 'true' : 'false',
          dataset: { key: `c:${c.name}`, name: c.name },
        },
        h(
          'div',
          { class: 'coll-line', onclick: () => this.open(c.name) },
          h('button', {
            type: 'button',
            class: 'twisty',
            tabindex: '-1',
            'aria-label': open ? `Hide the fields of ${c.name}` : `Show the fields of ${c.name}`,
            onclick: (e) => {
              e.stopPropagation();
              this.#toggle(c.name);
            },
          }),
          h('span', { class: 'coll-name' }, c.name),
          h('span', { class: 'coll-count', title: 'Rows your token can read' }, count === undefined ? '' : count === null ? '–' : number(count)),
        ),
        size !== undefined ? h('div', { class: 'coll-size' }, `${bytes(size)} live`, dead ? `, ${bytes(dead)} dead` : '') : null,
      );
      if (open) {
        const group = h(
          'ul',
          { role: 'group', class: 'fields' },
          c.fields.map((f) =>
            h(
              'li',
              { role: 'treeitem', tabindex: had === `f:${c.name}.${f.name}` ? '0' : '-1', class: 'field', dataset: { key: `f:${c.name}.${f.name}`, name: c.name } },
              h('span', { class: 'field-name' }, f.name),
              h('span', { class: 'field-type' }, f.type, f.collate ? ` collate ${f.collate}` : '', f.required ? ' required' : ''),
              f.index ? h('span', { class: 'field-index' }, indexLabel(f.index)) : null,
            ),
          ),
        );
        item.append(group);
      }
      return item;
    });
    fill(this.tree, items.length ? items : h('li', { class: 'side-empty' }, this.collections.length ? 'No collection by that name.' : 'No collections your token can read.'));
    if (had) this.tree.querySelector(`[data-key="${CSS.escape(had)}"]`)?.focus();
    fill(
      this.foot,
      this.stats?.file != null
        ? h(
            'dl',
            { class: 'store' },
            h('dt', {}, 'File'),
            h('dd', {}, bytes(this.stats.file)),
            h('dt', {}, 'Reclaimable'),
            h('dd', {}, bytes(this.stats.reclaimable ?? 0)),
          )
        : null,
    );
  }

  #toggle(name) {
    if (this.expanded.has(name)) this.expanded.delete(name);
    else this.expanded.add(name);
    const key = `c:${name}`;
    this.render();
    this.tree.querySelector(`[data-key="${CSS.escape(key)}"]`)?.focus();
  }

  focus() {
    const el = this.tree.querySelector('[tabindex="0"]') ?? this.tree.querySelector('[role=treeitem]');
    el?.focus();
  }

  #key(e) {
    const items = [...this.tree.querySelectorAll('[role=treeitem]')];
    const at = items.indexOf(document.activeElement);
    if (at < 0) return;
    const el = items[at];
    const isColl = el.classList.contains('coll');
    const move = (to) => {
      const next = items[Math.max(0, Math.min(items.length - 1, to))];
      if (!next) return;
      for (const i of items) i.tabIndex = -1;
      next.tabIndex = 0;
      next.focus();
    };
    const act = {
      ArrowDown: () => move(at + 1),
      ArrowUp: () => (at === 0 ? this.search.focus() : move(at - 1)),
      Home: () => move(0),
      End: () => move(items.length - 1),
      ArrowRight: () => (isColl && !this.expanded.has(el.dataset.name) ? this.#toggle(el.dataset.name) : move(at + 1)),
      ArrowLeft: () => {
        if (isColl && this.expanded.has(el.dataset.name)) this.#toggle(el.dataset.name);
        else if (!isColl) move(items.indexOf(el.parentElement.closest('.coll')));
      },
      Enter: () => this.open(el.dataset.name),
    }[e.key];
    if (act) {
      e.preventDefault();
      act();
    }
  }
}
