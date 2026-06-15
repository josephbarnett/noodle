import { describe, expect, it } from "vitest";
import {
  looksLikeAnthropicSse,
  parseAnthropicSse,
} from "../../src/store/derived/anthropic_sse";

describe("looksLikeAnthropicSse", () => {
  it("recognises a realistic stream", () => {
    expect(
      looksLikeAnthropicSse(`event: message_start\ndata: {"type":"message_start"}\n\n`),
    ).toBe(true);
  });
  it("rejects plain text", () => {
    expect(looksLikeAnthropicSse("just a string")).toBe(false);
  });
  it("rejects empty", () => {
    expect(looksLikeAnthropicSse("")).toBe(false);
  });
});

describe("parseAnthropicSse", () => {
  it("reconstructs a single text block from text_delta events", () => {
    const raw = [
      `event: message_start`,
      `data: {"type":"message_start","message":{"model":"claude-x","role":"assistant","content":[],"usage":{"input_tokens":1}}}`,
      ``,
      `event: content_block_start`,
      `data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}`,
      ``,
      `event: content_block_delta`,
      `data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi "}}`,
      ``,
      `event: content_block_delta`,
      `data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"there"}}`,
      ``,
      `event: content_block_stop`,
      `data: {"type":"content_block_stop","index":0}`,
      ``,
      `event: message_delta`,
      `data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}`,
      ``,
      `event: message_stop`,
      `data: {"type":"message_stop"}`,
      ``,
    ].join("\n");
    const parsed = parseAnthropicSse(raw);
    expect(parsed.model).toBe("claude-x");
    expect(parsed.stopReason).toBe("end_turn");
    expect(parsed.contentBlocks).toEqual([{ type: "text", text: "Hi there" }]);
    expect(parsed.usage?.output_tokens).toBe(5);
  });

  it("assembles tool_use input from input_json_delta partials", () => {
    const raw = [
      `event: content_block_start`,
      `data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"Bash"}}`,
      ``,
      `event: content_block_delta`,
      `data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\\"cmd\\":\\"l"}}`,
      ``,
      `event: content_block_delta`,
      `data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"s -la\\"}"}}`,
      ``,
      `event: content_block_stop`,
      `data: {"type":"content_block_stop","index":0}`,
      ``,
    ].join("\n");
    const parsed = parseAnthropicSse(raw);
    expect(parsed.contentBlocks).toHaveLength(1);
    const b = parsed.contentBlocks[0];
    expect(b.type).toBe("tool_use");
    if (b.type === "tool_use") {
      expect(b.name).toBe("Bash");
      expect(b.id).toBe("toolu_1");
      expect(b.input).toEqual({ cmd: "ls -la" });
    }
  });

  it("interleaves thinking + text + tool_use in index order", () => {
    const raw = [
      `event: content_block_start`,
      `data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}`,
      ``,
      `event: content_block_delta`,
      `data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"I should..."}}`,
      ``,
      `event: content_block_stop`,
      `data: {"type":"content_block_stop","index":0}`,
      ``,
      `event: content_block_start`,
      `data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}`,
      ``,
      `event: content_block_delta`,
      `data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"OK"}}`,
      ``,
      `event: content_block_stop`,
      `data: {"type":"content_block_stop","index":1}`,
      ``,
    ].join("\n");
    const parsed = parseAnthropicSse(raw);
    expect(parsed.contentBlocks.map((b) => b.type)).toEqual([
      "thinking",
      "text",
    ]);
  });

  it("ignores ping events and malformed data lines", () => {
    const raw = [
      `event: ping`,
      `data: {"type":"ping"}`,
      ``,
      `event: content_block_start`,
      `data: NOT JSON`,
      ``,
      `event: content_block_start`,
      `data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}`,
      ``,
      `event: content_block_delta`,
      `data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}`,
      ``,
      `event: content_block_stop`,
      `data: {"type":"content_block_stop","index":0}`,
      ``,
    ].join("\n");
    const parsed = parseAnthropicSse(raw);
    expect(parsed.contentBlocks).toEqual([{ type: "text", text: "hi" }]);
  });
});
