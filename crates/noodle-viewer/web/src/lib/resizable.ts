// Resizable-pane support for the right-hand side panels (detail +
// attribution).
//
// Persists the chosen width in localStorage. Clamps to a sane range so
// a panel can't be dragged to zero or off-screen. The gutter sits on
// the LEFT edge of the panel, so dragging it leftward grows the panel.

import { useCallback, useEffect, useRef, useState } from "react";

export const DETAIL_DEFAULT = 640;
export const DETAIL_MIN = 320;
export const DETAIL_MAX = 1200;

export const ATTRIBUTION_DEFAULT = 360;
export const ATTRIBUTION_MIN = 280;
export const ATTRIBUTION_MAX = 900;

interface PaneConfig {
  storageKey: string;
  def: number;
  min: number;
  max: number;
  /** Px to keep free for the list on the left edge of the viewport. */
  reserve: number;
}

function clamp(n: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, n));
}

function readWidth(cfg: PaneConfig): number {
  if (typeof window === "undefined") return cfg.def;
  const raw = window.localStorage.getItem(cfg.storageKey);
  if (!raw) return cfg.def;
  const n = parseInt(raw, 10);
  if (Number.isNaN(n)) return cfg.def;
  return clamp(n, cfg.min, cfg.max);
}

/**
 * Generic resizable-pane hook. Returns the current width, a mousedown
 * handler to attach to the resize gutter on the pane's left edge, and
 * a live `isResizing` flag for cursor/selection styling.
 */
function useResizablePane(cfg: PaneConfig): {
  width: number;
  onResizeStart: (e: React.MouseEvent) => void;
  isResizing: boolean;
} {
  const [width, setWidth] = useState<number>(() => readWidth(cfg));
  const widthRef = useRef(width);
  const [isResizing, setIsResizing] = useState(false);
  widthRef.current = width;

  // Cap to viewport on window resize so the list stays usable.
  useEffect(() => {
    const onWindowResize = () => {
      const maxNow = Math.max(cfg.min, window.innerWidth - cfg.reserve);
      if (widthRef.current > maxNow) setWidth(maxNow);
    };
    window.addEventListener("resize", onWindowResize);
    return () => window.removeEventListener("resize", onWindowResize);
  }, [cfg.min, cfg.reserve]);

  const onResizeStart = useCallback(
    (e: React.MouseEvent) => {
      e.preventDefault();
      const startX = e.clientX;
      const startWidth = widthRef.current;
      setIsResizing(true);

      const onMove = (m: MouseEvent) => {
        const dragged = startWidth + (startX - m.clientX);
        const maxNow = Math.max(cfg.min, window.innerWidth - cfg.reserve);
        const next = clamp(dragged, cfg.min, Math.min(cfg.max, maxNow));
        setWidth(next);
        widthRef.current = next;
      };
      const onUp = () => {
        document.removeEventListener("mousemove", onMove);
        document.removeEventListener("mouseup", onUp);
        document.body.style.cursor = "";
        document.body.style.userSelect = "";
        setIsResizing(false);
        window.localStorage.setItem(cfg.storageKey, String(widthRef.current));
      };
      document.addEventListener("mousemove", onMove);
      document.addEventListener("mouseup", onUp);
      // Visual cues + prevent text selection during the drag.
      document.body.style.cursor = "col-resize";
      document.body.style.userSelect = "none";
    },
    [cfg.min, cfg.max, cfg.reserve, cfg.storageKey],
  );

  return { width, onResizeStart, isResizing };
}

const DETAIL_CONFIG: PaneConfig = {
  storageKey: "noodle-viewer:detailWidth",
  def: DETAIL_DEFAULT,
  min: DETAIL_MIN,
  max: DETAIL_MAX,
  reserve: 360,
};

const ATTRIBUTION_CONFIG: PaneConfig = {
  storageKey: "noodle-viewer:attributionWidth",
  def: ATTRIBUTION_DEFAULT,
  min: ATTRIBUTION_MIN,
  max: ATTRIBUTION_MAX,
  reserve: 360,
};

export function useResizableDetail() {
  return useResizablePane(DETAIL_CONFIG);
}

export function useResizableAttribution() {
  return useResizablePane(ATTRIBUTION_CONFIG);
}
