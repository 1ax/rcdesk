// Pure recognizer for the Alt+Shift "switch keyboard layout" chord (slice
// 4.6d). No DOM: `input.ts` feeds it the same keydown/keyup `code`s that go
// to the host. Like Windows, the chord fires when the first of the two keys
// is released, provided both were held and no other key was pressed during
// the whole press. One firing per press; everything fully released resets.

const ALT = new Set(["AltLeft", "AltRight"]);
const SHIFT = new Set(["ShiftLeft", "ShiftRight"]);

export interface LayoutChord {
  /** Returns true when this keydown/keyup completes the chord. */
  keyDown(code: string): boolean;
  keyUp(code: string): boolean;
  /** Forget everything held (release_all / blur). */
  reset(): void;
}

export function createLayoutChord(): LayoutChord {
  const held = new Set<string>();
  let altSeen = false;
  let shiftSeen = false;
  let cancelled = false;
  let fired = false;

  function clearIfIdle(): void {
    if (held.size === 0) {
      altSeen = false;
      shiftSeen = false;
      cancelled = false;
      fired = false;
    }
  }

  return {
    keyDown(code) {
      held.add(code);
      if (ALT.has(code)) altSeen = true;
      else if (SHIFT.has(code)) shiftSeen = true;
      else cancelled = true;
      return false;
    },
    keyUp(code) {
      const wasHeld = held.delete(code);
      let fire = false;
      if (
        wasHeld &&
        !fired &&
        !cancelled &&
        altSeen &&
        shiftSeen &&
        (ALT.has(code) || SHIFT.has(code))
      ) {
        fire = true;
        fired = true;
      }
      clearIfIdle();
      return fire;
    },
    reset() {
      held.clear();
      altSeen = false;
      shiftSeen = false;
      cancelled = false;
      fired = false;
    },
  };
}
