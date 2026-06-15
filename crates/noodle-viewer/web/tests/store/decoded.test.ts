// S22: EventStore.ingestDecoded() unit tests.
//
// Pins:
//   - DecodedExchange records are indexed by `exchange.event_id`.
//   - Subsequent ingest for the same id overwrites (response wins
//     over request — response carries the richer payload).
//   - `marks.turn_id` is indexed for OODA grouping.
//   - `clearLocal()` wipes the decoded indices alongside the slim
//     exchange caches.
//   - Listeners fire on ingest so React subscribers re-render.

import { describe, expect, it } from "vitest";
import { EventStore } from "../../src/store/events";
import type { DecodedExchange } from "../../src/types";

function dx(
  eventId: string,
  patch: Partial<DecodedExchange> = {},
): DecodedExchange {
  return {
    exchange: {
      direction: "request",
      timestamp: "2026-05-21T00:00:00Z",
      event_id: eventId,
      provider: "anthropic",
    },
    ...patch,
  };
}

describe("EventStore.ingestDecoded", () => {
  it("indexes records by event_id and returns them via getDecodedFor", () => {
    const store = new EventStore();
    store.ingestDecoded(
      dx("nl-1", { marks: { session_id: "s", role: "main", frame_id: "ROOT", turn_id: "turn_a" } }),
    );
    const got = store.getDecodedFor("nl-1");
    expect(got).toBeDefined();
    expect(got?.marks?.turn_id).toBe("turn_a");
  });

  it("returns undefined for unknown ids", () => {
    const store = new EventStore();
    expect(store.getDecodedFor("nl-missing")).toBeUndefined();
    expect(store.getDecodedFor(null)).toBeUndefined();
    expect(store.getDecodedFor(undefined)).toBeUndefined();
  });

  it("the response side wins over the request side for the same id", () => {
    const store = new EventStore();
    store.ingestDecoded(dx("nl-2", { marks: { session_id: "s", role: "main", frame_id: "ROOT", turn_id: "t" } }));
    // Response — carries usage, no marks (proxy only stamps marks
    // on the request side).
    store.ingestDecoded({
      exchange: {
        direction: "response",
        timestamp: "2026-05-21T00:00:01Z",
        event_id: "nl-2",
        provider: "anthropic",
        status: 200,
      },
      usage: { tokens: { input_tokens: 12, output_tokens: 5 } },
    });
    const got = store.getDecodedFor("nl-2");
    expect(got?.exchange.direction).toBe("response");
    expect(got?.usage?.tokens?.input_tokens).toBe(12);
  });

  it("indexes event_ids by marks.turn_id in arrival order", () => {
    const store = new EventStore();
    store.ingestDecoded(
      dx("nl-1", { marks: { session_id: "s", role: "main", frame_id: "ROOT", turn_id: "turn_a" } }),
    );
    store.ingestDecoded(
      dx("nl-2", { marks: { session_id: "s", role: "main", frame_id: "ROOT", turn_id: "turn_a" } }),
    );
    store.ingestDecoded(
      dx("nl-3", { marks: { session_id: "s", role: "main", frame_id: "ROOT", turn_id: "turn_b" } }),
    );
    expect(store.getEventIdsForTurn("turn_a")).toEqual(["nl-1", "nl-2"]);
    expect(store.getEventIdsForTurn("turn_b")).toEqual(["nl-3"]);
  });

  it("clearLocal() wipes the decoded indices", () => {
    const store = new EventStore();
    store.ingestDecoded(
      dx("nl-1", { marks: { session_id: "s", role: "main", frame_id: "ROOT", turn_id: "t" } }),
    );
    store.clearLocal();
    expect(store.getDecodedFor("nl-1")).toBeUndefined();
    expect(store.getEventIdsForTurn("t")).toEqual([]);
  });

  it("notifies subscribers on ingest", () => {
    const store = new EventStore();
    let calls = 0;
    store.subscribe(() => {
      calls++;
    });
    store.ingestDecoded(dx("nl-1"));
    expect(calls).toBeGreaterThan(0);
  });

  it("records without an event_id are dropped (defensive)", () => {
    const store = new EventStore();
    store.ingestDecoded({
      exchange: {
        direction: "request",
        timestamp: "t",
        event_id: "",
        provider: "anthropic",
      },
    });
    expect(store.getDecodedFor("")).toBeUndefined();
  });
});
