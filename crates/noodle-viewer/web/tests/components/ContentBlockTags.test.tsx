// S22: ContentBlockTags — renders a tag per decoded content block;
// text / tool_use / vendor_specific can expand to show detail;
// turn_start / turn_end render as non-expandable boundary chips.

import { cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { ContentBlockTags } from "../../src/components/ContentBlockTags";
import type { DecodedContentBlock } from "../../src/types";

afterEach(cleanup);

const text: DecodedContentBlock = {
  kind: "content",
  request_id: "nl-1",
  provider: "anthropic",
  block_index: 0,
  category: "prose",
  text: "Hello world.",
};
const thinking: DecodedContentBlock = {
  kind: "content",
  request_id: "nl-1",
  provider: "anthropic",
  block_index: 1,
  category: "reasoning",
  text: "let me think...",
};
const tool: DecodedContentBlock = {
  kind: "tool_use",
  request_id: "nl-1",
  provider: "anthropic",
  block_index: 2,
  tool_use_id: "toolu_a",
  tool_name: "Read",
  input: { file_path: "/x" },
  capability: { kind: "file_read" },
};
const vendor: DecodedContentBlock = {
  kind: "vendor_specific",
  request_id: "nl-1",
  provider: "anthropic",
  direction: "response",
  block_kind: "vendor_specific",
  vendor_kind: "image",
  payload: { type: "image" },
};

describe("ContentBlockTags", () => {
  it("renders nothing when blocks is empty/absent", () => {
    const { container: c1 } = render(<ContentBlockTags blocks={[]} />);
    expect(c1.querySelector(".content-block-tags")).toBeNull();
    const { container: c2 } = render(<ContentBlockTags blocks={null} />);
    expect(c2.querySelector(".content-block-tags")).toBeNull();
  });

  it("renders one tag per block with the right kind label", () => {
    const { container } = render(
      <ContentBlockTags blocks={[text, thinking, tool, vendor]} />,
    );
    const tags = container.querySelectorAll(".block-tag");
    expect(tags).toHaveLength(4);
    expect(tags[0].textContent).toContain("text");
    expect(tags[1].textContent).toContain("thinking");
    expect(tags[2].textContent).toContain("tool_use");
    expect(tags[3].textContent).toContain("vendor:image");
  });

  it("expands text blocks on click revealing the full text", () => {
    const { container } = render(<ContentBlockTags blocks={[text]} />);
    const head = container.querySelector(".block-tag-head")! as HTMLButtonElement;
    expect(container.querySelector(".block-tag-detail")).toBeNull();
    fireEvent.click(head);
    const detail = container.querySelector(".block-tag-detail");
    expect(detail).toBeTruthy();
    expect(detail!.textContent).toContain("Hello world.");
  });

  it("expands tool_use blocks showing tool id + input", () => {
    const { container } = render(<ContentBlockTags blocks={[tool]} />);
    const head = container.querySelector(".block-tag-head")! as HTMLButtonElement;
    fireEvent.click(head);
    const detail = container.querySelector(".block-tag-detail")!;
    expect(detail.textContent).toContain("toolu_a");
    expect(detail.textContent).toContain("/x");
  });
});
