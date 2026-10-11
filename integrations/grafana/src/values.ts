import type { DataFrame, MetricFindValue, ScopedVars } from '@grafana/data';
import type { TemplateSrv } from '@grafana/runtime';

import { BUILTINS, VARIABLE } from './fenecql/language';
import type { FenecQuery } from './types';

/**
 * The values of the dashboard variables a query names -- in its text, its
 * parameters and its After -- one, or a list for a multi-value variable,
 * as the template service holds them. Nothing is written into the text:
 * the backend makes each name a `$n` parameter, so a value is a value
 * whatever it holds.
 */
export function variableValues(
  templateSrv: TemplateSrv,
  query: FenecQuery,
  scopedVars: ScopedVars = {}
): Record<string, string | string[]> {
  const names = new Set<string>();
  const texts = [query.queryText ?? '', query.after ?? '', ...(query.params ?? []).map((p) => p.value ?? '')];
  for (const text of texts) {
    for (const m of text.matchAll(VARIABLE)) {
      const name = m[1] ?? m[2];
      // Grafana's range and interval are the backend's, from the request
      // itself -- as an alert rule has them.
      if (!BUILTINS.includes(name)) {
        names.add(name);
      }
    }
  }
  const known = new Set(templateSrv.getVariables().map((v) => v.name));
  const out: Record<string, string | string[]> = {};
  for (const name of names) {
    if (!known.has(name) && !(name in scopedVars)) {
      // Not a variable here: the backend says so by name.
      continue;
    }
    let value: unknown = '';
    // A format function is handed the value as it is -- a list for a
    // multi-value variable -- before any formatting writes it as text.
    templateSrv.replace('${' + name + '}', scopedVars, (v: unknown) => {
      value = v;
      return '';
    });
    out[name] = Array.isArray(value) ? value.map((x) => String(x)) : String(value ?? '');
  }
  return out;
}

/** A frame's values for a variable: `__text` and `__value` where named, else the first column. */
export function valuesOf(frame: DataFrame | undefined): MetricFindValue[] {
  if (!frame || frame.fields.length === 0) {
    return [];
  }
  const text = frame.fields.find((f) => f.name === '__text') ?? frame.fields[0];
  const value = frame.fields.find((f) => f.name === '__value') ?? text;
  const out: MetricFindValue[] = [];
  for (let i = 0; i < frame.length; i++) {
    const t = text.values[i];
    const v = value.values[i];
    if (t === null || t === undefined) {
      continue;
    }
    out.push({ text: String(t), value: typeof v === 'number' ? v : String(v ?? t) });
  }
  return out;
}
