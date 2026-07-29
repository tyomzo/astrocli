import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';

import { App } from './App';
// Imported for its module-scope `beforeinstallprompt` listener, which has to be installed before
// Chrome fires the event — see the file for why that cannot be a hook.
import './lib/pwa/installPrompt';
import { registerServiceWorker } from './lib/pwa/serviceWorker';
import './styles/app.css';

const root = document.getElementById('root');
if (root === null) {
  throw new Error('index.html is missing #root');
}

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);

registerServiceWorker();
