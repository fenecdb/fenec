// highlight.js is written by site/build.py (`make studio-highlight`): the
// docs' FenecQL rules, the same file fenec studio's editor colours by.

/** `code` as runs of `[kind, text]`, kind `t-kw`, `t-string`, ... or `''`. */
export function tokens(code: string, lang: string): Array<[string, string]>;
export function highlight(code: string, lang: string): string;
export function overlay(area: HTMLTextAreaElement, lang?: string): () => void;
