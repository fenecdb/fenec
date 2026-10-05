import js from '@eslint/js';
import ts from 'typescript-eslint';

// public/app.js runs in the browser as it is written, with no build step.
const browser = Object.fromEntries(
  [
    'document', 'window', 'location', 'history', 'navigator', 'sessionStorage', 'fetch', 'setTimeout', 'clearTimeout',
    'crypto', 'CSS', 'FormData', 'URLSearchParams', 'TextDecoder', 'Node', 'confirm', 'prompt', 'console', 'AbortController',
  ].map((g) => [g, 'readonly']),
);

export default ts.config(
  { ignores: ['node_modules/**', 'data/**'] },
  js.configs.recommended,
  ...ts.configs.recommended,
  {
    rules: {
      '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_', varsIgnorePattern: '^_' }],
    },
  },
  { files: ['public/**/*.js'], languageOptions: { globals: browser } },
);
