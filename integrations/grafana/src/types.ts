import { DataSourceJsonData } from '@grafana/data';
import { DataQuery } from '@grafana/schema';

/** The type a parameter's value is read as, in the backend (bind.go). */
export type ParamType = 'text' | 'number' | 'int' | 'float' | 'bool' | 'timestamp' | 'json';

export const PARAM_TYPES: ParamType[] = ['text', 'number', 'int', 'float', 'bool', 'timestamp', 'json'];

/** `$1`, `$2`, ...: a literal of its type, or one variable whole (`$sym`). */
export interface Param {
  type: ParamType;
  value: string;
}

export type Format = 'auto' | 'table' | 'time_series';

/**
 * A panel's query: FenecQL, its parameters, and how its answer is framed.
 * `vars` is never typed or saved: `applyTemplateVariables` fills it with the
 * values of the dashboard variables the text names, which the backend binds
 * as parameters.
 */
export interface FenecQuery extends DataQuery {
  queryText?: string;
  params?: Param[];
  format?: Format;
  /** A change the server must hold before it answers (Fenec-After). */
  after?: string;
  vars?: Record<string, string | string[]>;
}

export const DEFAULT_QUERY: Partial<FenecQuery> = {
  queryText: '',
  params: [],
  format: 'auto',
};

/** A data source's settings; the URL is the data source's own field. */
export interface FenecOptions extends DataSourceJsonData {
  tenant?: string;
  /** How long a query sent after a change waits for it (Fenec-Wait), ms. */
  waitMs?: number;
  timeoutSeconds?: number;
}

/** Kept by Grafana encrypted, sent to the backend alone, never to a browser. */
export interface FenecSecureData {
  token?: string;
}

/** What the backend's explain resource answers. */
export interface Explained {
  statement: string;
  plan: string[];
}
