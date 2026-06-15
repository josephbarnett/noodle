// S22: EnvelopeInspector — collapsed by default; expanding reveals
// agent_app / machine / collector_app / subscription groups; absent
// inner groups don't render.

import { cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { EnvelopeInspector } from "../../src/components/EnvelopeInspector";

afterEach(cleanup);

describe("EnvelopeInspector", () => {
  it("renders nothing when envelope is absent", () => {
    const { container: c1 } = render(<EnvelopeInspector envelope={null} />);
    expect(c1.querySelector(".envelope-inspector")).toBeNull();
    const { container: c2 } = render(<EnvelopeInspector envelope={undefined} />);
    expect(c2.querySelector(".envelope-inspector")).toBeNull();
  });

  it("renders nothing when envelope has no inner fields", () => {
    const { container } = render(<EnvelopeInspector envelope={{}} />);
    expect(container.querySelector(".envelope-inspector")).toBeNull();
  });

  it("shows a summary line in the collapsed state", () => {
    const { container } = render(
      <EnvelopeInspector
        envelope={{
          agent_app: {
            name: "claude_code",
            version: "0.2.5",
            source: "user_agent_header",
          },
          collector_app: {
            name: "noodle",
            version: "0.0.1",
            build_hash: "deadbeef",
            build_date: "2026-05-21T00:00:00Z",
            features: ["tap"],
          },
        }}
      />,
    );
    const summary = container.querySelector(".envelope-summary")!;
    expect(summary.textContent).toContain("claude_code");
    expect(summary.textContent).toContain("noodle");
    // Collapsed: body is hidden.
    expect(container.querySelector(".envelope-body")).toBeNull();
  });

  it("expands on click revealing the typed fields", () => {
    const { container } = render(
      <EnvelopeInspector
        envelope={{
          agent_app: {
            name: "claude_code",
            version: "0.2.5",
            source: "user_agent_header",
          },
        }}
      />,
    );
    const head = container.querySelector(".envelope-head")! as HTMLButtonElement;
    fireEvent.click(head);
    const body = container.querySelector(".envelope-body");
    expect(body).toBeTruthy();
    expect(body!.textContent).toContain("agent_app");
    expect(body!.textContent).toContain("claude_code");
    expect(body!.textContent).toContain("user_agent_header");
  });

  it("renders subscription fields when present", () => {
    const { container } = render(
      <EnvelopeInspector
        defaultOpen
        envelope={{
          subscription: {
            api_key: { prefix: "sk-ant-api03-w", kind: "api_key", source: "x_api_key" },
            organization: {
              organization_id: "org_a1b2",
              account_type: "enterprise",
            },
          },
        }}
      />,
    );
    expect(container.textContent).toContain("sk-ant-api03-w");
    expect(container.textContent).toContain("org_a1b2");
  });

  it("only renders groups that are present", () => {
    const { container } = render(
      <EnvelopeInspector
        defaultOpen
        envelope={{
          machine: {
            os_family: "macos",
            architecture: "aarch64",
          },
        }}
      />,
    );
    const titles = Array.from(container.querySelectorAll(".envelope-group-title")).map(
      (e) => e.textContent,
    );
    expect(titles).toEqual(["machine"]);
  });
});
