<script lang="ts">
  import { loadLocale } from 'wuchale/load-utils';
  // Side-effect import: registers the wuchale catalog loaders (generated at
  // startup into src/locales/, gitignored) before the first loadLocale.
  import './locales/main.loader.svelte.js';
  import FriendHeader from './lib/components/FriendHeader.svelte';
  import Header from './lib/components/Header.svelte';
  import JobsTray from './lib/components/JobsTray.svelte';
  import { router } from './lib/router.svelte';
  import { session } from './lib/session.svelte';
  import Admin from './screens/Admin.svelte';
  import Audit from './screens/Audit.svelte';
  import Blob from './screens/Blob.svelte';
  import Browse from './screens/Browse.svelte';
  import Ingest from './screens/Ingest.svelte';
  import Invite from './screens/Invite.svelte';
  import Library from './screens/Library.svelte';
  import Login from './screens/Login.svelte';
  import Shelves from './screens/Shelves.svelte';
  import Storage from './screens/Storage.svelte';
  import Views from './screens/Views.svelte';

  // Locale is app-level state; a real switcher arrives with later polish.
  let locale = $state('en');

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
      router.replace('/login');
    } else if (session.status === 'authenticated' && route.screen === 'login') {
      router.replace('/');
    }
  });

  // Friend chrome (spec §4): whoami's role decides which app renders.
  // Friends get shelves home at `/` and browse at `/shelf/{view}` —
  // every owner route (and any miss) refuses by bouncing home, so the
  // owner screens never even mount for a friend.
  const friend = $derived(session.status === 'authenticated' && session.role === 'friend');
  $effect(() => {
    if (friend && !open && route.screen !== 'library' && route.screen !== 'browse') {
      router.replace('/');
    }
  });
</script>

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
      <FriendHeader view={route.screen === 'browse' ? route.view : null} />
      {#if route.screen === 'browse'}
        <!-- key: a different shelf remounts the browse screen clean. -->
        {#key route.view}
          <Browse view={route.view} />
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
      {:else if route.screen === 'admin'}
        <Admin />
      {:else}
        <main class="notfound">
          <p>nothing lives at this address</p>
        </main>
      {/if}
      <JobsTray />
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
    font: 400 12.5px var(--font-data);
    color: var(--faint);
    padding: 26px var(--pad-x);
  }

  .notfound {
    flex: 1;
    padding: 26px var(--pad-x);
    font: 400 12.5px var(--font-data);
    color: var(--faint);
  }
</style>
