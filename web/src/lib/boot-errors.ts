/**
 * Global error surface. CSP forbids the classic inline-script
 * fallback, and an iPhone has no devtools — so uncaught errors and
 * rejections paint a fixed banner instead of dying invisibly. This
 * module MUST be main.ts's first import: imports evaluate in order,
 * so only a first import sees a crash inside the app's own module
 * graph. Screens still own their designed error states; this is the
 * undesigned-catastrophe lane.
 */

const surface = (message: string) => {
  const existing = document.getElementById('boot-error');
  if (existing !== null) return; // first error wins; a cascade repeats it
  const p = document.createElement('p');
  p.id = 'boot-error';
  p.style.cssText =
    'position: fixed; top: 0; left: 0; right: 0; z-index: 9999; margin: 0; ' +
    'padding: 10px 16px; background: #7f1d1d; color: #fff; ' +
    'font: 12px monospace; overflow-wrap: anywhere;';
  // @wc-ignore — pre-catalog by definition; the catalog may be what failed.
  p.textContent = `datboi hit an unhandled error — ${message}`;
  (document.body ?? document.documentElement).appendChild(p);
};

window.addEventListener('error', (e) => surface(String(e.error ?? e.message)));
window.addEventListener('unhandledrejection', (e) => surface(String(e.reason)));
