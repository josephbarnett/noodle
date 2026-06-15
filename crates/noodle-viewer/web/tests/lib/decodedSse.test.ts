// S22: DecodedSseClient — wraps the browser's EventSource. The
// jsdom test environment doesn't ship a native EventSource, so we
// install a minimal stub to exercise the listener wiring +
// payload parsing without spinning up a real HTTP server.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DecodedSseClient } from "../../src/lib/decodedSse";
import type { DecodedExchange } from "../../src/types";

/**
 * Minimal EventSource stub. Captures listeners and exposes a
 * `dispatch(eventName, data)` helper the test driver uses to
 * simulate incoming SSE frames.
 */
class StubEventSource {
  static last: StubEventSource | null = null;
  url: string;
  closed = false;
  onerror: ((e: Event) => void) | null = null;
  private listeners = new Map<string, ((e: MessageEvent) => void)[]>();

  constructor(url: string) {
    this.url = url;
    StubEventSource.last = this;
  }
  addEventListener(name: string, fn: (e: MessageEvent) => void): void {
    const arr = this.listeners.get(name) ?? [];
    arr.push(fn);
    this.listeners.set(name, arr);
  }
  close(): void {
    this.closed = true;
  }
  dispatch(name: string, data: string): void {
    const ev = { data } as MessageEvent;
    for (const fn of this.listeners.get(name) ?? []) fn(ev);
  }
}

beforeEach(() => {
  (globalThis as unknown as { EventSource: typeof StubEventSource }).EventSource =
    StubEventSource;
  StubEventSource.last = null;
});
afterEach(() => {
  delete (globalThis as unknown as { EventSource?: unknown }).EventSource;
});

describe("DecodedSseClient", () => {
  it("opens an EventSource to the configured url", () => {
    const onDecodedExchange = vi.fn();
    const client = new DecodedSseClient({
      url: "/api/decoded-exchanges",
      onDecodedExchange,
    });
    expect(StubEventSource.last?.url).toBe("/api/decoded-exchanges");
    client.close();
  });

  it("parses decoded_exchange frames and forwards to the handler", () => {
    const onDecodedExchange = vi.fn();
    new DecodedSseClient({
      url: "/api/decoded-exchanges",
      onDecodedExchange,
    });
    const dx: DecodedExchange = {
      exchange: {
        direction: "response",
        timestamp: "t",
        event_id: "nl-1",
        provider: "anthropic",
      },
      marks: { session_id: "s", role: "main", frame_id: "ROOT", turn_id: "turn_a" },
    };
    StubEventSource.last!.dispatch("decoded_exchange", JSON.stringify(dx));
    expect(onDecodedExchange).toHaveBeenCalledTimes(1);
    const got = onDecodedExchange.mock.calls[0][0] as DecodedExchange;
    expect(got.exchange.event_id).toBe("nl-1");
    expect(got.marks?.turn_id).toBe("turn_a");
  });

  it("ignores frames that fail to JSON-parse", () => {
    const onDecodedExchange = vi.fn();
    new DecodedSseClient({
      url: "/api/decoded-exchanges",
      onDecodedExchange,
    });
    // No throw — bad JSON is swallowed and logged.
    expect(() =>
      StubEventSource.last!.dispatch("decoded_exchange", "not-json"),
    ).not.toThrow();
    expect(onDecodedExchange).not.toHaveBeenCalled();
  });

  it("invokes onLag with the parsed integer count", () => {
    const onLag = vi.fn();
    new DecodedSseClient({
      url: "/api/decoded-exchanges",
      onDecodedExchange: vi.fn(),
      onLag,
    });
    StubEventSource.last!.dispatch("lag", "42");
    expect(onLag).toHaveBeenCalledWith(42);
  });

  it("close() releases the EventSource", () => {
    const client = new DecodedSseClient({
      url: "/api/decoded-exchanges",
      onDecodedExchange: vi.fn(),
    });
    client.close();
    expect(StubEventSource.last!.closed).toBe(true);
  });
});
