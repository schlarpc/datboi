/**
 * ErrorCode → the user's language (D77). A Record over the closed
 * union does the exhaustiveness work: when the contract grows a code,
 * `npm run check` fails HERE until the code gets a translated message
 * — errors are translatable by construction, never English prose
 * passed through from the server. (exhaustive.ts explains why a
 * Record beats a branch chain where wuchale allows it.)
 *
 * Thunks, not bare strings: this module evaluates at import time,
 * before the locale catalog loads — the copy must be looked up when
 * the error is DESCRIBED, not when the module is imported.
 */
import { ApiError } from './client';
import type { ErrorCode } from './types';

// Lowercase copy, forced into the catalog at statement level.
// @wc-include
const badRequest = () => 'the daemon refused the request as malformed';
// @wc-include
const uploadExpired = () => 'the staged upload expired — upload the file again';
// @wc-include
const unauthorized = () => 'you need to sign in';
// @wc-include
const invalidCredentials = () => 'wrong username or password';
// @wc-include
const ownerOnly = () => 'only the owner can do this';
// @wc-include
const invalidInvite = () => 'this invite is invalid, already used, or expired';
// @wc-include
const csrfRejected = () => 'the daemon rejected a cross-origin request';
// @wc-include
const notFound = () => "the daemon can't find what this screen asked for";
// @wc-include
const usernameTaken = () => 'that username is already taken';
// @wc-include
const busy = () => 'the daemon is busy with a conflicting task — try again shortly';
// @wc-include
const storeFull = () => 'not enough free space in the store';
// @wc-include
const internal = () => 'the daemon hit an internal error';

const MESSAGES: Record<ErrorCode, () => string> = {
  bad_request: badRequest,
  upload_expired: uploadExpired,
  unauthorized,
  invalid_credentials: invalidCredentials,
  owner_only: ownerOnly,
  invalid_invite: invalidInvite,
  csrf_rejected: csrfRejected,
  not_found: notFound,
  username_taken: usernameTaken,
  busy,
  store_full: storeFull,
  internal,
};

/** Codes whose server-side detail genuinely helps the reader — the
 * translated line gets the raw diagnostic appended in parentheses. */
const DETAILED: ReadonlySet<ErrorCode> = new Set<ErrorCode>([
  'bad_request',
  'store_full',
  'internal',
]);

/**
 * Any rejection → a sentence in the user's language. Coded ApiErrors
 * map through the exhaustive Record above; a code this build doesn't
 * know (newer daemon) and every non-ApiError fall back to the raw
 * message, which is better than hiding it.
 */
export function describeError(e: unknown): string {
  if (e instanceof ApiError && e.code !== undefined) {
    const message = (MESSAGES[e.code] as (() => string) | undefined)?.();
    if (message !== undefined) {
      return DETAILED.has(e.code) ? `${message} (${e.message})` : message;
    }
  }
  return e instanceof Error ? e.message : String(e);
}
