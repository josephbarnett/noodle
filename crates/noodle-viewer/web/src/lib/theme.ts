// Theme management — light / dark with localStorage persistence.
//
// The dark variant is the CSS default (`:root` tokens); the light
// variant overrides via `[data-theme="light"]`. We apply by setting
// `document.documentElement.dataset.theme`.

import { useEffect, useState } from "react";

export type Theme = "dark" | "light";

const STORAGE_KEY = "noodle-viewer:theme";

function systemPrefers(): Theme {
  if (typeof window === "undefined") return "dark";
  return window.matchMedia?.("(prefers-color-scheme: light)").matches
    ? "light"
    : "dark";
}

function readTheme(): Theme {
  if (typeof window === "undefined") return "dark";
  const saved = window.localStorage.getItem(STORAGE_KEY);
  if (saved === "dark" || saved === "light") return saved;
  return systemPrefers();
}

export function useTheme(): [Theme, (t: Theme) => void, () => void] {
  const [theme, setThemeState] = useState<Theme>(() => readTheme());

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    window.localStorage.setItem(STORAGE_KEY, theme);
  }, [theme]);

  const setTheme = (t: Theme) => setThemeState(t);
  const toggle = () => setThemeState((prev) => (prev === "dark" ? "light" : "dark"));

  return [theme, setTheme, toggle];
}
