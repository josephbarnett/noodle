// Frame ingestion contract for `EventStore`.
//
// Pins the SSE-mode store invariants so a future refactor that
// breaks them (e.g. dropping the cached snapshot, mutating arrays
// in place, forgetting clearLocal) surfaces immediately.

import { describe, expect, it } from "vitest";
import { EventStore } from "../../src/store/events";
import type { Frame, ServerMsg } from "../../src/types";

function frameMsg(req: string, idx: number, event = "content_block_delta"): ServerMsg {
  return {
    kind: "frame",
    request_id: req,
    frame_index: idx,
    timestamp: `2026-05-11T12:00:${String(idx).padStart(2, "0")}Z`,
    ts_unix_ms: 1_778_544_000_000 + idx * 50,
    event,
    data: { delta: { text: `chunk-${idx}` } },
  };
}

describe("EventStore frames", () => {
  it("ingests frame ServerMsgs and indexes by request_id", () => {
    const store = new EventStore();
    store.ingest(frameMsg("nl-7", 0, "message_start"));
    store.ingest(frameMsg("nl-7", 1));
    store.ingest(frameMsg("nl-7", 2));
    const frames = store.getFramesFor("nl-7");
    expect(frames).toHaveLength(3);
    expect(frames.map((f) => f.frame_index)).toEqual([0, 1, 2]);
    expect(frames[0].event).toBe("message_start");
  });

  it("returns empty (stable) array for unknown request_ids", () => {
    const store = new EventStore();
    const a = store.getFramesFor("nl-missing");
    const b = store.getFramesFor("nl-missing");
    expect(a).toEqual([]);
    expect(a).toBe(b); // same reference — important for useSyncExternalStore
  });

  it("produces a new array reference on each ingest (snapshot invalidation)", () => {
    const store = new EventStore();
    store.ingest(frameMsg("nl-7", 0));
    const before = store.getFramesFor("nl-7");
    store.ingest(frameMsg("nl-7", 1));
    const after = store.getFramesFor("nl-7");
    expect(after).not.toBe(before);
    expect(after).toHaveLength(2);
    expect(before).toHaveLength(1); // prior reference NOT mutated
  });

  it("getFrameSummaries reflects all request_ids with frames", () => {
    const store = new EventStore();
    store.ingest(frameMsg("nl-7", 0, "message_start"));
    store.ingest(frameMsg("nl-7", 1));
    store.ingest(frameMsg("nl-9", 0, "message_start"));
    const summaries = store.getFrameSummaries();
    const byId: Record<string, ReturnType<typeof store.getFrameSummaries>[0]> = {};
    for (const s of summaries) byId[s.request_id] = s;
    expect(Object.keys(byId).sort()).toEqual(["nl-7", "nl-9"]);
    expect(byId["nl-7"].count).toBe(2);
    expect(byId["nl-7"].first_event).toBe("message_start");
    expect(byId["nl-9"].count).toBe(1);
    expect(byId["nl-7"].first_ts).toBeLessThanOrEqual(byId["nl-7"].last_ts);
  });

  it("clearLocal drops all frame state", () => {
    const store = new EventStore();
    store.ingest(frameMsg("nl-7", 0));
    store.ingest(frameMsg("nl-7", 1));
    expect(store.getFramesFor("nl-7")).toHaveLength(2);
    store.clearLocal();
    expect(store.getFramesFor("nl-7")).toHaveLength(0);
    expect(store.getFrameSummaries()).toHaveLength(0);
  });

  it("frame ingest does not affect pairs snapshot reference", () => {
    const store = new EventStore();
    const pairsBefore = store.getPairs();
    store.ingest(frameMsg("nl-7", 0));
    const pairsAfter = store.getPairs();
    // Pairs unchanged when only frames came in. (Caching is essential
    // for useSyncExternalStore not to ping every subscriber.)
    expect(pairsAfter).toBe(pairsBefore);
  });

  it("frame summary timestamps come from first and most-recent frame", () => {
    const store = new EventStore();
    const f0 = frameMsg("nl-7", 0);
    const f1 = frameMsg("nl-7", 5);
    store.ingest(f0);
    store.ingest(f1);
    const summary = store.getFrameSummaries().find((s) => s.request_id === "nl-7")!;
    expect(summary.first_ts).toBe((f0 as Frame).ts_unix_ms);
    expect(summary.last_ts).toBe((f1 as Frame).ts_unix_ms);
  });
});
