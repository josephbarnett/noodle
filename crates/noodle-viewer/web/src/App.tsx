import { useEffect, useMemo, useState, useSyncExternalStore } from "react";
import { AttributionPanel } from "./components/AttributionPanel";
import { CaptureControls } from "./components/CaptureControls";
import { ModeSwitcher, type Mode } from "./components/ModeSwitcher";
import { RowDetail } from "./components/RowDetail";
import { ThemeToggle } from "./components/ThemeToggle";
import { HttpMode } from "./modes/HttpMode";
import { OodaMode } from "./modes/OodaMode";
import { OtlpMode } from "./modes/OtlpMode";
import { api } from "./lib/api";
import { DecodedSseClient } from "./lib/decodedSse";
import { useResizableAttribution, useResizableDetail } from "./lib/resizable";
import { useTheme } from "./lib/theme";
import { WsClient } from "./lib/ws";
import { EventStore } from "./store/events";

const store = new EventStore();

function wsUrl(): string {
  const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${window.location.host}/ws`;
}

export function App() {
  const [mode, setMode] = useState<Mode>("http");
  const [selected, setSelected] = useState<string | null>(null);
  const [attributionOpen, setAttributionOpen] = useState(false);
  const [theme, , toggleTheme] = useTheme();
  const { width: detailWidth, onResizeStart, isResizing } = useResizableDetail();
  const {
    width: attributionWidth,
    onResizeStart: onAttributionResizeStart,
    isResizing: isResizingAttribution,
  } = useResizableAttribution();

  useEffect(() => {
    api.status().catch(() => {
      /* badge stays disconnected; ws onopen will fix */
    });
    const client = new WsClient({
      url: wsUrl(),
      onMessage: (m) => store.ingest(m),
      onConnected: () => store.setConnected(true),
      onDisconnected: () => store.setConnected(false),
    });
    // S22 (refactor-overview.md §10): typed DecodedExchange feed
    // riding alongside the legacy WS path. Same lifecycle: opens at
    // mount, closes on unmount. Browser EventSource handles
    // reconnect internally.
    const decoded = new DecodedSseClient({
      url: "/api/decoded-exchanges",
      onDecodedExchange: (dx) => store.ingestDecoded(dx),
    });
    return () => {
      client.close();
      decoded.close();
    };
  }, []);

  // Esc closes the detail panel.
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") setSelected(null);
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, []);

  const pairs = useSyncExternalStore(
    (cb) => store.subscribe(cb),
    () => store.getPairs(),
  );
  const capture = useSyncExternalStore(
    (cb) => store.subscribe(cb),
    () => store.getCapture(),
  );
  const connected = useSyncExternalStore(
    (cb) => store.subscribe(cb),
    () => store.isConnected(),
  );
  const attribution = useSyncExternalStore(
    (cb) => store.subscribe(cb),
    () => store.getAttribution(),
  );

  const resolvedCount = useMemo(
    () => attribution.filter((r) => r.event.kind === "resolved").length,
    [attribution],
  );

  const stats = useMemo(() => ({ pairs: pairs.length }), [pairs.length]);
  const selectedPair = useMemo(
    () => pairs.find((p) => p.event_id === selected),
    [pairs, selected],
  );
  const handleSelect = (id: string) => {
    setSelected((prev) => (prev === id ? null : id));
  };

  const showDetail = mode === "http" && !!selectedPair;
  const showAttribution = attributionOpen;
  // Workspace grid columns depend on which side-panels are visible.
  // Layout order, left to right: main · detail-handle · detail-panel ·
  // attribution-handle · attribution-panel. The grid keeps the main
  // pane elastic (`1fr`); each side panel has its own drag gutter
  // (`6px`) and a user-resizable width.
  const workspaceStyle = (() => {
    if (showDetail && showAttribution)
      return {
        gridTemplateColumns: `1fr 6px ${detailWidth}px 6px ${attributionWidth}px`,
      };
    if (showDetail)
      return { gridTemplateColumns: `1fr 6px ${detailWidth}px` };
    if (showAttribution)
      return { gridTemplateColumns: `1fr 6px ${attributionWidth}px` };
    return undefined;
  })();

  return (
    <div className="app">
      <header className="topbar">
        <h1>noodle viewer</h1>
        <ModeSwitcher mode={mode} onChange={setMode} />
        <span className="badge idle">{stats.pairs} exchanges</span>
        <span
          className={
            "badge " +
            (!connected
              ? "disconnected"
              : capture.enabled
                ? "live"
                : "idle")
          }
          title={capture.file ?? ""}
        >
          {connected ? (capture.enabled ? "● LIVE" : "○ idle") : "✕ disconnected"}
        </span>
        <span className="spacer" />
        <button
          type="button"
          className={`badge ${attributionOpen ? "live" : "idle"}`}
          onClick={() => setAttributionOpen((v) => !v)}
          title="Toggle attribution panel"
        >
          Attribution {resolvedCount > 0 && <>· {resolvedCount}</>}
        </button>
        <ThemeToggle theme={theme} onToggle={toggleTheme} />
        <CaptureControls
          capture={capture}
          onLocalClear={() => {
            store.clearLocal();
            setSelected(null);
          }}
        />
      </header>
      <section
        className={`workspace${showDetail ? " with-detail" : ""}${isResizing || isResizingAttribution ? " resizing" : ""}`}
        style={workspaceStyle}
      >
        {mode === "http" && (
          <HttpMode
            pairs={pairs}
            selected={selected}
            onSelect={handleSelect}
            resolvedFor={(p) => store.getResolvedForSession(p)}
            decodedFor={(id) => store.getDecodedFor(id)}
            brainFor={(id) => store.getBrainFor(id)}
          />
        )}
        {mode === "ooda" && (
          <OodaMode
            pairs={pairs}
            parseCache={store.parseCache}
            getMarks={(id) => store.getDecodedFor(id)?.marks ?? null}
          />
        )}
        {mode === "otlp" && <OtlpMode />}
        {showDetail && selectedPair && (
          <>
            <div
              className="resize-handle"
              onMouseDown={onResizeStart}
              title="Drag to resize"
              aria-label="Resize detail panel"
              role="separator"
            />
            <RowDetail
              pair={selectedPair}
              onClose={() => setSelected(null)}
              decoded={store.getDecodedFor(selectedPair.event_id)}
              learned={store.getLearnedFor(selectedPair.event_id)}
              contextWeight={store.getContextWeightFor(selectedPair.event_id)}
              onJumpTo={(id) => setSelected(id)}
            />
          </>
        )}
        {showAttribution && (
          <>
            <div
              className="resize-handle"
              onMouseDown={onAttributionResizeStart}
              title="Drag to resize"
              aria-label="Resize attribution panel"
              role="separator"
            />
            <AttributionPanel
              rows={attribution}
              onClose={() => setAttributionOpen(false)}
            />
          </>
        )}
      </section>
    </div>
  );
}
