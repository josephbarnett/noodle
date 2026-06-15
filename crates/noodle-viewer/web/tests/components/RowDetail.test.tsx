// RowDetail collapse-with-persistence contract.
//
// Pins:
//   - REQUEST + RESPONSE both open by default (no localStorage prefs).
//   - Clicking a section header hides ALL of that section (headers
//     table + body), leaves the other untouched.
//   - The collapse choice persists across `pair` prop changes
//     (re-render with a different exchange keeps the section
//     folded — that's the whole point).
//   - aria-expanded reflects state.

import { cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { RowDetail } from "../../src/components/RowDetail";
import type { ExchangePair } from "../../src/types";

function pair(id: string): ExchangePair {
  return {
    event_id: id,
    request: {
      direction: "request",
      timestamp: "2026-05-11T12:00:00Z",
      event_id: id,
      provider: "anthropic",
      method: "POST",
      url: "https://api.anthropic.com/v1/messages",
      headers: { "content-type": ["application/json"] },
      body: { model: "claude-haiku-4-5" },
    },
    response: {
      direction: "response",
      timestamp: "2026-05-11T12:00:00Z",
      event_id: id,
      provider: "anthropic",
      status: 200,
      headers: { "content-type": ["application/json"] },
      body: { id: "msg_01" },
    },
  };
}

function reqHead(container: HTMLElement): HTMLButtonElement {
  return container.querySelector(
    ".row-detail-section.request .row-detail-section-head",
  ) as HTMLButtonElement;
}
function resHead(container: HTMLElement): HTMLButtonElement {
  return container.querySelector(
    ".row-detail-section.response .row-detail-section-head",
  ) as HTMLButtonElement;
}

describe("RowDetail collapse", () => {
  // Each test starts with a clean localStorage so the default-open
  // assertion isn't contaminated by an earlier toggle.
  beforeEach(() => localStorage.clear());
  afterEach(() => cleanup());

  it("renders both sections open by default", () => {
    const { container } = render(<RowDetail pair={pair("nl-1")} onClose={() => {}} />);
    const req = container.querySelector(".row-detail-section.request");
    const res = container.querySelector(".row-detail-section.response");
    expect(req?.classList.contains("open")).toBe(true);
    expect(res?.classList.contains("open")).toBe(true);
    expect(reqHead(container).getAttribute("aria-expanded")).toBe("true");
    expect(resHead(container).getAttribute("aria-expanded")).toBe("true");
    // Body view present.
    expect(container.querySelector(".row-detail-section.request .body-pre")).not.toBeNull();
  });

  it("clicking REQUEST header hides REQUEST headers + body; RESPONSE stays", () => {
    const { container } = render(<RowDetail pair={pair("nl-1")} onClose={() => {}} />);
    fireEvent.click(reqHead(container));
    const req = container.querySelector(".row-detail-section.request");
    const res = container.querySelector(".row-detail-section.response");
    expect(req?.classList.contains("open")).toBe(false);
    expect(res?.classList.contains("open")).toBe(true);
    // REQUEST body + headers table gone.
    expect(container.querySelector(".row-detail-section.request .body-pre")).toBeNull();
    expect(container.querySelector(".row-detail-section.request .headers-table")).toBeNull();
    // RESPONSE body still visible.
    expect(container.querySelector(".row-detail-section.response .body-pre")).not.toBeNull();
  });

  it("collapse choice persists across pair changes (the core of the fix)", () => {
    const { container, rerender } = render(
      <RowDetail pair={pair("nl-1")} onClose={() => {}} />,
    );
    fireEvent.click(reqHead(container));
    expect(
      container.querySelector(".row-detail-section.request")?.classList.contains("open"),
    ).toBe(false);
    // Simulate clicking a different row — the parent passes a new
    // pair; the persisted state in localStorage means REQUEST
    // stays folded.
    rerender(<RowDetail pair={pair("nl-9")} onClose={() => {}} />);
    expect(
      container.querySelector(".row-detail-section.request")?.classList.contains("open"),
    ).toBe(false);
  });

  it("persists across remount (simulates page reload reading localStorage)", () => {
    const { container, unmount } = render(
      <RowDetail pair={pair("nl-1")} onClose={() => {}} />,
    );
    fireEvent.click(reqHead(container));
    unmount();
    // Mount a fresh component — should read the persisted "0".
    const { container: c2 } = render(
      <RowDetail pair={pair("nl-1")} onClose={() => {}} />,
    );
    expect(
      c2.querySelector(".row-detail-section.request")?.classList.contains("open"),
    ).toBe(false);
    expect(reqHead(c2).getAttribute("aria-expanded")).toBe("false");
  });

  it("aria-expanded toggles in sync with click", () => {
    const { container } = render(<RowDetail pair={pair("nl-1")} onClose={() => {}} />);
    expect(reqHead(container).getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(reqHead(container));
    expect(reqHead(container).getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(reqHead(container));
    expect(reqHead(container).getAttribute("aria-expanded")).toBe("true");
  });

  // S22 (refactor-overview.md §10): decoded panel renders only when
  // a DecodedExchange prop is passed. Without it, the existing
  // legacy layout is byte-identical — graceful degradation.
  it("does not render the decoded panel when `decoded` prop is absent", () => {
    const { container } = render(<RowDetail pair={pair("nl-1")} onClose={() => {}} />);
    expect(container.querySelector(".row-detail-decoded")).toBeNull();
  });

  it("renders the decoded panel with marks/usage/envelope chips when decoded is supplied", () => {
    const { container } = render(
      <RowDetail
        pair={pair("nl-1")}
        onClose={() => {}}
        decoded={{
          exchange: pair("nl-1").response!,
          marks: { session_id: "s", role: "main", frame_id: "ROOT", turn_id: "01HV5GH8X8WJ6E0CMQ8Q3Z4N9R" },
          usage: {
            tokens: { input_tokens: 12, output_tokens: 5 },
            latency: { total_ms: 987 },
          },
          envelope: {
            collector_app: {
              name: "noodle",
              version: "0.0.1",
              build_hash: "deadbeef",
              build_date: "2026-05-21T00:00:00Z",
              features: ["tap"],
            },
          },
        }}
      />,
    );
    expect(container.querySelector(".row-detail-decoded")).not.toBeNull();
    expect(container.querySelector(".turn-id-badge")).not.toBeNull();
    expect(container.querySelector(".usage-chip")).not.toBeNull();
    expect(container.querySelector(".envelope-inspector")).not.toBeNull();
  });
});
