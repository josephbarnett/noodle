// S22 (refactor-overview.md §10): tool-use cross-record pairing
// arrow. When a [`DecodedExchange`] carries
// `pairing.resolved_by_request_id` (response with a tool_use) or
// `pairing.resolves_tool_use_in_request_id` (request with a
// tool_result), render a clickable link to the matched row.
//
// Click → invokes `onJump` with the target event_id. The parent
// view is expected to highlight or scroll-into-view that row.

import type { DecodedPairing } from "../types";

interface Props {
  pairing: DecodedPairing | null | undefined;
  /** Called with the target event_id when the user clicks the
   *  arrow. The parent view decides what "jump" means (typically
   *  scrollIntoView + setSelected). */
  onJump?: (targetEventId: string) => void;
}

export function ToolPairingArrow({ pairing, onJump }: Props) {
  if (!pairing) return null;

  // A request record carrying a tool_result points BACK to the
  // response that emitted the tool_use.
  const backRef = pairing.resolves_tool_use_in_request_id;
  // A response record carrying a tool_use points FORWARD to the
  // next request whose tool_result resolves it.
  const fwdRef = pairing.resolved_by_request_id;

  if (!backRef && !fwdRef) return null;

  return (
    <span className="tool-pairing">
      {backRef && (
        <button
          type="button"
          className="tool-pairing-arrow tool-pairing-back"
          onClick={(e) => {
            e.stopPropagation();
            onJump?.(backRef);
          }}
          title={`tool_result resolves tool_use in ${backRef}`}
        >
          ← {shortId(backRef)}
        </button>
      )}
      {fwdRef && (
        <button
          type="button"
          className="tool-pairing-arrow tool-pairing-fwd"
          onClick={(e) => {
            e.stopPropagation();
            onJump?.(fwdRef);
          }}
          title={`tool_use resolved by ${fwdRef}`}
        >
          {shortId(fwdRef)} →
        </button>
      )}
    </span>
  );
}

function shortId(id: string): string {
  // Mirror the noodle event_id convention: "nl-N" stays whole;
  // longer hex / ulid ids get truncated.
  if (id.length <= 8) return id;
  return id.slice(0, 6) + "…";
}
