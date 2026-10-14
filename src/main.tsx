import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';

import './styles/tokens.css';
import './styles/base.css';
import './styles/ui.css';

import { App } from './App';

// Browser preview: outside the Tauri shell in dev, serve sample data instead of IPC.
if (import.meta.env.DEV && !('__TAURI_INTERNALS__' in window)) {
  const { installMockBackend } = await import('./dev/mockBackend');
  installMockBackend();
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
