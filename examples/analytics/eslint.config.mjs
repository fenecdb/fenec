import js from '@eslint/js';
import ts from 'typescript-eslint';

export default ts.config(
  { ignores: ['node_modules/**', 'data/**', 'public/**', '.lighthouseci/**'] },
  js.configs.recommended,
  ...ts.configs.recommended,
  {
    rules: {
      '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_', varsIgnorePattern: '^_' }],
    },
  },
  // Lighthouse CI's config and its summary run in Node as plain JavaScript.
  {
    files: ['**/*.cjs', '**/*.mjs'],
    languageOptions: { globals: { process: 'readonly', module: 'writable', console: 'readonly', URL: 'readonly' } },
  },
);
