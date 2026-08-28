/** Keys that require CGEventTap (not usable for mode hotkeys). */
export const EVENTTAP_HOTKEYS = new Set([
  "right_shift",
  "left_shift",
  "left_cmd",
  "right_cmd",
  "left_ctrl",
  "right_ctrl",
  "left_option",
  "right_option",
  "tab",
]);

const BARE_NAMED_KEYS = new Set([
  "space",
  "enter",
  "return",
  "backspace",
  "arrowup",
  "arrowdown",
  "arrowleft",
  "arrowright",
  "minus",
  "equal",
  "bracketleft",
  "bracketright",
  "backslash",
  "semicolon",
  "quote",
  "comma",
  "period",
  "slash",
  "backquote",
]);

/**
 * Returns true if the key is a bare printable key (single character a-z, 0-9,
 * or one of the named printable/navigation keys) with no modifier chords.
 */
export function isBarePrintableKey(key: string): boolean {
  if (key.includes("+")) return false;
  const k = key.toLowerCase();
  return /^[a-z0-9]$/.test(k) || BARE_NAMED_KEYS.has(k);
}

export function isEventTapHotkey(key: string): boolean {
  return EVENTTAP_HOTKEYS.has(key) || isBarePrintableKey(key);
}

export function formatHotkeyLabel(key: string): string {
  const SPECIAL_LABELS: Record<string, string> = {
    right_shift: "Right\u00a0Shift",
    left_shift: "Left\u00a0Shift",
    left_cmd: "Left\u00a0Cmd",
    right_cmd: "Right\u00a0Cmd",
    left_ctrl: "Left\u00a0Ctrl",
    right_ctrl: "Right\u00a0Ctrl",
    left_option: "Left\u00a0Option",
    right_option: "Right\u00a0Option",
    tab: "Tab",
  };
  if (SPECIAL_LABELS[key]) return SPECIAL_LABELS[key];
  return key
    .split("+")
    .map((part) => {
      if (part === "ctrl") return "Ctrl";
      if (part === "meta" || part === "cmd") return "Cmd";
      if (part === "alt") return "Alt";
      if (part === "shift") return "Shift";
      if (part.startsWith("f") && /^f\d+$/.test(part)) return part.toUpperCase();
      if (part === "space") return "Space";
      return part.toUpperCase();
    })
    .join("\u00a0+\u00a0");
}

/** Returns key string, null to cancel, or "" to keep waiting for a complete chord. */
export function hotkeyFromEvent(e: KeyboardEvent): string | null {
  if (e.code === "Escape") return null;

  const STANDALONE_MODIFIERS: Record<string, string> = {
    ShiftRight: "right_shift",
    ShiftLeft: "left_shift",
    MetaLeft: "left_cmd",
    MetaRight: "right_cmd",
    ControlLeft: "left_ctrl",
    ControlRight: "right_ctrl",
    AltLeft: "left_option",
    AltRight: "right_option",
  };

  const standalone = STANDALONE_MODIFIERS[e.code];
  if (standalone) {
    const otherMods =
      (e.code.startsWith("Shift") ? false : e.shiftKey) ||
      (e.code.startsWith("Meta") ? false : e.metaKey) ||
      (e.code.startsWith("Control") ? false : e.ctrlKey) ||
      (e.code.startsWith("Alt") ? false : e.altKey);
    if (!otherMods) return standalone;
    return "";
  }

  const parts: string[] = [];
  if (e.ctrlKey) parts.push("ctrl");
  if (e.metaKey) parts.push("meta");
  if (e.altKey) parts.push("alt");
  if (e.shiftKey) parts.push("shift");

  let keyName: string;
  if (e.code.startsWith("Key")) {
    keyName = e.code.slice(3).toLowerCase();
  } else if (e.code.startsWith("Digit")) {
    keyName = e.code.slice(5);
  } else if (e.code.startsWith("F") && /^F\d+$/.test(e.code)) {
    keyName = e.code.toLowerCase();
  } else {
    keyName = e.code.toLowerCase();
  }

  parts.push(keyName);
  return parts.join("+");
}
