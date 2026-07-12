/**
 * Clipboard copy that can fail says so. `navigator.clipboard` exists
 * only in secure contexts — on LAN http (a natural deployment for this
 * daemon) it is undefined, and a bare writeText call is an unhandled
 * TypeError behind a silently dead copy button. One guarded helper;
 * call sites branch on the answer and give non-lying feedback.
 */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard === undefined) {
      return false;
    }
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    // Permission denied, document unfocused, … — the copy didn't happen.
    return false;
  }
}
