// Shared "open/closed" state with localStorage persistence.
//
// Mirrors the useState + useEffect pattern in `theme.ts`. Used by
// `RowDetail` so collapsing REQUEST or RESPONSE persists across row
// selections (the choice is keyed by section name, not by the
// specific exchange).
//
// Storage values are `"0"` / `"1"` rather than JSON — cheaper to
// parse and matches the flat-key convention of `noodle-viewer:theme`.

import { useEffect, useState } from "react";

function read(key: string, defaultOpen: boolean): boolean {
  if (typeof window === "undefined") return defaultOpen;
  const saved = window.localStorage.getItem(key);
  if (saved === "1") return true;
  if (saved === "0") return false;
  return defaultOpen;
}

/**
 * `[open, toggle]` pair backed by `localStorage` under `key`.
 *
 * - Reads on first mount; absent / unrecognised values fall through
 *   to `defaultOpen`.
 * - Writes on every state change via `useEffect`.
 * - SSR-safe via the `typeof window` guard on the read path.
 */
export function usePersistedToggle(
  key: string,
  defaultOpen: boolean,
): [boolean, () => void] {
  const [open, setOpen] = useState<boolean>(() => read(key, defaultOpen));
  useEffect(() => {
    if (typeof window === "undefined") return;
    window.localStorage.setItem(key, open ? "1" : "0");
  }, [key, open]);
  return [open, () => setOpen((prev) => !prev)];
}
