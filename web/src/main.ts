// Self-hosted fonts (spec §1.2) via @fontsource — the UI must work offline on
// a NAS, so no font CDN. Bricolage `opsz.css` = the opsz+wght variable axes
// (display face); Plex Mono ships as static 400/500/600 (data face).
import '@fontsource-variable/bricolage-grotesque/opsz.css';
import '@fontsource/ibm-plex-mono/400.css';
import '@fontsource/ibm-plex-mono/500.css';
import '@fontsource/ibm-plex-mono/600.css';
import './lib/tokens.css';

import { mount } from 'svelte';
import App from './App.svelte';

// A boot-time crash on an untested browser renders as a silent white
// page (CSP forbids the inline-script fallback pattern), which is
// undiagnosable from a phone. If the shell never rendered, put the
// error text where the app would have been. Runtime errors after a
// successful mount stay out of this — screens own their error states.
const surfaceBootError = (message: string) => {
  const target = document.getElementById('app');
  if (target === null || target.childElementCount > 0) return;
  const p = document.createElement('p');
  p.style.cssText = 'font: 13px monospace; padding: 24px; overflow-wrap: anywhere;';
  // @wc-ignore — pre-catalog by definition; the catalog may be what failed.
  p.textContent = `datboi failed to start — ${message}`;
  target.appendChild(p);
};
window.addEventListener('error', (e) => surfaceBootError(String(e.error ?? e.message)));
window.addEventListener('unhandledrejection', (e) => surfaceBootError(String(e.reason)));

const app = mount(App, { target: document.getElementById('app')! });

export default app;
