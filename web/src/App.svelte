<script lang="ts">
  import { loadLocale } from 'wuchale/load-utils';
  // Side-effect import: registers the wuchale catalog loaders (generated at
  // startup into src/locales/, gitignored) before the first loadLocale.
  import './locales/main.loader.svelte.js';
  import FriendHeader from './lib/components/FriendHeader.svelte';
  import Header from './lib/components/Header.svelte';
  import { loginReturn, router, type Route } from './lib/router.svelte';
  import { session } from './lib/session.svelte';
  import Activity from './screens/Activity.svelte';
  import Admin from './screens/Admin.svelte';
  import Audit from './screens/Audit.svelte';
  import Blob from './screens/Blob.svelte';
  import Browse from './screens/Browse.svelte';
  import Ingest from './screens/Ingest.svelte';
  import Invite from './screens/Invite.svelte';
  import Library from './screens/Library.svelte';
  import Login from './screens/Login.svelte';
  import Play from './screens/Play.svelte';
  import Shelves from './screens/Shelves.svelte';
  import Storage from './screens/Storage.svelte';
  import Views from './screens/Views.svelte';

  // Locale is app-level state; a real switcher arrives with later polish.
  let locale = $state('en');

  // The document mirrors the app locale (WCAG 3.1.1) — index.html's
  // static lang="en" only covers the boot screen.
  $effect(() => {
    document.documentElement.lang = locale;
  });

  const route = $derived(router.route);

  // Boot: one whoami probe decides login page vs app shell. Loopback
  // browsers are implicitly the owner (D68) and land straight in.
  $effect(() => {
    void session.init();
  });

  // /login and /invite are the open pages; everything else needs a
  // session. Redirects use replace() so back doesn't ping-pong.
  const open = $derived(route.screen === 'login' || route.screen === 'invite');
  $effect(() => {
    if (session.status === 'anonymous' && !open) {
      loginReturn.stash(window.location.pathname);
      router.replace('/login');
    } else if (session.status === 'authenticated' && route.screen === 'login') {
      router.replace(loginReturn.consume());
    }
  });

  // Friend chrome (spec §4): whoami's role decides which app renders.
  // Friends get shelves home at `/` and browse at `/shelf/{view}` —
  // every owner route (and any miss) refuses by bouncing home, so the
  // owner screens never even mount for a friend.
  const friend = $derived(session.status === 'authenticated' && session.role === 'friend');
  // Play is friend-reachable by design (D84 amendment): play rights
  // are download rights, and the ROM bytes come from the same granted
  // /view surface the download anchor uses.
  $effect(() => {
    if (
      friend &&
      !open &&
      route.screen !== 'library' &&
      route.screen !== 'browse' &&
      route.screen !== 'play'
    ) {
      router.replace('/');
    }
  });

  // Route → tab title + screen-reader announcement. A Record over the
  // closed screen union: a new screen fails typecheck until it names
  // itself. Thunks because this script runs before the catalog loads.
  // @wc-include
  const titleLibrary = () => 'library';
  // @wc-include
  const titleAudit = () => 'audit';
  // @wc-include
  const titleViews = () => 'views';
  // @wc-include
  const titleIngest = () => 'ingest';
  // @wc-include
  const titleStorage = () => 'storage';
  // @wc-include
  const titleBlob = () => 'blob inspector';
  // @wc-include
  const titleActivity = () => 'activity';
  // @wc-include
  const titleAdmin = () => 'admin';
  // @wc-include
  const titleLogin = () => 'sign in';
  // @wc-include
  const titleInvite = () => 'invite';
  // @wc-include
  const titleBrowse = () => 'browse';
  // @wc-include
  const titlePlay = () => 'play';
  // @wc-include
  const titleNotfound = () => 'not found';
  const TITLES: Record<Route['screen'], () => string> = {
    library: titleLibrary,
    audit: titleAudit,
    views: titleViews,
    ingest: titleIngest,
    storage: titleStorage,
    blob: titleBlob,
    activity: titleActivity,
    admin: titleAdmin,
    login: titleLogin,
    invite: titleInvite,
    browse: titleBrowse,
    play: titlePlay,
    notfound: titleNotfound,
  };
  /** Read by the polite live region below: SPA navigation is silent to
   * a screen reader without it (the tab title change is not announced). */
  let announcement = $state('');
  $effect(() => {
    const name = TITLES[route.screen]();
    document.title = `${name} · datboi`;
    announcement = name;
  });
</script>

<div class="route-announcer" aria-live="polite">{announcement}</div>

{#await loadLocale(locale)}
  <!-- Rendered before the catalog is loaded, so untranslatable by design. -->
  <!-- @wc-ignore -->
  <p class="boot">Loading translations…</p>
{:then}
  {#if route.screen === 'login'}
    <Login />
  {:else if route.screen === 'invite'}
    <Invite />
  {:else if session.status === 'loading'}
    <p class="boot">checking session…</p>
  {:else if session.status === 'anonymous'}
    <!-- The redirect effect above is already swapping to /login. -->
  {:else if friend}
    <!-- Friend surface (spec §4): no nav tabs, no health chip, no jobs
         tray, no owner screens. The trust bar lives inside Browse. -->
    <div class="shell">
      <FriendHeader
        view={route.screen === 'browse' || route.screen === 'play' ? route.view : null}
      />
      {#if route.screen === 'browse'}
        <!-- key: a different shelf remounts the browse screen clean. -->
        {#key route.view}
          <Browse view={route.view} />
        {/key}
      {:else if route.screen === 'play'}
        {#key `${route.view}/${route.path}`}
          <Play view={route.view} path={route.path} />
        {/key}
      {:else}
        <Shelves />
      {/if}
    </div>
  {:else}
    <div class="shell">
      <Header />
      {#if route.screen === 'library'}
        <Library />
      {:else if route.screen === 'audit'}
        <!-- key: a different system remounts the drill-down clean. -->
        {#key route.systemId}
          <Audit systemId={route.systemId} />
        {/key}
      {:else if route.screen === 'views'}
        <Views />
      {:else if route.screen === 'ingest'}
        <Ingest />
      {:else if route.screen === 'storage'}
        <Storage />
      {:else if route.screen === 'blob'}
        <!-- key: a different blob remounts the inspector clean. -->
        {#key route.hash}
          <Blob hash={route.hash} />
        {/key}
      {:else if route.screen === 'activity'}
        <Activity />
      {:else if route.screen === 'admin'}
        <Admin />
      {:else if route.screen === 'browse'}
        <!-- Owner-reachable deep link since Play shipped (D84): the ▶
             lives in this screen's entry panel. Deliberately not a nav
             tab — the taxonomy naming pass (open-questions) owns that. -->
        {#key route.view}
          <Browse view={route.view} />
        {/key}
      {:else if route.screen === 'play'}
        {#key `${route.view}/${route.path}`}
          <Play view={route.view} path={route.path} />
        {/key}
      {:else}
        <main class="notfound">
          <p>nothing lives at this address</p>
        </main>
      {/if}
      <!-- The jobs tray is dead (D82): the header's activity indicator
           + the /activity page replaced it. -->
    </div>
  {/if}
{/await}

<style>
  .shell {
    height: 100vh;
    /* dvh tracks the *visible* viewport as mobile browser chrome slides
       in/out, so the footer tray isn't stranded under the URL bar. */
    height: 100dvh;
    display: flex;
    flex-direction: column;
  }

  .boot {
    font: 400 0.78125rem var(--font-data);
    color: var(--faint);
    padding: 26px var(--pad-x);
  }

  /* Present for screen readers, invisible and out of flow for everyone
     else — the standard visually-hidden recipe. */
  .route-announcer {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }

  .notfound {
    flex: 1;
    padding: 26px var(--pad-x);
    font: 400 0.78125rem var(--font-data);
    color: var(--faint);
  }
</style>
