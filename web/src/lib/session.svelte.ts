/**
 * Session store (Svelte 5 runes module). One whoami probe at boot
 * decides login page vs app shell; login/invite pages push their
 * results back through `apply` so the shell reacts without a reload.
 */

import * as api from './api/client';
import type { SessionInfo, Whoami } from './api/types';

type Status = 'loading' | 'anonymous' | 'authenticated';

const state = $state({
  status: 'loading' as Status,
  username: null as string | null,
  role: null as 'owner' | 'friend' | null,
  /** 'loopback' = implicit owner, no user row (D68). */
  via: null as 'loopback' | 'session' | 'bearer' | null,
});

// A mid-flight 401 anywhere means the session expired under us; flip to
// anonymous so App's redirect effect bounces to /login.
api.onUnauthorized(() => {
  clear();
});

function clear(): void {
  state.status = 'anonymous';
  state.username = null;
  state.role = null;
  state.via = null;
}

export const session = {
  get status(): Status {
    return state.status;
  },
  get username(): string | null {
    return state.username;
  },
  get role(): 'owner' | 'friend' | null {
    return state.role;
  },
  get via(): 'loopback' | 'session' | 'bearer' | null {
    return state.via;
  },

  /** Boot probe. Network failure reads as anonymous — fail closed. */
  async init(): Promise<void> {
    try {
      this.apply(await api.whoami());
    } catch {
      clear();
    }
  },

  /**
   * Feed a whoami/login/invite-accept answer into the store. The contract
   * (WhoamiResponse) makes username/role/via optional even when
   * authenticated — loopback callers have no user row (D68) — so absent
   * fields read as null; a login answer (SessionResponse) carries no
   * `via`, which is 'session' by construction.
   */
  apply(who: Whoami | SessionInfo): void {
    if (!who.authenticated) {
      clear();
      return;
    }
    state.status = 'authenticated';
    state.username = who.username ?? null;
    state.role = who.role ?? null;
    state.via = 'via' in who ? (who.via ?? null) : 'session';
  },

  async logout(): Promise<void> {
    try {
      await api.logout();
    } finally {
      // The cookie is cleared server-side; locally we are anonymous
      // even if the request failed (the session may already be dead).
      clear();
    }
  },
};
