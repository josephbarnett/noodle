// Item 4 viewer-panel slice (ADR 020 §7): attribution feed
// contract on `EventStore`.
//
// Pins:
// - `side_effect` ServerMsgs land on `getAttribution()` in
//   arrival order.
// - `getResolvedForSession` indexes the latest Resolved per
//   `session_prefix`.
// - `clearLocal` empties both the buffer and the session index.
// - Snapshot references are stable when state hasn't changed
//   (useSyncExternalStore contract).

import { describe, expect, it } from "vitest";
import { EventStore } from "../../src/store/events";
import type { ServerMsg } from "../../src/types";

function hintMsg(value: string): ServerMsg {
  return {
    kind: "side_effect",
    event: {
      kind: "hint",
      category: "tool",
      value,
      confidence: 0.95,
      source: "user_agent",
    },
  };
}

function resolvedMsg(
  session_prefix: string,
  resolved: Record<string, string>,
): ServerMsg {
  return {
    kind: "side_effect",
    event: {
      kind: "resolved",
      session_prefix,
      flow_id: 0,
      at_unix_ms: 1779000000000,
      resolved,
    },
  };
}

describe("EventStore attribution feed", () => {
  it("ingests side_effects and exposes them in arrival order", () => {
    const store = new EventStore();
    store.ingest(hintMsg("Claude Code"));
    store.ingest(
      resolvedMsg("abc12345", { tool: "Claude Code" }),
    );
    const rows = store.getAttribution();
    expect(rows).toHaveLength(2);
    expect(rows[0].event.kind).toBe("hint");
    expect(rows[1].event.kind).toBe("resolved");
    // seq is monotonic from 1.
    expect(rows[0].seq).toBe(1);
    expect(rows[1].seq).toBe(2);
  });

  it("indexes Resolved by session_prefix and returns the latest", () => {
    const store = new EventStore();
    store.ingest(resolvedMsg("abc12345", { tool: "Claude Code" }));
    store.ingest(
      resolvedMsg("abc12345", {
        tool: "Claude Code",
        work_type: "refactor",
      }),
    );
    const row = store.getResolvedForSession("abc12345");
    expect(row).toBeDefined();
    if (row && row.event.kind === "resolved") {
      // Latest one wins.
      expect(row.event.resolved.work_type).toBe("refactor");
    } else {
      throw new Error("expected Resolved");
    }
  });

  it("getResolvedForSession returns undefined for unknown/empty sessions", () => {
    const store = new EventStore();
    expect(store.getResolvedForSession("nope")).toBeUndefined();
    expect(store.getResolvedForSession(null)).toBeUndefined();
    expect(store.getResolvedForSession(undefined)).toBeUndefined();
  });

  it("clearLocal empties the buffer and resets the session index", () => {
    const store = new EventStore();
    store.ingest(resolvedMsg("abc12345", { tool: "Claude Code" }));
    store.ingest(hintMsg("Cursor"));
    expect(store.getAttribution()).toHaveLength(2);
    expect(store.getResolvedForSession("abc12345")).toBeDefined();

    store.clearLocal();
    expect(store.getAttribution()).toHaveLength(0);
    expect(store.getResolvedForSession("abc12345")).toBeUndefined();
  });

  it("snapshot reference is stable when no new side_effects arrive", () => {
    const store = new EventStore();
    store.ingest(hintMsg("Claude Code"));
    const a = store.getAttribution();
    const b = store.getAttribution();
    expect(a).toBe(b);
    store.ingest(hintMsg("Cursor"));
    const c = store.getAttribution();
    expect(c).not.toBe(a);
  });

  it("buffer is capped at ATTRIBUTION_CAP rows", () => {
    const store = new EventStore();
    for (let i = 0; i < 5005; i++) {
      store.ingest(hintMsg(`v${i}`));
    }
    expect(store.getAttribution().length).toBeLessThanOrEqual(5000);
    // Earliest fell off; latest is preserved.
    const last = store.getAttribution().at(-1);
    if (last && last.event.kind === "hint") {
      expect(last.event.value).toBe("v5004");
    }
  });
});
