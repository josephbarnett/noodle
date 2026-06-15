import { useState } from "react";
import { api } from "../lib/api";
import type { CaptureState } from "../types";

interface Props {
  capture: CaptureState;
  onLocalClear: () => void;
}

export function CaptureControls({ capture, onLocalClear }: Props) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const wrap = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      await fn();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div style={{ display: "flex", gap: "0.5rem", alignItems: "center" }}>
      {capture.enabled ? (
        <button
          className="danger"
          disabled={busy}
          onClick={() => wrap(() => api.disable())}
        >
          Stop Capture
        </button>
      ) : (
        <button
          className="primary"
          disabled={busy}
          onClick={() => wrap(() => api.enable())}
        >
          Start Capture
        </button>
      )}
      <button
        disabled={busy}
        onClick={() =>
          wrap(async () => {
            await api.clear();
            onLocalClear();
          })
        }
        title="Clear the viewer's in-memory list. Restart noodle to truncate the on-disk log."
      >
        Clear
      </button>
      {error && (
        <span style={{ color: "#fca5a5", fontSize: "0.8rem" }}>{error}</span>
      )}
    </div>
  );
}
