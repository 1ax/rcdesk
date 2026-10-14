// Символ -> физическая клавиша US-раскладки (W3C `KeyboardEvent.code`).
// Нужен, чтобы с экранной клавиатурой телефона работали сочетания (Ctrl+C):
// модификатор + код клавиши, а не Unicode-ввод. Чистая логика, без DOM.

export interface CharKey {
  code: string;
  shift: boolean;
}

const PLAIN: Record<string, string> = {
  " ": "Space",
  "`": "Backquote",
  "-": "Minus",
  "=": "Equal",
  "[": "BracketLeft",
  "]": "BracketRight",
  "\\": "Backslash",
  ";": "Semicolon",
  "'": "Quote",
  ",": "Comma",
  ".": "Period",
  "/": "Slash",
};

const SHIFTED: Record<string, string> = {
  "~": "Backquote",
  _: "Minus",
  "+": "Equal",
  "{": "BracketLeft",
  "}": "BracketRight",
  "|": "Backslash",
  ":": "Semicolon",
  '"': "Quote",
  "<": "Comma",
  ">": "Period",
  "?": "Slash",
  "!": "Digit1",
  "@": "Digit2",
  "#": "Digit3",
  $: "Digit4",
  "%": "Digit5",
  "^": "Digit6",
  "&": "Digit7",
  "*": "Digit8",
  "(": "Digit9",
  ")": "Digit0",
};

// Раскладка «Russian – PC» (ЙЦУКЕН), строчные буквы.
const CYRILLIC: Record<string, string> = {};
function fillCyrillic(letters: string, codes: string[]): void {
  Array.from(letters).forEach((ch, i) => {
    CYRILLIC[ch] = codes[i]!;
  });
}
fillCyrillic("ё", ["Backquote"]);
fillCyrillic("йцукенгшщзхъ", [
  "KeyQ", "KeyW", "KeyE", "KeyR", "KeyT", "KeyY", "KeyU", "KeyI", "KeyO", "KeyP",
  "BracketLeft", "BracketRight",
]);
fillCyrillic("фывапролджэ", [
  "KeyA", "KeyS", "KeyD", "KeyF", "KeyG", "KeyH", "KeyJ", "KeyK", "KeyL", "Semicolon", "Quote",
]);
fillCyrillic("ячсмитьбю", [
  "KeyZ", "KeyX", "KeyC", "KeyV", "KeyB", "KeyN", "KeyM", "Comma", "Period",
]);

export function charToKey(ch: string): CharKey | null {
  if (ch.length === 0) return null;
  if (/^[a-z]$/.test(ch)) return { code: `Key${ch.toUpperCase()}`, shift: false };
  if (/^[A-Z]$/.test(ch)) return { code: `Key${ch}`, shift: true };
  if (/^[0-9]$/.test(ch)) return { code: `Digit${ch}`, shift: false };
  const plain = PLAIN[ch];
  if (plain) return { code: plain, shift: false };
  const shifted = SHIFTED[ch];
  if (shifted) return { code: shifted, shift: true };
  const cyr = CYRILLIC[ch];
  if (cyr) return { code: cyr, shift: false };
  const lower = ch.toLowerCase();
  if (lower !== ch) {
    const cyrUpper = CYRILLIC[lower];
    if (cyrUpper) return { code: cyrUpper, shift: true };
  }
  return null;
}
