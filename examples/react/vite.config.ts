import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  // The database opens before the first render: top-level await.
  build: { target: 'es2022' },
});
