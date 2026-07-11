<script lang="ts">
  /**
   * Owner chrome header (spec §2.1): logo disc (frog slot), wordmark,
   * nav pills, then theme toggle · health chip · avatar. Nav set per
   * the recorded ruling: Library · Views · Ingest · Storage · Admin,
   * with audit highlighting Library (it is the Library drill-down).
   */
  import { storage } from '../api/client';
  import { prefs } from '../prefs.svelte';
  import { router, type Route } from '../router.svelte';
  import { session } from '../session.svelte';
  import Link from './Link.svelte';
  import ThemeToggle from './ThemeToggle.svelte';
  import logoUrl from '../assets/logo.svg';

  const items: { href: string; screens: Route['screen'][] }[] = [
    { href: '/', screens: ['library', 'audit'] },
    { href: '/views', screens: ['views'] },
    { href: '/ingest', screens: ['ingest'] },
    { href: '/storage', screens: ['storage'] },
    { href: '/admin', screens: ['admin'] },
  ];

  const active = $derived(items.find((i) => i.screens.includes(router.route.screen))?.href);

  // Health chip: quarantine count from /v1/storage (wireframe 2a: the
  // chip links to Storage). Owner-only endpoint; a friend (or an error)
  // just gets no chip rather than a lie.
  let warnCount = $state<number | null>(null);
  $effect(() => {
    storage().then(
      (body) => (warnCount = body.quarantine.count),
      () => (warnCount = null),
    );
  });

  // Avatar shows only for named users; loopback callers have no user row.
  const initial = $derived(session.username?.slice(0, 1) ?? null);
</script>

<header>
  <!-- Brand mark; the adjacent wordmark carries the name, so the frog is
       decorative (empty alt) and not a second home link. -->
  <img class="logo" src={logoUrl} alt="" width="30" height="30" />
  <!-- The wordmark is the brand, not copy. -->
  <!-- @wc-ignore -->
  <Link href="/" class="wordmark">datboi</Link>
  <nav>
    <Link href="/" class="nav-item {active === '/' ? 'nav-active' : ''}">Library</Link>
    <!-- "Views" = compiled shelves (D33), not UI views (spec §6). -->
    <Link href="/views" class="nav-item {active === '/views' ? 'nav-active' : ''}">
      <!-- @wc-context: compiled shelf -->Views
    </Link>
    <Link href="/ingest" class="nav-item {active === '/ingest' ? 'nav-active' : ''}">Ingest</Link>
    <Link href="/storage" class="nav-item {active === '/storage' ? 'nav-active' : ''}">
      Storage
    </Link>
    <Link href="/admin" class="nav-item {active === '/admin' ? 'nav-active' : ''}">Admin</Link>
  </nav>
  <div class="right">
    <ThemeToggle />
    {#if warnCount !== null}
      <Link href="/storage" class="health">
        <span class="health-dot"></span>
        <span>healthy</span>
        {#if warnCount > 0}
          <span class="health-warn">· {warnCount.toLocaleString()}⚠</span>
        {/if}
      </Link>
    {/if}
    {#if initial !== null}
      <span class="avatar" title={session.username}>{initial}</span>
    {/if}
  </div>
</header>

<style>
  header {
    display: flex;
    align-items: center;
    gap: 20px;
    padding: 10px 28px;
    border-bottom: 2px solid var(--ink);
    background: var(--bg);
    font-size: 13px;
    font-weight: 600;
  }

  .logo {
    width: 30px;
    height: 30px;
    flex: none;
  }

  header :global(a.wordmark) {
    font: 800 15px var(--font-display);
    letter-spacing: -0.02em;
    color: var(--text);
    text-decoration: none;
  }

  nav {
    display: flex;
    align-items: center;
    gap: 2px;
  }

  nav :global(a.nav-item) {
    padding: 3px 12px;
    color: var(--faint);
    text-decoration: none;
    border: 2px solid transparent;
    border-radius: var(--r-pill);
  }

  nav :global(a.nav-active) {
    border-color: var(--ink);
    background: var(--ink);
    color: var(--bg);
  }

  .right {
    margin-left: auto;
    display: flex;
    align-items: center;
    gap: 14px;
  }

  .right :global(a.health) {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font: 600 11.5px var(--font-data);
    color: var(--mut);
    text-decoration: none;
  }

  .health-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--ok);
  }

  .health-warn {
    color: var(--warnT);
  }

  .avatar {
    width: 30px;
    height: 30px;
    border-radius: 50%;
    background: var(--panel2);
    border: 2px solid var(--ink);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    font: 600 13px var(--font-display);
  }
</style>
