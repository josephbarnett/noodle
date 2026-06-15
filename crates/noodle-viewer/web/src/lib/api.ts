// REST client for /api/tap/*.

import type { CaptureState } from "../types";

async function call(path: string, method: string): Promise<CaptureState | null> {
  const r = await fetch(path, { method });
  if (r.status === 501) {
    // /api/tap/clear may return 501 if the engine clear isn't wired.
    return null;
  }
  if (!r.ok) throw new Error(`${method} ${path} → ${r.status}`);
  return (await r.json()) as CaptureState;
}

export const api = {
  status: () => call("/api/tap/status", "GET"),
  enable: () => call("/api/tap/enable", "POST"),
  disable: () => call("/api/tap/disable", "POST"),
  clear: () => fetch("/api/tap/clear", { method: "POST" }).then((r) => r.json()),
};
