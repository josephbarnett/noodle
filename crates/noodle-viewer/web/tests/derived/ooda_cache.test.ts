// Verifies the OODA derivation cache. We don't intercept the SSE
// parser; we instead use the `ParseCache.get` / `set` interface that
// `buildSessions` consults. By counting calls on a recording cache,
// we prove that a second pass over the same input pairs does NOT
// re-invoke `parseResponse` for cache hits.
//
// Caching is keyed by `event_id`, so the test fixtures use stable
// ids. The cache contract is intentionally narrow:
//   - `get` returns undefined on miss; the cached parsed value
//     thereafter.
//   - `set` populates the cache with the result of `parseResponse`.

import { describe, expect, it } from "vitest";
import {
  buildSessions,
  type ParseCache,
  type ParsedResponse,
} from "../../src/store/derived/ooda";
import type { ExchangePair } from "../../src/types";

class RecordingCache implements ParseCache {
  store = new Map<string, ParsedResponse>();
  getCount = 0;
  setCount = 0;
  getEvents: string[] = [];
  setEvents: string[] = [];
  get(eventId: string): ParsedResponse | undefined {
    this.getCount++;
    this.getEvents.push(eventId);
    return this.store.get(eventId);
  }
  set(eventId: string, parsed: ParsedResponse): void {
    this.setCount++;
    this.setEvents.push(eventId);
    this.store.set(eventId, parsed);
  }
}

function pair(opts: {
  id: string;
  url?: string;
  ts: string;
  sessionHash?: string;
  reqBody?: unknown;
  respBody?: unknown;
}): ExchangePair {
  return {
    event_id: opts.id,
    request: {
      direction: "request",
      timestamp: opts.ts,
      event_id: opts.id,
      provider: "anthropic",
      url: opts.url ?? "https://api.anthropic.com/v1/messages",
      method: "POST",
      session_hash: opts.sessionHash,
      headers: {},
      body: opts.reqBody,
    },
    response: {
      direction: "response",
      timestamp: opts.ts,
      event_id: opts.id,
      provider: "anthropic",
      status: 200,
      headers: {},
      body: opts.respBody,
    },
  };
}

function buildWithCache(
  pairs: ExchangePair[],
  cache: ParseCache,
): ReturnType<typeof buildSessions> {
  const m = new Map<string, ExchangePair>();
  for (const p of pairs) m.set(p.event_id, p);
  return buildSessions(pairs, m, cache);
}

describe("ParseCache", () => {
  it("second pass over the same pairs hits cache for every event", () => {
    const ps = [
      pair({
        id: "nl-1",
        ts: "2026-05-10T00:00:01Z",
        sessionHash: "s",
        reqBody: { messages: [{ role: "user", content: "hi" }] },
        respBody: { content: [{ type: "text", text: "ok" }], stop_reason: "end_turn" },
      }),
      pair({
        id: "nl-2",
        ts: "2026-05-10T00:00:02Z",
        sessionHash: "s",
        reqBody: {
          messages: [
            { role: "user", content: "hi" },
            { role: "assistant", content: "ok" },
            { role: "user", content: "again" },
          ],
        },
        respBody: { content: [{ type: "text", text: "ok2" }], stop_reason: "end_turn" },
      }),
    ];
    const cache = new RecordingCache();

    // Pass 1 — every event_id misses, every parse is then cached.
    buildWithCache(ps, cache);
    expect(cache.getCount).toBe(2);
    expect(cache.setCount).toBe(2);
    expect(cache.store.size).toBe(2);

    // Pass 2 — every event_id is a hit; no new sets.
    const setsBefore = cache.setCount;
    buildWithCache(ps, cache);
    expect(cache.setCount).toBe(setsBefore);
    expect(cache.getCount).toBeGreaterThanOrEqual(4);
  });

  it("a third pass with one new pair parses ONLY that new pair", () => {
    const ps1 = [
      pair({
        id: "nl-1",
        ts: "2026-05-10T00:00:01Z",
        sessionHash: "s",
        reqBody: { messages: [{ role: "user", content: "hi" }] },
        respBody: { content: [{ type: "text", text: "ok" }], stop_reason: "end_turn" },
      }),
    ];
    const cache = new RecordingCache();
    buildWithCache(ps1, cache);

    const setsAfterFirst = cache.setCount;
    expect(setsAfterFirst).toBe(1);

    const ps2 = [
      ...ps1,
      pair({
        id: "nl-2",
        ts: "2026-05-10T00:00:02Z",
        sessionHash: "s",
        reqBody: {
          messages: [
            { role: "user", content: "hi" },
            { role: "assistant", content: "ok" },
            { role: "user", content: "more" },
          ],
        },
        respBody: { content: [{ type: "text", text: "ok2" }], stop_reason: "end_turn" },
      }),
    ];
    buildWithCache(ps2, cache);

    // Only nl-2 parsed; nl-1 was a hit.
    expect(cache.setCount).toBe(setsAfterFirst + 1);
    expect(cache.setEvents.slice(-1)).toEqual(["nl-2"]);
  });

  it("does not pin an empty parse from a request-only pair (regression)", () => {
    // The slim WS feed delivers request + response as separate
    // Exchange messages. When the request lands first, the pair
    // briefly has `response: undefined`. If parseResponseCached
    // caches the empty parse against `event_id`, the AGENT content
    // block never renders once the response arrives — refresh is
    // the only recovery. Regression test: a build pass over a
    // request-only pair must NOT cache, so a follow-up pass with
    // the response now present produces the real assistant content.
    const requestOnly: ExchangePair = {
      event_id: "nl-1",
      request: {
        direction: "request",
        timestamp: "2026-05-10T00:00:01Z",
        event_id: "nl-1",
        provider: "anthropic",
        url: "https://api.anthropic.com/v1/messages",
        method: "POST",
        session_hash: "s",
        headers: {},
        body: { messages: [{ role: "user", content: "hi" }] },
      },
    };
    const cache = new RecordingCache();
    buildWithCache([requestOnly], cache);
    // No cache write for request-only pair.
    expect(cache.setCount).toBe(0);

    // Now the response arrives.
    const complete = pair({
      id: "nl-1",
      ts: "2026-05-10T00:00:01Z",
      sessionHash: "s",
      reqBody: { messages: [{ role: "user", content: "hi" }] },
      respBody: {
        content: [{ type: "text", text: "Hello there." }],
        stop_reason: "end_turn",
      },
    });
    const sessions = buildWithCache([complete], cache);
    expect(cache.setCount).toBe(1);
    const turns = sessions[0].agentRuns.flatMap((r) => r.turns);
    const assistant = turns[0].roundtrips[0].assistant;
    expect(assistant.some((b) => b.type === "text" && b.text === "Hello there.")).toBe(true);
  });

  it("works correctly without a cache passed (zero-overhead default)", () => {
    const ps = [
      pair({
        id: "nl-1",
        ts: "2026-05-10T00:00:01Z",
        sessionHash: "s",
        reqBody: { messages: [{ role: "user", content: "hi" }] },
        respBody: { content: [{ type: "text", text: "ok" }], stop_reason: "end_turn" },
      }),
    ];
    const m = new Map<string, ExchangePair>();
    for (const p of ps) m.set(p.event_id, p);
    // No cache passed — must still build sessions correctly.
    const sessions = buildSessions(ps, m);
    expect(sessions).toHaveLength(1);
    expect(sessions[0].agentRuns).toHaveLength(1);
  });
});
