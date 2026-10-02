import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { FenecProvider } from '@fenecdb/react';
import { db } from './db.js';
import { App } from './App.js';

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <FenecProvider db={db}>
      <App />
    </FenecProvider>
  </StrictMode>,
);
