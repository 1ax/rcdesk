// Pure functions backing the client's display picker (slice 2.4d): turning
// the host's `ControlMessage::Displays` payload into `<option>`-ready data,
// and parsing the picker's selected value back into a display id to send in
// `ControlMessage::SelectDisplay`. No DOM access here so this stays unit
// testable without a browser (see `stats.ts` for the same reasoning applied
// elsewhere in this codebase).

import type { DisplayEntry } from "./generated/DisplayEntry";

/** One `<option>` for the display picker, built from a `DisplayEntry` by
 * `displayOptions`. */
export interface DisplayOption {
  value: string;
  label: string;
  selected: boolean;
}

/** Builds the picker's options from the host's display list and the id of
 * the display currently being streamed, in the order `displays` arrived in.
 * `label` is 1-based (`index + 1`) so the picker reads `1: ...`, `2: ...`
 * regardless of the host's own `id` numbering, with ` (primary)` appended
 * for the primary display. */
export function displayOptions(displays: DisplayEntry[], current: number): DisplayOption[] {
  return displays.map((display, index) => {
    const suffix = display.primary ? " (основной)" : "";
    return {
      value: String(display.id),
      label: `${index + 1}: ${display.title} ${display.width}×${display.height}${suffix}`,
      selected: display.id === current,
    };
  });
}

/** Whether the picker should be shown at all -- only worth showing a choice
 * when the host can capture more than one display. */
export function shouldShowPicker(displays: DisplayEntry[]): boolean {
  return displays.length >= 2;
}

/** Parses a picker `<option>`'s `value` (see `displayOptions`) back into a
 * display id, or `null` if it isn't a non-negative integer. */
export function parseDisplayId(value: string): number | null {
  const parsed = Number.parseInt(value, 10);
  if (!Number.isInteger(parsed) || parsed < 0) return null;
  return parsed;
}
