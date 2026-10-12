import next from 'eslint-config-next/core-web-vitals';
import ts from 'eslint-config-next/typescript';

const config = [
  { ignores: ['.next/**', 'node_modules/**', 'data/**', '.lighthouseci/**', 'next-env.d.ts'] },
  ...next,
  ...ts,
  {
    rules: {
      // Plain <a> on purpose: every link is a full page the server sends,
      // so a page needs no router in the browser to navigate.
      '@next/next/no-html-link-for-pages': 'off',
      '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_', varsIgnorePattern: '^_' }],
    },
  },
];

export default config;
