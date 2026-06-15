// LEARNED assembly (ADR 051) — unit tests for the per-round-trip
// knowledge index. Two round-trips share one turn; the second
// reclassifies work_type and grows its input context. getLearnedFor
// must surface the attribution, the evidence, the per-turn delta, and
// the context-token delta keyed by event_id.

import { describe, expect, it } from "vitest";
import { EventStore } from "./events";
import type { DecodedExchange, ServerMsg } from "../types";

function resolved(
  eventId: string,
  turnId: string,
  values: Record<string, string>,
): ServerMsg {
  return {
    kind: "side_effect",
    event: {
      kind: "resolved",
      session_prefix: "sess1234",
      flow_id: 0,
      at_unix_ms: 0,
      resolved: values,
      event_id: eventId,
      turn_id: turnId,
    },
  };
}

function hint(
  eventId: string,
  turnId: string,
  category: string,
  value: string,
): ServerMsg {
  return {
    kind: "side_effect",
    event: {
      kind: "hint",
      category,
      value,
      confidence: 0.99,
      source: "marker",
      event_id: eventId,
      turn_id: turnId,
    },
  };
}

function decoded(
  eventId: string,
  turnId: string,
  inputTokens: number,
): DecodedExchange {
  return {
    exchange: { event_id: eventId, direction: "response" },
    marks: {
      session_id: "s",
      role: "main",
      frame_id: "ROOT",
      turn_id: turnId,
    },
    usage: { tokens: { input_tokens: inputTokens, output_tokens: 1 } },
  } as unknown as DecodedExchange;
}

describe("EventStore LEARNED assembly", () => {
  it("keys attribution + evidence to the round-trip and computes per-turn deltas", () => {
    const store = new EventStore();
    // Two round-trips in turn T1; decoded establishes turn order +
    // context tokens.
    store.ingestDecoded(decoded("nl-1", "T1", 1000));
    store.ingestDecoded(decoded("nl-2", "T1", 1500));
    store.ingest(resolved("nl-1", "T1", { work_type: "research", project: "noodle" }));
    store.ingest(hint("nl-1", "T1", "work_type", "research"));
    store.ingest(resolved("nl-2", "T1", { work_type: "admin", project: "noodle" }));

    const first = store.getLearnedFor("nl-1");
    expect(first?.attribution.values.work_type).toBe("research");
    expect(first?.evidence).toHaveLength(1);
    expect(first?.evidence[0]).toMatchObject({ category: "work_type", source: "marker", kind: "hint" });
    // First round-trip of the turn has no predecessor → no delta.
    expect(first?.attribution.delta).toEqual({});
    expect(first?.context.input_delta).toBeNull();

    const second = store.getLearnedFor("nl-2");
    expect(second?.attribution.values.work_type).toBe("admin");
    // work_type changed from research; project did not.
    expect(second?.attribution.delta).toEqual({ work_type: "research" });
    // Context grew 1000 → 1500.
    expect(second?.context.input_tokens).toBe(1500);
    expect(second?.context.input_delta).toBe(500);
  });

  it("returns undefined for an unknown round-trip and survives a missing decoded join", () => {
    const store = new EventStore();
    expect(store.getLearnedFor("nope")).toBeUndefined();
    // Side-effect with no decoded exchange still yields a record
    // (attribution present, context empty).
    store.ingest(resolved("nl-9", "T9", { tool: "Claude Code" }));
    const lr = store.getLearnedFor("nl-9");
    expect(lr?.attribution.values.tool).toBe("Claude Code");
    expect(lr?.context.input_tokens).toBeUndefined();
  });
});
