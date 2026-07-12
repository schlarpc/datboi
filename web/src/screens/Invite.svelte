<script lang="ts">
  /**
   * Invite acceptance (`/invite#<token>`). Never designed — same token
   * system as the login card, kept warm but simple. The token rides
   * location.hash so it never hits server logs or Referer headers
   * (admin.rs mints `/invite#<token>` URLs).
   */
  import { acceptInvite, ApiError } from '../lib/api/client';
  import { router } from '../lib/router.svelte';
  import { session } from '../lib/session.svelte';
  import logoUrl from '../lib/assets/logo.svg';

  // Read once at mount: the token is the fragment, sans '#'.
  const token = window.location.hash.slice(1);

  let username = $state('');
  let password = $state('');
  let error = $state<string | null>(null);
  let busy = $state(false);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (busy) return;
    busy = true;
    error = null;
    try {
      // Straight into the app: acceptance mints a session (auth.rs).
      session.apply(await acceptInvite(token, username, password));
      router.replace('/');
    } catch (e) {
      // Server messages are precise (bad username charset, short
      // password, expired invite, taken name) — surface them as-is.
      error = e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }
</script>

<div class="page">
  <form class="card" onsubmit={submit}>
    <div class="brand">
      <img class="logo" src={logoUrl} alt="" width="30" height="30" />
      <!-- @wc-ignore -->
      <span class="wordmark">datboi</span>
    </div>
    <h1>you're invited</h1>
    {#if token === ''}
      <p class="error">this invite link is missing its token — ask for a fresh one</p>
    {:else}
      <p class="sub">pick a username and password to join this shelf</p>
      <label>
        <span>username</span>
        <input type="text" bind:value={username} autocomplete="username" />
      </label>
      <label>
        <span>password</span>
        <input type="password" bind:value={password} autocomplete="new-password" />
      </label>
      {#if error !== null}
        <p class="error">{error}</p>
      {/if}
      <button type="submit" disabled={busy}>accept invite</button>
    {/if}
  </form>
</div>

<style>
  .page {
    min-height: 100vh;
    min-height: 100dvh;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 24px;
    box-sizing: border-box;
  }

  .card {
    width: 300px;
    max-width: 100%;
    background: var(--panel);
    border: 2px solid var(--ink);
    border-radius: var(--r-card);
    box-shadow: var(--shadow-card);
    padding: 26px 26px 24px;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .logo {
    width: 30px;
    height: 30px;
  }

  .wordmark {
    font: 800 15px var(--font-display);
    letter-spacing: -0.02em;
  }

  h1 {
    margin: 0;
    font: 800 20px var(--font-display);
    letter-spacing: -0.02em;
  }

  .sub {
    margin: 0;
    font: 400 12px var(--font-data);
    color: var(--faint);
  }

  label {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font: 500 11.5px var(--font-data);
    color: var(--mut);
  }

  input {
    font: 400 13px var(--font-data);
    padding: 6px 10px;
    border: 1.5px solid var(--edge);
    border-radius: var(--r-input);
    background: var(--bg);
    color: var(--text);
  }

  .error {
    margin: 0;
    font: 500 12px var(--font-data);
    color: var(--bad);
  }

  button {
    all: unset;
    text-align: center;
    background: var(--ink);
    color: var(--bg);
    border-radius: var(--r-pill);
    padding: 8px 18px;
    font: 600 13px var(--font-display);
    cursor: pointer;
  }

  button:disabled {
    opacity: 0.6;
    cursor: progress;
  }
</style>
