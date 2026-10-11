// The browser's part of binding, in modules of its own: a test importing
// datasource.ts -- @grafana/runtime and @grafana/ui with it -- sat at 0% CPU
// with no output for ten minutes, so types alone come from them here.
import type { DataFrame } from '@grafana/data';
import type { TemplateSrv } from '@grafana/runtime';

import { lineTokens } from './fenecql/language';
import { valuesOf, variableValues } from './values';

// A dashboard's variables as Grafana's template service holds them: `sym`
// one value, `syms` several, `n` a number, `all` with every value picked.
function templates(): TemplateSrv {
  const values: Record<string, unknown> = {
    sym: 'S01" or 1 = 1 --',
    syms: ['S01', 'S02'],
    n: 12,
    all: ['a', 'b', 'c'],
  };
  return {
    getVariables: () => Object.keys(values).map((name) => ({ name }) as never),
    replace: (target?: string, scopedVars?: Record<string, { value: unknown }>, format?: unknown) => {
      const name = (target ?? '').replace(/^\$\{|\}$/g, '');
      const value = scopedVars?.[name]?.value ?? values[name];
      return typeof format === 'function' ? format(value) : String(value);
    },
    containsTemplate: () => true,
    updateTimeRange: () => {},
  } as unknown as TemplateSrv;
}

const ds = {
  applyTemplateVariables: (q: Parameters<typeof variableValues>[1], scoped: Parameters<typeof variableValues>[2]) => ({
    ...q,
    vars: variableValues(templates(), q, scoped),
  }),
};

function frame(fields: Array<{ name: string; values: unknown[] }>): DataFrame {
  return { fields: fields.map((f) => ({ ...f, type: 'other', config: {} })), length: fields[0].values.length } as DataFrame;
}

describe('the values sent beside a query', () => {
  it('holds each variable the text names, as it is, and writes nothing into the text', () => {
    const q = {
      refId: 'A',
      queryText: 'get ticks where sym = $sym and qty > ${n:int} and sym in $syms and at >= $__from -- $unknown',
    };
    const out = ds.applyTemplateVariables(q, {});
    expect(out.queryText).toBe(q.queryText);
    expect(out.vars).toEqual({ sym: 'S01" or 1 = 1 --', n: '12', syms: ['S01', 'S02'] });
  });

  it('takes a repeated panel’s value over the dashboard’s, and names in the parameters and after', () => {
    const q = { refId: 'A', queryText: 'get t where a = $1', params: [{ type: 'text' as const, value: '$sym' }], after: '${all}' };
    const out = ds.applyTemplateVariables(q, { sym: { text: 'S09', value: 'S09' } });
    expect(out.vars).toEqual({ sym: 'S09', all: ['a', 'b', 'c'] });
  });
});

describe('a variable query', () => {
  it('lists the first column, or __text and __value', () => {
    const first = frame([
      { name: 'sym', values: ['S01', 'S02', null] },
      { name: 'n', values: [3, 4, 5] },
    ]);
    expect(valuesOf(first)).toEqual([
      { text: 'S01', value: 'S01' },
      { text: 'S02', value: 'S02' },
    ]);
    const named = frame([
      { name: '__value', values: [7] },
      { name: '__text', values: ['seven'] },
    ]);
    expect(valuesOf(named)).toEqual([{ text: 'seven', value: 7 }]);
    expect(valuesOf(undefined)).toEqual([]);
  });
});

describe('the editor’s colours', () => {
  it('are the docs’ rules, and a variable’s name outside strings and comments', () => {
    const line = 'get ticks where sym = $sym and note = "$sym" and at >= $1 -- $sym';
    const scopes = lineTokens(line).map((t, i, all) => [line.slice(t.startIndex, all[i + 1]?.startIndex), t.scopes]);
    expect(scopes).toEqual([
      ['get', 'keyword'],
      [' ticks ', ''],
      ['where', 'keyword'],
      [' sym = ', ''],
      ['$sym', 'predefined'],
      [' ', ''],
      ['and', 'keyword'],
      [' note = ', ''],
      ['"$sym"', 'string'],
      [' ', ''],
      ['and', 'keyword'],
      [' at >= ', ''],
      ['$1', 'predefined'],
      [' ', ''],
      ['-- $sym', 'comment'],
    ]);
  });
});
