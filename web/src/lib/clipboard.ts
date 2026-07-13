/**
 * Clipboard copy that can fail says so — but tries hard first.
 * `navigator.clipboard` exists only in secure contexts, and LAN http
 * is this daemon's PRIMARY deployment ("plex server" model), not an
 * edge case — so the legacy textarea + execCommand path is the main
 * road there, not a courtesy. Call sites branch on the answer and
 * give non-lying feedback.
 */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard !== undefined) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // Permission denied, document unfocused, … — fall through and let
    // the legacy path have a try.
  }
  try {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.setAttribute('readonly', '');
    // Off-canvas, not display:none — hidden elements can't be selected.
    ta.style.position = 'fixed';
    ta.style.top = '0';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand('copy');
    ta.remove();
    return ok;
  } catch {
    return false;
  }
}
