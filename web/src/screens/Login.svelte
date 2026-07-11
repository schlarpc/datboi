<script lang="ts">
  /**
   * Login page. Never designed (the comps only cover the owner shell
   * and friend surface) — a minimal centered card built purely from the
   * token system: cartridge card shell, mono inputs, filled pill.
   */
  import { ApiError, login } from '../lib/api/client';
  import { router } from '../lib/router.svelte';
  import { session } from '../lib/session.svelte';

  let username = $state('');
  let password = $state('');
  let failed = $state(false);
  let otherError = $state<string | null>(null);
  let busy = $state(false);

  // Same reserved frog slot as the header; lowercase title copy is
  // force-included at statement level.
  // @wc-include
  const logoTitle = 'logo slot — frog goes here';

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (busy) return;
    busy = true;
    failed = false;
    otherError = null;
    try {
      session.apply(await login(username, password));
      router.replace('/');
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) {
        failed = true;
      } else {
        otherError = e instanceof Error ? e.message : String(e);
      }
    } finally {
      busy = false;
    }
  }
</script>

<div class="page">
  <form class="card" onsubmit={submit}>
    <div class="brand">
      <span class="logo-disc" title={logoTitle}></span>
      <!-- @wc-ignore -->
      <span class="wordmark">datboi</span>
    </div>
    <label>
      <span>username</span>
      <input type="text" bind:value={username} autocomplete="username" />
    </label>
    <label>
      <span>password</span>
      <input type="password" bind:value={password} autocomplete="current-password" />
    </label>
    {#if failed}
      <p class="error">invalid credentials</p>
    {:else if otherError !== null}
      <p class="error">something went wrong — {otherError}</p>
    {/if}
    <button type="submit" disabled={busy}>log in</button>
  </form>
</div>

<style>
  .page {
    min-height: 100vh;
    display: flex;
    align-items: center;
    justify-content: center;
  }

  .card {
    width: 300px;
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
    margin-bottom: 6px;
  }

  .logo-disc {
    width: 24px;
    height: 24px;
    border-radius: 50%;
    background: var(--ok);
    border: 2px solid var(--ink);
  }

  .wordmark {
    font: 800 15px var(--font-display);
    letter-spacing: -0.02em;
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
    border: 1.5px solid var(--hair);
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
