const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]):not([type="hidden"]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

/** Minimal focus trap for a modal dialog: on Tab / Shift+Tab, cycles focus
 * between the first and last focusable elements inside `container` instead of
 * letting it leave. Returns true when it handled (preventDefault-ed) the event.
 * With no focusable element inside, Tab is swallowed and the container itself
 * is focused. */
export function trapFocus(
  container: HTMLElement,
  event: KeyboardEvent,
  active: Element | null = document.activeElement,
): boolean {
  if (event.key !== "Tab") return false;
  const items = Array.from(container.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
    (el) => !el.hidden,
  );
  if (items.length === 0) {
    event.preventDefault();
    container.focus();
    return true;
  }
  const first = items[0]!;
  const last = items[items.length - 1]!;
  const inside = active !== null && container.contains(active);
  if (event.shiftKey) {
    if (!inside || active === first) {
      event.preventDefault();
      last.focus();
      return true;
    }
  } else if (!inside || active === last) {
    event.preventDefault();
    first.focus();
    return true;
  }
  return false;
}
