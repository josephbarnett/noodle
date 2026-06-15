// Smoke + keyboard-nav contract for HttpMode. Pins what makes the
// list usable at a real-traffic scale: rows render with selection
// state, ↑/↓ move selection (clamped at the edges), and Enter on a
// focused row fires onSelect.

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { HttpMode } from "../../src/modes/HttpMode";
import type { ExchangePair } from "../../src/types";

function pair(id: string, ts: string, url: string): ExchangePair {
  return {
    event_id: id,
    request: {
      direction: "request",
      timestamp: ts,
      event_id: id,
      provider: "anthropic",
      method: "POST",
      url,
      headers: {},
      body: null,
    },
    response: {
      direction: "response",
      timestamp: ts,
      event_id: id,
      provider: "anthropic",
      status: 200,
      headers: {},
      body: null,
    },
  };
}

const PAIRS: ExchangePair[] = [
  pair("nl-1", "2026-05-11T12:00:00.001Z", "https://api.anthropic.com/v1/messages"),
  pair("nl-2", "2026-05-11T12:00:00.002Z", "https://api.anthropic.com/v1/messages"),
  pair("nl-3", "2026-05-11T12:00:00.003Z", "https://api.anthropic.com/v1/messages"),
];

describe("HttpMode", () => {
  // testing-library auto-cleanup isn't on (vitest globals off);
  // unmount manually between cases so window-level listeners and
  // DOM don't leak.
  afterEach(() => cleanup());

  it("renders one row per pair; selected row carries the selected class", () => {
    const { container } = render(
      <HttpMode pairs={PAIRS} selected="nl-2" onSelect={() => {}} />,
    );
    const rows = container.querySelectorAll(".http-row");
    expect(rows).toHaveLength(3);
    // Exactly one .selected; the row order matches the sorted-by-ts
    // order so the middle row is the selected one.
    const selectedRows = container.querySelectorAll(".http-row.selected");
    expect(selectedRows).toHaveLength(1);
    const allRows = Array.from(rows);
    expect(allRows.indexOf(selectedRows[0])).toBe(1);
  });

  it("ArrowDown moves selection to the next row (sorted order)", () => {
    const onSelect = vi.fn();
    render(<HttpMode pairs={PAIRS} selected="nl-1" onSelect={onSelect} />);
    fireEvent.keyDown(window, { key: "ArrowDown" });
    expect(onSelect).toHaveBeenCalledWith("nl-2");
  });

  it("ArrowUp at the first row clamps (no onSelect call)", () => {
    const onSelect = vi.fn();
    render(<HttpMode pairs={PAIRS} selected="nl-1" onSelect={onSelect} />);
    fireEvent.keyDown(window, { key: "ArrowUp" });
    expect(onSelect).not.toHaveBeenCalled();
  });

  it("ArrowDown with no selection picks the first row", () => {
    const onSelect = vi.fn();
    render(<HttpMode pairs={PAIRS} selected={null} onSelect={onSelect} />);
    fireEvent.keyDown(window, { key: "ArrowDown" });
    expect(onSelect).toHaveBeenCalledWith("nl-1");
  });

  it("ArrowUp with no selection picks the last row", () => {
    const onSelect = vi.fn();
    render(<HttpMode pairs={PAIRS} selected={null} onSelect={onSelect} />);
    fireEvent.keyDown(window, { key: "ArrowUp" });
    expect(onSelect).toHaveBeenCalledWith("nl-3");
  });

  it("Enter on a focused row fires onSelect", () => {
    const onSelect = vi.fn();
    const { container } = render(
      <HttpMode pairs={PAIRS} selected={null} onSelect={onSelect} />,
    );
    const row = container.querySelector('.http-row[aria-pressed="false"]');
    expect(row).not.toBeNull();
    fireEvent.keyDown(row!, { key: "Enter" });
    expect(onSelect).toHaveBeenCalled();
  });

  it("rows expose role=button + tabIndex=0 + aria-pressed reflecting selection", () => {
    const { container } = render(
      <HttpMode pairs={PAIRS} selected="nl-2" onSelect={() => {}} />,
    );
    const rows = Array.from(container.querySelectorAll(".http-row"));
    expect(rows.every((r) => r.getAttribute("role") === "button")).toBe(true);
    expect(rows.every((r) => r.getAttribute("tabindex") === "0")).toBe(true);
    const pressed = rows.find((r) => r.getAttribute("aria-pressed") === "true");
    expect(pressed).not.toBeUndefined();
    expect(rows.filter((r) => r.getAttribute("aria-pressed") === "true")).toHaveLength(1);
  });

  it("typing in an input swallows the arrow key (no onSelect)", () => {
    const onSelect = vi.fn();
    render(
      <>
        <input data-testid="search" />
        <HttpMode pairs={PAIRS} selected="nl-1" onSelect={onSelect} />
      </>,
    );
    const input = screen.getByTestId("search");
    input.focus();
    fireEvent.keyDown(input, { key: "ArrowDown" });
    expect(onSelect).not.toHaveBeenCalled();
  });
});
