import type { Monaco, monacoTypes } from '@grafana/ui';

import { tokens } from './highlight';

export const LANGUAGE = 'fenecql';

/**
 * The names the backend binds itself, from the request: Grafana's range and
 * interval, in milliseconds.
 */
export const BUILTINS = ['__from', '__to', '__interval_ms', '__max_data_points'];

/** `$sym`, `${sym}`, `${sym:int}`: a name the plugin binds as a value. */
export const VARIABLE = /\$(?:\{([A-Za-z_]\w*)(?::\w*)?\}|([A-Za-z_]\w*))/g;

// The docs' classes, as Grafana's Monaco themes colour them: `predefined`
// is the purple Grafana gives names its queries take, here a parameter or a
// variable.
const SCOPES: Record<string, string> = {
  't-kw': 'keyword',
  't-type': 'type',
  't-string': 'string',
  't-comment': 'comment',
  't-num': 'number',
  't-param': 'predefined',
  't-anno': 'tag',
};

/**
 * A line's tokens as Monaco takes them: the docs' rules (no FenecQL token
 * spans a line, which site/test_highlight.py holds), and a variable's name
 * where a plain run holds one -- never inside a string or a comment.
 */
export function lineTokens(line: string): Array<{ startIndex: number; scopes: string }> {
  const kinds: string[] = new Array(line.length).fill('');
  let pos = 0;
  for (const [kind, text] of tokens(line, LANGUAGE)) {
    kinds.fill(SCOPES[kind] ?? '', pos, pos + text.length);
    pos += text.length;
  }
  for (const m of line.matchAll(VARIABLE)) {
    const at = m.index ?? 0;
    // A `$` the rules left plain: in a string or a comment it is theirs.
    if (kinds[at] === '') {
      kinds.fill('predefined', at, at + m[0].length);
    }
  }
  const out: Array<{ startIndex: number; scopes: string }> = [];
  for (let i = 0; i < kinds.length; i++) {
    if (i === 0 || kinds[i] !== kinds[i - 1]) {
      out.push({ startIndex: i, scopes: kinds[i] });
    }
  }
  return out;
}

let registered = false;

/** Registers FenecQL with Monaco, once a page. */
export function registerFenecQL(monaco: Monaco) {
  if (registered || monaco.languages.getLanguages().some((l) => l.id === LANGUAGE)) {
    registered = true;
    return;
  }
  registered = true;
  monaco.languages.register({ id: LANGUAGE });
  const state: monacoTypes.languages.IState = {
    clone: () => state,
    equals: () => true,
  };
  monaco.languages.setTokensProvider(LANGUAGE, {
    getInitialState: () => state,
    tokenize: (line: string) => ({ tokens: lineTokens(line), endState: state }),
  });
  monaco.languages.setLanguageConfiguration(LANGUAGE, {
    comments: { lineComment: '--' },
    brackets: [
      ['(', ')'],
      ['[', ']'],
      ['{', '}'],
    ],
    autoClosingPairs: [
      { open: '(', close: ')' },
      { open: '[', close: ']' },
      { open: '{', close: '}' },
      { open: '"', close: '"', notIn: ['string'] },
    ],
  });
}
