/**
 * Client-side user preferences, persisted to localStorage:
 *
 * - theme (spec §1.3): `system | light | dark`, default system. A forced
 *   value sets `html[data-theme]`; system removes the attribute and lets
 *   the prefers-color-scheme media query rule (tokens.css).
 * - density (spec §1.3, audit prototype prop): comfortable/compact row
 *   padding. The prototype exposed it as a component prop; here it is a
 *   real preference with a small (undesigned) toggle in the filter rail.
 */

export type Theme = 'system' | 'light' | 'dark';
export type Density = 'comfortable' | 'compact';

const THEME_KEY = 'datboi-theme';
const DENSITY_KEY = 'datboi-density';

function load<T extends string>(key: string, valid: readonly T[], fallback: T): T {
  try {
    const stored = window.localStorage.getItem(key);
    return valid.includes(stored as T) ? (stored as T) : fallback;
  } catch {
    return fallback; // storage unavailable (privacy mode) — defaults rule
  }
}

function store(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    // best-effort persistence only
  }
}

const state = $state({
  theme: load(THEME_KEY, ['system', 'light', 'dark'] as const, 'system'),
  density: load(DENSITY_KEY, ['comfortable', 'compact'] as const, 'comfortable'),
});

function applyTheme(theme: Theme): void {
  if (theme === 'system') {
    document.documentElement.removeAttribute('data-theme');
  } else {
    document.documentElement.setAttribute('data-theme', theme);
  }
}

// Apply the persisted choice at module load, before first paint.
applyTheme(state.theme);

export const prefs = {
  get theme(): Theme {
    return state.theme;
  },
  setTheme(theme: Theme): void {
    state.theme = theme;
    store(THEME_KEY, theme);
    applyTheme(theme);
  },
  get density(): Density {
    return state.density;
  },
  setDensity(density: Density): void {
    state.density = density;
    store(DENSITY_KEY, density);
  },
};
