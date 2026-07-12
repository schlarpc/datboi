/**
 * wuchale's plural pattern (D67): call sites write
 * `plural(n, ['# item', '# items'])` and the build replaces the forms
 * array with the catalog's translated forms and appends the locale's
 * plural rule — so a two-form English msgid can grow however many
 * forms the target language needs without touching the call site.
 * `#` interpolates the count, locale-formatted; a form without `#`
 * (e.g. `['new blob', 'new blobs']`) is returned as-is for layouts
 * that render the number separately.
 */
export function plural(
  num: number,
  forms: string[],
  rule: (n: number) => number = (n) => (n === 1 ? 0 : 1),
): string {
  const form = forms[rule(num)] ?? forms[forms.length - 1];
  return form.replace('#', num.toLocaleString());
}
