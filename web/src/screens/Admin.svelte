<script module lang="ts">
  import type { MintedInvite } from '../lib/api/types';

  // Module scope on purpose: the minted URL is shown ONCE — the server
  // keeps only the token hash — so a stray navigation must not destroy
  // the only copy in existence. It lives until dismissed or replaced.
  let minted = $state<MintedInvite | null>(null);
</script>

<script lang="ts">
  /**
   * Admin (spec §3.9 — "deliberately boring"), FULLY REAL against
   * /v1/admin/*: invite minting (one-time URL), the USERS × VIEWS
   * grant grid with optimistic toggles (revert on error), per-user
   * session revocation.
   *
   * Rulings baked in (M5 scope ruling, open-questions 2026-07-11):
   * - Pending invites render as greyed rows with dashed inert cells:
   *   grants are keyed by user, and an invite has no user row yet.
   * - Owner rows are inert ✓ cells: views_body shows owners everything
   *   regardless of grants, so a toggle would be a lie.
   * - SETTINGS is read-only em-dashes: neither the listen address (a
   *   serve flag) nor analyzer state (config KV the CLI reads) is
   *   surfaced by any API — don't invent an endpoint.
   * - A non-owner gets a 403 from /v1/admin/users → owner-only empty
   *   state, no chrome pretending otherwise.
   */
  import {
    ApiError,
    adminGrant,
    adminMintInvite,
    adminRevoke,
    adminRevokeInvite,
    adminRevokeSessions,
    adminUsers,
    views as fetchViews,
  } from '../lib/api/client';
  import type { AdminUsersBody } from '../lib/api/types';
  import { copyText } from '../lib/clipboard';
  import { errorText } from '../lib/remote';
  import { shortHash } from '../lib/format';
  import LoadError from '../lib/components/LoadError.svelte';

  let data = $state<AdminUsersBody | null>(null);
  let viewNames = $state<string[] | null>(null);
  let error = $state<string | null>(null);
  let forbidden = $state(false);

  function load() {
    Promise.all([adminUsers(), fetchViews()]).then(
      ([users, views]) => {
        data = users;
        viewNames = views.views.map((view) => view.name);
      },
      (e: unknown) => {
        if (e instanceof ApiError && e.status === 403) {
          forbidden = true;
        } else {
          error = e instanceof Error ? e.message : String(e);
        }
      },
    );
  }

  $effect(() => {
    load();
  });

  // ---- invite minting (POST /v1/admin/invites) ----

  let mintOpen = $state(false);
  let mintRole = $state<'friend' | 'owner'>('friend');
  let mintDays = $state(7);
  let mintBusy = $state(false);
  let mintError = $state<string | null>(null);
  let inviteCopied = $state<'idle' | 'done' | 'failed'>('idle');
  /** The one-time URL's <code> block, for the no-clipboard fallback. */
  let mintedCode = $state<HTMLElement | null>(null);

  const mintedUrl = $derived(minted === null ? null : location.origin + minted.url_path);

  async function mint() {
    if (mintBusy) return;
    mintBusy = true;
    mintError = null;
    // The min/max attributes are advisory outside form validation:
    // clamp here so a cleared or hand-typed field never rides to a 400.
    const days = Math.min(365, Math.max(1, Math.round(mintDays || 7)));
    try {
      minted = await adminMintInvite({ role: mintRole, expires_days: days });
      mintOpen = false;
      load(); // the pending row appears in the grid
    } catch (e) {
      mintError = e instanceof Error ? e.message : String(e);
    } finally {
      mintBusy = false;
    }
  }

  async function copyInvite() {
    if (mintedUrl === null) return;
    const ok = await copyText(mintedUrl);
    inviteCopied = ok ? 'done' : 'failed';
    if (!ok) {
      // No clipboard (LAN http) — the one-time URL is right there in
      // the <code> block: hand it over selected, one keystroke to copy.
      selectMintedUrl();
    }
    setTimeout(() => (inviteCopied = 'idle'), 1400);
  }

  function selectMintedUrl(): void {
    if (mintedCode === null) return;
    const range = document.createRange();
    range.selectNodeContents(mintedCode);
    const selection = window.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(range);
  }

  // ---- grant grid: optimistic toggle, revert on error ----

  let busyCells = $state<Record<string, boolean>>({});
  /** Grant/invite mutations under the grid say why they failed. */
  let gridError = $state<string | null>(null);

  async function toggleGrant(username: string, view: string) {
    const user = data?.users.find((u) => u.username === username);
    if (user === undefined) return;
    const key = `${username} ${view}`;
    if (busyCells[key] === true) return; // don't race the same cell
    busyCells[key] = true;
    const had = user.grants.includes(view);
    // Optimistic: the cell flips now, the server confirms behind it.
    user.grants = had ? user.grants.filter((v) => v !== view) : [...user.grants, view];
    try {
      if (had) {
        await adminRevoke(username, view);
      } else {
        await adminGrant(username, view);
      }
    } catch (e) {
      // Revert — the server said no; the grid must tell the truth,
      // and so must the error line (a silent snap-back reads as a
      // misclick, not a failure).
      user.grants = had ? [...user.grants, view] : user.grants.filter((v) => v !== view);
      gridError = errorText(e);
    } finally {
      busyCells[key] = false;
    }
  }

  // ---- sessions (DELETE /v1/admin/sessions/{username}) ----

  let revoking = $state<Record<string, boolean>>({});
  let sessionsError = $state<string | null>(null);

  async function revoke(username: string) {
    if (revoking[username] === true) return;
    revoking[username] = true;
    try {
      await adminRevokeSessions(username);
      const user = data?.users.find((u) => u.username === username);
      if (user !== undefined) user.sessions = 0;
    } catch (e) {
      // count unchanged — nothing was revoked; say so
      sessionsError = errorText(e);
    } finally {
      revoking[username] = false;
    }
  }

  // ---- pending-invite revocation (DELETE /v1/admin/invites/{hash}) ----

  let revokingInvite = $state<Record<string, boolean>>({});

  async function revokeInvite(tokenHash: string) {
    if (revokingInvite[tokenHash] === true) return;
    revokingInvite[tokenHash] = true;
    try {
      await adminRevokeInvite(tokenHash);
      load(); // the pending row disappears
    } catch (e) {
      gridError = errorText(e);
    } finally {
      revokingInvite[tokenHash] = false;
    }
  }

  // Lowercase tooltip copy — statement-level force-includes (the
  // EntryDrawer pattern; an element directive would sweep classes in).
  // @wc-include
  const ownerCellTitle = 'owners see every view — grants apply to friends';
  // @wc-include
  const pendingCellTitle = 'grants attach to a user — cells unlock once the invite is accepted';
  // @wc-include
  const grantCellLabel = 'toggle grant';
  // Role words are user-visible but lowercase.
  // @wc-include
  const roleFriend = 'friend';
  // @wc-include
  const roleOwner = 'owner';
</script>

<main>
  {#if forbidden}
    <div class="title-row"><h2>Admin</h2></div>
    <!-- Undesigned empty state: plain mono in --faint. -->
    <p class="undesigned">owner-only — invites, grants, and sessions live here</p>
  {:else if error !== null}
    <LoadError msg={error} onretry={load} />
  {:else if data === null || viewNames === null}
    <p class="undesigned">loading…</p>
  {:else}
    <div class="title-row">
      <h2>Admin</h2>
      <button class="mint-btn" onclick={() => (mintOpen = !mintOpen)}>+ mint invite URL</button>
    </div>

    {#if mintOpen}
      <form
        class="mint-form"
        onsubmit={(e) => {
          e.preventDefault();
          void mint();
        }}
      >
        <span class="form-label">role</span>
        <div class="seg">
          <button
            type="button"
            class:active={mintRole === 'friend'}
            aria-pressed={mintRole === 'friend'}
            onclick={() => (mintRole = 'friend')}
          >
            {roleFriend}
          </button>
          <button
            type="button"
            class:active={mintRole === 'owner'}
            aria-pressed={mintRole === 'owner'}
            onclick={() => (mintRole = 'owner')}
          >
            {roleOwner}
          </button>
        </div>
        <label class="days">
          <span>expires in</span>
          <input type="number" min="1" max="365" bind:value={mintDays} />
          <span>days</span>
        </label>
        <button class="mint-go" type="submit" disabled={mintBusy}>mint</button>
        {#if mintError !== null}
          <span class="mint-error">{mintError}</span>
        {/if}
      </form>
    {/if}

    {#if mintedUrl !== null}
      <div class="minted">
        <div class="minted-note">shown once — the server keeps only a hash of this token</div>
        <div class="minted-row">
          <code bind:this={mintedCode}>{mintedUrl}</code>
          <button class="pill" onclick={copyInvite}>
            {#if inviteCopied === 'done'}copied ✓{:else if inviteCopied === 'failed'}couldn't copy — select it{:else}⎘ copy{/if}
          </button>
          <button class="pill" onclick={() => (minted = null)}>dismiss</button>
        </div>
      </div>
    {/if}

    <section>
      <div class="caps"><!-- @wc-context: compiled shelf -->USERS × VIEWS</div>
      {#if data.users.length === 0 && data.invites.length === 0}
        <p class="undesigned">no users yet — mint an invite</p>
      {:else if viewNames.length === 0}
        <p class="undesigned">no views to grant yet</p>
      {:else}
        <!-- The grant grid grows a column per view; on a phone it scrolls
             sideways inside this wrapper instead of forcing the page wide. -->
        <div class="grid-scroll">
          <table class="grid">
          <thead>
            <tr>
              <th></th>
              {#each viewNames as view (view)}
                <th>{view}</th>
              {/each}
            </tr>
          </thead>
          <tbody>
            {#each data.users as user (user.username)}
              <tr>
                <td class="user">
                  {user.username}
                  {#if user.role === 'owner'}
                    <span class="role-tag">{roleOwner}</span>
                  {/if}
                </td>
                {#each viewNames as view (view)}
                  {#if user.role === 'owner'}
                    <td class="cell owner-cell" title={ownerCellTitle}>✓</td>
                  {:else}
                    <td class="cell">
                      <!-- aria-pressed carries what the ✓ shows; the
                           label names the cell a screen reader is on. -->
                      <button
                        class="cell-btn"
                        class:granted={user.grants.includes(view)}
                        aria-pressed={user.grants.includes(view)}
                        aria-label={`${grantCellLabel}: ${user.username} · ${view}`}
                        onclick={() => toggleGrant(user.username, view)}
                      >
                        {#if user.grants.includes(view)}✓{/if}
                      </button>
                    </td>
                  {/if}
                {/each}
              </tr>
            {/each}
            {#each data.invites as invite (invite.token_hash)}
              <!-- Not-yet-users: no grants possible (the API keys
                   grants by user) — greyed row, dashed inert cells. -->
              <tr class="pending">
                <td class="user">
                  invite {shortHash(invite.token_hash)} · pending {invite.role === 'owner'
                    ? roleOwner
                    : roleFriend}
                </td>
                {#each viewNames as view (view)}
                  <td class="cell pending-cell" title={pendingCellTitle}></td>
                {/each}
                <td class="cell">
                  <button
                    class="s-revoke"
                    onclick={() => revokeInvite(invite.token_hash)}
                    disabled={revokingInvite[invite.token_hash] === true}
                  >
                    revoke invite
                  </button>
                </td>
              </tr>
            {/each}
          </tbody>
          </table>
        </div>
      {/if}
      {#if gridError !== null}
        <p class="undesigned bad-line">something went wrong — {gridError}</p>
      {/if}
      <p class="caption">friends see only ✓ views · dashboard/audit stay owner-only</p>
    </section>

    <section>
      <div class="caps">SESSIONS</div>
      {#if data.users.length === 0}
        <p class="undesigned">nobody to revoke — no users yet</p>
      {:else}
        {#if sessionsError !== null}
          <p class="undesigned bad-line">something went wrong — {sessionsError}</p>
        {/if}
        {#each data.users as user (user.username)}
          <div class="session-row">
            <span class="s-name">{user.username}</span>
            <span class="s-count">{user.sessions.toLocaleString()} active</span>
            <button
              class="s-revoke"
              onclick={() => revoke(user.username)}
              disabled={revoking[user.username] === true || user.sessions === 0}
            >
              revoke
            </button>
          </div>
        {/each}
      {/if}
    </section>

    <section>
      <div class="caps">SETTINGS</div>
      <!-- Neither value exists in any API (listen addr is a `serve`
           flag/env, analyzer state a config KV the CLI reads) — em-dash
           placeholders per the ruling; don't invent an endpoint
           (M5 scope ruling, open-questions 2026-07-11). -->
      <div class="setting">analyzer sweeps: —</div>
      <div class="setting">listen: —</div>
      <p class="settings-hint">not surfaced by the API yet — the CLI knows</p>
    </section>
  {/if}
</main>

<style>
  main {
    flex: 1;
    overflow-y: auto;
    padding: 24px var(--pad-x) 30px;
  }

  .grid-scroll {
    overflow-x: auto;
    -webkit-overflow-scrolling: touch;
  }

  .title-row {
    display: flex;
    align-items: baseline;
    gap: 14px;
    margin-bottom: 22px;
  }

  h2 {
    margin: 0;
    font: 800 1.5rem var(--font-display);
    letter-spacing: -0.03em;
  }

  .undesigned {
    font: 400 0.78125rem var(--font-data);
    color: var(--faint);
  }

  .mint-btn {
    all: unset;
    margin-left: auto;
    background: var(--ink);
    color: var(--bg);
    border-radius: var(--r-pill);
    padding: 7px 16px;
    font: 600 0.8125rem var(--font-display);
    cursor: pointer;
  }

  .mint-form {
    display: flex;
    align-items: center;
    gap: 12px;
    margin: -8px 0 18px;
    font: 500 0.75rem var(--font-data);
    color: var(--mut);
  }

  .form-label {
    color: var(--faint);
  }

  .seg {
    display: flex;
    border: 1.5px solid var(--hair);
    border-radius: var(--r-pill);
    overflow: hidden;
  }

  .seg button {
    all: unset;
    padding: 3px 10px;
    font: 500 0.6875rem var(--font-data);
    color: var(--faint);
    cursor: pointer;
  }

  .seg button.active {
    background: var(--ink);
    color: var(--bg);
    font-weight: 600;
  }

  .days {
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .days input {
    width: 52px;
    font: 400 0.75rem var(--font-data);
    padding: 3px 8px;
    border: 1.5px solid var(--edge);
    border-radius: var(--r-input);
    background: var(--bg);
    color: var(--text);
  }

  .mint-go {
    all: unset;
    border: 2px solid var(--ink);
    border-radius: var(--r-pill);
    padding: 4px 14px;
    background: var(--panel);
    font: 600 0.75rem var(--font-data);
    cursor: pointer;
  }

  .mint-go:disabled {
    color: var(--faint);
    cursor: progress;
  }

  .mint-error {
    font: 500 0.75rem var(--font-data);
    color: var(--bad);
  }

  .minted {
    border: 2px solid var(--ink);
    border-radius: var(--r-sub);
    background: var(--panel);
    box-shadow: var(--shadow-card);
    padding: 12px 16px;
    margin-bottom: 22px;
  }

  .minted-note {
    font: 400 0.71875rem var(--font-data);
    color: var(--faint);
    margin-bottom: 6px;
  }

  .minted-row {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .minted-row code {
    font: 400 0.78125rem var(--font-data);
    color: var(--text);
    overflow-wrap: anywhere;
  }

  .pill {
    all: unset;
    border: 2px solid var(--ink);
    border-radius: var(--r-pill);
    padding: 3px 12px;
    background: var(--panel);
    font: 600 0.75rem var(--font-data);
    cursor: pointer;
    flex: none;
  }

  section {
    margin-top: 26px;
  }

  .caps {
    font: 800 0.8125rem var(--font-display);
    letter-spacing: 0.02em;
    margin-bottom: 10px;
  }

  .grid {
    border-collapse: collapse;
    font: 400 0.78125rem var(--font-data);
    color: var(--mut);
  }

  .grid th {
    font: 500 0.71875rem var(--font-data);
    color: var(--faint);
    font-weight: 500;
    text-align: center;
    padding: 4px 10px;
    max-width: 110px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .grid td {
    padding: 4px 10px;
  }

  .user {
    color: var(--text);
    white-space: nowrap;
  }

  .role-tag {
    font: 400 0.65625rem var(--font-data);
    color: var(--faint);
    margin-left: 4px;
  }

  .cell {
    text-align: center;
  }

  .cell-btn {
    all: unset;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 26px;
    height: 22px;
    border: 1.5px solid var(--hair);
    border-radius: var(--r-input);
    cursor: pointer;
    color: var(--okT);
    box-sizing: border-box;
  }

  .cell-btn.granted {
    background: color-mix(in srgb, var(--ok) 10%, var(--panel));
    border-color: var(--ok);
  }

  .owner-cell {
    color: var(--faint);
    cursor: default;
  }

  .pending .user {
    color: var(--faint);
  }

  .pending-cell::after {
    content: '';
    display: inline-block;
    width: 26px;
    height: 22px;
    border: 1.5px dashed var(--hair);
    border-radius: var(--r-input);
    box-sizing: border-box;
  }

  .caption {
    margin-top: 10px;
    font: 400 0.71875rem var(--font-data);
    color: var(--faint);
  }

  .session-row {
    display: flex;
    align-items: baseline;
    gap: 14px;
    font: 400 0.78125rem var(--font-data);
    color: var(--mut);
    line-height: 2;
  }

  .s-name {
    color: var(--text);
    min-width: 120px;
  }

  .bad-line {
    color: var(--bad);
  }

  .s-revoke {
    all: unset;
    font: 500 0.75rem var(--font-data);
    color: var(--bad);
    cursor: pointer;
  }

  .s-revoke:disabled {
    color: var(--dim);
    cursor: default;
  }

  .setting {
    font: 400 0.78125rem var(--font-data);
    color: var(--mut);
    line-height: 2;
  }

  .settings-hint {
    margin-top: 4px;
    font: 400 0.65625rem var(--font-data);
    color: var(--dim);
  }

  @media (max-width: 720px) {
    .title-row {
      flex-wrap: wrap;
      gap: 8px 14px;
    }

    /* The invite minting controls (role · expiry · mint) stack instead
       of crowding one line. */
    .mint-form {
      flex-wrap: wrap;
      gap: 10px 12px;
    }

    /* Keep the minted URL and its copy button on one line but let the
       URL take whatever's left and wrap its characters. */
    .minted-row {
      flex-wrap: wrap;
    }
  }
</style>
