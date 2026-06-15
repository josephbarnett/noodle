// S22: TurnIdBadge — renders a short suffix of the ULID, shows
// the full id in `title`, omits entirely when no turn_id.

import { cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { TurnIdBadge } from "../../src/components/TurnIdBadge";

afterEach(cleanup);

describe("TurnIdBadge", () => {
  it("renders a short suffix of the turn id", () => {
    const { container } = render(
      <TurnIdBadge turnId="01HV5GH8X8WJ6E0CMQ8Q3Z4N9R" />,
    );
    const badge = container.querySelector(".turn-id-badge")!;
    expect(badge).toBeTruthy();
    expect(badge.textContent).toContain("Z4N9R");
    // Full id surfaced via the title attribute (hover for full).
    expect(badge.getAttribute("title")).toContain("01HV5GH8X8WJ6E0CMQ8Q3Z4N9R");
  });

  it("renders the full id when shorter than 6 chars", () => {
    const { container } = render(<TurnIdBadge turnId="abc" />);
    const badge = container.querySelector(".turn-id-badge")!;
    expect(badge.textContent).toBe("turn:abc");
  });

  it("renders nothing when turnId is absent (graceful degradation)", () => {
    const { container: c1 } = render(<TurnIdBadge turnId={null} />);
    expect(c1.querySelector(".turn-id-badge")).toBeNull();
    const { container: c2 } = render(<TurnIdBadge turnId={undefined} />);
    expect(c2.querySelector(".turn-id-badge")).toBeNull();
    const { container: c3 } = render(<TurnIdBadge turnId="" />);
    expect(c3.querySelector(".turn-id-badge")).toBeNull();
  });
});
