// FIRST import on purpose: registers the global error surface before
// the rest of the module graph evaluates (see the module comment).
import './lib/boot-errors';

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

const app = mount(App, { target: document.getElementById('app')! });

export default app;
