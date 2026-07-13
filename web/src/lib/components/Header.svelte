<script lang="ts">
  /**
   * Owner chrome header (spec §2.1): logo disc (frog slot), wordmark,
   * nav pills, then health chip · avatar. Nav set per the recorded
   * ruling: Library · Views · Ingest · Storage · Admin, with audit
   * highlighting Library (it is the Library drill-down). No theme
   * toggle — color follows the system, D78.
   */
  import { storage } from '../api/client';
  import { registry, runningCount, trackRegistry } from '../activity.svelte';
  import { router, type Route } from '../router.svelte';
  import { session } from '../session.svelte';
  import Link from './Link.svelte';
  import logoUrl from '../assets/logo.svg';
  import { jobsSignal } from '../jobs.svelte';
  import { plural } from '../plural';

  /**
   * Every screen classifies into an owner-nav section — EXHAUSTIVE over
   * Route['screen'], so adding a route variant fails check until it's
   * placed. null = screens outside the owner nav: the open pages
   * (login/invite render without this header), the friend shelf (owner
   * chrome shows it as notfound), and notfound itself.
   */
  const NAV_SECTION: Record<Route['screen'], '/' | '/views' | '/ingest' | '/storage' | '/admin' | null> = {
    library: '/',
    audit: '/', // the Library drill-down (nav ruling, router.svelte.ts)
    views: '/views',
    ingest: '/ingest',
    storage: '/storage',
    blob: '/storage', // the Storage drill-down
    activity: null, // header-chrome drill-down, no nav pill of its own
    admin: '/admin',
    login: null,
    invite: null,
    browse: null, // friend chrome owns it
    notfound: null,
  };

  const active = $derived(NAV_SECTION[router.route.screen]);

  // Health chip: quarantine count from /v1/storage (wireframe 2a: the
  // chip links to Storage). Owner-only endpoint; a friend (or an error)
  // just gets no chip rather than a lie.
  let warnCount = $state<number | null>(null);
  $effect(() => {
    // A finished job (scrub, ingest) is exactly when quarantine can
    // change — the signal keeps a session-long header from lying.
    void jobsSignal.version;
    storage().then(
      (body) => (warnCount = body.quarantine.count),
      () => (warnCount = null),
    );
  });

  // Avatar shows only for named users; loopback callers have no user row.
  const initial = $derived(session.username?.slice(0, 1) ?? null);

  // The ONE registry poll loop (D82) lives with the header because the
  // header is mounted on every owner screen — including /activity,
  // which reads the same snapshot.
  trackRegistry();
  const running = $derived(runningCount());
</script>

<header>
  <!-- One home link wearing both the frog and the wordmark: the frog
       stays decorative (empty alt — the wordmark carries the name) and
       a screen reader hears one "/" link, not two adjacent ones. -->
  <Link href="/" class="brand">
    <img class="logo" src={logoUrl} alt="" width="30" height="30" />
    <!-- The wordmark is the brand, not copy. -->
    <!-- @wc-ignore -->
    <span class="wordmark">datboi</span>
  </Link>
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
    <!-- Activity indicator (D82): loud only while something runs —
         management by exception — but always a way into the history. -->
    <Link href="/activity" class="activity {running > 0 ? 'activity-live' : ''}">
      {#if running > 0}
        <span class="pulse"></span>
        <span>{plural(running, ['# job', '# jobs'])}</span>
        {#if registry.unreachable}
          <span class="activity-warn">?</span>
        {/if}
      {:else}
        activity
      {/if}
    </Link>
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
    /* Wrap-capable at EVERY width: between the desktop layout fitting
       and the ≤720px swipe strip there is a band (wider in longer
       locales) where min-content beats the viewport — wrapping there
       beats forcing the whole page to scroll sideways. */
    flex-wrap: wrap;
    align-items: center;
    gap: 20px;
    padding: 10px var(--pad-x);
    border-bottom: 2px solid var(--ink);
    background: var(--bg);
    font-size: 0.8125rem;
    font-weight: 600;
  }

  .logo {
    width: 30px;
    height: 30px;
    flex: none;
  }

  /* Internal gap matches the header's own, so merging the two into
     one anchor moved no pixels. */
  header :global(a.brand) {
    display: inline-flex;
    align-items: center;
    gap: 20px;
    color: var(--text);
    text-decoration: none;
  }

  .wordmark {
    font: 800 0.9375rem var(--font-display);
    letter-spacing: -0.02em;
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
    font: 600 0.71875rem var(--font-data);
    color: var(--mut);
    text-decoration: none;
  }

  /* Idle: a quiet way into the history. Live: pulse + count. */
  .right :global(a.activity) {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font: 600 0.71875rem var(--font-data);
    color: var(--faint);
    text-decoration: none;
  }

  .right :global(a.activity-live) {
    color: var(--text);
  }

  .pulse {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--ok);
    animation: pulse 1.6s ease-in-out infinite;
  }

  @keyframes pulse {
    50% {
      opacity: 0.35;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .pulse {
      animation: none;
    }
  }

  .activity-warn {
    color: var(--warnT);
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
    font: 600 0.8125rem var(--font-display);
    flex: none;
  }

  /* Mobile chrome: the five nav pills won't share a row with the brand
     and the toggle/health/avatar cluster, so the nav drops to its own
     full-width second row and scrolls sideways — every tab still
     reachable, nothing clipped. */
  @media (max-width: 720px) {
    header {
      flex-wrap: wrap;
      gap: 10px 12px;
      padding: 8px var(--pad-x);
    }

    header :global(a.brand) {
      gap: 10px;
    }

    nav {
      order: 3;
      flex-basis: 100%;
      overflow-x: auto;
      gap: 4px;
      /* Hide the scrollbar — it's a swipe strip, not a scroll region. */
      scrollbar-width: none;
      -webkit-overflow-scrolling: touch;
    }

    nav::-webkit-scrollbar {
      display: none;
    }

    nav :global(a.nav-item) {
      flex: none;
      white-space: nowrap;
    }
  }
</style>
