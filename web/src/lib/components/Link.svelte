<script lang="ts">
  /**
   * In-app anchor: a real <a> (middle-click, copy-link, a11y all work)
   * whose plain left-click goes through the router instead of a page
   * load. Styling is left entirely to the caller via class/children.
   */
  import type { Snippet } from 'svelte';
  import type { HTMLAnchorAttributes } from 'svelte/elements';
  import { router } from '../router.svelte';

  let {
    href,
    children,
    onclick: onclickProp,
    ...rest
  }: { href: string; children?: Snippet } & HTMLAnchorAttributes = $props();

  function onclick(event: MouseEvent & { currentTarget: EventTarget & HTMLAnchorElement }) {
    // Callers may attach their own handler (e.g. stopPropagation on
    // chips inside clickable cards); it runs before the routing.
    onclickProp?.(event);
    // Modified/secondary clicks keep native behavior (new tab etc).
    if (
      event.defaultPrevented ||
      event.button !== 0 ||
      event.metaKey ||
      event.ctrlKey ||
      event.shiftKey ||
      event.altKey
    ) {
      return;
    }
    event.preventDefault();
    router.navigate(href);
  }
</script>

<a {href} {onclick} {...rest}>{@render children?.()}</a>
