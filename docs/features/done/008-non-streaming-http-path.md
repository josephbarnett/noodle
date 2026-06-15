# 008 — Non-streaming HTTP path

## Value

A non-streaming chat completion (`stream: false`) is processed by the
same code path as a streaming one. The adapter's `decode` yields a
synthetic stream of `TurnStart` + one or more `Token`s + `TurnEnd`,
the policy runs, and `encode` reassembles the response body.

The "single architecture" claim from the design doc holds, or it
doesn't — this story is what makes it falsifiable.

## Acceptance criteria

- A non-streaming OpenAI completion through noodle has its marker
  redacted exactly like the streaming case does.
- The same `DefaultTagPolicy` instance runs against both streaming and
  non-streaming traffic without branches in calling code.
- A test confirms the response `Content-Type` and `Content-Length`
  (when present) are correct after redaction. (Length will change;
  noodle must update or remove the header.)
- The Anthropic non-streaming case also passes.

## Dependencies

- 007 (so we test against both adapters).

## Implementation notes

- The temptation here is to special-case non-streaming. Resist.
  Implement `decode` for the non-streaming branch as: buffer body,
  parse JSON, walk content into `NormalizedEvent`s, terminate stream.
  Implement `encode` symmetrically: collect events, rebuild JSON,
  emit as one `Bytes` chunk.
- `Content-Length` recalculation: when the policy redacted bytes, the
  rebuilt JSON is shorter. Either remove the header (forcing
  chunked-or-EOF framing) or recompute. Document the choice.
- If you find yourself wanting two `LlmInspectionLayer`s — one
  streaming, one buffered — back up. The whole point of the design
  is one layer, one pipeline.
