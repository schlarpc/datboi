<script lang="ts">
  /**
   * Friend chrome header (spec §4.1): logo disc + wordmark (→ shelves
   * home), breadcrumb `‹ your shelves / {view}` while browsing, then
   * theme toggle + account chip. Deliberately NO owner chrome — no nav
   * tabs, no health chip, no jobs tray (wireframe 3c: "friends land
   * HERE, not on a dashboard. no admin chrome at all").
   */
  import { session } from '../session.svelte';
  import Link from './Link.svelte';
  import ThemeToggle from './ThemeToggle.svelte';
  import logoUrl from '../assets/logo.svg';

  /** The view being browsed, or null on the shelves home. */
  let { view = null }: { view?: string | null } = $props();

  const initial = $derived(session.username?.slice(0, 1) ?? null);
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
  {#if view !== null}
    <span class="crumb">
      <!-- "shelves" is the friendly synonym for shared views — keep the
           warmth (spec §6 translator note). -->
      <Link href="/" class="crumb-back">‹ your shelves</Link>
      <span class="crumb-view">/ {view}</span>
    </span>
  {/if}
  <div class="right">
    <ThemeToggle />
    {#if session.username !== null}
      <span class="account">
        <span class="avatar">{initial}</span>
        <span class="user">{session.username}</span>
      </span>
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
    font: 800 15px var(--font-display);
    letter-spacing: -0.02em;
  }

  .crumb {
    display: inline-flex;
    align-items: baseline;
    gap: 8px;
  }

  .crumb :global(a.crumb-back) {
    font: 500 12.5px var(--font-data);
    color: var(--faint);
    text-decoration: none;
  }

  .crumb :global(a.crumb-back:hover) {
    color: var(--text);
  }

  .crumb-view {
    font: 600 13px var(--font-data);
  }

  .right {
    margin-left: auto;
    display: flex;
    align-items: center;
    gap: 14px;
  }

  .account {
    display: inline-flex;
    align-items: center;
    gap: 8px;
  }

  .avatar {
    width: 26px;
    height: 26px;
    border-radius: 50%;
    background: var(--panel2);
    border: 1.5px solid var(--hair);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    font: 600 12px var(--font-display);
  }

  .user {
    font: 500 12.5px var(--font-data);
    color: var(--mut);
  }
</style>
