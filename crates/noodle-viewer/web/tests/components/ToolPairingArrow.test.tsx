// S22: ToolPairingArrow — back-arrow when a tool_result resolves
// an earlier tool_use; forward-arrow when a tool_use is resolved
// by a subsequent request; nothing when pairing is absent.

import { cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ToolPairingArrow } from "../../src/components/ToolPairingArrow";

afterEach(cleanup);

describe("ToolPairingArrow", () => {
  it("renders nothing when pairing is absent", () => {
    const { container: c1 } = render(<ToolPairingArrow pairing={null} />);
    expect(c1.querySelector(".tool-pairing")).toBeNull();
    const { container: c2 } = render(<ToolPairingArrow pairing={undefined} />);
    expect(c2.querySelector(".tool-pairing")).toBeNull();
  });

  it("renders nothing when both pairing fields are null", () => {
    const { container } = render(
      <ToolPairingArrow
        pairing={{
          resolves_tool_use_in_request_id: null,
          resolved_by_request_id: null,
        }}
      />,
    );
    expect(container.querySelector(".tool-pairing-arrow")).toBeNull();
  });

  it("renders a back-arrow for a tool_result resolving an earlier tool_use", () => {
    const { container } = render(
      <ToolPairingArrow
        pairing={{ resolves_tool_use_in_request_id: "nl-100" }}
      />,
    );
    const arrows = container.querySelectorAll(".tool-pairing-arrow");
    expect(arrows).toHaveLength(1);
    expect(arrows[0].textContent).toContain("←");
    expect(arrows[0].textContent).toContain("nl-100");
  });

  it("renders a forward-arrow for a tool_use resolved by a later request", () => {
    const { container } = render(
      <ToolPairingArrow pairing={{ resolved_by_request_id: "nl-200" }} />,
    );
    const arrows = container.querySelectorAll(".tool-pairing-arrow");
    expect(arrows).toHaveLength(1);
    expect(arrows[0].textContent).toContain("→");
    expect(arrows[0].textContent).toContain("nl-200");
  });

  it("invokes onJump with the target event_id when clicked", () => {
    const onJump = vi.fn();
    const { container } = render(
      <ToolPairingArrow
        pairing={{ resolved_by_request_id: "nl-99" }}
        onJump={onJump}
      />,
    );
    const arrow = container.querySelector(".tool-pairing-arrow")!;
    fireEvent.click(arrow);
    expect(onJump).toHaveBeenCalledWith("nl-99");
  });
});
