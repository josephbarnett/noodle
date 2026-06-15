// S22: UsagePanel — inline summary and full table render only
// the fields actually present; absent usage gracefully omits.

import { cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { UsagePanel } from "../../src/components/UsagePanel";

afterEach(cleanup);

describe("UsagePanel inline", () => {
  it("renders nothing when usage is absent", () => {
    const { container: c1 } = render(<UsagePanel usage={null} />);
    expect(c1.querySelector(".usage-chip")).toBeNull();
    const { container: c2 } = render(<UsagePanel usage={undefined} />);
    expect(c2.querySelector(".usage-chip")).toBeNull();
  });

  it("renders nothing when tokens and latency are both absent", () => {
    const { container } = render(<UsagePanel usage={{ tokens: null, latency: null }} />);
    expect(container.querySelector(".usage-chip")).toBeNull();
  });

  it("renders input/output tokens", () => {
    const { container } = render(
      <UsagePanel
        usage={{
          tokens: { input_tokens: 100, output_tokens: 50 },
        }}
      />,
    );
    const chip = container.querySelector(".usage-chip")!;
    expect(chip.textContent).toContain("100");
    expect(chip.textContent).toContain("50");
  });

  it("includes cache and reasoning when present", () => {
    const { container } = render(
      <UsagePanel
        usage={{
          tokens: {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_input_tokens: 200,
            reasoning_tokens: 15,
          },
        }}
      />,
    );
    expect(container.textContent).toContain("cache:200");
    expect(container.textContent).toContain("reason:15");
  });

  it("formats latency in milliseconds and seconds", () => {
    const { container: short } = render(
      <UsagePanel
        usage={{
          tokens: { input_tokens: 1, output_tokens: 1 },
          latency: { total_ms: 750 },
        }}
      />,
    );
    expect(short.textContent).toContain("750ms");

    const { container: long } = render(
      <UsagePanel
        usage={{
          tokens: { input_tokens: 1, output_tokens: 1 },
          latency: { total_ms: 12345 },
        }}
      />,
    );
    expect(long.textContent).toContain("12.35s");
  });
});

describe("UsagePanel full", () => {
  it("renders a labeled table with on-disk field names", () => {
    const { container } = render(
      <UsagePanel
        mode="full"
        usage={{
          tokens: {
            input_tokens: 12,
            output_tokens: 5,
            cache_read_input_tokens: 7,
          },
          latency: { time_to_first_byte_ms: 42, total_ms: 987 },
        }}
      />,
    );
    // The on-disk wire field names are visible — `input_tokens`,
    // NOT the internal `input`.
    expect(container.textContent).toContain("input_tokens");
    expect(container.textContent).toContain("output_tokens");
    expect(container.textContent).toContain("cache_read_input_tokens");
    expect(container.textContent).toContain("time_to_first_byte_ms");
    expect(container.textContent).toContain("total_ms");
  });
});
