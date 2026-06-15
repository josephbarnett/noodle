import type { Theme } from "../lib/theme";

interface Props {
  theme: Theme;
  onToggle: () => void;
}

export function ThemeToggle({ theme, onToggle }: Props) {
  const label = theme === "dark" ? "Light" : "Dark";
  return (
    <button
      className="theme-toggle"
      onClick={onToggle}
      title={`Switch to ${label.toLowerCase()} theme`}
      aria-label={`Switch to ${label.toLowerCase()} theme`}
    >
      {theme === "dark" ? "☀" : "☾"} {label}
    </button>
  );
}
